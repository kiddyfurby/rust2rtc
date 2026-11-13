#![allow(dead_code)]

//! Minimal MPEG-TS muxer for the Tapo HTTP Streamd backchannel (port 8800).
//!
//! Tapo cameras expect talkback audio as MPEG-TS parts (`Content-Type: audio/mp2t`)
//! with a single PCM A-law track. This is a byte-level port of go2rtc's
//! `pkg/mpegts` (muxer.go / checksum.go / bits/writer.go), which is the
//! production-proven wire format for `tapo://` two-way audio.

use bytes::{BufMut, Bytes, BytesMut};

const PACKET_SIZE: usize = 188;
const SYNC_BYTE: u8 = 0x47;
const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const PES0_PID: u16 = 0x0100;

/// go2rtc `StreamTypePCMATapo` - Tapo-specific stream type carrying PCMA/8000.
pub const STREAM_TYPE_PCM_TAPO: u8 = 0x90;

// CRC32 table (MPEG-2), verbatim from go2rtc pkg/mpegts/checksum.go.
static CRC_TABLE: [u32; 256] = [
    0x00000000, 0xB71DC104, 0x6E3B8209, 0xD926430D, 0xDC760413, 0x6B6BC517,
    0xB24D861A, 0x0550471E, 0xB8ED0826, 0x0FF0C922, 0xD6D68A2F, 0x61CB4B2B,
    0x649B0C35, 0xD386CD31, 0x0AA08E3C, 0xBDBD4F38, 0x70DB114C, 0xC7C6D048,
    0x1EE09345, 0xA9FD5241, 0xACAD155F, 0x1BB0D45B, 0xC2969756, 0x758B5652,
    0xC836196A, 0x7F2BD86E, 0xA60D9B63, 0x11105A67, 0x14401D79, 0xA35DDC7D,
    0x7A7B9F70, 0xCD665E74, 0xE0B62398, 0x57ABE29C, 0x8E8DA191, 0x39906095,
    0x3CC0278B, 0x8BDDE68F, 0x52FBA582, 0xE5E66486, 0x585B2BBE, 0xEF46EABA,
    0x3660A9B7, 0x817D68B3, 0x842D2FAD, 0x3330EEA9, 0xEA16ADA4, 0x5D0B6CA0,
    0x906D32D4, 0x2770F3D0, 0xFE56B0DD, 0x494B71D9, 0x4C1B36C7, 0xFB06F7C3,
    0x2220B4CE, 0x953D75CA, 0x28803AF2, 0x9F9DFBF6, 0x46BBB8FB, 0xF1A679FF,
    0xF4F63EE1, 0x43EBFFE5, 0x9ACDBCE8, 0x2DD07DEC, 0x77708634, 0xC06D4730,
    0x194B043D, 0xAE56C539, 0xAB068227, 0x1C1B4323, 0xC53D002E, 0x7220C12A,
    0xCF9D8E12, 0x78804F16, 0xA1A60C1B, 0x16BBCD1F, 0x13EB8A01, 0xA4F64B05,
    0x7DD00808, 0xCACDC90C, 0x07AB9778, 0xB0B6567C, 0x69901571, 0xDE8DD475,
    0xDBDD936B, 0x6CC0526F, 0xB5E61162, 0x02FBD066, 0xBF469F5E, 0x085B5E5A,
    0xD17D1D57, 0x6660DC53, 0x63309B4D, 0xD42D5A49, 0x0D0B1944, 0xBA16D840,
    0x97C6A5AC, 0x20DB64A8, 0xF9FD27A5, 0x4EE0E6A1, 0x4BB0A1BF, 0xFCAD60BB,
    0x258B23B6, 0x9296E2B2, 0x2F2BAD8A, 0x98366C8E, 0x41102F83, 0xF60DEE87,
    0xF35DA999, 0x4440689D, 0x9D662B90, 0x2A7BEA94, 0xE71DB4E0, 0x500075E4,
    0x892636E9, 0x3E3BF7ED, 0x3B6BB0F3, 0x8C7671F7, 0x555032FA, 0xE24DF3FE,
    0x5FF0BCC6, 0xE8ED7DC2, 0x31CB3ECF, 0x86D6FFCB, 0x8386B8D5, 0x349B79D1,
    0xEDBD3ADC, 0x5AA0FBD8, 0xEEE00C69, 0x59FDCD6D, 0x80DB8E60, 0x37C64F64,
    0x3296087A, 0x858BC97E, 0x5CAD8A73, 0xEBB04B77, 0x560D044F, 0xE110C54B,
    0x38368646, 0x8F2B4742, 0x8A7B005C, 0x3D66C158, 0xE4408255, 0x535D4351,
    0x9E3B1D25, 0x2926DC21, 0xF0009F2C, 0x471D5E28, 0x424D1936, 0xF550D832,
    0x2C769B3F, 0x9B6B5A3B, 0x26D61503, 0x91CBD407, 0x48ED970A, 0xFFF0560E,
    0xFAA01110, 0x4DBDD014, 0x949B9319, 0x2386521D, 0x0E562FF1, 0xB94BEEF5,
    0x606DADF8, 0xD7706CFC, 0xD2202BE2, 0x653DEAE6, 0xBC1BA9EB, 0x0B0668EF,
    0xB6BB27D7, 0x01A6E6D3, 0xD880A5DE, 0x6F9D64DA, 0x6ACD23C4, 0xDDD0E2C0,
    0x04F6A1CD, 0xB3EB60C9, 0x7E8D3EBD, 0xC990FFB9, 0x10B6BCB4, 0xA7AB7DB0,
    0xA2FB3AAE, 0x15E6FBAA, 0xCCC0B8A7, 0x7BDD79A3, 0xC660369B, 0x717DF79F,
    0xA85BB492, 0x1F467596, 0x1A163288, 0xAD0BF38C, 0x742DB081, 0xC3307185,
    0x99908A5D, 0x2E8D4B59, 0xF7AB0854, 0x40B6C950, 0x45E68E4E, 0xF2FB4F4A,
    0x2BDD0C47, 0x9CC0CD43, 0x217D827B, 0x9660437F, 0x4F460072, 0xF85BC176,
    0xFD0B8668, 0x4A16476C, 0x93300461, 0x242DC565, 0xE94B9B11, 0x5E565A15,
    0x87701918, 0x306DD81C, 0x353D9F02, 0x82205E06, 0x5B061D0B, 0xEC1BDC0F,
    0x51A69337, 0xE6BB5233, 0x3F9D113E, 0x8880D03A, 0x8DD09724, 0x3ACD5620,
    0xE3EB152D, 0x54F6D429, 0x7926A9C5, 0xCE3B68C1, 0x171D2BCC, 0xA000EAC8,
    0xA550ADD6, 0x124D6CD2, 0xCB6B2FDF, 0x7C76EEDB, 0xC1CBA1E3, 0x76D660E7,
    0xAFF023EA, 0x18EDE2EE, 0x1DBDA5F0, 0xAAA064F4, 0x738627F9, 0xC49BE6FD,
    0x09FDB889, 0xBEE0798D, 0x67C63A80, 0xD0DBFB84, 0xD58BBC9A, 0x62967D9E,
    0xBBB03E93, 0x0CADFF97, 0xB110B0AF, 0x060D71AB, 0xDF2B32A6, 0x6836F3A2,
    0x6D66B4BC, 0xDA7B75B8, 0x035D36B5, 0xB440F7B1,
];

