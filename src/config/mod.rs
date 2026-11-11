//! Layered configuration: defaults < config.json files < .env files < process
//! environment. Mirrors the JS loader, including its deliberate quirks.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::json::Value;
use crate::util;

pub const DEFAULT_HEADERS: &[(&str, &str)] = &[
    ("user-agent", "cli"),
    ("x-command-code-version", "1.88.0"),
    ("x-cli-environment", "production"),
    ("x-taste-learning", "true"),
];

pub const DEFAULT_OPENCODE_MODELS: &[&str] = &[
    "beam-free",
    "big-pickle",
    "crouton-alpha-free",
    "deepseek-v4.1-flash-free",
    "flint-40b-free",
    "glm-5.3-flash-free",
    "mimo-v2.6-flash-free",
    "nemotron-3.5-lightning-free",
    "nemotron-3.5-super-vl-free",
    "pulse-alpha-free",
];

pub const DEFAULT_OPENCODE_GO_MODELS: &[&str] = &["beam-free", "crouton-alpha-free", "iceman-contributor-free"];

/// Models probed 2026-10-28 that this account cannot use (403
/// MODEL_NOT_IN_PLAN) or that are dead upstream (404/502). Override the whole
/// list with commandCode.excludedModels; an empty list disables the filter.
pub const DEFAULT_EXCLUDED_MODELS: &[&str] = &[
    "Qwen/Qwen3.8-Max-Prime",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-haiku-4-5-20251001",
    "claude-haiku-5-5",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "google/gemini-3.1-flash-lite",
    "google/gemini-3.5-flash",
    "google/gemini-3.5-flash-lite",
    "google/gemini-3.6-flash",
    "google/gemini-3.7-flash",
    "google/gemini-3.8-flash",
    "google/gemini-4-argon",
    "gpt-5.3-codex",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-6-astra",
    "gpt-6-sol",
    "gpt-6.1-sol",
    "gpt-6.1-terra",
    "meta/muse-spark-1.1",
    "meta/muse-spark-1.2",
    "meta/muse-spark-1.3",
    "meta/muse-wonder-1.0",
    "sakana/fugu-ultra",
    "xai/grok-4.6",
    "xai/grok-4.7",
    "xiaomi/mimo-v2.6-pro-ultraspeed",
    "opencode/jev-1.13-free",
    "kilo/nvidia/nemotron-3-super-120b-a12b:free",
];

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct CommandCodeConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_key_file: String,
    pub project_slug: String,
    pub models: Option<Vec<String>>,
    pub excluded_models: Option<Vec<String>>,
    pub fetch_retries: i64,
    pub fetch_backoff_ms: i64,
    pub headers: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone)]
pub struct OpencodeConfig {
    pub base_url: String,
    pub free_base_url: String,
    pub api_key: String,
    pub api_key_file: String,
    pub bin: String,
    pub models: Vec<String>,
    pub go_models: Vec<String>,
    pub timeout_ms: i64,
}

#[derive(Debug, Clone)]
pub struct KiloConfig {
    pub base_url: String,
    pub api_key: String,
    pub all_models: bool,
}

#[derive(Debug, Clone)]
pub struct BobConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_key_file: String,
    pub team_id: String,
    pub user_agent: String,
}

#[derive(Debug, Clone)]
pub struct MistralConfig {
    pub base_url: String,
    pub api_key: String,
    pub api_key_file: String,
    pub models: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub server: ServerConfig,
    pub debug: bool,
    pub advisor_max_bytes: i64,
    pub ledger_path: String,
    pub command_code: CommandCodeConfig,
    pub opencode: OpencodeConfig,
    pub kilo: KiloConfig,
    pub bob: BobConfig,
    pub mistral: MistralConfig,
}

#[derive(Debug, Clone)]
pub struct ConfigSource {
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub sources: Vec<ConfigSource>,
    pub warnings: Vec<String>,
    pub key_source: String,
}

impl Config {
    pub fn defaults() -> Config {
        let ledger_path = util::paig_home().join("paig-ledger.jsonl");
        Config {
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: crate::rules::DEFAULT_PORT,
            },
            debug: false,
            // TODO(paig): diagnose z-ai/glm-5.3-flashx failing around 1.9 MiB;
            // 1 MB is a safety margin, not a measured limit.
            advisor_max_bytes: 1_000_000,
            ledger_path: ledger_path.to_string_lossy().to_string(),
            command_code: CommandCodeConfig {
                base_url: "https://api.commandcode.ai".to_string(),
                api_key: String::new(),
                api_key_file: String::new(),
                project_slug: String::new(),
                models: None,
                excluded_models: None,
                fetch_retries: 2,
                fetch_backoff_ms: 500,
                headers: Some(
                    DEFAULT_HEADERS
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                ),
            },
            opencode: OpencodeConfig {
                base_url: "https://opencode.ai/inference/go/openai/v1".to_string(),
                free_base_url: "https://opencode.ai/inference/openai/v1".to_string(),
                api_key: String::new(),
                api_key_file: String::new(),
                bin: String::new(),
                models: DEFAULT_OPENCODE_MODELS.iter().map(|s| s.to_string()).collect(),
                go_models: DEFAULT_OPENCODE_GO_MODELS.iter().map(|s| s.to_string()).collect(),
                timeout_ms: 180_000,
            },
            kilo: KiloConfig {
                base_url: "https://ai-gateway.kilo.ai/api/v1".to_string(),
                api_key: String::new(),
                all_models: false,
            },
            bob: BobConfig {
                base_url: "https://api.us-east.bob.ibm.com/inference/v1".to_string(),
                api_key: String::new(),
                api_key_file: String::new(),
                team_id: String::new(),
                user_agent: "bob-shell/2.0.9".to_string(),
            },
            mistral: MistralConfig {
                base_url: "https://api.mistral.ai/v1".to_string(),
                api_key: String::new(),
                api_key_file: String::new(),
                models: None,
            },
        }
    }

    pub fn to_value(&self) -> Value {
        let mut root = Value::obj();
        let mut server = Value::obj();
        server.insert("host", Value::string(&self.server.host));
        server.insert("port", Value::Int(self.server.port as i64));
        root.insert("server", server);

        root.insert("debug", Value::Bool(self.debug));
        root.insert("advisorMaxBytes", Value::Int(self.advisor_max_bytes));
        root.insert("ledgerPath", Value::string(&self.ledger_path));

        let mut cc = Value::obj();
        cc.insert("baseUrl", Value::string(&self.command_code.base_url));
        cc.insert("apiKey", Value::string(&self.command_code.api_key));
        cc.insert("apiKeyFile", Value::string(&self.command_code.api_key_file));
        cc.insert("projectSlug", Value::string(&self.command_code.project_slug));
        cc.insert(
            "models",
            option_list_to_value(&self.command_code.models),
        );
        cc.insert(
            "excludedModels",
            option_list_to_value(&self.command_code.excluded_models),
        );
        cc.insert("fetchRetries", Value::Int(self.command_code.fetch_retries));
        cc.insert(
            "fetchBackoffMs",
            Value::Int(self.command_code.fetch_backoff_ms),
        );
        match &self.command_code.headers {
            None => cc.insert("headers", Value::Null),
            Some(headers) => {
                let mut obj = Value::obj();
                for (key, value) in headers {
                    obj.insert(key.clone(), Value::string(value));
                }
                cc.insert("headers", obj);
            }
        }
        root.insert("commandCode", cc);

        let mut oc = Value::obj();
        oc.insert("baseUrl", Value::string(&self.opencode.base_url));
        oc.insert("freeBaseUrl", Value::string(&self.opencode.free_base_url));
        oc.insert("apiKey", Value::string(&self.opencode.api_key));
        oc.insert("apiKeyFile", Value::string(&self.opencode.api_key_file));
        oc.insert("bin", Value::string(&self.opencode.bin));
        oc.insert("models", list_to_value(&self.opencode.models));
        oc.insert("goModels", list_to_value(&self.opencode.go_models));
        oc.insert("timeoutMs", Value::Int(self.opencode.timeout_ms));
        root.insert("opencode", oc);

        let mut kilo = Value::obj();
        kilo.insert("baseUrl", Value::string(&self.kilo.base_url));
        kilo.insert("apiKey", Value::string(&self.kilo.api_key));
        kilo.insert("allModels", Value::Bool(self.kilo.all_models));
        root.insert("kilo", kilo);

        let mut bob = Value::obj();
        bob.insert("baseUrl", Value::string(&self.bob.base_url));
        bob.insert("apiKey", Value::string(&self.bob.api_key));
        bob.insert("apiKeyFile", Value::string(&self.bob.api_key_file));
        bob.insert("teamId", Value::string(&self.bob.team_id));
        bob.insert("userAgent", Value::string(&self.bob.user_agent));
        root.insert("bob", bob);

        let mut mistral = Value::obj();
        mistral.insert("baseUrl", Value::string(&self.mistral.base_url));
        mistral.insert("apiKey", Value::string(&self.mistral.api_key));
        mistral.insert("apiKeyFile", Value::string(&self.mistral.api_key_file));
        mistral.insert("models", option_list_to_value(&self.mistral.models));
        root.insert("mistral", mistral);

        root
    }
}

