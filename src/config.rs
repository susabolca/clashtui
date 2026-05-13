use std::{collections::BTreeMap, env, path::PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

pub const DEFAULT_MIXED_PORT: u16 = 7070;
pub const DEFAULT_CONTROLLER_URL: &str = "http://127.0.0.1:19090";
const LEGACY_DEFAULT_MIXED_PORT: u16 = 7897;
const LEGACY_DEFAULT_CONTROLLER_URLS: [&str; 1] = ["http://127.0.0.1:9097"];
const DEFAULT_DNS_LISTEN: &str = "127.0.0.1:10553";
const LEGACY_DEFAULT_DNS_LISTENS: [&str; 2] = [":53", "127.0.0.1:1053"];

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub pid_file: PathBuf,
    pub core_pid_file: PathBuf,
    pub core_config_file: PathBuf,
    pub active_config_file: PathBuf,
    pub log_file: PathBuf,
    pub core_log_file: PathBuf,
    pub profiles_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub id: String,
    pub label: String,
    pub work_dir: PathBuf,
    pub pid_file: PathBuf,
    pub config_file: PathBuf,
    pub active_config_file: PathBuf,
    pub log_file: PathBuf,
    pub controller_url: String,
}

impl Paths {
    pub fn new() -> Result<Self> {
        let config_dir = config_dir().context("failed to resolve config directory")?;
        let config_file = config_dir.join("config.yaml");
        let pid_file = config_dir.join("clashtui.pid");
        let core_pid_file = config_dir.join("mihomo.pid");
        let core_config_file = config_dir.join("mihomo-run.yaml");
        let active_config_file = config_dir.join("mihomo-active.yaml");
        let log_file = config_dir.join("clashtui.log");
        let core_log_file = config_dir.join("mihomo.log");
        let profiles_dir = config_dir.join("profiles");
        Ok(Self {
            config_dir,
            config_file,
            pid_file,
            core_pid_file,
            core_config_file,
            active_config_file,
            log_file,
            core_log_file,
            profiles_dir,
        })
    }

    pub async fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir).await?;
        fs::create_dir_all(&self.profiles_dir).await?;
        Ok(())
    }

    pub fn global_runtime(&self, controller_url: impl Into<String>) -> RuntimePaths {
        RuntimePaths {
            id: "global".into(),
            label: "Global Proxy".into(),
            work_dir: self.config_dir.clone(),
            pid_file: self.core_pid_file.clone(),
            config_file: self.core_config_file.clone(),
            active_config_file: self.active_config_file.clone(),
            log_file: self.core_log_file.clone(),
            controller_url: controller_url.into(),
        }
    }

    pub fn port_proxy_runtime(
        &self,
        index: usize,
        name: impl Into<String>,
        controller_url: impl Into<String>,
    ) -> RuntimePaths {
        let id = format!("port-proxy-{}", index + 1);
        let work_dir = self.config_dir.join("runtimes").join(&id);
        RuntimePaths {
            id,
            label: name.into(),
            pid_file: work_dir.join("mihomo.pid"),
            config_file: work_dir.join("mihomo-run.yaml"),
            active_config_file: work_dir.join("mihomo-active.yaml"),
            log_file: work_dir.join("mihomo.log"),
            work_dir,
            controller_url: controller_url.into(),
        }
    }
}

fn config_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("CLASHTUI_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }

    if cfg!(target_os = "windows") {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE")
                    .map(|home| PathBuf::from(home).join("AppData").join("Roaming"))
            })
            .map(|base| base.join("clashtui"))
            .context("APPDATA or USERPROFILE is not set")
    } else if cfg!(target_os = "macos") {
        home_dir()
            .map(|home| {
                home.join("Library")
                    .join("Application Support")
                    .join("clashtui")
            })
            .context("HOME is not set")
    } else {
        env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home_dir().map(|home| home.join(".config")))
            .map(|base| base.join("clashtui"))
            .context("XDG_CONFIG_HOME or HOME is not set")
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub core_path: Option<String>,
    pub controller: ControllerConfig,
    pub proxy_host: String,
    pub mixed_port: u16,
    pub proxy_ports: ProxyPortsConfig,
    pub system_proxy: SystemProxyConfig,
    pub tun: TunConfig,
    pub dns: DnsConfig,
    pub port_allocation: PortAllocationConfig,
    pub runtime_mode: String,
    pub proxy_selections: BTreeMap<String, String>,
    pub subscriptions: Vec<Subscription>,
    pub active_profile: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            core_path: None,
            controller: ControllerConfig::default(),
            proxy_host: "127.0.0.1".into(),
            mixed_port: DEFAULT_MIXED_PORT,
            proxy_ports: ProxyPortsConfig::default(),
            system_proxy: SystemProxyConfig::default(),
            tun: TunConfig::default(),
            dns: DnsConfig::default(),
            port_allocation: PortAllocationConfig::default(),
            runtime_mode: "rule".into(),
            proxy_selections: BTreeMap::new(),
            subscriptions: Vec::new(),
            active_profile: None,
        }
    }
}

