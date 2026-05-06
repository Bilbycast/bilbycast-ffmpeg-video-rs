// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! VAAPI hwcontext + DRM PRIME mapping for the bilbycast-edge `display`
//! output's zero-copy scanout path.
//!
//! Pieces:
//!
//! - [`VaapiDevice`] — owns one `AVBufferRef` for an `AV_HWDEVICE_TYPE_VAAPI`
//!   device opened against `/dev/dri/renderD128`. Each [`VideoDecoder`]
//!   opened with [`DecoderBackend::Vaapi`] gets its own — VAAPI's driver
//!   surface pool is per-device, and sharing across decoders has tripped
//!   reference-count races on at least Mesa radeonsi 24.x.
//!
//! - [`vaapi_get_format_callback`] — the C callback FFmpeg invokes after
//!   parsing the first SPS. It walks the codec's advertised formats for
//!   `AV_PIX_FMT_VAAPI`, allocates `hw_frames_ctx` via
//!   `avcodec_get_hw_frames_parameters`, and returns `AV_PIX_FMT_VAAPI`
//!   so subsequent frames stay GPU-resident.
//!
//! - [`DrmPrimeFrame`] — the result of mapping a decoded VAAPI surface
//!   through `av_hwframe_map(DRM_PRIME, ...)`. Carries the
//!   `AVDRMFrameDescriptor` fields the KMS path needs (DMA-BUF fd,
//!   fourcc, modifier, width/height, plane offsets/strides) plus an Arc
//!   keepalive on the underlying `AVFrame`. The keepalive is what lets
//!   the display task hand the descriptor to the page-flip loop and
//!   release it only after the kernel posts `DRM_EVENT_FLIP_COMPLETE` —
//!   without it the VA surface gets reused mid-scanout and the screen
//!   tears or freezes.

use std::ffi::CString;
use std::sync::Arc;

use libffmpeg_video_sys::*;
use video_codec::VideoError;

/// Default render node — modern Linux installs always create
/// `/dev/dri/renderD128` for the first GPU. Multi-GPU hosts (iGPU + dGPU)
/// pick which device VAAPI uses by `LIBVA_DRIVER_NAME` + the render
/// node passed to `av_hwdevice_ctx_create`. Hard-coded for v1; a future
/// revision can let the `display` output pin to a specific render node
/// when the host has more than one.
pub const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";

/// `AV_HWFRAME_MAP_READ | AV_HWFRAME_MAP_DIRECT` — read-only direct-map
/// (no implicit copy). Direct mapping is what makes `vaapi_map_to_drm`
/// hand back DMA-BUF fds rather than copying through system memory.
const HWFRAME_MAP_READ_DIRECT: i32 =
    AV_HWFRAME_MAP_READ as i32 | AV_HWFRAME_MAP_DIRECT as i32;

/// AVERROR code helpers — FFmpeg negates POSIX errnos on POSIX systems.
const AVERROR_ENOSYS: i32 = -38;

/// Owned VAAPI device context. Wraps an `AVBufferRef*` returned by
/// `av_hwdevice_ctx_create(AV_HWDEVICE_TYPE_VAAPI, ...)`.
///
/// Cloning bumps the refcount via `av_buffer_ref` so callers can hand a
/// device handle to multiple consumers (e.g. a hwdevice shared between
/// the decoder and a future zero-copy scaler) without each one owning a
/// separate VAAPI display.
pub struct VaapiDevice {
    /// Non-null `AVBufferRef*` carrying the hwdevice. Wrapped in `Arc`
    /// so cheap clones share the same VAAPI display; the underlying
    /// FFmpeg refcount is only touched on the final drop.
    inner: Arc<VaapiDeviceInner>,
}

struct VaapiDeviceInner {
    /// Raw `AVBufferRef*`. Non-null between construction and Drop.
    /// Stored as `*mut` so we own freeing it; never aliased to other
    /// pointers outside FFmpeg-internal references it makes itself.
    buf: *mut AVBufferRef,
}

// SAFETY: AVBufferRef + the underlying AVHWDeviceContext are
// reference-counted and FFmpeg is internally synchronised for the
// reference-counting operations we perform (`av_buffer_ref`,
// `av_buffer_unref`). Holding one across threads is sound; FFmpeg
// itself documents AVHWDeviceContext as thread-safe to share.
unsafe impl Send for VaapiDeviceInner {}
unsafe impl Sync for VaapiDeviceInner {}

