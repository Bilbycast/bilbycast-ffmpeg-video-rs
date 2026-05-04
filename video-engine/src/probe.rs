// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Lightweight runtime probes for FFmpeg encoder / decoder availability.
//!
//! Two layers:
//!
//! 1. [`is_encoder_available`] / [`is_decoder_available`] — cheap
//!    `avcodec_find_*_by_name` registry check. Confirms the codec is
//!    **compiled in**. Does NOT verify a session can actually be opened.
//! 2. [`probe_open_encoder`] / [`probe_open_decoder`] — runtime
//!    `avcodec_open2` round-trip with a minimal config. Distinguishes
//!    "compiled in but no driver / no GPU / no permissions" from
//!    "actually usable". Plus [`count_max_encoder_sessions`] /
//!    [`count_max_decoder_sessions`] which open contexts in a loop until
//!    one fails to estimate concurrent-session capacity (NVENC consumer
//!    cards cap at 3–8, QSV varies by Intel iGPU generation).
//!
//! All probes are safe to call multiple times; each context is freed
//! before returning.

use libffmpeg_video_sys::*;

/// AVERROR_EOF = -FFERRTAG('E','O','F',' ').
const _AVERROR_EOF: i32 = -541478725;

/// AVERROR(EAGAIN) — "encoder busy, try again".
#[cfg(target_os = "macos")]
const AVERROR_EAGAIN: i32 = -35; // macOS EAGAIN = 35
#[cfg(not(target_os = "macos"))]
const AVERROR_EAGAIN: i32 = -11; // Linux EAGAIN = 11

/// Outcome of a runtime probe-open attempt. Surfaces the FFmpeg error
/// classification at a level the resource-budget caller cares about —
/// it doesn't need the raw errno, just "is the accelerator usable?"
/// plus enough detail to log a useful diagnostic.
#[derive(Debug, Clone)]
pub enum ProbeError {
    /// `avcodec_find_*_by_name` returned NULL — codec wasn't compiled into
    /// the vendored FFmpeg. Should never happen if the boolean
    /// pre-filter ran first.
    NotCompiled,
    /// `avcodec_alloc_context3` returned NULL.
    AllocFailed,
    /// `avcodec_open2` returned `EAGAIN` — encoder slot transiently busy
    /// (typical of NVENC under session pressure). Retry-once before
    /// declaring unavailable.
    Busy,
    /// `avcodec_open2` returned `ENOSYS` / `ENODEV` / `ENOENT` — driver or
    /// hardware missing. Permanent until reboot / driver install.
    DriverMissing,
    /// `avcodec_open2` returned `EACCES` — typical of QSV when the
    /// running user can't open `/dev/dri/renderD128`. Permission, not a
    /// missing-driver issue.
    PermissionDenied,
    /// `avcodec_open2` succeeded but the decoder's advertised output
    /// pixel formats contain only HW-surface formats (e.g. AV_PIX_FMT_VAAPI
    /// alone for a VAAPI decoder without hwdevice context wiring) —
    /// nothing the safe wrapper can drain through its `*_planes`
    /// accessors. Keeps the cost-plan resolver from picking a backend
    /// that opens cleanly but produces frames we can't read.
    NoReadablePixelFormat,
    /// Any other FFmpeg error. Holds the raw negative errno / AVERROR.
    OpenFailed(i32),
}

impl ProbeError {
    /// Classify a raw FFmpeg `avcodec_open2` return code into a
    /// [`ProbeError`].
    fn from_avcodec_ret(ret: i32) -> Self {
        // FFmpeg negates POSIX errnos on POSIX (`AVERROR(errno)`) so the
        // negative numbers below match `errno.h` directly.
        match ret {
            AVERROR_EAGAIN => ProbeError::Busy,
            -2 | -19 | -38 => ProbeError::DriverMissing, // ENOENT / ENODEV / ENOSYS
            -13 => ProbeError::PermissionDenied,         // EACCES
            other => ProbeError::OpenFailed(other),
        }
    }

