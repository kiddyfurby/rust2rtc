#![allow(dead_code)]

use crate::core::{MediaKind, Track};
use crate::ffmpeg::FfmpegManager;
use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::{Mutex, RwLock};
use tracing::info;

pub struct ActiveProducer {
    pub source: String,
    pub child: Option<Child>,
}

pub struct Stream {
    pub name: String,
    pub sources: Vec<String>,
    pub tracks: RwLock<Vec<Arc<Track>>>,
    pub producer: Mutex<Option<ActiveProducer>>,
    pub bytes_recv: AtomicU64,
    pub packets_recv: AtomicU64,
    pub backchannel: Mutex<Option<Arc<crate::backchannel::BackchannelSession>>>,
    pub opus_track: Arc<Mutex<Option<Arc<Track>>>>,
}

impl Stream {
    pub fn new(name: String, sources: Vec<String>) -> Self {
        Self {
            name,
            sources,
            tracks: RwLock::new(Vec::new()),
            producer: Mutex::new(None),
            bytes_recv: AtomicU64::new(0),
            packets_recv: AtomicU64::new(0),
            backchannel: Mutex::new(None),
            opus_track: Arc::new(Mutex::new(None)),
        }
    }

    /// Check if this stream has a configured backchannel source (Reolink ONVIF or Tapo MULTITRANS)
    pub fn has_backchannel(&self) -> bool {
        crate::backchannel::detect_backchannel(&self.sources).is_some()
    }

    /// Start a backchannel session on demand if not already active
    pub async fn start_backchannel(&self) -> Result<Option<Arc<crate::backchannel::BackchannelSession>>> {
        let mut guard = self.backchannel.lock().await;
        if let Some(session) = &*guard {
            return Ok(Some(Arc::clone(session)));
        }

        if let Some(session) = crate::backchannel::start_backchannel(&self.sources).await? {
            let session_arc = Arc::new(session);
            *guard = Some(Arc::clone(&session_arc));
            Ok(Some(session_arc))
        } else {
            Ok(None)
        }
    }

    /// Stop the active backchannel session
    pub async fn stop_backchannel(&self) {
        let mut guard = self.backchannel.lock().await;
        *guard = None;
    }

    /// Send audio payload to the backchannel speaker
    pub async fn send_backchannel_audio(&self, payload: Bytes, is_pcmu: bool) -> Result<()> {
        let guard = self.backchannel.lock().await;
        if let Some(session) = &*guard {
            session.send_audio(payload, is_pcmu).await?;
        }
        Ok(())
    }

    pub async fn add_track(&self, track: Arc<Track>) {
        let mut tracks = self.tracks.write().await;
        tracks.push(track);
    }

    pub async fn clear_tracks(&self) {
        let mut tracks = self.tracks.write().await;
        tracks.clear();
        let mut opus = self.opus_track.lock().await;
        *opus = None;
    }

    pub async fn get_tracks(&self) -> Vec<Arc<Track>> {
        let tracks = self.tracks.read().await;
        tracks.clone()
    }

