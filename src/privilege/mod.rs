use std::path::PathBuf;

use anyhow::Result;

#[cfg(target_os = "linux")]
mod imp {
    pub use super::linux::*;
}

#[cfg(target_os = "macos")]
mod imp {
    pub use super::macos::*;
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    pub use super::unsupported::*;
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod unsupported;

pub use imp::TunPermissionStatus;

pub fn tun_install(path: Option<PathBuf>) -> Result<()> {
    imp::tun_install(path)
}

pub fn tun_uninstall(path: Option<PathBuf>) -> Result<()> {
    imp::tun_uninstall(path)
}

pub fn tun_install_privileged(path: PathBuf, user: String) -> Result<()> {
    imp::tun_install_privileged(path, user)
}

pub fn tun_uninstall_privileged(path: PathBuf) -> Result<()> {
    imp::tun_uninstall_privileged(path)
}

pub fn current_tun_permission_status() -> Result<TunPermissionStatus> {
    imp::current_tun_permission_status()
}
