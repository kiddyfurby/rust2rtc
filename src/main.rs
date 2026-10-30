mod api;
mod backchannel;
mod config;
mod core;
mod ffmpeg;
mod rtsp;
mod sip;
mod streams;
mod webrtc;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use ffmpeg::FfmpegManager;
use rtsp::RtspServer;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use streams::StreamManager;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(author, version, about = "rust2rtc - high performance streaming gateway in Rust", long_about = None)]
struct Args {
    /// Path to config file (default: rust2rtc.yaml or go2rtc.yaml)
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust2rtc=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::parse();

    // Find config file
    let config_path = args.config.unwrap_or_else(|| {
        if std::path::Path::new("rust2rtc.yaml").exists() {
            PathBuf::from("rust2rtc.yaml")
        } else if std::path::Path::new("go2rtc.yaml").exists() {
            PathBuf::from("go2rtc.yaml")
        } else {
            PathBuf::from("rust2rtc.yaml")
        }
    });

    let config = if config_path.exists() {
        info!("Loading configuration from {:?}", config_path);
        Config::load_from_file(&config_path)?
    } else {
        info!("No configuration file found at {:?}, using defaults", config_path);
        Config::default()
    };

    let ffmpeg_manager = Arc::new(FfmpegManager::new(
        config.ffmpeg.bin.clone(),
        config.ffmpeg.global.clone(),
    ));

    match ffmpeg_manager.check_version().await {
        Ok(v) => info!("FFmpeg detected: {}", v),
        Err(e) => error!("FFmpeg check failed: {}. Ensure ffmpeg is in PATH", e),
    }

    let stream_manager = Arc::new(StreamManager::new());

    // Register configured streams
    for (name, stream_cfg) in config.streams {
        let sources = stream_cfg.to_vec();
        info!(stream = %name, sources = ?sources, "Registering stream");
        stream_manager.add_configured_stream(name, sources).await;
    }

    // Determine RTSP port
    let rtsp_listen = if config.rtsp.listen.starts_with(':') {
        format!("0.0.0.0{}", config.rtsp.listen)
    } else {
        config.rtsp.listen.clone()
    };

    let rtsp_port: u16 = rtsp_listen
        .split(':')
        .last()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8554);

    // Start RTSP Server
    let rtsp_server = Arc::new(RtspServer::new(
        rtsp_listen.clone(),
        Arc::clone(&stream_manager),
    ));

    tokio::spawn(async move {
        if let Err(e) = rtsp_server.run().await {
            error!("RTSP server exited with error: {}", e);
        }
    });

    // Start HTTP API Server
    let api_listen = if config.api.listen.starts_with(':') {
        format!("0.0.0.0{}", config.api.listen)
    } else {
        config.api.listen.clone()
    };

    let webrtc_candidates = config
        .webrtc
        .as_ref()
        .map(|w| w.candidates.clone())
        .unwrap_or_default();

    let webrtc_manager = Arc::new(webrtc::WebRtcManager::new(webrtc_candidates));

    // Initialize SIP Manager
    let sip_manager = Arc::new(sip::SipManager::new(Arc::clone(&stream_manager)));

    if let Some(sip_cfg) = &config.sip {
        let server = sip_cfg.server.as_deref().unwrap_or("127.0.0.1:5060");
        let domain = sip_cfg.domain.as_deref().unwrap_or("homeassistant.local");
        let default_ip = detect_local_ip(server);
        let local_ip = sip_cfg.local_ip.as_deref().unwrap_or(&default_ip);

        for (stream_name, acc_cfg) in &sip_cfg.accounts {
            let client_cfg = sip::client::SipAccountConfig {
                stream_name: stream_name.clone(),
                server: server.to_string(),
                domain: domain.to_string(),
                extension: acc_cfg.extension.clone(),
                auth_user: acc_cfg.auth_user.clone(),
                password: acc_cfg.password.clone(),
                display_name: acc_cfg.display_name.clone(),
                ring_group: acc_cfg.ring_group.clone(),
                auto_answer: acc_cfg.auto_answer,
                local_ip: local_ip.to_string(),
            };
            if let Err(e) = sip_manager.add_account(client_cfg).await {
                error!(stream = %stream_name, "Failed to initialize SIP account: {}", e);
            }
        }
    }

    let app_state = api::AppState {
        streams: Arc::clone(&stream_manager),
        ffmpeg: Arc::clone(&ffmpeg_manager),
        webrtc: Arc::clone(&webrtc_manager),
        sip: Arc::clone(&sip_manager),
        rtsp_port,
    };

    let app = api::create_router(app_state);
    let listener = tokio::net::TcpListener::bind(&api_listen)
        .await
        .with_context(|| format!("Failed to bind HTTP API on {}", api_listen))?;

    info!("HTTP API and Web Dashboard available on http://{}", api_listen);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    info!("rust2rtc stopped cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received, shutting down gracefully...");
}

fn detect_local_ip(server: &str) -> String {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        let target = if let Ok(mut addrs) = server.split(':').next().unwrap_or("8.8.8.8").to_string().add_port_str("5060") {
            addrs.next()
        } else {
            None
        };
        if let Some(addr) = target {
            if socket.connect(addr).is_ok() {
                if let Ok(local) = socket.local_addr() {
                    return local.ip().to_string();
                }
            }
        }
    }
    "127.0.0.1".to_string()
}

trait AddPortStr {
    fn add_port_str(&self, port: &str) -> std::io::Result<std::vec::IntoIter<std::net::SocketAddr>>;
}

impl AddPortStr for String {
    fn add_port_str(&self, port: &str) -> std::io::Result<std::vec::IntoIter<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        format!("{}:{}", self, port).to_socket_addrs()
    }
}

