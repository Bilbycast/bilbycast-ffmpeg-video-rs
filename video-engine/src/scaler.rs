// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe video scaler wrapping FFmpeg's libswscale.
//!
//! Converts between pixel formats and rescales video frames. Used to
//! resize decoded frames to thumbnail dimensions before JPEG encoding.

use libffmpeg_video_sys::*;
use video_codec::{ScalerDstFormat, VideoError};

use crate::decoder::DecodedFrame;

/// A scaled video frame ready for encoding or packetization.
///
/// Owns its pixel data buffer. Pixel format depends on the scaler's
/// destination format: YUVJ420P (legacy thumbnail path) or a planar
/// broadcast format (4:2:2 8-bit or 10-bit LE, for RFC 4175 packetizers).
pub struct ScaledFrame {
    pub(crate) frame: *mut AVFrame,
    pub(crate) dst_format: ScalerDstFormat,
}

impl ScaledFrame {
    pub fn width(&self) -> u32 {
        unsafe { (*self.frame).width as u32 }
    }

    pub fn height(&self) -> u32 {
        unsafe { (*self.frame).height as u32 }
    }

    pub fn dst_format(&self) -> ScalerDstFormat {
        self.dst_format
    }

    /// Returns `(data, linesize)` for plane index 0..=2 (Y, U, V).
    ///
    /// Length of `data` is `linesize * plane_height` where `plane_height`
    /// equals frame height for luma and (height for 4:2:2 / height/2 for 4:2:0)
    /// for chroma. Callers must consult `dst_format()` to know the layout.
    pub fn plane(&self, idx: usize) -> Option<(&[u8], usize)> {
        if idx > 2 {
            return None;
        }
        unsafe {
            let frame = &*self.frame;
            let linesize = frame.linesize[idx] as usize;
            if linesize == 0 {
                return None;
            }
            let data = frame.data[idx];
            if data.is_null() {
                return None;
            }
            let height = frame.height as usize;
            let plane_rows = match (self.dst_format, idx) {
                (ScalerDstFormat::Yuvj420p, 1 | 2) => height / 2,
                _ => height,
            };
            let slice = std::slice::from_raw_parts(data, linesize * plane_rows);
            Some((slice, linesize))
        }
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
    dst_format: ScalerDstFormat,
    dst_pix_fmt: i32,
}

// SAFETY: SwsContext is per-instance with no shared global state.
unsafe impl Send for VideoScaler {}

fn scaler_dst_pix_fmt(fmt: ScalerDstFormat) -> i32 {
    match fmt {
        ScalerDstFormat::Yuvj420p => AVPixelFormat_AV_PIX_FMT_YUVJ420P,
        ScalerDstFormat::Yuv422p8 => AVPixelFormat_AV_PIX_FMT_YUV422P,
        ScalerDstFormat::Yuv422p10le => AVPixelFormat_AV_PIX_FMT_YUV422P10LE,
    }
}

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
        Self::new_with_dst_format(
            src_width,
            src_height,
            src_format,
            dst_width,
            dst_height,
            ScalerDstFormat::Yuvj420p,
        )
    }

    /// Create a scaler that converts to an explicit destination pixel format.
    ///
    /// Used by RFC 4175 packetizers (ST 2110-20 / -23) which require planar
    /// 4:2:2 at 8-bit or 10-bit LE on the wire. Existing callers should keep
    /// using [`VideoScaler::new`] which defaults to `Yuvj420p`.
    pub fn new_with_dst_format(
        src_width: u32,
        src_height: u32,
        src_format: i32,
        dst_width: u32,
        dst_height: u32,
        dst_format: ScalerDstFormat,
    ) -> Result<Self, VideoError> {
        let dst_pix_fmt = scaler_dst_pix_fmt(dst_format);
        unsafe {
            let ctx = sws_getContext(
                src_width as i32,
                src_height as i32,
                src_format,
                dst_width as i32,
                dst_height as i32,
                dst_pix_fmt,
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
                dst_format,
                dst_pix_fmt,
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
            (*dst_frame).format = self.dst_pix_fmt;

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

            Ok(ScaledFrame {
                frame: dst_frame,
                dst_format: self.dst_format,
            })
        }
    }

    pub fn dst_format(&self) -> ScalerDstFormat {
        self.dst_format
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