impl VaapiDevice {
    /// Open a VAAPI device on `render_node` (defaults to
    /// [`DEFAULT_RENDER_NODE`] on `None`).
    ///
    /// On failure surfaces `VideoError::HwDeviceCreate` carrying the raw
    /// FFmpeg AVERROR. The two common ones in the wild:
    ///
    /// * `-ENOENT (-2)` — render node doesn't exist (no GPU /
    ///   container without `/dev/dri` passthrough).
    /// * `-EACCES (-13)` — running user isn't in the `render` (or
    ///   `video`) group; no read+write on `/dev/dri/renderD128`.
    pub fn open(render_node: Option<&str>) -> Result<Self, VideoError> {
        let path = render_node.unwrap_or(DEFAULT_RENDER_NODE);
        let cstr = CString::new(path).map_err(|_| VideoError::HwDeviceCreate(-22))?;
        unsafe {
            let mut buf: *mut AVBufferRef = std::ptr::null_mut();
            let ret = av_hwdevice_ctx_create(
                &mut buf,
                AVHWDeviceType_AV_HWDEVICE_TYPE_VAAPI,
                cstr.as_ptr(),
                std::ptr::null_mut(),
                0,
            );
            if ret < 0 {
                return Err(VideoError::HwDeviceCreate(ret));
            }
            if buf.is_null() {
                return Err(VideoError::HwDeviceCreate(-22));
            }
            Ok(Self {
                inner: Arc::new(VaapiDeviceInner { buf }),
            })
        }
    }

    /// Borrow the raw `AVBufferRef*` for one-off FFI calls. The caller
    /// must NOT free or call `av_buffer_unref` on this — the Arc owns
    /// the lifetime. Used by `VideoDecoder` to populate
    /// `codec_ctx->hw_device_ctx` via `av_buffer_ref`.
    pub fn as_ptr(&self) -> *mut AVBufferRef {
        self.inner.buf
    }

    /// Return a fresh `AVBufferRef*` referencing the same device. The
    /// caller takes ownership and must release it via `av_buffer_unref`
    /// (or pass it to FFmpeg, which will). Bumps the underlying refcount.
    pub fn new_buffer_ref(&self) -> *mut AVBufferRef {
        unsafe { av_buffer_ref(self.inner.buf) }
    }
}

