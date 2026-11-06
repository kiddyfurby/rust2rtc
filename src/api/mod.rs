use crate::core::MediaKind;
use crate::ffmpeg::FfmpegManager;
use crate::streams::StreamManager;
use crate::webrtc::WebRtcManager;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncReadExt;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info};

#[derive(Clone)]
struct CachedSnapshot {
    jpeg: Bytes,
    created_at: tokio::time::Instant,
}

#[derive(Default)]
pub struct SnapshotManager {
    cache: std::sync::Mutex<std::collections::HashMap<String, CachedSnapshot>>,
    in_flight: std::sync::Mutex<std::collections::HashMap<String, tokio::sync::broadcast::Sender<Result<Bytes, String>>>>,
}

impl SnapshotManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_cached(&self, key: &str) -> Option<Bytes> {
        let mut map = self.cache.lock().unwrap();
        if let Some(entry) = map.get(key) {
            if entry.created_at.elapsed() < std::time::Duration::from_millis(1000) {
                return Some(entry.jpeg.clone());
            } else {
                map.remove(key);
            }
        }
        None
    }

    pub fn insert_cached(&self, key: String, jpeg: Bytes) {
        let mut map = self.cache.lock().unwrap();
        map.insert(
            key,
            CachedSnapshot {
                jpeg,
                created_at: tokio::time::Instant::now(),
            },
        );
    }
}

#[derive(Clone)]
pub struct AppState {
    pub streams: Arc<StreamManager>,
    pub ffmpeg: Arc<FfmpegManager>,
    pub webrtc: Arc<WebRtcManager>,
    pub sip: Arc<crate::sip::SipManager>,
    pub record: Arc<crate::record::RecordManager>,
    pub rtsp_port: u16,
    pub snapshots: Arc<SnapshotManager>,
}

#[derive(Debug, Deserialize)]
pub struct FrameQuery {
    pub src: String,
}

