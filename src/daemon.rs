use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use tokio::fs;
use tokio::time::sleep;

use crate::config::{AppConfig, Paths};
use crate::mihomo::MihomoClient;
use crate::{core, dns, runtime_profile, subscription, system_proxy, tun};

const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(10);
const STOP_WAIT: Duration = Duration::from_millis(200);
const STOP_RETRIES: usize = 20;

pub async fn start(paths: &Paths, cli_controller: Option<&str>, cli_secret: Option<&str>) -> Result<()> {
    paths.ensure().await?;
    if let Some(pid) = read_pid(paths).await?
        && is_process_running(pid)
    {
        println!("clashtui already running: pid={pid}");
        return Ok(());
    }

    remove_stale_pid(paths).await?;

    let exe = std::env::current_exe().context("failed to locate current executable")?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .with_context(|| format!("failed to open {}", paths.log_file.display()))?;
    let err = log
        .try_clone()
        .with_context(|| format!("failed to clone {}", paths.log_file.display()))?;

    let mut command = Command::new(exe);
    command
        .arg("--daemon-run")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err));

    if let Some(controller) = cli_controller {
        command.args(["--controller", controller]);
    }
    if let Some(secret) = cli_secret {
        command.args(["--secret", secret]);
    }

    prepare_background_command(&mut command);

    let child = command.spawn().context("failed to start clashtui daemon")?;
    let pid = child.id();
    sleep(Duration::from_millis(300)).await;
    if !is_process_running(pid) {
        anyhow::bail!(
            "clashtui daemon exited during startup; check log={}",
            paths.log_file.display()
        );
    }
    println!(
        "clashtui started: pid={} config={} log={}",
        pid,
        paths.config_file.display(),
        paths.log_file.display()
    );
    Ok(())
}

pub async fn run(
    paths: Paths,
    mut config: AppConfig,
    controller_override: Option<String>,
    secret_override: Option<String>,
) -> Result<()> {
    paths.ensure().await?;
    write_pid(&paths).await?;
    let _guard = PidFileGuard::new(paths.pid_file.clone());
    apply_controller_overrides(&mut config, controller_override.as_deref(), secret_override.as_deref());
    let mut client = MihomoClient::new(&config.controller);

    eprintln!("clashtui daemon started pid={}", std::process::id());
    let mut last_config = serialize_config(&config)?;
    let mut needs_apply = true;

    loop {
        if needs_apply {
            match apply_runtime(&paths, &mut config, &client).await {
                Ok(()) => {
                    last_config = serialize_config(&config)?;
                    needs_apply = false;
                }
                Err(err) => eprintln!("failed to apply runtime config, will retry: {err:#}"),
            }
        }

        sleep(CONFIG_RELOAD_INTERVAL).await;
        match AppConfig::load_or_init(&paths).await {
            Ok(mut next_config) => {
                apply_controller_overrides(
                    &mut next_config,
                    controller_override.as_deref(),
                    secret_override.as_deref(),
                );
                match serialize_config(&next_config) {
                    Ok(serialized) if serialized != last_config => {
                        client = MihomoClient::new(&next_config.controller);
                        config = next_config;
                        last_config = serialized;
                        needs_apply = true;
                    }
                    Ok(_) => {}
                    Err(err) => eprintln!("failed to serialize config: {err:#}"),
                }
            }
            Err(err) => eprintln!("failed to load config: {err:#}"),
        }
    }
}

fn apply_controller_overrides(config: &mut AppConfig, controller: Option<&str>, secret: Option<&str>) {
    if let Some(controller) = controller {
        config.controller.url = controller.to_string();
    }
    if let Some(secret) = secret {
        config.controller.secret = Some(secret.to_string());
    }
}

pub async fn stop(paths: &Paths, config: &AppConfig, client: &MihomoClient) -> Result<()> {
    let Some(pid) = read_pid(paths).await? else {
        println!("clashtui is not running");
        return Ok(());
    };

    if !is_process_running(pid) {
        remove_stale_pid(paths).await?;
        println!("clashtui is not running");
        return Ok(());
    }

    terminate_process(pid).with_context(|| format!("failed to stop pid {pid}"))?;
    wait_for_exit(pid).await;
    remove_stale_pid(paths).await?;
    cleanup_runtime(config, client).await;
    core::stop(paths).await?;
    println!("clashtui stopped: pid={pid}");
    Ok(())
}