impl Clone for VaapiDevice {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for VaapiDeviceInner {
    fn drop(&mut self) {
        unsafe {
            av_buffer_unref(&mut self.buf);
        }
    }
}

/// `get_format` callback FFmpeg invokes once it has parsed the first
/// SPS / VPS and knows what HW pixel formats the codec can deliver. We
/// pin VAAPI when it's offered and lazily allocate the matching
/// `hw_frames_ctx` so the decoder writes frames straight into VAAPI
/// surfaces rather than downloading them to system memory.
///
/// Falls back to the first format on the list when VAAPI isn't
/// advertised — keeps the codec from locking up if the host has the
/// VAAPI feature compiled in but no usable VAAPI driver for this
/// stream's profile.
///
/// # Safety
///
/// Called by FFmpeg on the decode thread. `s` and `fmt` are non-null
/// pointers FFmpeg owns; we only read through them and write to
/// `s->hw_frames_ctx`.
pub unsafe extern "C" fn vaapi_get_format_callback(
    s: *mut AVCodecContext,
    fmt: *const AVPixelFormat,
) -> AVPixelFormat {
    let mut p = fmt;
    while *p != AVPixelFormat_AV_PIX_FMT_NONE {
        if *p == AVPixelFormat_AV_PIX_FMT_VAAPI {
            // Allocate hw_frames_ctx. `avcodec_get_hw_frames_parameters`
            // sizes the frames context based on the codec's required
            // pool depth + the negotiated SW format (e.g. NV12 / P010).
            // It also calls av_hwframe_ctx_alloc internally; we still
            // need to call av_hwframe_ctx_init to make the context
            // usable.
            let mut frames_ref: *mut AVBufferRef = std::ptr::null_mut();
            let device_ref = (*s).hw_device_ctx;
            if device_ref.is_null() {
                // Caller forgot to set hw_device_ctx — fall through
                // to the SW format below so the decoder doesn't crash.
                eprintln!(
                    "vaapi_get_format_callback: hw_device_ctx is NULL, demoting to SW"
                );
                return *fmt;
            }
            let ret = avcodec_get_hw_frames_parameters(
                s,
                device_ref,
                AVPixelFormat_AV_PIX_FMT_VAAPI,
                &mut frames_ref,
            );
            if ret < 0 || frames_ref.is_null() {
                eprintln!(
                    "vaapi_get_format_callback: avcodec_get_hw_frames_parameters failed ({ret}), demoting to SW"
                );
                return *fmt;
            }
            let init_ret = av_hwframe_ctx_init(frames_ref);
            if init_ret < 0 {
                eprintln!(
                    "vaapi_get_format_callback: av_hwframe_ctx_init failed ({init_ret}), demoting to SW"
                );
                let mut tmp = frames_ref;
                av_buffer_unref(&mut tmp);
                return *fmt;
            }
            // Hand ownership of frames_ref to the codec context. Release
            // any previous binding first (VAAPI context surviving a
            // resolution change reuses the codec context but rebuilds
            // the frames context).
            if !(*s).hw_frames_ctx.is_null() {
                let mut prev = (*s).hw_frames_ctx;
                av_buffer_unref(&mut prev);
            }
            (*s).hw_frames_ctx = frames_ref;
            return AVPixelFormat_AV_PIX_FMT_VAAPI;
        }
        p = p.add(1);
    }
    // VAAPI not offered — pick the first SW format the decoder
    // advertises so decoding doesn't stall.
    *fmt
}

/// Mapping outcome — handed back to the bilbycast-edge `display` output
/// for KMS PRIME framebuffer creation.
#[derive(Debug, Clone)]
pub struct DrmPrimePlane {
    /// DMA-BUF file descriptor borrowed from the underlying mapping.
    /// **Owned by [`DrmPrimeKeepalive`]** — the consumer must not close
    /// this fd; it's released when the keepalive's last clone drops.
    pub fd: i32,
    /// Byte offset of this plane within its DMA-BUF object.
    pub offset: u32,
    /// Pitch (line stride) in bytes.
    pub pitch: u32,
}

/// Decoded VAAPI surface mapped to a DRM PRIME descriptor. Lives until
/// every clone of [`Self::keepalive`] drops — the KMS scanout side holds
/// one until after the page-flip event arrives, at which point the
/// surface is safe to recycle into the VAAPI pool.
pub struct DrmPrimeFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `DRM_FORMAT_*` fourcc for the layer (single layer in every
    /// VAAPI mapping we've seen — NV12 / P010 / etc.).
    pub fourcc: u32,
    /// `DRM_FORMAT_MOD_*` modifier or `DRM_FORMAT_MOD_INVALID` when the
    /// driver doesn't advertise a tiling layout.
    pub modifier: u64,
    /// Per-plane DMA-BUF descriptors (1–3 entries, layer-order).
    pub planes: Vec<DrmPrimePlane>,
    /// Arc-shared lifetime guard on the AVFrame the planes' fds came
    /// from. Cloning is cheap (`Arc::clone`) and the KMS path holds one
    /// across the page-flip wait.
    keepalive: Arc<DrmPrimeKeepalive>,
}

impl DrmPrimeFrame {
    /// Cheap clone of the lifetime guard. The KMS task hangs onto this
    /// across the page-flip event — releasing it earlier lets FFmpeg /
    /// libva recycle the VA surface mid-scanout, which on Mesa
    /// radeonsi reads as a green flash, on iHD as garbage chroma. The
    /// guard's drop closes every DMA-BUF fd in `planes` AND releases
    /// the underlying VAAPI surface back into the decoder's pool.
    pub fn keepalive(&self) -> Arc<DrmPrimeKeepalive> {
        self.keepalive.clone()
    }
}

/// Lifetime guard. Wraps the mapped `AVFrame`'s reference counted
/// buffer so the DMA-BUF fds in [`DrmPrimeFrame::planes`] stay open
/// until the last clone drops.
pub struct DrmPrimeKeepalive {
    /// The DRM-PRIME `AVFrame` we mapped to. Its `buf[0]` carries an
    /// `AVBufferRef` whose free-callback closes the per-plane DMA-BUF
    /// fds and releases the source VAAPI frame — see
    /// `vaapi_unmap_to_drm_esh` in libavutil/hwcontext_vaapi.c.
    drm_frame: *mut AVFrame,
}

// SAFETY: AVFrame is reference-counted; we hold one reference and never
// concurrently mutate. The underlying buffers are FFmpeg-owned and
// thread-safe for read.
unsafe impl Send for DrmPrimeKeepalive {}
unsafe impl Sync for DrmPrimeKeepalive {}

impl Drop for DrmPrimeKeepalive {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.drm_frame);
        }
    }
}

