// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe video decoder wrapping FFmpeg's `avcodec_*` API.
//!
//! Supports H.264 and HEVC decoding. Feed Annex B NAL unit data via
//! [`send_packet`] and retrieve decoded frames via [`receive_frame`].
//!
//! # Thread Safety
//!
//! `VideoDecoder` is `Send` but not `Sync`. Each instance owns its
//! `AVCodecContext` and internal buffers. Safe to move between threads
//! but requires `&mut self` for all decode operations.

use libffmpeg_video_sys::*;
use video_codec::{VideoCodec, VideoError};

/// AVERROR_EOF = -FFERRTAG('E','O','F',' ')
const AVERROR_EOF: i32 = -541478725;

/// Compute AVERROR(errno) — FFmpeg negates POSIX errnos on POSIX systems.
#[cfg(target_os = "macos")]
const AVERROR_EAGAIN: i32 = -35; // macOS EAGAIN = 35
#[cfg(not(target_os = "macos"))]
const AVERROR_EAGAIN: i32 = -11; // Linux EAGAIN = 11

/// A decoded video frame. Wraps an `AVFrame` with accessor methods.
///
/// The frame data is owned by the decoder's internal reference-counted
/// buffers. The frame is valid until `av_frame_unref` is called.
pub struct DecodedFrame {
    frame: *mut AVFrame,
}

impl DecodedFrame {
    /// Width in pixels.
    pub fn width(&self) -> u32 {
        unsafe { (*self.frame).width as u32 }
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        unsafe { (*self.frame).height as u32 }
    }

    /// Pixel format (FFmpeg enum value).
    pub fn pixel_format(&self) -> i32 {
        unsafe { (*self.frame).format }
    }

    /// `AVColorSpace` (YUV→RGB matrix selector). Returns the FFmpeg
    /// `AVCOL_SPC_*` integer drawn from the decoded frame's VUI, or
    /// `AVCOL_SPC_UNSPECIFIED` when the bitstream did not signal it —
    /// callers must fall back to a sensible default per source size.
    pub fn colorspace(&self) -> i32 {
        unsafe { (*self.frame).colorspace as i32 }
    }

    /// `true` when the source is full-range (`AVCOL_RANGE_JPEG`); `false`
    /// when limited-range (`AVCOL_RANGE_MPEG` or unspecified, the broadcast
    /// default).
    pub fn is_full_range(&self) -> bool {
        unsafe { (*self.frame).color_range == 2 }
    }

    /// Whether this frame is a keyframe.
    pub fn is_keyframe(&self) -> bool {
        unsafe { (*self.frame).key_frame != 0 }
    }

    /// Raw pointer to the underlying AVFrame.
    ///
    /// # Safety
    /// The caller must not free or unref the frame.
    pub(crate) fn as_ptr(&self) -> *const AVFrame {
        self.frame
    }

