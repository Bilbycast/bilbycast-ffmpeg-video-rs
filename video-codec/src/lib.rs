// Copyright (c) 2026 Reza Rahimi / Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Pure Rust types for video codec operations.
//!
//! This crate has zero C dependencies. It provides shared types used by both
//! the `video-engine` safe wrapper and `bilbycast-edge`.

use thiserror::Error;

/// Supported video codecs for decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    /// MPEG-TS stream type identifier.
    pub fn stream_type(self) -> u8 {
        match self {
            VideoCodec::H264 => 0x1B,
            VideoCodec::Hevc => 0x24,
        }
    }

    /// Try to identify codec from MPEG-TS stream type.
    pub fn from_stream_type(st: u8) -> Option<Self> {
        match st {
            0x1B => Some(VideoCodec::H264),
            0x24 => Some(VideoCodec::Hevc),
            _ => None,
        }
    }
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoCodec::H264 => write!(f, "H.264"),
            VideoCodec::Hevc => write!(f, "H.265/HEVC"),
        }
    }
}

/// Pixel format of a decoded video frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Planar YUV 4:2:0, 12bpp (the most common decoded format).
    Yuv420p,
    /// Planar YUV 4:2:2, 16bpp.
    Yuv422p,
    /// Planar YUV 4:4:4, 24bpp.
    Yuv444p,
    /// 24-bit packed RGB (used for JPEG input).
    Rgb24,
    /// Planar YUV 4:2:0 with JPEG full range (0-255 luma).
    Yuvj420p,
    /// Planar YUV 4:2:2 with JPEG full range.
    Yuvj422p,
    /// Planar YUV 4:4:4 with JPEG full range.
    Yuvj444p,
    /// 10-bit YUV 4:2:0 (HEVC main 10).
    Yuv420p10le,
    /// Planar 10-bit YUV 4:2:2, little-endian (broadcast 4:2:2 P10).
    Yuv422p10le,
}

/// Destination pixel format for [`VideoScaler`]. Lets callers select between
/// the existing thumbnail-oriented YUVJ420P default and planar broadcast
/// formats required by RFC 4175 packetizers (ST 2110-20 / -23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalerDstFormat {
    /// Planar YUV 4:2:0, full-range. Existing default (MJPEG-compatible).
    Yuvj420p,
    /// Planar YUV 4:2:2, 8-bit. Broadcast-range.
    Yuv422p8,
    /// Planar YUV 4:2:2, 10-bit little-endian. Broadcast 10-bit.
    Yuv422p10le,
}

/// Configuration for thumbnail generation.
#[derive(Debug, Clone)]
pub struct ThumbnailConfig {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// JPEG quality (1-31, lower is better; ffmpeg mjpeg scale).
    pub quality: u32,
}

impl Default for ThumbnailConfig {
    fn default() -> Self {
        Self {
            width: 320,
            height: 180,
            quality: 5,
        }
    }
}

// ── Audio codec types ──────────────────────────────────────────────────

/// Supported audio codecs for in-process encoding.
///
/// AAC variants are handled by `bilbycast-fdk-aac-rs` — this enum covers
/// the non-AAC codecs that use FFmpeg's libavcodec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodecType {
    /// Opus (via libopus). WebRTC standard audio codec.
    Opus,
    /// MPEG-1 Layer II. Legacy broadcast audio.
    Mp2,
    /// AC-3 (Dolby Digital). Broadcast/cinema audio.
    Ac3,
}

impl std::fmt::Display for AudioCodecType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioCodecType::Opus => write!(f, "Opus"),
            AudioCodecType::Mp2 => write!(f, "MP2"),
            AudioCodecType::Ac3 => write!(f, "AC-3"),
        }
    }
}

/// Configuration for audio encoding.
#[derive(Debug, Clone)]
pub struct AudioEncoderConfig {
    /// Output codec.
    pub codec: AudioCodecType,
    /// Input/output sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels.
    pub channels: u8,
    /// Target bitrate in kbps.
    pub bitrate_kbps: u32,
}

/// Errors produced by audio encode operations.
#[derive(Debug, Error)]
pub enum AudioError {
    /// Audio codec not found in FFmpeg's registry.
    #[error("audio codec not found: {0}")]
    CodecNotFound(AudioCodecType),

