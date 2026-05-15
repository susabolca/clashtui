use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::AppConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigPatch {
    pub summary: String,
    pub restart_required: bool,
    pub operations: Vec<PatchOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchOperation {
    pub op: String,
    pub path: String,
    #[serde(default)]
    pub value: Value,
}

pub fn apply_config_patch(config: &AppConfig, patch: &ConfigPatch) -> Result<AppConfig> {
    let mut value = serde_json::to_value(config).context("failed to encode config as JSON")?;
    for operation in &patch.operations {
        apply_operation(&mut value, operation)
            .with_context(|| format!("failed to apply patch operation at {}", operation.path))?;
    }
    serde_json::from_value(value).context("patched config is not a valid AppConfig")
}

fn apply_operation(root: &mut Value, operation: &PatchOperation) -> Result<()> {
    let segments = pointer_segments(&operation.path)?;
    match operation.op.as_str() {
        "set" => set_value(root, &segments, operation.value.clone()),
        "append" => append_value(root, &segments, operation.value.clone()),
        "remove" => remove_value(root, &segments),
        other => anyhow::bail!("unsupported patch op: {other}"),
    }
}

fn pointer_segments(path: &str) -> Result<Vec<String>> {
    if !path.starts_with('/') {
        anyhow::bail!("patch path must be a JSON pointer");
    }
    Ok(path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect())
}

fn set_value(current: &mut Value, segments: &[String], value: Value) -> Result<()> {
    let Some((last, parents)) = segments.split_last() else {
        *current = value;
        return Ok(());
    };
    let parent = descend_mut(current, parents)?;
    match parent {
        Value::Object(map) => {
            map.insert(last.clone(), value);
            Ok(())
        }
        Value::Array(items) => {
            let index = last
                .parse::<usize>()
                .with_context(|| format!("invalid array index: {last}"))?;
            let Some(slot) = items.get_mut(index) else {
                anyhow::bail!("array index out of range: {index}");
            };
            *slot = value;
            Ok(())
        }
        _ => anyhow::bail!("patch parent is not an object or array"),
    }
}

fn append_value(current: &mut Value, segments: &[String], value: Value) -> Result<()> {
    let target = descend_mut(current, segments)?;
    let Value::Array(items) = target else {
        anyhow::bail!("append target is not an array");
    };
    items.push(value);
    Ok(())
}

fn remove_value(current: &mut Value, segments: &[String]) -> Result<()> {
    let Some((last, parents)) = segments.split_last() else {
        anyhow::bail!("cannot remove document root");
    };
    let parent = descend_mut(current, parents)?;
    match parent {
        Value::Object(map) => {
            map.remove(last);
            Ok(())
        }
        Value::Array(items) => {
            let index = last
                .parse::<usize>()
                .with_context(|| format!("invalid array index: {last}"))?;
            if index >= items.len() {
                anyhow::bail!("array index out of range: {index}");
            }
            items.remove(index);
            Ok(())
        }
        _ => anyhow::bail!("patch parent is not an object or array"),
    }
}

fn descend_mut<'a>(mut current: &'a mut Value, segments: &[String]) -> Result<&'a mut Value> {
    for segment in segments {
        current = match current {
            Value::Object(map) => map
                .get_mut(segment)
                .with_context(|| format!("object key not found: {segment}"))?,
            Value::Array(items) => {
                let index = segment
                    .parse::<usize>()
                    .with_context(|| format!("invalid array index: {segment}"))?;
                items
                    .get_mut(index)
                    .with_context(|| format!("array index out of range: {index}"))?
            }
            _ => anyhow::bail!("patch path descends through a scalar"),
        };
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortProxyService;

    #[test]
    fn patch_sets_dns_policy() -> Result<()> {
        let config = AppConfig::default();
        let patch = ConfigPatch {
            summary: "DNS policy".into(),
            restart_required: true,
            operations: vec![PatchOperation {
                op: "set".into(),
                path: "/dns/nameserver_policy/+.taobao.net".into(),
                value: serde_json::json!(["30.30.30.30"]),
            }],
        };

        let updated = apply_config_patch(&config, &patch)?;
        assert_eq!(
            updated.dns.nameserver_policy.get("+.taobao.net"),
            Some(&vec!["30.30.30.30".to_string()])
        );
        Ok(())
    }

    #[test]
    fn patch_appends_port_proxy() -> Result<()> {
        let config = AppConfig::default();
        let service = PortProxyService {
            name: "hk-socks".into(),
            kind: "socks".into(),
            port: 7081,
            proxy: Some("HK-01".into()),
            ..PortProxyService::default()
        };
        let patch = ConfigPatch {
            summary: "Add proxy".into(),
            restart_required: true,
            operations: vec![PatchOperation {
                op: "append".into(),
                path: "/proxy_ports/services".into(),
                value: serde_json::to_value(service)?,
            }],
        };

        let updated = apply_config_patch(&config, &patch)?;
        assert_eq!(updated.proxy_ports.services.len(), 1);
        assert_eq!(updated.proxy_ports.services[0].name, "hk-socks");
        Ok(())
    }
}
