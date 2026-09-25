# CLAUDE.md — bilbycast-ffmpeg-video-rs

## What Is This

Rust wrapper around FFmpeg's libavcodec, libavutil, libswscale, and libopus for the bilbycast ecosystem. Provides safe, in-process media processing — replacing all ffmpeg subprocess dependencies in bilbycast-edge:

- **Video decode:** H.264 / HEVC / MPEG-1 / MPEG-2 (the libavcodec `mpeg2video` decoder accepts both MPEG-1 and MPEG-2 bitstreams)
- **Video scale:** libswscale (YUVJ420P, YUV422P 8/10-bit, YUV420P 10-bit, BGRA output via `ScalerDstFormat`)
- **Video encode:** MJPEG (thumbnails, always on) + **optional** libx264 / libx265 / NVENC / QSV / VAAPI / RKMPP via opt-in Cargo features
- **Audio decode:** Opus / MP2 / AC-3 / E-AC-3 / AAC-LATM (non-AAC-LC broadcast codecs; AAC-LC decode is handled by `bilbycast-fdk-aac-rs`)
- **Audio encode:** Opus, MP2, AC-3 (AAC encode is handled by `bilbycast-fdk-aac-rs`)

## Projects

| Crate | Role |
|-------|------|
| **libffmpeg-video-sys** | Raw FFI bindings to FFmpeg via bindgen. Vendored build from `libffmpeg-video-sys/vendor/ffmpeg` (n9.0.2) + `libffmpeg-video-sys/vendor/opus` (v1.6.1). The `system-ffmpeg` alternative needs libavcodec >= 61.13.100 (FFmpeg 7.1) — `probe.rs` calls `avcodec_get_supported_config()`, since n9.0 deletes the `AVCodec::pix_fmts` field it replaces. |
| **video-codec** | Pure-Rust data types (video/audio codec enums, errors, config). No C dependency. |
| **video-engine** | Safe wrapper — `VideoDecoder`, `VideoScaler`, `JpegEncoder`, `AudioDecoder`, `AudioEncoder`, `VideoEncoder` (feature-gated), `decode_thumbnail()`, plus the equally public `probe` (host encoder/decoder availability + session-capacity probes) and `vaapi` (`VaapiDevice`, `hw_frames_ctx` allocation, DRM PRIME export) modules. The crate bilbycast-edge depends on. |

## Codec Support

| Feature | Support |
|---------|---------|
| H.264 video decode | Yes |
| HEVC/H.265 video decode | Yes |
| MPEG-1 / MPEG-2 video decode | Yes (covers DVB-T / ATSC / legacy contribution) |
| MJPEG encode | Yes (thumbnails) |
| Frame scaling | Yes (Lanczos via libswscale) |
| Black-screen detection | Yes (Y-plane luminance) |
| Opus audio decode | Yes (via vendored libopus, falls back to FFmpeg native) |
| MP2 audio decode | Yes (FFmpeg native) |
| AC-3 / E-AC-3 audio decode | Yes (FFmpeg native) |
| AAC-LATM audio decode | Yes (FFmpeg native — LATM/LOAS-framed AAC, `stream_type=0x11`) |
| Opus audio encode | Yes (via vendored libopus) |
| MP2 audio encode | Yes (FFmpeg native) |
| AC-3 audio encode | Yes (FFmpeg native) |
| **H.264 video encode (libx264)** | Opt-in via `video-encoder-x264` feature (GPL v2+) |
| **HEVC video encode (libx265)** | Opt-in via `video-encoder-x265` feature (GPL v2+) |
| **NVENC H.264 / HEVC encode** | Opt-in via `video-encoder-nvenc` feature (LGPL-clean, NVIDIA GPU required at runtime) |
| **QSV H.264 / HEVC encode (Intel oneVPL)** | Opt-in via `video-encoder-qsv` feature (LGPL-clean, x86_64 only, Intel iGPU + media driver required at runtime) |
| **VAAPI H.264 / HEVC encode + decode** | Opt-in via `video-encoder-vaapi` / `video-decoder-vaapi` features (LGPL-clean via libva, Linux only). Fully wired: `AVHWDeviceContext` + `hw_frames_ctx` setup in `video-engine/src/vaapi.rs`; encoder accepts the broadcast contribution matrix (4:2:0 + 4:2:2 × 8-bit + 10-bit, mapped to NV12 / NV16 / P010LE / P210LE surfaces — 4:4:4 / NV24 deferred); decoder exports DRM PRIME descriptors for zero-copy KMS scanout. h264_vaapi is 4:2:0 8-bit only by spec; HEVC covers the full broadcast matrix on Intel iHD (Tiger Lake+). AMD radeonsi typically rejects 4:2:2 at `avcodec_open2`. |
| **RKMPP H.264 / HEVC encode + decode** | Opt-in via `video-encoder-rkmpp` / `video-decoder-rkmpp` features (Rockchip MPP is LGPLv3; FFmpeg classifies it `version3`, so the build also passes `--enable-version3`). ARM Rockchip RK3568 / RK3588 only — there is no `rockchip_mpp` pkg-config module on x86_64 and `build.rs` aborts without it. Encode is **8-bit 4:2:0 only** (the VEPU has no 4:2:2, 4:4:4 or 10-bit encode path; `open()` rejects anything else with a named error). Decode natively emits `AV_PIX_FMT_DRM_PRIME` and `receive_frame()` downloads to sysmem NV12 unless the caller opts in via `set_rkmpp_zero_copy(true)`; no MPEG-2 decoder exists upstream for this backend. |
| **NVDEC H.264 / HEVC / MPEG-2 decode** | Opt-in via `video-decoder-nvdec` (`h264_cuvid` / `hevc_cuvid` / `mpeg2_cuvid`, NV12 system memory, LGPL-clean, same `nv-codec-headers` build dep and NVIDIA driver as NVENC). |
| **QSV H.264 / HEVC decode** | Opt-in via `video-decoder-qsv` (LGPL-clean, x86_64 only, same `libvpl-dev` build dep and Intel media driver as QSV encode). |

