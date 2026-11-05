# 🦀 rust2rtc

> **High-performance, drop-in [go2rtc](https://github.com/AlexxIT/go2rtc) media streaming gateway in Rust — zero-FFmpeg for the common path, with built-in Frigate segment recording and a bidirectional SIP intercom bridge.**

`rust2rtc` is a memory-safe, lightweight streaming gateway built in Rust using [Tokio](https://tokio.rs/), [Axum](https://github.com/tokio-rs/axum), and [`rustrtc`](https://github.com/restsend/rustrtc).

It serves as a **drop-in binary replacement for `go2rtc`** across the smart home ecosystem — working out of the box with **[Frigate NVR](https://frigate.video/)**, **[Advanced Camera Card](https://card.camera/)**, **Home Assistant**, and WebRTC client libraries like `video-rtc.js`.

In addition, `rust2rtc` functions as a **bidirectional SIP bridge**, bridging standard RTSP IP cameras (Reolink, Tapo, TP-Link, Yi) to SIP endpoints so you can **see live video and talk directly with your cameras** from VoIP phones, SIP softphones, and hardware intercom screens like **[ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/)**.

> [!NOTE]
> **AI-Assisted Origin & Production Status**: `rust2rtc` was developed as an AI-assisted Rust re-implementation of `go2rtc`'s core workflows, iteratively tested and hardened against real cameras and 24/7 NVR hardware (Orange Pi 5 Plus RK3588 running Frigate + Home Assistant). See **[What's Tested vs. What's Rough](#-whats-tested-vs-whats-rough)** below for an honest breakdown of hardware validation and current limitations.

---

## 📚 Documentation

Detailed setup and reference guides are organized in [`docs/`](docs/) and [`bench/`](bench/):
- **[Frigate NVR Setup & Zero-FFmpeg Recording Shim](docs/frigate-setup.md)**
- **[Bidirectional SIP Intercom & Doorbell Guide](docs/sip-intercom.md)**
- **[Configuration & Security Reference](docs/configuration.md)**
- **[Benchmarks & API Compatibility Test Suite](bench/README.md)**

---

## 🌟 Key Features

### Zero-FFmpeg for the Common Path
For the primary 24/7 NVR and live-viewing workflows, `rust2rtc` operates **100% in-process in pure Rust without spawning FFmpeg**:
- **📹 Native In-Process RTSP Client**: Connects directly to IP cameras over TCP (`OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN`, `GET_PARAMETER`), handles Digest/Basic authentication natively, and depacketizes interleaved RTP frames directly in-process (no `ffmpeg -c copy` ingestion subprocesses).
- **🔊 Pure-Rust In-Process Audio Transcoding**: Decodes camera AAC-hbr (via `symphonia`) and G.711 PCMA/PCMU in-process and encodes 48 kHz stereo Opus (via `opus-pure`) on a single shared broadcast track per camera, synchronized to the camera's RTP clock.
- **🌐 Pure-Rust WebRTC Streaming (`rustrtc`)**: Supports WHEP (`POST /api/webrtc`) and `go2rtc` WebSocket signaling (`/api/ws?src=<stream>`) with H.264, H.265/HEVC, zero-copy PCMA/PCMU/Opus passthrough, and **full-duplex two-way microphone talkback** (`OpusToG711Decoder` → RTSP/ONVIF/Tapo backchannel).
- **🎞️ Pure-Rust Fragmented MP4 (fMP4 / MSE)**: Native ISO BMFF muxer generates `ftyp`+`moov` init segments and `moof`+`mdat` fragments in-process for instant preview in `video-rtc.js` and Advanced Camera Card.
- **💾 Pure-Rust MP4 Segment Recorder for Frigate Cache**: Cuts standalone progressive `.mp4` segments (`avc1` H.264, `hvc1` H.265, `mp4a` AAC) on IDR keyframes directly into `/tmp/cache/`. Includes a lightweight `ffmpeg-shim` (`~1.5–1.9 MB` RSS) so unpatched Frigate can record without spawning `~25–38 MB` FFmpeg recording processes per camera.

### When External FFmpeg Is Used (On-Demand Only)
`ffmpeg` is **not required to compile** `rust2rtc` and is **never spawned at idle or during normal RTSP/WebRTC/MSE/recording playback**. An external `ffmpeg` binary in `PATH` is only invoked on-demand for:
- **JPEG Snapshots** (`/api/frame.jpeg`) — single-frame keyframe extraction (`-vframes 1`).
- **HTTP Motion JPEG** (`/api/stream.mjpeg`) & **SIP RFC 2435 JPEG RTP** — when transcoding H.264/H.265 video to MJPEG for legacy browsers or ESP32 smart panels (deduplicated across viewers).
- **Custom `ffmpeg:` Sources & Hardware Transcoding** — explicit `ffmpeg:...` stream definitions or Frigate non-recording detection/hardware-decode jobs (`rkmpp`, `vaapi`, `cuda`) passed through the shim.

### Bidirectional SIP Bridge & Intercom Gateway
- **Dialable Camera Extensions**: Auto-registers camera streams as SIP extensions with Digest MD5 authentication (`qop=auth`), symmetric RTP, and automatic Home Assistant `voip_stack` provisioning.
- **Parallel Forking Ring Groups**: Subscribes to Home Assistant doorbell button events over WebSocket and rings multiple SIP screens simultaneously (first to answer wins; remaining legs receive `CANCEL`).
- **Video & Two-Way Audio over SIP**: Streams H.264 (RFC 6184) or hardware-friendly MJPEG (RFC 2435 PT 26 at 15 fps) alongside bidirectional G.711a/u or 16 kHz L16 audio.

---

## ⚡ Resource Efficiency & Reproducible Benchmarks

All figures below were measured on an **Orange Pi 5 Plus (Rockchip RK3588, `aarch64-unknown-linux-musl`)** and **Apple Silicon M-series (`aarch64-apple-darwin`)** with 7 cameras (14 RTSP streams: 7 main + 7 sub).

See **[`bench/README.md`](bench/README.md)** for full methodology and run `./bench/measure_rss.sh` or `./bench/compat_test.sh` to reproduce on your own system.

| Workload Profile | `go2rtc` (v1.9.14) | `rust2rtc` (v0.5.0) | Notes |
| :--- | :---: | :---: | :--- |
| **1. Cold / Idle RSS** (0 active streams) | ~15 – 25 MB | **~4.5 MB** | Gateway process RSS at startup with 14 configured streams before any client connects. |
| **2. Active Gateway RSS** (7 cameras ingested) | ~35 – 65 MB *(+ external FFmpeg if transcoding audio)* | **~13.6 MB** *(Linux ARM64 `musl`)*<br>**~20 – 35 MB** *(with active WebRTC Opus transcode)* | In v0.5.0, replacing `webrtc-rs` (57.7 MB) with pure-Rust `rustrtc` reduced 7-camera active RSS on Linux ARM64 to **13.6 MB**. |
| **3. Frigate 7-Camera Segment Recording** | ~180 – 250 MB *(7 × ~25–38 MB `ffmpeg -f segment` procs)* | **~13.2 MB total** *(7 × ~1.8 MB `ffmpeg-shim` procs)* | Muxing runs in-process inside `rust2rtc`; saves **~170–200 MB RAM (~93% reduction)** across 7 recorded cameras. |
| **WebRTC Audio Transcoding** | Spawns `ffmpeg` child process per stream | **0 subprocesses** (in-process `symphonia` + `opus-pure`) | Shared broadcast Opus track across all concurrent viewers. |

---

## 🔍 What's Tested vs. What's Rough

Because camera gateways run 24/7, here is an upfront summary of what has been tested on real hardware versus what is still experimental or unimplemented:

### ✅ Tested 24/7 on Real Hardware
- **Host Platforms**: Rockchip RK3588 (Orange Pi 5 Plus, Debian/Armbian ARM64 in Docker with Frigate) and Apple Silicon macOS (M1–M4).
- **Cameras**:
  - **Reolink Video Doorbell (PoE & WiFi)**: 5MP H.264 High Profile Level 5.1, in-process AAC → Opus audio, ONVIF two-way backchannel talkback over WebRTC and SIP.
  - **TP-Link Tapo (C110, C200, C210, C310)**: RTSP TCP ingest, Digest auth, and Tapo HTTP `streamd` (`:8800`) two-way audio talkback.
  - **TP-Link AIPC / Chinese Regional PTZ**: Dual-lens H.264/H.265 RTSP TCP ingest, `multitrans://` backchannel, interleaved `$` stream resynchronization.
  - **Yi / Kami (`yi-hack-allwinner-v2` / `v4` / `v5`)**: RTSP TCP ingest and `rRTSPServer` backchannel.
- **Intercom Panels**: Waveshare ESP32-P4 Touch & Guition ESP32-P4 running [ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/) (RFC 2435 JPEG RTP video + L16/G.711 audio), FreePBX/Asterisk, and Home Assistant VoIP.

### ⚠️ Known Limitations & Rough Edges
- **TCP-Only RTSP Transport**: Both the RTSP client and RTSP server (`:8554`) currently implement TCP interleaved transport only (`RTP/AVP/TCP`). UDP unicast/multicast RTSP requests return `461 Unsupported Transport` so clients automatically fall back to TCP.
- **No Built-in TLS / HTTPS**: Like `go2rtc`, `rust2rtc` expects to run on a trusted LAN or behind a reverse proxy (Caddy, Nginx, Traefik) for TLS termination.
- **Authentication Defaults**: By default, the HTTP API (`:1984`) and RTSP server (`:8554`) run unauthenticated for zero-config Frigate/Home Assistant drop-in compatibility. Optional HTTP Basic Auth (`api.username` / `api.password`) is supported and recommended if port `:1984` is exposed outside a container network.
- **Subset of `go2rtc` Sources**: `rust2rtc` focuses on RTSP, ONVIF, Tapo, `ffmpeg:`, `exec:`, MSE/fMP4, WebRTC, and SIP. It does not yet implement HomeKit, RTMP, WebTorrent, or USB V4L2 device capture.

---

## 📋 Feature & Roadmap Matrix

| Feature | `go2rtc` | `rust2rtc` | Status |
| :--- | :---: | :---: | :---: |
| **RTSP Server & Client (TCP Interleaved)** | ✅ | ✅ | Working |
| **WebRTC Gateway (WHEP & `/api/ws`)** | ✅ | ✅ | Working (`rustrtc`, H.264 & H.265) |
| **go2rtc Streams API (`/api/streams`)** | ✅ | ✅ | Working (GET/PUT/POST/DELETE, contract-tested) |
| **Pure-Rust In-Process Opus Transcoding** | ❌ (FFmpeg) | ✅ (Native) | Working (AAC/G.711 ↔ Opus, zero-FFmpeg) |
| **Pure-Rust MSE / fMP4 Streaming** | ✅ | ✅ | Working (In-process ISO BMFF muxer) |
| **In-Process Frigate MP4 Segment Recorder** | ❌ | ✅ | Working (`ffmpeg-shim` + `/tmp/cache` muxer) |
| **JPEG Snapshots & HTTP MJPEG** | ✅ | ✅ | Working (Uses on-demand `ffmpeg`) |
| **SIP Gateway & Intercom (H.264 & RFC 2435 JPEG)**| ⚠️ | ✅ | Working (Parallel ring groups, Digest auth) |
| **Two-Way Audio (Backchannel)** | ✅ | ✅ | Working (ONVIF, Reolink, Tapo, `multitrans://`) |
| **Optional HTTP Basic Auth (`:1984`)** | ✅ | ✅ | Working (`api.username` / `api.password`) |
| **RTSP UDP / Multicast Transport** | ✅ | 🚧 | Planned (`good first issue`) |
| **Native ONVIF WS-Discovery** | ✅ | ⏳ | Planned (`good first issue`) |
| **HomeKit Camera Integration** | ✅ | ⏳ | Planned |

---

## 🚀 Building & Running

### Prerequisites
- **Rust** (stable toolchain 1.85+ supporting Edition 2024)
- **FFmpeg** *(Optional at runtime; not needed to compile)* — only required if you use `/api/frame.jpeg` snapshots, `/api/stream.mjpeg`, RFC 2435 SIP JPEG transcoding, or custom `ffmpeg:` sources.

### Build from Source

```bash
git clone https://github.com/kiddyfurby/rust2rtc.git
cd rust2rtc

# Build native release binary
cargo build --release   # or: just mac

# Run unit, parser fuzz/robustness, and API contract tests
cargo test

# Cross-compile statically linked musl binaries for Linux (requires cargo-zigbuild):
just linux-amd64        # x86_64-unknown-linux-musl
just linux-arm64        # aarch64-unknown-linux-musl
```

> [!TIP]
> **Pre-Compiled Binaries**: Automated CI builds for `rust2rtc_linux_arm64`, `rust2rtc_linux_amd64`, and `rust2rtc_mac_arm64` are configured in [`.github/workflows/release.yml`](.github/workflows/release.yml) and published to the [**GitHub Releases**](https://github.com/kiddyfurby/rust2rtc/releases) page when version tags (`v*`) are pushed. If no release binary is published for your target yet, build from source with `cargo build --release`.

---

## ⚙️ Quick Start & Configuration

1. Copy the sample configuration and edit it with your camera URLs (note: `rust2rtc.yaml` is gitignored so your credentials are never committed):
   ```bash
   cp rust2rtc.sample.yaml rust2rtc.yaml
   ```
2. Start `rust2rtc`:
   ```bash
   ./target/release/rust2rtc
   ```
3. Open the Web Dashboard at **[http://localhost:1984](http://localhost:1984)**.

### Key Endpoints
- **Web Dashboard**: `http://localhost:1984/`
- **WebSocket Signaling**: `ws://localhost:1984/api/ws?src=<stream>`
- **WebRTC WHEP**: `POST http://localhost:1984/api/webrtc?src=<stream>`
- **Snapshot API**: `GET http://localhost:1984/api/frame.jpeg?src=<stream>`
- **MJPEG Stream**: `GET http://localhost:1984/api/stream.mjpeg?src=<stream>`
- **Streams REST API**: `GET / PUT / POST / DELETE http://localhost:1984/api/streams`
- **RTSP TCP Relay**: `rtsp://localhost:8554/<stream>`

For Frigate NVR integration, `ffmpeg-shim` setup, SIP intercom configuration, and security settings, see:
- **[`docs/frigate-setup.md`](docs/frigate-setup.md)**
- **[`docs/sip-intercom.md`](docs/sip-intercom.md)**
- **[`docs/configuration.md`](docs/configuration.md)**

---

## 🤝 Contributing & Good First Issues

Contributions, bug reports, and pull requests are warmly welcomed! If you're looking for a self-contained place to start:
- **🟢 Good First Issue — RTSP UDP Transport**: Add UDP unicast RTP/RTCP socket pairs in `src/rtsp/mod.rs` and `src/rtsp/client.rs` alongside the existing interleaved TCP handler.
- **🟢 Good First Issue — Native ONVIF Discovery & `GetStreamUri`**: Implement WS-Discovery UDP multicast probe and SOAP `GetStreamUri` lookup so `onvif://` URLs with non-standard paths resolve automatically without custom path hints.
- **🟢 Good First Issue — `cargo-fuzz` Targets**: Expand the malformed-input parser tests in `src/core/mod.rs`, `src/rtsp/mod.rs`, and `src/sip/message.rs` into standalone `cargo-fuzz` / `libfuzzer` targets.
- **🟢 Good First Issue — Prometheus Metrics (`/api/metrics`)**: Expose stream packet/byte counters and active consumer counts in Prometheus text format.

---

## 📝 Changelog Summary

- **v0.5.0**: Migrated exclusively to the pure-Rust `rustrtc` WebRTC engine (cutting active 7-camera Linux ARM64 RSS from 57.7 MB to 13.6 MB); added full-duplex WebRTC two-way talkback (`OpusToG711Decoder`), in-process Frigate MP4 segment recording (`ffmpeg-shim`), on-demand HTTP MJPEG streaming with broadcaster deduplication, Home Assistant WebSocket doorbell subscriber with parallel SIP ring groups, call duration watchdog + `/api/sip/hangup`, and optional HTTP Basic Auth.
- **v0.4.0**: RTSP interleaved `$` stream resynchronization, dynamic H.264 `profile-level-id` extraction for 5MP/4K cameras, and H.264 High Profile Level 5.1 WebRTC negotiation.
- **v0.3.0**: Pure-Rust in-process TCP RTSP client and Home Assistant `go2rtc` API compatibility (`/api`, `/api/schemes`, `/api/preload`).
- **v0.2.0**: Pure-Rust in-process AAC/G.711 → Opus audio transcoding (`symphonia` + `opus-pure`) and bidirectional SIP intercom bridge with RFC 2435 JPEG RTP.
- **v0.1.0**: Initial Tokio/Axum core, WebRTC WHEP gateway, RTSP TCP server (`:8554`), and JPEG snapshots.

---

## 📜 License & Acknowledgments

- Inspired by the work of [AlexxIT](https://github.com/AlexxIT) and the [`go2rtc`](https://github.com/AlexxIT/go2rtc) project.
- Licensed under the [MIT License](LICENSE).
