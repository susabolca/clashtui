use std::env;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Write};
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::config::TunConfig;

const HELPER_LABEL: &str = "com.clashtui.tun-helper";
const HELPER_ARTIFACT_NAME: &str = "clashtui-tun-helper";
const HELPER_BINARY_PATH: &str = "/usr/local/libexec/clashtui-tun-helper";
const HELPER_SERVICE_PATH: &str = "/etc/systemd/system/clashtui-tun-helper.service";
const HELPER_SOCKET_PATH: &str = "/run/com.clashtui.tun-helper.sock";
const HELPER_LOG_PATH: &str = "/var/log/clashtui-tun-helper.log";
const HELPER_STATUS_TIMEOUT: Duration = Duration::from_millis(300);
const HELPER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_IDLE_SLEEP: Duration = Duration::from_millis(50);
const HELPER_OWNER_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const HELPER_START_RETRIES: usize = 20;
const TUN_DEVICE_PATH: &str = "/dev/net/tun";
const DEFAULT_TUN_ADDR_CIDR: &str = "198.18.0.1/30";
const ROUTE_TABLE: &str = "789";
const ROUTE_RULE_PRIORITY: &str = "17890";
const DEFAULT_ROUTES: [&str; 2] = ["0.0.0.0/1", "128.0.0.0/1"];
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const IFF_TUN: libc::c_short = 0x0001;
const IFF_NO_PI: libc::c_short = 0x1000;

#[derive(Debug)]
pub struct TunDevice {
    pub interface: String,
    pub outbound_interface: Option<String>,
    fd: OwnedFd,
}

