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
    let segments = path_segments(&operation.path)?;
    match operation.op.as_str() {
        "set" => set_value(root, &segments, operation.value.clone()),
        "append" => append_value(root, &segments, operation.value.clone()),
        "remove" => remove_value(root, &segments),
        other => anyhow::bail!("unsupported patch op: {other}"),
    }
}

fn path_segments(path: &str) -> Result<Vec<String>> {
    let path = path.trim();
    if path.starts_with('/') {
        return Ok(path
            .trim_start_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
            .collect());
    }

    dotted_path_segments(path)
}

fn dotted_path_segments(path: &str) -> Result<Vec<String>> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut bracket = String::new();
    let mut in_bracket = false;

    for ch in path.chars() {
        match ch {
            '.' if !in_bracket => {
                push_path_segment(&mut segments, &mut current);
            }
            '[' if !in_bracket => {
                push_path_segment(&mut segments, &mut current);
                in_bracket = true;
                bracket.clear();
            }
            ']' if in_bracket => {
                let value = bracket.trim();
                if value.is_empty() {
                    anyhow::bail!("empty array index in patch path: {path}");
                }
                segments.push(value.trim_matches(['"', '\'']).to_string());
                in_bracket = false;
            }
            _ if in_bracket => bracket.push(ch),
            _ => current.push(ch),
        }
    }

    if in_bracket {
        anyhow::bail!("unterminated array index in patch path: {path}");
    }
    push_path_segment(&mut segments, &mut current);
    if segments.is_empty() {
        anyhow::bail!("patch path is empty");
    }
    Ok(segments)
}

fn push_path_segment(segments: &mut Vec<String>, current: &mut String) {
    let value = current.trim();
    if !value.is_empty() {
        segments.push(value.to_string());
    }
    current.clear();
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
            let index = array_segment_index(items, last)?;
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
            let index = array_segment_index(items, last)?;
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
                let index = array_segment_index(items, segment)?;
                items
                    .get_mut(index)
                    .with_context(|| format!("array index out of range: {index}"))?
            }
            _ => anyhow::bail!("patch path descends through a scalar"),
        };
    }
    Ok(current)
}

fn array_segment_index(items: &[Value], segment: &str) -> Result<usize> {
    if let Ok(index) = segment.parse::<usize>() {
        return Ok(index);
    }
    let Some((field, expected)) = segment.split_once('=') else {
        anyhow::bail!("invalid array index or selector: {segment}");
    };
    let field = field.trim();
    let expected = expected.trim().trim_matches(['"', '\'']);
    items
        .iter()
        .position(|item| {
            item.as_object()
                .and_then(|object| object.get(field))
                .is_some_and(|value| value_matches_selector(value, expected))
        })
        .with_context(|| format!("array selector not found: {field}={expected}"))
}

fn value_matches_selector(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(actual) => actual == expected,
        Value::Number(actual) => actual.to_string() == expected,
        Value::Bool(actual) => actual.to_string() == expected,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_updates_app_config_scalar() -> Result<()> {
        let config = AppConfig::default();
        let patch = ConfigPatch {
            summary: "Update bind address".into(),
            restart_required: true,
            operations: vec![PatchOperation {
                op: "set".into(),
                path: "/proxy_host".into(),
                value: serde_json::json!("0.0.0.0"),
            }],
        };

        let updated = apply_config_patch(&config, &patch)?;
        assert_eq!(updated.proxy_host, "0.0.0.0");
        Ok(())
    }

    #[test]
    fn operation_appends_array_item() -> Result<()> {
        let mut value = serde_json::json!({ "items": [] });

        apply_operation(
            &mut value,
            &PatchOperation {
                op: "append".into(),
                path: "/items".into(),
                value: serde_json::json!({ "name": "created" }),
            },
        )?;

        assert_eq!(value["items"][0]["name"], "created");
        Ok(())
    }

    #[test]
    fn operation_sets_json_pointer_array_item() -> Result<()> {
        let mut value = serde_json::json!({
            "items": [
                { "enabled": true },
                { "enabled": false }
            ]
        });

        apply_operation(
            &mut value,
            &PatchOperation {
                op: "set".into(),
                path: "/items/1/enabled".into(),
                value: serde_json::json!(true),
            },
        )?;

        assert_eq!(value["items"][1]["enabled"], true);
        Ok(())
    }

    #[test]
    fn patch_accepts_dotted_array_path() -> Result<()> {
        let mut value = serde_json::json!({
            "items": [
                { "enabled": false }
            ]
        });

        apply_operation(
            &mut value,
            &PatchOperation {
                op: "set".into(),
                path: "items[0].enabled".into(),
                value: serde_json::json!(true),
            },
        )?;

        assert_eq!(value["items"][0]["enabled"], true);
        Ok(())
    }

    #[test]
    fn patch_accepts_dot_index_array_path() -> Result<()> {
        let mut value = serde_json::json!({
            "items": [
                { "enabled": false }
            ]
        });

        apply_operation(
            &mut value,
            &PatchOperation {
                op: "set".into(),
                path: "items.0.enabled".into(),
                value: serde_json::json!(true),
            },
        )?;

        assert_eq!(value["items"][0]["enabled"], true);
        Ok(())
    }

    #[test]
    fn patch_accepts_array_field_selector_path() -> Result<()> {
        let mut value = serde_json::json!({
            "items": [
                { "name": "primary", "enabled": true },
                { "name": "secondary", "enabled": false }
            ]
        });

        apply_operation(
            &mut value,
            &PatchOperation {
                op: "set".into(),
                path: "items[name=secondary].enabled".into(),
                value: serde_json::json!(true),
            },
        )?;

        assert_eq!(value["items"][1]["enabled"], true);
        Ok(())
    }
}