The canonical per-backend / chroma / bit-depth / host-class matrix, with the
verification commands for each host class, is `bilbycast-edge/docs/codec-matrix.md`
— this table is the wrapper crate's build-time view of the same ground.

## Build & Test

```bash
# Default LGPL-clean build (vendored FFmpeg + libopus; requires CMake + make)
cargo build

# Run tests
cargo test

# Use system-installed FFmpeg instead of vendored
cargo build --features libffmpeg-video-sys/system-ffmpeg

# Point to custom FFmpeg install
LIBFFMPEG_DIR=/path/to/ffmpeg cargo build

# ── Opt-in video encoders (Linux host) ──

# H.264 via libx264 (GPL v2+)
sudo apt install libx264-dev
cargo build -p video-engine --features video-encoder-x264

# HEVC via libx265 (GPL v2+)
sudo apt install libx265-dev
cargo build -p video-engine --features video-encoder-x265

# NVIDIA NVENC (LGPL-clean, needs NVIDIA driver at runtime)
sudo apt install nv-codec-headers
cargo build -p video-engine --features video-encoder-nvenc

# Intel QuickSync via oneVPL (LGPL-clean, x86_64 only, needs Intel iGPU
# + media driver at runtime). H.264 supported on Broadwell (5th gen) and
# newer; HEVC on Kaby Lake (7th gen) and newer.
sudo apt install libvpl-dev
cargo build -p video-engine --features video-encoder-qsv

# VAAPI encode + decode (LGPL-clean via libva, Linux only). Primary
# motivation is AMD-on-Linux (Mesa radeonsi). Also works on Intel iGPU
# (iHD driver) but oneVPL/QSV exposes more rate-control knobs there.
sudo apt install libva-dev
cargo build -p video-engine --features video-encoder-vaapi,video-decoder-vaapi

# NVIDIA NVDEC / Intel QSV hardware decode. Same build prerequisites as
# their encoder siblings above (nv-codec-headers / libvpl-dev); the host
# driver is what gates them at runtime.
cargo build -p video-engine --features video-decoder-nvdec
cargo build -p video-engine --features video-decoder-qsv

# Rockchip RKMPP encode + decode (aarch64 RK3568 / RK3588 hosts only).
# Needs librockchip-mpp-dev >= 1.3.8 (ships rockchip_mpp.pc) + libdrm-dev;
# runtime needs the MPP kernel driver at /dev/mpp_service.
sudo apt install librockchip-mpp-dev libdrm-dev
cargo build -p video-engine --features video-encoder-rkmpp,video-decoder-rkmpp
```

