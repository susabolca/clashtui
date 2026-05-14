use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::config::Paths;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ServiceStatus {
    pub installed: bool,
    pub reachable: bool,
    pub version: Option<String>,
    pub core_running: bool,
    pub core_pid: Option<u32>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartCoreRequest {
    pub core_path: String,
    pub config_path: String,
    /// User-owned source directory. The privileged service copies needed inputs
    /// into its own root-owned runtime directory before starting mihomo.
    pub work_dir: String,
    /// Kept for request compatibility with older installed service binaries.
    /// Current service-owned mihomo uses root-owned log and pid files instead.
    pub log_file: String,
    /// Kept for request compatibility with older installed service binaries.
    pub pid_file: String,
}

pub fn install(path: Option<PathBuf>) -> Result<()> {
    imp::install(path)
}

pub fn install_privileged(path: PathBuf, user: String) -> Result<()> {
    imp::install_privileged(path, user)
}

pub fn uninstall() -> Result<()> {
    imp::uninstall()
}

pub fn uninstall_privileged() -> Result<()> {
    imp::uninstall_privileged()
}

pub fn status() -> Result<ServiceStatus> {
    imp::status()
}

pub fn print_status() -> Result<()> {
    let status = status()?;
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
    );
    Ok(())
}

pub fn run() -> Result<()> {
    imp::run()
}

pub fn start_core(core_path: &Path, paths: &Paths, config_file: &Path) -> Result<()> {
    imp::start_core(StartCoreRequest {
        core_path: core_path.to_string_lossy().into_owned(),
        config_path: config_file.to_string_lossy().into_owned(),
        work_dir: paths.config_dir.to_string_lossy().into_owned(),
        log_file: paths.core_log_file.to_string_lossy().into_owned(),
        pid_file: paths.core_pid_file.to_string_lossy().into_owned(),
    })
}

pub fn stop_core() -> Result<()> {
    imp::stop_core()
}

