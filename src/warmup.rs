//! Periodic pre-warm of Frigate's ONVIF camera caches.
//!
//! Frigate initializes each camera's ONVIF controller lazily: the first
//! `ptz/info` request after a (re)start pays a one-time SOAP initialization
//! (seconds for PTZ cameras) on Frigate's single ONVIF thread, and until it
//! finishes, the API event loop — and with it the dashboard's signaling
//! requests — queues behind it. This task periodically requests `ptz/info`
//! for the configured cameras so the first dashboard load after a restart
//! finds the caches already warm.
//!
//! The requests target the Frigate API over loopback. This Frigate build
//! authenticates API requests with `remote-user` / `remote-role` proxy
//! headers and trusts them on the local API, so the warm-up needs no login
//! or stored credentials.

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};
use url::Url;

use crate::config::WarmupConfig;

/// Per-request timeout. A cold ONVIF initialization takes a few seconds;
/// anything longer is treated as a failure.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A ptz/info slower than this means a cold ONVIF initialization just
/// happened in the background and is worth an info log.
const SLOW_REQUEST: Duration = Duration::from_secs(1);

/// How quickly to retry after a failed cycle (e.g. Frigate still booting).
const RETRY_DELAY: Duration = Duration::from_secs(5);

pub struct WarmupTask {
    host: String,
    port: u16,
    host_header: String,
    user: String,
    role: String,
    cameras: Vec<String>,
    interval: Duration,
}

impl WarmupTask {
    /// Build a task from config. Returns `None` when the task is disabled
    /// or the URL is unusable (only plain http is supported).
    pub fn from_config(cfg: &WarmupConfig) -> Option<Self> {
        if !cfg.enabled {
            debug!("frigate warmup disabled in config");
            return None;
        }
        let url = match Url::parse(&cfg.url) {
            Ok(url) => url,
            Err(e) => {
                warn!(url = %cfg.url, error = %e, "invalid warmup url; warmup disabled");
                return None;
            }
        };
        if url.scheme() != "http" {
            warn!(url = %cfg.url, "warmup only supports plain http urls; warmup disabled");
            return None;
        }
        let host = match url.host_str() {
            Some(host) => host.to_string(),
            None => {
                warn!(url = %cfg.url, "warmup url has no host; warmup disabled");
                return None;
            }
        };
        Some(Self {
            port: url.port().unwrap_or(80),
            host_header: url.authority().to_string(),
            host,
            user: cfg.user.clone(),
            role: cfg.role.clone(),
            cameras: cfg.cameras.clone(),
            interval: Duration::from_secs(cfg.interval_secs.max(5)),
        })
    }

    /// Run forever: one warm-up cycle, then sleep for `interval` (or retry
    /// quickly after a failed cycle).
    pub async fn run(self) {
        info!(
            cameras = self.cameras.len(),
            interval_s = self.interval.as_secs(),
            "frigate onvif warmup task started"
        );
        let mut failures = 0u32;
        loop {
            match self.warm_cycle().await {
                Ok(()) => {
                    if failures > 0 {
                        info!("frigate warmup requests are succeeding again");
                    }
                    failures = 0;
                }
                Err(err) => {
                    failures += 1;
                    // First failure, then every 10th, to keep the log quiet.
                    if failures == 1 || failures % 10 == 0 {
                        warn!(error = %err, failures, "frigate warmup cycle failed");
                    }
                }
            }
            let nap = if failures > 0 {
                RETRY_DELAY
            } else {
                self.interval
            };
            tokio::time::sleep(nap).await;
        }
    }

    /// One warm-up cycle over all cameras. `Err` describes why nothing
    /// could be warmed (Frigate unreachable, auth rejected, ...).
    async fn warm_cycle(&self) -> Result<(), String> {
        let cameras = if self.cameras.is_empty() {
            let cameras = self
                .fetch_camera_list()
                .await
                .ok_or_else(|| "could not read the camera list from frigate".to_string())?;
            if cameras.is_empty() {
                return Err("frigate reports no cameras".to_string());
            }
            cameras
        } else {
            self.cameras.clone()
        };

        let mut warmed = 0usize;
        for camera in &cameras {
            let start = Instant::now();
            match self.get(&format!("/{camera}/ptz/info")).await {
                Ok((status, _)) if status == 200 => {
                    warmed += 1;
                    let elapsed = start.elapsed();
                    if elapsed >= SLOW_REQUEST {
                        // A multi-second ptz/info means a cold ONVIF
                        // initialization just ran in the background.
                        info!(
                            camera = %camera,
                            ms = elapsed.as_millis() as u64,
                            "frigate onvif cache initialized"
                        );
                    } else {
                        debug!(camera = %camera, "frigate ptz info warm");
                    }
                }
                Ok((401, _)) | Ok((403, _)) => {
                    return Err(format!(
                        "ptz/info for '{camera}' was rejected (auth); check the warmup user/role"
                    ));
                }
                Ok((404, _)) => {
                    debug!(camera = %camera, "camera not found on frigate; skipping");
                }
                Ok((status, _)) => {
                    warn!(camera = %camera, status, "unexpected ptz/info response");
                }
                Err(e) => {
                    return Err(format!("ptz/info for '{camera}' failed: {e}"));
                }
            }
        }
        if warmed == 0 {
            return Err("no camera answered ptz/info".to_string());
        }
        Ok(())
    }

