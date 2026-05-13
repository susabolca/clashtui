use std::{path::Path, time::Duration};

use anyhow::{Context as _, Result};
use reqwest::{Client, RequestBuilder};
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::config::ControllerConfig;

const MIHOMO_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct MihomoClient {
    base_url: String,
    secret: Option<String>,
    client: Client,
}

#[derive(Debug, Clone)]
pub struct ProxyGroup {
    pub name: String,
    pub kind: String,
    pub now: String,
    pub all: Vec<String>,
}

impl MihomoClient {
    pub fn new(config: &ControllerConfig) -> Self {
        Self {
            base_url: config.url.trim_end_matches('/').to_string(),
            secret: config.secret.clone().filter(|value| !value.is_empty()),
            client: Client::new(),
        }
    }

    pub async fn version(&self) -> Result<String> {
        let value: Value = self.send(self.client.get(self.url("/version"))).await?;
        Ok(value
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string())
    }

    pub async fn configs(&self) -> Result<Value> {
        self.send(self.client.get(self.url("/configs"))).await
    }

    pub async fn patch_configs(&self, patch: &Value) -> Result<()> {
        let _: Value = self
            .send(self.client.patch(self.url("/configs")).json(patch))
            .await?;
        Ok(())
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        self.patch_configs(&json!({ "mode": mode })).await
    }

    pub async fn set_mixed_port(&self, port: u16) -> Result<()> {
        self.patch_configs(&json!({ "mixed-port": port })).await
    }

    pub async fn proxy_groups(&self) -> Result<Vec<ProxyGroup>> {
        let value: Value = self.send(self.client.get(self.url("/proxies"))).await?;
        proxy_groups_from_value(&value)
    }

    pub async fn connections(&self) -> Result<Value> {
        self.send(self.client.get(self.url("/connections"))).await
    }

    pub async fn traffic(&self) -> Result<Value> {
        self.send_stream_sample(self.client.get(self.url("/traffic")))
            .await
    }

    pub async fn select_proxy(&self, group: &str, proxy: &str) -> Result<()> {
        let body = json!({ "name": proxy });
        let _: Value = self
            .send(
                self.client
                    .put(self.url(&format!("/proxies/{}", encode_path_segment(group))))
                    .json(&body),
            )
            .await?;
        Ok(())
    }

    pub async fn proxy_delay(&self, proxy: &str, test_url: &str, timeout_ms: u64) -> Result<u64> {
        let value: Value = self
            .send(self.client.get(self.url(&format!(
                "/proxies/{}/delay?timeout={}&url={}",
                encode_path_segment(proxy),
                timeout_ms,
                encode_query_component(test_url)
            ))))
            .await?;
        delay_from_value(&value)
    }

    pub async fn reload_config(&self, path: &Path) -> Result<()> {
        let body = json!({
            "path": path.to_string_lossy(),
            "payload": "",
        });
        let _: Value = self
            .send(self.client.put(self.url("/configs?force=true")).json(&body))
            .await?;
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn send<T>(&self, request: RequestBuilder) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let request = if let Some(secret) = &self.secret {
            request.bearer_auth(secret)
        } else {
            request
        };
        let response = timeout(MIHOMO_REQUEST_TIMEOUT, request.send())
            .await
            .context("mihomo request timed out")?
            .context("mihomo request failed")?;
        let status = response.status();
        let text = timeout(MIHOMO_REQUEST_TIMEOUT, response.text())
            .await
            .context("mihomo response body timed out")?
            .unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("mihomo returned {status}: {text}");
        }
        if text.is_empty() {
            return serde_json::from_str("{}").context("failed to decode empty mihomo response");
        }
        serde_json::from_str(&text).context("failed to decode mihomo response")
    }

    async fn send_stream_sample<T>(&self, request: RequestBuilder) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let request = if let Some(secret) = &self.secret {
            request.bearer_auth(secret)
        } else {
            request
        };
        let mut response = timeout(MIHOMO_REQUEST_TIMEOUT, request.send())
            .await
            .context("mihomo request timed out")?
            .context("mihomo request failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = timeout(MIHOMO_REQUEST_TIMEOUT, response.text())
                .await
                .context("mihomo response body timed out")?
                .unwrap_or_default();
            anyhow::bail!("mihomo returned {status}: {text}");
        }

        let mut buffer = Vec::new();
        loop {
            let chunk = timeout(MIHOMO_REQUEST_TIMEOUT, response.chunk())
                .await
                .context("mihomo stream sample timed out")?
                .context("mihomo stream sample failed")?
                .context("mihomo stream ended before a sample")?;
            buffer.extend_from_slice(&chunk);

            if let Some(sample) = first_complete_stream_line(&buffer) {
                return serde_json::from_slice(sample)
                    .context("failed to decode mihomo stream sample");
            }

            let sample = trim_ascii_bytes(&buffer);
            if !sample.is_empty()
                && let Ok(value) = serde_json::from_slice(sample)
            {
                return Ok(value);
            }

            if buffer.len() > 64 * 1024 {
                anyhow::bail!("mihomo stream sample exceeded 64KiB before JSON");
            }
        }
    }
}

