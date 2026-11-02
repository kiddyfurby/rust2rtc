use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::DoorbellConfig;
use crate::sip::SipManager;

/// Start background listener for Home Assistant doorbell visitor events
pub fn start_doorbell_subscriber(
    ha_url: String,
    ha_token: String,
    doorbells: HashMap<String, DoorbellConfig>,
    sip_manager: Arc<SipManager>,
) {
    tokio::spawn(async move {
        run_subscriber_loop(ha_url, ha_token, doorbells, sip_manager).await;
    });
}

/// Convert HTTP(S) HA URL to WS(S) URL with /api/websocket endpoint
fn normalize_ws_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let ws_base = if trimmed.starts_with("https://") {
        format!("wss://{}", &trimmed["https://".len()..])
    } else if trimmed.starts_with("http://") {
        format!("ws://{}", &trimmed["http://".len()..])
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("ws://{}", trimmed)
    };

    if ws_base.ends_with("/api/websocket") {
        ws_base
    } else {
        format!("{}/api/websocket", ws_base)
    }
}

async fn run_subscriber_loop(
    ha_url: String,
    ha_token: String,
    doorbells: HashMap<String, DoorbellConfig>,
    sip_manager: Arc<SipManager>,
) {
    let ws_url = normalize_ws_url(&ha_url);
    let last_triggers: Arc<Mutex<HashMap<String, Instant>>> = Arc::new(Mutex::new(HashMap::new()));

    info!(
        url = %ws_url,
        doorbells = ?doorbells.keys().collect::<Vec<_>>(),
        "Starting Home Assistant doorbell event subscriber"
    );

    loop {
        match connect_and_listen(&ws_url, &ha_token, &doorbells, &sip_manager, &last_triggers).await {
            Ok(()) => {
                info!("Home Assistant WebSocket disconnected cleanly; reconnecting in 5s...");
            }
            Err(e) => {
                warn!("Home Assistant WebSocket connection error: {:#}; reconnecting in 5s...", e);
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_and_listen(
    ws_url: &str,
    ha_token: &str,
    doorbells: &HashMap<String, DoorbellConfig>,
    sip_manager: &Arc<SipManager>,
    last_triggers: &Arc<Mutex<HashMap<String, Instant>>>,
) -> Result<()> {
    let connect_timeout = Duration::from_secs(10);
    let (ws_stream, _) = tokio::time::timeout(connect_timeout, connect_async(ws_url))
        .await
        .context("Timeout connecting to Home Assistant WebSocket")??;

    let (mut write, mut read) = ws_stream.split();
    let msg_id = AtomicU64::new(1);

    // 1. Receive auth_required
    let first_msg = tokio::time::timeout(Duration::from_secs(10), read.next())
        .await
        .context("Timeout waiting for auth_required message")?
        .context("WebSocket closed before auth_required")??;

    let first_text = first_msg.to_text().context("Non-text first message")?;
    let first_val: Value = serde_json::from_str(first_text)?;
    if first_val.get("type").and_then(|t| t.as_str()) != Some("auth_required") {
        bail!("Expected auth_required from Home Assistant, got: {}", first_text);
    }

    // 2. Send auth
    let auth_msg = json!({
        "type": "auth",
        "access_token": ha_token,
    })
    .to_string();
    write.send(Message::Text(auth_msg.into())).await?;

    // 3. Receive auth result
    let auth_resp = tokio::time::timeout(Duration::from_secs(10), read.next())
        .await
        .context("Timeout waiting for auth result")?
        .context("WebSocket closed during auth")??;

    let auth_resp_text = auth_resp.to_text().context("Non-text auth response")?;
    let auth_resp_val: Value = serde_json::from_str(auth_resp_text)?;
    let auth_type = auth_resp_val.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if auth_type != "auth_ok" {
        bail!("Home Assistant authentication rejected: {}", auth_resp_text);
    }

    info!("Authenticated with Home Assistant WebSocket successfully");

    // 4. Subscribe to triggers for each configured doorbell
    let mut subscriptions = HashMap::new();
    for (stream_name, cfg) in doorbells {
        let id = msg_id.fetch_add(1, Ordering::SeqCst);
        let sub_payload = json!({
            "id": id,
            "type": "subscribe_trigger",
            "trigger": {
                "platform": "state",
                "entity_id": cfg.entity,
                "to": "on"
            }
        })
        .to_string();

        write.send(Message::Text(sub_payload.into())).await?;
        subscriptions.insert(id, (stream_name.clone(), cfg.clone()));
        info!(
            stream = %stream_name,
            entity = %cfg.entity,
            sub_id = id,
            "Subscribed to doorbell visitor trigger"
        );
    }

    // 5. Keepalive ping interval & message reading loop
    let mut ping_ticker = tokio::time::interval(Duration::from_secs(30));
    ping_ticker.tick().await; // Consume first instant tick

    loop {
        tokio::select! {
            _ = ping_ticker.tick() => {
                let pid = msg_id.fetch_add(1, Ordering::SeqCst);
                let ping_msg = json!({
                    "id": pid,
                    "type": "ping"
                }).to_string();
                if let Err(e) = write.send(Message::Text(ping_msg.into())).await {
                    bail!("Failed to send ping: {}", e);
                }
            }
            maybe_msg = read.next() => {
                let msg = match maybe_msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => bail!("WebSocket error: {}", e),
                    None => bail!("WebSocket closed by Home Assistant"),
                };

                if msg.is_ping() {
                    write.send(Message::Pong(msg.into_data())).await?;
                    continue;
                }

                if !msg.is_text() {
                    continue;
                }

                let text = msg.to_text().unwrap_or_default();
                let v: Value = match serde_json::from_str(text) {
                    Ok(val) => val,
                    Err(_) => continue,
                };

                let msg_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

                if msg_type == "result" {
                    let id = v.get("id").and_then(|i| i.as_u64());
                    let success = v.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
                    if let Some(id) = id {
                        if let Some((stream, cfg)) = subscriptions.get(&id) {
                            if success {
                                debug!(stream = %stream, entity = %cfg.entity, sub_id = id, "HA subscription confirmed");
                            } else {
                                warn!(stream = %stream, entity = %cfg.entity, sub_id = id, error = ?v.get("error"), "HA subscription returned failure");
                            }
                        }
                    }
                } else if msg_type == "event" {
                    let id = v.get("id").and_then(|i| i.as_u64());
                    if let Some(id) = id {
                        if let Some((stream_name, cfg)) = subscriptions.get(&id) {
                            // Extract to_state if present
                            let new_state = v.pointer("/event/variables/trigger/to_state/state")
                                .or_else(|| v.pointer("/event/trigger/to_state/state"))
                                .or_else(|| v.pointer("/event/to_state/state"))
                                .or_else(|| v.pointer("/event/data/new_state/state"))
                                .and_then(|s| s.as_str());

                            debug!(
                                stream = %stream_name,
                                entity = %cfg.entity,
                                new_state = ?new_state,
                                "Received Home Assistant trigger event"
                            );

                            // The trigger was configured with "to": "on", but verify state if present
                            let is_on = match new_state {
                                Some(s) => s.eq_ignore_ascii_case("on") || s.eq_ignore_ascii_case("true"),
                                None => true, // Fallback to true since trigger matched "to": "on"
                            };

                            if !is_on {
                                continue;
                            }

                            // Enforce cooldown
                            let now = Instant::now();
                            let cooldown_dur = Duration::from_secs(cfg.cooldown_seconds);
                            {
                                let mut last_map = last_triggers.lock().await;
                                if let Some(last_time) = last_map.get(stream_name) {
                                    if now.duration_since(*last_time) < cooldown_dur {
                                        info!(
                                            stream = %stream_name,
                                            entity = %cfg.entity,
                                            cooldown_left_sec = (cooldown_dur - now.duration_since(*last_time)).as_secs(),
                                            "Doorbell button press ignored due to active cooldown"
                                        );
                                        continue;
                                    }
                                }
                                last_map.insert(stream_name.clone(), now);
                            }

                            info!(
                                stream = %stream_name,
                                entity = %cfg.entity,
                                targets = ?cfg.sip_targets,
                                "Doorbell button pressed! Initiating parallel intercom ring group"
                            );

                            let sip = Arc::clone(sip_manager);
                            let stream = stream_name.clone();
                            let doorbell_cfg = cfg.clone();

                            tokio::spawn(async move {
                                if let Err(e) = sip.trigger_doorbell_call(&stream, &doorbell_cfg).await {
                                    warn!(stream = %stream, "Error during doorbell call trigger: {}", e);
                                }
                            });
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_ws_url() {
        assert_eq!(
            normalize_ws_url("http://192.168.1.4:8123"),
            "ws://192.168.1.4:8123/api/websocket"
        );
        assert_eq!(
            normalize_ws_url("https://ha.mydomain.com:8123/"),
            "wss://ha.mydomain.com:8123/api/websocket"
        );
        assert_eq!(
            normalize_ws_url("ws://localhost:8123/api/websocket"),
            "ws://localhost:8123/api/websocket"
        );
    }
}
