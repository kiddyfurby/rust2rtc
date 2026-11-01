//! RFC 2435: RTP Payload Format for JPEG-compressed Video
//!
//! Implements baseline JPEG parsing and RTP packetization (Payload Type 26)
//! for ESPHome intercoms, touch panels (e.g. Waveshare P4), and SIP endpoints.

/// Parsed metadata from a baseline JPEG image for RFC 2435 packetization
pub struct RtpJpegFrame<'a> {
    pub scan: &'a [u8],
    pub quantizers: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub jpeg_type: u8,
    pub dri: u16,
}

/// Parse a baseline JPEG frame to extract RFC 2435 headers and entropy scan
pub fn parse_jpeg_for_rtp(data: &[u8]) -> Option<RtpJpegFrame<'_>> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }

    let mut quant_luma: Option<Vec<u8>> = None;
    let mut quant_chroma: Option<Vec<u8>> = None;
    let mut width: u16 = 0;
    let mut height: u16 = 0;
    let mut jpeg_type: u8 = 1; // 1 = 4:2:0, 0 = 4:2:2
    let mut dri: u16 = 0;
    let mut scan_start: Option<usize> = None;

    let mut pos = 2;
    while pos + 1 < data.len() {
        if data[pos] != 0xFF {
            pos += 1;
            continue;
        }
        while pos < data.len() && data[pos] == 0xFF {
            pos += 1;
        }
        if pos >= data.len() {
            break;
        }

        let marker = data[pos];
        pos += 1;

        // EOI or SOS or standalone markers
        if marker == 0xD9 {
            // End of image
            break;
        }
        if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }

        if pos + 2 > data.len() {
            return None;
        }
        let seg_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        if seg_len < 2 || pos + seg_len > data.len() {
            return None;
        }

        let seg = &data[pos + 2..pos + seg_len];

        match marker {
            0xDB => {
                // DQT (Define Quantization Table)
                let mut off = 0;
                while off < seg.len() {
                    let desc = seg[off];
                    let precision = desc >> 4;
                    let table_id = desc & 0x0F;
                    off += 1;
                    if precision == 0 && off + 64 <= seg.len() {
                        let table = seg[off..off + 64].to_vec();
                        if table_id == 0 {
                            quant_luma = Some(table);
                        } else if table_id == 1 {
                            quant_chroma = Some(table);
                        }
                        off += 64;
                    } else {
                        break;
                    }
                }
            }
            0xC0 => {
                // SOF0 (Baseline DCT frame header)
                if seg.len() >= 8 {
                    height = u16::from_be_bytes([seg[1], seg[2]]);
                    width = u16::from_be_bytes([seg[3], seg[4]]);
                    // Component 1 sampling factors: 0x22 = 4:2:0, 0x21 = 4:2:2
                    jpeg_type = if seg[7] == 0x21 { 0 } else { 1 };
                }
            }
            0xDD => {
                // DRI (Define Restart Interval)
                if seg.len() >= 2 {
                    dri = u16::from_be_bytes([seg[0], seg[1]]);
                }
            }
            0xDA => {
                // SOS (Start of Scan)
                scan_start = Some(pos + seg_len);
                break;
            }
            _ => {}
        }

        pos += seg_len;
    }

    let scan_start_idx = scan_start?;
    // Scan ends at 0xFF 0xD9 (EOI)
    let mut scan_end_idx = data.len();
    for i in (scan_start_idx..data.len().saturating_sub(1)).rev() {
        if data[i] == 0xFF && data[i + 1] == 0xD9 {
            scan_end_idx = i;
            break;
        }
    }

    if scan_end_idx <= scan_start_idx {
        return None;
    }

    let luma = quant_luma.unwrap_or_else(|| vec![16u8; 64]);
    let chroma = quant_chroma.unwrap_or_else(|| luma.clone());
    let mut quantizers = Vec::with_capacity(128);
    quantizers.extend_from_slice(&luma);
    quantizers.extend_from_slice(&chroma);

    Some(RtpJpegFrame {
        scan: &data[scan_start_idx..scan_end_idx],
        quantizers,
        width,
        height,
        jpeg_type,
        dri,
    })
}

