use std::path::PathBuf;

use anyhow::Result;

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