impl TunDevice {
    pub fn file_descriptor(&self) -> i32 {
        self.fd.as_raw_fd()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HelperRequest {
    command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    prepare: Option<PrepareTunRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<OwnerRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrepareTunRequest {
    device: String,
    mtu: u16,
    owner_uid: u32,
    owner_pid: u32,
    auto_route: bool,
    route_exclude_address: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnerRequest {
    owner_uid: u32,
    owner_pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_pid: Option<u32>,
    #[serde(default)]
    allow_experimental_routes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HelperResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    helper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    socket: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outbound_interface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fd: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    routes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Default)]
struct HelperState {
    active: Option<ActiveTun>,
}

#[derive(Debug)]
struct ActiveTun {
    interface: String,
    outbound_interface: Option<String>,
    owner_uid: u32,
    owner_pid: u32,
    auto_route: bool,
    route_exclude_address: Vec<String>,
    routes: Vec<RouteEntry>,
    rule_active: bool,
    subject_pid: Option<u32>,
}

#[derive(Debug, Clone)]
struct RouteEntry {
    destination: String,
    table: String,
    interface: String,
}

#[derive(Debug, Clone)]
pub struct TunPermissionStatus {
    pub target: PathBuf,
    pub capabilities: String,
    pub legacy_file_capabilities_detected: bool,
    pub polkit_rule_path: &'static str,
    pub polkit_rule_exists: bool,
    pub polkit_rule_matches_user: bool,
    pub tun_device_exists: bool,
    pub is_root: bool,
    pub helper_installed: bool,
    pub helper_reachable: bool,
    pub helper_binary_path: &'static str,
    pub helper_service_path: &'static str,
    pub helper_socket_path: &'static str,
}

impl TunPermissionStatus {
    pub const fn can_start_tun(&self) -> bool {
        self.helper_reachable
    }
}

pub fn tun_install(path: Option<PathBuf>) -> Result<()> {
    let target = target_binary(path)?;
    let user = invoking_user()?;
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
    let capabilities = getcap_output(target).unwrap_or_default();
    let helper_installed = helper_installed();
    let helper_reachable = helper_status().is_ok();
    Ok(TunPermissionStatus {
        target: target.to_path_buf(),
        legacy_file_capabilities_detected: capabilities.contains("cap_net_admin")
            && capabilities.contains("cap_net_bind_service")
            && capabilities.contains("=ep"),
        capabilities,
        polkit_rule_path: HELPER_SERVICE_PATH,
        polkit_rule_exists: Path::new(HELPER_SERVICE_PATH).exists(),
        polkit_rule_matches_user: helper_installed,
        tun_device_exists: Path::new(TUN_DEVICE_PATH).exists(),
        is_root: is_root(),
        helper_installed,
        helper_reachable,
        helper_binary_path: HELPER_BINARY_PATH,
        helper_service_path: HELPER_SERVICE_PATH,
        helper_socket_path: HELPER_SOCKET_PATH,
    })
}

pub fn tun_install_privileged(target: PathBuf, user: String) -> Result<()> {
    ensure_root("install Linux TUN helper")?;
    let uid = user_uid(&user)?;
    stop_helper().ok();
    remove_socket_if_exists()?;
    install_helper_binary(&target)?;
    install_systemd_service(&user, uid, helper_needs_entrypoint_arg(&target))?;
    reload_systemd()?;
    enable_and_start_helper()?;
    let status = wait_for_helper()?;

    println!("TUN helper installed: {HELPER_BINARY_PATH}");
    println!("systemd-service: {HELPER_SERVICE_PATH}");
    println!("helper-status: {}", status.trim());
    Ok(())
}

pub fn tun_uninstall(path: Option<PathBuf>) -> Result<()> {
    let target = target_binary(path)?;
    if !is_root() {
        return run_sudo_privileged("__tun-uninstall-root", &target);
    }
    tun_uninstall_privileged(target)
}

pub fn tun_uninstall_privileged(target: PathBuf) -> Result<()> {
    ensure_root("uninstall Linux TUN helper")?;
    let _ = helper_request(r#"{"command":"teardown_tun"}"#);
    let _ = stop_helper();
    remove_socket_if_exists()?;
    remove_file_if_exists(HELPER_SERVICE_PATH)?;
    remove_file_if_exists(HELPER_BINARY_PATH)?;
    if has_any_capabilities(&target).unwrap_or(false) {
        let _ = run_setcap(["-r", path_to_str(&target)?]);
    }
    let _ = reload_systemd();
    println!("TUN helper removed: {HELPER_BINARY_PATH}");
    println!("systemd-service removed: {HELPER_SERVICE_PATH}");
    Ok(())
}

pub fn prepare_tun(config: &TunConfig) -> Result<TunDevice> {
    let request = HelperRequest {
        command: "prepare_tun".into(),
        prepare: Some(PrepareTunRequest {
            device: config.device.clone(),
            mtu: config.mtu,
            owner_uid: current_uid(),
            owner_pid: std::process::id(),
            auto_route: config.auto_route,
            route_exclude_address: config.route_exclude_address.clone(),
        }),
        owner: None,
    };
    let request =
        serde_json::to_string(&request).context("failed to encode prepare_tun request")?;
    let (response, fd) = helper_request_with_optional_fd(&request)?;
    if !response.ok {
        anyhow::bail!(
            "TUN helper prepare_tun failed: {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        );
    }
    let fd = fd.context("TUN helper did not return a file descriptor")?;
    let interface = response
        .interface
        .filter(|value| !value.is_empty())
        .context("TUN helper did not return an interface name")?;
    Ok(TunDevice {
        interface,
        outbound_interface: response.outbound_interface,
        fd,
    })
}

pub fn activate_tun(subject_pid: Option<u32>) -> Result<()> {
    helper_owner_command("activate_routes", subject_pid)
}

pub fn deactivate_tun() -> Result<()> {
    helper_owner_command("deactivate_routes", None)
}

pub fn teardown_tun() -> Result<()> {
    let response = helper_request(r#"{"command":"teardown_tun"}"#)?;
    let response: HelperResponse =
        serde_json::from_str(response.trim()).context("failed to parse helper response")?;
    if response.ok {
        return Ok(());
    }
    anyhow::bail!(
        "TUN helper teardown failed: {}",
        response.error.unwrap_or_else(|| "unknown error".into())
    )
}

fn helper_owner_command(command: &str, subject_pid: Option<u32>) -> Result<()> {
    let request = HelperRequest {
        command: command.into(),
        prepare: None,
        owner: Some(OwnerRequest {
            owner_uid: current_uid(),
            owner_pid: std::process::id(),
            subject_pid,
            allow_experimental_routes: linux_experimental_routes_enabled(),
        }),
    };
    let request = serde_json::to_string(&request)
        .with_context(|| format!("failed to encode {command} request"))?;
    let response = helper_request(&request)?;
    let response: HelperResponse =
        serde_json::from_str(response.trim()).context("failed to parse helper response")?;
    if response.ok {
        return Ok(());
    }
    anyhow::bail!(
        "TUN helper {command} failed: {}",
        response.error.unwrap_or_else(|| "unknown error".into())
    )
}

pub fn tun_helper_run() -> Result<()> {
    ensure_root("run Linux TUN helper")?;
    let allowed_uid = env::var("CLASHTUI_TUN_HELPER_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .context("CLASHTUI_TUN_HELPER_UID is required")?;
    let allowed_user =
        env::var("CLASHTUI_TUN_HELPER_USER").unwrap_or_else(|_| allowed_uid.to_string());

    remove_socket_if_exists()?;
    let listener = UnixListener::bind(HELPER_SOCKET_PATH)
        .with_context(|| format!("failed to bind {HELPER_SOCKET_PATH}"))?;
    listener
        .set_nonblocking(true)
        .context("failed to set helper listener nonblocking")?;
    fs::set_permissions(HELPER_SOCKET_PATH, fs::Permissions::from_mode(0o666))
        .with_context(|| format!("failed to chmod {HELPER_SOCKET_PATH}"))?;

    eprintln!(
        "clashtui Linux TUN helper listening socket={} allowed_user={} allowed_uid={} log={}",
        HELPER_SOCKET_PATH, allowed_user, allowed_uid, HELPER_LOG_PATH
    );

    let mut state = HelperState::default();
    let mut last_owner_check = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                let _ = stream.set_read_timeout(Some(HELPER_REQUEST_TIMEOUT));
                let _ = stream.set_write_timeout(Some(HELPER_REQUEST_TIMEOUT));
                if let Err(err) =
                    handle_helper_client(&mut stream, &allowed_user, allowed_uid, &mut state)
                {
                    let _ = writeln!(stream, r#"{{"ok":false,"error":"{err:#}"}}"#);
                    eprintln!("tun-helper request failed: {err:#}");
                }
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                if last_owner_check.elapsed() >= HELPER_OWNER_CHECK_INTERVAL {
                    if let Err(err) = cleanup_stale_owner(&mut state) {
                        eprintln!("tun-helper stale owner cleanup failed: {err:#}");
                    }
                    last_owner_check = Instant::now();
                }
                std::thread::sleep(HELPER_IDLE_SLEEP);
            }
            Err(err) => return Err(err).context("failed to accept helper connection"),
        }
    }
}

fn handle_helper_client(
    stream: &mut UnixStream,
    allowed_user: &str,
    allowed_uid: u32,
    state: &mut HelperState,
) -> Result<()> {
    let (peer_uid, peer_gid) = peer_credentials(stream)?;
    if peer_uid != 0 && peer_uid != allowed_uid {
        anyhow::bail!("unauthorized peer uid={peer_uid} gid={peer_gid}");
    }

    let request = read_helper_request(stream)?;
    let response = match request.command.as_str() {
        "status" => {
            let active = state.active.as_ref();
            serde_json::json!({
                "ok": true,
                "helper": HELPER_LABEL,
                "version": env!("CARGO_PKG_VERSION"),
                "socket": HELPER_SOCKET_PATH,
                "allowed_user": allowed_user,
                "allowed_uid": allowed_uid,
                "peer_uid": peer_uid,
                "peer_gid": peer_gid,
                "tun": {
                    "active": active.is_some(),
                    "interface": active.map(|tun| tun.interface.as_str()).unwrap_or(""),
                    "outbound_interface": active
                        .and_then(|tun| tun.outbound_interface.as_deref())
                        .unwrap_or(""),
                    "owner_uid": active.map(|tun| tun.owner_uid).unwrap_or(0),
                    "owner_pid": active.map(|tun| tun.owner_pid).unwrap_or(0),
                    "subject_pid": active.and_then(|tun| tun.subject_pid).unwrap_or(0),
                    "routes_active": active.is_some_and(|tun| !tun.routes.is_empty() || tun.rule_active),
                    "route_count": active.map(|tun| tun.routes.len()).unwrap_or(0)
                }
            })
        }
        "prepare_tun" => {
            let prepare = request
                .prepare
                .context("prepare_tun request missing payload")?;
            let prepared = prepare_tun_for_client(state, prepare)?;
            let response = HelperResponse {
                ok: true,
                helper: Some(HELPER_LABEL.into()),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                socket: Some(HELPER_SOCKET_PATH.into()),
                interface: Some(prepared.interface.clone()),
                outbound_interface: prepared.outbound_interface.clone(),
                fd: Some(prepared.fd.as_raw_fd()),
                routes: Vec::new(),
                error: None,
            };
            send_fd_response(stream, prepared.fd.as_raw_fd(), &response)?;
            return Ok(());
        }
        "activate_routes" => {
            let owner = request
                .owner
                .context("activate_routes request missing owner")?;
            let routes = activate_routes_for_client(state, &owner)?;
            serde_json::json!({
                "ok": true,
                "helper": HELPER_LABEL,
                "routes": routes.iter().map(route_entry_summary).collect::<Vec<_>>()
            })
        }
        "deactivate_routes" => {
            let owner = request
                .owner
                .context("deactivate_routes request missing owner")?;
            deactivate_routes_for_client(state, &owner)?;
            serde_json::json!({
                "ok": true,
                "helper": HELPER_LABEL,
                "routes": []
            })
        }
        "teardown_tun" => {
            teardown_state(state)?;
            serde_json::json!({
                "ok": true,
                "helper": HELPER_LABEL,
                "teardown": "ok"
            })
        }
        "" => serde_json::json!({
            "ok": false,
            "error": "missing command"
        }),
        other => serde_json::json!({
            "ok": false,
            "error": format!("unsupported command: {other}")
        }),
    };
    writeln!(stream, "{response}")?;
    Ok(())
}

fn read_helper_request(stream: &UnixStream) -> Result<HelperRequest> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let input = line.trim();
    if input.starts_with('{') {
        let mut request: HelperRequest =
            serde_json::from_str(input).context("failed to decode helper request")?;
        request.command = request.command.trim().to_string();
        return Ok(request);
    }
    let command = input
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    Ok(HelperRequest {
        command,
        prepare: None,
        owner: None,
    })
}

fn helper_status() -> Result<String> {
    helper_request_with_timeout(r#"{"command":"status"}"#, HELPER_STATUS_TIMEOUT)
}

fn helper_request(request: &str) -> Result<String> {
    helper_request_with_timeout(request, HELPER_REQUEST_TIMEOUT)
}

fn helper_request_with_timeout(request: &str, timeout: Duration) -> Result<String> {
    let mut stream = UnixStream::connect(HELPER_SOCKET_PATH)
        .with_context(|| format!("failed to connect {HELPER_SOCKET_PATH}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .context("failed to set helper read timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("failed to set helper write timeout")?;
    writeln!(stream, "{request}").context("failed to write helper request")?;

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader
        .read_line(&mut response)
        .context("failed to read helper response")?;
    if response.trim().is_empty() {
        anyhow::bail!("empty helper response");
    }
    Ok(response)
}

fn helper_request_with_optional_fd(request: &str) -> Result<(HelperResponse, Option<OwnedFd>)> {
    let mut stream = UnixStream::connect(HELPER_SOCKET_PATH)
        .with_context(|| format!("failed to connect {HELPER_SOCKET_PATH}"))?;
    stream
        .set_read_timeout(Some(HELPER_REQUEST_TIMEOUT))
        .context("failed to set helper read timeout")?;
    stream
        .set_write_timeout(Some(HELPER_REQUEST_TIMEOUT))
        .context("failed to set helper write timeout")?;
    writeln!(stream, "{request}").context("failed to write helper request")?;
    recv_fd_response(&stream)
}

fn send_fd_response(stream: &UnixStream, fd: RawFd, response: &HelperResponse) -> Result<()> {
    let mut payload = serde_json::to_vec(response).context("failed to encode helper response")?;
    payload.push(b'\n');

    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: payload.len(),
    };
    let mut control =
        vec![0_u8; unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as libc::c_uint) as usize }];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast::<libc::c_void>();
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            anyhow::bail!("failed to allocate fd control message");
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<RawFd>() as libc::c_uint) as _;
        *(libc::CMSG_DATA(cmsg).cast::<RawFd>()) = fd;

        let sent = libc::sendmsg(stream.as_raw_fd(), &msg, 0);
        if sent < 0 {
            return Err(std::io::Error::last_os_error()).context("failed to send fd response");
        }
        if sent as usize != payload.len() {
            anyhow::bail!("short fd response write: {sent}/{}", payload.len());
        }
    }

    Ok(())
}

fn recv_fd_response(stream: &UnixStream) -> Result<(HelperResponse, Option<OwnedFd>)> {
    let mut payload = [0_u8; 8192];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast::<libc::c_void>(),
        iov_len: payload.len(),
    };
    let mut control =
        vec![0_u8; unsafe { libc::CMSG_SPACE(mem::size_of::<RawFd>() as libc::c_uint) as usize }];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast::<libc::c_void>();
    msg.msg_controllen = control.len() as _;

    let mut received_fd: Option<RawFd> = None;
    let received = unsafe {
        let received = libc::recvmsg(stream.as_raw_fd(), &mut msg, 0);
        if received < 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to receive helper response");
        }
        if msg.msg_flags & libc::MSG_CTRUNC != 0 {
            anyhow::bail!("helper response control message was truncated");
        }

        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET
                && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                && (*cmsg).cmsg_len >= libc::CMSG_LEN(mem::size_of::<RawFd>() as libc::c_uint) as _
            {
                received_fd = Some(*(libc::CMSG_DATA(cmsg).cast::<RawFd>()));
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        received
    };

    if received == 0 {
        anyhow::bail!("empty helper response");
    }
    let response: HelperResponse = serde_json::from_slice(&payload[..received as usize])
        .context("failed to decode helper response")?;
    let fd = received_fd.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    Ok((response, fd))
}

struct PreparedTun {
    interface: String,
    outbound_interface: Option<String>,
    fd: OwnedFd,
}

fn prepare_tun_for_client(
    state: &mut HelperState,
    request: PrepareTunRequest,
) -> Result<PreparedTun> {
    validate_prepare_request(&request)?;
    if let Err(err) = teardown_state(state) {
        eprintln!("tun-helper stale teardown before prepare failed: {err:#}");
    }

    let outbound_interface = default_route_interface().ok();
    let fd = create_tun(&request.device)?;
    let interface = request.device.clone();

    if let Err(err) = configure_tun_interface(&interface, request.mtu) {
        let _ = bring_interface_down(&interface);
        return Err(err);
    }

    state.active = Some(ActiveTun {
        interface: interface.clone(),
        outbound_interface: outbound_interface.clone(),
        owner_uid: request.owner_uid,
        owner_pid: request.owner_pid,
        auto_route: request.auto_route,
        route_exclude_address: request.route_exclude_address,
        routes: Vec::new(),
        rule_active: false,
        subject_pid: None,
    });

    Ok(PreparedTun {
        interface,
        outbound_interface,
        fd,
    })
}

fn activate_routes_for_client(
    state: &mut HelperState,
    owner: &OwnerRequest,
) -> Result<Vec<RouteEntry>> {
    let active = state
        .active
        .as_mut()
        .context("no active TUN lease to activate")?;
    validate_owner(active, owner)?;
    let subject_pid = owner
        .subject_pid
        .context("activate_routes requires a mihomo subject pid")?;
    validate_subject_pid(active, subject_pid)?;
    active.subject_pid = Some(subject_pid);
    if !active.auto_route || (!active.routes.is_empty() && active.rule_active) {
        return Ok(active.routes.clone());
    }
    if !owner.allow_experimental_routes {
        anyhow::bail!(
            "Linux TUN helper route activation is guarded until cgroup/fwmark loop prevention is implemented; rerun with CLASHTUI_LINUX_TUN_EXPERIMENTAL_ROUTES=1 only inside the guarded test script"
        );
    }

    let mut routes = Vec::new();
    if let Err(err) = configure_tun_routes(&active.interface, &mut routes) {
        let _ = teardown_routes(&routes);
        return Err(err);
    }
    if let Err(err) = add_route_rule() {
        let _ = teardown_routes(&routes);
        return Err(err);
    }
    active.routes = routes;
    active.rule_active = true;
    Ok(active.routes.clone())
}

fn deactivate_routes_for_client(state: &mut HelperState, owner: &OwnerRequest) -> Result<()> {
    let Some(active) = state.active.as_mut() else {
        return Ok(());
    };
    validate_cleanup_owner(active, owner)?;
    let routes = std::mem::take(&mut active.routes);
    let rule_active = std::mem::take(&mut active.rule_active);
    let rule_result = if rule_active {
        delete_route_rule()
    } else {
        Ok(())
    };
    let routes_result = teardown_routes(&routes);
    rule_result?;
    routes_result
}

fn validate_owner(active: &ActiveTun, owner: &OwnerRequest) -> Result<()> {
    if owner.owner_uid != active.owner_uid {
        anyhow::bail!(
            "owner uid mismatch: active={} requested={}",
            active.owner_uid,
            owner.owner_uid
        );
    }
    if owner.owner_pid != active.owner_pid {
        anyhow::bail!(
            "owner pid mismatch: active={} requested={}",
            active.owner_pid,
            owner.owner_pid
        );
    }
    Ok(())
}

fn validate_cleanup_owner(active: &ActiveTun, owner: &OwnerRequest) -> Result<()> {
    if owner.owner_uid != active.owner_uid {
        anyhow::bail!(
            "owner uid mismatch: active={} requested={}",
            active.owner_uid,
            owner.owner_uid
        );
    }
    if owner.owner_pid == active.owner_pid || !process_exists(active.owner_pid) {
        return Ok(());
    }
    anyhow::bail!(
        "owner pid mismatch: active={} requested={}",
        active.owner_pid,
        owner.owner_pid
    )
}

fn validate_subject_pid(active: &ActiveTun, subject_pid: u32) -> Result<()> {
    if !process_exists(subject_pid) {
        anyhow::bail!("subject pid {subject_pid} is not running");
    }
    let subject_uid = process_uid(subject_pid)?;
    if subject_uid != active.owner_uid {
        anyhow::bail!(
            "subject pid uid mismatch: active_owner={} subject_pid={} subject_uid={}",
            active.owner_uid,
            subject_pid,
            subject_uid
        );
    }
    Ok(())
}

fn process_uid(pid: u32) -> Result<u32> {
    let status_path = format!("/proc/{pid}/status");
    let status = fs::read_to_string(&status_path)
        .with_context(|| format!("failed to read {status_path}"))?;
    for line in status.lines() {
        let Some(rest) = line.strip_prefix("Uid:") else {
            continue;
        };
        let uid = rest
            .split_whitespace()
            .next()
            .context("Uid line did not contain a real uid")?
            .parse::<u32>()
            .with_context(|| format!("failed to parse real uid for pid {pid}"))?;
        return Ok(uid);
    }
    anyhow::bail!("{status_path} did not contain a Uid line")
}

fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if status == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn peer_credentials(stream: &UnixStream) -> Result<(u32, u32)> {
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read peer credentials");
    }
    Ok((cred.uid, cred.gid))
}

