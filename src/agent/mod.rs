pub mod knowledge;
pub mod patch;

use std::env;
use std::sync::mpsc::Sender;

use anyhow::{Context as _, Result};
use reqwest::Client;
use serde_json::{Value, json};
use tokio::fs;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

use crate::config::{AppConfig, Paths};
use crate::i18n::Language;
use crate::llm::{LlmClient, LlmMessage, LlmToolCall, LlmToolSpec, LlmToolSpecFunction};
use crate::llm_providers;
use crate::mihomo::MihomoClient;

pub use patch::{ConfigPatch, apply_config_patch};

const MAX_AGENT_TURNS: usize = 6;
const MAX_TOOL_OUTPUT: usize = 12_000;
const MAX_COMMAND_OUTPUT: usize = 8_000;
const MAX_COMMAND_ARGS: usize = 24;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Content(String),
    Tool(String),
    PatchReady(ConfigPatch),
    Done,
    Error(String),
}

pub async fn run_agent(
    paths: Paths,
    config: AppConfig,
    user_message: String,
    language: Language,
    sender: Sender<AgentEvent>,
) {
    if let Err(err) = run_agent_inner(paths, config, user_message, language, &sender).await {
        let _ = sender.send(AgentEvent::Error(err.to_string()));
    }
    let _ = sender.send(AgentEvent::Done);
}

async fn run_agent_inner(
    paths: Paths,
    config: AppConfig,
    user_message: String,
    language: Language,
    sender: &Sender<AgentEvent>,
) -> Result<()> {
    let api_key = resolve_api_key(&paths, &config).await?;
    if config.llm.model.trim().is_empty() {
        anyhow::bail!("LLM model is not configured");
    }

    let client = LlmClient::new(&config.llm.base_url, api_key);
    let mut messages = vec![
        LlmMessage::system(system_prompt(&user_message, language)),
        LlmMessage::user(format!(
            "Runtime snapshot:\n{}\n\nUser request:\n{}",
            runtime_snapshot(&paths, &config),
            user_message
        )),
    ];
    let tools = tool_specs();

    for _ in 0..MAX_AGENT_TURNS {
        let completion = client
            .stream_chat_completion(&config.llm.model, &messages, &tools, |part| {
                let _ = sender.send(AgentEvent::Content(part));
            })
            .await?;

        if completion.tool_calls.is_empty() {
            if completion.content.trim().is_empty() {
                let _ = sender.send(AgentEvent::Content("Done.".into()));
            }
            return Ok(());
        }

        let assistant = LlmMessage::assistant_tool_calls(
            completion.content.clone(),
            completion.tool_calls.clone(),
        );
        messages.push(assistant);

        for call in completion.tool_calls {
            let _ = sender.send(AgentEvent::Tool(format!("running {}", call.function.name)));
            let result = execute_tool(&paths, &config, &call, sender)
                .await
                .unwrap_or_else(|err| json!({ "ok": false, "error": err.to_string() }));
            messages.push(LlmMessage::tool(
                call.id,
                truncate_tool_output(&serde_json::to_string_pretty(&result)?),
            ));
        }
    }

    anyhow::bail!("agent stopped after too many tool turns")
}

pub async fn resolve_api_key(paths: &Paths, config: &AppConfig) -> Result<String> {
    if let Some(value) = llm_providers::api_key_for(&paths.llm_providers_file, &config.llm.provider)
    {
        return Ok(value);
    }

    let env_name = config.llm.api_key_env.trim();
    if !env_name.is_empty()
        && let Ok(value) = env::var(env_name)
        && !value.trim().is_empty()
    {
        return Ok(value);
    }

    let path = config
        .llm
        .api_key_file
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| paths.llm_api_key_file.clone());
    let value = fs::read_to_string(&path).await.with_context(|| {
        format!(
            "LLM API key is missing; paste it in Runtime > LLM API Key, set {env_name}, or {}",
            path.display()
        )
    })?;
    let value = value.trim().to_string();
    if value.is_empty() {
        anyhow::bail!("LLM API key file is empty: {}", path.display());
    }
    Ok(value)
}

pub async fn save_api_key(paths: &Paths, config: &AppConfig, value: &str) -> Result<()> {
    paths.ensure().await?;
    llm_providers::save_api_key(&paths.llm_providers_file, &config.llm.provider, value)?;
    llm_providers::reload_from_file(&paths.llm_providers_file)?;
    Ok(())
}