    /// Access all three planes of a planar YUV frame at once.
    ///
    /// Returns `Some((y, y_stride, u, u_stride, v, v_stride))` when the
    /// pixel format is one of the planar YUV 4:2:0 / 4:2:2 / 4:4:4
    /// variants (including the JPEG full-range siblings and the 10-bit
    /// `*P10LE` variants). The chroma plane lengths reflect the format's
    /// vertical sub-sampling — half-height for 4:2:0, full-height for
    /// 4:2:2 / 4:4:4 — so callers can safely `.to_vec()` or otherwise
    /// touch every byte without reading past the end of FFmpeg's
    /// per-plane allocation.
    ///
    /// **Memory safety**: an earlier revision returned chroma slices
    /// of length `stride * full_height` for every layout, which made
    /// 4:2:0 callers segfault when the over-sized slice was copied
    /// (the over-read crossed the chroma plane's allocated end). The
    /// segfault was reliably triggered on 3840x2160 yuv420p10le HEVC
    /// frames in the local-display output's `drain_video_frames`. Fix:
    /// dispatch off `frame.format` to compute the exact chroma height.
    ///
    /// Returns `None` for non-planar formats and for any format we
    /// haven't taught the chroma-height table about — better to surface
    /// "format unsupported here" than to over-read silently.
    pub fn yuv_planes(&self) -> Option<(&[u8], usize, &[u8], usize, &[u8], usize)> {
        unsafe {
            let frame = &*self.frame;
            let y_ptr = frame.data[0];
            let u_ptr = frame.data[1];
            let v_ptr = frame.data[2];
            if y_ptr.is_null() || u_ptr.is_null() || v_ptr.is_null() {
                return None;
            }
            let y_stride = frame.linesize[0] as usize;
            let u_stride = frame.linesize[1] as usize;
            let v_stride = frame.linesize[2] as usize;
            let h = frame.height as usize;
            // Vertical sub-sampling factor: 2 for 4:2:0, 1 for 4:2:2 /
            // 4:4:4. Round up so odd-height frames still fit (matches
            // FFmpeg's own allocation: `(h + 1) / 2`).
            let chroma_v_shift = match frame.format {
                f if f == AVPixelFormat_AV_PIX_FMT_YUV420P
                    || f == AVPixelFormat_AV_PIX_FMT_YUVJ420P
                    || f == AVPixelFormat_AV_PIX_FMT_YUV420P10LE
                    || f == AVPixelFormat_AV_PIX_FMT_YUV420P12LE => 1,
                f if f == AVPixelFormat_AV_PIX_FMT_YUV422P
                    || f == AVPixelFormat_AV_PIX_FMT_YUVJ422P
                    || f == AVPixelFormat_AV_PIX_FMT_YUV422P10LE
                    || f == AVPixelFormat_AV_PIX_FMT_YUV422P12LE
                    || f == AVPixelFormat_AV_PIX_FMT_YUV444P
                    || f == AVPixelFormat_AV_PIX_FMT_YUVJ444P
                    || f == AVPixelFormat_AV_PIX_FMT_YUV444P10LE
                    || f == AVPixelFormat_AV_PIX_FMT_YUV444P12LE => 0,
                _ => return None,
            };
            let chroma_rows = (h + (1 << chroma_v_shift) - 1) >> chroma_v_shift;
            Some((
                std::slice::from_raw_parts(y_ptr, y_stride * h),
                y_stride,
                std::slice::from_raw_parts(u_ptr, u_stride * chroma_rows),
                u_stride,
                std::slice::from_raw_parts(v_ptr, v_stride * chroma_rows),
                v_stride,
            ))
        }
    }

    /// Access the Y (luma) plane data for black-screen detection.
    ///
    /// Returns the Y plane bytes and the line stride. For planar YUV formats,
    /// plane 0 is always luma. The stride may be larger than `width` due to
    /// alignment padding.
    pub fn y_plane(&self) -> Option<(&[u8], usize)> {
        unsafe {
            let frame = &*self.frame;
            let data_ptr = frame.data[0];
            if data_ptr.is_null() {
                return None;
            }
            let stride = frame.linesize[0] as usize;
            let height = frame.height as usize;
            Some((
                std::slice::from_raw_parts(data_ptr, stride * height),
                stride,
            ))
        }
    }

    /// Compute average luminance from the Y plane.
    ///
    /// Subsamples every 8th pixel for speed. Returns 0.0-255.0.
    pub fn average_luminance(&self) -> f64 {
        let Some((y_data, stride)) = self.y_plane() else {
            return 0.0;
        };
        let width = self.width() as usize;
        let height = self.height() as usize;

        let mut sum: u64 = 0;
        let mut count: u64 = 0;

        for row in 0..height {
            let row_start = row * stride;
            // Sample every 8th pixel in each row
            let mut col = 0;
            while col < width {
                sum += y_data[row_start + col] as u64;
                count += 1;
                col += 8;
            }
        }

        if count == 0 {
            0.0
        } else {
            sum as f64 / count as f64
        }
    }
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.frame);
        }
    }
}

/// Safe video decoder.
///
/// Wraps FFmpeg's `AVCodecContext` for H.264 or HEVC decoding. Each instance
/// is independent (no global state). Not `Sync` — requires `&mut self`.
pub struct VideoDecoder {
    ctx: *mut AVCodecContext,
    packet: *mut AVPacket,
    codec: VideoCodec,
}

