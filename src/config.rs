use std::{collections::BTreeMap, env, path::PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

pub const DEFAULT_MIXED_PORT: u16 = 7070;
pub const DEFAULT_CONTROLLER_URL: &str = "http://127.0.0.1:19090";
pub const DEFAULT_RUNTIME_BACKEND: &str = "service";
pub const DEFAULT_LLM_BASE_URL: &str = "https://api.deepseek.com";
pub const DEFAULT_LLM_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
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
    pub llm_api_key_file: PathBuf,
    pub llm_providers_file: PathBuf,
    pub profiles_dir: PathBuf,
    pub cores_dir: PathBuf,
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
        let llm_api_key_file = config_dir.join("llm-api-key");
        let llm_providers_file = config_dir.join("llm-providers.yaml");
        let profiles_dir = config_dir.join("profiles");
        let cores_dir = config_dir.join("cores");
        Ok(Self {
            config_dir,
            config_file,
            pid_file,
            core_pid_file,
            core_config_file,
            active_config_file,
            log_file,
            core_log_file,
            llm_api_key_file,
            llm_providers_file,
            profiles_dir,
            cores_dir,
        })
    }

    pub async fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir).await?;
        fs::create_dir_all(&self.profiles_dir).await?;
        fs::create_dir_all(&self.cores_dir).await?;
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
    pub mihomo: MihomoConfig,
    pub core_path: Option<String>,
    pub controller: ControllerConfig,
    pub proxy_host: String,
    pub mixed_port: u16,
    pub proxy_ports: ProxyPortsConfig,
    pub system_proxy: SystemProxyConfig,
    pub tun: TunConfig,
    pub dns: DnsConfig,
    pub llm: LlmConfig,
    pub autostart: AutostartConfig,
    pub port_allocation: PortAllocationConfig,
    pub runtime_backend: String,
    #[serde(default = "default_proxy_profiles")]
    pub proxy_profiles: Vec<ProxyProfile>,
    #[serde(default = "default_active_proxy_profile")]
    pub active_proxy_profile: String,
    pub runtime_mode: String,
    pub proxy_selections: BTreeMap<String, String>,
    pub subscriptions: Vec<Subscription>,
    pub active_profile: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mihomo: MihomoConfig::default(),
            core_path: None,
            controller: ControllerConfig::default(),
            proxy_host: "127.0.0.1".into(),
            mixed_port: DEFAULT_MIXED_PORT,
            proxy_ports: ProxyPortsConfig::default(),
            system_proxy: SystemProxyConfig::default(),
            tun: TunConfig::default(),
            dns: DnsConfig::default(),
            llm: LlmConfig::default(),
            autostart: AutostartConfig::default(),
            port_allocation: PortAllocationConfig::default(),
            runtime_backend: DEFAULT_RUNTIME_BACKEND.into(),
            proxy_profiles: default_proxy_profiles(),
            active_proxy_profile: default_active_proxy_profile(),
            runtime_mode: "rule".into(),
            proxy_selections: BTreeMap::new(),
            subscriptions: Vec::new(),
            active_profile: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MihomoConfig {
    pub core: String,
    pub update: String,
}

