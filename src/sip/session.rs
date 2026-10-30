#![allow(dead_code)]

use crate::core::{MediaKind, RtpPacket};
use crate::streams::Stream;
use anyhow::{Context, Result};
use bytes::Bytes;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Active SIP Media Bridge Session for a Call
pub struct SipCallSession {
    pub call_id: String,
    pub local_audio_port: u16,
    pub local_video_port: Option<u16>,
    tasks: Vec<JoinHandle<()>>,
    stream: Arc<Stream>,
}

impl SipCallSession {
    /// Start bidirectional RTP audio/video bridge between SIP peer and Stream
    pub async fn start(
        call_id: String,
        stream: Arc<Stream>,
        local_bind_ip: &str,
        remote_audio_addr: SocketAddr,
        remote_video_addr: Option<SocketAddr>,
    ) -> Result<Self> {
        // Bind local UDP socket for audio
        let audio_socket = UdpSocket::bind(format!("{}:0", local_bind_ip))
            .await
            .context("Failed to bind local audio RTP socket")?;
        let local_audio_port = audio_socket.local_addr()?.port();
        let audio_socket = Arc::new(audio_socket);

        info!(
            call_id = %call_id,
            stream = %stream.name,
            local_audio = local_audio_port,
            remote_audio = %remote_audio_addr,
            "Starting SIP call media bridge"
        );

        let mut tasks = Vec::new();

        // Ensure backchannel is active on camera
        if stream.has_backchannel() {
            if let Err(e) = stream.start_backchannel().await {
                warn!("Could not start backchannel for stream: {}", e);
            }
        }

        // 1. Task: Receive audio from ESPHome/SIP peer -> Feed to camera backchannel
        let stream_bc = Arc::clone(&stream);
        let rx_audio_sock = Arc::clone(&audio_socket);
        let call_id_clone = call_id.clone();
        tasks.push(tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            loop {
                match rx_audio_sock.recv_from(&mut buf).await {
                    Ok((len, _addr)) => {
                        let packet_bytes = Bytes::copy_from_slice(&buf[..len]);
                        if let Some(rtp) = RtpPacket::parse(packet_bytes) {
                            let is_pcmu = rtp.payload_type == 0;
                            let payload = Bytes::copy_from_slice(rtp.payload());
                            let _ = stream_bc.send_backchannel_audio(payload, is_pcmu).await;
                        }
                    }
                    Err(e) => {
                        debug!(call_id = %call_id_clone, "Audio RTP receive loop ended: {}", e);
                        break;
                    }
                }
            }
        }));

        // 2. Task: Send doorbell microphone audio -> ESPHome/SIP peer
        let stream_tx = Arc::clone(&stream);
        let tx_audio_sock = Arc::clone(&audio_socket);
        tasks.push(tokio::spawn(async move {
            let tracks = stream_tx.get_tracks().await;
            if let Some(audio_track) = tracks.iter().find(|t| t.kind == MediaKind::Audio) {
                let mut sub = audio_track.subscribe();
                while let Ok(packet) = sub.recv().await {
                    if let Err(e) = tx_audio_sock.send_to(&packet.raw, remote_audio_addr).await {
                        debug!("Error forwarding audio to SIP peer: {}", e);
                        break;
                    }
                }
            }
        }));

        // 3. Task: Send doorbell camera video (H.264) -> ESPHome / HA Softphone
        let mut local_video_port = None;
        if let Some(remote_video) = remote_video_addr {
            let video_socket = UdpSocket::bind(format!("{}:0", local_bind_ip))
                .await
                .context("Failed to bind local video RTP socket")?;
            let v_port = video_socket.local_addr()?.port();
            local_video_port = Some(v_port);
            let video_socket = Arc::new(video_socket);

            let stream_video = Arc::clone(&stream);
            tasks.push(tokio::spawn(async move {
                let tracks = stream_video.get_tracks().await;
                if let Some(video_track) = tracks.iter().find(|t| t.kind == MediaKind::Video) {
                    let mut sub = video_track.subscribe();
                    while let Ok(packet) = sub.recv().await {
                        if let Err(e) = video_socket.send_to(&packet.raw, remote_video).await {
                            debug!("Error forwarding video to SIP peer: {}", e);
                            break;
                        }
                    }
                }
            }));
        }

        Ok(Self {
            call_id,
            local_audio_port,
            local_video_port,
            tasks,
            stream,
        })
    }

    /// Stop the call session and release backchannel resources
    pub async fn stop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.stream.stop_backchannel().await;
        info!(call_id = %self.call_id, "SIP call media bridge stopped");
    }
}

impl Drop for SipCallSession {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
