use anyhow::{Context as _, Result};
use serde_yaml_ng::{Mapping, Value};
use tokio::fs;

use crate::config::{AppConfig, Paths, PortProxyService, RuntimePaths, Subscription};
use crate::{dns, subscription, tun};

const CHECK_PROVIDER_PREFIX: &str = "__clashtui_check_";

pub async fn write_bootstrap_config(paths: &Paths, config: &AppConfig) -> Result<()> {
    paths.ensure().await?;
    let mut value = Value::Mapping(empty_profile());
    insert_subscription_check_providers(paths, config, &mut value).await?;
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
    insert_subscription_check_providers(paths, config, &mut value).await?;
    apply_overrides(&mut value, config)?;
    let content = serde_yaml_ng::to_string(&value)?;
    fs::write(&paths.active_config_file, content)
        .await
        .with_context(|| format!("failed to write {}", paths.active_config_file.display()))?;
    Ok(paths.active_config_file.clone())
}

pub async fn write_service_config(
    paths: &Paths,
    instance: &RuntimePaths,
    config: &AppConfig,
    service: &PortProxyService,
) -> Result<std::path::PathBuf> {
    fs::create_dir_all(&instance.work_dir)
        .await
        .with_context(|| format!("failed to create {}", instance.work_dir.display()))?;

    let runtime_config = service_runtime_config(config, &instance.controller_url, service);

    let mut value = match runtime_config.active_profile.as_deref() {
        Some(profile_name) => {
            let sub = runtime_config
                .subscriptions
                .iter()
                .find(|sub| sub.name == profile_name)
                .with_context(|| format!("service profile not found: {profile_name}"))?;
            read_subscription_profile(paths, sub).await?
        }
        None => Value::Mapping(empty_profile()),
    };

    apply_overrides(&mut value, &runtime_config)?;
    let mapping = value
        .as_mapping_mut()
        .context("mihomo profile root must be a YAML mapping")?;
    insert_service_listener(mapping, service)?;

    let content = serde_yaml_ng::to_string(&value)?;
    fs::write(&instance.active_config_file, &content)
        .await
        .with_context(|| format!("failed to write {}", instance.active_config_file.display()))?;
    fs::write(&instance.config_file, content)
        .await
        .with_context(|| format!("failed to write {}", instance.config_file.display()))?;
    Ok(instance.config_file.clone())
}

