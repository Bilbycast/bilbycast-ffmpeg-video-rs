// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! Can `scale_raw_planes_into_packed` blit into a **sub-rect** of a larger
//! canvas?
//!
//! The whole distributed-multiviewer compositor design rests on one claim:
//! that you can hand this function a canvas-pitched sub-slice at a byte offset
//! and it will scale a tile into that rectangle, leaving the rest of the canvas
//! untouched — no per-tile intermediate buffer, no manual row copying.
//!
//! That claim was written in a design document, not measured. This file
//! measures it, because the cost of it being wrong is an architecture.
//!
//! **Answer: yes, and the measurement corrected the design twice.**
//!
//! 1. The destination must be **packed BGRA8** — every planar format is
//!    refused — so the canvas is 4 bytes/pixel, not YUV420's 1.5. A stream head
//!    must therefore convert BGRA to YUV before encoding, and the canvas costs
//!    2.7x the memory the design assumed. Upside: BGRA has no chroma
//!    sub-sampling, so tile rects need no even alignment at all.
//! 2. The bounds check demanded `dst_pitch * dst_height`, which the remaining
//!    tail of a **bottom-row** tile does not satisfy — short by exactly
//!    `x0 * 4` bytes — even though the write was entirely in bounds. That
//!    refused the bottom row of every wall, and made a mosaic impossible on the
//!    panel path, where `KmsDisplay::back_buffer()` maps exactly
//!    `pitch * height`. Guard bytes showed the true requirement is
//!    `(h-1)*pitch + w*4`; the check now uses it.

use video_codec::ScalerDstFormat;
use video_engine::VideoScaler;

const AV_PIX_FMT_YUV420P: i32 = 0;

/// A synthetic YUV420p source of a single flat luma value.
fn flat_source(w: usize, h: usize, luma: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    (
        vec![luma; w * h],
        vec![128u8; (w / 2) * (h / 2)],
        vec![128u8; (w / 2) * (h / 2)],
    )
}

fn scaler(src: (u32, u32), dst: (u32, u32)) -> VideoScaler {
    VideoScaler::new_with_dst_format(
        src.0,
        src.1,
        AV_PIX_FMT_YUV420P,
        dst.0,
        dst.1,
        ScalerDstFormat::Bgra8,
    )
    .expect("scaler")
}

/// The load-bearing case: two tiles into two rects of one canvas.
#[test]
fn two_tiles_land_in_their_own_rects_and_touch_nothing_else() {
    // A deliberately small canvas so the assertions can be exhaustive.
    const CW: usize = 64;
    const CH: usize = 32;
    const PITCH: usize = CW * 4;

    // Sentinel fill — anything the blit does not write must survive as 0xAA.
    let mut canvas = vec![0xAAu8; PITCH * CH];

    // Tile A: 32x16 source scaled to 16x8, placed at (0, 0).
    // Tile B: 32x16 source scaled to 16x8, placed at (32, 16).
    let tile = scaler((32, 16), (16, 8));
    let (ya, ua, va) = flat_source(32, 16, 235); // near-white
    let (yb, ub, vb) = flat_source(32, 16, 16); // near-black

    let offset_a = 0usize;
    tile.scale_raw_planes_into_packed(
        32, 16, AV_PIX_FMT_YUV420P, &ya, 32, &ua, 16, &va, 16,
        &mut canvas[offset_a..], PITCH,
    )
    .expect("tile A blit");

    let offset_b = 16 * PITCH + 32 * 4;
    tile.scale_raw_planes_into_packed(
        32, 16, AV_PIX_FMT_YUV420P, &yb, 32, &ub, 16, &vb, 16,
        &mut canvas[offset_b..], PITCH,
    )
    .expect("tile B blit");

    // Classify every pixel by which rect it falls in.
    let px = |x: usize, y: usize| -> [u8; 4] {
        let at = y * PITCH + x * 4;
        [canvas[at], canvas[at + 1], canvas[at + 2], canvas[at + 3]]
    };
    let in_a = |x: usize, y: usize| x < 16 && y < 8;
    let in_b = |x: usize, y: usize| (32..48).contains(&x) && (16..24).contains(&y);

    let mut untouched = 0usize;
    for y in 0..CH {
        for x in 0..CW {
            let p = px(x, y);
            if in_a(x, y) {
                assert!(p[0] > 200 && p[1] > 200 && p[2] > 200, "tile A pixel ({x},{y}) = {p:?}");
            } else if in_b(x, y) {
                assert!(p[0] < 60 && p[1] < 60 && p[2] < 60, "tile B pixel ({x},{y}) = {p:?}");
            } else {
                assert_eq!(
                    p,
                    [0xAA, 0xAA, 0xAA, 0xAA],
                    "pixel ({x},{y}) outside both rects was overwritten — the blit is not \
                     rect-confined and the compositor design does not work"
                );
                untouched += 1;
            }
        }
    }
    assert_eq!(
        untouched,
        CW * CH - (16 * 8) - (16 * 8),
        "the sentinel accounting is wrong, so this test proves nothing"
    );
}