/// Map a decoded VAAPI `AVFrame` to a DRM PRIME descriptor.
///
/// `vaapi_frame` must have `format == AV_PIX_FMT_VAAPI` and a valid
/// VA surface in `data[3]` — the standard layout of every frame that
/// comes out of a `h264_vaapi` / `hevc_vaapi` decoder. The caller is
/// responsible for keeping `vaapi_frame` alive across this call (we
/// don't unref it — `av_hwframe_map` clones the reference internally).
///
/// On `Ok` the returned [`DrmPrimeFrame`] carries DMA-BUF fds the KMS
/// path imports via `drmPrimeFDToHandle` + `drmModeAddFB2WithModifiers`.
///
/// # Safety
///
/// `vaapi_frame` must be a non-null pointer to a valid VAAPI-formatted
/// `AVFrame`.
pub unsafe fn map_vaapi_to_drm_prime(
    vaapi_frame: *const AVFrame,
) -> Result<DrmPrimeFrame, VideoError> {
    if vaapi_frame.is_null() {
        return Err(VideoError::HwFrameNotOnDevice);
    }
    if (*vaapi_frame).format != AVPixelFormat_AV_PIX_FMT_VAAPI {
        return Err(VideoError::HwFrameNotOnDevice);
    }

    let drm_frame = av_frame_alloc();
    if drm_frame.is_null() {
        return Err(VideoError::AllocFrame);
    }
    (*drm_frame).format = AVPixelFormat_AV_PIX_FMT_DRM_PRIME;

    let ret = av_hwframe_map(drm_frame, vaapi_frame, HWFRAME_MAP_READ_DIRECT);
    if ret < 0 {
        let mut tmp = drm_frame;
        av_frame_free(&mut tmp);
        // -ENOSYS surfaces when the FFmpeg build doesn't have CONFIG_LIBDRM
        // → vaapi_map_to_drm is compiled out. Surface a more specific
        // diagnostic so the caller can demote to the CPU-blit path with
        // a useful event payload.
        if ret == AVERROR_ENOSYS {
            return Err(VideoError::HwFrameMap(ret));
        }
        return Err(VideoError::HwFrameMap(ret));
    }

    // `data[0]` is `AVDRMFrameDescriptor*` — see the docstring on
    // `AVDRMFrameDescriptor`. We copy the values out (cheap — they're
    // ints + a small array) so the caller doesn't have to chase
    // through FFmpeg-internal pointers.
    let desc_ptr = (*drm_frame).data[0] as *const AVDRMFrameDescriptor;
    if desc_ptr.is_null() {
        let mut tmp = drm_frame;
        av_frame_free(&mut tmp);
        eprintln!("map_vaapi_to_drm_prime: drm_frame.data[0] is NULL");
        return Err(VideoError::HwFrameMap(-22));
    }
    let desc = &*desc_ptr;
    if desc.nb_layers < 1 || desc.nb_objects < 1 {
        let mut tmp = drm_frame;
        av_frame_free(&mut tmp);
        return Err(VideoError::HwFrameMap(-22));
    }

    // VAAPI's `vaapi_map_to_drm` produces one of two descriptor shapes,
    // depending on which export path the libva backend implements:
    //
    // * **ESH** (Export Surface Handle, libva 1.1+) — one layer whose
    //   `format` field is the surface fourcc (NV12 / P010 / …) and
    //   whose planes are the in-place memory planes.
    //
    // * **ABH** (Attribute-Based Handle, older libva) — one layer per
    //   memory plane, each layer carrying a *single-plane* fourcc
    //   (R8 / GR88 for NV12, R16 / GR1616 for P010). The KMS scanout
    //   needs a *single* surface fourcc + per-plane handles, so we
    //   collapse ABH back into a single fourcc by matching the
    //   sequence of per-layer fourccs.
    let nb_layers = desc.nb_layers as usize;
    if nb_layers > AV_DRM_MAX_PLANES as usize {
        let mut tmp = drm_frame;
        av_frame_free(&mut tmp);
        return Err(VideoError::HwFrameMap(-22));
    }

    let modifier = desc.objects[0].format_modifier;

    let mut planes_out: Vec<DrmPrimePlane> = Vec::with_capacity(nb_layers);
    let mut layer_fourccs: Vec<u32> = Vec::with_capacity(nb_layers);
    for layer_idx in 0..nb_layers {
        let layer = &desc.layers[layer_idx];
        if layer.nb_planes != 1 {
            let mut tmp = drm_frame;
            av_frame_free(&mut tmp);
            return Err(VideoError::HwFrameMap(-22));
        }
        let plane = &layer.planes[0];
        let obj_idx = plane.object_index as usize;
        if obj_idx >= desc.nb_objects as usize {
            let mut tmp = drm_frame;
            av_frame_free(&mut tmp);
            return Err(VideoError::HwFrameMap(-22));
        }
        let object = &desc.objects[obj_idx];
        planes_out.push(DrmPrimePlane {
            fd: object.fd,
            offset: plane.offset as u32,
            pitch: plane.pitch as u32,
        });
        layer_fourccs.push(layer.format);
    }

    let width = (*vaapi_frame).width as u32;
    let height = (*vaapi_frame).height as u32;
    let fourcc = match nb_layers {
        // Single layer: ESH path — `format` is the surface fourcc.
        1 => layer_fourccs[0],
        // Two-layer ABH split: combine per-plane fourccs back to the
        // surface fourcc that KMS scanout planes advertise.
        2 => collapse_abh_fourcc(layer_fourccs[0], layer_fourccs[1])
            .ok_or_else(|| {
                let mut tmp = drm_frame;
                av_frame_free(&mut tmp);
                VideoError::HwFrameMap(-22)
            })?,
        // Three-layer ABH split for fully planar 4:2:0 / 4:2:2 / 4:4:4.
        // Not produced by any HEVC / H.264 hwaccel we've seen — surfaces
        // come back as NV12 / P010 / NV16 / P210 — but reject explicitly
        // rather than guess so a future driver doesn't silently
        // mis-render.
        _ => {
            let mut tmp = drm_frame;
            av_frame_free(&mut tmp);
            return Err(VideoError::HwFrameMap(-22));
        }
    };

    Ok(DrmPrimeFrame {
        width,
        height,
        fourcc,
        modifier,
        planes: planes_out,
        keepalive: Arc::new(DrmPrimeKeepalive { drm_frame }),
    })
}

