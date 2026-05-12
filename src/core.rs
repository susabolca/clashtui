use std::env;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::fs;
use tokio::time::sleep;

use crate::config::{AppConfig, Paths};
use crate::runtime_profile;

const CORE_NAMES: [&str; 4] = ["mihomo", "verge-mihomo", "verge-mihomo-alpha", "clash-meta"];
const GEODATA_FILES: [&str; 4] = ["Country.mmdb", "geoip.metadb", "geoip.dat", "geosite.dat"];
const GEODATA_APP_DIRS: [&str; 5] = [
    "io.github.clash-verge-rev.clash-verge-rev",
    "clash-verge-rev",
    "clash-verge",
    "mihomo",
    "clash",
];
const START_WAIT: Duration = Duration::from_millis(250);
const START_RETRIES: usize = 20;

pub async fn ensure_running(paths: &Paths, config: &AppConfig) -> Result<()> {
    if let Some(pid) = read_pid(&paths.core_pid_file).await?
        && is_process_running(pid)
    {
        return Ok(());
    }

    remove_pid(&paths.core_pid_file).await?;
    let Some(core_path) = resolve_core_path(config) else {
        anyhow::bail!(
            "mihomo core is not running and no core binary was found; set core_path in {} or MIHOMO_CORE",
            paths.config_file.display()
        );
    };

    ensure_geodata(paths).await?;
    let mut bootstrap_config = config.clone();
    if bootstrap_config.active_profile.is_some() {
        bootstrap_config.tun.enable = false;
        bootstrap_config.dns.enable = false;
    }
    runtime_profile::write_bootstrap_config(paths, &bootstrap_config).await?;
    start_core(paths, &core_path).await
}

pub async fn stop(paths: &Paths) -> Result<()> {
    let Some(pid) = read_pid(&paths.core_pid_file).await? else {
        return Ok(());
    };

    if is_process_running(pid) {
        terminate_process(pid).with_context(|| format!("failed to stop mihomo pid {pid}"))?;
        wait_for_exit(pid).await;
    }
    remove_pid(&paths.core_pid_file).await
}

pub fn resolve_core_path(config: &AppConfig) -> Option<PathBuf> {
    config
        .core_path
        .as_deref()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .or_else(|| {
            env::var_os("MIHOMO_CORE")
                .map(PathBuf::from)
                .filter(|path| path.exists())
        })
        .or_else(resolve_sibling_core)
        .or_else(resolve_path_core)
}

