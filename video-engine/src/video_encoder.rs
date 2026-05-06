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

use crate::vaapi::VaapiDevice;

/// Safe video encoder wrapping FFmpeg's AVCodecContext.
pub struct VideoEncoder {
    ctx: *mut AVCodecContext,
    /// Encoder-input AVFrame.
    ///
    /// * **SW backends** (libx264 / libx265 / NVENC / QSV): pre-allocated
    ///   at open time with the negotiated `pix_fmt`; planes are copied in
    ///   per `encode_frame` call.
    /// * **VAAPI backend**: allocated empty at open time. Per call we
    ///   `av_frame_unref` to release the prior surface (the encoder
    ///   internally retains its own reference for any frames still
    ///   in-flight in the reorder queue), then `av_hwframe_get_buffer`
    ///   pulls a fresh VAAPI surface from `hw_frames_ref`'s pool, which
    ///   `av_hwframe_transfer_data` populates from `sw_frame`.
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
    /// VAAPI hwdevice for `H264Vaapi` / `HevcVaapi` opens; `None` for
    /// every other backend. Held alongside the codec context so the
    /// `AVHWDeviceContext` outlives any encoder-internal references.
    /// Drop order: the `VideoEncoder`'s `ctx` field is dropped first
    /// (declaration order), which unrefs `hw_device_ctx` and
    /// `hw_frames_ctx`, then the `VaapiDevice` Arc here drops the last
    /// owning ref to the `AVBufferRef`.
    #[allow(dead_code)]
    vaapi_device: Option<VaapiDevice>,
    /// VAAPI `hw_frames_ctx` (`AVBufferRef*`). Null for non-VAAPI
    /// backends. The codec context owns its own bumped reference via
    /// the `hw_frames_ctx` field; this one keeps a parallel ref so we
    /// can `av_hwframe_get_buffer` per encode call.
    hw_frames_ref: *mut AVBufferRef,
    /// Sysmem source `AVFrame` for the VAAPI upload path. Carries
    /// NV12-packed Y + interleaved-UV data assembled per-call from the
    /// caller's planar Y/U/V buffers, then handed to
    /// `av_hwframe_transfer_data` to populate the VAAPI surface. Null
    /// for non-VAAPI backends.
    sw_frame: *mut AVFrame,
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

/// Pick the VAAPI surface layout (`hw_frames_ctx.sw_format`) for the
/// caller's chroma + bit-depth. Restricted to the four broadcast
/// contribution combinations the encoder accepts; 4:4:4 (NV24) and
/// non-8/non-10 bit depths are rejected at the top of `open()` before
/// we get here. Not feature-gated — the function body only references
/// pixel-format constants the bindgen pass always exposes; the call
/// sites in `open()` are reachable only when the VAAPI codec branch
/// passed the feature gate, so non-VAAPI builds compile this as
/// dead code.
fn vaapi_sw_format_for(chroma: VideoChroma, bit_depth: u8) -> AVPixelFormat {
    match (chroma, bit_depth) {
        (VideoChroma::Yuv420, 8) => AVPixelFormat_AV_PIX_FMT_NV12,
        (VideoChroma::Yuv420, 10) => AVPixelFormat_AV_PIX_FMT_P010LE,
        (VideoChroma::Yuv422, 8) => AVPixelFormat_AV_PIX_FMT_NV16,
        (VideoChroma::Yuv422, 10) => AVPixelFormat_AV_PIX_FMT_P210LE,
        // Yuv444 + non-{8,10} bit depths are rejected at the top of
        // `VideoEncoder::open()` so they never reach this function.
        _ => AVPixelFormat_AV_PIX_FMT_NV12,
    }
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
                // VAAPI encoder supports the broadcast contribution
                // matrix: 4:2:0 + 4:2:2, 8-bit + 10-bit. Mapping to
                // VAAPI surface formats:
                //
                //   chroma × depth  → sw_format    HEVC profile
                //   ──────────────────────────────────────────────
                //   Yuv420 × 8      → NV12         Main
                //   Yuv420 × 10     → P010LE       Main 10
                //   Yuv422 × 8      → NV16         Main 4:2:2 (rare)
                //   Yuv422 × 10     → P210LE       Main 4:2:2 10
                //
                // 4:4:4 (NV24) is staged for follow-up. Driver support
                // varies — Intel iHD on Tiger Lake (11th gen) and newer
                // covers HEVC 4:2:2 8/10-bit; AMD VCN HEVC encoder
                // generally rejects 4:2:2 at `avcodec_open2` (broadcast
                // contribution shops on AMD typically pick libx265 for
                // 4:2:2). The host probe surfaces this per-(codec,
                // chroma, bit-depth) so the manager UI can gate the
                // dropdown accordingly.
                if config.chroma == VideoChroma::Yuv444 {
                    return Err(VideoEncoderError::InvalidInput(
                        "VAAPI encode does not support chroma=yuv444p (NV24 packer staged for follow-up)".into(),
                    ));
                }
                if config.bit_depth != 8 && config.bit_depth != 10 {
                    return Err(VideoEncoderError::InvalidInput(format!(
                        "VAAPI encode supports bit_depth=8 or 10 (got {}-bit)",
                        config.bit_depth,
                    )));
                }
                // H.264 has no Main10 / Main 4:2:2 profile in any
                // VAAPI implementation. Reject early; otherwise
                // `avcodec_open2` surfaces an opaque EINVAL.
                if config.codec == VideoEncoderCodec::H264Vaapi
                    && (config.bit_depth != 8 || config.chroma != VideoChroma::Yuv420)
                {
                    return Err(VideoEncoderError::InvalidInput(
                        "h264_vaapi supports 4:2:0 8-bit only — use hevc_vaapi for 4:2:2 / 10-bit broadcast contribution".into(),
                    ));
                }
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

            // VAAPI: open the hwdevice + allocate encoder-side
            // `hw_frames_ctx` BEFORE `avcodec_open2`. The decoder lazy-
            // allocates its frames context inside `vaapi_get_format_callback`
            // once it has parsed the first SPS; encoders have no equivalent
            // negotiation hook, so the pool has to be ready before open.
            //
            // Override `pix_fmt` to the HW surface format — the underlying
            // surface layout is carried by `hw_frames_ctx.sw_format` and
            // chosen by `vaapi_sw_format_for(chroma)`:
            //   • Yuv420 8-bit → NV12  (4:2:0 contribution baseline)
            //   • Yuv422 8-bit → NV16  (4:2:2 broadcast contribution)
            // The same NV-style upload packer (Y plane direct copy +
            // U/V byte-interleaved into the chroma plane) handles both
            // — the only thing that differs is the chroma plane row
            // count, which `chroma_height()` already encodes.
            let mut vaapi_device: Option<VaapiDevice> = None;
            let mut hw_frames_ref: *mut AVBufferRef = std::ptr::null_mut();
            let is_vaapi = matches!(
                config.codec,
                VideoEncoderCodec::H264Vaapi | VideoEncoderCodec::HevcVaapi
            );
            if is_vaapi {
                (*ctx).pix_fmt = AVPixelFormat_AV_PIX_FMT_VAAPI;

                let device = match VaapiDevice::open(None) {
                    Ok(d) => d,
                    Err(video_codec::VideoError::HwDeviceCreate(code)) => {
                        let mut c = ctx;
                        avcodec_free_context(&mut c);
                        return Err(VideoEncoderError::OpenCodec(code));
                    }
                    Err(_) => {
                        let mut c = ctx;
                        avcodec_free_context(&mut c);
                        return Err(VideoEncoderError::OpenCodec(-22)); // EINVAL
                    }
                };
                (*ctx).hw_device_ctx = device.new_buffer_ref();

                let sw_format = vaapi_sw_format_for(config.chroma, config.bit_depth);

                // Pool size: encoder reorder window (`max_b_frames + 1`)
                // + in-flight upload surface + a small headroom. VAAPI's
                // surface pool is fixed-size; `av_hwframe_get_buffer`
                // returns ENOMEM once the pool is exhausted.
                let pool_size = (config.max_b_frames as i32 + 1).max(2) + 4;
                let frames_ref = match crate::vaapi::allocate_hw_frames_ctx(
                    &device,
                    config.width as i32,
                    config.height as i32,
                    sw_format,
                    pool_size,
                ) {
                    Ok(r) => r,
                    Err(video_codec::VideoError::HwFramesInit(code)) => {
                        let mut c = ctx;
                        avcodec_free_context(&mut c);
                        drop(device);
                        return Err(VideoEncoderError::OpenCodec(code));
                    }
                    Err(_) => {
                        let mut c = ctx;
                        avcodec_free_context(&mut c);
                        drop(device);
                        return Err(VideoEncoderError::OpenCodec(-22)); // EINVAL
                    }
                };
                (*ctx).hw_frames_ctx = av_buffer_ref(frames_ref);
                hw_frames_ref = frames_ref;
                vaapi_device = Some(device);
            }

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
                if !hw_frames_ref.is_null() {
                    av_buffer_unref(&mut hw_frames_ref);
                }
                drop(vaapi_device);
                return Err(VideoEncoderError::OpenCodec(ret));
            }

