use anyhow::{bail, Context, Result};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};
use url::Url;

#[derive(Debug, Clone)]
pub struct HaVoipClient {
    pub base_url: String,
    pub token: String,
}

impl HaVoipClient {
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
        }
    }

    /// Call a `voip_stack.<service>` action on Home Assistant via REST API
    pub async fn call_voip_service(
        &self,
        service: &str,
        payload: &serde_json::Value,
    ) -> Result<(u16, String)> {
        let (status, body) = self.call_voip_service_raw(service, payload, true).await?;
        if status == 400 && body.to_lowercase().contains("does not support response") {
            return self.call_voip_service_raw(service, payload, false).await;
        }
        Ok((status, body))
    }

    async fn call_voip_service_raw(
        &self,
        service: &str,
        payload: &serde_json::Value,
        return_response: bool,
    ) -> Result<(u16, String)> {
        let full_url = format!("{}/api/services/voip_stack/{}", self.base_url, service);
        let parsed = Url::parse(&full_url).context("Invalid Home Assistant URL")?;

        let host = parsed.host_str().context("Missing host in ha_url")?;
        let port = parsed.port_or_known_default().unwrap_or(8123);
        let path = if return_response {
            format!("{}?return_response", parsed.path())
        } else {
            parsed.path().to_string()
        };
        let addr = format!("{}:{}", host, port);

        let body = serde_json::to_string(payload)?;
        let req = format!(
            "POST {} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Authorization: Bearer {}\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            path,
            host,
            port,
            self.token,
            body.len(),
            body
        );

        let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(&addr))
            .await
            .context("Timeout connecting to Home Assistant API")?
            .with_context(|| format!("Failed to connect to Home Assistant at {}", addr))?;

        stream.write_all(req.as_bytes()).await?;

        let mut resp_bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut resp_bytes))
            .await
            .context("Timeout reading Home Assistant API response")??;

        let raw = String::from_utf8_lossy(&resp_bytes);
        parse_http_response(&raw)
    }

    /// Provision or update a SIP account in Home Assistant's `voip_stack`,
    /// automatically resolving existing username password mismatches or extension collisions.
    /// Returns the final provisioned extension string.
    pub async fn provision_account(
        &self,
        username: &str,
        password: &str,
        initial_ext: &str,
        display_name: Option<&str>,
        used_extensions: &mut std::collections::HashSet<String>,
    ) -> Result<String> {
        let mut current_ext = initial_ext.to_string();
        let mut numeric_ext = current_ext.parse::<u16>().ok();

        for attempt in 0..50 {
            if attempt > 0 {
                if let Some(ref mut num) = numeric_ext {
                    loop {
                        *num += 1;
                        let candidate = num.to_string();
                        if !used_extensions.contains(&candidate) {
                            current_ext = candidate;
                            break;
                        }
                    }
                } else {
                    break;
                }
            }

            let mut payload = serde_json::json!({
                "username": username,
                "password": password,
                "extension": current_ext,
                "replace": true,
            });
            if let Some(dn) = display_name
                && !dn.is_empty() {
                    payload["display_name"] = serde_json::Value::String(dn.to_string());
                }

            let (status, body) = self.call_voip_service("create_account", &payload).await?;
            if (200..300).contains(&status) {
                info!(
                    username = %username,
                    extension = %current_ext,
                    "Provisioned SIP account in Home Assistant voip_stack"
                );
                used_extensions.insert(current_ext.clone());
                return Ok(current_ext);
            }

            let body_lower = body.to_lowercase();
            debug!(
                status = status,
                body = %body,
                username = %username,
                extension = %current_ext,
                "voip_stack.create_account response"
            );

            if status == 401 || status == 403 {
                bail!("Home Assistant rejected ha_token (HTTP {})", status);
            }

            // If account username already exists, remove it first and retry with the same extension
            if attempt == 0 && body_lower.contains("already exists") && body_lower.contains(&username.to_lowercase()) {
                info!(
                    username = %username,
                    "SIP account already exists in Home Assistant; rotating/re-creating to sync credentials"
                );
                let rm_payload = serde_json::json!({ "username": username });
                let _ = self.call_voip_service("remove_account", &rm_payload).await;

                let (status2, body2) = self.call_voip_service("create_account", &payload).await?;
                if (200..300).contains(&status2) {
                    info!(
                        username = %username,
                        extension = %current_ext,
                        "Re-created SIP account in Home Assistant voip_stack"
                    );
                    used_extensions.insert(current_ext.clone());
                    return Ok(current_ext);
                }
                debug!(status = status2, body = %body2, "Retry after remove_account failed, trying next extension");
            }

            // Otherwise, likely an extension collision (e.g. extension already used by another device)
            warn!(
                username = %username,
                extension = %current_ext,
                body = %body.trim(),
                "Extension '{}' unavailable in Home Assistant, trying next extension",
                current_ext
            );
        }

        bail!(
            "Failed to provision SIP account '{}' in Home Assistant after multiple extension attempts",
            username
        )
    }
}

fn parse_http_response(raw: &str) -> Result<(u16, String)> {
    let (header_part, body_part) = match raw.split_once("\r\n\r\n") {
        Some((h, b)) => (h, b),
        None => match raw.split_once("\n\n") {
            Some((h, b)) => (h, b),
            None => (raw, ""),
        },
    };

    let status_line = header_part.lines().next().context("Empty HTTP response")?;
    let mut parts = status_line.split_whitespace();
    let _http_ver = parts.next();
    let status_code: u16 = parts
        .next()
        .context("Missing HTTP status code")?
        .parse()
        .context("Invalid HTTP status code")?;

    Ok((status_code, body_part.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_http_response() {
        let raw = "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\n\r\n{\"message\": \"SIP account doorbell already exists\"}";
        let (code, body) = parse_http_response(raw).unwrap();
        assert_eq!(code, 400);
        assert!(body.contains("already exists"));
    }
}
