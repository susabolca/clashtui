use std::env;
use std::fs;
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
const HELPER_BINARY_PATH: &str = "/Library/PrivilegedHelperTools/com.clashtui.tun-helper";
const HELPER_PLIST_PATH: &str = "/Library/LaunchDaemons/com.clashtui.tun-helper.plist";
const HELPER_SOCKET_PATH: &str = "/var/run/com.clashtui.tun-helper.sock";
const HELPER_LOG_PATH: &str = "/var/log/clashtui-tun-helper.log";
const HELPER_ARTIFACT_NAME: &str = "clashtui-tun-helper";
const HELPER_STATUS_TIMEOUT: Duration = Duration::from_millis(300);
const HELPER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const HELPER_IDLE_SLEEP: Duration = Duration::from_millis(50);
const HELPER_OWNER_CHECK_INTERVAL: Duration = Duration::from_secs(1);
const HELPER_START_RETRIES: usize = 20;
const UTUN_CONTROL_NAME: &str = "com.apple.net.utun_control";
const UTUN_OPT_IFNAME: libc::c_int = 2;
const AF_SYS_KERNCONTROL: u16 = 2;
const DEFAULT_TUN_ADDR: &str = "198.18.0.1";
const DEFAULT_TUN_DEST: &str = "198.18.0.1";
const DEFAULT_TUN_NETMASK: &str = "255.255.255.252";
const DEFAULT_ROUTES: [&str; 2] = ["0.0.0.0/1", "128.0.0.0/1"];

#[derive(Debug)]
pub struct TunDevice {
    pub interface: String,
    pub outbound_interface: Option<String>,
    fd: OwnedFd,
}

