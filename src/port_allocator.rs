use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};

use crate::config::{AppConfig, Paths};

const RANGE_LEN: u16 = 900;
const CONTROLLER_BASE: u16 = 19090;
const SERVICE_CONTROLLER_BASE: u16 = 20090;
const MIXED_BASE: u16 = 17070;
const DNS_BASE: u16 = 15053;
const LISTENER_BASE: u16 = 7071;
const LOCALHOST: &str = "127.0.0.1";
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_millis(120);

pub async fn ensure_allocated(paths: &Paths, config: &mut AppConfig) -> Result<bool> {
    ensure_allocated_with_controller(paths, config, true, false).await
}

pub async fn ensure_allocated_with_controller(
    paths: &Paths,
    config: &mut AppConfig,
    allocate_controller: bool,
    reassign_occupied_auto: bool,
) -> Result<bool> {
    let seed_created = ensure_seed(paths, config);
    let mut changed = seed_created;
    let seed = config.port_allocation.seed.unwrap_or_default();
    let mut used_tcp = fixed_tcp_ports(config);

    if config.port_allocation.auto_controller && allocate_controller {
        if !seed_created
            && let Some(port) = controller_port(&config.controller.url)
            && (!reassign_occupied_auto || tcp_available(LOCALHOST, port))
        {
            used_tcp.insert(port);
        } else {
            let port = allocate_tcp(CONTROLLER_BASE, seed, &used_tcp, "controller")?;
            if controller_port(&config.controller.url) != Some(port) {
                config.controller.url = format!("http://{LOCALHOST}:{port}");
                changed = true;
            }
            used_tcp.insert(port);
        }
    } else if let Some(port) = controller_port(&config.controller.url) {
        used_tcp.insert(port);
    }

    if config.port_allocation.auto_mixed {
        if !seed_created
            && config.mixed_port != 0
            && (!reassign_occupied_auto || tcp_available(LOCALHOST, config.mixed_port))
        {
            used_tcp.insert(config.mixed_port);
        } else {
            let port = allocate_tcp(MIXED_BASE, seed.wrapping_add(97), &used_tcp, "mixed-port")?;
            if config.mixed_port != port {
                config.mixed_port = port;
                changed = true;
            }
            used_tcp.insert(port);
        }
    } else {
        used_tcp.insert(config.mixed_port);
    }

    if config.dns.enable && config.port_allocation.auto_dns {
        if !seed_created
            && let Some(port) = listen_port(&config.dns.listen)
            && (!reassign_occupied_auto
                || (tcp_available(LOCALHOST, port) && udp_available(LOCALHOST, port)))
        {
            used_tcp.insert(port);
        } else {
            let port = allocate_dns(DNS_BASE, seed.wrapping_add(211), &used_tcp)?;
            if listen_port(&config.dns.listen) != Some(port) {
                config.dns.listen = format!("{LOCALHOST}:{port}");
                changed = true;
            }
            used_tcp.insert(port);
        }
    } else if config.dns.enable
        && let Some(port) = listen_port(&config.dns.listen)
    {
        used_tcp.insert(port);
    }

    for service in &mut config.proxy_ports.services {
        if service.enabled && service.port == 0 {
            let port = allocate_tcp(LISTENER_BASE, 0, &used_tcp, "listener")?;
            service.port = port;
            used_tcp.insert(port);
            changed = true;
        } else if service.enabled {
            used_tcp.insert(service.port);
        }
    }

    Ok(changed)
}

pub fn validate_required_ports_available(config: &AppConfig) -> Result<()> {
    let mut tcp_ports = BTreeMap::new();
    check_tcp_port(
        &mut tcp_ports,
        "controller",
        controller_host(&config.controller.url).unwrap_or(LOCALHOST),
        controller_port(&config.controller.url)
            .with_context(|| format!("invalid controller URL: {}", config.controller.url))?,
    )?;
    check_tcp_port(
        &mut tcp_ports,
        "mixed-port",
        &config.proxy_host,
        config.mixed_port,
    )?;

    if let Some(port) = config.proxy_ports.http {
        check_tcp_port(&mut tcp_ports, "http port", &config.proxy_host, port)?;
    }
    if let Some(port) = config.proxy_ports.socks {
        check_tcp_port(&mut tcp_ports, "socks port", &config.proxy_host, port)?;
    }
    for service in config
        .proxy_ports
        .services
        .iter()
        .enumerate()
        .filter(|(_, service)| service.enabled)
    {
        let (index, service) = service;
        if !config.use_single_runtime() {
            check_tcp_port(
                &mut tcp_ports,
                &format!("{} controller", service.name),
                LOCALHOST,
                service_controller_port(config, index),
            )?;
        }
        let listen = if service.listen.trim().is_empty() {
            LOCALHOST
        } else {
            service.listen.trim()
        };
        check_tcp_port(&mut tcp_ports, &service.name, listen, service.port)?;
    }

    if config.dns.enable {
        let host = listen_host(&config.dns.listen).unwrap_or(LOCALHOST);
        let port = listen_port(&config.dns.listen)
            .with_context(|| format!("invalid DNS listen address: {}", config.dns.listen))?;
        check_tcp_port(&mut tcp_ports, "dns listen", host, port)?;
        check_udp_available(host, port, "dns listen")?;
    }

    Ok(())
}

