use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result};

const TUN_CAPABILITIES: &str = "cap_net_admin,cap_net_bind_service+ep";
const POLKIT_RULE_PATH: &str = "/etc/polkit-1/rules.d/49-clashtui-mihomo-resolved.rules";

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
        self.is_root || self.has_tun_capabilities
    }
}

pub fn tun_install(path: Option<PathBuf>) -> Result<()> {
    let target = target_binary(path)?;
    let user = invoking_user()?;
    if has_tun_capabilities(&target)? && polkit_rule_matches(&user)? {
        println!("TUN permissions already installed: {}", target.display());
        print_getcap(&target)?;
        println!("polkit rule: {POLKIT_RULE_PATH}");
        print_tun_device_status();
        return Ok(());
    }
    if !is_root() {
        return run_sudo_install(&target, &user);
    }
    tun_install_privileged(target, user)
}

pub fn current_tun_permission_status() -> Result<TunPermissionStatus> {
    let target = target_binary(None)?;
    tun_permission_status(&target)
}

pub fn tun_permission_status(target: &Path) -> Result<TunPermissionStatus> {
    let user = invoking_user()?;
    let capabilities = getcap_output(target)?;
    let polkit_rule_matches_user = polkit_rule_matches(&user).unwrap_or(false);
    Ok(TunPermissionStatus {
        target: target.to_path_buf(),
        has_tun_capabilities: capabilities.contains("cap_net_admin")
            && capabilities.contains("cap_net_bind_service")
            && capabilities.contains("=ep"),
        capabilities,
        polkit_rule_path: POLKIT_RULE_PATH,
        polkit_rule_exists: polkit_rule_exists(),
        polkit_rule_matches_user,
        tun_device_exists: Path::new("/dev/net/tun").exists(),
        is_root: is_root(),
    })
}

pub fn tun_install_privileged(target: PathBuf, user: String) -> Result<()> {
    run_setcap([TUN_CAPABILITIES, path_to_str(&target)?])?;
    install_polkit_rule(&user)?;
    println!("TUN permissions installed: {}", target.display());
    print_getcap(&target)?;
    println!("polkit rule: {POLKIT_RULE_PATH}");
    print_tun_device_status();
    Ok(())
}

pub fn tun_uninstall(path: Option<PathBuf>) -> Result<()> {
    let target = target_binary(path)?;
    if !has_any_capabilities(&target)? && !polkit_rule_exists() {
        println!("TUN permissions already removed: {}", target.display());
        print_getcap(&target)?;
        return Ok(());
    }
    if !is_root() {
        return run_sudo_privileged("__tun-uninstall-root", &target);
    }
    tun_uninstall_privileged(target)
}

pub fn tun_uninstall_privileged(target: PathBuf) -> Result<()> {
    if has_any_capabilities(&target)? {
        run_setcap(["-r", path_to_str(&target)?])?;
    }
    remove_polkit_rule()?;
    println!("TUN permissions removed: {}", target.display());
    print_getcap(&target)?;
    Ok(())
}

fn target_binary(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path,
        None => std::env::current_exe().context("failed to locate current clashtui executable")?,
    };
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn run_sudo_install(target: &Path, user: &str) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate current clashtui executable")?;
    let status = Command::new("sudo")
        .arg(exe)
        .arg("__tun-install-root")
        .arg("--path")
        .arg(target)
        .arg("--user")
        .arg(user)
        .status()
        .context("failed to run sudo; run this command with sudo or install sudo")?;
    if status.success() {
        return Ok(());
    }
    anyhow::bail!("sudo command failed with status {status}");
}

fn run_sudo_privileged(command: &str, target: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate current clashtui executable")?;
    let status = Command::new("sudo")
        .arg(exe)
        .arg(command)
        .arg("--path")
        .arg(target)
        .status()
        .context("failed to run sudo; run this command with sudo or install sudo")?;
    if status.success() {
        return Ok(());
    }
    anyhow::bail!("sudo command failed with status {status}");
}

fn run_setcap<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = Command::new("setcap")
        .args(args)
        .output()
        .context("failed to run setcap; install libcap tools first")?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("setcap failed: {}", stderr.trim());
}

fn install_polkit_rule(user: &str) -> Result<()> {
    let path = Path::new(POLKIT_RULE_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, polkit_rule(user))
        .with_context(|| format!("failed to write {}", path.display()))
}

fn remove_polkit_rule() -> Result<()> {
    match fs::remove_file(POLKIT_RULE_PATH) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {POLKIT_RULE_PATH}")),
    }
}

fn polkit_rule_exists() -> bool {
    Path::new(POLKIT_RULE_PATH).exists()
}

fn polkit_rule_matches(user: &str) -> Result<bool> {
    match fs::read_to_string(POLKIT_RULE_PATH) {
        Ok(content) => Ok(content == polkit_rule(user)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to read {POLKIT_RULE_PATH}")),
    }
}

fn polkit_rule(user: &str) -> String {
    format!(
        r#"// Installed by clashtui tun-install.
polkit.addRule(function(action, subject) {{
  var allowed = [
    "org.freedesktop.resolve1.set-domains",
    "org.freedesktop.resolve1.set-default-route",
    "org.freedesktop.resolve1.set-dns-servers",
    "org.freedesktop.resolve1.revert"
  ];
  if (allowed.indexOf(action.id) >= 0 &&
      subject.user == "{}" &&
      subject.local &&
      subject.active) {{
    return polkit.Result.YES;
  }}
}});
"#,
        js_string(user)
    )
}

fn js_string(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            ch => vec![ch],
        })
        .collect()
}

fn invoking_user() -> Result<String> {
    env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("USERNAME"))
        .context("failed to determine invoking user")
}

fn has_tun_capabilities(path: &Path) -> Result<bool> {
    let output = getcap_output(path)?;
    Ok(output.contains("cap_net_admin")
        && output.contains("cap_net_bind_service")
        && output.contains("=ep"))
}

fn has_any_capabilities(path: &Path) -> Result<bool> {
    Ok(!getcap_output(path)?.trim().is_empty())
}

fn print_getcap(path: &Path) -> Result<()> {
    let stdout = getcap_output(path)?;
    if stdout.trim().is_empty() {
        println!("capabilities: none");
    } else {
        print!("{stdout}");
    }
    Ok(())
}

fn getcap_output(path: &Path) -> Result<String> {
    let output = Command::new("getcap")
        .arg(path)
        .output()
        .context("failed to run getcap")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("getcap failed: {}", stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn print_tun_device_status() {
    let tun = Path::new("/dev/net/tun");
    if tun.exists() {
        println!("tun device: {}", tun.display());
    } else {
        println!(
            "tun device: missing {}; load the tun kernel module first",
            tun.display()
        );
    }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}
