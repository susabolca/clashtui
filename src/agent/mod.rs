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
use crate::llm::{
    LlmClient, LlmCompletion, LlmMessage, LlmToolCall, LlmToolSpec, LlmToolSpecFunction,
};
use crate::llm_providers;
use crate::mihomo::MihomoClient;

pub use patch::{ConfigPatch, apply_config_patch};

pub const MAX_CONTEXT_TOKENS: usize = 200_000;
pub const MAX_CONTEXT_MESSAGES: usize = 200;
pub const MAX_CONTEXT_CHARS: usize = MAX_CONTEXT_TOKENS * 4;
pub const MAX_CONTEXT_MESSAGE_CHARS: usize = MAX_CONTEXT_CHARS;
pub const MAX_AGENT_TURNS: usize = 24;
pub const MAX_TOOL_CALLS: usize = 96;
const MAX_TOOL_OUTPUT: usize = 12_000;
const MAX_COMMAND_OUTPUT: usize = 8_000;
const MAX_COMMAND_ARGS: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: String,
}

impl ConversationMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ConversationRole::Assistant,
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Content(String),
    Tool(String),
    Usage(AgentUsage),
    PatchReady(ConfigPatch),
    Done,
    Error(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub context_tokens: usize,
    pub context_chars: usize,
    pub context_messages: usize,
    pub turns: usize,
    pub tool_calls: usize,
    pub estimated: bool,
}

pub async fn run_agent(
    paths: Paths,
    config: AppConfig,
    conversation: Vec<ConversationMessage>,
    language: Language,
    sender: Sender<AgentEvent>,
) {
    if let Err(err) = run_agent_inner(paths, config, conversation, language, &sender).await {
        let _ = sender.send(AgentEvent::Error(err.to_string()));
    }
    let _ = sender.send(AgentEvent::Done);
}

async fn run_agent_inner(
    paths: Paths,
    config: AppConfig,
    conversation: Vec<ConversationMessage>,
    language: Language,
    sender: &Sender<AgentEvent>,
) -> Result<()> {
    let api_key = resolve_api_key(&paths, &config).await?;
    if config.llm.model.trim().is_empty() {
        anyhow::bail!("LLM model is not configured");
    }
    let client = LlmClient::new(&config.llm.base_url, api_key);
    let mut messages = build_llm_messages(&paths, &config, &conversation, language);
    let tools = tool_specs();
    let mut tool_calls_used = 0usize;
    let mut usage = AgentUsage::default();

    for turn in 0..MAX_AGENT_TURNS {
        usage.turns = turn + 1;
        usage.context_chars = estimate_context_chars(&messages, &tools);
        usage.context_tokens = estimate_tokens_from_chars(usage.context_chars);
        usage.context_messages = messages.len();
        let prompt_estimate = usage.context_tokens;
        let _ = sender.send(AgentEvent::Usage(usage.clone()));

        let completion = client
            .stream_chat_completion(&config.llm.model, &messages, &tools, |part| {
                let _ = sender.send(AgentEvent::Content(part));
            })
            .await?;
        let completion_usage = completion_token_usage(&completion, prompt_estimate);
        usage.prompt_tokens = usage
            .prompt_tokens
            .saturating_add(completion_usage.prompt_tokens);
        usage.completion_tokens = usage
            .completion_tokens
            .saturating_add(completion_usage.completion_tokens);
        usage.estimated |= completion_usage.estimated;
        let _ = sender.send(AgentEvent::Usage(usage.clone()));

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
            tool_calls_used += 1;
            usage.tool_calls = tool_calls_used;
            let _ = sender.send(AgentEvent::Usage(usage.clone()));
            if tool_calls_used > MAX_TOOL_CALLS {
                anyhow::bail!("agent stopped after too many tool calls");
            }
            let result = match execute_tool(&paths, &config, &call).await {
                Ok(result) => {
                    let _ = sender.send(AgentEvent::Tool(format_tool_display(&call, &result)));
                    result
                }
                Err(err) => {
                    let message = err.to_string();
                    let result = json!({ "ok": false, "error": message });
                    let _ = sender.send(AgentEvent::Tool(format_tool_display(&call, &result)));
                    result
                }
            };
            let patch_sent = call.function.name == "propose_config_patch"
                && result.get("ok").and_then(Value::as_bool) == Some(true)
                && result.get("patch_sent").and_then(Value::as_bool) == Some(true);
            let patch_ready = if patch_sent {
                Some(
                    serde_json::from_str::<ConfigPatch>(&call.function.arguments)
                        .context("invalid config patch returned from tool call")?,
                )
            } else {
                None
            };
            messages.push(LlmMessage::tool(
                call.id,
                truncate_tool_output(&serde_json::to_string_pretty(&result)?),
            ));
            if let Some(patch) = patch_ready {
                let _ = sender.send(AgentEvent::PatchReady(patch));
                return Ok(());
            }
        }
    }

    anyhow::bail!("agent stopped after too many tool turns")
}

