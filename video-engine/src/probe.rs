// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Lightweight runtime probes for FFmpeg encoder / decoder availability.
//!
//! Calls `avcodec_find_encoder_by_name` / `avcodec_find_decoder_by_name`
//! with the codec's FFmpeg name (e.g. `"h264_nvenc"`, `"h264_qsv"`,
//! `"h264_videotoolbox"`, `"h264_amf"`). A non-NULL pointer means the
//! vendored FFmpeg has the codec compiled in. It does **not** mean a
//! session can actually be opened — driver / hardware / runtime
//! dependencies are still resolved at `avcodec_open2` time. Treat the
//! result as "present in the build" rather than "guaranteed to work".

use libffmpeg_video_sys::{avcodec_find_decoder_by_name, avcodec_find_encoder_by_name};

/// Returns `true` if the given encoder name is compiled into the vendored
/// FFmpeg build. Names follow FFmpeg's naming convention — see
/// <https://ffmpeg.org/ffmpeg-codecs.html>.
pub fn is_encoder_available(name: &str) -> bool {
    let Ok(cstr) = std::ffi::CString::new(name) else {
        return false;
    };
    unsafe { !avcodec_find_encoder_by_name(cstr.as_ptr()).is_null() }
}

/// Returns `true` if the given decoder name is compiled into the vendored
/// FFmpeg build.
pub fn is_decoder_available(name: &str) -> bool {
    let Ok(cstr) = std::ffi::CString::new(name) else {
        return false;
    };
    unsafe { !avcodec_find_decoder_by_name(cstr.as_ptr()).is_null() }
}
