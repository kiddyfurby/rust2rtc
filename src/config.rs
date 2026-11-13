use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct Config {
    #[serde(default)]
    pub worker_threads: Option<usize>,
    #[serde(default)]
    pub streams: HashMap<String, StreamConfig>,
    #[serde(default)]
    pub api: ApiConfig,
    #[serde(default)]
    pub rtsp: RtspConfig,
    #[serde(default)]
    pub ffmpeg: FfmpegConfig,
    #[serde(default)]
    pub webrtc: Option<WebRtcConfig>,
    #[serde(default)]
    pub sip: Option<SipConfig>,
    #[serde(default)]
    pub ha: Option<HaConfig>,
    #[serde(default)]
    pub doorbells: HashMap<String, DoorbellConfig>,
    #[serde(default)]
    pub record: Option<RecordConfig>,
    #[serde(default)]
    pub warmup: Option<WarmupConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_record_dir")]
    pub dir: String,
    #[serde(default = "default_segment_duration")]
    pub segment_duration: u32,
    #[serde(default)]
    pub streams: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: default_record_dir(),
            segment_duration: default_segment_duration(),
            streams: Vec::new(),
        }
    }
}

fn default_record_dir() -> String {
    "/tmp/cache".to_string()
}

fn default_segment_duration() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HaConfig {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DoorbellConfig {
    pub entity: String,
    #[serde(default)]
    pub sip_targets: Vec<String>,
    #[serde(default)]
    pub ha_destination: Option<String>,
    #[serde(default = "default_doorbell_cooldown")]
    pub cooldown_seconds: u64,
}

fn default_doorbell_cooldown() -> u64 {
    15
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebRtcConfig {
    #[serde(default)]
    pub candidates: Vec<String>,
    /// Local addresses the WebRTC media sockets bind on (one socket each).
    /// Empty (default): every interface, which advertises a host candidate
    /// per interface on multi-homed hosts. Set to a single address to keep
    /// ICE negotiation on that interface.
    #[serde(default)]
    pub bind_ips: Vec<IpAddr>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub max_payload_size: Option<usize>,
}

/// Periodic pre-warm of Frigate's ONVIF camera caches.
///
/// Frigate initializes each camera's ONVIF controller lazily: the first
/// `ptz/info` request after a (re)start pays a one-time SOAP initialization
/// (seconds for PTZ cameras) that blocks the API event loop, which is what
/// makes the first post-restart dashboard load slow. The warm-up task
/// requests `ptz/info` for the configured cameras locally and periodically
/// so the caches are already warm when a browser connects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WarmupConfig {
    #[serde(default = "default_warmup_enabled")]
    pub enabled: bool,
    /// Base URL of the Frigate HTTP API.
    #[serde(default = "default_warmup_url")]
    pub url: String,
    /// Value for the `remote-user` proxy auth header.
    #[serde(default = "default_warmup_user")]
    pub user: String,
    /// Value for the `remote-role` proxy auth header; the role needs
    /// camera read access.
    #[serde(default = "default_warmup_role")]
    pub role: String,
    /// Cameras to warm. When empty, the camera list is read from Frigate.
    #[serde(default)]
    pub cameras: Vec<String>,
    /// Seconds between warm-up cycles.
    #[serde(default = "default_warmup_interval_secs")]
    pub interval_secs: u64,
}

fn default_warmup_enabled() -> bool {
    true
}

fn default_warmup_url() -> String {
    "http://127.0.0.1:5001".to_string()
}

fn default_warmup_user() -> String {
    "rust2rtc".to_string()
}

fn default_warmup_role() -> String {
    "viewer".to_string()
}

fn default_warmup_interval_secs() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SipConfig {
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub local_ip: Option<String>,
    #[serde(default)]
    pub ha_url: Option<String>,
    #[serde(default)]
    pub ha_token: Option<String>,
    #[serde(default)]
    pub auto_register_streams: bool,
    #[serde(default = "default_base_extension")]
    pub base_extension: u16,
    #[serde(default = "default_auto_password")]
    pub auto_password: String,
    #[serde(default)]
    pub names: HashMap<String, String>,
    #[serde(default)]
    pub max_call_duration_secs: Option<u64>,
    #[serde(default)]
    pub accounts: HashMap<String, SipAccountYamlConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SipAccountYamlConfig {
    #[serde(default)]
    pub extension: Option<String>,
    #[serde(default)]
    pub auth_user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(alias = "name", alias = "sip_name", default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub ring_group: Option<String>,
    #[serde(default = "default_auto_answer")]
    pub auto_answer: bool,
    #[serde(default)]
    pub max_call_duration_secs: Option<u64>,
}

fn default_base_extension() -> u16 {
    101
}

fn default_auto_password() -> String {
    "YourSecurePasswordHere".to_string()
}

fn default_auto_answer() -> bool {
    true
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamConfig {
    Single(String),
    Multiple(Vec<String>),
    Detailed(StreamDetailedConfig),
}

/// Expanded (map) form of a stream entry, allowing per-stream options:
///
/// ```yaml
/// streams:
///   doorbell:
///     sources:
///       - "rtsp://camera/Preview_01_main"
///     keep_warm_secs: 86400
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamDetailedConfig {
    /// Camera sources; accepts the same string-or-list forms as the shorthand.
    pub sources: Box<StreamConfig>,
    /// Keep the on-demand RTSP producer alive this many seconds after the last
    /// consumer disconnects (keeps the GOP cache hot so SIP calls and new
    /// subscribers get an instant keyframe). 0 or absent keeps the default
    /// 50s idle stop.
    #[serde(default)]
    pub keep_warm_secs: Option<u64>,
}

impl StreamConfig {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StreamConfig::Single(s) => vec![s.clone()],
            StreamConfig::Multiple(v) => v.clone(),
            StreamConfig::Detailed(d) => d.sources.to_vec(),
        }
    }

    /// Per-stream keep-warm window in seconds (expanded map form only).
    pub fn keep_warm_secs(&self) -> Option<u64> {
        match self {
            StreamConfig::Detailed(d) => d.keep_warm_secs,
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiConfig {
    #[serde(default = "default_api_listen")]
    pub listen: String,
    #[serde(default)]
    pub origin: Option<String>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            listen: default_api_listen(),
            origin: None,
        }
    }
}

fn default_api_listen() -> String {
    ":1984".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RtspConfig {
    #[serde(default = "default_rtsp_listen")]
    pub listen: String,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            listen: default_rtsp_listen(),
        }
    }
}

fn default_rtsp_listen() -> String {
    ":8554".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FfmpegConfig {
    #[serde(default = "default_ffmpeg_bin")]
    pub bin: String,
    #[serde(default = "default_ffmpeg_global")]
    pub global: String,
    #[serde(default)]
    pub hardware: Option<String>,
    #[serde(flatten, default)]
    pub templates: HashMap<String, String>,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self {
            bin: default_ffmpeg_bin(),
            global: default_ffmpeg_global(),
            hardware: None,
            templates: HashMap::new(),
        }
    }
}

fn default_ffmpeg_bin() -> String {
    "ffmpeg".to_string()
}

fn default_ffmpeg_global() -> String {
    "-hide_banner -v error".to_string()
}

impl Config {
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        let config: Config = serde_yaml::from_str(&content)?;
        Ok(config)
    }

    pub fn merge(&mut self, other: Config) {
        if other.worker_threads.is_some() {
            self.worker_threads = other.worker_threads;
        }
        for (name, stream) in other.streams {
            self.streams.insert(name, stream);
        }
        if other.api.listen != default_api_listen() {
            self.api = other.api;
        }
        if other.rtsp.listen != default_rtsp_listen() {
            self.rtsp = other.rtsp;
        }
        if other.ffmpeg.bin != default_ffmpeg_bin() || other.ffmpeg.global != default_ffmpeg_global() {
            self.ffmpeg = other.ffmpeg;
        }
        if let Some(webrtc) = other.webrtc {
            if let Some(ref mut our_webrtc) = self.webrtc {
                for cand in webrtc.candidates {
                    if !our_webrtc.candidates.contains(&cand) {
                        our_webrtc.candidates.push(cand);
                    }
                }
                for ip in webrtc.bind_ips {
                    if !our_webrtc.bind_ips.contains(&ip) {
                        our_webrtc.bind_ips.push(ip);
                    }
                }
                if webrtc.engine.is_some() {
                    our_webrtc.engine = webrtc.engine;
                }
                if webrtc.max_payload_size.is_some() {
                    our_webrtc.max_payload_size = webrtc.max_payload_size;
                }
            } else {
                self.webrtc = Some(webrtc);
            }
        }
        if let Some(sip) = other.sip {
            self.sip = Some(sip);
        }
        if let Some(ha) = other.ha {
            self.ha = Some(ha);
        }
        for (name, db) in other.doorbells {
            self.doorbells.insert(name, db);
        }
        if let Some(record) = other.record {
            self.record = Some(record);
        }
        if let Some(warmup) = other.warmup {
            self.warmup = Some(warmup);
        }
    }

    pub fn ha_url(&self) -> Option<String> {
        self.ha
            .as_ref()
            .and_then(|h| h.url.clone())
            .or_else(|| self.sip.as_ref().and_then(|s| s.ha_url.clone()))
            .or_else(|| std::env::var("HA_URL").ok())
    }

    pub fn ha_token(&self) -> Option<String> {
        self.ha
            .as_ref()
            .and_then(|h| h.token.clone())
            .or_else(|| self.sip.as_ref().and_then(|s| s.ha_token.clone()))
            .or_else(|| std::env::var("HA_TOKEN").ok())
            .or_else(|| std::env::var("SUPERVISOR_TOKEN").ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sip_names_and_ffmpeg_config() {
        let yaml = r#"
sip:
  server: "198.51.100.4:5060"
  names:
    living2: "Living Room 2"
    doorbell: "Front Door"
  accounts:
    patio:
      sip_name: "Back Patio"

ffmpeg:
  hardware: "rkmpp"
  h264: "-c:v h264_rkmpp -b:v 2M"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("Failed to parse YAML");
        let sip = cfg.sip.expect("SIP config missing");
        assert_eq!(sip.names.get("living2").unwrap(), "Living Room 2");
        assert_eq!(sip.names.get("doorbell").unwrap(), "Front Door");
        assert_eq!(sip.accounts.get("patio").unwrap().display_name.as_deref(), Some("Back Patio"));

        assert_eq!(cfg.ffmpeg.hardware.as_deref(), Some("rkmpp"));
        assert_eq!(cfg.ffmpeg.templates.get("h264").unwrap(), "-c:v h264_rkmpp -b:v 2M");
    }

    #[test]
    fn test_record_config_defaults() {
        // When completely omitted:
        let yaml = "streams: {}\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.record.is_none());

        // When record is provided with defaults:
        let yaml_record = "record: {}\n";
        let cfg2: Config = serde_yaml::from_str(yaml_record).unwrap();
        let rec = cfg2.record.expect("record block present");
        assert!(rec.enabled);
        assert_eq!(rec.dir, "/tmp/cache");
        assert_eq!(rec.segment_duration, 10);
        assert!(rec.streams.is_empty());
    }

    #[test]
    fn test_worker_threads_config() {
        let yaml = "worker_threads: 4\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.worker_threads, Some(4));

        let mut base = Config::default();
        assert_eq!(base.worker_threads, None);
        base.merge(cfg);
        assert_eq!(base.worker_threads, Some(4));
    }

    #[test]
    fn test_stream_config_expanded_form() {
        let yaml = r#"
streams:
  doorbell:
    sources:
      - "rtsp://198.51.100.42/Preview_01_main"
      - "ffmpeg:doorbell#audio=opus"
    keep_warm_secs: 600
  garden: "rtsp://198.51.100.20/stream"
  plato:
    - "rtsp://198.51.100.21/stream"
"#;
        let cfg: Config = serde_yaml::from_str(yaml).expect("Failed to parse YAML");

        let doorbell = cfg.streams.get("doorbell").expect("doorbell stream");
        assert_eq!(
            doorbell.to_vec(),
            vec!["rtsp://198.51.100.42/Preview_01_main", "ffmpeg:doorbell#audio=opus"]
        );
        assert_eq!(doorbell.keep_warm_secs(), Some(600));

        // Shorthand forms keep working and report no keep-warm window.
        let garden = cfg.streams.get("garden").expect("garden stream");
        assert_eq!(garden.to_vec(), vec!["rtsp://198.51.100.20/stream"]);
        assert_eq!(garden.keep_warm_secs(), None);

        let plato = cfg.streams.get("plato").expect("plato stream");
        assert_eq!(plato.to_vec(), vec!["rtsp://198.51.100.21/stream"]);
        assert_eq!(plato.keep_warm_secs(), None);
    }

    #[test]
    fn test_stream_merge_expanded_replaces_shorthand() {
        let mut base: Config =
            serde_yaml::from_str("streams:\n  doorbell: \"rtsp://old\"\n").unwrap();
        let over: Config = serde_yaml::from_str(
            "streams:\n  doorbell:\n    sources: [\"rtsp://new\"]\n    keep_warm_secs: 60\n",
        )
        .unwrap();

        base.merge(over);
        let doorbell = base.streams.get("doorbell").expect("doorbell stream");
        assert_eq!(doorbell.to_vec(), vec!["rtsp://new"]);
        assert_eq!(doorbell.keep_warm_secs(), Some(60));
    }
}