#[derive(Debug, Deserialize)]
pub struct MjpegQuery {
    pub src: Option<String>,
    pub name: Option<String>,
    pub val: Option<String>,
    pub width: Option<u32>,
    pub w: Option<u32>,
    pub height: Option<u32>,
    pub h: Option<u32>,
    pub fps: Option<u32>,
    pub r: Option<u32>,
    pub q: Option<u32>,
    pub quality: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct SipCallQuery {
    pub stream: String,
    pub dest: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamsQuery {
    pub src: Option<String>,
    pub name: Option<String>,
    pub val: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StreamPayload {
    pub src: Option<String>,
    pub name: Option<String>,
    pub val: Option<String>,
    pub sources: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WsMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/api", get(api_info_handler))
        .route("/api/", get(api_info_handler))
        .route("/api/schemes", get(api_schemes_handler))
        .route(
            "/api/preload",
            get(api_preload_get_handler)
                .put(api_preload_action_handler)
                .post(api_preload_action_handler)
                .delete(api_preload_action_handler),
        )
        .route(
            "/api/streams",
            get(api_streams_get_handler)
                .put(api_streams_put_handler)
                .post(api_streams_put_handler)
                .delete(api_streams_delete_handler),
        )
        .route("/api/ffmpeg", get(api_ffmpeg_handler))
        .route("/api/frame.jpeg", get(api_frame_handler))
        .route("/api/stream.mjpeg", get(api_stream_mjpeg_handler))
        .route("/api/frame.mjpeg", get(api_stream_mjpeg_handler))
        .route("/stream.mjpeg", get(api_stream_mjpeg_handler))
        .route("/api/webrtc", post(api_webrtc_handler))
        .route("/api/ws", get(api_ws_handler))
        .route("/api/sip/call", post(api_sip_call_handler))
        .route("/api/sip/hangup", post(api_sip_hangup_handler).get(api_sip_hangup_handler))
        .route("/api/sip/accounts", get(api_sip_accounts_handler))
        .route("/api/record", get(api_record_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = concat!(
    r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>rust2rtc</title>
    <style>
        :root {
            --bg: #0d1117;
            --card: #161b22;
            --border: #30363d;
            --text: #c9d1d9;
            --accent: #58a6ff;
            --green: #3fb950;
            --red: #f85149;
        }
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            background: var(--bg);
            color: var(--text);
            margin: 0;
            padding: 24px;
        }
        header {
            display: flex;
            align-items: center;
            justify-content: space-between;
            border-bottom: 1px solid var(--border);
            padding-bottom: 16px;
            margin-bottom: 24px;
        }
        h1 { margin: 0; font-size: 24px; color: #fff; }
        .tag {
            background: #238636;
            color: #fff;
            padding: 4px 8px;
            border-radius: 12px;
            font-size: 12px;
            font-weight: bold;
        }
        .stream-grid {
            display: grid;
            grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
            gap: 20px;
        }
        .card {
            background: var(--card);
            border: 1px solid var(--border);
            border-radius: 8px;
            overflow: hidden;
            display: flex;
            flex-direction: column;
        }
        .card-header {
            padding: 12px 16px;
            background: rgba(255, 255, 255, 0.03);
            border-bottom: 1px solid var(--border);
            display: flex;
            justify-content: space-between;
            align-items: center;
            font-weight: 600;
        }
        .card-body {
            padding: 16px;
            flex: 1;
        }
        .preview {
            width: 100%;
            height: 200px;
            background: #000;
            display: flex;
            align-items: center;
            justify-content: center;
            position: relative;
        }
        .preview img, .preview video {
            width: 100%;
            height: 100%;
            object-fit: cover;
        }
        .meta {
            font-size: 13px;
            color: #8b949e;
            margin-top: 8px;
        }
        .badge {
            display: inline-block;
            padding: 2px 6px;
            border-radius: 4px;
            font-size: 11px;
            background: var(--border);
            color: #fff;
            margin-right: 4px;
        }
        .links {
            margin-top: 12px;
            display: flex;
            flex-wrap: wrap;
            gap: 8px;
        }
        a.btn, button.btn {
            background: #21262d;
            border: 1px solid var(--border);
            color: var(--accent);
            text-decoration: none;
            padding: 6px 12px;
            border-radius: 6px;
            font-size: 12px;
            cursor: pointer;
        }
        a.btn:hover, button.btn:hover {
            background: #30363d;
        }
        button.btn-primary {
            background: #238636;
            color: #fff;
            border-color: #2ea043;
        }
        button.btn-primary:hover {
            background: #2ea043;
        }
        footer {
            margin-top: 48px;
            padding-top: 16px;
            border-top: 1px solid var(--border);
            display: flex;
            align-items: center;
            justify-content: space-between;
            color: #8b949e;
            font-size: 13px;
        }
        footer .commit-hash {
            font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace;
            background: #21262d;
            padding: 2px 6px;
            border-radius: 4px;
            color: var(--text);
            border: 1px solid var(--border);
        }
    </style>
</head>
<body>
    <header>
        <div>
            <h1>🦀 rust2rtc</h1>
            <div style="font-size: 13px; color: #8b949e; margin-top: 4px;">High-Performance WebRTC & RTSP Media Gateway in Rust</div>
        </div>
        <div id="status" class="tag">Active</div>
    </header>

    <div id="streams" class="stream-grid">Loading streams...</div>

    <footer>
        <div>
            <span>🦀 <b>rust2rtc</b> v"#,
    env!("CARGO_PKG_VERSION"),
    r#"</span>
            <span style="margin: 0 8px; color: var(--border);">|</span>
            <span>Commit: <span class="commit-hash">"#,
    env!("GIT_HASH"),
    r#"</span></span>
        </div>
        <div>
            <span style="color: #6e7681;">High-Performance WebRTC &amp; RTSP Gateway</span>
        </div>
    </footer>

    <script>
        const activePeerConnections = new Map();

        async function playWebRtc(streamName, previewEl) {
            try {
                if (activePeerConnections.has(streamName)) {
                    activePeerConnections.get(streamName).close();
                    activePeerConnections.delete(streamName);
                }

                previewEl.innerHTML = '<video autoplay playsinline muted controls style="width:100%; height:100%; object-fit:cover; background:#000;"></video>';
                const videoEl = previewEl.querySelector('video');

                const mediaStream = new MediaStream();
                videoEl.srcObject = mediaStream;

                const pc = new RTCPeerConnection({
                    iceServers: [{ urls: 'stun:stun.l.google.com:19302' }]
                });
                activePeerConnections.set(streamName, pc);

                const vTrans = pc.addTransceiver('video', { direction: 'recvonly' });
                const aTrans = pc.addTransceiver('audio', { direction: 'recvonly' });

                if (vTrans && vTrans.receiver && vTrans.receiver.track) {
                    mediaStream.addTrack(vTrans.receiver.track);
                }
                if (aTrans && aTrans.receiver && aTrans.receiver.track) {
                    mediaStream.addTrack(aTrans.receiver.track);
                }

                if (window.RTCRtpReceiver && RTCRtpReceiver.getCapabilities) {
                    const caps = RTCRtpReceiver.getCapabilities('video');
                    if (caps && caps.codecs) {
                        const hasH265 = caps.codecs.some(c => c.mimeType.toLowerCase().includes('h265') || c.mimeType.toLowerCase().includes('hevc'));
                        if (hasH265) {
                            const sorted = caps.codecs.slice().sort((a, b) => {
                                const aH = a.mimeType.toLowerCase().includes('h265') || a.mimeType.toLowerCase().includes('hevc');
                                const bH = b.mimeType.toLowerCase().includes('h265') || b.mimeType.toLowerCase().includes('hevc');
                                return (bH ? 1 : 0) - (aH ? 1 : 0);
                            });
                            try { vTrans.setCodecPreferences(sorted); } catch (e) { console.warn(e); }
                        }
                    }
                }

                pc.ontrack = (event) => {
                    console.log(`[WebRTC:${streamName}] ontrack:`, event.track.kind, event.track.id);
                    if (!mediaStream.getTracks().some(t => t.id === event.track.id)) {
                        mediaStream.addTrack(event.track);
                    }
                    videoEl.play().catch(e => console.warn('Autoplay prevented:', e));
                };

                videoEl.onloadedmetadata = () => {
                    console.log(`[WebRTC:${streamName}] Metadata loaded: ${videoEl.videoWidth}x${videoEl.videoHeight}`);
                };
                videoEl.onplaying = () => console.log(`[WebRTC:${streamName}] Video playing`);
                videoEl.onerror = (e) => console.error(`[WebRTC:${streamName}] Video error:`, e);

                const offer = await pc.createOffer();
                await pc.setLocalDescription(offer);

                await new Promise(resolve => {
                    if (pc.iceGatheringState === 'complete') {
                        resolve();
                    } else {
                        const check = () => {
                            if (pc.iceGatheringState === 'complete') {
                                pc.removeEventListener('icegatheringstatechange', check);
                                resolve();
                            }
                        };
                        pc.addEventListener('icegatheringstatechange', check);
                        setTimeout(resolve, 800);
                    }
                });

                const res = await fetch(`/api/webrtc?src=${encodeURIComponent(streamName)}`, {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/sdp' },
                    body: pc.localDescription.sdp
                });

                if (!res.ok) {
                    throw new Error(`WHEP error ${res.status}: ${await res.text()}`);
                }

                const answerSdp = await res.text();
                await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });
            } catch (err) {
                console.error(err);
                previewEl.innerHTML = `<div style="color:#f85149; padding:12px; font-size:12px;">WebRTC Error: ${err.message}</div>`;
            }
        }

        async function loadStreams() {
            try {
                const res = await fetch('/api/streams');
                const data = await res.json();
                const container = document.getElementById('streams');

                const streams = Array.isArray(data) ? data : Object.entries(data).map(([name, s]) => {
                    const isLive = s.producers && s.producers.some(p => p.recv > 0 || (p.medias && p.medias.length > 0));
                    const totalConsumers = s.consumers ? s.consumers.length : 0;
                    const recvBytes = s.producers ? s.producers.reduce((acc, p) => acc + (p.recv || 0), 0) : 0;
                    const recvPackets = s.producers ? s.producers.reduce((acc, p) => acc + (p.recv_packets || 0), 0) : 0;
                    const medias = (s.producers && s.producers[0] && s.producers[0].medias) || [];
                    const tracks = medias.map(m => {
                        const parts = m.split(',').map(p => p.trim());
                        return { kind: parts[0] || 'video', codec: parts[2] || parts[1] || '' };
                    });
                    return {
                        name,
                        active: isLive,
                        consumers: totalConsumers,
                        bytes_recv: recvBytes,
                        packets_recv: recvPackets,
                        tracks
                    };
                });

                if (streams.length === 0) {
                    container.innerHTML = '<div style="color: #8b949e">No streams configured.</div>';
                    return;
                }

                // If already initialized cards, only update dynamic numbers to avoid resetting active video players
                if (container.querySelectorAll('.card').length === streams.length) {
                    streams.forEach(s => {
                        const metaEl = document.getElementById(`meta-${s.name}`);
                        if (metaEl) {
                            metaEl.innerHTML = `
                                <div>Consumers: <b>${s.consumers}</b></div>
                                <div>Packets: <b>${s.packets_recv}</b> | Bytes: <b>${(s.bytes_recv / 1024 / 1024).toFixed(2)} MB</b></div>
                            `;
                        }
                    });
                    return;
                }

                container.innerHTML = streams.map(s => {
                    const tracksHtml = s.tracks.map(t => `<span class="badge">${t.kind}: ${t.codec}</span>`).join(' ');
                    return `
                    <div class="card" id="card-${s.name}">
                        <div class="card-header">
                            <span>${s.name}</span>
                            <span class="badge" style="background: ${s.active ? '#238636' : '#6e7681'}">${s.active ? 'LIVE' : 'IDLE'}</span>
                        </div>
                        <div class="preview" id="preview-${s.name}">
                            <img src="/api/frame.jpeg?src=${encodeURIComponent(s.name)}" onerror="this.style.display='none'" alt="Snapshot" />
                        </div>
                        <div class="card-body">
                            <div>${tracksHtml || '<span class="badge">on-demand</span>'}</div>
                            <div class="meta" id="meta-${s.name}">
                                <div>Consumers: <b>${s.consumers}</b></div>
                                <div>Packets: <b>${s.packets_recv}</b> | Bytes: <b>${(s.bytes_recv / 1024 / 1024).toFixed(2)} MB</b></div>
                            </div>
                            <div class="links">
                                <button class="btn btn-primary" onclick="playWebRtc('${s.name}', document.getElementById('preview-${s.name}'))">▶ WebRTC Live</button>
                                <a class="btn" href="/api/stream.mjpeg?src=${encodeURIComponent(s.name)}" target="_blank">MJPEG</a>
                                <a class="btn" href="/api/frame.jpeg?src=${encodeURIComponent(s.name)}" target="_blank">Snapshot</a>
                                <a class="btn" href="rtsp://${window.location.hostname}:8554/${s.name}">RTSP</a>
                            </div>
                        </div>
                    </div>
                    `;
                }).join('');
            } catch (e) {
                console.error(e);
            }
        }
        loadStreams();
        setInterval(loadStreams, 3000);
    </script>
</body>
</html>"#
);

async fn api_info_handler(State(state): State<AppState>) -> Response {
    let resp = serde_json::json!({
        "version": "1.9.14",
        "revision": concat!("rust2rtc-", env!("CARGO_PKG_VERSION")),
        "rust2rtc_version": env!("CARGO_PKG_VERSION"),
        "commit": env!("GIT_HASH"),
        "host": "127.0.0.1:1984",
        "rtsp": {
            "listen": format!(":{}", state.rtsp_port),
            "default_query": "video&audio"
        }
    });
    (StatusCode::OK, Json(resp)).into_response()
}

async fn api_schemes_handler() -> Response {
    let schemes = [
        "alsa",
        "bubble",
        "doorbird",
        "dvrip",
        "echo",
        "eseecloud",
        "exec",
        "expr",
        "ffmpeg",
        "flussonic",
        "gopro",
        "hass",
        "homeassistant",
        "homekit",
        "http",
        "https",
        "httpx",
        "isapi",
        "ivideon",
        "kasa",
        "mjpeg",
        "mpjpeg",
        "multitrans",
        "nest",
        "onvif",
        "ring",
        "roborock",
        "rtmp",
        "rtmps",
        "rtmpx",
        "rtsp",
        "rtsps",
        "rtspx",
        "tapo",
        "tcp",
        "tuya",
        "v4l2",
        "vigi",
        "webrtc",
        "webtorrent",
        "wyoming",
        "wyze",
        "xiaomi",
        "yandex",
    ];
    (StatusCode::OK, Json(&schemes[..])).into_response()
}

async fn api_preload_get_handler() -> Response {
    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}

async fn api_preload_action_handler() -> Response {
    (StatusCode::OK, Json(serde_json::json!({}))).into_response()
}

async fn api_streams_get_handler(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let mut target = None;
    if let Some(query) = raw_query {
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            if (k == "name" || k == "src") && target.is_none() {
                target = Some(v.into_owned());
            }
        }
    }

    if let Some(name) = target {
        if let Some(stream_info) = state.streams.get_go2rtc_stream(&name).await {
            (StatusCode::OK, Json(stream_info)).into_response()
        } else {
            (StatusCode::NOT_FOUND, "Stream not found").into_response()
        }
    } else {
        let all_streams = state.streams.get_go2rtc_streams().await;
        (StatusCode::OK, Json(all_streams)).into_response()
    }
}

async fn api_streams_put_handler(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    let mut name = None;
    let mut sources = Vec::new();

    if let Some(query) = raw_query {
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            match k.as_ref() {
                "name"
                    if name.is_none() => {
                        name = Some(v.into_owned());
                    }
                "src" => {
                    sources.push(v.into_owned());
                }
                "val" => {
                    sources.push(v.into_owned());
                }
                _ => {}
            }
        }
    }

    if !body.is_empty()
        && let Ok(p) = serde_json::from_slice::<StreamPayload>(&body) {
            if name.is_none() {
                name = p.name.or(p.src);
            }
            if let Some(s) = p.sources {
                sources.extend(s);
            }
            if let Some(val) = p.val {
                sources.push(val);
            }
        }

    if name.is_none() && !sources.is_empty() {
        name = Some(sources[0].clone());
    }

    if let Some(name) = name
        && !sources.is_empty() {
            state.streams.add_configured_stream(name.clone(), sources).await;
            info!(stream = %name, "Dynamically configured stream via /api/streams");
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "ok", "stream": name })),
            )
                .into_response();
        }

    (StatusCode::BAD_REQUEST, "Missing stream name or source URL").into_response()
}

