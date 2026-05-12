use anyhow::{Context as _, Result};
use serde_yaml_ng::{Mapping, Value};
use tokio::fs;

use crate::config::{AppConfig, Paths, PortProxyService, Subscription};
use crate::{dns, subscription, tun};

pub async fn write_bootstrap_config(paths: &Paths, config: &AppConfig) -> Result<()> {
    paths.ensure().await?;
    let mut value = Value::Mapping(empty_profile());
    apply_overrides(&mut value, config)?;
    let content = serde_yaml_ng::to_string(&value)?;
    fs::write(&paths.core_config_file, content)
        .await
        .with_context(|| format!("failed to write {}", paths.core_config_file.display()))
}

pub async fn write_active_config(
    paths: &Paths,
    config: &AppConfig,
    sub: &Subscription,
) -> Result<std::path::PathBuf> {
    paths.ensure().await?;
    let profile = subscription::profile_path(paths, sub);
    let content = fs::read_to_string(&profile)
        .await
        .with_context(|| format!("failed to read {}", profile.display()))?;
    let mut value: Value = serde_yaml_ng::from_str(&content)
        .with_context(|| format!("failed to parse {}", profile.display()))?;
    apply_overrides(&mut value, config)?;
    let content = serde_yaml_ng::to_string(&value)?;
    fs::write(&paths.active_config_file, content)
        .await
        .with_context(|| format!("failed to write {}", paths.active_config_file.display()))?;
    Ok(paths.active_config_file.clone())
}

pub async fn write_current_config(paths: &Paths, config: &AppConfig) -> Result<std::path::PathBuf> {
    let Some(active_profile) = config.active_profile.as_deref() else {
        write_bootstrap_config(paths, config).await?;
        return Ok(paths.core_config_file.clone());
    };

    let sub = config
        .subscriptions
        .iter()
        .find(|sub| sub.name == active_profile)
        .with_context(|| format!("active profile not found: {active_profile}"))?;
    write_active_config(paths, config, sub).await
}

fn empty_profile() -> Mapping {
    let mut mapping = Mapping::new();
    mapping.insert("proxies".into(), Vec::<Value>::new().into());
    mapping.insert("proxy-groups".into(), Vec::<Value>::new().into());
    mapping.insert("rules".into(), Vec::<Value>::new().into());
    mapping
}

fn apply_overrides(value: &mut Value, config: &AppConfig) -> Result<()> {
    let mapping = value
        .as_mapping_mut()
        .context("mihomo profile root must be a YAML mapping")?;

    remove_unmanaged_inbounds(mapping);
    mapping.insert("mixed-port".into(), config.mixed_port.into());
    if let Some(port) = config.proxy_ports.http {
        mapping.insert("port".into(), port.into());
    }
    if let Some(port) = config.proxy_ports.socks {
        mapping.insert("socks-port".into(), port.into());
    }
    insert_port_proxy_listeners(mapping, config)?;
    mapping.insert(
        "external-controller".into(),
        controller_addr(&config.controller.url).into(),
    );
    mapping.insert("mode".into(), config.runtime_mode.clone().into());
    mapping.insert("log-level".into(), "info".into());
    mapping.insert("allow-lan".into(), config.proxy_ports.allow_lan.into());
    mapping.insert("ipv6".into(), true.into());
    mapping.insert("unified-delay".into(), true.into());

    if let Some(secret) = config
        .controller
        .secret
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        mapping.insert("secret".into(), secret.into());
    } else {
        mapping.remove("secret");
    }

    insert_json_patch(mapping, tun::patch(&config.tun))?;
    insert_json_patch(mapping, dns::patch(&config.dns))?;

    Ok(())
}

fn insert_port_proxy_listeners(mapping: &mut Mapping, config: &AppConfig) -> Result<()> {
    let mut listeners = Vec::new();
    for service in &config.proxy_ports.services {
        if let Some(listener) = listener_from_service(service)? {
            listeners.push(listener);
        }
    }
    if !listeners.is_empty() {
        mapping.insert("listeners".into(), listeners.into());
    }
    Ok(())
}

