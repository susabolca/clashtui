use std::collections::BTreeSet;
use std::env;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::{Context as _, Result};
use reqwest::{
    Client, Proxy,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::config::{AppConfig, DEFAULT_PAC_RULE_SOURCE_URL, Paths};
use crate::system_proxy;

const PAC_RULE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);
const PAC_RULE_USER_AGENT: &str = concat!("clashtui/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
struct PacServerConfig {
    paths: Paths,
    config: AppConfig,
}

#[derive(Debug, Default)]
pub struct PacServer {
    port: Option<u16>,
    config: Option<Arc<RwLock<PacServerConfig>>>,
    handle: Option<JoinHandle<()>>,
}

impl PacServer {
    pub async fn ensure(&mut self, paths: &Paths, config: &AppConfig) -> Result<()> {
        let next = PacServerConfig {
            paths: paths.clone(),
            config: config.clone(),
        };
        let port = config.system_proxy.pac_port;

        if self.port == Some(port)
            && let Some(shared) = &self.config
        {
            if let Ok(mut current) = shared.write() {
                *current = next;
            }
            return Ok(());
        }

        self.stop();
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .with_context(|| format!("failed to bind PAC server on 127.0.0.1:{port}"))?;
        let shared = Arc::new(RwLock::new(next));
        let server_config = Arc::clone(&shared);
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let request_config = Arc::clone(&server_config);
                        tokio::spawn(async move {
                            let _ = serve_pac_request(stream, request_config).await;
                        });
                    }
                    Err(err) => {
                        eprintln!("pac server: accept failed: {err:#}");
                        break;
                    }
                }
            }
        });

        self.port = Some(port);
        self.config = Some(shared);
        self.handle = Some(handle);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        self.port = None;
        self.config = None;
    }

    pub fn running_port(&self) -> Option<u16> {
        self.port
    }
}

#[derive(Debug, Clone)]
pub struct PacRuleUpdate {
    pub source_url: String,
    pub raw_bytes: usize,
    pub decoded_bytes: usize,
    pub proxy_rule_count: usize,
    pub direct_rule_count: usize,
    pub target_file: PathBuf,
}

