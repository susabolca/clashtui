use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Duration, timeout};

const LLM_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const LLM_CHUNK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct LlmClient {
    base_url: String,
    api_key: String,
    client: Client,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<LlmToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: LlmToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmToolSpec {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: LlmToolSpecFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct LlmToolSpecFunction {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub struct LlmCompletion {
    pub content: String,
    pub tool_calls: Vec<LlmToolCall>,
    pub usage: Option<LlmUsage>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmUsage {
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    pub total_tokens: usize,
}

#[derive(Debug, Clone, Default)]
struct PendingToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

impl LlmMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn assistant_tool_calls(content: String, tool_calls: Vec<LlmToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: Some(content),
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

impl LlmClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            client: Client::new(),
        }
    }

    pub async fn stream_chat_completion(
        &self,
        model: &str,
        messages: &[LlmMessage],
        tools: &[LlmToolSpec],
        mut on_content: impl FnMut(String),
    ) -> Result<LlmCompletion> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty()
            && let Some(object) = body.as_object_mut()
        {
            object.insert("tools".into(), json!(tools));
        }
        let mut response = timeout(
            LLM_REQUEST_TIMEOUT,
            self.client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&body)
                .send(),
        )
        .await
        .context("LLM request timed out")?
        .context("LLM request failed")?;
        let status = response.status();
        if !status.is_success() {
            let text = timeout(LLM_CHUNK_TIMEOUT, response.text())
                .await
                .context("LLM error body timed out")?
                .unwrap_or_default();
            if stream_options_unsupported(status, &text) {
                if let Some(object) = body.as_object_mut() {
                    object.remove("stream_options");
                }
                response = timeout(
                    LLM_REQUEST_TIMEOUT,
                    self.client
                        .post(&url)
                        .bearer_auth(&self.api_key)
                        .json(&body)
                        .send(),
                )
                .await
                .context("LLM request timed out")?
                .context("LLM request failed")?;
                let status = response.status();
                if !status.is_success() {
                    let text = timeout(LLM_CHUNK_TIMEOUT, response.text())
                        .await
                        .context("LLM error body timed out")?
                        .unwrap_or_default();
                    anyhow::bail!("LLM returned {status}: {}", truncate_for_tool(&text));
                }
            } else {
                anyhow::bail!("LLM returned {status}: {}", truncate_for_tool(&text));
            }
        }

        let mut buffer = String::new();
        let mut content = String::new();
        let mut tool_calls = BTreeMap::<usize, PendingToolCall>::new();
        let mut usage = None;

        loop {
            let Some(chunk) = timeout(LLM_CHUNK_TIMEOUT, response.chunk())
                .await
                .context("LLM stream timed out")?
                .context("LLM stream failed")?
            else {
                break;
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim_end_matches('\r').to_string();
                buffer.drain(..=line_end);
                if apply_stream_line(
                    &line,
                    &mut content,
                    &mut tool_calls,
                    &mut usage,
                    &mut on_content,
                )? {
                    return Ok(LlmCompletion {
                        content,
                        tool_calls: finish_tool_calls(tool_calls),
                        usage,
                    });
                }
            }
        }

        if !buffer.trim().is_empty()
            && apply_stream_line(
                buffer.trim_end_matches('\r'),
                &mut content,
                &mut tool_calls,
                &mut usage,
                &mut on_content,
            )?
        {
            return Ok(LlmCompletion {
                content,
                tool_calls: finish_tool_calls(tool_calls),
                usage,
            });
        }

        Ok(LlmCompletion {
            content,
            tool_calls: finish_tool_calls(tool_calls),
            usage,
        })
    }
}

fn apply_stream_line(
    line: &str,
    content: &mut String,
    tool_calls: &mut BTreeMap<usize, PendingToolCall>,
    usage: &mut Option<LlmUsage>,
    on_content: &mut impl FnMut(String),
) -> Result<bool> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(false);
    };
    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }
    apply_stream_delta(data, content, tool_calls, usage, on_content)?;
    Ok(false)
}

fn apply_stream_delta(
    data: &str,
    content: &mut String,
    tool_calls: &mut BTreeMap<usize, PendingToolCall>,
    usage: &mut Option<LlmUsage>,
    on_content: &mut impl FnMut(String),
) -> Result<()> {
    let value: Value = serde_json::from_str(data).context("failed to parse LLM stream chunk")?;
    if let Some(parsed) = parse_usage(value.get("usage")) {
        *usage = Some(parsed);
    }
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return Ok(());
    };
    for choice in choices {
        let Some(delta) = choice.get("delta") else {
            continue;
        };
        if let Some(part) = delta.get("content").and_then(Value::as_str) {
            content.push_str(part);
            on_content(part.to_string());
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let pending = tool_calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    pending.id = id.to_string();
                }
                if let Some(kind) = call.get("type").and_then(Value::as_str) {
                    pending.kind = kind.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        pending.name = name.to_string();
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        pending.arguments.push_str(arguments);
                    }
                }
            }
        }
    }
    Ok(())
}

