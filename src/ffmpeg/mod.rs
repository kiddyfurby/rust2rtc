#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use md5::{Digest, Md5};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub struct FfmpegManager {
    bin: String,
    global_args: String,
}

impl FfmpegManager {
    pub fn new(bin: String, global_args: String) -> Self {
        Self { bin, global_args }
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

        // Default RTSP or file remux: stream via ffmpeg into rtsp loopback
        let output_url = format!("rtsp://127.0.0.1:{}/{}", rtsp_port, stream_id);
        let args = vec![
            "-hide_banner".to_string(),
            "-v".to_string(),
            "warning".to_string(),
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
                            match v {
                                "h264" => {
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
                                "h265" => {
                                    args.extend([
                                        "-c:v".to_string(),
                                        "libx265".to_string(),
                                        "-preset".to_string(),
                                        "superfast".to_string(),
                                        "-tune".to_string(),
                                        "zerolatency".to_string(),
                                    ]);
                                }
                                "mjpeg" => {
                                    args.extend(["-c:v".to_string(), "mjpeg".to_string()]);
                                }
                                "copy" => {
                                    args.extend(["-c:v".to_string(), "copy".to_string()]);
                                }
                                _ => {
                                    args.extend(["-c:v".to_string(), v.to_string()]);
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

    /// Generate a snapshot JPEG image for a given input source
    pub async fn take_snapshot(&self, source: &str, rtsp_port: u16) -> Result<Bytes> {
        let resolved = resolve_source(source, rtsp_port);
        let mut cmd = Command::new(&self.bin);
        cmd.args(["-hide_banner", "-v", "error"]);

        if resolved.starts_with("rtsp://") {
            cmd.args(["-rtsp_transport", "tcp"]);
        }

        cmd.args(["-i", &resolved, "-vframes", "1", "-f", "image2", "-c:v", "mjpeg", "-"]);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| "Failed to spawn snapshot FFmpeg")?;
        let mut stdout = child.stdout.take().context("No stdout")?;
        let mut buffer = Vec::new();

        stdout.read_to_end(&mut buffer).await?;
        let status = child.wait().await?;

        if !status.success() || buffer.is_empty() {
            bail!("FFmpeg snapshot failed or produced empty image");
        }

        Ok(Bytes::from(buffer))
    }
}

pub fn resolve_source(source: &str, rtsp_port: u16) -> String {
    if source.starts_with("onvif://") {
        if let Ok(url) = url::Url::parse(source) {
            let host = url.host_str().unwrap_or("127.0.0.1");
            let user = url.username();
            let pass = url.password().unwrap_or("");
            return format!("rtsp://{}:{}@{}/Preview_01_main", user, pass, host);
        }
    }

    if source.starts_with("tapo://") {
        if let Ok(url) = url::Url::parse(source) {
            let host = url.host_str().unwrap_or("127.0.0.1");
            return format!("rtsp://frigate:REDACTED_PASSWORD@{}/stream1", host);
        }
    }

    if !source.contains("://") && !source.starts_with("exec:") && !source.starts_with("ffmpeg:") {
        // References another stream by name
        return format!("rtsp://127.0.0.1:{}/{}", rtsp_port, source);
    }

    source.to_string()
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
