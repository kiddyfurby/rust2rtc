#![allow(dead_code)]

use crate::core::MediaKind;
use crate::ffmpeg::FfmpegManager;
use crate::mp4::{
    parse_h264_sps_dimensions, parse_h265_sps, write_standalone_mp4, H264RtpDepacketizer,
    H265RtpDepacketizer, VideoCodecConfig, VideoSample,
};
use crate::streams::Stream;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::watch;
use tracing::{error, info, warn};

pub struct SegmentRecorder {
    pub stream_name: String,
    pub stream: Arc<Stream>,
    pub output_dir: PathBuf,
    pub segment_duration: u32,
    pub rtsp_port: u16,
    pub ffmpeg: Arc<FfmpegManager>,
}

struct ActiveSegment {
    start_utc: chrono::DateTime<chrono::Utc>,
    start_instant: tokio::time::Instant,
    last_rtp_ts: u32,
    samples: Vec<VideoSample>,
}

struct SegmentJob {
    stream_name: String,
    output_dir: PathBuf,
    start_utc: chrono::DateTime<chrono::Utc>,
    elapsed: std::time::Duration,
    codec_config: VideoCodecConfig,
    samples: Vec<VideoSample>,
}

impl SegmentRecorder {
    pub fn new(
        stream_name: String,
        stream: Arc<Stream>,
        output_dir: PathBuf,
        segment_duration: u32,
        rtsp_port: u16,
        ffmpeg: Arc<FfmpegManager>,
    ) -> Self {
        Self {
            stream_name,
            stream,
            output_dir,
            segment_duration,
            rtsp_port,
            ffmpeg,
        }
    }

