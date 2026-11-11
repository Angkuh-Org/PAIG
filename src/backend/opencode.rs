//! OpenCode backend: `opencode/*` is the keyless public/free endpoint and
//! `opencode-go/*` uses the stored OpenCode Go credential. Session ids are
//! derived, so a restart loses no mapping; the caller must resend history.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::{HttpClient, Url};
use crate::config::{read_key_file, Config};
use crate::http::Response;
use crate::json::Value;
use crate::sse::Parser as SseParser;
use crate::util::{logw, now_ms, scrub, uuid_from_string};

const GO_PREFIXES: &[&str] = &["opencode-go/"];
const FREE_PREFIXES: &[&str] = &["opencode/", "opencode:", "oc/"];
const PUBLIC_KEY: &str = "public";
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    Free,
    Go,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub model_id: String,
    pub backend: Backend,
}

struct KeyCache {
    key: String,
    at: Option<Instant>,
}

static KEY_CACHE: OnceLock<Mutex<KeyCache>> = OnceLock::new();

fn strip_prefix<'a>(model: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    for prefix in prefixes {
        if let Some(rest) = model.strip_prefix(prefix) {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest);
            }
        }
    }
    None
}

pub fn opencode_model_id(model: &str) -> Option<Target> {
    let raw = model.trim();
    if let Some(model_id) = strip_prefix(raw, GO_PREFIXES) {
        return Some(Target {
            model_id: model_id.to_string(),
            backend: Backend::Go,
        });
    }
    if let Some(model_id) = strip_prefix(raw, FREE_PREFIXES) {
        return Some(Target {
            model_id: model_id.to_string(),
            backend: Backend::Free,
        });
    }
    None
}

fn key_cache() -> &'static Mutex<KeyCache> {
    KEY_CACHE.get_or_init(|| {
        Mutex::new(KeyCache {
            key: String::new(),
            at: None,
        })
    })
}

fn auth_bin(config: &Config) -> Option<PathBuf> {
    if !config.opencode.bin.is_empty() {
        return Some(PathBuf::from(&config.opencode.bin));
    }
    let local = crate::util::home_dir().join(".opencode").join("bin").join("opencode");
    if local.exists() {
        Some(local)
    } else {
        None
    }
}

fn run_auth_export(bin: &Path) -> Option<String> {
    // TODO(paig): single-flight this so concurrent cache misses cannot spawn
    // multiple exports; a grandchild that inherits stdout can also outlive the
    // killed CLI and keep the reader thread alive.
    let mut child = match Command::new(bin)
        .args(["auth", "export"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            logw!("cannot run {} auth export: {error}", bin.display());
            return None;
        }
    };
    let stdout = child.stdout.take()?;
    let overflow = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&overflow);
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut stdout = stdout;
        let mut chunk = [0u8; 8192];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len().saturating_add(n) > AUTH_MAX_BYTES {
                        flag.store(true, Ordering::Relaxed);
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                Err(_) => break,
            }
        }
        let _ = sender.send(buf);
    });

    let deadline = Instant::now() + AUTH_TIMEOUT;
    let mut success = false;
    loop {
        if overflow.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            logw!(
                "{} auth export exceeded {} bytes",
                bin.display(),
                AUTH_MAX_BYTES
            );
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                success = status.success();
                if !success {
                    logw!("{} auth export exited with {status}", bin.display());
                }
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    logw!(
                        "{} auth export timed out after {:?}",
                        bin.display(),
                        AUTH_TIMEOUT
                    );
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                logw!("cannot wait for {} auth export: {error}", bin.display());
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let buf = receiver.recv_timeout(Duration::from_secs(1)).unwrap_or_default();
    if !success || overflow.load(Ordering::Relaxed) || buf.len() > AUTH_MAX_BYTES {
        return None;
    }
    String::from_utf8(buf).ok()
}