fn list_to_value(list: &[String]) -> Value {
    Value::Array(list.iter().map(|item| Value::string(item)).collect())
}

fn option_list_to_value(list: &Option<Vec<String>>) -> Value {
    match list {
        Some(list) => list_to_value(list),
        None => Value::Null,
    }
}

struct EnvSpec {
    name: &'static str,
    dotted: &'static str,
    kind: &'static str,
}

const ENV_MAP: &[EnvSpec] = &[
    EnvSpec { name: "PAIG_HOST", dotted: "server.host", kind: "string" },
    EnvSpec { name: "PAIG_PORT", dotted: "server.port", kind: "integer" },
    EnvSpec { name: "PAIG_DEBUG", dotted: "debug", kind: "boolean" },
    EnvSpec { name: "PAIG_ADVISOR_MAX_BYTES", dotted: "advisorMaxBytes", kind: "integer" },
    EnvSpec { name: "PAIG_LEDGER", dotted: "ledgerPath", kind: "string" },
    EnvSpec { name: "CC_API_KEY", dotted: "commandCode.apiKey", kind: "string" },
    EnvSpec { name: "PAIG_CC_API_KEY", dotted: "commandCode.apiKey", kind: "string" },
    EnvSpec { name: "CC_AUTH_FILE", dotted: "commandCode.apiKeyFile", kind: "string" },
    EnvSpec { name: "CC_API_KEY_FILE", dotted: "commandCode.apiKeyFile", kind: "string" },
    EnvSpec { name: "CC_PROJECT_SLUG", dotted: "commandCode.projectSlug", kind: "string" },
    EnvSpec { name: "CC_MODELS", dotted: "commandCode.models", kind: "list" },
    EnvSpec { name: "PAIG_MODELS", dotted: "commandCode.models", kind: "list" },
    EnvSpec { name: "PAIG_EXCLUDED_MODELS", dotted: "commandCode.excludedModels", kind: "list" },
    EnvSpec { name: "CC_FETCH_RETRIES", dotted: "commandCode.fetchRetries", kind: "integer" },
    EnvSpec { name: "CC_FETCH_BACKOFF_MS", dotted: "commandCode.fetchBackoffMs", kind: "integer" },
    EnvSpec { name: "CC_USER_AGENT", dotted: "commandCode.headers.user-agent", kind: "string" },
    EnvSpec { name: "CC_CLI_VERSION", dotted: "commandCode.headers.x-command-code-version", kind: "string" },
    EnvSpec { name: "CC_CLI_ENVIRONMENT", dotted: "commandCode.headers.x-cli-environment", kind: "string" },
    EnvSpec { name: "CC_TASTE_LEARNING", dotted: "commandCode.headers.x-taste-learning", kind: "string" },
    EnvSpec { name: "OPENCODE_API_KEY", dotted: "opencode.apiKey", kind: "string" },
    EnvSpec { name: "OPENCODE_API_KEY_FILE", dotted: "opencode.apiKeyFile", kind: "string" },
    EnvSpec { name: "OPENCODE_BIN", dotted: "opencode.bin", kind: "string" },
    EnvSpec { name: "OPENCODE_MODELS", dotted: "opencode.models", kind: "list" },
    EnvSpec { name: "OPENCODE_GO_MODELS", dotted: "opencode.goModels", kind: "list" },
    EnvSpec { name: "OPENCODE_TIMEOUT_MS", dotted: "opencode.timeoutMs", kind: "integer" },
    EnvSpec { name: "KILO_API_KEY", dotted: "kilo.apiKey", kind: "string" },
    EnvSpec { name: "KILO_ALL_MODELS", dotted: "kilo.allModels", kind: "boolean" },
    EnvSpec { name: "PAIG_BOB_API_KEY", dotted: "bob.apiKey", kind: "string" },
    EnvSpec { name: "PAIG_BOB_API_KEY_FILE", dotted: "bob.apiKeyFile", kind: "string" },
    EnvSpec { name: "PAIG_BOB_TEAM_ID", dotted: "bob.teamId", kind: "string" },
    EnvSpec { name: "PAIG_BOB_USER_AGENT", dotted: "bob.userAgent", kind: "string" },
    EnvSpec { name: "PAIG_MISTRAL_API_KEY", dotted: "mistral.apiKey", kind: "string" },
    EnvSpec { name: "PAIG_MISTRAL_API_KEY_FILE", dotted: "mistral.apiKeyFile", kind: "string" },
    EnvSpec { name: "PAIG_MISTRAL_MODELS", dotted: "mistral.models", kind: "list" },
];

fn is_plain_object(value: &Value) -> bool {
    matches!(value, Value::Object(_))
}

fn deep_merge(target: &mut Value, source: &Value) {
    if let (Value::Object(target_items), Value::Object(source_items)) = (&mut *target, source) {
        for (key, value) in source_items {
            if matches!(key.as_str(), "__proto__" | "constructor" | "prototype") {
                continue;
            }
            if let Some((_, slot)) = target_items.iter_mut().find(|(k, _)| k == key) {
                if is_plain_object(slot) && is_plain_object(value) {
                    deep_merge(slot, value);
                } else {
                    *slot = value.clone();
                }
            } else {
                target_items.push((key.clone(), value.clone()));
            }
        }
        return;
    }
    *target = source.clone();
}

