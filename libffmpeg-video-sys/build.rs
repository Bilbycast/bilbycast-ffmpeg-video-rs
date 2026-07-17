// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
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
        link_ffmpeg_libs(false, &GplEncoderLink::default());
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
        .allowlist_function("avcodec_find_decoder_by_name")
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
        .allowlist_function("av_frame_is_writable")
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
        // ── hwcontext (VAAPI / DRM PRIME runtime plumbing) ──
        // Powers the bilbycast-edge `display` output's zero-copy path:
        // open a VAAPI hwdevice on /dev/dri/renderD128, decode into VAAPI
        // surfaces, map each surface to an `AVDRMFrameDescriptor` (DMA-BUF
        // fds + fourcc + modifier + plane offsets/strides) and KMS-import
        // it via `drmModeAddFB2WithModifiers`, eliminating the CPU
        // YUV→XRGB blit. The codec-side wiring is via `hw_device_ctx`
        // + the `get_format` callback returning AV_PIX_FMT_VAAPI; the
        // decoder fills `hw_frames_ctx` automatically when the callback
        // calls `avcodec_get_hw_frames_parameters`.
        .allowlist_function("av_hwdevice_ctx_create")
        .allowlist_function("av_hwdevice_ctx_alloc")
        .allowlist_function("av_hwdevice_ctx_init")
        .allowlist_function("av_hwframe_ctx_alloc")
        .allowlist_function("av_hwframe_ctx_init")
        .allowlist_function("av_hwframe_map")
        .allowlist_function("av_hwframe_transfer_data")
        .allowlist_function("av_hwframe_get_buffer")
        .allowlist_function("avcodec_get_hw_frames_parameters")
        .allowlist_function("avcodec_get_hw_config")
        .allowlist_function("av_buffer_ref")
        .allowlist_function("av_buffer_unref")
        .allowlist_function("av_buffer_alloc")
        .allowlist_type("AVHWDeviceType")
        .allowlist_type("AVHWDeviceContext")
        .allowlist_type("AVHWFramesContext")
        .allowlist_type("AVHWFramesConstraints")
        .allowlist_type("AVHWAccel")
        .allowlist_type("AVCodecHWConfig")
        .allowlist_type("AVVAAPIDeviceContext")
        .allowlist_type("AVVAAPIFramesContext")
        .allowlist_type("AVDRMFrameDescriptor")
        .allowlist_type("AVDRMObjectDescriptor")
        .allowlist_type("AVDRMLayerDescriptor")
        .allowlist_type("AVDRMPlaneDescriptor")
        .allowlist_type("AVDRMDeviceContext")
        .allowlist_type("AVBufferRef")
        .allowlist_var("AV_HWDEVICE_TYPE_.*")
        .allowlist_var("AV_HWFRAME_MAP_.*")
        .allowlist_var("AV_CODEC_HW_CONFIG_METHOD_.*")
        .allowlist_var("AV_DRM_MAX_PLANES")
        .allowlist_type("AVDictionary")
        .allowlist_type("AVRational")
        // ── swscale ──
        .allowlist_function("sws_getContext")
        .allowlist_function("sws_scale")
        .allowlist_function("sws_freeContext")
        .allowlist_function("sws_setColorspaceDetails")
        .allowlist_function("sws_getCoefficients")
        // ── swresample ──
        // Used by the audio decoder to normalise non-fltp source formats
        // (MP2 → s16p, AC-3 → fltp, Opus → fltp, etc.) into planar f32
        // for the bilbycast-edge audio pipeline.
        .allowlist_function("swr_alloc_set_opts2")
        .allowlist_function("swr_init")
        .allowlist_function("swr_free")
        .allowlist_function("swr_convert")
        .allowlist_function("swr_get_delay")
        .allowlist_function("swr_get_out_samples")
        .allowlist_type("SwrContext")
        // ── Types ──
        .allowlist_type("AVCodecContext")
        .allowlist_type("AVCodec")
        .allowlist_type("AVCodecID")
        .allowlist_type("AVFrame")
        .allowlist_type("AVPacket")
        .allowlist_type("AVPixelFormat")
        .allowlist_type("AVColorSpace")
        .allowlist_type("AVColorRange")
        .allowlist_type("AVSampleFormat")
        .allowlist_type("AVChannelLayout")
        .allowlist_type("SwsContext")
        // ── Constants ──
        .allowlist_var("AV_CODEC_ID_.*")
        .allowlist_var("AV_PIX_FMT_.*")
        .allowlist_var("AVCOL_SPC_.*")
        .allowlist_var("AVCOL_RANGE_.*")
        .allowlist_var("AV_SAMPLE_FMT_.*")
        .allowlist_var("AV_CH_LAYOUT_.*")
        .allowlist_var("SWS_.*")
        .allowlist_var("AV_LOG_.*")
        .allowlist_var("AV_PKT_FLAG_.*")
        // AVFrame.key_frame / .interlaced_frame / .top_field_first were removed
        // in FFmpeg 8.0 (FF_API_FRAME_KEY / FF_API_INTERLACED_FRAME); the
        // replacement is the AV_FRAME_FLAG_* bitmask on AVFrame.flags (present
        // since 6.1, so this also works on the current n7.1.3 tree).
        .allowlist_var("AV_FRAME_FLAG_.*")
        .allowlist_var("AVERROR.*")
        .allowlist_var("AV_INPUT_BUFFER_PADDING_SIZE")
        .allowlist_var("AV_CODEC_FLAG_.*")
        .allowlist_var("FF_COMPLIANCE_.*")
        // FF_PROFILE_* was renamed to AV_PROFILE_* in FFmpeg 8.0
        // (FF_API_FF_PROFILE_LEVEL). We set encoder profiles via string
        // AVOptions, so neither symbol set is load-bearing — allowlist both
        // so the build works across the 7.1 → 8.1 bump either way.
        .allowlist_var("FF_PROFILE_.*")
        .allowlist_var("AV_PROFILE_.*")
        .derive_debug(true)
        .derive_copy(true)
        .derive_default(true)
        .generate()
        .expect("bindgen failed to generate FFmpeg bindings");

    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings.rs");
}