impl Default for MihomoConfig {
    fn default() -> Self {
        Self {
            core: "auto".into(),
            update: "manual".into(),
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
        let profile_changed = config.normalize_proxy_profiles();
        if allocation_changed || legacy_changed || profile_changed {
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

    pub fn use_single_runtime(&self) -> bool {
        !matches!(
            self.runtime_backend.trim().to_ascii_lowercase().as_str(),
            "legacy" | "multi" | "multi-process"
        )
    }

    pub fn use_service_runtime(&self) -> bool {
        matches!(
            self.runtime_backend.trim().to_ascii_lowercase().as_str(),
            "" | "service"
        )
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

    pub fn normalize_proxy_profiles(&mut self) -> bool {
        let mut changed = false;
        if self.proxy_profiles.is_empty() {
            self.proxy_profiles
                .push(ProxyProfile::from_global_config("default", self));
            self.active_proxy_profile = "default".into();
            return true;
        }

        let has_legacy_global_settings = self.active_profile.is_some()
            || self.runtime_mode != "rule"
            || !self.proxy_selections.is_empty();
        if self.proxy_profiles.len() == 1
            && self.proxy_profiles[0] == ProxyProfile::default()
            && self.active_proxy_profile == "default"
            && has_legacy_global_settings
        {
            self.proxy_profiles[0] = ProxyProfile::from_global_config("default", self);
            changed = true;
        }

        for (index, profile) in self.proxy_profiles.iter_mut().enumerate() {
            if profile.name.trim().is_empty() {
                profile.name = if index == 0 {
                    "default".into()
                } else {
                    format!("profile {}", index + 1)
                };
                changed = true;
            }
            if !is_proxy_profile_mode(&profile.mode) {
                profile.mode = "rule".into();
                changed = true;
            }
        }

        if self.active_proxy_profile.trim().is_empty()
            || !self
                .proxy_profiles
                .iter()
                .any(|profile| profile.name == self.active_proxy_profile)
        {
            self.active_proxy_profile = self
                .proxy_profiles
                .first()
                .map(|profile| profile.name.clone())
                .unwrap_or_else(default_active_proxy_profile);
            changed = true;
        }

        changed |= self.apply_active_proxy_profile();
        changed
    }

    pub fn active_proxy_profile(&self) -> Option<&ProxyProfile> {
        self.proxy_profiles
            .iter()
            .find(|profile| profile.name == self.active_proxy_profile)
            .or_else(|| self.proxy_profiles.first())
    }

    pub fn activate_proxy_profile(&mut self, name: &str) -> bool {
        if !self
            .proxy_profiles
            .iter()
            .any(|profile| profile.name == name)
        {
            return false;
        }
        self.active_proxy_profile = name.to_string();
        self.apply_active_proxy_profile();
        true
    }

    pub fn apply_active_proxy_profile(&mut self) -> bool {
        let Some(profile) = self.active_proxy_profile().cloned() else {
            return false;
        };
        let mut changed = false;
        if self.active_profile != profile.subscription {
            self.active_profile = profile.subscription.clone();
            changed = true;
        }
        if self.runtime_mode != profile.mode {
            self.runtime_mode = profile.mode.clone();
            changed = true;
        }
        let mut selections = profile.rule_selections.clone();
        if let Some(proxy) = profile.proxy.clone().filter(|value| !value.is_empty()) {
            selections.insert("GLOBAL".into(), proxy);
        }
        if self.proxy_selections != selections {
            self.proxy_selections = selections;
            changed = true;
        }
        changed
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyProfile {
    pub name: String,
    pub subscription: Option<String>,
    pub mode: String,
    pub proxy: Option<String>,
    pub rule_selections: BTreeMap<String, String>,
}

impl Default for ProxyProfile {
    fn default() -> Self {
        Self {
            name: "default".into(),
            subscription: None,
            mode: "rule".into(),
            proxy: None,
            rule_selections: BTreeMap::new(),
        }
    }
}

impl ProxyProfile {
    pub fn from_global_config(name: impl Into<String>, config: &AppConfig) -> Self {
        let proxy = config
            .proxy_selections
            .get("GLOBAL")
            .cloned()
            .or_else(|| config.proxy_selections.values().next().cloned());
        Self {
            name: name.into(),
            subscription: config.active_profile.clone(),
            mode: config.runtime_mode.clone(),
            proxy,
            rule_selections: config
                .proxy_selections
                .iter()
                .filter(|(group, _)| group.as_str() != "GLOBAL")
                .map(|(group, proxy)| (group.clone(), proxy.clone()))
                .collect(),
        }
    }
}

fn default_proxy_profiles() -> Vec<ProxyProfile> {
    vec![ProxyProfile::default()]
}

fn default_active_proxy_profile() -> String {
    "default".into()
}

fn is_proxy_profile_mode(mode: &str) -> bool {
    matches!(
        mode.trim().to_ascii_lowercase().as_str(),
        "rule" | "global" | "direct"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub api_key_file: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "deepseek".into(),
            base_url: DEFAULT_LLM_BASE_URL.into(),
            model: "deepseek-v4-flash".into(),
            api_key_env: DEFAULT_LLM_API_KEY_ENV.into(),
            api_key_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AutostartConfig {
    pub enabled: bool,
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

    #[test]
    fn proxy_profile_migration_preserves_legacy_global_settings() {
        let mut config = AppConfig::default();
        config.active_profile = Some("work".into());
        config.runtime_mode = "global".into();
        config
            .proxy_selections
            .insert("GLOBAL".into(), "HK 01".into());

        assert!(config.normalize_proxy_profiles());

        let profile = config.active_proxy_profile().expect("active profile");
        assert_eq!(profile.name, "default");
        assert_eq!(profile.subscription.as_deref(), Some("work"));
        assert_eq!(profile.mode, "global");
        assert_eq!(profile.proxy.as_deref(), Some("HK 01"));
        assert_eq!(config.active_profile.as_deref(), Some("work"));
        assert_eq!(config.runtime_mode, "global");
    }

    #[test]
    fn activating_proxy_profile_materializes_global_runtime_fields() {
        let mut config = AppConfig::default();
        config.proxy_profiles.push(ProxyProfile {
            name: "work".into(),
            subscription: Some("sub-a".into()),
            mode: "global".into(),
            proxy: Some("JP 01".into()),
            rule_selections: BTreeMap::new(),
        });

        assert!(config.activate_proxy_profile("work"));

        assert_eq!(config.active_proxy_profile, "work");
        assert_eq!(config.active_profile.as_deref(), Some("sub-a"));
        assert_eq!(config.runtime_mode, "global");
        assert_eq!(
            config.proxy_selections.get("GLOBAL").map(String::as_str),
            Some("JP 01")
        );
    }
}