pub async fn status(paths: &Paths, config: &AppConfig, client: &MihomoClient) -> Result<()> {
    let pid = read_pid(paths).await?;
    let running = pid.is_some_and(is_process_running);
    if let Some(pid) = pid {
        if running {
            println!("daemon: running pid={pid}");
        } else {
            println!("daemon: stale pid={pid}");
            remove_stale_pid(paths).await?;
        }
    } else {
        println!("daemon: stopped");
    }

    println!("config: {}", paths.config_file.display());
    println!("log: {}", paths.log_file.display());
    println!(
        "core: {}",
        core::resolve_core_path(config)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found; set core_path or MIHOMO_CORE".into())
    );
    println!("controller: {}", config.controller.url);
    println!("proxy: {}", config.proxy_port_summary());
    println!(
        "configured: mode={} system_proxy={} tun={} dns={} allow_lan={} proxy_selections={}",
        config.runtime_mode,
        config.system_proxy.enabled,
        config.tun.enable,
        config.dns.enable,
        config.proxy_ports.allow_lan,
        config.proxy_selections.len()
    );
    println!(
        "subscriptions: count={} active={}",
        config.subscriptions.len(),
        config.active_profile.as_deref().unwrap_or("-")
    );

    match client.version().await {
        Ok(version) => println!("mihomo: online version={version}"),
        Err(err) => println!("mihomo: offline error={err}"),
    }
    if let Ok(configs) = client.configs().await {
        let mode = configs
            .get("mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let mixed_port = configs
            .get("mixed-port")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "unknown".to_string(), |port| port.to_string());
        let http_port = configs
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "-".to_string(), |port| port.to_string());
        let socks_port = configs
            .get("socks-port")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| "-".to_string(), |port| port.to_string());
        println!("mihomo-config: mode={mode} mixed-port={mixed_port} port={http_port} socks-port={socks_port}");
    }
    match system_proxy::status() {
        Ok(status) => println!(
            "system-proxy: enabled={} server={} bypass={}",
            status.enabled, status.server, status.bypass
        ),
        Err(err) => println!("system-proxy: unavailable error={err}"),
    }

    Ok(())
}

async fn apply_runtime(paths: &Paths, config: &mut AppConfig, client: &MihomoClient) -> Result<()> {
    let mut errors = Vec::new();

    if client.version().await.is_err() {
        core::ensure_running(paths, config).await?;
        wait_for_mihomo(client).await?;
    }

    if let Err(err) = load_runtime_profile(paths, config, client).await {
        errors.push(format!("profile: {err:#}"));
    }

    if let Err(err) = client.set_mixed_port(config.mixed_port).await {
        errors.push(format!("mixed port: {err:#}"));
    }

    if let Err(err) = client.set_mode(&config.runtime_mode).await {
        errors.push(format!("mode: {err:#}"));
    }

    for (group, proxy) in &config.proxy_selections {
        if let Err(err) = client.select_proxy(group, proxy).await {
            errors.push(format!("proxy {group}: {err:#}"));
        }
    }

    if config.system_proxy.enabled
        && let Err(err) = system_proxy::apply(&config.system_proxy_target())
    {
        errors.push(format!("system proxy: {err:#}"));
    }

    if let Err(err) = tun::apply(client, &config.tun).await {
        errors.push(format!("tun: {err:#}"));
    }

    if let Err(err) = dns::apply(client, &config.dns).await {
        errors.push(format!("dns: {err:#}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

async fn load_runtime_profile(paths: &Paths, config: &mut AppConfig, client: &MihomoClient) -> Result<()> {
    if let Some(active_profile) = config.active_profile.clone() {
        let Some(index) = config
            .subscriptions
            .iter()
            .position(|subscription| subscription.name == active_profile)
        else {
            anyhow::bail!("active profile not found: {active_profile}");
        };

        let sub = config.subscriptions[index].clone();
        let profile = subscription::profile_path(paths, &sub);
        if !profile.exists() {
            subscription::update(paths, config, index).await?;
            config.save(paths).await?;
        }
    }

    let runtime_config = runtime_profile::write_current_config(paths, config).await?;
    client.reload_config(&runtime_config).await
}

async fn cleanup_runtime(config: &AppConfig, client: &MihomoClient) {
    if config.system_proxy.enabled
        && let Err(err) = system_proxy::clear()
    {
        eprintln!("failed to clear system proxy: {err:#}");
    }

    if config.tun.enable {
        let mut tun_config = config.tun.clone();
        tun_config.enable = false;
        if let Err(err) = tun::apply(client, &tun_config).await {
            eprintln!("failed to disable TUN: {err:#}");
        }
    }

    if config.dns.enable {
        let mut dns_config = config.dns.clone();
        dns_config.enable = false;
        if let Err(err) = dns::apply(client, &dns_config).await {
            eprintln!("failed to disable DNS: {err:#}");
        }
    }
}

async fn wait_for_mihomo(client: &MihomoClient) -> Result<()> {
    for _ in 0..STOP_RETRIES {
        if client.version().await.is_ok() {
            return Ok(());
        }
        sleep(STOP_WAIT).await;
    }
    client.version().await.context("mihomo is not ready after core start")?;
    Ok(())
}

async fn read_pid(paths: &Paths) -> Result<Option<u32>> {
    read_pid_file(&paths.pid_file).await
}

async fn read_pid_file(path: &Path) -> Result<Option<u32>> {
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

async fn write_pid(paths: &Paths) -> Result<()> {
    fs::write(&paths.pid_file, std::process::id().to_string())
        .await
        .with_context(|| format!("failed to write {}", paths.pid_file.display()))
}

async fn remove_stale_pid(paths: &Paths) -> Result<()> {
    match fs::remove_file(&paths.pid_file).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", paths.pid_file.display())),
    }
}

fn serialize_config(config: &AppConfig) -> Result<String> {
    serde_yaml_ng::to_string(config).context("failed to serialize config")
}

async fn wait_for_exit(pid: u32) {
    for _ in 0..STOP_RETRIES {
        if !is_process_running(pid) {
            return;
        }
        sleep(STOP_WAIT).await;
    }
}

struct PidFileGuard {
    path: std::path::PathBuf,
}

impl PidFileGuard {
    const fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn prepare_background_command(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
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
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
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
