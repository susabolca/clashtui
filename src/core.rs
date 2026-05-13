#[cfg(target_os = "macos")]
use std::collections::BTreeSet;
use std::env;
use std::fs::OpenOptions;
#[cfg(target_os = "macos")]
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
#[cfg(target_os = "macos")]
use serde_yaml_ng::Value;
use tokio::fs;
use tokio::time::sleep;

use crate::config::{AppConfig, Paths, PortProxyService, RuntimePaths};
use crate::mihomo::MihomoClient;
use crate::port_allocator;
#[cfg(target_os = "macos")]
use crate::privilege;
use crate::runtime_profile;
#[cfg(target_os = "macos")]
use crate::subscription;

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

pub async fn ensure_running(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    let instance = global_instance(paths, config);
    if let Some(pid) = read_pid(&instance.pid_file).await?
        && is_process_running(pid)
    {
        return Ok(());
    }

    remove_pid(&instance.pid_file).await?;
    let Some(core_path) = resolve_core_path(config) else {
        anyhow::bail!(
            "mihomo core is not running and no core binary was found; set core_path in {} or MIHOMO_CORE",
            paths.config_file.display()
        );
    };

    ensure_geodata(&instance.work_dir).await?;
    port_allocator::validate_required_ports_available(config)?;

    #[cfg(target_os = "macos")]
    let tun_device = prepare_macos_tun(paths, config).await?;

    let mut bootstrap_config = config.clone();
    let helper_tun = bootstrap_config.tun.file_descriptor.is_some() && cfg!(target_os = "macos");
    if bootstrap_config.active_profile.is_some() && !helper_tun {
        bootstrap_config.tun.enable = false;
        bootstrap_config.dns.enable = false;
    }
    if bootstrap_config.active_profile.is_some() {
        let active = runtime_profile::write_current_config(paths, &bootstrap_config).await?;
        fs::copy(&active, &instance.config_file)
            .await
            .with_context(|| {
                format!(
                    "failed to copy {} to {}",
                    active.display(),
                    instance.config_file.display()
                )
            })?;
    } else {
        runtime_profile::write_bootstrap_config(paths, &bootstrap_config).await?;
    }
    let result = start_instance(&instance, &core_path, config.tun.file_descriptor).await;
    #[cfg(target_os = "macos")]
    if result.is_err() && tun_device.is_some() {
        let _ = privilege::teardown_tun();
        config.tun.file_descriptor = None;
    }
    #[cfg(target_os = "macos")]
    drop(tun_device);
    result
}

pub async fn ensure_service_running(
    paths: &Paths,
    config: &AppConfig,
    index: usize,
    service: &PortProxyService,
) -> Result<RuntimePaths> {
    let instance = service_instance(paths, config, index, service);
    if let Some(pid) = read_pid(&instance.pid_file).await?
        && is_process_running(pid)
    {
        return Ok(instance);
    }

    remove_pid(&instance.pid_file).await?;
    let Some(core_path) = resolve_core_path(config) else {
        anyhow::bail!(
            "{} mihomo core is not running and no core binary was found; set core_path in {} or MIHOMO_CORE",
            instance.label,
            paths.config_file.display()
        );
    };

    ensure_geodata(&instance.work_dir).await?;
    runtime_profile::write_service_config(paths, &instance, config, service).await?;
    start_instance(&instance, &core_path, None).await?;
    Ok(instance)
}

pub async fn owned_core_running(paths: &Paths) -> Result<bool> {
    Ok(read_pid(&paths.core_pid_file)
        .await?
        .is_some_and(is_process_running))
}

pub async fn ensure_controller_is_owned(
    paths: &Paths,
    config: &AppConfig,
    client: &MihomoClient,
) -> Result<()> {
    if owned_core_running(paths).await? {
        return Ok(());
    }

    if client.version().await.is_ok() {
        anyhow::bail!(
            "mihomo controller {} is online, but clashtui has no owned mihomo pid at {}; refusing to modify an external mihomo instance",
            config.controller.url,
            paths.core_pid_file.display()
        );
    }

    Ok(())
}

pub async fn stop(paths: &Paths) -> Result<()> {
    let instance = paths.global_runtime(String::new());
    stop_instance(&instance).await
}

pub async fn stop_service(
    paths: &Paths,
    config: &AppConfig,
    index: usize,
    service: &PortProxyService,
) -> Result<()> {
    let instance = service_instance(paths, config, index, service);
    stop_instance(&instance).await
}

pub async fn stop_all(paths: &Paths, config: &AppConfig) -> Result<()> {
    stop(paths).await?;
    for (index, service) in config.proxy_ports.services.iter().enumerate() {
        stop_service(paths, config, index, service).await?;
    }
    stop_removed_services(paths, config.proxy_ports.services.len()).await
}

pub async fn stop_removed_services(paths: &Paths, current_count: usize) -> Result<()> {
    stop_stale_service_instances(paths, current_count).await
}