    /// Failed to allocate codec context.
    #[error("failed to allocate audio codec context")]
    AllocContext,

    /// Failed to open codec.
    #[error("failed to open audio codec: FFmpeg error {0}")]
    OpenCodec(i32),

    /// Failed to allocate frame.
    #[error("failed to allocate audio frame")]
    AllocFrame,

    /// Failed to allocate frame buffer.
    #[error("failed to allocate audio frame buffer: FFmpeg error {0}")]
    AllocFrameBuffer(i32),

    /// Failed to send frame to encoder.
    #[error("failed to send audio frame to encoder: FFmpeg error {0}")]
    SendFrame(i32),

    /// Failed to receive encoded packet.
    #[error("failed to receive encoded audio packet: FFmpeg error {0}")]
    ReceivePacket(i32),

    /// Failed to allocate packet.
    #[error("failed to allocate audio packet")]
    AllocPacket,

    /// Invalid input parameters.
    #[error("invalid audio input: {0}")]
    InvalidInput(String),
}

// ── Video encoder types ─────────────────────────────────────────────────

/// Encoder backend for H.264 / HEVC video compression.
///
/// The availability of each backend depends on the build configuration:
/// - `X264` requires the `video-encoder-x264` feature (GPL v2+).
/// - `X265` requires the `video-encoder-x265` feature (GPL v2+).
/// - `H264Nvenc` / `HevcNvenc` require the `video-encoder-nvenc` feature
///   and an NVIDIA GPU with a suitable driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEncoderCodec {
    /// libx264 → H.264 (Advanced Video Coding).
    X264,
    /// libx265 → HEVC (H.265).
    X265,
    /// NVENC H.264 hardware encoder.
    H264Nvenc,
    /// NVENC HEVC hardware encoder.
    HevcNvenc,
}

impl VideoEncoderCodec {
    /// The codec family produced on the wire, irrespective of backend.
    pub fn family(self) -> VideoCodec {
        match self {
            VideoEncoderCodec::X264 | VideoEncoderCodec::H264Nvenc => VideoCodec::H264,
            VideoEncoderCodec::X265 | VideoEncoderCodec::HevcNvenc => VideoCodec::Hevc,
        }
    }

    /// FFmpeg encoder name passed to `avcodec_find_encoder_by_name`.
    pub fn ffmpeg_name(self) -> &'static str {
        match self {
            VideoEncoderCodec::X264 => "libx264",
            VideoEncoderCodec::X265 => "libx265",
            VideoEncoderCodec::H264Nvenc => "h264_nvenc",
            VideoEncoderCodec::HevcNvenc => "hevc_nvenc",
        }
    }
}

impl std::fmt::Display for VideoEncoderCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.ffmpeg_name())
    }
}

/// Encoder speed / quality preset. Semantics mirror libx264/x265 presets;
/// NVENC maps them onto the nearest equivalent `-preset` internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoPreset {
    Ultrafast,
    Superfast,
    Veryfast,
    Faster,
    Fast,
    #[default]
    Medium,
    Slow,
    Slower,
    Veryslow,
}

impl VideoPreset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ultrafast => "ultrafast",
            Self::Superfast => "superfast",
            Self::Veryfast => "veryfast",
            Self::Faster => "faster",
            Self::Fast => "fast",
            Self::Medium => "medium",
            Self::Slow => "slow",
            Self::Slower => "slower",
            Self::Veryslow => "veryslow",
        }
    }
}

/// H.264/HEVC profile target. `Auto` lets the encoder pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoProfile {
    #[default]
    Auto,
    Baseline,
    Main,
    High,
}

impl VideoProfile {
    pub fn as_str(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Baseline => Some("baseline"),
            Self::Main => Some("main"),
            Self::High => Some("high"),
        }
    }
}

/// Configuration for a single video encoder instance.
#[derive(Debug, Clone)]
pub struct VideoEncoderConfig {
    /// Which backend to use. Must match a compiled-in feature flag.
    pub codec: VideoEncoderCodec,
    /// Output frame width in pixels.
    pub width: u32,
    /// Output frame height in pixels.
    pub height: u32,
    /// Frame rate numerator (e.g. 30000 for 29.97 fps).
    pub fps_num: u32,
    /// Frame rate denominator (e.g. 1001 for 29.97 fps).
    pub fps_den: u32,
    /// Target average bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Keyframe interval (GOP size) in frames.
    pub gop_size: u32,
    /// Speed / quality preset.
    pub preset: VideoPreset,
    /// Profile target.
    pub profile: VideoProfile,
    /// Emit SPS/PPS (or VPS/SPS/PPS) as a separate extradata blob
    /// instead of inside the bitstream. Required for RTP / RTMP / MP4.
    pub global_header: bool,
}

