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

    // 2. Send all NAL data, then drain every decoded frame — we want the
    //    LATEST picture in the buffer, not just the first one. On a long-
    //    GOP source (e.g. an MP4 with a 10 s GOP) the buffer typically
    //    contains [IDR, P, P, …, P]; returning the IDR means the
    //    thumbnail freezes for the duration of one GOP and the
    //    upstream freeze detector falsely flags the stream. Walking
    //    forward to the last P-frame is what an operator actually
    //    wants to see and matches what every consumer-grade media
    //    player shows.
    decoder.send_packet(nalu_data)?;

    let mut latest: Option<crate::decoder::DecodedFrame> = None;
    loop {
        match decoder.receive_frame() {
            Ok(frame) => latest = Some(frame),
            Err(VideoError::NeedMoreInput) => break,
            Err(VideoError::Eof) => break,
            Err(e) => {
                // Mid-stream decode error: keep what we already have if
                // anything, otherwise propagate.
                if latest.is_none() {
                    return Err(e);
                } else {
                    break;
                }
            }
        }
    }

    // Flush to release any trailing frame still buffered inside
    // libavcodec (the trailing P-frame after the last full slice).
    if decoder.send_flush().is_ok() {
        loop {
            match decoder.receive_frame() {
                Ok(frame) => latest = Some(frame),
                Err(VideoError::Eof) | Err(VideoError::NeedMoreInput) => break,
                Err(_) => break,
            }
        }
    }

    let frame = latest.ok_or(VideoError::NeedMoreInput)?;

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

/// Decode a thumbnail from a per-access-unit packet sequence.
///
/// `decode_thumbnail` feeds the decoder one big concatenated Annex B blob.
/// That works for every input we tested historically because the codec's
/// internal NAL parser still walks the bytes correctly — but it falls
/// apart on open-GOP broadcast streams that use non-IDR I-slices with a
/// `recovery_point` SEI for random access (DVB-T 1080i25 is the canonical
/// case): without per-AU framing the decoder cannot match the SEI to the
/// slice that follows and never produces a picture. The mpegts demuxer
/// inside ffmpeg avoids this by feeding one PES per `avcodec_send_packet`,
/// and that's what this function does — the upstream `TsDemuxer` already
/// surfaces complete access units with their PTS.
///
/// `headers` is an optional Annex B blob containing decoder configuration
/// NALUs (H.264: SPS+PPS; HEVC: VPS+SPS+PPS). Sent first with no PTS so
/// the decoder has parameter sets in place before any VCL slice arrives.
/// `&[]` is fine when the parameter sets ride inline with the first AU.
///
/// `packets` is the access-unit sequence in decode order. Each `(au, pts)`
/// pair is sent as its own packet via `send_packet_with_pts`. PTS is in
/// 90 kHz ticks. After all packets the decoder is flushed and every
/// produced frame is consumed; the most recent one wins (matches what an
/// operator expects in the live preview).
///
/// Returns `Err(NeedMoreInput)` when the decoder produced no frame —
/// callers treat that as a soft failure and wait for more data.
///
/// Like [`decode_thumbnail`], this is synchronous FFmpeg work; wrap it in
/// `tokio::task::spawn_blocking` from async contexts.
pub fn decode_thumbnail_packets(
    headers: &[u8],
    packets: &[(Vec<u8>, i64)],
    codec: VideoCodec,
    config: &ThumbnailConfig,
) -> Result<ThumbnailResult, VideoError> {
    if packets.iter().all(|(au, _)| au.is_empty()) && headers.is_empty() {
        return Err(VideoError::EmptyInput);
    }

    let mut decoder = VideoDecoder::open(codec)?;
    let mut latest: Option<crate::decoder::DecodedFrame> = None;

    // Parameter sets first — non-VCL, no PTS, never produce a picture.
    // Errors are swallowed: a bad headers blob just leaves the decoder
    // uninitialised, and the first VCL packet retries parameter parsing
    // on its own (broadcast streams often re-send SPS/PPS before each I).
    if !headers.is_empty() {
        let _ = decoder.send_packet(headers);
        loop {
            match decoder.receive_frame() {
                Ok(frame) => latest = Some(frame),
                Err(VideoError::NeedMoreInput) | Err(VideoError::Eof) => break,
                Err(_) => break,
            }
        }
    }

    for (au, pts) in packets {
        if au.is_empty() {
            continue;
        }
        if decoder.send_packet_with_pts(au, *pts).is_err() {
            continue;
        }
        loop {
            match decoder.receive_frame() {
                Ok(frame) => latest = Some(frame),
                Err(VideoError::NeedMoreInput) | Err(VideoError::Eof) => break,
                Err(_) => break,
            }
        }
    }

    // Drain the reorder queue. Any B-frame waiting for a future reference
    // is released here, becoming the freshest decoded picture.
    if decoder.send_flush().is_ok() {
        loop {
            match decoder.receive_frame() {
                Ok(frame) => latest = Some(frame),
                Err(VideoError::Eof) | Err(VideoError::NeedMoreInput) => break,
                Err(_) => break,
            }
        }
    }

    let frame = latest.ok_or(VideoError::NeedMoreInput)?;

    let luminance = frame.average_luminance();
    let source_width = frame.width();
    let source_height = frame.height();

    let scaler = VideoScaler::new(
        source_width,
        source_height,
        frame.pixel_format(),
        config.width,
        config.height,
    )?;
    let scaled = scaler.scale(&frame)?;

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