async fn api_streams_delete_handler(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let mut target = None;
    if let Some(query) = raw_query {
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            if (k == "name" || k == "src") && target.is_none() {
                target = Some(v.into_owned());
            }
        }
    }

    if let Some(name) = target {
        if state.streams.delete(&name).await {
            info!(stream = %name, "Deleted stream via /api/streams");
            (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "ok" })),
            )
                .into_response()
        } else {
            (StatusCode::NOT_FOUND, "Stream not found").into_response()
        }
    } else {
        (StatusCode::BAD_REQUEST, "Missing stream name").into_response()
    }
}

async fn api_ffmpeg_handler(State(state): State<AppState>) -> Response {
    match state.ffmpeg.check_version().await {
        Ok(v) => (StatusCode::OK, v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn generate_snapshot(state: &AppState, src: &str) -> anyhow::Result<Bytes> {
    if let Some(stream) = state.streams.get(src).await {
        let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
        // Wait up to 3 seconds for tracks to be populated if just starting
        for _ in 0..30 {
            if !stream.get_tracks().await.is_empty() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        let tracks = stream.get_tracks().await;
        let vtrack = tracks.iter().find(|t| t.kind == MediaKind::Video);
        if let Some(vtrack) = vtrack {
            let is_hevc = vtrack.codec.name.eq_ignore_ascii_case("H265")
                || vtrack.codec.name.eq_ignore_ascii_case("HEVC");

            // Wait up to 2 seconds for a keyframe in track cache if stream just started
            let mut kf_packets = vtrack.get_keyframe_packets();
            if kf_packets.is_empty() {
                let mut rx = vtrack.subscribe();
                let start = tokio::time::Instant::now();
                while start.elapsed() < tokio::time::Duration::from_millis(2000) {
                    if let Ok(pkt) = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv()).await {
                        if let Ok(p) = pkt {
                            let (is_kf, _) = crate::core::is_keyframe_or_param_set(is_hevc, p.payload());
                            if is_kf {
                                kf_packets = vtrack.get_keyframe_packets();
                                break;
                            }
                        }
                    }
                }
            }

            if !kf_packets.is_empty() {
                let annex_b = crate::core::rtp_packets_to_annex_b(is_hevc, &kf_packets);
                if !annex_b.is_empty() {
                    match state.ffmpeg.take_snapshot_from_keyframe(is_hevc, &annex_b).await {
                        Ok(bytes) => return Ok(bytes),
                        Err(e) => {
                            debug!(error = %e, "Stdin snapshot fallback to loopback RTSP");
                        }
                    }
                }
            }

            // Fallback to RTSP loopback if keyframe couldn't be extracted
            let source = format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, src);
            state.ffmpeg.take_snapshot(&source, state.rtsp_port, is_hevc).await
        } else {
            let source = format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, src);
            state.ffmpeg.take_snapshot(&source, state.rtsp_port, false).await
        }
    } else {
        state.ffmpeg.take_snapshot(src, state.rtsp_port, false).await
    }
}

async fn api_frame_handler(
    State(state): State<AppState>,
    Query(query): Query<FrameQuery>,
) -> Response {
    let key = query.src.clone();

    // 1. Check 1s TTL snapshot cache
    if let Some(cached) = state.snapshots.get_cached(&key) {
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg")],
            cached,
        )
            .into_response();
    }

    // 2. Request coalescing (singleflight)
    let (mut rx, is_leader) = {
        let mut in_flight = state.snapshots.in_flight.lock().unwrap();
        if let Some(tx) = in_flight.get(&key) {
            (tx.subscribe(), false)
        } else {
            let (tx, rx) = tokio::sync::broadcast::channel(1);
            in_flight.insert(key.clone(), tx);
            (rx, true)
        }
    };

    if !is_leader {
        return match rx.recv().await {
            Ok(Ok(jpeg_bytes)) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "image/jpeg")],
                jpeg_bytes,
            )
                .into_response(),
            Ok(Err(err_msg)) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to take snapshot: {}", err_msg),
            )
                .into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Snapshot generator cancelled".to_string(),
            )
                .into_response(),
        };
    }

    let result = generate_snapshot(&state, &key).await;

    // Cache on success & notify waiting followers
    {
        let mut in_flight = state.snapshots.in_flight.lock().unwrap();
        if let Some(tx) = in_flight.remove(&key) {
            match &result {
                Ok(bytes) => {
                    state.snapshots.insert_cached(key.clone(), bytes.clone());
                    let _ = tx.send(Ok(bytes.clone()));
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        }
    }

    match result {
        Ok(jpeg_bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg")],
            jpeg_bytes,
        )
            .into_response(),
        Err(e) => {
            error!(source = %key, "Failed to capture snapshot: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to take snapshot: {}", e),
            )
                .into_response()
        }
    }
}