impl TunDevice {
    pub fn file_descriptor(&self) -> RawFd {
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
    owner_uid: u32,
    owner_pid: u32,
    device: String,
    mtu: u16,
    auto_route: bool,
    route_exclude_address: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OwnerRequest {
    owner_uid: u32,
    owner_pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    subject_pid: Option<u32>,
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
    default_route: Option<DefaultRoute>,
    routes: Vec<RouteEntry>,
}

#[derive(Debug, Clone)]
struct RouteEntry {
    destination: String,
    gateway: RouteGateway,
    scope: Option<String>,
}

#[derive(Debug, Clone)]
struct DefaultRoute {
    gateway: String,
    interface: String,
}

#[derive(Debug, Clone)]
enum RouteGateway {
    Interface(String),
    Gateway(String),
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
    pub helper_reachable: bool,
}

impl TunPermissionStatus {
    pub const fn can_start_tun(&self) -> bool {
        self.helper_reachable
    }
}

pub fn current_tun_permission_status() -> Result<TunPermissionStatus> {
    let target = std::env::current_exe().context("failed to locate current clashtui executable")?;
    let is_root = is_root_user();
    let helper_installed = helper_installed();
    let helper_reachable = helper_status().is_ok();
    let helper_binary = Path::new(HELPER_BINARY_PATH);
    let capabilities = format!(
        "macOS helper label={} installed={} reachable={} binary={} socket={}",
        HELPER_LABEL,
        helper_installed,
        helper_reachable,
        helper_binary.display(),
        HELPER_SOCKET_PATH
    );

    Ok(TunPermissionStatus {
        target,
        capabilities,
        legacy_file_capabilities_detected: false,
        polkit_rule_path: HELPER_PLIST_PATH,
        polkit_rule_exists: helper_installed,
        polkit_rule_matches_user: helper_reachable,
        tun_device_exists: true,
        is_root,
        helper_reachable,
    })
}

pub fn tun_install(path: Option<PathBuf>) -> Result<()> {
    let target = target_binary(path)?;
    let user = invoking_user()?;

    if !is_root_user() {
        return run_sudo_install(&target, &user);
    }
    tun_install_privileged(target, user)
}

pub fn tun_uninstall(path: Option<PathBuf>) -> Result<()> {
    let target = target_binary(path)?;
    if !helper_installed() && !Path::new(HELPER_SOCKET_PATH).exists() {
        println!("TUN helper already removed: {HELPER_BINARY_PATH}");
        return Ok(());
    }

    if !is_root_user() {
        return run_sudo_privileged("__tun-uninstall-root", &target);
    }
    tun_uninstall_privileged(target)
}

pub fn tun_install_privileged(target: PathBuf, user: String) -> Result<()> {
    ensure_root("install macOS TUN helper")?;
    let uid = user_uid(&user)?;

    let _ = unload_helper();
    remove_socket_if_exists()?;
    install_helper_binary(&target)?;
    install_launchdaemon_plist(&user, uid, helper_needs_entrypoint_arg(&target))?;
    load_helper()?;
    let status = wait_for_helper()?;

    println!("TUN helper installed: {HELPER_BINARY_PATH}");
    println!("launchdaemon: {HELPER_PLIST_PATH}");
    println!("helper-status: {}", status.trim());
    Ok(())
}

pub fn tun_uninstall_privileged(_target: PathBuf) -> Result<()> {
    ensure_root("uninstall macOS TUN helper")?;

    if helper_status().is_ok() {
        let _ = helper_request(r#"{"command":"teardown_tun"}"#);
    }
    let _ = unload_helper();
    remove_file_if_exists(Path::new(HELPER_PLIST_PATH))?;
    remove_file_if_exists(Path::new(HELPER_BINARY_PATH))?;
    remove_socket_if_exists()?;

    println!("TUN helper removed: {HELPER_BINARY_PATH}");
    Ok(())
}

pub fn prepare_tun(config: &TunConfig) -> Result<TunDevice> {
    let config = crate::platform::tun::normalize_config(config);
    let request = HelperRequest {
        command: "prepare_tun".into(),
        prepare: Some(PrepareTunRequest {
            owner_uid: current_uid(),
            owner_pid: std::process::id(),
            device: config.device.clone(),
            mtu: config.mtu,
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
        "TUN helper teardown_tun failed: {}",
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
    ensure_root("run macOS TUN helper")?;
    let allowed_uid = env::var("CLASHTUI_TUN_HELPER_UID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let allowed_user = env::var("CLASHTUI_TUN_HELPER_USER").unwrap_or_else(|_| "root".into());

    remove_socket_if_exists()?;
    let listener = UnixListener::bind(HELPER_SOCKET_PATH)
        .with_context(|| format!("failed to bind {HELPER_SOCKET_PATH}"))?;
    listener
        .set_nonblocking(true)
        .context("failed to set helper listener nonblocking")?;
    fs::set_permissions(HELPER_SOCKET_PATH, fs::Permissions::from_mode(0o666))
        .with_context(|| format!("failed to chmod {HELPER_SOCKET_PATH}"))?;

    eprintln!(
        "clashtui tun-helper started label={} socket={} allowed_user={} allowed_uid={}",
        HELPER_LABEL, HELPER_SOCKET_PATH, allowed_user, allowed_uid
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
                    let response = serde_json::json!({
                        "ok": false,
                        "error": format!("{err:#}")
                    });
                    let _ = writeln!(stream, "{response}");
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
            Err(err) => eprintln!("tun-helper accept failed: {err}"),
        }
    }
}

fn handle_helper_client(
    stream: &mut UnixStream,
    allowed_user: &str,
    allowed_uid: u32,
    state: &mut HelperState,
) -> Result<()> {
    let (peer_uid, peer_gid) = peer_ids(stream)?;
    if peer_uid != 0 && peer_uid != allowed_uid {
        let response = serde_json::json!({
            "ok": false,
            "error": "caller is not authorized",
            "peer_uid": peer_uid,
            "allowed_uid": allowed_uid
        });
        writeln!(stream, "{response}")?;
        return Ok(());
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
                    "routes_active": active.is_some_and(|tun| !tun.routes.is_empty()),
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

    let requested_unit = utun_unit(&request.device)?;
    let default_route = default_route().ok();
    let outbound_interface = default_route.as_ref().map(|route| route.interface.clone());
    let fd = create_utun(requested_unit)?;
    let interface = utun_interface_name(fd.as_raw_fd())?;

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
        default_route,
        routes: Vec::new(),
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
    if !active.auto_route || !active.routes.is_empty() {
        return Ok(active.routes.clone());
    }

    let mut routes = Vec::new();
    if let Err(err) = configure_tun_routes(
        &active.interface,
        &active.route_exclude_address,
        active.default_route.as_ref(),
        &mut routes,
    ) {
        let _ = teardown_routes(&routes);
        return Err(err);
    }
    active.routes = routes;
    Ok(active.routes.clone())
}

fn deactivate_routes_for_client(state: &mut HelperState, owner: &OwnerRequest) -> Result<()> {
    let Some(active) = state.active.as_mut() else {
        return Ok(());
    };
    validate_cleanup_owner(active, owner)?;
    let routes = std::mem::take(&mut active.routes);
    teardown_routes(&routes)
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

fn process_exists(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if status == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn validate_prepare_request(request: &PrepareTunRequest) -> Result<()> {
    utun_unit(&request.device)?;
    if !(576..=9000).contains(&request.mtu) {
        anyhow::bail!("invalid TUN MTU {}; expected 576..=9000", request.mtu);
    }
    for route in &request.route_exclude_address {
        validate_ipv4_cidr(route).with_context(|| format!("invalid route exclude {route}"))?;
    }
    Ok(())
}

fn create_utun(unit: u32) -> Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::PF_SYSTEM, libc::SOCK_DGRAM, libc::SYSPROTO_CONTROL) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("failed to create utun socket");
    }

    let result = (|| {
        let mut info: libc::ctl_info = unsafe { mem::zeroed() };
        for (index, byte) in UTUN_CONTROL_NAME.bytes().enumerate() {
            info.ctl_name[index] = byte as libc::c_char;
        }

        let ioctl_status = unsafe { libc::ioctl(fd, libc::CTLIOCGINFO, &mut info) };
        if ioctl_status != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to resolve utun kernel control");
        }

        let mut addr: libc::sockaddr_ctl = unsafe { mem::zeroed() };
        addr.sc_len = mem::size_of::<libc::sockaddr_ctl>() as u8;
        addr.sc_family = libc::AF_SYSTEM as u8;
        addr.ss_sysaddr = AF_SYS_KERNCONTROL;
        addr.sc_id = info.ctl_id;
        addr.sc_unit = unit;

        let connect_status = unsafe {
            libc::connect(
                fd,
                (&addr as *const libc::sockaddr_ctl).cast::<libc::sockaddr>(),
                mem::size_of::<libc::sockaddr_ctl>() as libc::socklen_t,
            )
        };
        if connect_status != 0 {
            return Err(std::io::Error::last_os_error()).context("failed to connect utun socket");
        }

        Ok(())
    })();

    if let Err(err) = result {
        unsafe {
            libc::close(fd);
        }
        return Err(err);
    }

    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn utun_interface_name(fd: RawFd) -> Result<String> {
    let mut name = [0_u8; libc::IFNAMSIZ];
    let mut len = name.len() as libc::socklen_t;
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SYSPROTO_CONTROL,
            UTUN_OPT_IFNAME,
            name.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to read utun interface name");
    }
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    String::from_utf8(name[..end].to_vec()).context("utun interface name is not UTF-8")
}

fn configure_tun_interface(interface: &str, mtu: u16) -> Result<()> {
    validate_interface_name(interface)?;
    run_status(
        Command::new("/sbin/ifconfig")
            .arg(interface)
            .arg("inet")
            .arg(DEFAULT_TUN_ADDR)
            .arg(DEFAULT_TUN_DEST)
            .arg("netmask")
            .arg(DEFAULT_TUN_NETMASK)
            .arg("mtu")
            .arg(mtu.to_string())
            .arg("up"),
        "ifconfig utun",
    )
}

fn configure_tun_routes(
    interface: &str,
    route_exclude_address: &[String],
    default_route: Option<&DefaultRoute>,
    routes: &mut Vec<RouteEntry>,
) -> Result<()> {
    validate_interface_name(interface)?;

    if let Some(default_route) = default_route {
        validate_system_interface_name(&default_route.interface)?;
        for route in DEFAULT_ROUTES {
            let entry = RouteEntry {
                destination: route.into(),
                gateway: RouteGateway::Gateway(default_route.gateway.clone()),
                scope: Some(default_route.interface.clone()),
            };
            add_route(entry.clone())?;
            routes.push(entry);
        }
    }

    for route in route_exclude_address {
        validate_ipv4_cidr(route)?;
        if let Some(default_route) = default_route {
            let entry = RouteEntry {
                destination: route.clone(),
                gateway: RouteGateway::Gateway(default_route.gateway.clone()),
                scope: None,
            };
            add_route(entry.clone())?;
            routes.push(entry);
        }
    }

    for route in DEFAULT_ROUTES {
        let entry = RouteEntry {
            destination: route.into(),
            gateway: RouteGateway::Interface(interface.into()),
            scope: None,
        };
        add_route(entry.clone())?;
        routes.push(entry);
    }
    Ok(())
}

fn add_route(route: RouteEntry) -> Result<()> {
    let mut command = Command::new("/sbin/route");
    command
        .arg("-n")
        .arg("add")
        .arg("-net")
        .arg(&route.destination);
    match &route.gateway {
        RouteGateway::Interface(interface) => {
            validate_interface_name(interface)?;
            command.arg("-interface").arg(interface);
        }
        RouteGateway::Gateway(gateway) => {
            validate_ipv4_addr(gateway)?;
            command.arg(gateway);
        }
    }
    if let Some(scope) = &route.scope {
        validate_system_interface_name(scope)?;
        command.arg("-ifscope").arg(scope);
    }
    run_status(&mut command, "route add")
}

fn delete_route(route: &RouteEntry) -> Result<()> {
    let mut command = Command::new("/sbin/route");
    command
        .arg("-n")
        .arg("delete")
        .arg("-net")
        .arg(&route.destination);
    match &route.gateway {
        RouteGateway::Interface(interface) => {
            validate_interface_name(interface)?;
            command.arg("-interface").arg(interface);
        }
        RouteGateway::Gateway(gateway) => {
            validate_ipv4_addr(gateway)?;
            command.arg(gateway);
        }
    }
    if let Some(scope) = &route.scope {
        validate_system_interface_name(scope)?;
        command.arg("-ifscope").arg(scope);
    }
    run_status(&mut command, "route delete")
}

fn teardown_state(state: &mut HelperState) -> Result<()> {
    let Some(active) = state.active.take() else {
        return Ok(());
    };
    let routes_result = teardown_routes(&active.routes);
    let down_result = bring_interface_down(&active.interface);
    routes_result?;
    down_result
}

fn bring_interface_down(interface: &str) -> Result<()> {
    if !interface_exists(interface) {
        return Ok(());
    }
    run_status(
        Command::new("/sbin/ifconfig").arg(interface).arg("down"),
        "ifconfig utun down",
    )
}

fn interface_exists(interface: &str) -> bool {
    validate_interface_name(interface).is_ok()
        && Command::new("/sbin/ifconfig")
            .arg(interface)
            .output()
            .is_ok_and(|output| output.status.success())
}

fn cleanup_stale_owner(state: &mut HelperState) -> Result<()> {
    let Some(active) = state.active.as_ref() else {
        return Ok(());
    };
    if process_exists(active.owner_pid) {
        return Ok(());
    }
    eprintln!(
        "tun-helper owner pid={} is gone; tearing down interface={} routes={}",
        active.owner_pid,
        active.interface,
        active.routes.len()
    );
    teardown_state(state)
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

fn default_route() -> Result<DefaultRoute> {
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
    let mut gateway = None;
    let mut interface = None;
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("gateway:") {
            let value = value.trim();
            validate_ipv4_addr(value)?;
            gateway = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("interface:") {
            let value = value.trim();
            validate_system_interface_name(value)?;
            interface = Some(value.to_string());
        }
    }
    let gateway = gateway.context("route get default did not report a gateway")?;
    let interface = interface.context("route get default did not report an interface")?;
    Ok(DefaultRoute { gateway, interface })
}

fn validate_system_interface_name(interface: &str) -> Result<()> {
    if interface.is_empty() || interface.len() >= libc::IFNAMSIZ {
        anyhow::bail!("invalid system interface name: {interface}");
    }
    if !interface
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        anyhow::bail!("invalid system interface name: {interface}");
    }
    Ok(())
}

fn route_entry_summary(route: &RouteEntry) -> String {
    let scope = route
        .scope
        .as_ref()
        .map(|scope| format!(" scope {scope}"))
        .unwrap_or_default();
    match &route.gateway {
        RouteGateway::Interface(interface) => {
            format!("{} -> interface {interface}{scope}", route.destination)
        }
        RouteGateway::Gateway(gateway) => {
            format!("{} -> gateway {gateway}{scope}", route.destination)
        }
    }
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() as u32 }
}

fn validate_interface_name(interface: &str) -> Result<()> {
    let suffix = interface
        .strip_prefix("utun")
        .with_context(|| format!("unsupported TUN interface {interface}; expected utunN"))?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("unsupported TUN interface {interface}; expected utunN");
    }
    if interface.len() >= libc::IFNAMSIZ {
        anyhow::bail!("TUN interface name is too long: {interface}");
    }
    Ok(())
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

fn utun_unit(device: &str) -> Result<u32> {
    let device = device.trim();
    if device.is_empty() || device == "utun" {
        return Ok(0);
    }
    let suffix = device
        .strip_prefix("utun")
        .with_context(|| format!("unsupported TUN device {device}; expected utunN"))?;
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        anyhow::bail!("unsupported TUN device {device}; expected utunN");
    }
    let unit = suffix
        .parse::<u32>()
        .with_context(|| format!("invalid TUN device unit {device}"))?;
    unit.checked_add(1)
        .with_context(|| format!("TUN device unit is too large: {device}"))
}

fn peer_ids(stream: &UnixStream) -> Result<(u32, u32)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let status = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("failed to inspect peer credentials");
    }
    Ok((uid as u32, gid as u32))
}

fn install_helper_binary(source: &Path) -> Result<()> {
    let target = Path::new(HELPER_BINARY_PATH);
    let parent = target
        .parent()
        .context("helper binary path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::copy(source, target).with_context(|| {
        format!(
            "failed to copy {} to {}",
            source.display(),
            target.display()
        )
    })?;
    fs::set_permissions(target, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("failed to chmod {}", target.display()))?;
    run_status(
        Command::new("chown").arg("root:wheel").arg(target),
        "chown helper",
    )?;
    Ok(())
}

