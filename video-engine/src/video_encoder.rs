// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe video encoder wrapping FFmpeg's `avcodec_*` API.
//!
//! Supports H.264 (libx264) and HEVC (libx265) via opt-in Cargo features
//! (`video-encoder-x264`, `video-encoder-x265`) — both of which pull in
//! GPL-licensed libraries and flip the whole FFmpeg build to GPL v2+.
//! NVENC hardware encoders (`video-encoder-nvenc`) and Intel QuickSync
//! hardware encoders (`video-encoder-qsv`, via Intel oneVPL) are also
//! available as opt-ins — both LGPL-clean at the FFmpeg layer.
//!
//! Input: planar YUV 4:2:0 (8-bit) frames with explicit strides (i.e. the
//! byte layout produced by `VideoDecoder` after a pass through
//! `VideoScaler` to `YUV420P`). The encoder expects callers to deal with
//! colorspace / bit-depth alignment themselves.
//!
//! Output: [`EncodedVideoFrame`] values carrying Annex-B NAL units, with
//! PTS / DTS / keyframe markers. The encoder's `extradata()` holds the
//! out-of-band SPS/PPS (or VPS/SPS/PPS for HEVC) when
//! `VideoEncoderConfig::global_header` is `true`.
//!
//! # Thread Safety
//!
//! `VideoEncoder` is `Send` but not `Sync`. Each instance owns its
//! `AVCodecContext`, frame, and packet buffers and requires `&mut self`
//! for `encode_frame` / `flush`.

use libffmpeg_video_sys::*;
use video_codec::{
    EncodedVideoFrame, VideoChroma, VideoEncoderCodec, VideoEncoderConfig, VideoEncoderError,
    VideoRateControl,
};

/// Safe video encoder wrapping FFmpeg's AVCodecContext.
pub struct VideoEncoder {
    ctx: *mut AVCodecContext,
    frame: *mut AVFrame,
    packet: *mut AVPacket,
    codec: VideoEncoderCodec,
    width: u32,
    height: u32,
    fps_num: u32,
    fps_den: u32,
    chroma: VideoChroma,
    bit_depth: u8,
    frame_count: i64,
    extradata: Option<Vec<u8>>,
    /// When set, the next [`encode_frame`](Self::encode_frame) call marks
    /// its `AVFrame.pict_type = AV_PICTURE_TYPE_I` so the encoder emits an
    /// IDR for that frame. Cleared automatically once consumed.
    force_idr_next: bool,
}

/// Resolve `(chroma, bit_depth)` to an FFmpeg `AVPixelFormat`. Returns
/// `Err(InvalidInput)` for unsupported combinations (e.g. 12-bit) so the
/// caller surfaces a clear error instead of passing garbage to libav.
fn resolve_pix_fmt(
    chroma: VideoChroma,
    bit_depth: u8,
) -> Result<AVPixelFormat, VideoEncoderError> {
    Ok(match (chroma, bit_depth) {
        (VideoChroma::Yuv420, 8) => AVPixelFormat_AV_PIX_FMT_YUV420P,
        (VideoChroma::Yuv422, 8) => AVPixelFormat_AV_PIX_FMT_YUV422P,
        (VideoChroma::Yuv444, 8) => AVPixelFormat_AV_PIX_FMT_YUV444P,
        (VideoChroma::Yuv420, 10) => AVPixelFormat_AV_PIX_FMT_YUV420P10LE,
        (VideoChroma::Yuv422, 10) => AVPixelFormat_AV_PIX_FMT_YUV422P10LE,
        (VideoChroma::Yuv444, 10) => AVPixelFormat_AV_PIX_FMT_YUV444P10LE,
        (c, bd) => {
            return Err(VideoEncoderError::InvalidInput(format!(
                "unsupported chroma/bit_depth combination: {} at {}-bit (supported: 8 or 10)",
                c.as_str(),
                bd,
            )));
        }
    })
}

