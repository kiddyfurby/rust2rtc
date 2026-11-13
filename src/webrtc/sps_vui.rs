//! H.264 SPS VUI colour-signalling repair for the WebRTC path.
//!
//! Some IP cameras (Hisilicon OEM firmware, e.g. the `living` camera) embed
//! junk colour metadata in the SPS VUI: `matrix_coefficients = 0` ("identity",
//! i.e. GBR — wrong for 4:2:0 YUV) and `transfer_characteristics = 0`
//! (reserved). Chrome's MSE path ignores H.264 VUI colour info entirely and
//! falls back to BT.601, so recordings and the Frigate dashboard look correct.
//! The WebRTC receive path *honours* the VUI, so the same bitstream renders
//! green-tinted (Y shown in the G channel, U/V pinned at 128) in Home
//! Assistant. We rewrite only the broken fields (matrix → BT.601, transfer →
//! BT.709) in SPS NAL units forwarded to WebRTC consumers; the MSE path keeps
//! the original SPS untouched.

/// VUI colour fields of an SPS, with bit offsets into the unescaped RBSP
/// (MSB-first numbering) so the values can be patched in place.
#[derive(Debug, PartialEq)]
pub struct VuiColorFields {
    pub colour_desc_present: bool,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// Bit offsets of the three description fields; `None` when there is no
    /// colour description.
    pub primaries_bit: Option<usize>,
    pub transfer_bit: Option<usize>,
    pub matrix_bit: Option<usize>,
}

/// MSB-first bit reader over an RBSP.
struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, bit: 0 }
    }

    fn read_bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.bit / 8)?;
            let bit = (byte >> (7 - (self.bit % 8))) & 1;
            v = (v << 1) | bit as u32;
            self.bit += 1;
        }
        Some(v)
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.read_bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        if zeros == 0 {
            return Some(0);
        }
        let rest = self.read_bits(zeros)?;
        Some((1 << zeros) - 1 + rest)
    }

    fn read_se(&mut self) -> Option<i32> {
        let ue = self.read_ue()?;
        Some(if ue % 2 == 1 {
            ((ue + 1) / 2) as i32
        } else {
            -((ue / 2) as i32)
        })
    }
}

