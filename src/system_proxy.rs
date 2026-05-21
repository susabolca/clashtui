use anyhow::Result;
use sysproxy::{Autoproxy, Sysproxy};

use crate::config::SystemProxyTarget;

#[derive(Debug, Clone)]
pub struct SystemProxyStatus {
    pub enabled: bool,
    pub server: String,
    pub bypass: String,
    pub pac_enabled: bool,
    pub pac_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct SystemProxySyncState {
    last_http_server: Option<String>,
    last_pac_url: Option<String>,
}

pub fn status() -> Result<SystemProxyStatus> {
    let proxy = Sysproxy::get_system_proxy()?;
    let autoproxy = Autoproxy::get_auto_proxy().ok();
    Ok(SystemProxyStatus {
        enabled: proxy.enable,
        server: format!("{}:{}", proxy.host, proxy.port),
        bypass: proxy.bypass,
        pac_enabled: autoproxy.as_ref().is_some_and(|proxy| proxy.enable),
        pac_url: autoproxy.map_or_else(String::new, |proxy| proxy.url),
    })
}

pub fn apply_http(target: &SystemProxyTarget, state: &mut SystemProxySyncState) -> Result<()> {
    clear_pac()?;
    set_http(target)?;
    state.last_http_server = Some(target.server());
    state.last_pac_url = None;
    Ok(())
}

pub fn apply_pac(url: &str, state: &mut SystemProxySyncState) -> Result<()> {
    clear_http()?;
    set_pac(url)?;
    state.last_http_server = None;
    state.last_pac_url = Some(url.to_string());
    Ok(())
}

pub fn clear_owned(
    target: &SystemProxyTarget,
    pac_url: &str,
    state: &mut SystemProxySyncState,
) -> Result<()> {
    let current = status().ok();
    let clear_http_proxy = current.as_ref().is_some_and(|status| {
        status.enabled
            && (status.server == target.server()
                || state
                    .last_http_server
                    .as_deref()
                    .is_some_and(|server| server == status.server))
    });
    let clear_pac_proxy = current.as_ref().is_some_and(|status| {
        status.pac_enabled
            && (status.pac_url == pac_url
                || state
                    .last_pac_url
                    .as_deref()
                    .is_some_and(|url| url == status.pac_url))
    });

    if clear_http_proxy {
        clear_http()?;
    }
    if clear_pac_proxy {
        clear_pac()?;
    }

    *state = SystemProxySyncState::default();
    Ok(())
}

fn set_http(target: &SystemProxyTarget) -> Result<()> {
    let proxy = Sysproxy {
        host: target.host.clone(),
        port: target.port,
        bypass: target.bypass.clone(),
        enable: true,
    };
    proxy.set_system_proxy()?;
    Ok(())
}

fn set_pac(url: &str) -> Result<()> {
    let proxy = Autoproxy {
        url: url.to_string(),
        enable: true,
    };
    proxy.set_auto_proxy()?;
    Ok(())
}

fn clear_http() -> Result<()> {
    let proxy = Sysproxy {
        enable: false,
        ..Sysproxy::default()
    };
    proxy.set_system_proxy()?;
    Ok(())
}

fn clear_pac() -> Result<()> {
    let proxy = Autoproxy {
        enable: false,
        ..Autoproxy::default()
    };
    proxy.set_auto_proxy()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sync_state_tracks_last_targets() {
        let state = SystemProxySyncState {
            last_http_server: Some("127.0.0.1:7070".into()),
            last_pac_url: Some("http://127.0.0.1:18080/commands/pac".into()),
        };

        assert_eq!(state.last_http_server.as_deref(), Some("127.0.0.1:7070"));
        assert_eq!(
            state.last_pac_url.as_deref(),
            Some("http://127.0.0.1:18080/commands/pac")
        );
    }
}