impl Default for VideoEncoderConfig {
    fn default() -> Self {
        Self {
            codec: VideoEncoderCodec::X264,
            width: 1280,
            height: 720,
            fps_num: 30,
            fps_den: 1,
            bitrate_kbps: 4000,
            gop_size: 60,
            preset: VideoPreset::default(),
            profile: VideoProfile::default(),
            global_header: true,
        }
    }
}

/// One encoded video frame emitted by a [`VideoEncoder`].
#[derive(Debug, Clone)]
pub struct EncodedVideoFrame {
    /// Compressed bitstream (Annex B NALUs for H.264/HEVC).
    pub data: Vec<u8>,
    /// Presentation timestamp in the encoder time base (1 / (fps_num)).
    pub pts: i64,
    /// Decode timestamp in the same time base (may equal `pts` when
    /// there are no B-frames).
    pub dts: i64,
    /// True if this frame is an IDR / keyframe.
    pub keyframe: bool,
}

/// Errors produced by a video encoder.
#[derive(Debug, Error)]
pub enum VideoEncoderError {
    #[error("encoder not compiled in: {0}. Rebuild with the matching feature flag.")]
    EncoderDisabled(VideoEncoderCodec),
    #[error("encoder not found in FFmpeg: {0}")]
    EncoderNotFound(VideoEncoderCodec),
    #[error("failed to allocate encoder context")]
    AllocContext,
    #[error("failed to allocate frame")]
    AllocFrame,
    #[error("failed to allocate frame buffer: FFmpeg error {0}")]
    AllocFrameBuffer(i32),
    #[error("failed to allocate packet")]
    AllocPacket,
    #[error("failed to open encoder: FFmpeg error {0}")]
    OpenCodec(i32),
    #[error("failed to send frame to encoder: FFmpeg error {0}")]
    SendFrame(i32),
    #[error("failed to receive encoded packet: FFmpeg error {0}")]
    ReceivePacket(i32),
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

// ── Video error types ──────────────────────────────────────────────────

/// Errors produced by video decode, scale, and encode operations.
#[derive(Debug, Error)]
pub enum VideoError {
    /// Codec not found in FFmpeg's registry.
    #[error("video codec not found: {0}")]
    CodecNotFound(VideoCodec),

    /// Failed to allocate codec context.
    #[error("failed to allocate codec context")]
    AllocContext,

    /// Failed to open codec.
    #[error("failed to open codec: FFmpeg error {0}")]
    OpenCodec(i32),

    /// Failed to send packet to decoder.
    #[error("failed to send packet to decoder: FFmpeg error {0}")]
    SendPacket(i32),

    /// Failed to receive decoded frame.
    #[error("failed to receive decoded frame: FFmpeg error {0}")]
    ReceiveFrame(i32),

    /// No frame available (EAGAIN — need more input).
    #[error("no frame available yet (need more input data)")]
    NeedMoreInput,

    /// End of stream reached.
    #[error("end of stream")]
    Eof,

    /// Failed to allocate scaler context.
    #[error("failed to allocate scaler context")]
    AllocScaler,

    /// Failed to allocate frame.
    #[error("failed to allocate frame")]
    AllocFrame,

    /// Failed to allocate frame buffer.
    #[error("failed to allocate frame buffer: FFmpeg error {0}")]
    AllocFrameBuffer(i32),

    /// Failed to encode JPEG.
    #[error("failed to encode JPEG: FFmpeg error {0}")]
    JpegEncode(i32),

    /// No keyframe found in input data.
    #[error("no keyframe (IDR) found in input data")]
    NoKeyframe,

    /// Input data is empty or invalid.
    #[error("empty or invalid input data")]
    EmptyInput,

    /// Failed to allocate packet.
    #[error("failed to allocate packet")]
    AllocPacket,
}
