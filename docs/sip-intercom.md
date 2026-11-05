# Bidirectional SIP Bridge & Intercom Guide

`rust2rtc` includes a built-in asynchronous SIP client, UAS/UAC transaction engine, and RTP media bridge that exposes standard RTSP IP cameras as dialable video intercom extensions.

---

## 1. Supported Capabilities

- **Inbound & Outbound SIP Calls**: Dial any camera extension from a VoIP phone, softphone (Linphone, MicroSIP), or wall panel to view live camera video and talk through the camera speaker.
- **Parallel Forking Ring Groups**: When a Home Assistant doorbell binary sensor triggers, `rust2rtc` rings multiple SIP targets simultaneously (`doorbells.<stream>.sip_targets`). The first screen to answer (`200 OK`) receives the call while all other ringing legs are immediately cancelled (`CANCEL`).
- **Video over SIP**:
  - **H.264 (RFC 6184)**: Zero-transcode RTP packet forwarding with dynamic payload type rewriting, SSRC normalization, and automatic `sprop-parameter-sets` / STAP-A injection before IDR keyframes.
  - **JPEG RTP (RFC 2435, PT 26)**: On-demand MJPEG RTP streaming (15 fps paced with backlog dropping) for hardware-decoded smart screens such as Waveshare ESP32-P4 Touch panels running [ESPHome Intercom](https://github.com/n-IA-hane/esphome-intercom/).
- **Two-Way Audio (Backchannel)**:
  - Supports G.711 PCMA (8 kHz), G.711 PCMU (8 kHz), and uncompressed `L16/16000/1` PCM audio.
  - Repacks incoming RTP audio into 20 ms frames (160 samples @ 8 kHz) and relays to camera talkback channels (ONVIF `a=sendonly` backchannel, Reolink, TP-Link `multitrans://`, and Tapo HTTP `streamd` `:8800`).

---

## 2. Configuration Example

```yaml
sip:
  server: "192.168.1.4:5060"          # SIP PBX (Asterisk, FreePBX, or Home Assistant VoIP)
  domain: "192.168.1.4"
  ha_url: "http://192.168.1.4:8123"   # Optional: Home Assistant URL for voip_stack provisioning
  ha_token: "YOUR_HA_LONG_LIVED_TOKEN"
  auto_register_streams: true         # Auto-register all primary streams starting at base_extension
  base_extension: 101
  auto_password: "ChangeMeToAStrongPassword"
  max_call_duration_secs: 180         # Automatic call timeout watchdog (default: 180s)
  names:
    doorbell: "Front Door"
    driveway: "Driveway"

doorbells:
  doorbell:
    entity: "binary_sensor.front_doorbell_visitor"
    sip_targets:
      - "192.168.1.100:5060"          # Living Room ESPHome Intercom
      - "192.168.1.101:5060"          # Kitchen ESPHome Intercom
    cooldown_seconds: 15
```

---

## 3. REST API Control

- **List registered SIP accounts & active call status**:
  `GET http://localhost:1984/api/sip/accounts`
- **Trigger an outbound SIP call**:
  `POST http://localhost:1984/api/sip/call?stream=doorbell&dest=105`
- **Hang up an active SIP call**:
  `POST http://localhost:1984/api/sip/hangup?stream=doorbell`
