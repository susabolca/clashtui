use std::collections::BTreeSet;
use std::env;
use std::time::Duration;

use anyhow::{Context as _, Result};
use reqwest::{Client, Proxy};

use crate::config::AppConfig;
use crate::system_proxy;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(20);
const PROXY_ENV_KEYS: [&str; 6] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "ALL_PROXY",
    "all_proxy",
];

#[derive(Debug, Clone)]
pub struct DownloadAttempt {
    pub label: &'static str,
    pub proxy_url: Option<String>,
}

pub fn attempts(config: &AppConfig) -> Vec<DownloadAttempt> {
    let environment_proxies = PROXY_ENV_KEYS
        .into_iter()
        .filter_map(|key| env::var(key).ok());
    let system_proxy = system_proxy::status().ok().and_then(|status| {
        (status.enabled && !status.server.trim().is_empty())
            .then(|| format!("http://{}", status.server))
    });
    build_attempts(config, environment_proxies, system_proxy)
}

fn build_attempts(
    config: &AppConfig,
    environment_proxies: impl IntoIterator<Item = String>,
    system_proxy: Option<String>,
) -> Vec<DownloadAttempt> {
    let mut attempts = Vec::new();
    for proxy in environment_proxies {
        let proxy = proxy.trim();
        if !proxy.is_empty() {
            attempts.push(DownloadAttempt {
                label: "environment proxy",
                proxy_url: Some(proxy.to_string()),
            });
        }
    }

    let host = config.system_proxy_host();
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    attempts.push(DownloadAttempt {
        label: "local mixed proxy",
        proxy_url: Some(format!("http://{host}:{}", config.mixed_port)),
    });
    if let Some(proxy_url) = system_proxy {
        attempts.push(DownloadAttempt {
            label: "system proxy",
            proxy_url: Some(proxy_url),
        });
    }
    attempts.push(DownloadAttempt {
        label: "direct",
        proxy_url: None,
    });

    let mut seen = BTreeSet::new();
    attempts
        .into_iter()
        .filter(|attempt| seen.insert(attempt.proxy_url.clone()))
        .collect()
}

pub fn client(user_agent: &str, timeout: Duration, proxy_url: Option<&str>) -> Result<Client> {
    let mut builder = Client::builder()
        .user_agent(user_agent)
        .redirect(reqwest::redirect::Policy::limited(10))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(None)
        .timeout(timeout)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT);

    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(Proxy::all(proxy_url).context("invalid download proxy")?);
    } else {
        builder = builder.no_proxy();
    }
    builder.build().context("failed to build download client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn proxies_precede_direct_and_duplicates_are_removed() {
        let config = AppConfig {
            proxy_host: "0.0.0.0".into(),
            mixed_port: 7070,
            ..AppConfig::default()
        };
        let attempts = build_attempts(
            &config,
            [
                " ".into(),
                " socks5h://127.0.0.1:1080 ".into(),
                "socks5h://127.0.0.1:1080".into(),
            ],
            Some("http://127.0.0.1:7070".into()),
        );

        let proxies = attempts
            .iter()
            .map(|attempt| attempt.proxy_url.as_deref())
            .collect::<Vec<_>>();
        assert_eq!(
            proxies,
            [
                Some("socks5h://127.0.0.1:1080"),
                Some("http://127.0.0.1:7070"),
                None
            ]
        );
        assert_eq!(attempts[0].label, "environment proxy");
        assert_eq!(attempts[1].label, "local mixed proxy");
        assert_eq!(attempts[2].label, "direct");
    }

    #[test]
    fn local_proxy_uses_loopback_for_wildcard_listeners_and_brackets_ipv6() {
        for (host, expected) in [
            ("0.0.0.0", "http://127.0.0.1:7070"),
            ("::", "http://127.0.0.1:7070"),
            ("*", "http://127.0.0.1:7070"),
            ("::1", "http://[::1]:7070"),
            ("[::1]", "http://[::1]:7070"),
        ] {
            let config = AppConfig {
                proxy_host: host.into(),
                mixed_port: 7070,
                ..AppConfig::default()
            };
            let attempts = build_attempts(&config, [], None);
            assert_eq!(attempts[0].proxy_url.as_deref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn download_client_uses_proxy_for_redirects() -> Result<()> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let proxy_url = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in [
                "HTTP/1.1 302 Found\r\nLocation: http://asset.invalid/file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\narchive",
            ] {
                let (mut stream, _) = listener.accept().await?;
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                while !request.ends_with(b"\r\n\r\n") {
                    let read = stream.read(&mut buffer).await?;
                    anyhow::ensure!(read > 0, "proxy request ended before headers");
                    request.extend_from_slice(&buffer[..read]);
                }
                requests.push(String::from_utf8(request)?);
                stream.write_all(response.as_bytes()).await?;
            }
            Ok::<_, anyhow::Error>(requests)
        });

        let body = client("download-test", Duration::from_secs(2), Some(&proxy_url))?
            .get("http://release.invalid/version")
            .send()
            .await?
            .text()
            .await?;
        assert_eq!(body, "archive");
        let requests = tokio::time::timeout(Duration::from_secs(2), server).await???;
        assert!(requests[0].starts_with("GET http://release.invalid/version HTTP/1.1"));
        assert!(requests[1].starts_with("GET http://asset.invalid/file HTTP/1.1"));
        assert!(
            requests
                .iter()
                .all(|request| request.contains("user-agent: download-test"))
        );
        Ok(())
    }
}
