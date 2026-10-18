#![allow(dead_code)]

use crate::core::{MediaKind, Track};
use crate::ffmpeg::FfmpegManager;
use anyhow::Result;
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
        }
    }

    pub async fn add_track(&self, track: Arc<Track>) {
        let mut tracks = self.tracks.write().await;
        tracks.push(track);
    }

    pub async fn clear_tracks(&self) {
        let mut tracks = self.tracks.write().await;
        tracks.clear();
    }

    pub async fn get_tracks(&self) -> Vec<Arc<Track>> {
        let tracks = self.tracks.read().await;
        tracks.clone()
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
            .find(|s| !s.starts_with("multitrans://") && !s.contains("#video=mjpeg"))
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
                sdp.push_str(&format!(
                    "a=rtpmap:{} {}/{}\r\n",
                    track.codec.payload_type, track.codec.name, track.codec.clock_rate
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
}
