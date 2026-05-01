// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe audio decoder wrapping FFmpeg's `avcodec_*` API.
//!
//! Mirrors the [`crate::VideoDecoder`] shape:
//!
//! - `open(codec)` opens an `AVCodecContext` for MP2 / AC-3 / E-AC-3 / Opus.
//! - `send_packet(data, pts)` feeds one elementary-stream packet.
//! - `receive_frame()` returns one [`DecodedAudioFrame`] in **planar f32 PCM**.
//! - `flush()` resets the decoder state (call after a stream restart or
//!   after an `RecvError::Lagged` recovery on the broadcast subscriber).
//!
//! AAC variants stay on `bilbycast-fdk-aac-rs` (FDK is already in tree and
//! produces planar f32 directly); this module covers the non-AAC broadcast
//! codecs the bilbycast-edge `display` output renders to ALSA.
//!
//! # Sample-format normalisation
//!
//! Every codec is normalised to **planar f32** so the bilbycast-edge audio
//! pipeline can stay uniform. MP2 produces `s16p` natively; AC-3 / E-AC-3
//! produce `fltp` directly; Opus produces `flt` (interleaved). When the
//! source format is not already planar f32, we lazily allocate a
//! `SwrContext` keyed on `(input_fmt, sample_rate, channels)` and convert
//! through it. The resampler is reused across frames and only re-init'd
//! when the upstream format changes.
//!
//! # Thread Safety
//!
//! `AudioDecoder` is `Send` but not `Sync`. Each instance owns its
//! `AVCodecContext` + `SwrContext` + reusable frame/packet, and requires
//! `&mut self` for decode operations.

use libffmpeg_video_sys::*;
use video_codec::{AudioDecoderCodec, AudioError};

/// One decoded audio frame, **planar f32 PCM**.
#[derive(Debug, Clone)]
pub struct DecodedAudioFrame {
    /// Planar samples: outer vec is per-channel, inner is per-sample.
    /// `planar.len()` == channel count; every inner vec has the same
    /// length (== `samples_per_channel`).
    pub planar: Vec<Vec<f32>>,
    /// Number of samples per channel (== inner vec length).
    pub samples_per_channel: usize,
    /// Source sample rate (Hz).
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u8,
    /// Source PTS in the codec's time base. The caller is responsible
    /// for converting to wall-clock — bilbycast-edge passes the 90 kHz
    /// PTS straight through from `TsDemuxer`.
    pub pts: i64,
}

/// Safe audio decoder wrapping FFmpeg's `AVCodecContext` + `SwrContext`.
pub struct AudioDecoder {
    ctx: *mut AVCodecContext,
    frame: *mut AVFrame,
    packet: *mut AVPacket,
    /// Lazily-allocated resampler. `None` while we don't yet know the
    /// upstream sample format (i.e. before the first `receive_frame`),
    /// or when the source is already planar f32 (no conversion needed).
    swr: *mut SwrContext,
    /// Cached upstream format the resampler is configured for. Used to
    /// detect mid-stream format changes and re-init the resampler.
    swr_input_fmt: AVSampleFormat,
    swr_input_sample_rate: i32,
    swr_input_channels: i32,
    codec: AudioDecoderCodec,
}

// SAFETY: AVCodecContext and SwrContext are per-instance with no shared
// global state. Same shape as `AudioEncoder` and `VideoDecoder`.
unsafe impl Send for AudioDecoder {}