// ── DRM fourcc constants ──────────────────────────────────────────
// Defined inline rather than depending on the kernel's drm_fourcc.h
// (which would force every consumer to add a libdrm-headers dep);
// values come straight from drm_fourcc.h and never change.

const FOURCC_R8: u32 = u32::from_le_bytes(*b"R8  ");
const FOURCC_GR88: u32 = u32::from_le_bytes(*b"GR88");
const FOURCC_R16: u32 = u32::from_le_bytes(*b"R16 ");
const FOURCC_GR1616: u32 = u32::from_le_bytes(*b"GR32");
/// `vaapi_map_to_drm_abh` historically emits the chroma plane as
/// `DRM_FORMAT_GR88` (the kernel's preferred name) but some libva
/// builds use the legacy alias `DRM_FORMAT_RG88`. Treat the two as
/// interchangeable so we don't reject a perfectly valid NV12 split.
const FOURCC_RG88: u32 = u32::from_le_bytes(*b"RG88");
const FOURCC_RG1616: u32 = u32::from_le_bytes(*b"RG32");
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");
const FOURCC_P010: u32 = u32::from_le_bytes(*b"P010");
const FOURCC_NV16: u32 = u32::from_le_bytes(*b"NV16");
const FOURCC_P210: u32 = u32::from_le_bytes(*b"P210");