#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

fn create_tun(device: &str) -> Result<OwnedFd> {
    validate_interface_name(device)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(TUN_DEVICE_PATH)
        .with_context(|| format!("failed to open {TUN_DEVICE_PATH}"))?;
    let fd: OwnedFd = file.into();
    let mut request = IfReq {
        name: [0; libc::IFNAMSIZ],
        flags: IFF_TUN | IFF_NO_PI,
        _pad: [0; 22],
    };
    for (index, byte) in device.bytes().enumerate() {
        request.name[index] = byte as libc::c_char;
    }
    let status = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF, &mut request) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to create TUN device");
    }
    Ok(fd)
}

fn configure_tun_interface(interface: &str, mtu: u16) -> Result<()> {
    validate_interface_name(interface)?;
    run_status(
        Command::new("ip").args(["addr", "replace", DEFAULT_TUN_ADDR_CIDR, "dev", interface]),
        "ip addr replace tun",
    )?;
    run_status(
        Command::new("ip").args([
            "link",
            "set",
            "dev",
            interface,
            "mtu",
            &mtu.to_string(),
            "up",
        ]),
        "ip link set tun up",
    )
}

fn configure_tun_routes(interface: &str, routes: &mut Vec<RouteEntry>) -> Result<()> {
    validate_interface_name(interface)?;
    for destination in DEFAULT_ROUTES {
        let entry = RouteEntry {
            destination: destination.into(),
            table: ROUTE_TABLE.into(),
            interface: interface.into(),
        };
        add_route(entry.clone())?;
        routes.push(entry);
    }
    Ok(())
}

