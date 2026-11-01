use crate::core::MediaKind;
use crate::streams::Stream;
use anyhow::Result;
use base64::Engine;
use bytes::{Bytes, BytesMut};
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{
    MediaEngine, MIME_TYPE_H264, MIME_TYPE_HEVC, MIME_TYPE_OPUS, MIME_TYPE_PCMA, MIME_TYPE_PCMU,
};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp;
use rtc::rtp_transceiver::rtp_sender::RTCPFeedback;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use rtc::shared::marshal::Unmarshal;
use std::sync::Arc;
use tracing::debug;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
    RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::runtime::{channel, Sender};

#[derive(Clone)]
struct WebRtcHandler {
    session_id: String,
    gather_complete_tx: Sender<()>,
    connected_tx: Sender<()>,
    session_close_tx: tokio::sync::broadcast::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for WebRtcHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug!(session = %self.session_id, state = %state, "WebRTC peer connection state changed");
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected_tx.try_send(());
        } else if state == RTCPeerConnectionState::Failed
            || state == RTCPeerConnectionState::Closed
            || state == RTCPeerConnectionState::Disconnected
        {
            let _ = self.session_close_tx.send(());
        }
    }
}

#[derive(Clone)]
pub struct WebRtcManager {
    candidates: Vec<String>,
    #[allow(dead_code)]
    pub rtsp_port: u16,
    #[allow(dead_code)]
    pub ffmpeg_bin: String,
    pub sessions: Arc<tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn PeerConnection>>>>,
}

impl WebRtcManager {
    pub fn new(candidates: Vec<String>, rtsp_port: u16, ffmpeg_bin: String) -> Self {
        Self {
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
    ) -> Result<(String, Arc<dyn PeerConnection>)> {
        let mut media_engine = MediaEngine::default();

        let video_feedback = vec![
            RTCPFeedback {
                typ: "goog-remb".to_string(),
                parameter: "".to_string(),
            },
            RTCPFeedback {
                typ: "ccm".to_string(),
                parameter: "fir".to_string(),
            },
            RTCPFeedback {
                typ: "nack".to_string(),
                parameter: "".to_string(),
            },
            RTCPFeedback {
                typ: "nack".to_string(),
                parameter: "pli".to_string(),
            },
        ];

        let h264_high = RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640032".to_owned(),
            rtcp_feedback: video_feedback.clone(),
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: h264_high.clone(),
                payload_type: 98,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )?;

        let h264_cb = RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: video_feedback.clone(),
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: h264_cb.clone(),
                payload_type: 96,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )?;

