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

pub fn list_models(config: &Config, client: &HttpClient) -> Vec<Value> {
    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cache.lock() {
        if let Some((at, list)) = guard.as_ref() {
            if at.elapsed() < Duration::from_secs(300) {
                return list.clone();
            }
        }
    }

    let mut ids: Vec<String> = FALLBACK_FREE.iter().map(|id| id.to_string()).collect();
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
                            let all: Vec<String> = data
                                .iter()
                                .filter_map(|model| model.get("id").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect();
                            let picked: Vec<String> = if config.kilo.all_models {
                                all
                            } else {
                                all.into_iter()
                                    .filter(|id| id.ends_with(":free"))
                                    .collect()
                            };
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
        .map(|id| {
            let mut model = Value::obj();
            model.insert("id", Value::string(format!("kilo/{id}")));
            model.insert("object", Value::string("model"));
            model.insert("created", Value::Int(created));
            model.insert("owned_by", Value::string("kilo"));
            model
        })
        .collect();
    if let Ok(mut guard) = cache.lock() {
        *guard = Some((Instant::now(), list.clone()));
    }
    list
}
