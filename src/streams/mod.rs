#![allow(dead_code)]

use crate::core::{MediaKind, Track};
use crate::ffmpeg::FfmpegManager;
use anyhow::Result;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::{broadcast, oneshot, watch, Mutex, RwLock};
use tracing::{debug, info};

pub struct ActiveProducer {
    pub source: String,
    pub child: Option<Child>,
    pub cancel_tx: Option<oneshot::Sender<()>>,
}

pub struct Stream {
    pub name: String,
    pub sources: Vec<String>,
    pub tracks: RwLock<Vec<Arc<Track>>>,
    track_count_tx: watch::Sender<usize>,
    pub producer: Mutex<Option<ActiveProducer>>,
    pub bytes_recv: AtomicU64,
    pub packets_recv: AtomicU64,
    pub backchannel: Mutex<Option<Arc<crate::backchannel::BackchannelSession>>>,
    pub opus_track: Arc<Mutex<Option<Arc<Track>>>>,
    pub mjpeg_broadcaster: Arc<Mutex<Option<broadcast::Sender<Bytes>>>>,
}

impl Stream {
    pub fn new(name: String, sources: Vec<String>) -> Self {
        let (track_count_tx, _) = watch::channel(0);
        Self {
            name,
            sources,
            tracks: RwLock::new(Vec::new()),
            track_count_tx,
            producer: Mutex::new(None),
            bytes_recv: AtomicU64::new(0),
            packets_recv: AtomicU64::new(0),
            backchannel: Mutex::new(None),
            opus_track: Arc::new(Mutex::new(None)),
            mjpeg_broadcaster: Arc::new(Mutex::new(None)),
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
        let len = tracks.len();
        drop(tracks);
        let _ = self.track_count_tx.send_replace(len);
    }

    pub async fn set_tracks(&self, new_tracks: Vec<Arc<Track>>) {
        let mut tracks = self.tracks.write().await;
        *tracks = new_tracks;
        let len = tracks.len();
        drop(tracks);
        let mut opus = self.opus_track.lock().await;
        *opus = None;
        let mut mjpeg = self.mjpeg_broadcaster.lock().await;
        *mjpeg = None;
        let _ = self.track_count_tx.send_replace(len);
    }

    pub async fn clear_tracks(&self) {
        let mut tracks = self.tracks.write().await;
        for t in tracks.iter() {
            t.clear_gop_cache();
        }
        tracks.clear();
        drop(tracks);
        let mut opus = self.opus_track.lock().await;
        *opus = None;
        let mut mjpeg = self.mjpeg_broadcaster.lock().await;
        *mjpeg = None;
        let _ = self.track_count_tx.send_replace(0);
    }

    pub async fn get_tracks(&self) -> Vec<Arc<Track>> {
        let tracks = self.tracks.read().await;
        tracks.clone()
    }

    /// Wait until at least one track is registered on this stream, or until `timeout` elapses.
    pub async fn wait_for_tracks(&self, timeout: std::time::Duration) {
        if !self.tracks.read().await.is_empty() {
            return;
        }
        let mut rx = self.track_count_tx.subscribe();
        let _ = tokio::time::timeout(timeout, rx.wait_for(|&c| c > 0)).await;
    }

    /// Wait until a track matching `predicate` is registered, or until `timeout` elapses.
    pub async fn wait_for_track<F>(&self, timeout: std::time::Duration, mut predicate: F) -> Option<Arc<Track>>
    where
        F: FnMut(&Arc<Track>) -> bool,
    {
        let mut rx = self.track_count_tx.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let tracks = self.tracks.read().await;
                if let Some(t) = tracks.iter().find(|t| predicate(t)) {
                    return Some(Arc::clone(t));
                }
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return None;
            }
            if tokio::time::timeout(deadline - now, rx.changed()).await.is_err() {
                return None;
            }
        }
    }

    /// Get or dynamically spawn a shared in-process Opus transcoding track
    pub async fn get_or_create_opus_track(&self) -> Option<Arc<Track>> {
        // 1. If this stream already has a native WebRTC audio track (OPUS, PCMA, PCMU), return it directly without transcoding
        let tracks = self.tracks.read().await;
        for t in tracks.iter() {
            if t.kind == MediaKind::Audio {
                let name = t.codec.name.to_uppercase();
                if name.contains("OPUS") || name.contains("PCMA") || name.contains("PCMU") {
                    return Some(Arc::clone(t));
                }
            }
        }

        // 2. Check if a shared transcoded Opus track is already active
        let mut guard = self.opus_track.lock().await;
        if let Some(track) = &*guard {
            return Some(Arc::clone(track));
        }

        // 3. Find an audio track (e.g. AAC / MPEG4-GENERIC) to transcode from
        let source_audio_track = tracks.iter().find(|t| t.kind == MediaKind::Audio)?.clone();
        drop(tracks);

        // 4. Create shared Opus track
        let opus_track = Arc::new(Track::new(crate::core::Codec::new_opus(), MediaKind::Audio, 256));
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
                // If no viewers are listening to this Opus track for ~5 seconds, shut down worker to save CPU
                if track_for_worker.consumer_count() == 0 {
                    idle_ticks += 1;
                    if idle_ticks > 250 { // ~5 seconds with 20ms audio packets
                        info!(stream = %stream_name, "No active consumers on shared Opus track after 5s, terminating transcode worker");
                        break;
                    }
                } else {
                    idle_ticks = 0;
                }

                match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                    Ok(Ok(packet)) => {
                        let opus_packets = transcoder.process_packet(&packet);
                        for opus_pkt in opus_packets {
                            track_for_worker.send(opus_pkt);
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(n))) => {
                        tracing::warn!(stream = %stream_name, lagged = n, "Audio transcoder lagged");
                        continue;
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        break;
                    }
                    Err(_) => {
                        if track_for_worker.consumer_count() == 0 {
                            info!(stream = %stream_name, "Shared Opus source idle with no consumers, terminating transcode worker");
                            break;
                        }
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

    /// Get or dynamically spawn a shared in-process FFmpeg MJPEG broadcaster
    pub async fn get_or_create_mjpeg_broadcaster(
        self: &Arc<Self>,
        rtsp_port: u16,
        ffmpeg: &FfmpegManager,
    ) -> Option<broadcast::Sender<Bytes>> {
        let mut guard = self.mjpeg_broadcaster.lock().await;
        if let Some(ref sender) = *guard {
            if sender.receiver_count() > 0 {
                return Some(sender.clone());
            }
        }

        let custom_pipeline = self.sources.iter().find(|s| s.contains("#video=mjpeg")).cloned();
        let source = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, self.name);
        let args = ffmpeg.build_mjpeg_stream_args(&source, custom_pipeline.as_deref(), None, None, None, None);

        let mut child = match ffmpeg.spawn_stdout_process(&args) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(stream = %self.name, "Failed to spawn shared FFmpeg MJPEG transcoder: {}", e);
                return None;
            }
        };

        let mut stdout = child.stdout.take()?;

        let (tx, _rx) = broadcast::channel::<Bytes>(16);
        let tx_clone = tx.clone();
        *guard = Some(tx.clone());

        let stream_name = self.name.clone();
        let slot = Arc::clone(&self.mjpeg_broadcaster);

        tokio::spawn(async move {
            info!(stream = %stream_name, "Starting shared MJPEG broadcaster worker");
            let mut stream_buf = bytes::BytesMut::with_capacity(131072);
            let mut scan_pos = 0usize;
            let soi_finder = memchr::memmem::Finder::new(&[0xFF, 0xD8]);
            let eoi_finder = memchr::memmem::Finder::new(&[0xFF, 0xD9]);
            let mut idle_seconds = 0;

            loop {
                tokio::select! {
                    read_res = stdout.read_buf(&mut stream_buf) => {
                        let _n = match read_res {
                            Ok(0) => {
                                debug!(stream = %stream_name, "FFmpeg MJPEG stdout reached EOF");
                                break;
                            }
                            Ok(n) => n,
                            Err(e) => {
                                debug!(stream = %stream_name, "Error reading from FFmpeg MJPEG stdout: {}", e);
                                break;
                            }
                        };

                        loop {
                            // 1. Ensure buffer starts with JPEG SOI ([0xFF, 0xD8])
                            if !stream_buf.starts_with(&[0xFF, 0xD8]) {
                                if let Some(soi_pos) = soi_finder.find(&stream_buf) {
                                    let _ = stream_buf.split_to(soi_pos);
                                    scan_pos = 2;
                                } else {
                                    // No SOI in buffer; preserve at most 1 trailing byte if 0xFF
                                    if stream_buf.ends_with(&[0xFF]) {
                                        let drop_len = stream_buf.len() - 1;
                                        let _ = stream_buf.split_to(drop_len);
                                    } else {
                                        stream_buf.clear();
                                    }
                                    scan_pos = 0;
                                    break;
                                }
                            }

                            if stream_buf.len() < 4 {
                                break;
                            }

                            // 2. Scan for JPEG EOI ([0xFF, 0xD9]) starting from scan_pos - 1
                            let search_start = scan_pos.saturating_sub(1).max(2);
                            if let Some(rel) = eoi_finder.find(&stream_buf[search_start..]) {
                                let eoi_end = search_start + rel + 2;
                                let jpeg = stream_buf.split_to(eoi_end).freeze();
                                scan_pos = 0;

                                let part_header = format!(
                                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                    jpeg.len()
                                );
                                let mut packet = bytes::BytesMut::with_capacity(part_header.len() + jpeg.len() + 2);
                                packet.extend_from_slice(part_header.as_bytes());
                                packet.extend_from_slice(&jpeg);
                                packet.extend_from_slice(b"\r\n");

                                let _ = tx_clone.send(packet.freeze());
                            } else {
                                scan_pos = stream_buf.len();
                                break;
                            }
                        }
                    }
                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {
                        if tx_clone.receiver_count() == 0 {
                            idle_seconds += 1;
                            if idle_seconds >= 8 {
                                info!(stream = %stream_name, "No active consumers on shared MJPEG stream for 8s, terminating worker");
                                break;
                            }
                        } else {
                            idle_seconds = 0;
                        }
                    }
                }
            }

            let _ = child.kill().await;
            let mut guard = slot.lock().await;
            *guard = None;
            info!(stream = %stream_name, "Shared MJPEG broadcaster worker terminated and slot cleared");
        });

        Some(tx)
    }

    /// Ensure that a producer is actively running for this stream
    pub async fn ensure_started(self: &Arc<Self>, rtsp_port: u16, ffmpeg: &FfmpegManager) -> Result<()> {
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

        let resolved = crate::ffmpeg::resolve_source(source, rtsp_port);

        // If source resolves to standard RTSP, ingest natively in-process (zero FFmpeg subprocess)
        if resolved.starts_with("rtsp://") {
            info!(stream = %self.name, source = %resolved, "Starting native in-process RTSP producer");
            let (cancel_tx, cancel_rx) = oneshot::channel();
            let self_clone = Arc::clone(self);
            let resolved_clone = resolved.clone();

            tokio::spawn(async move {
                crate::rtsp::RtspClient::run(resolved_clone, self_clone, cancel_rx).await;
            });

            *producer_guard = Some(ActiveProducer {
                source: source.clone(),
                child: None,
                cancel_tx: Some(cancel_tx),
            });

            return Ok(());
        }

        info!(stream = %self.name, source = %source, "Starting on-demand FFmpeg fallback producer");
        let (bin, args) = ffmpeg.build_command(source, rtsp_port, &self.name);
        let child = ffmpeg.spawn_process(&bin, &args)?;

        *producer_guard = Some(ActiveProducer {
            source: source.clone(),
            child: Some(child),
            cancel_tx: None,
        });

        Ok(())
    }

    /// Stop any active producer
    pub async fn stop(&self) {
        let mut producer_guard = self.producer.lock().await;
        if let Some(mut prod) = producer_guard.take() {
            if let Some(cancel_tx) = prod.cancel_tx {
                info!(stream = %self.name, "Stopping native in-process RTSP stream");
                let _ = cancel_tx.send(());
            }
            if let Some(mut child) = prod.child.take() {
                info!(stream = %self.name, "Stopping FFmpeg stream producer");
                let _ = child.kill().await;
            }
        }
        let mut mjpeg = self.mjpeg_broadcaster.lock().await;
        *mjpeg = None;
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

                let effective_fmtp = if let Some(ref fmtp) = track.codec.fmtp {
                    if fmtp.contains("sprop-parameter-sets") || fmtp.contains("sprop-sps") {
                        Some(fmtp.clone())
                    } else if let Some(sprop) = track.get_sprop() {
                        Some(format!("{};{}", fmtp, sprop))
                    } else {
                        Some(fmtp.clone())
                    }
                } else { track.get_sprop().map(|sprop| format!("packetization-mode=1;{}", sprop)) };

                if let Some(ref fmtp) = effective_fmtp {
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

        let mjpeg_consumers = {
            let guard = self.mjpeg_broadcaster.lock().await;
            guard.as_ref().map(|s| s.receiver_count()).unwrap_or(0)
        };
        total_consumers += mjpeg_consumers;

        let is_active = self.producer.lock().await.is_some() || mjpeg_consumers > 0;

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

        let mjpeg_consumers = {
            let guard = self.mjpeg_broadcaster.lock().await;
            guard.as_ref().map(|s| s.receiver_count()).unwrap_or(0)
        };
        total_consumers += mjpeg_consumers;

        let is_active = self.producer.lock().await.is_some() || !tracks.is_empty() || mjpeg_consumers > 0;
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

        if mjpeg_consumers > 0 {
            consumers.push(Go2rtcConsumer {
                id: Some(3),
                consumer_type: "HTTP MJPEG client".to_string(),
                remote_addr: None,
                medias: vec!["Video, sendonly, JPEG/90000".to_string()],
                send: None,
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

    #[tokio::test]
    async fn test_mjpeg_consumer_reporting() {
        let stream = Stream::new("garden".to_string(), vec!["rtsp://198.51.100.20/stream".to_string()]);
        let (tx, rx) = broadcast::channel(16);
        drop(rx); // Drop initial receiver so receiver_count is 0
        {
            let mut guard = stream.mjpeg_broadcaster.lock().await;
            *guard = Some(tx.clone());
        }

        // Before any subscriber
        let info = stream.to_info().await;
        assert_eq!(info.consumers, 0);

        // Subscribe an active consumer
        let _sub = tx.subscribe();
        let info_with_consumer = stream.to_info().await;
        assert_eq!(info_with_consumer.consumers, 1);
        assert!(info_with_consumer.active);

        let go2rtc_stream = stream.to_go2rtc().await;
        assert!(go2rtc_stream.consumers.iter().any(|c| c.consumer_type == "HTTP MJPEG client"));
    }

    #[test]
    fn test_mjpeg_bytesmut_split_parsing() {
        let mut buf = bytes::BytesMut::new();
        // Leading garbage before SOI
        buf.extend_from_slice(b"GARBAGE");
        // Frame 1
        buf.extend_from_slice(&[0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        // Inter-frame garbage
        buf.extend_from_slice(b"\r\n\r\n");
        // Frame 2
        buf.extend_from_slice(&[0xFF, 0xD8, 0x03, 0x04, 0x05, 0xFF, 0xD9]);

        let soi_finder = memchr::memmem::Finder::new(&[0xFF, 0xD8]);
        let eoi_finder = memchr::memmem::Finder::new(&[0xFF, 0xD9]);
        let mut scan_pos: usize = 0;
        let mut frames = Vec::new();

        loop {
            if !buf.starts_with(&[0xFF, 0xD8]) {
                if let Some(soi_pos) = soi_finder.find(&buf) {
                    let _ = buf.split_to(soi_pos);
                    scan_pos = 2;
                } else {
                    break;
                }
            }
            if buf.len() < 4 {
                break;
            }
            let search_start = scan_pos.saturating_sub(1).max(2);
            if let Some(rel) = eoi_finder.find(&buf[search_start..]) {
                let eoi_end = search_start + rel + 2;
                let jpeg = buf.split_to(eoi_end).freeze();
                frames.push(jpeg);
                scan_pos = 0;
            } else {
                break;
            }
        }

        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0][..], &[0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9]);
        assert_eq!(&frames[1][..], &[0xFF, 0xD8, 0x03, 0x04, 0x05, 0xFF, 0xD9]);
    }
}
