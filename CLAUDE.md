# CLAUDE.md — bilbycast-ffmpeg-video-rs

## What Is This

Rust wrapper around FFmpeg's libavcodec, libavutil, libswscale, and libopus for the bilbycast ecosystem. Provides safe, in-process media processing — replacing all ffmpeg subprocess dependencies in bilbycast-edge:

- **Video:** H.264/HEVC decoding, frame scaling, JPEG thumbnail generation
- **Audio:** Opus, MP2, AC-3 encoding (AAC is handled by bilbycast-fdk-aac-rs)

## Projects

| Crate | Role |
|-------|------|
| **libffmpeg-video-sys** | Raw FFI bindings to FFmpeg via bindgen. Vendored build from `vendor/ffmpeg/` (n7.1.3) + `vendor/opus/` (v1.5.2). |
| **video-codec** | Pure-Rust data types (video/audio codec enums, errors, config). No C dependency. |
| **video-engine** | Safe wrapper — `VideoDecoder`, `VideoScaler`, `JpegEncoder`, `AudioEncoder`, `decode_thumbnail()`. The crate bilbycast-edge depends on. |

## Codec Support

| Feature | Support |
|---------|---------|
| H.264 video decode | Yes |
| HEVC/H.265 video decode | Yes |
| MJPEG encode | Yes (thumbnails) |
| Frame scaling | Yes (Lanczos via libswscale) |
| Black-screen detection | Yes (Y-plane luminance) |
| Opus audio encode | Yes (via vendored libopus) |
| MP2 audio encode | Yes (FFmpeg native) |
| AC-3 audio encode | Yes (FFmpeg native) |

## Build & Test

```bash
# Build all crates (vendored FFmpeg + libopus, requires CMake + make)
cargo build

# Run tests
cargo test

# Use system-installed FFmpeg instead of vendored
cargo build --features libffmpeg-video-sys/system-ffmpeg

# Point to custom FFmpeg install
LIBFFMPEG_DIR=/path/to/ffmpeg cargo build
```

### Prerequisites

- **CMake** (for vendored libopus build)
- **Clang/LLVM** (for bindgen)
- **make** (for vendored FFmpeg build)
- **macOS**: `brew install cmake` or Xcode command line tools
- **Linux**: `apt install cmake clang make`

## Architecture

### Video Decoder (`video-engine/src/decoder.rs`)

`VideoDecoder` wraps FFmpeg's `AVCodecContext`:
- `open(codec)` — create decoder for H.264 or HEVC
- `send_packet(data)` — feed Annex B NAL unit data
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
3. **Use spawn_blocking for heavy C work** — video decode pipelines and multi-frame remux (tens of milliseconds) must use `spawn_blocking`. Single audio frame encoding (~100 µs) is exempt and may be called inline on a Tokio worker thread — the `spawn_blocking` overhead would exceed the encode time
4. **Minimal vendored build** — `--disable-everything` with only needed codecs enabled
5. **Y-plane stride != width** — always use `linesize[0]` when iterating luma data

## Integration with bilbycast-edge

Feature-gated via `video-thumbnail` in bilbycast-edge (default on).

**Video thumbnails:** `TsDemuxer` extracts NAL units → `decode_thumbnail()` via `spawn_blocking`.

**Audio encoding:** `AudioEncoder` replaces ffmpeg subprocess for Opus/MP2/AC-3 in RTMP, WebRTC, and HLS outputs. Used via the `InProcessLibav` backend in bilbycast-edge's `audio_encode.rs`.

**HLS remuxing:** In-process TS audio remuxer decodes AAC → re-encodes to target codec → remuxes TS with video passthrough, replacing per-segment ffmpeg subprocess.
