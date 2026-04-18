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
        .allowlist_function("av_dict_set")
        .allowlist_function("av_dict_free")
        .allowlist_function("av_rescale_q")
        .allowlist_type("AVDictionary")
        .allowlist_type("AVRational")
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
    let mut extra_cflags = format!("-I{}", opus_include.display());
    let mut extra_ldflags = format!("-L{}", opus_lib.display());

    let opus_pkgconfig = opus_lib.join("pkgconfig");
    if !opus_pkgconfig.join("opus.pc").exists() {
        panic!(
            "opus.pc not found at {}. Vendored libopus did not install its pkg-config module.",
            opus_pkgconfig.display()
        );
    }

    // ── Optional GPL/non-free video encoders ──
    //
    // These features are off by default. Enabling them links
    // system-installed encoder libraries into the vendored FFmpeg build
    // and — for libx264/libx265 — flips the whole FFmpeg binary to GPL.
    // Operators must install the libraries themselves and accept the
    // licence implications.
    let mut configure_args: Vec<String> = vec![
        format!("--prefix={}", install_dir.display()),
        "--disable-everything".into(),
        "--disable-programs".into(),
        "--disable-doc".into(),
        "--disable-avdevice".into(),
        "--disable-avformat".into(),
        "--disable-network".into(),
        "--disable-postproc".into(),
        "--disable-avfilter".into(),
        "--enable-avcodec".into(),
        "--enable-avutil".into(),
        "--enable-swscale".into(),
        // Video decoders
        "--enable-decoder=h264".into(),
        "--enable-decoder=hevc".into(),
        // Video encoder (thumbnails)
        "--enable-encoder=mjpeg".into(),
        // Audio encoders
        "--enable-libopus".into(),
        "--enable-encoder=libopus".into(),
        "--enable-encoder=mp2".into(),
        "--enable-encoder=ac3".into(),
        // Static only
        "--enable-static".into(),
        "--disable-shared".into(),
        // Needed so --libs returns Libs.private (e.g. -lm for static libopus)
        "--pkg-config-flags=--static".into(),
        // Disable optional deps that may be detected on the system
        "--disable-zlib".into(),
        "--disable-bzlib".into(),
        "--disable-lzma".into(),
        "--disable-iconv".into(),
        "--disable-sdl2".into(),
        "--disable-xlib".into(),
        "--disable-libxcb".into(),
        "--disable-securetransport".into(),
        "--disable-vulkan".into(),
        "--disable-metal".into(),
        "--disable-audiotoolbox".into(),
        "--disable-videotoolbox".into(),
        // Suppress assembly if nasm/yasm not available (fallback to C)
        "--disable-x86asm".into(),
    ];

    // Aggregated pkg-config search path so FFmpeg's configure can find
    // every enabled optional library at once.
    let mut pkgconfig_paths: Vec<PathBuf> = vec![opus_pkgconfig.clone()];

    let gpl_required = cfg!(feature = "video-encoder-x264")
        || cfg!(feature = "video-encoder-x265");
    if gpl_required {
        println!("cargo:warning=libffmpeg-video-sys: GPL encoder feature enabled — the resulting FFmpeg library is GPL v2+. Any bilbycast-edge binary linking it inherits GPL terms.");
        configure_args.push("--enable-gpl".into());
    }

    if cfg!(feature = "video-encoder-x264") {
        let x264 = pkg_config::Config::new()
            .probe("x264")
            .expect(
                "pkg-config: x264 not found. \
                 Install libx264-dev (Debian/Ubuntu) or `brew install x264` (macOS) \
                 to build with the video-encoder-x264 feature.",
            );
        for inc in &x264.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &x264.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        configure_args.push("--enable-libx264".into());
        configure_args.push("--enable-encoder=libx264".into());
    }

    if cfg!(feature = "video-encoder-x265") {
        let x265 = pkg_config::Config::new()
            .probe("x265")
            .expect(
                "pkg-config: x265 not found. \
                 Install libx265-dev (Debian/Ubuntu) or `brew install x265` (macOS) \
                 to build with the video-encoder-x265 feature.",
            );
        for inc in &x265.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &x265.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        configure_args.push("--enable-libx265".into());
        configure_args.push("--enable-encoder=libx265".into());
    }

    if cfg!(feature = "video-encoder-nvenc") {
        // NVENC is a header-only interface plus runtime driver. The
        // FFmpeg-side headers live in nv-codec-headers; we expect them
        // to be discoverable by pkg-config name "ffnvcodec". `--enable-nonfree`
        // is not strictly required for NVENC headers but is commonly
        // combined with it in distributions.
        let nv = pkg_config::Config::new().probe("ffnvcodec").expect(
            "pkg-config: ffnvcodec not found. \
             Install nv-codec-headers and the NVIDIA driver to build with \
             the video-encoder-nvenc feature.",
        );
        for inc in &nv.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        configure_args.push("--enable-nvenc".into());
        configure_args.push("--enable-encoder=h264_nvenc".into());
        configure_args.push("--enable-encoder=hevc_nvenc".into());
    }

    // Extra cflags / ldflags must be passed last (accumulated across
    // every optional dep above).
    configure_args.push(format!("--extra-cflags={extra_cflags}"));
    configure_args.push(format!("--extra-ldflags={extra_ldflags}"));

    // Join our extra pkg-config dirs (vendored opus + any -L paths surfaced by
    // x264/x265/ffnvcodec probes) with the platform separator. Setting
    // PKG_CONFIG_PATH *prepends* to pkg-config's compile-time default search
    // path, so system-installed libraries (x264/x265/ffnvcodec on Ubuntu) stay
    // discoverable. Do NOT set PKG_CONFIG_LIBDIR — that would *replace* the
    // defaults and hide system .pc files (pkg-config suppresses -L for system
    // lib dirs, so the Rust pkg-config crate returns empty link_paths for
    // them and we'd have nothing to repopulate those paths with).
    let joined_pkgconfig = std::env::join_paths(pkgconfig_paths.iter())
        .expect("failed to join pkg-config paths");
    eprintln!(
        "libffmpeg-video-sys: PKG_CONFIG_PATH={}",
        joined_pkgconfig.to_string_lossy()
    );

    let status = Command::new(&configure_path)
        .current_dir(&build_dir)
        .env("PKG_CONFIG_PATH", &joined_pkgconfig)
        .env_remove("PKG_CONFIG_LIBDIR")
        .args(&configure_args)
        .status()
        .expect("failed to execute FFmpeg configure");

    if !status.success() {
        // Surface FFmpeg's config.log so CI logs explain *why* configure died
        // (e.g. a specific pkg-config probe for --static dep resolution).
        let config_log = build_dir.join("ffbuild").join("config.log");
        if let Ok(contents) = std::fs::read_to_string(&config_log) {
            eprintln!(
                "===== FFmpeg ffbuild/config.log (tail, last 200 lines) =====\n{}\n===== end =====",
                contents.lines().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"),
            );
        } else {
            eprintln!("(could not read {})", config_log.display());
        }
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

    // Optional video encoder libraries. These must be findable by the
    // system linker; the pkg-config probes above already emitted
    // `cargo:rustc-link-search=` directives. We only have to name them
    // here so the final rustc invocation pulls them in.
    if cfg!(feature = "video-encoder-x264") {
        // libx264 is typically shipped statically; prefer static but
        // fall back to dylib lookup if the system only has .so/.dylib.
        println!("cargo:rustc-link-lib=x264");
    }
    if cfg!(feature = "video-encoder-x265") {
        println!("cargo:rustc-link-lib=x265");
        // libx265 is C++; pull in the C++ runtime so the static link
        // succeeds on Linux.
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        if target_os == "linux" {
            println!("cargo:rustc-link-lib=stdc++");
        } else if target_os == "macos" {
            println!("cargo:rustc-link-lib=c++");
        }
    }

    // Platform-specific system libs that FFmpeg requires
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "linux" => {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=pthread");
            if cfg!(feature = "video-encoder-nvenc") {
                println!("cargo:rustc-link-lib=dl");
            }
        }
        "macos" => {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=pthread");
        }
        _ => {}
    }
}
