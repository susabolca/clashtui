use anyhow::Result;
use serde_json::{Value, json};

use crate::config::TunConfig;
use crate::mihomo::MihomoClient;

pub async fn apply(client: &MihomoClient, config: &TunConfig) -> Result<()> {
    client.patch_configs(&patch(config)).await
}

pub fn patch(config: &TunConfig) -> Value {
    json!({
        "tun": {
            "enable": config.enable,
            "stack": config.stack,
            "device": config.device,
            "auto-route": config.auto_route,
            "auto-redirect": config.auto_redirect,
            "auto-detect-interface": config.auto_detect_interface,
            "dns-hijack": config.dns_hijack,
            "strict-route": config.strict_route,
            "mtu": config.mtu,
            "route-exclude-address": config.route_exclude_address,
        }
    })
}
