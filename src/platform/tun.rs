use crate::config::TunConfig;

#[cfg(target_os = "linux")]
mod imp {
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
}

#[cfg(target_os = "macos")]
mod imp {
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
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
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