/// MPEG-2 CRC32 over PSI sections (go2rtc pkg/mpegts/checksum.go).
fn checksum(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = CRC_TABLE[(b ^ (crc & 0xFF) as u8) as usize] ^ (crc >> 8);
    }
    crc
}

/// MSB-first bit writer with go2rtc `pkg/bits.Writer` semantics
/// (`Len()` counts whole bytes; `write_byte`/`write_bytes` pass through when aligned).
#[derive(Default)]
struct BitWriter {
    buf: Vec<u8>,
    /// Bits left in the current byte; 0 means "no current byte" (aligned).
    bits: u8,
}

impl BitWriter {
    fn write_bit(&mut self, b: u8) {
        if self.bits == 0 {
            self.buf.push(0);
            self.bits = 7;
        } else {
            self.bits -= 1;
        }
        if b & 1 == 1 {
            *self.buf.last_mut().unwrap() |= 1 << self.bits;
        }
    }

    fn write_bits(&mut self, v: u32, n: u8) {
        for i in (0..n).rev() {
            self.write_bit(((v >> i) & 1) as u8);
        }
    }

    fn write_byte(&mut self, b: u8) {
        if self.bits != 0 {
            self.write_bits(u32::from(b), 8);
        } else {
            self.buf.push(b);
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        if self.bits != 0 {
            for &b in bytes {
                self.write_byte(b);
            }
        } else {
            self.buf.extend_from_slice(bytes);
        }
    }
}

/// Single-track (PCMA/8000) MPEG-TS muxer producing the exact byte layout
/// Tapo Streamd expects for `audio/mp2t` backchannel parts.
pub struct TapoTsMuxer {
    stream_type: u8,
    stream_id: u8,
    pid: u16,
    sequence: u16,
    timestamp: u32,
    pts: u32,
}

impl Default for TapoTsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl TapoTsMuxer {
    /// New muxer with one PCMA track (stream type 0x90, stream id 0xC0, PID 0x100).
    pub fn new() -> Self {
        Self {
            stream_type: STREAM_TYPE_PCM_TAPO,
            stream_id: 0xC0,
            pid: PES0_PID,
            sequence: 0,
            timestamp: 0,
            pts: 0,
        }
    }

