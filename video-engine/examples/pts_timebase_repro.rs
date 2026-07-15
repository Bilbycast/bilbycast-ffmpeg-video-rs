// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Reproduces (and verifies the fix for) the libx264 SIGSEGV caused by
//! feeding 90 kHz pts against an undeclared (1/fps) timebase.
//!
//! ```text
//! cargo run -p video-engine --features video-encoder-x264 \
//!     --example pts_timebase_repro -- bad    # undeclared: segfaults (exit 139)
//! cargo run -p video-engine --features video-encoder-x264 \
//!     --example pts_timebase_repro -- good   # declared 1/90000: survives
//! ```
//!
//! With `bad`, x264's VBV rate control reads pts steps of 3600 in a 1/25
//! timebase as frames 144 seconds apart, overflows internally, and dies with
//! a wild store roughly one lookahead-depth of frames after open. The crash
//! is inside libx264 and unguardable from our side of the FFI — the declared
//! timebase is load-bearing.

fn main() {
    let good = std::env::args().nth(1).as_deref() == Some("good");
    // Match a tokio blocking thread's stack, ruling stack size out.
    let h = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || run(good))
        .unwrap();
    match h.join() {
        Ok(()) => println!("survived"),
        Err(_) => println!("THREAD DIED"),
    }
}

fn run(good: bool) {
    use video_codec::*;

    let (tb_num, tb_den): (u32, u32) = if good { (1, 90_000) } else { (0, 0) };
    eprintln!(
        "timebase declared: {}",
        if good { "1/90000 (fixed)" } else { "1/fps (the bug)" }
    );

    let (w, h) = (1920u32, 1080u32);
    let cw = (w / 2) as usize;
    let ch = (h / 2) as usize;
    let cfg = VideoEncoderConfig {
        codec: VideoEncoderCodec::X264,
        width: w,
        height: h,
        // The SDI capture's real frame rate shape.
        fps_num: 25_000,
        fps_den: 1_000,
        time_base_num: tb_num,
        time_base_den: tb_den,
        bitrate_kbps: 20_000,
        max_bitrate_kbps: 0,
        gop_size: 25,
        preset: VideoPreset::Ultrafast,
        profile: VideoProfile::Auto,
        chroma: VideoChroma::Yuv420,
        bit_depth: 8,
        rate_control: VideoRateControl::Cbr,
        crf: 23,
        max_b_frames: 0,
        refs: 0,
        tune: String::new(),
        level: String::new(),
        color_primaries: String::new(),
        color_transfer: String::new(),
        color_matrix: String::new(),
        color_range: String::new(),
        global_header: false,
        async_depth: 0,
    };

    let mut enc = video_engine::VideoEncoder::open(&cfg).expect("open x264");

    let mut y = vec![128u8; (w * h) as usize];
    let mut u = vec![128u8; cw * ch];
    let mut v = vec![128u8; cw * ch];
    let mut seed: u32 = 0x1234_5678;
    let mut total = 0usize;
    for i in 0..200i64 {
        // Varying content so lookahead / motion estimation do real work.
        for p in [&mut y, &mut u, &mut v] {
            for b in p.iter_mut() {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *b = (seed >> 24) as u8;
            }
        }
        // 90 kHz ticks at 25 fps — exactly what the SDI / ST 2110 ingest
        // paths feed.
        match enc.encode_frame(&y, w as usize, &u, cw, &v, cw, Some(i * 3600)) {
            Ok(pkts) => total += pkts.len(),
            Err(e) => {
                eprintln!("encode error at frame {i}: {e}");
                return;
            }
        }
    }
    eprintln!("200 frames encoded, {total} packet(s)");
}
