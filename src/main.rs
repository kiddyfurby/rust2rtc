mod api;
mod audio;
mod backchannel;
mod config;
mod core;
mod ffmpeg;
mod ha;
mod mp4;
mod record;
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

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

fn parse_cpuset_list(s: &str) -> Option<usize> {
    let mut total = 0;
    for part in s.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start.trim().parse().ok()?;
            let end: usize = end.trim().parse().ok()?;
            if end >= start {
                total += end - start + 1;
            }
        } else {
            let _cpu: usize = part.parse().ok()?;
            total += 1;
        }
    }
    if total > 0 {
        Some(total)
    } else {
        None
    }
}

fn detect_cpus_allowed_from_proc_status() -> Option<usize> {
    let content = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in content.lines() {
        if let Some(list_str) = line.strip_prefix("Cpus_allowed_list:") {
            return parse_cpuset_list(list_str);
        }
    }
    None
}

fn detect_cpus_from_cgroup() -> Option<usize> {
    let candidate_paths = [
        "/sys/fs/cgroup/cpuset.cpus.effective",
        "/sys/fs/cgroup/cpuset.cpus",
        "/sys/fs/cgroup/cpuset/cpuset.cpus",
    ];
    for path in candidate_paths {
        if let Ok(content) = std::fs::read_to_string(path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                if let Some(count) = parse_cpuset_list(trimmed) {
                    return Some(count);
                }
            }
        }
    }
    None
}

fn detect_available_cores() -> usize {
    let mut detected = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    if let Some(proc_cpus) = detect_cpus_allowed_from_proc_status() {
        detected = detected.min(proc_cpus);
    }

    if let Some(cgroup_cpus) = detect_cpus_from_cgroup() {
        detected = detected.min(cgroup_cpus);
    }

    detected.max(1)
}

fn main() -> Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();
    let exe = raw_args.first().map(|s| s.as_str()).unwrap_or("");
    if exe.ends_with("ffmpeg") || raw_args.get(1).map(|s| s.as_str()) == Some("ffmpeg-shim") {
        let shim_args: Vec<String> = if raw_args.get(1).map(|s| s.as_str()) == Some("ffmpeg-shim") {
            raw_args.into_iter().skip(2).collect()
        } else {
            raw_args.into_iter().skip(1).collect()
        };
        crate::record::run_ffmpeg_shim(shim_args);
    }

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

    let affined_cores = detect_available_cores();
    let worker_threads = config.worker_threads.unwrap_or(affined_cores).max(1);

    info!(
        "Starting rust2rtc v{} with {} worker threads (detected {} affined CPU cores)",
        env!("CARGO_PKG_VERSION"),
        worker_threads,
        affined_cores
    );

    if config.worker_threads.is_some() {
        info!(
            worker_threads = worker_threads,
            affined_cores = affined_cores,
            "Tokio runtime configured with {} worker threads (explicitly set via worker_threads in config, affined cores: {})",
            worker_threads,
            affined_cores
        );
    } else {
        info!(
            worker_threads = worker_threads,
            affined_cores = affined_cores,
            "Tokio runtime configured with {} worker threads (defaulted to affined core count: {}; set 'worker_threads' in rust2rtc.yaml to override)",
            worker_threads,
            affined_cores
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads)
        .build()?;

    runtime.block_on(async_main(config))
}

async fn async_main(config: Config) -> Result<()> {
    let ffmpeg_manager = Arc::new(FfmpegManager::new(
        config.ffmpeg.bin.clone(),
        config.ffmpeg.global.clone(),
        config.ffmpeg.hardware.clone(),
        config.ffmpeg.templates.clone(),
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
        .next_back()
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
    let webrtc_engine = config
        .webrtc
        .as_ref()
        .and_then(|w| w.engine.clone())
        .unwrap_or_else(|| "rustrtc".to_string());

    info!(engine = %webrtc_engine, "WebRTC engine selected");

    let webrtc_max_payload = config
        .webrtc
        .as_ref()
        .and_then(|w| w.max_payload_size)
        .unwrap_or(1200);

    let webrtc_manager = Arc::new(webrtc::WebRtcManager::with_max_payload_size(
        webrtc_candidates,
        rtsp_port,
        ffmpeg_manager.bin().to_string(),
        webrtc_engine,
        webrtc_max_payload,
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

            let display_name = acc_cfg
                .display_name
                .clone()
                .or_else(|| sip_cfg.names.get(stream_name.as_str()).cloned())
                .unwrap_or_else(|| format!("Camera {}", stream_name));

            let client_cfg = sip::client::SipAccountConfig {
                stream_name: stream_name.clone(),
                server: server.to_string(),
                domain: domain.to_string(),
                extension,
                auth_user,
                password,
                display_name,
                ring_group: acc_cfg.ring_group.clone(),
                auto_answer: acc_cfg.auto_answer,
                local_ip: local_ip.to_string(),
                ha_client: ha_client.clone(),
                max_call_duration_secs: acc_cfg
                    .max_call_duration_secs
                    .or(sip_cfg.max_call_duration_secs)
                    .or(Some(180)),
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
                let display_name = sip_cfg
                    .names
                    .get(&stream_name)
                    .cloned()
                    .unwrap_or_else(|| format!("Camera {}", stream_name));

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
                    max_call_duration_secs: sip_cfg.max_call_duration_secs.or(Some(180)),
                };
                if let Err(e) = sip_manager.add_account(client_cfg).await {
                    error!(stream = %stream_name, "Failed to auto-register SIP account: {}", e);
                }
            }
        }
    }

    // Start Home Assistant Doorbell event subscriber if doorbells are configured
    if !config.doorbells.is_empty() {
        if let (Some(ha_url), Some(ha_token)) = (config.ha_url(), config.ha_token()) {
            info!(
                count = config.doorbells.len(),
                ha_url = %ha_url,
                "Starting Home Assistant doorbell subscriber for parallel intercom calling"
            );
            ha::start_doorbell_subscriber(
                ha_url,
                ha_token,
                config.doorbells.clone(),
                Arc::clone(&sip_manager),
            );
        } else {
            error!(
                "Doorbells are configured but Home Assistant ha_url and/or ha_token are missing; \
                 please specify ha_url and ha_token under sip: or ha: in configuration"
            );
        }
    }

    // Initialize in-process MP4 segment RecordManager for Frigate cache
    let record_manager = record::RecordManager::new(
        config.record.clone(),
        Arc::clone(&stream_manager),
        rtsp_port,
        Arc::clone(&ffmpeg_manager),
    );

    let app_state = api::AppState {
        streams: Arc::clone(&stream_manager),
        ffmpeg: Arc::clone(&ffmpeg_manager),
        webrtc: Arc::clone(&webrtc_manager),
        sip: Arc::clone(&sip_manager),
        record: record_manager,
        rtsp_port,
        snapshots: Arc::new(api::SnapshotManager::new()),
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
        if let Some(addr) = target
            && socket.connect(addr).is_ok()
                && let Ok(local) = socket.local_addr() {
                    return local.ip().to_string();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cpuset_list() {
        assert_eq!(parse_cpuset_list("4-6"), Some(3));
        assert_eq!(parse_cpuset_list("0,2,4-6"), Some(5));
        assert_eq!(parse_cpuset_list("0-7"), Some(8));
        assert_eq!(parse_cpuset_list("3"), Some(1));
        assert_eq!(parse_cpuset_list(""), None);
        assert_eq!(parse_cpuset_list("invalid"), None);
        assert_eq!(parse_cpuset_list("0-3,7,9-11"), Some(8));
    }
}