fn parse_usage(value: Option<&Value>) -> Option<LlmUsage> {
    let value = value?;
    if value.is_null() {
        return None;
    }
    Some(LlmUsage {
        prompt_tokens: value
            .get("prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        completion_tokens: value
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default() as usize,
    })
}

fn finish_tool_calls(tool_calls: BTreeMap<usize, PendingToolCall>) -> Vec<LlmToolCall> {
    tool_calls
        .into_values()
        .filter(|call| !call.name.trim().is_empty())
        .map(|call| LlmToolCall {
            id: if call.id.is_empty() {
                format!("call_{}", call.name)
            } else {
                call.id
            },
            kind: if call.kind.is_empty() {
                "function".into()
            } else {
                call.kind
            },
            function: LlmToolFunction {
                name: call.name,
                arguments: call.arguments,
            },
        })
        .collect()
}

fn stream_options_unsupported(status: reqwest::StatusCode, body: &str) -> bool {
    if status.as_u16() != 400 && status.as_u16() != 422 {
        return false;
    }
    let lower = body.to_ascii_lowercase();
    lower.contains("stream_options")
        && (lower.contains("unsupported")
            || lower.contains("unknown")
            || lower.contains("unrecognized")
            || lower.contains("invalid")
            || lower.contains("extra"))
}

fn truncate_for_tool(value: &str) -> String {
    const MAX: usize = 2_000;
    if value.chars().count() <= MAX {
        return value.to_string();
    }
    let mut output = value.chars().take(MAX).collect::<String>();
    output.push_str("...");
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_delta_accumulates_content_and_tool_calls() -> Result<()> {
        let mut content = String::new();
        let mut tools = BTreeMap::new();
        let mut streamed = String::new();
        let mut usage = None;

        apply_stream_delta(
            r#"{"choices":[{"delta":{"content":"hello "}}]}"#,
            &mut content,
            &mut tools,
            &mut usage,
            &mut |part| streamed.push_str(&part),
        )?;
        apply_stream_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_config","arguments":"{\"kind\":"}}]}}]}"#,
            &mut content,
            &mut tools,
            &mut usage,
            &mut |part| streamed.push_str(&part),
        )?;
        apply_stream_delta(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"draft\"}"}}]}}]}"#,
            &mut content,
            &mut tools,
            &mut usage,
            &mut |part| streamed.push_str(&part),
        )?;

        assert_eq!(content, "hello ");
        assert_eq!(streamed, "hello ");
        let calls = finish_tool_calls(tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].function.name, "read_config");
        assert_eq!(calls[0].function.arguments, r#"{"kind":"draft"}"#);
        Ok(())
    }

    #[test]
    fn stream_line_accepts_final_chunk_without_newline() -> Result<()> {
        let mut content = String::new();
        let mut tools = BTreeMap::new();
        let mut streamed = String::new();
        let mut usage = None;

        let done = apply_stream_line(
            r#"data: {"choices":[{"delta":{"content":"last line"}}]}"#,
            &mut content,
            &mut tools,
            &mut usage,
            &mut |part| streamed.push_str(&part),
        )?;

        assert!(!done);
        assert_eq!(content, "last line");
        assert_eq!(streamed, "last line");
        Ok(())
    }

    #[test]
    fn stream_line_detects_done_marker() -> Result<()> {
        let mut content = String::new();
        let mut tools = BTreeMap::new();
        let mut streamed = String::new();
        let mut usage = None;

        let done = apply_stream_line(
            "data: [DONE]",
            &mut content,
            &mut tools,
            &mut usage,
            &mut |part| streamed.push_str(&part),
        )?;

        assert!(done);
        assert!(content.is_empty());
        assert!(streamed.is_empty());
        Ok(())
    }

    #[test]
    fn stream_delta_reads_usage_chunk() -> Result<()> {
        let mut content = String::new();
        let mut tools = BTreeMap::new();
        let mut streamed = String::new();
        let mut usage = None;

        apply_stream_delta(
            r#"{"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#,
            &mut content,
            &mut tools,
            &mut usage,
            &mut |part| streamed.push_str(&part),
        )?;

        assert_eq!(
            usage,
            Some(LlmUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
                total_tokens: 18,
            })
        );
        Ok(())
    }
}