#[derive(Debug, Clone)]
struct RuleDownloadAttempt {
    label: &'static str,
    proxy_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedPacRules {
    proxy_rules: Vec<String>,
    direct_rules: Vec<String>,
}

pub async fn update_gfwlist_rules(paths: &Paths, config: &AppConfig) -> Result<PacRuleUpdate> {
    let source_url = config.system_proxy.pac_rule_source_url.trim();
    let source_url = if source_url.is_empty() {
        DEFAULT_PAC_RULE_SOURCE_URL
    } else {
        source_url
    }
    .to_string();
    let body = fetch_rule_source(&source_url, config).await?;
    let raw_bytes = body.len();
    if raw_bytes == 0 {
        anyhow::bail!("PAC rule source returned an empty body: {source_url}");
    }

    let content = decode_gfwlist_body(&body).with_context(|| {
        format!("failed to decode PAC rule source from {source_url} as gfwlist")
    })?;
    let decoded_bytes = content.len();
    let parsed = parse_gfwlist_rules(&content);
    if parsed.proxy_rules.is_empty() && parsed.direct_rules.is_empty() {
        anyhow::bail!("PAC rule source did not contain usable gfwlist rules: {source_url}");
    }
    let proxy_rule_count = parsed.proxy_rules.len();
    let direct_rule_count = parsed.direct_rules.len();
    save_gfwlist_content(paths, &content).await?;

    Ok(PacRuleUpdate {
        source_url,
        raw_bytes,
        decoded_bytes,
        proxy_rule_count,
        direct_rule_count,
        target_file: paths.pac_gfwlist_file.clone(),
    })
}

async fn load_gfwlist_rules(paths: &Paths) -> Result<ParsedPacRules> {
    match load_gfwlist_content(paths).await? {
        Some(content) => Ok(parse_gfwlist_rules(&content)),
        None => Ok(ParsedPacRules::default()),
    }
}

async fn load_gfwlist_content(paths: &Paths) -> Result<Option<String>> {
    match fs::read_to_string(&paths.pac_gfwlist_file).await {
        Ok(content) => Ok(Some(content)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => {
            Err(err).with_context(|| format!("failed to read {}", paths.pac_gfwlist_file.display()))
        }
    }
}

async fn save_gfwlist_content(paths: &Paths, content: &str) -> Result<()> {
    paths.ensure().await?;
    fs::write(&paths.pac_gfwlist_file, content)
        .await
        .with_context(|| format!("failed to write {}", paths.pac_gfwlist_file.display()))
}

async fn fetch_rule_source(url: &str, config: &AppConfig) -> Result<Vec<u8>> {
    let attempts = rule_download_attempts(config);
    let mut errors = Vec::new();
    for attempt in attempts {
        match fetch_rule_source_once(url, &attempt).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => errors.push(format!("{}: {err:#}", attempt.label)),
        }
    }
    anyhow::bail!(
        "failed to download PAC rules from {url}: {}",
        errors.join("; ")
    )
}

async fn fetch_rule_source_once(url: &str, attempt: &RuleDownloadAttempt) -> Result<Vec<u8>> {
    let client = rule_download_client(attempt.proxy_url.as_deref())?;
    let response = client.get(url).send().await.context("request failed")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("failed to read response body")?;
    if !status.is_success() {
        anyhow::bail!("status {status}: {}", summarize_body(&body));
    }
    Ok(body.to_vec())
}

fn rule_download_client(proxy_url: Option<&str>) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(PAC_RULE_USER_AGENT).context("invalid PAC update user agent")?,
    );

    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(None)
        .timeout(PAC_RULE_DOWNLOAD_TIMEOUT)
        .connect_timeout(PAC_RULE_DOWNLOAD_TIMEOUT)
        .default_headers(headers);

    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(Proxy::all(proxy_url)?);
    } else {
        builder = builder.no_proxy();
    }

    Ok(builder.build()?)
}

fn rule_download_attempts(config: &AppConfig) -> Vec<RuleDownloadAttempt> {
    let mut attempts = vec![RuleDownloadAttempt {
        label: "direct",
        proxy_url: None,
    }];

    if let Some(proxy) = local_proxy_from_env() {
        attempts.push(RuleDownloadAttempt {
            label: "environment proxy",
            proxy_url: Some(proxy),
        });
    }

    attempts.push(RuleDownloadAttempt {
        label: "local mixed proxy",
        proxy_url: Some(format!(
            "http://{}:{}",
            proxy_url_host(&config.system_proxy_host()),
            config.mixed_port
        )),
    });

    if let Ok(status) = system_proxy::status()
        && status.enabled
        && !status.server.trim().is_empty()
    {
        attempts.push(RuleDownloadAttempt {
            label: "system proxy",
            proxy_url: Some(format!("http://{}", status.server)),
        });
    }

    let mut seen = BTreeSet::new();
    attempts
        .into_iter()
        .filter(|attempt| seen.insert(attempt.proxy_url.clone().unwrap_or_default()))
        .collect()
}

