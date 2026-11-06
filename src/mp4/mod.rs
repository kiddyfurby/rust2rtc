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

#[derive(Debug, Clone)]
pub struct VideoSample {
    pub is_keyframe: bool,
    pub duration: u32,
    pub nalus: Vec<bytes::Bytes>,
}

#[derive(Debug, Clone)]
pub struct AudioSample {
    pub duration: u32,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum AudioCodecConfig {
    Aac {
        channels: u16,
        sample_rate: u32,
        config: Vec<u8>,
    },
    Pcma {
        channels: u16,
        sample_rate: u32,
    },
    Pcmu {
        channels: u16,
        sample_rate: u32,
    },
}

impl AudioCodecConfig {
    pub fn sample_rate(&self) -> u32 {
        match self {
            Self::Aac { sample_rate, .. } => *sample_rate,
            Self::Pcma { sample_rate, .. } => *sample_rate,
            Self::Pcmu { sample_rate, .. } => *sample_rate,
        }
    }

    pub fn channels(&self) -> u16 {
        match self {
            Self::Aac { channels, .. } => *channels,
            Self::Pcma { channels, .. } => *channels,
            Self::Pcmu { channels, .. } => *channels,
        }
    }
}

#[derive(Debug, Clone)]
pub enum VideoCodecConfig {
    H264 {
        sps: Vec<u8>,
        pps: Vec<u8>,
        width: u32,
        height: u32,
    },
    H265 {
        vps: Vec<u8>,
        sps: Vec<u8>,
        pps: Vec<u8>,
        width: u32,
        height: u32,
    },
}

pub fn write_avc1_stsd(writer: &mut BoxWriter, sps: &[u8], pps: &[u8], width: u32, height: u32) {
    let stsd_pos = writer.start_box(b"stsd");
    writer.buf.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0
    writer.buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1

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

    let profile = if sps.len() > 1 { sps[1] } else { 0x64 };
    let compat = if sps.len() > 2 { sps[2] } else { 0x00 };
    let level = if sps.len() > 3 { sps[3] } else { 0x28 };

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
}

pub fn write_hvc1_stsd(writer: &mut BoxWriter, vps: &[u8], sps: &[u8], pps: &[u8], width: u32, height: u32) {
    let stsd_pos = writer.start_box(b"stsd");
    writer.buf.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0
    writer.buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1

    let hvc1_pos = writer.start_box(b"hvc1");
    let mut hvc1_hdr = Vec::with_capacity(70);
    hvc1_hdr.extend_from_slice(&[0u8; 6]);
    hvc1_hdr.extend_from_slice(&1u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&[0u8; 12]);
    hvc1_hdr.extend_from_slice(&(width as u16).to_be_bytes());
    hvc1_hdr.extend_from_slice(&(height as u16).to_be_bytes());
    hvc1_hdr.extend_from_slice(&0x00480000u32.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0x00480000u32.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0u32.to_be_bytes());
    hvc1_hdr.extend_from_slice(&1u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&[0u8; 32]);
    hvc1_hdr.extend_from_slice(&0x0018u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&(-1i16).to_be_bytes());
    writer.buf.extend_from_slice(&hvc1_hdr);

    let sps_rbsp = unescape_rbsp(if sps.len() >= 2 { &sps[2..] } else { sps });
    let mut hvcc = vec![0u8; 23];
    hvcc[0] = 1;
    if sps_rbsp.len() >= 13 {
        hvcc[1..13].copy_from_slice(&sps_rbsp[1..13]);
    } else {
        hvcc[1] = 0x01;
        hvcc[2] = 0x60;
        hvcc[12] = 150;
    }
    hvcc[13] = 0xF0;
    hvcc[14] = 0x00;
    hvcc[15] = 0xFC;
    hvcc[16] = 0xFD; // chroma 4:2:0
    hvcc[17] = 0xF8;
    hvcc[18] = 0xF8;
    hvcc[19] = 0x00;
    hvcc[20] = 0x00;
    hvcc[21] = 0x0F;
    hvcc[22] = 0x03;

    for (nal_type, nalu) in [(32u8, vps), (33u8, sps), (34u8, pps)] {
        hvcc.push(0x80 | (nal_type & 0x3F));
        hvcc.extend_from_slice(&1u16.to_be_bytes());
        hvcc.extend_from_slice(&(nalu.len() as u16).to_be_bytes());
        hvcc.extend_from_slice(nalu);
    }
    writer.write_box(b"hvcC", &hvcc);

    writer.end_box(hvc1_pos);
    writer.end_box(stsd_pos);
}

pub fn write_audio_stsd(writer: &mut BoxWriter, codec: &AudioCodecConfig) {
    let stsd_pos = writer.start_box(b"stsd");
    writer.buf.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0
    writer.buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1

    match codec {
        AudioCodecConfig::Aac {
            channels,
            sample_rate,
            config,
        } => {
            let mp4a_pos = writer.start_box(b"mp4a");
            let mut mp4a_hdr = Vec::with_capacity(28);
            mp4a_hdr.extend_from_slice(&[0u8; 6]); // reserved
            mp4a_hdr.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index = 1
            mp4a_hdr.extend_from_slice(&[0u8; 8]); // reserved
            mp4a_hdr.extend_from_slice(&channels.to_be_bytes()); // channelcount
            mp4a_hdr.extend_from_slice(&16u16.to_be_bytes()); // samplesize = 16
            mp4a_hdr.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
            mp4a_hdr.extend_from_slice(&0u16.to_be_bytes()); // reserved
            mp4a_hdr.extend_from_slice(&(*sample_rate << 16).to_be_bytes()); // samplerate (16.16)
            writer.buf.extend_from_slice(&mp4a_hdr);

            // 'esds' box
            let esds_pos = writer.start_box(b"esds");
            writer.buf.extend_from_slice(&[0, 0, 0, 0]); // version 0, flags 0

            // DecoderSpecificInfo payload (tag 0x05)
            let mut dsi = Vec::with_capacity(2 + config.len());
            dsi.push(0x05);
            dsi.push(config.len() as u8);
            dsi.extend_from_slice(config);

            // DecoderConfigDescriptor payload (tag 0x04)
            let dcd_payload_len = 13 + dsi.len();
            let mut dcd = Vec::with_capacity(2 + dcd_payload_len);
            dcd.push(0x04);
            dcd.push(dcd_payload_len as u8);
            dcd.push(0x40); // Audio ISO/IEC 14496-3 (AAC)
            dcd.push(0x15); // AudioStream
            dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
            dcd.extend_from_slice(&0u32.to_be_bytes()); // maxBitrate
            dcd.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate
            dcd.extend_from_slice(&dsi);

            // SLConfigDescriptor (tag 0x06)
            let sl = vec![0x06, 0x01, 0x02];

            // ES_Descriptor (tag 0x03)
            let es_payload_len = 3 + dcd.len() + sl.len();
            let mut es = Vec::with_capacity(2 + es_payload_len);
            es.push(0x03);
            es.push(es_payload_len as u8);
            es.extend_from_slice(&2u16.to_be_bytes()); // ES_ID = 2
            es.push(0); // flags
            es.extend_from_slice(&dcd);
            es.extend_from_slice(&sl);

            writer.buf.extend_from_slice(&es);
            writer.end_box(esds_pos);
            writer.end_box(mp4a_pos);
        }
        AudioCodecConfig::Pcma {
            channels,
            sample_rate,
        } => {
            let alaw_pos = writer.start_box(b"alaw");
            let mut alaw_hdr = Vec::with_capacity(28);
            alaw_hdr.extend_from_slice(&[0u8; 6]);
            alaw_hdr.extend_from_slice(&1u16.to_be_bytes());
            alaw_hdr.extend_from_slice(&[0u8; 8]);
            alaw_hdr.extend_from_slice(&channels.to_be_bytes());
            alaw_hdr.extend_from_slice(&16u16.to_be_bytes());
            alaw_hdr.extend_from_slice(&0u16.to_be_bytes());
            alaw_hdr.extend_from_slice(&0u16.to_be_bytes());
            alaw_hdr.extend_from_slice(&(*sample_rate << 16).to_be_bytes());
            writer.buf.extend_from_slice(&alaw_hdr);
            writer.end_box(alaw_pos);
        }
        AudioCodecConfig::Pcmu {
            channels,
            sample_rate,
        } => {
            let ulaw_pos = writer.start_box(b"ulaw");
            let mut ulaw_hdr = Vec::with_capacity(28);
            ulaw_hdr.extend_from_slice(&[0u8; 6]);
            ulaw_hdr.extend_from_slice(&1u16.to_be_bytes());
            ulaw_hdr.extend_from_slice(&[0u8; 8]);
            ulaw_hdr.extend_from_slice(&channels.to_be_bytes());
            ulaw_hdr.extend_from_slice(&16u16.to_be_bytes());
            ulaw_hdr.extend_from_slice(&0u16.to_be_bytes());
            ulaw_hdr.extend_from_slice(&0u16.to_be_bytes());
            ulaw_hdr.extend_from_slice(&(*sample_rate << 16).to_be_bytes());
            writer.buf.extend_from_slice(&ulaw_hdr);
            writer.end_box(ulaw_pos);
        }
    }

    writer.end_box(stsd_pos);
}

/// Generate a complete, standalone progressive MP4 file (`ftyp` + `moov` + `mdat`)
/// compatible with Frigate, FFmpeg, QuickTime, and browsers.
pub fn build_standalone_mp4(
    codec_config: &VideoCodecConfig,
    samples: &[VideoSample],
    audio: Option<(&AudioCodecConfig, &[AudioSample])>,
) -> Vec<u8> {
    if samples.is_empty() {
        return Vec::new();
    }

    let (width, height) = match codec_config {
        VideoCodecConfig::H264 { width, height, .. } => (*width, *height),
        VideoCodecConfig::H265 { width, height, .. } => (*width, *height),
    };

    let total_video_bytes: usize = samples
        .iter()
        .map(|s| s.nalus.iter().map(|n| 4 + n.len()).sum::<usize>())
        .sum();
    let total_audio_bytes: usize = audio
        .map(|(_, a_samples)| a_samples.iter().map(|s| s.data.len()).sum::<usize>())
        .unwrap_or(0);

    let mut writer = BoxWriter::with_capacity(4096 + total_video_bytes + total_audio_bytes);

    // 1. 'ftyp'
    let mut ftyp = Vec::with_capacity(16);
    ftyp.extend_from_slice(b"isom");
    ftyp.extend_from_slice(&0x00000200u32.to_be_bytes());
    ftyp.extend_from_slice(b"isomiso2mp41");
    writer.write_box(b"ftyp", &ftyp);

    let total_video_duration: u32 = samples.iter().map(|s| s.duration).sum();
    let (total_audio_duration_native, audio_duration_90k) = if let Some((acfg, a_samples)) = audio {
        let total_native: u64 = a_samples.iter().map(|s| s.duration as u64).sum();
        let rate = acfg.sample_rate().max(1) as u64;
        let d90k = ((total_native * 90000) / rate) as u32;
        (total_native as u32, d90k)
    } else {
        (0, 0)
    };

    let movie_duration = total_video_duration.max(audio_duration_90k);
    let next_track_id = if audio.is_some() { 3u32 } else { 2u32 };

    // 2. 'moov'
    let moov_pos = writer.start_box(b"moov");

    let unity_matrix: [u32; 9] = [
        0x00010000, 0, 0,
        0, 0x00010000, 0,
        0, 0, 0x40000000,
    ];

    // 'mvhd'
    let mut mvhd = Vec::with_capacity(96);
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation time
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification time
    mvhd.extend_from_slice(&90000u32.to_be_bytes()); // timescale 90k
    mvhd.extend_from_slice(&movie_duration.to_be_bytes()); // duration
    mvhd.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate 1.0
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // vol 1.0
    mvhd.extend_from_slice(&[0u8; 10]); // reserved
    for val in unity_matrix {
        mvhd.extend_from_slice(&val.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0u8; 24]);
    mvhd.extend_from_slice(&next_track_id.to_be_bytes());
    writer.write_full_box(b"mvhd", 0, 0, &mvhd);

    // ================= TRACK 1: VIDEO =================
    let v_trak_pos = writer.start_box(b"trak");

    // 'tkhd'
    let mut v_tkhd = Vec::with_capacity(80);
    v_tkhd.extend_from_slice(&0u32.to_be_bytes());
    v_tkhd.extend_from_slice(&0u32.to_be_bytes());
    v_tkhd.extend_from_slice(&1u32.to_be_bytes()); // track_id = 1
    v_tkhd.extend_from_slice(&0u32.to_be_bytes());
    v_tkhd.extend_from_slice(&total_video_duration.to_be_bytes());
    v_tkhd.extend_from_slice(&[0u8; 8]);
    v_tkhd.extend_from_slice(&0u16.to_be_bytes());
    v_tkhd.extend_from_slice(&0u16.to_be_bytes());
    v_tkhd.extend_from_slice(&0u16.to_be_bytes());
    v_tkhd.extend_from_slice(&0u16.to_be_bytes());
    for val in unity_matrix {
        v_tkhd.extend_from_slice(&val.to_be_bytes());
    }
    v_tkhd.extend_from_slice(&(width << 16).to_be_bytes());
    v_tkhd.extend_from_slice(&(height << 16).to_be_bytes());
    writer.write_full_box(b"tkhd", 0, 0x07, &v_tkhd);

    // 'mdia'
    let v_mdia_pos = writer.start_box(b"mdia");

    // 'mdhd'
    let mut v_mdhd = Vec::with_capacity(20);
    v_mdhd.extend_from_slice(&0u32.to_be_bytes());
    v_mdhd.extend_from_slice(&0u32.to_be_bytes());
    v_mdhd.extend_from_slice(&90000u32.to_be_bytes());
    v_mdhd.extend_from_slice(&total_video_duration.to_be_bytes());
    v_mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // und
    v_mdhd.extend_from_slice(&0u16.to_be_bytes());
    writer.write_full_box(b"mdhd", 0, 0, &v_mdhd);

    // 'hdlr'
    let mut v_hdlr = Vec::with_capacity(32);
    v_hdlr.extend_from_slice(&0u32.to_be_bytes());
    v_hdlr.extend_from_slice(b"vide");
    v_hdlr.extend_from_slice(&[0u8; 12]);
    v_hdlr.extend_from_slice(b"VideoHandler\0");
    writer.write_full_box(b"hdlr", 0, 0, &v_hdlr);

    // 'minf'
    let v_minf_pos = writer.start_box(b"minf");
    writer.write_full_box(b"vmhd", 0, 1, &[0u8; 8]);

    // 'dinf'
    let v_dinf_pos = writer.start_box(b"dinf");
    let mut dref = Vec::with_capacity(20);
    dref.extend_from_slice(&1u32.to_be_bytes());
    dref.extend_from_slice(&12u32.to_be_bytes());
    dref.extend_from_slice(b"url ");
    dref.extend_from_slice(&[0, 0, 0, 1]);
    writer.write_full_box(b"dref", 0, 0, &dref);
    writer.end_box(v_dinf_pos);

    // 'stbl'
    let v_stbl_pos = writer.start_box(b"stbl");

    // 'stsd'
    match codec_config {
        VideoCodecConfig::H264 {
            sps,
            pps,
            width,
            height,
        } => {
            write_avc1_stsd(&mut writer, sps, pps, *width, *height);
        }
        VideoCodecConfig::H265 {
            vps,
            sps,
            pps,
            width,
            height,
        } => {
            write_hvc1_stsd(&mut writer, vps, sps, pps, *width, *height);
        }
    }

    // 'stts' (video duration entries)
    let mut v_stts_entries: Vec<(u32, u32)> = Vec::new();
    for s in samples {
        if let Some(last) = v_stts_entries.last_mut()
            && last.1 == s.duration {
                last.0 += 1;
                continue;
            }
        v_stts_entries.push((1, s.duration));
    }
    let mut v_stts_payload = Vec::with_capacity(4 + v_stts_entries.len() * 8);
    v_stts_payload.extend_from_slice(&(v_stts_entries.len() as u32).to_be_bytes());
    for (count, delta) in v_stts_entries {
        v_stts_payload.extend_from_slice(&count.to_be_bytes());
        v_stts_payload.extend_from_slice(&delta.to_be_bytes());
    }
    writer.write_full_box(b"stts", 0, 0, &v_stts_payload);

    // 'stss' (keyframes / sync samples)
    let mut keyframes: Vec<u32> = Vec::new();
    for (idx, s) in samples.iter().enumerate() {
        if s.is_keyframe {
            keyframes.push((idx + 1) as u32);
        }
    }
    if keyframes.is_empty() && !samples.is_empty() {
        keyframes.push(1);
    }
    let mut stss_payload = Vec::with_capacity(4 + keyframes.len() * 4);
    stss_payload.extend_from_slice(&(keyframes.len() as u32).to_be_bytes());
    for kf in keyframes {
        stss_payload.extend_from_slice(&kf.to_be_bytes());
    }
    writer.write_full_box(b"stss", 0, 0, &stss_payload);

    // 'stsc' (sample to chunk: 1 sample per chunk)
    let mut v_stsc_payload = Vec::with_capacity(16);
    v_stsc_payload.extend_from_slice(&1u32.to_be_bytes()); // entry count = 1
    v_stsc_payload.extend_from_slice(&1u32.to_be_bytes()); // first chunk = 1
    v_stsc_payload.extend_from_slice(&1u32.to_be_bytes()); // samples per chunk = 1
    v_stsc_payload.extend_from_slice(&1u32.to_be_bytes()); // sample description index = 1
    writer.write_full_box(b"stsc", 0, 0, &v_stsc_payload);

    // 'stsz' (sample sizes)
    let mut v_stsz_payload = Vec::with_capacity(8 + samples.len() * 4);
    v_stsz_payload.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 (variable)
    v_stsz_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for s in samples {
        let s_size: u32 = s.nalus.iter().map(|n| 4 + n.len() as u32).sum();
        v_stsz_payload.extend_from_slice(&s_size.to_be_bytes());
    }
    writer.write_full_box(b"stsz", 0, 0, &v_stsz_payload);

    // 'stco' (chunk offsets placeholder for video)
    let v_stco_pos = writer.buf.len();
    let mut v_stco_payload = vec![0u8; 4 + samples.len() * 4];
    v_stco_payload[0..4].copy_from_slice(&(samples.len() as u32).to_be_bytes());
    writer.write_full_box(b"stco", 0, 0, &v_stco_payload);

    writer.end_box(v_stbl_pos);
    writer.end_box(v_minf_pos);
    writer.end_box(v_mdia_pos);
    writer.end_box(v_trak_pos);

    // ================= TRACK 2: AUDIO (OPTIONAL) =================
    let mut a_stco_pos = None;
    if let Some((acfg, a_samples)) = audio
        && !a_samples.is_empty() {
            let a_trak_pos = writer.start_box(b"trak");

            // 'tkhd' (audio: vol 1.0, width=0, height=0)
            let mut a_tkhd = Vec::with_capacity(80);
            a_tkhd.extend_from_slice(&0u32.to_be_bytes());
            a_tkhd.extend_from_slice(&0u32.to_be_bytes());
            a_tkhd.extend_from_slice(&2u32.to_be_bytes()); // track_id = 2
            a_tkhd.extend_from_slice(&0u32.to_be_bytes());
            a_tkhd.extend_from_slice(&audio_duration_90k.to_be_bytes());
            a_tkhd.extend_from_slice(&[0u8; 8]);
            a_tkhd.extend_from_slice(&0u16.to_be_bytes());
            a_tkhd.extend_from_slice(&0u16.to_be_bytes());
            a_tkhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
            a_tkhd.extend_from_slice(&0u16.to_be_bytes());
            for val in unity_matrix {
                a_tkhd.extend_from_slice(&val.to_be_bytes());
            }
            a_tkhd.extend_from_slice(&0u32.to_be_bytes()); // width = 0
            a_tkhd.extend_from_slice(&0u32.to_be_bytes()); // height = 0
            writer.write_full_box(b"tkhd", 0, 0x07, &a_tkhd);

            // 'mdia'
            let a_mdia_pos = writer.start_box(b"mdia");

            // 'mdhd'
            let mut a_mdhd = Vec::with_capacity(20);
            a_mdhd.extend_from_slice(&0u32.to_be_bytes());
            a_mdhd.extend_from_slice(&0u32.to_be_bytes());
            a_mdhd.extend_from_slice(&acfg.sample_rate().to_be_bytes());
            a_mdhd.extend_from_slice(&total_audio_duration_native.to_be_bytes());
            a_mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // und
            a_mdhd.extend_from_slice(&0u16.to_be_bytes());
            writer.write_full_box(b"mdhd", 0, 0, &a_mdhd);

            // 'hdlr'
            let mut a_hdlr = Vec::with_capacity(32);
            a_hdlr.extend_from_slice(&0u32.to_be_bytes());
            a_hdlr.extend_from_slice(b"soun");
            a_hdlr.extend_from_slice(&[0u8; 12]);
            a_hdlr.extend_from_slice(b"SoundHandler\0");
            writer.write_full_box(b"hdlr", 0, 0, &a_hdlr);

            // 'minf'
            let a_minf_pos = writer.start_box(b"minf");
            // 'smhd'
            writer.write_full_box(b"smhd", 0, 0, &[0u8; 4]);

            // 'dinf'
            let a_dinf_pos = writer.start_box(b"dinf");
            let mut a_dref = Vec::with_capacity(20);
            a_dref.extend_from_slice(&1u32.to_be_bytes());
            a_dref.extend_from_slice(&12u32.to_be_bytes());
            a_dref.extend_from_slice(b"url ");
            a_dref.extend_from_slice(&[0, 0, 0, 1]);
            writer.write_full_box(b"dref", 0, 0, &a_dref);
            writer.end_box(a_dinf_pos);

            // 'stbl'
            let a_stbl_pos = writer.start_box(b"stbl");

            // 'stsd'
            write_audio_stsd(&mut writer, acfg);

            // 'stts' (audio duration entries)
            let mut a_stts_entries: Vec<(u32, u32)> = Vec::new();
            for s in a_samples.iter() {
                if let Some(last) = a_stts_entries.last_mut()
                    && last.1 == s.duration {
                        last.0 += 1;
                        continue;
                    }
                a_stts_entries.push((1, s.duration));
            }
            let mut a_stts_payload = Vec::with_capacity(4 + a_stts_entries.len() * 8);
            a_stts_payload.extend_from_slice(&(a_stts_entries.len() as u32).to_be_bytes());
            for (count, delta) in a_stts_entries {
                a_stts_payload.extend_from_slice(&count.to_be_bytes());
                a_stts_payload.extend_from_slice(&delta.to_be_bytes());
            }
            writer.write_full_box(b"stts", 0, 0, &a_stts_payload);

            // 'stsc' (1 audio sample per chunk)
            let mut a_stsc_payload = Vec::with_capacity(16);
            a_stsc_payload.extend_from_slice(&1u32.to_be_bytes());
            a_stsc_payload.extend_from_slice(&1u32.to_be_bytes());
            a_stsc_payload.extend_from_slice(&1u32.to_be_bytes());
            a_stsc_payload.extend_from_slice(&1u32.to_be_bytes());
            writer.write_full_box(b"stsc", 0, 0, &a_stsc_payload);

            // 'stsz' (audio sample sizes)
            let mut a_stsz_payload = Vec::with_capacity(8 + a_samples.len() * 4);
            a_stsz_payload.extend_from_slice(&0u32.to_be_bytes());
            a_stsz_payload.extend_from_slice(&(a_samples.len() as u32).to_be_bytes());
            for s in a_samples.iter() {
                a_stsz_payload.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
            }
            writer.write_full_box(b"stsz", 0, 0, &a_stsz_payload);

            // 'stco' (chunk offsets placeholder for audio)
            let pos = writer.buf.len();
            let mut a_stco_payload = vec![0u8; 4 + a_samples.len() * 4];
            a_stco_payload[0..4].copy_from_slice(&(a_samples.len() as u32).to_be_bytes());
            writer.write_full_box(b"stco", 0, 0, &a_stco_payload);
            a_stco_pos = Some(pos);

            writer.end_box(a_stbl_pos);
            writer.end_box(a_minf_pos);
            writer.end_box(a_mdia_pos);
            writer.end_box(a_trak_pos);
        }

    writer.end_box(moov_pos);

    // Now moov is closed and its length is fixed!
    let mdat_pos = writer.buf.len();
    let mut current_offset = (mdat_pos + 8) as u32; // 8 bytes for mdat box header

    // Patch video stco offsets
    let v_stco_entries_pos = v_stco_pos + 16;
    for (i, s) in samples.iter().enumerate() {
        let target = v_stco_entries_pos + i * 4;
        writer.buf[target..target + 4].copy_from_slice(&current_offset.to_be_bytes());
        let s_size: u32 = s.nalus.iter().map(|n| 4 + n.len() as u32).sum();
        current_offset += s_size;
    }

    // Patch audio stco offsets if audio is present
    if let (Some(a_pos), Some((_, a_samples))) = (a_stco_pos, audio) {
        let a_stco_entries_pos = a_pos + 16;
        for (i, s) in a_samples.iter().enumerate() {
            let target = a_stco_entries_pos + i * 4;
            writer.buf[target..target + 4].copy_from_slice(&current_offset.to_be_bytes());
            current_offset += s.data.len() as u32;
        }
    }

    // 3. 'mdat'
    let mdat_size = (8 + total_video_bytes + total_audio_bytes) as u32;
    writer.buf.extend_from_slice(&mdat_size.to_be_bytes());
    writer.buf.extend_from_slice(b"mdat");

    // Write video samples
    for s in samples {
        for nalu in &s.nalus {
            writer.buf.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
            writer.buf.extend_from_slice(nalu);
        }
    }

    // Write audio samples
    if let Some((_, a_samples)) = audio {
        for s in a_samples {
            writer.buf.extend_from_slice(&s.data);
        }
    }

    writer.buf
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
    nalus: &[bytes::Bytes],
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
    // Flags: 0x000701
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
    writer.write_full_box(b"trun", 0, 0x000701, &trun);
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

/// Remove emulation prevention bytes (0x00, 0x00, 0x03 -> 0x00, 0x00) from NAL units
pub fn unescape_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if i + 2 < data.len() && data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 3 {
            out.push(0);
            out.push(0);
            i += 3;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// Helper to parse H.265 SPS dimensions and level_idc
pub fn parse_h265_sps(sps: &[u8]) -> (u32, u32, u8) {
    if sps.len() < 3 {
        return (2560, 1440, 150);
    }
    let unescaped = unescape_rbsp(if sps.len() >= 2 { &sps[2..] } else { sps });
    let mut reader = BitReader::new(&unescaped);

    reader.skip_bits(4); // sps_video_parameter_set_id
    let max_sub_layers = reader.read_bits(3).unwrap_or(0);
    reader.skip_bits(1); // sps_temporal_id_nesting_flag

    // profile_tier_level(1, max_sub_layers)
    reader.skip_bits(2 + 1 + 5 + 32 + 48); // profile_space(2), tier_flag(1), profile_idc(5), compat(32), constraints(48)
    let level_idc = reader.read_bits(8).unwrap_or(150) as u8;

    let mut sub_layer_profile_present = Vec::new();
    let mut sub_layer_level_present = Vec::new();
    for _ in 0..max_sub_layers {
        sub_layer_profile_present.push(reader.read_bits(1).unwrap_or(0) != 0);
        sub_layer_level_present.push(reader.read_bits(1).unwrap_or(0) != 0);
    }
    if max_sub_layers > 0 {
        for _ in max_sub_layers..8 {
            reader.skip_bits(2);
        }
    }
    for i in 0..max_sub_layers as usize {
        if sub_layer_profile_present.get(i).copied().unwrap_or(false) {
            reader.skip_bits(2 + 1 + 5 + 32 + 48);
        }
        if sub_layer_level_present.get(i).copied().unwrap_or(false) {
            reader.skip_bits(8);
        }
    }

    let _sps_id = reader.read_ue().unwrap_or(0);
    let chroma_idc = reader.read_ue().unwrap_or(1);
    if chroma_idc == 3 {
        reader.skip_bits(1);
    }

    let mut pic_w = reader.read_ue().unwrap_or(2560);
    let mut pic_h = reader.read_ue().unwrap_or(1440);

    let conf_win = reader.read_bits(1).unwrap_or(0) != 0;
    if conf_win {
        let c_left = reader.read_ue().unwrap_or(0);
        let c_right = reader.read_ue().unwrap_or(0);
        let c_top = reader.read_ue().unwrap_or(0);
        let c_bottom = reader.read_ue().unwrap_or(0);
        let sub_w = if chroma_idc == 1 || chroma_idc == 2 { 2 } else { 1 };
        let sub_h = if chroma_idc == 1 { 2 } else { 1 };
        pic_w = pic_w.saturating_sub((c_left + c_right) * sub_w);
        pic_h = pic_h.saturating_sub((c_top + c_bottom) * sub_h);
    }

    (pic_w.max(320), pic_h.max(240), level_idc)
}

/// Extract raw AAC frame from RFC 3640 AU-header encapsulated RTP payload
pub fn extract_aac_frame(payload: &[u8]) -> &[u8] {
    if payload.len() < 4 {
        return payload;
    }
    let au_headers_len_bits = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let au_headers_len_bytes = au_headers_len_bits.div_ceil(8);
    if au_headers_len_bytes > 0 && payload.len() >= 2 + au_headers_len_bytes {
        let au_header = u16::from_be_bytes([payload[2], payload[3]]);
        let au_size = (au_header >> 3) as usize;
        let data_offset = 2 + au_headers_len_bytes;
        if payload.len() >= data_offset + au_size && au_size > 0 {
            &payload[data_offset..data_offset + au_size]
        } else {
            &payload[data_offset..]
        }
    } else {
        payload
    }
}

/// Parse or generate MPEG-4 AudioSpecificConfig (ASC) bytes
pub fn parse_aac_config(fmtp: Option<&str>, rate: u32, channels: usize) -> Vec<u8> {
    if let Some(f) = fmtp {
        for part in f.split(';') {
            let trimmed = part.trim();
            if let Some(val) = trimmed.strip_prefix("config=")
                && let Ok(bytes) = hex::decode(val.trim())
                    && !bytes.is_empty() {
                        return bytes;
                    }
        }
    }
    generate_asc(rate, channels)
}

fn generate_asc(rate: u32, channels: usize) -> Vec<u8> {
    let rate_idx: u8 = match rate {
        96000 => 0x0,
        88200 => 0x1,
        64000 => 0x2,
        48000 => 0x3,
        44100 => 0x4,
        32000 => 0x5,
        24000 => 0x6,
        22050 => 0x7,
        16000 => 0x8,
        12000 => 0x9,
        11025 => 0xA,
        8000 => 0xB,
        7350 => 0xC,
        _ => 0x4,
    };
    let ch = (channels as u8).clamp(1, 8);
    let byte0 = (2 << 3) | (rate_idx >> 1);
    let byte1 = ((rate_idx & 1) << 7) | (ch << 3);
    vec![byte0, byte1]
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
        Some(sign * val.div_ceil(2) as i32)
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// H.264 RTP Depacketizer: reconstructs complete NALUs from single NAL, STAP-A, and FU-A RTP packets
pub struct H264RtpDepacketizer {
    fu_buffer: Vec<u8>,
    current_frame: Vec<bytes::Bytes>,
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

    pub fn reset(&mut self) {
        self.fu_buffer.clear();
        self.current_frame.clear();
        self.last_timestamp = None;
    }

    /// Feed an RTP packet; returns (is_keyframe, timestamp, nalus) when a complete frame is finished
    pub fn feed_packet(&mut self, packet: &crate::core::RtpPacket) -> Option<(bool, u32, Vec<bytes::Bytes>)> {
        let payload = packet.payload();
        if payload.is_empty() {
            return None;
        }

        let mut completed_frame = None;

        // Check if timestamp changed (new frame boundary)
        if let Some(last_ts) = self.last_timestamp
            && last_ts != packet.timestamp && !self.current_frame.is_empty() {
                let is_keyframe = self.current_frame.iter().any(|n| !n.is_empty() && (n[0] & 0x1F == 5));
                let nalus = std::mem::take(&mut self.current_frame);
                completed_frame = Some((is_keyframe, last_ts, nalus));
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
            let nalu = packet.raw.slice(packet.payload_offset..packet.payload_offset + packet.payload_len);
            self.current_frame.push(nalu);
        } else if nal_type == 24 {
            // STAP-A packet (aggregates multiple NALUs)
            let mut offset = 1;
            while offset + 2 <= payload.len() {
                let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;
                if offset + size <= payload.len() {
                    let nalu = packet.raw.slice(packet.payload_offset + offset..packet.payload_offset + offset + size);
                    let inner_type = nalu[0] & 0x1F;
                    if inner_type == 7 {
                        self.sps = Some(nalu.to_vec());
                    } else if inner_type == 8 {
                        self.pps = Some(nalu.to_vec());
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
                    let complete_nalu = bytes::Bytes::from(std::mem::take(&mut self.fu_buffer));
                    if inner_type == 7 {
                        self.sps = Some(complete_nalu.to_vec());
                    } else if inner_type == 8 {
                        self.pps = Some(complete_nalu.to_vec());
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

/// Generate ISO BMFF Initialization Segment (`ftyp` + `moov`) for H.265 / HEVC (`hvc1` + `hvcC`)
pub fn make_h265_init_segment(
    vps: &[u8],
    sps: &[u8],
    pps: &[u8],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let mut writer = BoxWriter::with_capacity(1024);

    // 1. 'ftyp' box
    let mut ftyp = Vec::with_capacity(16);
    ftyp.extend_from_slice(b"isom");
    ftyp.extend_from_slice(&0x00000200u32.to_be_bytes());
    ftyp.extend_from_slice(b"isomiso6mp41");
    writer.write_box(b"ftyp", &ftyp);

    // 2. 'moov' box
    let moov_pos = writer.start_box(b"moov");

    let unity_matrix: [u32; 9] = [
        0x00010000, 0, 0,
        0, 0x00010000, 0,
        0, 0, 0x40000000,
    ];

    let mut mvhd = Vec::with_capacity(96);
    mvhd.extend_from_slice(&0u32.to_be_bytes());
    mvhd.extend_from_slice(&0u32.to_be_bytes());
    mvhd.extend_from_slice(&90000u32.to_be_bytes());
    mvhd.extend_from_slice(&0u32.to_be_bytes());
    mvhd.extend_from_slice(&0x00010000u32.to_be_bytes());
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 10]);
    for val in unity_matrix {
        mvhd.extend_from_slice(&val.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0u8; 24]);
    mvhd.extend_from_slice(&2u32.to_be_bytes());
    writer.write_full_box(b"mvhd", 0, 0, &mvhd);

    let trak_pos = writer.start_box(b"trak");

    let mut tkhd = Vec::with_capacity(80);
    tkhd.extend_from_slice(&0u32.to_be_bytes());
    tkhd.extend_from_slice(&0u32.to_be_bytes());
    tkhd.extend_from_slice(&1u32.to_be_bytes());
    tkhd.extend_from_slice(&0u32.to_be_bytes());
    tkhd.extend_from_slice(&0u32.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend_from_slice(&0u16.to_be_bytes());
    tkhd.extend_from_slice(&0u16.to_be_bytes());
    tkhd.extend_from_slice(&0u16.to_be_bytes());
    tkhd.extend_from_slice(&0u16.to_be_bytes());
    for val in unity_matrix {
        tkhd.extend_from_slice(&val.to_be_bytes());
    }
    tkhd.extend_from_slice(&(width << 16).to_be_bytes());
    tkhd.extend_from_slice(&(height << 16).to_be_bytes());
    writer.write_full_box(b"tkhd", 0, 0x07, &tkhd);

    let mdia_pos = writer.start_box(b"mdia");

    let mut mdhd = Vec::with_capacity(20);
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&90000u32.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&0x55C4u16.to_be_bytes());
    mdhd.extend_from_slice(&0u16.to_be_bytes());
    writer.write_full_box(b"mdhd", 0, 0, &mdhd);

    let mut hdlr = Vec::with_capacity(32);
    hdlr.extend_from_slice(&0u32.to_be_bytes());
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(b"VideoHandler\0");
    writer.write_full_box(b"hdlr", 0, 0, &hdlr);

    let minf_pos = writer.start_box(b"minf");
    writer.write_full_box(b"vmhd", 0, 1, &[0u8; 8]);

    let dinf_pos = writer.start_box(b"dinf");
    let mut dref = Vec::with_capacity(20);
    dref.extend_from_slice(&1u32.to_be_bytes());
    dref.extend_from_slice(&12u32.to_be_bytes());
    dref.extend_from_slice(b"url ");
    dref.extend_from_slice(&[0, 0, 0, 1]);
    writer.write_full_box(b"dref", 0, 0, &dref);
    writer.end_box(dinf_pos);

    let stbl_pos = writer.start_box(b"stbl");

    let stsd_pos = writer.start_box(b"stsd");
    writer.buf.extend_from_slice(&[0, 0, 0, 0]);
    writer.buf.extend_from_slice(&1u32.to_be_bytes());

    // 'hvc1' box
    let hvc1_pos = writer.start_box(b"hvc1");
    let mut hvc1_hdr = Vec::with_capacity(70);
    hvc1_hdr.extend_from_slice(&[0u8; 6]);
    hvc1_hdr.extend_from_slice(&1u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&[0u8; 12]);
    hvc1_hdr.extend_from_slice(&(width as u16).to_be_bytes());
    hvc1_hdr.extend_from_slice(&(height as u16).to_be_bytes());
    hvc1_hdr.extend_from_slice(&0x00480000u32.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0x00480000u32.to_be_bytes());
    hvc1_hdr.extend_from_slice(&0u32.to_be_bytes());
    hvc1_hdr.extend_from_slice(&1u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&[0u8; 32]);
    hvc1_hdr.extend_from_slice(&0x0018u16.to_be_bytes());
    hvc1_hdr.extend_from_slice(&(-1i16).to_be_bytes());
    writer.buf.extend_from_slice(&hvc1_hdr);

    // 'hvcC' box (HEVCDecoderConfigurationRecord, ISO/IEC 14496-15)
    // Remove emulation prevention bytes from SPS RBSP before extracting profile_tier_level
    let sps_rbsp = unescape_rbsp(if sps.len() >= 2 { &sps[2..] } else { sps });

    let mut hvcc = vec![0u8; 23];
    hvcc[0] = 1; // configurationVersion = 1
    if sps_rbsp.len() >= 13 {
        // sps_rbsp[1..13]: profile_tier_level (12 bytes: profile_space/tier/profile_idc, 32-bit compat, 48-bit constraints, 8-bit level_idc)
        hvcc[1..13].copy_from_slice(&sps_rbsp[1..13]);
    } else {
        hvcc[1] = 0x01; // Main profile
        hvcc[2] = 0x60;
        hvcc[12] = 150; // level 5.0 default
    }
    hvcc[13] = 0xF0;
    hvcc[14] = 0x00;
    hvcc[15] = 0xFC;
    hvcc[16] = 0xFD; // chromaFormat = 1 (4:2:0)
    hvcc[17] = 0xF8; // 8-bit luma
    hvcc[18] = 0xF8; // 8-bit chroma
    hvcc[19] = 0x00;
    hvcc[20] = 0x00;
    hvcc[21] = 0x0F; // bit(2) constantFrameRate=0, bit(3) numTemporalLayers=1, bit(1) temporalIdNested=1, lengthSizeMinusOne=3
    hvcc[22] = 0x03; // numOfArrays = 3 (VPS, SPS, PPS)

    for (nal_type, nalu) in [(32u8, vps), (33u8, sps), (34u8, pps)] {
        hvcc.push(0x80 | (nal_type & 0x3F));
        hvcc.extend_from_slice(&1u16.to_be_bytes());
        hvcc.extend_from_slice(&(nalu.len() as u16).to_be_bytes());
        hvcc.extend_from_slice(nalu);
    }
    writer.write_box(b"hvcC", &hvcc);

    writer.end_box(hvc1_pos);
    writer.end_box(stsd_pos);

    writer.write_full_box(b"stts", 0, 0, &[0u8; 4]);
    writer.write_full_box(b"stsc", 0, 0, &[0u8; 4]);
    writer.write_full_box(b"stsz", 0, 0, &[0u8; 8]);
    writer.write_full_box(b"stco", 0, 0, &[0u8; 4]);

    writer.end_box(stbl_pos);
    writer.end_box(minf_pos);
    writer.end_box(mdia_pos);
    writer.end_box(trak_pos);

    let mvex_pos = writer.start_box(b"mvex");
    let mut trex = Vec::with_capacity(24);
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&3000u32.to_be_bytes());
    trex.extend_from_slice(&0u32.to_be_bytes());
    trex.extend_from_slice(&0u32.to_be_bytes());
    writer.write_full_box(b"trex", 0, 0, &trex);
    writer.end_box(mvex_pos);

    writer.end_box(moov_pos);

    writer.buf
}

/// H.265 RTP Depacketizer: reconstructs complete NALUs from single NAL, AP (48), and FU (49) RTP packets
pub struct H265RtpDepacketizer {
    fu_buffer: Vec<u8>,
    current_frame: Vec<bytes::Bytes>,
    last_timestamp: Option<u32>,
    pub vps: Option<Vec<u8>>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

impl H265RtpDepacketizer {
    pub fn new() -> Self {
        Self {
            fu_buffer: Vec::with_capacity(65536),
            current_frame: Vec::new(),
            last_timestamp: None,
            vps: None,
            sps: None,
            pps: None,
        }
    }

    pub fn reset(&mut self) {
        self.fu_buffer.clear();
        self.current_frame.clear();
        self.last_timestamp = None;
    }

    fn cache_param_set(&mut self, nalu: &[u8]) {
        if nalu.len() >= 2 {
            let nal_type = (nalu[0] >> 1) & 0x3F;
            match nal_type {
                32 => self.vps = Some(nalu.to_vec()),
                33 => self.sps = Some(nalu.to_vec()),
                34 => self.pps = Some(nalu.to_vec()),
                _ => {}
            }
        }
    }

    pub fn feed_packet(&mut self, packet: &crate::core::RtpPacket) -> Option<(bool, u32, Vec<bytes::Bytes>)> {
        let payload = packet.payload();
        if payload.len() < 2 {
            return None;
        }

        let mut completed_frame = None;

        if let Some(last_ts) = self.last_timestamp
            && last_ts != packet.timestamp && !self.current_frame.is_empty() {
                let is_keyframe = self.current_frame.iter().any(|n| {
                    !n.is_empty() && (19..=21).contains(&((n[0] >> 1) & 0x3F))
                });
                let nalus = std::mem::take(&mut self.current_frame);
                completed_frame = Some((is_keyframe, last_ts, nalus));
            }
        self.last_timestamp = Some(packet.timestamp);

        let nal_type = (payload[0] >> 1) & 0x3F;

        if nal_type < 48 {
            self.cache_param_set(payload);
            let nalu = packet.raw.slice(packet.payload_offset..packet.payload_offset + packet.payload_len);
            self.current_frame.push(nalu);
        } else if nal_type == 48 {
            // Aggregation Packet (AP)
            let mut offset = 2;
            while offset + 2 <= payload.len() {
                let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
                offset += 2;
                if size > 0 && offset + size <= payload.len() {
                    let nalu = packet.raw.slice(packet.payload_offset + offset..packet.payload_offset + offset + size);
                    self.cache_param_set(&nalu);
                    self.current_frame.push(nalu);
                    offset += size;
                } else {
                    break;
                }
            }
        } else if nal_type == 49 && payload.len() >= 3 {
            // Fragmentation Unit (FU)
            let h0 = payload[0];
            let h1 = payload[1];
            let fu_header = payload[2];
            let s_bit = (fu_header & 0x80) != 0;
            let e_bit = (fu_header & 0x40) != 0;
            let inner_type = fu_header & 0x3F;

            if s_bit {
                self.fu_buffer.clear();
                let reconstructed_h0 = (h0 & 0x81) | (inner_type << 1);
                self.fu_buffer.push(reconstructed_h0);
                self.fu_buffer.push(h1);
                self.fu_buffer.extend_from_slice(&payload[3..]);
            } else if !self.fu_buffer.is_empty() {
                self.fu_buffer.extend_from_slice(&payload[3..]);
            }

            if e_bit && !self.fu_buffer.is_empty() {
                let complete_nalu = bytes::Bytes::from(std::mem::take(&mut self.fu_buffer));
                self.cache_param_set(&complete_nalu);
                self.current_frame.push(complete_nalu);
            }
        }

        if packet.marker && !self.current_frame.is_empty() {
            let is_keyframe = self.current_frame.iter().any(|n| {
                !n.is_empty() && (19..=21).contains(&((n[0] >> 1) & 0x3F))
            });
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
    fn test_living2_h265_sps() {
        // living2 raw SPS with emulation prevention bytes (from camera SDP)
        let sps = vec![
            66, 1, 1, 1, 96, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 150, 160, 1, 64, 32, 5, 161,
            99, 107, 146, 76, 154, 229, 156, 2, 0, 0, 3, 0, 2, 0, 0, 3, 0, 30, 16,
        ];
        let (w, h, level) = parse_h265_sps(&sps);
        assert_eq!((w, h), (2560, 1440));
        assert_eq!(level, 150);

        let vps = vec![64, 1, 12, 1, 255, 255, 1, 96, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 150, 172, 9];
        let pps = vec![68, 1, 224, 118, 176, 38, 64];
        let init = make_h265_init_segment(&vps, &sps, &pps, w, h);
        assert!(init.len() > 100);

        // Verify hvcC box contains unescaped profile_tier_level with general_level_idc = 150
        let hvcc_idx = find_subsequence(&init, b"hvcC").expect("hvcC box must exist");
        let hvcc_data = &init[hvcc_idx + 4..hvcc_idx + 4 + 23];
        assert_eq!(hvcc_data[0], 1); // configurationVersion = 1
        assert_eq!(hvcc_data[12], 150); // general_level_idc = 150
        assert_eq!(hvcc_data[21], 0x0F); // lengthSizeMinusOne=3, numTemporalLayers=1
    }

    #[test]
    fn test_doorbell_sps() {
        let sps = vec![0x67, 0x64, 0x00, 0x33, 0xac, 0x15, 0x14, 0xa0, 0x28, 0x00, 0xf1, 0xa1, 0x00, 0x00, 0x04, 0xf6, 0x00, 0x00, 0x94, 0xd4, 0x04, 0x00];
        let pps = vec![0x68, 0xee, 0x3c, 0xb0];
        let (w, h) = parse_h264_sps_dimensions(&sps);
        println!("Doorbell parsed resolution: {}x{}", w, h);
        assert_eq!((w, h), (2560, 1920));
        let init = make_h264_init_segment(&sps, &pps, w, h);
        println!("Init segment len: {}", init.len());
    }

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
        let nalus = vec![bytes::Bytes::from_static(&[0x65, 0x88, 0x84, 0x00, 0x10])];
        let seg = make_h264_media_segment(1, 0, 3000, true, &nalus);
        assert!(seg.len() > 50);
        assert_eq!(&seg[4..8], b"moof");
        assert!(find_subsequence(&seg, b"mdat").is_some());

        // Verify trun flags: version 0, flags 0x000701 (0x00, 0x07, 0x01)
        let trun_idx = find_subsequence(&seg, b"trun").expect("trun box must exist");
        assert_eq!(&seg[trun_idx + 4..trun_idx + 8], &[0x00, 0x00, 0x07, 0x01]);
    }

    #[test]
    fn test_standalone_mp4_h264_and_h265() {
        let sps = vec![0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9, 0x40, 0x78, 0x02, 0x27, 0xE5, 0xC0, 0x5A, 0x80, 0x80, 0x80, 0x82];
        let pps = vec![0x68, 0xEB, 0xE3, 0xCB, 0x22, 0xC0];
        let (w, h) = parse_h264_sps_dimensions(&sps);

        let samples = vec![
            VideoSample {
                is_keyframe: true,
                duration: 6000,
                nalus: vec![bytes::Bytes::from_static(&[0x65, 0x88, 0x84, 0x00, 0x10])],
            },
            VideoSample {
                is_keyframe: false,
                duration: 6000,
                nalus: vec![bytes::Bytes::from_static(&[0x41, 0x9a, 0x12, 0x34])],
            },
        ];

        let mp4_h264 = build_standalone_mp4(
            &VideoCodecConfig::H264 {
                sps: sps.clone(),
                pps: pps.clone(),
                width: w,
                height: h,
            },
            &samples,
            None,
        );

        assert!(mp4_h264.len() > 100);
        assert_eq!(&mp4_h264[4..8], b"ftyp");
        assert!(find_subsequence(&mp4_h264, b"moov").is_some());
        assert!(find_subsequence(&mp4_h264, b"avc1").is_some());
        assert!(find_subsequence(&mp4_h264, b"stco").is_some());
        assert!(find_subsequence(&mp4_h264, b"mdat").is_some());

        // Test H.265
        let vps = vec![64, 1, 12, 1, 255, 255, 1, 96, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 150, 172, 9];
        let h265_sps = vec![
            66, 1, 1, 1, 96, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 150, 160, 1, 64, 32, 5, 161,
            99, 107, 146, 76, 154, 229, 156, 2, 0, 0, 3, 0, 2, 0, 0, 3, 0, 30, 16,
        ];
        let h265_pps = vec![68, 1, 224, 118, 176, 38, 64];
        let (h265_w, h265_h, _) = parse_h265_sps(&h265_sps);

        let mp4_h265 = build_standalone_mp4(
            &VideoCodecConfig::H265 {
                vps,
                sps: h265_sps,
                pps: h265_pps,
                width: h265_w,
                height: h265_h,
            },
            &samples,
            None,
        );

        assert!(mp4_h265.len() > 100);
        assert_eq!(&mp4_h265[4..8], b"ftyp");
        assert!(find_subsequence(&mp4_h265, b"moov").is_some());
        assert!(find_subsequence(&mp4_h265, b"hvc1").is_some());
        assert!(find_subsequence(&mp4_h265, b"stco").is_some());
        assert!(find_subsequence(&mp4_h265, b"mdat").is_some());

        // Test Video + AAC Audio Multiplexing
        let aac_config = AudioCodecConfig::Aac {
            channels: 1,
            sample_rate: 16000,
            config: vec![0x14, 0x08],
        };
        let aac_samples = vec![
            AudioSample {
                duration: 1024,
                data: vec![0x21, 0x00, 0x49, 0x90],
            },
            AudioSample {
                duration: 1024,
                data: vec![0x21, 0x00, 0x49, 0x91],
            },
        ];

        let mp4_aac = build_standalone_mp4(
            &VideoCodecConfig::H264 {
                sps: sps.clone(),
                pps: pps.clone(),
                width: w,
                height: h,
            },
            &samples,
            Some((&aac_config, &aac_samples)),
        );

        assert!(mp4_aac.len() > 100);
        assert!(find_subsequence(&mp4_aac, b"vide").is_some());
        assert!(find_subsequence(&mp4_aac, b"soun").is_some());
        assert!(find_subsequence(&mp4_aac, b"mp4a").is_some());
        assert!(find_subsequence(&mp4_aac, b"esds").is_some());

        // Test Video + PCMA Audio Multiplexing
        let pcma_config = AudioCodecConfig::Pcma {
            channels: 1,
            sample_rate: 8000,
        };
        let pcma_samples = vec![
            AudioSample {
                duration: 160,
                data: vec![0xD5; 160],
            },
            AudioSample {
                duration: 160,
                data: vec![0xD5; 160],
            },
        ];

        let mp4_pcma = build_standalone_mp4(
            &VideoCodecConfig::H265 {
                vps: vec![64, 1, 12, 1, 255, 255, 1, 96, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 150, 172, 9],
                sps: vec![
                    66, 1, 1, 1, 96, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 0, 3, 0, 150, 160, 1, 64, 32, 5, 161,
                    99, 107, 146, 76, 154, 229, 156, 2, 0, 0, 3, 0, 2, 0, 0, 3, 0, 30, 16,
                ],
                pps: vec![68, 1, 224, 118, 176, 38, 64],
                width: h265_w,
                height: h265_h,
            },
            &samples,
            Some((&pcma_config, &pcma_samples)),
        );

        assert!(mp4_pcma.len() > 100);
        assert!(find_subsequence(&mp4_pcma, b"vide").is_some());
        assert!(find_subsequence(&mp4_pcma, b"soun").is_some());
        assert!(find_subsequence(&mp4_pcma, b"alaw").is_some());
    }
}
