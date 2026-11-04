# 🦀 rust2rtc

> **High-performance, drop-in [go2rtc](https://github.com/AlexxIT/go2rtc) media streaming gateway in pure Rust.**

`rust2rtc` is a modern, memory-safe, ultra-lightweight streaming gateway built from the ground up in Rust using [Tokio](https://tokio.rs/), [Axum](https://github.com/tokio-rs/axum), and [webrtc-rs](https://github.com/webrtc-rs/webrtc).

It serves as a **drop-in binary replacement for `go2rtc`** across the smart home ecosystem — working out of the box with **[Frigate NVR](https://frigate.video/)**, **[Advanced Camera Card](https://card.camera/)**, **Home Assistant**, and WebRTC client libraries like `video-rtc.js`.

In addition, `rust2rtc` functions as a **high-performance bidirectional SIP bridge**, bridging standard RTSP Wi-Fi / IP cameras (such as Reolink, Tapo, TP-Link, Yi) to SIP endpoints. This allows you to **see live video and talk directly with your cameras** from VoIP phones, SIP softphones, smart panels, and hardware intercom devices like **[ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/)**.

> [!NOTE]
> **Project Notice**: `rust2rtc` is an active, high-performance rewrite of `go2rtc`. The core RTSP relay, WebRTC (WHEP + WebSocket), in-process AAC/G.711 to Opus transcoding, native fMP4 (MSE), SIP intercom/backchannel, and snapshot pipelines are fully working and verified in production environments. Contributions, issues, and PRs are warmly welcomed!

---

## 🌟 Key Features

- **⚡ Zero-Cost Asynchronous Architecture**: Built on Tokio with multi-producer, multi-consumer lock-free broadcast pipelines.
- **📹 Native In-Process RTSP Client (Zero-FFmpeg Camera Ingestion)**:
  - Connects directly to IP cameras and NVRs over TCP in pure Rust.
  - Full support for standard RTSP handshakes (`OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN`, `GET_PARAMETER` keepalive).
  - Handles Digest and Basic authentication natively with zero external dependencies.
  - Ingests and depacketizes interleaved binary RTP frames directly into stream tracks in-process, completely eliminating the need for `ffmpeg -c copy` ingestion subprocesses.
- **🔊 Pure-Rust In-Process Audio Transcoder (Zero-FFmpeg WebRTC Audio)**:
  - **In-process decoding**: Native decoding of AAC-hbr (RFC 3640 via `symphonia`) and G.711 $\mu$-law/A-law without spawning FFmpeg child subprocesses.
  - **In-process Opus encoding**: High-fidelity 48kHz stereo encoding via `opus-pure`.
  - **Deduplicated shared broadcast tracks**: Multiple concurrent WebRTC viewers share a single background transcoder worker per stream, slashing CPU and memory.
  - **Camera-clock A/V lip-sync**: Outgoing Opus RTP timestamps synchronize directly with the camera's reference clock, eliminating browser NetEq jitter-buffer stalls and `media_unavailable` timeouts in Chromium, Safari, and Firefox.
- **🎞️ Pure-Rust Fragmented MP4 (fMP4) Muxer for MSE**:
  - Native ISO BMFF engine generates `ftyp` + `moov` init headers and `moof` + `mdat` video fragments in-process.
  - Fully supports `video-rtc.js` dual-mode negotiation (MSE instant preview + WebRTC swap) used by **Advanced Camera Card**.
- **💾 In-Process MP4 Segment Recorder for Frigate Cache (Zero-FFmpeg Recording)**:
  - Generates standalone progressive ISO BMFF `.mp4` segment files directly in `/tmp/cache/` in pure Rust.
  - Automatically cuts segments on IDR keyframes matching Frigate's naming convention (`<stream>@%Y%m%d%H%M%S%z.mp4`) with atomic temporary file renaming.
  - Native multiplexing for both **H.264** (`avc1`) and **H.265 / HEVC** (`hvc1`), with AAC audio track multiplexing (`mp4a` + `esds`).
  - Built-in transparent `ffmpeg-shim` stands in for Frigate's recording subprocesses, slashing recording subprocess RSS from ~25–38 MB down to **~1.5–2.0 MB per camera (-93% memory savings)** with 0% CPU overhead. Frigate's `RecordingMaintainer` indexes and archives segments seamlessly to disk/NFS.
- **🌐 Real-Time WebRTC Streaming & Dual-Engine Selection**:
  - **Dual WebRTC Engine**: Runtime selection between the lightweight, pure-Rust **`rustrtc`** engine (reduces process RSS down to **~14 MB**, -76% memory footprint) and the standard **`webrtc-rs`** engine via `webrtc.engine: "rustrtc"` in `rust2rtc.yaml`.
  - **🎙️ Full-Duplex WebRTC Two-Way Audio (Talkback)**: Built-in pure-Rust **`OpusToG711Decoder`** captures incoming microphone Opus audio from browsers, Frigate WebUI, and Home Assistant cards (`a=sendrecv`), decodes directly to 8kHz PCM, and relays it into the camera's RTSP backchannel speaker in real time.
  - **Low-Latency Browser Playback**: Standard WHEP (`/api/webrtc`) and native **`go2rtc` WebSocket Signaling (`/api/ws?src=<stream>`)** supporting `webrtc/offer`, `webrtc/answer`, and trickle ICE for **`advanced-camera-card`**, **Frigate**, and Home Assistant.
  - **Codec Support**: Automatic negotiation supporting **H.264** (Baseline, Constrained Baseline, High Profile up to Level 5.1) and **H.265 / HEVC** (in supported browsers like Safari).
  - **Audio Passthrough & Transcoding**: In-process Opus, PCMA (G.711a), and PCMU (G.711u).
  - In-band parameter sets (SPS / PPS / VPS) caching and IDR keyframe gating for instant decode.
- **🔄 go2rtc-Compatible REST API**:
  - `GET /api/streams` returning standard go2rtc `{ "stream": { "producers": [...], "consumers": [...] } }` schema.
  - Dynamic stream management via `PUT / POST / DELETE /api/streams` for Home Assistant and Frigate automated provisioning.
- **📹 RTSP Gateway**:
  - Built-in RTSP server on port `:8554` supporting TCP interleaved media transport.
  - Native RTSP relay from IP cameras, NVRs, and video encoders.
- **🎞️ External FFmpeg Integration (When needed)**:
  - Remuxes and transcodes on-demand without overhead when idle.
  - Custom pipeline syntax compatible with go2rtc (e.g. `ffmpeg:stream#video=mjpeg#raw=-r 15 -q:v 3`).
- **📸 High-Speed Snapshots**:
  - Instant JPEG frame capture via `/api/frame.jpeg?src=<stream>` for both H.264 and H.265 (HEVC) streams.
  - Optimized pipeline with low-delay decoding and track readiness polling.
- **📞 Bidirectional SIP Bridge & Intercom Gateway**:
  - **Bridge RTSP Cameras to SIP**: Exposes ordinary RTSP Wi-Fi / IP cameras as interactive SIP intercom extensions that can be dialed, viewed, and spoken to from VoIP phones or smart screens.
  - **Hardware Intercom Support**: Seamlessly stream live video and two-way audio to [**ESPHome Intercom**](https://github.com/n-IA-hane/esphome-intercom/), Grandstream, Akuvox, and low-power ESP32 panels.
  - Asynchronous SIP protocol engine supporting standard Digest MD5 authentication, auto-registration, Keep-Alive `OPTIONS` pings, and graceful unregistration (`REGISTER Expires: 0`) on shutdown.
  - Inbound call handling with symmetric RTP and Call-ID response routing.
  - Auto-configuration of Home Assistant SIP integration via REST API with dynamic account provisioning.
- **🔊 Two-Way Audio (Backchannel)**:
  - Full-duplex backchannel for Reolink and ONVIF-compatible smart doorbells and intercom cameras (`backchannel=1`).
  - High-fidelity PCM / G.711a (PCMA) / G.711u (PCMU) transcoding and repacking into standard 20ms audio frames (160 samples @ 8kHz).
- **📺 Video over SIP (H.264 & RFC 2435 JPEG RTP)**:
  - **H.264 Video Relay**: SSRC normalization, dynamic payload type rewriting, and SPS/PPS parameter set injection for SIP video phones.
  - **RFC 2435 JPEG RTP Streaming**: Hardware-friendly MJPEG RTP streaming for low-power smart panels and intercom screens (e.g. Waveshare ESP32-P4 Touch panels running [**ESPHome Intercom**](https://github.com/n-IA-hane/esphome-intercom/), Grandstream, Akuvox).
  - Real-time 15 fps frame pacing and backlog-dropping pipeline to ensure zero lag and prevent socket buffer bloat.
- **🖥️ Built-in Web Dashboard**:
  - Web UI on port `:1984` featuring auto-refreshing snapshot tiles, stream statistics (packets, bytes, active consumers), and instant WebRTC live preview.

---

## ⚡ Resource Efficiency (RAM & CPU Savings)

Rewriting in Rust eliminates garbage collection runtime overhead, goroutine stack allocations, and child subprocess spawning.

| Metric | `go2rtc` (Go Runtime) | `rust2rtc` (Native Rust) | Improvement |
| :--- | :---: | :---: | :---: |
| **Idle Memory (RSS)** | ~40 MB – 80 MB | **~4.5 MB** | **~85% – 90% less RAM** |
| **Frigate Segment Recording** | External FFmpeg per camera (~25–38 MB each) | **In-Process MP4 Builder + 1.5 MB Shim** | **-93% RAM (~187 MB saved across 7 cameras)** |
| **Idle CPU Usage** | ~0.5% – 2.0% (GC / scheduler) | **0.0%** | **Near-zero idle CPU** |
| **Garbage Collection Pauses** | Periodic GC cycles | **None (Zero GC)** | Deterministic latency |
| **WebRTC Audio Transcoding** | Spawns FFmpeg subprocess + UDP sockets | **Zero subprocesses (Pure Rust in-process)** | **0 extra OS processes / ports** |
| **Transcoder Deduplication** | Independent transcode per viewer | **Shared broadcast Opus track** | **$N \times$ CPU savings across viewers** |
| **Memory Management** | Go GC heap & runtime allocator | Manual / RAII stack + zero-copy `Bytes` | Flat, non-fragmenting footprint |
| **On-Demand Processing** | Persistent or background remux | Workers spawned strictly on-demand | 0 MB / 0% CPU for idle streams |

*Measured on macOS (Darwin arm64) and Linux (aarch64) with 14 active camera configurations loaded in `rust2rtc.yaml`.*

---

## 🧪 Tested Environment & Implemented Features

The following features have been implemented and verified in live testing:

| Feature | Protocol / Codec | Server Environment | Client / Browser | Test Result & Notes |
| :--- | :--- | :--- | :--- | :--- |
| **H.264 WebRTC Live** | H.264 (Baseline, High Profile) | macOS / Linux (ARM64/x86_64) | Chrome / Safari / Edge | ✅ **Verified** – Low latency, instant playout clock & rendering |
| **H.265 WebRTC Live** | H.265 (HEVC) | macOS / Linux (ARM64/x86_64) | Safari / HEVC-enabled Chrome | ✅ **Verified** – Negotiated via WebRTC when browser supports HEVC |
| **Pure-Rust Opus Transcoding** | AAC / G.711a / G.711u -> Opus | macOS / Linux (ARM64/x86_64) | Chrome / Safari / Firefox | ✅ **Verified** – 100% in-process transcoding with camera-clock A/V sync |
| **Pure-Rust MSE / fMP4** | ISO BMFF fMP4 (`video/mp4`) | macOS / Linux (ARM64/x86_64) | `video-rtc.js` / ACC | ✅ **Verified** – Instant fMP4 preview fallback before WebRTC swap |
| **H.264 Snapshots** | H.264 (`/api/frame.jpeg`) | macOS / Linux (ARM64/x86_64) | HTTP Client / Browser | ✅ **Verified** – Low-delay instant capture (`-flags low_delay`) |
| **H.265 Snapshots** | H.265 (`/api/frame.jpeg`) | macOS / Linux (ARM64/x86_64) | HTTP Client / Browser | ✅ **Verified** – Keyframe extraction without grey artifacts (`-skip_frame nokey`) |
| **RTSP TCP Relay** | RTSP Interleaved TCP (`:8554`) | macOS / Linux (ARM64/x86_64) | FFmpeg / RTSP Clients | ✅ **Verified** – Interleaved channel demuxing & broadcast |
| **Multi-Source (`multitrans`)**| `multitrans://` & fallback lists | macOS / Linux (ARM64/x86_64) | Internal Engine | ✅ **Verified** – Source fallback and candidate selection |
| **SIP Audio & Backchannel** | SIP / G.711a / G.711u | macOS / Linux (ARM64/x86_64) | Home Assistant / FreePBX | ✅ **Verified** – Symmetric RTP, 20ms audio repacking & Reolink backchannel |
| **SIP Video (H.264)** | SIP / RFC 6184 H.264 RTP | macOS / Linux (ARM64/x86_64) | SIP Video Phone / Softphone | ✅ **Verified** – Live video forwarder with dynamic PT & SSRC normalization |
| **SIP Video (RFC 2435 JPEG)**| SIP / RFC 2435 JPEG RTP | macOS / Linux (ARM64/x86_64) | Waveshare ESP32-P4 Touch Panel | ✅ **Verified** – Hardware-decoded MJPEG with 15fps pacing & backlog drop |
| **SIP Registration & Exit** | SIP Digest MD5 | macOS / Linux (ARM64/x86_64) | Asterisk / FreePBX | ✅ **Verified** – Auto-registration, keep-alive OPTIONS, graceful unregister |
| **Web Dashboard** | HTTP / HTML5 / JS (`:1984`) | macOS / Linux (ARM64/x86_64) | Chrome / Safari / Firefox | ✅ **Verified** – Live stream cards, bitrates, snapshot previews |

---

## 📋 General Status & Roadmap

| Feature | `go2rtc` | `rust2rtc` | Status |
| :--- | :---: | :---: | :---: |
| **RTSP Server & Client (TCP)** | ✅ | ✅ | Working |
| **WebRTC Gateway (WHEP)** | ✅ | ✅ | Working (H.264 & H.265) |
| **go2rtc WS Signaling (`/api/ws`)** | ✅ | ✅ | Working (`advanced-camera-card` / Frigate) |
| **go2rtc Streams API (`/api/streams`)** | ✅ | ✅ | Working (GET/PUT/POST/DELETE) |
| **Pure-Rust In-Process Opus Transcoding**| ❌ (FFmpeg) | ✅ (Native) | Working (AAC/G.711 -> Opus, zero-FFmpeg) |
| **MSE / fMP4 Streaming** | ✅ | ✅ | Working (Pure-Rust ISO BMFF muxer) |
| **External FFmpeg Remux / Transcode** | ✅ | ✅ | Working (On-demand fallback) |
| **JPEG Snapshots API** | ✅ | ✅ | Working (H.264 & H.265) |
| **SIP Gateway & Intercom** | ⚠️ | ✅ | Working (Audio, H.264 & RFC 2435 JPEG) |
| **Two-Way Audio (Backchannel)** | ✅ | ✅ | Working (Reolink & SIP G.711) |
| **Web UI Dashboard** | ✅ | ✅ | Working |
| **RTSP UDP / Multicast** | ✅ | 🚧 | Planned |
| **HomeKit Integration** | ✅ | ⏳ | Planned |
| **Native ONVIF Discovery** | ✅ | ⏳ | Planned |

---

## 📥 Downloads & Pre-Compiled Binaries

Pre-compiled, standalone, statically linked binaries are provided for all 3 supported production platforms on the [**GitHub Releases**](https://github.com/kiddyfurby/rust2rtc/releases/latest) page:

| Platform / Architecture | Recommended Hardware | Standalone Binary | Archive |
| :--- | :--- | :---: | :---: |
| **Linux ARM64** (`aarch64`) | Rockchip RK3588 (Orange Pi 5 Plus), Raspberry Pi 4 / 5, Home Assistant OS | [**`rust2rtc_linux_arm64`**](https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_linux_arm64) | — |
| **Linux x86_64** (`amd64`) | Intel & AMD 64-bit servers, mini PCs, Frigate Docker containers | [**`rust2rtc_linux_amd64`**](https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_linux_amd64) | — |
| **macOS Apple Silicon** (`arm64`) | Apple M1 / M2 / M3 / M4 MacBook, Mac mini, Mac Studio (macOS 12+) | [**`rust2rtc_mac_arm64`**](https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_mac_arm64) | [`.zip`](https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_mac_arm64.zip) |

> [!TIP]
> **Zero Dependencies**: Linux binaries are fully statically linked against `musl` (`*-unknown-linux-musl`), requiring zero system dependencies or shared C libraries.

### Quick One-Liner Download

#### Linux ARM64 (RK3588 / Orange Pi 5 Plus / Raspberry Pi)
```bash
curl -L -o rust2rtc https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_linux_arm64
chmod +x rust2rtc
./rust2rtc
```

#### Linux x86_64 (Intel / AMD Server)
```bash
curl -L -o rust2rtc https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_linux_amd64
chmod +x rust2rtc
./rust2rtc
```

#### macOS Apple Silicon (MacBook / Mac mini)
```bash
curl -L -o rust2rtc https://github.com/kiddyfurby/rust2rtc/releases/latest/download/rust2rtc_mac_arm64
chmod +x rust2rtc
./rust2rtc
```

---

## 🚀 Building from Source

### Prerequisites

- **Rust** (stable toolchain 1.80+ or nightly)
- **FFmpeg** installed and accessible in your system `PATH`

### Installation & Build

```bash
# Clone the repository
git clone https://github.com/kiddyfurby/rust2rtc.git
cd rust2rtc

# Build macOS release binary
just mac          # or: cargo build --release

# Cross-compile for Linux (statically-linked musl ELF binaries):
just linux-amd64  # for x86_64 Linux servers / NVRs
just linux-arm64  # for Raspberry Pi / ARM64 Home Assistant OS

# Build and package all targets into ./dist/:
just dist
```

---

## ⚙️ Configuration

`rust2rtc` reads configuration from `rust2rtc.yaml` in the current working directory. A fully documented reference template is included in the project root at [`rust2rtc.sample.yaml`](rust2rtc.sample.yaml).

### Configuration Sections Walkthrough

A minimal or full configuration can include any of the following sections:

```yaml
# 1. Camera Streams & Ingestion (Optional / Handled by Frigate)
# NOTE: When running alongside Frigate NVR, Frigate automatically generates all camera
# stream definitions from its config.yml. You can completely omit this section.
# For standalone deployments, define your camera RTSP feeds below:
# streams:
#   doorbell:
#     - "rtsp://admin:password@198.51.100.50:554/h264Preview_01_main#backchannel=1"
#     - "ffmpeg:doorbell#audio=opus"

# 2. WebRTC ICE & STUN
webrtc:
  candidates:
    - "198.51.100.4:8555"
    - "stun:stun.l.google.com:19302"

# 3. Ports & Network Services
api:
  listen: ":1984" # Web dashboard, REST API & WebSocket signaling

rtsp:
  listen: ":8554" # Embedded RTSP server for Frigate, Home Assistant, VLC

# 4. FFmpeg & Hardware Acceleration
ffmpeg:
  bin: "ffmpeg"
  # Hardware profile: "rkmpp" (Rockchip RK3588), "vaapi" (Intel/AMD), "cuda" (Nvidia)
  hardware: "rkmpp"

# 5. In-Process MP4 Recording for Frigate (Optional)
# NOTE: In-process progressive MP4 segment recording is ENABLED BY DEFAULT for
# ALL streams when using the FFmpeg shim. You do NOT need to include this section
# unless you want to customize the cache directory or restrict recording:
# record:
#   enabled: true             # Default: true (set to false to disable)
#   dir: "/tmp/cache"         # Default: /tmp/cache (Frigate cache)
#   segment_duration: 10      # Default: 10 seconds per segment
#   # streams:                # Optional whitelist. If omitted, ALL streams are recorded!
#   #   - doorbell

# 6. Built-in SIP PBX / Intercom Server (Optional)
sip:
  server: "198.51.100.4:5060"
  domain: "198.51.100.4"
  ha_url: "http://198.51.100.4:8123"
  ha_token: "YOUR_HA_LONG_LIVED_ACCESS_TOKEN"
  auto_register_streams: true # Automatically assigns SIP extensions to camera streams
  base_extension: 101
  auto_password: "YourSecurePasswordHere"
  names:
    doorbell: "Front Door"
    driveway: "Driveway"

# 7. Automated Doorbell Calling (Optional)
doorbells:
  doorbell:
    entity: "binary_sensor.front_doorbell_visitor" # Home Assistant binary sensor
    sip_targets:
      - "198.51.100.100:5060" # Living Room ESPHome Intercom
      - "198.51.100.101:5060" # Dining Room ESPHome Intercom
    cooldown_seconds: 15
```

See [`rust2rtc.sample.yaml`](rust2rtc.sample.yaml) for full configuration comments and options.

---

## 🖥️ Usage

Run `rust2rtc`:

```bash
./target/release/rust2rtc
```

- **Web Dashboard**: Open [http://localhost:1984](http://localhost:1984) in your browser.
- **WebSocket Signaling**: `ws://localhost:1984/api/ws?src=<stream_name>` (WebRTC offer/answer/candidate)
- **Snapshot API**: `GET http://localhost:1984/api/frame.jpeg?src=<stream_name>`
- **WebRTC WHEP**: `POST http://localhost:1984/api/webrtc?src=<stream_name>` (Body: Offer SDP, returns Answer SDP)
- **Streams API**: `GET / PUT / DELETE http://localhost:1984/api/streams`
- **RTSP Relay**: `rtsp://localhost:8554/<stream_name>`

### 🏠 Drop-in `go2rtc` Replacement & Integrations

`rust2rtc` is built to be a direct, drop-in replacement for `go2rtc` across the smart home and NVR ecosystem.

#### 1. Frigate NVR
Frigate natively supports custom `go2rtc` binaries mounted in the container. Simply compile or copy `rust2rtc` into your Frigate config directory:

```bash
# Copy rust2rtc binary into Frigate's mounted config directory:
cp dist/rust2rtc-linux-arm64 /path/to/frigate/config/go2rtc
chmod +x /path/to/frigate/config/go2rtc

# Restart Frigate container
docker restart frigate
```

Frigate's s6-overlay runner detects `/config/go2rtc` and automatically runs it with multi-config loading (`-config=/config/go2rtc_homekit.yml -config=/dev/shm/go2rtc.yaml`), seamlessly handled by `rust2rtc`.

##### 💾 Zero-FFmpeg Recording Setup (93% RAM Reduction per Camera)
Frigate by default spawns an external FFmpeg instance for every camera assigned a `record` role, consuming ~25–38 MB RSS each. With `rust2rtc`, progressive MP4 segment recording runs directly in-process with zero extra processes.

To enable in-process recording for **all cameras** with **100% stock, unpatched Frigate**:
1. Create an FFmpeg shim directory inside `/config`:
   ```bash
   mkdir -p /path/to/frigate/config/ffmpeg/bin
   ln -sf ../../go2rtc /path/to/frigate/config/ffmpeg/bin/ffmpeg
   ln -sf /usr/lib/ffmpeg/6.0/bin/ffprobe /path/to/frigate/config/ffmpeg/bin/ffprobe
   ```
2. In Frigate's `config.yml`, set the custom ffmpeg path:
   ```yaml
   ffmpeg:
     path: /config/ffmpeg
   ```
3. Restart Frigate.

**How it works:**
- When Frigate spawns a recording process (`-f segment ...`), the shim intercepts the call, asks `rust2rtc` to record in-process, and sits dormant at **~1.5 MB RSS and 0.0% CPU**.
- When Frigate terminates or rotates the recording, the shim sends a stop signal to `rust2rtc` and exits cleanly.
- For all non-recording operations (e.g. object detection, hardware RKMPP/VAAPI decoding, `ffprobe`), the shim immediately and transparently calls real FFmpeg.
- **No `record:` block is required in `rust2rtc.yaml`**—all cameras defined in `rust2rtc` are recorded automatically!

#### 2. Advanced Camera Card ([advanced-camera-card](https://card.camera/))
`rust2rtc` is 100% compatible with AlexxIT's `video-rtc.js` dual-mode negotiation used by Advanced Camera Card:
- **Instant fMP4 preview**: Plays immediately via pure-Rust MSE muxer while WebRTC negotiates.
- **WebRTC handoff**: Instantly swaps to WebRTC without stalls or jitter buffer timeouts thanks to camera-clock RTP audio synchronization.
- **Two-way audio**: Full talkback support for Reolink and ONVIF smart doorbells.

```yaml
type: custom:advanced-camera-card
cameras:
  - camera_entity: camera.doorbell
    live_provider: go2rtc
    go2rtc:
      url: http://198.51.100.4:1984   # Your rust2rtc IP and port
      stream: doorbell               # Stream name in rust2rtc.yaml
```

#### 3. Home Assistant & Dashboard Cards
Works out-of-the-box with:
- Standard Home Assistant WebRTC Cards (`type: custom:webrtc-card`)
- Native snapshot capture (`GET /api/frame.jpeg?src=<stream>`)
- WebRTC WHEP endpoint (`POST /api/webrtc?src=<stream>`)
- Dynamic stream provisioning (`POST / PUT / DELETE /api/streams`)

#### 4. Bidirectional SIP Intercom Bridge ([ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/))
`rust2rtc` acts as a full-duplex SIP-to-RTSP media bridge, enabling any SIP endpoint, VoIP handset, softphone, or smart wall panel to interact directly with standard RTSP Wi-Fi / IP cameras:
- **See & Talk with Wi-Fi Cameras**: Exposes RTSP camera feeds (such as Reolink, Tapo, TP-Link, Yi) as dialable SIP extensions (e.g., ext `101`, `102`). Calling a camera extension instantly starts real-time video streaming and bidirectional talkback.
- **Hardware Intercom Support ([ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/))**: Fully tested with ESP32-based intercom screens and wall panels (e.g. Waveshare ESP32-P4 Touch, ESP32-S3). Displays low-latency RFC 2435 JPEG RTP or H.264 video while providing synchronized two-way audio.
- **Automated PBX Registration**: Configure the `sip:` section with your SIP PBX (FreePBX, Asterisk, or Home Assistant VoIP Integration) with `auto_register_streams: true` to automatically assign and register extensions for all streams with zero manual SIP config.

```yaml
sip:
  server: "198.51.100.5:5060"     # IP of your SIP PBX or Home Assistant VoIP
  auto_register_streams: true    # Automatically registers extensions 101, 102...
  base_extension: 101
  auto_password: "YourSecurePasswordHere"
```

---

## 🧪 Tested Devices & Hardware Compatibility

`rust2rtc` is actively validated in real-world 24/7 smart home and surveillance deployments across a wide variety of host hardware, operating systems, and camera vendors:

### 🖥️ Host Systems & Architectures
| Hardware / Platform | Architecture | Operating System | Role & Notes |
| :--- | :---: | :---: | :--- |
| **Apple Silicon MacBook** | `aarch64` / `arm64` | macOS Sonoma / Sequoia | Primary development, unit testing, Safari WebRTC (HEVC/H.264) & MSE evaluation |
| **Rockchip RK3588 (Orange Pi 5 Plus)** | `aarch64-unknown-linux-musl` | Debian / Armbian Linux | 24/7 production node running Frigate NVR, Home Assistant, and multi-camera RTSP/WebRTC ingest |
| **x86_64 Server / PC** | `x86_64-unknown-linux-musl` | Linux / Docker | Containerized NVR workloads and CI automated verification |

### 📹 Cameras, Doorbells & Encoders
| Device / Model | Firmware / Platform | Protocols & Features Tested |
| :--- | :--- | :--- |
| **Reolink Video Doorbell** (PoE & WiFi) | Official v3.0.0+ | • 5MP H.264 High Profile Level 5.1 (`profile-level-id=640033`, 2560×1920 @ 20fps)<br>• Native pure-Rust in-process AAC-to-Opus audio transcoding<br>• Full-duplex two-way audio (backchannel) over WebRTC & SIP |
| **TP-Link AIPC / Chinese PTZ**<br>(e.g. `TL-AIPC657-ZOOM-DUAL`) | Chinese Regional Firmware | • Dual-lens main & sub RTSP TCP streams<br>• In-process interleaved stream recovery & `OPTIONS` keepalive<br>• ONVIF port 2020 integration for Frigate detection |
| **TP-Link Tapo Series**<br>(Tapo C200, C210, C310, C110) | Official Tapo Firmware | • Standard RTSP over TCP with Digest MD5 authentication<br>• Low-latency H.264 video with PCMA audio |
| **Yi / Kami Cameras** | `yi-hack-allwinner-v2` / `yi-hack-v4/v5` | • Allwinner v2 platform RTSP stream ingestion<br>• In-process H.264 depacketization & sub-second JPEG snapshot capture |
| **SIP Panels & Intercom Screens**<br>(Grandstream, Akuvox, ESP32-P4 Touch) | FreePBX / Asterisk / SIP2.0 | • Bi-directional SIP audio (PCMA/PCMU 20ms)<br>• Video over SIP via RFC 2435 JPEG RTP (15 fps paced) and H.264 |

---

## 📝 Changelog & Release History

### v0.5.0 (Current)
- **🎙️ WebRTC Full-Duplex Two-Way Talkback**:
  - Pure-Rust in-process `OpusToG711Decoder` automatically decodes incoming Opus audio from browser microphones / Home Assistant cards (`a=sendrecv`) down to 8kHz G.711 $\mu$-law/A-law.
  - Relays audio directly into camera RTSP backchannel sessions in real time across both `webrtc-rs` and `rustrtc` engines.
- **💾 In-Process MP4 Segment Recorder for Frigate Cache (Zero-FFmpeg Recording)**:
  - Generates standalone progressive ISO BMFF `.mp4` segment files directly in `/tmp/cache/` in pure Rust.
  - Automatically cuts segments on IDR keyframes matching Frigate's naming convention (`<stream>@%Y%m%d%H%M%S%z.mp4`) with atomic temporary file renaming.
  - Native multiplexing for both H.264 (`avc1`) and H.265 / HEVC (`hvc1`), with AAC audio track multiplexing (`mp4a` + `esds`).
  - Transparent `ffmpeg-shim` replaces Frigate's recording subprocesses, slashing subprocess RSS from ~200 MB down to **13.2 MB total** across 7 cameras (**-93.4% memory reduction**).
- **⚡ Dual WebRTC Engine (`rustrtc` + `webrtc-rs`)**:
  - Added runtime engine toggle via `webrtc.engine: "rustrtc"` in `rust2rtc.yaml`.
  - Integrates lightweight, pure-Rust `rustrtc` crate with automatic sample packetization and PLI feedback loops.
  - Slashes gateway process RSS from **57.7 MB down to 13.6 MB (-76.4% memory footprint)** on embedded Linux (ARM64 RK3588).
- **Home Assistant WebSocket Event Subscriber**:
  - Direct WebSocket client connection to Home Assistant (`ws://.../api/websocket`) with automatic authentication and connection health monitoring.
  - Subscribes natively to doorbell visitor button events (e.g. `binary_sensor.reolink_video_doorbell_wifi_visitor`).
  - Configurable debounce cooldown timer to prevent duplicated calls while ringing or active.
- **Parallel Forking Conference Call & Ring Groups**:
  - Implements RFC 3261 parallel forking to ring multiple SIP intercom screens simultaneously (e.g. [ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/) devices).
  - "First to answer wins" race resolution: the first screen to answer (`200 OK`) is connected to live camera video + bidirectional audio, and all other ringing legs are immediately cancelled with `CANCEL`.
  - Full client transaction lifecycle support with automated non-2xx `ACK`ing, preventing server transaction lockups.
- **L16 16kHz & ESPHome VoIP Stack Support**:
  - Supports uncompressed linear PCM `L16/16000/1` audio alongside G.711 PCMA/PCMU.
  - Real-time pure-Rust conversion between 16kHz L16 audio from ESPHome smart panels and 8kHz G.711 camera talkback channels.

### v0.4.0
- **RTSP Interleaved Stream Resynchronization**:
  - Implemented automatic binary packet delimiter (`$`) scanner to recover instantly from corrupted or desynchronized interleaved TCP streams.
  - Added full response body draining based on `Content-Length` to prevent parser lockups from RTSP responses that include trailing payload bytes.
  - Replaced periodic keepalive with universal bodyless `OPTIONS`, eliminating keepalive rejection on cameras with strict parameter requirements.
- **Dynamic H.264 Profile & Level Negotiation for 5MP/4K Cameras**:
  - Dynamically extracts exact `profile-level-id` (e.g. `640033` for 5MP Reolink Doorbell vs `640029` for 1080p) from SDP `fmtp` and SPS NAL units.
  - Prevents Chromium and Safari `SourceBuffer` decode rejections caused by Level 4.1 frame macroblock limits.
  - Supports both JSON string and array formats (`["avc1", "hvc1"]`) in client codec negotiation messages.
- **WebRTC Codec Registration & WAN Connectivity**:
  - Added H.264 High Profile Level 5.1 (`profile-level-id=640033`) SDP codec registration.
  - Extended STUN ICE candidate gathering window to 500 ms to reliably obtain server-reflexive candidates for external WAN and cellular IPv6 clients.

### v0.3.0
- **Pure-Rust In-Process RTSP Client**:
  - High-performance TCP interleaved RTSP ingest engine completely eliminating `ffmpeg -c copy` background processes.
  - Native Digest MD5 and Basic authentication without external tools.
  - Dynamic parameter set (SPS/PPS/VPS) extraction from SDP and inline bitstreams.
- **Home Assistant go2rtc API Compatibility**:
  - Implemented `/api` returning compatible `1.9.14` version and dynamic `rust2rtc` revision string.
  - Added `/api/schemes` and `/api/preload` endpoints to allow Home Assistant to validate `rust2rtc` as an external go2rtc instance without spawning its bundled binaries.

### v0.2.0
- **Pure-Rust In-Process Audio Transcoding**:
  - Native decoding of AAC-hbr (RFC 3640) and G.711 to 48kHz Opus via `symphonia` and `opus-pure`.
  - Camera-clock lip-sync and deduplicated shared broadcast tracks across concurrent viewers.
- **SIP Intercom & Backchannel**:
  - Full-duplex backchannel for Reolink and ONVIF smart doorbells.
  - High-fidelity G.711a/u 20ms audio repacking and RFC 2435 JPEG RTP video streaming for hardware intercom screens.

### v0.1.0
- Initial release: Tokio/Axum asynchronous core, WebRTC WHEP gateway, RTSP TCP server (`:8554`), and fast JPEG snapshots.

---

## 🤝 Contributing

Contributions are very welcome! Whether you are interested in:
- Adding support for more protocols (HLS, MSE, RTMP, HomeKit)
- Enhancing WebRTC ICE/STUN/TURN handling
- Writing unit and integration tests
- Optimizing zero-copy packet forwarding and memory utilization

Please feel free to open an issue or submit a pull request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'feat: add amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

---

## 📜 License & Acknowledgments

- Inspired by the brilliant work of [AlexxIT](https://github.com/AlexxIT) and the [`go2rtc`](https://github.com/AlexxIT/go2rtc) project.
- Licensed under the [MIT License](LICENSE).