    pub async fn run(self: Arc<Self>, mut stop_rx: Option<watch::Receiver<bool>>) {
        info!(
            stream = %self.stream_name,
            dir = %self.output_dir.display(),
            duration = self.segment_duration,
            "Starting in-process MP4 segment recorder"
        );

        let _ = tokio::fs::create_dir_all(&self.output_dir).await;

        let (seg_tx, mut seg_rx) = tokio::sync::mpsc::channel::<SegmentJob>(16);
        tokio::spawn(async move {
            while let Some(job) = seg_rx.recv().await {
                tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let date_str = job.start_utc.format("%Y%m%d%H%M%S+0000").to_string();
                    let target_file = job.output_dir.join(format!("{}@{}.mp4", job.stream_name, date_str));
                    let tmp_file = job.output_dir.join(format!(".{}@{}.mp4.tmp", job.stream_name, date_str));
                    let write_res = std::fs::File::create(&tmp_file).and_then(|file| {
                        let mut writer = std::io::BufWriter::with_capacity(65536, file);
                        let bytes = write_standalone_mp4(&mut writer, &job.codec_config, &job.samples, None)?;
                        writer.flush()?;
                        Ok(bytes)
                    });
                    let written_bytes = match write_res {
                        Ok(b) => b,
                        Err(e) => {
                            error!(stream = %job.stream_name, error = %e, "Failed to write MP4 segment tmp file");
                            return;
                        }
                    };
                    if let Err(e) = std::fs::rename(&tmp_file, &target_file) {
                        error!(stream = %job.stream_name, error = %e, "Failed to atomically rename MP4 segment");
                    } else {
                        info!(
                            stream = %job.stream_name,
                            path = %target_file.display(),
                            bytes = written_bytes,
                            samples = job.samples.len(),
                            duration = ?job.elapsed,
                            "Saved MP4 segment for Frigate cache"
                        );
                    }
                })
                .await
                .ok();
            }
        });

        loop {
            if let Some(ref rx) = stop_rx
                && *rx.borrow() {
                    info!(stream = %self.stream_name, "Stop signal active; terminating recorder");
                    return;
                }

            // 1. Ensure producer is running
            if let Err(e) = self.stream.ensure_started(self.rtsp_port, &self.ffmpeg).await {
                warn!(stream = %self.stream_name, error = %e, "Failed to start stream producer; retrying in 3s");
                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                continue;
            }

            // 2. Discover video track
            let video_track = self
                .stream
                .wait_for_track(tokio::time::Duration::from_secs(15), |t| t.kind == MediaKind::Video)
                .await;

            let video_track = match video_track {
                Some(t) => t,
                None => {
                    warn!(stream = %self.stream_name, "No video track available; retrying in 5s");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            let is_hevc = video_track.codec.name.eq_ignore_ascii_case("H265")
                || video_track.codec.name.eq_ignore_ascii_case("HEVC");

            info!(
                stream = %self.stream_name,
                codec = %video_track.codec.name,
                "Recorder subscribed to video track"
            );

            let mut rx = video_track.subscribe();
            let mut h264_depack = (!is_hevc).then(H264RtpDepacketizer::new);
            let mut h265_depack = is_hevc.then(H265RtpDepacketizer::new);

            // Pre-seed parameter sets if already cached in track
            let initial_params = video_track.get_param_sets_data();
            if let Some(ref mut depack) = h265_depack {
                if let Some(ref vps) = initial_params.h265_vps {
                    depack.vps = Some(vps.clone());
                }
                if let Some(ref sps) = initial_params.h265_sps {
                    depack.sps = Some(sps.clone());
                }
                if let Some(ref pps) = initial_params.h265_pps {
                    depack.pps = Some(pps.clone());
                }
            }
            if let Some(ref mut depack) = h264_depack {
                if let Some(ref sps) = initial_params.h264_sps {
                    depack.sps = Some(sps.clone());
                }
                if let Some(ref pps) = initial_params.h264_pps {
                    depack.pps = Some(pps.clone());
                }
            }

            let mut current_segment: Option<ActiveSegment> = None;

            loop {
                let packet = tokio::select! {
                    stop_signal = async {
                        if let Some(ref mut rx) = stop_rx {
                            while rx.changed().await.is_ok() {
                                if *rx.borrow() {
                                    return true;
                                }
                            }
                        }
                        std::future::pending().await
                    } => {
                        if stop_signal {
                            info!(stream = %self.stream_name, "Stop signal received; exiting recorder");
                            return;
                        }
                        continue;
                    }
                    res = rx.recv() => {
                        match res {
                            Ok(p) => p,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!(stream = %self.stream_name, skipped = n, "Recorder subscriber lagged; resetting depacketizer");
                                if let Some(ref mut d) = h265_depack {
                                    d.reset();
                                }
                                if let Some(ref mut d) = h264_depack {
                                    d.reset();
                                }
                                current_segment = None;
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                warn!(stream = %self.stream_name, "Track channel closed; reconnecting");
                                break;
                            }
                        }
                    }
                };

                let mut frame_opt = if let Some(ref mut d) = h265_depack {
                    d.feed_packet(&packet)
                } else if let Some(ref mut d) = h264_depack {
                    d.feed_packet(&packet)
                } else {
                    None
                };

                while let Some((is_keyframe, rtp_ts, nalus)) = frame_opt {

                // A segment MUST always start with a keyframe
                if current_segment.is_none() {
                    if !is_keyframe {
                        frame_opt = if let Some(ref mut d) = h265_depack {
                            d.pop_frame()
                        } else if let Some(ref mut d) = h264_depack {
                            d.pop_frame()
                        } else {
                            None
                        };
                        continue;
                    }
                    current_segment = Some(ActiveSegment {
                        start_utc: chrono::Utc::now(),
                        start_instant: tokio::time::Instant::now(),
                        last_rtp_ts: rtp_ts,
                        samples: vec![VideoSample {
                            is_keyframe: true,
                            duration: 6000,
                            nalus,
                        }],
                    });
                    frame_opt = if let Some(ref mut d) = h265_depack {
                        d.pop_frame()
                    } else if let Some(ref mut d) = h264_depack {
                        d.pop_frame()
                    } else {
                        None
                    };
                    continue;
                }

                let seg = current_segment.as_mut().unwrap();

                // Compute exact delta from RTP timestamp
                let ts_delta = rtp_ts.wrapping_sub(seg.last_rtp_ts);
                seg.last_rtp_ts = rtp_ts;
                let sample_duration = if ts_delta > 0 && ts_delta < 90000 {
                    ts_delta
                } else {
                    6000
                };

                // Fix up previous sample's duration with the true delta
                if let Some(last_sample) = seg.samples.last_mut() {
                    last_sample.duration = sample_duration;
                }

                let elapsed = seg.start_instant.elapsed();
                let should_cut = elapsed >= tokio::time::Duration::from_secs(self.segment_duration as u64)
                    && is_keyframe;

                if should_cut {
                    let finished_seg = current_segment.take().unwrap();

                    // Resolve codec config
                    let codec_config_opt = if let Some(ref d) = h265_depack {
                        let track_params = video_track.get_param_sets_data();
                        let vps = d.vps.clone().or(track_params.h265_vps);
                        let sps = d.sps.clone().or(track_params.h265_sps);
                        let pps = d.pps.clone().or(track_params.h265_pps);

                        if let (Some(vps), Some(sps), Some(pps)) = (vps, sps, pps) {
                            let (w, h, _) = parse_h265_sps(&sps);
                            Some(VideoCodecConfig::H265 {
                                vps,
                                sps,
                                pps,
                                width: w,
                                height: h,
                            })
                        } else {
                            None
                        }
                    } else if let Some(ref d) = h264_depack {
                        let track_params = video_track.get_param_sets_data();
                        let sps = d.sps.clone().or(track_params.h264_sps);
                        let pps = d.pps.clone().or(track_params.h264_pps);

                        if let (Some(sps), Some(pps)) = (sps, pps) {
                            let (w, h) = parse_h264_sps_dimensions(&sps);
                            Some(VideoCodecConfig::H264 {
                                sps,
                                pps,
                                width: w,
                                height: h,
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    if let Some(codec_config) = codec_config_opt {
                        let job = SegmentJob {
                            stream_name: self.stream_name.clone(),
                            output_dir: self.output_dir.clone(),
                            start_utc: finished_seg.start_utc,
                            elapsed,
                            codec_config,
                            samples: finished_seg.samples,
                        };
                        if let Err(e) = seg_tx.send(job).await {
                            error!(stream = %self.stream_name, error = %e, "Failed to queue MP4 segment for background writer");
                        }
                    } else {
                        warn!(stream = %self.stream_name, "Skipping segment write: missing parameter sets (VPS/SPS/PPS)");
                    }

                    // Start new segment with the current keyframe
                    current_segment = Some(ActiveSegment {
                        start_utc: chrono::Utc::now(),
                        start_instant: tokio::time::Instant::now(),
                        last_rtp_ts: rtp_ts,
                        samples: vec![VideoSample {
                            is_keyframe: true,
                            duration: sample_duration,
                            nalus,
                        }],
                    });
                } else {
                    seg.samples.push(VideoSample {
                        is_keyframe,
                        duration: sample_duration,
                        nalus,
                    });
                }

                frame_opt = if let Some(ref mut d) = h265_depack {
                    d.pop_frame()
                } else if let Some(ref mut d) = h264_depack {
                    d.pop_frame()
                } else {
                    None
                };
            }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
    }
}

pub struct RecordManager {
    pub stream_manager: Arc<crate::streams::StreamManager>,
    pub rtsp_port: u16,
    pub ffmpeg: Arc<FfmpegManager>,
    pub dir: PathBuf,
    pub segment_duration: u32,
    pub enabled: bool,
    configured_streams: RwLock<HashSet<String>>,
    active_recorders: tokio::sync::Mutex<HashMap<String, watch::Sender<bool>>>,
}

impl RecordManager {
    pub fn new(
        record_config: Option<crate::config::RecordConfig>,
        stream_manager: Arc<crate::streams::StreamManager>,
        rtsp_port: u16,
        ffmpeg: Arc<FfmpegManager>,
    ) -> Arc<Self> {
        let (enabled, dir, duration, streams) = if let Some(cfg) = record_config {
            (
                cfg.enabled,
                PathBuf::from(cfg.dir),
                cfg.segment_duration,
                cfg.streams.into_iter().collect::<HashSet<_>>(),
            )
        } else {
            (true, PathBuf::from("/tmp/cache"), 10, HashSet::new())
        };

        let mgr = Arc::new(Self {
            stream_manager,
            rtsp_port,
            ffmpeg,
            dir,
            segment_duration: duration,
            enabled,
            configured_streams: RwLock::new(streams),
            active_recorders: tokio::sync::Mutex::new(HashMap::new()),
        });

        // Auto-start any streams configured in YAML
        let configured = mgr.configured_streams.read().unwrap().clone();
        for stream_name in configured {
            let m = Arc::clone(&mgr);
            tokio::spawn(async move {
                let _ = m.start(&stream_name).await;
            });
        }

        mgr
    }

    pub fn is_configured(&self, stream: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let configured = self.configured_streams.read().unwrap();
        // When streams list is empty, all streams are handled in-process
        configured.is_empty() || configured.contains(stream)
    }

    pub async fn is_active(&self, stream: &str) -> bool {
        self.active_recorders.lock().await.contains_key(stream)
    }

    pub async fn start(self: &Arc<Self>, stream_name: &str) -> Result<(), String> {
        if !self.is_configured(stream_name) {
            return Err(format!("Stream '{}' not configured for in-process recording", stream_name));
        }

        let mut recorders = self.active_recorders.lock().await;
        if recorders.contains_key(stream_name) {
            info!(stream = %stream_name, "In-process recorder already active for stream");
            return Ok(());
        }

        let (tx, rx) = watch::channel(false);
        recorders.insert(stream_name.to_string(), tx);

        let this = Arc::clone(self);
        let s_name = stream_name.to_string();

        tokio::spawn(async move {
            let mut stream = None;
            for _ in 0..30 {
                if let Some(s) = this.stream_manager.get(&s_name).await {
                    stream = Some(s);
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }

            let stream = match stream {
                Some(s) => s,
                None => {
                    error!(stream = %s_name, "Recording stream not found in StreamManager");
                    this.active_recorders.lock().await.remove(&s_name);
                    return;
                }
            };

            let recorder = Arc::new(SegmentRecorder::new(
                s_name.clone(),
                stream,
                this.dir.clone(),
                this.segment_duration,
                this.rtsp_port,
                Arc::clone(&this.ffmpeg),
            ));

            recorder.run(Some(rx)).await;
            this.active_recorders.lock().await.remove(&s_name);
        });

        Ok(())
    }

    pub async fn stop(&self, stream_name: &str) -> Result<(), String> {
        let mut recorders = self.active_recorders.lock().await;
        if let Some(tx) = recorders.remove(stream_name) {
            let _ = tx.send(true);
            info!(stream = %stream_name, "Stopped in-process recorder for stream");
        }
        Ok(())
    }
}

pub fn run_ffmpeg_shim(args: Vec<String>) -> ! {
    let real_ffmpeg = std::env::var("REAL_FFMPEG")
        .unwrap_or_else(|_| "/usr/lib/ffmpeg/6.0/bin/ffmpeg".to_string());

    // 1. Detect if this is a segment recording command
    let is_segment = args.windows(2).any(|w| w[0] == "-f" && w[1] == "segment")
        || args.iter().any(|a| a == "-f segment" || a.starts_with("-segment_format"));

    if !is_segment {
        exec_real_ffmpeg(&real_ffmpeg, &args);
    }

    // 2. Identify camera / stream name from segment output pattern (e.g. /tmp/cache/<camera>@%Y%m%d%H%M%S%z.mp4)
    let mut camera_name = None;
    for arg in &args {
        if arg.contains('@') && (arg.contains("/cache/") || arg.ends_with(".mp4") || arg.contains("%Y%m%d"))
            && let Some(filename) = std::path::Path::new(arg).file_name().and_then(|f| f.to_str())
                && let Some((cam, _)) = filename.split_once('@') {
                    camera_name = Some(cam.to_string());
                    break;
                }
    }

    let camera = match camera_name {
        Some(c) => c,
        None => {
            exec_real_ffmpeg(&real_ffmpeg, &args);
        }
    };

    // 3. Ask rust2rtc if this stream should be recorded in-process
    let handled = call_rust2rtc_record_api(&camera, "start");
    if !handled {
        exec_real_ffmpeg(&real_ffmpeg, &args);
    }

    eprintln!(
        "[rust2rtc ffmpeg-shim] Stream '{}' recording handled in-process by rust2rtc. Shim standing by.",
        camera
    );

    // 4. Supervise the recorder until SIGINT, SIGTERM, or SIGHUP
    run_recorder_supervisor(&camera);
}

fn exec_real_ffmpeg(real_ffmpeg: &str, args: &[String]) -> ! {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new(real_ffmpeg).args(args).exec();
        eprintln!("[rust2rtc ffmpeg-shim] Failed to exec {}: {}", real_ffmpeg, err);
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        std::process::exit(1);
    }
}

fn call_rust2rtc_record_api(stream: &str, action: &str) -> bool {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let api_port: u16 = std::env::var("GO2RTC_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1984);

    let mut retries = 5;
    while retries > 0 {
        if let Ok(mut conn) = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], api_port)),
            Duration::from_millis(500),
        ) {
            let req = format!(
                "GET /api/record?stream={}&action={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                stream, action, api_port
            );
            if conn.write_all(req.as_bytes()).is_ok() {
                let mut reader = BufReader::new(conn);
                let mut status_line = String::new();
                if reader.read_line(&mut status_line).is_ok() {
                    if status_line.contains(" 200 ") {
                        return true;
                    } else if status_line.contains(" 404 ") {
                        return false;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        retries -= 1;
    }
    false
}

/// Set by the shim's signal handlers; checked by the supervisor loop.
static SHIM_SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn shim_on_signal(_: std::os::raw::c_int) {
    SHIM_SHUTDOWN.store(true, Ordering::SeqCst);
}

/// How often the supervisor re-checks the recorder status (seconds).
const RECORDER_POLL_SECS: usize = 10;

/// Keep the in-process recorder alive across rust2rtc restarts.
///
/// The shim is spawned by Frigate and outlives rust2rtc: config reloads,
/// redeploys, or crashes drop the in-process recorders while the shim keeps
/// standing by, silently stopping all recordings (Frigate's watchdog only
/// reacts to process death, and the shim never dies). Poll the record API and
/// re-request recording whenever the recorder is gone, until SIGINT, SIGTERM,
/// or SIGHUP arrives.
fn run_recorder_supervisor(camera: &str) -> ! {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, shim_on_signal as *const () as usize);
        libc::signal(libc::SIGTERM, shim_on_signal as *const () as usize);
        libc::signal(libc::SIGHUP, shim_on_signal as *const () as usize);
    }

    while !SHIM_SHUTDOWN.load(Ordering::SeqCst) {
        // Recorder gone (e.g. rust2rtc restarted)? Ask for it again.
        if matches!(call_rust2rtc_record_status(camera), Some(false)) {
            let _ = call_rust2rtc_record_api(camera, "start");
        }

        // Sleep in 1s slices so shutdown signals are honoured promptly.
        for _ in 0..RECORDER_POLL_SECS {
            if SHIM_SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }

    eprintln!("[rust2rtc ffmpeg-shim] Stopping in-process recording for '{}'", camera);
    let _ = call_rust2rtc_record_api(camera, "stop");
    std::process::exit(0);
}

/// Query `/api/record?action=status` and report whether the in-process
/// recorder is currently active. `None` when the API is unreachable or the
/// response carries no recognisable flag.
fn call_rust2rtc_record_status(stream: &str) -> Option<bool> {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let api_port: u16 = std::env::var("GO2RTC_API_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(1984);

    let mut conn = TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], api_port)),
        Duration::from_millis(500),
    )
    .ok()?;
    let req = format!(
        "GET /api/record?stream={}&action=status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        stream, api_port
    );
    conn.write_all(req.as_bytes()).ok()?;

    let mut reader = BufReader::new(conn);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).ok()?;
    if !status_line.contains(" 200 ") {
        return None;
    }
    let mut body = String::new();
    reader.read_to_string(&mut body).ok()?;
    status_reports_active(&body)
}

/// Parse the `active` flag from a `/api/record` status response body.
fn status_reports_active(body: &str) -> Option<bool> {
    if body.contains("\"active\":true") {
        Some(true)
    } else if body.contains("\"active\":false") {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_reports_active() {
        assert_eq!(
            status_reports_active("{\"stream\":\"doorbell\",\"configured\":true,\"active\":true}"),
            Some(true)
        );
        assert_eq!(
            status_reports_active("{\"stream\":\"doorbell\",\"configured\":true,\"active\":false}"),
            Some(false)
        );
        assert_eq!(status_reports_active("not json at all"), None);
        assert_eq!(status_reports_active(""), None);
    }

    #[tokio::test]
    async fn test_record_manager_is_configured_defaults() {
        let stream_mgr = Arc::new(crate::streams::StreamManager::new());
        let ffmpeg_mgr = Arc::new(FfmpegManager::new("ffmpeg".into(), "".into(), None, HashMap::new()));

        // 1. When record config is None -> default is enabled for ALL streams
        let mgr_default = RecordManager::new(None, stream_mgr.clone(), 8554, ffmpeg_mgr.clone());
        assert!(mgr_default.is_configured("any_camera"));
        assert!(mgr_default.is_configured("doorbell"));

        // 2. When record config has empty streams list -> enabled for ALL streams
        let mgr_empty_streams = RecordManager::new(
            Some(crate::config::RecordConfig {
                enabled: true,
                dir: "/tmp/cache".into(),
                segment_duration: 10,
                streams: vec![],
            }),
            stream_mgr.clone(),
            8554,
            ffmpeg_mgr.clone(),
        );
        assert!(mgr_empty_streams.is_configured("plato"));
        assert!(mgr_empty_streams.is_configured("kitchen"));

        // 3. When record config has specific streams -> only those return true
        let mgr_whitelist = RecordManager::new(
            Some(crate::config::RecordConfig {
                enabled: true,
                dir: "/tmp/cache".into(),
                segment_duration: 10,
                streams: vec!["plato".into()],
            }),
            stream_mgr.clone(),
            8554,
            ffmpeg_mgr.clone(),
        );
        assert!(mgr_whitelist.is_configured("plato"));
        assert!(!mgr_whitelist.is_configured("kitchen"));

        // 4. When record is disabled -> returns false for all
        let mgr_disabled = RecordManager::new(
            Some(crate::config::RecordConfig {
                enabled: false,
                dir: "/tmp/cache".into(),
                segment_duration: 10,
                streams: vec![],
            }),
            stream_mgr,
            8554,
            ffmpeg_mgr,
        );
        assert!(!mgr_disabled.is_configured("plato"));
    }
}
