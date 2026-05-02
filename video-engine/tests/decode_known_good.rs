//! Manual regression test for the chroma-stripe thumbnail bug.
//!
//! Reads an Annex-B H.264 ES from $THUMB_DEBUG_H264, runs it through
//! `decode_thumbnail`, and writes the resulting JPEG to $THUMB_DEBUG_JPEG
//! so the operator can eyeball it. The bug we're guarding against:
//! libswscale built with `--disable-x86asm` corrupts the chroma planes
//! on x86_64 YUV→YUV downscales — every thumbnail comes back with
//! magenta/green stripe artifacts even though the source bitstream
//! decodes cleanly via stock ffmpeg.
//!
//! Capture an input by running an edge with the test pattern flow active,
//! then `ffmpeg -i <flow.ts> -c copy -map 0:v -f h264 /tmp/edge.h264`.
//! Then `THUMB_DEBUG_H264=/tmp/edge.h264 cargo test --release -p video-engine
//! --test decode_known_good -- --nocapture`.

use std::path::PathBuf;
use video_codec::{ThumbnailConfig, VideoCodec};
use video_engine::decode_thumbnail;

#[test]
fn debug_decode_to_jpeg() {
    video_engine::silence_ffmpeg_logs();

    let Some(path) = std::env::var_os("THUMB_DEBUG_H264") else {
        eprintln!("skipping: set THUMB_DEBUG_H264 to a path of an Annex-B H.264 file");
        return;
    };
    let path = PathBuf::from(path);
    let nalu = std::fs::read(&path).expect("read input");
    eprintln!("input bytes: {}", nalu.len());

    let cfg = ThumbnailConfig {
        width: 320,
        height: 180,
        quality: 5,
    };
    let result = decode_thumbnail(&nalu, VideoCodec::H264, &cfg).expect("decode_thumbnail");
    eprintln!(
        "decoded source {}x{}, luma={:.1}, jpeg {} bytes",
        result.source_width,
        result.source_height,
        result.luminance,
        result.jpeg.len()
    );

    let out = std::env::var_os("THUMB_DEBUG_JPEG")
        .unwrap_or_else(|| std::ffi::OsString::from("/tmp/thumb-debug.jpg"));
    std::fs::write(&out, &result.jpeg).expect("write jpeg");
    eprintln!("wrote {}", PathBuf::from(out).display());
}
