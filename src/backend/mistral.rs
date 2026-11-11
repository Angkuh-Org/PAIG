//! Mistral backend: `mistral/<id>`. Mistral speaks OpenAI chat completions,
//! but reasoning models answer with `content: [{type:"thinking"},...]` and the
//! Tool model rejects the whole `tools[]` union when one no-argument function
//! omits `parameters`. Both quirks are normalized at the edge.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::client::{HttpClient, Response as UpstreamResponse, Url};
use crate::config::{read_key_file, Config};
use crate::http::Response;
use crate::json::Value;
use crate::util::logw;

const MODEL_TTL: Duration = Duration::from_secs(300);
const API_TIMEOUT: Duration = Duration::from_secs(300);
const MODELS_TIMEOUT: Duration = Duration::from_secs(5);
const KEY_MISSING: &str = "Mistral API key is not configured. Set mistral.apiKey, mistral.apiKeyFile, or PAIG_MISTRAL_API_KEY.";
const RATE_HEADERS: &[&str] = &[
    "retry-after",
    "x-ratelimit-limit-req-minute",
    "x-ratelimit-remaining-req-minute",
    "x-ratelimit-limit-tokens-minute",
    "x-ratelimit-remaining-tokens-minute",
];

static CACHE: OnceLock<Mutex<Option<(Instant, Vec<Value>)>>> = OnceLock::new();

// TODO(paig): Command Code also exposes vendor-prefixed ids such as
// `mistral/mistral-large-4`. Dispatch order makes the Mistral copy win and the
// catalogue dedupe is last-wins; choose an explicit disambiguation if CC's copy
// must remain reachable.
pub fn mistral_model_id(model: &str) -> Option<String> {
    let id = model.trim().strip_prefix("mistral/")?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn api_key(config: &Config) -> String {
    if !config.mistral.api_key.is_empty() {
        return config.mistral.api_key.clone();
    }
    if config.mistral.api_key_file.is_empty() {
        return String::new();
    }
    read_key_file(Path::new(&config.mistral.api_key_file))
}

fn upstream_headers(key: &str) -> Vec<(String, String)> {
    let mut headers = Vec::with_capacity(2);
    headers.push(("content-type".to_string(), "application/json".to_string()));
    if !key.is_empty() {
        headers.push(("authorization".to_string(), format!("Bearer {key}")));
    }
    headers
}

fn unreachable(res: &mut Response, error: &str) -> Result<(), String> {
    res.send_openai_error(
        502,
        &format!("Cannot reach Mistral API: {error}"),
        "upstream_error",
        None,
    )
    .map_err(|error| error.to_string())
}

pub fn handle(
    body: &Value,
    res: &mut Response,
    model_id: &str,
    config: &Config,
    client: &HttpClient,
) -> Result<(), String> {
    let key = api_key(config);
    if key.is_empty() {
        return res
            .send_openai_error(503, KEY_MISSING, "bridge_error", None)
            .map_err(|error| error.to_string());
    }
    let url = match Url::parse(&format!(
        "{}/chat/completions",
        config.mistral.base_url.trim_end_matches('/')
    )) {
        Ok(url) => url,
        Err(error) => return unreachable(res, &error),
    };
    let payload = normalize_request(body, model_id);
    let wire = payload.stringify();
    let started = Instant::now();
    let mut upstream = match client.request(
        "POST",
        &url,
        &upstream_headers(&key),
        Some(wire.as_bytes()),
        Some(API_TIMEOUT),
    ) {
        Ok(upstream) => upstream,
        Err(error) => return unreachable(res, &error.to_string()),
    };
    if !(200..300).contains(&upstream.status) {
        return forward_error(res, &mut upstream, &payload, model_id, wire.len(), started.elapsed());
    }
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        return stream_upstream(res, &mut upstream);
    }
    forward_json(res, &mut upstream, body, model_id)
}

/// No-argument tools get the `parameters` Mistral insists on, and the model is
/// the stripped id. Everything else is passed through untouched.
fn normalize_request(body: &Value, model_id: &str) -> Value {
    let mut payload = body.clone();
    payload.insert("model", Value::string(model_id));
    if let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                continue;
            }
            let Some(function) = tool.get_mut("function") else { continue };
            if !matches!(function, Value::Object(_)) {
                continue;
            }
            if function
                .get("parameters")
                .map(|value| !value.is_null())
                .unwrap_or(false)
            {
                continue;
            }
            let mut parameters = Value::obj();
            parameters.insert("type", Value::string("object"));
            parameters.insert("properties", Value::obj());
            function.insert("parameters", parameters);
        }
    }
    payload
}

