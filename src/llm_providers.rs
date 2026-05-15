use std::path::Path;
use std::sync::{OnceLock, RwLock};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

const BUNDLED_PROVIDERS_YAML: &str = include_str!("../doc/llm-providers.yaml");

static PROVIDERS: OnceLock<RwLock<Vec<LlmProviderPreset>>> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmProviderPreset {
    pub id: String,
    pub label: String,
    pub base_url: String,
    #[serde(default, alias = "model")]
    pub default_model: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LlmProviderCatalog {
    providers: Vec<LlmProviderPreset>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderUpdateReport {
    pub added_providers: usize,
    pub updated_providers: usize,
    pub preserved_custom_providers: usize,
    pub preserved_custom_models: usize,
}

pub fn init_from_file(path: &Path) -> Option<String> {
    if let Err(err) = ensure_local_file(path) {
        return Some(err.to_string());
    }
    let (providers, warning) = load_from_file_or_bundled(path);
    replace_presets(providers);
    warning
}

pub fn reload_from_file(path: &Path) -> Result<()> {
    let providers = providers_for_update(path)?;
    replace_presets(providers);
    Ok(())
}

pub fn presets() -> Vec<LlmProviderPreset> {
    store()
        .read()
        .map(|providers| providers.clone())
        .unwrap_or_else(|_| fallback_presets())
}

pub fn provider(id: &str) -> Option<LlmProviderPreset> {
    presets().into_iter().find(|provider| provider.id == id)
}

pub fn model_options(provider_id: &str) -> Vec<String> {
    let Some(provider) = provider(provider_id) else {
        return Vec::new();
    };
    provider.effective_models()
}

pub fn api_key_for(path: &Path, provider_id: &str) -> Option<String> {
    provider_from_file(path, provider_id)
        .and_then(|provider| non_empty(provider.api_key))
        .or_else(|| provider(provider_id).and_then(|provider| non_empty(provider.api_key.clone())))
}

pub fn save_api_key(path: &Path, provider_id: &str, api_key: &str) -> Result<()> {
    let mut providers = providers_for_update(path)?;
    let Some(provider) = providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    else {
        anyhow::bail!("unknown LLM provider: {provider_id}");
    };
    provider.api_key = api_key.trim().to_string();
    write_catalog(path, &providers)
}

pub fn save_model(path: &Path, provider_id: &str, model: &str) -> Result<bool> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(false);
    }

    let mut providers = providers_for_update(path)?;
    let Some(provider) = providers
        .iter_mut()
        .find(|provider| provider.id == provider_id)
    else {
        anyhow::bail!("unknown LLM provider: {provider_id}");
    };
    let mut changed = if provider.default_model.trim().is_empty() {
        provider.default_model = model.to_string();
        true
    } else {
        false
    };
    if provider.models.iter().any(|known| known == model) {
        if changed {
            write_catalog(path, &providers)?;
        }
        return Ok(changed);
    }
    provider.models.push(model.to_string());
    changed = true;
    write_catalog(path, &providers)?;
    Ok(changed)
}

pub fn provider_has_api_key(path: &Path, provider_id: &str) -> bool {
    api_key_for(path, provider_id).is_some()
}