fn read_cli_key(config: &Config) -> String {
    let Some(bin) = auth_bin(config) else {
        return String::new();
    };
    let Some(text) = run_auth_export(&bin) else {
        return String::new();
    };
    let Ok(value) = Value::parse_str(&text) else {
        return String::new();
    };
    let Value::Array(entries) = value else {
        return String::new();
    };
    let mut fallback = None;
    for entry in entries {
        if entry.get("integrationID").and_then(Value::as_str) != Some("opencode-go") {
            continue;
        }
        let Some(key) = entry
            .get("value")
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if key.is_empty() {
            continue;
        }
        if entry.get("active").and_then(Value::as_bool) == Some(true) {
            return key.to_string();
        }
        if fallback.is_none() {
            fallback = Some(key.to_string());
        }
    }
    fallback.unwrap_or_default()
}

fn read_key(config: &Config, force: bool) -> String {
    if !config.opencode.api_key.is_empty() {
        return config.opencode.api_key.clone();
    }
    if !config.opencode.api_key_file.is_empty() {
        let key = read_key_file(std::path::Path::new(&config.opencode.api_key_file))
            .trim_start_matches("Bearer ")
            .trim()
            .to_string();
        if !key.is_empty() {
            return key;
        }
    }
    let cache = key_cache();
    if !force {
        if let Ok(guard) = cache.lock() {
            if let Some(at) = guard.at {
                if at.elapsed() < Duration::from_secs(60) {
                    return guard.key.clone();
                }
            }
        }
    }
    let key = read_cli_key(config);
    if let Ok(mut guard) = cache.lock() {
        if key.is_empty() {
            // A failed/timed-out export must not poison the next minute.
            guard.key.clear();
            guard.at = None;
        } else {
            guard.key = key.clone();
            guard.at = Some(Instant::now());
        }
    }
    key
}

fn key_is_cli(config: &Config) -> bool {
    config.opencode.api_key.is_empty() && config.opencode.api_key_file.is_empty()
}

