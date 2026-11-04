#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use md5::{Digest, Md5};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info};

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct FfmpegManager {
    bin: String,
    global_args: String,
    pub hardware: Option<String>,
    pub templates: HashMap<String, String>,
}

impl FfmpegManager {
    pub fn new(
        bin: String,
        global_args: String,
        hardware: Option<String>,
        templates: HashMap<String, String>,
    ) -> Self {
        Self {
            bin,
            global_args,
            hardware,
            templates,
        }
    }

    pub fn bin(&self) -> &str {
        &self.bin
    }

    /// Resolve H.264 encoder arguments based on configured templates or hardware profile
    pub fn h264_encoder_args(&self) -> Vec<String> {
        if let Some(custom) = self.templates.get("h264") {
            return shell_words(custom);
        }
        match self.hardware.as_deref() {
            Some("rkmpp") => vec![
                "-c:v".to_string(),
                "h264_rkmpp".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            Some("vaapi") => vec![
                "-c:v".to_string(),
                "h264_vaapi".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            Some("cuda") | Some("nvenc") => vec![
                "-c:v".to_string(),
                "h264_nvenc".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            Some("videotoolbox") => vec![
                "-c:v".to_string(),
                "h264_videotoolbox".to_string(),
                "-b:v".to_string(),
                "2M".to_string(),
            ],
            _ => vec![
                "-c:v".to_string(),
                "libx264".to_string(),
                "-preset".to_string(),
                "superfast".to_string(),
                "-tune".to_string(),
                "zerolatency".to_string(),
                "-pix_fmt".to_string(),
                "yuv420p".to_string(),
            ],
        }
    }

    /// Resolve H.264 encoder arguments tailored for SIP softphones:
    /// Constrained Baseline profile (level 3.1), repeated headers (SPS/PPS on every keyframe),
    /// short GOP (1s) for instant recovery, and downscaling to max 720p (matching level 3.1 max-fs limit).
    pub fn sip_h264_encoder_args(&self) -> Vec<String> {
        if let Some(custom) = self.templates.get("sip_h264") {
            return shell_words(custom);
        }
        vec![
            "-vf".to_string(),
            "scale=w='min(1280,iw)':h=-2".to_string(),
            "-c:v".to_string(),
            "libx264".to_string(),
            "-preset".to_string(),
            "ultrafast".to_string(),
            "-tune".to_string(),
            "zerolatency".to_string(),
            "-profile:v".to_string(),
            "baseline".to_string(),
            "-level".to_string(),
            "3.1".to_string(),
            "-x264-params".to_string(),
            "repeat-headers=1".to_string(),
            "-g".to_string(),
            "15".to_string(),
            "-keyint_min".to_string(),
            "15".to_string(),
            "-b:v".to_string(),
            "1500k".to_string(),
            "-maxrate".to_string(),
            "2000k".to_string(),
            "-bufsize".to_string(),
            "3000k".to_string(),
            "-pix_fmt".to_string(),
            "yuv420p".to_string(),
        ]
    }

    /// Check if FFmpeg is available and return version info
    pub async fn check_version(&self) -> Result<String> {
        let output = Command::new(&self.bin)
            .arg("-version")
            .output()
            .await
            .with_context(|| format!("Failed to execute '{}'", self.bin))?;

        if !output.status.success() {
            bail!("FFmpeg returned non-zero status");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let first_line = stdout.lines().next().unwrap_or("FFmpeg (unknown version)");
        Ok(first_line.to_string())
    }

    /// Resolve an input URL or exec string into command line arguments
    /// Replaces `{output}` with the loopback RTSP publish URL
    pub fn build_command(&self, source: &str, rtsp_port: u16, stream_id: &str) -> (String, Vec<String>) {
        if let Some(cmd_str) = source.strip_prefix("exec:") {
            let output_url = if !cmd_str.contains("-rtsp_transport") {
                format!("-rtsp_transport tcp rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id)
            } else {
                format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id)
            };
            let replaced = cmd_str.replace("{output}", &output_url);
            let parts = shell_words(&replaced);
            if parts.is_empty() {
                return (self.bin.clone(), vec![]);
            }
            return (parts[0].clone(), parts[1..].to_vec());
        }

        if let Some(ffmpeg_url) = source.strip_prefix("ffmpeg:") {
            return self.build_ffmpeg_url_args(ffmpeg_url, rtsp_port, stream_id);
        }

        let resolved = resolve_source(source, rtsp_port);

        // Default RTSP or file remux: stream copy video and audio without re-encoding
        let output_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id);
        let args = vec![
            "-hide_banner".to_string(),
            "-v".to_string(),
            "warning".to_string(),
            "-fflags".to_string(),
            "nobuffer".to_string(),
            "-flags".to_string(),
            "low_delay".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            "-i".to_string(),
            resolved,
            "-c".to_string(),
            "copy".to_string(),
            "-f".to_string(),
            "rtsp".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            output_url,
        ];

        (self.bin.clone(), args)
    }

    /// Build FFmpeg arguments from a `ffmpeg:input#param=val` style string
    fn build_ffmpeg_url_args(&self, url_str: &str, rtsp_port: u16, stream_id: &str) -> (String, Vec<String>) {
        let (raw_input, params_str) = match url_str.split_once('#') {
            Some((i, p)) => (i, Some(p)),
            None => (url_str, None),
        };

        let resolved_input = resolve_source(raw_input, rtsp_port);

        let mut args = vec!["-hide_banner".to_string(), "-v".to_string(), "warning".to_string()];

        // Check if stream explicitly requests hardware acceleration (e.g. #hardware=rkmpp)
        let mut hardware_profile = self.hardware.clone();
        if let Some(params) = params_str {
            for param in params.split('#') {
                if let Some((k, v)) = param.split_once('=')
                    && (k == "hardware" || k == "hw") {
                        hardware_profile = Some(v.to_string());
                    }
            }
        }

        // Apply hardware decode acceleration before input
        if let Some(ref hw) = hardware_profile {
            match hw.as_str() {
                "rkmpp" => {
                    args.extend([
                        "-hwaccel".to_string(),
                        "rkmpp".to_string(),
                        "-hwaccel_output_format".to_string(),
                        "drm_prime".to_string(),
                    ]);
                }
                "vaapi" => {
                    args.extend(["-hwaccel".to_string(), "vaapi".to_string()]);
                }
                "cuda" => {
                    args.extend(["-hwaccel".to_string(), "cuda".to_string()]);
                }
                "videotoolbox" => {
                    args.extend(["-hwaccel".to_string(), "videotoolbox".to_string()]);
                }
                custom => {
                    if let Some(custom_flags) = self.templates.get(custom) {
                        args.extend(shell_words(custom_flags));
                    }
                }
            }
        }

        // If input looks like RTSP, use TCP low delay
        if resolved_input.starts_with("rtsp://") {
            args.extend([
                "-fflags".to_string(),
                "nobuffer".to_string(),
                "-flags".to_string(),
                "low_delay".to_string(),
                "-rtsp_transport".to_string(),
                "tcp".to_string(),
            ]);
        }

        args.extend(["-i".to_string(), resolved_input]);

        let mut has_video = false;
        let mut has_audio = false;

        if let Some(params) = params_str {
            for param in params.split('#') {
                if let Some((k, v)) = param.split_once('=') {
                    match k {
                        "video" => {
                            has_video = true;
                            if let Some(custom) = self.templates.get(v) {
                                args.extend(shell_words(custom));
                            } else {
                                match v {
                                    "h264" => {
                                        if hardware_profile.as_deref() == Some("rkmpp") {
                                            args.extend(["-c:v".to_string(), "h264_rkmpp".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("vaapi") {
                                            args.extend(["-c:v".to_string(), "h264_vaapi".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("cuda") || hardware_profile.as_deref() == Some("nvenc") {
                                            args.extend(["-c:v".to_string(), "h264_nvenc".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("videotoolbox") {
                                            args.extend(["-c:v".to_string(), "h264_videotoolbox".to_string()]);
                                        } else {
                                            args.extend([
                                                "-c:v".to_string(),
                                                "libx264".to_string(),
                                                "-preset".to_string(),
                                                "superfast".to_string(),
                                                "-tune".to_string(),
                                                "zerolatency".to_string(),
                                                "-pix_fmt".to_string(),
                                                "yuv420p".to_string(),
                                            ]);
                                        }
                                    }
                                    "h265" | "hevc" => {
                                        if hardware_profile.as_deref() == Some("rkmpp") {
                                            args.extend(["-c:v".to_string(), "hevc_rkmpp".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("vaapi") {
                                            args.extend(["-c:v".to_string(), "hevc_vaapi".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("cuda") || hardware_profile.as_deref() == Some("nvenc") {
                                            args.extend(["-c:v".to_string(), "hevc_nvenc".to_string()]);
                                        } else if hardware_profile.as_deref() == Some("videotoolbox") {
                                            args.extend(["-c:v".to_string(), "hevc_videotoolbox".to_string()]);
                                        } else {
                                            args.extend([
                                                "-c:v".to_string(),
                                                "libx265".to_string(),
                                                "-preset".to_string(),
                                                "superfast".to_string(),
                                                "-tune".to_string(),
                                                "zerolatency".to_string(),
                                            ]);
                                        }
                                    }
                                    "mjpeg" => {
                                        if hardware_profile.as_deref() == Some("rkmpp") {
                                            args.extend(["-c:v".to_string(), "mjpeg_rkmpp".to_string()]);
                                        } else {
                                            args.extend(["-c:v".to_string(), "mjpeg".to_string()]);
                                        }
                                    }
                                    "copy" => {
                                        args.extend(["-c:v".to_string(), "copy".to_string()]);
                                    }
                                    _ => {
                                        args.extend(["-c:v".to_string(), v.to_string()]);
                                    }
                                }
                            }
                        }
                        "audio" => {
                            has_audio = true;
                            match v {
                                "opus" => {
                                    args.extend([
                                        "-c:a".to_string(),
                                        "libopus".to_string(),
                                        "-application".to_string(),
                                        "lowdelay".to_string(),
                                        "-ar".to_string(),
                                        "48000".to_string(),
                                        "-ac".to_string(),
                                        "2".to_string(),
                                    ]);
                                }
                                "pcmu" => {
                                    args.extend([
                                        "-c:a".to_string(),
                                        "pcm_mulaw".to_string(),
                                        "-ar".to_string(),
                                        "8000".to_string(),
                                        "-ac".to_string(),
                                        "1".to_string(),
                                    ]);
                                }
                                "copy" => {
                                    args.extend(["-c:a".to_string(), "copy".to_string()]);
                                }
                                _ => {
                                    args.extend(["-c:a".to_string(), v.to_string()]);
                                }
                            }
                        }
                        "raw" => {
                            args.extend(shell_words(v));
                        }
                        "height" => {
                            args.extend(["-vf".to_string(), format!("scale=-2:{}", v)]);
                        }
                        _ => {}
                    }
                }
            }
        }

        if !has_video && !has_audio {
            args.extend(["-c".to_string(), "copy".to_string()]);
        } else if !has_video {
            args.extend(["-vn".to_string()]);
        } else if !has_audio {
            args.extend(["-an".to_string()]);
        }

        let output_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id);
        args.extend([
            "-f".to_string(),
            "rtsp".to_string(),
            "-rtsp_transport".to_string(),
            "tcp".to_string(),
            output_url,
        ]);

        (self.bin.clone(), args)
    }

    /// Spawns an external FFmpeg child process and streams its stderr for logs
    pub fn spawn_process(&self, bin: &str, args: &[String]) -> Result<Child> {
        info!(bin = bin, args = ?args, "Spawning FFmpeg process");
        let mut child = Command::new(bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("Failed to spawn {}", bin))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    debug!(target: "ffmpeg", "{}", line);
                }
            });
        }

        Ok(child)
    }

    /// Spawns an external FFmpeg child process with piped stdout for streaming transcoded media
    pub fn spawn_stdout_process(&self, args: &[String]) -> Result<Child> {
        info!(bin = %self.bin, args = ?args, "Spawning FFmpeg stdout transcode process");
        let mut child = Command::new(&self.bin)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to spawn {}", self.bin))?;

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    debug!(target: "ffmpeg_stdout", "{}", line);
                }
            });
        }

        Ok(child)
    }

    /// Generate a snapshot JPEG image for a given input source
    pub async fn take_snapshot(&self, source: &str, rtsp_port: u16, is_hevc: bool) -> Result<Bytes> {
        let resolved = resolve_source(source, rtsp_port);
        let mut cmd = Command::new(&self.bin);
        cmd.args(["-hide_banner", "-v", "error"]);

        if resolved.starts_with("rtsp://") {
            cmd.args(["-rtsp_transport", "tcp"]);
            if is_hevc {
                cmd.args(["-skip_frame", "nokey"]);
            } else {
                cmd.args([
                    "-fflags",
                    "nobuffer",
                    "-flags",
                    "low_delay",
                ]);
            }
        }

        cmd.args(["-i", &resolved, "-vframes", "1", "-f", "image2", "-c:v", "mjpeg", "-"]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| "Failed to spawn snapshot FFmpeg")?;
        let mut stdout = child.stdout.take().context("No stdout")?;
        let mut buffer = Vec::new();

        let read_res = tokio::time::timeout(tokio::time::Duration::from_secs(12), stdout.read_to_end(&mut buffer)).await;
        if read_res.is_err() {
            let _ = child.kill().await;
            bail!("FFmpeg snapshot timed out");
        }
        let status = child.wait().await?;

        if !status.success() || buffer.is_empty() {
            bail!("FFmpeg snapshot failed or produced empty image");
        }

        Ok(Bytes::from(buffer))
    }
}

pub fn resolve_source(source: &str, rtsp_port: u16) -> String {
    let clean_source = source
        .split('#')
        .next()
        .unwrap_or(source)
        .trim_end_matches("?backchannel=1");

    if clean_source.starts_with("onvif://")
        && let Ok(url) = url::Url::parse(clean_source) {
            let host = url.host_str().unwrap_or("127.0.0.1");
            let user = url.username();
            let pass = url.password().unwrap_or("");
            return format!("rtsp://{}:{}@{}/Preview_01_main", user, pass, host);
        }

    if clean_source.starts_with("tapo://")
        && let Ok(url) = url::Url::parse(clean_source) {
            let host = url.host_str().unwrap_or("127.0.0.1");
            return format!("rtsp://frigate:password@{}/stream1", host);
        }

    if !clean_source.contains("://") && !clean_source.starts_with("exec:") && !clean_source.starts_with("ffmpeg:") {
        // References another stream by name
        return format!("rtsp://127.0.0.1:{}/{}", rtsp_port, clean_source);
    }

    clean_source.to_string()
}

/// Helper function to generate an MD5 stream hash path like go2rtc
pub fn generate_stream_hash(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

/// Simple shell words splitter for exec strings
fn shell_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut quote_char = ' ';

    for ch in s.chars() {
        if in_quotes {
            if ch == quote_char {
                in_quotes = false;
            } else {
                current.push(ch);
            }
        } else if ch == '"' || ch == '\'' {
            in_quotes = true;
            quote_char = ch;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(current.clone());
                current.clear();
            }
        } else {
            current.push(ch);
        }
    }

    if !current.is_empty() {
        words.push(current);
    }

    words
}