### Prerequisites

- **CMake** (for vendored libopus build)
- **Clang/LLVM** (for bindgen)
- **make** (for vendored FFmpeg build)
- **Linux (Debian / Ubuntu — primary target)**: `sudo apt install cmake clang make pkg-config`
- **macOS (dev boxes only)**: `brew install cmake` or Xcode command line tools
- **Optional encoder libraries (Linux)**:
  - `libx264-dev` when building with `video-encoder-x264` (GPL v2+)
  - `libx265-dev` when building with `video-encoder-x265` (GPL v2+)
  - `nv-codec-headers` when building with `video-encoder-nvenc` / `video-decoder-nvdec` (royalty-free, NVIDIA driver required at runtime)
  - `libvpl-dev` when building with `video-encoder-qsv` / `video-decoder-qsv` (royalty-free, x86_64 only, Intel media driver + libvpl runtime required at runtime)
  - `libva-dev` when building with `video-encoder-vaapi` / `video-decoder-vaapi` (royalty-free, Linux only, working VAAPI driver — Mesa radeonsi for AMD or iHD for Intel — required at runtime)
  - `librockchip-mpp-dev` (>= 1.3.8, ships `rockchip_mpp.pc`) + `libdrm-dev` when building with `video-encoder-rkmpp` / `video-decoder-rkmpp` (aarch64 Rockchip only — pkg-config finds no `rockchip_mpp` on x86_64 and `build.rs` panics; `/dev/mpp_service` required at runtime)

## Architecture

### Video Decoder (`video-engine/src/decoder.rs`)

`VideoDecoder` wraps FFmpeg's `AVCodecContext`:
- `open(codec)` — create a CPU decoder for H.264, HEVC, or MPEG-1/2
- `open_threaded(codec)` — same, with libavcodec's automatic thread count,
  which resolves to **frame** threading (`active_thread_type == 1`)
- `open_opts(codec, DecoderOptions { backend, threading, reorder_seed })` —
  every option explicit. `open`, `open_threaded` and `open_with_backend` are
  wrappers over it with `reorder_seed: ReorderSeed::LibavcodecDefault`, so
  their behaviour is unchanged. `ReorderSeed::FromAccessUnit(au)` seeds an
  H.264 decoder's `has_b_frames` before `avcodec_open2` (Cpu and Vaapi
  backends only; HEVC / MPEG-2 / NVDEC / QSV / RKMPP ignore it): **0** when
  the AU's SPS declares `max_num_reorder_frames` (libavcodec applies the
  declared depth itself, before gap handling — including 0 for x264
  `zerolatency` output), **1** otherwise. Without it, a mid-stream join on a
  non-IDR I picture of a stream with no VUI `bitstream_restriction` (14 of the
  18 broadcast captures on the rig) lets libavcodec's unmarked-random-access
  heuristic (`h264_refs.c`) fire on the synthesised frame-num-gap
  placeholders, which are then never greyed nor swapped for a real reference
  — a whole GOP of garbage on Sky Sports. Costs one frame of latency on an
  IPPP stream whose SPS declares nothing. Does **not** recover the 1-3
  decodable leading B-pictures a join on an undeclared depth-≥2 stream still
  drops once (seeding the level DPB would, at permanent extra latency).
  Callers that open lazily on the triggering AU should pass it
- `reorder_depth()` — the current `has_b_frames` (seed, then whatever
  libavcodec learns; it only grows). `flush()` does **not** reset it and
  cannot re-seed (frame-thread workers never re-read it), so a decoder that
  switches to a different source is dropped and reopened with `open_opts`
- **`AV_CODEC_FLAG2_CHUNKS` stays off.** Until 2026-09 `open_inner` and the
  decoder probe set `flags2 |= 1 << 1` under a CHUNKS comment; bit 1 is
  unassigned, so it was a no-op in every release. The real flag (`1 << 15`)
  makes libavcodec refuse frame threading (silent slice threading, which also
  disables H.264 error resilience) and changed decoded output on the rig.
  `open_threaded_uses_frame_threading` pins it
