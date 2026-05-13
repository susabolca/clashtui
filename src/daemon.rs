use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use serde_json::Value;
use tokio::fs;
use tokio::time::sleep;

use crate::config::{AppConfig, ControllerConfig, Paths, PortProxyService, RuntimePaths};
use crate::mihomo::MihomoClient;
use crate::{
    core, dns, port_allocator, privilege, runtime_profile, subscription, system_proxy, tun,
};

const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(10);
const STARTUP_CHECK_WAIT: Duration = Duration::from_millis(1200);
const STOP_WAIT: Duration = Duration::from_millis(200);
const STOP_RETRIES: usize = 20;
const LOG_TAIL_LINES: usize = 30;

pub async fn start(
    paths: &Paths,
    config: &mut AppConfig,
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
        if !print_runtime_summary(config, &client).await {
            print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
            print_mihomo_log_tails(paths, config);
        }
        return Ok(());
    }

    validate_start_permissions(config)?;
    remove_stale_pid(paths).await?;
    if port_allocator::ensure_allocated_with_controller(
        paths,
        config,
        cli_controller.is_none(),
        true,
    )
    .await?
    {
        config.save(paths).await?;
    }
    port_allocator::validate_required_ports_available(config)?;

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
        print_mihomo_log_tails(paths, config);
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
    if !print_runtime_summary(config, &client).await {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
    }
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
    apply_controller_overrides(
        &mut config,
        controller_override.as_deref(),
        secret_override.as_deref(),
    );
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
            "daemon tun permissions: target={} can_start_tun={} legacy_file_capabilities_detected={} legacy_capabilities={} tun_device={} polkit_exists={} polkit_matches_user={}",
            status.target.display(),
            status.can_start_tun(),
            status.legacy_file_capabilities_detected,
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
            match apply_runtime(&paths, &mut config).await {
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
        match runtime_health(&paths, &config).await {
            Ok(()) => {
                if last_health_ok != Some(true) {
                    eprintln!("runtime health: all mihomo instances online");
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
                match port_allocator::ensure_allocated_with_controller(
                    &paths,
                    &mut next_config,
                    controller_override.is_none(),
                    false,
                )
                .await
                {
                    Ok(true) => {
                        if let Err(err) = next_config.save(&paths).await {
                            eprintln!("failed to save allocated ports: {err:#}");
                        }
                    }
                    Ok(false) => {}
                    Err(err) => eprintln!("failed to allocate ports: {err:#}"),
                }
                apply_controller_overrides(
                    &mut next_config,
                    controller_override.as_deref(),
                    secret_override.as_deref(),
                );
                preserve_runtime_state(&config, &mut next_config);
                match serialize_config(&next_config) {
                    Ok(serialized) if serialized != last_config => {
                        eprintln!("config changed: reloading desired runtime");
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

fn apply_controller_overrides(
    config: &mut AppConfig,
    controller: Option<&str>,
    secret: Option<&str>,
) {
    if let Some(controller) = controller {
        config.controller.url = controller.to_string();
    }
    if let Some(secret) = secret {
        config.controller.secret = Some(secret.to_string());
    }
}

fn preserve_runtime_state(current: &AppConfig, next: &mut AppConfig) {
    #[cfg(target_os = "macos")]
    if current.tun.enable && next.tun.enable {
        next.tun.file_descriptor = current.tun.file_descriptor;
        next.runtime_interface_name = current.runtime_interface_name.clone();
        if current.tun.file_descriptor.is_some() && matches!(next.tun.device.trim(), "" | "utun") {
            next.tun.device.clone_from(&current.tun.device);
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = (current, next);
}

#[cfg(target_os = "macos")]
fn macos_tun_needs_core_restart(config: &AppConfig) -> bool {
    config.tun.enable && config.tun.file_descriptor.is_none()
}

#[cfg(not(target_os = "macos"))]
fn macos_tun_needs_core_restart(_config: &AppConfig) -> bool {
    false
}

fn teardown_inactive_macos_tun(config: &mut AppConfig) {
    #[cfg(target_os = "macos")]
    {
        if config.tun.enable {
            return;
        }
        config.tun.file_descriptor = None;
        config.runtime_interface_name = None;
        let Ok(status) = privilege::current_tun_permission_status() else {
            return;
        };
        if status.polkit_rule_matches_user
            && let Err(err) = privilege::teardown_tun()
        {
            eprintln!("failed to teardown inactive macOS TUN helper state: {err:#}");
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = config;
}

pub async fn stop(paths: &Paths, config: &AppConfig, client: &MihomoClient) -> Result<()> {
    println!("clashtui stop");
    print_static_summary(paths, config);
    let Some(pid) = read_pid(paths).await? else {
        println!("clashtui is not running");
        cleanup_owned_runtimes(paths, config, client).await?;
        print_process_summary(paths, config).await?;
        return Ok(());
    };

    if !is_process_running(pid) {
        remove_stale_pid(paths).await?;
        println!("clashtui is not running; removed stale pid={pid}");
        cleanup_owned_runtimes(paths, config, client).await?;
        print_process_summary(paths, config).await?;
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
    cleanup_owned_runtimes(paths, config, client).await?;
    println!("clashtui stopped: pid={pid}");
    print_process_summary(paths, config).await?;
    if is_process_running(pid) {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
    }
    Ok(())
}

async fn cleanup_owned_runtimes(
    paths: &Paths,
    config: &AppConfig,
    client: &MihomoClient,
) -> Result<()> {
    if config.system_proxy.enabled
        && let Err(err) = system_proxy::clear()
    {
        eprintln!("failed to clear system proxy: {err:#}");
    }

    if core::owned_core_running(paths).await? {
        cleanup_runtime(config, client).await;
    } else {
        println!(
            "runtime cleanup: skipped global mihomo cleanup because no clashtui-owned global core is running"
        );
    }
    println!("mihomo core: stopping all instances owned by clashtui");
    let stop_result = core::stop_all(paths, config).await;
    #[cfg(target_os = "macos")]
    if config.tun.enable
        && let Err(err) = privilege::teardown_tun()
    {
        eprintln!("failed to teardown macOS TUN helper state: {err:#}");
    }
    stop_result
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
    print_process_summary(paths, config).await?;

    print_static_summary(paths, config);
    print_tun_permission_summary(config);
    println!(
        "subscriptions: count={} active={}",
        config.subscriptions.len(),
        config.active_profile.as_deref().unwrap_or("-")
    );

    let runtime_healthy = print_runtime_summary(config, client).await;
    match system_proxy::status() {
        Ok(status) => println!(
            "system-proxy: enabled={} server={} bypass={}",
            status.enabled, status.server, status.bypass
        ),
        Err(err) => println!("system-proxy: unavailable error={err}"),
    }
    print_network_summary(config, running);
    if running && !runtime_healthy {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
    } else {
        println!(
            "logs: daemon={} mihomo={}",
            paths.log_file.display(),
            paths.core_log_file.display()
        );
    }

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
                "tun-permission: target={} can_start_tun={} legacy_file_capabilities_detected={} is_root={} tun_device={}",
                status.target.display(),
                status.can_start_tun(),
                status.legacy_file_capabilities_detected,
                status.is_root,
                status.tun_device_exists
            );
            println!(
                "tun-permission: legacy_capabilities={}",
                normalize_inline(status.capabilities.trim())
            );
            println!(
                "tun-permission: {}={} exists={} matches_user={}",
                tun_permission_rule_label(),
                status.polkit_rule_path,
                status.polkit_rule_exists,
                status.polkit_rule_matches_user
            );
            #[cfg(target_os = "linux")]
            println!(
                "tun-permission: helper installed={} reachable={} binary={} service={} socket={}",
                status.helper_installed,
                status.helper_reachable,
                status.helper_binary_path,
                status.helper_service_path,
                status.helper_socket_path
            );
            #[cfg(target_os = "linux")]
            if status.helper_reachable && !linux_tun_experimental_routes_enabled() {
                println!(
                    "tun-permission: linux helper route activation=guarded reason=cgroup/fwmark-policy-pending"
                );
            }
            if !status.can_start_tun() {
                print_tun_permission_hint(&status);
            }
        }
        Err(err) => println!("tun-permission: check failed: {err:#}"),
    }
}

#[cfg(target_os = "macos")]
fn print_tun_permission_hint(status: &privilege::TunPermissionStatus) {
    println!(
        "tun-permission: macOS TUN needs the clashtui root helper; current process is_root={}",
        status.is_root
    );
    println!(
        "tun-permission: run {} tun-install to install or repair the helper",
        status.target.display()
    );
    println!("tun-permission: mihomo remains user-mode; the helper owns utun/routes only.");
}

#[cfg(target_os = "linux")]
fn print_tun_permission_hint(status: &privilege::TunPermissionStatus) {
    println!(
        "tun-permission: run {} tun-install to install or repair the Linux TUN helper",
        status.target.display()
    );
    println!("tun-permission: Linux TUN requires the helper; mihomo stays user-mode.");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn print_tun_permission_hint(_status: &privilege::TunPermissionStatus) {
    println!("tun-permission: TUN is not supported on this platform.");
}

fn validate_start_permissions(config: &AppConfig) -> Result<()> {
    if !config.tun.enable {
        return Ok(());
    }

    let status =
        privilege::current_tun_permission_status().context("failed to inspect TUN permissions")?;
    if !status.tun_device_exists {
        anyhow::bail!(
            "TUN is enabled but /dev/net/tun is missing; load the tun kernel module before starting"
        );
    }
    if !status.can_start_tun() {
        #[cfg(target_os = "macos")]
        {
            println!(
                "warning: TUN is enabled but the macOS TUN helper is missing or unreachable; run {} tun-install",
                status.target.display()
            );
            println!("warning: Port Proxy/system proxy can still run without TUN.");
            return Ok(());
        }

        #[cfg(not(target_os = "macos"))]
        anyhow::bail!(
            "TUN is enabled but the TUN helper is missing or unreachable: {}\nrun: {} tun-install",
            status.target.display(),
            status.target.display()
        );
    }
    #[cfg(target_os = "linux")]
    if status.helper_reachable && !linux_tun_experimental_routes_enabled() {
        anyhow::bail!(
            "Linux TUN helper route activation is guarded until cgroup/fwmark loop prevention is implemented; use scripts/tun_guarded_test.sh for validation or set CLASHTUI_LINUX_TUN_EXPERIMENTAL_ROUTES=1 for a guarded manual run"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_tun_experimental_routes_enabled() -> bool {
    std::env::var("CLASHTUI_LINUX_TUN_EXPERIMENTAL_ROUTES").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

async fn print_runtime_summary(config: &AppConfig, client: &MihomoClient) -> bool {
    match client.version().await {
        Ok(version) => println!("mihomo: online version={version}"),
        Err(err) => {
            println!("mihomo: offline error={err}");
            return false;
        }
    }

    let config_healthy = match client.configs().await {
        Ok(configs) => {
            let mut healthy = true;
            let mode = configs
                .get("mode")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
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
            println!(
                "mihomo-config: mode={mode} mixed-port={mixed_port} port={http_port} socks-port={socks_port}"
            );
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
                healthy = false;
                println!(
                    "warning: TUN desired={} but mihomo reports {}; check permissions and mihomo log",
                    config.tun.enable,
                    bool_value(tun_enabled)
                );
            }
            if dns_enabled.is_some() && Some(config.dns.enable) != dns_enabled {
                healthy = false;
                println!(
                    "warning: DNS desired={} but mihomo reports {}; check mihomo log",
                    config.dns.enable,
                    bool_value(dns_enabled)
                );
            }
            healthy
        }
        Err(err) => {
            println!("mihomo-config: unavailable error={err}");
            false
        }
    };

    print_mihomo_metrics_summary(client).await;
    config_healthy
}

async fn print_mihomo_metrics_summary(client: &MihomoClient) {
    match client.traffic().await {
        Ok(traffic) => {
            let up = json_u64(&traffic, "up").unwrap_or_default();
            let down = json_u64(&traffic, "down").unwrap_or_default();
            let up_total = json_u64(&traffic, "upTotal");
            let down_total = json_u64(&traffic, "downTotal");
            println!(
                "mihomo-traffic: up={}/s down={}/s upTotal={} downTotal={}",
                format_bytes_short(up),
                format_bytes_short(down),
                format_optional_bytes_short(up_total),
                format_optional_bytes_short(down_total)
            );
        }
        Err(err) => println!("mihomo-traffic: unavailable error={err}"),
    }

    match client.connections().await {
        Ok(connections) => {
            let active = connections
                .get("connections")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let upload_total = json_u64(&connections, "uploadTotal").unwrap_or_default();
            let download_total = json_u64(&connections, "downloadTotal").unwrap_or_default();
            println!(
                "mihomo-connections: active={} uploadTotal={} downloadTotal={}",
                active,
                format_bytes_short(upload_total),
                format_bytes_short(download_total)
            );
        }
        Err(err) => println!("mihomo-connections: unavailable error={err}"),
    }
}

fn json_u64(value: &Value, key: &str) -> Option<u64> {
    value
        .get(key)
        .and_then(|value| value.as_u64().or_else(|| value.as_i64()?.try_into().ok()))
}

fn format_optional_bytes_short(bytes: Option<u64>) -> String {
    bytes.map(format_bytes_short).unwrap_or_else(|| "-".into())
}

fn format_bytes_short(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

async fn apply_runtime(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    let mut errors = Vec::new();
    eprintln!(
        "runtime apply: desired mode={} proxy={} system_proxy={} tun={} dns={} active_profile={} port_proxies={}",
        config.runtime_mode,
        config.proxy_port_summary(),
        config.system_proxy.enabled,
        config.tun.enable,
        config.dns.enable,
        config.active_profile.as_deref().unwrap_or("-"),
        config
            .proxy_ports
            .services
            .iter()
            .filter(|service| service.enabled)
            .count()
    );

    if let Err(err) = apply_global_runtime(paths, config).await {
        errors.push(format!("global: {err:#}"));
    }
    if let Err(err) = apply_port_proxy_runtimes(paths, config).await {
        errors.push(format!("port proxies: {err:#}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

async fn apply_global_runtime(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    let client = MihomoClient::new(&config.controller);
    let mut errors = Vec::new();

    if core::owned_core_running(paths).await? {
        if macos_tun_needs_core_restart(config) {
            eprintln!("runtime apply: macOS TUN needs inherited helper fd; restarting core");
            core::stop(paths).await?;
            core::ensure_running(paths, config).await?;
            wait_for_mihomo(&client).await?;
        } else if client.version().await.is_err() {
            eprintln!("runtime apply: owned global mihomo controller unhealthy; restarting core");
            core::stop(paths).await?;
            core::ensure_running(paths, config).await?;
            wait_for_mihomo(&client).await?;
        }
    } else if client.version().await.is_ok() {
        anyhow::bail!(
            "mihomo controller {} is online but is not owned by clashtui; refusing to modify external mihomo",
            config.controller.url
        );
    } else {
        eprintln!("runtime apply: owned global mihomo is not running; ensuring core is running");
        core::ensure_running(paths, config).await?;
        if let Err(err) = wait_for_mihomo(&client).await {
            eprintln!(
                "runtime apply: global mihomo controller still unhealthy after wait: {err:#}"
            );
            eprintln!("runtime apply: restarting owned global mihomo core");
            core::stop(paths).await?;
            core::ensure_running(paths, config).await?;
            wait_for_mihomo(&client).await?;
        }
    }

    if let Err(err) = load_runtime_profile(paths, config, &client).await {
        errors.push(format!("profile: {err:#}"));
    } else {
        eprintln!("runtime apply: global profile loaded");
    }

    if let Err(err) = client.set_mixed_port(config.mixed_port).await {
        errors.push(format!("mixed port: {err:#}"));
    } else {
        eprintln!("runtime apply: global mixed-port={}", config.mixed_port);
    }

    if let Err(err) = client.set_mode(&config.runtime_mode).await {
        errors.push(format!("mode: {err:#}"));
    } else {
        eprintln!("runtime apply: global mode={}", config.runtime_mode);
    }

    if let Err(err) = apply_proxy_selections(
        &client,
        "Global Proxy",
        desired_global_proxy_selections(config),
    )
    .await
    {
        errors.push(format!("proxy selection: {err:#}"));
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

    if let Err(err) = tun::apply(&client, &config.tun).await {
        errors.push(format!("tun: {err:#}"));
    } else {
        eprintln!("runtime apply: tun requested={}", config.tun.enable);
    }
    teardown_inactive_macos_tun(config);

    if let Err(err) = dns::apply(&client, &config.dns).await {
        errors.push(format!("dns: {err:#}"));
    } else {
        eprintln!(
            "runtime apply: dns requested={} listen={}",
            config.dns.enable, config.dns.listen
        );
    }

    if let Err(err) = verify_runtime_state(config, &client).await {
        errors.push(format!("runtime verify: {err:#}"));
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

async fn apply_port_proxy_runtimes(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    let mut errors = Vec::new();
    let services = config.proxy_ports.services.clone();
    for (index, service) in services.iter().enumerate() {
        if !service.enabled {
            if let Err(err) = core::stop_service(paths, config, index, service).await {
                errors.push(format!("{} stop: {err:#}", service_name(index, service)));
            }
            continue;
        }
        if let Err(err) = apply_port_proxy_runtime(paths, config, index, service).await {
            errors.push(format!("{}: {err:#}", service_name(index, service)));
        }
    }
    if let Err(err) = core::stop_removed_services(paths, services.len()).await {
        errors.push(format!("removed services cleanup: {err:#}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

async fn apply_port_proxy_runtime(
    paths: &Paths,
    config: &mut AppConfig,
    index: usize,
    service: &PortProxyService,
) -> Result<()> {
    let profile_name = service
        .subscription
        .as_deref()
        .or(config.active_profile.as_deref())
        .map(str::to_string);
    if let Some(profile_name) = profile_name {
        ensure_subscription_profile(paths, config, &profile_name).await?;
    }

    let instance = core::ensure_service_running(paths, config, index, service).await?;
    let client = mihomo_client_for_instance(config, &instance);
    if let Err(err) = wait_for_mihomo(&client).await {
        eprintln!(
            "runtime apply: {} controller unhealthy after wait: {err:#}; restarting",
            instance.label
        );
        core::stop_service(paths, config, index, service).await?;
        core::ensure_service_running(paths, config, index, service).await?;
        wait_for_mihomo(&client).await?;
    }

    runtime_profile::write_service_config(paths, &instance, config, service).await?;
    client.reload_config(&instance.config_file).await?;
    client.set_mode(&service.mode).await?;
    apply_proxy_selections(
        &client,
        &instance.label,
        desired_service_proxy_selections(service),
    )
    .await?;
    eprintln!(
        "runtime apply: {} ready controller={} listen={}:{} mode={} log={}",
        instance.label,
        instance.controller_url,
        if service.listen.trim().is_empty() {
            "127.0.0.1"
        } else {
            service.listen.trim()
        },
        service.port,
        service.mode,
        instance.log_file.display()
    );
    Ok(())
}

fn desired_global_proxy_selections(config: &AppConfig) -> Vec<(&str, &str)> {
    let mut selections = config
        .proxy_selections
        .iter()
        .map(|(group, proxy)| (group.as_str(), proxy.as_str()))
        .collect::<Vec<_>>();

    if config.runtime_mode.eq_ignore_ascii_case("rule")
        && let Some(active_profile) = config.active_profile.as_ref()
        && let Some(subscription) = config
            .subscriptions
            .iter()
            .find(|subscription| &subscription.name == active_profile)
    {
        for (group, proxy) in &subscription.rule_selections {
            if let Some(existing) = selections
                .iter_mut()
                .find(|(existing_group, _)| existing_group == group)
            {
                *existing = (group.as_str(), proxy.as_str());
            } else {
                selections.push((group.as_str(), proxy.as_str()));
            }
        }
    }

    selections
}

fn desired_service_proxy_selections(service: &PortProxyService) -> Vec<(&str, &str)> {
    if service.mode.eq_ignore_ascii_case("global")
        && let Some(proxy) = service.proxy.as_deref().filter(|value| !value.is_empty())
    {
        return vec![("GLOBAL", proxy)];
    }
    if service.mode.eq_ignore_ascii_case("rule") {
        return service
            .rule_selections
            .iter()
            .map(|(group, proxy)| (group.as_str(), proxy.as_str()))
            .collect();
    }
    Vec::new()
}

async fn apply_proxy_selections(
    client: &MihomoClient,
    label: &str,
    selections: Vec<(&str, &str)>,
) -> Result<()> {
    if selections.is_empty() {
        return Ok(());
    }

    let groups = client
        .proxy_groups()
        .await
        .with_context(|| format!("{label} proxy groups unavailable"))?;
    for (group, proxy) in selections {
        let Some(proxy_group) = groups.iter().find(|candidate| candidate.name == group) else {
            eprintln!("runtime apply: {label} proxy selection skipped; group not found {group}");
            continue;
        };
        if !proxy_group.all.iter().any(|candidate| candidate == proxy) {
            eprintln!(
                "runtime apply: {label} proxy selection skipped; proxy not found {group} -> {proxy}"
            );
            continue;
        }
        client
            .select_proxy(group, proxy)
            .await
            .with_context(|| format!("{label} proxy selection {group} -> {proxy}"))?;
        eprintln!("runtime apply: {label} proxy selection {group} -> {proxy}");
    }
    Ok(())
}

async fn load_runtime_profile(
    paths: &Paths,
    config: &mut AppConfig,
    client: &MihomoClient,
) -> Result<()> {
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
            subscription::update_preserving_last_good(paths, config, index).await?;
            config.save(paths).await?;
        }
    }

    let runtime_config = runtime_profile::write_current_config(paths, config).await?;
    client.reload_config(&runtime_config).await
}

async fn ensure_subscription_profile(
    paths: &Paths,
    config: &mut AppConfig,
    profile_name: &str,
) -> Result<()> {
    let Some(index) = config
        .subscriptions
        .iter()
        .position(|subscription| subscription.name == profile_name)
    else {
        anyhow::bail!("subscription profile not found: {profile_name}");
    };
    let sub = config.subscriptions[index].clone();
    let profile = subscription::profile_path(paths, &sub);
    if !profile.exists() {
        subscription::update_preserving_last_good(paths, config, index).await?;
        config.save(paths).await?;
    }
    Ok(())
}

async fn runtime_health(paths: &Paths, config: &AppConfig) -> Result<()> {
    let global_client = MihomoClient::new(&config.controller);
    global_client
        .version()
        .await
        .context("global mihomo offline")?;

    for (index, service) in config
        .proxy_ports
        .services
        .iter()
        .enumerate()
        .filter(|(_, service)| service.enabled)
    {
        let instance = core::service_instance(paths, config, index, service);
        mihomo_client_for_instance(config, &instance)
            .version()
            .await
            .with_context(|| format!("{} mihomo offline", instance.label))?;
    }
    Ok(())
}

fn mihomo_client_for_instance(config: &AppConfig, instance: &RuntimePaths) -> MihomoClient {
    MihomoClient::new(&ControllerConfig {
        url: instance.controller_url.clone(),
        secret: config.controller.secret.clone(),
    })
}

fn service_name(index: usize, service: &PortProxyService) -> String {
    if service.name.trim().is_empty() {
        format!("Port Proxy {}", index + 1)
    } else {
        service.name.clone()
    }
}

async fn verify_runtime_state(config: &AppConfig, client: &MihomoClient) -> Result<()> {
    let configs = client
        .configs()
        .await
        .context("failed to read mihomo runtime config")?;
    let tun_enabled = nested_bool(&configs, "tun", "enable");
    let dns_enabled = nested_bool(&configs, "dns", "enable");

    if Some(config.tun.enable) != tun_enabled {
        eprintln!(
            "runtime warning: TUN desired={} but mihomo reports {}; this usually means missing privileges or a TUN setup failure",
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
    client
        .version()
        .await
        .context("mihomo is not ready after core start")?;
    Ok(())
}

async fn print_process_summary(paths: &Paths, config: &AppConfig) -> Result<()> {
    print_pid_state("daemon-pid", &paths.pid_file).await?;
    print_pid_state("global-mihomo-pid", &paths.core_pid_file).await?;
    for (index, service) in config.proxy_ports.services.iter().enumerate() {
        let instance = core::service_instance(paths, config, index, service);
        print_pid_state(&format!("{}-pid", instance.id), &instance.pid_file).await?;
    }
    Ok(())
}

async fn print_pid_state(label: &str, path: &Path) -> Result<()> {
    match read_pid_file(path).await? {
        Some(pid) if is_process_running(pid) => {
            println!("{label}: running pid={pid}")
        }
        Some(pid) => println!("{label}: stale pid={pid} file={}", path.display()),
        None => println!("{label}: stopped"),
    }
    Ok(())
}

fn print_network_summary(config: &AppConfig, runtime_running: bool) {
    if !config.tun.enable {
        return;
    }
    if !runtime_running {
        println!("network: skipped because daemon is stopped");
        return;
    }

    println!("network: expected tun device={}", config.tun.device);
    print_platform_network_summary(config);
}

#[cfg(target_os = "linux")]
fn print_platform_network_summary(config: &AppConfig) {
    print_command_summary(
        "network/ip-addr",
        "ip",
        &["addr", "show", "dev", &config.tun.device],
    );
    print_command_summary("network/ip-rule", "ip", &["rule", "show"]);
    print_command_summary(
        "network/ip-route-default",
        "ip",
        &["route", "show", "default"],
    );
}

#[cfg(target_os = "macos")]
fn print_platform_network_summary(config: &AppConfig) {
    print_command_summary(
        "network/ifconfig-tun",
        "ifconfig",
        &[config.tun.device.as_str()],
    );
    print_command_summary("network/route-default", "route", &["-n", "get", "default"]);
    print_command_summary("network/netstat-default", "netstat", &["-rn", "-f", "inet"]);
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn print_platform_network_summary(_config: &AppConfig) {
    println!("network: platform-specific TUN route inspection is not implemented");
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
            println!(
                "{label}: command failed status={} {}",
                output.status,
                stderr.trim()
            );
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

fn print_mihomo_log_tails(paths: &Paths, config: &AppConfig) {
    print_log_tail("global mihomo log", &paths.core_log_file, LOG_TAIL_LINES);
    for (index, service) in config.proxy_ports.services.iter().enumerate() {
        let instance = core::service_instance(paths, config, index, service);
        print_log_tail(
            &format!("{} mihomo log", instance.label),
            &instance.log_file,
            LOG_TAIL_LINES,
        );
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

#[cfg(target_os = "macos")]
fn tun_permission_rule_label() -> &'static str {
    "launchdaemon"
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn tun_permission_rule_label() -> &'static str {
    "polkit_rule"
}

#[cfg(target_os = "linux")]
fn tun_permission_rule_label() -> &'static str {
    "systemd_service"
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
        Err(err) => {
            Err(err).with_context(|| format!("failed to remove {}", paths.pid_file.display()))
        }
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