pub fn service_controller_url(config: &AppConfig, index: usize) -> String {
    format!(
        "http://{LOCALHOST}:{}",
        service_controller_port(config, index)
    )
}

pub fn service_controller_port(config: &AppConfig, index: usize) -> u16 {
    let seed = config.port_allocation.seed.unwrap_or_default();
    SERVICE_CONTROLLER_BASE + ((seed + index as u16) % RANGE_LEN)
}

fn ensure_seed(paths: &Paths, config: &mut AppConfig) -> bool {
    if config.port_allocation.seed.is_some() {
        return false;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    paths.config_dir.hash(&mut hasher);
    let path_hash = hasher.finish();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or_default();
    let seed = ((path_hash ^ now) % u64::from(RANGE_LEN)) as u16;
    config.port_allocation.seed = Some(seed);
    true
}

fn fixed_tcp_ports(config: &AppConfig) -> BTreeSet<u16> {
    let mut ports = BTreeSet::new();
    if !config.port_allocation.auto_controller
        && let Some(port) = controller_port(&config.controller.url)
    {
        ports.insert(port);
    }
    if !config.port_allocation.auto_mixed {
        ports.insert(config.mixed_port);
    }
    if let Some(port) = config.proxy_ports.http {
        ports.insert(port);
    }
    if let Some(port) = config.proxy_ports.socks {
        ports.insert(port);
    }
    if config.dns.enable
        && !config.port_allocation.auto_dns
        && let Some(port) = listen_port(&config.dns.listen)
    {
        ports.insert(port);
    }
    for service in config
        .proxy_ports
        .services
        .iter()
        .filter(|service| service.enabled)
    {
        if service.port != 0 {
            ports.insert(service.port);
        }
    }
    ports
}

fn allocate_tcp(base: u16, seed: u16, used: &BTreeSet<u16>, label: &str) -> Result<u16> {
    for offset in 0..RANGE_LEN {
        let port = base + ((seed + offset) % RANGE_LEN);
        if used.contains(&port) {
            continue;
        }
        if tcp_available(LOCALHOST, port) {
            return Ok(port);
        }
    }
    anyhow::bail!(
        "no available {label} port in {}-{}",
        base,
        base + RANGE_LEN - 1
    )
}

fn allocate_dns(base: u16, seed: u16, used: &BTreeSet<u16>) -> Result<u16> {
    for offset in 0..RANGE_LEN {
        let port = base + ((seed + offset) % RANGE_LEN);
        if used.contains(&port) {
            continue;
        }
        if tcp_available(LOCALHOST, port) && udp_available(LOCALHOST, port) {
            return Ok(port);
        }
    }
    anyhow::bail!("no available DNS port in {}-{}", base, base + RANGE_LEN - 1)
}

fn check_tcp_port(
    seen: &mut BTreeMap<u16, String>,
    label: &str,
    host: &str,
    port: u16,
) -> Result<()> {
    if port == 0 {
        anyhow::bail!("{label} has no allocated port");
    }
    if let Some(existing) = seen.insert(port, label.to_string()) {
        anyhow::bail!("{label} port {port} conflicts with {existing}");
    }
    if !tcp_available(host, port) {
        anyhow::bail!("{label} port {host}:{port} is already in use");
    }
    Ok(())
}

fn check_udp_available(host: &str, port: u16, label: &str) -> Result<()> {
    if !udp_available(host, port) {
        anyhow::bail!("{label} UDP port {host}:{port} is already in use");
    }
    Ok(())
}

fn tcp_available(host: &str, port: u16) -> bool {
    !tcp_port_in_use(host, port)
}

fn udp_available(_host: &str, port: u16) -> bool {
    !udp_port_in_use(port)
}

fn tcp_port_in_use(host: &str, port: u16) -> bool {
    tcp_connects(host, port)
        || proc_net_port_in_use("tcp", port, true)
        || lsof_port_in_use("TCP", port)
}

fn udp_port_in_use(port: u16) -> bool {
    proc_net_port_in_use("udp", port, false) || lsof_port_in_use("UDP", port)
}

fn tcp_connects(host: &str, port: u16) -> bool {
    let target = (probe_host(host), port);
    let Ok(addrs) = target.to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|addr| TcpStream::connect_timeout(&addr, TCP_CONNECT_TIMEOUT).is_ok())
}