fn local_proxy_from_env() -> Option<String> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .into_iter()
    .find_map(|key| {
        env::var(key)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn proxy_url_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn decode_gfwlist_body(body: &[u8]) -> Result<String> {
    if let Ok(decoded) = decode_base64(body)
        && let Ok(content) = String::from_utf8(decoded)
        && looks_like_gfwlist(&content)
    {
        return Ok(content);
    }

    String::from_utf8(body.to_vec()).context("PAC rule source is not valid UTF-8")
}

fn looks_like_gfwlist(content: &str) -> bool {
    let content = content.trim_start_matches('\u{feff}').trim_start();
    content.starts_with("[AutoProxy")
        || content.lines().any(|line| {
            let line = line.trim();
            line.starts_with("||")
                || line.starts_with("@@")
                || line.starts_with('|')
                || line.starts_with('!')
        })
}

fn decode_base64(input: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u8;

    for &byte in input {
        let Some(value) = base64_value(byte)? else {
            continue;
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        while bits >= 8 {
            bits -= 8;
            output.push(((buffer >> bits) & 0xff) as u8);
            if bits > 0 {
                buffer &= (1u32 << bits) - 1;
            } else {
                buffer = 0;
            }
        }
    }

    Ok(output)
}

fn base64_value(byte: u8) -> Result<Option<u8>> {
    let value = match byte {
        b'A'..=b'Z' => byte - b'A',
        b'a'..=b'z' => byte - b'a' + 26,
        b'0'..=b'9' => byte - b'0' + 52,
        b'+' | b'-' => 62,
        b'/' | b'_' => 63,
        b'=' | b'\r' | b'\n' | b'\t' | b' ' => return Ok(None),
        _ => anyhow::bail!("invalid base64 byte 0x{byte:02x}"),
    };
    Ok(Some(value))
}

fn parse_gfwlist_rules(content: &str) -> ParsedPacRules {
    let mut parsed = ParsedPacRules::default();
    let mut proxy_seen = BTreeSet::new();
    let mut direct_seen = BTreeSet::new();

    for line in content.lines() {
        let Some((direct, rule)) = parse_gfwlist_line(line) else {
            continue;
        };
        if direct {
            if direct_seen.insert(rule.clone()) {
                parsed.direct_rules.push(rule);
            }
        } else if proxy_seen.insert(rule.clone()) {
            parsed.proxy_rules.push(rule);
        }
    }

    parsed
}

fn parse_gfwlist_line(line: &str) -> Option<(bool, String)> {
    let mut rule = line.trim().trim_start_matches('\u{feff}').trim();
    if rule.is_empty()
        || rule.starts_with('!')
        || rule.starts_with('[')
        || rule.starts_with('#')
        || rule.contains("##")
        || rule.contains("#@#")
    {
        return None;
    }

    let direct = rule.starts_with("@@");
    if direct {
        rule = rule.strip_prefix("@@")?.trim();
    }
    rule = strip_rule_options(rule).trim();
    normalize_gfwlist_rule(rule).map(|rule| (direct, rule))
}

fn strip_rule_options(rule: &str) -> &str {
    if rule.starts_with('/') {
        return rule;
    }
    rule.split_once('$').map_or(rule, |(rule, _)| rule)
}

fn normalize_gfwlist_rule(rule: &str) -> Option<String> {
    let mut rule = rule.trim().to_string();
    if rule.is_empty() || rule == "*" {
        return None;
    }
    while rule.ends_with('^') || (rule.ends_with('|') && !rule.ends_with("||")) {
        rule.pop();
    }
    rule = rule.replace('^', "*");
    if rule.is_empty() || rule == "*" {
        None
    } else {
        Some(rule)
    }
}

fn summarize_body(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body)
        .trim()
        .replace(['\r', '\n', '\t'], " ");
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

async fn serve_pac_request(
    mut stream: TcpStream,
    config: Arc<RwLock<PacServerConfig>>,
) -> Result<()> {
    let mut buffer = [0u8; 2048];
    let bytes = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let path = request_path(&request).unwrap_or("/");

    if path_matches_pac(path) {
        let snapshot = config
            .read()
            .map(|config| config.clone())
            .map_err(|_| anyhow::anyhow!("PAC server config lock poisoned"))?;
        let content = render_pac_response(&snapshot.paths, &snapshot.config).await;
        write_response(
            &mut stream,
            "200 OK",
            "application/x-ns-proxy-autoconfig",
            content.as_bytes(),
        )
        .await?;
    } else {
        write_response(&mut stream, "404 Not Found", "text/plain", b"not found").await?;
    }

    Ok(())
}

async fn render_pac_response(paths: &Paths, config: &AppConfig) -> String {
    let gfwlist = match load_gfwlist_rules(paths).await {
        Ok(rules) => rules,
        Err(err) => {
            eprintln!("pac server: failed to load gfwlist PAC rules: {err:#}");
            ParsedPacRules::default()
        }
    };
    config.rendered_pac_content_with_rules(&gfwlist.proxy_rules, &gfwlist.direct_rules)
}

fn request_path(request: &str) -> Option<&str> {
    let mut parts = request.lines().next()?.split_whitespace();
    let method = parts.next()?;
    if method != "GET" && method != "HEAD" {
        return Some("/");
    }
    parts.next()
}

fn path_matches_pac(path: &str) -> bool {
    matches!(
        path.split_once('?').map_or(path, |(path, _)| path),
        "/commands/pac" | "/pac"
    )
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<()> {
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(body).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_accepts_pac_paths() {
        assert_eq!(
            request_path("GET /commands/pac HTTP/1.1\r\nHost: localhost\r\n\r\n"),
            Some("/commands/pac")
        );
        assert_eq!(request_path("GET /pac HTTP/1.1\r\n\r\n"), Some("/pac"));
        assert!(path_matches_pac("/commands/pac?ts=1"));
    }

    #[test]
    fn decodes_base64_encoded_gfwlist_body() -> Result<()> {
        let content = decode_gfwlist_body(
            b"W0F1dG9Qcm94eSAwLjIuOV0KfHxnb29nbGUuY29tXgpAQHx8ZXhhbXBsZS5jb20K",
        )?;

        assert!(content.contains("[AutoProxy 0.2.9]"));
        assert!(content.contains("||google.com^"));
        Ok(())
    }

    #[test]
    fn parses_gfwlist_proxy_and_direct_rules() {
        let parsed = parse_gfwlist_rules(
            r"[AutoProxy 0.2.9]
! comment
||google.com^
||google.com^
@@||example.com^
||ads.example.com^$script
/regexp.*/
##selector
",
        );

        assert_eq!(
            parsed.proxy_rules,
            vec![
                "||google.com".to_string(),
                "||ads.example.com".to_string(),
                "/regexp.*/".to_string()
            ]
        );
        assert_eq!(parsed.direct_rules, vec!["||example.com".to_string()]);
    }

    #[tokio::test]
    async fn gfwlist_rules_round_trip() -> Result<()> {
        let paths = test_paths("gfwlist-rules-round-trip");
        let _ = fs::remove_dir_all(&paths.config_dir).await;

        save_gfwlist_content(
            &paths,
            "[AutoProxy 0.2.9]\n||proxy.example\n@@||direct.example\n",
        )
        .await?;
        let loaded = load_gfwlist_rules(&paths).await?;

        assert_eq!(loaded.proxy_rules, vec!["||proxy.example".to_string()]);
        assert_eq!(loaded.direct_rules, vec!["||direct.example".to_string()]);
        Ok(())
    }

    fn test_paths(name: &str) -> Paths {
        let root = std::env::temp_dir().join(format!("clashtui-pac-test-{name}"));
        Paths {
            config_dir: root.clone(),
            config_file: root.join("config.yaml"),
            pid_file: root.join("clashtui.pid"),
            core_pid_file: root.join("mihomo.pid"),
            core_config_file: root.join("mihomo-run.yaml"),
            active_config_file: root.join("mihomo-active.yaml"),
            log_file: root.join("clashtui.log"),
            core_log_file: root.join("mihomo.log"),
            llm_api_key_file: root.join("llm-api-key"),
            llm_providers_file: root.join("llm-providers.yaml"),
            pac_gfwlist_file: root.join("gfwlist.txt"),
            profiles_dir: root.join("profiles"),
            cores_dir: root.join("cores"),
        }
    }
}
