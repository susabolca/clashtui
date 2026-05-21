use std::path::PathBuf;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use reqwest::{
    Client, Proxy, Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT},
};
use serde_yaml_ng::Value;
use tokio::fs;

use crate::config::{AppConfig, Paths, Subscription, SubscriptionUserInfo};
use crate::system_proxy;

const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(20);
const SUBSCRIPTION_USER_AGENT: &str = concat!("clash-verge/v", env!("CARGO_PKG_VERSION"));

struct SubscriptionResponse {
    body: String,
    user_info: Option<SubscriptionUserInfo>,
}

struct DownloadAttempt {
    label: &'static str,
    proxy_url: Option<String>,
}

pub async fn update(paths: &Paths, config: &mut AppConfig, index: usize) -> Result<PathBuf> {
    let sub = config
        .subscriptions
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("subscription index out of range"))?;
    let response = download_subscription(config, &sub.url)
        .await
        .with_context(|| format!("failed to download subscription {}", sub.name))?;
    let body = response.body;

    if body.trim().is_empty() {
        anyhow::bail!("subscription {} returned empty content", sub.name);
    }
    validate_subscription_profile_body(&sub.name, &body)?;

    paths.ensure().await?;
    let profile_path = profile_path(paths, &sub);
    fs::write(&profile_path, body)
        .await
        .with_context(|| format!("failed to write {}", profile_path.display()))?;

    if let Some(current) = config.subscriptions.get_mut(index) {
        current.updated_at = Some(now_unix().to_string());
        current.last_error = None;
        if let Some(user_info) = response.user_info {
            current.user_info = user_info;
        }
    }

    Ok(profile_path)
}

pub async fn update_preserving_last_good(
    paths: &Paths,
    config: &mut AppConfig,
    index: usize,
) -> Result<PathBuf> {
    match update(paths, config, index).await {
        Ok(path) => Ok(path),
        Err(err) => {
            if let Some(current) = config.subscriptions.get_mut(index) {
                current.last_error = Some(err.to_string());
            }
            Err(err)
        }
    }
}

async fn download_subscription(config: &AppConfig, url: &str) -> Result<SubscriptionResponse> {
    let attempts = download_attempts(config);
    let mut errors = Vec::new();

    for attempt in attempts {
        match download_subscription_once(url, &attempt).await {
            Ok(response) => return Ok(response),
            Err(err) => errors.push(format!("{}: {err:#}", attempt.label)),
        }
    }

    anyhow::bail!("{}", errors.join("; "))
}

async fn download_subscription_once(
    url: &str,
    attempt: &DownloadAttempt,
) -> Result<SubscriptionResponse> {
    let (url, auth) = subscription_request_url(url)?;
    let client = subscription_client(attempt.proxy_url.as_deref())?;
    let mut request = client.get(url);
    if let Some(auth) = auth {
        request = request.header(AUTHORIZATION, auth);
    }

    let response = request.send().await.context("request failed")?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .text()
        .await
        .context("failed to read response body")?;

    if !status.is_success() {
        anyhow::bail!("status {status}: {}", summarize_response_body(&body));
    }

    Ok(SubscriptionResponse {
        user_info: parse_user_info_headers(&headers),
        body,
    })
}

fn subscription_client(proxy_url: Option<&str>) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(SUBSCRIPTION_USER_AGENT)
            .context("invalid subscription user agent")?,
    );

    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(None)
        .timeout(SUBSCRIPTION_TIMEOUT)
        .connect_timeout(SUBSCRIPTION_TIMEOUT)
        .default_headers(headers);

    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(Proxy::all(proxy_url)?);
    } else {
        builder = builder.no_proxy();
    }

    Ok(builder.build()?)
}

fn validate_subscription_profile_body(name: &str, body: &str) -> Result<()> {
    let value: Value = serde_yaml_ng::from_str(body)
        .with_context(|| format!("subscription {name} returned invalid YAML"))?;
    let Some(mapping) = value.as_mapping() else {
        anyhow::bail!("subscription {name} returned non-profile content");
    };

    let has_proxies = mapping
        .get("proxies")
        .and_then(Value::as_sequence)
        .is_some_and(|items| !items.is_empty());
    let has_proxy_providers = mapping
        .get("proxy-providers")
        .and_then(Value::as_mapping)
        .is_some_and(|items| !items.is_empty());
    if !has_proxies && !has_proxy_providers {
        anyhow::bail!("subscription {name} returned a profile without proxies");
    }

    Ok(())
}

