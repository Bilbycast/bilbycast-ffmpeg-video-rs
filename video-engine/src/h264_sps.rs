// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: MPL-2.0

//! Pure-Rust parser for the parts of an H.264 sequence parameter set that
//! decide how a decoder or an encoder must be set up.
//!
//! Its first job is [`h264_declared_reorder_depth`]: whether the stream
//! **declares** its reorder depth (`max_num_reorder_frames` in the VUI
//! `bitstream_restriction`). [`crate::VideoDecoder::open_opts`] seeds
//! libavcodec's `has_b_frames` from that answer, so the parser is written to
//! agree with libavcodec's own `ff_h264_decode_seq_parameter_set` (n9.0.2,
//! `libavcodec/h264_ps.c`) rather than with the letter of the specification
//! wherever the two differ:
//!
//! - the chroma/bit-depth/scaling branch is taken for exactly libavcodec's
//!   profile list (which includes the old High 4:4:4 `144` and omits the MFC
//!   profiles `134` / `135` / `139`);
//! - every SPS libavcodec refuses (`sps_id > 31`, `chroma_format_idc > 3`,
//!   separate colour planes, unequal or out-of-range bit depths, a scaling
//!   delta outside ±128, `log2_max_frame_num` / `log2_max_poc_lsb` out of
//!   range, POC type > 2, a POC cycle of 256 or more, more than 16 reference
//!   frames, `cpb_cnt > 32`, `num_reorder_frames > 16`) parses to `None`;
//! - the bit reader stops at the `rbsp_stop_one_bit`, as libavcodec's does
//!   (`h2645_parse.c` sizes each NAL's reader up to, not including, that bit),
//!   and any read past it makes the declared reorder depth unknown. That
//!   covers libavcodec's "Truncated VUI" early return, its `!get_bits_left`
//!   return before `bitstream_restriction_flag`, and its clearing of the flag
//!   on an overread inside the restriction block.
//!
//! The asymmetry matters. Reporting a depth libavcodec did not apply
//! reproduces the join bug the seed exists to fix; reporting none when
//! libavcodec did apply one only costs the seed of 1 a single frame on an
//! IPPP stream. So every doubt resolves to `None`.
//!
//! The same parse also yields the fields an interlace / aspect-ratio test
//! wants to read back from an encoder's output — `frame_mbs_only_flag`,
//! `mb_adaptive_frame_field_flag`, the VUI sample aspect ratio, timing and
//! `pic_struct_present_flag` — so it is public.

/// The parsed subset of one H.264 SPS. Built by [`parse_h264_sps`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct H264SpsInfo {
    /// `profile_idc` (66 baseline, 77 main, 100 high, ...).
    pub profile_idc: u8,
    /// `level_idc` (e.g. 40 for level 4.0).
    pub level_idc: u8,
    /// `seq_parameter_set_id`.
    pub sps_id: u32,
    /// `chroma_format_idc` — 1 (4:2:0) when the profile does not carry it.
    pub chroma_format_idc: u32,
    /// Luma bit depth (8 when the profile does not carry it).
    pub bit_depth_luma: u32,
    /// `frame_mbs_only_flag`. `false` means the stream may code fields —
    /// PAFF, MBAFF or both.
    pub frame_mbs_only: bool,
    /// `mb_adaptive_frame_field_flag` (MBAFF). Always `false` when
    /// `frame_mbs_only` is `true`.
    pub mb_adaptive_frame_field: bool,
    /// Luma width after the SPS frame cropping (cropping libavcodec would
    /// discard as invalid is ignored, as it does).
    pub width: u32,
    /// Luma frame height (both fields) after cropping.
    pub height: u32,
    /// `vui_parameters_present_flag`.
    pub vui_present: bool,
    /// Sample aspect ratio from `aspect_ratio_idc` (Table E-1, or the
    /// extended `sar_width:sar_height`). `None` when absent, unspecified
    /// (`idc 0`), reserved, or an extended ratio with a zero term — the cases
    /// libavcodec reports as `0/1`.
    pub sample_aspect_ratio: Option<(u16, u16)>,
    /// `(num_units_in_tick, time_scale, fixed_frame_rate_flag)` when the VUI
    /// carries valid timing. Frame rate = `time_scale / (2 * num_units_in_tick)`.
    pub timing: Option<(u32, u32, bool)>,
    /// `nal_hrd_parameters_present_flag` — the stream signals NAL HRD
    /// (buffering-period / picture-timing SEI), e.g. x264 `nal-hrd=cbr`.
    pub nal_hrd_present: bool,
    /// `pic_struct_present_flag` — the picture-timing SEI carries
    /// `pic_struct` (how a decoder should display each picture's fields).
    pub pic_struct_present: bool,
    /// `max_num_reorder_frames`, present only when the VUI
    /// `bitstream_restriction_flag` is set **and** libavcodec would have
    /// applied it — see the module doc. `None` means "not declared".
    pub max_num_reorder_frames: Option<u32>,
}