fn build_llm_messages(
    paths: &Paths,
    config: &AppConfig,
    conversation: &[ConversationMessage],
    language: Language,
) -> Vec<LlmMessage> {
    let latest_user = latest_user_message(conversation).unwrap_or_default();
    let mut messages = vec![
        LlmMessage::system(system_prompt(&latest_user, language)),
        LlmMessage::user(format!(
            "Current runtime snapshot:\n{}\n\nConversation follows. Use the latest user message as the active request.",
            runtime_snapshot(paths, config)
        )),
    ];
    messages.extend(trim_conversation(conversation).into_iter().map(
        |message| match message.role {
            ConversationRole::User => LlmMessage::user(message.content),
            ConversationRole::Assistant => LlmMessage::assistant(message.content),
        },
    ));
    messages
}

fn latest_user_message(conversation: &[ConversationMessage]) -> Option<String> {
    conversation
        .iter()
        .rev()
        .find(|message| message.role == ConversationRole::User)
        .map(|message| message.content.clone())
}

fn trim_conversation(conversation: &[ConversationMessage]) -> Vec<ConversationMessage> {
    let mut total_chars = 0usize;
    let mut selected = Vec::new();
    for message in conversation.iter().rev() {
        let content = trim_context_message(&message.content);
        if content.trim().is_empty() {
            continue;
        }
        let chars = content.chars().count();
        if selected.len() >= MAX_CONTEXT_MESSAGES {
            break;
        }
        if !selected.is_empty() && total_chars.saturating_add(chars) > MAX_CONTEXT_CHARS {
            break;
        }
        total_chars = total_chars.saturating_add(chars);
        selected.push(ConversationMessage {
            role: message.role,
            content,
        });
    }
    selected.reverse();
    selected
}

fn trim_context_message(value: &str) -> String {
    if value.chars().count() <= MAX_CONTEXT_MESSAGE_CHARS {
        return value.to_string();
    }
    let keep = MAX_CONTEXT_MESSAGE_CHARS.saturating_sub(16);
    let mut output = value.chars().take(keep).collect::<String>();
    output.push_str("\n...[truncated]");
    output
}

fn estimate_context_chars(messages: &[LlmMessage], tools: &[LlmToolSpec]) -> usize {
    let message_chars = messages.iter().map(estimate_message_chars).sum::<usize>();
    let tools_chars = serde_json::to_string(tools)
        .map(|value| value.chars().count())
        .unwrap_or_default();
    message_chars.saturating_add(tools_chars)
}

fn estimate_message_chars(message: &LlmMessage) -> usize {
    let mut count = message.role.chars().count().saturating_add(4);
    if let Some(content) = &message.content {
        count = count.saturating_add(content.chars().count());
    }
    if let Some(tool_call_id) = &message.tool_call_id {
        count = count.saturating_add(tool_call_id.chars().count());
    }
    if let Some(tool_calls) = &message.tool_calls {
        count = count.saturating_add(
            serde_json::to_string(tool_calls)
                .map(|value| value.chars().count())
                .unwrap_or_default(),
        );
    }
    count
}

#[derive(Debug, Clone, Copy)]
struct CompletionTokenUsage {
    prompt_tokens: usize,
    completion_tokens: usize,
    estimated: bool,
}

