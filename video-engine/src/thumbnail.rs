// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! High-level thumbnail generation: NAL units → JPEG bytes.
//!
//! This module provides the end-to-end thumbnail pipeline used by
//! bilbycast-edge. Feed it Annex B video data (H.264 or HEVC NAL units
//! with start codes) and get back a scaled JPEG thumbnail.

use bytes::Bytes;
use video_codec::{ThumbnailConfig, VideoCodec, VideoError};

use crate::decoder::VideoDecoder;
use crate::encoder::JpegEncoder;
use crate::scaler::VideoScaler;

/// Result of thumbnail generation, including metadata for black-screen detection.
#[derive(Debug)]
pub struct ThumbnailResult {
    /// The JPEG thumbnail bytes.
    pub jpeg: Bytes,
    /// Average luminance of the decoded frame (0.0-255.0).
    /// Use this for black-screen detection (e.g., threshold < 16.0).
    pub luminance: f64,
    /// Width of the decoded source frame.
    pub source_width: u32,
    /// Height of the decoded source frame.
    pub source_height: u32,
}

/// Decode a video thumbnail from Annex B NAL unit data.
///
/// This is the main entry point for in-process thumbnail generation.
/// It performs the complete pipeline:
///
/// 1. Open a decoder for the specified codec
/// 2. Feed the NAL data and decode until a frame is produced
/// 3. Compute luminance from the Y plane (for black-screen detection)
/// 4. Scale the frame to the configured thumbnail dimensions
/// 5. Encode as JPEG
///
/// # Parameters
///
/// - `nalu_data`: Annex B encoded video data (with 0x00000001 start codes).
///   Should contain at least one IDR/keyframe for reliable single-frame decode.
/// - `codec`: The video codec (H.264 or HEVC).
/// - `config`: Thumbnail dimensions and quality settings.
///
/// # Threading
///
/// This function performs synchronous FFmpeg C calls. In an async context,
/// wrap it in `tokio::task::spawn_blocking`.
pub fn decode_thumbnail(
    nalu_data: &[u8],
    codec: VideoCodec,
    config: &ThumbnailConfig,
) -> Result<ThumbnailResult, VideoError> {
    if nalu_data.is_empty() {
        return Err(VideoError::EmptyInput);
    }

    // 1. Open decoder
    let mut decoder = VideoDecoder::open(codec)?;

    // 2. Send data and try to decode a frame
    decoder.send_packet(nalu_data)?;

    let frame = match decoder.receive_frame() {
        Ok(frame) => frame,
        Err(VideoError::NeedMoreInput) => {
            // Flush decoder to get any buffered frames
            decoder.send_flush()?;
            decoder.receive_frame()?
        }
        Err(e) => return Err(e),
    };

    // 3. Compute luminance before scaling (source resolution Y plane)
    let luminance = frame.average_luminance();

    let source_width = frame.width();
    let source_height = frame.height();

    // 4. Scale to thumbnail dimensions
    let scaler = VideoScaler::new(
        source_width,
        source_height,
        frame.pixel_format(),
        config.width,
        config.height,
    )?;
    let scaled = scaler.scale(&frame)?;

    // 5. Encode as JPEG
    let encoder = JpegEncoder::new(config.quality);
    let jpeg = encoder.encode(&scaled)?;

    Ok(ThumbnailResult {
        jpeg,
        luminance,
        source_width,
        source_height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        crate::silence_ffmpeg_logs();
    }

    #[test]
    fn empty_input_rejected() {
        init();
        let config = ThumbnailConfig::default();
        let result = decode_thumbnail(&[], VideoCodec::H264, &config);
        assert!(matches!(result, Err(VideoError::EmptyInput)));
    }

    #[test]
    fn garbage_input_fails_gracefully() {
        init();
        let config = ThumbnailConfig::default();
        let garbage = vec![0x00, 0x00, 0x00, 0x01, 0xFF, 0xAA, 0xBB, 0xCC];
        let result = decode_thumbnail(&garbage, VideoCodec::H264, &config);
        // Should fail but not crash/panic
        assert!(result.is_err());
    }

    #[test]
    fn thumbnail_config_defaults() {
        let config = ThumbnailConfig::default();
        assert_eq!(config.width, 320);
        assert_eq!(config.height, 180);
        assert_eq!(config.quality, 5);
    }
}