pub fn api_key_status(paths: &Paths, config: &AppConfig) -> String {
    if llm_providers::provider_has_api_key(&paths.llm_providers_file, &config.llm.provider) {
        return "providers.yaml".into();
    }

    let env_name = config.llm.api_key_env.trim();
    if !env_name.is_empty()
        && env::var(env_name)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    {
        return format!("env:{env_name}");
    }
    let path = config
        .llm
        .api_key_file
        .as_deref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| paths.llm_api_key_file.clone());
    if path.exists() {
        "secret-file".into()
    } else {
        "missing".into()
    }
}

fn system_prompt(user_message: &str, language: Language) -> String {
    let docs = knowledge::select_docs(user_message);
    format!(
        r"You are the native clashtui configuration assistant.

Rules:
- {}
- Help the user operate clashtui and understand mihomo behavior.
- Use tools for current facts instead of guessing.
- clashtui does not proxy traffic itself; mihomo does.
- Do not edit generated runtime files.
- Use run_command only for bounded read-only diagnostics.
- Do not claim a config change is applied unless the user applies a draft patch.
- Config changes must be proposed with propose_config_patch.
- Save and restart are user-controlled.
- Do not expose API keys, controller secrets, or unnecessary subscription URLs.

Bundled domain knowledge:

{}",
        language.assistant_rule(),
        knowledge::render_docs(&docs)
    )
}

fn runtime_snapshot(paths: &Paths, config: &AppConfig) -> String {
    let service = crate::service::status().ok();
    let port_proxies = config
        .proxy_ports
        .services
        .iter()
        .map(|service| {
            json!({
                "name": service.name,
                "enabled": service.enabled,
                "kind": service.kind,
                "listen": service.listen,
                "port": service.port,
                "subscription": service.subscription,
                "mode": service.mode,
                "proxy": service.proxy,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "config_file": paths.config_file.display().to_string(),
        "runtime_backend": config.runtime_backend,
        "controller_url": config.controller.url,
        "mixed": format!("{}:{}", config.proxy_host, config.mixed_port),
        "system_proxy_enabled": config.system_proxy.enabled,
        "tun_enabled": config.tun.enable,
        "dns": {
            "enabled": config.dns.enable,
            "listen": config.dns.listen,
            "nameserver_policy_count": config.dns.nameserver_policy.len(),
        },
        "subscriptions": {
            "active": config.active_profile,
            "count": config.subscriptions.len(),
        },
        "service": service.map(|status| json!({
            "installed": status.installed,
            "reachable": status.reachable,
            "core_running": status.core_running,
            "core_pid": status.core_pid,
        })),
        "port_proxies": port_proxies,
    })
    .to_string()
}

fn tool_specs() -> Vec<LlmToolSpec> {
    vec![
        tool_spec(
            "read_config",
            "Read the current clashtui draft config as YAML.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        tool_spec(
            "read_runtime_files",
            "Read generated mihomo runtime files for inspection.",
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["run", "active", "both"] }
                },
                "required": ["kind"],
                "additionalProperties": false
            }),
        ),
        tool_spec(
            "read_log_tail",
            "Read a bounded tail of clashtui or mihomo logs.",
            json!({
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["clashtui", "mihomo"] },
                    "lines": { "type": "integer", "minimum": 1, "maximum": 200 }
                },
                "required": ["kind"],
                "additionalProperties": false
            }),
        ),
        tool_spec(
            "get_mihomo_state",
            "Read mihomo controller status and proxy group summary.",
            json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        ),
        tool_spec(
            "http_probe",
            "Probe an HTTP URL directly or through an HTTP/SOCKS proxy.",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" },
                    "proxy_url": { "type": ["string", "null"] },
                    "method": { "type": "string", "enum": ["GET", "HEAD"] },
                    "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 15000 }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        ),
        tool_spec(
            "run_command",
            "Run a bounded read-only diagnostic command without shell expansion.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "maxItems": MAX_COMMAND_ARGS
                    },
                    "timeout_ms": { "type": "integer", "minimum": 1000, "maximum": 15000 }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        tool_spec(
            "propose_config_patch",
            "Validate and queue a structured patch for the user to apply to the TUI draft.",
            json!({
                "type": "object",
                "properties": {
                    "summary": { "type": "string" },
                    "restart_required": { "type": "boolean" },
                    "operations": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": { "type": "string", "enum": ["set", "append", "remove"] },
                                "path": { "type": "string" },
                                "value": {}
                            },
                            "required": ["op", "path"]
                        }
                    }
                },
                "required": ["summary", "restart_required", "operations"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn tool_spec(name: &'static str, description: &'static str, parameters: Value) -> LlmToolSpec {
    LlmToolSpec {
        kind: "function",
        function: LlmToolSpecFunction {
            name,
            description,
            parameters,
        },
    }
}

async fn execute_tool(
    paths: &Paths,
    config: &AppConfig,
    call: &LlmToolCall,
    sender: &Sender<AgentEvent>,
) -> Result<Value> {
    let args: Value = if call.function.arguments.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&call.function.arguments)
            .with_context(|| format!("invalid arguments for {}", call.function.name))?
    };
    match call.function.name.as_str() {
        "read_config" => read_config_tool(config),
        "read_runtime_files" => read_runtime_files_tool(paths, &args).await,
        "read_log_tail" => read_log_tail_tool(paths, &args).await,
        "get_mihomo_state" => get_mihomo_state_tool(config).await,
        "http_probe" => http_probe_tool(&args).await,
        "run_command" => run_command_tool(&args).await,
        "propose_config_patch" => propose_config_patch_tool(config, args, sender),
        other => Ok(json!({ "ok": false, "error": format!("unknown tool: {other}") })),
    }
}