fn merge_missing(target: &mut Value, source: &Value) {
    if let (Value::Object(target_items), Value::Object(source_items)) = (&mut *target, source) {
        for (key, value) in source_items {
            if let Some((_, slot)) = target_items.iter_mut().find(|(existing, _)| existing == key) {
                merge_missing(slot, value);
            } else {
                target_items.push((key.clone(), value.clone()));
            }
        }
    }
}

fn set_path(target: &mut Value, dotted: &str, value: Value) {
    let parts: Vec<&str> = dotted.split('.').collect();
    let mut cursor = target;
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !is_plain_object(cursor) {
            return;
        }
        if cursor.get(part).is_none() {
            cursor.insert(*part, Value::obj());
        }
        let Some(next) = cursor.get_mut(part) else { return };
        cursor = next;
    }
    if let Some(last) = parts.last() {
        if is_plain_object(cursor) {
            cursor.insert(*last, value);
        }
    }
}

fn get_path<'a>(target: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut cursor = target;
    for part in dotted.split('.') {
        cursor = cursor.get(part)?;
    }
    Some(cursor)
}

fn convert_env(value: &str, kind: &str) -> Option<Value> {
    match kind {
        "integer" => {
            if value.trim().is_empty() {
                None
            } else {
                value.trim().parse::<i64>().ok().map(Value::Int)
            }
        }
        "boolean" => Some(Value::Bool(util::parse_bool(Some(value), false))),
        "list" => Some(Value::Array(
            util::split_list(value).into_iter().map(Value::Str).collect(),
        )),
        _ => Some(Value::Str(value.to_string())),
    }
}

fn apply_env(config: &mut Value, env: &HashMap<String, String>) {
    for spec in ENV_MAP {
        let Some(raw) = env.get(spec.name) else { continue };
        let Some(value) = convert_env(raw, spec.kind) else { continue };
        // An empty template line must not erase a value from config.json.
        if spec.kind == "string" {
            if let Value::Str(text) = &value {
                if text.is_empty() {
                    continue;
                }
            }
        }
        set_path(config, spec.dotted, value);
    }
}

fn normalize_list(value: Option<&Value>, label: &str, warnings: &mut Vec<String>) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::Str(text)) => Some(util::split_list(text)),
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                if let Some(text) = item.as_str() {
                    let text = text.trim();
                    if !text.is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
            Some(out)
        }
        Some(_) => {
            warnings.push(format!("{label} must be an array or a comma-separated string"));
            None
        }
    }
}

fn string_at(config: &Value, path: &str, fallback: &str) -> String {
    get_path(config, path)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn string_exact_at(config: &Value, path: &str, fallback: &str) -> String {
    match get_path(config, path).and_then(Value::as_str) {
        Some(value) => value.trim().to_string(),
        None => fallback.to_string(),
    }
}

fn int_at(config: &Value, path: &str, fallback: i64) -> i64 {
    get_path(config, path)
        .and_then(Value::as_i64)
        .unwrap_or(fallback)
}

fn bool_at(config: &Value, path: &str, fallback: bool) -> bool {
    get_path(config, path)
        .and_then(Value::as_bool)
        .unwrap_or(fallback)
}

fn normalize_headers(
    config: &Value,
    base: &[(String, String)],
    warnings: &mut Vec<String>,
) -> Option<Vec<(String, String)>> {
    match get_path(config, "commandCode.headers") {
        Some(Value::Null) => None,
        None => Some(base.to_vec()),
        Some(Value::Object(items)) => {
            let mut merged = base.to_vec();
            for (name, value) in items {
                let key = name.to_ascii_lowercase();
                let text = match value {
                    Value::Null => {
                        merged.retain(|(existing, _)| existing != &key);
                        continue;
                    }
                    Value::Str(text) if text.is_empty() => {
                        merged.retain(|(existing, _)| existing != &key);
                        continue;
                    }
                    Value::Str(text) => text.clone(),
                    Value::Int(number) => number.to_string(),
                    Value::Float(number) if number.is_finite() => number.to_string(),
                    Value::Bool(flag) => flag.to_string(),
                    _ => continue,
                };
                if let Some((_, slot)) = merged.iter_mut().find(|(existing, _)| existing == &key) {
                    *slot = text;
                } else {
                    merged.push((key, text));
                }
            }
            Some(merged)
        }
        Some(_) => {
            warnings.push("commandCode.headers must be an object or null; using defaults".to_string());
            Some(base.to_vec())
        }
    }
}

fn normalize(config: &mut Value, cwd: &Path, base: &Config, warnings: &mut Vec<String>) -> Config {
    let mut out = base.clone();

    out.server.host = string_exact_at(config, "server.host", "127.0.0.1");
    let port = int_at(config, "server.port", crate::rules::DEFAULT_PORT as i64);
    out.server.port = if (1..=65535).contains(&port) {
        port as u16
    } else {
        0
    };
    out.debug = bool_at(config, "debug", false);
    out.advisor_max_bytes = int_at(config, "advisorMaxBytes", 1_000_000);
    let ledger = string_at(config, "ledgerPath", "");
    out.ledger_path = if ledger.is_empty() {
        util::paig_home()
            .join("paig-ledger.jsonl")
            .to_string_lossy()
            .to_string()
    } else {
        util::resolve_path(cwd, &ledger).to_string_lossy().to_string()
    };

    out.command_code.base_url = string_at(config, "commandCode.baseUrl", "https://api.commandcode.ai")
        .trim_end_matches('/')
        .to_string();
    out.command_code.api_key = string_at(config, "commandCode.apiKey", "");
    let key_file = string_at(config, "commandCode.apiKeyFile", "");
    out.command_code.api_key_file = if key_file.is_empty() {
        String::new()
    } else {
        util::resolve_path(cwd, &key_file).to_string_lossy().to_string()
    };
    out.command_code.project_slug = string_at(config, "commandCode.projectSlug", "");
    out.command_code.models = normalize_list(
        get_path(config, "commandCode.models"),
        "commandCode.models",
        warnings,
    );
    out.command_code.excluded_models = normalize_list(
        get_path(config, "commandCode.excludedModels"),
        "commandCode.excludedModels",
        warnings,
    );
    out.command_code.fetch_retries = int_at(config, "commandCode.fetchRetries", 2);
    out.command_code.fetch_backoff_ms = int_at(config, "commandCode.fetchBackoffMs", 500);
    out.command_code.headers = normalize_headers(
        config,
        &base.command_code.headers.clone().unwrap_or_default(),
        warnings,
    );

    out.opencode.base_url = string_at(
        config,
        "opencode.baseUrl",
        "https://opencode.ai/inference/go/openai/v1",
    )
    .trim_end_matches('/')
    .to_string();
    out.opencode.free_base_url = string_at(
        config,
        "opencode.freeBaseUrl",
        "https://opencode.ai/inference/openai/v1",
    )
    .trim_end_matches('/')
    .to_string();
    out.opencode.api_key = string_at(config, "opencode.apiKey", "");
    let oc_key_file = string_at(config, "opencode.apiKeyFile", "");
    out.opencode.api_key_file = if oc_key_file.is_empty() {
        String::new()
    } else {
        util::resolve_path(cwd, &oc_key_file).to_string_lossy().to_string()
    };
    out.opencode.bin = string_at(config, "opencode.bin", "");
    if out.opencode.bin.starts_with('~') {
        out.opencode.bin = util::expand_home(&out.opencode.bin).to_string_lossy().to_string();
    }
    let models = normalize_list(get_path(config, "opencode.models"), "opencode.models", warnings);
    out.opencode.models = models
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| DEFAULT_OPENCODE_MODELS.iter().map(|s| s.to_string()).collect());
    let go_models = normalize_list(
        get_path(config, "opencode.goModels"),
        "opencode.goModels",
        warnings,
    );
    out.opencode.go_models = go_models
        .filter(|list| !list.is_empty())
        .unwrap_or_else(|| DEFAULT_OPENCODE_GO_MODELS.iter().map(|s| s.to_string()).collect());
    out.opencode.timeout_ms = int_at(config, "opencode.timeoutMs", 180_000);

    out.kilo.base_url = string_at(config, "kilo.baseUrl", "https://ai-gateway.kilo.ai/api/v1")
        .trim_end_matches('/')
        .to_string();
    out.kilo.api_key = string_at(config, "kilo.apiKey", "");
    out.kilo.all_models = bool_at(config, "kilo.allModels", false);

    out.bob.base_url = string_at(
        config,
        "bob.baseUrl",
        "https://api.us-east.bob.ibm.com/inference/v1",
    )
    .trim_end_matches('/')
    .to_string();
    out.bob.api_key = string_at(config, "bob.apiKey", "");
    let bob_file = string_at(config, "bob.apiKeyFile", "");
    out.bob.api_key_file = if bob_file.is_empty() {
        String::new()
    } else {
        util::resolve_path(cwd, &bob_file).to_string_lossy().to_string()
    };
    out.bob.team_id = string_at(config, "bob.teamId", "");
    out.bob.user_agent = string_at(config, "bob.userAgent", "bob-shell/2.0.9");

    out.mistral.base_url = string_at(config, "mistral.baseUrl", "https://api.mistral.ai/v1")
        .trim_end_matches('/')
        .to_string();
    out.mistral.api_key = string_at(config, "mistral.apiKey", "");
    let mistral_file = string_at(config, "mistral.apiKeyFile", "");
    out.mistral.api_key_file = if mistral_file.is_empty() {
        String::new()
    } else {
        util::resolve_path(cwd, &mistral_file).to_string_lossy().to_string()
    };
    out.mistral.models = normalize_list(
        get_path(config, "mistral.models"),
        "mistral.models",
        warnings,
    );

    out
}