    /// Get or dynamically spawn a shared in-process Opus transcoding track
    pub async fn get_or_create_opus_track(&self) -> Option<Arc<Track>> {
        // 1. If this stream already has a native OPUS track, return it directly
        let tracks = self.tracks.read().await;
        for t in tracks.iter() {
            if t.kind == MediaKind::Audio && t.codec.name.eq_ignore_ascii_case("OPUS") {
                return Some(Arc::clone(t));
            }
        }

        // 2. Check if a shared transcoded Opus track is already active
        let mut guard = self.opus_track.lock().await;
        if let Some(track) = &*guard {
            return Some(Arc::clone(track));
        }

        // 3. Find an audio track (AAC, PCMU, PCMA) to transcode from
        let source_audio_track = tracks.iter().find(|t| t.kind == MediaKind::Audio)?.clone();
        drop(tracks);

        // 4. Create shared Opus track
        let opus_track = Arc::new(Track::new(crate::core::Codec::new_opus(), MediaKind::Audio, 50));
        *guard = Some(Arc::clone(&opus_track));

        // 5. Spawn background in-process audio transcode worker
        let stream_name = self.name.clone();
        let track_for_worker = Arc::clone(&opus_track);
        let opus_track_slot = Arc::clone(&self.opus_track);
        tokio::spawn(async move {
            info!(stream = %stream_name, codec = %source_audio_track.codec.name, "Starting shared in-process Opus audio transcode worker");
            let mut transcoder = match crate::audio::AudioTranscoder::new(&source_audio_track.codec) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(stream = %stream_name, "Failed to initialize AudioTranscoder: {}", e);
                    let mut guard = opus_track_slot.lock().await;
                    *guard = None;
                    return;
                }
            };

            let mut rx = source_audio_track.subscribe();
            let mut idle_ticks = 0;

            loop {
                // If no viewers are listening to this Opus track for ~60 seconds, shut down worker to save resources
                if track_for_worker.consumer_count() == 0 {
                    idle_ticks += 1;
                    if idle_ticks > 3000 { // ~60 seconds with 20ms audio packets
                        info!(stream = %stream_name, "No active consumers on shared Opus track after 60s, terminating transcode worker");
                        break;
                    }
                } else {
                    idle_ticks = 0;
                }

                match rx.recv().await {
                    Ok(packet) => {
                        let opus_packets = transcoder.process_packet(&packet);
                        for opus_pkt in opus_packets {
                            track_for_worker.send(opus_pkt);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(stream = %stream_name, lagged = n, "Audio transcoder lagged");
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }

            // Always reset the slot so subsequent connections restart a fresh worker
            let mut guard = opus_track_slot.lock().await;
            *guard = None;
            info!(stream = %stream_name, "Shared Opus audio transcode worker terminated and slot cleared");
        });

        Some(opus_track)
    }

    /// Ensure that a producer is actively running for this stream
    pub async fn ensure_started(&self, rtsp_port: u16, ffmpeg: &FfmpegManager) -> Result<()> {
        let mut producer_guard = self.producer.lock().await;
        if producer_guard.is_some() {
            return Ok(());
        }

        if self.sources.is_empty() {
            return Ok(());
        }

        let source = self
            .sources
            .iter()
            .find(|s| !s.starts_with("multitrans://") && !s.contains("#video=mjpeg") && !s.starts_with("ffmpeg:"))
            .or_else(|| self.sources.iter().find(|s| !s.starts_with("multitrans://")))
            .unwrap_or(&self.sources[0]);
        info!(stream = %self.name, source = %source, "Starting on-demand producer");

        let (bin, args) = ffmpeg.build_command(source, rtsp_port, &self.name);
        let child = ffmpeg.spawn_process(&bin, &args)?;

        *producer_guard = Some(ActiveProducer {
            source: source.clone(),
            child: Some(child),
        });

        Ok(())
    }

    /// Stop any active producer
    pub async fn stop(&self) {
        let mut producer_guard = self.producer.lock().await;
        if let Some(mut prod) = producer_guard.take() {
            if let Some(mut child) = prod.child.take() {
                info!(stream = %self.name, "Stopping stream producer");
                let _ = child.kill().await;
            }
        }
    }

    /// Generate SDP string for RTSP DESCRIBE
    pub async fn generate_sdp(&self, server_host: &str) -> String {
        let tracks = self.tracks.read().await;
        let mut sdp = String::new();

        sdp.push_str("v=0\r\n");
        sdp.push_str(&format!("o=- 0 0 IN IP4 {}\r\n", server_host));
        sdp.push_str("s=rust2rtc\r\n");
        sdp.push_str("c=IN IP4 0.0.0.0\r\n");
        sdp.push_str("t=0 0\r\n");

        if tracks.is_empty() {
            // Default video dummy track if not yet announced
            sdp.push_str("m=video 0 RTP/AVP 96\r\n");
            sdp.push_str("a=rtpmap:96 H264/90000\r\n");
            sdp.push_str("a=control:track0\r\n");
        } else {
            for (idx, track) in tracks.iter().enumerate() {
                let kind_str = match track.kind {
                    MediaKind::Video => "video",
                    MediaKind::Audio => "audio",
                };

                sdp.push_str(&format!(
                    "m={} 0 RTP/AVP {}\r\n",
                    kind_str, track.codec.payload_type
                ));
                let rtpmap_suffix = match track.codec.channels {
                    Some(ch) if track.kind == MediaKind::Audio => format!("/{}", ch),
                    _ => String::new(),
                };
                sdp.push_str(&format!(
                    "a=rtpmap:{} {}/{}{}\r\n",
                    track.codec.payload_type, track.codec.name, track.codec.clock_rate, rtpmap_suffix
                ));

                if let Some(ref fmtp) = track.codec.fmtp {
                    sdp.push_str(&format!(
                        "a=fmtp:{} {}\r\n",
                        track.codec.payload_type, fmtp
                    ));
                }

                sdp.push_str(&format!("a=control:track{}\r\n", idx));
            }
        }

        sdp
    }

    pub async fn to_info(&self) -> StreamInfo {
        let tracks = self.tracks.read().await;
        let mut track_infos = Vec::new();

        let mut total_consumers = 0;
        let mut total_packets = 0;
        let mut total_bytes = 0;

        for track in tracks.iter() {
            let consumers = track.consumer_count();
            let packets = track.packets_total.load(Ordering::Relaxed);
            let bytes = track.bytes_total.load(Ordering::Relaxed);

            total_consumers += consumers;
            total_packets += packets;
            total_bytes += bytes;

            track_infos.push(TrackInfo {
                kind: track.kind,
                codec: track.codec.name.clone(),
                clock_rate: track.codec.clock_rate,
                consumers,
                packets,
                bytes,
            });
        }

        let is_active = self.producer.lock().await.is_some();

        StreamInfo {
            name: self.name.clone(),
            sources: self.sources.clone(),
            active: is_active,
            consumers: total_consumers,
            packets_recv: total_packets,
            bytes_recv: total_bytes,
            tracks: track_infos,
        }
    }

    /// Convert to go2rtc-compatible stream schema for Home Assistant / Frigate / advanced-camera-card
    pub async fn to_go2rtc(&self) -> Go2rtcStream {
        let tracks = self.tracks.read().await;
        let mut total_consumers = 0;
        let mut total_packets = self.packets_recv.load(Ordering::Relaxed);
        let mut total_bytes = self.bytes_recv.load(Ordering::Relaxed);
        let mut medias = Vec::new();

        for track in tracks.iter() {
            total_consumers += track.consumer_count();
            total_packets += track.packets_total.load(Ordering::Relaxed);
            total_bytes += track.bytes_total.load(Ordering::Relaxed);
            medias.push(format!(
                "{:?}, sendonly, {}/{}",
                track.kind, track.codec.name, track.codec.clock_rate
            ));
        }

        let is_active = self.producer.lock().await.is_some() || !tracks.is_empty();
        let source_url = self.sources.first().cloned().unwrap_or_else(|| "unknown".to_string());

        let mut producers = Vec::new();
        if is_active {
            producers.push(Go2rtcProducer {
                id: Some(1),
                producer_type: "RTSP active producer".to_string(),
                url: source_url,
                remote_addr: None,
                medias: medias.clone(),
                recv: total_bytes,
                recv_packets: Some(total_packets),
            });
        } else if !self.sources.is_empty() {
            producers.push(Go2rtcProducer {
                id: None,
                producer_type: "configured".to_string(),
                url: source_url,
                remote_addr: None,
                medias: vec![],
                recv: 0,
                recv_packets: Some(0),
            });
        }

        let mut consumers = Vec::new();
        if total_consumers > 0 {
            consumers.push(Go2rtcConsumer {
                id: Some(2),
                consumer_type: "WebRTC / RTSP client".to_string(),
                remote_addr: None,
                medias,
                send: Some(total_bytes),
            });
        }

        Go2rtcStream {
            producers,
            consumers,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Go2rtcProducer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(rename = "type")]
    pub producer_type: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    pub medias: Vec<String>,
    pub recv: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recv_packets: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Go2rtcConsumer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(rename = "type")]
    pub consumer_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
    pub medias: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub send: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Go2rtcStream {
    pub producers: Vec<Go2rtcProducer>,
    pub consumers: Vec<Go2rtcConsumer>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TrackInfo {
    pub kind: MediaKind,
    pub codec: String,
    pub clock_rate: u32,
    pub consumers: usize,
    pub packets: u64,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamInfo {
    pub name: String,
    pub sources: Vec<String>,
    pub active: bool,
    pub consumers: usize,
    pub packets_recv: u64,
    pub bytes_recv: u64,
    pub tracks: Vec<TrackInfo>,
}

pub struct StreamManager {
    streams: RwLock<HashMap<String, Arc<Stream>>>,
}

impl StreamManager {
    pub fn new() -> Self {
        Self {
            streams: RwLock::new(HashMap::new()),
        }
    }

    pub async fn add_configured_stream(&self, name: String, sources: Vec<String>) {
        let mut map = self.streams.write().await;
        map.insert(name.clone(), Arc::new(Stream::new(name, sources)));
    }

    pub async fn get(&self, name: &str) -> Option<Arc<Stream>> {
        let map = self.streams.read().await;
        map.get(name).cloned()
    }

    pub async fn get_or_create(&self, name: &str) -> Arc<Stream> {
        let mut map = self.streams.write().await;
        if let Some(stream) = map.get(name) {
            return Arc::clone(stream);
        }
        let stream = Arc::new(Stream::new(name.to_string(), vec![]));
        map.insert(name.to_string(), Arc::clone(&stream));
        stream
    }

    pub async fn get_all_infos(&self) -> Vec<StreamInfo> {
        let map = self.streams.read().await;
        let mut list = Vec::new();
        for stream in map.values() {
            list.push(stream.to_info().await);
        }
        list
    }

    pub async fn get_go2rtc_streams(&self) -> HashMap<String, Go2rtcStream> {
        let map = self.streams.read().await;
        let mut result = HashMap::new();
        for (name, stream) in map.iter() {
            result.insert(name.clone(), stream.to_go2rtc().await);
        }
        result
    }

    pub async fn get_go2rtc_stream(&self, name: &str) -> Option<Go2rtcStream> {
        let map = self.streams.read().await;
        if let Some(stream) = map.get(name) {
            Some(stream.to_go2rtc().await)
        } else {
            None
        }
    }

    pub async fn delete(&self, name: &str) -> bool {
        let mut map = self.streams.write().await;
        map.remove(name).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_go2rtc_stream_schema() {
        let stream = Stream::new("doorbell".to_string(), vec!["rtsp://198.51.100.10/stream".to_string()]);
        let go2rtc_info = stream.to_go2rtc().await;

        let json_val = serde_json::to_value(&go2rtc_info).expect("serialize to json");
        assert!(json_val.get("producers").is_some());
        assert!(json_val.get("consumers").is_some());

        let producers = json_val.get("producers").unwrap().as_array().unwrap();
        assert_eq!(producers.len(), 1);
        assert_eq!(producers[0].get("url").unwrap(), "rtsp://198.51.100.10/stream");
    }
}