fn forward_error(
    res: &mut Response,
    upstream: &mut UpstreamResponse,
    payload: &Value,
    model_id: &str,
    bytes: usize,
    elapsed: Duration,
) -> Result<(), String> {
    let raw = upstream.read_all().unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    let len_of = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.len())
            .unwrap_or(0)
    };
    let max_tokens = payload
        .get("max_tokens")
        .filter(|value| !value.is_null())
        .map(Value::stringify)
        .unwrap_or_else(|| "unset".to_string());
    logw!(
        "mistral {} after {}ms (model={}, messages={}, tools={}, max_tokens={}, bytes={})",
        upstream.status,
        elapsed.as_millis(),
        model_id,
        len_of("messages"),
        len_of("tools"),
        max_tokens,
        bytes
    );
    let status = if (400..600).contains(&upstream.status) {
        upstream.status
    } else {
        502
    };
    let body = match normalize_error_body(text.as_ref()) {
        Some(value) => value.stringify(),
        None => text.to_string(),
    };
    // send_bytes sets content-type/length last; the rate-limit hints must be
    // queued on the response before that call.
    let content_type = upstream
        .header("content-type")
        .unwrap_or("application/json")
        .to_string();
    for name in RATE_HEADERS {
        if let Some(value) = upstream.header(name) {
            res.set_header(name, value);
        }
    }
    res.send_bytes(status, &content_type, body.as_bytes())
        .map_err(|error| error.to_string())
}

fn stream_upstream(res: &mut Response, upstream: &mut UpstreamResponse) -> Result<(), String> {
    res.start_sse().map_err(|error| error.to_string())?;
    loop {
        let line = match upstream.read_line() {
            Ok(line) => line,
            Err(error) => {
                logw!("mistral stream read error: {error}");
                break;
            }
        };
        if line.is_empty() {
            if upstream.at_eof() {
                break;
            }
            if res.sse_write(b"\n").is_err() {
                break;
            }
            continue;
        }
        let text = String::from_utf8_lossy(&line);
        let Some(raw) = text.strip_prefix("data:") else {
            if res.sse_write(format!("{text}\n").as_bytes()).is_err() {
                break;
            }
            continue;
        };
        let raw = raw.trim();
        if raw == "[DONE]" {
            let _ = res.sse_write(b"data: [DONE]\n\n");
            break;
        }
        match Value::parse_str(raw) {
            Ok(mut chunk) => {
                normalize_chunk(&mut chunk);
                let frame = format!("data: {}\n\n", chunk.stringify());
                if res.sse_write(frame.as_bytes()).is_err() {
                    break;
                }
            }
            Err(_) => {
                if res.sse_write(format!("{text}\n").as_bytes()).is_err() {
                    break;
                }
            }
        }
    }
    res.sse_end().map_err(|error| error.to_string())
}

