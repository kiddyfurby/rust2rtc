#![allow(dead_code)]

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
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

/// Track broadcaster holding a multi-consumer channel
#[derive(Debug)]
pub struct Track {
    pub codec: Codec,
    pub kind: MediaKind,
    tx: broadcast::Sender<RtpPacket>,
    pub packets_total: Arc<AtomicU64>,
    pub bytes_total: Arc<AtomicU64>,
}

impl Track {
    pub fn new(codec: Codec, kind: MediaKind, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            codec,
            kind,
            tx,
            packets_total: Arc::new(AtomicU64::new(0)),
            bytes_total: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn send(&self, packet: RtpPacket) {
        self.packets_total.fetch_add(1, Ordering::Relaxed);
        self.bytes_total.fetch_add(packet.raw.len() as u64, Ordering::Relaxed);
        let _ = self.tx.send(packet);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RtpPacket> {
        self.tx.subscribe()
    }

    pub fn consumer_count(&self) -> usize {
        self.tx.receiver_count()
    }
}
