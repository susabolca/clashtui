use std::path::PathBuf;

use anyhow::Result;

use crate::config::{AppConfig, Paths};

#[derive(Debug, Clone)]
pub struct AutostartStatus {
    pub configured: bool,
    pub installed: bool,
    pub path: Option<PathBuf>,
    pub message: Option<String>,
}

pub fn sync(paths: &Paths, config: &AppConfig) -> Result<AutostartStatus> {
    imp::sync(paths, config)
}

pub fn status(config: &AppConfig) -> AutostartStatus {
    imp::status(config)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::env;
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::{Path, PathBuf};

    use anyhow::{Context as _, Result};

    use super::AutostartStatus;
    use crate::config::{AppConfig, Paths};

    const LABEL: &str = "com.clashtui.daemon";

    pub fn sync(paths: &Paths, config: &AppConfig) -> Result<AutostartStatus> {
        let plist = plist_path()?;
        if config.autostart.enabled {
            write_launch_agent(paths, &plist)?;
        } else {
            remove_file_if_exists(&plist)?;
        }
        Ok(status_for_path(&plist, config))
    }

    pub fn status(config: &AppConfig) -> AutostartStatus {
        match plist_path() {
            Ok(path) => status_for_path(&path, config),
            Err(err) => AutostartStatus {
                configured: config.autostart.enabled,
                installed: false,
                path: None,
                message: Some(format!("{err:#}")),
            },
        }
    }

    fn status_for_path(path: &Path, config: &AppConfig) -> AutostartStatus {
        AutostartStatus {
            configured: config.autostart.enabled,
            installed: path.exists(),
            path: Some(path.to_path_buf()),
            message: None,
        }
    }

    fn write_launch_agent(paths: &Paths, plist: &Path) -> Result<()> {
        let exe = env::current_exe()
            .context("failed to locate current executable")?
            .canonicalize()
            .context("failed to resolve current executable")?;
        if let Some(parent) = plist.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::write(plist, launch_agent_plist(&exe, paths))
            .with_context(|| format!("failed to write {}", plist.display()))?;
        fs::set_permissions(plist, fs::Permissions::from_mode(0o644))
            .with_context(|| format!("failed to chmod {}", plist.display()))
    }

    fn plist_path() -> Result<PathBuf> {
        let home = env::var_os("HOME").context("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn launch_agent_plist(exe: &Path, paths: &Paths) -> String {
        let config_env = env::var("CLASHTUI_CONFIG_DIR").ok();
        let env_block = config_env
            .as_ref()
            .map(|dir| {
                format!(
                    r#"  <key>EnvironmentVariables</key>
  <dict>
    <key>CLASHTUI_CONFIG_DIR</key>
    <string>{}</string>
  </dict>
"#,
                    xml_escape(dir)
                )
            })
            .unwrap_or_default();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--daemon-run</string>
  </array>
{env_block}  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
            label = LABEL,
            exe = xml_escape(&exe.to_string_lossy()),
            env_block = env_block,
            log = xml_escape(&paths.log_file.to_string_lossy())
        )
    }

    fn remove_file_if_exists(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
        }
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn launch_agent_runs_daemon_without_keepalive() {
            let paths = Paths {
                config_dir: PathBuf::from("/tmp/clashtui"),
                config_file: PathBuf::from("/tmp/clashtui/config.yaml"),
                pid_file: PathBuf::from("/tmp/clashtui/clashtui.pid"),
                core_pid_file: PathBuf::from("/tmp/clashtui/mihomo.pid"),
                core_config_file: PathBuf::from("/tmp/clashtui/mihomo-run.yaml"),
                active_config_file: PathBuf::from("/tmp/clashtui/mihomo-active.yaml"),
                log_file: PathBuf::from("/tmp/clashtui/clashtui.log"),
                core_log_file: PathBuf::from("/tmp/clashtui/mihomo.log"),
                profiles_dir: PathBuf::from("/tmp/clashtui/profiles"),
                cores_dir: PathBuf::from("/tmp/clashtui/cores"),
            };
            let plist = launch_agent_plist(Path::new("/usr/local/bin/clashtui"), &paths);
            assert!(plist.contains("--daemon-run"));
            assert!(plist.contains("RunAtLoad"));
            assert!(!plist.contains("KeepAlive"));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::AutostartStatus;
    use crate::config::{AppConfig, Paths};
    use anyhow::Result;

    pub fn sync(_paths: &Paths, config: &AppConfig) -> Result<AutostartStatus> {
        Ok(status(config))
    }

    pub fn status(config: &AppConfig) -> AutostartStatus {
        AutostartStatus {
            configured: config.autostart.enabled,
            installed: false,
            path: None,
            message: Some("autostart is not implemented on this platform yet".into()),
        }
    }
}