/// Where the static archives for the GPL video encoders live, if findable.
///
/// libx264 / libx265 are dynamically linked by default, which ties the final
/// binary to the build host's `libx264.so.<X264_BUILD>` / `libx265.so.<api>`
/// SONAME — and those SONAMEs *are* the upstream build number, which bumps on
/// every distro release (Ubuntu 24.04 ships libx264.so.164 / libx265.so.199;
/// 26.04 ships libx264.so.165 / libx265.so.215, which are ABI-incompatible —
/// x264 even token-pastes the build number into the exported symbol names, so
/// a soname symlink fails at the symbol level). A dynamically-linked binary
/// therefore only runs on the exact distro release it was built against.
/// Preferring a static archive bakes the encoder into the binary so it has no
/// runtime DT_NEEDED on libx264/libx265 and runs on any host of the same arch.
#[derive(Default)]
struct GplEncoderLink {
    /// Directory containing `libx264.a`, when a static link is possible.
    x264_static_dir: Option<PathBuf>,
    /// Directory containing `libx265.a`, when a static link is possible.
    x265_static_dir: Option<PathBuf>,
}

/// Locate a static archive `lib<name>.a` on the probe's link paths or the
/// platform's standard library directories, returning its containing dir.
///
/// pkg-config suppresses `-L` for the standard system library directories, so
/// a distro-shipped archive (e.g. Debian/Ubuntu's `libx265.a` in the multiarch
/// dir) never appears in `probe_link_paths` — we must search the well-known
/// dirs explicitly. Archives surfaced via a non-standard prefix (e.g. a
/// from-source static `libx264.a` on `PKG_CONFIG_PATH`) *do* show up in
/// `probe_link_paths`, so we check those first.
fn find_static_archive(name: &str, probe_link_paths: &[PathBuf]) -> Option<PathBuf> {
    let archive = format!("lib{name}.a");
    let mut dirs: Vec<PathBuf> = probe_link_paths.to_vec();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let triplet = match arch.as_str() {
        "x86_64" => Some("x86_64-linux-gnu"),
        "aarch64" => Some("aarch64-linux-gnu"),
        _ => None,
    };
    if let Some(t) = triplet {
        dirs.push(PathBuf::from(format!("/usr/lib/{t}")));
        dirs.push(PathBuf::from(format!("/usr/local/lib/{t}")));
    }
    dirs.push(PathBuf::from("/usr/lib"));
    dirs.push(PathBuf::from("/usr/lib64"));
    dirs.push(PathBuf::from("/usr/local/lib"));
    dirs.into_iter().find(|d| d.join(&archive).is_file())
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
        // NOTE: --disable-postproc is added conditionally below — libpostproc
        // was removed entirely in FFmpeg 8.0, so the option no longer exists
        // there and configure hard-fails on it as unknown.
        "--disable-avfilter".into(),
        "--enable-avcodec".into(),
        "--enable-avutil".into(),
        "--enable-swscale".into(),
        // libswresample — required by the audio_decoder crate for codecs
        // whose native sample format isn't planar f32 (MP2 → s16p, AC-3 →
        // fltp, E-AC-3 → fltp, Opus → fltp). The decoder normalises every
        // codec to planar f32 so the bilbycast-edge audio path stays
        // uniform.
        "--enable-swresample".into(),
        // Video decoders
        "--enable-decoder=h264".into(),
        "--enable-decoder=hevc".into(),
        // MPEG-2 video covers DVB-T / ATSC / legacy contribution. The
        // libavcodec mpeg2video decoder also accepts MPEG-1 bitstreams,
        // so a single decoder covers both stream_type=0x01 and 0x02.
        "--enable-decoder=mpeg2video".into(),
        // mpegvideo parser — needed so libavcodec correctly slices the
        // raw ES we feed it into picture-aligned packets when the source
        // stream lacks AU framing on PES boundaries.
        "--enable-parser=mpegvideo".into(),
        // Video encoder (thumbnails)
        "--enable-encoder=mjpeg".into(),
        // Audio encoders
        "--enable-libopus".into(),
        "--enable-encoder=libopus".into(),
        "--enable-encoder=mp2".into(),
        "--enable-encoder=ac3".into(),
        // Audio decoders — feed the bilbycast-edge `display` output's
        // ALSA backend through the new video-engine `AudioDecoder`. AAC
        // stays on bilbycast-fdk-aac-rs (better quality, already in tree);
        // these are the broadcast codecs FDK doesn't cover, plus the
        // LATM/LOAS-framed AAC carriage common on Australian / Asian
        // DVB-T (`stream_type=0x11`) which fdk-aac's transport layer
        // doesn't unwrap — we feed libavcodec the LOAS frames after
        // the bilbycast-edge LOAS splitter strips sync + length.
        "--enable-decoder=mp2".into(),
        "--enable-decoder=ac3".into(),
        "--enable-decoder=eac3".into(),
        "--enable-decoder=libopus".into(),
        "--enable-decoder=aac_latm".into(),
        "--enable-parser=aac_latm".into(),
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
    ];

    // libpostproc was removed in FFmpeg 8.0. Only pass --disable-postproc on
    // trees that still ship it (n7.1.x and earlier); on 8.x the option is
    // unknown and configure aborts. Gate on the source tree so this build.rs
    // works across the 7.1.3 → 8.1 submodule bump either way.
    if source_abs.join("libpostproc").is_dir() {
        configure_args.push("--disable-postproc".into());
    }

    // libdrm: the bilbycast-edge `display` output drives KMS directly
    // via the `drm` Rust crate, so historically we disabled libdrm in
    // FFmpeg to avoid hwcontext_drm.o landing in libavutil and forcing
    // every downstream binary to link `-ldrm`. The VAAPI zero-copy path
    // in `display` needs FFmpeg's `vaapi_map_to_drm` (gated on
    // `CONFIG_LIBDRM` in `libavutil/hwcontext_vaapi.c`) to export a
    // decoded VAAPI surface as an `AVDRMFrameDescriptor` carrying
    // DMA-BUF fds the KMS scanout side imports as a PRIME framebuffer.
    // So flip the rule: keep libdrm off for builds without VAAPI (the
    // common AGPL-only edge), but enable it whenever any VAAPI feature
    // is on — they already pull libva-drm and are explicitly opting
    // into the GPU integration that needs DMA-BUF interop.
    // RKMPP also mandates --enable-libdrm (FFmpeg configure dies otherwise —
    // see configure:7500-7502): the MPP encoder's frame buffers are
    // DRM/DMA-BUF backed.
    let drm_required = cfg!(feature = "video-encoder-vaapi")
        || cfg!(feature = "video-decoder-vaapi")
        || cfg!(feature = "video-encoder-rkmpp");
    if drm_required {
        configure_args.push("--enable-libdrm".into());
    } else {
        configure_args.push("--disable-libdrm".into());
    }

    // VAAPI auto-detects from `libva` on the host. Even when the
    // operator hasn't enabled `video-encoder-vaapi` / `video-decoder-vaapi`,
    // a build host that ships libva (typical Mesa / Intel driver
    // install on Linux) makes FFmpeg pull `hwcontext_vaapi.o` into
    // libavutil and `vaapi_*` references into every linked binary.
    // Without `libva-dev` headers / `pkg-config` lookups completing
    // the link side, the final `bilbycast-edge` link fails with
    // `undefined reference to vaInitialize` etc. Force VAAPI off
    // unless an explicit VAAPI feature was selected — those branches
    // run libva probes below and append `--enable-vaapi`.
    if !cfg!(feature = "video-encoder-vaapi") && !cfg!(feature = "video-decoder-vaapi")
    {
        configure_args.push("--disable-vaapi".into());
    }

    // x86 assembly. The C-only fallback in libswscale (Lanczos / bilinear
    // YUV→YUV downscale) corrupts the chroma planes on x86_64 — every
    // thumbnail comes back with magenta-and-green stripe artifacts. So
    // require nasm on x86_64 and enable assembly when present.
    // On other architectures (ARM/AArch64), the C path is fine.
    //
    // NOTE: FFmpeg 8.0 dropped yasm support entirely — nasm is now mandatory
    // for x86 assembly. A host that has yasm but not nasm would build fine on
    // n7.1.3 but the 8.x configure now dies ("nasm not found or too old"). So
    // we gate strictly on nasm and gracefully fall back to --disable-x86asm
    // (correct, degraded) rather than letting configure fail.
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_arch == "x86" || target_arch == "x86_64" {
        let have_asm = which("nasm");
        if have_asm {
            // Default behaviour — let configure auto-enable x86asm.
        } else {
            println!(
                "cargo:warning=libffmpeg-video-sys: nasm not found on x86_64 (FFmpeg 8.0+ requires nasm; yasm is no longer supported). The vendored FFmpeg will be built without x86 assembly, which causes libswscale to corrupt YUV downscale output (every thumbnail will come back with stripe artifacts). Install nasm: `sudo apt install nasm` (Debian/Ubuntu) or `brew install nasm` (macOS x86)."
            );
            configure_args.push("--disable-x86asm".into());
        }
    }

    // Aggregated pkg-config search path so FFmpeg's configure can find
    // every enabled optional library at once.
    //
    // Seed with the caller's PKG_CONFIG_PATH. The `pkg_config` probes below
    // honour that variable, so omitting it here lets a dependency look present
    // to us and missing to FFmpeg's own configure — which is exactly what
    // happens with a header-only .pc such as ffnvcodec installed to a custom
    // prefix: it contributes no `link_paths`, so nothing re-adds its directory.
    let mut pkgconfig_paths: Vec<PathBuf> = vec![opus_pkgconfig.clone()];
    if let Some(inherited) = std::env::var_os("PKG_CONFIG_PATH") {
        println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");
        pkgconfig_paths.extend(std::env::split_paths(&inherited));
    }

    let gpl_required = cfg!(feature = "video-encoder-x264")
        || cfg!(feature = "video-encoder-x265");
    if gpl_required {
        println!("cargo:warning=libffmpeg-video-sys: GPL encoder feature enabled — the resulting FFmpeg library is GPL v2+. Any bilbycast-edge binary linking it inherits GPL terms.");
        configure_args.push("--enable-gpl".into());
    }

    // Static-vs-dynamic decision for the GPL encoders is made here (we have the
    // pkg-config probe link paths) and applied in `link_ffmpeg_libs`.
    let mut gpl_link = GplEncoderLink::default();

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
        gpl_link.x264_static_dir = find_static_archive("x264", &x264.link_paths);
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
        gpl_link.x265_static_dir = find_static_archive("x265", &x265.link_paths);
        configure_args.push("--enable-libx265".into());
        configure_args.push("--enable-encoder=libx265".into());
    }

    // NVENC encoder + NVDEC decoder share `nv-codec-headers` + the
    // NVIDIA proprietary runtime driver. Probe pkg-config once when
    // either feature is on so we don't re-probe on combined builds.
    let nv_required = cfg!(feature = "video-encoder-nvenc")
        || cfg!(feature = "video-decoder-nvdec");
    if nv_required {
        let nv = pkg_config::Config::new().probe("ffnvcodec").expect(
            "pkg-config: ffnvcodec not found. \
             Install nv-codec-headers and the NVIDIA driver to build with \
             the video-encoder-nvenc / video-decoder-nvdec features.",
        );
        for inc in &nv.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
    }

    if cfg!(feature = "video-encoder-nvenc") {
        configure_args.push("--enable-nvenc".into());
        configure_args.push("--enable-encoder=h264_nvenc".into());
        configure_args.push("--enable-encoder=hevc_nvenc".into());
    }

    if cfg!(feature = "video-decoder-nvdec") {
        // CUVID is the FFmpeg name for the NVDEC-driven decoders. The
        // `--enable-cuvid` flag wires up the codec parsers + bitstream
        // filters; the per-codec `--enable-decoder=*_cuvid` lines pull
        // in the actual decoder entries (`h264_cuvid`, `hevc_cuvid`).
        // No `--enable-nonfree` needed — the cuvid headers are
        // permissive at the FFmpeg layer.
        configure_args.push("--enable-cuvid".into());
        configure_args.push("--enable-decoder=h264_cuvid".into());
        configure_args.push("--enable-decoder=hevc_cuvid".into());
    }

    // QSV encoder + decoder share libvpl. Probe pkg-config once when
    // either feature is on. `--enable-libvpl` is also added once.
    let vpl_required = cfg!(feature = "video-encoder-qsv")
        || cfg!(feature = "video-decoder-qsv");
    if vpl_required {
        // Intel QuickSync via oneVPL (libvpl). Modern path — replaces the
        // legacy MediaSDK / `--enable-libmfx`. Headers + dispatcher are
        // MIT/Apache; no `--enable-nonfree` needed. The pkg-config module
        // shipped by `libvpl-dev` (Debian/Ubuntu 24.04+) is named "vpl".
        let vpl = pkg_config::Config::new().probe("vpl").expect(
            "pkg-config: vpl not found. \
             Install libvpl-dev (Debian/Ubuntu 24.04+) to build with the \
             video-encoder-qsv / video-decoder-qsv features.",
        );
        for inc in &vpl.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &vpl.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        configure_args.push("--enable-libvpl".into());
    }

    if cfg!(feature = "video-encoder-qsv") {
        configure_args.push("--enable-encoder=h264_qsv".into());
        configure_args.push("--enable-encoder=hevc_qsv".into());
    }

    if cfg!(feature = "video-decoder-qsv") {
        configure_args.push("--enable-decoder=h264_qsv".into());
        configure_args.push("--enable-decoder=hevc_qsv".into());
    }

    // VAAPI encoder + decoder share libva. Probe pkg-config once when
    // either feature is on. VAAPI on Linux opens render nodes
    // (`/dev/dri/renderD*`) via libva-drm; FFmpeg's `--enable-vaapi`
    // pulls in libva-drm automatically when libdrm is detected, and the
    // build needs both `libva` and `libva-drm` system libraries linked
    // into the final binary because FFmpeg's static archives don't
    // contain them.
    let va_required = cfg!(feature = "video-encoder-vaapi")
        || cfg!(feature = "video-decoder-vaapi");
    if va_required {
        let va = pkg_config::Config::new().probe("libva").expect(
            "pkg-config: libva not found. \
             Install libva-dev (Debian/Ubuntu) to build with the \
             video-encoder-vaapi / video-decoder-vaapi features.",
        );
        for inc in &va.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &va.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        // libva-drm is a separate pkg-config module; required for
        // `vaGetDisplayDRM` which is how FFmpeg opens render nodes.
        let _va_drm = pkg_config::Config::new().probe("libva-drm").expect(
            "pkg-config: libva-drm not found. \
             Install libva-dev (it ships libva-drm.pc) to build with the \
             video-encoder-vaapi / video-decoder-vaapi features.",
        );
        // libdrm: needed for FFmpeg's `vaapi_map_to_drm` (DMA-BUF export
        // of decoded VAAPI surfaces). Without `--enable-libdrm` +
        // `<drm_fourcc.h>` headers the DRM PRIME mapping path in
        // hwcontext_vaapi.c is compiled out and the bilbycast-edge
        // `display` zero-copy scanout falls back to the CPU blit.
        let libdrm = pkg_config::Config::new().probe("libdrm").expect(
            "pkg-config: libdrm not found. \
             Install libdrm-dev (Debian/Ubuntu) to build with the \
             video-encoder-vaapi / video-decoder-vaapi features.",
        );
        for inc in &libdrm.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &libdrm.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        configure_args.push("--enable-vaapi".into());
    }

    if cfg!(feature = "video-encoder-vaapi") {
        configure_args.push("--enable-encoder=h264_vaapi".into());
        configure_args.push("--enable-encoder=hevc_vaapi".into());
    }

    if cfg!(feature = "video-decoder-vaapi") {
        // VAAPI decode is implemented as a HWACCEL on the regular SW
        // decoder (not a separate codec entry like NVDEC's `h264_cuvid`),
        // so `--enable-hwaccel=*` is the correct flag here. Using
        // `--enable-decoder=hevc_vaapi` silently no-ops because there's
        // no decoder of that name — the VAAPI hwaccel rides into the
        // standard `hevc` / `h264` decoder via `hw_configs` once the
        // caller sets `hw_device_ctx` + returns `AV_PIX_FMT_VAAPI` from
        // the `get_format` callback.
        //
        // We enable hwaccels for every codec the bilbycast-edge
        // `display` output can hand a frame for: H.264, HEVC, and
        // MPEG-2. Keeping `mpeg2_vaapi` on means a DVB-T MPEG-2
        // source on a host with VAAPI also benefits from the
        // zero-copy DMA-BUF + KMS-PRIME scanout path — without it
        // the resolver's Auto-preference for VAAPI would silently
        // demote MPEG-2 to SW decode (still correct, but the
        // `decoder_kind` label would lie).
        configure_args.push("--enable-hwaccel=h264_vaapi".into());
        configure_args.push("--enable-hwaccel=hevc_vaapi".into());
        configure_args.push("--enable-hwaccel=mpeg2_vaapi".into());
    }

    // RKMPP: Rockchip Media Process Platform hardware H.264/HEVC encode
    // (`h264_rkmpp` / `hevc_rkmpp`, upstream since FFmpeg 8.1). ARM Rockchip
    // SoCs only (RK3568 / RK3588 / ...). FFmpeg's configure hard-requires
    // pkg-config `rockchip_mpp >= 1.3.8` and `--enable-libdrm`
    // (configure:7500-7502). `--enable-libdrm` is emitted via the shared
    // `drm_required` gate above (which now includes this feature). The
    // library is distributed only as `librockchip_mpp.so` from the Rockchip
    // BSP, so it is linked dynamically in `link_ffmpeg_libs`. There is no
    // `rockchip_mpp` on x86_64 — this feature is only buildable on an
    // aarch64 Rockchip host, where the probe below finds the BSP package.
    if cfg!(feature = "video-encoder-rkmpp") {
        let mpp = pkg_config::Config::new().probe("rockchip_mpp").expect(
            "pkg-config: rockchip_mpp not found. The video-encoder-rkmpp \
             feature is ARM Rockchip-only and needs the Rockchip BSP \
             librockchip-mpp-dev (>= 1.3.8, ships rockchip_mpp.pc) at build \
             time. It cannot be built on x86_64.",
        );
        for inc in &mpp.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &mpp.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        // rkmpp's FFmpeg code needs the libdrm headers at configure time
        // (DRM/DMA-BUF-backed MPP buffers). Probe libdrm so its include
        // path lands on extra_cflags even when VAAPI is not also enabled;
        // if both are on the extra `-I`/`-L` are harmless duplicates.
        let libdrm = pkg_config::Config::new().probe("libdrm").expect(
            "pkg-config: libdrm not found. video-encoder-rkmpp requires \
             libdrm-dev (FFmpeg's rkmpp code is gated on --enable-libdrm).",
        );
        for inc in &libdrm.include_paths {
            extra_cflags.push(' ');
            extra_cflags.push_str(&format!("-I{}", inc.display()));
        }
        for lp in &libdrm.link_paths {
            extra_ldflags.push(' ');
            extra_ldflags.push_str(&format!("-L{}", lp.display()));
            pkgconfig_paths.push(lp.join("pkgconfig"));
        }
        // FFmpeg classifies the Rockchip MPP wrapper as a `version3` external
        // library, so `--enable-rkmpp` hard-requires `--enable-version3` —
        // configure aborts otherwise ("rkmpp is version3 and --enable-version3
        // is not specified"). License-compatible: the rockchip artefact is
        // already AGPL-3.0-or-later via `--enable-gpl`, and version3
        // ((L)GPLv3) relicensing is subsumed by AGPL-3.0.
        configure_args.push("--enable-version3".into());
        configure_args.push("--enable-rkmpp".into());
        configure_args.push("--enable-encoder=h264_rkmpp".into());
        configure_args.push("--enable-encoder=hevc_rkmpp".into());
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
    link_ffmpeg_libs(true, &gpl_link);

    install_dir.join("include")
}