fn read_config_tool(config: &AppConfig) -> Result<Value> {
    let mut redacted = config.clone();
    redacted.controller.secret = redacted
        .controller
        .secret
        .as_ref()
        .map(|_| "<redacted>".into());
    let yaml = serde_yaml_ng::to_string(&redacted)?;
    Ok(json!({ "ok": true, "config_yaml": truncate_tool_output(&yaml) }))
}

async fn read_runtime_files_tool(paths: &Paths, args: &Value) -> Result<Value> {
    let kind = args.get("kind").and_then(Value::as_str).unwrap_or("both");
    let mut result = serde_json::Map::new();
    if matches!(kind, "run" | "both") {
        result.insert(
            "mihomo_run".into(),
            json!(read_optional_file(&paths.core_config_file).await),
        );
    }
    if matches!(kind, "active" | "both") {
        result.insert(
            "mihomo_active".into(),
            json!(read_optional_file(&paths.active_config_file).await),
        );
    }
    Ok(json!({ "ok": true, "files": result }))
}

async fn read_log_tail_tool(paths: &Paths, args: &Value) -> Result<Value> {
    let kind = args
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("clashtui");
    let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(80) as usize;
    let path = if kind == "mihomo" {
        &paths.core_log_file
    } else {
        &paths.log_file
    };
    let content = read_optional_file(path).await;
    Ok(json!({
        "ok": true,
        "path": path.display().to_string(),
        "tail": tail_lines(&content, lines.min(200)),
    }))
}

async fn get_mihomo_state_tool(config: &AppConfig) -> Result<Value> {
    let client = MihomoClient::new(&config.controller);
    let version = client.version().await.map_err(|err| err.to_string());
    let configs = client.configs().await.map_err(|err| err.to_string());
    let groups = client.proxy_groups().await.map_err(|err| err.to_string());
    let group_summary = groups.as_ref().ok().map(|groups| {
        groups
            .iter()
            .take(80)
            .map(|group| {
                json!({
                    "name": group.name,
                    "kind": group.kind,
                    "now": group.now,
                    "count": group.all.len(),
                })
            })
            .collect::<Vec<_>>()
    });
    Ok(json!({
        "ok": version.is_ok(),
        "version": result_value(version),
        "configs": result_value(configs),
        "proxy_groups": group_summary,
        "proxy_group_error": groups.err(),
    }))
}

async fn http_probe_tool(args: &Value) -> Result<Value> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .context("http_probe requires url")?;
    let method = args.get("method").and_then(Value::as_str).unwrap_or("HEAD");
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(8_000)
        .clamp(1_000, 15_000);
    let mut builder = Client::builder().timeout(Duration::from_millis(timeout_ms));
    if let Some(proxy_url) = args
        .get("proxy_url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
    }
    let client = builder.build()?;
    let request = if method == "GET" {
        client.get(url)
    } else {
        client.head(url)
    };
    let started = std::time::Instant::now();
    match request.send().await {
        Ok(response) => Ok(json!({
            "ok": response.status().is_success(),
            "status": response.status().as_u16(),
            "duration_ms": started.elapsed().as_millis(),
            "url": url,
        })),
        Err(err) => Ok(json!({
            "ok": false,
            "error": err.to_string(),
            "duration_ms": started.elapsed().as_millis(),
            "url": url,
        })),
    }
}