        let h264_b = RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f".to_owned(),
            rtcp_feedback: video_feedback.clone(),
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: h264_b.clone(),
                payload_type: 97,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )?;

        let h265_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_HEVC.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "".to_owned(),
            rtcp_feedback: video_feedback.clone(),
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: h265_codec.clone(),
                payload_type: 100,
                ..Default::default()
            },
            RtpCodecKind::Video,
        )?;

        let opus_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: opus_codec.clone(),
                payload_type: 111,
                ..Default::default()
            },
            RtpCodecKind::Audio,
        )?;

        let pcma_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_PCMA.to_owned(),
            clock_rate: 8000,
            channels: 1,
            sdp_fmtp_line: "".to_owned(),
            rtcp_feedback: vec![],
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: pcma_codec.clone(),
                payload_type: 8,
                ..Default::default()
            },
            RtpCodecKind::Audio,
        )?;

        let pcmu_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_PCMU.to_owned(),
            clock_rate: 8000,
            channels: 1,
            sdp_fmtp_line: "".to_owned(),
            rtcp_feedback: vec![],
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: pcmu_codec.clone(),
                payload_type: 0,
                ..Default::default()
            },
            RtpCodecKind::Audio,
        )?;

        let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

        let mut ice_servers = vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Default::default()
        }];

        for cand in &self.candidates {
            if cand.starts_with("stun:") || cand.starts_with("turn:") {
                ice_servers.push(RTCIceServer {
                    urls: vec![cand.clone()],
                    ..Default::default()
                });
            }
        }

        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(ice_servers)
            .build();

        let session_id = uuid::Uuid::new_v4().to_string();
        let (session_close_tx, _) = tokio::sync::broadcast::channel::<()>(1);

        let (gather_complete_tx, mut gather_complete_rx) = channel::<()>(1);
        let (connected_tx, mut _connected_rx) = channel::<()>(1);

        let handler = Arc::new(WebRtcHandler {
            session_id: session_id.clone(),
            gather_complete_tx,
            connected_tx,
            session_close_tx: session_close_tx.clone(),
        });

        let peer_connection = PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(media_engine)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(vec!["0.0.0.0:0".to_string()])
            .build()
            .await?;

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

        let mut video_track_info: Option<(Arc<TrackLocalStaticRTP>, tokio::sync::broadcast::Receiver<crate::core::RtpPacket>, u32, bool, bool, Option<Vec<u8>>, u8)> = None;
        let mut audio_track_info: Option<(Arc<TrackLocalStaticRTP>, tokio::sync::broadcast::Receiver<crate::core::RtpPacket>, u32, u8)> = None;

        for track in &tracks {
            if track.kind == MediaKind::Video && video_track_info.is_none() {
                let is_hevc = track.codec.name.to_uppercase().contains("265")
                    || track.codec.name.to_uppercase().contains("HEVC");

                let is_high = track.codec.fmtp.as_deref().map_or(true, |f| f.contains("6400"));

                let (video_codec, default_pt) = if is_hevc {
                    (h265_codec.clone(), 100)
                } else if is_high {
                    (h264_high.clone(), 98)
                } else {
                    (h264_cb.clone(), 96)
                };

                let video_ssrc = crate::core::generate_id();
                let video_track: Arc<TrackLocalStaticRTP> =
                    Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
                        format!("stream-{}", stream.name),
                        format!("video-track-{}", stream.name),
                        "video".to_string(),
                        RtpCodecKind::Video,
                        vec![RTCRtpEncodingParameters {
                            rtp_coding_parameters: RTCRtpCodingParameters {
                                ssrc: Some(video_ssrc),
                                ..Default::default()
                            },
                            codec: video_codec,
                            ..Default::default()
                        }],
                    )));

                peer_connection
                    .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
                    .await?;

                let rx = track.subscribe();

                // Pre-generate parameter sets packet payload (STAP-A for H264, AP for H265) if available
                let param_set_packet_payload = if let Some(ref fmtp) = track.codec.fmtp {
                    if is_hevc {
                        extract_h265_vps_sps_pps(fmtp)
                            .map(|(vps, sps, pps)| build_h265_ap(&vps, &sps, &pps))
                    } else {
                        extract_h264_sps_pps(fmtp)
                            .map(|(sps, pps)| build_h264_stap_a(&sps, &pps))
                    }
                } else {
                    None
                };

                video_track_info = Some((video_track, rx, video_ssrc, is_hevc, is_high, param_set_packet_payload, default_pt));
            }
        }

        // Add shared Opus audio track if audio is available (native or in-process transcoded)
        if let Some(opus_track) = stream.get_or_create_opus_track().await {
            let audio_ssrc = crate::core::generate_id();
            let audio_track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
                format!("stream-{}", stream.name),
                format!("audio-track-{}", stream.name),
                "audio".to_string(),
                RtpCodecKind::Audio,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters {
                        ssrc: Some(audio_ssrc),
                        ..Default::default()
                    },
                    codec: opus_codec.clone(),
                    ..Default::default()
                }],
            )));

            if let Err(e) = peer_connection
                .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
                .await
            {
                tracing::warn!("Failed to add audio track: {}", e);
            } else {
                let rx = opus_track.subscribe();
                audio_track_info = Some((audio_track, rx, audio_ssrc, 111));
            }
        }

        let offer = if let Ok(desc) = serde_json::from_str::<RTCSessionDescription>(offer_sdp) {
            desc
        } else {
            RTCSessionDescription::offer(offer_sdp.to_string())?
        };

        peer_connection.set_remote_description(offer).await?;
        let answer = peer_connection.create_answer(None).await?;
        peer_connection.set_local_description(answer).await?;

        // Wait for ICE candidate gathering (timeout 1 second)
        let _ = tokio::time::timeout(tokio::time::Duration::from_millis(1000), gather_complete_rx.recv()).await;

        let local_desc = peer_connection
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to generate local SDP answer"))?;

        let pc_arc: Arc<dyn PeerConnection> = Arc::new(peer_connection);
        {
            let mut guard = self.sessions.write().await;
            guard.insert(session_id.clone(), Arc::clone(&pc_arc));
        }

        let mut keeper_rx = session_close_tx.subscribe();
        let sessions_clone = Arc::clone(&self.sessions);
        let sid = session_id.clone();
        let pc_holder = Arc::clone(&pc_arc);
        tokio::spawn(async move {
            let _ = keeper_rx.recv().await;
            debug!(session = %sid, "WebRTC session closed, removing from active sessions");
            {
                let mut guard = sessions_clone.write().await;
                guard.remove(&sid);
            }
            let _ = pc_holder.close().await;
        });

        // Spawn video RTP forwarder after the track is bound
        if let Some((vtrack, mut rx, video_ssrc, is_hevc, is_high, mut param_set_payload, default_pt)) = video_track_info {
            let actual_pt = find_negotiated_video_pt(&local_desc.sdp, is_hevc, is_high, default_pt);
            tracing::info!(stream = %stream.name, pt = actual_pt, is_hevc = is_hevc, "Starting WebRTC video forwarder");

            let mut close_rx = session_close_tx.subscribe();
            tokio::spawn(async move {
                let mut seq: u16 = 0;
                let mut sent_first_keyframe = false;
                let mut last_was_param_set = false;
                let mut last_keyframe_ts: Option<u32> = None;

                loop {
                    let packet = tokio::select! {
                        _ = close_rx.recv() => break,
                        res = rx.recv() => match res {
                            Ok(pkt) => pkt,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!("WebRTC video receiver lagged by {} packets", n);
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    };
                    let payload_slice = if packet.payload_offset + packet.payload_len <= packet.raw.len() {
                        &packet.raw[packet.payload_offset..packet.payload_offset + packet.payload_len]
                    } else {
                        continue;
                    };

                    let (is_keyframe, is_param_set) = is_keyframe_or_param_set(is_hevc, payload_slice);

                    // Dynamically cache STAP-A parameter sets if seen in stream
                    if is_param_set && !is_hevc && payload_slice[0] & 0x1F == 24 {
                        param_set_payload = Some(payload_slice.to_vec());
                    }

                    if !sent_first_keyframe {
                        // Wait until an actual IDR keyframe arrives before forwarding frames
                        if is_param_set {
                            continue;
                        }
                        if !is_keyframe {
                            continue;
                        }
                        sent_first_keyframe = true;
                    }

                    // Before keyframes, inject the parameter sets packet (STAP-A / AP) if not immediately preceded by parameter sets
                    if is_keyframe && last_keyframe_ts != Some(packet.timestamp) {
                        last_keyframe_ts = Some(packet.timestamp);
                        if !last_was_param_set {
                            if let Some(ref ps_bytes) = param_set_payload {
                                let ps_rtp = rtp::packet::Packet {
                                    header: rtp::header::Header {
                                        version: 2,
                                        payload_type: actual_pt,
                                        sequence_number: seq,
                                        timestamp: packet.timestamp,
                                        ssrc: video_ssrc,
                                        marker: false,
                                        ..Default::default()
                                    },
                                    payload: Bytes::copy_from_slice(ps_bytes),
                                };
                                seq = seq.wrapping_add(1);
                                let _ = vtrack.write_rtp(ps_rtp).await;
                            }
                        }
                    }

                    last_was_param_set = is_param_set;

                    let chunks = fragment_payload_for_webrtc(is_hevc, payload_slice, packet.marker);
                    for (chunk_bytes, chunk_marker) in chunks {
                        let rtp_pkt = rtp::packet::Packet {
                            header: rtp::header::Header {
                                version: 2,
                                payload_type: actual_pt,
                                sequence_number: seq,
                                timestamp: packet.timestamp,
                                ssrc: video_ssrc,
                                marker: chunk_marker,
                                ..Default::default()
                            },
                            payload: chunk_bytes,
                        };
                        seq = seq.wrapping_add(1);

                        if vtrack.write_rtp(rtp_pkt).await.is_err() {
                            // Transient error or disconnected
                        }
                    }
                }
            });
        }

        // Spawn native in-process audio RTP forwarder if audio track was negotiated
        if let Some((atrack, mut rx, audio_ssrc, default_pt)) = audio_track_info {
            if is_audio_active(&local_desc.sdp) {
                let actual_audio_pt = find_negotiated_audio_pt(&local_desc.sdp, default_pt);
                tracing::info!(stream = %stream.name, pt = actual_audio_pt, "Starting native in-process WebRTC audio forwarder");

                let mut close_rx = session_close_tx.subscribe();
                tokio::spawn(async move {
                    let mut seq: u16 = 0;
                    loop {
                        let packet = tokio::select! {
                            _ = close_rx.recv() => break,
                            res = rx.recv() => match res {
                                Ok(pkt) => pkt,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                    tracing::warn!("WebRTC audio receiver lagged by {} packets", n);
                                    continue;
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            }
                        };

                        let mut bytes = BytesMut::from(&packet.raw[..]);
                        if let Ok(mut rtp_pkt) = rtp::packet::Packet::unmarshal(&mut bytes) {
                            rtp_pkt.header.ssrc = audio_ssrc;
                            rtp_pkt.header.payload_type = actual_audio_pt;
                            rtp_pkt.header.sequence_number = seq;
                            seq = seq.wrapping_add(1);

                            let _ = atrack.write_rtp(rtp_pkt).await;
                        }
                    }
                });
            } else {
                tracing::debug!(stream = %stream.name, "Audio was rejected by client in SDP answer");
            }
        }

        Ok((local_desc.sdp, pc_arc))
    }
}