    /// Short tag for log lines.
    pub fn as_tag(&self) -> &'static str {
        match self {
            ProbeError::NotCompiled => "not_compiled",
            ProbeError::AllocFailed => "alloc_failed",
            ProbeError::Busy => "busy",
            ProbeError::DriverMissing => "driver_missing",
            ProbeError::PermissionDenied => "permission_denied",
            ProbeError::NoReadablePixelFormat => "no_readable_pixfmt",
            ProbeError::OpenFailed(_) => "open_failed",
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::NotCompiled => write!(f, "codec not compiled into vendored FFmpeg"),
            ProbeError::AllocFailed => write!(f, "avcodec_alloc_context3 returned NULL"),
            ProbeError::Busy => write!(f, "avcodec_open2 EAGAIN (encoder busy)"),
            ProbeError::DriverMissing => write!(f, "avcodec_open2 driver/device missing"),
            ProbeError::PermissionDenied => {
                write!(f, "avcodec_open2 EACCES (check /dev/dri permissions)")
            }
            ProbeError::NoReadablePixelFormat => write!(
                f,
                "decoder advertises only HW-surface pixel formats; nothing drainable"
            ),
            ProbeError::OpenFailed(code) => write!(f, "avcodec_open2 failed (code {})", code),
        }
    }
}

/// Returns `true` if the given encoder name is compiled into the vendored
/// FFmpeg build. Names follow FFmpeg's naming convention — see
/// <https://ffmpeg.org/ffmpeg-codecs.html>.
///
/// This is a fast registry check — it does **not** verify the encoder can
/// actually open a session. Use [`probe_open_encoder`] for that.
pub fn is_encoder_available(name: &str) -> bool {
    let Ok(cstr) = std::ffi::CString::new(name) else {
        return false;
    };
    unsafe { !avcodec_find_encoder_by_name(cstr.as_ptr()).is_null() }
}

/// Returns `true` if the given decoder name is compiled into the vendored
/// FFmpeg build. Same caveats as [`is_encoder_available`].
pub fn is_decoder_available(name: &str) -> bool {
    let Ok(cstr) = std::ffi::CString::new(name) else {
        return false;
    };
    unsafe { !avcodec_find_decoder_by_name(cstr.as_ptr()).is_null() }
}

/// Try to actually **open** an encoder session with a minimal config and
/// immediately close it. Confirms not just that the codec is compiled in
/// (registry check) but that the host has the driver / GPU / permissions
/// needed to instantiate a session.
///
/// Used at edge startup so the resource-budget probe reports
/// "NVENC available" only when NVENC actually works, not just because the
/// `--features video-encoder-nvenc` build had nv-codec-headers at compile
/// time.
///
/// Minimal config (320×240 @ 25 fps, YUV420P, 1 Mbps, GOP 25) is chosen to
/// satisfy every supported backend's pixel-format and frame-rate gates.
/// Returns `Ok(())` when the encoder opens cleanly.
pub fn probe_open_encoder(name: &str) -> Result<(), ProbeError> {
    probe_open_encoder_chroma(name, ProbeChroma::Yuv420_8bit)
}

/// Like [`probe_open_encoder`] but verifies a specific chroma +
/// bit-depth combination. Returns `Err(ProbeError::NotCompiled)` for
/// `(codec, chroma)` pairs that the codec definitely doesn't support
/// (e.g. NVENC + any 4:2:2, h264_nvenc + 10-bit, h264_qsv + 10-bit) —
/// the probe doesn't even attempt `avcodec_open2`, treating "not in
/// the matrix" as equivalent to "not compiled in" so the caller can
/// fold the result into a single boolean per (codec, chroma).
pub fn probe_open_encoder_chroma(name: &str, chroma: ProbeChroma) -> Result<(), ProbeError> {
    let cstr = std::ffi::CString::new(name).map_err(|_| ProbeError::NotCompiled)?;
    let Some(pix_fmt) = probe_pix_fmt_for_chroma(name, chroma) else {
        return Err(ProbeError::NotCompiled);
    };
    unsafe {
        let codec_ptr = avcodec_find_encoder_by_name(cstr.as_ptr());
        if codec_ptr.is_null() {
            return Err(ProbeError::NotCompiled);
        }
        try_open_encoder_context(codec_ptr, pix_fmt).map(|ctx| free_encoder_context(ctx))
    }
}