async fn api_stream_mjpeg_handler(
    State(state): State<AppState>,
    Query(query): Query<MjpegQuery>,
) -> Response {
    let stream_name = match query.src.or(query.name).or(query.val) {
        Some(s) if !s.is_empty() => s,
        _ => return (StatusCode::BAD_REQUEST, "Missing stream parameter (src/name)").into_response(),
    };

    let width = query.width.or(query.w);
    let height = query.height.or(query.h);
    let fps = query.fps.or(query.r);
    let quality = query.quality.or(query.q);
    let is_custom = width.is_some() || height.is_some() || fps.is_some() || quality.is_some();

    if let Some(stream) = state.streams.get(&stream_name).await {
        let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
        // Wait up to 3 seconds for tracks to be populated if just starting
        for _ in 0..30 {
            if !stream.get_tracks().await.is_empty() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        if !is_custom {
            if let Some(broadcaster) = stream.get_or_create_mjpeg_broadcaster(state.rtsp_port, &state.ffmpeg).await {
                let rx = broadcaster.subscribe();
                let stream_body = futures_util::stream::unfold(rx, |mut rx| async move {
                    loop {
                        match rx.recv().await {
                            Ok(chunk) => return Some((Ok::<Bytes, std::convert::Infallible>(chunk), rx)),
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                });

                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "multipart/x-mixed-replace; boundary=frame")
                    .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
                    .header(header::PRAGMA, "no-cache")
                    .header(header::EXPIRES, "0")
                    .header(header::CONNECTION, "close")
                    .body(axum::body::Body::from_stream(stream_body))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
        } else {
            // Dedicated transcode process with custom width/height/fps/quality options
            let custom_pipeline = stream.sources.iter().find(|s| s.contains("#video=mjpeg")).cloned();
            let source = format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, stream.name);
            if let Some(resp) = create_dedicated_mjpeg_response(&source, custom_pipeline.as_deref(), width, height, fps, quality, &state.ffmpeg) {
                return resp;
            }
        }
    } else {
        // Direct source URL (e.g. rtsp://... or external HTTP stream)
        if let Some(resp) = create_dedicated_mjpeg_response(&stream_name, None, width, height, fps, quality, &state.ffmpeg) {
            return resp;
        }
    }

    (StatusCode::INTERNAL_SERVER_ERROR, "Failed to start MJPEG stream").into_response()
}

fn create_dedicated_mjpeg_response(
    source: &str,
    custom_pipeline: Option<&str>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    quality: Option<u32>,
    ffmpeg: &FfmpegManager,
) -> Option<Response> {
    let args = ffmpeg.build_mjpeg_stream_args(source, custom_pipeline, width, height, fps, quality);
    let mut child = ffmpeg.spawn_stdout_process(&args).ok()?;
    let mut stdout = child.stdout.take()?;

    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);

    tokio::spawn(async move {
        let mut stream_buf = Vec::with_capacity(65536);
        let mut chunk = [0u8; 8192];

        loop {
            let n = match stdout.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            stream_buf.extend_from_slice(&chunk[..n]);

            while let Some(soi_pos) = crate::ffmpeg::find_marker(&stream_buf, &[0xFF, 0xD8]) {
                if soi_pos > 0 {
                    stream_buf.drain(..soi_pos);
                }
                if stream_buf.len() < 4 {
                    break;
                }
                if let Some(eoi_rel) = crate::ffmpeg::find_marker(&stream_buf[2..], &[0xFF, 0xD9]) {
                    let eoi_pos = 2 + eoi_rel + 2;
                    let jpeg = &stream_buf[..eoi_pos];
                    let part_header = format!(
                        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        jpeg.len()
                    );
                    let mut packet = Vec::with_capacity(part_header.len() + jpeg.len() + 2);
                    packet.extend_from_slice(part_header.as_bytes());
                    packet.extend_from_slice(jpeg);
                    packet.extend_from_slice(b"\r\n");

                    if tx.send(Bytes::from(packet)).await.is_err() {
                        // Consumer disconnected
                        let _ = child.kill().await;
                        return;
                    }
                    stream_buf.drain(..eoi_pos);
                } else {
                    break;
                }
            }
        }
        let _ = child.kill().await;
    });

    let stream_body = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok::<Bytes, std::convert::Infallible>(chunk), rx))
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "multipart/x-mixed-replace; boundary=frame")
        .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
        .header(header::PRAGMA, "no-cache")
        .header(header::EXPIRES, "0")
        .header(header::CONNECTION, "close")
        .body(axum::body::Body::from_stream(stream_body))
        .ok()
}