async fn start_core(paths: &Paths, core_path: &Path) -> Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.core_log_file)
        .with_context(|| format!("failed to open {}", paths.core_log_file.display()))?;
    let err = log
        .try_clone()
        .with_context(|| format!("failed to clone {}", paths.core_log_file.display()))?;

    let mut command = Command::new(core_path);
    command
        .args([
            "-d",
            path_to_str(&paths.config_dir)?,
            "-f",
            path_to_str(&paths.core_config_file)?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    prepare_background_command(&mut command);

    let child = command
        .spawn()
        .with_context(|| format!("failed to start mihomo core {}", core_path.display()))?;
    let pid = child.id();
    fs::write(&paths.core_pid_file, pid.to_string())
        .await
        .with_context(|| format!("failed to write {}", paths.core_pid_file.display()))?;

    sleep(START_WAIT).await;
    if !is_process_running(pid) {
        anyhow::bail!(
            "mihomo core exited during startup; check log={}",
            paths.core_log_file.display()
        );
    }
    eprintln!(
        "mihomo core started pid={} path={}",
        pid,
        core_path.display()
    );
    Ok(())
}

async fn ensure_geodata(paths: &Paths) -> Result<()> {
    paths.ensure().await?;
    for file_name in GEODATA_FILES {
        let target = paths.config_dir.join(file_name);
        if is_usable_file(&target).await? {
            continue;
        }
        let Some(source) = find_geodata_source(file_name).await? else {
            continue;
        };
        fs::copy(&source, &target).await.with_context(|| {
            format!(
                "failed to copy geodata {} to {}",
                source.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

async fn find_geodata_source(file_name: &str) -> Result<Option<PathBuf>> {
    for dir in geodata_search_dirs() {
        let candidate = dir.join(file_name);
        if is_usable_file(&candidate).await? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn geodata_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = env::var_os("CLASHTUI_GEODATA_DIR") {
        dirs.push(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("MIHOMO_HOME") {
        dirs.push(PathBuf::from(path));
    }

    if cfg!(target_os = "windows") {
        if let Some(base) = env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            push_app_dirs(&mut dirs, &base);
        }
        if let Some(base) = env::var_os("APPDATA").map(PathBuf::from) {
            push_app_dirs(&mut dirs, &base);
        }
    } else if cfg!(target_os = "macos") {
        if let Some(home) = home_dir() {
            let base = home.join("Library").join("Application Support");
            push_app_dirs(&mut dirs, &base);
        }
    } else {
        if let Some(base) = env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
            push_app_dirs(&mut dirs, &base);
        }
        if let Some(home) = home_dir() {
            push_app_dirs(&mut dirs, &home.join(".local").join("share"));
        }
    }

    dirs
}

fn push_app_dirs(dirs: &mut Vec<PathBuf>, base: &Path) {
    for name in GEODATA_APP_DIRS {
        dirs.push(base.join(name));
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

async fn is_usable_file(path: &Path) -> Result<bool> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_file() && metadata.len() > 0),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err).with_context(|| format!("failed to inspect {}", path.display())),
    }
}

fn resolve_sibling_core() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    let dir = exe.parent()?;
    CORE_NAMES
        .iter()
        .map(|name| dir.join(binary_name(name)))
        .find(|path| path.exists())
}

fn resolve_path_core() -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths).find_map(|dir| {
        CORE_NAMES
            .iter()
            .map(|name| dir.join(binary_name(name)))
            .find(|path| path.exists())
    })
}

fn binary_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

async fn read_pid(path: &Path) -> Result<Option<u32>> {
    let content = match fs::read_to_string(path).await {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let pid = content
        .trim()
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(pid))
}

async fn remove_pid(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

async fn wait_for_exit(pid: u32) {
    for _ in 0..START_RETRIES {
        if !is_process_running(pid) {
            return;
        }
        sleep(START_WAIT).await;
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn prepare_background_command(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(target_os = "linux")]
fn prepare_background_command(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    unsafe {
        command.pre_exec(raise_ambient_capabilities_for_core);
    }
}

#[cfg(windows)]
fn prepare_background_command(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(not(any(unix, windows)))]
fn prepare_background_command(_command: &mut Command) {}

#[cfg(target_os = "linux")]
fn raise_ambient_capabilities_for_core() -> std::io::Result<()> {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const CAP_NET_BIND_SERVICE: u32 = 10;
    const CAP_NET_ADMIN: u32 = 12;
    const PR_CAP_AMBIENT: libc::c_int = 47;
    const PR_CAP_AMBIENT_RAISE: libc::c_ulong = 2;

    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
        CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        },
    ];

    let capget_status = unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) };
    if capget_status != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let caps = [CAP_NET_ADMIN, CAP_NET_BIND_SERVICE];
    let mut changed = false;
    for cap in caps {
        let index = (cap / 32) as usize;
        let bit = 1_u32 << (cap % 32);
        if data[index].permitted & bit == 0 {
            continue;
        }
        if data[index].inheritable & bit == 0 {
            data[index].inheritable |= bit;
            changed = true;
        }
    }

    if changed {
        let capset_status = unsafe { libc::syscall(libc::SYS_capset, &mut header, data.as_ptr()) };
        if capset_status != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    for cap in caps {
        let index = (cap / 32) as usize;
        let bit = 1_u32 << (cap % 32);
        if data[index].permitted & bit == 0 {
            continue;
        }
        let prctl_status = unsafe {
            libc::prctl(
                PR_CAP_AMBIENT,
                PR_CAP_AMBIENT_RAISE,
                libc::c_ulong::from(cap),
                0,
                0,
            )
        };
        if prctl_status != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
        })
}

#[cfg(not(any(unix, windows)))]
fn is_process_running(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("kill").arg(pid.to_string()).status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("kill exited with {status}");
    }
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("taskkill exited with {status}");
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process(_pid: u32) -> Result<()> {
    anyhow::bail!("stop is not supported on this platform");
}
