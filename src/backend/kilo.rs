//! Kilo gateway backend: `kilo/<id>` and `kilocode/<id>`. The gateway speaks
//! OpenAI chat completions, so request/response passthrough is enough.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::client::{HttpClient, Url};
use crate::config::Config;
use crate::http::Response;
use crate::json::Value;
use crate::proxy;

const FREE_PREFIXES: &[&str] = &["kilo-free/"];
const KEYED_PREFIXES: &[&str] = &["kilo/", "kilocode/"];
const FALLBACK_FREE: &[&str] = &[
    "nvidia/nemotron-3.5-super-vl:free",
    "inclusionai/ling-3.2-flash:free",
    "baidu/ernie-5.5-flash-preview:free",
    "liquid/lfm-3-1.7b:free",
    "particle-ai/particle-1:free",
    "dots-studio/dots-3-note-preview:free",
    "nvidia/nemotron-3.5-lightning:free",
    "cohere/north-mini-code:free",
    "nvidia/nemotron-3-ultra-550b-a55b:free",
    "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Free,
    Keyed,
}

#[derive(Debug, Clone)]
pub struct Target {
    pub model_id: String,
    pub kind: Kind,
}

static CACHE: OnceLock<Mutex<Option<(Instant, Vec<(String, Value)>)>>> = OnceLock::new();

fn is_free_id(id: &str) -> bool {
    id.ends_with(":free")
}

pub fn kilo_model_id(model: &str) -> Option<Target> {
    let raw = model.trim();
    for prefix in FREE_PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let id = rest.trim();
            return if id.is_empty() {
                None
            } else {
                Some(Target {
                    model_id: id.to_string(),
                    kind: Kind::Free,
                })
            };
        }
    }
    for prefix in KEYED_PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let id = rest.trim();
            return if id.is_empty() {
                None
            } else {
                Some(Target {
                    model_id: id.to_string(),
                    kind: Kind::Keyed,
                })
            };
        }
    }
    None
}

fn headers(config: &Config, kind: Kind) -> Vec<(String, String)> {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if kind == Kind::Keyed && !config.kilo.api_key.is_empty() {
        headers.push((
            "authorization".to_string(),
            format!("Bearer {}", config.kilo.api_key),
        ));
    }
    headers
}

