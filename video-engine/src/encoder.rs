// Copyright (c) 2026 Reza Rahimi / Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe JPEG encoder wrapping FFmpeg's MJPEG encoder.
//!
//! Encodes a single video frame as a JPEG image. Used for thumbnail
//! generation after decoding and scaling.

use bytes::Bytes;
use libffmpeg_video_sys::*;
use video_codec::VideoError;

use crate::scaler::ScaledFrame;

/// Safe JPEG encoder.
///
/// Wraps FFmpeg's MJPEG encoder for single-frame JPEG encoding. Creates
/// a fresh encoder context per frame for simplicity (acceptable at the
/// 10-second thumbnail cadence).
pub struct JpegEncoder {
    /// JPEG quality (1-31 in FFmpeg's scale, lower = better quality).
    quality: i32,
}

// SAFETY: No internal state beyond the quality setting.
unsafe impl Send for JpegEncoder {}

impl JpegEncoder {
    /// Create a JPEG encoder with the given quality.
    ///
    /// Quality is on FFmpeg's scale: 1 (best) to 31 (worst). Default is 5.
    pub fn new(quality: u32) -> Self {
        Self {
            quality: quality.clamp(1, 31) as i32,
        }
    }

    /// Encode a scaled frame as JPEG.
    ///
    /// Returns the JPEG bytes. The frame must be in YUVJ420P format
    /// (as produced by [`VideoScaler`]).
    pub fn encode(&self, frame: &ScaledFrame) -> Result<Bytes, VideoError> {
        unsafe {
            let codec = avcodec_find_encoder(AVCodecID_AV_CODEC_ID_MJPEG);
            if codec.is_null() {
                return Err(VideoError::CodecNotFound(video_codec::VideoCodec::H264));
            }

            let ctx = avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(VideoError::AllocContext);
            }

            // Configure the encoder
            let src_frame = frame.as_ptr();
            (*ctx).width = (*src_frame).width;
            (*ctx).height = (*src_frame).height;
            (*ctx).pix_fmt = AVPixelFormat_AV_PIX_FMT_YUVJ420P;
            (*ctx).time_base.num = 1;
            (*ctx).time_base.den = 1;
            // Set quality via global_quality (qscale * FF_QP2LAMBDA)
            (*ctx).flags |= AV_CODEC_FLAG_QSCALE as i32;
            (*ctx).global_quality = self.quality * 118; // FF_QP2LAMBDA ≈ 118

            let ret = avcodec_open2(ctx, codec, std::ptr::null_mut());
            if ret < 0 {
                avcodec_free_context(&mut { ctx });
                return Err(VideoError::OpenCodec(ret));
            }

            // Send frame to encoder
            let ret = avcodec_send_frame(ctx, src_frame);
            if ret < 0 {
                avcodec_free_context(&mut { ctx });
                return Err(VideoError::JpegEncode(ret));
            }

            // Receive encoded packet
            let pkt = av_packet_alloc();
            if pkt.is_null() {
                avcodec_free_context(&mut { ctx });
                return Err(VideoError::AllocPacket);
            }

            let ret = avcodec_receive_packet(ctx, pkt);
            if ret < 0 {
                av_packet_free(&mut { pkt });
                avcodec_free_context(&mut { ctx });
                return Err(VideoError::JpegEncode(ret));
            }

            // Copy the JPEG data
            let jpeg_data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
            let result = Bytes::copy_from_slice(jpeg_data);

            av_packet_free(&mut { pkt });
            avcodec_free_context(&mut { ctx });

            Ok(result)
        }
    }
}

impl Default for JpegEncoder {
    fn default() -> Self {
        Self::new(5)
    }
}

impl std::fmt::Debug for JpegEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JpegEncoder")
            .field("quality", &self.quality)
            .finish()
    }
}
