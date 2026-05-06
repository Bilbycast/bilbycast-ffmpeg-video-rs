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
    ProbeChroma, ProbeError,
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