/// Bytes per luma/chroma sample for a given bit depth (8 → 1, 10 → 2).
fn bytes_per_sample(bit_depth: u8) -> usize {
    if bit_depth > 8 { 2 } else { 1 }
}

// SAFETY: AVCodecContext is per-instance with no shared global state.
unsafe impl Send for VideoEncoder {}

impl VideoEncoder {
    /// Open a video encoder for the specified backend.
    ///
    /// Returns `EncoderDisabled` if the build was compiled without the
    /// matching feature flag. Returns `EncoderNotFound` if the feature
    /// is present but the vendored FFmpeg fails to locate the encoder
    /// at runtime (unusual — usually indicates a broken build).
    pub fn open(config: &VideoEncoderConfig) -> Result<Self, VideoEncoderError> {
        // Compile-time gate: refuse to even try opening a backend the
        // build was configured to omit.
        match config.codec {
            VideoEncoderCodec::X264 => {
                if !cfg!(feature = "video-encoder-x264") {
                    return Err(VideoEncoderError::EncoderDisabled(config.codec));
                }
            }
            VideoEncoderCodec::X265 => {
                if !cfg!(feature = "video-encoder-x265") {
                    return Err(VideoEncoderError::EncoderDisabled(config.codec));
                }
            }
            VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc => {
                if !cfg!(feature = "video-encoder-nvenc") {
                    return Err(VideoEncoderError::EncoderDisabled(config.codec));
                }
            }
            VideoEncoderCodec::H264Qsv | VideoEncoderCodec::HevcQsv => {
                if !cfg!(feature = "video-encoder-qsv") {
                    return Err(VideoEncoderError::EncoderDisabled(config.codec));
                }
            }
            VideoEncoderCodec::H264Vaapi | VideoEncoderCodec::HevcVaapi => {
                if !cfg!(feature = "video-encoder-vaapi") {
                    return Err(VideoEncoderError::EncoderDisabled(config.codec));
                }
                // VAAPI encode additionally requires an
                // `AVHWDeviceContext` set on the codec context plus a
                // `hw_frames_ctx` describing the surface pool, neither
                // of which is wrapped in `video-engine` yet. Surface a
                // clear error pointing at the missing plumbing instead
                // of letting `avcodec_open2` fall over with an opaque
                // EINVAL at first frame.
                return Err(VideoEncoderError::InvalidInput(
                    "VAAPI encode is feature-gated but not yet wired through video-engine \
                     (pending AVHWDeviceContext + hw_frames_ctx wrapping)"
                        .into(),
                ));
            }
        }

        if config.width == 0 || config.height == 0 {
            return Err(VideoEncoderError::InvalidInput(
                "width and height must be non-zero".into(),
            ));
        }
        if config.fps_num == 0 || config.fps_den == 0 {
            return Err(VideoEncoderError::InvalidInput(
                "fps_num and fps_den must be non-zero".into(),
            ));
        }
        if config.bit_depth != 8 && config.bit_depth != 10 {
            return Err(VideoEncoderError::InvalidInput(format!(
                "bit_depth must be 8 or 10, got {}",
                config.bit_depth
            )));
        }
        // NVENC's pixel-format matrix is narrower than libx264/x265. Reject
        // 10-bit 4:2:2 / 4:4:4 here rather than letting avcodec_open2
        // return an opaque error.
        if matches!(
            config.codec,
            VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc
        ) {
            if config.codec == VideoEncoderCodec::H264Nvenc && config.bit_depth != 8 {
                return Err(VideoEncoderError::InvalidInput(
                    "h264_nvenc requires bit_depth=8".into(),
                ));
            }
            if config.chroma == VideoChroma::Yuv444 {
                return Err(VideoEncoderError::InvalidInput(
                    "NVENC backends do not support chroma=yuv444p".into(),
                ));
            }
            if config.chroma == VideoChroma::Yuv422 {
                // NVENC has no 4:2:2 input path on any GPU generation
                // (Pascal through Ada Lovelace) — both H.264 and HEVC
                // are 4:2:0 + 4:4:4 only. Reject up front so the
                // operator gets a clear error instead of an opaque
                // avcodec_open2 EINVAL at first frame.
                return Err(VideoEncoderError::InvalidInput(
                    "NVENC backends do not support chroma=yuv422p (NVENC is 4:2:0 / 4:4:4 only)".into(),
                ));
            }
        }
        // QSV's pixel-format matrix is similar to NVENC: h264_qsv is
        // 8-bit only (use hevc_qsv for 10-bit on supported hardware), and
        // neither QSV variant supports 4:4:4 chroma in oneVPL today.
        if matches!(
            config.codec,
            VideoEncoderCodec::H264Qsv | VideoEncoderCodec::HevcQsv
        ) {
            if config.codec == VideoEncoderCodec::H264Qsv && config.bit_depth != 8 {
                return Err(VideoEncoderError::InvalidInput(
                    "h264_qsv requires bit_depth=8 (use hevc_qsv for 10-bit)".into(),
                ));
            }
            if config.chroma == VideoChroma::Yuv444 {
                return Err(VideoEncoderError::InvalidInput(
                    "QSV backends do not support chroma=yuv444p".into(),
                ));
            }
        }

        let pix_fmt = resolve_pix_fmt(config.chroma, config.bit_depth)?;

        unsafe {
            // FFmpeg encoder name → *const c_char (NUL-terminated).
            let name_cstr = std::ffi::CString::new(config.codec.ffmpeg_name())
                .map_err(|_| VideoEncoderError::InvalidInput("null byte in codec name".into()))?;
            let codec_ptr = avcodec_find_encoder_by_name(name_cstr.as_ptr());
            if codec_ptr.is_null() {
                return Err(VideoEncoderError::EncoderNotFound(config.codec));
            }

            let ctx = avcodec_alloc_context3(codec_ptr);
            if ctx.is_null() {
                return Err(VideoEncoderError::AllocContext);
            }

            (*ctx).width = config.width as i32;
            (*ctx).height = config.height as i32;
            (*ctx).pix_fmt = pix_fmt;
            // time_base = 1 / fps (single tick per frame in the encoder clock)
            (*ctx).time_base.num = config.fps_den as i32;
            (*ctx).time_base.den = config.fps_num as i32;
            (*ctx).framerate.num = config.fps_num as i32;
            (*ctx).framerate.den = config.fps_den as i32;
            (*ctx).gop_size = config.gop_size as i32;
            (*ctx).max_b_frames = config.max_b_frames as i32;

            // Rate control. `Crf` leaves bit_rate unset (the encoder uses
            // the CRF value instead). CBR clamps min=max=target. VBR/ABR
            // set bit_rate and optionally rc_max_rate for a cap.
            match config.rate_control {
                VideoRateControl::Crf => {}
                VideoRateControl::Cbr => {
                    let br = (config.bitrate_kbps as i64) * 1000;
                    (*ctx).bit_rate = br;
                    (*ctx).rc_min_rate = br;
                    (*ctx).rc_max_rate = br;
                    (*ctx).rc_buffer_size = (br * 2) as i32; // 2 s VBV
                }
                VideoRateControl::Vbr | VideoRateControl::Abr => {
                    (*ctx).bit_rate = (config.bitrate_kbps as i64) * 1000;
                    if config.max_bitrate_kbps > 0 {
                        (*ctx).rc_max_rate = (config.max_bitrate_kbps as i64) * 1000;
                        (*ctx).rc_buffer_size = ((config.max_bitrate_kbps as i64) * 2000) as i32;
                    }
                }
            }

            // Colour metadata — only set when operator provided a value so
            // we don't override the encoder's reasonable defaults.
            if let Some(v) = parse_color_primaries(&config.color_primaries) {
                (*ctx).color_primaries = v;
            }
            if let Some(v) = parse_color_transfer(&config.color_transfer) {
                (*ctx).color_trc = v;
            }
            if let Some(v) = parse_color_matrix(&config.color_matrix) {
                (*ctx).colorspace = v;
            }
            if let Some(v) = parse_color_range(&config.color_range) {
                (*ctx).color_range = v;
            }

            if config.global_header {
                (*ctx).flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
            }

            // Codec-private options (preset / profile / tune / level / refs /
            // crf). All four supported backends accept `preset` as a named
            // string option; libx264 and libx265 additionally understand
            // `profile`, `level`, `refs`, and `crf`. NVENC consumes `preset`
            // and `cq` (for constant-quality).
            let mut opts: *mut AVDictionary = std::ptr::null_mut();
            let preset_key = std::ffi::CString::new("preset").unwrap();
            let preset_val = std::ffi::CString::new(config.preset.as_str()).unwrap();
            av_dict_set(&mut opts, preset_key.as_ptr(), preset_val.as_ptr(), 0);

            if let Some(profile) = config.profile.as_str() {
                let profile_key = std::ffi::CString::new("profile").unwrap();
                let profile_val = std::ffi::CString::new(profile).unwrap();
                av_dict_set(&mut opts, profile_key.as_ptr(), profile_val.as_ptr(), 0);
            }

            // Tuning: configurable. Empty string = don't pass to encoder
            // (let it choose). Default remains "zerolatency" via the
            // VideoEncoderConfig default.
            if !config.tune.is_empty() {
                let tune_key = std::ffi::CString::new("tune").unwrap();
                let tune_val = std::ffi::CString::new(config.tune.as_str())
                    .map_err(|_| VideoEncoderError::InvalidInput("tune contains NUL".into()))?;
                av_dict_set(&mut opts, tune_key.as_ptr(), tune_val.as_ptr(), 0);
            }

            if !config.level.is_empty() {
                let level_key = std::ffi::CString::new("level").unwrap();
                let level_val = std::ffi::CString::new(config.level.as_str())
                    .map_err(|_| VideoEncoderError::InvalidInput("level contains NUL".into()))?;
                av_dict_set(&mut opts, level_key.as_ptr(), level_val.as_ptr(), 0);
            }

            if config.refs > 0 {
                let refs_key = std::ffi::CString::new("refs").unwrap();
                let refs_val = std::ffi::CString::new(config.refs.to_string()).unwrap();
                av_dict_set(&mut opts, refs_key.as_ptr(), refs_val.as_ptr(), 0);
            }

            if matches!(config.rate_control, VideoRateControl::Crf) {
                // libx264/x265 use `crf`; NVENC uses `cq`. Setting both is
                // harmless on the encoder that doesn't understand the other.
                let crf_val_str = std::ffi::CString::new(config.crf.to_string()).unwrap();
                let crf_key = std::ffi::CString::new("crf").unwrap();
                av_dict_set(&mut opts, crf_key.as_ptr(), crf_val_str.as_ptr(), 0);
                let cq_key = std::ffi::CString::new("cq").unwrap();
                av_dict_set(&mut opts, cq_key.as_ptr(), crf_val_str.as_ptr(), 0);
                // NVENC needs the rc mode flipped explicitly.
                if matches!(
                    config.codec,
                    VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc
                ) {
                    let rc_key = std::ffi::CString::new("rc").unwrap();
                    let rc_val = std::ffi::CString::new("vbr").unwrap();
                    av_dict_set(&mut opts, rc_key.as_ptr(), rc_val.as_ptr(), 0);
                }
            } else if matches!(config.rate_control, VideoRateControl::Cbr) {
                // Surface CBR to x264's HRD so the bitstream carries
                // nal-hrd=cbr; NVENC has a native `rc=cbr` switch.
                if matches!(config.codec, VideoEncoderCodec::X264) {
                    let xkey = std::ffi::CString::new("x264-params").unwrap();
                    let xval = std::ffi::CString::new("nal-hrd=cbr").unwrap();
                    av_dict_set(&mut opts, xkey.as_ptr(), xval.as_ptr(), 0);
                } else if matches!(
                    config.codec,
                    VideoEncoderCodec::H264Nvenc | VideoEncoderCodec::HevcNvenc
                ) {
                    let rc_key = std::ffi::CString::new("rc").unwrap();
                    let rc_val = std::ffi::CString::new("cbr").unwrap();
                    av_dict_set(&mut opts, rc_key.as_ptr(), rc_val.as_ptr(), 0);
                }
            }

            let ret = avcodec_open2(ctx, codec_ptr, &mut opts);
            av_dict_free(&mut opts);
            if ret < 0 {
                let mut c = ctx;
                avcodec_free_context(&mut c);
                return Err(VideoEncoderError::OpenCodec(ret));
            }

            let frame = av_frame_alloc();
            if frame.is_null() {
                let mut c = ctx;
                avcodec_free_context(&mut c);
                return Err(VideoEncoderError::AllocFrame);
            }
            (*frame).width = (*ctx).width;
            (*frame).height = (*ctx).height;
            (*frame).format = (*ctx).pix_fmt;
            let ret = av_frame_get_buffer(frame, 32);
            if ret < 0 {
                let mut f = frame;
                let mut c = ctx;
                av_frame_free(&mut f);
                avcodec_free_context(&mut c);
                return Err(VideoEncoderError::AllocFrameBuffer(ret));
            }

            let packet = av_packet_alloc();
            if packet.is_null() {
                let mut f = frame;
                let mut c = ctx;
                av_frame_free(&mut f);
                avcodec_free_context(&mut c);
                return Err(VideoEncoderError::AllocPacket);
            }

            // Snapshot extradata (SPS/PPS) if the encoder produced it
            // during `open` — it's the bitstream out-of-band parameters
            // required by callers that negotiate via SDP / FLV metadata.
            let extradata = if !(*ctx).extradata.is_null() && (*ctx).extradata_size > 0 {
                let bytes = std::slice::from_raw_parts(
                    (*ctx).extradata,
                    (*ctx).extradata_size as usize,
                );
                Some(bytes.to_vec())
            } else {
                None
            };

            Ok(Self {
                ctx,
                frame,
                packet,
                codec: config.codec,
                width: config.width,
                height: config.height,
                fps_num: config.fps_num,
                fps_den: config.fps_den,
                chroma: config.chroma,
                bit_depth: config.bit_depth,
                frame_count: 0,
                extradata,
                force_idr_next: false,
            })
        }
    }