fn subscription_request_url(url: &str) -> Result<(Url, Option<HeaderValue>)> {
    let mut url = Url::parse(url).context("invalid subscription URL")?;
    let auth = if !url.username().is_empty() {
        let value = match url.password() {
            Some(password) => format!("{}:{password}", url.username()),
            None => format!("{}:", url.username()),
        };
        Some(HeaderValue::from_str(&format!(
            "Basic {}",
            base64_encode(value.as_bytes())
        ))?)
    } else {
        None
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Ok((url, auth))
}

fn download_attempts(config: &AppConfig) -> Vec<DownloadAttempt> {
    let mut attempts = vec![DownloadAttempt {
        label: "direct",
        proxy_url: None,
    }];

    let local_proxy = format!("http://{}:{}", config.proxy_host, config.mixed_port);
    attempts.push(DownloadAttempt {
        label: "local proxy",
        proxy_url: Some(local_proxy.clone()),
    });

    if let Ok(status) = system_proxy::status()
        && status.enabled
        && !status.server.trim().is_empty()
    {
        let system_proxy = format!("http://{}", status.server);
        if system_proxy != local_proxy {
            attempts.push(DownloadAttempt {
                label: "system proxy",
                proxy_url: Some(system_proxy),
            });
        }
    }

    attempts
}

fn parse_user_info_headers(headers: &HeaderMap) -> Option<SubscriptionUserInfo> {
    headers.iter().find_map(|(key, value)| {
        let key = key.as_str().to_ascii_lowercase();
        key.strip_suffix("subscription-userinfo")
            .filter(|prefix| prefix.is_empty() || prefix.ends_with('-'))
            .and_then(|_| value.to_str().ok())
            .and_then(parse_user_info)
    })
}

fn summarize_response_body(body: &str) -> String {
    let body = body.trim().replace(['\r', '\n', '\t'], " ");
    if body.is_empty() {
        return "empty body".into();
    }
    const MAX: usize = 160;
    if body.chars().count() <= MAX {
        body
    } else {
        let mut output = body.chars().take(MAX).collect::<String>();
        output.push_str("...");
        output
    }
}

fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(b0 >> 2) as usize] as char);
        output.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

pub fn profile_path(paths: &Paths, sub: &Subscription) -> PathBuf {
    paths
        .profiles_dir
        .join(format!("{}.yaml", sanitize(&sub.name)))
}

fn sanitize(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "profile".into()
    } else {
        output
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn parse_user_info(value: &str) -> Option<SubscriptionUserInfo> {
    let mut info = SubscriptionUserInfo::default();
    for item in value.split(';') {
        let Some((key, value)) = item.trim().split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "upload" => info.upload = Some(value),
            "download" => info.download = Some(value),
            "total" => info.total = Some(value),
            "expire" => info.expire = Some(value),
            _ => {}
        }
    }
    (!info.is_empty()).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn parses_subscription_user_info_header() -> Result<()> {
        let info = parse_user_info("upload=10; download=20; total=100; expire=200")
            .ok_or_else(|| anyhow::anyhow!("missing user info"))?;

        assert_eq!(info.upload, Some(10));
        assert_eq!(info.download, Some(20));
        assert_eq!(info.used(), Some(30));
        assert_eq!(info.total, Some(100));
        assert_eq!(info.expire, Some(200));
        Ok(())
    }

    #[test]
    fn parses_provider_prefixed_subscription_user_info_header() -> Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-meta-subscription-userinfo",
            HeaderValue::from_static("upload=10; download=20; total=100; expire=200"),
        );
        let info = parse_user_info_headers(&headers)
            .ok_or_else(|| anyhow::anyhow!("missing user info"))?;

        assert_eq!(info.used(), Some(30));
        assert_eq!(info.total, Some(100));
        Ok(())
    }

    #[test]
    fn subscription_request_mimics_clash_verge_user_agent() {
        assert!(SUBSCRIPTION_USER_AGENT.starts_with("clash-verge/v"));
    }

    #[test]
    fn rejects_plain_text_subscription_body() {
        let err = validate_subscription_profile_body("demo", "暂停支持该协议")
            .expect_err("plain text should be rejected");

        assert!(err.to_string().contains("non-profile content"));
    }

    #[test]
    fn rejects_profile_without_proxies() {
        let err = validate_subscription_profile_body(
            "demo",
            r"
rules:
  - MATCH,DIRECT
",
        )
        .expect_err("profile without proxies should be rejected");

        assert!(err.to_string().contains("without proxies"));
    }

    #[test]
    fn accepts_profile_with_proxies() -> Result<()> {
        validate_subscription_profile_body(
            "demo",
            r"
proxies:
  - name: HK 01
    type: socks5
    server: 127.0.0.1
    port: 1080
proxy-groups:
  - name: Auto
    type: select
    proxies: [HK 01]
rules:
  - MATCH,Auto
",
        )
    }

    #[test]
    fn subscription_request_url_moves_basic_auth_to_header() -> Result<()> {
        let (url, auth) = subscription_request_url("https://user:pass@example.com/a")?;

        assert_eq!(url.as_str(), "https://example.com/a");
        assert_eq!(
            auth.and_then(|value| value.to_str().ok().map(str::to_string)),
            Some("Basic dXNlcjpwYXNz".into())
        );
        Ok(())
    }
}
