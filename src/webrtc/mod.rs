pub mod rustrtc_backend;

use crate::core::MediaKind;
use crate::streams::Stream;
use anyhow::Result;

use bytes::Bytes;
use std::sync::Arc;
use tracing::debug;

#[derive(Clone)]
pub enum ActivePeerConnection {
    RustRtc(Arc<rustrtc_backend::RustRtcSession>),
}

impl ActivePeerConnection {
    pub async fn add_ice_candidate(&self, cand_str: &str) -> Result<()> {
        match self {
            ActivePeerConnection::RustRtc(session) => {
                let clean_cand = cand_str.trim().trim_start_matches("a=");
                let parsed = rustrtc::transports::ice::IceCandidate::from_sdp(clean_cand)
                    .map_err(|e| anyhow::anyhow!("Failed to parse ICE candidate: {:?}", e))?;
                session.add_ice_candidate(parsed)?;
            }
        }
        Ok(())
    }

    pub async fn close(&self) {
        match self {
            ActivePeerConnection::RustRtc(session) => {
                session.pc.close();
            }
        }
    }
}

#[derive(Clone)]
pub struct WebRtcManager {
    #[allow(dead_code)]
    pub engine: String,
    candidates: Vec<String>,
    #[allow(dead_code)]
    pub rtsp_port: u16,
    #[allow(dead_code)]
    pub ffmpeg_bin: String,
    pub sessions: Arc<tokio::sync::RwLock<std::collections::HashMap<String, ActivePeerConnection>>>,
}

impl WebRtcManager {
    pub fn new(candidates: Vec<String>, rtsp_port: u16, ffmpeg_bin: String, engine: String) -> Self {
        Self {
            engine,
            candidates,
            rtsp_port,
            ffmpeg_bin,
            sessions: Arc::new(tokio::sync::RwLock::new(std::collections::HashMap::new())),
        }
    }

    pub async fn handle_whep(
        &self,
        stream: Arc<Stream>,
        offer_sdp: &str,
    ) -> Result<(String, ActivePeerConnection)> {
        let tracks = stream.get_tracks().await;
        let is_stream_hevc = tracks.iter().any(|t| {
            t.kind == MediaKind::Video
                && (t.codec.name.to_uppercase().contains("265")
                    || t.codec.name.to_uppercase().contains("HEVC"))
        });

        let offer_upper = offer_sdp.to_uppercase();
        let offer_has_hevc = offer_upper.contains("H265") || offer_upper.contains("HEVC");

        if is_stream_hevc && !offer_has_hevc {
            anyhow::bail!(
                "Camera stream is H.265 (HEVC), but this browser does not support WebRTC H.265. Please view the H.264 sub-stream ({}_sub) or use a browser with HEVC WebRTC enabled (e.g. Safari).",
                stream.name
            );
        }

        let (rustrtc_session, answer_sdp) = rustrtc_backend::RustRtcSession::create(
            Arc::clone(&stream),
            offer_sdp,
            &self.candidates,
        )
        .await?;
        rustrtc_session.start_forwarding(Arc::clone(&stream), is_stream_hevc, &answer_sdp);
        let session_id = rustrtc_session.session_id.clone();
        let mut keeper_rx = rustrtc_session.close_tx.subscribe();
        let session_arc = Arc::new(rustrtc_session);
        let active_pc = ActivePeerConnection::RustRtc(Arc::clone(&session_arc));
        {
            let mut guard = self.sessions.write().await;
            guard.insert(session_id.clone(), active_pc.clone());
        }
        let sessions_clone = Arc::clone(&self.sessions);
        let sid = session_id;
        let pc_holder = active_pc.clone();
        tokio::spawn(async move {
            let _ = keeper_rx.recv().await;
            debug!(session = %sid, "WebRTC session closed, removing from active sessions");
            {
                let mut guard = sessions_clone.write().await;
                guard.remove(&sid);
            }
            pc_holder.close().await;
        });
        Ok((answer_sdp, active_pc))
    }
}