/// An x offset that is **odd in pixels** is fine, because the canvas is packed
/// BGRA rather than sub-sampled YUV.
///
/// Worth pinning: had the destination been YUV420, an odd x or y offset would
/// not be representable and every tile rect would have to be even-aligned. It
/// is not, so tile rects are unconstrained — which is a real freedom for the
/// layout editor.
#[test]
fn an_odd_pixel_offset_is_legal_because_the_canvas_is_packed() {
    const CW: usize = 48;
    const CH: usize = 24;
    const PITCH: usize = CW * 4;
    let mut canvas = vec![0u8; PITCH * CH];

    let tile = scaler((16, 16), (8, 8));
    let (y, u, v) = flat_source(16, 16, 235);

    // (3, 5) — odd in both axes.
    let offset = 5 * PITCH + 3 * 4;
    tile.scale_raw_planes_into_packed(
        16, 16, AV_PIX_FMT_YUV420P, &y, 16, &u, 8, &v, 8,
        &mut canvas[offset..], PITCH,
    )
    .expect("odd-offset blit must be accepted");

    let at = |x: usize, yy: usize| canvas[yy * PITCH + x * 4];
    assert!(at(3, 5) > 200, "top-left of the odd-offset rect was not written");
    assert!(at(10, 12) > 200, "bottom-right of the odd-offset rect was not written");
    assert_eq!(at(2, 5), 0, "the column left of the rect was overwritten");
    assert_eq!(at(3, 4), 0, "the row above the rect was overwritten");
}

/// **The constraint the design document missed — and the fix.**
///
/// The check used to demand `dst.len() >= dst_pitch * dst_height`. For a tile
/// on the bottom row of the canvas the remaining tail is exactly `x0 * 4` bytes
/// short of that, because the buffer stops at the canvas's last pixel instead of
/// running on for a whole extra pitch. So the call was refused even though the
/// write would have been entirely in bounds — failing on the last row of tiles
/// and nowhere else, which is the kind of defect that survives a demo.
///
/// It also made a mosaic impossible on the panel path outright:
/// `KmsDisplay::back_buffer()` maps exactly `pitch * height`, so no bottom-row
/// tile could be blitted straight into the scanout buffer.
///
/// The requirement is now the true one, `(h-1)*pitch + w*4`, measured with
/// guard bytes rather than assumed. This test pins that a bottom-row tile is
/// accepted on an exactly-sized canvas and that its pixels land.
#[test]
fn a_bottom_row_tile_fits_an_exactly_sized_canvas() {
    const CW: usize = 64;
    const CH: usize = 32;
    const PITCH: usize = CW * 4;

    let tile = scaler((32, 16), (16, 8));
    let (y, u, v) = flat_source(32, 16, 200);

    // Bottom-right tile: rect (48, 24) size 16x8 — its last row is the
    // canvas's last row, and its x origin is non-zero. This is the case the
    // old check refused.
    let offset = 24 * PITCH + 48 * 4;
    let mut canvas = vec![0u8; PITCH * CH];
    let tail = canvas.len() - offset;
    assert!(
        tail < PITCH * 8,
        "premise: the tail ({tail}) is shorter than the old requirement ({})",
        PITCH * 8
    );

    tile.scale_raw_planes_into_packed(
        32, 16, AV_PIX_FMT_YUV420P, &y, 32, &u, 16, &v, 16,
        &mut canvas[offset..], PITCH,
    )
    .expect("a bottom-row tile must fit an exactly-sized canvas");

    assert!(canvas[24 * PITCH + 48 * 4] > 150, "the tile's top-left was not written");
    assert!(
        canvas[31 * PITCH + 63 * 4] > 150,
        "the bottom-right-most pixel of the canvas was not written"
    );
}

/// The relaxed check is still a bounds check.
///
/// Relaxing an over-strict guard is only safe if it still rejects the writes it
/// exists to stop. A buffer one byte short of the true requirement must be
/// refused, and a pitch narrower than a single row must be refused.
#[test]
fn a_genuinely_short_buffer_is_still_refused() {
    let tile = scaler((32, 16), (16, 8));
    let (y, u, v) = flat_source(32, 16, 200);
    const PITCH: usize = 64 * 4;

    // True requirement for a 16x8 tile at this pitch: 7*PITCH + 16*4.
    let needed = 7 * PITCH + 16 * 4;

    let mut exact = vec![0u8; needed];
    tile.scale_raw_planes_into_packed(
        32, 16, AV_PIX_FMT_YUV420P, &y, 32, &u, 16, &v, 16, &mut exact, PITCH,
    )
    .expect("exactly the required size must be accepted");

    let mut one_short = vec![0u8; needed - 1];
    assert!(
        tile.scale_raw_planes_into_packed(
            32, 16, AV_PIX_FMT_YUV420P, &y, 32, &u, 16, &v, 16, &mut one_short, PITCH,
        )
        .is_err(),
        "one byte short of the requirement must still be refused"
    );

    // A pitch narrower than one row would make every row overlap the next.
    let mut wide_enough = vec![0u8; needed];
    assert!(
        tile.scale_raw_planes_into_packed(
            32, 16, AV_PIX_FMT_YUV420P, &y, 32, &u, 16, &v, 16, &mut wide_enough, 16 * 4 - 4,
        )
        .is_err(),
        "a pitch narrower than one destination row must be refused"
    );
}

/// The destination format is not negotiable: planar targets are refused.
///
/// This is what makes the canvas BGRA (4 bytes/pixel) rather than YUV420
/// (1.5), which is a 2.7x memory and bandwidth difference on every canvas
/// frame, and means a stream head must convert BGRA->YUV before encoding.
#[test]
fn a_planar_destination_is_refused() {
    let planar = VideoScaler::new_with_dst_format(
        32, 16, AV_PIX_FMT_YUV420P, 16, 8, ScalerDstFormat::Yuv420p8,
    )
    .expect("scaler");
    let (y, u, v) = flat_source(32, 16, 128);
    let mut dst = vec![0u8; 16 * 8 * 4];
    let err = planar.scale_raw_planes_into_packed(
        32, 16, AV_PIX_FMT_YUV420P, &y, 32, &u, 16, &v, 16, &mut dst, 16 * 4,
    );
    assert!(err.is_err(), "a planar destination must be refused by the packed entry point");
}