    /// Chroma subsampling the encoder was opened with.
    pub fn chroma(&self) -> VideoChroma {
        self.chroma
    }

    /// Sample bit depth the encoder was opened with (8 or 10).
    pub fn bit_depth(&self) -> u8 {
        self.bit_depth
    }

    /// The backend this encoder was opened for.
    pub fn codec(&self) -> VideoEncoderCodec {
        self.codec
    }

    /// Output resolution.
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Configured frame rate (numerator / denominator).
    pub fn frame_rate(&self) -> (u32, u32) {
        (self.fps_num, self.fps_den)
    }

    /// Out-of-band bitstream headers (SPS/PPS for H.264, VPS/SPS/PPS for
    /// HEVC). `None` if the encoder emits headers inline.
    pub fn extradata(&self) -> Option<&[u8]> {
        self.extradata.as_deref()
    }

    /// Arm a one-shot IDR request on the next encoded frame.
    ///
    /// The next call to [`encode_frame`](Self::encode_frame) marks its
    /// `AVFrame.pict_type = AV_PICTURE_TYPE_I` before sending to the
    /// encoder, which causes libx264 / libx265 / NVENC to emit an IDR for
    /// that frame regardless of the configured GOP position. The flag is
    /// consumed by the next encode call; subsequent frames revert to the
    /// encoder's normal rate-control-driven frame-type decisions.
    ///
    /// Used by the seamless input-switch path: when a flow switches to a
    /// different re-encoded input, the new input's encoder needs to emit a
    /// keyframe so downstream decoders can resync immediately instead of
    /// waiting up to a full GOP for the next natural IDR.
    pub fn force_next_keyframe(&mut self) {
        self.force_idr_next = true;
    }

