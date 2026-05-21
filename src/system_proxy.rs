use anyhow::Result;
use sysproxy::Sysproxy;

use crate::config::SystemProxyTarget;

#[derive(Debug, Clone)]
pub struct SystemProxyStatus {
    pub enabled: bool,
    pub server: String,
    pub bypass: String,
}

pub fn status() -> Result<SystemProxyStatus> {
    let proxy = Sysproxy::get_system_proxy()?;
    Ok(SystemProxyStatus {
        enabled: proxy.enable,
        server: format!("{}:{}", proxy.host, proxy.port),
        bypass: proxy.bypass,
    })
}

pub fn apply(target: &SystemProxyTarget) -> Result<()> {
    let proxy = Sysproxy {
        host: target.host.clone(),
        port: target.port,
        bypass: target.bypass.clone(),
        enable: true,
    };
    proxy.set_system_proxy()?;
    Ok(())
}

pub fn clear() -> Result<()> {
    let proxy = Sysproxy {
        enable: false,
        ..Sysproxy::default()
    };
    proxy.set_system_proxy()?;
    Ok(())
}