fn add_route(route: RouteEntry) -> Result<()> {
    validate_ipv4_cidr(&route.destination)?;
    validate_table(&route.table)?;
    validate_interface_name(&route.interface)?;
    run_status(
        Command::new("ip").args([
            "route",
            "replace",
            &route.destination,
            "dev",
            &route.interface,
            "table",
            &route.table,
        ]),
        "ip route replace",
    )
}

fn delete_route(route: &RouteEntry) -> Result<()> {
    validate_ipv4_cidr(&route.destination)?;
    validate_table(&route.table)?;
    validate_interface_name(&route.interface)?;
    run_status_idempotent(
        Command::new("ip").args([
            "route",
            "delete",
            &route.destination,
            "dev",
            &route.interface,
            "table",
            &route.table,
        ]),
        "ip route delete",
    )
}

fn add_route_rule() -> Result<()> {
    let _ = delete_route_rule();
    run_status(
        Command::new("ip").args([
            "rule",
            "add",
            "priority",
            ROUTE_RULE_PRIORITY,
            "lookup",
            ROUTE_TABLE,
        ]),
        "ip rule add",
    )
}

fn delete_route_rule() -> Result<()> {
    run_status_idempotent(
        Command::new("ip").args([
            "rule",
            "delete",
            "priority",
            ROUTE_RULE_PRIORITY,
            "lookup",
            ROUTE_TABLE,
        ]),
        "ip rule delete",
    )
}