// SAFETY: AVCodecContext is per-instance with no shared global state.
// Each context owns its internal buffers. Safe to move between threads.
unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    /// Open a decoder for the specified video codec.
    pub fn open(codec: VideoCodec) -> Result<Self, VideoError> {
        let codec_id = match codec {
            VideoCodec::H264 => AVCodecID_AV_CODEC_ID_H264,
            VideoCodec::Hevc => AVCodecID_AV_CODEC_ID_HEVC,
        };

        unsafe {
            let av_codec = avcodec_find_decoder(codec_id);
            if av_codec.is_null() {
                return Err(VideoError::CodecNotFound(codec));
            }

            let ctx = avcodec_alloc_context3(av_codec);
            if ctx.is_null() {
                return Err(VideoError::AllocContext);
            }

            // Allow truncated packets (common in TS streams)
            (*ctx).flags2 |= 1 << 1; // AV_CODEC_FLAG2_CHUNKS

            let ret = avcodec_open2(ctx, av_codec, std::ptr::null_mut());
            if ret < 0 {
                avcodec_free_context(&mut { ctx });
                return Err(VideoError::OpenCodec(ret));
            }

            let packet = av_packet_alloc();
            if packet.is_null() {
                avcodec_free_context(&mut { ctx });
                return Err(VideoError::AllocPacket);
            }

            Ok(Self {
                ctx,
                packet,
                codec,
            })
        }
    }

    /// Send a packet of compressed video data to the decoder.
    ///
    /// `data` should contain one or more NAL units in Annex B format
    /// (with 0x00000001 start codes) or as raw NAL unit data.
    ///
    /// After sending, call [`receive_frame`] to retrieve decoded frames.
    pub fn send_packet(&mut self, data: &[u8]) -> Result<(), VideoError> {
        if data.is_empty() {
            return Err(VideoError::EmptyInput);
        }

        unsafe {
            (*self.packet).data = data.as_ptr() as *mut u8;
            (*self.packet).size = data.len() as i32;

            let ret = avcodec_send_packet(self.ctx, self.packet);
            if ret < 0 {
                return Err(VideoError::SendPacket(ret));
            }
        }

        Ok(())
    }

    /// Flush the decoder (signal end of stream).
    pub fn send_flush(&mut self) -> Result<(), VideoError> {
        unsafe {
            let ret = avcodec_send_packet(self.ctx, std::ptr::null());
            if ret < 0 {
                return Err(VideoError::SendPacket(ret));
            }
        }
        Ok(())
    }

    /// Receive a decoded frame from the decoder.
    ///
    /// Returns `Err(VideoError::NeedMoreInput)` if the decoder needs more
    /// packets before it can produce a frame. Returns `Err(VideoError::Eof)`
    /// when the stream has been fully drained after a flush.
    pub fn receive_frame(&mut self) -> Result<DecodedFrame, VideoError> {
        unsafe {
            let frame = av_frame_alloc();
            if frame.is_null() {
                return Err(VideoError::AllocFrame);
            }

            let ret = avcodec_receive_frame(self.ctx, frame);
            if ret < 0 {
                av_frame_free(&mut { frame });
                if ret == AVERROR_EAGAIN {
                    return Err(VideoError::NeedMoreInput);
                }
                if ret == AVERROR_EOF {
                    return Err(VideoError::Eof);
                }
                return Err(VideoError::ReceiveFrame(ret));
            }

            Ok(DecodedFrame { frame })
        }
    }

    /// Reset the decoder state. Use after seeking or stream discontinuity.
    pub fn flush(&mut self) {
        unsafe {
            avcodec_flush_buffers(self.ctx);
        }
    }

    /// The codec this decoder was opened for.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe {
            av_packet_free(&mut self.packet);
            avcodec_free_context(&mut self.ctx);
        }
    }
}

impl std::fmt::Debug for VideoDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoDecoder")
            .field("codec", &self.codec)
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
    fn open_close_h264() {
        init();
        let _dec = VideoDecoder::open(VideoCodec::H264).expect("open H.264 decoder");
    }

    #[test]
    fn open_close_hevc() {
        init();
        let _dec = VideoDecoder::open(VideoCodec::Hevc).expect("open HEVC decoder");
    }

    #[test]
    fn decode_garbage_returns_error() {
        init();
        let mut dec = VideoDecoder::open(VideoCodec::H264).unwrap();
        // Send some garbage data — the decoder should accept it (buffered)
        // but receive_frame should return NeedMoreInput or an error
        let garbage = [0x00, 0x00, 0x00, 0x01, 0xDE, 0xAD, 0xBE, 0xEF];
        // send_packet may succeed (decoder buffers input)
        let _ = dec.send_packet(&garbage);
        // But no valid frame should be produced
        let result = dec.receive_frame();
        assert!(result.is_err());
    }

    #[test]
    fn empty_input_rejected() {
        init();
        let mut dec = VideoDecoder::open(VideoCodec::H264).unwrap();
        let result = dec.send_packet(&[]);
        assert!(matches!(result, Err(VideoError::EmptyInput)));
    }
}
