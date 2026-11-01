#![allow(dead_code)]

/// Pure-Rust fragmented MP4 (fMP4 / ISO BMFF) muxer for MSE streaming
pub struct BoxWriter {
    pub buf: Vec<u8>,
}

impl BoxWriter {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(1024),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
        }
    }

    pub fn start_box(&mut self, name: &[u8; 4]) -> usize {
        let pos = self.buf.len();
        self.buf.extend_from_slice(&[0, 0, 0, 0]); // placeholder for 32-bit size
        self.buf.extend_from_slice(name);
        pos
    }

    pub fn end_box(&mut self, pos: usize) {
        let size = (self.buf.len() - pos) as u32;
        self.buf[pos..pos + 4].copy_from_slice(&size.to_be_bytes());
    }

    pub fn write_box(&mut self, name: &[u8; 4], payload: &[u8]) {
        let size = (8 + payload.len()) as u32;
        self.buf.extend_from_slice(&size.to_be_bytes());
        self.buf.extend_from_slice(name);
        self.buf.extend_from_slice(payload);
    }

    pub fn write_full_box(&mut self, name: &[u8; 4], version: u8, flags: u32, payload: &[u8]) {
        let size = (12 + payload.len()) as u32;
        self.buf.extend_from_slice(&size.to_be_bytes());
        self.buf.extend_from_slice(name);
        self.buf.push(version);
        self.buf.push(((flags >> 16) & 0xFF) as u8);
        self.buf.push(((flags >> 8) & 0xFF) as u8);
        self.buf.push((flags & 0xFF) as u8);
        self.buf.extend_from_slice(payload);
    }
}