    /// Encode one decoded YUV-4:2:0 frame.
    ///
    /// The three planes `y`, `u`, `v` must already be at the encoder's
    /// output resolution (callers should run `VideoScaler` first when the
    /// source size differs). `y_stride` / `u_stride` / `v_stride` are the
    /// number of bytes between rows in the respective planes.
    ///
    /// `pts`, when `Some`, fixes the frame's presentation timestamp in
    /// the encoder time base (1 / fps_num). When `None`, the encoder
    /// counts frames monotonically from zero.
    pub fn encode_frame(
        &mut self,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
        pts: Option<i64>,
    ) -> Result<Vec<EncodedVideoFrame>, VideoEncoderError> {
        let h = self.height as usize;
        let hh = self.chroma.chroma_height(self.height) as usize;
        let w = self.width as usize;
        let ww = self.chroma.chroma_width(self.width) as usize;
        let bps = bytes_per_sample(self.bit_depth);
        // Minimum useful stride (in bytes): one sample per pixel in the
        // row, widened for 10-bit (2 bytes/sample).
        let min_y_stride = w * bps;
        let min_chroma_stride = ww * bps;

        if y.len() < y_stride * h || y_stride < min_y_stride {
            return Err(VideoEncoderError::InvalidInput(format!(
                "Y plane too small: need {}x{} (stride>={} bytes), got {} bytes at stride {}",
                w, h, min_y_stride, y.len(), y_stride
            )));
        }
        if u.len() < u_stride * hh || u_stride < min_chroma_stride {
            return Err(VideoEncoderError::InvalidInput(format!(
                "U plane too small: need {}x{} (stride>={} bytes), got {} bytes at stride {}",
                ww, hh, min_chroma_stride, u.len(), u_stride
            )));
        }
        if v.len() < v_stride * hh || v_stride < min_chroma_stride {
            return Err(VideoEncoderError::InvalidInput(format!(
                "V plane too small: need {}x{} (stride>={} bytes), got {} bytes at stride {}",
                ww, hh, min_chroma_stride, v.len(), v_stride
            )));
        }

        unsafe {
            // Copy the caller's planes into the AVFrame buffers respecting
            // the frame's internal linesize (which may differ from the
            // caller's stride due to libavutil alignment).
            copy_plane((*self.frame).data[0], (*self.frame).linesize[0], y, y_stride, h);
            copy_plane((*self.frame).data[1], (*self.frame).linesize[1], u, u_stride, hh);
            copy_plane((*self.frame).data[2], (*self.frame).linesize[2], v, v_stride, hh);

            (*self.frame).pts = pts.unwrap_or(self.frame_count);
            self.frame_count = (*self.frame).pts + 1;

            // One-shot IDR request: libx264 / libx265 / NVENC all honour
            // `pict_type = I` by emitting an IDR for that frame. Required
            // for seamless input switching — the downstream decoder needs
            // a keyframe on the first frame after a switch, regardless of
            // where we are in the current GOP. Always reset to NONE on
            // normal frames so the encoder's rate control stays in charge.
            if self.force_idr_next {
                (*self.frame).pict_type = AVPictureType_AV_PICTURE_TYPE_I;
                self.force_idr_next = false;
            } else {
                (*self.frame).pict_type = AVPictureType_AV_PICTURE_TYPE_NONE;
            }

            self.send_and_receive()
        }
    }