pub fn core_running() -> Result<bool> {
    Ok(status()?.core_running)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod imp {
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::{BufRead as _, BufReader, Write as _};
    #[cfg(target_os = "linux")]
    use std::mem;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt as _;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context as _, Result};
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::{ServiceStatus, StartCoreRequest};

    #[cfg(target_os = "macos")]
    const SERVICE_LABEL: &str = "com.clashtui.service";
    #[cfg(target_os = "linux")]
    const SERVICE_LABEL: &str = "clashtui.service";
    #[cfg(target_os = "macos")]
    const SERVICE_BINARY_PATH: &str = "/Library/PrivilegedHelperTools/com.clashtui.service";
    #[cfg(target_os = "linux")]
    const SERVICE_BINARY_PATH: &str = "/usr/local/libexec/clashtui-service";
    #[cfg(target_os = "macos")]
    const SERVICE_PLIST_PATH: &str = "/Library/LaunchDaemons/com.clashtui.service.plist";
    #[cfg(target_os = "linux")]
    const SERVICE_PLIST_PATH: &str = "/etc/systemd/system/clashtui.service";
    #[cfg(target_os = "macos")]
    const SERVICE_SOCKET_PATH: &str = "/var/run/com.clashtui.service.sock";
    #[cfg(target_os = "linux")]
    const SERVICE_SOCKET_PATH: &str = "/run/com.clashtui.service.sock";
    #[cfg(target_os = "macos")]
    const SERVICE_CORE_STATE_PATH: &str = "/var/run/com.clashtui.service.core.json";
    #[cfg(target_os = "linux")]
    const SERVICE_CORE_STATE_PATH: &str = "/run/com.clashtui.service.core.json";
    const SERVICE_LOG_PATH: &str = "/var/log/clashtui-service.log";
    #[cfg(target_os = "macos")]
    const SERVICE_RUNTIME_BASE_PATH: &str = "/Library/Application Support/clashtui/service";
    #[cfg(target_os = "linux")]
    const SERVICE_RUNTIME_BASE_PATH: &str = "/var/lib/clashtui";
    #[cfg(target_os = "macos")]
    const LEGACY_TUN_HELPER_BINARY_PATH: &str =
        "/Library/PrivilegedHelperTools/com.clashtui.tun-helper";
    #[cfg(target_os = "linux")]
    const LEGACY_TUN_HELPER_BINARY_PATH: &str = "/usr/local/libexec/clashtui-tun-helper";
    #[cfg(target_os = "macos")]
    const LEGACY_TUN_HELPER_PLIST_PATH: &str =
        "/Library/LaunchDaemons/com.clashtui.tun-helper.plist";
    #[cfg(target_os = "linux")]
    const LEGACY_TUN_HELPER_PLIST_PATH: &str = "/etc/systemd/system/clashtui-tun-helper.service";
    #[cfg(target_os = "macos")]
    const LEGACY_TUN_HELPER_SOCKET_PATH: &str = "/var/run/com.clashtui.tun-helper.sock";
    #[cfg(target_os = "linux")]
    const LEGACY_TUN_HELPER_SOCKET_PATH: &str = "/run/com.clashtui.tun-helper.sock";
    const LEGACY_TUN_HELPER_LOG_PATH: &str = "/var/log/clashtui-tun-helper.log";
    const SERVICE_GEODATA_FILES: [&str; 7] = [
        "Country.mmdb",
        "geoip.metadb",
        "GeoIP.dat",
        "geoip.dat",
        "GeoSite.dat",
        "geosite.dat",
        "ASN.mmdb",
    ];
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    const START_WAIT: Duration = Duration::from_millis(250);
    const STOP_WAIT: Duration = Duration::from_millis(100);
    const STOP_RETRIES: usize = 150;

    #[derive(Debug, Deserialize)]
    struct ServiceRequest {
        command: String,
        start: Option<StartCoreRequest>,
    }

    struct CoreState {
        child: Child,
        pid_file: PathBuf,
    }

    struct ServiceRuntimePaths {
        work_dir: PathBuf,
        config_file: PathBuf,
        log_file: PathBuf,
        pid_file: PathBuf,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct PersistedCoreState {
        pid: u32,
        pid_file: String,
    }

    #[derive(Default)]
    struct ServiceState {
        core: Option<CoreState>,
    }

    pub fn install(path: Option<PathBuf>) -> Result<()> {
        let target = target_binary(path)?;
        let user = invoking_user()?;
        if !is_root_user() {
            print_sudo_install_notice();
            return run_sudo_install(&target, &user);
        }
        install_privileged(target, user)
    }

    pub fn install_privileged(target: PathBuf, user: String) -> Result<()> {
        ensure_root("install clashtui service")?;
        let uid = user_uid(&user)?;

        let _ = stop_core();
        let _ = stop_persisted_core_state();
        let _ = stop_matching_service_runtime_cores(&service_runtime_paths(uid));
        let _ = unload_service();
        let _ = stop_persisted_core_state();
        let _ = stop_matching_service_runtime_cores(&service_runtime_paths(uid));
        cleanup_legacy_tun_helper()?;
        remove_socket_if_exists()?;
        install_service_binary(&target)?;
        install_launchdaemon_plist(&user, uid)?;
        load_service()?;

        println!("service installed: {SERVICE_BINARY_PATH}");
        println!("service-definition: {SERVICE_PLIST_PATH}");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        if !is_root_user() {
            return run_sudo_uninstall();
        }
        uninstall_privileged()
    }

    pub fn uninstall_privileged() -> Result<()> {
        ensure_root("uninstall clashtui service")?;
        let user = invoking_user().unwrap_or_else(|_| env::var("SUDO_USER").unwrap_or_default());
        if let Ok(uid) = user_uid(&user) {
            let _ = stop_matching_service_runtime_cores(&service_runtime_paths(uid));
        }
        let _ = stop_core();
        let _ = stop_persisted_core_state();
        let _ = unload_service();
        let _ = stop_persisted_core_state();
        remove_file_if_exists(Path::new(SERVICE_PLIST_PATH))?;
        remove_file_if_exists(Path::new(SERVICE_BINARY_PATH))?;
        remove_socket_if_exists()?;
        remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH))?;
        remove_dir_all_if_exists(Path::new(SERVICE_RUNTIME_BASE_PATH))?;
        cleanup_legacy_tun_helper()?;
        println!("service removed: {SERVICE_BINARY_PATH}");
        Ok(())
    }

    pub fn status() -> Result<ServiceStatus> {
        let installed =
            Path::new(SERVICE_BINARY_PATH).exists() && Path::new(SERVICE_PLIST_PATH).exists();
        match service_request::<ServiceStatus>(&json!({ "command": "status" })) {
            Ok(mut status) => {
                status.installed = installed;
                Ok(status)
            }
            Err(err) => Ok(ServiceStatus {
                installed,
                reachable: false,
                message: Some(format!("{err:#}")),
                ..ServiceStatus::default()
            }),
        }
    }

    pub fn start_core(start: StartCoreRequest) -> Result<()> {
        let status: ServiceStatus = service_request(&json!({
            "command": "start_core",
            "start": start
        }))?;
        if !status.core_running {
            anyhow::bail!(
                "service did not report a running mihomo core: {}",
                status.message.as_deref().unwrap_or("unknown")
            );
        }
        Ok(())
    }

    pub fn stop_core() -> Result<()> {
        let _status: ServiceStatus = service_request(&json!({ "command": "stop_core" }))?;
        Ok(())
    }

    pub fn run() -> Result<()> {
        ensure_root("run clashtui service")?;
        remove_socket_if_exists()?;
        let listener = UnixListener::bind(SERVICE_SOCKET_PATH)
            .with_context(|| format!("failed to bind {SERVICE_SOCKET_PATH}"))?;
        fs::set_permissions(SERVICE_SOCKET_PATH, fs::Permissions::from_mode(0o666))
            .with_context(|| format!("failed to chmod {SERVICE_SOCKET_PATH}"))?;

        let allowed_uid = env::var("CLASHTUI_SERVICE_UID")
            .ok()
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let allowed_user = env::var("CLASHTUI_SERVICE_USER").unwrap_or_else(|_| "root".into());
        eprintln!(
            "clashtui service started label={SERVICE_LABEL} socket={SERVICE_SOCKET_PATH} allowed_user={allowed_user} allowed_uid={allowed_uid}"
        );

        let mut state = ServiceState::default();
        for stream in listener.incoming() {
            match stream {
                Ok(mut stream) => {
                    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                    if let Err(err) = handle_client(&mut stream, allowed_uid, &mut state) {
                        let _ = writeln!(
                            stream,
                            "{}",
                            json!({
                                "installed": true,
                                "reachable": true,
                                "message": format!("{err:#}")
                            })
                        );
                    }
                }
                Err(err) => eprintln!("service accept failed: {err}"),
            }
        }
        Ok(())
    }

    fn handle_client(
        stream: &mut UnixStream,
        allowed_uid: u32,
        state: &mut ServiceState,
    ) -> Result<()> {
        let (peer_uid, _peer_gid) = peer_ids(stream)?;
        if peer_uid != 0 && peer_uid != allowed_uid {
            anyhow::bail!("caller uid {peer_uid} is not authorized");
        }

        refresh_core_state(state);
        let request = read_request(stream)?;
        let runtime_uid = if peer_uid == 0 { allowed_uid } else { peer_uid };
        let response = match request.command.as_str() {
            "status" => status_response(state, None, runtime_uid),
            "start_core" => {
                let start = request
                    .start
                    .context("start_core request missing payload")?;
                start_core_for_state(state, start, runtime_uid)?;
                status_response(state, Some("started".into()), runtime_uid)
            }
            "stop_core" => {
                stop_core_for_state(state, runtime_uid)?;
                status_response(state, Some("stopped".into()), runtime_uid)
            }
            other => status_response(
                state,
                Some(format!("unknown command: {other}")),
                runtime_uid,
            ),
        };
        writeln!(stream, "{}", serde_json::to_string(&response)?)?;
        Ok(())
    }

    fn start_core_for_state(
        state: &mut ServiceState,
        start: StartCoreRequest,
        runtime_uid: u32,
    ) -> Result<()> {
        stop_core_for_state(state, runtime_uid)?;
        let core_path = PathBuf::from(&start.core_path);
        let source_config_path = PathBuf::from(&start.config_path);
        let source_work_dir = PathBuf::from(&start.work_dir);

        ensure_safe_file(&core_path, "core binary")?;
        ensure_safe_file(&source_config_path, "mihomo config")?;
        let runtime_paths =
            prepare_service_runtime(&source_config_path, &source_work_dir, runtime_uid)?;
        stop_pid_file_core(&runtime_paths.pid_file)?;
        stop_matching_service_runtime_cores(&runtime_paths)?;

        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&runtime_paths.log_file)
            .with_context(|| format!("failed to open {}", runtime_paths.log_file.display()))?;
        fs::set_permissions(&runtime_paths.log_file, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", runtime_paths.log_file.display()))?;
        let err = log
            .try_clone()
            .with_context(|| format!("failed to clone {}", runtime_paths.log_file.display()))?;

        let mut child = Command::new(&core_path)
            .args([
                "-d",
                path_to_str(&runtime_paths.work_dir)?,
                "-f",
                path_to_str(&runtime_paths.config_file)?,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(err))
            .spawn()
            .with_context(|| format!("failed to start mihomo core {}", core_path.display()))?;
        let pid = child.id();
        fs::write(&runtime_paths.pid_file, pid.to_string())
            .with_context(|| format!("failed to write {}", runtime_paths.pid_file.display()))?;
        fs::set_permissions(&runtime_paths.pid_file, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", runtime_paths.pid_file.display()))?;
        write_persisted_core_state(pid, &runtime_paths.pid_file)?;

        thread::sleep(START_WAIT);
        if !pid_running(pid) {
            let _ = child.try_wait();
            let _ = fs::remove_file(&runtime_paths.pid_file);
            let _ = remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH));
            anyhow::bail!(
                "mihomo core exited during startup; check log={}",
                runtime_paths.log_file.display()
            );
        }
        let pid_file = runtime_paths.pid_file;
        let runtime_config = runtime_paths.config_file;
        state.core = Some(CoreState { child, pid_file });
        eprintln!(
            "mihomo core started by service pid={pid} uid={runtime_uid} work_dir={} config={}",
            service_runtime_paths(runtime_uid).work_dir.display(),
            runtime_config.display()
        );
        Ok(())
    }

    fn prepare_service_runtime(
        source_config_path: &Path,
        source_work_dir: &Path,
        runtime_uid: u32,
    ) -> Result<ServiceRuntimePaths> {
        let paths = service_runtime_paths(runtime_uid);
        fs::create_dir_all(&paths.work_dir)
            .with_context(|| format!("failed to create {}", paths.work_dir.display()))?;
        fs::set_permissions(&paths.work_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to chmod {}", paths.work_dir.display()))?;

        fs::copy(source_config_path, &paths.config_file).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_config_path.display(),
                paths.config_file.display()
            )
        })?;
        fs::set_permissions(&paths.config_file, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to chmod {}", paths.config_file.display()))?;

        copy_service_geodata(source_work_dir, &paths.work_dir)?;
        Ok(paths)
    }

    fn copy_service_geodata(source_work_dir: &Path, runtime_work_dir: &Path) -> Result<()> {
        for file_name in SERVICE_GEODATA_FILES {
            let source = source_work_dir.join(file_name);
            if !source.is_file() {
                continue;
            }
            let target = runtime_work_dir.join(file_name);
            fs::copy(&source, &target).with_context(|| {
                format!(
                    "failed to copy geodata {} to {}",
                    source.display(),
                    target.display()
                )
            })?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("failed to chmod {}", target.display()))?;
        }
        Ok(())
    }

    fn service_runtime_paths(runtime_uid: u32) -> ServiceRuntimePaths {
        let work_dir = Path::new(SERVICE_RUNTIME_BASE_PATH).join(runtime_uid.to_string());
        ServiceRuntimePaths {
            config_file: work_dir.join("mihomo-run.yaml"),
            log_file: work_dir.join("mihomo.log"),
            pid_file: work_dir.join("mihomo.pid"),
            work_dir,
        }
    }

    fn stop_core_for_state(state: &mut ServiceState, runtime_uid: u32) -> Result<()> {
        let Some(mut core) = state.core.take() else {
            stop_persisted_core_state()?;
            return stop_matching_service_runtime_cores(&service_runtime_paths(runtime_uid));
        };
        let pid = core.child.id();
        terminate_pid(pid)?;
        let deadline = Instant::now() + STOP_WAIT * STOP_RETRIES as u32;
        while Instant::now() < deadline {
            if let Some(_status) = core
                .child
                .try_wait()
                .context("failed to poll mihomo core")?
            {
                let _ = fs::remove_file(&core.pid_file);
                let _ = remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH));
                let _ = stop_matching_service_runtime_cores(&service_runtime_paths(runtime_uid));
                return Ok(());
            }
            thread::sleep(STOP_WAIT);
        }
        let _ = core.child.kill();
        let _ = core.child.wait();
        let _ = fs::remove_file(&core.pid_file);
        let _ = remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH));
        stop_matching_service_runtime_cores(&service_runtime_paths(runtime_uid))?;
        Ok(())
    }

    fn refresh_core_state(state: &mut ServiceState) {
        let Some(core) = state.core.as_mut() else {
            return;
        };
        if matches!(core.child.try_wait(), Ok(Some(_))) {
            let pid_file = core.pid_file.clone();
            state.core = None;
            let _ = fs::remove_file(pid_file);
            let _ = remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH));
        }
    }

    fn status_response(
        state: &ServiceState,
        message: Option<String>,
        runtime_uid: u32,
    ) -> ServiceStatus {
        let core_pid = state
            .core
            .as_ref()
            .map(|core| core.child.id())
            .or_else(persisted_core_pid)
            .or_else(|| find_service_runtime_core_pid(runtime_uid));
        ServiceStatus {
            installed: true,
            reachable: true,
            version: Some(env!("CARGO_PKG_VERSION").into()),
            core_running: core_pid.is_some_and(pid_running),
            core_pid,
            message,
        }
    }

    fn write_persisted_core_state(pid: u32, pid_file: &Path) -> Result<()> {
        let state = PersistedCoreState {
            pid,
            pid_file: pid_file.to_string_lossy().into_owned(),
        };
        let payload = serde_json::to_vec(&state)?;
        fs::write(SERVICE_CORE_STATE_PATH, payload)
            .with_context(|| format!("failed to write {SERVICE_CORE_STATE_PATH}"))
    }

    fn read_persisted_core_state() -> Option<PersistedCoreState> {
        let payload = fs::read(SERVICE_CORE_STATE_PATH).ok()?;
        serde_json::from_slice(&payload).ok()
    }

    fn persisted_core_pid() -> Option<u32> {
        let state = read_persisted_core_state()?;
        if pid_running(state.pid) {
            Some(state.pid)
        } else {
            let _ = fs::remove_file(state.pid_file);
            let _ = remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH));
            None
        }
    }

    fn stop_persisted_core_state() -> Result<()> {
        let Some(state) = read_persisted_core_state() else {
            return Ok(());
        };
        stop_pid(state.pid)?;
        let _ = fs::remove_file(state.pid_file);
        remove_file_if_exists(Path::new(SERVICE_CORE_STATE_PATH))
    }

    fn stop_pid_file_core(pid_file: &Path) -> Result<()> {
        let content = match fs::read_to_string(pid_file) {
            Ok(content) => content,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", pid_file.display()));
            }
        };
        if let Ok(pid) = content.trim().parse::<u32>() {
            stop_pid(pid)?;
        }
        remove_file_if_exists(pid_file)
    }

    fn find_service_runtime_core_pid(runtime_uid: u32) -> Option<u32> {
        matching_service_runtime_core_pids(&service_runtime_paths(runtime_uid))
            .ok()?
            .into_iter()
            .find(|pid| pid_running(*pid))
    }

    fn stop_matching_service_runtime_cores(runtime_paths: &ServiceRuntimePaths) -> Result<()> {
        for pid in matching_service_runtime_core_pids(runtime_paths)? {
            stop_pid(pid)?;
        }
        let _ = remove_file_if_exists(&runtime_paths.pid_file);
        Ok(())
    }

    fn matching_service_runtime_core_pids(runtime_paths: &ServiceRuntimePaths) -> Result<Vec<u32>> {
        let work_dir = path_to_str(&runtime_paths.work_dir)?;
        let config_file = path_to_str(&runtime_paths.config_file)?;
        let output = Command::new("ps")
            .args(["-axo", "pid=,command="])
            .output()
            .context("failed to list processes for service mihomo cleanup")?;
        if !output.status.success() {
            return Ok(Vec::new());
        }

        let current_pid = std::process::id();
        let mut pids = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let mut parts = line.trim_start().splitn(2, char::is_whitespace);
            let Some(pid_text) = parts.next() else {
                continue;
            };
            let Ok(pid) = pid_text.parse::<u32>() else {
                continue;
            };
            if pid == 0 || pid == current_pid {
                continue;
            }
            let Some(command) = parts.next().map(str::trim_start) else {
                continue;
            };
            if command.contains(work_dir) && command.contains(config_file) {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    fn stop_pid(pid: u32) -> Result<()> {
        if !pid_running(pid) {
            return Ok(());
        }
        terminate_pid(pid)?;
        if wait_pid_stopped(pid) {
            return Ok(());
        }
        force_kill_pid(pid)?;
        if wait_pid_stopped(pid) {
            Ok(())
        } else {
            anyhow::bail!("pid {pid} did not exit after stop")
        }
    }

    fn wait_pid_stopped(pid: u32) -> bool {
        for _ in 0..STOP_RETRIES {
            if !pid_running(pid) {
                return true;
            }
            thread::sleep(STOP_WAIT);
        }
        !pid_running(pid)
    }

    fn read_request(stream: &mut UnixStream) -> Result<ServiceRequest> {
        let mut line = String::new();
        BufReader::new(stream.try_clone()?).read_line(&mut line)?;
        serde_json::from_str(line.trim()).context("failed to parse service request")
    }

    fn service_request<T: for<'de> Deserialize<'de>>(request: &serde_json::Value) -> Result<T> {
        let mut stream = UnixStream::connect(SERVICE_SOCKET_PATH)
            .with_context(|| format!("failed to connect {SERVICE_SOCKET_PATH}"))?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        writeln!(stream, "{request}")?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        serde_json::from_str(line.trim()).context("failed to parse service response")
    }

    #[cfg(target_os = "macos")]
    fn peer_ids(stream: &UnixStream) -> Result<(u32, u32)> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        let status = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        if status != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect peer credentials");
        }
        Ok((uid as u32, gid as u32))
    }

    #[cfg(target_os = "linux")]
    fn peer_ids(stream: &UnixStream) -> Result<(u32, u32)> {
        let mut credentials: libc::ucred = unsafe { mem::zeroed() };
        let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                &mut credentials as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to inspect peer credentials");
        }
        Ok((credentials.uid as u32, credentials.gid as u32))
    }

    fn target_binary(path: Option<PathBuf>) -> Result<PathBuf> {
        let path = match path {
            Some(path) => path,
            None => env::current_exe().context("failed to locate current executable")?,
        };
        path.canonicalize()
            .with_context(|| format!("failed to resolve {}", path.display()))
    }

    fn invoking_user() -> Result<String> {
        env::var("SUDO_USER")
            .or_else(|_| env::var("USER"))
            .or_else(|_| env::var("LOGNAME"))
            .context("failed to determine invoking user")
    }

    fn user_uid(user: &str) -> Result<u32> {
        let output = Command::new("id")
            .args(["-u", user])
            .output()
            .with_context(|| format!("failed to resolve uid for {user}"))?;
        if !output.status.success() {
            anyhow::bail!("failed to resolve uid for {user}");
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .with_context(|| format!("invalid uid for {user}"))
    }

    fn run_sudo_install(target: &Path, user: &str) -> Result<()> {
        run_status(
            Command::new("sudo")
                .arg(target)
                .arg("__service-install-root")
                .arg("--path")
                .arg(target)
                .arg("--user")
                .arg(user),
            "sudo service install",
        )
    }

    fn print_sudo_install_notice() {
        println!("sudo is required to install the privileged clashtui service.");
        println!(
            "Impact: installs a root service and lets service-owned mihomo create TUN when enabled."
        );
        println!("Uninstall later with: clashtui service-uninstall");
    }

    fn run_sudo_uninstall() -> Result<()> {
        let target = env::current_exe().context("failed to locate current executable")?;
        run_status(
            Command::new("sudo")
                .arg(target)
                .arg("__service-uninstall-root"),
            "sudo service uninstall",
        )
    }

    fn install_service_binary(source: &Path) -> Result<()> {
        let target = Path::new(SERVICE_BINARY_PATH);
        fs::create_dir_all(
            target
                .parent()
                .context("service binary path has no parent")?,
        )?;
        fs::copy(source, target).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source.display(),
                target.display()
            )
        })?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o755))?;
        run_status(
            Command::new("chown").arg(service_file_owner()).arg(target),
            "chown service binary",
        )
    }

    fn install_launchdaemon_plist(user: &str, uid: u32) -> Result<()> {
        let path = Path::new(SERVICE_PLIST_PATH);
        fs::create_dir_all(
            path.parent()
                .context("service definition path has no parent")?,
        )?;
        fs::write(path, service_definition(user, uid))
            .with_context(|| format!("failed to write {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
        run_status(
            Command::new("chown").arg(service_file_owner()).arg(path),
            "chown service definition",
        )
    }

    #[cfg(target_os = "macos")]
    fn service_definition(user: &str, uid: u32) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>__service-run</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>CLASHTUI_SERVICE_USER</key>
    <string>{user}</string>
    <key>CLASHTUI_SERVICE_UID</key>
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
            label = SERVICE_LABEL,
            binary = SERVICE_BINARY_PATH,
            user = xml_escape(user),
            uid = uid,
            log = SERVICE_LOG_PATH
        )
    }

    #[cfg(target_os = "linux")]
    fn service_definition(user: &str, uid: u32) -> String {
        format!(
            r#"[Unit]
Description=clashtui privileged service
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={binary} __service-run
Restart=always
RestartSec=1
Environment=CLASHTUI_SERVICE_USER={user}
Environment=CLASHTUI_SERVICE_UID={uid}
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=multi-user.target
"#,
            binary = SERVICE_BINARY_PATH,
            user = systemd_escape_value(user),
            uid = uid,
            log = SERVICE_LOG_PATH
        )
    }

    #[cfg(target_os = "macos")]
    fn load_service() -> Result<()> {
        run_status(
            Command::new("launchctl")
                .arg("bootstrap")
                .arg("system")
                .arg(SERVICE_PLIST_PATH),
            "launchctl bootstrap",
        )?;
        run_status(
            Command::new("launchctl")
                .arg("enable")
                .arg(format!("system/{SERVICE_LABEL}")),
            "launchctl enable",
        )?;
        run_status(
            Command::new("launchctl")
                .arg("kickstart")
                .arg("-k")
                .arg(format!("system/{SERVICE_LABEL}")),
            "launchctl kickstart",
        )
    }

    #[cfg(target_os = "linux")]
    fn load_service() -> Result<()> {
        reload_systemd()?;
        run_status(
            Command::new("systemctl")
                .arg("enable")
                .arg("--now")
                .arg(SERVICE_LABEL),
            "systemctl enable service",
        )
    }

    #[cfg(target_os = "macos")]
    fn unload_service() -> Result<()> {
        run_status(
            Command::new("launchctl")
                .arg("bootout")
                .arg("system")
                .arg(SERVICE_PLIST_PATH),
            "launchctl bootout",
        )
    }

    #[cfg(target_os = "linux")]
    fn unload_service() -> Result<()> {
        let result = run_status(
            Command::new("systemctl")
                .arg("disable")
                .arg("--now")
                .arg(SERVICE_LABEL),
            "systemctl disable service",
        );
        let _ = reload_systemd();
        result
    }

    #[cfg(target_os = "macos")]
    fn cleanup_legacy_tun_helper() -> Result<()> {
        let _ = run_status(
            Command::new("launchctl")
                .arg("bootout")
                .arg("system")
                .arg(LEGACY_TUN_HELPER_PLIST_PATH),
            "launchctl bootout legacy tun helper",
        );
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_PLIST_PATH))?;
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_BINARY_PATH))?;
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_SOCKET_PATH))?;
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_LOG_PATH))
    }

    #[cfg(target_os = "linux")]
    fn cleanup_legacy_tun_helper() -> Result<()> {
        let _ = run_status(
            Command::new("systemctl")
                .arg("disable")
                .arg("--now")
                .arg("clashtui-tun-helper.service"),
            "systemctl disable legacy tun helper",
        );
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_PLIST_PATH))?;
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_BINARY_PATH))?;
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_SOCKET_PATH))?;
        remove_file_if_exists(Path::new(LEGACY_TUN_HELPER_LOG_PATH))?;
        let _ = reload_systemd();
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn reload_systemd() -> Result<()> {
        run_status(
            Command::new("systemctl").arg("daemon-reload"),
            "systemctl daemon-reload",
        )
    }

    #[cfg(target_os = "macos")]
    fn service_file_owner() -> &'static str {
        "root:wheel"
    }

    #[cfg(target_os = "linux")]
    fn service_file_owner() -> &'static str {
        "root:root"
    }

    fn remove_file_if_exists(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
        }
    }

    fn remove_dir_all_if_exists(path: &Path) -> Result<()> {
        match fs::remove_dir_all(path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
        }
    }

    fn remove_socket_if_exists() -> Result<()> {
        remove_file_if_exists(Path::new(SERVICE_SOCKET_PATH))
    }

    fn run_status(command: &mut Command, label: &str) -> Result<()> {
        let output = command
            .output()
            .with_context(|| format!("failed to run {label}"))?;
        if output.status.success() {
            return Ok(());
        }
        anyhow::bail!(
            "{label} failed status={} stdout={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn ensure_root(action: &str) -> Result<()> {
        if is_root_user() {
            Ok(())
        } else {
            anyhow::bail!("{action} requires root")
        }
    }

    fn is_root_user() -> bool {
        unsafe { libc::geteuid() == 0 }
    }

    fn pid_running(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        let status = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if status == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn terminate_pid(pid: u32) -> Result<()> {
        signal_pid(pid, libc::SIGTERM)
    }

    fn force_kill_pid(pid: u32) -> Result<()> {
        signal_pid(pid, libc::SIGKILL)
    }

    fn signal_pid(pid: u32, signal: libc::c_int) -> Result<()> {
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

    fn ensure_safe_file(path: &Path, label: &str) -> Result<()> {
        if path.exists() && path.is_file() {
            Ok(())
        } else {
            anyhow::bail!(
                "{label} does not exist or is not a file: {}",
                path.display()
            )
        }
    }

    fn path_to_str(path: &Path) -> Result<&str> {
        path.to_str()
            .with_context(|| format!("path is not valid UTF-8: {}", path.display()))
    }

    fn xml_escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    #[cfg(target_os = "linux")]
    fn systemd_escape_value(value: &str) -> String {
        value.replace('\\', "\\\\").replace('"', "\\\"")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        #[cfg(target_os = "macos")]
        fn launchdaemon_uses_service_entrypoint() {
            let plist = service_definition("alice", 501);
            assert!(plist.contains(SERVICE_LABEL));
            assert!(plist.contains("__service-run"));
            assert!(plist.contains("CLASHTUI_SERVICE_UID"));
        }

        #[test]
        #[cfg(target_os = "linux")]
        fn systemd_service_uses_service_entrypoint() {
            let unit = service_definition("alice", 1000);
            assert!(unit.contains("ExecStart="));
            assert!(unit.contains("__service-run"));
            assert!(unit.contains("CLASHTUI_SERVICE_UID=1000"));
        }

        #[test]
        fn service_runtime_paths_are_root_owned_domain_paths() {
            let paths = service_runtime_paths(501);
            let work_dir = paths.work_dir.to_string_lossy();
            assert!(work_dir.contains("501"));
            assert!(work_dir.starts_with(SERVICE_RUNTIME_BASE_PATH));
            assert_eq!(paths.config_file.file_name().unwrap(), "mihomo-run.yaml");
            assert_eq!(paths.log_file.file_name().unwrap(), "mihomo.log");
            assert_eq!(paths.pid_file.file_name().unwrap(), "mihomo.pid");
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::path::PathBuf;

    use anyhow::Result;

    use super::{ServiceStatus, StartCoreRequest};

    pub fn install(_path: Option<PathBuf>) -> Result<()> {
        anyhow::bail!("clashtui service is not implemented on this platform yet")
    }

    pub fn install_privileged(_path: PathBuf, _user: String) -> Result<()> {
        anyhow::bail!("clashtui service is not implemented on this platform yet")
    }

    pub fn uninstall() -> Result<()> {
        anyhow::bail!("clashtui service is not implemented on this platform yet")
    }

    pub fn uninstall_privileged() -> Result<()> {
        anyhow::bail!("clashtui service is not implemented on this platform yet")
    }

    pub fn status() -> Result<ServiceStatus> {
        Ok(ServiceStatus {
            message: Some("clashtui service is not implemented on this platform yet".into()),
            ..ServiceStatus::default()
        })
    }

    pub fn run() -> Result<()> {
        anyhow::bail!("clashtui service is not implemented on this platform yet")
    }

    pub fn start_core(_start: StartCoreRequest) -> Result<()> {
        anyhow::bail!("clashtui service is not implemented on this platform yet")
    }

    pub fn stop_core() -> Result<()> {
        Ok(())
    }
}
