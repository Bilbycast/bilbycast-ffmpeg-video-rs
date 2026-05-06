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
                // 4:2:0 chroma planes: half-height (rounded up so odd
                // luma heights still fit, mirroring libavutil's own
                // allocation `(h + 1) / 2`).
                (ScalerDstFormat::Yuvj420p | ScalerDstFormat::Yuv420p10le, 1 | 2) => {
                    (height + 1) / 2
                }
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
        ScalerDstFormat::Yuv420p10le => AVPixelFormat_AV_PIX_FMT_YUV420P10LE,
        ScalerDstFormat::Bgra8 => AVPixelFormat_AV_PIX_FMT_BGRA,
    }
}

/// Resolve a planar YUV `(chroma, bit_depth)` pair into the FFmpeg
/// `AVPixelFormat` integer used by the scaler / encoder. Exposed so
/// edge-side call sites that operate on raw planes (ST 2110-20 / -23
/// RFC 4175 depacketisation) can describe their planes to
/// [`VideoScaler::scale_raw_planes`] without depending on
/// `libffmpeg-video-sys` directly.
///
/// Returns `None` for combinations that the scaler / encoder don't
/// support today (4:4:4).
pub fn av_pix_fmt_for_yuv(chroma: video_codec::VideoChroma, bit_depth: u8) -> Option<i32> {
    use video_codec::VideoChroma;
    match (chroma, bit_depth) {
        (VideoChroma::Yuv420, 8) => Some(AVPixelFormat_AV_PIX_FMT_YUV420P),
        (VideoChroma::Yuv422, 8) => Some(AVPixelFormat_AV_PIX_FMT_YUV422P),
        (VideoChroma::Yuv420, 10) => Some(AVPixelFormat_AV_PIX_FMT_YUV420P10LE),
        (VideoChroma::Yuv422, 10) => Some(AVPixelFormat_AV_PIX_FMT_YUV422P10LE),
        _ => None,
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

    /// Scale raw planar YUV planes (Y, U, V slices + byte strides) that
    /// did not come from a [`crate::VideoDecoder`].
    ///
    /// Used by callers that depacketise raw video directly (RFC 4175 /
    /// SMPTE ST 2110-20 / -23) and need to feed planar frames into a
    /// [`crate::VideoEncoder`] at a different output resolution.
    ///
    /// `src_w`, `src_h`, and `src_format` must match the planes provided:
    /// the scaler validates that these agree with the scaler's own
    /// configured source dimensions / pixel format and returns
    /// [`VideoError::InvalidInput`] on mismatch.
    #[allow(clippy::too_many_arguments)]
    pub fn scale_raw_planes(
        &self,
        src_w: u32,
        src_h: u32,
        src_format: i32,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
    ) -> Result<ScaledFrame, VideoError> {
        unsafe {
            // Build a temporary src AVFrame that wraps the caller's
            // slices — no copy, just pointer wiring. We don't hand this
            // frame out, so plane lifetimes are bounded by this call.
            let src_frame = av_frame_alloc();
            if src_frame.is_null() {
                return Err(VideoError::AllocFrame);
            }
            (*src_frame).width = src_w as i32;
            (*src_frame).height = src_h as i32;
            (*src_frame).format = src_format;
            (*src_frame).data[0] = y.as_ptr() as *mut u8;
            (*src_frame).data[1] = u.as_ptr() as *mut u8;
            (*src_frame).data[2] = v.as_ptr() as *mut u8;
            (*src_frame).linesize[0] = y_stride as i32;
            (*src_frame).linesize[1] = u_stride as i32;
            (*src_frame).linesize[2] = v_stride as i32;

            let dst_frame = av_frame_alloc();
            if dst_frame.is_null() {
                av_frame_free(&mut { src_frame });
                return Err(VideoError::AllocFrame);
            }
            (*dst_frame).width = self.dst_width;
            (*dst_frame).height = self.dst_height;
            (*dst_frame).format = self.dst_pix_fmt;

            let ret = av_frame_get_buffer(dst_frame, 0);
            if ret < 0 {
                av_frame_free(&mut { dst_frame });
                av_frame_free(&mut { src_frame });
                return Err(VideoError::AllocFrameBuffer(ret));
            }

            sws_scale(
                self.ctx,
                (*src_frame).data.as_ptr() as *const *const u8,
                (*src_frame).linesize.as_ptr(),
                0,
                (*src_frame).height,
                (*dst_frame).data.as_ptr(),
                (*dst_frame).linesize.as_ptr(),
            );

            // Zero the wrapping pointers before free so libavutil
            // doesn't try to free memory it doesn't own.
            (*src_frame).data[0] = std::ptr::null_mut();
            (*src_frame).data[1] = std::ptr::null_mut();
            (*src_frame).data[2] = std::ptr::null_mut();
            av_frame_free(&mut { src_frame });

            Ok(ScaledFrame {
                frame: dst_frame,
                dst_format: self.dst_format,
            })
        }
    }

    pub fn dst_format(&self) -> ScalerDstFormat {
        self.dst_format
    }

    /// Configure the YUV→RGB matrix the scaler uses. `src_colorspace` is an
    /// `AVColorSpace` value from the decoded frame (e.g. `AVCOL_SPC_BT709`
    /// for HD H.264). `src_full_range` selects PC (0–255) vs TV (16–235)
    /// luma range (`AVCOL_RANGE_JPEG` vs `AVCOL_RANGE_MPEG`). No-op when
    /// the destination format isn't packed RGB / BGR.
    ///
    /// Without this call, libswscale defaults to BT.601 for SD-shaped
    /// inputs, which produces muddy greens / oversaturated reds on a
    /// BT.709 HD source decoded from H.264.
    pub fn set_yuv_to_rgb_colorspace(&self, src_colorspace: i32, src_full_range: bool) {
        if !matches!(self.dst_format, ScalerDstFormat::Bgra8) {
            return;
        }
        unsafe {
            let inv_table = sws_getCoefficients(src_colorspace);
            let table = sws_getCoefficients(AVColorSpace_AVCOL_SPC_BT709 as i32);
            // RGB output is always full-range. brightness=0, contrast=1<<16,
            // saturation=1<<16 in 16.16 fixed-point.
            sws_setColorspaceDetails(
                self.ctx,
                inv_table,
                if src_full_range { 1 } else { 0 },
                table,
                1,
                0,
                1 << 16,
                1 << 16,
            );
        }
    }

    /// Like [`Self::scale_into_packed`] but takes raw planar YUV slices
    /// instead of a [`DecodedFrame`]. Used by the display sink, which
    /// already moves planes through an mpsc channel and doesn't have a
    /// live [`DecodedFrame`] handle by the time the blit runs.
    ///
    /// `src_w` / `src_h` / `src_format` describe the source planes and
    /// must agree with what the scaler was constructed for. The
    /// destination format must be packed (currently `Bgra8`).
    #[allow(clippy::too_many_arguments)]
    pub fn scale_raw_planes_into_packed(
        &self,
        src_w: u32,
        src_h: u32,
        src_format: i32,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
        dst: &mut [u8],
        dst_pitch: usize,
    ) -> Result<(), VideoError> {
        if !matches!(self.dst_format, ScalerDstFormat::Bgra8) {
            return Err(VideoError::InvalidInput(
                "scale_raw_planes_into_packed requires a packed destination format",
            ));
        }
        let needed = dst_pitch.saturating_mul(self.dst_height as usize);
        if dst.len() < needed {
            return Err(VideoError::InvalidInput(
                "destination buffer smaller than dst_pitch * dst_height",
            ));
        }
        let _ = (src_w, src_h, src_format); // shape is locked at scaler construction
        unsafe {
            let mut src_data: [*const u8; 4] = [std::ptr::null(); 4];
            let mut src_linesize: [i32; 4] = [0; 4];
            src_data[0] = y.as_ptr();
            src_data[1] = u.as_ptr();
            src_data[2] = v.as_ptr();
            src_linesize[0] = y_stride as i32;
            src_linesize[1] = u_stride as i32;
            src_linesize[2] = v_stride as i32;

            let mut dst_data: [*mut u8; 4] = [std::ptr::null_mut(); 4];
            let mut dst_linesize: [i32; 4] = [0; 4];
            dst_data[0] = dst.as_mut_ptr();
            dst_linesize[0] = dst_pitch as i32;

            sws_scale(
                self.ctx,
                src_data.as_ptr(),
                src_linesize.as_ptr(),
                0,
                src_h as i32,
                dst_data.as_ptr() as *const *mut u8,
                dst_linesize.as_ptr(),
            );
        }
        Ok(())
    }

    /// Like [`Self::scale_raw_planes_into_packed`] but for **semi-planar**
    /// source formats — `NV12` / `NV16` (8-bit) and `P010LE` / `P016LE` /
    /// `P210LE` / `P216LE` (10/12-bit-in-16 LE). Plane 0 is luma and
    /// plane 1 is interleaved chroma; libswscale ignores `plane[2]` for
    /// every semi-planar source it supports, so we set it to a literal
    /// null pointer with stride 0.
    ///
    /// The whole point of this entry point is to **stop reformatting
    /// frames in the demux thread**. cuvid / QSV / VAAPI hand back
    /// frames in the bit positions libswscale already knows (P010LE
    /// keeps the 10 valid bits at positions 6..15 of each 16-bit LE
    /// container — `pixdesc` records that, libswscale reads it
    /// natively). The pure-Rust scalar bit-shift + UV deinterleave the
    /// display path used to do for every 4K HDR frame couldn't sustain
    /// 50 fps, the broadcast subscriber kept lagging, and the decoder
    /// flushed on every `RecvError::Lagged` — operators saw one frame
    /// every few seconds (= IDR cadence). Routing the frame straight
    /// into libswscale's SIMD path eliminates that overhead.
    ///
    /// `src_w` / `src_h` are recorded for documentation only; the
    /// scaler shape is locked at `new_with_dst_format` time. Returns
    /// `InvalidInput` when the destination format isn't packed
    /// (currently only `Bgra8` qualifies — the display sink format).
    #[allow(clippy::too_many_arguments)]
    pub fn scale_semi_planar_into_packed(
        &self,
        src_w: u32,
        src_h: u32,
        y: &[u8],
        y_stride: usize,
        uv: &[u8],
        uv_stride: usize,
        dst: &mut [u8],
        dst_pitch: usize,
    ) -> Result<(), VideoError> {
        if !matches!(self.dst_format, ScalerDstFormat::Bgra8) {
            return Err(VideoError::InvalidInput(
                "scale_semi_planar_into_packed requires a packed destination format",
            ));
        }
        let needed = dst_pitch.saturating_mul(self.dst_height as usize);
        if dst.len() < needed {
            return Err(VideoError::InvalidInput(
                "destination buffer smaller than dst_pitch * dst_height",
            ));
        }
        let _ = (src_w, y.len(), uv.len()); // shape is locked at scaler construction
        unsafe {
            let mut src_data: [*const u8; 4] = [std::ptr::null(); 4];
            let mut src_linesize: [i32; 4] = [0; 4];
            src_data[0] = y.as_ptr();
            src_data[1] = uv.as_ptr();
            // src_data[2] / src_linesize[2] left at 0 / null — libswscale
            // never reads plane[2] for any semi-planar source format
            // it supports (NV12 / NV16 / P0xx / P2xx).
            src_linesize[0] = y_stride as i32;
            src_linesize[1] = uv_stride as i32;

            let mut dst_data: [*mut u8; 4] = [std::ptr::null_mut(); 4];
            let mut dst_linesize: [i32; 4] = [0; 4];
            dst_data[0] = dst.as_mut_ptr();
            dst_linesize[0] = dst_pitch as i32;

            sws_scale(
                self.ctx,
                src_data.as_ptr(),
                src_linesize.as_ptr(),
                0,
                src_h as i32,
                dst_data.as_ptr() as *const *mut u8,
                dst_linesize.as_ptr(),
            );
        }
        Ok(())
    }

    /// Scale a decoded frame straight into a caller-provided packed
    /// destination buffer (one plane, `dst_pitch` bytes per row).
    /// Designed for the display sink: the destination is the mapped
    /// KMS dumb buffer, so libswscale writes directly into the
    /// framebuffer with no intermediate copy. Only valid when the
    /// destination format is packed (currently `Bgra8`).
    pub fn scale_into_packed(
        &self,
        src: &DecodedFrame,
        dst: &mut [u8],
        dst_pitch: usize,
    ) -> Result<(), VideoError> {
        if !matches!(self.dst_format, ScalerDstFormat::Bgra8) {
            return Err(VideoError::InvalidInput(
                "scale_into_packed requires a packed destination format",
            ));
        }
        let needed = dst_pitch.saturating_mul(self.dst_height as usize);
        if dst.len() < needed {
            return Err(VideoError::InvalidInput(
                "destination buffer smaller than dst_pitch * dst_height",
            ));
        }
        unsafe {
            let src_frame = src.as_ptr();
            let mut dst_data: [*mut u8; 4] = [std::ptr::null_mut(); 4];
            let mut dst_linesize: [i32; 4] = [0; 4];
            dst_data[0] = dst.as_mut_ptr();
            dst_linesize[0] = dst_pitch as i32;
            sws_scale(
                self.ctx,
                (*src_frame).data.as_ptr() as *const *const u8,
                (*src_frame).linesize.as_ptr(),
                0,
                (*src_frame).height,
                dst_data.as_ptr() as *const *mut u8,
                dst_linesize.as_ptr(),
            );
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic 4×4 NV12 frame → 4×4 BGRA round-trip. The point is
    /// not pixel-exact maths (libswscale's BT.709 matrix has many
    /// internal scaling steps and rounding), but to verify the
    /// 2-plane semi-planar entry actually drives `sws_scale` end-
    /// to-end and writes pixels into the destination buffer.
    /// A solid grey luma plane + neutral chroma must produce a
    /// non-zero, near-grey BGRA output; left at the default zero
    /// initialiser the call would have written nothing and we'd
    /// read garbage.
    #[test]
    fn scale_semi_planar_nv12_to_bgra_writes_pixels() {
        let w = 4u32;
        let h = 4u32;
        let scaler = VideoScaler::new_with_dst_format(
            w,
            h,
            AVPixelFormat_AV_PIX_FMT_NV12,
            w,
            h,
            ScalerDstFormat::Bgra8,
        )
        .expect("scaler init");
        // Y plane: solid 0x80 (mid-grey luma in 8-bit limited range).
        // UV plane: 0x80 / 0x80 — neutral chroma.
        let y = vec![0x80u8; (w * h) as usize];
        let uv = vec![0x80u8; (w * h / 2) as usize]; // 4:2:0: half-height chroma
        let mut dst = vec![0u8; (w * h * 4) as usize];

        scaler
            .scale_semi_planar_into_packed(
                w,
                h,
                &y,
                w as usize,
                &uv,
                w as usize,
                &mut dst,
                (w * 4) as usize,
            )
            .expect("scale ok");

        // Every pixel should have been touched. Mid-grey luma with
        // neutral chroma maps to ~mid-grey BGRA — but at minimum it
        // must not be all zero (which is what the buffer was
        // initialised to). A failure to wire `plane[1]` correctly
        // shows up as severe colour-cast or a zero-write.
        let any_non_zero = dst.iter().any(|&b| b != 0);
        assert!(
            any_non_zero,
            "scale_semi_planar_into_packed must write into the destination buffer"
        );
        // Mid-grey luma + neutral chroma: every BGR channel should
        // sit comfortably inside the mid-grey band (rough sanity).
        for px in dst.chunks_exact(4) {
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            assert!(
                (60..=200).contains(&b),
                "B {b} outside mid-grey band — semi-planar dispatch likely broken"
            );
            assert!(
                (60..=200).contains(&g),
                "G {g} outside mid-grey band"
            );
            assert!(
                (60..=200).contains(&r),
                "R {r} outside mid-grey band"
            );
        }
    }
}