pub fn handle(
    body: &Value,
    res: &mut Response,
    target: &Target,
    config: &Config,
    client: &HttpClient,
) -> Result<(), String> {
    if target.kind == Kind::Free && !is_free_id(&target.model_id) {
        return res
            .send_openai_error(
                400,
                &format!(
                    "kilo-free/ only serves :free models; use kilo/{} with KILO_API_KEY",
                    target.model_id
                ),
                "invalid_request_error",
                Some("kilo_free_model_required"),
            )
            .map_err(|error| error.to_string());
    }
    if target.kind == Kind::Keyed && config.kilo.api_key.is_empty() {
        return res
            .send_openai_error(
                503,
                "Kilo keyed models require KILO_API_KEY or kilo.apiKey; use kilo-free/ for :free models.",
                "bridge_error",
                Some("kilo_key_missing"),
            )
            .map_err(|error| error.to_string());
    }
    let url = Url::parse(&format!(
        "{}/chat/completions",
        config.kilo.base_url.trim_end_matches('/')
    ))?;
    let mut payload = body.clone();
    payload.insert("model", Value::string(&target.model_id));
    let upstream = match proxy::post_json(
        client,
        &url,
        &headers(config, target.kind),
        &payload,
        Duration::from_secs(300),
    ) {
        Ok(upstream) => upstream,
        Err(error) => {
            return res
                .send_openai_error(
                    502,
                    &format!("Cannot reach Kilo gateway: {error}"),
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };
    let original_model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(target.model_id.as_str());
    proxy::forward(
        res,
        upstream,
        "Kilo gateway",
        original_model,
        body.get("stream").and_then(Value::as_bool) == Some(true),
    )
}

fn has_string(list: &[Value], want: &str) -> bool {
    list.iter().any(|value| value.as_str() == Some(want))
}

fn kilo_caps(model: &Value) -> Value {
    let mut caps = Value::obj();
    crate::backend::cap_int(model, "context_length", &mut caps, "context_window");
    if let Some(top) = model.get("top_provider") {
        if caps.get("context_window").is_none() {
            crate::backend::cap_int(top, "context_length", &mut caps, "context_window");
        }
        crate::backend::cap_int(
            top,
            "max_completion_tokens",
            &mut caps,
            "max_output_tokens",
        );
    }
    if let Some(modalities) = model
        .get("architecture")
        .and_then(|arch| arch.get("input_modalities"))
        .and_then(Value::as_array)
    {
        caps.insert(
            "supports_vision",
            Value::Bool(has_string(modalities, "image")),
        );
    }
    if let Some(params) = model.get("supported_parameters").and_then(Value::as_array) {
        caps.insert(
            "supports_function_calling",
            Value::Bool(has_string(params, "tools")),
        );
        caps.insert(
            "supports_reasoning",
            Value::Bool(has_string(params, "reasoning")),
        );
    }
    if let Some(pricing) = model.get("pricing") {
        for (from, to) in [
            ("prompt", "input_cost_per_token"),
            ("completion", "output_cost_per_token"),
        ] {
            let cost = pricing
                .get(from)
                .and_then(Value::as_str)
                .and_then(|raw| raw.parse::<f64>().ok())
                .filter(|cost| *cost >= 0.0);
            if let Some(cost) = cost {
                caps.insert(to, Value::Float(cost));
            }
        }
    }
    caps
}

fn catalogue(config: &Config, client: &HttpClient) -> Vec<(String, Value)> {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, list)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(300) {
                return list.clone();
            }
        }
    }

    let mut entries: Vec<(String, Value)> = FALLBACK_FREE
        .iter()
        .map(|id| (id.to_string(), Value::obj()))
        .collect();
    // Kilo's catalogue is public; the keyless free route never sends the key.
    if let Ok(url) = Url::parse(&format!("{}/models", config.kilo.base_url.trim_end_matches('/'))) {
        if let Ok(mut response) = client.request("GET", &url, &[], None, Some(Duration::from_secs(5)))
        {
            if response.status == 200 {
                if let Ok(body) = response.read_all() {
                    if let Ok(value) = Value::parse(&body) {
                        if let Some(data) = value.get("data").and_then(Value::as_array) {
                            let picked: Vec<(String, Value)> = data
                                .iter()
                                .filter_map(|model| {
                                    let id = model.get("id").and_then(Value::as_str)?;
                                    Some((id.to_string(), kilo_caps(model)))
                                })
                                .collect();
                            if !picked.is_empty() {
                                entries = picked;
                            }
                        }
                    }
                }
            }
        }
    }
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), entries.clone()));
    }
    entries
}

fn model_entry(prefix: &str, id: &str, caps: &Value, created: i64) -> Value {
    let mut model = Value::obj();
    model.insert("id", Value::string(format!("{prefix}{id}")));
    model.insert("object", Value::string("model"));
    model.insert("created", Value::Int(created));
    model.insert("owned_by", Value::string("kilo"));
    crate::backend::attach_caps(&mut model, caps.clone());
    model
}

fn build_models(entries: &[(String, Value)], config: &Config, created: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for (id, caps) in entries {
        if is_free_id(id) {
            out.push(model_entry("kilo-free/", id, caps, created));
        }
    }
    if !config.kilo.api_key.is_empty() && config.kilo.all_models {
        for (id, caps) in entries {
            if is_free_id(id) {
                continue;
            }
            out.push(model_entry("kilo/", id, caps, created));
        }
    }
    out
}