/// Packetize a baseline JPEG into RFC 2435 RTP packets
pub fn packetize_jpeg_rtp(
    jpeg_data: &[u8],
    seq: &mut u16,
    timestamp: u32,
    ssrc: u32,
    max_payload: usize,
) -> Option<Vec<Vec<u8>>> {
    let frame = parse_jpeg_for_rtp(jpeg_data)?;
    let width_blocks = ((frame.width + 7) / 8).min(255) as u8;
    let height_blocks = ((frame.height + 7) / 8).min(255) as u8;

    let mut packets = Vec::new();
    let mut offset: usize = 0;
    let total_scan = frame.scan.len();

    while offset < total_scan {
        let first = offset == 0;
        let mut rtp_packet = Vec::with_capacity(12 + max_payload);

        // 1. RTP Header (12 bytes)
        // V=2, P=0, X=0, CC=0 -> 0x80
        rtp_packet.push(0x80);
        // Byte 1: Marker bit set later, Payload Type = 26 (JPEG)
        let pt_byte_idx = rtp_packet.len();
        rtp_packet.push(26);
        rtp_packet.extend_from_slice(&seq.to_be_bytes());
        *seq = seq.wrapping_add(1);
        rtp_packet.extend_from_slice(&timestamp.to_be_bytes());
        rtp_packet.extend_from_slice(&ssrc.to_be_bytes());

        // 2. Main RFC 2435 JPEG Header (8 bytes)
        rtp_packet.push(0); // Type-specific (0)
        rtp_packet.push(((offset >> 16) & 0xFF) as u8); // Fragment offset 24-bit
        rtp_packet.push(((offset >> 8) & 0xFF) as u8);
        rtp_packet.push((offset & 0xFF) as u8);
        let type_field = frame.jpeg_type | if frame.dri > 0 { 0x40 } else { 0 };
        rtp_packet.push(type_field);
        rtp_packet.push(255); // Q = 255 (explicit quantization table header)
        rtp_packet.push(width_blocks);
        rtp_packet.push(height_blocks);

        // 3. Restart Marker Header (4 bytes, if DRI > 0)
        if frame.dri > 0 {
            rtp_packet.extend_from_slice(&frame.dri.to_be_bytes());
            rtp_packet.extend_from_slice(&0xFFFFu16.to_be_bytes());
        }

        // 4. Quantization Table Header (4 bytes + Q table bytes, only if offset == 0)
        if first {
            rtp_packet.push(0); // MBZ
            rtp_packet.push(0); // Precision = 8-bit
            rtp_packet.extend_from_slice(&(frame.quantizers.len() as u16).to_be_bytes());
            rtp_packet.extend_from_slice(&frame.quantizers);
        }

        // 5. Entropy scan chunk
        let header_overhead = rtp_packet.len() - 12;
        let room = max_payload.saturating_sub(header_overhead);
        if room == 0 {
            return None;
        }

        let end = (offset + room).min(total_scan);
        rtp_packet.extend_from_slice(&frame.scan[offset..end]);

        let is_last = end == total_scan;
        if is_last {
            // Set marker bit (bit 7 of byte 1)
            rtp_packet[pt_byte_idx] |= 0x80;
        }

        packets.push(rtp_packet);
        offset = end;
    }

    Some(packets)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_and_packetize_jpeg() {
        // Construct a minimal valid JPEG with SOF0, DQT, SOS
        let mut mock_jpeg = Vec::new();
        mock_jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI

        // DQT (0xFF, 0xDB)
        mock_jpeg.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]); // len 67, table 0
        mock_jpeg.extend_from_slice(&[16u8; 64]);

        // SOF0 (0xFF, 0xC0)
        mock_jpeg.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x11, 0x08, // len 17, 8-bit
            0x00, 0xF0, // height 240
            0x01, 0x40, // width 320
            0x03,       // 3 components
            0x01, 0x22, 0x00, // Y: 4:2:0, q0
            0x02, 0x11, 0x00, // Cb: 1:1, q0
            0x03, 0x11, 0x00, // Cr: 1:1, q0
        ]);

        // SOS (0xFF, 0xDA)
        mock_jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03]);
        // Dummy scan bytes
        mock_jpeg.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0]);
        // EOI (0xFF, 0xD9)
        mock_jpeg.extend_from_slice(&[0xFF, 0xD9]);

        let frame = parse_jpeg_for_rtp(&mock_jpeg).expect("Must parse valid mock JPEG");
        assert_eq!(frame.width, 320);
        assert_eq!(frame.height, 240);
        assert_eq!(frame.jpeg_type, 1);
        assert_eq!(frame.scan.len(), 8);

        let mut seq = 100u16;
        let packets = packetize_jpeg_rtp(&mock_jpeg, &mut seq, 90000, 0x12345678, 1200)
            .expect("Must packetize");
        assert_eq!(packets.len(), 1);
        let p = &packets[0];
        assert_eq!(p[1] & 0x7F, 26); // PT 26
        assert_eq!(p[1] & 0x80, 0x80); // Marker bit set on last/only packet
        assert_eq!(seq, 101);
    }

    #[test]
    fn test_multi_packet_fragmentation() {
        let mut mock_jpeg = Vec::new();
        mock_jpeg.extend_from_slice(&[0xFF, 0xD8]); // SOI
        mock_jpeg.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        mock_jpeg.extend_from_slice(&[16u8; 64]);
        mock_jpeg.extend_from_slice(&[
            0xFF, 0xC0, 0x00, 0x11, 0x08,
            0x00, 0xF0, 0x01, 0x40, 0x03,
            0x01, 0x22, 0x00, 0x02, 0x11, 0x00, 0x03, 0x11, 0x00,
        ]);
        mock_jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x03, 0x01, 0x00, 0x02, 0x11, 0x03]);
        // 300 bytes of dummy scan data
        mock_jpeg.extend_from_slice(&[0x42u8; 300]);
        mock_jpeg.extend_from_slice(&[0xFF, 0xD9]); // EOI

        let mut seq = 200u16;
        // max_payload = 160: first pkt has 140-byte RFC header + 20 bytes scan = 160
        let packets = packetize_jpeg_rtp(&mock_jpeg, &mut seq, 90000, 0x12345678, 160)
            .expect("Must packetize into multiple fragments");
        assert_eq!(packets.len(), 3);
        // First packet: M=0
        assert_eq!(packets[0][1] & 0x80, 0);
        assert_eq!(packets[0][1] & 0x7F, 26);
        // Second packet: M=0
        assert_eq!(packets[1][1] & 0x80, 0);
        assert_eq!(packets[1][1] & 0x7F, 26);
        // Third packet: M=1 (last fragment)
        assert_eq!(packets[2][1] & 0x80, 0x80);
        assert_eq!(packets[2][1] & 0x7F, 26);
        assert_eq!(seq, 203);
    }
}