fn teardown_state(state: &mut HelperState) -> Result<()> {
    let Some(active) = state.active.take() else {
        return Ok(());
    };
    let rule_result = if active.rule_active {
        delete_route_rule()
    } else {
        Ok(())
    };
    let routes_result = teardown_routes(&active.routes);
    let down_result = bring_interface_down(&active.interface);
    rule_result?;
    routes_result?;
    down_result
}

fn bring_interface_down(interface: &str) -> Result<()> {
    if !interface_exists(interface) {
        return Ok(());
    }
    run_status(
        Command::new("ip").args(["link", "set", "dev", interface, "down"]),
        "ip link set tun down",
    )
}

fn interface_exists(interface: &str) -> bool {
    validate_interface_name(interface).is_ok()
        && Command::new("ip")
            .args(["link", "show", "dev", interface])
            .output()
            .is_ok_and(|output| output.status.success())
}

fn cleanup_stale_owner(state: &mut HelperState) -> Result<()> {
    let Some(active) = state.active.as_ref() else {
        return Ok(());
    };
    let owner_alive = process_exists(active.owner_pid);
    let subject_alive = active.subject_pid.is_none_or(process_exists);
    if owner_alive && subject_alive {
        return Ok(());
    }
    let reason = if !owner_alive {
        format!("owner pid={} is gone", active.owner_pid)
    } else {
        format!(
            "subject pid={} is gone",
            active.subject_pid.unwrap_or_default()
        )
    };
    eprintln!(
        "tun-helper {reason}; tearing down interface={} routes={}",
        active.interface,
        active.routes.len()
    );
    teardown_state(state)
}

