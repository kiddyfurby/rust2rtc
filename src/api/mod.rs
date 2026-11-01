use crate::core::MediaKind;
use crate::ffmpeg::FfmpegManager;
use crate::streams::StreamManager;
use crate::webrtc::WebRtcManager;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use futures_util::{SinkExt, StreamExt};
use tokio::io::AsyncReadExt;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info};

#[derive(Clone)]
pub struct AppState {
    pub streams: Arc<StreamManager>,
    pub ffmpeg: Arc<FfmpegManager>,
    pub webrtc: Arc<WebRtcManager>,
    pub sip: Arc<crate::sip::SipManager>,
    pub rtsp_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct FrameQuery {
    pub src: String,
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
        .route(
            "/api/streams",
            get(api_streams_get_handler)
                .put(api_streams_put_handler)
                .post(api_streams_put_handler)
                .delete(api_streams_delete_handler),
        )
        .route("/api/ffmpeg", get(api_ffmpeg_handler))
        .route("/api/frame.jpeg", get(api_frame_handler))
        .route("/api/webrtc", post(api_webrtc_handler))
        .route("/api/ws", get(api_ws_handler))
        .route("/api/sip/call", post(api_sip_call_handler))
        .route("/api/sip/accounts", get(api_sip_accounts_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn index_handler() -> Html<&'static str> {
    Html(r#"<!DOCTYPE html>
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
</html>"#)
}

async fn api_streams_get_handler(
    State(state): State<AppState>,
    Query(query): Query<StreamsQuery>,
) -> Response {
    let target = query.src.or(query.name);
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
    Query(query): Query<StreamsQuery>,
    body: axum::body::Bytes,
) -> Response {
    let payload: Option<StreamPayload> = if !body.is_empty() {
        serde_json::from_slice(&body).ok()
    } else {
        None
    };

    let (name, sources) = if let Some(p) = payload {
        let name = p.name.or(p.src.clone()).or(query.name.clone()).or(query.src.clone());
        let mut sources = p.sources.unwrap_or_default();
        if sources.is_empty() {
            if let Some(val) = p.val.or(p.src) {
                sources.push(val);
            }
        }
        (name, sources)
    } else {
        let name = query.name.clone().or(query.src.clone());
        let mut sources = Vec::new();
        if let Some(val) = query.val {
            sources.push(val);
        } else if let Some(src) = query.src {
            if query.name.is_some() {
                sources.push(src);
            }
        }
        (name, sources)
    };

    if let Some(name) = name {
        if !sources.is_empty() {
            state.streams.add_configured_stream(name.clone(), sources).await;
            info!(stream = %name, "Dynamically configured stream via /api/streams");
            return (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "ok", "stream": name })),
            )
                .into_response();
        }
    }

    (StatusCode::BAD_REQUEST, "Missing stream name or source URL").into_response()
}

async fn api_streams_delete_handler(
    State(state): State<AppState>,
    Query(query): Query<StreamsQuery>,
) -> Response {
    let target = query.src.or(query.name);
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

async fn api_frame_handler(
    State(state): State<AppState>,
    Query(query): Query<FrameQuery>,
) -> Response {
    let (source, is_hevc) = if let Some(stream) = state.streams.get(&query.src).await {
        let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
        // Wait up to 3 seconds for tracks to be populated if just starting
        for _ in 0..30 {
            if !stream.get_tracks().await.is_empty() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        let tracks = stream.get_tracks().await;
        let is_hevc = tracks.iter().any(|t| {
            t.kind == crate::core::MediaKind::Video
                && (t.codec.name.to_uppercase().contains("265")
                    || t.codec.name.to_uppercase().contains("HEVC"))
        });
        (format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, query.src), is_hevc)
    } else {
        (query.src.clone(), false)
    };

    match state.ffmpeg.take_snapshot(&source, state.rtsp_port, is_hevc).await {
        Ok(jpeg_bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "image/jpeg")],
            jpeg_bytes,
        )
            .into_response(),
        Err(e) => {
            error!(source = %source, "Failed to capture snapshot: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to take snapshot: {}", e),
            )
                .into_response()
        }
    }
}

async fn api_webrtc_handler(
    State(state): State<AppState>,
    Query(query): Query<FrameQuery>,
    body: String,
) -> Response {
    if let Some(stream) = state.streams.get(&query.src).await {
        let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
        // Wait up to 5 seconds for producer to establish RTSP recording if starting
        for _ in 0..50 {
            if !stream.get_tracks().await.is_empty() {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }

        match state.webrtc.handle_whep(stream, &body).await {
            Ok(answer_sdp) => (
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

                    match state.webrtc.handle_whep(stream, &offer_sdp).await {
                        Ok(answer_sdp) => {
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

                    let client_codecs = ws_msg
                        .value
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    let allow_audio = has_audio && client_codecs.contains("mp4a");
                    let copy_audio = has_aac && allow_audio;
                    let transcode_audio = !has_aac && allow_audio;

                    let (mime, copy_video) = if has_h265 {
                        if client_codecs.contains("hvc1") {
                            if allow_audio {
                                ("video/mp4; codecs=\"hvc1.1.6.L153.B0,mp4a.40.2\"", true)
                            } else {
                                ("video/mp4; codecs=\"hvc1.1.6.L153.B0\"", true)
                            }
                        } else {
                            if allow_audio {
                                ("video/mp4; codecs=\"avc1.640029,mp4a.40.2\"", false)
                            } else {
                                ("video/mp4; codecs=\"avc1.640029\"", false)
                            }
                        }
                    } else {
                        if allow_audio {
                            ("video/mp4; codecs=\"avc1.640029,mp4a.40.2\"", true)
                        } else {
                            ("video/mp4; codecs=\"avc1.640029\"", true)
                        }
                    };

                    info!(stream = %src_name, mime = %mime, copy_video = copy_video, copy_audio = copy_audio, transcode_audio = transcode_audio, "Starting MSE fMP4 stream");

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

                    let rtsp_url = format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, src_name);
                    let ffmpeg_bin = state.ffmpeg.bin().to_string();
                    let tx_mse = tx.clone();

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
                            cmd.args(["-c:v", "libx264", "-preset", "superfast", "-tune", "zerolatency", "-pix_fmt", "yuv420p"]);
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
                            "-movflags", "empty_moov+default_base_moof+frag_every_frame",
                            "-flush_packets", "1",
                            "-reset_timestamps", "1",
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
                            let mut buf = vec![0u8; 8192];
                            loop {
                                match stdout.read(&mut buf).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        if tx_mse.send(Message::Binary(buf[..n].to_vec().into())).await.is_err() {
                                            break;
                                        }
                                    }
                                    Err(_) => break,
                                }
                            }
                        }
                        let _ = child.kill().await;
                    }));
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
}