fn service_runtime_config(
    config: &AppConfig,
    controller_url: &str,
    service: &PortProxyService,
) -> AppConfig {
    let mut runtime_config = config.clone();
    runtime_config.controller.url = controller_url.to_string();
    runtime_config.active_profile = service
        .subscription
        .clone()
        .or_else(|| config.active_profile.clone());
    runtime_config.runtime_mode.clone_from(&service.mode);
    runtime_config.system_proxy.enabled = false;
    runtime_config.tun.enable = false;
    runtime_config.dns.enable = false;
    runtime_config.mixed_port = 0;
    runtime_config.proxy_ports.http = None;
    runtime_config.proxy_ports.socks = None;
    runtime_config.proxy_ports.services.clear();
    runtime_config
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

pub async fn write_single_runtime_config(
    paths: &Paths,
    config: &AppConfig,
) -> Result<std::path::PathBuf> {
    let mut value = match config.active_profile.as_deref() {
        Some(profile_name) => {
            let sub = config
                .subscriptions
                .iter()
                .find(|sub| sub.name == profile_name)
                .with_context(|| format!("active profile not found: {profile_name}"))?;
            read_subscription_profile(paths, sub).await?
        }
        None => Value::Mapping(empty_profile()),
    };

    merge_single_runtime_port_profiles(paths, config, &mut value).await?;
    insert_subscription_check_providers(paths, config, &mut value).await?;
    value = build_single_runtime_config(config, value)?;
    let content = serde_yaml_ng::to_string(&value)?;
    fs::write(&paths.active_config_file, &content)
        .await
        .with_context(|| format!("failed to write {}", paths.active_config_file.display()))?;
    fs::write(&paths.core_config_file, content)
        .await
        .with_context(|| format!("failed to write {}", paths.core_config_file.display()))?;
    Ok(paths.core_config_file.clone())
}

fn build_single_runtime_config(config: &AppConfig, mut value: Value) -> Result<Value> {
    apply_overrides(&mut value, config)?;
    let mapping = value
        .as_mapping_mut()
        .context("mihomo profile root must be a YAML mapping")?;
    insert_single_runtime_listeners(mapping, &config.proxy_ports.services)?;
    Ok(value)
}

pub fn subscription_check_provider_name(subscription: &Subscription) -> String {
    format!(
        "{CHECK_PROVIDER_PREFIX}{:016x}",
        stable_hash(subscription.name.as_bytes())
    )
}

async fn insert_subscription_check_providers(
    paths: &Paths,
    config: &AppConfig,
    target: &mut Value,
) -> Result<()> {
    for subscription in &config.subscriptions {
        let Ok(profile) = read_subscription_profile(paths, subscription).await else {
            continue;
        };
        insert_subscription_check_provider(target, subscription, &profile)?;
    }
    Ok(())
}

fn insert_subscription_check_provider(
    target: &mut Value,
    subscription: &Subscription,
    source: &Value,
) -> Result<()> {
    let Some(proxies) = source
        .as_mapping()
        .and_then(|mapping| mapping.get("proxies"))
        .and_then(Value::as_sequence)
        .filter(|proxies| !proxies.is_empty())
    else {
        return Ok(());
    };

    let mapping = target
        .as_mapping_mut()
        .context("mihomo profile root must be a YAML mapping")?;
    let providers_key = Value::from("proxy-providers");
    if !mapping.contains_key(&providers_key) {
        mapping.insert(providers_key.clone(), Value::Mapping(Mapping::new()));
    }
    let providers = mapping
        .get_mut(&providers_key)
        .and_then(Value::as_mapping_mut)
        .context("mihomo profile proxy-providers must be a YAML mapping")?;

    let mut provider = Mapping::new();
    provider.insert("type".into(), "inline".into());
    provider.insert("payload".into(), proxies.clone().into());
    providers.insert(
        subscription_check_provider_name(subscription).into(),
        Value::Mapping(provider),
    );
    Ok(())
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

async fn merge_single_runtime_port_profiles(
    paths: &Paths,
    config: &AppConfig,
    target: &mut Value,
) -> Result<()> {
    let active_profile = config.active_profile.as_deref();
    for (index, service) in config
        .proxy_ports
        .services
        .iter()
        .enumerate()
        .filter(|(_, service)| service.enabled)
    {
        let Some(subscription) = service
            .subscription
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        if Some(subscription) == active_profile {
            continue;
        }
        let sub = config
            .subscriptions
            .iter()
            .find(|sub| sub.name == subscription)
            .with_context(|| format!("port proxy subscription not found: {subscription}"))?;
        let source = read_subscription_profile(paths, sub).await?;
        merge_port_proxy_profile(target, &source, index, service)
            .with_context(|| format!("failed to merge port proxy profile {}", service.name))?;
    }
    Ok(())
}

fn merge_port_proxy_profile(
    target: &mut Value,
    source: &Value,
    _index: usize,
    service: &PortProxyService,
) -> Result<()> {
    if service.mode.eq_ignore_ascii_case("global")
        && let Some(proxy) = service
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        let mut seen = Vec::new();
        merge_proxy_or_group(target, source, proxy, &mut seen)?;
    }

    if service.mode.eq_ignore_ascii_case("rule")
        && let Some(rule) = service
            .rule
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        merge_named_map_entry(target, source, "sub-rules", rule)?;
    }

    Ok(())
}

fn merge_proxy_or_group(
    target: &mut Value,
    source: &Value,
    name: &str,
    seen: &mut Vec<String>,
) -> Result<()> {
    if matches!(
        name,
        "DIRECT" | "REJECT" | "REJECT-DROP" | "PASS" | "COMPATIBLE"
    ) {
        return Ok(());
    }
    if seen.iter().any(|item| item == name) {
        return Ok(());
    }
    seen.push(name.to_string());

    if let Some(proxy) = find_named_sequence_item(source, "proxies", name) {
        append_named_sequence_item(target, "proxies", proxy, name)?;
        return Ok(());
    }

    if let Some(group) = find_named_sequence_item(source, "proxy-groups", name) {
        for proxy in group
            .as_mapping()
            .and_then(|mapping| mapping.get("proxies"))
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            merge_proxy_or_group(target, source, proxy, seen)?;
        }
        for provider in group
            .as_mapping()
            .and_then(|mapping| mapping.get("use"))
            .and_then(Value::as_sequence)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            merge_named_map_entry(target, source, "proxy-providers", provider)?;
        }
        append_named_sequence_item(target, "proxy-groups", group, name)?;
        return Ok(());
    }

    if target_has_proxy_or_group(target, name) {
        return Ok(());
    }

    anyhow::bail!("proxy or group not found in port proxy subscription: {name}")
}

