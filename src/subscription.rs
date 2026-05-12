use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use tokio::fs;

use crate::config::{AppConfig, Paths, Subscription};

pub async fn update(paths: &Paths, config: &mut AppConfig, index: usize) -> Result<PathBuf> {
    let sub = config
        .subscriptions
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("subscription index out of range"))?;
    let body = reqwest::Client::new()
        .get(&sub.url)
        .header("User-Agent", "clashtui/0.1")
        .send()
        .await
        .with_context(|| format!("failed to download subscription {}", sub.name))?
        .error_for_status()
        .with_context(|| format!("subscription server rejected {}", sub.name))?
        .text()
        .await
        .with_context(|| format!("failed to read subscription {}", sub.name))?;

    if body.trim().is_empty() {
        anyhow::bail!("subscription {} returned empty content", sub.name);
    }

    paths.ensure().await?;
    let profile_path = profile_path(paths, &sub);
    fs::write(&profile_path, body)
        .await
        .with_context(|| format!("failed to write {}", profile_path.display()))?;

    if let Some(current) = config.subscriptions.get_mut(index) {
        current.updated_at = Some(now_unix().to_string());
    }

    Ok(profile_path)
}

pub fn profile_path(paths: &Paths, sub: &Subscription) -> PathBuf {
    paths
        .profiles_dir
        .join(format!("{}.yaml", sanitize(&sub.name)))
}

fn sanitize(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        "profile".into()
    } else {
        output
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
