// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe audio encoder wrapping FFmpeg's `avcodec_*` API.
//!
//! Supports Opus, MP2, and AC-3 encoding. AAC variants are handled by
//! `bilbycast-fdk-aac-rs` — this module covers the non-AAC codecs.
//!
//! Input is planar f32 PCM (matching the bilbycast-edge audio pipeline).
//! Output is raw encoded frames without any container framing.
//!
//! # Thread Safety
//!
//! `AudioEncoder` is `Send` but not `Sync`. Each instance owns its
//! `AVCodecContext` and internal buffers. Requires `&mut self` for encode.

use bytes::Bytes;
use libffmpeg_video_sys::*;
use video_codec::{AudioCodecType, AudioEncoderConfig, AudioError};

/// A single encoded audio frame.
#[derive(Debug, Clone)]
pub struct EncodedAudioFrame {
    /// Raw encoded frame data (no container framing).
    /// - Opus: raw Opus packet
    /// - MP2: raw MP2 frame (with sync header)
    /// - AC-3: raw AC-3 frame (with sync header)
    pub data: Bytes,
    /// Number of PCM samples per channel that produced this frame.
    pub num_samples: usize,
    /// Presentation timestamp libavcodec stamped on the packet, in samples
    /// at [`AudioEncoder::sample_rate`], counted from the first sample fed
    /// to [`AudioEncoder::encode_frame`] (index 0). It already has the
    /// encoder delay subtracted, so the first packet is stamped
    /// `-initial_padding()`: the packet whose decoded output starts at
    /// sample `pts` of the input timeline. A caller that stamps a wire PTS
    /// from its own input clock must subtract
    /// [`AudioEncoder::initial_padding`] too, or every frame presents that
    /// many samples late.
    pub pts: i64,
}

/// Safe audio encoder wrapping FFmpeg's AVCodecContext.
pub struct AudioEncoder {
    ctx: *mut AVCodecContext,
    frame: *mut AVFrame,
    packet: *mut AVPacket,
    codec: AudioCodecType,
    /// Samples per frame required by this codec's encoder.
    frame_size: usize,
    sample_rate: u32,
    channels: u8,
    /// Monotonic frame counter for pts assignment.
    frame_count: i64,
    /// `AVCodecContext.initial_padding` after open — the encoder delay in
    /// samples at `sample_rate`.
    initial_padding: usize,
}

// SAFETY: AVCodecContext is per-instance with no shared global state.
unsafe impl Send for AudioEncoder {}

