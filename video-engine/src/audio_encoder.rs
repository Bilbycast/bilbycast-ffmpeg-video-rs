// Copyright (c) 2026 Reza Rahimi / Softside Tech Pty Ltd. All rights reserved.
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
                    avcodec_find_encoder_by_name(b"libopus\0".as_ptr() as *const std::os::raw::c_char)
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
            (*ctx).strict_std_compliance = FF_COMPLIANCE_EXPERIMENTAL as i32;

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
                    for ch in 0..self.channels as usize {
                        let dst = std::slice::from_raw_parts_mut(
                            (*self.frame).data[ch] as *mut f32,
                            samples_per_channel,
                        );
                        dst.copy_from_slice(&planar[ch]);
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
