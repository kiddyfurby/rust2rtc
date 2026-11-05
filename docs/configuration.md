# Configuration & Security Reference

`rust2rtc` loads configuration from `rust2rtc.yaml` (or `-c` / `--config` / `-config=` CLI flags, merging multiple files in order). A complete template is available in [`rust2rtc.sample.yaml`](../rust2rtc.sample.yaml).

---

## 1. Top-Level Configuration Sections

| Section | Description | Default |
| :--- | :--- | :--- |
| `streams` | Map of stream names to one or more source URLs (`rtsp://`, `onvif://`, `tapo://`, `multitrans://`, `ffmpeg:`, `exec:`). | `{}` (Managed by Frigate if omitted) |
| `api` | HTTP REST API, WebSocket signaling, and Web UI settings (`listen`, `origin`, `username`, `password`). | `listen: ":1984"` |
| `rtsp` | Embedded TCP interleaved RTSP server settings (`listen`). | `listen: ":8554"` |
| `webrtc` | ICE candidate list (`candidates`) and engine selection (`engine: "rustrtc"`). | `engine: "rustrtc"` |
| `ffmpeg` | External FFmpeg binary path (`bin`), global flags (`global`), and hardware acceleration (`hardware: "rkmpp" \| "vaapi" \| "cuda"`). | `bin: "ffmpeg"` |
| `record` | In-process MP4 segment recorder settings (`enabled`, `dir`, `segment_duration`, `streams`). | `enabled: true`, `dir: "/tmp/cache"`, `segment_duration: 10` |
| `sip` | SIP registration, Digest auth credentials, and Home Assistant `voip_stack` integration. | `None` |
| `doorbells` | Home Assistant WebSocket binary sensor triggers for outbound parallel SIP calls. | `{}` |

---

## 2. Security & Network Hardening

### HTTP API Authentication (`:1984`)
By default, like `go2rtc`, the HTTP API and Web Dashboard on port `:1984` do not require authentication so local containers (Frigate, Home Assistant) can query `/api/streams` and `/api/webrtc` without extra headers.

If port `:1984` is exposed beyond a trusted LAN or container network, enable **HTTP Basic Authentication** in `rust2rtc.yaml`:

```yaml
api:
  listen: ":1984"
  username: "admin"
  password: "YourStrongApiPassword"
```

When `username` and `password` are configured, all HTTP and WebSocket endpoints on `:1984` require a valid `Authorization: Basic ...` header and return `401 Unauthorized` otherwise.

### RTSP (`:8554`) & SIP (`:5060`) Exposure
- **RTSP Server (`:8554`)**: Intended for local loopback (`127.0.0.1:8554`) or trusted LAN consumers (Frigate, VLC, Home Assistant). Bind to `127.0.0.1:8554` if external LAN clients do not need direct RTSP relay access.
- **SIP Client / UAS**: Authenticates outbound `REGISTER` and `INVITE` requests using RFC 2617 Digest MD5 (`qop=auth` and legacy MD5). Do not expose UDP SIP/RTP ports directly to the public internet without a firewall or SBC.
- **Parser Resource Limits**: Network parsers for RTSP, SIP, and RTP enforce strict header and body size caps (e.g., 64 KB RTSP/SIP body limits, 256 KB read buffer ceilings) and validate packet bounds without panicking on malformed network input.

### Keeping Credentials Out of Git
- **`rust2rtc.yaml` is gitignored**: Copy [`rust2rtc.sample.yaml`](../rust2rtc.sample.yaml) to `rust2rtc.yaml` for local deployment. Never commit real camera passwords, SIP secrets, or Home Assistant tokens (`HA_TOKEN` / `HA_URL` can also be supplied via environment variables).
