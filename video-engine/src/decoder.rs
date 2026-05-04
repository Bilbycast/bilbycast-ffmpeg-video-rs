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

/// Decoder backend — selects which FFmpeg decoder family `VideoDecoder`
/// opens against. `Cpu` is the always-available libavcodec software
/// path; the hardware variants need their corresponding Cargo features
/// (`video-decoder-nvdec`, `video-decoder-qsv`) AND a working driver +
/// hardware at runtime — open will return `EncoderDisabled` /
/// `OpenCodec` when the host can't satisfy the request.
///
/// HW frames come back in NV12 system memory by default — the cuvid /
/// QSV decoders auto-download to host memory via FFmpeg's built-in
/// hwframe transfer. Callers pick up the layout via
/// [`DecodedFrame::pixel_format`] and either use [`DecodedFrame::yuv_planes`]
/// (planar YUV) or [`DecodedFrame::nv12_planes`] (semi-planar NV12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderBackend {
    /// libavcodec software decoder. Always available.
    Cpu,
    /// NVIDIA NVDEC via `h264_cuvid` / `hevc_cuvid`. Needs the
    /// `video-decoder-nvdec` Cargo feature.
    Nvdec,
    /// Intel QuickSync via `h264_qsv` / `hevc_qsv`. Needs the
    /// `video-decoder-qsv` Cargo feature.
    Qsv,
}

