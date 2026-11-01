mod api;
mod audio;
mod backchannel;
mod config;
mod core;
mod ffmpeg;
mod mp4;
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
    /// Path to config file(s) (can be specified multiple times)
    #[arg(short = 'c', long = "config", action = clap::ArgAction::Append)]
    config: Vec<PathBuf>,
}

fn normalize_args() -> Vec<String> {
    std::env::args()
        .map(|arg| {
            if let Some(val) = arg.strip_prefix("-config=") {
                format!("--config={}", val)
            } else if arg == "-config" {
                "--config".to_string()
            } else if let Some(val) = arg.strip_prefix("-c=") {
                format!("-c={}", val)
            } else {
                arg
            }
        })
        .collect()
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

    let normalized = normalize_args();
    let args = Args::parse_from(normalized);

    // Collect all config paths
    let mut config_paths: Vec<PathBuf> = if !args.config.is_empty() {
        args.config
    } else if std::path::Path::new("rust2rtc.yaml").exists() {
        vec![PathBuf::from("rust2rtc.yaml")]
    } else if std::path::Path::new("go2rtc.yaml").exists() {
        vec![PathBuf::from("go2rtc.yaml")]
    } else {
        vec![PathBuf::from("rust2rtc.yaml")]
    };

    // Auto-detect companion rust2rtc.yaml configs (e.g. in /config/rust2rtc.yaml inside Docker,
    // or alongside any explicit config path, or in current dir)
    let supplemental = [
        PathBuf::from("/config/rust2rtc.yaml"),
        PathBuf::from("rust2rtc.yaml"),
    ];
    for supp in supplemental {
        if supp.exists() && !config_paths.contains(&supp) {
            info!("Found supplemental config file: {:?}", supp);
            config_paths.push(supp);
        }
    }

    let mut config = Config::default();
    let mut loaded_any = false;
    for path in &config_paths {
        if path.exists() {
            info!("Loading configuration from {:?}", path);
            match Config::load_from_file(path) {
                Ok(loaded) => {
                    config.merge(loaded);
                    loaded_any = true;
                }
                Err(e) => {
                    error!("Failed to load config file {:?}: {}", path, e);
                }
            }
        } else {
            info!("Config file {:?} does not exist, skipping", path);
        }
    }

    if !loaded_any {
        info!("No configuration files loaded, using default config");
    }

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
    for (name, stream_cfg) in &config.streams {
        let sources = stream_cfg.to_vec();
        info!(stream = %name, sources = ?sources, "Registering stream");
        stream_manager.add_configured_stream(name.clone(), sources).await;
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
        rtsp_port,
        Arc::clone(&stream_manager),
        Arc::clone(&ffmpeg_manager),
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

    let webrtc_manager = Arc::new(webrtc::WebRtcManager::new(
        webrtc_candidates,
        rtsp_port,
        ffmpeg_manager.bin().to_string(),
    ));

    // Initialize SIP Manager
    let sip_manager = Arc::new(sip::SipManager::new(
        Arc::clone(&stream_manager),
        Arc::clone(&ffmpeg_manager),
        rtsp_port,
    ));

    if let Some(sip_cfg) = &config.sip {
        let server = sip_cfg.server.as_deref().unwrap_or("127.0.0.1:5060");
        let domain = sip_cfg.domain.as_deref().unwrap_or("homeassistant.local");
        let default_ip = detect_local_ip(server);
        let local_ip = sip_cfg.local_ip.as_deref().unwrap_or(&default_ip);

        let server_host = server.split(':').next().unwrap_or("127.0.0.1");
        let default_ha_url = format!("http://{}:8123", server_host);
        let ha_url = sip_cfg.ha_url.as_deref().unwrap_or(&default_ha_url);
        let ha_token = sip_cfg
            .ha_token
            .clone()
            .or_else(|| std::env::var("HA_TOKEN").ok())
            .or_else(|| std::env::var("SUPERVISOR_TOKEN").ok());

        let ha_client = ha_token
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .map(|token| sip::ha::HaVoipClient::new(ha_url, token));

        if ha_client.is_some() {
            info!(ha_url = %ha_url, "Home Assistant voip_stack auto-provisioning enabled");
        }

        let mut used_exts = std::collections::HashSet::new();
        for acc in sip_cfg.accounts.values() {
            if let Some(ext) = &acc.extension {
                used_exts.insert(ext.clone());
            }
        }

        let mut next_ext = sip_cfg.base_extension;
        let mut alloc_ext = || -> String {
            loop {
                let candidate = next_ext.to_string();
                next_ext += 1;
                if !used_exts.contains(&candidate) {
                    used_exts.insert(candidate.clone());
                    return candidate;
                }
            }
        };

        // 1. Add explicitly configured SIP accounts (sorted for deterministic extension assignment)
        let mut explicit_names: Vec<_> = sip_cfg.accounts.keys().cloned().collect();
        explicit_names.sort();

        for stream_name in explicit_names {
            let acc_cfg = &sip_cfg.accounts[&stream_name];
            let extension = acc_cfg.extension.clone().unwrap_or_else(&mut alloc_ext);
            let auth_user = acc_cfg
                .auth_user
                .clone()
                .unwrap_or_else(|| format!("camera_{}", stream_name));
            let password = acc_cfg
                .password
                .clone()
                .unwrap_or_else(|| sip_cfg.auto_password.clone());

            let client_cfg = sip::client::SipAccountConfig {
                stream_name: stream_name.clone(),
                server: server.to_string(),
                domain: domain.to_string(),
                extension,
                auth_user,
                password,
                display_name: acc_cfg.display_name.clone(),
                ring_group: acc_cfg.ring_group.clone(),
                auto_answer: acc_cfg.auto_answer,
                local_ip: local_ip.to_string(),
                ha_client: ha_client.clone(),
            };
            if let Err(e) = sip_manager.add_account(client_cfg).await {
                error!(stream = %stream_name, "Failed to initialize SIP account: {}", e);
            }
        }

        // 2. Auto-register remaining primary camera streams if enabled
        if sip_cfg.auto_register_streams {
            let mut stream_names: Vec<_> = config.streams.keys().cloned().collect();
            stream_names.sort();

            for stream_name in stream_names {
                if stream_name.ends_with("_sub") || sip_cfg.accounts.contains_key(&stream_name) {
                    continue;
                }
                let extension = alloc_ext();
                let auth_user = format!("camera_{}", stream_name);
                let display_name = format!("Camera {}", stream_name);

                let client_cfg = sip::client::SipAccountConfig {
                    stream_name: stream_name.clone(),
                    server: server.to_string(),
                    domain: domain.to_string(),
                    extension,
                    auth_user,
                    password: sip_cfg.auto_password.clone(),
                    display_name,
                    ring_group: None,
                    auto_answer: true,
                    local_ip: local_ip.to_string(),
                    ha_client: ha_client.clone(),
                };
                if let Err(e) = sip_manager.add_account(client_cfg).await {
                    error!(stream = %stream_name, "Failed to auto-register SIP account: {}", e);
                }
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

    sip_manager.unregister_all().await;

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