            let frame = av_frame_alloc();
            if frame.is_null() {
                let mut c = ctx;
                avcodec_free_context(&mut c);
                if !hw_frames_ref.is_null() {
                    av_buffer_unref(&mut hw_frames_ref);
                }
                drop(vaapi_device);
                return Err(VideoEncoderError::AllocFrame);
            }
            // Sysmem source frame for the VAAPI upload path; null for SW.
            let mut sw_frame: *mut AVFrame = std::ptr::null_mut();
            if is_vaapi {
                // Don't pre-allocate the encoder-input frame's buffer.
                // Per `encode_frame` call we `av_frame_unref` to release
                // the prior surface and `av_hwframe_get_buffer` to pull
                // a fresh one from the pool.
                (*frame).width = (*ctx).width;
                (*frame).height = (*ctx).height;
                (*frame).format = AVPixelFormat_AV_PIX_FMT_VAAPI;

                // Sysmem source frame in NV12 layout — populated per
                // call from the caller's planar Y/U/V then handed to
                // `av_hwframe_transfer_data`.
                let s = av_frame_alloc();
                if s.is_null() {
                    let mut f = frame;
                    let mut c = ctx;
                    av_frame_free(&mut f);
                    avcodec_free_context(&mut c);
                    if !hw_frames_ref.is_null() {
                        av_buffer_unref(&mut hw_frames_ref);
                    }
                    drop(vaapi_device);
                    return Err(VideoEncoderError::AllocFrame);
                }
                (*s).width = config.width as i32;
                (*s).height = config.height as i32;
                // Match the surface layout: NV12 / P010LE for 4:2:0,
                // NV16 / P210LE for 4:2:2. `av_hwframe_transfer_data`
                // requires the sysmem source's `format` to equal the
                // VAAPI hw_frames_ctx `sw_format`.
                (*s).format = vaapi_sw_format_for(config.chroma, config.bit_depth);
                let ret = av_frame_get_buffer(s, 32);
                if ret < 0 {
                    let mut sf = s;
                    let mut f = frame;
                    let mut c = ctx;
                    av_frame_free(&mut sf);
                    av_frame_free(&mut f);
                    avcodec_free_context(&mut c);
                    if !hw_frames_ref.is_null() {
                        av_buffer_unref(&mut hw_frames_ref);
                    }
                    drop(vaapi_device);
                    return Err(VideoEncoderError::AllocFrameBuffer(ret));
                }
                sw_frame = s;
            } else {
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
            }