/// Decoder twin of [`probe_open_encoder`]. Verifies that the named
/// decoder (e.g. `h264_cuvid` for NVDEC) can be opened on the host.
pub fn probe_open_decoder(name: &str) -> Result<(), ProbeError> {
    let cstr = std::ffi::CString::new(name).map_err(|_| ProbeError::NotCompiled)?;
    unsafe {
        let codec_ptr = avcodec_find_decoder_by_name(cstr.as_ptr());
        if codec_ptr.is_null() {
            return Err(ProbeError::NotCompiled);
        }
        try_open_decoder_context(codec_ptr).map(|ctx| free_encoder_context(ctx))
    }
}

/// Probe how many concurrent encoder sessions of the named codec the host
/// will support. Opens contexts in a loop until one fails or `upper_bound`
/// is reached, returning the count of successful opens. Releases every
/// context before returning.
///
/// On NVENC consumer cards (3–5 session cap depending on driver patch),
/// this triggers the cap right at startup — returns 3, 4, or 5. On pro
/// cards the cap is far higher; we report `upper_bound` to keep startup
/// fast and just say "≥N". Default `upper_bound` from the caller is 8.
///
/// Returns `0` if the very first open fails (codec is unavailable at
/// runtime — caller should already have filtered via
/// [`probe_open_encoder`]).
pub fn count_max_encoder_sessions(name: &str, upper_bound: u32) -> u32 {
    let cstr = match std::ffi::CString::new(name) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let pix_fmt = match probe_pix_fmt_for_chroma(name, ProbeChroma::Yuv420_8bit) {
        Some(f) => f,
        None => return 0,
    };
    unsafe {
        let codec_ptr = avcodec_find_encoder_by_name(cstr.as_ptr());
        if codec_ptr.is_null() {
            return 0;
        }
        let mut held: Vec<*mut AVCodecContext> = Vec::with_capacity(upper_bound as usize);
        let mut count: u32 = 0;
        while count < upper_bound {
            match try_open_encoder_context(codec_ptr, pix_fmt) {
                Ok(ctx) => {
                    held.push(ctx);
                    count += 1;
                }
                Err(_) => break,
            }
        }
        for ctx in held.drain(..) {
            free_encoder_context(ctx);
        }
        count
    }
}

/// Decoder twin of [`count_max_encoder_sessions`].
pub fn count_max_decoder_sessions(name: &str, upper_bound: u32) -> u32 {
    let cstr = match std::ffi::CString::new(name) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    unsafe {
        let codec_ptr = avcodec_find_decoder_by_name(cstr.as_ptr());
        if codec_ptr.is_null() {
            return 0;
        }
        let mut held: Vec<*mut AVCodecContext> = Vec::with_capacity(upper_bound as usize);
        let mut count: u32 = 0;
        while count < upper_bound {
            match try_open_decoder_context(codec_ptr) {
                Ok(ctx) => {
                    held.push(ctx);
                    count += 1;
                }
                Err(_) => break,
            }
        }
        for ctx in held.drain(..) {
            free_encoder_context(ctx);
        }
        count
    }
}

// ── internals ──────────────────────────────────────────────────────

/// Minimum probe frame size — chosen to satisfy NVENC's 145-pixel minimum
/// width and QSV's even-dimension requirement. 320×240 is universally
/// accepted across libx264 / libx265 / NVENC / QSV.
const PROBE_WIDTH: i32 = 320;
const PROBE_HEIGHT: i32 = 240;
const PROBE_FPS_NUM: i32 = 25;
const PROBE_BITRATE: i64 = 1_000_000;
const PROBE_GOP: i32 = 25;

