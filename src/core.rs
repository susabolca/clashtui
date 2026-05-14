use std::env;
use std::fs::OpenOptions;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};
use flate2::read::GzDecoder;
use reqwest::{
    Client, Proxy,
    header::{HeaderMap, HeaderValue, USER_AGENT},
};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tokio::time::sleep;

use crate::config::{AppConfig, Paths, PortProxyService, RuntimePaths};
use crate::mihomo::MihomoClient;
use crate::port_allocator;
use crate::runtime_profile;
use crate::service;
use crate::system_proxy;

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
const STOP_WAIT: Duration = Duration::from_millis(200);
const STOP_RETRIES: usize = 75;
const CORE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);
const CORE_DOWNLOAD_USER_AGENT: &str = concat!("clashtui/v", env!("CARGO_PKG_VERSION"));
const META_ALPHA_VERSION_URL: &str =
    "https://github.com/MetaCubeX/mihomo/releases/download/Prerelease-Alpha/version.txt";
const META_ALPHA_URL_PREFIX: &str =
    "https://github.com/MetaCubeX/mihomo/releases/download/Prerelease-Alpha";
const META_VERSION_URL: &str =
    "https://github.com/MetaCubeX/mihomo/releases/latest/download/version.txt";
const META_URL_PREFIX: &str = "https://github.com/MetaCubeX/mihomo/releases/download";

pub const CORE_SOURCE_AUTO: &str = "auto";
pub const CORE_SOURCE_RELEASE: &str = "verge-mihomo";
pub const CORE_SOURCE_ALPHA: &str = "verge-mihomo-alpha";
pub const CORE_SOURCE_CUSTOM: &str = "custom";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreSource {
    Auto,
    Release,
    Alpha,
    Custom,
}

impl CoreSource {
    pub const ALL: [Self; 4] = [Self::Auto, Self::Release, Self::Alpha, Self::Custom];