            let packet = av_packet_alloc();
            if packet.is_null() {
                let mut f = frame;
                let mut c = ctx;
                av_frame_free(&mut f);
                if !sw_frame.is_null() {
                    let mut sf = sw_frame;
                    av_frame_free(&mut sf);
                }
                avcodec_free_context(&mut c);
                if !hw_frames_ref.is_null() {
                    av_buffer_unref(&mut hw_frames_ref);
                }
                drop(vaapi_device);
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
                vaapi_device,
                hw_frames_ref,
                sw_frame,
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
            if self.is_vaapi() {
                self.encode_frame_vaapi(y, y_stride, u, u_stride, v, v_stride, hh, ww, pts)
            } else {
                self.encode_frame_sw(y, y_stride, u, u_stride, v, v_stride, hh, pts)
            }
        }
    }

    /// True when this encoder is opened against a VAAPI backend.
    fn is_vaapi(&self) -> bool {
        matches!(
            self.codec,
            VideoEncoderCodec::H264Vaapi | VideoEncoderCodec::HevcVaapi
        )
    }

    /// SW encode path: copy three planar Y/U/V planes into the
    /// pre-allocated `self.frame` buffer and send straight to the
    /// encoder. Used by libx264 / libx265 / NVENC / QSV.
    unsafe fn encode_frame_sw(
        &mut self,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
        hh: usize,
        pts: Option<i64>,
    ) -> Result<Vec<EncodedVideoFrame>, VideoEncoderError> {
        let h = self.height as usize;
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

    /// VAAPI encode path: pack the caller's planar YUV planes into the
    /// sysmem `sw_frame` (NV12 / NV16 / P010LE / P210LE layout — Y
    /// plane direct copy, U/V planes byte-interleaved into the chroma
    /// plane), pull a fresh VAAPI surface from the pool via
    /// `av_hwframe_get_buffer`, upload via `av_hwframe_transfer_data`,
    /// and send the HW frame to the encoder. AMD VCN (radeonsi) and
    /// Intel iHD both follow this pattern; the surface stays
    /// GPU-resident through the encode.
    ///
    /// 8-bit and 10-bit paths differ only in the per-sample width and
    /// (for 10-bit) the upper-10-bit shift libavutil's P010 / P210
    /// formats expect — `YUV420P10LE` / `YUV422P10LE` source samples
    /// store valid bits in the lower 10 of a 16-bit word, P010 / P210
    /// surfaces expect them in the upper 10 (the lower 6 zeroed).
    unsafe fn encode_frame_vaapi(
        &mut self,
        y: &[u8],
        y_stride: usize,
        u: &[u8],
        u_stride: usize,
        v: &[u8],
        v_stride: usize,
        hh: usize,
        ww: usize,
        pts: Option<i64>,
    ) -> Result<Vec<EncodedVideoFrame>, VideoEncoderError> {
        let h = self.height as usize;
        let w = self.width as usize;

        if self.bit_depth == 8 {
            // 8-bit path: NV12 (4:2:0) or NV16 (4:2:2). Y plane is a
            // direct byte copy; chroma plane interleaves U + V bytes.
            // `hh` differentiates the two layouts: H/2 for 4:2:0,
            // H for 4:2:2 (chroma_height varies; chroma_width is W/2
            // for both).
            copy_plane(
                (*self.sw_frame).data[0],
                (*self.sw_frame).linesize[0],
                y,
                y_stride,
                h,
            );
            interleave_uv_8bit(
                (*self.sw_frame).data[1],
                (*self.sw_frame).linesize[1],
                u,
                u_stride,
                v,
                v_stride,
                ww,
                hh,
            );
        } else {
            // 10-bit path: P010LE (4:2:0) or P210LE (4:2:2). Y plane
            // is a 16-bit-per-sample copy with the low-10 → upper-10
            // shift; chroma plane interleaves 16-bit U + V samples
            // with the same shift.
            copy_plane_10bit_lo_to_hi(
                (*self.sw_frame).data[0],
                (*self.sw_frame).linesize[0],
                y,
                y_stride,
                h,
                w,
            );
            interleave_uv_10bit_lo_to_hi(
                (*self.sw_frame).data[1],
                (*self.sw_frame).linesize[1],
                u,
                u_stride,
                v,
                v_stride,
                ww,
                hh,
            );
        }

        let frame_pts = pts.unwrap_or(self.frame_count);
        self.frame_count = frame_pts + 1;
        (*self.sw_frame).pts = frame_pts;

        // Release any prior VAAPI surface ref the encoder no longer
        // needs (the encoder retains its own ref for in-flight frames),
        // then pull a fresh surface from the hw_frames pool. ENOMEM
        // here means the pool is exhausted — caller should reduce
        // concurrent in-flight frames or increase pool_size at open.
        av_frame_unref(self.frame);
        let ret = av_hwframe_get_buffer(self.hw_frames_ref, self.frame, 0);
        if ret < 0 {
            return Err(VideoEncoderError::AllocFrameBuffer(ret));
        }

        // Upload sysmem NV12 → VAAPI surface. Format must match
        // hw_frames_ctx.sw_format (set to NV12 at open time); FFmpeg
        // returns EINVAL otherwise — `av_hwframe_transfer_data` does
        // NOT do format conversion, so we packed NV12 explicitly above.
        let ret = av_hwframe_transfer_data(self.frame, self.sw_frame, 0);
        if ret < 0 {
            return Err(VideoEncoderError::SendFrame(ret));
        }

        (*self.frame).pts = frame_pts;
        if self.force_idr_next {
            (*self.frame).pict_type = AVPictureType_AV_PICTURE_TYPE_I;
            self.force_idr_next = false;
        } else {
            (*self.frame).pict_type = AVPictureType_AV_PICTURE_TYPE_NONE;
        }

        self.send_and_receive()
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
            if !self.sw_frame.is_null() {
                av_frame_free(&mut self.sw_frame);
            }
            if !self.ctx.is_null() {
                // `avcodec_free_context` unrefs `hw_device_ctx` and
                // `hw_frames_ctx` that the codec owned; our parallel
                // `hw_frames_ref` ref is released next.
                avcodec_free_context(&mut self.ctx);
            }
            if !self.hw_frames_ref.is_null() {
                av_buffer_unref(&mut self.hw_frames_ref);
            }
            // `vaapi_device: Option<VaapiDevice>` drops here naturally,
            // releasing the last AVHWDeviceContext ref.
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

/// Copy a 10-bit Y plane from `YUV420P10LE` / `YUV422P10LE` source
/// (16-bit samples, valid bits in the LOWER 10) into a P010LE / P210LE
/// destination (16-bit samples, valid bits in the UPPER 10) — i.e. one
/// `u16 << 6` per sample.
///
/// `width_samples` is the luma plane width in samples (one sample = 2
/// bytes both source and destination). The destination's `dst_stride`
/// is in bytes — typically `width_samples * 2` plus libavutil
/// alignment padding.
///
/// Per-sample throughput is the v1 implementation (no SIMD); broadcast
/// 1080p30 at 10-bit is well under 100 MB/s and stays inside the
/// `block_in_place` worker without touching the reactor.
unsafe fn copy_plane_10bit_lo_to_hi(
    dst: *mut u8,
    dst_stride: i32,
    src: &[u8],
    src_stride: usize,
    rows: usize,
    width_samples: usize,
) {
    let dst_stride = dst_stride as usize;
    for r in 0..rows {
        let dst_row = dst.add(r * dst_stride) as *mut u16;
        let src_row = src.as_ptr().add(r * src_stride) as *const u16;
        for c in 0..width_samples {
            let s = src_row.add(c).read_unaligned();
            dst_row.add(c).write_unaligned(s << 6);
        }
    }
}

/// Pack two 10-bit chroma planes (`u`, `v` — `YUV420P10LE` /
/// `YUV422P10LE` layout, valid bits in the LOWER 10 of a 16-bit word)
/// into P010LE / P210LE chroma layout — `[U0, V0, U1, V1, …]`
/// 16-bit-interleaved, with each sample shifted left by 6 to land its
/// valid bits in the UPPER 10 bits as the P010 / P210 surface formats
/// expect.
///
/// `width_chroma` is the chroma plane width in *samples* (luma width
/// divided by 2 for both 4:2:0 and 4:2:2 — VAAPI's
/// horizontal-subsampling shape). `rows` is the chroma plane height
/// (`H/2` for 4:2:0, `H` for 4:2:2).
unsafe fn interleave_uv_10bit_lo_to_hi(
    dst: *mut u8,
    dst_stride: i32,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width_chroma: usize,
    rows: usize,
) {
    let dst_stride = dst_stride as usize;
    for r in 0..rows {
        let dst_row = dst.add(r * dst_stride) as *mut u16;
        let u_row = u.as_ptr().add(r * u_stride) as *const u16;
        let v_row = v.as_ptr().add(r * v_stride) as *const u16;
        for c in 0..width_chroma {
            let us = u_row.add(c).read_unaligned();
            let vs = v_row.add(c).read_unaligned();
            dst_row.add(c * 2).write_unaligned(us << 6);
            dst_row.add(c * 2 + 1).write_unaligned(vs << 6);
        }
    }
}

/// Pack two single-byte planes (`u`, `v`) into NV12 / NV16 chroma
/// layout — `[U0, V0, U1, V1, …]` byte-interleaved, one row at a time.
///
/// `dst` is the chroma plane (`AVFrame.data[1]`), `dst_stride` its
/// linesize (typically `width` bytes — full luma width because U+V
/// alternate at half width × 2 bytes-per-pair). `width_chroma` is the
/// chroma plane width in samples (luma width / 2) and `rows` the
/// chroma plane height (`H/2` for NV12 4:2:0, `H` for NV16 4:2:2).
///
/// 8-bit only — the 10-bit P010 / P210 path lives in
/// [`interleave_uv_10bit_lo_to_hi`].
unsafe fn interleave_uv_8bit(
    dst: *mut u8,
    dst_stride: i32,
    u: &[u8],
    u_stride: usize,
    v: &[u8],
    v_stride: usize,
    width_chroma: usize,
    rows: usize,
) {
    let dst_stride = dst_stride as usize;
    for r in 0..rows {
        let dst_row = dst.add(r * dst_stride);
        let u_row = u.as_ptr().add(r * u_stride);
        let v_row = v.as_ptr().add(r * v_stride);
        for c in 0..width_chroma {
            *dst_row.add(c * 2) = *u_row.add(c);
            *dst_row.add(c * 2 + 1) = *v_row.add(c);
        }
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