fn read_json_file(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Cannot read config file {}: {error}", path.display()))?;
    let value = Value::parse_str(&text)
        .map_err(|error| format!("Invalid JSON in {}: {error}", path.display()))?;
    if !is_plain_object(&value) {
        return Err(format!("Invalid JSON in {}: expected a JSON object", path.display()));
    }
    Ok(value)
}

pub fn read_key_file(path: &Path) -> String {
    let Ok(text) = fs::read_to_string(path) else {
        return String::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    if text.starts_with('{') {
        if let Ok(value) = Value::parse_str(text) {
            for key in ["apiKey", "api_key", "key", "token"] {
                if let Some(found) = value.get(key).and_then(Value::as_str) {
                    let found = found.trim();
                    if !found.is_empty() {
                        return found.to_string();
                    }
                }
            }
        }
        return String::new();
    }
    if text.starts_with('"') {
        if let Ok(Value::Str(found)) = Value::parse_str(text) {
            return found.trim().to_string();
        }
        return String::new();
    }
    if text
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '{' | '}' | '"' | '\''))
    {
        return String::new();
    }
    text.to_string()
}

fn parse_dotenv_value(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut escaped = false;
        for ch in rest.chars() {
            if escaped {
                out.push(match ch {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => ch,
                });
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                return out;
            } else {
                out.push(ch);
            }
        }
        return out;
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        if let Some(end) = rest.find('\'') {
            return rest[..end].to_string();
        }
        return rest.to_string();
    }
    if let Some(pos) = raw.find(" #") {
        raw[..pos].trim().to_string()
    } else {
        raw.trim().to_string()
    }
}

fn parse_dotenv(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for raw_line in text.split(['\n', '\r']) {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((name, value)) = line.split_once('=') else { continue };
        let name = name.trim();
        if name.is_empty()
            || !name
                .chars()
                .enumerate()
                .all(|(index, ch)| ch == '_' || ch.is_ascii_alphanumeric() && (index > 0 || ch.is_ascii_alphabetic()))
        {
            continue;
        }
        out.insert(name.to_string(), parse_dotenv_value(value.trim()));
    }
    out
}

fn existing_files(files: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for file in files {
        if !file.exists() {
            continue;
        }
        let key = fs::canonicalize(&file).unwrap_or_else(|_| file.clone());
        if seen.insert(key) {
            out.push(file);
        }
    }
    out
}

fn resolve_key(config: &mut Config, warnings: &mut Vec<String>) -> String {
    if !config.command_code.api_key.is_empty() && !config.command_code.api_key_file.is_empty() {
        warnings.push("both commandCode.apiKey and commandCode.apiKeyFile are set; apiKey wins".to_string());
        return "key".to_string();
    }
    if config.command_code.api_key_file.is_empty() {
        return if config.command_code.api_key.is_empty() {
            "none".to_string()
        } else {
            "key".to_string()
        };
    }
    let path = PathBuf::from(&config.command_code.api_key_file);
    if !path.exists() {
        warnings.push(format!("commandCode.apiKeyFile does not exist: {}", path.display()));
        return if config.command_code.api_key.is_empty() {
            "none".to_string()
        } else {
            "key".to_string()
        };
    }
    let key = read_key_file(&path);
    if key.is_empty() {
        warnings.push(format!("commandCode.apiKeyFile contains no apiKey: {}", path.display()));
    } else {
        config.command_code.api_key = key;
        return "file".to_string();
    }
    if config.command_code.api_key.is_empty() {
        "none".to_string()
    } else {
        "key".to_string()
    }
}

