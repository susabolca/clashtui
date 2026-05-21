use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::DnsConfig;
use crate::mihomo::MihomoClient;

pub async fn apply(client: &MihomoClient, config: &DnsConfig) -> Result<()> {
    client.patch_configs(&patch(config)).await
}

pub fn patch(config: &DnsConfig) -> Value {
    json!({
        "dns": {
            "enable": config.enable,
            "listen": config.listen,
            "enhanced-mode": config.enhanced_mode,
            "fake-ip-range": config.fake_ip_range,
            "fake-ip-filter-mode": config.fake_ip_filter_mode,
            "prefer-h3": config.prefer_h3,
            "respect-rules": config.respect_rules,
            "use-hosts": config.use_hosts,
            "use-system-hosts": config.use_system_hosts,
            "ipv6": config.ipv6,
            "fake-ip-filter": effective_fake_ip_filter(config),
            "default-nameserver": config.default_nameserver,
            "nameserver-policy": effective_nameserver_policy(config),
            "nameserver": config.nameserver,
            "fallback": config.fallback,
            "proxy-server-nameserver": config.proxy_server_nameserver,
            "direct-nameserver": config.direct_nameserver,
            "direct-nameserver-follow-policy": config.direct_nameserver_follow_policy,
            "fallback-filter": {
                "geoip": true,
                "geoip-code": "CN",
                "ipcidr": ["240.0.0.0/4", "0.0.0.0/32"],
            }
        }
    })
}

pub fn effective_nameserver_policy(config: &DnsConfig) -> BTreeMap<String, Vec<String>> {
    let mut policy = config.nameserver_policy.clone();
    if !config.lan_nameserver.is_empty() {
        for domain in &config.lan_domains {
            if !domain.trim().is_empty() {
                policy.insert(domain.clone(), config.lan_nameserver.clone());
            }
        }
    }
    policy
}

pub fn effective_fake_ip_filter(config: &DnsConfig) -> Vec<String> {
    let mut filter = config.fake_ip_filter.clone();
    for domain in &config.lan_domains {
        if !domain.trim().is_empty() && !filter.iter().any(|item| item == domain) {
            filter.push(domain.clone());
        }
    }
    filter
}