- `open_with_backend(codec, DecoderBackend)` — pick the backend explicitly.
  `DecoderBackend` has five variants: `Cpu` (always available), `Nvdec`
  (`h264_cuvid` / `hevc_cuvid` / `mpeg2_cuvid`), `Qsv`, `Vaapi` and `Rkmpp`
  (no MPEG-2 upstream), each behind its own `video-decoder-*` Cargo feature.
  A HW backend fails with `CodecNotFound` when the feature is off and
  `OpenCodec` when the host lacks the driver / hardware / permissions, so a
  caller can demote to `Cpu` on the error rather than at build time
- `backend()` — which backend this decoder actually opened with
- `set_rkmpp_zero_copy(bool)` — `Vaapi` and `Rkmpp` are deliberately
  asymmetric. `Vaapi` always yields hardware frames (the `*_planes()`
  accessors return `None`), while `Rkmpp` decodes to `AV_PIX_FMT_DRM_PRIME`
  and `receive_frame()` downloads it to sysmem NV12 by default, so it looks
  like NVDEC / QSV to a transcode caller. Only the display path opts into the
  raw DMA-BUF frames with this
- `send_packet(data)` / `send_packet_with_pts(data, pts)` — feed Annex B NAL
  unit data (or MPEG-2 elementary stream verbatim); the PTS variant rides the
  stamp through libavcodec's reorder queue
- `receive_frame()` → `DecodedFrame` with Y-plane access for luminance
- `send_flush()` — signal end-of-stream so `receive_frame()` drains
- `flush()` — reset decoder state

`DecodedFrame` carries the hardware-frame surface alongside the sysmem
accessors: `is_vaapi()` / `is_drm_prime()` classify it, `map_drm_prime()`
exports a `DrmPrimeFrame` DMA-BUF descriptor (what bilbycast-edge's
`engine::output_display` repacks into a `display::kms::DrmPrimeDescriptor` for
the `drm` crate's `add_planar_framebuffer` — zero-copy KMS scanout),
and `download_to_sysmem()` copies it back to a plain planar frame when a CPU
consumer needs one.

### H.264 SPS parser (`video-engine/src/h264_sps.rs`)

Pure Rust, no FFI. `parse_h264_sps(nal)` / `find_h264_sps(annexb)` →
`H264SpsInfo` (profile / level, chroma format, bit depth, `frame_mbs_only`,
`mb_adaptive_frame_field`, cropped width / height, VUI sample aspect ratio,
timing, NAL HRD presence, `pic_struct_present`, `max_num_reorder_frames`), and
`h264_declared_reorder_depth(annexb)` for the decoder seed above.
`annexb_nal_units` iterates an Annex B buffer. It mirrors libavcodec n9.0.2's
`ff_h264_decode_seq_parameter_set` rather than the letter of the spec where
they differ (libavcodec's profile list for the chroma branch; every SPS it
refuses parses to `None`; the reader stops at the `rbsp_stop_one_bit`, and any
overread leaves the reorder depth undeclared), because a depth libavcodec did
not apply would reproduce the join bug while an undeclared one only costs a
frame. Tested on SPS bytes cut from the rig's captures (Nine 3, Seven 3,
sync-test 2, BTS204 2, Sky Sports / Sky Witness / ABC none, x264 0 / 1) with
FFmpeg's `trace_headers` as the oracle, plus every truncation and every
single-bit flip.

### Video Scaler (`video-engine/src/scaler.rs`)

`VideoScaler` wraps FFmpeg's `SwsContext`:
- `new(src_w, src_h, src_fmt, dst_w, dst_h)` — configure scaling
- `scale(frame)` → `ScaledFrame` in YUVJ420P (full-range, MJPEG-compatible)
- `new_with_dst_format(src_w, src_h, src_fmt, dst_w, dst_h, ScalerDstFormat)` —
  pick the destination format instead of taking `new()`'s YUVJ420P: the planar
  broadcast formats `Yuv420p8` / `Yuv422p8` / `Yuv420p10le` / `Yuv422p10le`
  (encoder feeds, RFC 4175 packetizers) or packed `Bgra8`
- `scale_raw_planes(src_w, src_h, src_fmt, y, y_stride, u, u_stride, v, v_stride)`
  → `ScaledFrame` — scale from caller-owned Y/U/V planes with explicit strides;
  no `DecodedFrame` needed. The planes are wrapped, not copied
