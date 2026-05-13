use std::path::PathBuf;
use std::process::Command;

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
        self.is_root
    }
}

pub fn current_tun_permission_status() -> Result<TunPermissionStatus> {
    let target = std::env::current_exe().context("failed to locate current clashtui executable")?;
    let is_root = is_root_user();
    Ok(TunPermissionStatus {
        target,
        capabilities: "not applicable".into(),
        has_tun_capabilities: is_root,
        polkit_rule_path: "not applicable",
        polkit_rule_exists: true,
        polkit_rule_matches_user: true,
        tun_device_exists: true,
        is_root,
    })
}

fn is_root_user() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|value| value.trim() == "0")
}

pub fn tun_install(_path: Option<PathBuf>) -> Result<()> {
    anyhow::bail!(
        "tun-install manages Linux capabilities only; on macOS mihomo uses utun devices and must be run with elevated privileges or a helper service"
    )
}

pub fn tun_uninstall(_path: Option<PathBuf>) -> Result<()> {
    anyhow::bail!("tun-uninstall is only needed for Linux capabilities")
}

pub fn tun_install_privileged(_path: PathBuf, _user: String) -> Result<()> {
    anyhow::bail!("privileged tun-install is only supported on Linux")
}

pub fn tun_uninstall_privileged(_path: PathBuf) -> Result<()> {
    anyhow::bail!("privileged tun-uninstall is only supported on Linux")
}