fn first_complete_stream_line(buffer: &[u8]) -> Option<&[u8]> {
    let mut start = 0;
    for (index, byte) in buffer.iter().enumerate() {
        if *byte == b'\n' {
            let line = trim_ascii_bytes(&buffer[start..index]);
            if !line.is_empty() {
                return Some(line);
            }
            start = index + 1;
        }
    }
    None
}

fn trim_ascii_bytes(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn proxy_groups_from_value(value: &Value) -> Result<Vec<ProxyGroup>> {
    let proxies = value
        .get("proxies")
        .and_then(Value::as_object)
        .context("mihomo /proxies response has no proxies object")?;
    let mut groups = proxies
        .iter()
        .filter_map(|(name, value)| {
            let all = value.get("all")?.as_array()?;
            if all.is_empty() {
                return None;
            }
            Some(ProxyGroup {
                name: name.clone(),
                kind: value
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                now: value
                    .get("now")
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string(),
                all: all
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(groups)
}

fn delay_from_value(value: &Value) -> Result<u64> {
    value
        .get("delay")
        .and_then(Value::as_u64)
        .context("mihomo delay response has no numeric delay")
}

fn encode_path_segment(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push('%');
            output.push_str(&format!("{byte:02X}"));
        }
    }
    output
}

fn encode_query_component(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            output.push(char::from(byte));
        } else if byte == b' ' {
            output.push_str("%20");
        } else {
            output.push('%');
            output.push_str(&format!("{byte:02X}"));
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_groups_from_mihomo_response() -> Result<()> {
        let value = json!({
            "proxies": {
                "DIRECT": {"type": "Direct"},
                "HK-01": {"type": "Vmess"},
                "Proxy": {
                    "type": "Selector",
                    "now": "HK-01",
                    "all": ["HK-01", "DIRECT"]
                },
                "Auto": {
                    "type": "URLTest",
                    "now": "HK-01",
                    "all": ["HK-01", "US-01"]
                }
            }
        });

        let groups = proxy_groups_from_value(&value)?;

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "Auto");
        assert_eq!(groups[0].kind, "URLTest");
        assert_eq!(groups[1].name, "Proxy");
        assert_eq!(groups[1].now, "HK-01");
        assert_eq!(groups[1].all, ["HK-01", "DIRECT"]);
        Ok(())
    }

    #[test]
    fn encodes_proxy_group_path_segment() {
        assert_eq!(
            encode_path_segment("Proxy Group/香港"),
            "Proxy%20Group%2F%E9%A6%99%E6%B8%AF"
        );
    }

    #[test]
    fn encodes_delay_query_url() {
        assert_eq!(
            encode_query_component("https://cp.cloudflare.com/generate_204"),
            "https%3A%2F%2Fcp.cloudflare.com%2Fgenerate_204"
        );
    }

    #[test]
    fn parses_proxy_delay_response() -> Result<()> {
        assert_eq!(delay_from_value(&json!({ "delay": 123 }))?, 123);
        Ok(())
    }

    #[test]
    fn reads_first_json_line_from_stream_buffer() {
        let buffer = b"\n {\"up\":1024,\"down\":2048}\n{\"up\":1,\"down\":2}\n";

        assert_eq!(
            first_complete_stream_line(buffer),
            Some(br#"{"up":1024,"down":2048}"#.as_slice())
        );
    }
}