fn linux_experimental_routes_enabled() -> bool {
    env::var("CLASHTUI_LINUX_TUN_EXPERIMENTAL_ROUTES").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn teardown_routes(routes: &[RouteEntry]) -> Result<()> {
    let mut last_error = None;
    for route in routes.iter().rev() {
        if let Err(err) = delete_route(route) {
            last_error = Some(err);
        }
    }
    if let Some(err) = last_error {
        return Err(err).context("failed to delete one or more TUN routes");
    }
    Ok(())
}

fn default_route_interface() -> Result<String> {
    let output = Command::new("ip")
        .args(["route", "show", "default"])
        .output()
        .context("failed to run ip route show default")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ip route show default failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let words = line.split_whitespace().collect::<Vec<_>>();
        for pair in words.windows(2) {
            if pair[0] == "dev" {
                validate_interface_name_relaxed(pair[1])?;
                return Ok(pair[1].into());
            }
        }
    }
    anyhow::bail!("default route did not report an interface")
}

fn validate_prepare_request(request: &PrepareTunRequest) -> Result<()> {
    validate_interface_name(&request.device)?;
    if request.mtu < 576 {
        anyhow::bail!("invalid TUN MTU {}; expected >= 576", request.mtu);
    }
    for route in &request.route_exclude_address {
        validate_ipv4_cidr(route)?;
    }
    Ok(())
}

