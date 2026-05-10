use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::Value;
use tokio::fs;
use tokio::time::sleep;

use crate::config::{AppConfig, Paths};
use crate::mihomo::MihomoClient;
use crate::{core, dns, privilege, runtime_profile, subscription, system_proxy, tun};

const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(10);
const STARTUP_CHECK_WAIT: Duration = Duration::from_millis(1200);
const STOP_WAIT: Duration = Duration::from_millis(200);
const STOP_RETRIES: usize = 20;
const LOG_TAIL_LINES: usize = 30;

pub async fn start(
    paths: &Paths,
    config: &AppConfig,
    cli_controller: Option<&str>,
    cli_secret: Option<&str>,
) -> Result<()> {
    paths.ensure().await?;
    println!("clashtui start");
    print_static_summary(paths, config);
    print_tun_permission_summary(config);

    if let Some(pid) = read_pid(paths).await?
        && is_process_running(pid)
    {
        println!("clashtui already running: pid={pid}");
        let client = MihomoClient::new(&config.controller);
        print_runtime_summary(config, &client).await;
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
        return Ok(());
    }

    validate_start_permissions(config)?;
    remove_stale_pid(paths).await?;

    let exe = std::env::current_exe().context("failed to locate current executable")?;
    println!("daemon executable: {}", exe.display());
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
    println!("daemon spawned: pid={pid}");
    sleep(STARTUP_CHECK_WAIT).await;
    if !is_process_running(pid) {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
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
    let client = MihomoClient::new(&config.controller);
    print_runtime_summary(config, &client).await;
    print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
    print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
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
    eprintln!(
        "daemon config: config={} core_config={} active_config={} log={} core_log={}",
        paths.config_file.display(),
        paths.core_config_file.display(),
        paths.active_config_file.display(),
        paths.log_file.display(),
        paths.core_log_file.display()
    );
    eprintln!(
        "daemon desired runtime: controller={} proxy={} mode={} system_proxy={} tun={} dns={} active_profile={}",
        config.controller.url,
        config.proxy_port_summary(),
        config.runtime_mode,
        config.system_proxy.enabled,
        config.tun.enable,
        config.dns.enable,
        config.active_profile.as_deref().unwrap_or("-")
    );
    match privilege::current_tun_permission_status() {
        Ok(status) => eprintln!(
            "daemon tun permissions: target={} can_start_tun={} capabilities={} tun_device={} polkit_exists={} polkit_matches_user={}",
            status.target.display(),
            status.can_start_tun(),
            normalize_inline(status.capabilities.trim()),
            status.tun_device_exists,
            status.polkit_rule_exists,
            status.polkit_rule_matches_user
        ),
        Err(err) => eprintln!("daemon tun permissions: check failed: {err:#}"),
    }
    let mut last_config = serialize_config(&config)?;
    let mut needs_apply = true;
    let mut last_health_ok: Option<bool> = None;

    loop {
        if needs_apply {
            eprintln!("runtime apply: begin");
            match apply_runtime(&paths, &mut config, &client).await {
                Ok(()) => {
                    last_config = serialize_config(&config)?;
                    needs_apply = false;
                    last_health_ok = Some(true);
                    eprintln!("runtime apply: ok");
                }
                Err(err) => eprintln!("failed to apply runtime config, will retry: {err:#}"),
            }
        }

        sleep(CONFIG_RELOAD_INTERVAL).await;
        match client.version().await {
            Ok(version) => {
                if last_health_ok != Some(true) {
                    eprintln!("runtime health: mihomo online version={version}");
                }
                last_health_ok = Some(true);
            }
            Err(err) => {
                eprintln!("runtime health: mihomo offline or unhealthy: {err:#}; will reapply");
                last_health_ok = Some(false);
                needs_apply = true;
            }
        }

        match AppConfig::load_or_init(&paths).await {
            Ok(mut next_config) => {
                apply_controller_overrides(
                    &mut next_config,
                    controller_override.as_deref(),
                    secret_override.as_deref(),
                );
                match serialize_config(&next_config) {
                    Ok(serialized) if serialized != last_config => {
                        eprintln!("config changed: reloading desired runtime");
                        client = MihomoClient::new(&next_config.controller);
                        config = next_config;
                        last_config = serialized;
                        last_health_ok = None;
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
    println!("clashtui stop");
    print_static_summary(paths, config);
    let Some(pid) = read_pid(paths).await? else {
        println!("clashtui is not running");
        print_process_summary(paths).await?;
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
        return Ok(());
    };

    if !is_process_running(pid) {
        remove_stale_pid(paths).await?;
        println!("clashtui is not running; removed stale pid={pid}");
        print_process_summary(paths).await?;
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
        return Ok(());
    }

    println!("daemon: stopping pid={pid}");
    terminate_process(pid).with_context(|| format!("failed to stop pid {pid}"))?;
    wait_for_exit(pid).await;
    if is_process_running(pid) {
        println!("daemon: stop requested but process still exists after wait pid={pid}");
    } else {
        println!("daemon: stopped pid={pid}");
    }
    remove_stale_pid(paths).await?;
    println!(
        "runtime cleanup: system_proxy={} tun={} dns={}",
        config.system_proxy.enabled, config.tun.enable, config.dns.enable
    );
    cleanup_runtime(config, client).await;
    println!("mihomo core: stopping if owned by clashtui");
    core::stop(paths).await?;
    println!("clashtui stopped: pid={pid}");
    print_process_summary(paths).await?;
    print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
    print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
    Ok(())
}

pub async fn status(paths: &Paths, config: &AppConfig, client: &MihomoClient) -> Result<()> {
    println!("clashtui status");
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
    print_process_summary(paths).await?;

    print_static_summary(paths, config);
    print_tun_permission_summary(config);
    println!(
        "subscriptions: count={} active={}",
        config.subscriptions.len(),
        config.active_profile.as_deref().unwrap_or("-")
    );

    print_runtime_summary(config, client).await;
    match system_proxy::status() {
        Ok(status) => println!(
            "system-proxy: enabled={} server={} bypass={}",
            status.enabled, status.server, status.bypass
        ),
        Err(err) => println!("system-proxy: unavailable error={err}"),
    }
    print_network_summary(config);
    print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
    print_log_tail("mihomo log", &paths.core_log_file, LOG_TAIL_LINES);

    Ok(())
}

fn print_static_summary(paths: &Paths, config: &AppConfig) {
    println!("config: {}", paths.config_file.display());
    println!("log: {}", paths.log_file.display());
    println!("mihomo-log: {}", paths.core_log_file.display());
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
}

fn print_tun_permission_summary(config: &AppConfig) {
    if !config.tun.enable {
        println!("tun-permission: skipped because tun=false");
        return;
    }

    match privilege::current_tun_permission_status() {
        Ok(status) => {
            println!(
                "tun-permission: target={} can_start_tun={} is_root={} tun_device={}",
                status.target.display(),
                status.can_start_tun(),
                status.is_root,
                status.tun_device_exists
            );
            println!(
                "tun-permission: capabilities={}",
                normalize_inline(status.capabilities.trim())
            );
            println!(
                "tun-permission: polkit_rule={} exists={} matches_user={}",
                status.polkit_rule_path, status.polkit_rule_exists, status.polkit_rule_matches_user
            );
        }
        Err(err) => println!("tun-permission: check failed: {err:#}"),
    }
}

fn validate_start_permissions(config: &AppConfig) -> Result<()> {
    if !config.tun.enable {
        return Ok(());
    }

    let status = privilege::current_tun_permission_status().context("failed to inspect TUN permissions")?;
    if !status.tun_device_exists {
        anyhow::bail!("TUN is enabled but /dev/net/tun is missing; load the tun kernel module before starting");
    }
    if !status.can_start_tun() {
        anyhow::bail!(
            "TUN is enabled but current binary lacks CAP_NET_ADMIN: {}\nrun: sudo {} tun-install",
            status.target.display(),
            status.target.display()
        );
    }
    Ok(())
}

async fn print_runtime_summary(config: &AppConfig, client: &MihomoClient) {
    match client.version().await {
        Ok(version) => println!("mihomo: online version={version}"),
        Err(err) => {
            println!("mihomo: offline error={err}");
            return;
        }
    }

    match client.configs().await {
        Ok(configs) => {
            let mode = configs.get("mode").and_then(Value::as_str).unwrap_or("unknown");
            let mixed_port = configs
                .get("mixed-port")
                .and_then(Value::as_u64)
                .map_or_else(|| "unknown".to_string(), |port| port.to_string());
            let http_port = configs
                .get("port")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_string(), |port| port.to_string());
            let socks_port = configs
                .get("socks-port")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_string(), |port| port.to_string());
            let tun_enabled = nested_bool(&configs, "tun", "enable");
            let tun_device = nested_str(&configs, "tun", "device").unwrap_or("unknown");
            let tun_stack = nested_str(&configs, "tun", "stack").unwrap_or("unknown");
            let dns_enabled = nested_bool(&configs, "dns", "enable");
            let dns_listen = nested_str(&configs, "dns", "listen").unwrap_or("unknown");
            println!("mihomo-config: mode={mode} mixed-port={mixed_port} port={http_port} socks-port={socks_port}");
            println!(
                "mihomo-config: tun.enable={} desired={} device={} stack={}",
                bool_value(tun_enabled),
                config.tun.enable,
                tun_device,
                tun_stack
            );
            println!(
                "mihomo-config: dns.enable={} desired={} listen={}",
                bool_value(dns_enabled),
                config.dns.enable,
                dns_listen
            );
            if Some(config.tun.enable) != tun_enabled {
                println!(
                    "warning: TUN desired={} but mihomo reports {}; check permissions and mihomo log",
                    config.tun.enable,
                    bool_value(tun_enabled)
                );
            }
            if dns_enabled.is_some() && Some(config.dns.enable) != dns_enabled {
                println!(
                    "warning: DNS desired={} but mihomo reports {}; check mihomo log",
                    config.dns.enable,
                    bool_value(dns_enabled)
                );
            }
        }
        Err(err) => println!("mihomo-config: unavailable error={err}"),
    }
}

async fn apply_runtime(paths: &Paths, config: &mut AppConfig, client: &MihomoClient) -> Result<()> {
    let mut errors = Vec::new();
    eprintln!(
        "runtime apply: desired mode={} proxy={} system_proxy={} tun={} dns={} active_profile={}",
        config.runtime_mode,
        config.proxy_port_summary(),
        config.system_proxy.enabled,
        config.tun.enable,
        config.dns.enable,
        config.active_profile.as_deref().unwrap_or("-")
    );

    if client.version().await.is_err() {
        eprintln!("runtime apply: mihomo controller offline; ensuring core is running");
        core::ensure_running(paths, config).await?;
        if let Err(err) = wait_for_mihomo(client).await {
            eprintln!("runtime apply: mihomo controller still unhealthy after wait: {err:#}");
            eprintln!("runtime apply: restarting owned mihomo core");
            core::stop(paths).await?;
            core::ensure_running(paths, config).await?;
            wait_for_mihomo(client).await?;
        }
    }

    if let Err(err) = load_runtime_profile(paths, config, client).await {
        errors.push(format!("profile: {err:#}"));
    } else {
        eprintln!("runtime apply: profile loaded");
    }

    if let Err(err) = client.set_mixed_port(config.mixed_port).await {
        errors.push(format!("mixed port: {err:#}"));
    } else {
        eprintln!("runtime apply: mixed-port={}", config.mixed_port);
    }

    if let Err(err) = client.set_mode(&config.runtime_mode).await {
        errors.push(format!("mode: {err:#}"));
    } else {
        eprintln!("runtime apply: mode={}", config.runtime_mode);
    }

    for (group, proxy) in &config.proxy_selections {
        if let Err(err) = client.select_proxy(group, proxy).await {
            errors.push(format!("proxy {group}: {err:#}"));
        } else {
            eprintln!("runtime apply: proxy selection {group} -> {proxy}");
        }
    }

    if config.system_proxy.enabled
        && let Err(err) = system_proxy::apply(&config.system_proxy_target())
    {
        errors.push(format!("system proxy: {err:#}"));
    } else if config.system_proxy.enabled {
        eprintln!(
            "runtime apply: system proxy -> {}:{}",
            config.proxy_host, config.mixed_port
        );
    }

    if let Err(err) = tun::apply(client, &config.tun).await {
        errors.push(format!("tun: {err:#}"));
    } else {
        eprintln!("runtime apply: tun requested={}", config.tun.enable);
    }

    if let Err(err) = dns::apply(client, &config.dns).await {
        errors.push(format!("dns: {err:#}"));
    } else {
        eprintln!(
            "runtime apply: dns requested={} listen={}",
            config.dns.enable, config.dns.listen
        );
    }

    if let Err(err) = verify_runtime_state(config, client).await {
        errors.push(format!("runtime verify: {err:#}"));
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

async fn verify_runtime_state(config: &AppConfig, client: &MihomoClient) -> Result<()> {
    let configs = client.configs().await.context("failed to read mihomo runtime config")?;
    let tun_enabled = nested_bool(&configs, "tun", "enable");
    let dns_enabled = nested_bool(&configs, "dns", "enable");

    if Some(config.tun.enable) != tun_enabled {
        anyhow::bail!(
            "TUN desired={} but mihomo reports {}; this usually means missing CAP_NET_ADMIN or a TUN setup failure",
            config.tun.enable,
            bool_value(tun_enabled)
        );
    }
    if dns_enabled.is_some() && Some(config.dns.enable) != dns_enabled {
        anyhow::bail!(
            "DNS desired={} but mihomo reports {}",
            config.dns.enable,
            bool_value(dns_enabled)
        );
    }
    Ok(())
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

async fn print_process_summary(paths: &Paths) -> Result<()> {
    print_pid_state("daemon-pid", &paths.pid_file).await?;
    print_pid_state("mihomo-pid", &paths.core_pid_file).await?;
    Ok(())
}

async fn print_pid_state(label: &str, path: &Path) -> Result<()> {
    match read_pid_file(path).await? {
        Some(pid) if is_process_running(pid) => println!("{label}: running pid={pid} file={}", path.display()),
        Some(pid) => println!("{label}: stale pid={pid} file={}", path.display()),
        None => println!("{label}: missing file={}", path.display()),
    }
    Ok(())
}

fn print_network_summary(config: &AppConfig) {
    if !config.tun.enable {
        return;
    }

    println!("network: expected tun device={}", config.tun.device);
    print_command_summary("network/ip-addr", "ip", &["addr", "show", "dev", &config.tun.device]);
    print_command_summary("network/ip-rule", "ip", &["rule", "show"]);
    print_command_summary("network/ip-route-default", "ip", &["route", "show", "default"]);
}

fn print_command_summary(label: &str, command: &str, args: &[&str]) {
    match Command::new(command).args(args).output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let value = stdout.trim();
            if value.is_empty() {
                println!("{label}: empty");
            } else {
                for line in value.lines().take(12) {
                    println!("{label}: {line}");
                }
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("{label}: command failed status={} {}", output.status, stderr.trim());
        }
        Err(err) => println!("{label}: unavailable error={err}"),
    }
}

fn print_log_tail(label: &str, path: &Path, lines: usize) {
    println!("--- {label}: {} (last {lines} lines) ---", path.display());
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let tail = tail_lines(&content, lines);
            if tail.is_empty() {
                println!("(empty)");
            } else {
                for line in tail {
                    println!("{line}");
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => println!("(missing)"),
        Err(err) => println!("(failed to read: {err})"),
    }
}

fn tail_lines(content: &str, limit: usize) -> Vec<&str> {
    let mut lines = content.lines().rev().take(limit).collect::<Vec<_>>();
    lines.reverse();
    lines
}

fn nested_bool(value: &Value, section: &str, key: &str) -> Option<bool> {
    value.get(section)?.get(key)?.as_bool()
}

fn nested_str<'a>(value: &'a Value, section: &str, key: &str) -> Option<&'a str> {
    value.get(section)?.get(key)?.as_str()
}

fn bool_value(value: Option<bool>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

fn normalize_inline(value: &str) -> String {
    if value.is_empty() {
        "none".into()
    } else {
        value.replace('\n', " | ")
    }
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
