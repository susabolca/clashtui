use std::path::PathBuf;

use anyhow::{Context as _, Result};

#[derive(Debug, Clone)]
pub struct TunPermissionStatus {
    pub target: PathBuf,
    pub capabilities: String,
    pub has_tun_capabilities: bool,
    pub polkit_rule_path: &'static str,
    pub polkit_rule_exists: bool,
    pub polkit_rule_matches_user: bool,
    pub tun_device_exists: bool,
    pub is_root: bool,
}

impl TunPermissionStatus {
    pub const fn can_start_tun(&self) -> bool {
        true
    }
}

pub fn current_tun_permission_status() -> Result<TunPermissionStatus> {
    let target = std::env::current_exe().context("failed to locate current clashtui executable")?;
    Ok(TunPermissionStatus {
        target,
        capabilities: "not applicable".into(),
        has_tun_capabilities: true,
        polkit_rule_path: "not applicable",
        polkit_rule_exists: true,
        polkit_rule_matches_user: true,
        tun_device_exists: true,
        is_root: false,
    })
}

pub fn tun_install(_path: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("tun-install is not supported on this platform")
}

pub fn tun_uninstall(_path: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("tun-uninstall is not supported on this platform")
}

pub fn tun_install_privileged(_path: PathBuf, _user: String) -> Result<()> {
    anyhow::bail!("privileged tun-install is not supported on this platform")
}

pub fn tun_uninstall_privileged(_path: PathBuf) -> Result<()> {
    anyhow::bail!("privileged tun-uninstall is not supported on this platform")
}

pub fn tun_helper_run() -> Result<()> {
    anyhow::bail!("macOS TUN helper is not supported on this platform")
}