/// Bit reader over an RBSP, bounded at the `rbsp_stop_one_bit`.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> BitReader<'a> {
    /// `end_bits` is the number of payload bits (everything before the stop
    /// bit).
    fn new(data: &'a [u8], end_bits: usize) -> Self {
        Self { data, pos: 0, end: end_bits }
    }

    fn bits_left(&self) -> usize {
        self.end - self.pos
    }

    fn bit(&mut self) -> Option<u32> {
        if self.pos >= self.end {
            return None;
        }
        let b = (self.data[self.pos / 8] >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(b as u32)
    }

    fn peek_bit(&self) -> Option<u32> {
        if self.pos >= self.end {
            return None;
        }
        Some(((self.data[self.pos / 8] >> (7 - (self.pos % 8))) & 1) as u32)
    }

    fn flag(&mut self) -> Option<bool> {
        self.bit().map(|b| b == 1)
    }

    /// `u(n)`, `n <= 32`.
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()? as u64;
        }
        Some(v as u32)
    }

    /// `ue(v)`, values up to `u32::MAX - 1`.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0u32;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        let rest = self.u(leading)? as u64;
        u32::try_from((1u64 << leading) - 1 + rest).ok()
    }

    /// `se(v)`.
    fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// Strip the emulation-prevention bytes (`00 00 03` → `00 00`) from a NAL
/// payload.
fn unescape_rbsp(nal_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal_payload.len());
    let mut zeros = 0usize;
    for &b in nal_payload {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Number of payload bits before the `rbsp_stop_one_bit`, or `None` when the
/// RBSP holds no set bit at all.
fn payload_bits(rbsp: &[u8]) -> Option<usize> {
    let last = rbsp.iter().rposition(|&b| b != 0)?;
    let trailing = rbsp[last].trailing_zeros() as usize;
    Some(last * 8 + (7 - trailing))
}

/// Profiles that carry `chroma_format_idc`, bit depths and scaling
/// matrices — libavcodec's list, not the specification's (see module doc).
fn has_chroma_info(profile_idc: u8) -> bool {
    matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 144)
}

/// Table E-1.
fn sar_for_idc(idc: u32) -> Option<(u16, u16)> {
    const TABLE: [(u16, u16); 17] = [
        (0, 1),
        (1, 1),
        (12, 11),
        (10, 11),
        (16, 11),
        (40, 33),
        (24, 11),
        (20, 11),
        (32, 11),
        (80, 33),
        (18, 11),
        (15, 11),
        (64, 33),
        (160, 99),
        (4, 3),
        (3, 2),
        (2, 1),
    ];
    match TABLE.get(idc as usize) {
        Some(&(0, _)) | None => None,
        Some(&sar) => Some(sar),
    }
}