pub fn update_local_from_bundled(path: &Path) -> Result<ProviderUpdateReport> {
    ensure_local_file(path)?;
    let local = providers_for_update(path)?;
    let bundled = parse_bundled().context("failed to parse bundled LLM providers")?;
    let mut report = ProviderUpdateReport::default();
    let mut merged = Vec::with_capacity(local.len().max(bundled.len()));

    for bundled_provider in &bundled {
        match local
            .iter()
            .find(|provider| provider.id == bundled_provider.id)
        {
            Some(local_provider) => {
                let mut provider = bundled_provider.clone();
                provider.api_key.clone_from(&local_provider.api_key);
                if !local_provider.api_key_env.trim().is_empty() {
                    provider.api_key_env.clone_from(&local_provider.api_key_env);
                }

                let custom_models = custom_models(local_provider, bundled_provider);
                report.preserved_custom_models += custom_models.len();
                provider.models.extend(custom_models.iter().cloned());
                if custom_models
                    .iter()
                    .any(|model| model == &local_provider.default_model)
                {
                    provider
                        .default_model
                        .clone_from(&local_provider.default_model);
                }

                merged.push(provider);
                report.updated_providers += 1;
            }
            None => {
                merged.push(bundled_provider.clone());
                report.added_providers += 1;
            }
        }
    }

    for local_provider in &local {
        if bundled
            .iter()
            .any(|provider| provider.id == local_provider.id)
        {
            continue;
        }
        merged.push(local_provider.clone());
        report.preserved_custom_providers += 1;
    }

    write_catalog(path, &merged)?;
    replace_presets(merged);
    Ok(report)
}

fn load_from_file_or_bundled(path: &Path) -> (Vec<LlmProviderPreset>, Option<String>) {
    if path.exists() {
        match std::fs::read_to_string(path)
            .ok()
            .and_then(|content| parse_catalog(&content).ok())
        {
            Some(providers) if !providers.is_empty() => return (providers, None),
            _ => {
                let warning = format!(
                    "Ignoring invalid {}; using bundled LLM providers",
                    path.display()
                );
                return (
                    parse_bundled().unwrap_or_else(|_| fallback_presets()),
                    Some(warning),
                );
            }
        }
    }
    (parse_bundled().unwrap_or_else(|_| fallback_presets()), None)
}

fn parse_bundled() -> Result<Vec<LlmProviderPreset>, serde_yaml_ng::Error> {
    parse_catalog(BUNDLED_PROVIDERS_YAML)
}

fn parse_catalog(content: &str) -> Result<Vec<LlmProviderPreset>, serde_yaml_ng::Error> {
    let catalog: LlmProviderCatalog = serde_yaml_ng::from_str(content)?;
    Ok(catalog
        .providers
        .into_iter()
        .filter(|provider| !provider.id.trim().is_empty() && !provider.label.trim().is_empty())
        .collect())
}

fn provider_from_file(path: &Path, provider_id: &str) -> Option<LlmProviderPreset> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_catalog(&content)
        .ok()?
        .into_iter()
        .find(|provider| provider.id == provider_id)
}

fn providers_for_update(path: &Path) -> Result<Vec<LlmProviderPreset>> {
    if path.exists() {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let providers = parse_catalog(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        if !providers.is_empty() {
            return Ok(providers);
        }
    }
    parse_bundled().context("failed to parse bundled LLM providers")
}

fn ensure_local_file(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    write_catalog(
        path,
        &parse_bundled().context("failed to parse bundled LLM providers")?,
    )
}

fn write_catalog(path: &Path, providers: &[LlmProviderPreset]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let catalog = LlmProviderCatalog {
        providers: providers.to_vec(),
    };
    let content = serde_yaml_ng::to_string(&catalog).context("failed to encode LLM providers")?;
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let permissions = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, permissions)
            .with_context(|| format!("failed to chmod {}", path.display()))?;
    }
    Ok(())
}

fn store() -> &'static RwLock<Vec<LlmProviderPreset>> {
    PROVIDERS.get_or_init(|| RwLock::new(parse_bundled().unwrap_or_else(|_| fallback_presets())))
}

fn replace_presets(providers: Vec<LlmProviderPreset>) {
    if let Ok(mut guard) = store().write() {
        *guard = providers;
    }
}