/// Remove H.264 emulation-prevention bytes (0x03 after 00 00).
fn unescape_rbsp(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut zeros = 0u8;
    for &b in escaped {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Re-insert emulation-prevention bytes.
fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 2);
    let mut zeros = 0u8;
    for &b in rbsp {
        if zeros >= 2 && b <= 0x03 {
            out.push(0x03);
            zeros = 0;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

/// Write `n` bits (MSB-first) at bit offset `start` into an RBSP in place.
fn write_bits(rbsp: &mut [u8], start: usize, n: u32, value: u32) {
    for i in 0..n {
        let bit = ((value >> (n - 1 - i)) & 1) as u8;
        let pos = start + i as usize;
        let byte = &mut rbsp[pos / 8];
        let mask = 1 << (7 - (pos % 8));
        if bit == 1 {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Parse an SPS RBSP (NAL header byte excluded) and return the VUI colour
/// fields plus their bit offsets. Returns `None` if the SPS is truncated or
/// malformed.
pub fn parse_vui_color(rbsp: &[u8]) -> Option<VuiColorFields> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.read_bits(8)?;
    r.read_bits(8)?; // constraint flags + reserved
    r.read_bits(8)?; // level_idc
    r.read_ue()?; // seq_parameter_set_id

    if [
        100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135,
    ]
    .contains(&profile_idc)
    {
        let chroma_format_idc = r.read_ue()?;
        if chroma_format_idc == 3 {
            r.read_bits(1)?; // separate_colour_plane_flag
        }
        r.read_ue()?; // bit_depth_luma_minus8
        r.read_ue()?; // bit_depth_chroma_minus8
        r.read_bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.read_bits(1)? == 1 {
            // seq_scaling_matrix_present_flag: skip the scaling lists
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                if r.read_bits(1)? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..size {
                        if next != 0 {
                            let delta = r.read_se()?;
                            next = (last + delta + 256) % 256;
                        }
                        last = next;
                    }
                }
            }
        }
    }

    r.read_ue()?; // log2_max_frame_num_minus4
    let poc_type = r.read_ue()?;
    match poc_type {
        0 => {
            r.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.read_bits(1)?; // delta_pic_order_always_zero_flag
            r.read_se()?; // offset_for_non_ref_pic
            r.read_se()?; // offset_for_top_to_bottom_field
            let cycle = r.read_ue()?;
            for _ in 0..cycle {
                r.read_se()?;
            }
        }
        _ => {}
    }

    r.read_ue()?; // max_num_ref_frames
    r.read_bits(1)?; // gaps_in_frame_num_value_allowed_flag
    r.read_ue()?; // pic_width_in_mbs_minus1
    r.read_ue()?; // pic_height_in_map_units_minus1
    let frame_mbs_only = r.read_bits(1)?;
    if frame_mbs_only == 0 {
        r.read_bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.read_bits(1)?; // direct_8x8_inference_flag
    if r.read_bits(1)? == 1 {
        // frame_cropping_flag
        r.read_ue()?; // crop left
        r.read_ue()?; // crop right
        r.read_ue()?; // crop top
        r.read_ue()?; // crop bottom
    }

    if r.read_bits(1)? == 0 {
        // vui_parameters_present_flag
        return Some(VuiColorFields {
            colour_desc_present: false,
            primaries: 2,
            transfer: 2,
            matrix: 2,
            primaries_bit: None,
            transfer_bit: None,
            matrix_bit: None,
        });
    }

    // VUI
    if r.read_bits(1)? == 1 {
        // aspect_ratio_info_present_flag
        let idc = r.read_bits(8)?;
        if idc == 255 {
            r.read_bits(16)?; // sar_width
            r.read_bits(16)?; // sar_height
        }
    }
    if r.read_bits(1)? == 1 {
        r.read_bits(1)?; // overscan_info_present_flag -> overscan_appropriate_flag
    }
    let mut out = VuiColorFields {
        colour_desc_present: false,
        primaries: 2,
        transfer: 2,
        matrix: 2,
        primaries_bit: None,
        transfer_bit: None,
        matrix_bit: None,
    };
    if r.read_bits(1)? == 1 {
        // video_signal_type_present_flag
        r.read_bits(3)?; // video_format
        r.read_bits(1)?; // video_full_range_flag (kept as-is: Chrome's WebRTC path honours it)
        if r.read_bits(1)? == 1 {
            // colour_description_present_flag
            out.colour_desc_present = true;
            out.primaries_bit = Some(r.bit);
            out.primaries = r.read_bits(8)? as u8;
            out.transfer_bit = Some(r.bit);
            out.transfer = r.read_bits(8)? as u8;
            out.matrix_bit = Some(r.bit);
            out.matrix = r.read_bits(8)? as u8;
        }
    }
    Some(out)
}

/// If `nal` is an H.264 SPS NAL unit (header byte + escaped RBSP) carrying junk
/// colour signalling, return a rewritten NAL with sane values (BT.601 matrix,
/// BT.709 transfer). Returns `None` when the NAL is not an SPS, carries no
/// colour description, or already signals sanely.
pub fn rewrite_sps_color_if_junk(nal: &[u8]) -> Option<Vec<u8>> {
    if nal.len() < 5 || nal[0] & 0x1F != 7 {
        return None;
    }
    let rbsp = unescape_rbsp(&nal[1..]);
    let fields = parse_vui_color(&rbsp)?;
    if !fields.colour_desc_present {
        return None;
    }
    let junk_matrix = fields.matrix == 0;
    let junk_transfer = fields.transfer == 0;
    if !junk_matrix && !junk_transfer {
        return None;
    }
    let mut rbsp = rbsp;
    if junk_transfer {
        write_bits(&mut rbsp, fields.transfer_bit?, 8, 1); // BT.709
    }
    if junk_matrix {
        write_bits(&mut rbsp, fields.matrix_bit?, 8, 5); // BT.601
    }
    let escaped = escape_rbsp(&rbsp);
    let mut out = Vec::with_capacity(escaped.len() + 1);
    out.push(nal[0]);
    out.extend_from_slice(&escaped);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Living camera SPS from its RTSP SDP sprop-parameter-sets
    // (profile_idc=77 Main, level 3.0, full_range=1, primaries=1, transfer=0, matrix=0).
    const LIVING_SPS: &[u8] = &[
        0x67, 0x4d, 0x00, 0x20, 0x96, 0x54, 0x01, 0x20, 0x05, 0x19, 0x37, 0x01, 0x00, 0x00, 0x40,
        0x00, 0x00, 0x7d, 0x00, 0x00, 0x13, 0x88, 0x21,
    ];

    #[test]
    fn parses_living_sps_junk_vui() {
        let rbsp = unescape_rbsp(&LIVING_SPS[1..]);
        let f = parse_vui_color(&rbsp).unwrap();
        assert!(f.colour_desc_present);
        assert_eq!(f.primaries, 1); // BT.709
        assert_eq!(f.transfer, 0); // reserved (junk)
        assert_eq!(f.matrix, 0); // identity/GBR (junk)
    }

    #[test]
    fn rewrites_living_sps_to_bt601() {
        let rewritten = rewrite_sps_color_if_junk(LIVING_SPS).expect("should rewrite");
        assert_eq!(rewritten[0], 0x67); // NAL header preserved
        // Same length: fields are patched in place and this SPS has no
        // emulation-prevention bytes to rebalance.
        assert_eq!(rewritten.len(), LIVING_SPS.len());
        // transfer -> 1 (BT.709), matrix -> 5 (BT.601). The colour
        // description sits at RBSP bytes 10-12, i.e. NAL bytes 11-13
        // (NAL byte 0 is the header): primaries (NAL byte 11) stays 0x01,
        // transfer (NAL byte 12) becomes 0x01 and matrix (NAL byte 13)
        // becomes 0x05.
        assert_eq!(rewritten[12], 0x01);
        assert_eq!(rewritten[13], 0x05);
        for i in 0..LIVING_SPS.len() {
            if i != 12 && i != 13 {
                assert_eq!(rewritten[i], LIVING_SPS[i], "byte {} changed", i);
            }
        }
        // Rewritten SPS still parses to sane fields.
        let rbsp = unescape_rbsp(&rewritten[1..]);
        let f = parse_vui_color(&rbsp).unwrap();
        assert!(f.colour_desc_present);
        assert_eq!(f.transfer, 1);
        assert_eq!(f.matrix, 5);
    }

    #[test]
    fn leaves_sane_or_vui_less_sps_alone() {
        // SPS with no VUI at all (vui_parameters_present_flag = 0)
        let no_vui: &[u8] = &[0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x01, 0x20, 0x05, 0x19, 0x37];
        assert!(rewrite_sps_color_if_junk(no_vui).is_none());
        // Not an SPS NAL
        assert!(rewrite_sps_color_if_junk(&[0x41, 0x9a, 0x00, 0x11, 0x22, 0x33]).is_none());
    }

    #[test]
    fn unescape_escape_roundtrip() {
        let escaped = [0x00, 0x00, 0x03, 0x01, 0x00, 0x00, 0x03, 0x03, 0xff];
        let rbsp = unescape_rbsp(&escaped);
        assert_eq!(rbsp, [0x00, 0x00, 0x01, 0x00, 0x00, 0x03, 0xff]);
        assert_eq!(escape_rbsp(&rbsp), escaped);
    }
}