async fn api_webrtc_handler(
    State(state): State<AppState>,
    Query(query): Query<FrameQuery>,
    body: String,
) -> Response {
    if let Some(stream) = state.streams.get(&query.src).await {
        let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
        // Wait up to 5 seconds for producer to establish RTSP streaming if starting
        for _ in 0..50 {
            if !stream.get_tracks().await.is_empty() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        let offer_upper = body.to_uppercase();
        let offer_has_hevc = offer_upper.contains("H265") || offer_upper.contains("HEVC");
        let stream_to_use = if !offer_has_hevc {
            let is_hevc = stream.get_tracks().await.iter().any(|t| {
                t.kind == crate::core::MediaKind::Video
                    && (t.codec.name.to_uppercase().contains("265") || t.codec.name.to_uppercase().contains("HEVC"))
            });
            if is_hevc {
                let sub_name = format!("{}_sub", query.src);
                if let Some(sub_stream) = state.streams.get(&sub_name).await {
                    let _ = sub_stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
                    for _ in 0..50 {
                        if !sub_stream.get_tracks().await.is_empty() {
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                    info!(stream = %query.src, fallback = %sub_name, "Client does not support WebRTC H.265, seamlessly serving H.264 sub-stream");
                    sub_stream
                } else {
                    stream
                }
            } else {
                stream
            }
        } else {
            stream
        };

        match state.webrtc.handle_whep(stream_to_use, &body).await {
            Ok((answer_sdp, _pc)) => (
                StatusCode::CREATED,
                [
                    (header::CONTENT_TYPE, "application/sdp"),
                    (header::LOCATION, "/api/webrtc"),
                ],
                answer_sdp,
            )
                .into_response(),
            Err(e) => {
                error!("WebRTC WHEP error: {}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("WebRTC error: {}", e),
                )
                    .into_response()
            }
        }
    } else {
        (StatusCode::NOT_FOUND, "Stream not found").into_response()
    }
}

async fn api_ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<StreamsQuery>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, query, state))
}