pub(crate) fn find_negotiated_video_pt(sdp: &str, is_hevc: bool, is_high: bool, default_pt: u8) -> u8 {
    let mut rtpmap: Vec<(u8, String)> = Vec::new();
    let mut fmtp: Vec<(u8, String)> = Vec::new();

    for line in sdp.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("a=rtpmap:") {
            if let Some((pt_str, rest)) = r.split_once(' ')
                && let Ok(pt) = pt_str.parse::<u8>()
            {
                rtpmap.push((pt, rest.to_uppercase()));
            }
        } else if let Some(f) = line.strip_prefix("a=fmtp:")
            && let Some((pt_str, rest)) = f.split_once(' ')
            && let Ok(pt) = pt_str.parse::<u8>()
        {
            fmtp.push((pt, rest.to_string()));
        }
    }

    if is_hevc {
        // Prefer Main Profile (profile-id=1) when multiple H.265 payload types are offered
        for (pt, name) in &rtpmap {
            if name.contains("H265") || name.contains("HEVC") {
                let is_profile_1 = fmtp
                    .iter()
                    .find(|(fpt, _)| fpt == pt)
                    .is_none_or(|(_, params)| {
                        params.contains("profile-id=1") || !params.contains("profile-id=")
                    });
                if is_profile_1 {
                    return *pt;
                }
            }
        }
        for (pt, name) in &rtpmap {
            if name.contains("H265") || name.contains("HEVC") {
                return *pt;
            }
        }
        return default_pt;
    }

    if is_high {
        for (pt, params) in &fmtp {
            if params.contains("6400") {
                return *pt;
            }
        }
    } else {
        for (pt, params) in &fmtp {
            if params.contains("42") {
                return *pt;
            }
        }
    }

    for (pt, name) in &rtpmap {
        if name.contains("H264") {
            return *pt;
        }
    }

    default_pt
}

pub(crate) fn find_negotiated_audio_pt(sdp: &str, default_pt: u8) -> u8 {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("a=rtpmap:")
            && let Some((pt_str, rest)) = r.split_once(' ')
            && let Ok(pt) = pt_str.parse::<u8>()
        {
            let upper = rest.to_uppercase();
            if default_pt == 111 && upper.contains("OPUS") {
                return pt;
            } else if default_pt == 8 && upper.contains("PCMA") {
                return pt;
            } else if default_pt == 0 && upper.contains("PCMU") {
                return pt;
            }
        }
    }
    default_pt
}

pub(crate) fn is_audio_active(sdp: &str) -> bool {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("m=audio ")
            && let Some((port_str, _)) = r.split_once(' ')
            && port_str != "0"
        {
            return true;
        }
    }
    false
}

#[allow(unused_imports)]
pub use crate::core::{
    build_h264_stap_a, build_h265_ap, extract_h264_sps_pps, extract_h265_vps_sps_pps,
    is_keyframe_or_param_set, unpack_h265_ap,
};

