#![allow(dead_code)]

use base64::Engine;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

pub fn generate_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaKind {
    Video,
    Audio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    SendOnly,
    RecvOnly,
    SendRecv,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Codec {
    pub name: String,
    pub clock_rate: u32,
    pub channels: Option<u8>,
    pub payload_type: u8,
    pub fmtp: Option<String>,
}

impl Codec {
    pub fn new_h264() -> Self {
        Self {
            name: "H264".to_string(),
            clock_rate: 90000,
            channels: None,
            payload_type: 96,
            fmtp: Some("packetization-mode=1;profile-level-id=42e01f".to_string()),
        }
    }

    pub fn new_h265() -> Self {
        Self {
            name: "H265".to_string(),
            clock_rate: 90000,
            channels: None,
            payload_type: 97,
            fmtp: None,
        }
    }

    pub fn new_opus() -> Self {
        Self {
            name: "OPUS".to_string(),
            clock_rate: 48000,
            channels: Some(2),
            payload_type: 111,
            fmtp: Some("minptime=10;useinbandfec=1".to_string()),
        }
    }

    pub fn new_pcmu() -> Self {
        Self {
            name: "PCMU".to_string(),
            clock_rate: 8000,
            channels: Some(1),
            payload_type: 0,
            fmtp: None,
        }
    }

    pub fn new_pcma() -> Self {
        Self {
            name: "PCMA".to_string(),
            clock_rate: 8000,
            channels: Some(1),
            payload_type: 8,
            fmtp: None,
        }
    }

    pub fn new_aac() -> Self {
        Self {
            name: "MPEG4-GENERIC".to_string(),
            clock_rate: 16000,
            channels: Some(1),
            payload_type: 98,
            fmtp: Some("streamtype=5;profile-level-id=1;mode=AAC-hbr;sizelength=13;indexlength=3;indexdeltalength=3;config=1408".to_string()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub kind: MediaKind,
    pub direction: Direction,
    pub codecs: Vec<Codec>,
    pub control: String,
}

/// Lightweight, zero-copy ref-counted RTP packet
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub marker: bool,
    pub raw: Bytes,
    pub payload_offset: usize,
    pub payload_len: usize,
}

impl RtpPacket {
    /// Parse an RTP packet from raw bytes
    pub fn parse(raw: Bytes) -> Option<Self> {
        if raw.len() < 12 {
            return None;
        }

        let first = raw[0];
        let version = (first >> 6) & 0x03;
        if version != 2 {
            return None;
        }

        let has_padding = (first & 0x20) != 0;
        let has_extension = (first & 0x10) != 0;
        let csrc_count = (first & 0x0f) as usize;

        let second = raw[1];
        let marker = (second & 0x80) != 0;
        let payload_type = second & 0x7f;

        let sequence_number = u16::from_be_bytes([raw[2], raw[3]]);
        let timestamp = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
        let ssrc = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]);

        let mut offset = 12 + csrc_count * 4;
        if raw.len() < offset {
            return None;
        }

        if has_extension {
            if raw.len() < offset + 4 {
                return None;
            }
            let ext_len = u16::from_be_bytes([raw[offset + 2], raw[offset + 3]]) as usize * 4;
            offset += 4 + ext_len;
            if raw.len() < offset {
                return None;
            }
        }

        let mut payload_len = raw.len() - offset;
        if has_padding && payload_len > 0 {
            let padding_len = raw[raw.len() - 1] as usize;
            if padding_len <= payload_len {
                payload_len -= padding_len;
            }
        }

        Some(Self {
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            marker,
            raw,
            payload_offset: offset,
            payload_len,
        })
    }

    pub fn payload(&self) -> &[u8] {
        &self.raw[self.payload_offset..self.payload_offset + self.payload_len]
    }

    /// Construct a valid RFC 3550 RTP packet
    pub fn create(payload_type: u8, sequence_number: u16, timestamp: u32, ssrc: u32, marker: bool, payload: &[u8]) -> Self {
        let mut raw = Vec::with_capacity(12 + payload.len());
        // Byte 0: V=2, P=0, X=0, CC=0
        raw.push(0x80);
        // Byte 1: Marker bit + 7-bit payload type
        raw.push(if marker { 0x80 | (payload_type & 0x7f) } else { payload_type & 0x7f });
        // Bytes 2..4: Sequence number
        raw.extend_from_slice(&sequence_number.to_be_bytes());
        // Bytes 4..8: Timestamp
        raw.extend_from_slice(&timestamp.to_be_bytes());
        // Bytes 8..12: SSRC
        raw.extend_from_slice(&ssrc.to_be_bytes());
        // Payload
        raw.extend_from_slice(payload);

        let raw_bytes = Bytes::from(raw);
        Self {
            payload_type,
            sequence_number,
            timestamp,
            ssrc,
            marker,
            raw: raw_bytes,
            payload_offset: 12,
            payload_len: payload.len(),
        }
    }
}

/// Cached parameter sets for video tracks (SPS, PPS, VPS)
#[derive(Debug, Default, Clone)]
pub struct ParamSets {
    pub h264_sps: Option<Vec<u8>>,
    pub h264_pps: Option<Vec<u8>>,
    pub h265_vps: Option<Vec<u8>>,
    pub h265_sps: Option<Vec<u8>>,
    pub h265_pps: Option<Vec<u8>>,
    pub rtp_packets: Vec<RtpPacket>,
}

#[derive(Debug, Default, Clone)]
struct GopCache {
    packets: Vec<RtpPacket>,
    bytes: usize,
}

/// Track broadcaster holding a multi-consumer channel
#[derive(Debug)]
pub struct Track {
    pub codec: Codec,
    pub kind: MediaKind,
    pub is_hevc: bool,
    tx: broadcast::Sender<RtpPacket>,
    pub packets_total: Arc<AtomicU64>,
    pub bytes_total: Arc<AtomicU64>,
    param_sets: RwLock<ParamSets>,
    gop_cache: RwLock<GopCache>,
}

impl Track {
    pub const DEFAULT_VIDEO_CAPACITY: usize = 512;
    pub const DEFAULT_AUDIO_CAPACITY: usize = 256;
    pub const MAX_GOP_BYTES: usize = 2 * 1024 * 1024; // 2 MB byte-bound
    pub const MAX_GOP_PACKETS: usize = 512;

    pub fn new(codec: Codec, kind: MediaKind, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        let mut param_sets = ParamSets::default();
        let is_hevc = kind == MediaKind::Video
            && (codec.name.eq_ignore_ascii_case("H265") || codec.name.eq_ignore_ascii_case("HEVC"));

        if kind == MediaKind::Video {
            if let Some(ref fmtp) = codec.fmtp {
                if !is_hevc {
                    if let Some((sps, pps)) = extract_h264_sps_pps(fmtp) {
                        let stap_a = build_h264_stap_a(&sps, &pps);
                        let rtp = RtpPacket::create(codec.payload_type, 1, 0, 0x12345678, false, &stap_a);
                        param_sets.h264_sps = Some(sps);
                        param_sets.h264_pps = Some(pps);
                        param_sets.rtp_packets = vec![rtp];
                    }
                } else if let Some((vps, sps, pps)) = extract_h265_vps_sps_pps(fmtp) {
                    let p1 = RtpPacket::create(codec.payload_type, 1, 0, 0x12345678, false, &vps);
                    let p2 = RtpPacket::create(codec.payload_type, 2, 0, 0x12345678, false, &sps);
                    let p3 = RtpPacket::create(codec.payload_type, 3, 0, 0x12345678, false, &pps);
                    param_sets.h265_vps = Some(vps);
                    param_sets.h265_sps = Some(sps);
                    param_sets.h265_pps = Some(pps);
                    param_sets.rtp_packets = vec![p1, p2, p3];
                }
            }
        }

        Self {
            codec,
            kind,
            is_hevc,
            tx,
            packets_total: Arc::new(AtomicU64::new(0)),
            bytes_total: Arc::new(AtomicU64::new(0)),
            param_sets: RwLock::new(param_sets),
            gop_cache: RwLock::new(GopCache::default()),
        }
    }

    pub fn send(&self, packet: RtpPacket) {
        self.packets_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_total.fetch_add(packet.raw.len() as u64, Ordering::Relaxed);

        if self.kind == MediaKind::Video {
            let payload = packet.payload();
            let (is_keyframe, is_param_set) = is_keyframe_or_param_set(self.is_hevc, payload);

            if is_param_set {
                self.inspect_and_cache_params(&packet);
            }

            let mut gop = self.gop_cache.write().unwrap();
            let pkt_len = packet.raw.len();
            if is_keyframe {
                gop.packets.clear();
                gop.bytes = 0;
                let ps = self.param_sets.read().unwrap();
                for ps_pkt in &ps.rtp_packets {
                    gop.bytes += ps_pkt.raw.len();
                    gop.packets.push(ps_pkt.clone());
                }
                gop.bytes += pkt_len;
                gop.packets.push(packet.clone());
            } else if !gop.packets.is_empty() {
                if gop.packets.len() < Self::MAX_GOP_PACKETS && gop.bytes + pkt_len <= Self::MAX_GOP_BYTES {
                    gop.bytes += pkt_len;
                    gop.packets.push(packet.clone());
                }
            }
        }

        let _ = self.tx.send(packet);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RtpPacket> {
        self.tx.subscribe()
    }

    pub fn consumer_count(&self) -> usize {
        self.tx.receiver_count()
    }

    pub fn get_param_sets(&self) -> Vec<RtpPacket> {
        self.param_sets.read().unwrap().rtp_packets.clone()
    }

    pub fn get_gop_packets(&self) -> Vec<RtpPacket> {
        self.gop_cache.read().unwrap().packets.clone()
    }

    pub fn get_keyframe_packets(&self) -> Vec<RtpPacket> {
        let gop = self.gop_cache.read().unwrap();
        if gop.packets.is_empty() {
            return self.get_param_sets();
        }
        let kf_ts = gop.packets.iter().find(|p| {
            let (is_kf, _) = is_keyframe_or_param_set(self.is_hevc, p.payload());
            is_kf
        }).map(|p| p.timestamp);

        if let Some(ts) = kf_ts {
            gop.packets.iter().filter(|p| {
                let (_, is_ps) = is_keyframe_or_param_set(self.is_hevc, p.payload());
                is_ps || p.timestamp == ts
            }).cloned().collect()
        } else {
            gop.packets.clone()
        }
    }

    pub fn get_param_sets_data(&self) -> ParamSets {
        self.param_sets.read().unwrap().clone()
    }

    pub fn get_sprop(&self) -> Option<String> {
        let ps = self.param_sets.read().unwrap();
        if let (Some(sps), Some(pps)) = (&ps.h264_sps, &ps.h264_pps) {
            let sps_b64 = base64::engine::general_purpose::STANDARD.encode(sps);
            let pps_b64 = base64::engine::general_purpose::STANDARD.encode(pps);
            Some(format!("sprop-parameter-sets={},{}", sps_b64, pps_b64))
        } else if let (Some(vps), Some(sps), Some(pps)) = (&ps.h265_vps, &ps.h265_sps, &ps.h265_pps) {
            let vps_b64 = base64::engine::general_purpose::STANDARD.encode(vps);
            let sps_b64 = base64::engine::general_purpose::STANDARD.encode(sps);
            let pps_b64 = base64::engine::general_purpose::STANDARD.encode(pps);
            Some(format!("sprop-vps={};sprop-sps={};sprop-pps={}", vps_b64, sps_b64, pps_b64))
        } else {
            None
        }
    }

    fn inspect_and_cache_params(&self, packet: &RtpPacket) {
        let payload = packet.payload();
        if payload.is_empty() {
            return;
        }

        let mut ps = self.param_sets.write().unwrap();
        if !self.is_hevc {
            let nalu_type = payload[0] & 0x1F;
            if nalu_type == 7 {
                ps.h264_sps = Some(payload.to_vec());
            } else if nalu_type == 8 {
                ps.h264_pps = Some(payload.to_vec());
            } else if nalu_type == 24 {
                let nals = unpack_h264_stap_a(payload);
                for nal in nals {
                    if !nal.is_empty() {
                        let sub_type = nal[0] & 0x1F;
                        if sub_type == 7 {
                            ps.h264_sps = Some(nal);
                        } else if sub_type == 8 {
                            ps.h264_pps = Some(nal);
                        }
                    }
                }
            }
            if let (Some(sps), Some(pps)) = (&ps.h264_sps, &ps.h264_pps) {
                let stap_a = build_h264_stap_a(sps, pps);
                let rtp = RtpPacket::create(packet.payload_type, packet.sequence_number, packet.timestamp, packet.ssrc, false, &stap_a);
                ps.rtp_packets = vec![rtp];
            }
        } else {
            let nalu_type = (payload[0] >> 1) & 0x3F;
            if nalu_type == 32 {
                ps.h265_vps = Some(payload.to_vec());
            } else if nalu_type == 33 {
                ps.h265_sps = Some(payload.to_vec());
            } else if nalu_type == 34 {
                ps.h265_pps = Some(payload.to_vec());
            } else if nalu_type == 48 {
                let nals = unpack_h265_ap(payload);
                for nal in nals {
                    if !nal.is_empty() {
                        let sub_type = (nal[0] >> 1) & 0x3F;
                        if sub_type == 32 {
                            ps.h265_vps = Some(nal);
                        } else if sub_type == 33 {
                            ps.h265_sps = Some(nal);
                        } else if sub_type == 34 {
                            ps.h265_pps = Some(nal);
                        }
                    }
                }
            }
            let mut rtp_pkts = Vec::new();
            if let Some(ref vps) = ps.h265_vps {
                rtp_pkts.push(RtpPacket::create(packet.payload_type, packet.sequence_number, packet.timestamp, packet.ssrc, false, vps));
            }
            if let Some(ref sps) = ps.h265_sps {
                rtp_pkts.push(RtpPacket::create(packet.payload_type, packet.sequence_number.wrapping_add(1), packet.timestamp, packet.ssrc, false, sps));
            }
            if let Some(ref pps) = ps.h265_pps {
                rtp_pkts.push(RtpPacket::create(packet.payload_type, packet.sequence_number.wrapping_add(2), packet.timestamp, packet.ssrc, false, pps));
            }
            if !rtp_pkts.is_empty() {
                ps.rtp_packets = rtp_pkts;
            }
        }
    }
}

pub fn unpack_h264_stap_a(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    if payload.len() < 3 || (payload[0] & 0x1F) != 24 {
        return nals;
    }
    let mut idx = 1;
    while idx + 2 <= payload.len() {
        let nlen = u16::from_be_bytes([payload[idx], payload[idx + 1]]) as usize;
        idx += 2;
        if nlen > 0 && idx + nlen <= payload.len() {
            nals.push(payload[idx..idx + nlen].to_vec());
            idx += nlen;
        } else {
            break;
        }
    }
    nals
}

pub fn extract_h264_sps_pps(fmtp: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    if let Some(pos) = fmtp.find("sprop-parameter-sets=") {
        let after = &fmtp[pos + "sprop-parameter-sets=".len()..];
        let val = after.split(';').next()?.trim();
        let (sps_b64, pps_b64) = val.split_once(',')?;
        let sps = base64::engine::general_purpose::STANDARD
            .decode(sps_b64.trim())
            .ok()?;
        let pps = base64::engine::general_purpose::STANDARD
            .decode(pps_b64.trim())
            .ok()?;
        return Some((sps, pps));
    }
    None
}

pub fn extract_h265_vps_sps_pps(fmtp: &str) -> Option<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;

    for part in fmtp.split(';') {
        let part = part.trim();
        if let Some((k, v)) = part.split_once('=') {
            match k.trim() {
                "sprop-vps" => {
                    vps = base64::engine::general_purpose::STANDARD
                        .decode(v.trim())
                        .ok();
                }
                "sprop-sps" => {
                    sps = base64::engine::general_purpose::STANDARD
                        .decode(v.trim())
                        .ok();
                }
                "sprop-pps" => {
                    pps = base64::engine::general_purpose::STANDARD
                        .decode(v.trim())
                        .ok();
                }
                _ => {}
            }
        }
    }

    if let (Some(v), Some(s), Some(p)) = (vps, sps, pps) {
        Some((v, s, p))
    } else {
        None
    }
}

pub fn build_h264_stap_a(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(1 + 2 + sps.len() + 2 + pps.len());
    payload.push(0x78); // STAP-A header: F=0, NRI=3, Type=24
    payload.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    payload.extend_from_slice(sps);
    payload.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    payload.extend_from_slice(pps);
    payload
}

pub fn build_h265_ap(vps: &[u8], sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(2 + 2 + vps.len() + 2 + sps.len() + 2 + pps.len());
    payload.push(0x60); // AP header byte 1: Type=48 << 1
    payload.push(0x01); // AP header byte 2: LayerId=0, TID=1
    payload.extend_from_slice(&(vps.len() as u16).to_be_bytes());
    payload.extend_from_slice(vps);
    payload.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    payload.extend_from_slice(sps);
    payload.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    payload.extend_from_slice(pps);
    payload
}

pub fn unpack_h265_ap(payload: &[u8]) -> Vec<Vec<u8>> {
    let mut nals = Vec::new();
    if payload.len() < 4 || ((payload[0] >> 1) & 0x3F) != 48 {
        return nals;
    }
    let mut idx = 2;
    while idx + 2 <= payload.len() {
        let nlen = u16::from_be_bytes([payload[idx], payload[idx + 1]]) as usize;
        idx += 2;
        if nlen > 0 && idx + nlen <= payload.len() {
            nals.push(payload[idx..idx + nlen].to_vec());
            idx += nlen;
        } else {
            break;
        }
    }
    nals
}

pub fn is_keyframe_or_param_set(is_hevc: bool, payload: &[u8]) -> (bool, bool) {
    if payload.is_empty() {
        return (false, false);
    }
    if is_hevc {
        let nalu_type = (payload[0] >> 1) & 0x3F;
        if nalu_type == 32 || nalu_type == 33 || nalu_type == 34 || nalu_type == 48 {
            return (false, true); // VPS, SPS, PPS, or AP
        }
        if (19..=21).contains(&nalu_type) {
            return (true, false); // IDR / CRA
        }
        if nalu_type == 49 && payload.len() >= 3 {
            let start = (payload[2] & 0x80) != 0;
            let fu_type = payload[2] & 0x3F;
            if (19..=21).contains(&fu_type) {
                return (start, false);
            }
        }
    } else {
        let nalu_type = payload[0] & 0x1F;
        if nalu_type == 7 || nalu_type == 8 || nalu_type == 24 {
            return (false, true); // SPS, PPS, or STAP-A
        }
        if nalu_type == 5 {
            return (true, false); // IDR slice
        }
        if nalu_type == 28 && payload.len() >= 2 {
            let start = (payload[1] & 0x80) != 0;
            let fu_type = payload[1] & 0x1F;
            if fu_type == 5 {
                return (start, false);
            }
        }
    }
    (false, false)
}

pub fn rtp_packets_to_annex_b(is_hevc: bool, packets: &[RtpPacket]) -> Vec<u8> {
    let est_cap: usize = packets.iter().map(|p| p.payload_len + 4).sum();
    let mut annex_b = Vec::with_capacity(est_cap);
    let mut fu_buf = Vec::with_capacity(65536);

    for pkt in packets {
        let payload = pkt.payload();
        if payload.is_empty() {
            continue;
        }

        if !is_hevc {
            let nal_type = payload[0] & 0x1F;
            if (1..=23).contains(&nal_type) {
                annex_b.extend_from_slice(&[0, 0, 0, 1]);
                annex_b.extend_from_slice(payload);
            } else if nal_type == 24 {
                let nals = unpack_h264_stap_a(payload);
                for nal in nals {
                    if !nal.is_empty() {
                        annex_b.extend_from_slice(&[0, 0, 0, 1]);
                        annex_b.extend_from_slice(&nal);
                    }
                }
            } else if nal_type == 28 && payload.len() >= 2 {
                let s_bit = (payload[1] & 0x80) != 0;
                let e_bit = (payload[1] & 0x40) != 0;
                let inner_type = payload[1] & 0x1F;

                if s_bit {
                    fu_buf.clear();
                    fu_buf.push((payload[0] & 0xE0) | inner_type);
                    fu_buf.extend_from_slice(&payload[2..]);
                } else if !fu_buf.is_empty() {
                    fu_buf.extend_from_slice(&payload[2..]);
                }

                if e_bit && !fu_buf.is_empty() {
                    annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    annex_b.extend_from_slice(&fu_buf);
                    fu_buf.clear();
                }
            }
        } else {
            let nal_type = (payload[0] >> 1) & 0x3F;
            if nal_type < 48 {
                annex_b.extend_from_slice(&[0, 0, 0, 1]);
                annex_b.extend_from_slice(payload);
            } else if nal_type == 48 {
                let nals = unpack_h265_ap(payload);
                for nal in nals {
                    if !nal.is_empty() {
                        annex_b.extend_from_slice(&[0, 0, 0, 1]);
                        annex_b.extend_from_slice(&nal);
                    }
                }
            } else if nal_type == 49 && payload.len() >= 3 {
                let s_bit = (payload[2] & 0x80) != 0;
                let e_bit = (payload[2] & 0x40) != 0;
                let inner_type = payload[2] & 0x3F;

                if s_bit {
                    fu_buf.clear();
                    fu_buf.push((payload[0] & 0x81) | (inner_type << 1));
                    fu_buf.push(payload[1]);
                    fu_buf.extend_from_slice(&payload[3..]);
                } else if !fu_buf.is_empty() {
                    fu_buf.extend_from_slice(&payload[3..]);
                }

                if e_bit && !fu_buf.is_empty() {
                    annex_b.extend_from_slice(&[0, 0, 0, 1]);
                    annex_b.extend_from_slice(&fu_buf);
                    fu_buf.clear();
                }
            }
        }
    }
    annex_b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_track_h264_sps_pps_caching() {
        let codec = Codec::new_h264();
        let track = Track::new(codec, MediaKind::Video, 100);

        // Initially no parameter sets
        assert!(track.get_param_sets().is_empty());
        assert!(track.get_sprop().is_none());

        // Send SPS (NAL type 7)
        let sps_payload = vec![0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, 0xed];
        let sps_pkt = RtpPacket::create(96, 1, 1000, 0x1234, false, &sps_payload);
        track.send(sps_pkt);

        // Send PPS (NAL type 8)
        let pps_payload = vec![0x68, 0xce, 0x38, 0x80];
        let pps_pkt = RtpPacket::create(96, 2, 1000, 0x1234, false, &pps_payload);
        track.send(pps_pkt);

        // Track should now have cached STAP-A packet and sprop string
        let ps = track.get_param_sets();
        assert_eq!(ps.len(), 1);
        let stap_payload = ps[0].payload();
        assert_eq!(stap_payload[0] & 0x1F, 24); // STAP-A

        let sprop = track.get_sprop().expect("sprop should be generated");
        assert!(sprop.starts_with("sprop-parameter-sets="));
        assert!(sprop.contains(","));
    }

    #[test]
    fn test_track_fmtp_init() {
        let mut codec = Codec::new_h264();
        codec.fmtp = Some("packetization-mode=1;sprop-parameter-sets=Z0IAKeNQDwBE/LgLcBAQFA==,aM48gA==".to_string());
        let track = Track::new(codec, MediaKind::Video, 100);

        let ps = track.get_param_sets();
        assert_eq!(ps.len(), 1);
        let stap_payload = ps[0].payload();
        assert_eq!(stap_payload[0] & 0x1F, 24);

        let sprop = track.get_sprop().expect("sprop should exist from fmtp");
        assert!(sprop.contains("Z0IAKeNQDwBE/LgLcBAQFA=="));
    }

    #[test]
    fn test_track_gop_caching_and_replay() {
        let codec = Codec::new_h264();
        let track = Track::new(codec, MediaKind::Video, 100);

        // Pre-cache SPS / PPS
        let sps_payload = vec![0x67, 0x42, 0x00, 0x1f, 0x96, 0x54, 0x05, 0x01, 0xed];
        track.send(RtpPacket::create(96, 1, 1000, 0x1234, false, &sps_payload));
        let pps_payload = vec![0x68, 0xce, 0x38, 0x80];
        track.send(RtpPacket::create(96, 2, 1000, 0x1234, false, &pps_payload));

        // Send Keyframe (IDR slice: NAL type 5)
        let idr_payload = vec![0x65, 0x88, 0x84, 0x00, 0x10];
        track.send(RtpPacket::create(96, 3, 3000, 0x1234, true, &idr_payload));

        // Send P-frame (NAL type 1)
        let p1_payload = vec![0x41, 0x9a, 0x12, 0x34];
        track.send(RtpPacket::create(96, 4, 6000, 0x1234, true, &p1_payload));

        // Send second P-frame
        let p2_payload = vec![0x41, 0x9a, 0x56, 0x78];
        track.send(RtpPacket::create(96, 5, 9000, 0x1234, true, &p2_payload));

        let gop = track.get_gop_packets();
        assert_eq!(gop.len(), 4); // STAP-A parameter set + IDR + P1 + P2

        let kf = track.get_keyframe_packets();
        assert_eq!(kf.len(), 2); // STAP-A parameter set + IDR
        assert_eq!(kf[1].timestamp, 3000);

        // Send next Keyframe at timestamp 12000
        let idr2_payload = vec![0x65, 0x88, 0x84, 0x00, 0x20];
        track.send(RtpPacket::create(96, 6, 12000, 0x1234, true, &idr2_payload));

        let gop2 = track.get_gop_packets();
        assert_eq!(gop2.len(), 2); // STAP-A + new IDR
        assert_eq!(gop2[1].timestamp, 12000);
    }

    #[test]
    fn test_rtp_packets_to_annex_b_conversion() {
        let sps = vec![0x67, 0x42, 0x00, 0x1f];
        let pps = vec![0x68, 0xce, 0x38, 0x80];
        let stap_a = build_h264_stap_a(&sps, &pps);
        let stap_pkt = RtpPacket::create(96, 1, 1000, 0x1234, false, &stap_a);

        // Single NAL IDR packet
        let idr = vec![0x65, 0x88, 0x84, 0x00, 0x10];
        let idr_pkt = RtpPacket::create(96, 2, 1000, 0x1234, true, &idr);

        let annex_b = rtp_packets_to_annex_b(false, &[stap_pkt, idr_pkt]);
        // Expect [0, 0, 0, 1] + SPS + [0, 0, 0, 1] + PPS + [0, 0, 0, 1] + IDR
        assert!(annex_b.starts_with(&[0, 0, 0, 1, 0x67]));
        assert!(annex_b.contains(&0x68));
        assert!(annex_b.contains(&0x65));
    }

    #[test]
    fn test_track_gop_cache_byte_bound() {
        let codec = Codec::new_h264();
        let track = Track::new(codec, MediaKind::Video, 100);

        // IDR Keyframe
        let idr = vec![0x65; 1000];
        track.send(RtpPacket::create(96, 1, 1000, 0x1234, true, &idr));
        assert_eq!(track.get_gop_packets().len(), 1);

        // Push large P-frames up to 2MB limit
        // Each packet is ~500KB
        let huge_p = vec![0x41; 500_000];
        track.send(RtpPacket::create(96, 2, 2000, 0x1234, true, &huge_p)); // ~500KB
        track.send(RtpPacket::create(96, 3, 3000, 0x1234, true, &huge_p)); // ~1MB
        track.send(RtpPacket::create(96, 4, 4000, 0x1234, true, &huge_p)); // ~1.5MB
        track.send(RtpPacket::create(96, 5, 5000, 0x1234, true, &huge_p)); // ~2MB
        assert_eq!(track.get_gop_packets().len(), 5);

        // Next 500KB packet would exceed 2MB bound (MAX_GOP_BYTES) -> rejected from cache
        track.send(RtpPacket::create(96, 6, 6000, 0x1234, true, &huge_p));
        assert_eq!(track.get_gop_packets().len(), 5);
    }
}
