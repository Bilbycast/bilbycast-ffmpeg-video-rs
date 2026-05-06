# CLAUDE.md — bilbycast-ffmpeg-video-rs

## What Is This

Rust wrapper around FFmpeg's libavcodec, libavutil, libswscale, and libopus for the bilbycast ecosystem. Provides safe, in-process media processing — replacing all ffmpeg subprocess dependencies in bilbycast-edge:

- **Video decode:** H.264 / HEVC / MPEG-1 / MPEG-2 (the libavcodec `mpeg2video` decoder accepts both MPEG-1 and MPEG-2 bitstreams)
- **Video scale:** libswscale (currently only YUVJ420P output — see deferred items)
- **Video encode:** MJPEG (thumbnails, always on) + **optional** libx264 / libx265 / NVENC via opt-in Cargo features
- **Audio:** Opus, MP2, AC-3 encoding (AAC is handled by `bilbycast-fdk-aac-rs`)

## Projects

| Crate | Role |
|-------|------|
| **libffmpeg-video-sys** | Raw FFI bindings to FFmpeg via bindgen. Vendored build from `vendor/ffmpeg/` (n7.1.3) + `vendor/opus/` (v1.5.2). |
| **video-codec** | Pure-Rust data types (video/audio codec enums, errors, config). No C dependency. |
| **video-engine** | Safe wrapper — `VideoDecoder`, `VideoScaler`, `JpegEncoder`, `AudioEncoder`, `VideoEncoder` (feature-gated), `decode_thumbnail()`. The crate bilbycast-edge depends on. |

## Codec Support

| Feature | Support |
|---------|---------|
| H.264 video decode | Yes |
| HEVC/H.265 video decode | Yes |
| MPEG-1 / MPEG-2 video decode | Yes (covers DVB-T / ATSC / legacy contribution) |
| MJPEG encode | Yes (thumbnails) |
| Frame scaling | Yes (Lanczos via libswscale) |
| Black-screen detection | Yes (Y-plane luminance) |
| Opus audio encode | Yes (via vendored libopus) |
| MP2 audio encode | Yes (FFmpeg native) |
| AC-3 audio encode | Yes (FFmpeg native) |
| **H.264 video encode (libx264)** | Opt-in via `video-encoder-x264` feature (GPL v2+) |
| **HEVC video encode (libx265)** | Opt-in via `video-encoder-x265` feature (GPL v2+) |
| **NVENC H.264 / HEVC encode** | Opt-in via `video-encoder-nvenc` feature (LGPL-clean, NVIDIA GPU required at runtime) |
| **QSV H.264 / HEVC encode (Intel oneVPL)** | Opt-in via `video-encoder-qsv` feature (LGPL-clean, x86_64 only, Intel iGPU + media driver required at runtime) |
| **VAAPI H.264 / HEVC encode + decode** | Opt-in via `video-encoder-vaapi` / `video-decoder-vaapi` features (LGPL-clean via libva, Linux only). Fully wired: `AVHWDeviceContext` + `hw_frames_ctx` setup in `video-engine/src/vaapi.rs`; encoder accepts the broadcast contribution matrix (4:2:0 + 4:2:2 × 8-bit + 10-bit, mapped to NV12 / NV16 / P010LE / P210LE surfaces — 4:4:4 / NV24 deferred); decoder exports DRM PRIME descriptors for zero-copy KMS scanout. h264_vaapi is 4:2:0 8-bit only by spec; HEVC covers the full broadcast matrix on Intel iHD (Tiger Lake+). AMD radeonsi typically rejects 4:2:2 at `avcodec_open2`. |

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
  - `nv-codec-headers` when building with `video-encoder-nvenc` (royalty-free, NVIDIA driver required at runtime)
  - `libvpl-dev` when building with `video-encoder-qsv` (royalty-free, x86_64 only, Intel media driver + libvpl runtime required at runtime)
  - `libva-dev` when building with `video-encoder-vaapi` / `video-decoder-vaapi` (royalty-free, Linux only, working VAAPI driver — Mesa radeonsi for AMD or iHD for Intel — required at runtime)

## Architecture

### Video Decoder (`video-engine/src/decoder.rs`)

`VideoDecoder` wraps FFmpeg's `AVCodecContext`:
- `open(codec)` — create decoder for H.264, HEVC, or MPEG-1/2
- `send_packet(data)` — feed Annex B NAL unit data (or MPEG-2 elementary stream verbatim)
- `receive_frame()` → `DecodedFrame` with Y-plane access for luminance
- `flush()` — reset decoder state

### Video Scaler (`video-engine/src/scaler.rs`)

`VideoScaler` wraps FFmpeg's `SwsContext`:
- `new(src_w, src_h, src_fmt, dst_w, dst_h)` — configure scaling
- `scale(frame)` → `ScaledFrame` in YUVJ420P (full-range, MJPEG-compatible)

### JPEG Encoder (`video-engine/src/encoder.rs`)

`JpegEncoder` wraps FFmpeg's MJPEG encoder:
- `new(quality)` — quality 1 (best) to 31 (worst)
- `encode(frame)` → JPEG `Bytes`

### Video Encoder (`video-engine/src/video_encoder.rs`)

**Feature-gated.** `VideoEncoder` wraps FFmpeg's `AVCodecContext` for
H.264 / HEVC compression:
- `open(config)` — backend selected by `VideoEncoderCodec::{X264, X265, H264Nvenc, HevcNvenc, H264Qsv, HevcQsv, H264Vaapi, HevcVaapi}`.
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

### Thumbnail (`video-engine/src/thumbnail.rs`)

`decode_thumbnail(nalu_data, codec, config)` — end-to-end pipeline:
1. Open decoder for codec
2. Send NAL data, receive decoded frame
3. Compute Y-plane luminance (for black-screen detection)
4. Scale to thumbnail dimensions
5. Encode as JPEG

Returns `ThumbnailResult { jpeg, luminance, source_width, source_height }`.

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

Feature-gated via `video-thumbnail` in bilbycast-edge (default on).

**Video thumbnails:** `TsDemuxer` extracts NAL units → `decode_thumbnail()` via `spawn_blocking`.

**Audio encoding:** `AudioEncoder` replaces ffmpeg subprocess for Opus/MP2/AC-3 in RTMP, WebRTC, and HLS outputs. Used via the `InProcessLibav` backend in bilbycast-edge's `audio_encode.rs`.

**HLS remuxing:** In-process TS audio remuxer decodes AAC → re-encodes to target codec → remuxes TS with video passthrough, replacing per-segment ffmpeg subprocess.

**Video transcoding (Phase 4 MVP):** `VideoEncoder` — when the caller builds with `video-encoder-x264` / `video-encoder-x265` / `video-encoder-nvenc` — is driven from `bilbycast-edge/src/engine/ts_video_replace.rs` to re-encode H.264/HEVC elementary streams inside SRT / UDP / RTP outputs. RTMP / HLS / WebRTC video paths are **not yet wired**; see `bilbycast-edge/docs/transcoding.md` for the deferred-items list.