/// Generate ISO BMFF Initialization Segment (`ftyp` + `moov`) for H.264
pub fn make_h264_init_segment(sps: &[u8], pps: &[u8], width: u32, height: u32) -> Vec<u8> {
    let mut writer = BoxWriter::with_capacity(1024);

    // 1. 'ftyp' box (24 bytes)
    // Major brand: isom, Minor version: 0x00000200, Compatible: isom, iso6, mp41
    let mut ftyp = Vec::with_capacity(16);
    ftyp.extend_from_slice(b"isom");
    ftyp.extend_from_slice(&0x00000200u32.to_be_bytes());
    ftyp.extend_from_slice(b"isomiso6mp41");
    writer.write_box(b"ftyp", &ftyp);

    // 2. 'moov' box
    let moov_pos = writer.start_box(b"moov");

    // 'mvhd' box (108 bytes)
    // version 0, flags 0, creation 0, mod 0, timescale 90000, duration 0, rate 1.0, vol 1.0
    let mut mvhd = Vec::with_capacity(96);
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation time
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification time
    mvhd.extend_from_slice(&90000u32.to_be_bytes()); // timescale
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // duration = 0 for live fMP4
    mvhd.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate = 1.0
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume = 1.0
    mvhd.extend_from_slice(&[0u8; 10]); // reserved
    // Unity matrix (3x3)
    let unity_matrix: [u32; 9] = [
        0x00010000, 0, 0,
        0, 0x00010000, 0,
        0, 0, 0x40000000,
    ];
    for val in unity_matrix {
        mvhd.extend_from_slice(&val.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0u8; 24]); // pre-defined
    mvhd.extend_from_slice(&2u32.to_be_bytes()); // next_track_id = 2
    writer.write_full_box(b"mvhd", 0, 0, &mvhd);

    // 'trak' box (Video Track ID = 1)
    let trak_pos = writer.start_box(b"trak");

    // 'tkhd' box
    // version 0, flags 0x07 (track enabled, track in movie, track in preview)
    let mut tkhd = Vec::with_capacity(80);
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // creation time
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // mod time
    tkhd.extend_from_slice(&1u32.to_be_bytes()); // track_id = 1
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // reserved
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // duration = 0
    tkhd.extend_from_slice(&[0u8; 8]); // reserved
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer = 0
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // alternate group = 0
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // volume = 0 (video)
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // reserved
    for val in unity_matrix {
        tkhd.extend_from_slice(&val.to_be_bytes());
    }
    tkhd.extend_from_slice(&(width << 16).to_be_bytes()); // width (16.16 fixed point)
    tkhd.extend_from_slice(&(height << 16).to_be_bytes()); // height (16.16 fixed point)
    writer.write_full_box(b"tkhd", 0, 0x07, &tkhd);

    // 'mdia' box
    let mdia_pos = writer.start_box(b"mdia");

    // 'mdhd' box (Media Header: timescale 90000, duration 0)
    let mut mdhd = Vec::with_capacity(20);
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // creation
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // mod
    mdhd.extend_from_slice(&90000u32.to_be_bytes()); // timescale
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // language = 'und'
    mdhd.extend_from_slice(&0u16.to_be_bytes()); // pre-defined
    writer.write_full_box(b"mdhd", 0, 0, &mdhd);

    // 'hdlr' box (Handler: vide)
    let mut hdlr = Vec::with_capacity(32);
    hdlr.extend_from_slice(&0u32.to_be_bytes()); // pre-defined
    hdlr.extend_from_slice(b"vide"); // handler_type
    hdlr.extend_from_slice(&[0u8; 12]); // reserved
    hdlr.extend_from_slice(b"VideoHandler\0");
    writer.write_full_box(b"hdlr", 0, 0, &hdlr);

    // 'minf' box (Media Information)
    let minf_pos = writer.start_box(b"minf");

    // 'vmhd' box (Video Media Header)
    writer.write_full_box(b"vmhd", 0, 1, &[0u8; 8]); // graphicsmode = 0, opcolor = 0

    // 'dinf' box
    let dinf_pos = writer.start_box(b"dinf");
    // 'dref' box containing single 'url ' box with flag 1 (in-file)
    let mut dref = Vec::with_capacity(20);
    dref.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
    // 'url ' box inside dref
    dref.extend_from_slice(&12u32.to_be_bytes()); // size of url box
    dref.extend_from_slice(b"url ");
    dref.extend_from_slice(&[0, 0, 0, 1]); // version 0, flags 1
    writer.write_full_box(b"dref", 0, 0, &dref);
    writer.end_box(dinf_pos);

    // 'stbl' box (Sample Table)
    let stbl_pos = writer.start_box(b"stbl");

    // 'stsd' box (Sample Description)
    let stsd_pos = writer.start_box(b"stsd");
    writer.buf.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0
    writer.buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1

    // 'avc1' box
    let avc1_pos = writer.start_box(b"avc1");
    let mut avc1_hdr = Vec::with_capacity(70);
    avc1_hdr.extend_from_slice(&[0u8; 6]); // reserved
    avc1_hdr.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index = 1
    avc1_hdr.extend_from_slice(&0u16.to_be_bytes()); // pre-defined
    avc1_hdr.extend_from_slice(&0u16.to_be_bytes()); // reserved
    avc1_hdr.extend_from_slice(&[0u8; 12]); // pre-defined
    avc1_hdr.extend_from_slice(&(width as u16).to_be_bytes()); // width
    avc1_hdr.extend_from_slice(&(height as u16).to_be_bytes()); // height
    avc1_hdr.extend_from_slice(&0x00480000u32.to_be_bytes()); // horizresolution 72 dpi
    avc1_hdr.extend_from_slice(&0x00480000u32.to_be_bytes()); // vertresolution 72 dpi
    avc1_hdr.extend_from_slice(&0u32.to_be_bytes()); // reserved
    avc1_hdr.extend_from_slice(&1u16.to_be_bytes()); // frame_count = 1
    avc1_hdr.extend_from_slice(&[0u8; 32]); // compressorname (32 bytes)
    avc1_hdr.extend_from_slice(&0x0018u16.to_be_bytes()); // depth = 24
    avc1_hdr.extend_from_slice(&(-1i16).to_be_bytes()); // pre-defined = -1
    writer.buf.extend_from_slice(&avc1_hdr);

    // 'avcC' box (AVC Configuration Record with SPS and PPS)
    let profile = if sps.len() > 1 { sps[1] } else { 0x64 }; // High profile default
    let compat = if sps.len() > 2 { sps[2] } else { 0x00 };
    let level = if sps.len() > 3 { sps[3] } else { 0x28 }; // Level 4.0 default

    let mut avcc = Vec::with_capacity(32 + sps.len() + pps.len());
    avcc.push(1); // configurationVersion = 1
    avcc.push(profile); // AVCProfileIndication
    avcc.push(compat); // profile_compatibility
    avcc.push(level); // AVCLevelIndication
    avcc.push(0xFF); // lengthSizeMinusOne = 3 (4 bytes length prefix)
    avcc.push(0xE1); // numOfSequenceParameterSets = 1
    avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(sps);
    avcc.push(1); // numOfPictureParameterSets = 1
    avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    avcc.extend_from_slice(pps);
    writer.write_box(b"avcC", &avcc);

    writer.end_box(avc1_pos);
    writer.end_box(stsd_pos);

    // Empty stts, stsc, stsz, stco for fMP4
    writer.write_full_box(b"stts", 0, 0, &[0u8; 4]);
    writer.write_full_box(b"stsc", 0, 0, &[0u8; 4]);
    writer.write_full_box(b"stsz", 0, 0, &[0u8; 8]);
    writer.write_full_box(b"stco", 0, 0, &[0u8; 4]);

    writer.end_box(stbl_pos);
    writer.end_box(minf_pos);
    writer.end_box(mdia_pos);
    writer.end_box(trak_pos);

    // 'mvex' box (Movie Extends)
    let mvex_pos = writer.start_box(b"mvex");
    // 'trex' box (Track Extends for track 1)
    let mut trex = Vec::with_capacity(24);
    trex.extend_from_slice(&1u32.to_be_bytes()); // track_id = 1
    trex.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index = 1
    trex.extend_from_slice(&3000u32.to_be_bytes()); // default_sample_duration = 3000 (30fps)
    trex.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size = 0
    trex.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags = 0
    writer.write_full_box(b"trex", 0, 0, &trex);
    writer.end_box(mvex_pos);

    writer.end_box(moov_pos);

    writer.buf
}