fn text_of(content: &Value) -> String {
    match content {
        Value::Str(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .map(str::to_string)
                    .or_else(|| part.get("text").and_then(Value::as_str).map(str::to_string))
                    .or_else(|| part.get("content").and_then(Value::as_str).map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn session_id(body: &Value) -> String {
    if let Some(user) = body.get("user").and_then(Value::as_str) {
        let user = user.trim();
        if !user.is_empty() {
            return uuid_from_string(&format!("user\0{user}"));
        }
    }
    let messages = body.get("messages").and_then(Value::as_array);
    let mut parts = Vec::new();
    if let Some(messages) = messages {
        if let Some(system) = messages.iter().find(|message| {
            matches!(
                message.get("role").and_then(Value::as_str),
                Some("system" | "developer")
            )
        }) {
            let text = text_of(system.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                parts.push(text);
            }
        }
        if let Some(first_user) = messages
            .iter()
            .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
        {
            let text = text_of(first_user.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                parts.push(text);
            }
        }
    }
    if parts.is_empty() {
        uuid_from_string(&format!("random\0{}", crate::crypto::random_hex(16)))
    } else {
        uuid_from_string(&parts.join("\0"))
    }
}

fn strip_allowlist(list: &[String]) -> Vec<String> {
    list.iter()
        .filter_map(|entry| {
            let raw = entry.trim();
            strip_prefix(raw, GO_PREFIXES)
                .or_else(|| strip_prefix(raw, FREE_PREFIXES))
                .map(str::to_string)
                .or_else(|| {
                    if raw.is_empty() {
                        None
                    } else {
                        Some(raw.to_string())
                    }
                })
        })
        .collect()
}

fn is_stream(body: &Value) -> bool {
    match body.get("stream") {
        Some(Value::Bool(value)) => *value,
        Some(Value::Str(value)) => value == "true",
        Some(Value::Int(value)) => *value == 1,
        _ => false,
    }
}

fn ledger(config: &Config, entry: &Value) {
    let path = std::path::Path::new(&config.ledger_path);
    if let Some(parent) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        let _ = builder.create(parent);
    }
    let mut line = entry.stringify();
    line.push('\n');
    let result = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut file| {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
            file.write_all(line.as_bytes())
        });
    if let Err(error) = result {
        if config.debug {
            logw!("ledger write failed: {error}");
        }
    }
}

struct Observer {
    parser: SseParser,
    finish: String,
    usage: Option<Value>,
}

impl Observer {
    fn new() -> Self {
        Observer {
            parser: SseParser::new(),
            finish: "stream".to_string(),
            usage: None,
        }
    }

    fn feed(&mut self, chunk: &[u8]) {
        for event in self.parser.push(chunk) {
            let Some(value) = crate::sse::data_json(&event.data) else {
                continue;
            };
            if let Some(finish) = value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
                .and_then(|choice| choice.get("finish_reason"))
                .and_then(Value::as_str)
            {
                if !finish.is_empty() {
                    self.finish = finish.to_string();
                }
            }
            if let Some(usage) = value.get("usage") {
                if !usage.is_null() {
                    self.usage = Some(usage.clone());
                }
            }
        }
    }

    fn finish(&mut self) {
        for event in self.parser.finish() {
            if let Some(value) = crate::sse::data_json(&event.data) {
                if let Some(usage) = value.get("usage") {
                    if !usage.is_null() {
                        self.usage = Some(usage.clone());
                    }
                }
            }
        }
    }
}

pub fn handle(
    body: &Value,
    res: &mut Response,
    target: &Target,
    config: &Config,
    client: &HttpClient,
) -> Result<(), String> {
    let go = target.backend == Backend::Go;
    let label = if go { "opencode-go" } else { "opencode" };
    let streaming = is_stream(body);
    let allowlist = if go {
        &config.opencode.go_models
    } else {
        &config.opencode.models
    };
    let enabled = strip_allowlist(allowlist);
    if !enabled.iter().any(|id| id == &target.model_id) {
        return res
            .send_openai_error(
                404,
                &format!(
                    "Model {label}/{} is not enabled; add it to opencode.{}.",
                    target.model_id,
                    if go { "goModels" } else { "models" }
                ),
                "invalid_request_error",
                Some("model_not_found"),
            )
            .map_err(|error| error.to_string());
    }
    // TODO(paig): crouton-alpha-free returns empty text with finish_reason
    // "length" at low completion budgets; retry once with a larger budget.
    // TODO(paig): enforce per-model capabilities before forwarding: crouton
    // accepts only tool_choice "auto"; iceman rejects named tool_choice and
    // response_format.

    let key = if go {
        let key = read_key(config, false);
        if key.is_empty() {
            return res
                .send_openai_error(
                    503,
                    "OpenCode Go requires a credential. Run `opencode auth login`, or set OPENCODE_API_KEY / opencode.apiKey.",
                    "bridge_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
        key
    } else {
        PUBLIC_KEY.to_string()
    };
    let base_url = if go {
        &config.opencode.base_url
    } else {
        &config.opencode.free_base_url
    };
    let url = Url::parse(&format!(
        "{}/chat/completions",
        base_url.trim_end_matches('/')
    ))?;
    let request_id = format!("chatcmpl-{}", crate::crypto::random_hex(12));
    let started = now_ms();
    let original_model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{label}/{}", target.model_id));
    let mut payload = body.clone();
    payload.insert("model", Value::string(&target.model_id));
    if streaming {
        payload.insert("stream", Value::Bool(true));
    }
    let session = session_id(body);

    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("authorization".to_string(), format!("Bearer {key}")),
        ("x-opencode-session".to_string(), session),
    ];
    // TODO(paig): this is a socket/request timeout, not the oracle's total
    // wall-clock deadline with an idle reset per streamed chunk.
    let timeout = Duration::from_millis(config.opencode.timeout_ms.max(1) as u64);
    let body_bytes = payload.stringify();
    let mut upstream = match client.request(
        "POST",
        &url,
        &headers,
        Some(body_bytes.as_bytes()),
        Some(timeout),
    ) {
        Ok(upstream) => upstream,
        Err(error) => {
            let timed_out = matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) || error.to_string().to_ascii_lowercase().contains("timeout");
            let status = if timed_out { 504 } else { 502 };
            let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
            entry.insert("status", Value::Int(status as i64));
            entry.insert("finish", Value::string("error"));
            entry.insert("error", Value::string(scrub(&error.to_string())));
            entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
            ledger(config, &entry);
            return res
                .send_openai_error(
                    status,
                    &if timed_out {
                        format!("{label} request timed out")
                    } else {
                        format!("Cannot reach {label}: {error}")
                    },
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };

    if (upstream.status == 401 || upstream.status == 403) && go && key_is_cli(config) {
        let refreshed = read_key(config, true);
        if !refreshed.is_empty() && refreshed != key {
            let _ = upstream.read_all();
            headers[1].1 = format!("Bearer {refreshed}");
            match client.request(
                "POST",
                &url,
                &headers,
                Some(body_bytes.as_bytes()),
                Some(timeout),
            ) {
                Ok(retry) => upstream = retry,
                Err(error) => {
                    let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
                    entry.insert("status", Value::Int(502));
                    entry.insert("finish", Value::string("error"));
                    entry.insert("error", Value::string(scrub(&error.to_string())));
                    entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
                    ledger(config, &entry);
                    return res
                        .send_openai_error(
                            502,
                            &format!("{label} credential refresh retry failed: {error}"),
                            "upstream_error",
                            None,
                        )
                        .map_err(|error| error.to_string());
                }
            }
        }
    }

    if upstream.status == 204 || upstream.status == 304 {
        return res
            .send_openai_error(
                502,
                "OpenCode returned no body",
                "upstream_error",
                None,
            )
            .map_err(|error| error.to_string());
    }

    if !(200..300).contains(&upstream.status) {
        let body = upstream.read_all().unwrap_or_default();
        let status = if (400..600).contains(&upstream.status) {
            upstream.status
        } else {
            502
        };
        let content_type = upstream
            .header("content-type")
            .unwrap_or("application/json")
            .to_string();
        let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
        entry.insert("status", Value::Int(status as i64));
        entry.insert("finish", Value::string("error"));
        entry.insert(
            "error",
            Value::string(scrub(&crate::util::truncate_utf8(
                &String::from_utf8_lossy(&body),
                200,
            ))),
        );
        entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
        ledger(config, &entry);
        return res
            .send_bytes(status, &content_type, &body)
            .map_err(|error| error.to_string());
    }

    if streaming {
        // TODO(paig): streamed chunks keep the upstream bare model id; rewrite
        // it to `{label}/{model_id}` like the non-stream response.
        // TODO(paig): iceman-contributor-free may emit streaming tool_calls
        // without `index`; normalize per-call indices if clients need them.
        res.start_sse().map_err(|error| error.to_string())?;
        let mut observer = Observer::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match upstream.read_chunk(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    observer.feed(&buf[..n]);
                    if res.sse_write(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
                    entry.insert("status", Value::Int(200));
                    entry.insert("finish", Value::string("stream-error"));
                    entry.insert("error", Value::string(scrub(&error.to_string())));
                    entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
                    ledger(config, &entry);
                    let mut error_value = Value::obj();
                    error_value.insert(
                        "message",
                        Value::string(format!("{label} stream error: {error}")),
                    );
                    error_value.insert("type", Value::string("upstream_error"));
                    let mut root = Value::obj();
                    root.insert("error", error_value);
                    let frame = format!("data: {}\n\n", root.stringify());
                    let _ = res.sse_write(frame.as_bytes());
                    let _ = res.sse_end();
                    return Ok(());
                }
            }
        }
        observer.finish();
        let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
        entry.insert("status", Value::Int(200));
        entry.insert("finish", Value::string(&observer.finish));
        if let Some(usage) = observer.usage {
            entry.insert("usage", usage);
        }
        entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
        ledger(config, &entry);
        return res.sse_end().map_err(|error| error.to_string());
    }

    let body = match upstream.read_all() {
        Ok(body) => body,
        Err(error) => {
            return res
                .send_openai_error(
                    502,
                    &format!("{label} response failed: {error}"),
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };
    let mut value = match Value::parse(&body) {
        Ok(value) => value,
        Err(_) => {
            let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
            entry.insert("status", Value::Int(502));
            entry.insert("finish", Value::string("error"));
            entry.insert("error", Value::string(format!("{label} returned a non-JSON body")));
            entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
            ledger(config, &entry);
            return res
                .send_openai_error(
                    502,
                    &format!("{label} returned a non-JSON body"),
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };
    if value.get("model").is_some() {
        value.insert("model", Value::string(original_model));
    }
    let finish = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop")
        .to_string();
    let mut entry = base_ledger(&request_id, label, &target.model_id, streaming, go);
    entry.insert("status", Value::Int(200));
    entry.insert("finish", Value::string(finish));
    if let Some(usage) = value.get("usage") {
        if !usage.is_null() {
            entry.insert("usage", usage.clone());
        }
    }
    entry.insert("duration_ms", Value::Int(now_ms().saturating_sub(started) as i64));
    ledger(config, &entry);
    res.send_json(200, &value).map_err(|error| error.to_string())
}

fn base_ledger(
    request_id: &str,
    label: &str,
    model_id: &str,
    stream: bool,
    go: bool,
) -> Value {
    let mut entry = Value::obj();
    entry.insert("ts", Value::string(crate::util::utc_iso()));
    entry.insert("request_id", Value::string(request_id));
    entry.insert("model", Value::string(format!("{label}/{model_id}")));
    entry.insert("stream", Value::Bool(stream));
    entry.insert(
        "credential",
        Value::string(if go { "key" } else { "public" }),
    );
    entry
}

pub fn list_models(config: &Config) -> Vec<Value> {
    let created = crate::util::now_unix() as i64;
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in strip_allowlist(&config.opencode.models) {
        let full = format!("opencode/{id}");
        if seen.insert(full.clone()) {
            let mut model = Value::obj();
            model.insert("id", Value::string(full));
            model.insert("object", Value::string("model"));
            model.insert("created", Value::Int(created));
            model.insert("owned_by", Value::string("opencode"));
            out.push(model);
        }
    }
    for id in strip_allowlist(&config.opencode.go_models) {
        let full = format!("opencode-go/{id}");
        if seen.insert(full.clone()) {
            let mut model = Value::obj();
            model.insert("id", Value::string(full));
            model.insert("object", Value::string("model"));
            model.insert("created", Value::Int(created));
            model.insert("owned_by", Value::string("opencode-go"));
            out.push(model);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_route_to_the_right_backend() {
        assert_eq!(
            opencode_model_id("opencode/beam-free").unwrap().backend,
            Backend::Free
        );
        assert_eq!(
            opencode_model_id("opencode:beam-free").unwrap().backend,
            Backend::Free
        );
        assert_eq!(
            opencode_model_id("oc/beam-free").unwrap().backend,
            Backend::Free
        );
        let go = opencode_model_id("opencode-go/iceman-contributor-free").unwrap();
        assert_eq!(go.backend, Backend::Go);
        assert_eq!(go.model_id, "iceman-contributor-free");
        assert!(opencode_model_id("opencode/").is_none());
        assert!(opencode_model_id("other/model").is_none());
    }

    #[test]
    fn session_id_is_stable_for_user() {
        let mut a = Value::obj();
        a.insert("user", Value::string("u1"));
        let mut b = a.clone();
        let c = {
            let mut c = Value::obj();
            c.insert("user", Value::string("u2"));
            c
        };
        assert_eq!(session_id(&a), session_id(&b));
        assert_ne!(session_id(&a), session_id(&c));
        b.insert("user", Value::string("u3"));
        assert_ne!(session_id(&a), session_id(&b));
    }

    #[test]
    fn allowlist_strips_any_prefix() {
        let list = vec![
            "opencode/beam-free".to_string(),
            "opencode-go/iceman-contributor-free".to_string(),
            "plain".to_string(),
        ];
        let stripped = strip_allowlist(&list);
        assert_eq!(stripped, vec!["beam-free", "iceman-contributor-free", "plain"]);
    }
}