pub fn load_config() -> Result<LoadedConfig, String> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let config_dir = util::paig_home();
    let base = Config::defaults();
    let mut root = base.to_value();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();

    let explicit_config = std::env::var("PAIG_CONFIG").ok().filter(|v| !v.is_empty());
    let config_files = if let Some(path) = explicit_config {
        let path = util::resolve_path(&cwd, &path);
        if !path.exists() {
            return Err(format!("PAIG_CONFIG does not exist: {}", path.display()));
        }
        vec![path]
    } else {
        existing_files(vec![
            config_dir.join("config.json"),
            cwd.join("paig.config.json"),
        ])
    };

    let explicit_env = std::env::var("PAIG_ENV_FILE").ok().filter(|v| !v.is_empty());
    let env_files = if let Some(path) = explicit_env {
        let path = util::resolve_path(&cwd, &path);
        if !path.exists() {
            return Err(format!("PAIG_ENV_FILE does not exist: {}", path.display()));
        }
        vec![path]
    } else {
        existing_files(vec![config_dir.join(".env"), cwd.join(".env")])
    };

    for file in &config_files {
        let data = read_json_file(file)?;
        if let Some(Value::Object(headers)) = data.get("commandCode").and_then(|v| v.get("headers")) {
            if headers.is_empty() {
                warnings.push(format!(
                    "{}: commandCode.headers is an empty object and merges with defaults; use null to disable the identity headers",
                    file.display()
                ));
            }
        }
        deep_merge(&mut root, &data);
        sources.push(ConfigSource {
            path: file.display().to_string(),
        });
    }

    let mut file_env = HashMap::new();
    for file in &env_files {
        if let Ok(text) = fs::read_to_string(file) {
            for (key, value) in parse_dotenv(&text) {
                file_env.insert(key, value);
            }
            sources.push(ConfigSource {
                path: file.display().to_string(),
            });
        }
    }
    for (key, value) in std::env::vars() {
        file_env.insert(key, value);
    }
    apply_env(&mut root, &file_env);

    let mut config = normalize(&mut root, &cwd, &base, &mut warnings);
    for file in &config_files {
        if let Some(mode) = util::file_mode(file) {
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "config {} is mode {mode:o}; chmod 600 is recommended",
                    file.display()
                ));
            }
        }
    }
    for file in &env_files {
        if let Some(mode) = util::file_mode(file) {
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "env {} is mode {mode:o}; chmod 600 is recommended",
                    file.display()
                ));
            }
        }
    }
    for (label, path) in [
        ("bob.apiKeyFile", &config.bob.api_key_file),
        ("mistral.apiKeyFile", &config.mistral.api_key_file),
        ("opencode.apiKeyFile", &config.opencode.api_key_file),
    ] {
        if path.is_empty() {
            continue;
        }
        let path = PathBuf::from(path);
        if !path.exists() {
            warnings.push(format!("{label} does not exist: {}", path.display()));
            continue;
        }
        if let Some(mode) = util::file_mode(&path) {
            if mode & 0o077 != 0 {
                warnings.push(format!(
                    "{label} {} is mode {mode:o}; chmod 600 is recommended",
                    path.display()
                ));
            }
        }
    }

    let key_origin = resolve_key(&mut config, &mut warnings);
    let mut key_source = "none".to_string();
    if key_origin == "file" {
        key_source = format!("apiKeyFile:{}", config.command_code.api_key_file);
    } else if key_origin == "key" {
        let env_key = file_env
            .get("CC_API_KEY")
            .or_else(|| file_env.get("PAIG_CC_API_KEY"))
            .map(|v| v.trim())
            .unwrap_or("");
        key_source = if env_key == config.command_code.api_key.trim() {
            "env".to_string()
        } else {
            "config".to_string()
        };
    }

    Ok(LoadedConfig {
        config,
        sources,
        warnings,
        key_source,
    })
}

fn is_http_url(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return false;
    }
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
        .unwrap_or("");
    !rest.is_empty() && !rest.starts_with('/')
}

fn key_configured(api_key: &str, api_key_file: &str) -> bool {
    if !api_key.trim().is_empty() {
        return true;
    }
    if api_key_file.trim().is_empty() {
        return false;
    }
    !read_key_file(Path::new(api_key_file)).trim().is_empty()
}

fn env_configured(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    })
}

fn env_key_file_configured(names: &[&str]) -> bool {
    names.iter().any(|name| {
        std::env::var(name)
            .map(|path| !read_key_file(Path::new(path.trim())).trim().is_empty())
            .unwrap_or(false)
    })
}

fn auth_backend(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "commandcode" | "cc" => Some("commandCode"),
        "opencode" | "oc" => Some("opencode"),
        "kilo" | "kilocode" => Some("kilo"),
        "bob" => Some("bob"),
        "mistral" => Some("mistral"),
        _ => None,
    }
}

/// `--from-auth [backend:]file`. A plain path keeps the Command Code default;
/// an unrecognized prefix is treated as part of the path.
fn auth_target(spec: &str) -> (&'static str, &str) {
    if let Some((prefix, rest)) = spec.split_once(':') {
        if let Some(backend) = auth_backend(prefix) {
            return (backend, rest);
        }
    }
    ("commandCode", spec)
}

fn set_backend_key(config: &mut Config, backend: &str, key: String) {
    match backend {
        "opencode" => config.opencode.api_key = key,
        "kilo" => config.kilo.api_key = key,
        "bob" => config.bob.api_key = key,
        "mistral" => config.mistral.api_key = key,
        _ => config.command_code.api_key = key,
    }
}

/// Credential state for every backend. States are "configured", "free-only"
/// (OpenCode without a key: free models only), or "missing".
fn credential_status(
    config: &Config,
    env_aware: bool,
) -> Vec<(&'static str, &'static str, &'static str)> {
    let configured = |key: &str, file: &str, env_keys: &[&str], env_files: &[&str]| {
        key_configured(key, file)
            || (env_aware
                && (env_configured(env_keys) || env_key_file_configured(env_files)))
    };
    let command_code = configured(
        &config.command_code.api_key,
        &config.command_code.api_key_file,
        &["CC_API_KEY", "PAIG_CC_API_KEY"],
        &["CC_API_KEY_FILE", "CC_AUTH_FILE"],
    );
    let opencode = configured(
        &config.opencode.api_key,
        &config.opencode.api_key_file,
        &["OPENCODE_API_KEY"],
        &["OPENCODE_API_KEY_FILE"],
    );
    let kilo = configured(&config.kilo.api_key, "", &["KILO_API_KEY"], &[]);
    let bob = configured(
        &config.bob.api_key,
        &config.bob.api_key_file,
        &["PAIG_BOB_API_KEY"],
        &["PAIG_BOB_API_KEY_FILE"],
    );
    let mistral = configured(
        &config.mistral.api_key,
        &config.mistral.api_key_file,
        &["PAIG_MISTRAL_API_KEY"],
        &["PAIG_MISTRAL_API_KEY_FILE"],
    );
    vec![
        (
            "commandCode",
            if command_code { "configured" } else { "missing" },
            "Command Code models fail; set CC_API_KEY or commandCode.apiKey",
        ),
        (
            "opencode",
            if opencode { "configured" } else { "free-only" },
            "Go models need OPENCODE_API_KEY",
        ),
        (
            "kilo",
            if kilo { "configured" } else { "missing" },
            "set KILO_API_KEY or kilo.apiKey",
        ),
        (
            "bob",
            if bob { "configured" } else { "missing" },
            "set PAIG_BOB_API_KEY or bob.apiKeyFile",
        ),
        (
            "mistral",
            if mistral { "configured" } else { "missing" },
            "set PAIG_MISTRAL_API_KEY or mistral.apiKeyFile",
        ),
    ]
}

