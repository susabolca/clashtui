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
    autostart, core, dns, port_allocator, runtime_profile, service, subscription, system_proxy, tun,
};

const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(10);
const STARTUP_CHECK_WAIT: Duration = Duration::from_millis(1200);
const STOP_WAIT: Duration = Duration::from_millis(200);
const STOP_RETRIES: usize = 75;
const PORT_RELEASE_RETRIES: usize = 75;
const LOG_TAIL_LINES: usize = 30;

pub async fn start(
    paths: &Paths,
    config: &mut AppConfig,
    cli_controller: Option<&str>,
    cli_secret: Option<&str>,
    verbose: bool,
) -> Result<()> {
    paths.ensure().await?;
    println!("clashtui start");
    print_service_summary(config, verbose);
    sync_autostart(paths, config, verbose)?;

    if let Some(pid) = read_pid(paths).await?
        && is_process_running(pid)
    {
        if verbose {
            print_static_summary(paths, config);
        }
        println!("daemon: already running pid={pid}");
        let client = MihomoClient::new(&config.controller);
        if !print_runtime_summary(config, &client, verbose).await {
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
    let core_path = core::ensure_core_path(paths, config).await?;
    if verbose {
        print_static_summary(paths, config);
        println!("mihomo core: ready path={}", core_path.display());
    }
    if config.use_service_runtime() {
        if verbose {
            println!("runtime cleanup: stopping legacy runtimes before service start");
        }
        core::stop_all(paths, config).await?;
    } else if config.use_single_runtime() {
        core::stop_removed_services(paths, 0).await?;
    }
    wait_for_required_ports_available(config).await?;

    let exe = std::env::current_exe().context("failed to locate current executable")?;
    if verbose {
        println!("daemon executable: {}", exe.display());
    }
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
    if verbose {
        println!("daemon spawned: pid={pid}");
    }
    sleep(STARTUP_CHECK_WAIT).await;
    if !is_process_running(pid) {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
        anyhow::bail!(
            "clashtui daemon exited during startup; check log={}",
            paths.log_file.display()
        );
    }
    if verbose {
        println!(
            "clashtui started: pid={} config={} log={}",
            pid,
            paths.config_file.display(),
            paths.log_file.display()
        );
    } else {
        println!("daemon: started pid={pid}");
    }
    let client = MihomoClient::new(&config.controller);
    if let Err(err) = wait_for_mihomo(&client).await {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
        return Err(err).context("mihomo runtime did not become ready after daemon start");
    }
    if !print_runtime_summary(config, &client, verbose).await {
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
        "daemon desired runtime: backend={} controller={} proxy={} mode={} system_proxy={} tun={} dns={} active_profile={}",
        config.runtime_backend,
        config.controller.url,
        config.proxy_port_summary(),
        config.runtime_mode,
        config.system_proxy.enabled,
        config.tun.enable,
        config.dns.enable,
        config.active_profile.as_deref().unwrap_or("-")
    );
    match service::status() {
        Ok(status) => eprintln!(
            "daemon service: installed={} reachable={} core_running={} core_pid={}",
            status.installed,
            status.reachable,
            status.core_running,
            status
                .core_pid
                .map_or_else(|| "-".into(), |pid| pid.to_string())
        ),
        Err(err) => eprintln!("daemon service: check failed: {err:#}"),
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
    let _ = (current, next);
}

pub async fn stop(
    paths: &Paths,
    config: &AppConfig,
    client: &MihomoClient,
    verbose: bool,
) -> Result<()> {
    println!("clashtui stop");
    if verbose {
        print_static_summary(paths, config);
    }
    let Some(pid) = read_pid(paths).await? else {
        println!("daemon: stopped");
        cleanup_owned_runtimes(paths, config, client, verbose).await?;
        print_process_summary(paths, config).await?;
        return Ok(());
    };

    if !is_process_running(pid) {
        remove_stale_pid(paths).await?;
        println!("daemon: stopped removed_stale_pid={pid}");
        cleanup_owned_runtimes(paths, config, client, verbose).await?;
        print_process_summary(paths, config).await?;
        return Ok(());
    }

    if verbose {
        println!("daemon: stopping pid={pid}");
    }
    terminate_process(pid).with_context(|| format!("failed to stop pid {pid}"))?;
    if !wait_for_exit(pid).await {
        if verbose {
            println!("daemon: still running after graceful stop wait pid={pid}; force killing");
        }
        force_terminate_process(pid).with_context(|| format!("failed to force stop pid {pid}"))?;
    }
    if wait_for_exit(pid).await {
        println!("daemon: stopped pid={pid}");
    } else {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
        anyhow::bail!("daemon pid {pid} did not exit after stop");
    }
    remove_stale_pid(paths).await?;
    if verbose {
        println!(
            "runtime cleanup: system_proxy={} tun={} dns={}",
            config.system_proxy.enabled, config.tun.enable, config.dns.enable
        );
    }
    cleanup_owned_runtimes(paths, config, client, verbose).await?;
    print_process_summary(paths, config).await?;
    Ok(())
}

pub async fn restart(
    paths: &Paths,
    config: &mut AppConfig,
    cli_controller: Option<&str>,
    cli_secret: Option<&str>,
    verbose: bool,
) -> Result<()> {
    println!("clashtui restart");
    let client = MihomoClient::new(&config.controller);
    stop(paths, config, &client, verbose).await?;
    match start(paths, config, cli_controller, cli_secret, verbose).await {
        Ok(()) => Ok(()),
        Err(err) => {
            if restart_recovered(paths, config).await {
                println!("restart: runtime healthy after transient start error");
                Ok(())
            } else {
                Err(err)
            }
        }
    }
}

async fn cleanup_owned_runtimes(
    paths: &Paths,
    config: &AppConfig,
    client: &MihomoClient,
    verbose: bool,
) -> Result<()> {
    if config.system_proxy.enabled
        && let Err(err) = system_proxy::clear()
    {
        eprintln!("failed to clear system proxy: {err:#}");
    }

    if core::owned_core_running_for(paths, config).await? {
        cleanup_runtime(config, client).await;
    } else if verbose {
        println!(
            "runtime cleanup: skipped global mihomo cleanup because no clashtui-owned global core is running"
        );
    }
    if verbose {
        println!("mihomo core: stopping all instances owned by clashtui");
    }
    core::stop_all(paths, config).await
}

pub async fn status(
    paths: &Paths,
    config: &AppConfig,
    client: &MihomoClient,
    verbose: bool,
) -> Result<()> {
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
    if verbose {
        print_process_summary(paths, config).await?;
    }

    if verbose {
        print_static_summary(paths, config);
    } else {
        println!("proxy: {}", config.proxy_port_summary());
        println!(
            "configured: backend={} mode={} system_proxy={} tun={} dns={}",
            config.runtime_backend,
            config.runtime_mode,
            config.system_proxy.enabled,
            config.tun.enable,
            config.dns.enable
        );
    }
    print_service_summary(config, verbose);
    print_autostart_summary(paths, config, verbose);
    println!(
        "subscriptions: count={} active={}",
        config.subscriptions.len(),
        config.active_profile.as_deref().unwrap_or("-")
    );

    let runtime_healthy = print_runtime_summary(config, client, verbose).await;
    match system_proxy::status() {
        Ok(status) if verbose => println!(
            "system-proxy: enabled={} server={} bypass={}",
            status.enabled, status.server, status.bypass
        ),
        Ok(status) => println!(
            "system-proxy: enabled={} server={}",
            status.enabled, status.server
        ),
        Err(err) => println!("system-proxy: unavailable error={err}"),
    }
    if verbose {
        print_network_summary(config, running);
    }
    if running && !runtime_healthy {
        print_log_tail("clashtui log", &paths.log_file, LOG_TAIL_LINES);
        print_mihomo_log_tails(paths, config);
    } else if verbose {
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
        core::resolve_core_path(paths, config)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found; set core_path or MIHOMO_CORE".into())
    );
    println!("controller: {}", config.controller.url);
    println!("proxy: {}", config.proxy_port_summary());
    println!(
        "configured: backend={} mode={} system_proxy={} tun={} dns={} allow_lan={} proxy_selections={}",
        config.runtime_backend,
        config.runtime_mode,
        config.system_proxy.enabled,
        config.tun.enable,
        config.dns.enable,
        config.proxy_ports.allow_lan,
        config.proxy_selections.len()
    );
}

fn print_service_summary(config: &AppConfig, verbose: bool) {
    if !config.use_service_runtime() {
        println!(
            "service: skipped because runtime_backend={}",
            config.runtime_backend
        );
        return;
    }

    match service::status() {
        Ok(status) if verbose => {
            println!(
                "service: installed={} reachable={} core_running={} core_pid={} version={} message={}",
                status.installed,
                status.reachable,
                status.core_running,
                status
                    .core_pid
                    .map_or_else(|| "-".into(), |pid| pid.to_string()),
                status.version.as_deref().unwrap_or("-"),
                status.message.as_deref().unwrap_or("-")
            )
        }
        Ok(status) => {
            let state = if status.reachable && status.core_running {
                "ok"
            } else if status.reachable {
                "ready"
            } else {
                "unavailable"
            };
            println!(
                "service: {state} installed={} reachable={} core_running={} core_pid={}",
                status.installed,
                status.reachable,
                status.core_running,
                status
                    .core_pid
                    .map_or_else(|| "-".into(), |pid| pid.to_string())
            );
        }
        Err(err) => println!("service: check failed: {err:#}"),
    }
}

fn sync_autostart(paths: &Paths, config: &AppConfig, verbose: bool) -> Result<()> {
    let status = autostart::sync(paths, config).context("failed to sync autostart")?;
    if verbose {
        println!(
            "autostart: configured={} installed={} path={} message={}",
            status.configured,
            status.installed,
            status
                .path
                .as_ref()
                .map_or_else(|| "-".into(), |path| path.display().to_string()),
            status.message.as_deref().unwrap_or("-")
        );
    } else {
        println!(
            "autostart: configured={} installed={}",
            status.configured, status.installed
        );
    }
    Ok(())
}

fn print_autostart_summary(paths: &Paths, config: &AppConfig, verbose: bool) {
    let _ = paths;
    let status = autostart::status(config);
    if verbose {
        println!(
            "autostart: configured={} installed={} path={} message={}",
            status.configured,
            status.installed,
            status
                .path
                .as_ref()
                .map_or_else(|| "-".into(), |path| path.display().to_string()),
            status.message.as_deref().unwrap_or("-")
        );
    } else {
        println!(
            "autostart: configured={} installed={}",
            status.configured, status.installed
        );
    }
}

fn validate_start_permissions(config: &AppConfig) -> Result<()> {
    if !config.use_service_runtime() {
        return Ok(());
    }

    let status = service::status().context("failed to inspect clashtui service")?;
    if status.reachable {
        return Ok(());
    }

    if config.tun.enable {
        eprintln!(
            "runtime warning: service is unavailable; TUN will be disabled for this user-mode runtime until `clashtui service-install` is run"
        );
    }
    Ok(())
}

async fn print_runtime_summary(config: &AppConfig, client: &MihomoClient, verbose: bool) -> bool {
    let version = match client.version().await {
        Ok(version) => {
            if verbose {
                println!("mihomo: online version={version}");
            }
            version
        }
        Err(err) => {
            println!("runtime: offline error={err}");
            return false;
        }
    };

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
            if verbose {
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
            }
            if Some(config.tun.enable) != tun_enabled {
                healthy = false;
                if verbose {
                    println!(
                        "warning: TUN desired={} but mihomo reports {}; check permissions and mihomo log",
                        config.tun.enable,
                        bool_value(tun_enabled)
                    );
                }
            }
            if dns_enabled.is_some() && Some(config.dns.enable) != dns_enabled {
                healthy = false;
                if verbose {
                    println!(
                        "warning: DNS desired={} but mihomo reports {}; check mihomo log",
                        config.dns.enable,
                        bool_value(dns_enabled)
                    );
                }
            }
            if !verbose {
                let state = if healthy { "ok" } else { "degraded" };
                println!(
                    "runtime: {state} version={version} mode={mode} mixed-port={mixed_port} tun={} dns={}",
                    feature_match(config.tun.enable, tun_enabled),
                    feature_match(config.dns.enable, dns_enabled)
                );
            }
            healthy
        }
        Err(err) => {
            println!("runtime: degraded config_error={err}");
            false
        }
    };

    print_mihomo_metrics_summary(client, verbose).await;
    config_healthy
}

async fn print_mihomo_metrics_summary(client: &MihomoClient, verbose: bool) {
    let traffic_result = client.traffic().await;
    let connections_result = client.connections().await;

    if !verbose {
        let traffic = traffic_result
            .as_ref()
            .ok()
            .map(|traffic| {
                let up = json_u64(traffic, "up").unwrap_or_default();
                let down = json_u64(traffic, "down").unwrap_or_default();
                format!(
                    "up={}/s down={}/s",
                    format_bytes_short(up),
                    format_bytes_short(down)
                )
            })
            .unwrap_or_else(|| "traffic=unavailable".into());
        let connections = connections_result
            .as_ref()
            .ok()
            .map(|connections| {
                let active = connections
                    .get("connections")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                format!("connections={active}")
            })
            .unwrap_or_else(|| "connections=unavailable".into());
        println!("metrics: {traffic} {connections}");
        return;
    }

    match traffic_result {
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

    match connections_result {
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

fn feature_match(desired: bool, actual: Option<bool>) -> String {
    match actual {
        Some(actual) if actual == desired => format!("ok({actual})"),
        Some(actual) => format!("mismatch(desired={desired},actual={actual})"),
        None => "unknown".into(),
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
    if config.use_single_runtime() {
        return apply_single_runtime(paths, config).await;
    }

    let mut errors = Vec::new();
    eprintln!(
        "runtime apply: backend={} desired mode={} proxy={} system_proxy={} tun={} dns={} active_profile={} port_proxies={}",
        config.runtime_backend,
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

async fn apply_single_runtime(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    let client = MihomoClient::new(&config.controller);
    let mut runtime_config = effective_single_runtime_config(config);
    let mut errors = Vec::new();
    eprintln!(
        "runtime apply: backend={} desired mode={} proxy={} system_proxy={} tun={} dns={} active_profile={} port_proxies={}",
        config.runtime_backend,
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

    core::stop_removed_services(paths, 0)
        .await
        .context("legacy port proxy cleanup")?;
    ensure_active_runtime_profile(paths, config)
        .await
        .context("profile prepare")?;

    if runtime_config.use_service_runtime()
        && !service::core_running()?
        && core::owned_core_running(paths).await?
    {
        eprintln!("runtime apply: stopping user-mode single mihomo before switching to service");
        core::stop_legacy_global(paths).await?;
    }

    if core::owned_core_running_for(paths, &runtime_config).await? {
        if client.version().await.is_err() {
            eprintln!("runtime apply: single mihomo controller unhealthy; restarting core");
            core::stop(paths, &runtime_config).await?;
            core::ensure_running(paths, &mut runtime_config).await?;
            wait_for_mihomo(&client).await?;
        }
    } else if client.version().await.is_ok() {
        anyhow::bail!(
            "mihomo controller {} is online but is not owned by clashtui; refusing to modify external mihomo",
            config.controller.url
        );
    } else {
        eprintln!("runtime apply: single mihomo is not running; ensuring core is running");
        core::ensure_running(paths, &mut runtime_config).await?;
        if let Err(err) = wait_for_mihomo(&client).await {
            eprintln!(
                "runtime apply: single mihomo controller still unhealthy after wait: {err:#}"
            );
            eprintln!("runtime apply: restarting single mihomo core");
            core::stop(paths, &runtime_config).await?;
            core::ensure_running(paths, &mut runtime_config).await?;
            wait_for_mihomo(&client).await?;
        }
    }

    if let Err(err) = load_single_runtime_profile(paths, &mut runtime_config, &client).await {
        errors.push(format!("profile: {err:#}"));
    } else {
        eprintln!("runtime apply: single runtime profile loaded");
    }

    if let Err(err) = client.set_mode(&runtime_config.runtime_mode).await {
        errors.push(format!("mode: {err:#}"));
    } else {
        eprintln!("runtime apply: single mode={}", runtime_config.runtime_mode);
    }

    if let Err(err) = apply_proxy_selections(
        &client,
        "Global Proxy",
        desired_global_proxy_selections(config),
    )
    .await
    {
        errors.push(format!("global proxy selection: {err:#}"));
    }

    if let Err(err) = apply_single_runtime_service_selections(&client, config).await {
        errors.push(format!("port proxy selection: {err:#}"));
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

    if let Err(err) = tun::apply(&client, &runtime_config.tun).await {
        errors.push(format!("tun: {err:#}"));
    } else {
        eprintln!("runtime apply: tun requested={}", runtime_config.tun.enable);
    }

    if let Err(err) = dns::apply(&client, &runtime_config.dns).await {
        errors.push(format!("dns: {err:#}"));
    } else {
        eprintln!(
            "runtime apply: dns requested={} listen={}",
            runtime_config.dns.enable, runtime_config.dns.listen
        );
    }

    if let Err(err) = verify_runtime_state(&runtime_config, &client).await {
        errors.push(format!("runtime verify: {err:#}"));
    }

    for service in config
        .proxy_ports
        .services
        .iter()
        .filter(|service| service.enabled)
    {
        eprintln!(
            "runtime apply: listener ready name={} listen={}:{} kind={} proxy={} rule={}",
            service.name,
            if service.listen.trim().is_empty() {
                "127.0.0.1"
            } else {
                service.listen.trim()
            },
            service.port,
            service.kind,
            service.proxy.as_deref().unwrap_or("-"),
            service.rule.as_deref().unwrap_or("-")
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("{}", errors.join("; "))
    }
}

fn effective_single_runtime_config(config: &AppConfig) -> AppConfig {
    let mut runtime_config = config.clone();
    if !config.use_service_runtime() {
        return runtime_config;
    }

    match service::status() {
        Ok(status) if status.reachable => runtime_config,
        Ok(status) => {
            runtime_config.runtime_backend = "single".into();
            if runtime_config.tun.enable {
                runtime_config.tun.enable = false;
                eprintln!(
                    "runtime apply: service unavailable (installed={} message={}); using user-mode single runtime without TUN",
                    status.installed,
                    status.message.as_deref().unwrap_or("-")
                );
            }
            runtime_config
        }
        Err(err) => {
            runtime_config.runtime_backend = "single".into();
            if runtime_config.tun.enable {
                runtime_config.tun.enable = false;
                eprintln!(
                    "runtime apply: service status failed ({err:#}); using user-mode single runtime without TUN"
                );
            }
            runtime_config
        }
    }
}

async fn apply_global_runtime(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    let client = MihomoClient::new(&config.controller);
    let mut errors = Vec::new();

    if core::owned_core_running_for(paths, config).await? {
        if client.version().await.is_err() {
            eprintln!("runtime apply: owned global mihomo controller unhealthy; restarting core");
            core::stop(paths, config).await?;
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
            core::stop(paths, config).await?;
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

    let profile_owns_rule_selections = config
        .active_proxy_profile()
        .is_some_and(|profile| !profile.rule_selections.is_empty());

    if config.runtime_mode.eq_ignore_ascii_case("rule")
        && !profile_owns_rule_selections
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

async fn load_single_runtime_profile(
    paths: &Paths,
    config: &mut AppConfig,
    client: &MihomoClient,
) -> Result<()> {
    ensure_active_runtime_profile(paths, config).await?;
    let runtime_config = runtime_profile::write_single_runtime_config(paths, config).await?;
    client.reload_config(&runtime_config).await
}

async fn ensure_active_runtime_profile(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    if let Some(active_profile) = config.active_profile.clone() {
        ensure_subscription_profile(paths, config, &active_profile).await?;
    }
    Ok(())
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

    if config.use_single_runtime() {
        let _ = paths;
        return Ok(());
    }

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

async fn apply_single_runtime_service_selections(
    client: &MihomoClient,
    config: &AppConfig,
) -> Result<()> {
    for (index, service) in config
        .proxy_ports
        .services
        .iter()
        .enumerate()
        .filter(|(_, service)| service.enabled)
    {
        if !service.mode.eq_ignore_ascii_case("rule") {
            continue;
        }
        let selections = service
            .rule_selections
            .iter()
            .map(|(group, proxy)| (group.as_str(), proxy.as_str()))
            .collect::<Vec<_>>();
        apply_proxy_selections(client, &service_name(index, service), selections).await?;
    }
    Ok(())
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

async fn wait_for_required_ports_available(config: &AppConfig) -> Result<()> {
    let mut last_error = None;
    for _ in 0..PORT_RELEASE_RETRIES {
        match port_allocator::validate_required_ports_available(config) {
            Ok(()) => return Ok(()),
            Err(err) if err.to_string().contains("already in use") => {
                last_error = Some(err);
                sleep(STOP_WAIT).await;
            }
            Err(err) => return Err(err),
        }
    }
    match last_error {
        Some(err) => Err(err).context("required ports did not become available after cleanup"),
        None => Ok(()),
    }
}

async fn restart_recovered(paths: &Paths, config: &AppConfig) -> bool {
    let client = MihomoClient::new(&config.controller);
    for _ in 0..PORT_RELEASE_RETRIES {
        let daemon_running = read_pid(paths)
            .await
            .ok()
            .flatten()
            .is_some_and(is_process_running);
        if daemon_running && client.version().await.is_ok() {
            return true;
        }
        sleep(STOP_WAIT).await;
    }
    false
}

async fn print_process_summary(paths: &Paths, config: &AppConfig) -> Result<()> {
    print_pid_state("daemon-pid", &paths.pid_file).await?;
    if config.use_service_runtime() {
        match service::status() {
            Ok(status) if status.core_running => {
                let pid = status
                    .core_pid
                    .map_or_else(|| "-".into(), |pid| pid.to_string());
                println!("service-mihomo-pid: running pid={pid}");
            }
            Ok(_) => println!("service-mihomo-pid: stopped"),
            Err(err) => println!("service-mihomo-pid: unavailable error={err:#}"),
        }
        return Ok(());
    }
    print_pid_state("global-mihomo-pid", &paths.core_pid_file).await?;
    if config.use_single_runtime() {
        return Ok(());
    }
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
    if config.use_single_runtime() {
        return;
    }
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

async fn wait_for_exit(pid: u32) -> bool {
    for _ in 0..STOP_RETRIES {
        if !is_process_running(pid) {
            return true;
        }
        sleep(STOP_WAIT).await;
    }
    !is_process_running(pid)
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
    let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if status == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
    signal_process(pid, libc::SIGTERM)
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

#[cfg(unix)]
fn force_terminate_process(pid: u32) -> Result<()> {
    signal_process(pid, libc::SIGKILL)
}

#[cfg(windows)]
fn force_terminate_process(pid: u32) -> Result<()> {
    terminate_process(pid)
}

#[cfg(not(any(unix, windows)))]
fn force_terminate_process(_pid: u32) -> Result<()> {
    anyhow::bail!("stop is not supported on this platform");
}

#[cfg(unix)]
fn signal_process(pid: u32, signal: libc::c_int) -> Result<()> {
    let status = unsafe { libc::kill(pid as libc::pid_t, signal) };
    if status == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err).with_context(|| format!("failed to signal pid {pid}"))
}
