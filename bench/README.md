# Benchmarking & Reproducibility Guide

This directory contains scripts and documentation to reproduce the memory (RSS), CPU, and API compatibility benchmarks comparing `rust2rtc` and `go2rtc`.

---

## 1. How the Benchmarks Are Measured

To avoid ambiguity across different hardware and stream counts, we break down `rust2rtc` memory consumption into three distinct operating profiles:

| Profile | Description | `rust2rtc` RSS | `go2rtc` (v1.9.14) RSS | Notes |
| :--- | :--- | :---: | :---: | :--- |
| **1. Idle (No active streams)** | Gateway process started with 14 configured streams (7 main + 7 sub), 0 active RTSP/WebRTC connections | **~4.5 MB** | **~15 – 25 MB** | Measured via `ps -o rss=` immediately after startup before on-demand producers connect. |
| **2. Active Streaming (7 Cameras, Pure-Rust Path)** | 7 concurrent RTSP TCP camera feeds ingested in-process (5MP H.264/HEVC + AAC/G.711), serving RTSP relay + in-process MP4 recording | **~13.6 MB** (Linux `musl` ARM64)<br>**~20 – 35 MB** (macOS / with active WebRTC Opus transcode) | **~35 – 65 MB** (plus external `ffmpeg` processes if transcoding audio) | On embedded Linux (`aarch64-unknown-linux-musl` on RK3588), migrating from `webrtc-rs` (57.7 MB) to `rustrtc` reduced active gateway RSS to **13.6 MB**. |
| **3. Frigate 7-Camera Segment Recording** | 7 recording workers writing 10-second `.mp4` cache segments to `/tmp/cache` | **~13.2 MB total** (7 × ~1.5–1.9 MB `ffmpeg-shim` processes; muxing runs inside `rust2rtc`) | **~180 – 250 MB total** (7 × ~25–38 MB real `ffmpeg -f segment` processes spawned by Frigate) | Saves **~170–200 MB of RAM** (~93% reduction in recording subprocess RSS) across a 7-camera Frigate NVR deployment. |

### Why Earlier Numbers Differed in the Changelog
- **`~4.5 MB`**: Cold/idle RSS of the `rust2rtc` binary with zero active camera streams connected.
- **`57.7 MB` → `13.6 MB`**: Active RSS measured on an Orange Pi 5 Plus (RK3588, `aarch64-unknown-linux-musl`) with 7 active cameras before and after replacing `webrtc-rs` with `rustrtc` in v0.5.0.
- **`20 – 50 MB`**: Typical active RSS range when one or more WebRTC viewers are actively connected and running the in-process `symphonia` AAC decoder + `opus-pure` 48 kHz stereo Opus encoder.
- **`~187 MB` / `~200 MB` recording savings**: Total RAM saved on a 7-camera Frigate NVR host when replacing 7 external `ffmpeg -c copy -f segment` processes (~28.5 MB each ≈ 200 MB total) with 7 lightweight `rust2rtc` shim processes (~1.8 MB each ≈ 13.2 MB total, net savings ~187 MB).

---

## 2. Running the Memory & CPU Benchmark (`measure_rss.sh`)

You can measure idle and active RSS on your own machine (macOS or Linux) using [`bench/measure_rss.sh`](measure_rss.sh):

```bash
# Measure rust2rtc alone:
./bench/measure_rss.sh ./target/release/rust2rtc

# Compare rust2rtc and go2rtc side-by-side:
GO2RTC_BIN=/path/to/go2rtc ./bench/measure_rss.sh ./target/release/rust2rtc
```

The script:
1. Starts the binary with a temporary test configuration.
2. Samples resident set size (`ps -o rss=`) and CPU (`ps -o %cpu=`) at idle.
3. Queries `/api` and `/api/streams` to verify responsiveness.
4. Prints a formatted summary table.

---

## 3. Running the `go2rtc` Drop-In API Compatibility Suite (`compat_test.sh`)

To verify that `rust2rtc` honors the `go2rtc` HTTP REST API contract used by Home Assistant, Frigate NVR, and `advanced-camera-card`, run [`bench/compat_test.sh`](compat_test.sh):

```bash
# Test a running rust2rtc instance (or auto-spawn one):
./bench/compat_test.sh

# Compare against both rust2rtc and a real go2rtc binary side-by-side:
GO2RTC_URL=http://127.0.0.1:1985 ./bench/compat_test.sh http://127.0.0.1:1984
```

Additionally, `cargo test` includes an automated in-process contract test (`api::tests::test_go2rtc_api_contract_and_optional_auth`) that verifies:
- `GET /api` version and revision schema
- `GET /api/streams` producer/consumer JSON schema
- `PUT /api/streams` dynamic stream registration
- `DELETE /api/streams` stream removal
- Optional HTTP Basic Authentication (`401 Unauthorized` when unauthenticated, `200 OK` with valid credentials)
