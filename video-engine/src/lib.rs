// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Safe Rust wrappers around FFmpeg for video operations.
//!
//! This crate provides three main types:
//!
//! - [`VideoDecoder`] — Decode H.264 or HEVC NAL units into raw video frames
//! - [`VideoScaler`] — Scale and convert pixel formats using libswscale
//! - [`JpegEncoder`] — Encode raw frames as MJPEG
//!
//! And one high-level function:
//!
//! - [`decode_thumbnail`] — End-to-end: NAL units in, JPEG bytes out
//!
//! All FFI calls are encapsulated behind safe Rust APIs. The types are `Send`
//! but not `Sync` (same pattern as `AacDecoder` in bilbycast-fdk-aac-rs).

pub mod audio_decoder;
pub mod audio_encoder;
pub mod decoder;
pub mod encoder;
pub mod probe;
pub mod scaler;
pub mod thumbnail;
pub mod vaapi;
pub mod video_encoder;

pub use audio_decoder::{AudioDecoder, DecodedAudioFrame};
pub use audio_encoder::AudioEncoder;
pub use decoder::{DecodedFrame, DecoderBackend, VideoDecoder};
pub use vaapi::{
    allocate_hw_frames_ctx, map_vaapi_to_drm_prime, DrmPrimeFrame, DrmPrimeKeepalive,
    DrmPrimePlane, VaapiDevice,
};
pub use video_codec::{ScalerDstFormat, VideoCodec};
pub use encoder::JpegEncoder;
pub use probe::{
    count_max_decoder_sessions, count_max_encoder_sessions, count_max_vaapi_encoder_sessions,
    is_decoder_available, is_encoder_available, probe_open_decoder, probe_open_encoder,
    probe_open_encoder_chroma, probe_open_vaapi_encoder, probe_open_vaapi_encoder_chroma,
    ProbeChroma, ProbeError, PROBE_HEIGHT, PROBE_HEIGHT_1080P, PROBE_HEIGHT_4K, PROBE_WIDTH,
    PROBE_WIDTH_1080P, PROBE_WIDTH_4K,
};
pub use scaler::{av_pix_fmt_for_yuv, ScaledFrame, VideoScaler};
pub use thumbnail::{decode_thumbnail, decode_thumbnail_packets};
pub use video_encoder::VideoEncoder;

/// Silence FFmpeg's internal logging. Call once at startup.
pub fn silence_ffmpeg_logs() {
    unsafe {
        libffmpeg_video_sys::av_log_set_level(libffmpeg_video_sys::AV_LOG_QUIET as i32);
    }
}

/// `true` when the raw FFmpeg `AVPixelFormat` integer (as exposed by
/// [`DecodedFrame::pixel_format`]) is one of the planar YUV layouts
/// drainable through [`DecodedFrame::yuv_planes`]. Companion to
/// [`DecodedFrame::is_planar_yuv`] for callers that don't have a live
/// frame yet — the lazy-open path of bilbycast-edge's
/// `ScaledVideoEncoder` queries this from inside `try_build_scaler` to
/// decide whether to wire a libswscale conversion stage between a
/// semi-planar / hardware source and the SW encoder's planar input.
pub fn is_planar_yuv_av_pix_fmt(av_pix_fmt: i32) -> bool {
    use libffmpeg_video_sys::*;
    av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV420P
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUVJ420P
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV420P10LE
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV420P12LE
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV422P
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUVJ422P
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV422P10LE
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV422P12LE
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV444P
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUVJ444P
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV444P10LE
        || av_pix_fmt == AVPixelFormat_AV_PIX_FMT_YUV444P12LE
}