async fn handle_ws(socket: WebSocket, query: StreamsQuery, state: AppState) {
    let default_src = query.src.or(query.name).or(query.val);
    let (mut ws_sink, mut ws_stream) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Message>(256);

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut mse_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut active_pc: Option<crate::webrtc::ActivePeerConnection> = None;

    while let Some(msg_result) = ws_stream.next().await {
        let msg = match msg_result {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(p)) => {
                let _ = tx.send(Message::Pong(p)).await;
                continue;
            }
            Ok(_) => continue,
            Err(e) => {
                debug!("WebSocket receive error: {}", e);
                break;
            }
        };

        let ws_msg: WsMessage = match serde_json::from_str(&msg) {
            Ok(m) => m,
            Err(e) => {
                debug!("Invalid WebSocket JSON: {}", e);
                continue;
            }
        };

        match ws_msg.msg_type.as_str() {
            "webrtc/offer" => {
                let offer_sdp = match ws_msg.value {
                    Some(serde_json::Value::String(s)) => s,
                    Some(serde_json::Value::Object(map)) => {
                        map.get("sdp").and_then(|v| v.as_str()).unwrap_or("").to_string()
                    }
                    _ => String::new(),
                };

                if offer_sdp.is_empty() {
                    let err_reply = WsMessage {
                        msg_type: "error".to_string(),
                        value: Some(serde_json::Value::String("Empty SDP offer".to_string())),
                    };
                    if let Ok(json_str) = serde_json::to_string(&err_reply) {
                        let _ = tx.send(Message::Text(json_str.into())).await;
                    }
                    continue;
                }

                let src_name = default_src.as_deref().unwrap_or("");
                if let Some(stream) = state.streams.get(src_name).await {
                    let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
                    for _ in 0..50 {
                        if !stream.get_tracks().await.is_empty() {
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }

                    let offer_upper = offer_sdp.to_uppercase();
                    let offer_has_hevc = offer_upper.contains("H265") || offer_upper.contains("HEVC");
                    let stream_to_use = if !offer_has_hevc {
                        let is_hevc = stream.get_tracks().await.iter().any(|t| {
                            t.kind == crate::core::MediaKind::Video
                                && (t.codec.name.to_uppercase().contains("265") || t.codec.name.to_uppercase().contains("HEVC"))
                        });
                        if is_hevc {
                            let sub_name = format!("{}_sub", src_name);
                            if let Some(sub_stream) = state.streams.get(&sub_name).await {
                                let _ = sub_stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
                                for _ in 0..50 {
                                    if !sub_stream.get_tracks().await.is_empty() {
                                        break;
                                    }
                                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                }
                                info!(stream = %src_name, fallback = %sub_name, "Client does not support WebRTC H.265, seamlessly serving H.264 sub-stream");
                                sub_stream
                            } else {
                                stream
                            }
                        } else {
                            stream
                        }
                    } else {
                        stream
                    };

                    match state.webrtc.handle_whep(stream_to_use, &offer_sdp).await {
                        Ok((answer_sdp, pc)) => {
                            active_pc = Some(pc);
                            let answer_msg = WsMessage {
                                msg_type: "webrtc/answer".to_string(),
                                value: Some(serde_json::Value::String(answer_sdp)),
                            };
                            if let Ok(json_str) = serde_json::to_string(&answer_msg) {
                                let _ = tx.send(Message::Text(json_str.into())).await;
                            }
                            // Signal end of ICE candidates (compatible with go2rtc WebRTC client)
                            let cand_msg = WsMessage {
                                msg_type: "webrtc/candidate".to_string(),
                                value: Some(serde_json::Value::String(String::new())),
                            };
                            if let Ok(json_str) = serde_json::to_string(&cand_msg) {
                                let _ = tx.send(Message::Text(json_str.into())).await;
                            }
                        }
                        Err(e) => {
                            error!("WebRTC WebSocket error for stream {}: {}", src_name, e);
                            let err_msg = WsMessage {
                                msg_type: "error".to_string(),
                                value: Some(serde_json::Value::String(format!("WebRTC error: {}", e))),
                            };
                            if let Ok(json_str) = serde_json::to_string(&err_msg) {
                                let _ = tx.send(Message::Text(json_str.into())).await;
                            }
                        }
                    }
                } else {
                    let err_msg = WsMessage {
                        msg_type: "error".to_string(),
                        value: Some(serde_json::Value::String(format!("Stream '{}' not found", src_name))),
                    };
                    if let Ok(json_str) = serde_json::to_string(&err_msg) {
                        let _ = tx.send(Message::Text(json_str.into())).await;
                    }
                }
            }
            "webrtc/candidate" => {
                debug!("Received ICE candidate via WebSocket: {:?}", ws_msg.value);
                if let Some(ref pc) = active_pc {
                    let cand_str = match ws_msg.value {
                        Some(serde_json::Value::String(s)) => s,
                        _ => String::new(),
                    };
                    if !cand_str.is_empty()
                        && let Err(e) = pc.add_ice_candidate(&cand_str).await {
                            debug!("Failed to add ICE candidate: {}", e);
                        }
                }
            }
            "mse" => {
                let src_name = default_src.as_deref().unwrap_or("").to_string();
                if let Some(stream) = state.streams.get(&src_name).await {
                    let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
                    for _ in 0..50 {
                        if !stream.get_tracks().await.is_empty() {
                            break;
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }

                    let tracks = stream.get_tracks().await;
                    let mut has_h265 = false;
                    let mut has_aac = false;
                    let mut has_audio = false;
                    for t in &tracks {
                        if t.kind == MediaKind::Video {
                            let name = t.codec.name.to_uppercase();
                            if name.contains("265") || name.contains("HEVC") {
                                has_h265 = true;
                            }
                        } else if t.kind == MediaKind::Audio {
                            has_audio = true;
                            let name = t.codec.name.to_uppercase();
                            if name.contains("AAC") || name.contains("MP4A") || name.contains("MPEG4-GENERIC") {
                                has_aac = true;
                            }
                        }
                    }

                    let client_codecs: Vec<String> = match &ws_msg.value {
                        Some(serde_json::Value::String(s)) => {
                            s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
                        }
                        Some(serde_json::Value::Array(arr)) => {
                            arr.iter().filter_map(|v| v.as_str().map(|s| s.trim().to_string())).collect()
                        }
                        _ => Vec::new(),
                    };
                    let client_codecs_str = client_codecs.join(",");

                    let allow_audio = has_audio && client_codecs_str.contains("mp4a");
                    let copy_audio = has_aac && allow_audio;
                    let transcode_audio = !has_aac && allow_audio;

                    let copy_video = if has_h265 {
                        client_codecs.is_empty() || client_codecs_str.contains("hvc1")
                    } else {
                        true
                    };

                    let video_track = stream.get_tracks().await.into_iter().find(|t| t.kind == crate::core::MediaKind::Video);
                    let use_native_fmp4 = copy_video && video_track.is_some();

                    // Dynamically resolve H.264 and H.265 profile/level parameters
                    let (_h265_w, _h265_h, h265_level) = video_track
                        .as_ref()
                        .and_then(|t| t.codec.fmtp.as_deref())
                        .and_then(|f| {
                            if let Some((_, sps, _)) = crate::webrtc::extract_h265_vps_sps_pps(f) {
                                Some(crate::mp4::parse_h265_sps(&sps))
                            } else {
                                None
                            }
                        })
                        .unwrap_or((2560, 1440, 150));

                    let h265_codec_str = format!("hvc1.1.6.L{}.B0", h265_level);

                    let h264_profile_level = video_track
                        .as_ref()
                        .and_then(|t| t.codec.fmtp.as_deref())
                        .and_then(|f| {
                            for part in f.split(';') {
                                let part = part.trim();
                                if let Some((k, v)) = part.split_once('=')
                                    && k.trim().eq_ignore_ascii_case("profile-level-id") {
                                        let v = v.trim();
                                        if v.len() == 6 && v.chars().all(|c| c.is_ascii_hexdigit()) {
                                            return Some(v.to_lowercase());
                                        }
                                    }
                            }
                            None
                        })
                        .unwrap_or_else(|| "640029".to_string());

                    let h264_avc1 = format!("avc1.{}", h264_profile_level);

                    let mime = if use_native_fmp4 {
                        if has_h265 {
                            format!("video/mp4; codecs=\"{}\"", h265_codec_str)
                        } else {
                            format!("video/mp4; codecs=\"{}\"", h264_avc1)
                        }
                    } else if has_h265 {
                        if client_codecs_str.contains("hvc1") {
                            if allow_audio {
                                format!("video/mp4; codecs=\"{},mp4a.40.2\"", h265_codec_str)
                            } else {
                                format!("video/mp4; codecs=\"{}\"", h265_codec_str)
                            }
                        } else {
                            if allow_audio {
                                format!("video/mp4; codecs=\"{},mp4a.40.2\"", h264_avc1)
                            } else {
                                format!("video/mp4; codecs=\"{}\"", h264_avc1)
                            }
                        }
                    } else {
                        if allow_audio {
                            format!("video/mp4; codecs=\"{},mp4a.40.2\"", h264_avc1)
                        } else {
                            format!("video/mp4; codecs=\"{}\"", h264_avc1)
                        }
                    };

                    info!(stream = %src_name, mime = %mime, copy_video = copy_video, use_native_fmp4 = use_native_fmp4, "Starting MSE fMP4 stream");

                    let reply = WsMessage {
                        msg_type: "mse".to_string(),
                        value: Some(serde_json::Value::String(mime.to_string())),
                    };
                    if let Ok(json_str) = serde_json::to_string(&reply) {
                        let _ = tx.send(Message::Text(json_str.into())).await;
                    }

                    if let Some(h) = mse_handle.take() {
                        h.abort();
                    }

                    let tx_mse = tx.clone();
                    let stream_name = src_name.clone();

                    if use_native_fmp4 {
                        // Pure-Rust in-process fMP4 multiplexer (Zero FFmpeg subprocess)
                        let vtrack = video_track.unwrap();
                        info!(stream = %stream_name, is_hevc = has_h265, "Starting pure-Rust in-process MSE fMP4 stream (no FFmpeg)");

                        mse_handle = Some(tokio::spawn(async move {
                            let mut rx = vtrack.subscribe();
                            let mut sent_init = false;
                            let mut seq = 1u32;
                            let mut base_pts = 0u64;
                            let mut last_rtp_ts: Option<u32> = None;

                            if has_h265 {
                                let mut depacketizer = crate::mp4::H265RtpDepacketizer::new();
                                if let Some(ref fmtp) = vtrack.codec.fmtp
                                    && let Some((vps, sps, pps)) = crate::webrtc::extract_h265_vps_sps_pps(fmtp) {
                                        depacketizer.vps = Some(vps);
                                        depacketizer.sps = Some(sps);
                                        depacketizer.pps = Some(pps);
                                    }

                                loop {
                                    let packet = match rx.recv().await {
                                        Ok(p) => p,
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                            tracing::warn!(stream = %stream_name, lagged = n, "MSE H.265 receiver lagged; resetting depacketizer");
                                            depacketizer.reset();
                                            continue;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    };

                                    let mut frame_opt = depacketizer.feed_packet(&packet);
                                    while let Some((is_keyframe, rtp_ts, nalus)) = frame_opt {
                                        if !sent_init {
                                            if !is_keyframe {
                                                frame_opt = depacketizer.pop_frame();
                                                continue;
                                            }
                                            if let (Some(vps), Some(sps), Some(pps)) = (&depacketizer.vps, &depacketizer.sps, &depacketizer.pps) {
                                                let (w, h, _) = crate::mp4::parse_h265_sps(sps);
                                                let init_seg = crate::mp4::make_h265_init_segment(vps, sps, pps, w, h);
                                                if tx_mse.send(Message::Binary(init_seg.into())).await.is_err() {
                                                    break;
                                                }
                                                sent_init = true;
                                            } else {
                                                frame_opt = depacketizer.pop_frame();
                                                continue;
                                            }
                                        }

                                        let duration = if let Some(last) = last_rtp_ts {
                                            rtp_ts.wrapping_sub(last).max(1000).min(90000)
                                        } else {
                                            3000
                                        };
                                        last_rtp_ts = Some(rtp_ts);

                                        let media_seg = crate::mp4::make_h264_media_segment(seq, base_pts, duration, is_keyframe, &nalus);
                                        seq = seq.wrapping_add(1);
                                        base_pts += duration as u64;

                                        if tx_mse.send(Message::Binary(media_seg.into())).await.is_err() {
                                            break;
                                        }

                                        frame_opt = depacketizer.pop_frame();
                                    }
                                }
                            } else {
                                let mut depacketizer = crate::mp4::H264RtpDepacketizer::new();

                                // Pre-load SPS / PPS from track SDP fmtp if available
                                if let Some(ref fmtp) = vtrack.codec.fmtp
                                    && let Some((sps, pps)) = crate::webrtc::extract_h264_sps_pps(fmtp) {
                                        depacketizer.sps = Some(sps);
                                        depacketizer.pps = Some(pps);
                                    }

                                loop {
                                    let packet = match rx.recv().await {
                                        Ok(p) => p,
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                            tracing::warn!(stream = %stream_name, lagged = n, "MSE receiver lagged; resetting depacketizer");
                                            depacketizer.reset();
                                            continue;
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                    };

                                    let mut frame_opt = depacketizer.feed_packet(&packet);
                                    while let Some((is_keyframe, rtp_ts, nalus)) = frame_opt {
                                        if !sent_init {
                                            // Gate initial playback on a keyframe
                                            if !is_keyframe {
                                                frame_opt = depacketizer.pop_frame();
                                                continue;
                                            }

                                            if let (Some(sps), Some(pps)) = (&depacketizer.sps, &depacketizer.pps) {
                                                let (w, h) = crate::mp4::parse_h264_sps_dimensions(sps);
                                                let init_seg = crate::mp4::make_h264_init_segment(sps, pps, w, h);
                                                if tx_mse.send(Message::Binary(init_seg.into())).await.is_err() {
                                                    break;
                                                }
                                                sent_init = true;
                                            } else {
                                                frame_opt = depacketizer.pop_frame();
                                                continue;
                                            }
                                        }

                                        let duration = if let Some(last) = last_rtp_ts {
                                            rtp_ts.wrapping_sub(last).max(1000).min(90000)
                                        } else {
                                            3000
                                        };
                                        last_rtp_ts = Some(rtp_ts);

                                        let media_seg = crate::mp4::make_h264_media_segment(seq, base_pts, duration, is_keyframe, &nalus);
                                        seq = seq.wrapping_add(1);
                                        base_pts += duration as u64;

                                        if tx_mse.send(Message::Binary(media_seg.into())).await.is_err() {
                                            break;
                                        }

                                        frame_opt = depacketizer.pop_frame();
                                    }
                                }
                            }
                        }));
                    } else {
                        // Fallback to FFmpeg transcoding process if video re-encoding is required
                        let rtsp_url = format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, src_name);
                        let ffmpeg_bin = state.ffmpeg.bin().to_string();
                        let h264_encoder_args = state.ffmpeg.h264_encoder_args();

                        mse_handle = Some(tokio::spawn(async move {
                            let mut cmd = tokio::process::Command::new(&ffmpeg_bin);
                            cmd.kill_on_drop(true);
                            cmd.args([
                                "-hide_banner",
                                "-loglevel", "warning",
                                "-fflags", "nobuffer",
                                "-flags", "low_delay",
                                "-rtsp_transport", "tcp",
                                "-i", &rtsp_url,
                            ]);

                            if copy_video {
                                cmd.args(["-c:v", "copy"]);
                            } else {
                                cmd.args(&h264_encoder_args);
                            }

                            if copy_audio {
                                cmd.args(["-c:a", "copy"]);
                            } else if transcode_audio {
                                cmd.args(["-c:a", "aac", "-b:a", "64k", "-ar", "16000", "-ac", "1"]);
                            } else {
                                cmd.arg("-an");
                            }

                            cmd.args([
                                "-f", "mp4",
                                "-movflags", "empty_moov+default_base_moof+frag_keyframe",
                                "-flush_packets", "1",
                                "pipe:1",
                            ]);
                            cmd.stdout(std::process::Stdio::piped());
                            cmd.stderr(std::process::Stdio::null());

                            let mut child = match cmd.spawn() {
                                Ok(c) => c,
                                Err(e) => {
                                    error!("Failed to spawn FFmpeg for MSE stream {}: {}", src_name, e);
                                    return;
                                }
                            };

                            if let Some(mut stdout) = child.stdout.take() {
                                let mut pending = Vec::new();
                                let mut chunk = vec![0u8; 16384];
                                let mut init_seg: Option<Vec<u8>> = None;
                                let mut moof_seg: Option<Vec<u8>> = None;

                                loop {
                                    match stdout.read(&mut chunk).await {
                                        Ok(0) => break,
                                        Ok(n) => {
                                            pending.extend_from_slice(&chunk[..n]);
                                            while pending.len() >= 8 {
                                                let box_size = u32::from_be_bytes([
                                                    pending[0], pending[1], pending[2], pending[3],
                                                ]) as usize;
                                                if box_size < 8 {
                                                    // Invalid box header or EOS
                                                    break;
                                                }
                                                if pending.len() < box_size {
                                                    // Wait for full box
                                                    break;
                                                }
                                                let mut box_name = [0u8; 4];
                                                box_name.copy_from_slice(&pending[4..8]);
                                                let box_data = pending.drain(..box_size).collect::<Vec<u8>>();

                                                if &box_name == b"ftyp" {
                                                    init_seg = Some(box_data);
                                                } else if &box_name == b"moov" {
                                                    if let Some(mut init) = init_seg.take() {
                                                        init.extend_from_slice(&box_data);
                                                        if tx_mse.send(Message::Binary(init.into())).await.is_err() {
                                                            return;
                                                        }
                                                    } else {
                                                        if tx_mse.send(Message::Binary(box_data.into())).await.is_err() {
                                                            return;
                                                        }
                                                    }
                                                } else if &box_name == b"moof" {
                                                    moof_seg = Some(box_data);
                                                } else if &box_name == b"mdat" {
                                                    if let Some(mut moof) = moof_seg.take() {
                                                        moof.extend_from_slice(&box_data);
                                                        if tx_mse.send(Message::Binary(moof.into())).await.is_err() {
                                                            return;
                                                        }
                                                    } else {
                                                        if tx_mse.send(Message::Binary(box_data.into())).await.is_err() {
                                                            return;
                                                        }
                                                    }
                                                } else {
                                                    // Other top-level box
                                                    if tx_mse.send(Message::Binary(box_data.into())).await.is_err() {
                                                        return;
                                                    }
                                                }
                                            }
                                        }
                                        Err(_) => break,
                                    }
                                }
                            }
                            let _ = child.kill().await;
                        }));
                    }
                } else {
                    let err_msg = WsMessage {
                        msg_type: "error".to_string(),
                        value: Some(serde_json::Value::String(format!("Stream '{}' not found", src_name))),
                    };
                    if let Ok(json_str) = serde_json::to_string(&err_msg) {
                        let _ = tx.send(Message::Text(json_str.into())).await;
                    }
                }
            }
            other => {
                debug!("Received unhandled WebSocket message type: {}", other);
            }
        }
    }

    // Note: Do NOT close `active_pc` when the WebSocket closes!
    // In go2rtc's `video-rtc.js` (`onpcvideo`), once WebRTC connects and decodes its first
    // video frame, the client switches playback to WebRTC and intentionally calls `this.ws.close()`
    // to stop the redundant MSE stream while keeping the WebRTC PeerConnection alive over UDP.
    let _ = active_pc;
    if let Some(h) = mse_handle {
        h.abort();
    }
    writer_task.abort();
}