fn find_named_sequence_item(value: &Value, key: &str, name: &str) -> Option<Value> {
    value
        .as_mapping()?
        .get(key)?
        .as_sequence()?
        .iter()
        .find(|item| named_item_name(item) == Some(name))
        .cloned()
}

fn append_named_sequence_item(
    target: &mut Value,
    key: &str,
    item: Value,
    name: &str,
) -> Result<()> {
    let mapping = target
        .as_mapping_mut()
        .context("mihomo profile root must be a YAML mapping")?;
    let key_value = Value::from(key);
    if !mapping.contains_key(&key_value) {
        mapping.insert(key_value.clone(), Vec::<Value>::new().into());
    }
    let sequence = mapping
        .get_mut(&key_value)
        .and_then(Value::as_sequence_mut)
        .with_context(|| format!("mihomo profile {key} must be a YAML sequence"))?;

    if let Some(existing) = sequence
        .iter()
        .find(|candidate| named_item_name(candidate) == Some(name))
    {
        if existing == &item {
            return Ok(());
        }
        anyhow::bail!("{key} entry {name} conflicts with an existing entry");
    }

    sequence.push(item);
    Ok(())
}

fn merge_named_map_entry(target: &mut Value, source: &Value, key: &str, name: &str) -> Result<()> {
    let Some(item) = source
        .as_mapping()
        .and_then(|mapping| mapping.get(key))
        .and_then(Value::as_mapping)
        .and_then(|mapping| mapping.get(name))
        .cloned()
    else {
        if target
            .as_mapping()
            .and_then(|mapping| mapping.get(key))
            .and_then(Value::as_mapping)
            .is_some_and(|mapping| mapping.contains_key(name))
        {
            return Ok(());
        }
        anyhow::bail!("{key} entry not found in port proxy subscription: {name}");
    };

    let mapping = target
        .as_mapping_mut()
        .context("mihomo profile root must be a YAML mapping")?;
    let key_value = Value::from(key);
    if !mapping.contains_key(&key_value) {
        mapping.insert(key_value.clone(), Value::Mapping(Mapping::new()));
    }
    let section = mapping
        .get_mut(&key_value)
        .and_then(Value::as_mapping_mut)
        .with_context(|| format!("mihomo profile {key} must be a YAML mapping"))?;
    let name_value = Value::from(name);

    if let Some(existing) = section.get(&name_value) {
        if existing == &item {
            return Ok(());
        }
        anyhow::bail!("{key} entry {name} conflicts with an existing entry");
    }

    section.insert(name_value, item);
    Ok(())
}

fn target_has_proxy_or_group(target: &Value, name: &str) -> bool {
    find_named_sequence_item(target, "proxies", name).is_some()
        || find_named_sequence_item(target, "proxy-groups", name).is_some()
}

fn named_item_name(item: &Value) -> Option<&str> {
    item.as_mapping()?.get("name")?.as_str()
}