fn install_launchdaemon_plist(user: &str, uid: u32, include_entrypoint_arg: bool) -> Result<()> {
    let path = Path::new(HELPER_PLIST_PATH);
    let parent = path
        .parent()
        .context("helper plist path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::write(path, launchdaemon_plist(user, uid, include_entrypoint_arg))
        .with_context(|| format!("failed to write {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))
        .with_context(|| format!("failed to chmod {}", path.display()))?;
    run_status(
        Command::new("chown").arg("root:wheel").arg(path),
        "chown plist",
    )?;
    Ok(())
}

fn launchdaemon_plist(user: &str, uid: u32, include_entrypoint_arg: bool) -> String {
    let entrypoint_arg = if include_entrypoint_arg {
        "    <string>__tun-helper-run</string>\n"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{helper}</string>
{entrypoint_arg}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>CLASHTUI_TUN_HELPER_USER</key>
    <string>{user}</string>
    <key>CLASHTUI_TUN_HELPER_UID</key>
    <string>{uid}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml_escape(HELPER_LABEL),
        helper = xml_escape(HELPER_BINARY_PATH),
        entrypoint_arg = entrypoint_arg,
        user = xml_escape(user),
        uid = uid,
        log = xml_escape(HELPER_LOG_PATH)
    )
}

fn load_helper() -> Result<()> {
    run_status(
        Command::new("launchctl")
            .arg("bootstrap")
            .arg("system")
            .arg(HELPER_PLIST_PATH),
        "launchctl bootstrap",
    )?;
    run_status(
        Command::new("launchctl")
            .arg("enable")
            .arg(format!("system/{HELPER_LABEL}")),
        "launchctl enable",
    )?;
    run_status(
        Command::new("launchctl")
            .arg("kickstart")
            .arg("-k")
            .arg(format!("system/{HELPER_LABEL}")),
        "launchctl kickstart",
    )
}

fn unload_helper() -> Result<()> {
    run_status(
        Command::new("launchctl")
            .arg("bootout")
            .arg("system")
            .arg(HELPER_PLIST_PATH),
        "launchctl bootout",
    )
}

fn wait_for_helper() -> Result<String> {
    let mut last_error = None;
    for _ in 0..HELPER_START_RETRIES {
        match helper_status() {
            Ok(status) => return Ok(status),
            Err(err) => last_error = Some(err),
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    match last_error {
        Some(err) => Err(err).context("TUN helper did not become reachable"),
        None => anyhow::bail!("TUN helper did not become reachable"),
    }
}

fn target_binary(path: Option<PathBuf>) -> Result<PathBuf> {
    let path = match path {
        Some(path) => path,
        None => default_helper_artifact()?,
    };
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn default_helper_artifact() -> Result<PathBuf> {
    let current =
        std::env::current_exe().context("failed to locate current clashtui executable")?;
    let Some(parent) = current.parent() else {
        return Ok(current);
    };
    let helper = parent.join(HELPER_ARTIFACT_NAME);
    if helper.exists() {
        return Ok(helper);
    }
    Ok(current)
}

fn helper_needs_entrypoint_arg(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
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

fn run_status(command: &mut Command, label: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("failed to run {label}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    anyhow::bail!(
        "{label} failed status={} stderr={} stdout={}",
        output.status,
        stderr.trim(),
        stdout.trim()
    );
}

fn remove_socket_if_exists() -> Result<()> {
    remove_file_if_exists(Path::new(HELPER_SOCKET_PATH))
}

fn remove_file_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn helper_installed() -> bool {
    Path::new(HELPER_BINARY_PATH).exists() && Path::new(HELPER_PLIST_PATH).exists()
}

fn ensure_root(action: &str) -> Result<()> {
    if is_root_user() {
        return Ok(());
    }
    anyhow::bail!("{action} requires root privileges")
}

fn is_root_user() -> bool {
    unsafe { libc::geteuid() == 0 }
}

fn invoking_user() -> Result<String> {
    env::var("SUDO_USER")
        .or_else(|_| env::var("USER"))
        .or_else(|_| env::var("USERNAME"))
        .context("failed to determine invoking user")
}

fn user_uid(user: &str) -> Result<u32> {
    let output = Command::new("id")
        .arg("-u")
        .arg(user)
        .output()
        .with_context(|| format!("failed to resolve uid for {user}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("failed to resolve uid for {user}: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .trim()
        .parse::<u32>()
        .with_context(|| format!("failed to parse uid for {user}: {}", stdout.trim()))
}

fn xml_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '&' => "&amp;".chars().collect::<Vec<_>>(),
            '<' => "&lt;".chars().collect(),
            '>' => "&gt;".chars().collect(),
            '"' => "&quot;".chars().collect(),
            '\'' => "&apos;".chars().collect(),
            ch => vec![ch],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_utun_units_for_kernel_control() -> Result<()> {
        assert_eq!(utun_unit("")?, 0);
        assert_eq!(utun_unit("utun")?, 0);
        assert_eq!(utun_unit("utun0")?, 1);
        assert_eq!(utun_unit("utun1024")?, 1025);
        assert!(utun_unit("Mihomo").is_err());
        assert!(utun_unit("utunx").is_err());
        Ok(())
    }

    #[test]
    fn validates_helper_route_inputs() {
        assert!(validate_interface_name("utun1024").is_ok());
        assert!(validate_interface_name("en0").is_err());
        assert!(validate_interface_name("utun").is_err());

        assert!(validate_ipv4_cidr("0.0.0.0/1").is_ok());
        assert!(validate_ipv4_cidr("128.0.0.0/1").is_ok());
        assert!(validate_ipv4_cidr("192.168.1.1").is_ok());
        assert!(validate_ipv4_cidr("192.168.0.0/33").is_err());
        assert!(validate_ipv4_cidr("::1/128").is_err());
    }

    #[test]
    fn launchdaemon_uses_entrypoint_only_for_same_binary_fallback() {
        let helper_plist = launchdaemon_plist("alice", 501, false);
        assert!(!helper_plist.contains("__tun-helper-run"));

        let fallback_plist = launchdaemon_plist("alice", 501, true);
        assert!(fallback_plist.contains("__tun-helper-run"));
    }

    #[test]
    fn detects_separate_helper_artifact_name() {
        assert!(!helper_needs_entrypoint_arg(Path::new(
            "/tmp/clashtui-tun-helper"
        )));
        assert!(helper_needs_entrypoint_arg(Path::new("/tmp/clashtui")));
    }
}