/// The `(chroma, bit_depth)` a planar YUV `AVPixelFormat` actually carries,
/// or `None` for non-planar / hardware / unsupported formats.
///
/// The inverse of [`av_pix_fmt_for_yuv`], and the piece
/// [`is_planar_yuv_av_pix_fmt`] cannot express: knowing a source is *some*
/// planar YUV says nothing about whether its plane layout matches the
/// encoder's. A 4:2:2 source fed to a 4:2:0 encoder has the right number of
/// planes and the wrong number of chroma rows, so the encoder reads chroma
/// from the wrong lines — perfect luma, ghosted colour. Callers need this to
/// decide whether a libswscale conversion is genuinely unnecessary.
///
/// The full-range `YUVJ*` variants report the same layout as their limited-
/// range counterparts: they differ in colour range, not in plane geometry, and
/// the encoder's `color_range` carries that separately.
pub fn planar_yuv_layout(av_pix_fmt: i32) -> Option<(video_codec::VideoChroma, u8)> {
    use libffmpeg_video_sys::*;
    use video_codec::VideoChroma;

    let f = av_pix_fmt;
    if f == AVPixelFormat_AV_PIX_FMT_YUV420P || f == AVPixelFormat_AV_PIX_FMT_YUVJ420P {
        Some((VideoChroma::Yuv420, 8))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV420P10LE {
        Some((VideoChroma::Yuv420, 10))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV420P12LE {
        Some((VideoChroma::Yuv420, 12))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV422P || f == AVPixelFormat_AV_PIX_FMT_YUVJ422P {
        Some((VideoChroma::Yuv422, 8))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV422P10LE {
        Some((VideoChroma::Yuv422, 10))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV422P12LE {
        Some((VideoChroma::Yuv422, 12))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV444P || f == AVPixelFormat_AV_PIX_FMT_YUVJ444P {
        Some((VideoChroma::Yuv444, 8))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV444P10LE {
        Some((VideoChroma::Yuv444, 10))
    } else if f == AVPixelFormat_AV_PIX_FMT_YUV444P12LE {
        Some((VideoChroma::Yuv444, 12))
    } else {
        None
    }
}

#[cfg(test)]
mod planar_layout_tests {
    use super::{is_planar_yuv_av_pix_fmt, planar_yuv_layout};
    use video_codec::VideoChroma;

    /// Every format `is_planar_yuv_av_pix_fmt` accepts must have a known
    /// layout, otherwise a caller that trusts the former gets `None` from the
    /// latter and has no way to decide about conversion.
    #[test]
    fn layout_is_known_for_every_planar_format() {
        use libffmpeg_video_sys::*;
        for f in [
            AVPixelFormat_AV_PIX_FMT_YUV420P,
            AVPixelFormat_AV_PIX_FMT_YUVJ420P,
            AVPixelFormat_AV_PIX_FMT_YUV420P10LE,
            AVPixelFormat_AV_PIX_FMT_YUV420P12LE,
            AVPixelFormat_AV_PIX_FMT_YUV422P,
            AVPixelFormat_AV_PIX_FMT_YUVJ422P,
            AVPixelFormat_AV_PIX_FMT_YUV422P10LE,
            AVPixelFormat_AV_PIX_FMT_YUV422P12LE,
            AVPixelFormat_AV_PIX_FMT_YUV444P,
            AVPixelFormat_AV_PIX_FMT_YUVJ444P,
            AVPixelFormat_AV_PIX_FMT_YUV444P10LE,
            AVPixelFormat_AV_PIX_FMT_YUV444P12LE,
        ] {
            assert!(is_planar_yuv_av_pix_fmt(f), "fixture must be planar: {f}");
            assert!(planar_yuv_layout(f).is_some(), "layout unknown for {f}");
        }
    }

    /// Full-range J variants differ in colour range, not plane geometry.
    #[test]
    fn j_variants_report_the_same_geometry() {
        use libffmpeg_video_sys::*;
        assert_eq!(
            planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_YUVJ420P),
            planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_YUV420P),
        );
        assert_eq!(
            planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_YUVJ422P),
            planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_YUV422P),
        );
    }

    /// Round-trips against `av_pix_fmt_for_yuv` for the pairs it supports.
    #[test]
    fn inverts_av_pix_fmt_for_yuv() {
        for (chroma, depth) in [
            (VideoChroma::Yuv420, 8),
            (VideoChroma::Yuv420, 10),
            (VideoChroma::Yuv422, 8),
            (VideoChroma::Yuv422, 10),
        ] {
            let f = super::av_pix_fmt_for_yuv(chroma, depth).expect("supported pair");
            assert_eq!(planar_yuv_layout(f), Some((chroma, depth)));
        }
    }

    /// 4:2:2 and 4:2:0 must never compare equal — the whole point.
    #[test]
    fn distinguishes_422_from_420() {
        use libffmpeg_video_sys::*;
        assert_ne!(
            planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_YUV422P),
            planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_YUV420P),
        );
    }

    #[test]
    fn non_planar_has_no_layout() {
        use libffmpeg_video_sys::*;
        assert_eq!(planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_NV12), None);
        assert_eq!(planar_yuv_layout(AVPixelFormat_AV_PIX_FMT_BGRA), None);
    }
}