impl AudioEncoder {
    /// Open an audio encoder for the specified codec.
    pub fn open(config: &AudioEncoderConfig) -> Result<Self, AudioError> {
        unsafe {
            // Find the encoder
            let codec_ptr = match config.codec {
                AudioCodecType::Opus => {
                    // Use libopus encoder (higher quality than FFmpeg native)
                    avcodec_find_encoder_by_name(c"libopus".as_ptr())
                }
                AudioCodecType::Mp2 => {
                    avcodec_find_encoder(AVCodecID_AV_CODEC_ID_MP2)
                }
                AudioCodecType::Ac3 => {
                    avcodec_find_encoder(AVCodecID_AV_CODEC_ID_AC3)
                }
            };

            if codec_ptr.is_null() {
                return Err(AudioError::CodecNotFound(config.codec));
            }

            let ctx = avcodec_alloc_context3(codec_ptr);
            if ctx.is_null() {
                return Err(AudioError::AllocContext);
            }

            // Configure encoder parameters
            (*ctx).bit_rate = (config.bitrate_kbps as i64) * 1000;
            (*ctx).sample_rate = config.sample_rate as i32;

            // Sample format: all three encoders accept FLT planar
            (*ctx).sample_fmt = match config.codec {
                AudioCodecType::Opus => AVSampleFormat_AV_SAMPLE_FMT_FLT,  // libopus wants interleaved float
                AudioCodecType::Mp2 => AVSampleFormat_AV_SAMPLE_FMT_S16,   // mp2 wants s16
                AudioCodecType::Ac3 => AVSampleFormat_AV_SAMPLE_FMT_FLTP,  // ac3 wants planar float
            };

            // Channel layout — use the new AVChannelLayout API (FFmpeg >= 5.1)
            av_channel_layout_default(
                &mut (*ctx).ch_layout,
                config.channels as i32,
            );

            // Opus-specific: force 48 kHz (Opus requirement)
            if config.codec == AudioCodecType::Opus {
                (*ctx).sample_rate = 48000;
            }

            // Allow experimental codecs
            (*ctx).strict_std_compliance = FF_COMPLIANCE_EXPERIMENTAL;

            let ret = avcodec_open2(ctx, codec_ptr, std::ptr::null_mut());
            if ret < 0 {
                avcodec_free_context(&mut { ctx });
                return Err(AudioError::OpenCodec(ret));
            }

            // Read the frame size the encoder expects
            let frame_size = if (*ctx).frame_size > 0 {
                (*ctx).frame_size as usize
            } else {
                // Variable frame size — use a reasonable default
                1024
            };

            let actual_sample_rate = (*ctx).sample_rate as u32;
            // Encoder delay ("priming"), set by the codec at open: MP2 481
            // (512 - 32 + 1, mpegaudioenc.c), AC-3 256 (one block,
            // ac3enc.c), libopus its OPUS_GET_LOOKAHEAD (312 at 48 kHz).
            let initial_padding = (*ctx).initial_padding.max(0) as usize;

            // Allocate reusable frame
            let frame = av_frame_alloc();
            if frame.is_null() {
                avcodec_free_context(&mut { ctx });
                return Err(AudioError::AllocFrame);
            }

            (*frame).nb_samples = frame_size as i32;
            (*frame).format = (*ctx).sample_fmt;
            (*frame).ch_layout = (*ctx).ch_layout;
            (*frame).sample_rate = (*ctx).sample_rate;

            let ret = av_frame_get_buffer(frame, 0);
            if ret < 0 {
                av_frame_free(&mut { frame });
                avcodec_free_context(&mut { ctx });
                return Err(AudioError::AllocFrameBuffer(ret));
            }

            // Allocate reusable packet
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
                codec: config.codec,
                frame_size,
                sample_rate: actual_sample_rate,
                channels: config.channels,
                frame_count: 0,
                initial_padding,
            })
        }
    }

    /// Number of samples per channel the encoder expects per frame.
    pub fn frame_size(&self) -> usize {
        self.frame_size
    }

    /// Actual sample rate (may differ from requested for Opus: always 48 kHz).
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// The codec this encoder was opened for.
    pub fn codec(&self) -> AudioCodecType {
        self.codec
    }

    /// Encoder delay in samples at [`Self::sample_rate`]: how far the decoded
    /// output lags the input. The first `initial_padding()` decoded samples
    /// are priming, and input sample `n` decodes at output sample
    /// `n + initial_padding()` — MP2 481, AC-3 256, Opus 312 (at 48 kHz).
    ///
    /// libavcodec's own packet timestamps already account for it (see
    /// [`EncodedAudioFrame::pts`]). A caller that instead stamps packets from
    /// the PTS of the first input sample it fed must subtract
    /// `initial_padding() * 90_000 / sample_rate()` ticks, or the audio
    /// presents that much late against the video it was captured with.
    pub fn initial_padding(&self) -> usize {
        self.initial_padding
    }

    /// Encode one frame of planar f32 PCM audio.
    ///
    /// `planar` must have exactly `channels` inner vecs, each with exactly
    /// `frame_size()` samples. Returns zero or more encoded frames (most
    /// encoders produce exactly one, but some may buffer).
    pub fn encode_frame(&mut self, planar: &[Vec<f32>]) -> Result<Vec<EncodedAudioFrame>, AudioError> {
        if planar.len() != self.channels as usize {
            return Err(AudioError::InvalidInput(format!(
                "expected {} channels, got {}",
                self.channels,
                planar.len()
            )));
        }

        let samples_per_channel = planar[0].len();
        if samples_per_channel != self.frame_size {
            return Err(AudioError::InvalidInput(format!(
                "expected {} samples per channel, got {}",
                self.frame_size, samples_per_channel
            )));
        }

        unsafe {
            // Fill the AVFrame with PCM data based on the expected sample format
            (*self.frame).nb_samples = samples_per_channel as i32;
            (*self.frame).pts = self.frame_count * self.frame_size as i64;
            self.frame_count += 1;

            match (*self.ctx).sample_fmt {
                x if x == AVSampleFormat_AV_SAMPLE_FMT_FLT => {
                    // Interleaved f32: interleave channels into data[0]
                    let dst = std::slice::from_raw_parts_mut(
                        (*self.frame).data[0] as *mut f32,
                        samples_per_channel * self.channels as usize,
                    );
                    for s in 0..samples_per_channel {
                        for ch in 0..self.channels as usize {
                            dst[s * self.channels as usize + ch] = planar[ch][s];
                        }
                    }
                }
                x if x == AVSampleFormat_AV_SAMPLE_FMT_FLTP => {
                    // Planar f32: each channel in its own data[ch] plane
                    // `planar.len() == self.channels` is checked on entry.
                    for (ch, plane) in planar.iter().enumerate() {
                        let dst = std::slice::from_raw_parts_mut(
                            (*self.frame).data[ch] as *mut f32,
                            samples_per_channel,
                        );
                        dst.copy_from_slice(plane);
                    }
                }
                x if x == AVSampleFormat_AV_SAMPLE_FMT_S16 => {
                    // Interleaved s16: convert f32 → s16 and interleave
                    let dst = std::slice::from_raw_parts_mut(
                        (*self.frame).data[0] as *mut i16,
                        samples_per_channel * self.channels as usize,
                    );
                    for s in 0..samples_per_channel {
                        for ch in 0..self.channels as usize {
                            let sample = (planar[ch][s] * 32767.0).clamp(-32768.0, 32767.0);
                            dst[s * self.channels as usize + ch] = sample as i16;
                        }
                    }
                }
                _ => {
                    return Err(AudioError::InvalidInput(
                        "unsupported sample format".to_string(),
                    ));
                }
            }

            self.send_and_receive()
        }
    }

    /// Flush the encoder — drain any buffered frames.
    pub fn flush(&mut self) -> Result<Vec<EncodedAudioFrame>, AudioError> {
        unsafe {
            // Send NULL frame to signal end of stream
            let ret = avcodec_send_frame(self.ctx, std::ptr::null());
            if ret < 0 && ret != -11 && ret != -541478725 {
                return Err(AudioError::SendFrame(ret));
            }

            let mut frames = Vec::new();
            loop {
                av_packet_unref(self.packet);
                let ret = avcodec_receive_packet(self.ctx, self.packet);
                if ret < 0 {
                    break;
                }
                let data = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
                frames.push(EncodedAudioFrame {
                    data: Bytes::copy_from_slice(data),
                    num_samples: self.frame_size,
                    pts: (*self.packet).pts,
                });
            }
            Ok(frames)
        }
    }

    /// Send the current frame and receive any available encoded packets.
    unsafe fn send_and_receive(&mut self) -> Result<Vec<EncodedAudioFrame>, AudioError> {
        let ret = avcodec_send_frame(self.ctx, self.frame);
        if ret < 0 {
            return Err(AudioError::SendFrame(ret));
        }

        let mut frames = Vec::new();
        loop {
            av_packet_unref(self.packet);
            let ret = avcodec_receive_packet(self.ctx, self.packet);
            if ret < 0 {
                // EAGAIN or EOF — no more packets right now
                break;
            }

            let data = std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
            frames.push(EncodedAudioFrame {
                data: Bytes::copy_from_slice(data),
                num_samples: self.frame_size,
                pts: (*self.packet).pts,
            });
        }

        Ok(frames)
    }
}