    fn write_ts_header(wr: &mut BitWriter, pid: u16) {
        wr.write_byte(SYNC_BYTE);
        wr.write_bit(0); // transport error indicator
        wr.write_bit(1); // payload unit start indicator
        wr.write_bit(0); // transport priority
        wr.write_bits(u32::from(pid), 13);
        wr.write_bits(0, 2); // transport scrambling control
        wr.write_bit(0); // adaptation field
        wr.write_bit(1); // payload
        wr.write_bits(0, 4); // continuity counter
    }

    fn write_psi_header(wr: &mut BitWriter, table_id: u8, size: u16) {
        wr.write_byte(0); // pointer field
        wr.write_byte(table_id);
        wr.write_bit(1); // section syntax indicator
        wr.write_bit(0); // private bit
        wr.write_bits(0b11, 2); // reserved
        wr.write_bits(0, 2); // section length unused bits
        wr.write_bits(u32::from(5 + size + 4), 10); // section length
        wr.write_bits(1, 16); // table id extension
        wr.write_bits(0b11, 2); // reserved
        wr.write_bits(0, 5); // version number
        wr.write_bit(1); // current/next indicator
        wr.write_byte(0); // section number
        wr.write_byte(0); // last section number
    }

    fn write_tail(wr: &mut BitWriter) {
        let size = PACKET_SIZE - wr.buf.len() % PACKET_SIZE;
        wr.write_bytes(&vec![0u8; size]);
    }

    fn write_crc(wr: &mut BitWriter, from: usize) {
        let crc = checksum(&wr.buf[from..]);
        wr.write_bytes(&crc.to_le_bytes());
    }

    /// PAT + PMT (two TS packets, 376 bytes total). Send once, before any audio,
    /// as the first `audio/mp2t` part.
    pub fn header(&self) -> Bytes {
        let mut wr = BitWriter::default();

        // PAT: points at the PMT.
        Self::write_ts_header(&mut wr, PAT_PID);
        let from = wr.buf.len() + 1; // CRC starts after the pointer field
        Self::write_psi_header(&mut wr, 0x00, 4);
        wr.write_bits(1, 16); // program number
        wr.write_bits(0b111, 3); // reserved
        wr.write_bits(u32::from(PMT_PID), 13); // program map PID
        Self::write_crc(&mut wr, from);
        Self::write_tail(&mut wr);

        // PMT: single PCMA track.
        Self::write_ts_header(&mut wr, PMT_PID);
        let from = wr.buf.len() + 1;
        Self::write_psi_header(&mut wr, 0x02, 4 + 5);
        wr.write_bits(0b111, 3); // reserved
        wr.write_bits(0x1FFF, 13); // PCR PID (unused)
        wr.write_bits(0b1111, 4); // reserved
        wr.write_bits(0, 2); // program info length unused bits
        wr.write_bits(0, 10); // program info length
        wr.write_byte(self.stream_type);
        wr.write_bits(0b111, 3); // reserved
        wr.write_bits(u32::from(self.pid), 13); // elementary PID
        wr.write_bits(0b1111, 4); // reserved
        wr.write_bits(0, 2); // ES info length unused bits
        wr.write_bits(0, 10); // ES info length
        Self::write_crc(&mut wr, from);
        Self::write_tail(&mut wr);

        Bytes::from(wr.buf)
    }