impl AppConfig {
    pub async fn load_or_init(paths: &Paths) -> Result<Self> {
        paths.ensure().await?;
        if !paths.config_file.exists() {
            let config = Self::default();
            config.save(paths).await?;
            return Ok(config);
        }

        let content = fs::read_to_string(&paths.config_file)
            .await
            .with_context(|| format!("failed to read {}", paths.config_file.display()))?;
        let mut config: Self = serde_yaml_ng::from_str(&content)
            .with_context(|| format!("failed to parse {}", paths.config_file.display()))?;
        let allocation_changed = config.normalize_port_allocation_defaults();
        let legacy_changed = config.migrate_legacy_defaults();
        if allocation_changed || legacy_changed {
            config.save(paths).await?;
        }
        Ok(config)
    }

    pub async fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure().await?;
        let content = serde_yaml_ng::to_string(self)?;
        fs::write(&paths.config_file, content)
            .await
            .with_context(|| format!("failed to write {}", paths.config_file.display()))
    }

    pub fn system_proxy_target(&self) -> SystemProxyTarget {
        let bypass = if self.system_proxy.use_default_bypass && self.system_proxy.bypass.is_empty()
        {
            default_bypass()
        } else if self.system_proxy.use_default_bypass {
            format!("{},{}", default_bypass(), self.system_proxy.bypass)
        } else {
            self.system_proxy.bypass.clone()
        };

        SystemProxyTarget {
            host: self.proxy_host.clone(),
            port: self.mixed_port,
            bypass,
        }
    }

    pub fn proxy_port_summary(&self) -> String {
        let mut parts = vec![format!("mixed={}:{}", self.proxy_host, self.mixed_port)];
        if let Some(port) = self.proxy_ports.http {
            parts.push(format!("http={}:{}", self.proxy_host, port));
        }
        if let Some(port) = self.proxy_ports.socks {
            parts.push(format!("socks={}:{}", self.proxy_host, port));
        }
        for service in self
            .proxy_ports
            .services
            .iter()
            .filter(|service| service.enabled)
        {
            let listen = if service.listen.trim().is_empty() {
                "127.0.0.1"
            } else {
                service.listen.trim()
            };
            let name = if service.name.trim().is_empty() {
                service.kind.trim()
            } else {
                service.name.trim()
            };
            parts.push(format!("{name}={listen}:{}", service.port));
        }
        parts.join(" ")
    }

    fn migrate_legacy_defaults(&mut self) -> bool {
        let controller_changed =
            if LEGACY_DEFAULT_CONTROLLER_URLS.contains(&self.controller.url.as_str()) {
                self.controller.url = DEFAULT_CONTROLLER_URL.into();
                true
            } else {
                false
            };
        let mixed_port_changed = if self.mixed_port == LEGACY_DEFAULT_MIXED_PORT {
            self.mixed_port = DEFAULT_MIXED_PORT;
            true
        } else {
            false
        };
        let dns_listen_changed = if LEGACY_DEFAULT_DNS_LISTENS.contains(&self.dns.listen.as_str()) {
            self.dns.listen = DEFAULT_DNS_LISTEN.into();
            true
        } else {
            false
        };
        controller_changed || mixed_port_changed || dns_listen_changed
    }

    fn normalize_port_allocation_defaults(&mut self) -> bool {
        let mut changed = false;

        if self.port_allocation.auto_mixed {
            self.port_allocation.auto_mixed = false;
            changed = true;
            if self.mixed_port != DEFAULT_MIXED_PORT {
                self.mixed_port = DEFAULT_MIXED_PORT;
            }
        }

        if self.port_allocation.seed.is_some() {
            return changed;
        }

        let controller_changed = self.controller.url != DEFAULT_CONTROLLER_URL
            && !LEGACY_DEFAULT_CONTROLLER_URLS.contains(&self.controller.url.as_str())
            && self.port_allocation.auto_controller;
        if controller_changed {
            self.port_allocation.auto_controller = false;
        }
        let mixed_changed = self.mixed_port != DEFAULT_MIXED_PORT
            && self.mixed_port != LEGACY_DEFAULT_MIXED_PORT
            && self.port_allocation.auto_mixed;
        if mixed_changed {
            self.port_allocation.auto_mixed = false;
        }
        let dns_changed = self.dns.listen != DEFAULT_DNS_LISTEN
            && !LEGACY_DEFAULT_DNS_LISTENS.contains(&self.dns.listen.as_str())
            && self.port_allocation.auto_dns;
        if dns_changed {
            self.port_allocation.auto_dns = false;
        }
        changed || controller_changed || mixed_changed || dns_changed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PortAllocationConfig {
    pub seed: Option<u16>,
    pub auto_controller: bool,
    pub auto_mixed: bool,
    pub auto_dns: bool,
}

impl Default for PortAllocationConfig {
    fn default() -> Self {
        Self {
            seed: None,
            auto_controller: true,
            auto_mixed: false,
            auto_dns: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ControllerConfig {
    pub url: String,
    pub secret: Option<String>,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            url: DEFAULT_CONTROLLER_URL.into(),
            secret: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemProxyConfig {
    pub enabled: bool,
    pub use_default_bypass: bool,
    pub bypass: String,
}

impl Default for SystemProxyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            use_default_bypass: true,
            bypass: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SystemProxyTarget {
    pub host: String,
    pub port: u16,
    pub bypass: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ProxyPortsConfig {
    pub http: Option<u16>,
    pub socks: Option<u16>,
    pub allow_lan: bool,
    pub services: Vec<PortProxyService>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PortProxyService {
    pub enabled: bool,
    pub name: String,
    pub kind: String,
    pub listen: String,
    pub port: u16,
    pub subscription: Option<String>,
    pub mode: String,
    pub proxy: Option<String>,
    pub rule: Option<String>,
    pub rule_selections: BTreeMap<String, String>,
    pub udp: bool,
}

impl Default for PortProxyService {
    fn default() -> Self {
        Self {
            enabled: true,
            name: String::new(),
            kind: "mixed".into(),
            listen: "127.0.0.1".into(),
            port: 0,
            subscription: None,
            mode: "global".into(),
            proxy: None,
            rule: None,
            rule_selections: BTreeMap::new(),
            udp: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TunConfig {
    pub enable: bool,
    pub stack: String,
    pub device: String,
    pub auto_route: bool,
    pub auto_redirect: bool,
    pub auto_detect_interface: bool,
    pub dns_hijack: Vec<String>,
    pub strict_route: bool,
    pub mtu: u16,
    pub route_exclude_address: Vec<String>,
}

impl Default for TunConfig {
    fn default() -> Self {
        Self {
            enable: false,
            stack: "mixed".into(),
            device: default_tun_device(),
            auto_route: true,
            auto_redirect: false,
            auto_detect_interface: true,
            dns_hijack: vec!["any:53".into()],
            strict_route: false,
            mtu: 1500,
            route_exclude_address: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsConfig {
    pub enable: bool,
    pub listen: String,
    pub enhanced_mode: String,
    pub fake_ip_range: String,
    pub fake_ip_filter_mode: String,
    pub ipv6: bool,
    pub prefer_h3: bool,
    pub respect_rules: bool,
    pub use_hosts: bool,
    pub use_system_hosts: bool,
    pub direct_nameserver_follow_policy: bool,
    pub lan_domains: Vec<String>,
    pub lan_nameserver: Vec<String>,
    pub nameserver_policy: BTreeMap<String, Vec<String>>,
    pub default_nameserver: Vec<String>,
    pub nameserver: Vec<String>,
    pub fallback: Vec<String>,
    pub proxy_server_nameserver: Vec<String>,
    pub direct_nameserver: Vec<String>,
    pub fake_ip_filter: Vec<String>,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            enable: false,
            listen: DEFAULT_DNS_LISTEN.into(),
            enhanced_mode: "fake-ip".into(),
            fake_ip_range: "198.18.0.1/16".into(),
            fake_ip_filter_mode: "blacklist".into(),
            ipv6: true,
            prefer_h3: false,
            respect_rules: false,
            use_hosts: false,
            use_system_hosts: false,
            direct_nameserver_follow_policy: false,
            lan_domains: vec!["+.lan".into(), "+.local".into(), "+.arpa".into()],
            lan_nameserver: Vec::new(),
            nameserver_policy: BTreeMap::new(),
            default_nameserver: vec!["system".into(), "223.6.6.6".into(), "8.8.8.8".into()],
            nameserver: vec![
                "8.8.8.8".into(),
                "https://doh.pub/dns-query".into(),
                "https://dns.alidns.com/dns-query".into(),
            ],
            fallback: Vec::new(),
            proxy_server_nameserver: vec![
                "https://doh.pub/dns-query".into(),
                "https://dns.alidns.com/dns-query".into(),
                "tls://223.5.5.5".into(),
            ],
            direct_nameserver: Vec::new(),
            fake_ip_filter: vec![
                "*.lan".into(),
                "*.local".into(),
                "*.arpa".into(),
                "time.*.com".into(),
                "ntp.*.com".into(),
                "+.market.xiaomi.com".into(),
                "localhost.ptlogin2.qq.com".into(),
                "*.msftncsi.com".into(),
                "www.msftconnecttest.com".into(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Subscription {
    pub name: String,
    pub url: String,
    pub refresh: SubscriptionRefresh,
    pub updated_at: Option<String>,
    pub last_error: Option<String>,
    pub user_info: SubscriptionUserInfo,
    pub rule_selections: BTreeMap<String, String>,
}

impl Default for Subscription {
    fn default() -> Self {
        Self {
            name: "default".into(),
            url: String::new(),
            refresh: SubscriptionRefresh::default(),
            updated_at: None,
            last_error: None,
            user_info: SubscriptionUserInfo::default(),
            rule_selections: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SubscriptionUserInfo {
    pub upload: Option<u64>,
    pub download: Option<u64>,
    pub total: Option<u64>,
    pub expire: Option<u64>,
}

impl SubscriptionUserInfo {
    pub fn used(&self) -> Option<u64> {
        match (self.upload, self.download) {
            (Some(upload), Some(download)) => Some(upload.saturating_add(download)),
            (Some(upload), None) => Some(upload),
            (None, Some(download)) => Some(download),
            (None, None) => None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.upload.is_none()
            && self.download.is_none()
            && self.total.is_none()
            && self.expire.is_none()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubscriptionRefresh {
    Disabled,
    Daily,
    #[default]
    Weekly,
}

fn default_tun_device() -> String {
    crate::platform::tun::default_device().into()
}

fn default_bypass() -> String {
    if cfg!(target_os = "windows") {
        "localhost;127.*;192.168.*;10.*;172.16.*;172.17.*;172.18.*;172.19.*;172.20.*;172.21.*;172.22.*;172.23.*;172.24.*;172.25.*;172.26.*;172.27.*;172.28.*;172.29.*;172.30.*;172.31.*;<local>".into()
    } else {
        "localhost,127.0.0.1,192.168.0.0/16,10.0.0.0/8,172.16.0.0/12,::1".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_clash_verge_controller_port() {
        let mut config = AppConfig::default();
        config.controller.url = "http://127.0.0.1:9097".into();
        config.mixed_port = LEGACY_DEFAULT_MIXED_PORT;
        config.dns.listen = ":53".into();

        assert!(!config.normalize_port_allocation_defaults());
        assert!(config.migrate_legacy_defaults());
        assert_eq!(config.controller.url, DEFAULT_CONTROLLER_URL);
        assert_eq!(config.mixed_port, DEFAULT_MIXED_PORT);
        assert_eq!(config.dns.listen, DEFAULT_DNS_LISTEN);
        assert!(config.port_allocation.auto_controller);
        assert!(!config.port_allocation.auto_mixed);
        assert!(config.port_allocation.auto_dns);
    }
}