async fn read_subscription_profile(paths: &Paths, sub: &Subscription) -> Result<Value> {
    let profile = subscription::profile_path(paths, sub);
    let content = fs::read_to_string(&profile)
        .await
        .with_context(|| format!("failed to read {}", profile.display()))?;
    serde_yaml_ng::from_str(&content)
        .with_context(|| format!("failed to parse {}", profile.display()))
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
    if config.mixed_port != 0 {
        mapping.insert("mixed-port".into(), config.mixed_port.into());
    }
    if let Some(port) = config.proxy_ports.http {
        mapping.insert("port".into(), port.into());
    }
    if let Some(port) = config.proxy_ports.socks {
        mapping.insert("socks-port".into(), port.into());
    }
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

fn insert_service_listener(mapping: &mut Mapping, service: &PortProxyService) -> Result<()> {
    if !service.enabled {
        return Ok(());
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

    mapping.insert("listeners".into(), vec![Value::Mapping(listener)].into());
    Ok(())
}

fn insert_single_runtime_listeners(
    mapping: &mut Mapping,
    services: &[PortProxyService],
) -> Result<()> {
    let mut listeners = Vec::new();
    for (index, service) in services.iter().enumerate() {
        if !service.enabled {
            continue;
        }
        listeners.push(Value::Mapping(single_runtime_listener(index, service)?));
    }

    if listeners.is_empty() {
        mapping.remove("listeners");
    } else {
        mapping.insert("listeners".into(), listeners.into());
    }
    Ok(())
}

fn single_runtime_listener(index: usize, service: &PortProxyService) -> Result<Mapping> {
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

    let mut listener = Mapping::new();
    listener.insert(
        "name".into(),
        single_runtime_listener_name(index, service).into(),
    );
    listener.insert("type".into(), kind.clone().into());
    listener.insert("port".into(), service.port.into());
    listener.insert("listen".into(), listen.into());
    if kind != "http" {
        listener.insert("udp".into(), service.udp.into());
    }
    if service.mode.eq_ignore_ascii_case("global")
        && let Some(proxy) = service
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        listener.insert("proxy".into(), proxy.into());
    }
    if service.mode.eq_ignore_ascii_case("rule")
        && let Some(rule) = service
            .rule
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        listener.insert("rule".into(), rule.into());
    }

    Ok(listener)
}