pub fn list_models(config: &Config, client: &HttpClient) -> Vec<Value> {
    build_models(&catalogue(config, client), config, crate::util::now_unix() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_normalize_kilo_metadata() {
        let model = Value::parse_str(
            r#"{"context_length":256000,
                "top_provider":{"context_length":256000,"max_completion_tokens":32768},
                "architecture":{"input_modalities":["text","image"]},
                "supported_parameters":["max_tokens","tools","reasoning"],
                "pricing":{"prompt":"0","completion":"0.000002"}}"#,
        )
        .unwrap();
        let caps = kilo_caps(&model);
        assert_eq!(caps.get("context_window").unwrap().as_i64(), Some(256000));
        assert_eq!(
            caps.get("max_output_tokens").unwrap().as_i64(),
            Some(32768)
        );
        assert_eq!(caps.get("supports_vision").unwrap().as_bool(), Some(true));
        assert_eq!(
            caps.get("supports_function_calling").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            caps.get("supports_reasoning").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            caps.get("output_cost_per_token").unwrap().as_f64(),
            Some(0.000002)
        );
    }

    #[test]
    fn caps_skip_variable_pricing() {
        let model = Value::parse_str(
            r#"{"pricing":{"prompt":"-1","completion":"0"}}"#,
        )
        .unwrap();
        let caps = kilo_caps(&model);
        assert!(caps.get("input_cost_per_token").is_none());
        assert_eq!(
            caps.get("output_cost_per_token").unwrap().as_f64(),
            Some(0.0)
        );
    }

    #[test]
    fn prefixes_split_free_from_keyed() {
        let free = kilo_model_id("kilo-free/liquid/lfm-3-1.7b:free").unwrap();
        assert_eq!(free.model_id, "liquid/lfm-3-1.7b:free");
        assert_eq!(free.kind, Kind::Free);
        let keyed = kilo_model_id("kilo/liquid/lfm-3-1.7b:free").unwrap();
        assert_eq!(keyed.kind, Kind::Keyed);
        let alias = kilo_model_id("kilocode/kilo-auto/efficient").unwrap();
        assert_eq!(alias.model_id, "kilo-auto/efficient");
        assert_eq!(alias.kind, Kind::Keyed);
        assert!(kilo_model_id("kilo-free/").is_none());
        assert!(kilo_model_id("kilo/").is_none());
        assert!(kilo_model_id("kilocode/").is_none());
        assert!(kilo_model_id("other/x").is_none());
    }

    #[test]
    fn free_route_never_sends_the_key() {
        let mut config = Config::defaults();
        config.kilo.api_key = "secret".to_string();
        assert!(!headers(&config, Kind::Free)
            .iter()
            .any(|(name, _)| name == "authorization"));
        assert!(headers(&config, Kind::Keyed)
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer secret"));
    }

    #[test]
    fn catalogue_splits_free_and_keyed() {
        let entries = vec![
            ("liquid/lfm-3-1.7b:free".to_string(), Value::obj()),
            ("kilo-auto/efficient".to_string(), Value::obj()),
        ];
        let mut config = Config::defaults();
        config.kilo.api_key.clear();
        let ids = |list: Vec<Value>| -> Vec<String> {
            list.iter()
                .filter_map(|model| {
                    model
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        };
        let free = ids(build_models(&entries, &config, 1));
        assert_eq!(free, vec!["kilo-free/liquid/lfm-3-1.7b:free"]);

        config.kilo.api_key = "k".to_string();
        let keyed = ids(build_models(&entries, &config, 1));
        assert_eq!(keyed, vec!["kilo-free/liquid/lfm-3-1.7b:free"]);

        config.kilo.all_models = true;
        let all = ids(build_models(&entries, &config, 1));
        assert_eq!(
            all,
            vec!["kilo-free/liquid/lfm-3-1.7b:free", "kilo/kilo-auto/efficient"]
        );
    }
}