    pub const fn value(self) -> &'static str {
        match self {
            Self::Auto => CORE_SOURCE_AUTO,
            Self::Release => CORE_SOURCE_RELEASE,
            Self::Alpha => CORE_SOURCE_ALPHA,
            Self::Custom => CORE_SOURCE_CUSTOM,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Release => "Mihomo",
            Self::Alpha => "Mihomo Alpha",
            Self::Custom => "Custom Path",
        }
    }

    pub fn parse(value: &str) -> Self {
        match normalize_core_source(value).as_str() {
            CORE_SOURCE_RELEASE | "release" | "stable" | "mihomo" => Self::Release,
            CORE_SOURCE_ALPHA | "alpha" => Self::Alpha,
            CORE_SOURCE_CUSTOM | "path" => Self::Custom,
            _ => Self::Auto,
        }
    }

    const fn managed(self) -> Option<ManagedCore> {
        match self {
            Self::Release => Some(ManagedCore::Release),
            Self::Alpha => Some(ManagedCore::Alpha),
            Self::Auto | Self::Custom => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManagedCoreInstall {
    pub source: CoreSource,
    pub path: PathBuf,
    pub version: String,
    pub updated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManagedCore {
    Release,
    Alpha,
}

impl ManagedCore {
    const fn source(self) -> CoreSource {
        match self {
            Self::Release => CoreSource::Release,
            Self::Alpha => CoreSource::Alpha,
        }
    }

    const fn binary_name(self) -> &'static str {
        match self {
            Self::Release => CORE_SOURCE_RELEASE,
            Self::Alpha => CORE_SOURCE_ALPHA,
        }
    }

    const fn version_url(self) -> &'static str {
        match self {
            Self::Release => META_VERSION_URL,
            Self::Alpha => META_ALPHA_VERSION_URL,
        }
    }

    const fn url_prefix(self) -> &'static str {
        match self {
            Self::Release => META_URL_PREFIX,
            Self::Alpha => META_ALPHA_URL_PREFIX,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedCoreMetadata {
    source: String,
    version: String,
    asset: String,
}

struct DownloadAttempt {
    label: &'static str,
    proxy_url: Option<String>,
}

pub async fn ensure_running(paths: &Paths, config: &mut AppConfig) -> Result<()> {
    if config.use_service_runtime() {
        let status = service::status()?;
        if status.reachable {
            return ensure_service_runtime(paths, config).await;
        }

        let mut fallback_config = config.clone();
        fallback_config.runtime_backend = "single".into();
        fallback_config.tun.enable = false;
        return ensure_user_single_runtime(paths, &fallback_config).await;
    }

    if config.use_single_runtime() {
        return ensure_user_single_runtime(paths, config).await;
    }

    ensure_legacy_global_runtime(paths, config).await
}

async fn ensure_legacy_global_runtime(paths: &Paths, config: &AppConfig) -> Result<()> {
    let instance = global_instance(paths, config);
    if let Some(pid) = read_pid(&instance.pid_file).await?
        && is_process_running(pid)
    {
        return Ok(());
    }

    remove_pid(&instance.pid_file).await?;
    let core_path = ensure_core_path(paths, config).await?;

    ensure_geodata(&instance.work_dir).await?;
    port_allocator::validate_required_ports_available(config)?;

    let mut bootstrap_config = config.clone();
    if bootstrap_config.active_profile.is_some() {
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
    start_instance(&instance, &core_path).await?;
    Ok(())
}

async fn ensure_user_single_runtime(paths: &Paths, config: &AppConfig) -> Result<()> {
    let instance = global_instance(paths, config);
    if let Some(pid) = read_pid(&instance.pid_file).await?
        && is_process_running(pid)
    {
        return Ok(());
    }

    remove_pid(&instance.pid_file).await?;
    let core_path = ensure_core_path(paths, config).await?;

    ensure_geodata(&paths.config_dir).await?;
    port_allocator::validate_required_ports_available(config)?;
    runtime_profile::write_single_runtime_config(paths, config).await?;
    start_instance(&instance, &core_path).await
}

async fn ensure_service_runtime(paths: &Paths, config: &AppConfig) -> Result<()> {
    if service::core_running()? {
        return Ok(());
    }

    service::stop_core()?;
    stop_legacy_global(paths).await?;

    let core_path = ensure_core_path(paths, config).await?;

    ensure_geodata(&paths.config_dir).await?;
    port_allocator::validate_required_ports_available(config)?;
    let runtime_config = runtime_profile::write_single_runtime_config(paths, config).await?;
    service::start_core(&core_path, paths, &runtime_config)
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
    let core_path = ensure_core_path(paths, config)
        .await
        .with_context(|| format!("{} mihomo core is not available", instance.label))?;

    ensure_geodata(&instance.work_dir).await?;
    runtime_profile::write_service_config(paths, &instance, config, service).await?;
    start_instance(&instance, &core_path).await?;
    Ok(instance)
}

pub async fn owned_core_running(paths: &Paths) -> Result<bool> {
    Ok(read_pid(&paths.core_pid_file)
        .await?
        .is_some_and(is_process_running))
}

pub async fn owned_core_running_for(paths: &Paths, config: &AppConfig) -> Result<bool> {
    if config.use_service_runtime() {
        let status = service::status()?;
        if status.reachable {
            return Ok(status.core_running);
        }
    }
    owned_core_running(paths).await
}

pub async fn ensure_controller_is_owned(
    paths: &Paths,
    config: &AppConfig,
    client: &MihomoClient,
) -> Result<()> {
    if owned_core_running_for(paths, config).await? {
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

pub async fn stop(paths: &Paths, config: &AppConfig) -> Result<()> {
    if config.use_service_runtime() {
        if service::status()?.reachable {
            service::stop_core()?;
        }
        return stop_legacy_global(paths).await;
    }
    let instance = paths.global_runtime(String::new());
    stop_instance(&instance).await
}

pub async fn stop_legacy_global(paths: &Paths) -> Result<()> {
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
    stop(paths, config).await?;
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
        if !wait_for_exit(pid).await {
            force_terminate_process(pid).with_context(|| {
                format!("failed to force stop {} mihomo pid {pid}", instance.label)
            })?;
        }
        if !wait_for_exit(pid).await {
            anyhow::bail!(
                "{} mihomo pid {pid} did not exit after stop",
                instance.label
            );
        }
    }
    remove_pid(&instance.pid_file).await
}

pub fn selected_core_source(config: &AppConfig) -> CoreSource {
    CoreSource::parse(&config.mihomo.core)
}

pub fn resolve_core_path(paths: &Paths, config: &AppConfig) -> Option<PathBuf> {
    match selected_core_source(config) {
        CoreSource::Custom => configured_core_path(config, true),
        CoreSource::Release => existing_managed_core_path(paths, ManagedCore::Release),
        CoreSource::Alpha => existing_managed_core_path(paths, ManagedCore::Alpha),
        CoreSource::Auto => resolve_auto_core_path(paths, config),
    }
}

pub async fn ensure_core_path(paths: &Paths, config: &AppConfig) -> Result<PathBuf> {
    match selected_core_source(config) {
        CoreSource::Custom => configured_core_path(config, true).with_context(|| {
            format!(
                "custom mihomo core is missing; set core_path in {} or choose auto",
                paths.config_file.display()
            )
        }),
        CoreSource::Release => Ok(ensure_managed_core(paths, ManagedCore::Release, false)
            .await?
            .path),
        CoreSource::Alpha => Ok(ensure_managed_core(paths, ManagedCore::Alpha, false)
            .await?
            .path),
        CoreSource::Auto => ensure_auto_core_path(paths, config).await,
    }
}

pub async fn update_managed_core(paths: &Paths, config: &AppConfig) -> Result<ManagedCoreInstall> {
    let core = selected_core_source(config)
        .managed()
        .unwrap_or(ManagedCore::Release);
    ensure_managed_core(paths, core, true).await
}

async fn ensure_auto_core_path(paths: &Paths, config: &AppConfig) -> Result<PathBuf> {
    if let Some(path) = configured_core_path(config, false) {
        return Ok(path);
    }
    if let Some(path) = env_core_path() {
        return Ok(path);
    }

    if let Some(path) = existing_managed_core_path(paths, ManagedCore::Release) {
        return Ok(path);
    }

    match ensure_managed_core(paths, ManagedCore::Release, false).await {
        Ok(install) => return Ok(install.path),
        Err(err) => {
            if let Some(path) = resolve_unmanaged_core_path() {
                eprintln!("mihomo core: managed download failed, using discovered core: {err:#}");
                return Ok(path);
            }
            Err(err).with_context(|| {
                format!(
                    "mihomo core is not running and no core binary was found; set core_path in {} or MIHOMO_CORE",
                    paths.config_file.display()
                )
            })
        }
    }
}

fn resolve_auto_core_path(paths: &Paths, config: &AppConfig) -> Option<PathBuf> {
    configured_core_path(config, false)
        .or_else(env_core_path)
        .or_else(|| existing_managed_core_path(paths, ManagedCore::Release))
        .or_else(resolve_unmanaged_core_path)
}

fn resolve_unmanaged_core_path() -> Option<PathBuf> {
    resolve_sibling_core()
        .or_else(resolve_known_app_core)
        .or_else(resolve_path_core)
}

fn configured_core_path(config: &AppConfig, strict_custom: bool) -> Option<PathBuf> {
    let value = config.core_path.as_deref()?.trim();
    if value.is_empty() {
        return None;
    }
    if !strict_custom && is_core_source_alias(value) {
        return None;
    }
    Some(PathBuf::from(value)).filter(|path| path.exists())
}

fn env_core_path() -> Option<PathBuf> {
    env::var_os("MIHOMO_CORE")
        .map(PathBuf::from)
        .filter(|path| path.exists())
}

fn is_core_source_alias(value: &str) -> bool {
    matches!(
        normalize_core_source(value).as_str(),
        CORE_SOURCE_AUTO | CORE_SOURCE_RELEASE | CORE_SOURCE_ALPHA | CORE_SOURCE_CUSTOM
    )
}

fn normalize_core_source(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

async fn ensure_managed_core(
    paths: &Paths,
    core: ManagedCore,
    force_update: bool,
) -> Result<ManagedCoreInstall> {
    paths.ensure().await?;
    let path = managed_core_path(paths, core);

    if !force_update && is_usable_file(&path).await? {
        return Ok(ManagedCoreInstall {
            source: core.source(),
            version: read_managed_core_metadata(paths, core)
                .await?
                .map(|metadata| metadata.version)
                .unwrap_or_else(|| "unknown".into()),
            path,
            updated: false,
        });
    }

    let version = fetch_text(core.version_url()).await.with_context(|| {
        format!(
            "failed to fetch mihomo {} version",
            core.source().label().to_ascii_lowercase()
        )
    })?;
    let version = version.trim().to_string();
    if version.is_empty() {
        anyhow::bail!("mihomo version response is empty");
    }

    if force_update
        && is_usable_file(&path).await?
        && read_managed_core_metadata(paths, core)
            .await?
            .is_some_and(|metadata| metadata.version == version)
    {
        return Ok(ManagedCoreInstall {
            source: core.source(),
            version,
            path,
            updated: false,
        });
    }

    let asset = managed_core_asset(core)
        .with_context(|| format!("unsupported mihomo download target {}", env::consts::ARCH))?;
    let archive_ext = managed_core_archive_ext()?;
    let archive_name = format!("{asset}-{version}.{archive_ext}");
    let download_url = match core {
        ManagedCore::Release => format!("{}/{version}/{archive_name}", core.url_prefix()),
        ManagedCore::Alpha => format!("{}/{}", core.url_prefix(), archive_name),
    };

    let archive = fetch_bytes(&download_url).await.with_context(|| {
        format!(
            "failed to download mihomo core {} from {download_url}",
            core.source().label()
        )
    })?;
    let binary = unpack_core_archive(&archive, archive_ext)?;
    let temp_path = path.with_extension("download");
    fs::write(&temp_path, binary)
        .await
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    chmod_executable(&temp_path).await?;
    fs::rename(&temp_path, &path)
        .await
        .with_context(|| format!("failed to install {}", path.display()))?;
    write_managed_core_metadata(
        paths,
        core,
        &ManagedCoreMetadata {
            source: core.source().value().into(),
            version: version.clone(),
            asset: asset.into(),
        },
    )
    .await?;

    Ok(ManagedCoreInstall {
        source: core.source(),
        path,
        version,
        updated: true,
    })
}

fn managed_core_path(paths: &Paths, core: ManagedCore) -> PathBuf {
    paths.cores_dir.join(binary_name(core.binary_name()))
}

fn existing_managed_core_path(paths: &Paths, core: ManagedCore) -> Option<PathBuf> {
    let path = managed_core_path(paths, core);
    path.exists().then_some(path)
}

fn managed_core_metadata_path(paths: &Paths, core: ManagedCore) -> PathBuf {
    paths.cores_dir.join(format!("{}.json", core.binary_name()))
}

async fn read_managed_core_metadata(
    paths: &Paths,
    core: ManagedCore,
) -> Result<Option<ManagedCoreMetadata>> {
    let path = managed_core_metadata_path(paths, core);
    let content = match fs::read_to_string(&path).await {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let metadata = serde_json::from_str(&content)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(metadata))
}

async fn write_managed_core_metadata(
    paths: &Paths,
    core: ManagedCore,
    metadata: &ManagedCoreMetadata,
) -> Result<()> {
    let path = managed_core_metadata_path(paths, core);
    let content = serde_json::to_string_pretty(metadata)?;
    fs::write(&path, content)
        .await
        .with_context(|| format!("failed to write {}", path.display()))
}

async fn fetch_text(url: &str) -> Result<String> {
    let bytes = fetch_bytes(url).await?;
    String::from_utf8(bytes).context("response is not valid UTF-8")
}

async fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let attempts = core_download_attempts();
    let mut errors = Vec::new();
    for attempt in attempts {
        match fetch_bytes_once(url, &attempt).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => errors.push(format!("{}: {err:#}", attempt.label)),
        }
    }
    anyhow::bail!("{}", errors.join("; "))
}

async fn fetch_bytes_once(url: &str, attempt: &DownloadAttempt) -> Result<Vec<u8>> {
    let client = core_download_client(attempt.proxy_url.as_deref())?;
    let response = client.get(url).send().await.context("request failed")?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .context("failed to read response body")?;
    if !status.is_success() {
        anyhow::bail!("status {status}");
    }
    Ok(body.to_vec())
}

fn core_download_client(proxy_url: Option<&str>) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        USER_AGENT,
        HeaderValue::from_str(CORE_DOWNLOAD_USER_AGENT)
            .context("invalid core download user agent")?,
    );

    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(10))
        .tcp_keepalive(Duration::from_secs(60))
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(None)
        .timeout(CORE_DOWNLOAD_TIMEOUT)
        .connect_timeout(CORE_DOWNLOAD_TIMEOUT)
        .default_headers(headers);

    if let Some(proxy_url) = proxy_url {
        builder = builder.proxy(Proxy::all(proxy_url)?);
    } else {
        builder = builder.no_proxy();
    }

    Ok(builder.build()?)
}

fn core_download_attempts() -> Vec<DownloadAttempt> {
    let mut attempts = vec![DownloadAttempt {
        label: "direct",
        proxy_url: None,
    }];

    if let Some(proxy) = local_proxy_from_env() {
        attempts.push(DownloadAttempt {
            label: "environment proxy",
            proxy_url: Some(proxy),
        });
    }

    if let Ok(status) = system_proxy::status()
        && status.enabled
        && !status.server.trim().is_empty()
    {
        attempts.push(DownloadAttempt {
            label: "system proxy",
            proxy_url: Some(format!("http://{}", status.server)),
        });
    }

    attempts
}

fn local_proxy_from_env() -> Option<String> {
    env::var("HTTPS_PROXY")
        .or_else(|_| env::var("https_proxy"))
        .or_else(|_| env::var("HTTP_PROXY"))
        .or_else(|_| env::var("http_proxy"))
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn managed_core_asset(core: ManagedCore) -> Option<&'static str> {
    let platform = env::consts::OS;
    let arch = env::consts::ARCH;
    match (core, platform, arch) {
        (ManagedCore::Release, "macos", "x86_64") => Some("mihomo-darwin-amd64-v2-go122"),
        (ManagedCore::Alpha, "macos", "x86_64") => Some("mihomo-darwin-amd64-v1-go122"),
        (_, "macos", "aarch64") => Some("mihomo-darwin-arm64-go122"),
        (_, "linux", "x86_64") => Some("mihomo-linux-amd64-v2"),
        (_, "linux", "aarch64") => Some("mihomo-linux-arm64"),
        (_, "linux", "arm") => Some("mihomo-linux-armv7"),
        (_, "linux", "riscv64") => Some("mihomo-linux-riscv64"),
        (_, "linux", "loongarch64") => Some("mihomo-linux-loong64"),
        _ => None,
    }
}

fn managed_core_archive_ext() -> Result<&'static str> {
    if cfg!(windows) {
        anyhow::bail!("managed mihomo download is not implemented on Windows yet");
    }
    Ok("gz")
}

fn unpack_core_archive(archive: &[u8], archive_ext: &str) -> Result<Vec<u8>> {
    match archive_ext {
        "gz" => {
            let mut decoder = GzDecoder::new(Cursor::new(archive));
            let mut output = Vec::new();
            decoder
                .read_to_end(&mut output)
                .context("failed to unpack mihomo gz archive")?;
            Ok(output)
        }
        _ => anyhow::bail!("unsupported mihomo archive type: {archive_ext}"),
    }
}

#[cfg(unix)]
async fn chmod_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let permissions = std::fs::Permissions::from_mode(0o755);
    fs::set_permissions(path, permissions)
        .await
        .with_context(|| format!("failed to chmod {}", path.display()))
}

#[cfg(not(unix))]
async fn chmod_executable(_path: &Path) -> Result<()> {
    Ok(())
}

async fn start_instance(instance: &RuntimePaths, core_path: &Path) -> Result<()> {
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

async fn wait_for_exit(pid: u32) -> bool {
    for _ in 0..STOP_RETRIES {
        if !is_process_running(pid) {
            return true;
        }
        sleep(STOP_WAIT).await;
    }
    !is_process_running(pid)
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