fn listener_from_service(service: &PortProxyService) -> Result<Option<Value>> {
    if !service.enabled {
        return Ok(None);
    }
    if service.port == 0 {
        anyhow::bail!("port proxy service has invalid port: {}", service.name);
    }

    let kind = service.kind.trim().to_ascii_lowercase();
    if !matches!(kind.as_str(), "http" | "socks" | "mixed") {
        anyhow::bail!(
            "port proxy service {} has unsupported kind {}; expected http, socks, or mixed",
            service.name,
            service.kind
        );
    }

    let listen = if service.listen.trim().is_empty() {
        "127.0.0.1"
    } else {
        service.listen.trim()
    };
    let name = if service.name.trim().is_empty() {
        format!("{kind}-{}", service.port)
    } else {
        service.name.trim().to_string()
    };

    let mut listener = Mapping::new();
    listener.insert("name".into(), name.into());
    listener.insert("type".into(), kind.clone().into());
    listener.insert("port".into(), service.port.into());
    listener.insert("listen".into(), listen.into());
    if kind != "http" {
        listener.insert("udp".into(), service.udp.into());
    }
    if let Some(proxy) = service
        .proxy
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        listener.insert("proxy".into(), proxy.into());
    }
    if let Some(rule) = service
        .rule
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        listener.insert("rule".into(), rule.into());
    }

    Ok(Some(Value::Mapping(listener)))
}

fn remove_unmanaged_inbounds(mapping: &mut Mapping) {
    for key in [
        "port",
        "socks-port",
        "redir-port",
        "tproxy-port",
        "mixed-port",
        "authentication",
        "listeners",
    ] {
        mapping.remove(key);
    }
}

fn insert_json_patch(mapping: &mut Mapping, patch: serde_json::Value) -> Result<()> {
    let patch =
        serde_yaml_ng::to_value(patch).context("failed to convert runtime patch to YAML")?;
    let Value::Mapping(patch) = patch else {
        anyhow::bail!("runtime patch must be a YAML mapping");
    };

    for (key, value) in patch {
        mapping.insert(key, value);
    }
    Ok(())
}

fn controller_addr(url: &str) -> String {
    url.trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_overrides_keeps_subscription_sections_and_adds_dns_tun() -> Result<()> {
        let mut profile: Value = serde_yaml_ng::from_str(
            r"
proxies:
  - name: test
    type: http
    server: 127.0.0.1
    port: 8080
proxy-groups:
  - name: GLOBAL
    type: select
    proxies: [test]
rules:
  - MATCH,GLOBAL
secret: old
port: 7890
socks-port: 7891
redir-port: 7892
listeners:
  - name: sub-tun
    type: tun
    port: 19999
",
        )?;
        let mut config = AppConfig {
            mixed_port: 7070,
            runtime_mode: "global".into(),
            ..AppConfig::default()
        };
        config.proxy_ports.http = Some(18080);
        config.proxy_ports.socks = Some(18081);
        config.proxy_ports.allow_lan = true;
        config.proxy_ports.services.push(PortProxyService {
            name: "hk-mixed".into(),
            kind: "mixed".into(),
            listen: "127.0.0.1".into(),
            port: 18082,
            proxy: Some("GLOBAL".into()),
            rule: None,
            udp: true,
            enabled: true,
        });
        config.tun.enable = true;
        config.dns.enable = true;
        config.controller.secret = None;

        apply_overrides(&mut profile, &config)?;
        let mapping = profile
            .as_mapping()
            .context("profile root is not a mapping")?;

        assert_eq!(
            mapping.get("mixed-port").and_then(Value::as_i64),
            Some(7070)
        );
        assert_eq!(mapping.get("port").and_then(Value::as_i64), Some(18080));
        assert_eq!(
            mapping.get("socks-port").and_then(Value::as_i64),
            Some(18081)
        );
        assert_eq!(mapping.get("mode").and_then(Value::as_str), Some("global"));
        assert!(mapping.get("redir-port").is_none());
        assert_eq!(
            mapping.get("allow-lan").and_then(Value::as_bool),
            Some(true)
        );
        let listeners = mapping
            .get("listeners")
            .and_then(Value::as_sequence)
            .context("listeners missing")?;
        assert_eq!(listeners.len(), 1);
        let listener = listeners[0]
            .as_mapping()
            .context("listener is not a mapping")?;
        assert_eq!(
            listener.get("name").and_then(Value::as_str),
            Some("hk-mixed")
        );
        assert_eq!(listener.get("type").and_then(Value::as_str), Some("mixed"));
        assert_eq!(listener.get("port").and_then(Value::as_i64), Some(18082));
        assert_eq!(
            listener.get("proxy").and_then(Value::as_str),
            Some("GLOBAL")
        );
        assert!(mapping.get("proxies").is_some());
        assert!(mapping.get("secret").is_none());
        assert_eq!(
            mapping
                .get("tun")
                .and_then(Value::as_mapping)
                .and_then(|tun| tun.get("enable"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            mapping
                .get("dns")
                .and_then(Value::as_mapping)
                .and_then(|dns| dns.get("enable"))
                .and_then(Value::as_bool),
            Some(true)
        );
        Ok(())
    }
}