/// Generate ISO BMFF Media Segment (`moof` + `mdat`) containing a video frame
pub fn make_h264_media_segment(
    seq: u32,
    base_decode_time: u64,
    duration: u32,
    is_keyframe: bool,
    nalus: &[Vec<u8>],
) -> Vec<u8> {
    if nalus.is_empty() {
        return Vec::new();
    }

    // Calculate total size of sample in mdat (4 bytes length + NALU bytes)
    let sample_size: usize = nalus.iter().map(|n| 4 + n.len()).sum();

    let mut writer = BoxWriter::with_capacity(128 + sample_size);

    // 1. 'moof' box
    let moof_pos = writer.start_box(b"moof");

    // 'mfhd' box (Movie Fragment Header: sequence_number)
    let mut mfhd = Vec::with_capacity(4);
    mfhd.extend_from_slice(&seq.to_be_bytes());
    writer.write_full_box(b"mfhd", 0, 0, &mfhd);

    // 'traf' box
    let traf_pos = writer.start_box(b"traf");

    // 'tfhd' box (Track Fragment Header: track_id=1, default-base-is-moof = 0x020000)
    let mut tfhd = Vec::with_capacity(4);
    tfhd.extend_from_slice(&1u32.to_be_bytes()); // track_id = 1
    writer.write_full_box(b"tfhd", 0, 0x020000, &tfhd);

    // 'tfdt' box (Base Media Decode Time: version 1, 64-bit timestamp)
    let mut tfdt = Vec::with_capacity(8);
    tfdt.extend_from_slice(&base_decode_time.to_be_bytes());
    writer.write_full_box(b"tfdt", 1, 0, &tfdt);

    // 'trun' box (Track Fragment Run)
    // Flags: 0x000F01
    // 0x000001: data-offset-present
    // 0x000100: sample-duration-present
    // 0x000200: sample-size-present
    // 0x000400: sample-flags-present
    let sample_flags = if is_keyframe {
        0x02000000u32 // sample_depends_on = 2 (independent)
    } else {
        0x01010000u32 // sample_depends_on = 1 (dependent), sample_is_non_sync = 1
    };

    let mut trun = Vec::with_capacity(24);
    trun.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
    trun.extend_from_slice(&0i32.to_be_bytes()); // placeholder for data_offset
    trun.extend_from_slice(&duration.to_be_bytes()); // sample_duration
    trun.extend_from_slice(&(sample_size as u32).to_be_bytes()); // sample_size
    trun.extend_from_slice(&sample_flags.to_be_bytes()); // sample_flags

    // We write trun and traf, then fix up data_offset = moof_size + 8 (mdat header)
    writer.write_full_box(b"trun", 0, 0x000F01, &trun);
    writer.end_box(traf_pos);
    writer.end_box(moof_pos);

    // Fix data_offset inside trun:
    // data_offset is the byte offset from the start of the 'moof' box to the start of payload inside 'mdat'
    let moof_len = writer.buf.len();
    let data_offset = (moof_len + 8) as i32; // +8 for [mdat_size: 4][b"mdat": 4]

    // Find trun data offset placeholder and patch it
    // trun payload starts at writer.buf inside moof
    if let Some(pos) = find_subsequence(&writer.buf[..moof_len], b"trun") {
        // trun box: [4 bytes size][4 bytes name 'trun'][1 byte version][3 bytes flags][4 bytes sample_count][4 bytes data_offset]
        let offset_pos = pos + 4 + 1 + 3 + 4;
        if offset_pos + 4 <= moof_len {
            writer.buf[offset_pos..offset_pos + 4].copy_from_slice(&data_offset.to_be_bytes());
        }
    }

    // 2. 'mdat' box
    let mdat_size = (8 + sample_size) as u32;
    writer.buf.extend_from_slice(&mdat_size.to_be_bytes());
    writer.buf.extend_from_slice(b"mdat");
    for nalu in nalus {
        writer.buf.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
        writer.buf.extend_from_slice(nalu);
    }

    writer.buf
}