fn completion_token_usage(
    completion: &LlmCompletion,
    prompt_estimate: usize,
) -> CompletionTokenUsage {
    if let Some(usage) = completion.usage {
        return CompletionTokenUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            estimated: false,
        };
    }
    CompletionTokenUsage {
        prompt_tokens: prompt_estimate,
        completion_tokens: estimate_completion_tokens(completion),
        estimated: true,
    }
}

fn estimate_completion_tokens(completion: &LlmCompletion) -> usize {
    let tool_call_chars = serde_json::to_string(&completion.tool_calls)
        .map(|value| value.chars().count())
        .unwrap_or_default();
    estimate_tokens_from_chars(
        completion
            .content
            .chars()
            .count()
            .saturating_add(tool_call_chars),
    )
}

fn estimate_tokens_from_chars(chars: usize) -> usize {
    chars.saturating_add(3) / 4
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
- Do not claim a config change is saved or active in mihomo until the user saves/restarts.
- Config changes must be proposed with propose_config_patch.
- Tool output requirements for propose_config_patch:
  - Do not write YAML, unified diff text, or shell commands.
  - Call the tool with JSON arguments containing summary, restart_required, and a non-empty operations array.
  - Each operation must contain op and path; set/append operations must also contain value.
- For array entries, prefer selector patch paths after reading config, for example settings.items[name=primary].enabled.
- If propose_config_patch fails, read the error and retry with JSON Pointer, dotted index, or selector path syntax.
- The TUI applies validated chat patches to the draft config automatically. Save and restart are user-controlled.
- After a runtime-affecting patch is applied to draft, remind the user to press F10 to save and restart the service.
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
            "Validate and send a structured clashtui config patch to the TUI draft. Use this tool only for config edits. Do not generate YAML or unified diff text. The arguments must be a JSON object with a short summary, restart_required, and a non-empty operations array. Paths may be JSON Pointer, dotted/bracket indexes, or array selectors. Prefer selector paths for arrays when a stable field is available. The host applies a validated patch to draft automatically; after a runtime-affecting patch, remind the user to press F10 to save and restart the service.",
            json!({
                "type": "object",
                "properties": {
                    "summary": {
                        "type": "string",
                        "description": "Short human-readable description of the intended config change."
                    },
                    "restart_required": {
                        "type": "boolean",
                        "description": "Set true when the change affects mihomo/runtime behavior and needs F10 Save & Restart to take effect; set false only for changes that do not require a runtime restart."
                    },
                    "operations": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Ordered config patch operations. Use set to replace/create a field, append to add one array item, and remove to delete a field or array item.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "op": {
                                    "type": "string",
                                    "enum": ["set", "append", "remove"],
                                    "description": "set replaces or creates the target field, append adds value to the target array, remove deletes the target field or array item."
                                },
                                "path": {
                                    "type": "string",
                                    "description": "Config path, e.g. /section/items/0/enabled, section.items[0].enabled, or section.items[name=primary].enabled"
                                },
                                "value": {
                                    "description": "Required for set and append. Must be valid JSON matching the target config field type. Omit for remove."
                                }
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

async fn execute_tool(paths: &Paths, config: &AppConfig, call: &LlmToolCall) -> Result<Value> {
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
        "propose_config_patch" => propose_config_patch_tool(config, args),
        other => Ok(json!({ "ok": false, "error": format!("unknown tool: {other}") })),
    }
}

fn format_tool_display(call: &LlmToolCall, result: &Value) -> String {
    let args = parse_tool_arguments(call).unwrap_or_else(|| json!({}));
    let command = tool_command_display(call.function.name.as_str(), &args);
    let output = tool_output_display(call.function.name.as_str(), result);
    if output.trim().is_empty() {
        format!("Ran {command}")
    } else {
        format!("Ran {command}\n{output}")
    }
}

fn parse_tool_arguments(call: &LlmToolCall) -> Option<Value> {
    if call.function.arguments.trim().is_empty() {
        Some(json!({}))
    } else {
        serde_json::from_str(&call.function.arguments).ok()
    }
}

fn tool_command_display(name: &str, args: &Value) -> String {
    match name {
        "run_command" => {
            let command = args.get("command").and_then(Value::as_str).unwrap_or(name);
            let command_args = args
                .get("args")
                .and_then(Value::as_array)
                .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            shell_command_display(command, &command_args)
        }
        "read_log_tail" => {
            let kind = args
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("clashtui");
            let lines = args.get("lines").and_then(Value::as_u64).unwrap_or(80);
            format!("read_log_tail --kind {} --lines {lines}", shell_quote(kind))
        }
        "read_runtime_files" => {
            let kind = args.get("kind").and_then(Value::as_str).unwrap_or("both");
            format!("read_runtime_files --kind {}", shell_quote(kind))
        }
        "http_probe" => {
            let method = args.get("method").and_then(Value::as_str).unwrap_or("HEAD");
            let url = args.get("url").and_then(Value::as_str).unwrap_or("-");
            format!("http_probe {method} {}", shell_quote(url))
        }
        _ => name.to_string(),
    }
}

fn tool_output_display(name: &str, result: &Value) -> String {
    if let Some(error) = result.get("error").and_then(Value::as_str) {
        return format!("error: {error}");
    }

    match name {
        "run_command" => {
            let stdout = result.get("stdout").and_then(Value::as_str).unwrap_or("");
            let stderr = result.get("stderr").and_then(Value::as_str).unwrap_or("");
            match (stdout.trim().is_empty(), stderr.trim().is_empty()) {
                (false, false) => format!("{stdout}\n{stderr}"),
                (false, true) => stdout.to_string(),
                (true, false) => stderr.to_string(),
                (true, true) => {
                    let status = result
                        .get("status")
                        .and_then(Value::as_i64)
                        .map_or_else(|| "-".into(), |status| status.to_string());
                    format!("status={status}")
                }
            }
        }
        "read_log_tail" => result
            .get("tail")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        "http_probe" => {
            let status = result
                .get("status")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".into(), |status| status.to_string());
            let duration = result
                .get("duration_ms")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".into(), |duration| duration.to_string());
            format!("status={status} duration_ms={duration}")
        }
        _ => serde_json::to_string_pretty(result).unwrap_or_else(|_| result.to_string()),
    }
}

fn shell_command_display(command: &str, args: &[&str]) -> String {
    std::iter::once(shell_quote(command))
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/' | ':'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn read_config_tool(config: &AppConfig) -> Result<Value> {
    let mut redacted = config.clone();
    redacted.controller.secret = redacted
        .controller
        .secret
        .as_ref()
        .map(|_| "<redacted>".into());
    let yaml = serde_yaml_ng::to_string(&redacted)?;
    Ok(json!({
        "ok": true,
        "config_yaml": truncate_tool_output(&yaml),
        "patch_path_help": {
            "how_to_use": [
                "Inspect config_yaml first and choose the exact field to change.",
                "Build a structured JSON patch; do not output YAML, diff text, or shell commands.",
                "Call propose_config_patch once with summary, restart_required, and operations; the TUI applies a valid patch to draft automatically.",
                "If the tool reports an error, fix the path or value type and retry."
            ],
            "content_requirements": {
                "summary": "Short human-readable description.",
                "restart_required": "true for mihomo/runtime-affecting changes; false only when no runtime restart is needed.",
                "operations": "Non-empty array of { op, path, value }. set/append require value; remove omits value."
            },
            "syntax": [
                "JSON Pointer: /section/items/0/enabled",
                "Dotted bracket index: section.items[0].enabled",
                "Dotted numeric segment: section.items.0.enabled",
                "Array selector: section.items[name=primary].enabled"
            ],
            "notes": [
                "Use selector paths for arrays when a stable field such as name or port is present.",
                "The TUI applies a valid patch to draft automatically.",
                "For runtime-affecting changes, remind the user to press F10 to save and restart the service."
            ]
        }
    }))
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
    validate_command_args(command, &command_args)?;

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

fn validate_command_args(command: &str, args: &[String]) -> Result<()> {
    for arg in args {
        validate_command_arg(arg)?;
    }

    match command {
        "curl" => validate_curl_args(args),
        "wget" => validate_wget_args(args),
        _ => Ok(()),
    }
}

fn validate_command_arg(arg: &str) -> Result<()> {
    if arg.len() > 512 || arg.contains('\0') {
        anyhow::bail!("run_command argument is invalid");
    }
    let lower = arg.to_ascii_lowercase();
    let normalized = if arg.starts_with('-') && !arg.starts_with("--") {
        arg
    } else {
        lower.as_str()
    };
    if matches!(
        normalized,
        "add"
            | "apply"
            | "change"
            | "commit"
            | "del"
            | "delete"
            | "down"
            | "flush"
            | "kill"
            | "modify"
            | "replace"
            | "reset"
            | "restart"
            | "restore"
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

fn validate_curl_args(args: &[String]) -> Result<()> {
    for arg in args {
        let lower = arg.to_ascii_lowercase();
        if matches!(
            arg.as_str(),
            "-o" | "--output"
                | "-O"
                | "--remote-name"
                | "-J"
                | "--remote-header-name"
                | "-T"
                | "--upload-file"
                | "-d"
                | "--data"
                | "--data-ascii"
                | "--data-binary"
                | "--data-raw"
                | "--data-urlencode"
                | "-F"
                | "--form"
                | "--form-string"
                | "-X"
                | "--request"
                | "-K"
                | "--config"
                | "-c"
                | "--cookie-jar"
                | "-D"
                | "--dump-header"
                | "--output-dir"
                | "--create-dirs"
        ) || lower.starts_with("--output=")
            || lower.starts_with("--upload-file=")
            || lower.starts_with("--data")
            || lower.starts_with("--form")
            || lower.starts_with("--request=")
            || lower.starts_with("--cookie-jar=")
            || lower.starts_with("--dump-header=")
            || lower.starts_with("--output-dir=")
        {
            anyhow::bail!("curl is limited to read-only stdout diagnostics");
        }
    }
    Ok(())
}

fn validate_wget_args(args: &[String]) -> Result<()> {
    let mut read_only_mode = false;
    for (index, arg) in args.iter().enumerate() {
        let lower = arg.to_ascii_lowercase();
        if arg == "--spider" {
            read_only_mode = true;
        }
        if arg == "-O-" || (arg.starts_with('-') && arg.ends_with("O-")) {
            read_only_mode = true;
        }
        if arg == "-O" {
            read_only_mode |= args
                .get(index + 1)
                .is_some_and(|value| value.as_str() == "-");
        }
        if lower == "--output-document=-" {
            read_only_mode = true;
        }
        if matches!(
            arg.as_str(),
            "-O" | "--output-document"
                | "-a"
                | "--append-output"
                | "-o"
                | "--output-file"
                | "-m"
                | "--mirror"
                | "-r"
                | "--recursive"
                | "--post-data"
                | "--post-file"
                | "--method"
                | "--body-data"
                | "--body-file"
        ) || lower.starts_with("--output-document=")
            || lower.starts_with("--append-output=")
            || lower.starts_with("--output-file=")
            || lower.starts_with("--post-")
            || lower.starts_with("--method=")
            || lower.starts_with("--body-")
        {
            let stdout_output = (arg == "-O"
                && args
                    .get(index + 1)
                    .is_some_and(|value| value.as_str() == "-"))
                || lower == "--output-document=-";
            if !stdout_output {
                anyhow::bail!("wget is limited to --spider or stdout diagnostics");
            }
        }
    }

    if !read_only_mode {
        anyhow::bail!("wget requires --spider or stdout output (-O -)");
    }
    Ok(())
}

fn propose_config_patch_tool(config: &AppConfig, args: Value) -> Result<Value> {
    let patch: ConfigPatch = serde_json::from_value(args).context("invalid config patch")?;
    let _updated = apply_config_patch(config, &patch)?;
    Ok(json!({
        "ok": true,
        "patch_sent": true,
        "draft_apply": "automatic",
        "summary": patch.summary,
        "restart_required": patch.restart_required,
        "message": "Patch is validated and sent to the Chat page. The TUI applies it to draft automatically; if restart_required is true, remind the user to press F10 to save and restart the service."
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
    "cat",
    "curl",
    "date",
    "df",
    "dig",
    "dmesg",
    "drill",
    "du",
    "env",
    "file",
    "free",
    "grep",
    "head",
    "host",
    "hostname",
    "id",
    "ifconfig",
    "ip",
    "ipconfig",
    "iostat",
    "iw",
    "iwconfig",
    "iwgetid",
    "journalctl",
    "log",
    "ls",
    "lsof",
    "mtr",
    "netsh",
    "netstat",
    "networksetup",
    "networkctl",
    "nmcli",
    "nslookup",
    "pathping",
    "pgrep",
    "ping",
    "ping6",
    "printenv",
    "ps",
    "pwd",
    "resolvectl",
    "route",
    "rg",
    "sar",
    "scutil",
    "ss",
    "stat",
    "sw_vers",
    "sysctl",
    "systemd-resolve",
    "tail",
    "tcpdump",
    "tracepath",
    "tracepath6",
    "traceroute",
    "traceroute6",
    "tshark",
    "uname",
    "uptime",
    "vm_stat",
    "vmstat",
    "w",
    "wc",
    "wget",
    "where",
    "which",
    "who",
    "whois",
    "whoami",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_conversation_to_recent_message_budget() {
        let conversation = (0..(MAX_CONTEXT_MESSAGES + 8))
            .map(|index| {
                if index % 2 == 0 {
                    ConversationMessage::user(format!("user {index}"))
                } else {
                    ConversationMessage::assistant(format!("assistant {index}"))
                }
            })
            .collect::<Vec<_>>();

        let trimmed = trim_conversation(&conversation);

        assert_eq!(trimmed.len(), MAX_CONTEXT_MESSAGES);
        assert_eq!(
            trimmed.first().map(|message| message.content.as_str()),
            Some("user 8")
        );
        assert_eq!(
            trimmed.last().map(|message| message.content.as_str()),
            Some("assistant 207")
        );
    }

    #[test]
    fn trims_large_context_message() {
        let long = "x".repeat(MAX_CONTEXT_MESSAGE_CHARS + 100);
        let trimmed = trim_conversation(&[ConversationMessage::user(long)]);

        assert_eq!(trimmed.len(), 1);
        assert!(trimmed[0].content.ends_with("...[truncated]"));
        assert!(trimmed[0].content.chars().count() <= MAX_CONTEXT_MESSAGE_CHARS);
    }

    #[test]
    fn diagnostic_allowlist_includes_common_network_tools() -> Result<()> {
        for command in [
            "curl",
            "wget",
            "mtr",
            "tracepath",
            "tcpdump",
            "netstat",
            "ip",
        ] {
            validate_diagnostic_command(command)?;
        }
        Ok(())
    }

    #[test]
    fn diagnostic_args_reject_curl_write_and_upload_options() {
        assert!(validate_command_args("curl", &strings(["-I", "https://example.com"])).is_ok());
        assert!(
            validate_command_args("curl", &strings(["-o", "out", "https://example.com"])).is_err()
        );
        assert!(
            validate_command_args("curl", &strings(["-T", "file", "https://example.com"])).is_err()
        );
        assert!(
            validate_command_args("curl", &strings(["--data", "x=1", "https://example.com"]))
                .is_err()
        );
    }

    #[test]
    fn diagnostic_args_restrict_wget_to_no_file_output() {
        assert!(
            validate_command_args("wget", &strings(["--spider", "https://example.com"])).is_ok()
        );
        assert!(
            validate_command_args("wget", &strings(["-O", "-", "https://example.com"])).is_ok()
        );
        assert!(validate_command_args("wget", &strings(["-qO-", "https://example.com"])).is_ok());
        assert!(validate_command_args("wget", &strings(["https://example.com"])).is_err());
        assert!(
            validate_command_args("wget", &strings(["-O", "out", "https://example.com"])).is_err()
        );
    }

    fn strings(values: impl IntoIterator<Item = &'static str>) -> Vec<String> {
        values.into_iter().map(str::to_string).collect()
    }
}
