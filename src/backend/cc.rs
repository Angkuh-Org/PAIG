//! Command Code backend. The upstream `/alpha/generate` endpoint speaks a
//! CLI-shaped event stream, not OpenAI chat completions. This module owns the
//! translation in both directions and preserves the upstream quirks: CLI
//! identity headers, stable thread ids, cache_control on system parts, and a
//! 64k max_tokens default.

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use crate::client::{HttpClient, Response as UpstreamResponse, Url};
use crate::config::Config;
use crate::http::Response;
use crate::json::Value;
use crate::util::{logi, logw, uuid_from_string};

const EFFORTS: &[&str] = &["off", "low", "medium", "high", "xhigh", "max"];
const PREFIXES: &[&str] = &["cc:", "cc/", "commandcode:", "commandcode/"];

pub fn cc_model_id(model: &str) -> Option<&str> {
    let raw = model.trim();
    for prefix in PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

pub fn normalize_model(model: &str) -> String {
    let mut raw = model.trim();
    for prefix in PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            raw = rest.trim();
            break;
        }
    }
    for prefix in [
        "openai-responses:",
        "openai-responses/",
        "openai:",
        "openai/",
        "anthropic:",
        "anthropic/",
        "gemini:",
        "gemini/",
        "vertex-ai:",
        "vertex-ai/",
        "qwen-oauth:",
        "qwen-oauth/",
        "paig:",
        "paig/",
        "ccbridge:",
        "ccbridge/",
    ] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            raw = rest.trim();
            break;
        }
    }
    if raw.is_empty() {
        String::new()
    } else {
        raw.to_string()
    }
}