async fn api_sip_call_handler(
    State(state): State<AppState>,
    Query(query): Query<SipCallQuery>,
) -> impl IntoResponse {
    match state.sip.trigger_call(&query.stream, query.dest.as_deref()).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "calling",
                "stream": query.stream,
                "dest": query.dest,
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e.to_string(),
            })),
        ),
    }
}

async fn api_sip_accounts_handler(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let accounts = state.sip.list_accounts().await;
    Json(serde_json::json!({ "accounts": accounts }))
}

#[derive(Debug, Deserialize)]
pub struct SipHangupQuery {
    pub stream: String,
}

async fn api_sip_hangup_handler(
    State(state): State<AppState>,
    Query(query): Query<SipHangupQuery>,
) -> impl IntoResponse {
    match state.sip.hangup(&query.stream).await {
        Ok(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "hung_up",
                "stream": query.stream,
            })),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": e.to_string(),
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
pub struct RecordQuery {
    pub stream: String,
    pub action: Option<String>,
}

pub async fn api_record_handler(
    State(state): State<AppState>,
    Query(query): Query<RecordQuery>,
) -> Response {
    let action = query.action.as_deref().unwrap_or("start");
    match action {
        "start" => {
            if !state.record.is_configured(&query.stream) || state.streams.get(&query.stream).await.is_none() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "Stream not found or not configured for in-process recording",
                        "stream": query.stream
                    })),
                )
                    .into_response();
            }
            if let Err(e) = state.record.start(&query.stream).await {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": e })),
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "recording",
                    "stream": query.stream
                })),
            )
                .into_response()
        }
        "stop" => {
            let _ = state.record.stop(&query.stream).await;
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": "stopped",
                    "stream": query.stream
                })),
            )
                .into_response()
        }
        "status" => {
            let active = state.record.is_active(&query.stream).await;
            let configured = state.record.is_configured(&query.stream);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "stream": query.stream,
                    "configured": configured,
                    "active": active
                })),
            )
                .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Unknown action" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_message_serialization() {
        let offer_json = r#"{"type":"webrtc/offer","value":"v=0\r\no=-..."}"#;
        let parsed: WsMessage = serde_json::from_str(offer_json).expect("parse offer");
        assert_eq!(parsed.msg_type, "webrtc/offer");
        assert_eq!(
            parsed.value.unwrap().as_str().unwrap(),
            "v=0\r\no=-..."
        );

        let answer = WsMessage {
            msg_type: "webrtc/answer".to_string(),
            value: Some(serde_json::Value::String("v=0\r\no=rust2rtc...".to_string())),
        };
        let encoded = serde_json::to_string(&answer).expect("encode answer");
        assert!(encoded.contains(r#""type":"webrtc/answer""#));
        assert!(encoded.contains(r#""value":"v=0\r\no=rust2rtc...""#));
    }

    #[test]
    fn test_mjpeg_query_deserialization() {
        let json = serde_json::json!({
            "src": "doorbell",
            "fps": 10,
            "w": 640,
            "h": 480,
            "q": 3
        });
        let query: MjpegQuery = serde_json::from_value(json).expect("deserialize mjpeg query");
        assert_eq!(query.src.as_deref(), Some("doorbell"));
        assert_eq!(query.fps, Some(10));
        assert_eq!(query.w, Some(640));
        assert_eq!(query.h, Some(480));
        assert_eq!(query.q, Some(3));

        let json_alt = serde_json::json!({
            "name": "driveway",
            "r": 15,
            "width": 1280,
            "height": 720,
            "quality": 5
        });
        let query_alt: MjpegQuery = serde_json::from_value(json_alt).expect("deserialize alt query");
        assert_eq!(query_alt.name.as_deref(), Some("driveway"));
        assert_eq!(query_alt.r, Some(15));
        assert_eq!(query_alt.width, Some(1280));
        assert_eq!(query_alt.height, Some(720));
        assert_eq!(query_alt.quality, Some(5));
    }

    #[test]
    fn test_sip_hangup_query_deserialization() {
        let json = serde_json::json!({
            "stream": "doorbell"
        });
        let query: SipHangupQuery = serde_json::from_value(json).expect("deserialize hangup query");
        assert_eq!(query.stream, "doorbell");
    }

    #[tokio::test]
    async fn test_snapshot_manager_cache() {
        let sm = SnapshotManager::new();
        assert!(sm.get_cached("cam1").is_none());

        let fake_jpeg = Bytes::from_static(b"\xFF\xD8\xFF\xE0fakejpeg");
        sm.insert_cached("cam1".to_string(), fake_jpeg.clone());

        assert_eq!(sm.get_cached("cam1"), Some(fake_jpeg));
        assert!(sm.get_cached("cam2").is_none());
    }

    #[test]
    fn test_index_html_footer() {
        assert!(INDEX_HTML.contains("<footer>"));
        assert!(INDEX_HTML.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
        assert!(INDEX_HTML.contains(env!("GIT_HASH")));
        assert!(INDEX_HTML.contains("commit-hash"));
    }
}