/// Chroma + bit-depth axis the probe tests against. The mapping to
/// `AVPixelFormat` is per-codec — see [`probe_pix_fmt_for_chroma`].
/// Identifies the four combinations broadcast / contribution flows
/// most commonly need:
///
/// * `Yuv420_8bit` — consumer/contribution baseline, every HW backend
///   supports this when the codec is available at all.
/// * `Yuv422_8bit` — SDI-broadcast standard. QSV supports it on Coffee
///   Lake (8th gen) Intel iGPUs and newer; NVENC never supports it;
///   AMF rarely; libx264/libx265 always (via High422/Main422 profiles).
/// * `Yuv420_10bit` — 10-bit HDR (HEVC Main10 baseline). hevc_qsv and
///   hevc_nvenc support it on most modern hardware; h264_qsv /
///   h264_nvenc do not (H.264 Main10 isn't real).
/// * `Yuv422_10bit` — SDI broadcast at 10-bit. QSV supports it on
///   Tiger Lake (11th gen) and newer for HEVC; libx265 supports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProbeChroma {
    Yuv420_8bit,
    Yuv422_8bit,
    Yuv420_10bit,
    Yuv422_10bit,
}

impl ProbeChroma {
    pub fn label(&self) -> &'static str {
        match self {
            ProbeChroma::Yuv420_8bit => "yuv420p",
            ProbeChroma::Yuv422_8bit => "yuv422p",
            ProbeChroma::Yuv420_10bit => "yuv420p10le",
            ProbeChroma::Yuv422_10bit => "yuv422p10le",
        }
    }
}

/// Pick a pixel format the named codec accepts at open time for the
/// requested chroma + bit-depth combination. Returns `None` for combos
/// the codec definitely doesn't support — the probe skips those rather
/// than spending a session slot on a known-bad open. The mapping
/// follows FFmpeg's per-encoder `pix_fmts` array semantics:
///
/// * **QSV (`h264_qsv` / `hevc_qsv`)** — NV12 (4:2:0 8-bit), YUYV422
///   (4:2:2 8-bit), P010LE (4:2:0 10-bit), Y210LE (4:2:2 10-bit).
///   QSV has no 4:2:2 8-bit input format for `h264_qsv` on every
///   driver version, but YUYV422 is the documented entry that libvpl
///   accepts when the iGPU supports 4:2:2 — older iGPUs return
///   `EINVAL` here, which is the *correct* signal of "not supported
///   on this hardware".
/// * **NVENC (`h264_nvenc` / `hevc_nvenc`)** — YUV420P (4:2:0 8-bit),
///   YUV420P10LE (4:2:0 10-bit on hevc_nvenc; h264_nvenc skips this).
///   NVENC has *no* 4:2:2 input format, so 4:2:2 probes return `None`
///   and the family always reports yuv422 unsupported.
/// * **AMF** — YUV420P (4:2:0 8-bit), NV16 (4:2:2 8-bit, hevc_amf
///   only), P010LE (4:2:0 10-bit, hevc_amf only). H.264 AMF is 4:2:0
///   8-bit only.
/// * **VideoToolbox** — system-managed; we don't probe at all on
///   non-macOS, and macOS short-circuits the probe entirely.
/// * **libx264 / libx265** — YUV420P / YUV422P / YUV420P10LE /
///   YUV422P10LE. Both software encoders accept the full matrix when
///   the matching profile is compiled in (the bilbycast-vendored
///   build has High10 / High422 / High444 / Main10 enabled).
fn probe_pix_fmt_for_chroma(name: &str, chroma: ProbeChroma) -> Option<AVPixelFormat> {
    let lower = name.to_ascii_lowercase();
    let is_qsv = lower.contains("qsv");
    let is_nvenc = lower.contains("nvenc");
    let is_amf = lower.contains("amf");
    let is_h264 = lower.contains("h264") || lower == "libx264";
    let is_hevc = lower.contains("hevc") || lower.contains("h265") || lower == "libx265";

    match chroma {
        ProbeChroma::Yuv420_8bit => Some(if is_qsv {
            AVPixelFormat_AV_PIX_FMT_NV12
        } else {
            AVPixelFormat_AV_PIX_FMT_YUV420P
        }),
        ProbeChroma::Yuv422_8bit => {
            if is_nvenc {
                // NVENC has no 4:2:2 path on any GPU generation. Skip
                // the probe outright — reporting "not supported" without
                // burning a session slot.
                None
            } else if is_qsv {
                Some(AVPixelFormat_AV_PIX_FMT_YUYV422)
            } else if is_amf {
                if is_hevc {
                    Some(AVPixelFormat_AV_PIX_FMT_NV16)
                } else {
                    None // h264_amf is 4:2:0 only
                }
            } else {
                // libx264 / libx265 — planar 4:2:2 is the natural input
                // when the High422 / Main422 profile is enabled.
                Some(AVPixelFormat_AV_PIX_FMT_YUV422P)
            }
        }
        ProbeChroma::Yuv420_10bit => {
            if is_h264 && (is_nvenc || is_qsv || is_amf) {
                // H.264 has no Main10 profile on any HW backend. Skip.
                None
            } else if is_qsv {
                Some(AVPixelFormat_AV_PIX_FMT_P010LE)
            } else {
                // hevc_nvenc / hevc_amf / libx265 / libx264 (the
                // High10 profile is enabled in the vendored build).
                Some(AVPixelFormat_AV_PIX_FMT_YUV420P10LE)
            }
        }
        ProbeChroma::Yuv422_10bit => {
            if is_h264 && (is_nvenc || is_qsv || is_amf) {
                None
            } else if is_nvenc {
                None // NVENC: no 4:2:2 at any depth
            } else if is_amf {
                None // AMF: 4:2:2 10-bit not in libavcodec mapping
            } else if is_qsv {
                Some(AVPixelFormat_AV_PIX_FMT_Y210LE)
            } else {
                // libx264 (High422 + High10) / libx265 (Main422 10).
                Some(AVPixelFormat_AV_PIX_FMT_YUV422P10LE)
            }
        }
    }
}

