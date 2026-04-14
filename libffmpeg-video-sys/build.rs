// Copyright (c) 2026 Reza Rahimi / Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Build script for libffmpeg-video-sys.
//!
//! Default: compile vendored FFmpeg from `vendor/ffmpeg/` via `./configure` + `make`.
//! Vendored libopus is built first from `vendor/opus/` via CMake.
//! Override: set `LIBFFMPEG_DIR` env var to point to a pre-built FFmpeg install.
//! Override: enable `system-ffmpeg` feature to use pkg-config.
//!
//! The vendored build uses a minimal configure to produce:
//! - libavcodec (H.264/HEVC decoders, MJPEG encoder, Opus/MP2/AC-3 audio encoders)
//! - libavutil (pixel format utils, frame alloc, audio sample format conversion)
//! - libswscale (image scaling/conversion)
//!
//! No libavformat, no libavdevice, no network — the Rust TS demuxer handles
//! container parsing.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let include_path = if let Ok(ffmpeg_dir) = env::var("LIBFFMPEG_DIR") {
        // User-specified FFmpeg install
        let ffmpeg_path = PathBuf::from(&ffmpeg_dir);
        println!(
            "cargo:rustc-link-search=native={}",
            ffmpeg_path.join("lib").display()
        );
        link_ffmpeg_libs(false);
        ffmpeg_path.join("include")
    } else if cfg!(feature = "system-ffmpeg") {
        // System FFmpeg via pkg-config
        let avcodec = pkg_config::Config::new()
            .atleast_version("60.0.0")
            .probe("libavcodec")
            .expect(
                "pkg-config: libavcodec >= 60.0.0 not found. \
                 Install libavcodec-dev or set LIBFFMPEG_DIR",
            );
        let _avutil = pkg_config::Config::new()
            .atleast_version("58.0.0")
            .probe("libavutil")
            .expect("pkg-config: libavutil not found");
        let _swscale = pkg_config::Config::new()
            .atleast_version("7.0.0")
            .probe("libswscale")
            .expect("pkg-config: libswscale not found");

        PathBuf::from(
            avcodec
                .include_paths
                .first()
                .expect("no include path from pkg-config"),
        )
    } else {
        // Vendored build (default)
        build_vendored(&out_dir)
    };

    // Generate Rust bindings via bindgen
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_path.display()))
        // ── avcodec ──
        .allowlist_function("avcodec_find_decoder")
        .allowlist_function("avcodec_alloc_context3")
        .allowlist_function("avcodec_free_context")
        .allowlist_function("avcodec_open2")
        .allowlist_function("avcodec_send_packet")
        .allowlist_function("avcodec_receive_frame")
        .allowlist_function("avcodec_flush_buffers")
        .allowlist_function("avcodec_find_encoder")
        .allowlist_function("avcodec_find_encoder_by_name")
        .allowlist_function("avcodec_send_frame")
        .allowlist_function("avcodec_receive_packet")
        .allowlist_function("avcodec_parameters_to_context")
        .allowlist_function("av_packet_alloc")
        .allowlist_function("av_packet_free")
        .allowlist_function("av_packet_unref")
        // ── avutil ──
        .allowlist_function("av_frame_alloc")
        .allowlist_function("av_frame_free")
        .allowlist_function("av_frame_unref")
        .allowlist_function("av_frame_get_buffer")
        .allowlist_function("av_image_get_buffer_size")
        .allowlist_function("av_image_fill_arrays")
        .allowlist_function("av_opt_set")
        .allowlist_function("av_opt_set_int")
        .allowlist_function("av_log_set_level")
        .allowlist_function("av_get_default_channel_layout")
        .allowlist_function("av_samples_get_buffer_size")
        .allowlist_function("av_channel_layout_default")
        // ── swscale ──
        .allowlist_function("sws_getContext")
        .allowlist_function("sws_scale")
        .allowlist_function("sws_freeContext")
        // ── Types ──
        .allowlist_type("AVCodecContext")
        .allowlist_type("AVCodec")
        .allowlist_type("AVCodecID")
        .allowlist_type("AVFrame")
        .allowlist_type("AVPacket")
        .allowlist_type("AVPixelFormat")
        .allowlist_type("AVSampleFormat")
        .allowlist_type("AVChannelLayout")
        .allowlist_type("SwsContext")
        // ── Constants ──
        .allowlist_var("AV_CODEC_ID_.*")
        .allowlist_var("AV_PIX_FMT_.*")
        .allowlist_var("AV_SAMPLE_FMT_.*")
        .allowlist_var("AV_CH_LAYOUT_.*")
        .allowlist_var("SWS_.*")
        .allowlist_var("AV_LOG_.*")
        .allowlist_var("AV_PKT_FLAG_.*")
        .allowlist_var("AVERROR.*")
        .allowlist_var("AV_INPUT_BUFFER_PADDING_SIZE")
        .allowlist_var("AV_CODEC_FLAG_.*")
        .allowlist_var("FF_COMPLIANCE_.*")
        .allowlist_var("FF_PROFILE_.*")
        .derive_debug(true)
        .derive_copy(true)
        .derive_default(true)
        .generate()
        .expect("bindgen failed to generate FFmpeg bindings");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}

