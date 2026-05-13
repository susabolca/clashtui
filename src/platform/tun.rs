use crate::config::TunConfig;

#[cfg(target_os = "linux")]
mod imp {
    use std::process::Command;

    use anyhow::{Context as _, Result};

    use crate::config::TunConfig;

    pub const DEFAULT_DEVICE: &str = "Mihomo";
    pub const SUPPORTS_AUTO_REDIRECT: bool = true;

    pub fn normalize_config(config: &TunConfig) -> TunConfig {
        let mut config = config.clone();
        if config.device.trim().is_empty() {
            config.device = DEFAULT_DEVICE.into();
        }
        config
    }

    pub fn default_route_interface() -> Result<String> {
        let output = Command::new("ip")
            .args(["route", "show", "default"])
            .output()
            .context("failed to run ip route show default")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("ip route show default failed: {}", stderr.trim());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let words = line.split_whitespace().collect::<Vec<_>>();
            for pair in words.windows(2) {
                if pair[0] == "dev" {
                    validate_interface_name(pair[1])?;
                    return Ok(pair[1].into());
                }
            }
        }
        anyhow::bail!("default route did not report an interface")
    }

    fn validate_interface_name(interface: &str) -> Result<()> {
        if interface.is_empty() || interface.len() >= libc::IFNAMSIZ {
            anyhow::bail!("invalid interface name: {interface}");
        }
        if !interface
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            anyhow::bail!("invalid interface name: {interface}");
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::process::Command;

    use anyhow::{Context as _, Result};

    use crate::config::TunConfig;

    pub const DEFAULT_DEVICE: &str = "utun1024";
    pub const SUPPORTS_AUTO_REDIRECT: bool = false;

    pub fn normalize_config(config: &TunConfig) -> TunConfig {
        let mut config = config.clone();
        if config.device.trim().is_empty() || !config.device.starts_with("utun") {
            config.device = DEFAULT_DEVICE.into();
        }
        config.auto_redirect = false;
        config
    }

    pub fn default_route_interface() -> Result<String> {
        let output = Command::new("/sbin/route")
            .arg("-n")
            .arg("get")
            .arg("default")
            .output()
            .context("failed to run route get default")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("route get default failed: {}", stderr.trim());
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if let Some(interface) = line.strip_prefix("interface:") {
                let interface = interface.trim();
                validate_interface_name(interface)?;
                return Ok(interface.into());
            }
        }
        anyhow::bail!("route get default did not report an interface")
    }

    fn validate_interface_name(interface: &str) -> Result<()> {
        if interface.is_empty() || interface.len() >= libc::IFNAMSIZ {
            anyhow::bail!("invalid interface name: {interface}");
        }
        if !interface
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            anyhow::bail!("invalid interface name: {interface}");
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use anyhow::Result;

    use crate::config::TunConfig;

    pub const DEFAULT_DEVICE: &str = "Mihomo";
    pub const SUPPORTS_AUTO_REDIRECT: bool = false;

    pub fn normalize_config(config: &TunConfig) -> TunConfig {
        let mut config = config.clone();
        if config.device.trim().is_empty() {
            config.device = DEFAULT_DEVICE.into();
        }
        config.auto_redirect = false;
        config
    }

    pub fn default_route_interface() -> Result<String> {
        anyhow::bail!("default route interface is not supported on this platform")
    }
}

pub fn default_device() -> &'static str {
    imp::DEFAULT_DEVICE
}

pub fn supports_auto_redirect() -> bool {
    imp::SUPPORTS_AUTO_REDIRECT
}

pub fn normalize_config(config: &TunConfig) -> TunConfig {
    imp::normalize_config(config)
}

pub fn default_route_interface() -> anyhow::Result<String> {
    imp::default_route_interface()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_uses_utun_and_disables_auto_redirect() {
        let config = TunConfig {
            device: "Mihomo".into(),
            auto_redirect: true,
            ..TunConfig::default()
        };

        let normalized = normalize_config(&config);

        assert_eq!(normalized.device, "utun1024");
        assert!(!normalized.auto_redirect);
        assert!(!supports_auto_redirect());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn linux_keeps_named_device_and_auto_redirect() {
        let config = TunConfig {
            device: "Mihomo".into(),
            auto_redirect: true,
            ..TunConfig::default()
        };

        let normalized = normalize_config(&config);

        assert_eq!(normalized.device, "Mihomo");
        assert!(normalized.auto_redirect);
        assert!(supports_auto_redirect());
    }
}