- `scale_into_packed(frame, dst, dst_pitch)` /
  `scale_raw_planes_into_packed(...)` / `scale_semi_planar_into_packed(...)`
  (the last takes NV12-shaped Y + interleaved UV) — scale straight into a
  caller-supplied packed BGRA8 buffer at an arbitrary `dst_pitch`. Handing in a
  **canvas-pitched sub-slice starting at a tile's byte offset** would blit into
  a sub-rectangle with no intermediate copy, and `bilbycast-edge`'s canvas
  geometry (`engine/mosaic.rs`: `Canvas::byte_offset` + `required_tail`) is
  built around exactly that contract — but **no runtime caller uses that form
  today**, and those two helpers are currently only exercised by tests. Both
  in-tree callers hand in a whole buffer: the multiviewer tile path
  (`engine/input_mosaic.rs`) scales into a tight per-tile patch and then
  row-copies it onto the canvas, and the local-display path
  (`engine/output_display.rs`) scales straight into the mapped KMS back buffer
  at the scanout pitch.
  **Buffer-size contract: `(dst_height - 1) * dst_pitch + dst_width * 4`, not
  `dst_pitch * dst_height`** (`check_packed_dst`). libswscale never writes past
  the last row's real width, so demanding a full trailing stride over-rejected
  writes that were always in bounds — it refused every bottom-row tile of a
  mosaic (3840 bytes short on a 2x2 1080p wall) and made a mosaic impossible on
  the display path outright, where `KmsDisplay::back_buffer()` maps exactly
  `pitch * height`
- `set_yuv_to_rgb_colorspace(src_colorspace, src_full_range)` — the YUV→RGB
  matrix (`AVColorSpace`) and luma range for the packed path; a no-op when the
  destination is not packed. Without it libswscale assumes BT.601 for SD-shaped
  input, which gives muddy greens / oversaturated reds on a BT.709 HD source
- `av_pix_fmt_bgra()` / `av_pix_fmt_for_yuv(chroma, bit_depth)` — the raw
  `AVPixelFormat` integers, re-exported from the crate root so a consumer can
  describe a buffer without depending on `libffmpeg-video-sys` itself

### JPEG Encoder (`video-engine/src/encoder.rs`)

`JpegEncoder` wraps FFmpeg's MJPEG encoder:
- `new(quality)` — quality 1 (best) to 31 (worst)
- `encode(frame)` → JPEG `Bytes`

### Video Encoder (`video-engine/src/video_encoder.rs`)

**Feature-gated.** `VideoEncoder` wraps FFmpeg's `AVCodecContext` for
H.264 / HEVC compression:
- `open(config)` — backend selected by `VideoEncoderCodec::{X264, X265, H264Nvenc, HevcNvenc, H264Qsv, HevcQsv, H264Vaapi, HevcVaapi, H264Rkmpp, HevcRkmpp}`.
  Returns `EncoderDisabled` when the matching Cargo feature was not enabled at build.
- `encode_frame(y, y_stride, u, u_stride, v, v_stride, pts)` — accepts
  planar YUV 4:2:0 / 4:2:2 (8 + 10-bit) planes with explicit strides. Returns
  zero or more `EncodedVideoFrame` values with PTS / DTS / keyframe markers.
- `flush()` — drain trailing frames at end-of-stream.
- `extradata()` — out-of-band SPS/PPS when `global_header = true`.

**Production controls** (`VideoEncoderConfig`): rate-control mode
(VBR / CBR / CRF / ABR), CRF target, GOP size, B-frames, refs, preset,
profile (auto / baseline / main / high / high10 / high422 / high444 / main10),
chroma (4:2:0 / 4:2:2 / 4:4:4 — backend-validated), bit depth (8 / 10),
tune, level, full colorimetry passthrough (primaries / transfer / matrix
/ range — BT.709 / BT.2020 / PQ / HLG). `force_next_keyframe()` for
input-switch IDR injection. Scaler integration in `bilbycast-edge`
handles arbitrary input → target-resolution conversion before encode.

**Backend pixel-format matrix** (rejection happens at `open()` so
operators get a clear error, not opaque `avcodec_open2` EINVAL):