/// Allocate + configure an encoder `AVCodecContext` and call
/// `avcodec_open2`. Returns the opened context on success — caller must
/// free via [`free_encoder_context`]. Reused by both the one-shot
/// probe-open and the loop-open session-count probe.
unsafe fn try_open_encoder_context(
    codec_ptr: *const AVCodec,
    pix_fmt: AVPixelFormat,
) -> Result<*mut AVCodecContext, ProbeError> {
    let ctx = avcodec_alloc_context3(codec_ptr);
    if ctx.is_null() {
        return Err(ProbeError::AllocFailed);
    }
    (*ctx).width = PROBE_WIDTH;
    (*ctx).height = PROBE_HEIGHT;
    (*ctx).pix_fmt = pix_fmt;
    (*ctx).time_base.num = 1;
    (*ctx).time_base.den = PROBE_FPS_NUM;
    (*ctx).framerate.num = PROBE_FPS_NUM;
    (*ctx).framerate.den = 1;
    (*ctx).bit_rate = PROBE_BITRATE;
    (*ctx).gop_size = PROBE_GOP;
    (*ctx).max_b_frames = 0;

    let ret = avcodec_open2(ctx, codec_ptr, std::ptr::null_mut());
    if ret < 0 {
        let mut c = ctx;
        avcodec_free_context(&mut c);
        return Err(ProbeError::from_avcodec_ret(ret));
    }
    Ok(ctx)
}