    /// Flush the encoder — drain any buffered frames at end of stream.
    pub fn flush(&mut self) -> Result<Vec<EncodedVideoFrame>, VideoEncoderError> {
        unsafe {
            let ret = avcodec_send_frame(self.ctx, std::ptr::null());
            // EAGAIN (-11) and EOF (-541478725) are expected during drain;
            // anything else is an error.
            if ret < 0 && ret != -11 && ret != -541_478_725 {
                return Err(VideoEncoderError::SendFrame(ret));
            }
            self.drain_packets()
        }
    }

    unsafe fn send_and_receive(&mut self) -> Result<Vec<EncodedVideoFrame>, VideoEncoderError> {
        let ret = avcodec_send_frame(self.ctx, self.frame);
        if ret < 0 {
            return Err(VideoEncoderError::SendFrame(ret));
        }
        self.drain_packets()
    }

    unsafe fn drain_packets(&mut self) -> Result<Vec<EncodedVideoFrame>, VideoEncoderError> {
        let mut out = Vec::new();
        loop {
            av_packet_unref(self.packet);
            let ret = avcodec_receive_packet(self.ctx, self.packet);
            if ret < 0 {
                // EAGAIN / EOF — no more packets available right now.
                break;
            }
            let data = std::slice::from_raw_parts(
                (*self.packet).data,
                (*self.packet).size as usize,
            )
            .to_vec();
            let keyframe = ((*self.packet).flags & AV_PKT_FLAG_KEY as i32) != 0;
            out.push(EncodedVideoFrame {
                data,
                pts: (*self.packet).pts,
                dts: (*self.packet).dts,
                keyframe,
            });
        }
        Ok(out)
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe {
            if !self.packet.is_null() {
                av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                av_frame_free(&mut self.frame);
            }
            if !self.ctx.is_null() {
                avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder")
            .field("codec", &self.codec)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("fps_num", &self.fps_num)
            .field("fps_den", &self.fps_den)
            .finish()
    }
}

/// Copy `rows` rows from `src` (with `src_stride` bytes per row, contiguous)
/// into `dst` (with `dst_stride` bytes per row — may be larger than
/// `src_stride` due to libavutil alignment). Copies only the useful
/// `min(src_stride, dst_stride)` bytes per row.
unsafe fn copy_plane(
    dst: *mut u8,
    dst_stride: i32,
    src: &[u8],
    src_stride: usize,
    rows: usize,
) {
    let dst_stride = dst_stride as usize;
    let width = dst_stride.min(src_stride);
    for r in 0..rows {
        let dst_row = dst.add(r * dst_stride);
        let src_row = src.as_ptr().add(r * src_stride);
        std::ptr::copy_nonoverlapping(src_row, dst_row, width);
    }
}

fn parse_color_primaries(s: &str) -> Option<AVColorPrimaries> {
    match s {
        "" => None,
        "bt709" => Some(AVColorPrimaries_AVCOL_PRI_BT709),
        "bt2020" => Some(AVColorPrimaries_AVCOL_PRI_BT2020),
        "smpte170m" => Some(AVColorPrimaries_AVCOL_PRI_SMPTE170M),
        "smpte240m" => Some(AVColorPrimaries_AVCOL_PRI_SMPTE240M),
        "bt470m" => Some(AVColorPrimaries_AVCOL_PRI_BT470M),
        "bt470bg" => Some(AVColorPrimaries_AVCOL_PRI_BT470BG),
        _ => None,
    }
}

fn parse_color_transfer(s: &str) -> Option<AVColorTransferCharacteristic> {
    match s {
        "" => None,
        "bt709" => Some(AVColorTransferCharacteristic_AVCOL_TRC_BT709),
        "smpte170m" => Some(AVColorTransferCharacteristic_AVCOL_TRC_SMPTE170M),
        "smpte2084" | "pq" => Some(AVColorTransferCharacteristic_AVCOL_TRC_SMPTE2084),
        "arib-std-b67" | "hlg" => Some(AVColorTransferCharacteristic_AVCOL_TRC_ARIB_STD_B67),
        "bt2020-10" => Some(AVColorTransferCharacteristic_AVCOL_TRC_BT2020_10),
        "bt2020-12" => Some(AVColorTransferCharacteristic_AVCOL_TRC_BT2020_12),
        _ => None,
    }
}

fn parse_color_matrix(s: &str) -> Option<AVColorSpace> {
    match s {
        "" => None,
        "bt709" => Some(AVColorSpace_AVCOL_SPC_BT709),
        "bt2020nc" => Some(AVColorSpace_AVCOL_SPC_BT2020_NCL),
        "bt2020c" => Some(AVColorSpace_AVCOL_SPC_BT2020_CL),
        "smpte170m" => Some(AVColorSpace_AVCOL_SPC_SMPTE170M),
        "smpte240m" => Some(AVColorSpace_AVCOL_SPC_SMPTE240M),
        _ => None,
    }
}

fn parse_color_range(s: &str) -> Option<AVColorRange> {
    match s {
        "" => None,
        "tv" | "limited" | "mpeg" => Some(AVColorRange_AVCOL_RANGE_MPEG),
        "pc" | "full" | "jpeg" => Some(AVColorRange_AVCOL_RANGE_JPEG),
        _ => None,
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_without_feature_returns_disabled() {
        // On a build without the corresponding feature flag, every
        // backend must fail with `EncoderDisabled` before attempting
        // any FFmpeg calls.
        if !cfg!(feature = "video-encoder-x264") {
            let cfg = VideoEncoderConfig {
                codec: VideoEncoderCodec::X264,
                ..Default::default()
            };
            assert!(matches!(
                VideoEncoder::open(&cfg),
                Err(VideoEncoderError::EncoderDisabled(VideoEncoderCodec::X264))
            ));
        }
        if !cfg!(feature = "video-encoder-x265") {
            let cfg = VideoEncoderConfig {
                codec: VideoEncoderCodec::X265,
                ..Default::default()
            };
            assert!(matches!(
                VideoEncoder::open(&cfg),
                Err(VideoEncoderError::EncoderDisabled(VideoEncoderCodec::X265))
            ));
        }
    }

    #[test]
    fn zero_dimensions_rejected() {
        let cfg = VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: 0,
            ..Default::default()
        };
        assert!(VideoEncoder::open(&cfg).is_err());
    }

    #[test]
    fn zero_framerate_rejected() {
        let cfg = VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            fps_num: 0,
            ..Default::default()
        };
        assert!(VideoEncoder::open(&cfg).is_err());
    }

    #[cfg(feature = "video-encoder-x264")]
    #[test]
    fn x264_encodes_a_black_frame() {
        let cfg = VideoEncoderConfig {
            codec: VideoEncoderCodec::X264,
            width: 320,
            height: 240,
            fps_num: 25,
            fps_den: 1,
            bitrate_kbps: 500,
            gop_size: 25,
            ..Default::default()
        };
        let mut enc = VideoEncoder::open(&cfg).expect("open x264");

        // Black YUV: Y=0, U=V=128 (chroma neutral).
        let y = vec![0u8; 320 * 240];
        let u = vec![128u8; 160 * 120];
        let v = vec![128u8; 160 * 120];

        // Push several frames so the encoder emits at least one packet.
        let mut total = 0usize;
        for _ in 0..5 {
            let out = enc
                .encode_frame(&y, 320, &u, 160, &v, 160, None)
                .expect("encode");
            total += out.len();
        }
        let flushed = enc.flush().expect("flush");
        total += flushed.len();
        assert!(total > 0, "expected at least one encoded frame");
    }
}