fn validate_interface_name(interface: &str) -> Result<()> {
    if interface.is_empty() || interface.len() >= libc::IFNAMSIZ {
        anyhow::bail!("invalid TUN interface name: {interface}");
    }
    if !interface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("invalid TUN interface name: {interface}");
    }
    Ok(())
}

fn validate_interface_name_relaxed(interface: &str) -> Result<()> {
    validate_interface_name(interface)
}

fn validate_ipv4_addr(value: &str) -> Result<()> {
    value
        .parse::<Ipv4Addr>()
        .with_context(|| format!("invalid IPv4 address {value}"))?;
    Ok(())
}

fn validate_ipv4_cidr(value: &str) -> Result<()> {
    let value = value.trim();
    if let Some((addr, prefix)) = value.split_once('/') {
        validate_ipv4_addr(addr)?;
        let prefix = prefix
            .parse::<u8>()
            .with_context(|| format!("invalid IPv4 CIDR prefix {value}"))?;
        if prefix > 32 {
            anyhow::bail!("invalid IPv4 CIDR prefix {prefix}; expected 0..=32");
        }
        return Ok(());
    }
    validate_ipv4_addr(value)
}

fn validate_table(value: &str) -> Result<()> {
    let table = value
        .parse::<u32>()
        .with_context(|| format!("invalid route table {value}"))?;
    if table == 0 {
        anyhow::bail!("invalid route table {value}");
    }
    Ok(())
}

fn route_entry_summary(route: &RouteEntry) -> String {
    format!(
        "{} -> dev {} table {}",
        route.destination, route.interface, route.table
    )
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() as u32 }
}

fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn ensure_root(action: &str) -> Result<()> {
    if is_root() {
        return Ok(());
    }
    anyhow::bail!("{action} requires root privileges")
}

fn target_binary(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path,
        None => {
            let exe =
                std::env::current_exe().context("failed to locate current clashtui executable")?;
            default_helper_artifact(&exe).unwrap_or(exe)
        }
    };
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn default_helper_artifact(exe: &Path) -> Option<PathBuf> {
    let sibling = exe.with_file_name(HELPER_ARTIFACT_NAME);
    sibling.exists().then_some(sibling)
}

fn helper_needs_entrypoint_arg(target: &Path) -> bool {
    target
        .file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| name != HELPER_ARTIFACT_NAME)
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

