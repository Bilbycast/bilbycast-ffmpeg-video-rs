// Copyright (c) 2026 Reza Rahimi / Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe video scaler wrapping FFmpeg's libswscale.
//!
//! Converts between pixel formats and rescales video frames. Used to
//! resize decoded frames to thumbnail dimensions before JPEG encoding.

use libffmpeg_video_sys::*;
use video_codec::VideoError;

use crate::decoder::DecodedFrame;

/// A scaled video frame ready for encoding.
///
/// Owns its pixel data buffer. Always in YUVJ420P pixel format
/// (full-range YUV 4:2:0, compatible with the MJPEG encoder).
pub struct ScaledFrame {
    pub(crate) frame: *mut AVFrame,
}

impl ScaledFrame {
    pub fn width(&self) -> u32 {
        unsafe { (*self.frame).width as u32 }
    }

    pub fn height(&self) -> u32 {
        unsafe { (*self.frame).height as u32 }
    }

    pub(crate) fn as_ptr(&self) -> *const AVFrame {
        self.frame
    }
}

impl Drop for ScaledFrame {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.frame);
        }
    }
}

// SAFETY: ScaledFrame owns its AVFrame and buffer. No shared state.
unsafe impl Send for ScaledFrame {}

/// Safe video scaler.
///
/// Wraps FFmpeg's `SwsContext` for image scaling and pixel format conversion.
/// Configured for a specific input→output transformation on creation. Reusable
/// across frames with the same input dimensions and format.
pub struct VideoScaler {
    ctx: *mut SwsContext,
    dst_width: i32,
    dst_height: i32,
}

// SAFETY: SwsContext is per-instance with no shared global state.
unsafe impl Send for VideoScaler {}

impl VideoScaler {
    /// Create a scaler from the given input format to YUVJ420P at the target
    /// dimensions. Uses Lanczos scaling for high-quality thumbnails.
    ///
    /// The `src_format` is the FFmpeg `AVPixelFormat` value from the decoded
    /// frame (e.g., `AV_PIX_FMT_YUV420P`).
    pub fn new(
        src_width: u32,
        src_height: u32,
        src_format: i32,
        dst_width: u32,
        dst_height: u32,
    ) -> Result<Self, VideoError> {
        unsafe {
            let ctx = sws_getContext(
                src_width as i32,
                src_height as i32,
                src_format,
                dst_width as i32,
                dst_height as i32,
                AVPixelFormat_AV_PIX_FMT_YUVJ420P, // Full-range YUV for MJPEG
                SWS_LANCZOS as i32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );

            if ctx.is_null() {
                return Err(VideoError::AllocScaler);
            }

            Ok(Self {
                ctx,
                dst_width: dst_width as i32,
                dst_height: dst_height as i32,
            })
        }
    }

    /// Scale a decoded frame to the configured output dimensions.
    pub fn scale(&self, src: &DecodedFrame) -> Result<ScaledFrame, VideoError> {
        unsafe {
            // Allocate destination frame
            let dst_frame = av_frame_alloc();
            if dst_frame.is_null() {
                return Err(VideoError::AllocFrame);
            }

            (*dst_frame).width = self.dst_width;
            (*dst_frame).height = self.dst_height;
            (*dst_frame).format = AVPixelFormat_AV_PIX_FMT_YUVJ420P;

            let ret = av_frame_get_buffer(dst_frame, 0);
            if ret < 0 {
                av_frame_free(&mut { dst_frame });
                return Err(VideoError::AllocFrameBuffer(ret));
            }

            let src_frame = src.as_ptr();

            sws_scale(
                self.ctx,
                (*src_frame).data.as_ptr() as *const *const u8,
                (*src_frame).linesize.as_ptr(),
                0,
                (*src_frame).height,
                (*dst_frame).data.as_ptr(),
                (*dst_frame).linesize.as_ptr(),
            );

            Ok(ScaledFrame { frame: dst_frame })
        }
    }
}

impl Drop for VideoScaler {
    fn drop(&mut self) {
        unsafe {
            sws_freeContext(self.ctx);
        }
    }
}

impl std::fmt::Debug for VideoScaler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoScaler")
            .field("dst_width", &self.dst_width)
            .field("dst_height", &self.dst_height)
            .finish_non_exhaustive()
    }
}