/// Allocate + configure a decoder `AVCodecContext` and call
/// `avcodec_open2`. Decoders auto-detect dimensions from the first
/// packet, so we leave width/height at 0 — the open call only needs the
/// codec descriptor + sane time_base.
///
/// After `avcodec_open2` succeeds we additionally walk the codec's
/// advertised `pix_fmts` list and reject decoders whose only output
/// formats are HW-surface formats we have no `*_planes` accessor for
/// (`AV_PIX_FMT_VAAPI`, `AV_PIX_FMT_QSV`, `AV_PIX_FMT_CUDA`, ...).
/// Rationale — `h264_vaapi` opens cleanly without an `AVHWDeviceContext`
/// but produces only `AV_PIX_FMT_VAAPI` surfaces that the safe wrapper
/// can't drain into system memory; the cost-plan resolver would then
/// happily route a display output to a backend whose first frame
/// silently disappears. Catching this at probe time is the cheap half
/// of "is this backend actually usable end-to-end" — the runtime
/// watchdog in bilbycast-edge's `output_display.rs` covers the
/// "advertised format but driver lies" case.
unsafe fn try_open_decoder_context(
    codec_ptr: *const AVCodec,
) -> Result<*mut AVCodecContext, ProbeError> {
    let ctx = avcodec_alloc_context3(codec_ptr);
    if ctx.is_null() {
        return Err(ProbeError::AllocFailed);
    }
    // Allow truncated packets (matches VideoDecoder's open path) so the
    // probe is consistent with how decoders are used at runtime.
    (*ctx).flags2 |= 1 << 1; // AV_CODEC_FLAG2_CHUNKS
    (*ctx).time_base.num = 1;
    (*ctx).time_base.den = PROBE_FPS_NUM;

    let ret = avcodec_open2(ctx, codec_ptr, std::ptr::null_mut());
    if ret < 0 {
        let mut c = ctx;
        avcodec_free_context(&mut c);
        return Err(ProbeError::from_avcodec_ret(ret));
    }

    if !decoder_has_drainable_pix_fmt(codec_ptr) {
        let mut c = ctx;
        avcodec_free_context(&mut c);
        return Err(ProbeError::NoReadablePixelFormat);
    }

    Ok(ctx)
}

/// Walk a decoder's `pix_fmts` array (NULL-terminated by
/// `AV_PIX_FMT_NONE = -1`) and return `true` if at least one entry is
/// a system-memory layout the safe wrapper's `DecodedFrame::*_planes`
/// accessors can drain. Decoders with a NULL `pix_fmts` list (no
/// declared advertisement) are conservatively accepted — that includes
/// most software decoders (`avcodec_find_decoder(H264)`), which always
/// produce planar YUV.
unsafe fn decoder_has_drainable_pix_fmt(codec_ptr: *const AVCodec) -> bool {
    let mut p = (*codec_ptr).pix_fmts;
    if p.is_null() {
        // No advertisement → trust the runtime path to surface anything
        // unreadable. Keeps us from rejecting SW decoders.
        return true;
    }
    while *p != AVPixelFormat_AV_PIX_FMT_NONE {
        if is_drainable_pix_fmt(*p) {
            return true;
        }
        p = p.add(1);
    }
    false
}

/// Mirrors the dispatch chain in bilbycast-edge's `output_display.rs`
/// `drain_video_frames`: every layout that has a working
/// `DecodedFrame::*_planes` accessor is "drainable". Keep this list in
/// lock-step with that file when adding new accessors.
fn is_drainable_pix_fmt(fmt: AVPixelFormat) -> bool {
    // bindgen names like `AVPixelFormat_AV_PIX_FMT_*` are non-upper-case
    // by convention but stable, so use an if-guard chain rather than a
    // match arm (which lints on each constant). One guard per accessor.
    fmt == AVPixelFormat_AV_PIX_FMT_YUV420P
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV420P10LE
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV420P12LE
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV422P
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV422P10LE
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV422P12LE
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV444P
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV444P10LE
        || fmt == AVPixelFormat_AV_PIX_FMT_YUV444P12LE
        || fmt == AVPixelFormat_AV_PIX_FMT_YUVJ420P
        || fmt == AVPixelFormat_AV_PIX_FMT_YUVJ422P
        || fmt == AVPixelFormat_AV_PIX_FMT_YUVJ444P
        || fmt == AVPixelFormat_AV_PIX_FMT_NV12
        || fmt == AVPixelFormat_AV_PIX_FMT_NV16
        || fmt == AVPixelFormat_AV_PIX_FMT_P010LE
        || fmt == AVPixelFormat_AV_PIX_FMT_P016LE
        || fmt == AVPixelFormat_AV_PIX_FMT_P210LE
        || fmt == AVPixelFormat_AV_PIX_FMT_P216LE
}

