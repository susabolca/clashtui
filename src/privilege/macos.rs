use std::path::PathBuf;

use anyhow::Result;

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