fn find_negotiated_video_pt(sdp: &str, is_hevc: bool, is_high: bool, default_pt: u8) -> u8 {
    let mut rtpmap = std::collections::HashMap::new();
    let mut fmtp = std::collections::HashMap::new();

    for line in sdp.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("a=rtpmap:") {
            if let Some((pt_str, rest)) = r.split_once(' ') {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    rtpmap.insert(pt, rest.to_uppercase());
                }
            }
        } else if let Some(f) = line.strip_prefix("a=fmtp:") {
            if let Some((pt_str, rest)) = f.split_once(' ') {
                if let Ok(pt) = pt_str.parse::<u8>() {
                    fmtp.insert(pt, rest.to_string());
                }
            }
        }
    }

    if is_hevc {
        for (&pt, name) in &rtpmap {
            if name.contains("H265") || name.contains("HEVC") {
                return pt;
            }
        }
        return default_pt;
    }

    if is_high {
        for (&pt, params) in &fmtp {
            if params.contains("6400") {
                return pt;
            }
        }
    } else {
        for (&pt, params) in &fmtp {
            if params.contains("42") {
                return pt;
            }
        }
    }

    for (&pt, name) in &rtpmap {
        if name.contains("H264") {
            return pt;
        }
    }

    default_pt
}

fn find_negotiated_audio_pt(sdp: &str, default_pt: u8) -> u8 {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("a=rtpmap:") {
            if let Some((pt_str, rest)) = r.split_once(' ') {
                if let Ok(pt) = pt_str.parse::<u8>() {
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
        }
    }
    default_pt
}

fn is_audio_active(sdp: &str) -> bool {
    for line in sdp.lines() {
        let line = line.trim();
        if let Some(r) = line.strip_prefix("m=audio ") {
            if let Some((port_str, _)) = r.split_once(' ') {
                if port_str != "0" {
                    return true;
                }
            }
        }
    }
    false
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

/// Re-fragment oversized video payloads into safe MTU slices (<= 1180 bytes)
/// to prevent IP fragmentation and UDP packet loss across WebRTC paths.
pub fn fragment_payload_for_webrtc(
    is_hevc: bool,
    payload: &[u8],
    marker: bool,
) -> Vec<(Bytes, bool)> {
    const MAX_CHUNK: usize = 1170; // Guaranteed safe under 1280 MTU with IP/UDP/SRTP headers

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
}