unsafe fn free_encoder_context(ctx: *mut AVCodecContext) {
    let mut c = ctx;
    avcodec_free_context(&mut c);
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_error_classifies_posix_errnos() {
        assert!(matches!(
            ProbeError::from_avcodec_ret(AVERROR_EAGAIN),
            ProbeError::Busy
        ));
        assert!(matches!(
            ProbeError::from_avcodec_ret(-2),
            ProbeError::DriverMissing
        ));
        assert!(matches!(
            ProbeError::from_avcodec_ret(-19),
            ProbeError::DriverMissing
        ));
        assert!(matches!(
            ProbeError::from_avcodec_ret(-38),
            ProbeError::DriverMissing
        ));
        assert!(matches!(
            ProbeError::from_avcodec_ret(-13),
            ProbeError::PermissionDenied
        ));
        assert!(matches!(
            ProbeError::from_avcodec_ret(-22),
            ProbeError::OpenFailed(-22)
        ));
    }

    #[test]
    fn missing_codec_name_reports_not_compiled() {
        // A codec name that doesn't exist anywhere in FFmpeg.
        assert!(matches!(
            probe_open_encoder("definitely_not_a_real_encoder"),
            Err(ProbeError::NotCompiled)
        ));
        assert!(matches!(
            probe_open_decoder("definitely_not_a_real_decoder"),
            Err(ProbeError::NotCompiled)
        ));
    }

    #[test]
    fn probe_does_not_leak_on_repeat() {
        // Run the same probe twice in one process. Both calls should
        // return identical results — proves the context is freed cleanly
        // between iterations and the second call doesn't trip on leaked
        // state.
        let first = probe_open_encoder("definitely_not_a_real_encoder");
        let second = probe_open_encoder("definitely_not_a_real_encoder");
        assert_eq!(format!("{:?}", first), format!("{:?}", second));
    }

    #[cfg(feature = "video-encoder-x264")]
    #[test]
    fn x264_runtime_open_succeeds() {
        // libx264 is a software encoder — if compiled in, runtime open
        // must succeed regardless of host hardware.
        probe_open_encoder("libx264").expect("libx264 should open at runtime");
    }

    #[test]
    fn drainable_pix_fmt_classifier_covers_planar_and_semiplanar() {
        // Sample of the formats the safe wrapper's accessors handle —
        // sanity check the classifier table stays in lock-step with
        // `output_display.rs::drain_video_frames` so a future renamed
        // pixel-format constant doesn't silently start rejecting probes.
        for fmt in [
            AVPixelFormat_AV_PIX_FMT_YUV420P,
            AVPixelFormat_AV_PIX_FMT_YUV420P10LE,
            AVPixelFormat_AV_PIX_FMT_YUV422P,
            AVPixelFormat_AV_PIX_FMT_YUV422P10LE,
            AVPixelFormat_AV_PIX_FMT_NV12,
            AVPixelFormat_AV_PIX_FMT_NV16,
            AVPixelFormat_AV_PIX_FMT_P010LE,
            AVPixelFormat_AV_PIX_FMT_P210LE,
        ] {
            assert!(is_drainable_pix_fmt(fmt), "fmt {fmt} should be drainable");
        }
        // AV_PIX_FMT_NONE = -1 is the terminator sentinel; never
        // drainable. AV_PIX_FMT_VAAPI = 53, AV_PIX_FMT_QSV = 165, and
        // AV_PIX_FMT_CUDA = 119 are HW-surface formats that signal the
        // decoder needs an `AVHWDeviceContext` we never wired — must
        // not be classified as drainable.
        assert!(!is_drainable_pix_fmt(AVPixelFormat_AV_PIX_FMT_NONE));
        assert!(!is_drainable_pix_fmt(53));
        assert!(!is_drainable_pix_fmt(165));
        assert!(!is_drainable_pix_fmt(119));
    }
}