/// Build libopus from vendored source using CMake.
/// Returns the install directory.
fn build_opus(out_dir: &PathBuf) -> PathBuf {
    let opus_source = PathBuf::from("vendor/opus");
    if !opus_source.exists() {
        panic!(
            "Vendored opus source not found at {}. \
             Clone it with: git submodule update --init",
            opus_source.display()
        );
    }

    let install_dir = out_dir.join("opus-install");

    cmake::Config::new(&opus_source)
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "ON")
        .define("OPUS_INSTALL_CMAKE_CONFIG_MODULE", "OFF")
        .define("CMAKE_INSTALL_PREFIX", install_dir.to_str().unwrap())
        .build();

    install_dir
}

/// Build FFmpeg from vendored source using ./configure + make.
fn build_vendored(out_dir: &PathBuf) -> PathBuf {
    let ffmpeg_source = PathBuf::from("vendor/ffmpeg");
    if !ffmpeg_source.exists() {
        panic!(
            "Vendored FFmpeg source not found at {}. \
             Clone it with: git submodule update --init, \
             or set LIBFFMPEG_DIR to a pre-built install, \
             or enable the system-ffmpeg feature.",
            ffmpeg_source.display()
        );
    }

    // Build libopus first
    let opus_install = build_opus(out_dir);
    let opus_include = opus_install.join("include");
    let opus_lib = opus_install.join("lib");
    // Some systems use lib64
    let opus_lib = if opus_lib.exists() { opus_lib } else { opus_install.join("lib64") };

    let install_dir = out_dir.join("ffmpeg-install");
    let build_dir = out_dir.join("ffmpeg-build");

    std::fs::create_dir_all(&build_dir).expect("failed to create build dir");
    std::fs::create_dir_all(&install_dir).expect("failed to create install dir");

    let source_abs = std::fs::canonicalize(&ffmpeg_source)
        .expect("failed to canonicalize ffmpeg source path");

    // Determine number of parallel jobs
    let num_jobs = std::thread::available_parallelism()
        .map(|n| n.get().to_string())
        .unwrap_or_else(|_| "4".to_string());

    // Run ./configure with minimal flags
    let configure_path = source_abs.join("configure");
    let extra_cflags = format!("-I{}", opus_include.display());
    let extra_ldflags = format!("-L{}", opus_lib.display());

    let opus_pkgconfig = opus_lib.join("pkgconfig");
    if !opus_pkgconfig.join("opus.pc").exists() {
        panic!(
            "opus.pc not found at {}. Vendored libopus did not install its pkg-config module.",
            opus_pkgconfig.display()
        );
    }
    eprintln!("cargo:warning=Using PKG_CONFIG_PATH={}", opus_pkgconfig.display());

    // Sanity check: run pkg-config directly with the same env we're about to pass
    // to configure, to verify env propagation works.
    let pc_check = Command::new("pkg-config")
        .env("PKG_CONFIG_PATH", &opus_pkgconfig)
        .args(["--exists", "--print-errors", "opus"])
        .status();
    match pc_check {
        Ok(s) if s.success() => eprintln!("cargo:warning=pkg-config --exists opus: OK"),
        Ok(s) => panic!("pkg-config --exists opus failed (status {s}) despite PKG_CONFIG_PATH set"),
        Err(e) => panic!("failed to run pkg-config: {e}"),
    }

    let status = Command::new(&configure_path)
        .current_dir(&build_dir)
        .env("PKG_CONFIG_PATH", &opus_pkgconfig)
        .env("PKG_CONFIG_LIBDIR", &opus_pkgconfig)
        .args([
            &format!("--prefix={}", install_dir.display()),
            "--disable-everything",
            "--disable-programs",
            "--disable-doc",
            "--disable-avdevice",
            "--disable-avformat",
            "--disable-network",
            "--disable-postproc",
            "--disable-avfilter",
            "--enable-avcodec",
            "--enable-avutil",
            "--enable-swscale",
            // Video decoders
            "--enable-decoder=h264",
            "--enable-decoder=hevc",
            // Video encoder (thumbnails)
            "--enable-encoder=mjpeg",
            // Audio encoders
            "--enable-libopus",
            "--enable-encoder=libopus",
            "--enable-encoder=mp2",
            "--enable-encoder=ac3",
            // Static only
            "--enable-static",
            "--disable-shared",
            // Opus include/lib paths
            &format!("--extra-cflags={extra_cflags}"),
            &format!("--extra-ldflags={extra_ldflags}"),
            // Disable optional deps that may be detected on the system
            "--disable-zlib",
            "--disable-bzlib",
            "--disable-lzma",
            "--disable-iconv",
            "--disable-sdl2",
            "--disable-xlib",
            "--disable-libxcb",
            "--disable-securetransport",
            "--disable-vulkan",
            "--disable-metal",
            "--disable-audiotoolbox",
            "--disable-videotoolbox",
            // Suppress assembly if nasm/yasm not available (fallback to C)
            "--disable-x86asm",
        ])
        .status()
        .expect("failed to execute FFmpeg configure");

    if !status.success() {
        panic!("FFmpeg configure failed");
    }

    // Build
    let status = Command::new("make")
        .current_dir(&build_dir)
        .args(["-j", &num_jobs])
        .status()
        .expect("failed to execute make");

    if !status.success() {
        panic!("FFmpeg make failed");
    }

    // Install
    let status = Command::new("make")
        .current_dir(&build_dir)
        .arg("install")
        .status()
        .expect("failed to execute make install");

    if !status.success() {
        panic!("FFmpeg make install failed");
    }

    // Link
    let lib_dir = install_dir.join("lib");
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    // Also add opus lib path so the linker can find libopus
    println!("cargo:rustc-link-search=native={}", opus_lib.display());
    link_ffmpeg_libs(true);

    install_dir.join("include")
}

fn link_ffmpeg_libs(include_opus: bool) {
    // Order matters: avcodec depends on avutil and swscale depends on avutil
    println!("cargo:rustc-link-lib=static=avcodec");
    println!("cargo:rustc-link-lib=static=swscale");
    println!("cargo:rustc-link-lib=static=avutil");

    // libopus is statically linked into avcodec for the vendored build
    if include_opus {
        println!("cargo:rustc-link-lib=static=opus");
    }

    // Platform-specific system libs that FFmpeg requires
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "linux" => {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=pthread");
        }
        "macos" => {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=pthread");
        }
        _ => {}
    }
}