/// Skip one `scaling_list()`. `None` on overread or a delta libavcodec
/// rejects.
fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Option<()> {
    let mut last: i64 = 8;
    let mut next: i64 = 8;
    for _ in 0..size {
        if next != 0 {
            let delta = r.se()?;
            if !(-128..=127).contains(&delta) {
                return None;
            }
            next = (last + delta).rem_euclid(256);
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Skip `hrd_parameters()`. `Err(Invalid)` for a `cpb_cnt` libavcodec
/// refuses, `Err(Truncated)` on an overread.
fn skip_hrd(r: &mut BitReader<'_>) -> Result<(), VuiOutcome> {
    let cpb_cnt = r.ue().ok_or(VuiOutcome::Truncated)? as u64 + 1;
    if cpb_cnt > 32 {
        return Err(VuiOutcome::Invalid);
    }
    let mut body = || -> Option<()> {
        r.u(4)?; // bit_rate_scale
        r.u(4)?; // cpb_size_scale
        for _ in 0..cpb_cnt {
            r.ue()?; // bit_rate_value_minus1
            r.ue()?; // cpb_size_value_minus1
            r.flag()?; // cbr_flag
        }
        r.u(5)?; // initial_cpb_removal_delay_length_minus1
        r.u(5)?; // cpb_removal_delay_length_minus1
        r.u(5)?; // dpb_output_delay_length_minus1
        r.u(5)?; // time_offset_length
        Some(())
    };
    body().ok_or(VuiOutcome::Truncated)
}

/// What the VUI parse reached. A failure after the aspect ratio keeps what
/// was already read; only the declared reorder depth needs the whole block.
enum VuiOutcome {
    /// The VUI parsed as far as libavcodec reads it.
    Complete,
    /// libavcodec would keep the SPS but stop reading the VUI here, leaving
    /// the reorder depth undeclared.
    Truncated,
    /// libavcodec would refuse the whole SPS.
    Invalid,
}

fn parse_vui(r: &mut BitReader<'_>, info: &mut H264SpsInfo) -> VuiOutcome {
    macro_rules! read {
        ($e:expr) => {
            match $e {
                Some(v) => v,
                None => return VuiOutcome::Truncated,
            }
        };
    }

    // ff_h2645_decode_common_vui_params
    if read!(r.flag()) {
        let idc = read!(r.u(8));
        if idc == 255 {
            let num = read!(r.u(16)) as u16;
            let den = read!(r.u(16)) as u16;
            if num != 0 && den != 0 {
                info.sample_aspect_ratio = Some((num, den));
            }
        } else {
            info.sample_aspect_ratio = sar_for_idc(idc);
        }
    }
    if read!(r.flag()) {
        read!(r.flag()); // overscan_appropriate_flag
    }
    if read!(r.flag()) {
        read!(r.u(3)); // video_format
        read!(r.flag()); // video_full_range_flag
        if read!(r.flag()) {
            read!(r.u(24)); // colour_primaries, transfer, matrix
        }
    }
    if read!(r.flag()) {
        read!(r.ue()); // chroma_sample_loc_type_top_field
        read!(r.ue()); // chroma_sample_loc_type_bottom_field
    }

    // decode_vui_parameters: libavcodec's "Truncated VUI" early return.
    if r.peek_bit() == Some(1) && r.bits_left() < 10 {
        return VuiOutcome::Truncated;
    }
    if read!(r.flag()) {
        let num_units_in_tick = read!(r.u(32));
        let time_scale = read!(r.u(32));
        let fixed = read!(r.flag());
        if num_units_in_tick != 0 && time_scale != 0 {
            info.timing = Some((num_units_in_tick, time_scale, fixed));
        }
    }
    let nal_hrd = read!(r.flag());
    info.nal_hrd_present = nal_hrd;
    if nal_hrd {
        if let Err(outcome) = skip_hrd(r) {
            return outcome;
        }
    }
    let vcl_hrd = read!(r.flag());
    if vcl_hrd {
        if let Err(outcome) = skip_hrd(r) {
            return outcome;
        }
    }
    if nal_hrd || vcl_hrd {
        read!(r.flag()); // low_delay_hrd_flag
    }
    info.pic_struct_present = read!(r.flag());

    // libavcodec returns before reading the flag when no bits are left.
    if r.bits_left() == 0 || !read!(r.flag()) {
        return VuiOutcome::Complete;
    }
    // bitstream_restriction. libavcodec reads the six ue(v) fields with
    // get_ue_golomb_31, which is exact only up to 31: a larger value is
    // non-conformant and libavcodec would misread it, so it leaves the depth
    // undeclared here. An overread anywhere in the block clears the flag in
    // libavcodec, which `read!` mirrors.
    read!(r.flag()); // motion_vectors_over_pic_boundaries_flag
    let mut fields = [0u32; 6];
    for f in fields.iter_mut() {
        *f = read!(r.ue());
    }
    // max_bytes_per_pic_denom, max_bits_per_mb_denom,
    // log2_max_mv_length_horizontal, log2_max_mv_length_vertical,
    // max_num_reorder_frames, max_dec_frame_buffering.
    if fields.iter().any(|&v| v > 31) {
        return VuiOutcome::Truncated;
    }
    let num_reorder_frames = fields[4];
    if num_reorder_frames > 16 {
        // "Clipping illegal num_reorder_frames": libavcodec fails the SPS.
        return VuiOutcome::Invalid;
    }
    info.max_num_reorder_frames = Some(num_reorder_frames);
    VuiOutcome::Complete
}

/// Parse one H.264 SPS NAL unit — the NAL header byte followed by the
/// escaped payload, as it sits between two Annex B start codes (trailing
/// zero bytes are tolerated). `None` when the NAL is not an SPS or when
/// libavcodec would refuse it.
pub fn parse_h264_sps(nal: &[u8]) -> Option<H264SpsInfo> {
    let (&header, payload) = nal.split_first()?;
    if header & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape_rbsp(payload);
    let end = payload_bits(&rbsp)?;
    let mut r = BitReader::new(&rbsp, end);

    let profile_idc = r.u(8)? as u8;
    r.u(8)?; // constraint_set0..5 + reserved_zero_2bits
    let level_idc = r.u(8)? as u8;
    let sps_id = r.ue()?;
    if sps_id > 31 {
        return None;
    }

    let mut chroma_format_idc = 1;
    let mut bit_depth_luma = 8;
    if has_chroma_info(profile_idc) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc > 3 {
            return None;
        }
        if chroma_format_idc == 3 && r.flag()? {
            // separate_colour_plane_flag: libavcodec does not support it.
            return None;
        }
        bit_depth_luma = r.ue()?.checked_add(8)?;
        let bit_depth_chroma = r.ue()?.checked_add(8)?;
        if bit_depth_luma != bit_depth_chroma || !(8..=14).contains(&bit_depth_luma) {
            return None;
        }
        r.flag()?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let log2_max_frame_num_minus4 = r.ue()?;
    if log2_max_frame_num_minus4 > 12 {
        return None;
    }
    match r.ue()? {
        0 => {
            if r.ue()? > 12 {
                return None;
            }
        }
        1 => {
            r.flag()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let cycle = r.ue()?;
            if cycle >= 256 {
                return None;
            }
            for _ in 0..cycle {
                r.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    if r.ue()? > 16 {
        // max_num_ref_frames
        return None;
    }
    r.flag()?; // gaps_in_frame_num_value_allowed_flag
    let mb_width = r.ue()?.checked_add(1)?;
    let map_units_height = r.ue()?.checked_add(1)?;
    let frame_mbs_only = r.flag()?;
    let mb_adaptive_frame_field = if frame_mbs_only { false } else { r.flag()? };
    r.flag()?; // direct_8x8_inference_flag

    let mb_height = map_units_height.checked_mul(if frame_mbs_only { 1 } else { 2 })?;
    let full_w = mb_width.checked_mul(16)?;
    let full_h = mb_height.checked_mul(16)?;
    let (mut width, mut height) = (full_w, full_h);
    if r.flag()? {
        let (left, right, top, bottom) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (sub_w, sub_h) = match chroma_format_idc {
            1 => (2u64, 2u64),
            2 => (2, 1),
            _ => (1, 1),
        };
        let step_x = sub_w;
        let step_y = sub_h * if frame_mbs_only { 1 } else { 2 };
        let crop_w = (left as u64 + right as u64) * step_x;
        let crop_h = (top as u64 + bottom as u64) * step_y;
        // libavcodec discards cropping that would leave nothing.
        if crop_w < full_w as u64 && crop_h < full_h as u64 {
            width = full_w - crop_w as u32;
            height = full_h - crop_h as u32;
        }
    }

    let mut info = H264SpsInfo {
        profile_idc,
        level_idc,
        sps_id,
        chroma_format_idc,
        bit_depth_luma,
        frame_mbs_only,
        mb_adaptive_frame_field,
        width,
        height,
        vui_present: false,
        sample_aspect_ratio: None,
        timing: None,
        nal_hrd_present: false,
        pic_struct_present: false,
        max_num_reorder_frames: None,
    };

    info.vui_present = r.flag()?;
    if info.vui_present {
        match parse_vui(&mut r, &mut info) {
            VuiOutcome::Complete | VuiOutcome::Truncated => {}
            VuiOutcome::Invalid => return None,
        }
    }
    Some(info)
}

/// Iterate the NAL units of an Annex B byte stream (`00 00 01` /
/// `00 00 00 01` start codes). Each item runs from the NAL header byte to the
/// byte before the next start code, trailing zero bytes trimmed.
pub fn annexb_nal_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out.into_iter()
}

/// The first SPS NAL unit in an Annex B access unit, parsed. `None` when the
/// data holds no SPS, or the first one does not parse.
pub fn find_h264_sps(annexb: &[u8]) -> Option<H264SpsInfo> {
    let nal = annexb_nal_units(annexb).find(|n| n[0] & 0x1F == 7)?;
    parse_h264_sps(nal)
}

/// The reorder depth an Annex B access unit's SPS declares
/// (`max_num_reorder_frames`), counted the way libavcodec will apply it.
/// `None` when there is no SPS, it does not parse, or it does not declare
/// one — the cases in which libavcodec falls back to its reorder heuristic.
pub fn h264_declared_reorder_depth(annexb: &[u8]) -> Option<u32> {
    find_h264_sps(annexb)?.max_num_reorder_frames
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    // Real SPS NAL units (header byte + escaped payload) cut from the rig's
    // broadcast captures and from libx264. The expected values beside each
    // are FFmpeg's own reading (`-bsf:v trace_headers`), not this parser's.

    /// Nine (AU DVB-T), PAFF 1080i25: declares `max_num_reorder_frames = 3`.
    pub(crate) const NINE: &str = "67640028ac72140780447de02d3c000003000400000300ca72c70c2160";
    /// Seven (AU DVB-T), MBAFF 1080i25: declares 3.
    const SEVEN: &str = "67640028ac7214078044fde02d3c000003000400000300ca72c70c2160";
    /// sync-test 1080p25 Main profile: declares 2.
    const SYNCTEST: &str = "674d4028eca03c0113f2e022000003000200000300641e30632c";
    /// BTS204, MBAFF 1080i29.97 with NAL HRD: declares 2.
    const BTS204: &str =
        "67640028acd10078044fde036a020202800001f4800075307140000da048001541faf7f072a1629920";
    /// Sky Sports Arena 1080i25 (PAFF, scaling matrices, NAL + VCL HRD): no
    /// `bitstream_restriction` — the stream whose joins showed a GOP of
    /// garbage.
    pub(crate) const SKY_SPORTS: &str = "27640028ad00ec0780447de028404040500000030010000003032e48000f424001e84e17b83240007a12000f4250bdc0a0";
    /// Sky Witness 1080i25 (MBAFF, scaling matrices): no restriction.
    const SKY_WITNESS: &str = "67640028ad843588742050b041074421ac47361882a210ffa69a4d5a31526c862c92c8a431cb104111251c28c716161d1e09050b05d06d087fd7af93f11fd7e6f8af0870d608a0a0b290078044fde036a020202800000300080000030194a0";
    /// ABC (AU DVB-T) 1080i25 MBAFF, scaling matrices: no restriction.
    const ABC: &str = "67640028ad843588742050b041075086b10e840a160820ea10d621d08142c1041d421ac47361882a843588e6c31055086b11cd8620aa10ffa69a4d5a31526c862c92c8a431cb104111251c28c716161d1e09050b05d06d087fd7af93f11fd7e6f8af0870d608a0a0b294078044fde0342040404800000300080000030194a0";
    /// libx264 `-tune zerolatency`, High 4:4:4 Predictive (chroma 3),
    /// 720x576, extended SAR 64:45: declares 0.
    const X264_ZL_444_SAR6445: &str =
        "67f4001e9196402d049bff0040002d1000000300100000030328f162e480";
    /// libx264 `bframes=2`, High 4:4:4, 320x240: declares 1.
    const X264_B2_444: &str = "67f4000d919a20283f6022000003000200000300641e285224";
    /// libx264 `-tune zerolatency`, High 4:2:0, 704x576, POC type 2, table
    /// SAR 16:11 (`aspect_ratio_idc` 4): declares 0.
    const X264_ZL_420_SAR1611: &str = "6764001eacb2016024d820800000030080000019478b1724";

    fn parse(s: &str) -> H264SpsInfo {
        parse_h264_sps(&hex(s)).expect("fixture SPS must parse")
    }

    #[test]
    fn declared_reorder_depth_matches_ffmpeg_on_real_streams() {
        for (name, sps, want) in [
            ("Nine", NINE, Some(3)),
            ("Seven", SEVEN, Some(3)),
            ("sync-test", SYNCTEST, Some(2)),
            ("BTS204", BTS204, Some(2)),
            ("Sky Sports", SKY_SPORTS, None),
            ("Sky Witness", SKY_WITNESS, None),
            ("ABC", ABC, None),
            ("x264 zerolatency 4:4:4", X264_ZL_444_SAR6445, Some(0)),
            ("x264 bframes=2", X264_B2_444, Some(1)),
            ("x264 zerolatency 4:2:0", X264_ZL_420_SAR1611, Some(0)),
        ] {
            assert_eq!(parse(sps).max_num_reorder_frames, want, "{name}");
        }
    }

    #[test]
    fn interlace_and_geometry_fields_match_ffmpeg() {
        let nine = parse(NINE);
        assert_eq!((nine.profile_idc, nine.chroma_format_idc), (100, 1));
        assert!(!nine.frame_mbs_only, "Nine is field-coded");
        assert!(!nine.mb_adaptive_frame_field, "Nine is PAFF, not MBAFF");
        assert_eq!((nine.width, nine.height), (1920, 1080));
        assert_eq!(nine.timing, Some((1, 50, true)));
        assert!(nine.pic_struct_present);

        let seven = parse(SEVEN);
        assert!(!seven.frame_mbs_only && seven.mb_adaptive_frame_field);

        let sky = parse(SKY_SPORTS);
        assert!(!sky.frame_mbs_only && !sky.mb_adaptive_frame_field);
        assert!(sky.nal_hrd_present, "Sky Sports carries NAL HRD");
        assert_eq!((sky.width, sky.height), (1920, 1080));

        let witness = parse(SKY_WITNESS);
        assert!(!witness.frame_mbs_only && witness.mb_adaptive_frame_field);

        let bts = parse(BTS204);
        assert_eq!(bts.timing, Some((1001, 60000, true)));
        assert!(bts.nal_hrd_present);

        let sync = parse(SYNCTEST);
        assert_eq!(sync.profile_idc, 77);
        assert!(sync.frame_mbs_only && !sync.mb_adaptive_frame_field);
        assert!(!sync.pic_struct_present);
        assert!(!sync.nal_hrd_present);
        assert_eq!((sync.width, sync.height), (1920, 1080));
    }

    #[test]
    fn sample_aspect_ratio_table_and_extended() {
        // aspect_ratio_idc 1 on every broadcast fixture.
        for s in [NINE, SEVEN, SKY_SPORTS, SKY_WITNESS, ABC, BTS204, SYNCTEST] {
            assert_eq!(parse(s).sample_aspect_ratio, Some((1, 1)));
        }
        // idc 255 with an explicit 64:45, and the 4:4:4 branch that reads
        // separate_colour_plane_flag.
        let x = parse(X264_ZL_444_SAR6445);
        assert_eq!(x.sample_aspect_ratio, Some((64, 45)));
        assert_eq!((x.chroma_format_idc, x.width, x.height), (3, 720, 576));
        // Table E-1 idc 4 = 16:11.
        let t = parse(X264_ZL_420_SAR1611);
        assert_eq!(t.sample_aspect_ratio, Some((16, 11)));
        assert_eq!((t.width, t.height), (704, 576));
    }

    #[test]
    fn every_truncation_of_a_declaring_sps_is_undeclared() {
        // A depth the reader did not fully read must never be reported: a
        // false `Some(0)` reproduces the very join bug the seed fixes, while
        // `None` only costs one frame. Cutting any whole byte off the end
        // moves the stop bit into real data, so the restriction block can no
        // longer be read to its end.
        for s in [NINE, SEVEN, SYNCTEST, BTS204, X264_ZL_444_SAR6445, X264_B2_444, X264_ZL_420_SAR1611] {
            let full = hex(s);
            for cut in 1..full.len() {
                let nal = &full[..full.len() - cut];
                assert_eq!(
                    parse_h264_sps(nal).and_then(|i| i.max_num_reorder_frames),
                    None,
                    "{s} cut by {cut} bytes"
                );
            }
        }
    }

    #[test]
    fn arbitrary_bit_flips_never_panic() {
        for s in [NINE, SKY_SPORTS, SKY_WITNESS, BTS204, X264_ZL_444_SAR6445] {
            let full = hex(s);
            for byte in 1..full.len() {
                for bit in 0..8 {
                    let mut m = full.clone();
                    m[byte] ^= 1 << bit;
                    let _ = parse_h264_sps(&m);
                }
            }
        }
        let _ = parse_h264_sps(&[]);
        let _ = parse_h264_sps(&[0x67]);
        let _ = parse_h264_sps(&[0x67, 0, 0, 0]);
        let _ = parse_h264_sps(&[0x67; 64]);
        let _ = parse_h264_sps(&[0x67, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    }

    #[test]
    fn only_sps_nal_units_parse() {
        let mut pps = hex(NINE);
        pps[0] = 0x68; // same bytes, PPS NAL type
        assert_eq!(parse_h264_sps(&pps), None);
    }

    fn annexb(nals: &[&[u8]], four_byte: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for n in nals {
            if four_byte {
                out.push(0);
            }
            out.extend_from_slice(&[0, 0, 1]);
            out.extend_from_slice(n);
        }
        out
    }

    #[test]
    fn finds_the_sps_inside_an_access_unit() {
        let aud: &[u8] = &[0x09, 0xf0];
        let sei: &[u8] = &[0x06, 0x05, 0x01, 0x00, 0x80];
        let sps = hex(NINE);
        let pps: &[u8] = &[0x68, 0xee, 0x3c, 0x80];
        let slice: &[u8] = &[0x65, 0x88, 0x84, 0x00, 0x33];
        for four in [false, true] {
            let au = annexb(&[aud, sei, &sps, pps, slice], four);
            assert_eq!(h264_declared_reorder_depth(&au), Some(3));
            assert_eq!(annexb_nal_units(&au).count(), 5);
            // Trailing zero bytes (the next start code's leading zero, or
            // stuffing) do not end up in the SPS.
            let mut padded = au.clone();
            padded.extend_from_slice(&[0, 0]);
            assert_eq!(h264_declared_reorder_depth(&padded), Some(3));
        }
        let no_sps = annexb(&[aud, pps, slice], true);
        assert_eq!(h264_declared_reorder_depth(&no_sps), None);
        assert_eq!(h264_declared_reorder_depth(&hex(NINE)), None, "no start code");
        let sky = annexb(&[aud, &hex(SKY_SPORTS), pps, slice], true);
        assert_eq!(h264_declared_reorder_depth(&sky), None);
        assert!(find_h264_sps(&sky).is_some(), "the SPS parses; it just declares nothing");
    }

    /// Bit writer for synthesising SPS edge cases the captures do not have.
    struct BitWriter {
        bits: Vec<bool>,
    }

    impl BitWriter {
        fn new() -> Self {
            Self { bits: Vec::new() }
        }
        fn u(&mut self, n: u32, v: u64) -> &mut Self {
            for i in (0..n).rev() {
                self.bits.push((v >> i) & 1 == 1);
            }
            self
        }
        fn ue(&mut self, v: u64) -> &mut Self {
            let x = v + 1;
            let len = 64 - x.leading_zeros();
            self.u(len - 1, 0).u(len, x)
        }
        /// Stop bit, byte alignment, emulation prevention, NAL header.
        fn nal(&self) -> Vec<u8> {
            let mut bits = self.bits.clone();
            bits.push(true);
            while !bits.len().is_multiple_of(8) {
                bits.push(false);
            }
            let rbsp: Vec<u8> = bits
                .chunks(8)
                .map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8))
                .collect();
            let mut out = vec![0x67];
            let mut zeros = 0;
            for b in rbsp {
                if zeros >= 2 && b <= 3 {
                    out.push(3);
                    zeros = 0;
                }
                zeros = if b == 0 { zeros + 1 } else { 0 };
                out.push(b);
            }
            out
        }
    }

    /// Baseline-profile SPS up to and including `vui_parameters_present_flag
    /// = 1`, 1280x720 progressive.
    fn baseline_head() -> BitWriter {
        let mut w = BitWriter::new();
        w.u(8, 66).u(8, 0).u(8, 31); // profile, constraints, level
        w.ue(0); // sps_id
        w.ue(0); // log2_max_frame_num_minus4
        w.ue(2); // poc type 2
        w.ue(1); // max_num_ref_frames
        w.u(1, 0); // gaps
        w.ue(79).ue(44); // 80x45 MBs
        w.u(1, 1); // frame_mbs_only
        w.u(1, 1); // direct_8x8
        w.u(1, 0); // no cropping
        w.u(1, 1); // vui present
        w
    }

    /// Common VUI part: no SAR, overscan, signal type or chroma location.
    fn vui_common(w: &mut BitWriter) {
        w.u(4, 0);
    }

    fn restriction(w: &mut BitWriter, fields: [u64; 6]) {
        w.u(1, 1); // bitstream_restriction_flag
        w.u(1, 1); // motion_vectors_over_pic_boundaries
        for f in fields {
            w.ue(f);
        }
    }

    #[test]
    fn synthetic_restriction_edge_cases() {
        // Plain declaration → reported.
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(1, 0).u(1, 0).u(1, 0).u(1, 0); // timing, nal hrd, vcl hrd, pic_struct
        restriction(&mut w, [2, 1, 16, 16, 4, 5]);
        let info = parse_h264_sps(&w.nal()).unwrap();
        assert_eq!(info.max_num_reorder_frames, Some(4));
        assert_eq!((info.width, info.height), (1280, 720));

        // num_reorder_frames > 16: libavcodec refuses the SPS outright.
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(4, 0);
        restriction(&mut w, [2, 1, 16, 16, 17, 17]);
        assert_eq!(parse_h264_sps(&w.nal()), None);

        // A field past get_ue_golomb_31's exact range: libavcodec misreads
        // it, so the depth is undeclared (but the SPS stands).
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(4, 0);
        restriction(&mut w, [2, 1, 40, 16, 1, 2]);
        let info = parse_h264_sps(&w.nal()).unwrap();
        assert_eq!(info.max_num_reorder_frames, None);

        // SPS ends right after pic_struct_present_flag: libavcodec returns
        // before reading bitstream_restriction_flag.
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(3, 0).u(1, 1);
        let info = parse_h264_sps(&w.nal()).unwrap();
        assert!(info.pic_struct_present);
        assert_eq!(info.max_num_reorder_frames, None);

        // "Truncated VUI": timing flag set with fewer than 10 bits left.
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(1, 1).u(8, 0);
        let info = parse_h264_sps(&w.nal()).unwrap();
        assert_eq!((info.timing, info.max_num_reorder_frames), (None, None));

        // cpb_cnt 33 in the NAL HRD: libavcodec refuses the SPS.
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(1, 0); // timing
        w.u(1, 1).ue(32); // nal hrd, cpb_cnt_minus1 = 32
        w.u(8, 0);
        for _ in 0..33 {
            w.ue(0).ue(0).u(1, 0);
        }
        w.u(20, 0);
        w.u(1, 0).u(1, 0).u(1, 0); // vcl hrd, low_delay, pic_struct
        restriction(&mut w, [2, 1, 16, 16, 0, 1]);
        assert_eq!(parse_h264_sps(&w.nal()), None);

        // Same with cpb_cnt 32: legal, declared.
        let mut w = baseline_head();
        vui_common(&mut w);
        w.u(1, 0);
        w.u(1, 1).ue(31);
        w.u(8, 0);
        for _ in 0..32 {
            w.ue(0).ue(0).u(1, 0);
        }
        w.u(20, 0);
        w.u(1, 0).u(1, 0).u(1, 0);
        restriction(&mut w, [2, 1, 16, 16, 0, 1]);
        let info = parse_h264_sps(&w.nal()).unwrap();
        assert!(info.nal_hrd_present);
        assert_eq!(info.max_num_reorder_frames, Some(0));
    }

    #[test]
    fn synthetic_rejections_mirror_libavcodec() {
        // POC type 3 → refused.
        let mut w = BitWriter::new();
        w.u(8, 66).u(8, 0).u(8, 31).ue(0).ue(0).ue(3);
        w.u(32, 0);
        assert_eq!(parse_h264_sps(&w.nal()), None);
        // 17 reference frames → refused.
        let mut w = BitWriter::new();
        w.u(8, 66).u(8, 0).u(8, 31).ue(0).ue(0).ue(2).ue(17);
        w.u(32, 0);
        assert_eq!(parse_h264_sps(&w.nal()), None);
        // sps_id 32 → refused.
        let mut w = BitWriter::new();
        w.u(8, 66).u(8, 0).u(8, 31).ue(32);
        w.u(32, 0);
        assert_eq!(parse_h264_sps(&w.nal()), None);
        // High profile, unequal luma/chroma bit depth → refused.
        let mut w = BitWriter::new();
        w.u(8, 100).u(8, 0).u(8, 40).ue(0).ue(1).ue(2).ue(0);
        w.u(32, 0);
        assert_eq!(parse_h264_sps(&w.nal()), None);
        // Profile 139 is not on libavcodec's chroma-info list, so its SPS is
        // read without chroma_format_idc / bit depths / scaling there — and
        // must be here, or every later field lands on the wrong bits.
        let mut w = baseline_head();
        w.bits.splice(0..8, (0..8).rev().map(|i| (139u8 >> i) & 1 == 1));
        vui_common(&mut w);
        w.u(4, 0);
        restriction(&mut w, [2, 1, 16, 16, 1, 2]);
        let info = parse_h264_sps(&w.nal()).unwrap();
        assert_eq!(info.profile_idc, 139);
        assert_eq!(info.max_num_reorder_frames, Some(1));
        assert_eq!((info.width, info.height), (1280, 720));
    }
}