pub fn validate_config(config: &Config) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if config.server.host.is_empty() {
        errors.push("server.host must not be empty".to_string());
    }
    if config.server.port == 0 {
        errors.push(format!("server.port must be an integer from 1 to 65535 (got {})", config.server.port));
    }
    if config.advisor_max_bytes < 0 {
        errors.push("advisorMaxBytes must be a non-negative integer".to_string());
    }
    if !(0..=10).contains(&config.command_code.fetch_retries) {
        errors.push("commandCode.fetchRetries must be an integer from 0 to 10".to_string());
    }
    if !(0..=60_000).contains(&config.command_code.fetch_backoff_ms) {
        errors.push("commandCode.fetchBackoffMs must be an integer from 0 to 60000".to_string());
    }
    if config.command_code.headers.is_none() {
        // Explicit null is allowed.
    }
    for (label, value) in [
        ("commandCode.baseUrl", &config.command_code.base_url),
        ("opencode.baseUrl", &config.opencode.base_url),
        ("opencode.freeBaseUrl", &config.opencode.free_base_url),
        ("kilo.baseUrl", &config.kilo.base_url),
        ("bob.baseUrl", &config.bob.base_url),
        ("mistral.baseUrl", &config.mistral.base_url),
    ] {
        if !is_http_url(value) {
            errors.push(format!("{label} must be an http(s) URL (got {value:?})"));
        }
    }
    if !(1..=2_147_483_647).contains(&config.opencode.timeout_ms) {
        errors.push(format!(
            "opencode.timeoutMs must be a positive integer <= 2147483647 (got {})",
            config.opencode.timeout_ms
        ));
    }
    if config.opencode.models.is_empty() {
        errors.push("opencode.models must not be empty".to_string());
    }
    if config.opencode.go_models.is_empty() {
        errors.push("opencode.goModels must not be empty".to_string());
    }
    if config.ledger_path.is_empty() {
        errors.push("ledgerPath must not be empty".to_string());
    }
    if !credential_status(config, false)
        .iter()
        .any(|(_, state, _)| *state == "configured")
    {
        warnings.push(
            "no backend credentials configured; only OpenCode free models will work; set CC_API_KEY, OPENCODE_API_KEY, KILO_API_KEY, PAIG_BOB_API_KEY, or PAIG_MISTRAL_API_KEY".to_string(),
        );
    }
    (errors, warnings)
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "api_key"
        || lower == "apikey"
        || lower == "x-api-key"
        || lower == "authorization"
        || lower == "password"
        || lower == "secret"
        || lower == "token"
        || lower == "access_token"
        || lower == "refresh_token"
        || lower == "client_secret"
        || lower.ends_with("_secret")
        || lower.ends_with("_token")
        || lower.ends_with("password")
}