fn forward_json(
    res: &mut Response,
    upstream: &mut UpstreamResponse,
    body: &Value,
    model_id: &str,
) -> Result<(), String> {
    let raw = upstream.read_all().unwrap_or_default();
    let mut data = match Value::parse(&raw) {
        Ok(value) if !value.is_null() => value,
        _ => {
            return res
                .send_openai_error(
                    502,
                    "Mistral API returned a non-JSON body",
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };
    if let Some(choices) = data.get_mut("choices").and_then(Value::as_array_mut) {
        for choice in choices.iter_mut() {
            if let Some(message) = choice.get_mut("message") {
                normalize_message(message);
            }
        }
    }
    if data.get("model").is_some() {
        let original = body
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("mistral/{model_id}"));
        data.insert("model", Value::string(original));
    }
    res.send_json(200, &data).map_err(|error| error.to_string())
}

/// `{type:"thinking"}` parts become `reasoning_content`; text parts collapse
/// into one string so OpenAI clients can read it.
fn flatten_parts(parts: &[Value]) -> (String, String) {
    let mut text = String::new();
    let mut reasoning = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("thinking") {
            if let Some(thinking) = part.get("thinking") {
                reasoning.push_str(&text_of(thinking));
            }
        } else if let Some(piece) = part.get("text").and_then(Value::as_str) {
            text.push_str(piece);
        }
    }
    (text, reasoning)
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        Value::Array(items) => items.iter().fold(String::new(), |mut out, item| {
            out.push_str(&text_of(item));
            out
        }),
        Value::Object(_) => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn normalize_message(message: &mut Value) {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    let (text, reasoning) = flatten_parts(parts);
    message.insert("content", Value::string(text));
    if !reasoning.is_empty() {
        message.insert("reasoning_content", Value::string(reasoning));
    }
}

fn normalize_chunk(chunk: &mut Value) {
    chunk.remove("p");
    let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices.iter_mut() {
        let Some(delta) = choice.get_mut("delta") else { continue };
        let Some(parts) = delta.get("content").and_then(Value::as_array) else {
            continue;
        };
        let (text, reasoning) = flatten_parts(parts);
        if text.is_empty() {
            delta.remove("content");
        } else {
            delta.insert("content", Value::string(text));
        }
        if !reasoning.is_empty() {
            delta.insert("reasoning_content", Value::string(reasoning));
        }
    }
}

/// Mistral returns `{object:"error",message}` or Pydantic's `{detail:[...]}`,
/// which clients render as "no body". Rewrite both into OpenAI's shape.
fn normalize_error_body(text: &str) -> Option<Value> {
    let data = Value::parse_str(text).ok()?;
    if !matches!(data, Value::Object(_)) || data.get("error").is_some() {
        return None;
    }
    let detail = data.get("detail").and_then(Value::as_array);
    let mut message = data
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if message.is_empty() {
        if let Some(items) = detail {
            message = detail_message(items);
        }
    }
    if message.is_empty() {
        return None;
    }
    let kind = data
        .get("type")
        .and_then(Value::as_str)
        .filter(|kind| !kind.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if detail.is_some() {
                "invalid_request_error".to_string()
            } else {
                "upstream_error".to_string()
            }
        });
    let mut error = Value::obj();
    error.insert("message", Value::string(message));
    error.insert("type", Value::string(kind));
    error.insert("code", data.get("code").cloned().unwrap_or(Value::Null));
    let mut root = Value::obj();
    root.insert("error", error);
    Some(root)
}

