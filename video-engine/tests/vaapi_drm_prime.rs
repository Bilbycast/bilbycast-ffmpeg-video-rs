//! VAAPI → DRM PRIME mapping integration test.
//!
//! Opens an `hevc_vaapi` decoder, decodes one IDR from a fixture
//! Annex-B HEVC bitstream, and verifies `DecodedFrame::map_drm_prime`
//! returns a non-zero DMA-BUF fd plus a valid descriptor.
//!
//! **Skip semantics** — designed so the CI matrix stays green on hosts
//! without a VAAPI device:
//!
//! * `video-decoder-vaapi` Cargo feature off → test compiled away.
//! * `$VAAPI_TEST_HEVC` env-var unset → skip with message ("set
//!   VAAPI_TEST_HEVC=/path/to/fixture.hevc to enable").
//! * `VaapiDevice::open` returns `HwDeviceCreate(_)` → skip with
//!   message ("VAAPI device unavailable: ...").
//! * `avcodec_open2` returns "VAAPI not advertised" → skip ("vendored
//!   FFmpeg lacks hevc_vaapi for this profile").
//!
//! When the test does run, capture a fixture by:
//!
//! ```bash
//! ffmpeg -y -i source.ts -c:v copy -bsf:v hevc_mp4toannexb \
//!     -map 0:v -t 0.04 /tmp/idr.hevc
//! VAAPI_TEST_HEVC=/tmp/idr.hevc cargo test \
//!     --features video-decoder-vaapi \
//!     --test vaapi_drm_prime -- --nocapture
//! ```

#![cfg(feature = "video-decoder-vaapi")]

use std::path::PathBuf;

use video_codec::{VideoCodec, VideoError};
use video_engine::{DecoderBackend, VaapiDevice, VideoDecoder};

#[test]
fn vaapi_decoder_exports_drm_prime_for_first_idr() {
    // Leave FFmpeg logs at default (INFO) so a host-driver mismatch
    // surfaces a useful diagnostic on a failing test rather than a
    // bare assertion.

    // Skip-with-message gate 1: fixture path.
    let Some(path_os) = std::env::var_os("VAAPI_TEST_HEVC") else {
        eprintln!(
            "skipping vaapi_drm_prime: set VAAPI_TEST_HEVC=/path/to/fixture.hevc \
             to enable. The fixture must be an Annex-B HEVC bitstream containing \
             at least one IDR; capture one with `ffmpeg -bsf:v hevc_mp4toannexb`."
        );
        return;
    };
    let path = PathBuf::from(&path_os);
    let bitstream = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            panic!("read VAAPI_TEST_HEVC ({}): {e}", path.display());
        }
    };
    if bitstream.is_empty() {
        panic!("VAAPI_TEST_HEVC fixture is empty: {}", path.display());
    }
    eprintln!("vaapi_drm_prime: fixture bytes = {}", bitstream.len());

    // Skip-with-message gate 2: VAAPI device open. CI without an iGPU /
    // dGPU surfaces this — `/dev/dri/renderD128` doesn't exist or the
    // running user has no `render`/`video` group membership. We don't
    // call `VideoDecoder::open_with_backend` directly because it bundles
    // device + codec open into one step and we want the diagnostic to
    // distinguish "no VAAPI hardware" from "VAAPI hardware but codec
    // open refused".
    match VaapiDevice::open(None) {
        Ok(_) => {}
        Err(VideoError::HwDeviceCreate(code)) => {
            eprintln!(
                "skipping vaapi_drm_prime: VaapiDevice::open failed (FFmpeg error {code}). \
                 Common causes: no VAAPI driver installed, no render node, or \
                 user not in `render` group. Add to render group: \
                 `sudo gpasswd -a $USER render && newgrp render`."
            );
            return;
        }
        Err(e) => {
            panic!("VaapiDevice::open returned unexpected error: {e:?}");
        }
    }

    // Skip-with-message gate 3: codec open. The vendored FFmpeg may
    // have `--enable-decoder=hevc_vaapi` compiled in but the runtime
    // VAAPI driver may not advertise HEVC support for the host's
    // hardware (typical: pre-Skylake Intel, very old AMD).
    let mut decoder = match VideoDecoder::open_with_backend(VideoCodec::Hevc, DecoderBackend::Vaapi) {
        Ok(d) => d,
        Err(VideoError::OpenCodec(code)) => {
            eprintln!(
                "skipping vaapi_drm_prime: hevc_vaapi avcodec_open2 returned {code}. \
                 The driver may not advertise HEVC decode for this host's hardware."
            );
            return;
        }
        Err(VideoError::CodecNotFound(_)) => {
            eprintln!(
                "skipping vaapi_drm_prime: vendored FFmpeg has no hevc_vaapi entry — \
                 build with `--features video-decoder-vaapi`."
            );
            return;
        }
        Err(e) => panic!("VideoDecoder::open_with_backend(Hevc, Vaapi): unexpected {e:?}"),
    };

    // Feed the entire fixture and pull frames until we get one. The
    // first IDR is enough — every VAAPI driver we've tested produces a
    // VAAPI surface for the very first decoded frame after `get_format`
    // returns AV_PIX_FMT_VAAPI.
    if let Err(e) = decoder.send_packet(&bitstream) {
        panic!("send_packet failed on fixture: {e}");
    }
    // Flush so frames trickle out even when the fixture has trailing
    // frames missing references.
    let _ = decoder.send_flush();

    let frame = match decoder.receive_frame() {
        Ok(f) => f,
        Err(VideoError::NeedMoreInput) => {
            panic!(
                "decoder buffered the IDR but produced no frame (fixture too short \
                 or missing IDR). Capture a longer slice (-t 0.5)."
            );
        }
        Err(e) => panic!("receive_frame: {e:?}"),
    };

    assert!(
        frame.is_vaapi(),
        "decoded frame format {} is not AV_PIX_FMT_VAAPI — `get_format` callback \
         must have demoted to SW. Check stderr for the warning.",
        frame.pixel_format()
    );
    assert!(frame.width() > 0 && frame.height() > 0);

    let prime = frame
        .map_drm_prime()
        .expect("map_drm_prime should succeed on a VAAPI frame");

    assert_eq!(prime.width, frame.width());
    assert_eq!(prime.height, frame.height());
    assert!(
        !prime.planes.is_empty(),
        "DRM PRIME descriptor has zero planes"
    );
    // Every plane must reference a non-stub DMA-BUF. -1 / 0 are sentinels
    // (-1 = "no fd", 0 = stdin, neither legitimate for a hwframe export).
    for (i, plane) in prime.planes.iter().enumerate() {
        assert!(
            plane.fd > 0,
            "plane {i} fd = {}, expected a positive DMA-BUF fd",
            plane.fd
        );
        assert!(
            plane.pitch > 0,
            "plane {i} pitch is zero — driver returned a malformed descriptor"
        );
    }
    eprintln!(
        "vaapi_drm_prime: {}x{} fourcc=0x{:08x} mod=0x{:016x} planes={}, first fd={}",
        prime.width,
        prime.height,
        prime.fourcc,
        prime.modifier,
        prime.planes.len(),
        prime.planes[0].fd,
    );
}