pub fn redact_value(value: &Value) -> Value {
    match value {
        Value::Object(items) => Value::Object(
            items
                .iter()
                .map(|(key, child)| {
                    let redacted = if is_secret_name(key) {
                        match child {
                            Value::Str(text) => Value::string(util::mask_secret(text)),
                            Value::Null => Value::Null,
                            _ => Value::string("********"),
                        }
                    } else {
                        redact_value(child)
                    };
                    (key.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact_value).collect()),
        _ => value.clone(),
    }
}

fn write_atomic_600(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("cannot chmod {}: {error}", temp.display()))?;
        file.write_all(body.as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", temp.display()))?;
    }
    fs::rename(&temp, path).map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
    let _ = fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(())
}

fn badge(pretty: &'static str, ascii: &'static str, style: &str, stderr: bool) -> String {
    util::paint(util::icon(pretty, ascii, stderr), style, stderr)
}

fn print_credentials(status: &[(&'static str, &'static str, &'static str)]) {
    println!("{}", util::paint("api keys:", "1", false));
    for (name, state, hint) in status {
        match *state {
            "configured" => println!(
                "  {}  {name:<12} configured",
                badge("\u{f058}", "[ok]", "1;32", false)
            ),
            "free-only" => println!(
                "  {}  {name:<12} free-only ({hint})",
                badge("\u{f05a}", "[i]", "1;34", false)
            ),
            _ => println!(
                "  {}  {name:<12} missing ({hint})",
                badge("\u{f071}", "[!]", "1;33", false)
            ),
        }
    }
}

fn print_cli_help() {
    println!("{}", util::paint("PAIG gateway", "1", false));
    println!();
    println!("{} paig <command>", util::paint("usage:", "2", false));
    println!();
    println!("{}", util::paint("commands:", "1", false));
    let rows = [
        (
            "\u{f055}",
            "[+]",
            "1;32",
            "--init [--force] [--from-auth [backend:]file]...",
            "create ~/.paig/config.json",
        ),
        (
            "\u{f06e}",
            "[p]",
            "1;36",
            "--print",
            "print the resolved config (secrets masked)",
        ),
        (
            "\u{f058}",
            "[ok]",
            "1;34",
            "--check",
            "validate config and print the sources",
        ),
        (
            "\u{f02b}",
            "[v]",
            "1;36",
            "--version",
            "print the build version",
        ),
        (
            "\u{f084}",
            "[k]",
            "1;33",
            "--get <dotted.key>",
            "print one resolved value",
        ),
        (
            "\u{f02d}",
            "[r]",
            "1;35",
            "--rules",
            "print the game rulebook",
        ),
        (
            "\u{f04b}",
            "[>]",
            "1;32",
            "--start",
            "start in the background (pid/log in ~/.paig/)",
        ),
        (
            "\u{f04d}",
            "[#]",
            "1;33",
            "--stop",
            "stop the background server",
        ),
        (
            "\u{f021}",
            "[~]",
            "1;36",
            "--restart",
            "stop, then start in the background",
        ),
        (
            "\u{f05a}",
            "[i]",
            "1;34",
            "--status",
            "report whether the background server runs",
        ),
    ];
    for (pretty, ascii, style, flag, description) in rows {
        let flag = format!("{flag:<48}");
        println!(
            "  {}  {} {}",
            badge(pretty, ascii, style, false),
            util::paint(&flag, "1;36", false),
            description
        );
    }
    println!();
    println!("{}", util::paint("examples:", "1", false));
    println!("  paig --init");
    println!("  paig --check");
    println!("  paig --start");
    println!("  paig --version");
}

pub fn run_cli(argv: &[String]) -> i32 {
    let has = |name: &str| argv.iter().any(|arg| arg == name);
    let value = |name: &str| -> Option<String> {
        argv.iter()
            .position(|arg| arg == name)
            .and_then(|index| argv.get(index + 1))
            .cloned()
    };
    let values = |name: &str| -> Vec<String> {
        let mut out = Vec::new();
        for (index, arg) in argv.iter().enumerate() {
            if arg.as_str() == name {
                if let Some(value) = argv.get(index + 1) {
                    out.push(value.clone());
                }
            }
        }
        out
    };
    if argv.is_empty() || has("--help") || has("-h") {
        print_cli_help();
        return 0;
    }
    if has("--version") || has("-V") {
        println!("paig {}", crate::VERSION);
        return 0;
    }
    if has("--rules") {
        println!(
            "{}  {} rulebook v{}",
            badge("\u{f02d}", "[r]", "1;35", false),
            crate::rules::PROJECT,
            crate::rules::RULEBOOK_VERSION
        );
        for rule in crate::rules::RULES {
            println!("  {} {:<32} {}", rule.id, rule.title, rule.detail);
        }
        for level in crate::rules::LEVELS {
            println!(
                "  L{} {:<20} {} (boss: {})",
                level.number, level.name, level.mission, level.boss
            );
        }
        println!("  {}", crate::rules::scorecard());
        return 0;
    }
    if has("--init") {
        let target = std::env::var("PAIG_CONFIG")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| util::resolve_path(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")), &v))
            .unwrap_or_else(|| util::paig_home().join("config.json"));
        let force = has("--force");
        if target.exists() && !force {
            println!(
                "{}  config exists: {}",
                badge("\u{f071}", "[!]", "1;33", false),
                target.display()
            );
            println!("{}", util::paint("   use --force to overwrite", "2", false));
            return 0;
        }
        let mut config = Config::defaults();
        let mut existing_raw = None;
        if target.exists() && force {
            match read_json_file(&target) {
                Ok(existing) => {
                    existing_raw = Some(existing.clone());
                    let mut base = config.to_value();
                    deep_merge(&mut base, &existing);
                    let mut warnings = Vec::new();
                    config = normalize(
                        &mut base,
                        &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                        &Config::defaults(),
                        &mut warnings,
                    );
                    for warning in warnings {
                        println!(
                            "{}  {warning}",
                            badge("\u{f071}", "[!]", "1;33", false)
                        );
                    }
                }
                Err(error) => {
                    println!(
                        "{}  {error}; writing defaults",
                        badge("\u{f071}", "[!]", "1;33", false)
                    );
                }
            }
        }
        let mut imported: Vec<String> = Vec::new();
        let auth_specs = values("--from-auth");
        for spec in &auth_specs {
            let (backend, file) = auth_target(spec);
            let path = util::resolve_path(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                file,
            );
            let key = if path.exists() { read_key_file(&path) } else { String::new() };
            if key.is_empty() {
                eprintln!(
                    "{}  no apiKey found in {}",
                    badge("\u{f057}", "[x]", "1;31", true),
                    path.display()
                );
                return 1;
            }
            set_backend_key(&mut config, backend, key);
            imported.push(format!("{backend}: {}", path.display()));
        }
        let mut env_imported = false;
        if auth_specs.is_empty()
            && config.command_code.api_key.is_empty()
            && config.command_code.api_key_file.is_empty()
        {
            if let Ok(key) = std::env::var("CC_API_KEY").or_else(|_| std::env::var("PAIG_CC_API_KEY")) {
                if !key.trim().is_empty() {
                    config.command_code.api_key = key.trim().to_string();
                    env_imported = true;
                }
            }
        }
        let mut output = config.to_value();
        if let Some(existing) = &existing_raw {
            merge_missing(&mut output, existing);
        }
        let body = format!("{}\n", output.stringify_pretty());
        if let Err(error) = write_atomic_600(&target, &body) {
            eprintln!("{}  {error}", badge("\u{f057}", "[x]", "1;31", true));
            return 1;
        }
        println!(
            "{}  config: {}",
            badge("\u{f013}", "[cfg]", "1;36", false),
            target.display()
        );
        for line in &imported {
            println!(
                "{}  imported key: {line}",
                badge("\u{f084}", "[k]", "1;32", false)
            );
        }
        if env_imported {
            println!(
                "{}  command code key: from CC_API_KEY",
                badge("\u{f084}", "[k]", "1;32", false)
            );
        } else if imported.is_empty()
            && key_configured(&config.command_code.api_key, &config.command_code.api_key_file)
        {
            println!(
                "{}  command code key: kept from existing config",
                badge("\u{f084}", "[k]", "1;36", false)
            );
        }
        let status = credential_status(&config, true);
        print_credentials(&status);
        if !status
            .iter()
            .any(|(_, state, _)| *state == "configured")
        {
            println!(
                "{}  note: only OpenCode free models (opencode/*) will work without credentials",
                badge("\u{f05a}", "[i]", "1;34", false)
            );
        }
        println!(
            "{}  {}",
            badge("\u{f061}", "->", "2", false),
            util::paint("restart paig to apply: ./build/paig --restart", "2", false)
        );
        return 0;
    }
    if has("--print") {
        match load_config() {
            Ok(loaded) => {
                println!("{}", redact_value(&loaded.config.to_value()).stringify_pretty());
                0
            }
            Err(error) => {
                eprintln!("{}  paig: {error}", badge("\u{f057}", "[x]", "1;31", true));
                1
            }
        }
    } else if has("--check") {
        match load_config() {
            Ok(loaded) => {
                let (errors, warnings) = validate_config(&loaded.config);
                let sources = if loaded.sources.is_empty() {
                    "defaults".to_string()
                } else {
                    loaded
                        .sources
                        .iter()
                        .map(|source| source.path.clone())
                        .collect::<Vec<_>>()
                        .join(" + ")
                };
                println!(
                    "{}  config: {sources}",
                    badge("\u{f013}", "[cfg]", "1;36", false)
                );
                let status = credential_status(&loaded.config, false);
                print_credentials(&status);
                if !loaded.config.command_code.api_key.is_empty() {
                    println!(
                        "{}  command code key: {} via {}",
                        badge("\u{f084}", "[k]", "1;33", false),
                        util::mask_secret(&loaded.config.command_code.api_key),
                        loaded.key_source
                    );
                }
                for warning in loaded.warnings.iter().chain(warnings.iter()) {
                    println!(
                        "{}  {warning}",
                        badge("\u{f071}", "[!]", "1;33", false)
                    );
                }
                for error in &errors {
                    eprintln!("{}  {error}", badge("\u{f057}", "[x]", "1;31", true));
                }
                if errors.is_empty() {
                    println!("{}  ok", badge("\u{f058}", "[ok]", "1;32", false));
                    0
                } else {
                    1
                }
            }
            Err(error) => {
                eprintln!("{}  paig: {error}", badge("\u{f057}", "[x]", "1;31", true));
                1
            }
        }
    } else if let Some(dotted) = value("--get") {
        match load_config() {
            Ok(loaded) => {
                let redacted = redact_value(&loaded.config.to_value());
                match get_path(&redacted, &dotted) {
                    Some(Value::Str(text)) => println!("{text}"),
                    Some(Value::Null) => println!("null"),
                    Some(value) => println!("{}", value.stringify()),
                    None => {
                        eprintln!(
                            "{}  unknown config key: {dotted}",
                            badge("\u{f057}", "[x]", "1;31", true)
                        );
                        return 1;
                    }
                }
                0
            }
            Err(error) => {
                eprintln!("{}  paig: {error}", badge("\u{f057}", "[x]", "1;31", true));
                1
            }
        }
    } else {
        eprintln!(
            "{}  unknown command; try --help",
            badge("\u{f057}", "[x]", "1;31", true)
        );
        2
    }
}

trait Pretty {
    fn stringify_pretty(&self) -> String;
}

impl Pretty for Value {
    fn stringify_pretty(&self) -> String {
        fn emit(value: &Value, indent: usize, out: &mut String) {
            let pad = "  ".repeat(indent);
            let pad2 = "  ".repeat(indent + 1);
            match value {
                Value::Object(items) => {
                    if items.is_empty() {
                        out.push_str("{}");
                        return;
                    }
                    out.push_str("{\n");
                    for (index, (key, child)) in items.iter().enumerate() {
                        out.push_str(&pad2);
                        out.push_str(&Value::string(key).stringify());
                        out.push_str(": ");
                        emit(child, indent + 1, out);
                        if index + 1 != items.len() {
                            out.push(',');
                        }
                        out.push('\n');
                    }
                    out.push_str(&pad);
                    out.push('}');
                }
                Value::Array(items) => {
                    if items.is_empty() {
                        out.push_str("[]");
                        return;
                    }
                    out.push_str("[\n");
                    for (index, child) in items.iter().enumerate() {
                        out.push_str(&pad2);
                        emit(child, indent + 1, out);
                        if index + 1 != items.len() {
                            out.push(',');
                        }
                        out.push('\n');
                    }
                    out.push_str(&pad);
                    out.push(']');
                }
                _ => out.push_str(&value.stringify()),
            }
        }
        let mut out = String::new();
        emit(self, 0, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_quoting_and_comment_rules() {
        let env = parse_dotenv(
            "A=plain\nB=\"x\\nY\" #tail\nC='a b'\nD=value # comment\nE=value#not-comment\nexport F=1\n",
        );
        assert_eq!(env.get("A").unwrap(), "plain");
        assert_eq!(env.get("B").unwrap(), "x\nY");
        assert_eq!(env.get("C").unwrap(), "a b");
        assert_eq!(env.get("D").unwrap(), "value");
        assert_eq!(env.get("E").unwrap(), "value#not-comment");
        assert_eq!(env.get("F").unwrap(), "1");
    }

    #[test]
    fn deep_merge_preserves_unknown_and_merges_objects() {
        let mut base = Value::parse_str(r#"{"a":{"b":1,"c":2},"keep":"x"}"#).unwrap();
        let over = Value::parse_str(r#"{"a":{"b":3},"new":true}"#).unwrap();
        deep_merge(&mut base, &over);
        assert_eq!(base.get("a").unwrap().get("b").unwrap().as_i64(), Some(3));
        assert_eq!(base.get("a").unwrap().get("c").unwrap().as_i64(), Some(2));
        assert_eq!(base.get("keep").unwrap().as_str(), Some("x"));
        assert_eq!(base.get("new").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn redaction_masks_keys_but_not_paths() {
        let value = Value::parse_str(
            r#"{"commandCode":{"apiKey":"secretsecret","apiKeyFile":"/tmp/k"},"token":"abc","authorization":"Bearer x"}"#,
        )
        .unwrap();
        let redacted = redact_value(&value);
        let cc = redacted.get("commandCode").unwrap();
        assert_eq!(cc.get("apiKey").unwrap().as_str(), Some("****cret"));
        assert_eq!(cc.get("apiKeyFile").unwrap().as_str(), Some("/tmp/k"));
        assert_eq!(redacted.get("token").unwrap().as_str(), Some("********"));
    }

    #[test]
    fn empty_header_removes_the_default() {
        let base = vec![("user-agent".to_string(), "cli".to_string())];
        let config = Value::parse_str(r#"{"commandCode":{"headers":{"user-agent":""}}}"#).unwrap();
        let mut warnings = Vec::new();
        let merged = normalize_headers(&config, &base, &mut warnings).unwrap();
        assert!(merged.is_empty());
    }

    #[test]
    fn negative_numbers_are_rejected() {
        let mut config = Config::defaults();
        config.advisor_max_bytes = -1;
        config.command_code.fetch_retries = -1;
        config.command_code.fetch_backoff_ms = -1;
        config.opencode.timeout_ms = 0;
        let (errors, _) = validate_config(&config);
        assert!(errors.iter().any(|error| error.contains("advisorMaxBytes")));
        assert!(errors.iter().any(|error| error.contains("fetchRetries")));
        assert!(errors.iter().any(|error| error.contains("fetchBackoffMs")));
        assert!(errors.iter().any(|error| error.contains("timeoutMs")));

        config.command_code.fetch_retries = 11;
        config.command_code.fetch_backoff_ms = 60_001;
        config.advisor_max_bytes = 1;
        config.opencode.timeout_ms = 1;
        let (errors, _) = validate_config(&config);
        assert!(errors.iter().any(|error| error.contains("fetchRetries")));
        assert!(errors.iter().any(|error| error.contains("fetchBackoffMs")));
    }

    #[test]
    fn merge_missing_preserves_unknown_keys() {
        let mut output = Value::parse_str(r#"{"commandCode":{"apiKey":"new"}}"#).unwrap();
        let existing = Value::parse_str(
            r#"{"commandCode":{"apiKey":"old","custom":1},"unknown":{"x":true}}"#,
        )
        .unwrap();
        merge_missing(&mut output, &existing);
        assert_eq!(
            output
                .get("commandCode")
                .unwrap()
                .get("apiKey")
                .unwrap()
                .as_str(),
            Some("new")
        );
        assert_eq!(
            output
                .get("commandCode")
                .unwrap()
                .get("custom")
                .unwrap()
                .as_i64(),
            Some(1)
        );
        assert_eq!(
            output.get("unknown").unwrap().get("x").unwrap().as_bool(),
            Some(true)
        );
    }

    #[test]
    fn urls_are_http_or_https() {
        assert!(is_http_url("https://api.example.com"));
        assert!(is_http_url("http://127.0.0.1:1"));
        assert!(!is_http_url("ftp://x"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("not a url"));
    }

    #[test]
    fn from_auth_accepts_backend_prefixes() {
        assert_eq!(auth_target("mistral:/tmp/m.key"), ("mistral", "/tmp/m.key"));
        assert_eq!(auth_target("CC:/tmp/cc.json"), ("commandCode", "/tmp/cc.json"));
        assert_eq!(auth_target("kilo:/tmp/k"), ("kilo", "/tmp/k"));
        assert_eq!(auth_target("bob:/tmp/b.key"), ("bob", "/tmp/b.key"));
        assert_eq!(auth_target("opencode:/tmp/o.key"), ("opencode", "/tmp/o.key"));
        assert_eq!(auth_target("/tmp/plain-key"), ("commandCode", "/tmp/plain-key"));
        assert_eq!(auth_target("C:/tmp/plain"), ("commandCode", "C:/tmp/plain"));
    }

    #[test]
    fn any_backend_key_silences_missing_credentials_warning() {
        let mut config = Config::defaults();
        let (_, warnings) = validate_config(&config);
        assert!(warnings.iter().any(|warning| warning.contains("no backend credentials")));

        config.mistral.api_key = "test".to_string();
        let (_, warnings) = validate_config(&config);
        assert!(!warnings.iter().any(|warning| warning.contains("no backend credentials")));
    }

    #[test]
    fn credential_status_reflects_backend_keys() {
        let mut config = Config::defaults();
        let state = |config: &Config, backend: &str| {
            credential_status(config, false)
                .into_iter()
                .find(|(name, _, _)| *name == backend)
                .map(|(_, state, _)| state)
                .unwrap()
        };
        assert_eq!(state(&config, "opencode"), "free-only");
        assert_eq!(state(&config, "commandCode"), "missing");

        config.mistral.api_key = "test".to_string();
        config.kilo.api_key = "test".to_string();
        assert_eq!(state(&config, "mistral"), "configured");
        assert_eq!(state(&config, "kilo"), "configured");
    }

    #[test]
    fn missing_key_file_is_not_configured() {
        let mut config = Config::defaults();
        config.bob.api_key_file = "/nonexistent/paig-test-key".to_string();
        let bob = credential_status(&config, false)
            .into_iter()
            .find(|(name, _, _)| *name == "bob")
            .unwrap();
        assert_eq!(bob.1, "missing");
    }
}