impl AudioDecoder {
    /// Open a decoder for the specified codec. Most callers don't need to
    /// pre-supply codec extradata — MP2 / AC-3 / E-AC-3 carry the necessary
    /// info inline in every frame, and Opus inside MPEG-TS rides with a
    /// registration descriptor that the demuxer ingests separately.
    pub fn open(codec: AudioDecoderCodec) -> Result<Self, AudioError> {
        unsafe {
            let codec_ptr = match codec {
                AudioDecoderCodec::Mp2 => avcodec_find_decoder(AVCodecID_AV_CODEC_ID_MP2),
                AudioDecoderCodec::Ac3 => avcodec_find_decoder(AVCodecID_AV_CODEC_ID_AC3),
                AudioDecoderCodec::Eac3 => avcodec_find_decoder(AVCodecID_AV_CODEC_ID_EAC3),
                AudioDecoderCodec::Opus => {
                    // libopus has higher quality than the FFmpeg native
                    // Opus decoder; both are LGPL-clean.
                    let by_name = avcodec_find_decoder_by_name(
                        b"libopus\0".as_ptr() as *const std::os::raw::c_char,
                    );
                    if by_name.is_null() {
                        avcodec_find_decoder(AVCodecID_AV_CODEC_ID_OPUS)
                    } else {
                        by_name
                    }
                }
            };
            if codec_ptr.is_null() {
                return Err(AudioError::DecoderNotFound(codec));
            }

            let ctx = avcodec_alloc_context3(codec_ptr);
            if ctx.is_null() {
                return Err(AudioError::AllocContext);
            }

            let ret = avcodec_open2(ctx, codec_ptr, std::ptr::null_mut());
            if ret < 0 {
                avcodec_free_context(&mut { ctx });
                return Err(AudioError::OpenCodec(ret));
            }

            let frame = av_frame_alloc();
            if frame.is_null() {
                avcodec_free_context(&mut { ctx });
                return Err(AudioError::AllocFrame);
            }
            let packet = av_packet_alloc();
            if packet.is_null() {
                av_frame_free(&mut { frame });
                avcodec_free_context(&mut { ctx });
                return Err(AudioError::AllocPacket);
            }

            Ok(Self {
                ctx,
                frame,
                packet,
                swr: std::ptr::null_mut(),
                swr_input_fmt: AVSampleFormat_AV_SAMPLE_FMT_NONE,
                swr_input_sample_rate: 0,
                swr_input_channels: 0,
                codec,
            })
        }
    }

    /// Codec being decoded.
    pub fn codec(&self) -> AudioDecoderCodec {
        self.codec
    }

    /// Feed one elementary-stream audio packet. PTS is in the codec's
    /// native time base — bilbycast-edge passes the demuxer's 90 kHz PTS
    /// through unchanged; the decoded frame echoes it back so the caller
    /// can pace ALSA writes.
    pub fn send_packet(&mut self, data: &[u8], pts: i64) -> Result<(), AudioError> {
        unsafe {
            (*self.packet).data = data.as_ptr() as *mut u8;
            (*self.packet).size = data.len() as i32;
            (*self.packet).pts = pts;
            (*self.packet).dts = pts;

            let ret = avcodec_send_packet(self.ctx, self.packet);
            // Reset packet pointers so we don't dangle into freed memory
            // when the borrow ends.
            (*self.packet).data = std::ptr::null_mut();
            (*self.packet).size = 0;
            if ret < 0 {
                return Err(AudioError::SendPacket(ret));
            }
            Ok(())
        }
    }

    /// Drain one decoded frame, normalised to planar f32 PCM. Call
    /// repeatedly after each `send_packet` until you get
    /// `AudioError::NeedMoreInput`.
    pub fn receive_frame(&mut self) -> Result<DecodedAudioFrame, AudioError> {
        unsafe {
            let ret = avcodec_receive_frame(self.ctx, self.frame);
            if ret == AVERROR_EAGAIN_HACK {
                return Err(AudioError::NeedMoreInput);
            }
            if ret == AVERROR_EOF_HACK {
                return Err(AudioError::Eof);
            }
            if ret < 0 {
                return Err(AudioError::ReceiveFrame(ret));
            }

            let nb = (*self.frame).nb_samples as usize;
            let sr = (*self.frame).sample_rate as u32;
            let channels = (*self.frame).ch_layout.nb_channels.max(1) as u8;
            let pts = (*self.frame).pts;
            let src_fmt = (*self.frame).format;

            // Fast path: codec already produces planar f32. AC-3, E-AC-3,
            // and the FFmpeg Opus decoder all take this path on every
            // mainline build.
            let planar = if src_fmt == AVSampleFormat_AV_SAMPLE_FMT_FLTP {
                let mut out: Vec<Vec<f32>> = Vec::with_capacity(channels as usize);
                for ch in 0..(channels as usize) {
                    let p = (*self.frame).data[ch] as *const f32;
                    let slice = std::slice::from_raw_parts(p, nb);
                    out.push(slice.to_vec());
                }
                out
            } else {
                // Slow path: convert through swresample. Re-init when the
                // upstream format / SR / channel count changes.
                self.ensure_resampler(src_fmt, sr as i32, channels as i32)?;
                self.convert_to_planar_f32(nb, channels)?
            };

            // Done with the frame — unref before the next decode.
            av_frame_unref(self.frame);

            Ok(DecodedAudioFrame {
                planar,
                samples_per_channel: nb,
                sample_rate: sr,
                channels,
                pts,
            })
        }
    }

    /// Reset the codec state. Called after broadcast `Lagged` recovery so
    /// the next IDR / sync frame is the new anchor.
    pub fn flush(&mut self) {
        unsafe {
            avcodec_flush_buffers(self.ctx);
        }
    }