fn link_ffmpeg_libs(include_opus: bool, gpl: &GplEncoderLink) {
    // Order matters: avcodec depends on avutil; swscale and swresample
    // both depend on avutil. swresample is consumed by the audio decoder
    // for sample-format conversion.
    println!("cargo:rustc-link-lib=static=avcodec");
    println!("cargo:rustc-link-lib=static=swscale");
    println!("cargo:rustc-link-lib=static=swresample");
    println!("cargo:rustc-link-lib=static=avutil");

    // libopus is statically linked into avcodec for the vendored build
    if include_opus {
        println!("cargo:rustc-link-lib=static=opus");
    }

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // Optional GPL video encoder libraries (libx264 / libx265). These must
    // appear AFTER libavcodec on the link line because avcodec references
    // their symbols. PREFER a static link (see `GplEncoderLink`): baking the
    // encoder in means the final binary carries no runtime DT_NEEDED on
    // libx264.so.<build> / libx265.so.<build>, whose SONAMEs bump on every
    // distro release and are ABI-incompatible across releases. Fall back to a
    // dynamic link with a loud warning when no static archive is available.
    if cfg!(feature = "video-encoder-x264") {
        if let Some(dir) = &gpl.x264_static_dir {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static=x264");
        } else {
            println!("cargo:rustc-link-lib=x264");
            println!("cargo:warning=libffmpeg-video-sys: no static libx264.a found — linking libx264 DYNAMICALLY. The binary will require the build host's libx264.so.<build> SONAME at runtime and will NOT start on a distro shipping a different x264 build (e.g. Ubuntu 26.04's libx264.so.165 vs 24.04's .164 — x264 bakes the build number into its symbol names, so a soname symlink also fails). Build x264 with `--enable-static` and put its prefix on PKG_CONFIG_PATH for a portable binary.");
        }
    }
    if cfg!(feature = "video-encoder-x265") {
        let x265_static = gpl.x265_static_dir.is_some();
        if let Some(dir) = &gpl.x265_static_dir {
            println!("cargo:rustc-link-search=native={}", dir.display());
            println!("cargo:rustc-link-lib=static=x265");
        } else {
            println!("cargo:rustc-link-lib=x265");
            println!("cargo:warning=libffmpeg-video-sys: no static libx265.a found — linking libx265 DYNAMICALLY. The binary will require the build host's libx265.so SONAME at runtime and will NOT start on a distro shipping a different x265 build (e.g. Ubuntu 26.04's libx265.so.215 vs 24.04's .199). Install libx265-dev (ships libx265.a on Debian/Ubuntu) for a portable static link.");
        }
        // libx265 is C++; pull in the C++ runtime so the link resolves.
        if target_os == "linux" {
            println!("cargo:rustc-link-lib=stdc++");
            // A static libx265.a's transitive deps are no longer satisfied by
            // a shared object — they become DIRECT deps of the final binary.
            // libnuma is x265's NUMA-topology dep (x265.pc Libs.private:
            // `-lnuma`). Link it STATICALLY too when a libnuma.a is available
            // (libnuma-dev) so the binary gains no runtime DT_NEEDED on
            // libnuma.so.1 — otherwise we'd just relocate the "won't start on
            // a minimal image" failure from libx265.so to libnuma.so.1. Falls
            // back to a dynamic numa link (host must have libnuma1) if no .a.
            // Must come after x265 on the link line.
            if x265_static {
                if let Some(dir) = find_static_archive("numa", &[]) {
                    println!("cargo:rustc-link-search=native={}", dir.display());
                    println!("cargo:rustc-link-lib=static=numa");
                } else {
                    println!("cargo:rustc-link-lib=numa");
                    println!("cargo:warning=libffmpeg-video-sys: libx265 is statically linked but no libnuma.a was found — linking libnuma DYNAMICALLY. The binary will require libnuma.so.1 at runtime (install libnuma1 on the target, or libnuma-dev at build time for a fully self-contained static link).");
                }
            }
        } else if target_os == "macos" {
            println!("cargo:rustc-link-lib=c++");
        }
    }

    // Platform-specific system libs that FFmpeg requires
    match target_os.as_str() {
        "linux" => {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=pthread");
            // NVENC + NVDEC both load `libnvcuvid.so.1` / `libnvidia-encode.so.1`
            // dynamically through `dlopen` at runtime, so `-ldl` is required
            // whenever either feature is on. A STATIC libx264.a / libx265.a
            // also pulls `-ldl` from its Libs.private — so emit it for those
            // too, decoupling the GPL-encoder static link from the NVENC
            // feature. Harmless no-op on glibc ≥ 2.34 (dl merged into libc),
            // correct on older toolchains.
            if cfg!(feature = "video-encoder-nvenc")
                || cfg!(feature = "video-decoder-nvdec")
                || gpl.x264_static_dir.is_some()
                || gpl.x265_static_dir.is_some()
            {
                println!("cargo:rustc-link-lib=dl");
            }
            // VAAPI: libva (the dispatch core) + libva-drm (render-node
            // backend FFmpeg uses to open `/dev/dri/renderD*`). FFmpeg
            // static archives reference these symbols; final-binary link
            // resolves them from the system shared libraries.
            if cfg!(feature = "video-encoder-vaapi") || cfg!(feature = "video-decoder-vaapi") {
                println!("cargo:rustc-link-lib=va");
                println!("cargo:rustc-link-lib=va-drm");
            }
            // RKMPP: librockchip_mpp. The Rockchip BSP ships only a shared
            // object, so this is a DYNAMIC link — the resulting binary
            // carries a runtime DT_NEEDED on librockchip_mpp.so (>= 1.3.8),
            // which is board/BSP-version-specific and cannot be statically
            // baked. ARM Rockchip only.
            if cfg!(feature = "video-encoder-rkmpp") {
                println!("cargo:rustc-link-lib=rockchip_mpp");
            }
            // libdrm: linked because both the VAAPI DMA-BUF export path
            // (hwcontext_vaapi.c's `<drm_fourcc.h>` constants) and the
            // RKMPP encoder (DRM/DMA-BUF-backed MPP buffers) reference its
            // symbols, which FFmpeg does not statically vendor. Emit it
            // once when any of those features is on.
            if cfg!(feature = "video-encoder-vaapi")
                || cfg!(feature = "video-decoder-vaapi")
                || cfg!(feature = "video-encoder-rkmpp")
            {
                println!("cargo:rustc-link-lib=drm");
            }
        }
        "macos" => {
            println!("cargo:rustc-link-lib=m");
            println!("cargo:rustc-link-lib=pthread");
        }
        _ => {}
    }
}

/// True if `prog` resolves on PATH.
fn which(prog: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(prog);
        candidate.is_file() && {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&candidate)
                    .map(|m| m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            }
            #[cfg(not(unix))]
            {
                true
            }
        }
    })
}