/// Helper to parse H.264 SPS width and height
pub fn parse_h264_sps_dimensions(sps: &[u8]) -> (u32, u32) {
    if sps.len() < 4 {
        return (1920, 1080);
    }

    let mut reader = BitReader::new(&sps[1..]); // Skip NAL header
    let profile_idc = reader.read_bits(8).unwrap_or(0);
    reader.skip_bits(8); // constraint flags
    reader.skip_bits(8); // level_idc
    let _sps_id = reader.read_ue().unwrap_or(0);

    if [100, 110, 122, 244, 44, 83, 86, 118, 128].contains(&profile_idc) {
        let chroma_format_idc = reader.read_ue().unwrap_or(1);
        if chroma_format_idc == 3 {
            reader.skip_bits(1); // separate_colour_plane_flag
        }
        let _bit_depth_luma = reader.read_ue().unwrap_or(0);
        let _bit_depth_chroma = reader.read_ue().unwrap_or(0);
        reader.skip_bits(1); // qpprime_y_zero_transform_bypass_flag
        let seq_scaling_matrix_present = reader.read_bits(1).unwrap_or(0);
        if seq_scaling_matrix_present != 0 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for _ in 0..count {
                if reader.read_bits(1).unwrap_or(0) != 0 {
                    // skip scaling list
                    let mut last = 8i32;
                    let mut next = 8i32;
                    for _ in 0..16 {
                        if next != 0 {
                            let delta = reader.read_se().unwrap_or(0);
                            next = (last + delta + 256) % 256;
                        }
                        last = if next == 0 { last } else { next };
                    }
                }
            }
        }
    }

    let _log2_max_frame_num = reader.read_ue().unwrap_or(0);
    let pic_order_cnt_type = reader.read_ue().unwrap_or(0);
    if pic_order_cnt_type == 0 {
        let _log2_max_pic_order_cnt_lsb = reader.read_ue().unwrap_or(0);
    } else if pic_order_cnt_type == 1 {
        reader.skip_bits(1);
        let _offset_for_non_ref_pic = reader.read_se().unwrap_or(0);
        let _offset_for_top_to_bottom = reader.read_se().unwrap_or(0);
        let num_ref_frames_in_pic_order_cnt_cycle = reader.read_ue().unwrap_or(0);
        for _ in 0..num_ref_frames_in_pic_order_cnt_cycle {
            let _ = reader.read_se();
        }
    }

    let _max_num_ref_frames = reader.read_ue().unwrap_or(0);
    reader.skip_bits(1); // gaps_in_frame_num_value_allowed_flag
    let pic_width_in_mbs_minus1 = reader.read_ue().unwrap_or(119);
    let pic_height_in_map_units_minus1 = reader.read_ue().unwrap_or(67);
    let frame_mbs_only_flag = reader.read_bits(1).unwrap_or(1);
    if frame_mbs_only_flag == 0 {
        reader.skip_bits(1); // mb_adaptive_frame_field_flag
    }
    reader.skip_bits(1); // direct_8x8_inference_flag
    let frame_cropping_flag = reader.read_bits(1).unwrap_or(0);

    let (crop_left, crop_right, crop_top, crop_bottom) = if frame_cropping_flag != 0 {
        (
            reader.read_ue().unwrap_or(0),
            reader.read_ue().unwrap_or(0),
            reader.read_ue().unwrap_or(0),
            reader.read_ue().unwrap_or(0),
        )
    } else {
        (0, 0, 0, 0)
    };

    let width = ((pic_width_in_mbs_minus1 + 1) * 16) - (crop_left + crop_right) * 2;
    let height = ((2 - frame_mbs_only_flag) * (pic_height_in_map_units_minus1 + 1) * 16)
        - (crop_top + crop_bottom) * 2;

    if width > 0 && height > 0 {
        (width, height)
    } else {
        (1920, 1080)
    }
}