| Backend | 4:2:0 / 8 | 4:2:2 / 8 | 4:2:0 / 10 | 4:2:2 / 10 | 4:4:4 |
|---|:-:|:-:|:-:|:-:|:-:|
| libx264 / libx265 | ✓ | ✓ | ✓ | ✓ | ✓ |
| h264_nvenc / h264_qsv | ✓ | ✗ | ✗ | ✗ | ✗ |
| hevc_nvenc / hevc_qsv | ✓ | ✗ | ✓ | ✗ | ✗ |
| h264_vaapi | ✓ | ✗ | ✗ | ✗ | ✗ |
| hevc_vaapi (Intel iHD) | ✓ | ✓ | ✓ | ✓ | ✗ (NV24 deferred) |
| hevc_vaapi (AMD radeonsi) | ✓ | usually ✗ | ✓ | usually ✗ | ✗ |
| h264_rkmpp / hevc_rkmpp | ✓ | ✗ | ✗ | ✗ | ✗ |

RKMPP is the narrowest row deliberately: the Rockchip VEPU has no 4:2:2,
no 4:4:4 and no 10-bit encode path (10-bit on RK3588 is decode-only), and
`open()` rejects either up front so the edge auto-resolver can never land a
10-bit / 4:2:2 request on it. Like QSV it is fed sysmem NV12 with **no**
`hw_frames_ctx` — the FFmpeg wrapper auto-creates its own RKMPP device and
copies each frame into an MPP/DRM buffer internally.

### Hardware probe (`video-engine/src/probe.rs`)

The public module behind bilbycast-edge's `engine::hardware_probe` and the
`resource_budget` block it advertises on the health tick. Three tiers, all
re-exported from the crate root:

- **Availability** — `is_encoder_available(name)` / `is_decoder_available(name)`.
  A registry lookup only (`avcodec_find_*_by_name`); it proves the codec was
  compiled into the vendored FFmpeg, *not* that a session will open.