pub fn global_instance(paths: &Paths, config: &AppConfig) -> RuntimePaths {
    paths.global_runtime(config.controller.url.clone())
}

pub fn service_instance(
    paths: &Paths,
    config: &AppConfig,
    index: usize,
    service: &PortProxyService,
) -> RuntimePaths {
    let label = if service.name.trim().is_empty() {
        format!("Port Proxy {}", index + 1)
    } else {
        service.name.clone()
    };
    paths.port_proxy_runtime(
        index,
        label,
        port_allocator::service_controller_url(config, index),
    )
}

async fn stop_instance(instance: &RuntimePaths) -> Result<()> {
    let Some(pid) = read_pid(&instance.pid_file).await? else {
        return Ok(());
    };

    if is_process_running(pid) {
        terminate_process(pid)
            .with_context(|| format!("failed to stop {} mihomo pid {pid}", instance.label))?;
        wait_for_exit(pid).await;
    }
    remove_pid(&instance.pid_file).await
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
        .or_else(resolve_known_app_core)
        .or_else(resolve_path_core)
}

async fn start_instance(
    instance: &RuntimePaths,
    core_path: &Path,
    inherited_fd: Option<i32>,
) -> Result<()> {
    fs::create_dir_all(&instance.work_dir)
        .await
        .with_context(|| format!("failed to create {}", instance.work_dir.display()))?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&instance.log_file)
        .with_context(|| format!("failed to open {}", instance.log_file.display()))?;
    let err = log
        .try_clone()
        .with_context(|| format!("failed to clone {}", instance.log_file.display()))?;

    if let Some(fd) = inherited_fd {
        clear_close_on_exec(fd)?;
    }

    let mut command = Command::new(core_path);
    command
        .args([
            "-d",
            path_to_str(&instance.work_dir)?,
            "-f",
            path_to_str(&instance.config_file)?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));
    prepare_background_command(&mut command);

    let child = command.spawn().with_context(|| {
        format!(
            "failed to start {} mihomo core {}",
            instance.label,
            core_path.display()
        )
    })?;
    let pid = child.id();
    fs::write(&instance.pid_file, pid.to_string())
        .await
        .with_context(|| format!("failed to write {}", instance.pid_file.display()))?;

    sleep(START_WAIT).await;
    if !is_process_running(pid) {
        anyhow::bail!(
            "{} mihomo core exited during startup; check log={}",
            instance.label,
            instance.log_file.display()
        );
    }
    eprintln!(
        "{} mihomo core started pid={} path={} controller={} log={}",
        instance.label,
        pid,
        core_path.display(),
        instance.controller_url,
        instance.log_file.display()
    );
    Ok(())
}

#[cfg(target_os = "macos")]
async fn prepare_macos_tun(
    paths: &Paths,
    config: &mut AppConfig,
) -> Result<Option<privilege::TunDevice>> {
    if !config.tun.enable {
        config.tun.file_descriptor = None;
        config.runtime_interface_name = None;
        return Ok(None);
    }

    let fallback_interface = macos_default_interface().ok();
    let mut tun_config = config.tun.clone();
    match macos_auto_route_excludes(paths, config).await {
        Ok(excludes) => {
            for exclude in excludes {
                if !tun_config.route_exclude_address.contains(&exclude) {
                    tun_config.route_exclude_address.push(exclude);
                }
            }
        }
        Err(err) => eprintln!("failed to collect macOS TUN route excludes: {err:#}"),
    }
    let tun_device =
        privilege::prepare_tun(&tun_config).context("failed to prepare macOS TUN helper")?;
    config.tun.device.clone_from(&tun_device.interface);
    config.tun.file_descriptor = Some(tun_device.file_descriptor());
    config.runtime_interface_name = tun_device.outbound_interface.clone().or(fallback_interface);
    Ok(Some(tun_device))
}

#[cfg(target_os = "macos")]
fn macos_default_interface() -> Result<String> {
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
            if valid_interface_name(interface) {
                return Ok(interface.into());
            }
            anyhow::bail!("route get default returned invalid interface: {interface}");
        }
    }
    anyhow::bail!("route get default did not report an interface")
}

