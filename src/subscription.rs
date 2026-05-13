use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use tokio::fs;

use crate::config::{AppConfig, Paths, Subscription, SubscriptionUserInfo};

pub async fn update(paths: &Paths, config: &mut AppConfig, index: usize) -> Result<PathBuf> {
    let sub = config
        .subscriptions
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("subscription index out of range"))?;
    let response = reqwest::Client::new()
        .get(&sub.url)
        .header("User-Agent", "clashtui/0.1")
        .send()
        .await
        .with_context(|| format!("failed to download subscription {}", sub.name))?
        .error_for_status()
        .with_context(|| format!("subscription server rejected {}", sub.name))?;
    let user_info = response
        .headers()
        .get("subscription-userinfo")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_user_info);
    let body = response
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
        current.last_error = None;
        if let Some(user_info) = user_info {
            current.user_info = user_info;
        }
    }

    Ok(profile_path)
}

pub async fn update_preserving_last_good(
    paths: &Paths,
    config: &mut AppConfig,
    index: usize,
) -> Result<PathBuf> {
    match update(paths, config, index).await {
        Ok(path) => Ok(path),
        Err(err) => {
            if let Some(current) = config.subscriptions.get_mut(index) {
                current.last_error = Some(err.to_string());
            }
            Err(err)
        }
    }
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

fn parse_user_info(value: &str) -> Option<SubscriptionUserInfo> {
    let mut info = SubscriptionUserInfo::default();
    for item in value.split(';') {
        let Some((key, value)) = item.trim().split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "upload" => info.upload = Some(value),
            "download" => info.download = Some(value),
            "total" => info.total = Some(value),
            "expire" => info.expire = Some(value),
            _ => {}
        }
    }
    (!info.is_empty()).then_some(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subscription_user_info_header() -> Result<()> {
        let info = parse_user_info("upload=10; download=20; total=100; expire=200")
            .ok_or_else(|| anyhow::anyhow!("missing user info"))?;

        assert_eq!(info.upload, Some(10));
        assert_eq!(info.download, Some(20));
        assert_eq!(info.used(), Some(30));
        assert_eq!(info.total, Some(100));
        assert_eq!(info.expire, Some(200));
        Ok(())
    }
}