- **Open probes** — `probe_open_encoder(name)`, `probe_open_encoder_chroma(name,
  ProbeChroma)`, `probe_open_decoder(name)`, plus `probe_open_vaapi_encoder[_chroma]`,
  which route through `VideoEncoder::open()` (hwdevice + frames-context setup)
  and so return `NotCompiled` unless `video-encoder-vaapi` is on. These
  actually run `avcodec_open2` at `PROBE_WIDTH` × `PROBE_HEIGHT` (320×240 —
  above NVENC's 145-pixel minimum width and even-dimensioned for QSV). `(codec, chroma)` pairs the
  backend definitely rejects return `ProbeError::NotCompiled` without attempting
  an open, so a caller can fold "not in the matrix" and "not built" into one bit.
- **Session capacity** — `count_max_encoder_sessions(name, upper_bound, w, h)`,
  `count_max_decoder_sessions(...)` and the VAAPI-aware
  `count_max_vaapi_encoder_sessions(...)`. Each holds every successful open until
  the loop ends, so it measures the real concurrent cap (3–5 on consumer NVENC,
  1–2 per VCN engine on AMD radeonsi) rather than a surface-pool artefact. Two
  tiers of geometry ship as constants: `PROBE_WIDTH/HEIGHT_1080P` and
  `PROBE_WIDTH/HEIGHT_4K` — capacity at 4K is materially lower and is reported
  separately.

`ProbeChroma` is the chroma + bit-depth axis (`Yuv420_8bit`, `Yuv422_8bit`,
`Yuv420_10bit`, `Yuv422_10bit`) and `ProbeError` the result type.

### Thumbnail (`video-engine/src/thumbnail.rs`)

`decode_thumbnail(nalu_data, codec, config)` — end-to-end pipeline:
1. Open decoder for codec
2. Send NAL data, receive decoded frame
3. Compute Y-plane luminance (for black-screen detection)
4. Scale to thumbnail dimensions
5. Encode as JPEG

Returns `ThumbnailResult { jpeg, luminance, source_width, source_height }`.

### Audio Decoder (`video-engine/src/audio_decoder.rs`)

`AudioDecoder` wraps FFmpeg's `AVCodecContext` + `SwrContext` for the non-AAC-LC broadcast audio codecs (AAC-LC decode lives in `bilbycast-fdk-aac-rs`):
- `open(codec)` — create decoder for `Mp2`, `Ac3`, `Eac3`, `Opus`, or `AacLatm` (the `AudioDecoderCodec` enum in `video-codec`). Opus prefers libopus, falling back to FFmpeg's native decoder.
- `send_packet(data, pts)` — feed one encoded frame; PTS rides through in the codec time base
- `receive_frame()` → `DecodedAudioFrame` in planar f32 PCM (resampled via the lazily-allocated `SwrContext`)
- `flush()` — reset decoder state

### Audio Encoder (`video-engine/src/audio_encoder.rs`)

`AudioEncoder` wraps FFmpeg's `AVCodecContext` for audio encoding:
- `open(config)` — create encoder for Opus, MP2, or AC-3
- `encode_frame(planar_f32)` → `Vec<EncodedAudioFrame>` (raw codec frames, no container)
- `flush()` — drain buffered frames
- `frame_size()` — samples per frame for caller's accumulation buffer

Input: planar f32 PCM (matching bilbycast-edge's audio pipeline).
Output: raw encoded frames — Opus packets, MP2 frames, AC-3 frames.

## Key Design Constraints

1. **Send but not Sync** — all wrappers can move between threads but require &mut
2. **No libavformat** — TS demuxing/muxing stays in Rust; only codec data crosses FFI
3. **Use spawn_blocking / block_in_place for heavy C work** — video decode pipelines, multi-frame remux, and video encoding (single-digit milliseconds per frame) must run under `spawn_blocking` or `block_in_place`. Single audio frame encoding (~100 µs) is exempt
4. **Minimal vendored build** — `--disable-everything` with only needed codecs enabled; optional encoder features add targeted `--enable-*` flags and pull in system-installed libraries
5. **Y-plane stride != width** — always use `linesize[0..=2]` when iterating planar data; the `yuv_planes()` accessor surfaces all three strides in one call
6. **Feature gates are two-layer** — enabling a `video-encoder-*` feature on the `video-engine` crate automatically forwards to `libffmpeg-video-sys`, which in turn appends `--enable-gpl --enable-libx264` (or equivalent) to the FFmpeg configure invocation and pkg-config-finds the system library. bilbycast-edge forwards the same feature names one level up

## Integration with bilbycast-edge

Feature-gated via `media-codecs` in bilbycast-edge — `media-codecs = ["dep:video-engine", "dep:video-codec"]`, in the default set alongside `tls` / `webrtc` / `fdk-aac` / `replay` / `display`. Turning it off drops thumbnails, every non-AAC audio codec, and the universal CPU video decoder.

**Video thumbnails:** `TsDemuxer` extracts NAL units → `decode_thumbnail()` via `spawn_blocking`.

**Audio encoding:** `AudioEncoder` replaces ffmpeg subprocess for Opus/MP2/AC-3 in RTMP, WebRTC, and HLS outputs. Used via the `InProcessLibav` backend in bilbycast-edge's `audio_encode.rs`.

**HLS remuxing:** In-process TS audio remuxer decodes AAC → re-encodes to target codec → remuxes TS with video passthrough, replacing per-segment ffmpeg subprocess.

**Video transcoding:** `VideoEncoder` — when the caller builds with any `video-encoder-*` feature (x264 / x265 / NVENC / QSV / VAAPI / RKMPP) — is driven from `bilbycast-edge/src/engine/ts_video_replace.rs` to re-encode H.264/HEVC elementary streams inside SRT / UDP / RTP outputs. RTMP, WebRTC and CMAF-LL are wired too, each building an `engine::video_encode_util::ScaledVideoEncoder` from the output's own `video_encode` config: `output_rtmp.rs` (`VideoEncoderState` + `init_video_encoder_state`), `output_webrtc.rs` (`WebrtcVideoEncoderState`, H.264-only — codec strings `x264` / `h264_nvenc` / `h264_qsv` / `h264_vaapi` / `h264_rkmpp`, with `auto` resolved through `engine::hardware_probe` and an HEVC resolution rejected outright because browsers won't decode it) and `cmaf/encode.rs`. Only the legacy `output_hls.rs` segment path is still unwired for video — it handles `audio_encode` alone; see `bilbycast-edge/docs/transcoding.md` for the deferred-items list.
