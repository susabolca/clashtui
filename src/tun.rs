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
    let helper_fd = config.file_descriptor.is_some() && cfg!(target_os = "macos");
    tun.insert("auto-route".into(), json!(config.auto_route && !helper_fd));
    if platform::tun::supports_auto_redirect() {
        tun.insert("auto-redirect".into(), json!(config.auto_redirect));
    }
    tun.insert(
        "auto-detect-interface".into(),
        json!(config.auto_detect_interface && !helper_fd),
    );
    tun.insert("dns-hijack".into(), json!(config.dns_hijack));
    tun.insert("strict-route".into(), json!(config.strict_route));
    tun.insert("mtu".into(), json!(config.mtu));
    tun.insert(
        "route-exclude-address".into(),
        json!(config.route_exclude_address),
    );
    if let Some(file_descriptor) = config.file_descriptor {
        tun.insert("file-descriptor".into(), json!(file_descriptor));
    }

    json!({
        "tun": tun
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_file_descriptor_disables_mihomo_route_setup() {
        let config = TunConfig {
            enable: true,
            file_descriptor: Some(7),
            ..TunConfig::default()
        };

        let patch = patch(&config);
        let tun = &patch["tun"];
        assert_eq!(tun["enable"], true);
        assert_eq!(tun["file-descriptor"], 7);
        assert_eq!(tun["auto-route"], false);
        assert_eq!(tun["auto-detect-interface"], false);
    }
}