fn custom_models(
    local_provider: &LlmProviderPreset,
    bundled_provider: &LlmProviderPreset,
) -> Vec<String> {
    local_provider
        .models
        .iter()
        .filter(|model| {
            let model = model.trim();
            !model.is_empty() && !bundled_provider.models.iter().any(|known| known == model)
        })
        .cloned()
        .collect()
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn fallback_presets() -> Vec<LlmProviderPreset> {
    vec![LlmProviderPreset {
        id: "deepseek".into(),
        label: "DeepSeek".into(),
        base_url: "https://api.deepseek.com".into(),
        default_model: "deepseek-v4-flash".into(),
        models: vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
        api_key: String::new(),
        api_key_env: "DEEPSEEK_API_KEY".into(),
        note: "DeepSeek official OpenAI-compatible endpoint.".into(),
    }]
}

impl LlmProviderPreset {
    pub fn effective_models(&self) -> Vec<String> {
        if !self.models.is_empty() {
            return self.models.clone();
        }
        if self.default_model.trim().is_empty() {
            return Vec::new();
        }
        vec![self.default_model.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_provider_catalog_parses() -> Result<()> {
        let providers = parse_bundled()?;
        assert!(providers.iter().any(|provider| provider.id == "deepseek"));
        assert!(providers.iter().any(|provider| provider.id == "kimi-code"));
        assert!(
            providers
                .iter()
                .any(|provider| provider.id == "qianfan-coding")
        );
        Ok(())
    }

    #[test]
    fn coding_plan_endpoints_are_distinct_from_normal_endpoints() -> Result<()> {
        let providers = parse_bundled()?;
        let Some(kimi) = providers
            .iter()
            .find(|provider| provider.id == "kimi-platform")
        else {
            anyhow::bail!("missing Kimi Platform provider");
        };
        let Some(kimi_code) = providers.iter().find(|provider| provider.id == "kimi-code") else {
            anyhow::bail!("missing Kimi Code provider");
        };
        let Some(qianfan) = providers.iter().find(|provider| provider.id == "qianfan") else {
            anyhow::bail!("missing Qianfan provider");
        };
        let Some(qianfan_coding) = providers
            .iter()
            .find(|provider| provider.id == "qianfan-coding")
        else {
            anyhow::bail!("missing Qianfan Coding provider");
        };

        assert_ne!(kimi.base_url, kimi_code.base_url);
        assert_ne!(qianfan.base_url, qianfan_coding.base_url);
        Ok(())
    }

    #[test]
    fn update_preserves_api_keys_custom_models_and_custom_providers() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "clashtui-llm-providers-test-{}.yaml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let local = vec![
            LlmProviderPreset {
                id: "deepseek".into(),
                label: "DeepSeek Local".into(),
                base_url: "https://old.example.invalid".into(),
                default_model: "local-model".into(),
                models: vec!["deepseek-v4-flash".into(), "local-model".into()],
                api_key: "sk-local".into(),
                api_key_env: "LOCAL_KEY".into(),
                note: "local".into(),
            },
            LlmProviderPreset {
                id: "custom-provider".into(),
                label: "Custom Provider".into(),
                base_url: "https://custom.example.invalid/v1".into(),
                default_model: "custom-model".into(),
                models: vec!["custom-model".into()],
                api_key: "sk-custom".into(),
                api_key_env: String::new(),
                note: "custom".into(),
            },
        ];
        write_catalog(&path, &local)?;

        let report = update_local_from_bundled(&path)?;
        let merged = providers_for_update(&path)?;
        let Some(deepseek) = merged.iter().find(|provider| provider.id == "deepseek") else {
            anyhow::bail!("missing merged DeepSeek provider");
        };
        let Some(custom) = merged
            .iter()
            .find(|provider| provider.id == "custom-provider")
        else {
            anyhow::bail!("missing merged custom provider");
        };

        assert_eq!(deepseek.api_key, "sk-local");
        assert_eq!(deepseek.api_key_env, "LOCAL_KEY");
        assert!(deepseek.models.iter().any(|model| model == "local-model"));
        assert_eq!(deepseek.default_model, "local-model");
        assert_eq!(custom.api_key, "sk-custom");
        assert_eq!(report.preserved_custom_providers, 1);
        assert_eq!(report.preserved_custom_models, 1);

        let _ = std::fs::remove_file(path);
        Ok(())
    }
}