    /// Camera names from Frigate's own config, used when `warmup.cameras`
    /// is not set.
    async fn fetch_camera_list(&self) -> Option<Vec<String>> {
        let (status, body) = self.get("/config").await.ok()?;
        if status != 200 {
            return None;
        }
        let config: serde_json::Value = serde_json::from_slice(&body).ok()?;
        Some(
            config
                .get("cameras")?
                .as_object()?
                .keys()
                .cloned()
                .collect(),
        )
    }

    /// A single minimal HTTP/1.1 GET with proxy auth headers. Reads the
    /// whole response (`Connection: close`).
    async fn get(&self, path: &str) -> std::io::Result<(u16, Vec<u8>)> {
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {}\r\n\
             Accept: application/json\r\n\
             remote-user: {}\r\n\
             remote-role: {}\r\n\
             Connection: close\r\n\
             \r\n",
            self.host_header, self.user, self.role
        );
        let exchange = async {
            let mut stream = TcpStream::connect((self.host.as_str(), self.port)).await?;
            stream.write_all(request.as_bytes()).await?;
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).await?;
            std::io::Result::Ok(raw)
        };
        let raw = tokio::time::timeout(REQUEST_TIMEOUT, exchange).await??;
        let (status, _headers, body) = parse_response(&raw)?;
        Ok((status, body))
    }
}

/// Parse a minimal HTTP/1.1 response into (status, headers, body). Header
/// names are lowercased. Handles a chunked body, though Frigate's API
/// answers with content-length.
fn parse_response(raw: &[u8]) -> std::io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| malformed("no header terminator"))?;
    let head = std::str::from_utf8(&raw[..split]).map_err(|_| malformed("non-utf8 headers"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| malformed("missing status code"))?;
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let mut body = raw[split + 4..].to_vec();
    if headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"))
    {
        body = dechunk(&body);
    }
    Ok((status, headers, body))
}

/// Decode a chunked transfer-encoded body.
fn dechunk(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = data;
    while let Some(pos) = rest.windows(2).position(|w| w == b"\r\n") {
        let len = std::str::from_utf8(&rest[..pos])
            .ok()
            .and_then(|s| usize::from_str_radix(s.trim(), 16).ok());
        let Some(len) = len else { break };
        if len == 0 {
            break;
        }
        let start = pos + 2;
        let end = (start + len).min(rest.len());
        out.extend_from_slice(&rest[start..end]);
        rest = &rest[(end + 2).min(rest.len())..];
    }
    out
}

fn malformed(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_defaults_from_yaml() {
        let cfg: WarmupConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.url, "http://127.0.0.1:5001");
        assert_eq!(cfg.user, "rust2rtc");
        assert_eq!(cfg.role, "viewer");
        assert_eq!(cfg.interval_secs, 60);
        assert!(cfg.cameras.is_empty());
    }

    #[test]
    fn from_config_parses_url_and_interval() {
        let cfg: WarmupConfig = serde_yaml::from_str(
            "url: http://127.0.0.1:5001\ncameras: [driveway]\ninterval_secs: 30\n",
        )
        .unwrap();
        let task = WarmupTask::from_config(&cfg).unwrap();
        assert_eq!(task.host, "127.0.0.1");
        assert_eq!(task.port, 5001);
        assert_eq!(task.host_header, "127.0.0.1:5001");
        assert_eq!(task.interval, Duration::from_secs(30));

        // Sub-minimum intervals are clamped so a misconfigured value cannot
        // turn the warm-up into a busy loop.
        let cfg: WarmupConfig = serde_yaml::from_str("interval_secs: 1\n").unwrap();
        let task = WarmupTask::from_config(&cfg).unwrap();
        assert_eq!(task.interval, Duration::from_secs(5));
    }

    #[test]
    fn from_config_rejects_unusable_urls() {
        let mut cfg: WarmupConfig = serde_yaml::from_str("{}").unwrap();
        cfg.url = "https://frigate.example".to_string();
        assert!(WarmupTask::from_config(&cfg).is_none());
        cfg.url = "not a url".to_string();
        assert!(WarmupTask::from_config(&cfg).is_none());
        cfg.url = "http://127.0.0.1:5001".to_string();
        cfg.enabled = false;
        assert!(WarmupTask::from_config(&cfg).is_none());
    }

    #[test]
    fn parses_http_responses() {
        let (status, headers, body) =
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}").unwrap();
        assert_eq!(status, 200);
        assert_eq!(
            headers,
            vec![("content-length".to_string(), "2".to_string())]
        );
        assert_eq!(body, b"{}");

        let (status, _, body) = parse_response(b"HTTP/1.1 401 Unauthorized\r\n\r\n").unwrap();
        assert_eq!(status, 401);
        assert!(body.is_empty());
    }
}
