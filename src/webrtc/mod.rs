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
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::runtime::{channel, Sender};

#[derive(Clone)]
struct WebRtcHandler {
    gather_complete_tx: Sender<()>,
    connected_tx: Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for WebRtcHandler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_complete_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        debug!(state = %state, "WebRTC peer connection state changed");
        if state == RTCPeerConnectionState::Connected {
            let _ = self.connected_tx.try_send(());
        }
    }
}

#[derive(Clone)]
pub struct WebRtcManager {
    candidates: Vec<String>,
}

impl WebRtcManager {
    pub fn new(candidates: Vec<String>) -> Self {
        Self { candidates }
    }

    pub async fn handle_whep(&self, stream: Arc<Stream>, offer_sdp: &str) -> Result<String> {
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

        let (gather_complete_tx, mut gather_complete_rx) = channel::<()>(1);
        let (connected_tx, mut _connected_rx) = channel::<()>(1);

        let handler = Arc::new(WebRtcHandler {
            gather_complete_tx,
            connected_tx,
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
            } else if track.kind == MediaKind::Audio && audio_track_info.is_none() {
                let name = track.codec.name.to_uppercase();
                let (audio_codec, default_pt) = if name.contains("OPUS") {
                    (opus_codec.clone(), 111)
                } else if name.contains("PCMU") {
                    (pcmu_codec.clone(), 0)
                } else {
                    (pcma_codec.clone(), 8)
                };

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
                        codec: audio_codec,
                        ..Default::default()
                    }],
                )));

                if let Err(e) = peer_connection
                    .add_track(Arc::clone(&audio_track) as Arc<dyn TrackLocal>)
                    .await
                {
                    tracing::warn!("Failed to add audio track: {}", e);
                } else {
                    let rx = track.subscribe();
                    audio_track_info = Some((audio_track, rx, audio_ssrc, default_pt));
                }
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

        // Spawn video RTP forwarder after the track is bound
        if let Some((vtrack, mut rx, video_ssrc, is_hevc, is_high, mut param_set_payload, default_pt)) = video_track_info {
            let actual_pt = find_negotiated_video_pt(&local_desc.sdp, is_hevc, is_high, default_pt);
            tracing::info!(stream = %stream.name, pt = actual_pt, is_hevc = is_hevc, "Starting WebRTC video forwarder");

            tokio::spawn(async move {
                let mut seq: u16 = 0;
                let mut sent_first_keyframe = false;
                let mut last_was_param_set = false;

                while let Ok(packet) = rx.recv().await {
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
                        // Do not drop parameter sets or keyframes
                        if !is_keyframe && !is_param_set {
                            continue;
                        }
                        sent_first_keyframe = true;
                    }

                    // Before keyframes, inject the parameter sets packet (STAP-A / AP) if not immediately preceded by parameter sets
                    if is_keyframe && !last_was_param_set {
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

                    last_was_param_set = is_param_set;

                    let mut bytes = BytesMut::from(&packet.raw[..]);
                    if let Ok(mut rtp_pkt) = rtp::packet::Packet::unmarshal(&mut bytes) {
                        rtp_pkt.header.ssrc = video_ssrc;
                        rtp_pkt.header.payload_type = actual_pt;
                        rtp_pkt.header.sequence_number = seq;
                        seq = seq.wrapping_add(1);

                        if vtrack.write_rtp(rtp_pkt).await.is_err() {
                            // Transient error or disconnected
                        }
                    }
                }
            });
        }

        // Spawn audio RTP forwarder if audio track was negotiated
        if let Some((atrack, mut rx, audio_ssrc, default_pt)) = audio_track_info {
            let actual_audio_pt = find_negotiated_audio_pt(&local_desc.sdp, default_pt);
            tracing::info!(stream = %stream.name, pt = actual_audio_pt, "Starting WebRTC audio forwarder");

            tokio::spawn(async move {
                let mut seq: u16 = 0;
                while let Ok(packet) = rx.recv().await {
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
        }

        Ok(local_desc.sdp)
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

fn is_keyframe_or_param_set(is_hevc: bool, payload: &[u8]) -> (bool, bool) {
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