fn detail_message(items: &[Value]) -> String {
    let kept: Vec<&Value> = items
        .iter()
        .filter(|item| {
            !matches!(
                item.get("type").and_then(Value::as_str),
                Some("literal_error") | Some("extra_forbidden")
            )
        })
        .collect();
    let tools: Vec<&Value> = kept
        .iter()
        .copied()
        .filter(|item| {
            item.get("loc")
                .and_then(Value::as_array)
                .map(|loc| loc.iter().any(|part| part.as_str() == Some("Tool")))
                .unwrap_or(false)
        })
        .collect();
    let picked = if tools.is_empty() { &kept } else { &tools };
    picked
        .iter()
        .take(3)
        .filter_map(|item| {
            let loc = item
                .get("loc")
                .and_then(Value::as_array)
                .map(|loc| {
                    let start = loc.len().saturating_sub(3);
                    loc[start..]
                        .iter()
                        .map(joinable)
                        .collect::<Vec<_>>()
                        .join(".")
                })
                .unwrap_or_default();
            let msg = item.get("msg").and_then(Value::as_str).unwrap_or("");
            let line = if loc.is_empty() {
                msg.to_string()
            } else {
                format!("{loc}: {msg}")
            };
            if line.is_empty() {
                None
            } else {
                Some(line)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn joinable(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        Value::Int(number) => number.to_string(),
        Value::Float(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }
}

fn model_object(id: &str, created: i64) -> Value {
    let mut model = Value::obj();
    model.insert("id", Value::string(format!("mistral/{id}")));
    model.insert("object", Value::string("model"));
    model.insert("created", Value::Int(created));
    model.insert("owned_by", Value::string("mistral"));
    model
}

pub fn list_models(config: &Config, client: &HttpClient) -> Vec<Value> {
    if let Some(models) = config.mistral.models.as_ref().filter(|list| !list.is_empty()) {
        let created = crate::util::now_unix() as i64;
        return models
            .iter()
            .filter(|id| !id.is_empty())
            .map(|id| model_object(id, created))
            .collect();
    }
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    let cached = cache.lock().ok().and_then(|guard| guard.clone());
    if let Some((at, list)) = &cached {
        if at.elapsed() < MODEL_TTL {
            return list.clone();
        }
    }
    let stale = cached.map(|(_, list)| list).unwrap_or_default();
    let key = api_key(config);
    if key.is_empty() {
        return Vec::new();
    }
    let url = match Url::parse(&format!(
        "{}/models",
        config.mistral.base_url.trim_end_matches('/')
    )) {
        Ok(url) => url,
        Err(_) => return stale,
    };
    let mut headers = Vec::with_capacity(1);
    headers.push(("authorization".to_string(), format!("Bearer {key}")));
    let mut response = match client.request("GET", &url, &headers, None, Some(MODELS_TIMEOUT)) {
        Ok(response) => response,
        Err(_) => return stale,
    };
    if !(200..300).contains(&response.status) {
        return stale;
    }
    let body = match response.read_all() {
        Ok(body) => body,
        Err(_) => return stale,
    };
    let value = match Value::parse(&body) {
        Ok(value) => value,
        Err(_) => return stale,
    };
    let ids: Vec<&str> = value
        .get("data")
        .and_then(Value::as_array)
        .map(|data| {
            data.iter()
                // /v1/models is platform-wide, not an entitlement list.
                .filter(|model| {
                    model
                        .get("capabilities")
                        .and_then(|caps| caps.get("completion_chat"))
                        .and_then(Value::as_bool)
                        == Some(true)
                })
                .filter_map(|model| model.get("id").and_then(Value::as_str))
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return stale;
    }
    let created = crate::util::now_unix() as i64;
    let list: Vec<Value> = ids
        .into_iter()
        .map(|id| model_object(id, created))
        .collect();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), list.clone()));
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_stripped() {
        assert_eq!(mistral_model_id("mistral/mistral-large-4").unwrap(), "mistral-large-4");
        assert!(mistral_model_id("mistral/").is_none());
        assert!(mistral_model_id("kilo/x").is_none());
    }

    #[test]
    fn request_fills_missing_tool_parameters() {
        let body = Value::parse_str(
            r#"{"model":"x","tools":[
                {"type":"function","function":{"name":"a","parameters":{"type":"object","properties":{"x":{"type":"string"}}}}},
                {"type":"function","function":{"name":"b"}}
            ]}"#,
        )
        .unwrap();
        let payload = normalize_request(&body, "mistral-small");
        assert_eq!(payload.get("model").unwrap().as_str(), Some("mistral-small"));
        let tools = payload.get("tools").unwrap().as_array().unwrap();
        assert!(tools[0]
            .get("function")
            .unwrap()
            .get("parameters")
            .unwrap()
            .get("properties")
            .is_some());
        let empty = tools[1]
            .get("function")
            .unwrap()
            .get("parameters")
            .unwrap();
        assert_eq!(empty.get("type").unwrap().as_str(), Some("object"));
        assert!(empty.get("properties").unwrap().as_object().unwrap().is_empty());
    }

    #[test]
    fn thinking_parts_become_reasoning_content() {
        let mut message = Value::parse_str(
            r#"{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"answer"}]}"#,
        )
        .unwrap();
        normalize_message(&mut message);
        assert_eq!(message.get("content").unwrap().as_str(), Some("answer"));
        assert_eq!(
            message.get("reasoning_content").unwrap().as_str(),
            Some("hmm")
        );

        let mut chunk = Value::parse_str(
            r#"{"p":"x","choices":[{"delta":{"content":[{"type":"thinking","thinking":"t"},{"type":"text","text":"v"}]}}]}"#,
        )
        .unwrap();
        normalize_chunk(&mut chunk);
        assert!(chunk.get("p").is_none());
        let delta = chunk
            .get("choices")
            .unwrap()
            .as_array()
            .unwrap()[0]
            .get("delta")
            .unwrap();
        assert_eq!(delta.get("content").unwrap().as_str(), Some("v"));
        assert_eq!(delta.get("reasoning_content").unwrap().as_str(), Some("t"));
    }

    #[test]
    fn pydantic_errors_are_reshaped() {
        let text = r#"{"detail":[
            {"type":"extra_forbidden","loc":["body","Tool","x"],"msg":"extra"},
            {"type":"missing","loc":["body","Tool","f","parameters"],"msg":"Field required"}
        ]}"#;
        let value = normalize_error_body(text).unwrap();
        let error = value.get("error").unwrap();
        assert_eq!(error.get("type").unwrap().as_str(), Some("invalid_request_error"));
        let message = error.get("message").unwrap().as_str().unwrap();
        assert!(message.contains("parameters"));
        assert!(message.contains("Field required"));
        assert!(!message.contains("extra"));
    }
}
