use crate::ffmpeg::FfmpegManager;
use crate::streams::StreamManager;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tracing::error;

#[derive(Clone)]
pub struct AppState {
    pub streams: Arc<StreamManager>,
    pub ffmpeg: Arc<FfmpegManager>,
    pub rtsp_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct FrameQuery {
    pub src: String,
}

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/index.html", get(index_handler))
        .route("/api/streams", get(api_streams_handler))
        .route("/api/ffmpeg", get(api_ffmpeg_handler))
        .route("/api/frame.jpeg", get(api_frame_handler))
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
            grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
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
            height: 180px;
            background: #000;
            display: flex;
            align-items: center;
            justify-content: center;
            position: relative;
        }
        .preview img {
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
            gap: 8px;
        }
        a.btn {
            background: #21262d;
            border: 1px solid var(--border);
            color: var(--accent);
            text-decoration: none;
            padding: 6px 10px;
            border-radius: 6px;
            font-size: 12px;
        }
        a.btn:hover {
            background: #30363d;
        }
    </style>
</head>
<body>
    <header>
        <div>
            <h1>🦀 rust2rtc</h1>
            <div style="font-size: 13px; color: #8b949e; margin-top: 4px;">Zero-copy streaming server & external FFmpeg runner in Rust</div>
        </div>
        <div id="status" class="tag">Active</div>
    </header>

    <div id="streams" class="stream-grid">Loading streams...</div>

    <script>
        async function loadStreams() {
            try {
                const res = await fetch('/api/streams');
                const streams = await res.json();
                const container = document.getElementById('streams');
                if (streams.length === 0) {
                    container.innerHTML = '<div style="color: #8b949e">No streams configured. Add them to rust2rtc.yaml</div>';
                    return;
                }

                container.innerHTML = streams.map(s => {
                    const tracksHtml = s.tracks.map(t => `<span class="badge">${t.kind}: ${t.codec}</span>`).join(' ');
                    return `
                    <div class="card">
                        <div class="card-header">
                            <span>${s.name}</span>
                            <span class="badge" style="background: ${s.active ? '#238636' : '#6e7681'}">${s.active ? 'LIVE' : 'IDLE'}</span>
                        </div>
                        <div class="preview">
                            <img src="/api/frame.jpeg?src=${encodeURIComponent(s.name)}" onerror="this.style.display='none'" alt="Snapshot" />
                        </div>
                        <div class="card-body">
                            <div>${tracksHtml || '<span class="badge">on-demand</span>'}</div>
                            <div class="meta">
                                <div>Consumers: <b>${s.consumers}</b></div>
                                <div>Packets: <b>${s.packets_recv}</b> | Bytes: <b>${(s.bytes_recv / 1024 / 1024).toFixed(2)} MB</b></div>
                            </div>
                            <div class="links">
                                <a class="btn" href="/api/frame.jpeg?src=${encodeURIComponent(s.name)}" target="_blank">Snapshot</a>
                                <a class="btn" href="rtsp://${window.location.hostname}:8554/${s.name}">RTSP Stream</a>
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

async fn api_streams_handler(State(state): State<AppState>) -> Json<Vec<crate::streams::StreamInfo>> {
    let infos = state.streams.get_all_infos().await;
    Json(infos)
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
    // If query.src is configured stream name, ensure started and snapshot from loopback RTSP
    let source = if let Some(stream) = state.streams.get(&query.src).await {
        let _ = stream.ensure_started(state.rtsp_port, &state.ffmpeg).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        format!("rtsp://127.0.0.1:{}/{}", state.rtsp_port, query.src)
    } else {
        query.src.clone()
    };

    match state.ffmpeg.take_snapshot(&source).await {
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