fn requested_model(body: &Value) -> String {
    body.get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

fn text_of(content: &Value) -> String {
    match content {
        Value::Str(text) => text.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Value::Str(text) => out.push_str(text),
                    Value::Object(_) => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        } else if let Some(text) = item.get("content").and_then(Value::as_str) {
                            out.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| content.get("content").and_then(Value::as_str))
            .unwrap_or("")
            .to_string(),
        Value::Null => String::new(),
        other => other.stringify(),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::Str(text) => text.clone(),
        Value::Array(items) => {
            let mut out = String::new();
            for item in items {
                match item {
                    Value::Str(text) => out.push_str(text),
                    Value::Object(_) => {
                        if let Some(text) = item.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        Value::Null => String::new(),
        other => other.stringify(),
    }
}

fn image_part(input: &Value) -> Option<Value> {
    let source = input
        .get("image_url")
        .or_else(|| input.get("image"))
        .or_else(|| input.get("url"))?;
    let url = match source {
        Value::Str(url) => url.as_str(),
        Value::Object(_) => source.get("url").and_then(Value::as_str)?,
        _ => return None,
    };
    if url.is_empty() {
        return None;
    }
    let mut part = Value::obj();
    part.insert("type", Value::string("image"));
    part.insert("image", Value::string(url));
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some(end) = rest.find([';', ',']) {
            part.insert("mimeType", Value::string(&rest[..end]));
        }
    } else if let Some(mime) = input.get("mimeType").and_then(Value::as_str) {
        part.insert("mimeType", Value::string(mime));
    }
    Some(part)
}

fn content_to_parts(content: &Value) -> Vec<Value> {
    match content {
        Value::Str(text) => {
            if text.is_empty() {
                Vec::new()
            } else {
                let mut part = Value::obj();
                part.insert("type", Value::string("text"));
                part.insert("text", Value::string(text));
                vec![part]
            }
        }
        Value::Array(items) => {
            let mut parts = Vec::new();
            for item in items {
                match item {
                    Value::Str(text) => {
                        if !text.is_empty() {
                            let mut part = Value::obj();
                            part.insert("type", Value::string("text"));
                            part.insert("text", Value::string(text));
                            parts.push(part);
                        }
                    }
                    Value::Object(_) => {
                        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
                        match kind {
                            "text" | "input_text" => {
                                let text = item
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .or_else(|| item.get("content").and_then(Value::as_str))
                                    .unwrap_or("");
                                if !text.is_empty() {
                                    let mut part = Value::obj();
                                    part.insert("type", Value::string("text"));
                                    part.insert("text", Value::string(text));
                                    parts.push(part);
                                }
                            }
                            "image_url" | "input_image" => {
                                if let Some(image) = image_part(item) {
                                    parts.push(image);
                                }
                            }
                            "image" => {
                                if let Some(url) = item.get("image").and_then(Value::as_str) {
                                    let mut part = Value::obj();
                                    part.insert("type", Value::string("image"));
                                    part.insert("image", Value::string(url));
                                    if let Some(mime) = item.get("mimeType").and_then(Value::as_str) {
                                        part.insert("mimeType", Value::string(mime));
                                    }
                                    parts.push(part);
                                }
                            }
                            "file" => {
                                if let Some(file) = item.get("file") {
                                    if let Some(data) = file.get("file_data").and_then(Value::as_str) {
                                        let mut input = Value::obj();
                                        input.insert("image_url", Value::string(data));
                                        if let Some(image) = image_part(&input) {
                                            parts.push(image);
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            parts
        }
        Value::Object(_) => content_to_parts(&Value::Array(vec![content.clone()])),
        _ => Vec::new(),
    }
}

fn to_cc_tools(tools: &Value) -> Vec<Value> {
    let mut out = Vec::new();
    let Value::Array(items) = tools else { return out };
    for tool in items {
        let Some(object) = tool.as_object() else { continue };
        let definition = if tool.get("type").and_then(Value::as_str) == Some("function") {
            tool.get("function").unwrap_or(tool)
        } else {
            tool
        };
        let Some(name) = definition.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let description = definition
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        let mut schema = Value::obj();
        schema.insert("type", Value::string("object"));
        schema.insert("properties", Value::obj());
        if let Some(source) = definition
            .get("parameters")
            .or_else(|| definition.get("input_schema"))
        {
            if let Value::Object(items) = source {
                for (key, value) in items {
                    if key == "$schema" {
                        continue;
                    }
                    schema.insert(key.clone(), value.clone());
                }
            }
        }
        let mut entry = Value::obj();
        entry.insert("name", Value::string(name));
        entry.insert("description", Value::string(description));
        entry.insert("input_schema", schema);
        out.push(entry);
        let _ = object;
    }
    out
}

fn get_str_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cursor = value;
    for part in path {
        cursor = cursor.get(part)?;
    }
    cursor.as_str()
}

pub fn build_cc_request(body: &Value) -> Value {
    let model = normalize_model(body.get("model").and_then(Value::as_str).unwrap_or(""));
    let user = body.get("user").and_then(Value::as_str).unwrap_or("");
    let thread_id = get_str_path(body, &["thread_id"])
        .or_else(|| get_str_path(body, &["threadId"]))
        .or_else(|| get_str_path(body, &["extra_body", "thread_id"]))
        .or_else(|| get_str_path(body, &["extra_body", "threadId"]))
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid_from_string(&format!("{model}\0{user}")));

    let mut system_parts = Vec::new();
    let mut messages = Vec::new();
    if let Some(Value::Array(items)) = body.get("messages") {
        for message in items {
            let Some(role) = message.get("role").and_then(Value::as_str) else {
                continue;
            };
            match role {
                "system" | "developer" => {
                    let text = text_of(message.get("content").unwrap_or(&Value::Null));
                    if !text.is_empty() {
                        let mut part = Value::obj();
                        part.insert("type", Value::string("text"));
                        part.insert("text", Value::string(text));
                        system_parts.push(part);
                    }
                }
                "assistant" => {
                    let mut parts = Vec::new();
                    let text = text_of(message.get("content").unwrap_or(&Value::Null));
                    if !text.is_empty() {
                        let mut part = Value::obj();
                        part.insert("type", Value::string("text"));
                        part.insert("text", Value::string(text));
                        parts.push(part);
                    }
                    if let Some(reasoning) = message
                        .get("reasoning_content")
                        .or_else(|| message.get("reasoning"))
                        .and_then(Value::as_str)
                    {
                        if !reasoning.is_empty() {
                            let mut part = Value::obj();
                            part.insert("type", Value::string("reasoning"));
                            part.insert("text", Value::string(reasoning));
                            parts.push(part);
                        }
                    }
                    if let Some(Value::Array(calls)) = message.get("tool_calls") {
                        for call in calls {
                            let Some(name) = call
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                                .or_else(|| call.get("name").and_then(Value::as_str))
                            else {
                                continue;
                            };
                            let raw_arguments = call
                                .get("function")
                                .and_then(|f| f.get("arguments"))
                                .or_else(|| call.get("arguments"));
                            let input = match raw_arguments {
                                Some(Value::Str(text)) if !text.trim().is_empty() => {
                                    Value::parse_str(text).unwrap_or_else(|_| {
                                        let mut raw = Value::obj();
                                        raw.insert("raw", Value::string(text));
                                        raw
                                    })
                                }
                                Some(Value::Object(_)) => raw_arguments.cloned().unwrap(),
                                _ => Value::obj(),
                            };
                            let mut part = Value::obj();
                            part.insert("type", Value::string("tool-call"));
                            part.insert(
                                "toolCallId",
                                Value::string(call.get("id").and_then(Value::as_str).unwrap_or("")),
                            );
                            part.insert("toolName", Value::string(name));
                            part.insert("input", input);
                            parts.push(part);
                        }
                    }
                    if !parts.is_empty() {
                        let mut message = Value::obj();
                        message.insert("role", Value::string("assistant"));
                        message.insert("content", Value::Array(parts));
                        messages.push(message);
                    }
                }
                "tool" => {
                    let mut part = Value::obj();
                    part.insert("type", Value::string("tool-result"));
                    part.insert(
                        "toolCallId",
                        Value::string(message.get("tool_call_id").and_then(Value::as_str).unwrap_or("")),
                    );
                    part.insert(
                        "toolName",
                        Value::string(message.get("name").and_then(Value::as_str).unwrap_or("unknown")),
                    );
                    let mut output = Value::obj();
                    output.insert("type", Value::string("text"));
                    output.insert("value", Value::string(text_of(message.get("content").unwrap_or(&Value::Null))));
                    part.insert("output", output);
                    if let Some(Value::Object(last)) = messages.last_mut() {
                        let is_tool = last
                            .iter()
                            .find(|(key, _)| key == "role")
                            .and_then(|(_, value)| value.as_str())
                            == Some("tool");
                        if is_tool {
                            if let Some((_, Value::Array(content))) =
                                last.iter_mut().find(|(key, _)| key == "content")
                            {
                                content.push(part);
                                continue;
                            }
                        }
                    }
                    let mut message = Value::obj();
                    message.insert("role", Value::string("tool"));
                    message.insert("content", Value::Array(vec![part]));
                    messages.push(message);
                }
                _ => {
                    let parts = content_to_parts(message.get("content").unwrap_or(&Value::Null));
                    if !parts.is_empty() {
                        let mut message = Value::obj();
                        message.insert("role", Value::string("user"));
                        message.insert("content", Value::Array(parts));
                        messages.push(message);
                    }
                }
            }
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        let count = system_parts.len();
        let mut parts = Vec::with_capacity(count);
        for (index, mut part) in system_parts.into_iter().enumerate() {
            if index + 1 < count {
                let mut cache = Value::obj();
                cache.insert("type", Value::string("ephemeral"));
                part.insert("cache_control", cache);
            }
            parts.push(part);
        }
        Some(Value::Array(parts))
    };

    let mut params = Value::obj();
    params.insert("model", Value::string(&model));
    params.insert("messages", Value::Array(messages));
    params.insert("tools", Value::Array(to_cc_tools(body.get("tools").unwrap_or(&Value::Null))));
    let max_tokens = body
        .get("max_completion_tokens")
        .and_then(Value::as_i64)
        .or_else(|| body.get("max_tokens").and_then(Value::as_i64))
        .unwrap_or(64_000);
    params.insert("max_tokens", Value::Int(max_tokens));
    params.insert("stream", Value::Bool(true));
    if let Some(system) = system {
        params.insert("system", system);
    }
    if let Some(temperature) = body.get("temperature").and_then(Value::as_f64) {
        if temperature.is_finite() {
            params.insert("temperature", Value::Float(temperature));
        }
    }
    let raw_effort = get_str_path(body, &["reasoning_effort"])
        .or_else(|| get_str_path(body, &["reasoning", "effort"]))
        .or_else(|| get_str_path(body, &["extra_body", "reasoning_effort"]))
        .or_else(|| get_str_path(body, &["extra_body", "reasoning", "effort"]));
    if let Some(raw_effort) = raw_effort {
        let effort = raw_effort.trim().to_ascii_lowercase();
        if EFFORTS.contains(&effort.as_str()) {
            params.insert("reasoning_effort", Value::string(effort));
        }
    }

    let mut cc_config = Value::obj();
    cc_config.insert("workingDir", Value::string(""));
    cc_config.insert("environment", Value::string(platform_name()));
    cc_config.insert("structure", Value::Array(Vec::new()));
    cc_config.insert("isGitRepo", Value::Bool(false));
    cc_config.insert("currentBranch", Value::string(""));
    cc_config.insert("mainBranch", Value::string(""));
    cc_config.insert("gitStatus", Value::string(""));
    cc_config.insert("recentCommits", Value::Array(Vec::new()));
    cc_config.insert("date", Value::string(crate::util::utc_date()));

    let permission_mode = get_str_path(body, &["permission_mode"])
        .or_else(|| get_str_path(body, &["permissionMode"]))
        .or_else(|| get_str_path(body, &["extra_body", "permission_mode"]))
        .or_else(|| get_str_path(body, &["extra_body", "permissionMode"]))
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .unwrap_or("standard");

    let mut root = Value::obj();
    root.insert("config", cc_config);
    root.insert("memory", Value::Null);
    root.insert("taste", Value::Null);
    root.insert("skills", Value::Null);
    root.insert("permissionMode", Value::string(permission_mode));
    root.insert("threadId", Value::string(thread_id));
    root.insert("params", params);
    root
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

pub(crate) fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    if let Some((_, slot)) = headers
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        *slot = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

fn cc_headers(config: &Config, api_key: &str, session_id: &str) -> Vec<(String, String)> {
    let mut headers = config.command_code.headers.clone().unwrap_or_default();
    set_header(
        &mut headers,
        "content-type",
        "application/json".to_string(),
    );
    set_header(
        &mut headers,
        "authorization",
        format!("Bearer {api_key}"),
    );
    set_header(
        &mut headers,
        "x-project-slug",
        if config.command_code.project_slug.is_empty() {
            "paig".to_string()
        } else {
            config.command_code.project_slug.clone()
        },
    );
    set_header(&mut headers, "x-session-id", session_id.to_string());
    headers
}

pub fn handle(body: &Value, res: &mut Response, config: &Config, client: &HttpClient) -> Result<(), String> {
    let api_key = config.command_code.api_key.trim();
    if api_key.is_empty() {
        return res
            .send_openai_error(
                500,
                "No Command Code API key configured. Set commandCode.apiKey in ~/.paig/config.json, set CC_API_KEY, or run: paig --init",
                "bridge_error",
                None,
            )
            .map_err(|error| error.to_string());
    }

    let mut owned = None;
    let advisor_max = config.advisor_max_bytes.max(0) as usize;
    if oversized_user_payload(body, advisor_max) {
        let mut copy = body.clone();
        trim_advisor_payload(&mut copy, advisor_max);
        owned = Some(copy);
    }
    let body = owned.as_ref().unwrap_or(body);
    let cc_body = build_cc_request(body);
    let thread_id = cc_body
        .get("threadId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let payload = cc_body.stringify();
    let url = Url::parse(&format!(
        "{}/alpha/generate",
        config.command_code.base_url.trim_end_matches('/')
    ))?;
    let headers = cc_headers(config, api_key, &thread_id);
    if config.debug {
        let params = cc_body.get("params").unwrap_or(&Value::Null);
        logi!(
            "request model={} messages={} tools={} system={} effort={} stream={}",
            params.get("model").and_then(Value::as_str).unwrap_or(""),
            params
                .get("messages")
                .and_then(Value::as_array)
                .map(|v| v.len())
                .unwrap_or(0),
            params
                .get("tools")
                .and_then(Value::as_array)
                .map(|v| v.len())
                .unwrap_or(0),
            params
                .get("system")
                .and_then(Value::as_array)
                .map(|v| v.len())
                .unwrap_or(0),
            params
                .get("reasoning_effort")
                .and_then(Value::as_str)
                .unwrap_or(""),
            body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        );
    }

    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let upstream = fetch_with_retries(config, client, &url, &headers, payload.as_bytes())?;
        return finish_fetched(upstream, res, body, config, true);
    }
    fetch_nonstream_with_cancel(res, body, config, client, &url, &headers, payload)
}

fn finish_fetched(
    upstream: UpstreamResponse,
    res: &mut Response,
    body: &Value,
    config: &Config,
    streaming: bool,
) -> Result<(), String> {
    if upstream.status == 204 || upstream.status == 304 {
        return res
            .send_openai_error(
                502,
                "Upstream returned no body",
                "upstream_error",
                None,
            )
            .map_err(|error| error.to_string());
    }
    if !(200..300).contains(&upstream.status) {
        let status = if (400..600).contains(&upstream.status) {
            upstream.status
        } else {
            502
        };
        let mut upstream = upstream;
        let text = upstream.read_all().unwrap_or_default();
        let (message, code) = upstream_error(&text, status);
        return res
            .send_openai_error(status, &message, "upstream_error", code.as_deref())
            .map_err(|error| error.to_string());
    }

    if streaming {
        stream_cc(res, upstream, body, config)
    } else {
        collect_cc(res, upstream, body)
    }
}

fn fetch_nonstream_with_cancel(
    res: &mut Response,
    body: &Value,
    config: &Config,
    client: &HttpClient,
    url: &Url,
    headers: &[(String, String)],
    payload: String,
) -> Result<(), String> {
    // TODO(paig): this frees the worker on client disconnect, but the detached
    // fetch thread can live until the upstream timeout. True socket-level
    // cancellation is still an open residual risk.
    let (sender, receiver) = mpsc::channel();
    let fetch_config = config.clone();
    let fetch_client = client.clone();
    let fetch_url = url.clone();
    let fetch_headers = headers.to_vec();
    thread::Builder::new()
        .name("paig-cc-fetch".to_string())
        .stack_size(crate::rules::THREAD_STACK_BYTES)
        .spawn(move || {
            let result = fetch_with_retries(
                &fetch_config,
                &fetch_client,
                &fetch_url,
                &fetch_headers,
                payload.as_bytes(),
            );
            let _ = sender.send(result);
        })
        .map_err(|error| format!("cannot start upstream fetch: {error}"))?;

    loop {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(upstream)) => return finish_fetched(upstream, res, body, config, false),
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => {
                if res.client_closed() {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err("upstream fetch worker disappeared".to_string());
            }
        }
    }
}

fn fetch_with_retries(
    config: &Config,
    client: &HttpClient,
    url: &Url,
    headers: &[(String, String)],
    payload: &[u8],
) -> Result<UpstreamResponse, String> {
    // TODO(paig): a read timeout after the POST reached the upstream is also
    // retried; distinguish pre-send failures to avoid duplicate generation or
    // billing when upstreams are non-idempotent.
    let retries = config.command_code.fetch_retries.clamp(0, 10) as u32;
    let mut last_error = String::new();
    for attempt in 0..=retries {
        let started = crate::util::now_ms();
        match client.request(
            "POST",
            url,
            headers,
            Some(payload),
            Some(Duration::from_secs(600)),
        ) {
            Ok(response) => return Ok(response),
            Err(error) => {
                last_error = error.to_string();
                if attempt >= retries {
                    break;
                }
                let wait = (config.command_code.fetch_backoff_ms.clamp(0, 60_000) as u64)
                    .saturating_mul(attempt as u64 + 1);
                logw!(
                    "Command Code fetch failed after {}ms ({}); retry {}/{} in {}ms",
                    crate::util::now_ms().saturating_sub(started),
                    last_error,
                    attempt + 1,
                    retries,
                    wait
                );
                thread::sleep(Duration::from_millis(wait));
            }
        }
    }
    Err(format!("Cannot reach Command Code: {last_error}"))
}

fn oversized_user_payload(body: &Value, max: usize) -> bool {
    if max == 0 {
        return false;
    }
    body.get("messages")
        .and_then(Value::as_array)
        .map(|messages| {
            messages.iter().any(|message| {
                message.get("role").and_then(Value::as_str) == Some("user")
                    && content_text(message.get("content").unwrap_or(&Value::Null)).len() > max
            })
        })
        .unwrap_or(false)
}

fn trim_advisor_payload(body: &mut Value, max: usize) {
    if max == 0 {
        return;
    }
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return;
    };
    let mut target = None;
    for (index, message) in messages.iter().enumerate() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = content_text(message.get("content").unwrap_or(&Value::Null));
        if text.len() > max {
            target = Some((index, text));
            break;
        }
    }
    let Some((index, text)) = target else { return };
    let Some(start) = text.find('{') else { return };
    let Some(end) = text.rfind('}') else { return };
    if end <= start {
        return;
    }
    let Ok(mut parsed) = Value::parse_str(&text[start..=end]) else {
        logw!("advisor payload {} bytes but no JSON transcript; not trimmed", text.len());
        return;
    };
    let initial_entries = parsed
        .get("transcript")
        .and_then(Value::as_array)
        .map(|v| v.len())
        .unwrap_or(0);
    if initial_entries == 0 {
        return;
    }
    let original = text.len();
    loop {
        let len = parsed
            .get("transcript")
            .and_then(Value::as_array)
            .map(|v| v.len())
            .unwrap_or(0);
        if len <= 1 || parsed.stringify().len() <= max {
            break;
        }
        if let Some(Value::Array(transcript)) = parsed.get_mut("transcript") {
            transcript.remove(0);
        } else {
            break;
        }
    }
    let entries = parsed
        .get("transcript")
        .and_then(Value::as_array)
        .map(|v| v.len())
        .unwrap_or(0);
    let note = format!(
        "\n\n[PAIG] Transcript truncated to the most recent {entries} entries to fit the upstream request budget."
    );
    let existing = parsed
        .get("executorSystemInstruction")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    parsed.insert(
        "executorSystemInstruction",
        Value::string(format!("{existing}{note}")),
    );
    let next = format!("{}{}{}", &text[..start], parsed.stringify(), &text[end + 1..]);
    if next.len() >= original {
        return;
    }
    if let Some(message) = messages.get_mut(index) {
        message.insert("content", Value::string(next.clone()));
    }
    logi!(
        "advisor transcript trimmed {} -> {} bytes ({} entries)",
        original,
        next.len(),
        entries
    );
}

fn upstream_error(text: &[u8], status: u16) -> (String, Option<String>) {
    let text = String::from_utf8_lossy(text);
    let fallback = if text.is_empty() {
        format!("upstream returned HTTP {status}")
    } else {
        text.to_string()
    };
    if let Ok(value) = Value::parse_str(&text) {
        if let Some(error) = value.get("error") {
            if let Some(message) = error.get("message").and_then(Value::as_str) {
                return (
                    message.to_string(),
                    error.get("code").and_then(Value::as_str).map(str::to_string),
                );
            }
            if let Some(message) = error.as_str() {
                return (message.to_string(), None);
            }
        }
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            return (message.to_string(), None);
        }
    }
    (fallback, None)
}

fn map_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?;
    if !matches!(usage, Value::Object(_)) {
        return None;
    }
    let prompt = usage
        .get("inputTokens")
        .and_then(Value::as_i64)
        .or_else(|| usage.get("prompt_tokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let completion = usage
        .get("outputTokens")
        .and_then(Value::as_i64)
        .or_else(|| usage.get("completion_tokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let cached = usage
        .get("inputTokenDetails")
        .and_then(|v| v.get("cacheReadTokens"))
        .and_then(Value::as_i64)
        .or_else(|| usage.get("cacheReadTokens").and_then(Value::as_i64))
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    let reasoning = usage
        .get("outputTokenDetails")
        .and_then(|v| v.get("reasoningTokens"))
        .and_then(Value::as_i64)
        .or_else(|| usage.get("reasoningTokens").and_then(Value::as_i64))
        .unwrap_or(0);
    let mut out = Value::obj();
    out.insert("prompt_tokens", Value::Int(prompt));
    out.insert("completion_tokens", Value::Int(completion));
    out.insert("total_tokens", Value::Int(prompt + completion));
    if cached > 0 {
        let mut details = Value::obj();
        details.insert("cached_tokens", Value::Int(cached));
        out.insert("prompt_tokens_details", details);
    }
    if reasoning > 0 {
        let mut details = Value::obj();
        details.insert("reasoning_tokens", Value::Int(reasoning));
        out.insert("completion_tokens_details", details);
    }
    Some(out)
}

fn map_finish_reason(event: &Value, had_tool_call: bool) -> String {
    if had_tool_call {
        return "tool_calls".to_string();
    }
    let raw = event
        .get("rawFinishReason")
        .or_else(|| event.get("finishReason"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    if raw.contains("length") || raw.contains("max_tokens") {
        "length".to_string()
    } else if raw.contains("tool") {
        "tool_calls".to_string()
    } else if raw.contains("content_filter")
        || raw.contains("content-filter")
        || raw.contains("refusal")
    {
        "content_filter".to_string()
    } else {
        "stop".to_string()
    }
}

fn stream_error(event: &Value) -> String {
    event
        .get("error")
        .and_then(|error| {
            error
                .as_str()
                .map(str::to_string)
                .or_else(|| error.get("message").and_then(Value::as_str).map(str::to_string))
        })
        .or_else(|| event.get("message").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "Upstream stream error".to_string())
}

fn stream_error_status(event: &Value) -> u16 {
    event
        .get("error")
        .and_then(|error| error.get("statusCode"))
        .and_then(Value::as_i64)
        .filter(|status| (400..600).contains(status))
        .map(|status| status as u16)
        .unwrap_or(502)
}

fn write_text_frame(out: &mut Vec<u8>, field: &str, text: &str) -> io::Result<()> {
    out.push(b'{');
    crate::json::write_json_string(out, field)?;
    out.push(b':');
    crate::json::write_json_string(out, text)?;
    out.extend_from_slice(b"},\"finish_reason\":null,\"logprobs\":null}]}\n\n");
    Ok(())
}

struct StreamState {
    id: String,
    model: String,
    created: u64,
    role_sent: bool,
    finish_sent: bool,
    saw_finish: bool,
    finish_reason: String,
    usage: Option<Value>,
    had_tool_call: bool,
    tool_index: i64,
    tool_indices: HashMap<String, i64>,
    /// `data: {...,"choices":[{"index":0,"delta":` — identical for every
    /// chunk, so text deltas only append their value and the tail.
    head: Vec<u8>,
    frame: Vec<u8>,
}

impl StreamState {
    fn new(body: &Value) -> Self {
        let id = format!("chatcmpl-{}", crate::crypto::random_hex(12));
        let model = requested_model(body);
        let created = crate::util::now_unix();
        let mut head = Vec::with_capacity(id.len() + model.len() + 128);
        head.extend_from_slice(b"data: {\"id\":");
        crate::json::write_json_string(&mut head, &id).expect("Vec writes cannot fail");
        head.extend_from_slice(b",\"object\":\"chat.completion.chunk\",\"created\":");
        crate::json::write_i64(&mut head, created as i64).expect("Vec writes cannot fail");
        head.extend_from_slice(b",\"model\":");
        crate::json::write_json_string(&mut head, &model).expect("Vec writes cannot fail");
        head.extend_from_slice(b",\"choices\":[{\"index\":0,\"delta\":");
        StreamState {
            id,
            model,
            created,
            role_sent: false,
            finish_sent: false,
            saw_finish: false,
            finish_reason: "stop".to_string(),
            usage: None,
            had_tool_call: false,
            tool_index: 0,
            tool_indices: HashMap::new(),
            head,
            frame: Vec::new(),
        }
    }

    /// Single-string delta (`content` or `reasoning_content`) without building
    /// a DOM, stringifying it, and formatting it into a second allocation.
    fn chunk_text(&mut self, res: &mut Response, field: &str, text: &str) -> io::Result<()> {
        self.frame.clear();
        self.frame.extend_from_slice(&self.head);
        write_text_frame(&mut self.frame, field, text)?;
        res.sse_write(&self.frame)
    }

    fn dom_frame(&self, delta: Value, finish_reason: Option<&str>) -> String {
        let mut choice = Value::obj();
        choice.insert("index", Value::Int(0));
        choice.insert("delta", delta);
        choice.insert(
            "finish_reason",
            finish_reason
                .map(Value::string)
                .unwrap_or(Value::Null),
        );
        choice.insert("logprobs", Value::Null);
        let mut root = Value::obj();
        root.insert("id", Value::string(&self.id));
        root.insert("object", Value::string("chat.completion.chunk"));
        root.insert("created", Value::Int(self.created as i64));
        root.insert("model", Value::string(&self.model));
        root.insert("choices", Value::Array(vec![choice]));
        format!("data: {}\n\n", root.stringify())
    }

    fn chunk(
        &mut self,
        res: &mut Response,
        delta: Value,
        finish_reason: Option<&str>,
    ) -> io::Result<()> {
        let frame = self.dom_frame(delta, finish_reason);
        res.sse_write(frame.as_bytes())
    }

    fn ensure_role(&mut self, res: &mut Response) -> io::Result<()> {
        if self.role_sent {
            return Ok(());
        }
        self.role_sent = true;
        let mut delta = Value::obj();
        delta.insert("role", Value::string("assistant"));
        delta.insert("content", Value::string(""));
        self.chunk(res, delta, None)
    }

    fn handle(&mut self, event: &Value, res: &mut Response) -> Result<(), String> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text-delta" => {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.ensure_role(res).map_err(|e| e.to_string())?;
                        self.chunk_text(res, "content", text)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            "reasoning-delta" => {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.ensure_role(res).map_err(|e| e.to_string())?;
                        self.chunk_text(res, "reasoning_content", text)
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            "tool-call" => {
                self.ensure_role(res).map_err(|e| e.to_string())?;
                let name = event
                    .get("toolName")
                    .or_else(|| event.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                let call_id = event
                    .get("toolCallId")
                    .or_else(|| event.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", crate::crypto::random_hex(8)));
                let index = if let Some(index) = self.tool_indices.get(&call_id) {
                    *index
                } else {
                    let index = self.tool_index;
                    self.tool_index += 1;
                    self.tool_indices.insert(call_id.clone(), index);
                    index
                };
                let input = event
                    .get("input")
                    .or_else(|| event.get("args"))
                    .cloned()
                    .unwrap_or_else(Value::obj);
                let arguments = match input {
                    Value::Str(text) => text,
                    other => other.stringify(),
                };
                let mut function = Value::obj();
                function.insert("name", Value::string(name));
                function.insert("arguments", Value::string(arguments));
                let mut call = Value::obj();
                call.insert("index", Value::Int(index));
                call.insert("id", Value::string(call_id));
                call.insert("type", Value::string("function"));
                call.insert("function", function);
                let mut delta = Value::obj();
                delta.insert("tool_calls", Value::Array(vec![call]));
                self.chunk(res, delta, None).map_err(|e| e.to_string())?;
                self.had_tool_call = true;
            }
            "finish" => {
                self.saw_finish = true;
                if let Some(usage) = event.get("totalUsage") {
                    if let Some(mapped) = map_usage(Some(usage)) {
                        self.usage = Some(mapped);
                    }
                }
                self.finish_reason = map_finish_reason(event, self.had_tool_call);
            }
            "error" => return Err(stream_error(event)),
            _ => {}
        }
        Ok(())
    }

    fn finish(&mut self, res: &mut Response) -> io::Result<()> {
        if self.finish_sent {
            return Ok(());
        }
        self.finish_sent = true;
        self.ensure_role(res)?;
        self.chunk(res, Value::obj(), Some(&self.finish_reason.clone()))?;
        if let Some(usage) = &self.usage {
            let mut root = Value::obj();
            root.insert("id", Value::string(&self.id));
            root.insert("object", Value::string("chat.completion.chunk"));
            root.insert("created", Value::Int(self.created as i64));
            root.insert("model", Value::string(&self.model));
            root.insert("choices", Value::Array(Vec::new()));
            root.insert("usage", usage.clone());
            let frame = format!("data: {}\n\n", root.stringify());
            res.sse_write(frame.as_bytes())?;
        }
        res.sse_write(b"data: [DONE]\n\n")?;
        res.sse_end()
    }
}

fn stream_error_frame(res: &mut Response, message: &str, status: Option<u16>) {
    let mut error = Value::obj();
    error.insert("message", Value::string(message));
    error.insert("type", Value::string("upstream_error"));
    if let Some(status) = status {
        error.insert("code", Value::string(status.to_string()));
    }
    let mut root = Value::obj();
    root.insert("error", error);
    let frame = format!("data: {}\n\n", root.stringify());
    let _ = res.sse_write(frame.as_bytes());
}

/// Command Code currently streams bare NDJSON objects;
fn cc_stream_event(line: &[u8]) -> Option<Value> {
    let text = line.trim_ascii();
    let payload = text.strip_prefix(b"data:").unwrap_or(text).trim_ascii();
    if payload.is_empty() || payload == b"[DONE]".as_slice() || !payload.starts_with(b"{") {
        return None;
    }
    Value::parse(payload).ok()
}

fn stream_cc(
    res: &mut Response,
    mut upstream: UpstreamResponse,
    body: &Value,
    config: &Config,
) -> Result<(), String> {
    res.start_sse().map_err(|error| error.to_string())?;
    let mut state = StreamState::new(body);
    let mut line = Vec::new();
    loop {
        let has_line = match upstream.read_line_into(&mut line) {
            Ok(has_line) => has_line,
            Err(error) => {
                let message = format!("upstream stream error: {error}");
                stream_error_frame(res, &message, Some(502));
                let _ = res.sse_end();
                return Ok(());
            }
        };
        if !has_line {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let trimmed = line.trim_ascii();
        if trimmed == b"data: [DONE]".as_slice() || trimmed == b"[DONE]".as_slice() {
            break;
        }
        let Some(event) = cc_stream_event(trimmed) else {
            if config.debug {
                logw!(
                    "non-JSON stream line {}",
                    crate::util::truncate_utf8(&String::from_utf8_lossy(trimmed), 200)
                );
            }
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("error") {
            let message = stream_error(&event);
            let status = stream_error_status(&event);
            stream_error_frame(res, &message, Some(status));
            let _ = res.sse_end();
            return Ok(());
        }
        if let Err(error) = state.handle(&event, res) {
            stream_error_frame(res, &error, Some(502));
            let _ = res.sse_end();
            return Ok(());
        }
    }
    if !state.saw_finish && config.debug {
        logw!("upstream ended without finish event");
    }
    state.finish(res).map_err(|error| error.to_string())
}

fn collect_cc(
    res: &mut Response,
    mut upstream: UpstreamResponse,
    body: &Value,
) -> Result<(), String> {
    let model = requested_model(body);
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish_reason = "stop".to_string();
    let mut usage = None;
    let mut tool_calls = Vec::new();
    let mut lines = 0usize;
    let mut line = Vec::new();
    loop {
        if lines % 16 == 0 && res.client_closed() {
            return Ok(());
        }
        lines += 1;
        let has_line = upstream
            .read_line_into(&mut line)
            .map_err(|error| error.to_string())?;
        if !has_line {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let trimmed = line.trim_ascii();
        if trimmed == b"data: [DONE]".as_slice() || trimmed == b"[DONE]".as_slice() {
            break;
        }
        let Some(event) = cc_stream_event(trimmed) else {
            continue;
        };
        match event.get("type").and_then(Value::as_str).unwrap_or("") {
            "text-delta" => {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    content.push_str(text);
                }
            }
            "reasoning-delta" => {
                if let Some(text) = event.get("text").and_then(Value::as_str) {
                    reasoning.push_str(text);
                }
            }
            "tool-call" => {
                let call_id = event
                    .get("toolCallId")
                    .or_else(|| event.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", crate::crypto::random_hex(8)));
                let input = event
                    .get("input")
                    .or_else(|| event.get("args"))
                    .cloned()
                    .unwrap_or_else(Value::obj);
                let arguments = match input {
                    Value::Str(text) => text,
                    other => other.stringify(),
                };
                let mut function = Value::obj();
                function.insert(
                    "name",
                    Value::string(
                        event
                            .get("toolName")
                            .or_else(|| event.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown"),
                    ),
                );
                function.insert("arguments", Value::string(arguments));
                let mut call = Value::obj();
                call.insert("id", Value::string(call_id));
                call.insert("type", Value::string("function"));
                call.insert("function", function);
                tool_calls.push(call);
            }
            "finish" => {
                if let Some(mapped) = map_usage(event.get("totalUsage")) {
                    usage = Some(mapped);
                }
                finish_reason = map_finish_reason(&event, !tool_calls.is_empty());
            }
            "error" => {
                let status = stream_error_status(&event);
                return res
                    .send_openai_error(status, &stream_error(&event), "upstream_error", None)
                    .map_err(|error| error.to_string());
            }
            _ => {}
        }
    }

    let mut message = Value::obj();
    message.insert("role", Value::string("assistant"));
    message.insert(
        "content",
        if content.is_empty() {
            Value::Null
        } else {
            Value::string(content)
        },
    );
    if !reasoning.is_empty() {
        message.insert("reasoning_content", Value::string(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls", Value::Array(tool_calls));
    }
    let mut choice = Value::obj();
    choice.insert("index", Value::Int(0));
    choice.insert("message", message);
    choice.insert("finish_reason", Value::string(finish_reason));
    choice.insert("logprobs", Value::Null);
    let mut root = Value::obj();
    root.insert("id", Value::string(format!("chatcmpl-{}", crate::crypto::random_hex(12))));
    root.insert("object", Value::string("chat.completion"));
    root.insert("created", Value::Int(crate::util::now_unix() as i64));
    root.insert("model", Value::string(model));
    root.insert("choices", Value::Array(vec![choice]));
    if let Some(usage) = usage {
        root.insert("usage", usage);
    }
    res.send_json(200, &root).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_prefixes_are_stripped() {
        assert_eq!(normalize_model("openai:gpt-5"), "gpt-5");
        assert_eq!(normalize_model("paig:deepseek/x"), "deepseek/x");
        assert_eq!(normalize_model("cc:deepseek/x"), "deepseek/x");
        assert_eq!(normalize_model("cc/deepseek/x"), "deepseek/x");
        assert_eq!(normalize_model("commandcode/foo"), "foo");
        assert_eq!(cc_model_id("commandcode/foo"), Some("foo"));
        assert_eq!(cc_model_id("cc/"), None);
        assert_eq!(normalize_model(""), "");
    }

    #[test]
    fn cc_stream_event_accepts_ndjson_and_sse() {
        let bare = r#"{"type":"text-delta","id":"txt-0","text":"OK"}"#;
        let event = cc_stream_event(bare.as_bytes()).expect("bare NDJSON event");
        assert_eq!(event.get("type").unwrap().as_str(), Some("text-delta"));
        assert_eq!(event.get("text").unwrap().as_str(), Some("OK"));

        let sse = format!("data: {bare}");
        assert_eq!(
            cc_stream_event(sse.as_bytes()).unwrap().get("text").unwrap().as_str(),
            Some("OK")
        );
        assert!(cc_stream_event(b"data: [DONE]").is_none());
        assert!(cc_stream_event(b"[DONE]").is_none());
        assert!(cc_stream_event(b"event: message").is_none());
    }

    #[test]
    fn text_frames_match_the_dom_serializer() {
        for text in ["hello", "a\"b\\c\n", "é😀"] {
            let mut head = Vec::new();
            head.extend_from_slice(
                b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\
                  \"created\":1,\"model\":\"cc/test\",\"choices\":[{\"index\":0,\"delta\":",
            );
            let mut fast = head.clone();
            write_text_frame(&mut fast, "content", text).unwrap();

            let mut delta = Value::obj();
            delta.insert("content", Value::string(text));
            let mut choice = Value::obj();
            choice.insert("index", Value::Int(0));
            choice.insert("delta", delta);
            choice.insert("finish_reason", Value::Null);
            choice.insert("logprobs", Value::Null);
            let mut root = Value::obj();
            root.insert("id", Value::string("chatcmpl-x"));
            root.insert("object", Value::string("chat.completion.chunk"));
            root.insert("created", Value::Int(1));
            root.insert("model", Value::string("cc/test"));
            root.insert("choices", Value::Array(vec![choice]));
            let slow = format!("data: {}\n\n", root.stringify());
            assert_eq!(fast, slow.as_bytes(), "text={text:?}");
        }
    }

    #[test]
    fn stream_head_matches_the_dom_prefix() {
        let body = Value::parse_str(r#"{"model":"cc/test-model"}"#).unwrap();
        let state = StreamState::new(&body);
        let mut fast = state.head.clone();
        write_text_frame(&mut fast, "content", "x").unwrap();

        let mut delta = Value::obj();
        delta.insert("content", Value::string("x"));
        let slow = state.dom_frame(delta, None);
        assert_eq!(fast, slow.as_bytes());
    }

    #[test]
    fn cc_request_maps_system_messages_tools_and_thread() {
        let body = Value::parse_str(
            r#"{
              "model":"test-model",
              "user":"u1",
              "messages":[
                {"role":"system","content":"sys"},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"ok","reasoning_content":"why",
                 "tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{\"a\":1}"}}]},
                {"role":"tool","tool_call_id":"c1","name":"f","content":"result"}
              ],
              "tools":[{"type":"function","function":{"name":"f","description":"d",
                "parameters":{"$schema":"x","type":"object","properties":{"a":{"type":"integer"}}}}}],
              "max_tokens":123,
              "reasoning_effort":"high",
              "permission_mode":"custom-mode"
            }"#,
        )
        .unwrap();
        let cc = build_cc_request(&body);
        let params = cc.get("params").unwrap();
        assert_eq!(params.get("model").unwrap().as_str(), Some("test-model"));
        assert_eq!(params.get("max_tokens").unwrap().as_i64(), Some(123));
        assert_eq!(params.get("reasoning_effort").unwrap().as_str(), Some("high"));
        assert_eq!(params.get("stream").unwrap().as_bool(), Some(true));
        let system = params.get("system").unwrap().as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert!(system[0].get("cache_control").is_none());
        let messages = params.get("messages").unwrap().as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].get("role").unwrap().as_str(), Some("assistant"));
        assert_eq!(messages[2].get("role").unwrap().as_str(), Some("tool"));
        let tools = params.get("tools").unwrap().as_array().unwrap();
        assert_eq!(tools.len(), 1);
        let schema = tools[0].get("input_schema").unwrap();
        assert!(schema.get("$schema").is_none());
        assert!(schema.get("properties").is_some());
        assert_eq!(
            cc.get("threadId").unwrap().as_str().unwrap().len(),
            36,
            "uuid-shaped thread id"
        );
        assert_eq!(cc.get("permissionMode").unwrap().as_str(), Some("custom-mode"));
    }

    #[test]
    fn thread_id_is_stable_and_user_sensitive() {
        let mut a = Value::obj();
        a.insert("model", Value::string("m"));
        a.insert("user", Value::string("u1"));
        let mut b = a.clone();
        let mut c = a.clone();
        c.insert("user", Value::string("u2"));
        let ca = build_cc_request(&a);
        let cb = build_cc_request(&b);
        let cc = build_cc_request(&c);
        assert_eq!(ca.get("threadId"), cb.get("threadId"));
        assert_ne!(ca.get("threadId"), cc.get("threadId"));
        b.insert("user", Value::string("u3"));
        assert_ne!(build_cc_request(&a).get("threadId"), build_cc_request(&b).get("threadId"));
        let mut d = a.clone();
        d.insert("thread_id", Value::string("fixed-thread"));
        assert_eq!(
            build_cc_request(&d).get("threadId").unwrap().as_str(),
            Some("fixed-thread")
        );
    }

    #[test]
    fn advisor_payload_is_trimmed() {
        let mut transcript = Vec::new();
        for index in 0..6 {
            let mut entry = Value::obj();
            entry.insert("n", Value::Int(index));
            entry.insert("text", Value::string("x".repeat(80)));
            transcript.push(entry);
        }
        let mut payload = Value::obj();
        payload.insert("transcript", Value::Array(transcript));
        payload.insert("executorSystemInstruction", Value::string("base"));
        let text = format!("prefix-{}-{}", payload.stringify(), "-suffix");
        let mut body = Value::obj();
        let mut message = Value::obj();
        message.insert("role", Value::string("user"));
        message.insert("content", Value::string(text));
        body.insert("messages", Value::Array(vec![message]));
        trim_advisor_payload(&mut body, 220);
        let content = body
            .get("messages")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("content")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(content.contains("[PAIG]"));
        assert!(content.len() < 600);
    }

    #[test]
    fn fixed_headers_replace_config_collisions() {
        let mut config = Config::defaults();
        config.command_code.headers = Some(vec![
            ("authorization".to_string(), "old".to_string()),
            ("X-Project-Slug".to_string(), "old".to_string()),
        ]);
        let headers = cc_headers(&config, "key", "session");
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .count(),
            1
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.as_str()),
            Some("Bearer key")
        );
        assert_eq!(
            headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("x-project-slug"))
                .count(),
            1
        );
    }

    #[test]
    fn usage_and_finish_reasons_map() {
        let usage = Value::parse_str(
            r#"{"inputTokens":10,"outputTokens":4,"inputTokenDetails":{"cacheReadTokens":3},"outputTokenDetails":{"reasoningTokens":2}}"#,
        )
        .unwrap();
        let mapped = map_usage(Some(&usage)).unwrap();
        assert_eq!(mapped.get("prompt_tokens").unwrap().as_i64(), Some(10));
        assert_eq!(mapped.get("completion_tokens").unwrap().as_i64(), Some(4));
        assert_eq!(mapped.get("total_tokens").unwrap().as_i64(), Some(14));
        assert_eq!(
            mapped
                .get("prompt_tokens_details")
                .unwrap()
                .get("cached_tokens")
                .unwrap()
                .as_i64(),
            Some(3)
        );
        let finish = Value::parse_str(r#"{"rawFinishReason":"max_tokens"}"#).unwrap();
        assert_eq!(map_finish_reason(&finish, false), "length");
        assert_eq!(map_finish_reason(&finish, true), "tool_calls");
        let stop = Value::parse_str(r#"{"rawFinishReason":"stop"}"#).unwrap();
        assert_eq!(map_finish_reason(&stop, false), "stop");
    }
}