async fn run_command_tool(args: &Value) -> Result<Value> {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .context("run_command requires command")?
        .trim();
    validate_diagnostic_command(command)?;

    let command_args = args
        .get("args")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .take(MAX_COMMAND_ARGS)
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for arg in &command_args {
        validate_command_arg(arg)?;
    }

    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(5_000)
        .clamp(1_000, 15_000);
    let started = std::time::Instant::now();
    let output = timeout(
        Duration::from_millis(timeout_ms),
        Command::new(command)
            .args(&command_args)
            .kill_on_drop(true)
            .output(),
    )
    .await;

    match output {
        Ok(Ok(output)) => Ok(json!({
            "ok": output.status.success(),
            "status": output.status.code(),
            "duration_ms": started.elapsed().as_millis(),
            "stdout": truncate_command_output(&String::from_utf8_lossy(&output.stdout)),
            "stderr": truncate_command_output(&String::from_utf8_lossy(&output.stderr)),
        })),
        Ok(Err(err)) => Ok(json!({
            "ok": false,
            "error": err.to_string(),
            "duration_ms": started.elapsed().as_millis(),
        })),
        Err(_) => Ok(json!({
            "ok": false,
            "error": "command timed out",
            "duration_ms": started.elapsed().as_millis(),
        })),
    }
}

fn validate_diagnostic_command(command: &str) -> Result<()> {
    if command.is_empty()
        || command.contains('/')
        || !command
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        anyhow::bail!("run_command only accepts a program name from the diagnostic allowlist");
    }
    if !DIAGNOSTIC_COMMANDS.contains(&command) {
        anyhow::bail!("run_command command is not allowed: {command}");
    }
    Ok(())
}

fn validate_command_arg(arg: &str) -> Result<()> {
    if arg.len() > 512 || arg.contains('\0') {
        anyhow::bail!("run_command argument is invalid");
    }
    let lower = arg.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "add"
            | "apply"
            | "change"
            | "del"
            | "delete"
            | "down"
            | "flush"
            | "kill"
            | "replace"
            | "restart"
            | "set"
            | "start"
            | "stop"
            | "up"
            | "-i"
            | "--in-place"
            | "-w"
            | "--write"
    ) || lower.starts_with("-set")
        || lower.starts_with("--set")
    {
        anyhow::bail!("run_command only accepts read-only diagnostic arguments");
    }
    Ok(())
}

fn propose_config_patch_tool(
    config: &AppConfig,
    args: Value,
    sender: &Sender<AgentEvent>,
) -> Result<Value> {
    let patch: ConfigPatch = serde_json::from_value(args).context("invalid config patch")?;
    let _updated = apply_config_patch(config, &patch)?;
    let _ = sender.send(AgentEvent::PatchReady(patch.clone()));
    Ok(json!({
        "ok": true,
        "queued": true,
        "summary": patch.summary,
        "restart_required": patch.restart_required,
        "message": "Patch is validated and waiting for user confirmation in the Chat page."
    }))
}

async fn read_optional_file(path: &std::path::Path) -> String {
    match fs::read_to_string(path).await {
        Ok(content) => truncate_tool_output(&content),
        Err(err) => format!("unavailable: {err}"),
    }
}

fn tail_lines(value: &str, lines: usize) -> String {
    let mut selected = value.lines().rev().take(lines).collect::<Vec<_>>();
    selected.reverse();
    truncate_tool_output(&selected.join("\n"))
}

fn result_value<T: serde::Serialize>(result: std::result::Result<T, String>) -> Value {
    match result {
        Ok(value) => json!({ "ok": true, "value": value }),
        Err(err) => json!({ "ok": false, "error": err }),
    }
}

fn truncate_tool_output(value: &str) -> String {
    if value.chars().count() <= MAX_TOOL_OUTPUT {
        return value.to_string();
    }
    let mut output = value.chars().take(MAX_TOOL_OUTPUT).collect::<String>();
    output.push_str("\n...[truncated]");
    output
}

fn truncate_command_output(value: &str) -> String {
    if value.chars().count() <= MAX_COMMAND_OUTPUT {
        return value.to_string();
    }
    let mut output = value.chars().take(MAX_COMMAND_OUTPUT).collect::<String>();
    output.push_str("\n...[truncated]");
    output
}

const DIAGNOSTIC_COMMANDS: &[&str] = &[
    "arp",
    "date",
    "dig",
    "dmesg",
    "host",
    "id",
    "ifconfig",
    "ip",
    "ipconfig",
    "lsof",
    "netsh",
    "netstat",
    "networksetup",
    "nslookup",
    "pgrep",
    "ping",
    "ps",
    "resolvectl",
    "route",
    "scutil",
    "ss",
    "sw_vers",
    "systemd-resolve",
    "traceroute",
    "uname",
    "where",
    "which",
    "whoami",
];
