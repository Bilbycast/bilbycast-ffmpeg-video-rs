//! Standalone diagnostic — runs the runtime open probe against every HW
//! encoder/decoder name the edge probes at startup and prints the outcome
//! (compiled-in or not, runtime open success/failure with the FFmpeg
//! error classification). Cheap one-shot, useful for narrowing down
//! "why doesn't the manager show NVENC/QSV?" without restarting an edge.
//!
//! Run with the same feature flags as a full edge build:
//!
//! ```bash
//! cargo run -p video-engine --example probe_diagnostics \
//!   --features video-encoder-x264,video-encoder-x265,video-encoder-nvenc,video-encoder-qsv
//! ```

use video_engine::{
    count_max_decoder_sessions, count_max_encoder_sessions, is_decoder_available,
    is_encoder_available, probe_open_decoder, probe_open_encoder,
    probe_open_encoder_chroma, ProbeChroma,
};

fn main() {
    println!("=== bilbycast-edge HW probe diagnostics ===\n");

    let encoders = [
        "libx264",
        "libx265",
        "h264_nvenc",
        "hevc_nvenc",
        "h264_qsv",
        "hevc_qsv",
        "h264_videotoolbox",
        "hevc_videotoolbox",
        "h264_amf",
        "hevc_amf",
    ];

    println!("Encoders:");
    println!("  {:<20} {:<14} {}", "name", "compiled-in", "runtime open");
    println!("  {}", "-".repeat(70));
    for name in encoders {
        let compiled = is_encoder_available(name);
        let runtime = if compiled {
            match probe_open_encoder(name) {
                Ok(()) => "OK".to_string(),
                Err(e) => format!("FAIL — {} ({})", e.as_tag(), e),
            }
        } else {
            "—".to_string()
        };
        println!("  {:<20} {:<14} {}", name, compiled, runtime);
    }

    let decoders = [
        "h264_cuvid",
        "hevc_cuvid",
        "h264_qsv",
        "hevc_qsv",
        "h264_videotoolbox",
        "hevc_videotoolbox",
    ];

    println!("\nDecoders:");
    println!("  {:<20} {:<14} {}", "name", "compiled-in", "runtime open");
    println!("  {}", "-".repeat(70));
    for name in decoders {
        let compiled = is_decoder_available(name);
        let runtime = if compiled {
            match probe_open_decoder(name) {
                Ok(()) => "OK".to_string(),
                Err(e) => format!("FAIL — {} ({})", e.as_tag(), e),
            }
        } else {
            "—".to_string()
        };
        println!("  {:<20} {:<14} {}", name, compiled, runtime);
    }

    // Concurrent-session capacity per family — the loop opens 1, 2, 3, …
    // sessions and counts the successful ones until one fails (cap 8).
    println!("\nEncoder session capacity (cap 8):");
    let session_probes = [
        ("nvenc h264", "h264_nvenc"),
        ("qsv h264", "h264_qsv"),
        ("amf h264", "h264_amf"),
    ];
    for (label, name) in session_probes {
        if !is_encoder_available(name) {
            continue;
        }
        let max = count_max_encoder_sessions(name, 8);
        println!("  {:<14} → {}", label, max);
    }

    println!("\nDecoder session capacity (cap 8):");
    let session_probes_dec = [("nvdec h264", "h264_cuvid"), ("qsv decode h264", "h264_qsv")];
    for (label, name) in session_probes_dec {
        if !is_decoder_available(name) {
            continue;
        }
        let max = count_max_decoder_sessions(name, 8);
        println!("  {:<14} → {}", label, max);
    }

    // Per-codec chroma + bit-depth matrix. Only printed for codecs the
    // baseline 4:2:0 8-bit probe accepted (otherwise the answer is
    // trivially "none of the chroma combos work").
    let chroma_axes = [
        ("4:2:0 8b", ProbeChroma::Yuv420_8bit),
        ("4:2:2 8b", ProbeChroma::Yuv422_8bit),
        ("4:2:0 10b", ProbeChroma::Yuv420_10bit),
        ("4:2:2 10b", ProbeChroma::Yuv422_10bit),
    ];
    println!("\nChroma + bit-depth matrix (✓ = avcodec_open2 succeeded):");
    println!(
        "  {:<20} {:<11} {:<11} {:<11} {:<11}",
        "codec", chroma_axes[0].0, chroma_axes[1].0, chroma_axes[2].0, chroma_axes[3].0
    );
    println!("  {}", "-".repeat(70));
    for name in encoders {
        if !is_encoder_available(name) {
            continue;
        }
        // Skip codecs whose 4:2:0 8-bit baseline failed at runtime —
        // every chroma combo will fail the same way and the row is
        // noise.
        if probe_open_encoder(name).is_err() {
            continue;
        }
        let mut cells: Vec<String> = Vec::with_capacity(4);
        for (_, axis) in chroma_axes {
            let mark = match probe_open_encoder_chroma(name, axis) {
                Ok(()) => "✓".to_string(),
                Err(_) => "—".to_string(),
            };
            cells.push(format!("{:<11}", mark));
        }
        println!(
            "  {:<20} {} {} {} {}",
            name, cells[0], cells[1], cells[2], cells[3]
        );
    }
    println!();
}