pub fn single_runtime_listener_name(index: usize, service: &PortProxyService) -> String {
    let name = service.name.trim();
    if name.is_empty() {
        format!("clashtui-port-proxy-{}", index + 1)
    } else {
        format!("clashtui-port-proxy-{}-{name}", index + 1)
    }
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
            subscription: None,
            mode: "global".into(),
            proxy: Some("GLOBAL".into()),
            rule: None,
            rule_selections: Default::default(),
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
        assert!(mapping.get("listeners").is_none());
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

    #[test]
    fn service_listener_does_not_pin_proxy_inside_dedicated_runtime() -> Result<()> {
        let mut profile: Value = serde_yaml_ng::from_str(
            r"
proxies:
  - name: existing
    type: http
    server: 127.0.0.1
    port: 8080
proxy-groups: []
rules: []
",
        )?;
        let service = PortProxyService {
            name: "port-1".into(),
            kind: "mixed".into(),
            listen: "127.0.0.1".into(),
            port: 7071,
            subscription: None,
            mode: "global".into(),
            proxy: Some("missing".into()),
            rule: None,
            rule_selections: Default::default(),
            udp: true,
            enabled: true,
        };

        let mapping = profile
            .as_mapping_mut()
            .context("profile root is not a mapping")?;
        insert_service_listener(mapping, &service)?;
        let listener = profile
            .as_mapping()
            .and_then(|mapping| mapping.get("listeners"))
            .and_then(Value::as_sequence)
            .and_then(|listeners| listeners.first())
            .and_then(Value::as_mapping)
            .context("listener missing")?;

        assert_eq!(listener.get("name").and_then(Value::as_str), Some("port-1"));
        assert_eq!(listener.get("type").and_then(Value::as_str), Some("mixed"));
        assert_eq!(listener.get("port").and_then(Value::as_i64), Some(7071));
        assert!(listener.get("proxy").is_none());
        Ok(())
    }

    #[test]
    fn single_runtime_config_adds_all_enabled_port_proxy_listeners() -> Result<()> {
        let profile: Value = serde_yaml_ng::from_str(
            r"
proxies:
  - name: node-a
    type: http
    server: 127.0.0.1
    port: 8080
proxy-groups:
  - name: GLOBAL
    type: select
    proxies: [node-a]
rules:
  - MATCH,GLOBAL
listeners:
  - name: old
    type: mixed
    port: 7000
",
        )?;
        let mut config = AppConfig {
            mixed_port: 7070,
            runtime_mode: "rule".into(),
            ..AppConfig::default()
        };
        config.proxy_ports.services.push(PortProxyService {
            name: "fixed".into(),
            kind: "mixed".into(),
            listen: "127.0.0.1".into(),
            port: 7071,
            proxy: Some("node-a".into()),
            udp: true,
            ..PortProxyService::default()
        });
        config.proxy_ports.services.push(PortProxyService {
            enabled: false,
            name: "disabled".into(),
            port: 7072,
            ..PortProxyService::default()
        });
        config.proxy_ports.services.push(PortProxyService {
            name: "rule".into(),
            kind: "http".into(),
            listen: "127.0.0.1".into(),
            port: 7073,
            mode: "rule".into(),
            rule: Some("port-rule".into()),
            ..PortProxyService::default()
        });

        let value = build_single_runtime_config(&config, profile)?;
        let listeners = value
            .as_mapping()
            .and_then(|mapping| mapping.get("listeners"))
            .and_then(Value::as_sequence)
            .context("listeners missing")?;

        assert_eq!(listeners.len(), 2);
        let first = listeners
            .first()
            .and_then(Value::as_mapping)
            .context("first listener missing")?;
        assert_eq!(
            first.get("name").and_then(Value::as_str),
            Some("clashtui-port-proxy-1-fixed")
        );
        assert_eq!(first.get("proxy").and_then(Value::as_str), Some("node-a"));
        assert_eq!(first.get("udp").and_then(Value::as_bool), Some(true));

        let second = listeners
            .get(1)
            .and_then(Value::as_mapping)
            .context("second listener missing")?;
        assert_eq!(second.get("type").and_then(Value::as_str), Some("http"));
        assert_eq!(
            second.get("rule").and_then(Value::as_str),
            Some("port-rule")
        );
        assert!(second.get("udp").is_none());
        Ok(())
    }

    #[test]
    fn inserts_hidden_inline_provider_for_subscription_delay_checks() -> Result<()> {
        let subscription = Subscription {
            name: "demo".into(),
            ..Subscription::default()
        };
        let source: Value = serde_yaml_ng::from_str(
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
        )?;
        let mut target = Value::Mapping(empty_profile());

        insert_subscription_check_provider(&mut target, &subscription, &source)?;

        let provider_name = subscription_check_provider_name(&subscription);
        let provider = target
            .as_mapping()
            .and_then(|mapping| mapping.get("proxy-providers"))
            .and_then(Value::as_mapping)
            .and_then(|providers| providers.get(&provider_name))
            .and_then(Value::as_mapping)
            .context("check provider missing")?;
        assert_eq!(provider.get("type").and_then(Value::as_str), Some("inline"));
        let payload = provider
            .get("payload")
            .and_then(Value::as_sequence)
            .context("provider payload missing")?;
        assert_eq!(payload.len(), 1);
        assert_eq!(named_item_name(&payload[0]), Some("HK 01"));

        let groups = target
            .as_mapping()
            .and_then(|mapping| mapping.get("proxy-groups"))
            .and_then(Value::as_sequence)
            .context("proxy-groups missing")?;
        assert!(groups.is_empty());
        Ok(())
    }

    #[test]
    fn single_runtime_merges_port_proxy_subscription_proxy() -> Result<()> {
        let mut target: Value = serde_yaml_ng::from_str(
            r"
proxies:
  - name: active
    type: http
    server: 127.0.0.1
    port: 8080
proxy-groups: []
rules: []
",
        )?;
        let source: Value = serde_yaml_ng::from_str(
            r"
proxies:
  - name: imported
    type: http
    server: 127.0.0.2
    port: 8081
proxy-groups:
  - name: imported-group
    type: select
    proxies: [imported]
rules: []
",
        )?;
        let service = PortProxyService {
            mode: "global".into(),
            proxy: Some("imported".into()),
            ..PortProxyService::default()
        };

        merge_port_proxy_profile(&mut target, &source, 0, &service)?;

        let proxies = target
            .as_mapping()
            .and_then(|mapping| mapping.get("proxies"))
            .and_then(Value::as_sequence)
            .context("proxies missing")?;
        assert!(
            proxies
                .iter()
                .any(|item| named_item_name(item) == Some("active"))
        );
        assert!(
            proxies
                .iter()
                .any(|item| named_item_name(item) == Some("imported"))
        );
        Ok(())
    }
}