/// Simple bit reader for parsing SPS Exp-Golomb codes
struct BitReader<'a> {
    data: &'a [u8],
    byte_idx: usize,
    bit_idx: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_idx: 0,
            bit_idx: 0,
        }
    }

    fn read_bits(&mut self, n: usize) -> Option<u32> {
        let mut val = 0u32;
        for _ in 0..n {
            if self.byte_idx >= self.data.len() {
                return None;
            }
            let bit = (self.data[self.byte_idx] >> (7 - self.bit_idx)) & 1;
            val = (val << 1) | (bit as u32);
            self.bit_idx += 1;
            if self.bit_idx == 8 {
                self.bit_idx = 0;
                self.byte_idx += 1;
            }
        }
        Some(val)
    }

    fn skip_bits(&mut self, n: usize) {
        let total = self.bit_idx as usize + n;
        self.byte_idx += total / 8;
        self.bit_idx = (total % 8) as u8;
    }

    fn read_ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
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
        let val = self.read_ue()?;
        let sign = if val & 1 == 1 { 1 } else { -1 };
        Some(sign * ((val + 1) / 2) as i32)
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// H.264 RTP Depacketizer: reconstructs complete NALUs from single NAL, STAP-A, and FU-A RTP packets
pub struct H264RtpDepacketizer {
    fu_buffer: Vec<u8>,
    current_frame: Vec<Vec<u8>>,
    last_timestamp: Option<u32>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

impl H264RtpDepacketizer {
    pub fn new() -> Self {
        Self {
            fu_buffer: Vec::with_capacity(65536),
            current_frame: Vec::new(),
            last_timestamp: None,
            sps: None,
            pps: None,
        }
    }