fn probe_host(host: &str) -> &str {
    let host = host.trim();
    if host.is_empty() || host == "*" || host == "0.0.0.0" || host == "::" {
        LOCALHOST
    } else {
        host
    }
}

fn lsof_port_in_use(protocol: &str, port: u16) -> bool {
    let mut command = Command::new("lsof");
    command.arg("-nP").arg(format!("-i{protocol}:{port}"));
    if protocol == "TCP" {
        command.arg("-sTCP:LISTEN");
    }
    let Ok(output) = command.output() else {
        return false;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .skip(1)
        .any(|line| !line.trim().is_empty())
}

#[cfg(target_os = "linux")]
fn proc_net_port_in_use(protocol: &str, port: u16, tcp_listen_only: bool) -> bool {
    [
        format!("/proc/net/{protocol}"),
        format!("/proc/net/{protocol}6"),
    ]
    .iter()
    .any(|path| proc_net_file_port_in_use(path, port, tcp_listen_only))
}

#[cfg(not(target_os = "linux"))]
fn proc_net_port_in_use(_protocol: &str, _port: u16, _tcp_listen_only: bool) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn proc_net_file_port_in_use(path: &str, port: u16, tcp_listen_only: bool) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    content.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        let _slot = fields.next();
        let Some(local_address) = fields.next() else {
            return false;
        };
        let Some(state) = fields.next() else {
            return false;
        };
        if tcp_listen_only && state != "0A" {
            return false;
        }
        let Some((_addr, port_hex)) = local_address.rsplit_once(':') else {
            return false;
        };
        u16::from_str_radix(port_hex, 16).is_ok_and(|value| value == port)
    })
}

fn controller_host(value: &str) -> Option<&str> {
    split_host_port(value).map(|(host, _)| host)
}

fn controller_port(value: &str) -> Option<u16> {
    split_host_port(value).and_then(|(_, port)| port.parse().ok())
}

fn listen_host(value: &str) -> Option<&str> {
    split_host_port(value).map(|(host, _)| host)
}

fn listen_port(value: &str) -> Option<u16> {
    split_host_port(value).and_then(|(_, port)| port.parse().ok())
}

fn split_host_port(value: &str) -> Option<(&str, &str)> {
    let value = value
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    let (host, port) = value.rsplit_once(':')?;
    Some((host.trim_matches(['[', ']']), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_controller_port() {
        assert_eq!(controller_port("http://127.0.0.1:19090"), Some(19090));
        assert_eq!(controller_host("http://127.0.0.1:19090"), Some("127.0.0.1"));
    }

    #[tokio::test]
    async fn preserves_fixed_mixed_port_during_allocation() -> Result<()> {
        let mut config = AppConfig::default();
        config.port_allocation.seed = Some(1);
        config.port_allocation.auto_mixed = false;
        config.mixed_port = 7070;

        let paths = Paths {
            config_dir: std::env::temp_dir().join("clashtui-port-test"),
            config_file: std::env::temp_dir().join("clashtui-port-test/config.yaml"),
            pid_file: std::env::temp_dir().join("clashtui-port-test/clashtui.pid"),
            core_pid_file: std::env::temp_dir().join("clashtui-port-test/mihomo.pid"),
            core_config_file: std::env::temp_dir().join("clashtui-port-test/mihomo-run.yaml"),
            active_config_file: std::env::temp_dir().join("clashtui-port-test/mihomo-active.yaml"),
            log_file: std::env::temp_dir().join("clashtui-port-test/clashtui.log"),
            core_log_file: std::env::temp_dir().join("clashtui-port-test/mihomo.log"),
            profiles_dir: std::env::temp_dir().join("clashtui-port-test/profiles"),
            cores_dir: std::env::temp_dir().join("clashtui-port-test/cores"),
        };

        ensure_allocated(&paths, &mut config).await?;

        assert_eq!(config.mixed_port, 7070);
        Ok(())
    }
}