/// Allocate and initialise an encoder-side `hw_frames_ctx` against the
/// given device. The decoder lazy-allocates this from inside
/// [`vaapi_get_format_callback`] once it has parsed the first SPS, but
/// VAAPI **encoders** require a ready `hw_frames_ctx` set on the codec
/// context **before** `avcodec_open2` — there's no equivalent
/// negotiation point on the encode side.
///
/// `sw_format` is the underlying VAAPI-surface layout. The v1 encoder
/// uses [`AV_PIX_FMT_NV12`](AVPixelFormat_AV_PIX_FMT_NV12) for 8-bit
/// 4:2:0; future work adds [`AV_PIX_FMT_P010LE`](AVPixelFormat_AV_PIX_FMT_P010LE)
/// for 10-bit. VAAPI's `vaCreateSurfaces` requires a fixed pool size,
/// so `pool_size` is the pre-allocated surface count — sized in the
/// encoder caller as `gop_size + max_b_frames + a small headroom` to
/// cover the encoder's internal references plus the in-flight upload.
///
/// On any failure the partially-allocated buffer is released before
/// returning the error so the caller doesn't have to track partial
/// ownership.
pub fn allocate_hw_frames_ctx(
    device: &VaapiDevice,
    width: i32,
    height: i32,
    sw_format: AVPixelFormat,
    pool_size: i32,
) -> Result<*mut AVBufferRef, VideoError> {
    unsafe {
        let frames_ref = av_hwframe_ctx_alloc(device.as_ptr());
        if frames_ref.is_null() {
            return Err(VideoError::HwFramesInit(-12)); // ENOMEM
        }
        let frames_ctx = (*frames_ref).data as *mut AVHWFramesContext;
        (*frames_ctx).format = AVPixelFormat_AV_PIX_FMT_VAAPI;
        (*frames_ctx).sw_format = sw_format;
        (*frames_ctx).width = width;
        (*frames_ctx).height = height;
        (*frames_ctx).initial_pool_size = pool_size;
        let ret = av_hwframe_ctx_init(frames_ref);
        if ret < 0 {
            let mut tmp = frames_ref;
            av_buffer_unref(&mut tmp);
            return Err(VideoError::HwFramesInit(ret));
        }
        Ok(frames_ref)
    }
}

/// Collapse a 2-layer ABH descriptor's per-plane fourccs into the
/// matching semi-planar surface fourcc the KMS scanout side advertises.
/// Returns `None` for combinations no scanout plane handles natively —
/// the caller demotes to the CPU-blit path on `None`.
///
/// **4:2:2 ambiguity** — NV16/P210 share the same single-plane fourccs
/// as NV12/P010 in the ABH layout (the ABH split fourcc only encodes
/// per-byte channel layout, not chroma sub-sampling). The chroma
/// sub-sampling has to be recovered from the descriptor's plane
/// **height** ratio, not the fourcc. We assume 4:2:0 here because every
/// HEVC / H.264 hwaccel surface bilbycast-edge consumes today is
/// 4:2:0 (NV12 / P010) — broadcast 4:2:2 sources arrive as
/// transcoder input, not display-output input. If a future driver
/// produces 4:2:2 surfaces here we'll need to plumb the layer's plane
/// height through to disambiguate.
fn collapse_abh_fourcc(luma: u32, chroma: u32) -> Option<u32> {
    // Suppress dead-code warnings on the NV16 / P210 fourcc constants
    // we keep around as pre-emptive documentation for the 4:2:2 path.
    let _ = (FOURCC_NV16, FOURCC_P210);

    match (luma, chroma) {
        // 8-bit 4:2:0 (NV12) — Y is R8, UV is GR88 / RG88.
        (FOURCC_R8, FOURCC_GR88) | (FOURCC_R8, FOURCC_RG88) => Some(FOURCC_NV12),
        // 10-bit 4:2:0 (P010) — Y is R16, UV is GR1616 / RG1616.
        (FOURCC_R16, FOURCC_GR1616) | (FOURCC_R16, FOURCC_RG1616) => Some(FOURCC_P010),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abh_fourcc_collapse_nv12() {
        assert_eq!(collapse_abh_fourcc(FOURCC_R8, FOURCC_GR88), Some(FOURCC_NV12));
        assert_eq!(collapse_abh_fourcc(FOURCC_R8, FOURCC_RG88), Some(FOURCC_NV12));
    }

    #[test]
    fn abh_fourcc_collapse_p010() {
        assert_eq!(
            collapse_abh_fourcc(FOURCC_R16, FOURCC_GR1616),
            Some(FOURCC_P010)
        );
        assert_eq!(
            collapse_abh_fourcc(FOURCC_R16, FOURCC_RG1616),
            Some(FOURCC_P010)
        );
    }

    #[test]
    fn abh_fourcc_unknown_returns_none() {
        // A made-up 32-bit fourcc that no hwaccel emits.
        assert_eq!(collapse_abh_fourcc(0xDEADBEEF, FOURCC_GR88), None);
    }
}