#[cfg(target_os = "macos")]
fn valid_interface_name(interface: &str) -> bool {
    !interface.is_empty()
        && interface.len() < libc::IFNAMSIZ
        && interface
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

#[cfg(target_os = "macos")]
async fn macos_auto_route_excludes(paths: &Paths, config: &AppConfig) -> Result<Vec<String>> {
    let mut hosts = BTreeSet::new();
    collect_dns_hosts(config, &mut hosts);
    collect_active_profile_proxy_hosts(paths, config, &mut hosts).await?;
    Ok(resolve_ipv4_cidrs(&hosts))
}

#[cfg(target_os = "macos")]
fn collect_dns_hosts(config: &AppConfig, hosts: &mut BTreeSet<String>) {
    for endpoint in config
        .dns
        .default_nameserver
        .iter()
        .chain(config.dns.nameserver.iter())
        .chain(config.dns.fallback.iter())
        .chain(config.dns.proxy_server_nameserver.iter())
        .chain(config.dns.direct_nameserver.iter())
        .chain(config.dns.lan_nameserver.iter())
    {
        insert_endpoint_host(hosts, endpoint);
    }
}

#[cfg(target_os = "macos")]
async fn collect_active_profile_proxy_hosts(
    paths: &Paths,
    config: &AppConfig,
    hosts: &mut BTreeSet<String>,
) -> Result<()> {
    let Some(active_profile) = config.active_profile.as_deref() else {
        return Ok(());
    };
    let Some(sub) = config
        .subscriptions
        .iter()
        .find(|sub| sub.name == active_profile)
    else {
        return Ok(());
    };
    let path = subscription::profile_path(paths, sub);
    let content = fs::read_to_string(&path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let profile: Value = serde_yaml_ng::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let Some(proxies) = profile
        .as_mapping()
        .and_then(|mapping| mapping.get("proxies"))
        .and_then(Value::as_sequence)
    else {
        return Ok(());
    };

    for proxy in proxies {
        if let Some(server) = proxy
            .as_mapping()
            .and_then(|mapping| mapping.get("server"))
            .and_then(Value::as_str)
        {
            insert_endpoint_host(hosts, server);
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn insert_endpoint_host(hosts: &mut BTreeSet<String>, endpoint: &str) {
    if let Some(host) = endpoint_host(endpoint) {
        hosts.insert(host);
    }
}

#[cfg(target_os = "macos")]
fn endpoint_host(endpoint: &str) -> Option<String> {
    let mut value = endpoint.trim();
    if value.is_empty() || matches!(value, "system" | "dhcp") {
        return None;
    }
    if let Some((_, rest)) = value.split_once("://") {
        value = rest;
    }
    value = value.split('/').next().unwrap_or(value);
    value = value.rsplit('@').next().unwrap_or(value);
    if value.starts_with('[') {
        return None;
    }
    let host = value.split(':').next().unwrap_or(value).trim();
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

#[cfg(target_os = "macos")]
fn resolve_ipv4_cidrs(hosts: &BTreeSet<String>) -> Vec<String> {
    let mut cidrs = BTreeSet::new();
    for host in hosts {
        if let Ok(addr) = host.parse::<Ipv4Addr>() {
            cidrs.insert(format!("{addr}/32"));
            continue;
        }
        let Ok(addrs) = (host.as_str(), 0).to_socket_addrs() else {
            continue;
        };
        for addr in addrs {
            if let IpAddr::V4(ip) = addr.ip() {
                cidrs.insert(format!("{ip}/32"));
            }
        }
    }
    cidrs.into_iter().collect()
}

async fn ensure_geodata(target_dir: &Path) -> Result<()> {
    fs::create_dir_all(target_dir).await?;
    for file_name in GEODATA_FILES {
        let target = target_dir.join(file_name);
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

async fn stop_stale_service_instances(paths: &Paths, current_count: usize) -> Result<()> {
    let runtimes_dir = paths.config_dir.join("runtimes");
    let mut entries = match fs::read_dir(&runtimes_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("failed to read {}", runtimes_dir.display()));
        }
    };

    while let Some(entry) = entries.next_entry().await? {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let Some(index) = file_name
            .strip_prefix("port-proxy-")
            .and_then(|value| value.parse::<usize>().ok())
            .and_then(|value| value.checked_sub(1))
        else {
            continue;
        };
        if index < current_count {
            continue;
        }
        let dir = entry.path();
        let instance = RuntimePaths {
            id: file_name.clone(),
            label: file_name,
            pid_file: dir.join("mihomo.pid"),
            config_file: dir.join("mihomo-run.yaml"),
            active_config_file: dir.join("mihomo-active.yaml"),
            log_file: dir.join("mihomo.log"),
            work_dir: dir,
            controller_url: String::new(),
        };
        stop_instance(&instance).await?;
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

fn resolve_known_app_core() -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.extend([
            PathBuf::from("/Applications/Clash Verge.app/Contents/MacOS"),
            PathBuf::from("/Applications/Clash Verge Rev.app/Contents/MacOS"),
        ]);
        if let Some(home) = home_dir() {
            dirs.extend([
                home.join("Applications/Clash Verge.app/Contents/MacOS"),
                home.join("Applications/Clash Verge Rev.app/Contents/MacOS"),
            ]);
        }
    }

    dirs.into_iter().find_map(|dir| {
        CORE_NAMES
            .iter()
            .map(|name| dir.join(binary_name(name)))
            .find(|path| path.exists())
    })
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

#[cfg(target_os = "macos")]
fn clear_close_on_exec(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to read fd flags for inherited fd {fd}"));
    }
    let status = unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
    if status != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to clear FD_CLOEXEC for inherited fd {fd}"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clear_close_on_exec(_fd: i32) -> Result<()> {
    Ok(())
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
