#![allow(dead_code)]

use crate::core::MediaKind;
use crate::streams::Stream;
use anyhow::Result;
use bytes::BytesMut;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp;
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

        let h264_codec = RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        };

        media_engine.register_codec(
            rtc::rtp_transceiver::rtp_sender::RTCRtpCodecParameters {
                rtp_codec: h264_codec.clone(),
                payload_type: 96,
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
        for track in tracks {
            if track.kind == MediaKind::Video {
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
                            codec: h264_codec.clone(),
                            ..Default::default()
                        }],
                    )));

                peer_connection
                    .add_track(Arc::clone(&video_track) as Arc<dyn TrackLocal>)
                    .await?;

                let mut rx = track.subscribe();
                let vtrack = Arc::clone(&video_track);

                tokio::spawn(async move {
                    while let Ok(packet) = rx.recv().await {
                        let mut bytes = BytesMut::from(&packet.raw[..]);
                        if let Ok(mut rtp_pkt) = rtp::packet::Packet::unmarshal(&mut bytes) {
                            rtp_pkt.header.ssrc = video_ssrc;
                            if vtrack.write_rtp(rtp_pkt).await.is_err() {
                                break;
                            }
                        }
                    }
                });
                break;
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

        if let Some(local_desc) = peer_connection.local_description().await {
            Ok(local_desc.sdp)
        } else {
            anyhow::bail!("Failed to generate local SDP answer")
        }
    }
}