impl Drop for AudioEncoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.packet);
            av_frame_free(&mut self.frame);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

impl std::fmt::Debug for AudioEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioEncoder")
            .field("codec", &self.codec)
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("frame_size", &self.frame_size)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        crate::silence_ffmpeg_logs();
    }

    #[test]
    fn open_close_opus() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Opus,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 128,
        };
        let enc = AudioEncoder::open(&config).expect("open Opus encoder");
        assert!(enc.frame_size() > 0);
        assert_eq!(enc.sample_rate(), 48000);
    }

    #[test]
    fn open_close_mp2() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Mp2,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 192,
        };
        let enc = AudioEncoder::open(&config).expect("open MP2 encoder");
        assert!(enc.frame_size() > 0);
    }

    #[test]
    fn open_close_ac3() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Ac3,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 192,
        };
        let enc = AudioEncoder::open(&config).expect("open AC-3 encoder");
        assert!(enc.frame_size() > 0);
    }

    #[test]
    fn encode_opus_silence() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Opus,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 128,
        };
        let mut enc = AudioEncoder::open(&config).unwrap();
        let frame_size = enc.frame_size();

        // Generate silence (two channels of zeros)
        let planar = vec![vec![0.0f32; frame_size]; 2];

        // Encode a few frames
        let mut total_encoded = 0;
        for _ in 0..5 {
            let frames = enc.encode_frame(&planar).expect("encode should succeed");
            total_encoded += frames.len();
        }

        // Flush remaining
        let flush_frames = enc.flush().expect("flush should succeed");
        total_encoded += flush_frames.len();

        assert!(total_encoded > 0, "should have produced at least one encoded frame");
    }

    #[test]
    fn encode_mp2_silence() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Mp2,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 192,
        };
        let mut enc = AudioEncoder::open(&config).unwrap();
        let frame_size = enc.frame_size();

        let planar = vec![vec![0.0f32; frame_size]; 2];

        let mut total_encoded = 0;
        for _ in 0..3 {
            let frames = enc.encode_frame(&planar).expect("encode should succeed");
            total_encoded += frames.len();
        }
        let flush_frames = enc.flush().expect("flush should succeed");
        total_encoded += flush_frames.len();

        assert!(total_encoded > 0);
    }

    #[test]
    fn encode_ac3_silence() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Ac3,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 192,
        };
        let mut enc = AudioEncoder::open(&config).unwrap();
        let frame_size = enc.frame_size();

        let planar = vec![vec![0.0f32; frame_size]; 2];

        let mut total_encoded = 0;
        for _ in 0..3 {
            let frames = enc.encode_frame(&planar).expect("encode should succeed");
            total_encoded += frames.len();
        }
        let flush_frames = enc.flush().expect("flush should succeed");
        total_encoded += flush_frames.len();

        assert!(total_encoded > 0);
    }

    fn open(codec: AudioCodecType) -> AudioEncoder {
        AudioEncoder::open(&AudioEncoderConfig {
            codec,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 192,
        })
        .unwrap()
    }

    #[test]
    fn initial_padding_is_the_codec_declared_delay() {
        // mpegaudioenc.c: 512 - 32 + 1; ac3enc.c: AC3_BLOCK_SIZE; libopus:
        // OPUS_GET_LOOKAHEAD at 48 kHz (2.5 ms + 4 ms).
        init();
        assert_eq!(open(AudioCodecType::Mp2).initial_padding(), 481);
        assert_eq!(open(AudioCodecType::Ac3).initial_padding(), 256);
        assert_eq!(open(AudioCodecType::Opus).initial_padding(), 312);
    }

    #[test]
    fn packet_pts_is_input_position_minus_padding() {
        // libavcodec stamps each packet at (first input sample it covers) -
        // initial_padding, in samples. The first one is therefore negative:
        // that is the priming a receiver must not present.
        init();
        for codec in [AudioCodecType::Mp2, AudioCodecType::Ac3, AudioCodecType::Opus] {
            let mut enc = open(codec);
            let fs = enc.frame_size();
            let pad = enc.initial_padding() as i64;
            let silence = vec![vec![0.0f32; fs]; 2];
            let mut pts = Vec::new();
            for _ in 0..6 {
                pts.extend(enc.encode_frame(&silence).unwrap().iter().map(|f| f.pts));
            }
            pts.extend(enc.flush().unwrap().iter().map(|f| f.pts));
            assert!(!pts.is_empty(), "{codec}");
            assert_eq!(pts[0], -pad, "{codec}: first packet pts");
            for w in pts.windows(2) {
                assert_eq!(w[1] - w[0], fs as i64, "{codec}: packet spacing");
            }
        }
    }

    /// Encode silence with a short band-limited tone burst starting at input
    /// sample `at`, decode it with libavcodec, and return the decoded sample
    /// index where the burst correlates best.
    fn round_trip_burst_position(codec: AudioCodecType, at: usize) -> usize {
        use crate::audio_decoder::AudioDecoder;
        use video_codec::AudioDecoderCodec;

        let mut enc = open(codec);
        let fs = enc.frame_size();
        let burst: Vec<f32> = (0..480)
            .map(|k| {
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / 479.0).cos();
                0.5 * w * (2.0 * std::f32::consts::PI * 1000.0 * k as f32 / 48000.0).sin()
            })
            .collect();
        let total = fs * 20;
        let mut signal = vec![0.0f32; total];
        signal[at..at + burst.len()].copy_from_slice(&burst);

        let mut packets = Vec::new();
        for chunk in signal.chunks(fs) {
            let planar = vec![chunk.to_vec(), chunk.to_vec()];
            packets.extend(enc.encode_frame(&planar).unwrap());
        }
        packets.extend(enc.flush().unwrap());

        let mut dec = AudioDecoder::open(match codec {
            AudioCodecType::Mp2 => AudioDecoderCodec::Mp2,
            AudioCodecType::Ac3 => AudioDecoderCodec::Ac3,
            AudioCodecType::Opus => AudioDecoderCodec::Opus,
        })
        .unwrap();
        let mut decoded = Vec::new();
        for p in &packets {
            dec.send_packet(&p.data, p.pts).unwrap();
            while let Ok(f) = dec.receive_frame() {
                decoded.extend_from_slice(&f.planar[0]);
            }
        }
        assert!(decoded.len() > at + 4000, "{codec}: decoded {} samples", decoded.len());

        (0..4000)
            .max_by(|&a, &b| {
                let score = |lag: usize| -> f32 {
                    burst
                        .iter()
                        .zip(&decoded[at + lag..])
                        .map(|(x, y)| x * y)
                        .sum()
                };
                score(a).total_cmp(&score(b))
            })
            .map(|lag| at + lag)
            .unwrap()
    }

    #[test]
    fn round_trip_lag_equals_initial_padding() {
        // The whole encode + decode delay of a libavcodec MP2 / AC-3 chain is
        // the encoder's declared padding (their decoders add none), which is
        // what a caller stamping wire PTS from its input clock has to take
        // off. Without it, re-encoded audio presents 10.0 ms (MP2) / 5.3 ms
        // (AC-3) late at 48 kHz.
        init();
        for codec in [AudioCodecType::Mp2, AudioCodecType::Ac3] {
            let at = 4800;
            let found = round_trip_burst_position(codec, at);
            let lag = found as i64 - at as i64;
            let pad = open(codec).initial_padding() as i64;
            assert!(
                (lag - pad).abs() <= 2,
                "{codec}: round-trip lag {lag} samples vs initial_padding {pad}"
            );
        }
    }

    #[test]
    fn wrong_channel_count_rejected() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Opus,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 128,
        };
        let mut enc = AudioEncoder::open(&config).unwrap();
        let frame_size = enc.frame_size();

        // Send mono instead of stereo
        let planar = vec![vec![0.0f32; frame_size]; 1];
        let result = enc.encode_frame(&planar);
        assert!(result.is_err());
    }

    #[test]
    fn wrong_frame_size_rejected() {
        init();
        let config = AudioEncoderConfig {
            codec: AudioCodecType::Opus,
            sample_rate: 48000,
            channels: 2,
            bitrate_kbps: 128,
        };
        let mut enc = AudioEncoder::open(&config).unwrap();

        // Send wrong frame size
        let planar = vec![vec![0.0f32; 100]; 2];
        let result = enc.encode_frame(&planar);
        assert!(result.is_err());
    }
}