    /// Wrap one PCMA chunk as a PES packet and packetize it into 188-byte TS
    /// packets. `timestamp` is the 8kHz RTP timestamp of this chunk (the PTS
    /// deltas follow it, matching go2rtc).
    pub fn payload(&mut self, timestamp: u32, data: &[u8]) -> Bytes {
        if self.timestamp != 0 {
            self.pts = self
                .pts
                .wrapping_add(timestamp.wrapping_sub(self.timestamp));
        }
        self.timestamp = timestamp;

        // PES header: start code + stream id + length + flags + PTS.
        let size = 3 + 5 + data.len();
        let mut body = BytesMut::with_capacity(6 + 3 + 5 + data.len());
        body.extend_from_slice(&[0x00, 0x00, 0x01, self.stream_id]);
        if size <= 0xFFFF {
            body.extend_from_slice(&(size as u16).to_be_bytes());
        }
        body.extend_from_slice(&[0x80, 0x80, 0x05]); // optional header: PTS only
        write_time(&mut body, self.pts);
        body.extend_from_slice(data);

        let mut out = BytesMut::new();
        let mut first = true;
        while !body.is_empty() {
            out.put_u8(SYNC_BYTE);
            let pid = self.pid | if first { 0x4000 } else { 0x00 };
            out.put_u16(pid);
            let counter = (self.sequence & 0x0F) as u8;
            if body.len() < PACKET_SIZE - 4 {
                out.put_u8(0x20 | 0x10 | counter); // adaptation + payload
                let ad_size = (PACKET_SIZE - 4 - 1 - body.len()) as u8;
                out.put_u8(ad_size);
                out.put_bytes(0, ad_size as usize);
                out.extend_from_slice(&body);
                body.clear();
            } else {
                out.put_u8(0x10 | counter); // payload only
                let rest = body.split_off(PACKET_SIZE - 4);
                out.extend_from_slice(&body);
                body = rest;
            }
            self.sequence = self.sequence.wrapping_add(1);
            first = false;
        }

        out.freeze()
    }
}

/// 33-bit PTS, byte layout identical to go2rtc `mpegts.WriteTime`.
fn write_time(out: &mut BytesMut, t: u32) {
    out.put_u8(0x20 | ((t >> 29) & 0xFF) as u8 | 1);
    out.put_u8((t >> 22) as u8);
    out.put_u8(((t >> 14) & 0xFF) as u8 | 1);
    out.put_u8((t >> 7) as u8);
    out.put_u8(((t << 1) & 0xFF) as u8 | 1);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference vectors generated with the validated Python port of the same
    // algorithm (go2rtc pkg/mpegts) against firmware Streamd on a Tapo C210.
    #[test]
    fn test_header_pat_pmt_vectors() {
        let mx = TapoTsMuxer::new();
        let h = mx.header();
        assert_eq!(h.len(), 376);
        assert_eq!(h[0], 0x47);
        assert_eq!(h[188], 0x47);
        assert_eq!(
            &h[..32],
            &hex::decode("474000100000b00d0001c100000001f0002ab104b20000000000000000000000")
                .unwrap()
        );
        assert_eq!(
            &h[188..220],
            &hex::decode("475000100002b0120001c10000fffff00090e100f0005631e8cd000000000000")
                .unwrap()
        );
    }

    #[test]
    fn test_payload_small_vectors() {
        let mut mx = TapoTsMuxer::new();
        let _ = mx.header();

        let p1 = mx.payload(640, &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(p1.len(), 188);
        assert_eq!(
            &p1[..],
            hex::decode(concat!(
                "47410030a5000000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000000000000000",
                "000000000000000000000000000000000000000001c0000c808005210001000101020304"
            ))
            .unwrap()
            .as_slice()
        );

        let p2 = mx.payload(1280, &[0x01, 0x02, 0x03, 0x04]);
        assert_eq!(
            &p2[..],
            hex::decode(concat!(
                "47410031a5000000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000000000000000",
                "0000000000000000000000000000000000000000000000000000000000000000000000000000",
                "000000000000000000000000000000000000000001c0000c808005210001050101020304"
            ))
            .unwrap()
            .as_slice()
        );
    }

    #[test]
    fn test_payload_640_chunk() {
        let mut mx = TapoTsMuxer::new();
        let h = mx.header();
        assert_eq!(h.len(), 376);
        let _ = mx.payload(640, &[0x01, 0x02, 0x03, 0x04]); // keep seq aligned with vector
        let _ = mx.payload(1280, &[0x01, 0x02, 0x03, 0x04]);
        let p3 = mx.payload(1920, &vec![0xD5u8; 640]);
        assert_eq!(p3.len(), 752);
        assert_eq!(&p3[..16], &hex::decode("47410012000001c00288808005210001").unwrap());
        // every TS packet starts with a sync byte
        for i in (0..p3.len()).step_by(PACKET_SIZE) {
            assert_eq!(p3[i], 0x47);
        }
    }

    #[test]
    fn test_checksum_known_vectors() {
        // The go2rtc table computes the MPEG-2 CRC in "another endian": the
        // returned u32 is the byte-swap of the standard MSB-first CRC-32/MPEG-2
        // value and is serialized little-endian, so the wire bytes equal the
        // standard big-endian representation. Expected values come from the
        // validated reference vectors (PAT/PMT sections of the Streamd header).
        let pat = hex::decode("00b00d0001c100000001f000").unwrap();
        assert_eq!(checksum(&pat), 0xb204_b12a); // wire CRC bytes: 2a b1 04 b2

        let pmt = hex::decode("02b0120001c10000fffff00090e100f000").unwrap();
        assert_eq!(checksum(&pmt), 0xcde8_3156); // wire CRC bytes: 56 31 e8 cd
    }
}