    // ── internals ──────────────────────────────────────────────────

    unsafe fn ensure_resampler(
        &mut self,
        input_fmt: AVSampleFormat,
        input_sr: i32,
        input_channels: i32,
    ) -> Result<(), AudioError> {
        if !self.swr.is_null()
            && self.swr_input_fmt == input_fmt
            && self.swr_input_sample_rate == input_sr
            && self.swr_input_channels == input_channels
        {
            return Ok(());
        }
        // Tear down stale resampler before re-init.
        if !self.swr.is_null() {
            swr_free(&mut self.swr);
            self.swr = std::ptr::null_mut();
        }

        let mut in_layout = std::mem::zeroed::<AVChannelLayout>();
        let mut out_layout = std::mem::zeroed::<AVChannelLayout>();
        av_channel_layout_default(&mut in_layout, input_channels);
        av_channel_layout_default(&mut out_layout, input_channels);

        let ret = swr_alloc_set_opts2(
            &mut self.swr,
            &out_layout,
            AVSampleFormat_AV_SAMPLE_FMT_FLTP,
            input_sr,
            &in_layout,
            input_fmt,
            input_sr,
            0,
            std::ptr::null_mut(),
        );
        if ret < 0 || self.swr.is_null() {
            return Err(AudioError::AllocResampler(ret));
        }
        let ret = swr_init(self.swr);
        if ret < 0 {
            swr_free(&mut self.swr);
            self.swr = std::ptr::null_mut();
            return Err(AudioError::AllocResampler(ret));
        }
        self.swr_input_fmt = input_fmt;
        self.swr_input_sample_rate = input_sr;
        self.swr_input_channels = input_channels;
        Ok(())
    }

    unsafe fn convert_to_planar_f32(
        &mut self,
        nb: usize,
        channels: u8,
    ) -> Result<Vec<Vec<f32>>, AudioError> {
        // Allocate per-channel destination buffers.
        let mut planes: Vec<Vec<f32>> = (0..(channels as usize))
            .map(|_| vec![0.0_f32; nb])
            .collect();
        // Build a Vec of *mut u8 (one per channel) for swr_convert.
        let mut plane_ptrs: Vec<*mut u8> = planes
            .iter_mut()
            .map(|v| v.as_mut_ptr() as *mut u8)
            .collect();

        let in_data: *mut *const u8 = (*self.frame).data.as_mut_ptr() as *mut *const u8;
        let out_data: *mut *mut u8 = plane_ptrs.as_mut_ptr();
        let written = swr_convert(
            self.swr,
            out_data,
            nb as i32,
            in_data,
            nb as i32,
        );
        if written < 0 {
            return Err(AudioError::ResampleConvert(written));
        }
        // If the resampler wrote fewer samples than expected, truncate.
        let w = written as usize;
        if w < nb {
            for v in planes.iter_mut() {
                v.truncate(w);
            }
        }
        Ok(planes)
    }
}

impl Drop for AudioDecoder {
    fn drop(&mut self) {
        unsafe {
            if !self.swr.is_null() {
                swr_free(&mut self.swr);
            }
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame);
            }
            if !self.ctx.is_null() {
                avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

// FFmpeg's AVERROR macros are C macros that bindgen doesn't lift cleanly
// across all toolchains. The two we need are EAGAIN and EOF; their
// values are stable in practice (`AVERROR(EAGAIN)` == -11 on Linux/macOS,
// `AVERROR_EOF` == -541478725). Rather than hard-code, we synthesise the
// same arithmetic FFmpeg does so a future libc surprise still works.
#[allow(non_upper_case_globals)]
const AVERROR_EAGAIN_HACK: i32 = -(libc_eagain() as i32);

const AVERROR_EOF_HACK: i32 = {
    // 'E' << 24 | 'O' << 16 | 'F' << 8 | ' ' (with sign flip per FFTAG).
    // Identical to FFmpeg's MKTAG('E','O','F',' ') sign-flipped.
    let tag: u32 = (b'E' as u32)
        | ((b'O' as u32) << 8)
        | ((b'F' as u32) << 16)
        | ((b' ' as u32) << 24);
    -(tag as i32)
};

const fn libc_eagain() -> u32 {
    // Linux + macOS + FreeBSD + Android all use 11 for EAGAIN/EWOULDBLOCK
    // on every architecture currently supported by the FFmpeg vendored
    // build. Windows uses a different value but we don't target it for
    // the display output.
    11
}