/// Re-fragment oversized video payloads into safe MTU slices (<= 1180 bytes)
/// to prevent IP fragmentation and UDP packet loss across WebRTC paths.
pub fn fragment_payload_for_webrtc(
    is_hevc: bool,
    payload: &[u8],
    marker: bool,
) -> Vec<(Bytes, bool)> {
    const MAX_CHUNK: usize = 1170; // Guaranteed safe under 1280 MTU with IP/UDP/SRTP headers

    if payload.is_empty() {
        return Vec::new();
    }

    // For H.265, unpack Aggregation Packets (NAL 48) into individual Single NAL Unit packets
    // so SRST WebRTC receivers (Chromium/Safari) decode VPS/SPS/PPS reliably, and ensure marker=false on parameter sets.
    if is_hevc {
        let nalu_type = (payload[0] >> 1) & 0x3F;
        if nalu_type == 48 {
            let sub_nals = unpack_h265_ap(payload);
            let mut out = Vec::with_capacity(sub_nals.len());
            for (i, nalu) in sub_nals.into_iter().enumerate() {
                if nalu.is_empty() {
                    continue;
                }
                let sub_type = (nalu[0] >> 1) & 0x3F;
                // Parameter sets (32, 33, 34) and SEI (39, 40) must have marker=false
                let is_ps_or_sei = matches!(sub_type, 32 | 33 | 34 | 39 | 40);
                let sub_marker = if is_ps_or_sei { false } else { marker && i == 0 };
                out.extend(fragment_payload_for_webrtc(true, &nalu, sub_marker));
            }
            return out;
        } else if matches!(nalu_type, 32 | 33 | 34 | 39 | 40) && payload.len() <= MAX_CHUNK {
            return vec![(Bytes::copy_from_slice(payload), false)];
        }
    }

    if payload.len() <= MAX_CHUNK {
        return vec![(Bytes::copy_from_slice(payload), marker)];
    }

    let mut result = Vec::new();

    if !is_hevc {
        // H.264 (RFC 6184)
        let nalu_type = payload[0] & 0x1F;
        if nalu_type == 28 && payload.len() >= 2 {
            // FU-A packet
            let indicator = payload[0];
            let fu_header = payload[1];
            let s_bit = (fu_header & 0x80) != 0;
            let e_bit = (fu_header & 0x40) != 0;
            let r_and_type = fu_header & 0x3F;

            let data = &payload[2..];
            let chunks: Vec<_> = data.chunks(MAX_CHUNK - 2).collect();
            let total = chunks.len();

            for (idx, chunk) in chunks.into_iter().enumerate() {
                let is_first = idx == 0;
                let is_last = idx == total - 1;
                let chunk_s = if is_first { s_bit } else { false };
                let chunk_e = if is_last { e_bit } else { false };
                let new_fu = (if chunk_s { 0x80 } else { 0 })
                    | (if chunk_e { 0x40 } else { 0 })
                    | r_and_type;

                let mut out = Vec::with_capacity(2 + chunk.len());
                out.push(indicator);
                out.push(new_fu);
                out.extend_from_slice(chunk);
                result.push((Bytes::from(out), is_last && marker));
            }
        } else if nalu_type < 24 && payload.len() > 1 {
            // Single NAL unit needing fragmentation into FU-A
            let nal_header = payload[0];
            let indicator = (nal_header & 0xE0) | 28;
            let nal_type = nal_header & 0x1F;

            let data = &payload[1..];
            let chunks: Vec<_> = data.chunks(MAX_CHUNK - 2).collect();
            let total = chunks.len();

            for (idx, chunk) in chunks.into_iter().enumerate() {
                let is_first = idx == 0;
                let is_last = idx == total - 1;
                let new_fu = (if is_first { 0x80 } else { 0 })
                    | (if is_last { 0x40 } else { 0 })
                    | nal_type;

                let mut out = Vec::with_capacity(2 + chunk.len());
                out.push(indicator);
                out.push(new_fu);
                out.extend_from_slice(chunk);
                result.push((Bytes::from(out), is_last && marker));
            }
        } else {
            // STAP-A or unknown; pass through
            result.push((Bytes::copy_from_slice(payload), marker));
        }
    } else {
        // H.265 (HEVC RFC 7798)
        let nalu_type = (payload[0] >> 1) & 0x3F;
        if nalu_type == 49 && payload.len() >= 3 {
            // Fragmentation Unit (FU)
            let h0 = payload[0];
            let h1 = payload[1];
            let fu_header = payload[2];
            let s_bit = (fu_header & 0x80) != 0;
            let e_bit = (fu_header & 0x40) != 0;
            let fu_type = fu_header & 0x3F;

            let data = &payload[3..];
            let chunks: Vec<_> = data.chunks(MAX_CHUNK - 3).collect();
            let total = chunks.len();

            for (idx, chunk) in chunks.into_iter().enumerate() {
                let is_first = idx == 0;
                let is_last = idx == total - 1;
                let chunk_s = if is_first { s_bit } else { false };
                let chunk_e = if is_last { e_bit } else { false };
                let new_fu = (if chunk_s { 0x80 } else { 0 })
                    | (if chunk_e { 0x40 } else { 0 })
                    | fu_type;

                let mut out = Vec::with_capacity(3 + chunk.len());
                out.push(h0);
                out.push(h1);
                out.push(new_fu);
                out.extend_from_slice(chunk);
                result.push((Bytes::from(out), is_last && marker));
            }
        } else if nalu_type < 48 && payload.len() > 2 {
            // Single NAL unit needing fragmentation into FU
            let h0 = payload[0];
            let h1 = payload[1];
            let fu_h0 = (h0 & 0x81) | (49 << 1);
            let fu_h1 = h1;
            let nal_type = (h0 >> 1) & 0x3F;

            let data = &payload[2..];
            let chunks: Vec<_> = data.chunks(MAX_CHUNK - 3).collect();
            let total = chunks.len();

            for (idx, chunk) in chunks.into_iter().enumerate() {
                let is_first = idx == 0;
                let is_last = idx == total - 1;
                let new_fu = (if is_first { 0x80 } else { 0 })
                    | (if is_last { 0x40 } else { 0 })
                    | nal_type;

                let mut out = Vec::with_capacity(3 + chunk.len());
                out.push(fu_h0);
                out.push(fu_h1);
                out.push(new_fu);
                out.extend_from_slice(chunk);
                result.push((Bytes::from(out), is_last && marker));
            }
        } else {
            result.push((Bytes::copy_from_slice(payload), marker));
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fragment_payload_h264_fua() {
        // Create an oversized FU-A packet (e.g. 1460 bytes) with S=1, E=0, type=5 (IDR)
        let mut raw = vec![0x7c, 0x85]; // Indicator: 28, FU header: S=1, type=5
        raw.extend(vec![0xAA; 1458]);

        let chunks = fragment_payload_for_webrtc(false, &raw, false);
        assert_eq!(chunks.len(), 2);

        // Chunk 0: <= 1170 bytes, S=1, E=0
        assert!(chunks[0].0.len() <= 1170);
        assert_eq!(chunks[0].0[0], 0x7c);
        assert_eq!(chunks[0].0[1], 0x85); // S=1, E=0, type=5
        assert!(!chunks[0].1);

        // Chunk 1: <= 1170 bytes, S=0, E=0
        assert!(chunks[1].0.len() <= 1170);
        assert_eq!(chunks[1].0[0], 0x7c);
        assert_eq!(chunks[1].0[1], 0x05); // S=0, E=0, type=5
        assert!(!chunks[1].1);

        // Reassembled payload length must match original
        let total_data = (chunks[0].0.len() - 2) + (chunks[1].0.len() - 2);
        assert_eq!(total_data, 1458);
    }

    #[test]
    fn test_fragment_payload_h264_single_nal() {
        // Single NAL unit of 2000 bytes (NAL type 1: non-IDR slice)
        let mut raw = vec![0x61]; // NRI=3, Type=1
        raw.extend(vec![0xBB; 1999]);

        let chunks = fragment_payload_for_webrtc(false, &raw, true);
        assert_eq!(chunks.len(), 2);

        // Chunk 0: S=1, E=0
        assert_eq!(chunks[0].0[0] & 0x1F, 28); // FU-A
        assert_eq!(chunks[0].0[1], 0x81); // S=1, type=1
        assert!(!chunks[0].1); // marker false on non-final chunk

        // Chunk 1: S=0, E=1, marker true
        assert_eq!(chunks[1].0[0] & 0x1F, 28);
        assert_eq!(chunks[1].0[1], 0x41); // E=1, type=1
        assert!(chunks[1].1); // marker true on final chunk
    }

    #[test]
    fn test_unpack_h265_ap_and_fragment() {
        let vps = vec![0x40, 0x01, 0x0c, 0x01]; // type 32
        let sps = vec![0x42, 0x01, 0x01, 0x01]; // type 33
        let pps = vec![0x44, 0x01, 0xc0, 0x73]; // type 34
        let ap = build_h265_ap(&vps, &sps, &pps);

        let unpacked = unpack_h265_ap(&ap);
        assert_eq!(unpacked, vec![vps.clone(), sps.clone(), pps.clone()]);

        let chunks = fragment_payload_for_webrtc(true, &ap, true);
        assert_eq!(chunks.len(), 3);
        assert_eq!(&chunks[0].0[..], &vps[..]);
        assert!(!chunks[0].1); // marker must be false on VPS
        assert_eq!(&chunks[1].0[..], &sps[..]);
        assert!(!chunks[1].1); // marker must be false on SPS
        assert_eq!(&chunks[2].0[..], &pps[..]);
        assert!(!chunks[2].1); // marker must be false on PPS
    }

    #[test]
    fn test_find_negotiated_video_pt_hevc_profile1() {
        let sdp = "m=video 9 UDP/TLS/RTP/SAVPF 49 51\r\n\
                   a=rtpmap:51 H265/90000\r\n\
                   a=fmtp:51 level-id=180;profile-id=2;tier-flag=0;tx-mode=SRST\r\n\
                   a=rtpmap:49 H265/90000\r\n\
                   a=fmtp:49 level-id=180;profile-id=1;tier-flag=0;tx-mode=SRST\r\n";
        for _ in 0..20 {
            assert_eq!(find_negotiated_video_pt(sdp, true, true, 100), 49);
        }
    }

    #[tokio::test]
    async fn test_webrtc_manager_rustrtc_engine_selection() {
        let stream = Arc::new(Stream::new("test_cam".to_string(), vec![]));
        let offer_sdp = "\
v=0\r\n\
o=- 1495799811084970 1 IN IP4 127.0.0.1\r\n\
s=-\r\n\
t=0 0\r\n\
a=msid-semantic: WMS\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
c=IN IP4 0.0.0.0\r\n\
a=rtcp:9 IN IP4 0.0.0.0\r\n\
a=ice-ufrag:testufrag\r\n\
a=ice-pwd:testpassword0123456789abcdef\r\n\
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n\
a=setup:actpass\r\n\
a=mid:0\r\n\
a=sendrecv\r\n\
a=rtcp-mux\r\n\
a=rtpmap:96 H264/90000\r\n\
a=fmtp:96 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n";

        let manager = WebRtcManager::new(
            vec!["127.0.0.1".to_string()],
            8554,
            "ffmpeg".to_string(),
            "rustrtc".to_string(),
        );
        let (answer_sdp, active_pc) = manager.handle_whep(Arc::clone(&stream), offer_sdp).await.unwrap();
        assert!(answer_sdp.contains("v=0"));
        let ActivePeerConnection::RustRtc(session) = active_pc;
        assert!(!session.session_id.is_empty());
    }
}