fn install_helper_binary(target: &Path) -> Result<()> {
    if let Some(parent) = Path::new(HELPER_BINARY_PATH).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::copy(target, HELPER_BINARY_PATH).with_context(|| {
        format!(
            "failed to copy {} to {HELPER_BINARY_PATH}",
            target.display()
        )
    })?;
    fs::set_permissions(HELPER_BINARY_PATH, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to chmod {HELPER_BINARY_PATH}"))?;
    run_status(
        Command::new("chown").args(["root:root", HELPER_BINARY_PATH]),
        "chown helper",
    )
}

fn install_systemd_service(user: &str, uid: u32, include_entrypoint_arg: bool) -> Result<()> {
    let path = Path::new(HELPER_SERVICE_PATH);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(path, systemd_service(user, uid, include_entrypoint_arg))
        .with_context(|| format!("failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("failed to chmod {}", path.display()))
}

fn systemd_service(user: &str, uid: u32, include_entrypoint_arg: bool) -> String {
    let exec_start = if include_entrypoint_arg {
        format!("{HELPER_BINARY_PATH} __tun-helper-run")
    } else {
        HELPER_BINARY_PATH.into()
    };
    format!(
        "[Unit]\n\
         Description=clashtui TUN helper\n\
         After=network.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec_start}\n\
         Restart=on-failure\n\
         RestartSec=1\n\
         Environment=CLASHTUI_TUN_HELPER_UID={uid}\n\
         Environment=CLASHTUI_TUN_HELPER_USER={user}\n\
         StandardOutput=append:{HELPER_LOG_PATH}\n\
         StandardError=append:{HELPER_LOG_PATH}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

fn reload_systemd() -> Result<()> {
    run_status(
        Command::new("systemctl").arg("daemon-reload"),
        "systemctl daemon-reload",
    )
}

fn enable_and_start_helper() -> Result<()> {
    run_status(
        Command::new("systemctl").args(["enable", "--now", "clashtui-tun-helper.service"]),
        "systemctl enable --now helper",
    )
}

fn stop_helper() -> Result<()> {
    run_status(
        Command::new("systemctl").args(["disable", "--now", "clashtui-tun-helper.service"]),
        "systemctl disable --now helper",
    )
}

fn wait_for_helper() -> Result<String> {
    let mut last_error = None;
    for _ in 0..HELPER_START_RETRIES {
        match helper_status() {
            Ok(status) => return Ok(status),
            Err(err) => last_error = Some(err),
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match last_error {
        Some(err) => Err(err).context("TUN helper did not become reachable"),
        None => anyhow::bail!("TUN helper did not become reachable"),
    }
}

fn helper_installed() -> bool {
    Path::new(HELPER_BINARY_PATH).exists() && Path::new(HELPER_SERVICE_PATH).exists()
}

fn remove_socket_if_exists() -> Result<()> {
    remove_file_if_exists(HELPER_SOCKET_PATH)
}

fn remove_file_if_exists(path: &str) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {path}")),
    }
}

fn invoking_user() -> Result<String> {
    env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("USERNAME"))
        .context("failed to determine invoking user")
}

fn user_uid(user: &str) -> Result<u32> {
    let output = Command::new("id")
        .args(["-u", user])
        .output()
        .with_context(|| format!("failed to run id -u {user}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("id -u {user} failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .trim()
        .parse::<u32>()
        .with_context(|| format!("failed to parse uid for {user}"))
}

fn has_any_capabilities(path: &Path) -> Result<bool> {
    Ok(!getcap_output(path)?.trim().is_empty())
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

fn path_to_str(path: &Path) -> Result<&str> {
    path.to_str()
        .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
}

fn run_status(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {action}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    anyhow::bail!(
        "{action} failed status={} stdout={} stderr={}",
        output.status,
        stdout.trim(),
        stderr.trim()
    )
}

fn run_status_idempotent(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {action}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr_trimmed = stderr.trim();
    if stderr_trimmed.contains("No such process")
        || stderr_trimmed.contains("No such file or directory")
        || stderr_trimmed.contains("Cannot find")
    {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    anyhow::bail!(
        "{action} failed status={} stdout={} stderr={}",
        output.status,
        stdout.trim(),
        stderr_trimmed
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_separate_helper_artifact_name() {
        assert!(helper_needs_entrypoint_arg(Path::new("/tmp/clashtui")));
        assert!(!helper_needs_entrypoint_arg(Path::new(
            "/tmp/clashtui-tun-helper"
        )));
    }

    #[test]
    fn validates_helper_route_inputs() {
        assert!(validate_interface_name("Mihomo").is_ok());
        assert!(validate_interface_name("../bad").is_err());
        assert!(validate_ipv4_cidr("0.0.0.0/1").is_ok());
        assert!(validate_ipv4_cidr("0.0.0.0/33").is_err());
    }

    #[test]
    fn systemd_service_uses_entrypoint_only_for_same_binary_fallback() {
        let direct = systemd_service("alice", 1000, false);
        assert!(direct.contains("ExecStart=/usr/local/libexec/clashtui-tun-helper\n"));
        assert!(!direct.contains("__tun-helper-run"));

        let fallback = systemd_service("alice", 1000, true);
        assert!(
            fallback
                .contains("ExecStart=/usr/local/libexec/clashtui-tun-helper __tun-helper-run\n")
        );
    }
}
