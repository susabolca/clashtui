use anyhow::Result;
use serde_json::{Value, json};

use crate::config::TunConfig;
use crate::mihomo::MihomoClient;
use crate::platform;

pub async fn apply(client: &MihomoClient, config: &TunConfig) -> Result<()> {
    client.patch_configs(&patch(config)).await
}

pub fn patch(config: &TunConfig) -> Value {
    let config = platform::tun::normalize_config(config);
    let mut tun = serde_json::Map::new();
    tun.insert("enable".into(), json!(config.enable));
    tun.insert("stack".into(), json!(config.stack));
    tun.insert("device".into(), json!(config.device));
    tun.insert("auto-route".into(), json!(config.auto_route));
    if platform::tun::supports_auto_redirect() {
        tun.insert("auto-redirect".into(), json!(config.auto_redirect));
    }
    tun.insert(
        "auto-detect-interface".into(),
        json!(config.auto_detect_interface),
    );
    tun.insert("dns-hijack".into(), json!(config.dns_hijack));
    tun.insert("strict-route".into(), json!(config.strict_route));
    tun.insert("mtu".into(), json!(config.mtu));
    tun.insert(
        "route-exclude-address".into(),
        json!(config.route_exclude_address),
    );

    json!({
        "tun": tun
    })
}
