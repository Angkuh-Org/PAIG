//! Kilo gateway backend: `kilo/<id>` and `kilocode/<id>`. The gateway speaks
//! OpenAI chat completions, so request/response passthrough is enough.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::client::{HttpClient, Url};
use crate::config::Config;
use crate::http::Response;
use crate::json::Value;
use crate::proxy;

const PREFIXES: &[&str] = &["kilo/", "kilocode/"];
const FALLBACK_FREE: &[&str] = &[
    "nvidia/nemotron-3.5-super-vl:free",
    "recursive/flint-v1.0:free",
    "xingchen-agi/xing4.0-29b-a4b:free",
    "dots-studio/dots-3-note-preview:free",
    "liquid/lfm-2.5-2.6b:free",
    "nvidia/nemotron-3.5-lightning:free",
    "poolside/laguna-s-2.1:free",
    "poolside/laguna-xs-2.1:free",
    "cohere/north-mini-code:free",
    "nvidia/nemotron-3.5-content-safety:free",
    "nvidia/nemotron-3-ultra-550b-a55b:free",
    "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
];

static CACHE: OnceLock<Mutex<Option<(Instant, Vec<Value>)>>> = OnceLock::new();

pub fn kilo_model_id(model: &str) -> Option<String> {
    let raw = model.trim();
    for prefix in PREFIXES {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let id = rest.trim();
            return if id.is_empty() { None } else { Some(id.to_string()) };
        }
    }
    None
}

fn headers(config: &Config) -> Vec<(String, String)> {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    if !config.kilo.api_key.is_empty() {
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
    model_id: &str,
    config: &Config,
    client: &HttpClient,
) -> Result<(), String> {
    let url = Url::parse(&format!(
        "{}/chat/completions",
        config.kilo.base_url.trim_end_matches('/')
    ))?;
    let mut payload = body.clone();
    payload.insert("model", Value::string(model_id));
    let upstream = match proxy::post_json(
        client,
        &url,
        &headers(config),
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
        .unwrap_or(model_id);
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

pub fn list_models(config: &Config, client: &HttpClient) -> Vec<Value> {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, list)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(300) {
                return list.clone();
            }
        }
    }

    let mut ids: Vec<(String, Value)> = FALLBACK_FREE
        .iter()
        .map(|id| (id.to_string(), Value::obj()))
        .collect();
    let mut headers = Vec::new();
    if !config.kilo.api_key.is_empty() {
        headers.push((
            "authorization".to_string(),
            format!("Bearer {}", config.kilo.api_key),
        ));
    }
    if let Ok(url) = Url::parse(&format!("{}/models", config.kilo.base_url.trim_end_matches('/'))) {
        if let Ok(mut response) =
            client.request("GET", &url, &headers, None, Some(Duration::from_secs(5)))
        {
            if response.status == 200 {
                if let Ok(body) = response.read_all() {
                    if let Ok(value) = Value::parse(&body) {
                        if let Some(data) = value.get("data").and_then(Value::as_array) {
                            let picked: Vec<(String, Value)> = data
                                .iter()
                                .filter_map(|model| {
                                    let id = model.get("id").and_then(Value::as_str)?;
                                    if !config.kilo.all_models && !id.ends_with(":free") {
                                        return None;
                                    }
                                    Some((id.to_string(), kilo_caps(model)))
                                })
                                .collect();
                            if !picked.is_empty() {
                                ids = picked;
                            }
                        }
                    }
                }
            }
        }
    }

    let created = crate::util::now_unix() as i64;
    let list: Vec<Value> = ids
        .into_iter()
        .map(|(id, caps)| {
            let mut model = Value::obj();
            model.insert("id", Value::string(format!("kilo/{id}")));
            model.insert("object", Value::string("model"));
            model.insert("created", Value::Int(created));
            model.insert("owned_by", Value::string("kilo"));
            crate::backend::attach_caps(&mut model, caps);
            model
        })
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
}