    /// Feed an RTP packet; returns (is_keyframe, timestamp, nalus) when a complete frame is finished
    pub fn feed_packet(&mut self, packet: &crate::core::RtpPacket) -> Option<(bool, u32, Vec<Vec<u8>>)> {
        let payload = packet.payload();
        if payload.is_empty() {
            return None;
        }

        let mut completed_frame = None;

        // Check if timestamp changed (new frame boundary)
        if let Some(last_ts) = self.last_timestamp {
            if last_ts != packet.timestamp && !self.current_frame.is_empty() {
                let is_keyframe = self.current_frame.iter().any(|n| !n.is_empty() && (n[0] & 0x1F == 5));
                let nalus = std::mem::take(&mut self.current_frame);
                completed_frame = Some((is_keyframe, last_ts, nalus));
            }
        }
        self.last_timestamp = Some(packet.timestamp);

        let nal_type = payload[0] & 0x1F;

        if (1..=23).contains(&nal_type) {
            // Single NAL unit packet
            if nal_type == 7 {
                self.sps = Some(payload.to_vec());
            } else if nal_type == 8 {
                self.pps = Some(payload.to_vec());
            }
            self.current_frame.push(payload.to_vec());
        } else if nal_type == 24 {
            // STAP-A packet (aggregates multiple NALUs)
            let mut offset = 1;
            while offset + 2 <= payload.len() {
                let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;
                if offset + size <= payload.len() {
                    let nalu = payload[offset..offset + size].to_vec();
                    let inner_type = nalu[0] & 0x1F;
                    if inner_type == 7 {
                        self.sps = Some(nalu.clone());
                    } else if inner_type == 8 {
                        self.pps = Some(nalu.clone());
                    }
                    self.current_frame.push(nalu);
                    offset += size;
                } else {
                    break;
                }
            }
        } else if nal_type == 28 {
            // FU-A fragmented NALU
            if payload.len() >= 2 {
                let indicator = payload[0];
                let header = payload[1];
                let s_bit = (header & 0x80) != 0;
                let e_bit = (header & 0x40) != 0;
                let inner_type = header & 0x1F;

                if s_bit {
                    self.fu_buffer.clear();
                    let reconstructed_header = (indicator & 0xE0) | inner_type;
                    self.fu_buffer.push(reconstructed_header);
                    self.fu_buffer.extend_from_slice(&payload[2..]);
                } else if !self.fu_buffer.is_empty() {
                    self.fu_buffer.extend_from_slice(&payload[2..]);
                }

                if e_bit && !self.fu_buffer.is_empty() {
                    let complete_nalu = std::mem::take(&mut self.fu_buffer);
                    if inner_type == 7 {
                        self.sps = Some(complete_nalu.clone());
                    } else if inner_type == 8 {
                        self.pps = Some(complete_nalu.clone());
                    }
                    self.current_frame.push(complete_nalu);
                }
            }
        }

        // If RTP marker bit is set, the frame is complete
        if packet.marker && !self.current_frame.is_empty() {
            let is_keyframe = self.current_frame.iter().any(|n| !n.is_empty() && (n[0] & 0x1F == 5));
            let nalus = std::mem::take(&mut self.current_frame);
            completed_frame = Some((is_keyframe, packet.timestamp, nalus));
        }

        completed_frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fmp4_init_and_media_segment() {
        let sps = vec![0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x5A, 0x80, 0x80, 0x80, 0x82];
        let pps = vec![0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];

        let (w, h) = parse_h264_sps_dimensions(&sps);
        assert!(w > 0 && h > 0);

        let init = make_h264_init_segment(&sps, &pps, w, h);
        assert!(init.len() > 100);
        // Must start with ftyp
        assert_eq!(&init[4..8], b"ftyp");
        // Must contain moov
        assert!(find_subsequence(&init, b"moov").is_some());

        // Test media segment
        let nalus = vec![vec![0x65, 0x88, 0x84, 0x00, 0x10]];
        let seg = make_h264_media_segment(1, 0, 3000, true, &nalus);
        assert!(seg.len() > 50);
        assert_eq!(&seg[4..8], b"moof");
        assert!(find_subsequence(&seg, b"mdat").is_some());
    }
}