impl DecoderBackend {
    /// FFmpeg decoder name for this backend + codec, or `None` for
    /// `Cpu` (which uses `avcodec_find_decoder` with the codec ID, not
    /// a name lookup). Used by [`VideoDecoder::open_with_backend`] and
    /// the runtime probe in `probe.rs`.
    pub fn ffmpeg_decoder_name(self, codec: VideoCodec) -> Option<&'static str> {
        match (self, codec) {
            (DecoderBackend::Cpu, _) => None,
            (DecoderBackend::Nvdec, VideoCodec::H264) => Some("h264_cuvid"),
            (DecoderBackend::Nvdec, VideoCodec::Hevc) => Some("hevc_cuvid"),
            (DecoderBackend::Qsv, VideoCodec::H264) => Some("h264_qsv"),
            (DecoderBackend::Qsv, VideoCodec::Hevc) => Some("hevc_qsv"),
        }
    }
}

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

    /// Per-frame PTS in the timebase the caller supplied to
    /// [`VideoDecoder::send_packet_with_pts`]. FFmpeg propagates the
    /// input packet's PTS through the decoder's internal reorder
    /// queue, so this is the **display-order** PTS of the frame —
    /// callers don't have to deal with B-frame reorder themselves.
    /// Returns `None` when the input had no PTS attached
    /// (`AV_NOPTS_VALUE` sentinel).
    pub fn pts(&self) -> Option<i64> {
        let raw = unsafe { (*self.frame).pts };
        // AV_NOPTS_VALUE = INT64_MIN per FFmpeg headers.
        if raw == i64::MIN {
            None
        } else {
            Some(raw)
        }
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

    /// Access the two planes of an NV12 frame (Y + interleaved UV).
    ///
    /// Returns `Some((y, y_stride, uv, uv_stride))` when the pixel
    /// format is `AV_PIX_FMT_NV12` — the default system-memory output
    /// of `h264_cuvid` / `hevc_cuvid` / `h264_qsv` / `hevc_qsv`. The UV
    /// plane is half-height (4:2:0 chroma sub-sampling) and contains
    /// interleaved U/V byte pairs at full chroma width — i.e. one CbCr
    /// pair per 2×2 luma block.
    ///
    /// Returns `None` for any other format. Callers in the display
    /// path use this accessor when [`pixel_format`] reports NV12 and
    /// fall back to [`yuv_planes`] for planar YUV (the CPU decoder
    /// path).
    pub fn nv12_planes(&self) -> Option<(&[u8], usize, &[u8], usize)> {
        unsafe {
            let frame = &*self.frame;
            if frame.format != AVPixelFormat_AV_PIX_FMT_NV12 {
                return None;
            }
            let y_ptr = frame.data[0];
            let uv_ptr = frame.data[1];
            if y_ptr.is_null() || uv_ptr.is_null() {
                return None;
            }
            let y_stride = frame.linesize[0] as usize;
            let uv_stride = frame.linesize[1] as usize;
            let h = frame.height as usize;
            // 4:2:0 chroma: half-height. Round up so odd-height frames
            // still fit (mirrors `yuv_planes()` rounding).
            let chroma_rows = (h + 1) >> 1;
            Some((
                std::slice::from_raw_parts(y_ptr, y_stride * h),
                y_stride,
                std::slice::from_raw_parts(uv_ptr, uv_stride * chroma_rows),
                uv_stride,
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
    /// Open a software (libavcodec) decoder for the specified video codec.
    ///
    /// Equivalent to [`open_with_backend`] with `DecoderBackend::Cpu`.
    pub fn open(codec: VideoCodec) -> Result<Self, VideoError> {
        Self::open_with_backend(codec, DecoderBackend::Cpu)
    }

    /// Open a decoder for the specified video codec, selecting the
    /// backend (software libavcodec or one of the hardware families
    /// gated on the matching Cargo feature).
    ///
    /// `DecoderBackend::Cpu` always succeeds when the codec is
    /// compiled in. HW backends fail with `VideoError::CodecNotFound`
    /// when the matching `video-decoder-*` Cargo feature is off, and
    /// with `VideoError::OpenCodec` when the host lacks the driver /
    /// hardware / permissions to instantiate a session.
    pub fn open_with_backend(
        codec: VideoCodec,
        backend: DecoderBackend,
    ) -> Result<Self, VideoError> {
        unsafe {
            let av_codec = match backend {
                DecoderBackend::Cpu => {
                    let codec_id = match codec {
                        VideoCodec::H264 => AVCodecID_AV_CODEC_ID_H264,
                        VideoCodec::Hevc => AVCodecID_AV_CODEC_ID_HEVC,
                    };
                    avcodec_find_decoder(codec_id)
                }
                DecoderBackend::Nvdec | DecoderBackend::Qsv => {
                    // HW backends are name-keyed (`h264_cuvid`, `hevc_qsv`,
                    // ...). Look up the name; non-NULL result means the
                    // matching `--enable-decoder=*` was passed to FFmpeg
                    // configure, which corresponds to the Cargo feature
                    // being on.
                    let Some(name) = backend.ffmpeg_decoder_name(codec) else {
                        return Err(VideoError::CodecNotFound(codec));
                    };
                    let cstr = std::ffi::CString::new(name)
                        .map_err(|_| VideoError::CodecNotFound(codec))?;
                    avcodec_find_decoder_by_name(cstr.as_ptr())
                }
            };
            if av_codec.is_null() {
                return Err(VideoError::CodecNotFound(codec));
            }

            let ctx = avcodec_alloc_context3(av_codec);
            if ctx.is_null() {
                return Err(VideoError::AllocContext);
            }

            // Allow truncated packets (common in TS streams). Safe on
            // the cuvid / QSV decoders too — they buffer NAL units
            // internally and tolerate the same partial-packet feeding
            // pattern as the SW decoder.
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
        // No-PTS path. Set both pts and dts to AV_NOPTS_VALUE so the
        // packet doesn't carry a stale value from a previous send.
        self.send_packet_inner(data, i64::MIN)
    }

    /// Same as [`send_packet`] but attaches a presentation timestamp to
    /// the input packet. FFmpeg propagates `pkt.pts` → `frame.pts`
    /// through the decoder's reorder queue, so callers can read each
    /// decoded frame's true display-order PTS via
    /// [`DecodedFrame::pts`]. Required for any consumer that has to
    /// schedule frame display against an audio master clock — e.g.
    /// the local-display output, where every decoded frame in a GOP
    /// otherwise inherits the same most-recent input PTS and the
    /// audio dup/drop logic misfires on every B-frame.
    pub fn send_packet_with_pts(&mut self, data: &[u8], pts: i64) -> Result<(), VideoError> {
        self.send_packet_inner(data, pts)
    }

    fn send_packet_inner(&mut self, data: &[u8], pts: i64) -> Result<(), VideoError> {
        if data.is_empty() {
            return Err(VideoError::EmptyInput);
        }

        unsafe {
            (*self.packet).data = data.as_ptr() as *mut u8;
            (*self.packet).size = data.len() as i32;
            (*self.packet).pts = pts;
            // DTS doesn't matter to a pull-mode decoder when the bit-
            // stream's own DTS is implicit (we only feed complete
            // access units), but FFmpeg complains in some paths if dts
            // is set while pts is not. Mirror pts so the two stay in
            // sync — the decoder uses pts for frame ordering anyway.
            (*self.packet).dts = pts;

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

    #[test]
    fn open_with_backend_cpu_matches_open() {
        // Regression check: `open(codec)` must remain a thin wrapper
        // over `open_with_backend(codec, Cpu)`. Both must succeed for
        // H.264 and HEVC on every host.
        init();
        let _h264 = VideoDecoder::open_with_backend(VideoCodec::H264, DecoderBackend::Cpu)
            .expect("Cpu H.264 should always open");
        let _hevc = VideoDecoder::open_with_backend(VideoCodec::Hevc, DecoderBackend::Cpu)
            .expect("Cpu HEVC should always open");
    }

    #[test]
    fn nvdec_unavailable_when_feature_off() {
        init();
        // When the `video-decoder-nvdec` feature is off, looking up
        // `h264_cuvid` returns NULL and the open surfaces
        // `CodecNotFound`. Caller (display output's resolver) can
        // distinguish this from "host has no NVIDIA GPU" by reading
        // the `ProbeError` returned by the startup probe — but at the
        // open layer, a missing FFmpeg-side decoder presents as
        // `CodecNotFound`.
        #[cfg(not(feature = "video-decoder-nvdec"))]
        {
            let result = VideoDecoder::open_with_backend(
                VideoCodec::H264,
                DecoderBackend::Nvdec,
            );
            assert!(matches!(result, Err(VideoError::CodecNotFound(_))));
        }
    }

    #[test]
    fn ffmpeg_decoder_name_mapping() {
        assert_eq!(
            DecoderBackend::Cpu.ffmpeg_decoder_name(VideoCodec::H264),
            None
        );
        assert_eq!(
            DecoderBackend::Nvdec.ffmpeg_decoder_name(VideoCodec::H264),
            Some("h264_cuvid")
        );
        assert_eq!(
            DecoderBackend::Nvdec.ffmpeg_decoder_name(VideoCodec::Hevc),
            Some("hevc_cuvid")
        );
        assert_eq!(
            DecoderBackend::Qsv.ffmpeg_decoder_name(VideoCodec::H264),
            Some("h264_qsv")
        );
        assert_eq!(
            DecoderBackend::Qsv.ffmpeg_decoder_name(VideoCodec::Hevc),
            Some("hevc_qsv")
        );
    }
}
