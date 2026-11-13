//! Bob gateway backend: `bob/<id>`. IBM Bob speaks OpenAI chat completions
//! but wants an `apikey` credential plus a team id header.

use std::fs;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::client::{HttpClient, Url};
use crate::config::Config;
use crate::http::Response;
use crate::json::Value;
use crate::proxy;
use crate::util::logw;

const TTL: Duration = Duration::from_secs(300);
const PROFILE_TIMEOUT: Duration = Duration::from_secs(5);
const CHAT_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Default)]
struct Cache {
    team: Option<(String, Instant, String)>,
    models: Option<(String, Instant, Vec<Value>)>,
}

static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> &'static Mutex<Cache> {
    CACHE.get_or_init(|| Mutex::new(Cache::default()))
}

pub fn bob_model_id(model: &str) -> Option<String> {
    let raw = model.trim();
    let id = raw.strip_prefix("bob/")?.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn api_key(config: &Config) -> String {
    if !config.bob.api_key.is_empty() {
        return config.bob.api_key.clone();
    }
    if config.bob.api_key_file.is_empty() {
        return String::new();
    }
    match fs::read_to_string(&config.bob.api_key_file) {
        Ok(text) => {
            let key = text.trim();
            if key.is_empty() {
                logw!("bob.apiKeyFile contains no key: {}", config.bob.api_key_file);
            }
            key.to_string()
        }
        Err(error) => {
            logw!("cannot read bob.apiKeyFile: {error}");
            String::new()
        }
    }
}

fn admin_base(base_url: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if let Some(prefix) = base.strip_suffix("/inference/v1") {
        return Some(format!("{prefix}/admin/v1"));
    }
    let url = Url::parse(base).ok()?;
    Some(format!("{}://{}/admin/v1", url.scheme, url.host_header()))
}

fn auth_headers(config: &Config, key: &str) -> Vec<(String, String)> {
    vec![
        ("user-agent".to_string(), config.bob.user_agent.clone()),
        ("authorization".to_string(), format!("apikey {key}")),
    ]
}

fn upstream_headers(config: &Config, key: &str, team_id: &str) -> Vec<(String, String)> {
    let mut headers = vec![("content-type".to_string(), "application/json".to_string())];
    headers.extend(auth_headers(config, key));
    if !team_id.is_empty() {
        headers.push(("x-team-id".to_string(), team_id.to_string()));
    }
    headers
}

fn cached_team(scope: &str) -> Option<String> {
    let guard = cache().lock().ok()?;
    let (cached_scope, at, value) = guard.team.as_ref()?;
    if cached_scope == scope && at.elapsed() < TTL {
        return Some(value.clone());
    }
    None
}

fn fetch_team(admin: &str, config: &Config, client: &HttpClient, key: &str) -> Option<String> {
    let url = Url::parse(&format!("{admin}/profile")).ok()?;
    let mut response = client
        .request("GET", &url, &auth_headers(config, key), None, Some(PROFILE_TIMEOUT))
        .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let body = response.read_all().ok()?;
    let value = Value::parse(&body).ok()?;
    let id = value
        .get("instances")?
        .as_array()?
        .first()?
        .get("teams")?
        .as_array()?
        .first()?
        .get("id")?
        .as_str()?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

// Bob returns 402 "team user not found" unless x-team-id is present, so the
// team is discovered from the profile when it is not configured.
fn resolve_team_id(config: &Config, client: &HttpClient, key: &str) -> String {
    if !config.bob.team_id.is_empty() {
        return config.bob.team_id.clone();
    }
    if key.is_empty() {
        return String::new();
    }
    let Some(admin) = admin_base(&config.bob.base_url) else {
        return String::new();
    };
    if let Some(team) = cached_team(&admin) {
        return team;
    }
    let team = fetch_team(&admin, config, client, key).unwrap_or_default();
    if let Ok(mut guard) = cache().lock() {
        guard.team = Some((admin, Instant::now(), team.clone()));
    }
    team
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
            .send_openai_error(
                503,
                "Bob API key is not configured. Set bob.apiKey, bob.apiKeyFile, or PAIG_BOB_API_KEY.",
                "bridge_error",
                None,
            )
            .map_err(|error| error.to_string());
    }
    let team_id = resolve_team_id(config, client, &key);
    if team_id.is_empty() {
        return res
            .send_openai_error(
                502,
                "Bob team id is not available. Set bob.teamId or make /admin/v1/profile reachable.",
                "upstream_error",
                None,
            )
            .map_err(|error| error.to_string());
    }
    let url = match Url::parse(&format!(
        "{}/chat/completions",
        config.bob.base_url.trim_end_matches('/')
    )) {
        Ok(url) => url,
        Err(error) => {
            return res
                .send_openai_error(
                    502,
                    &format!("Cannot reach Bob gateway: {error}"),
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };
    let mut payload = body.clone();
    payload.insert("model", Value::string(model_id));
    let upstream = match proxy::post_json(
        client,
        &url,
        &upstream_headers(config, &key, &team_id),
        &payload,
        CHAT_TIMEOUT,
    ) {
        Ok(upstream) => upstream,
        Err(error) => {
            return res
                .send_openai_error(
                    502,
                    &format!("Cannot reach Bob gateway: {error}"),
                    "upstream_error",
                    None,
                )
                .map_err(|error| error.to_string());
        }
    };
    let original_model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("bob/{model_id}"));
    proxy::forward(
        res,
        upstream,
        "Bob gateway",
        &original_model,
        body.get("stream").and_then(Value::as_bool) == Some(true),
    )
}

pub fn list_models(config: &Config, client: &HttpClient) -> Vec<Value> {
    let base = config.bob.base_url.trim_end_matches('/');
    if let Some(list) = cached_models(base, TTL) {
        return list;
    }
    let key = api_key(config);
    if key.is_empty() {
        return Vec::new();
    }
    let team_id = resolve_team_id(config, client, &key);
    if let Some(list) = fetch_models(base, config, client, &key, &team_id) {
        if let Ok(mut guard) = cache().lock() {
            guard.models = Some((base.to_string(), Instant::now(), list.clone()));
        }
        return list;
    }
    cached_models(base, Duration::MAX).unwrap_or_default()
}

fn cached_models(scope: &str, ttl: Duration) -> Option<Vec<Value>> {
    let guard = cache().lock().ok()?;
    let (cached_scope, at, list) = guard.models.as_ref()?;
    if cached_scope == scope && at.elapsed() < ttl {
        return Some(list.clone());
    }
    None
}

fn bob_caps(info: &Value) -> Value {
    let mut caps = Value::obj();
    crate::backend::cap_int(info, "max_input_tokens", &mut caps, "context_window");
    crate::backend::cap_int(info, "max_output_tokens", &mut caps, "max_output_tokens");
    crate::backend::cap_bool(info, "supports_vision", &mut caps, "supports_vision");
    crate::backend::cap_bool(
        info,
        "supports_prompt_caching",
        &mut caps,
        "supports_prompt_caching",
    );
    crate::backend::cap_f64(
        info,
        "input_cost_per_token",
        &mut caps,
        "input_cost_per_token",
    );
    crate::backend::cap_f64(
        info,
        "output_cost_per_token",
        &mut caps,
        "output_cost_per_token",
    );
    caps
}

fn fetch_models(
    base: &str,
    config: &Config,
    client: &HttpClient,
    key: &str,
    team_id: &str,
) -> Option<Vec<Value>> {
    let url = Url::parse(&format!("{base}/model/info")).ok()?;
    let mut response = client
        .request(
            "GET",
            &url,
            &upstream_headers(config, key, team_id),
            None,
            Some(PROFILE_TIMEOUT),
        )
        .ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    let body = response.read_all().ok()?;
    let value = Value::parse(&body).ok()?;
    let data = value.get("data")?.as_array()?;
    let created = crate::util::now_unix() as i64;
    let mut list = Vec::new();
    for model in data {
        let Some(id) = model.get("model_name").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let mut entry = Value::obj();
        entry.insert("id", Value::string(format!("bob/{id}")));
        entry.insert("object", Value::string("model"));
        entry.insert("created", Value::Int(created));
        entry.insert("owned_by", Value::string("bob"));
        if let Some(info) = model.get("model_info") {
            crate::backend::attach_caps(&mut entry, bob_caps(info));
        }
        list.push(entry);
    }
    if list.is_empty() {
        None
    } else {
        Some(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_is_stripped() {
        assert_eq!(bob_model_id("bob/ibm/granite").unwrap(), "ibm/granite");
        assert!(bob_model_id("bob/").is_none());
        assert!(bob_model_id("kilo/x").is_none());
    }

    #[test]
    fn admin_base_is_derived_from_inference_path() {
        assert_eq!(
            admin_base("https://api.us-east.bob.ibm.com/inference/v1").unwrap(),
            "https://api.us-east.bob.ibm.com/admin/v1"
        );
    }

    #[test]
    fn bob_auth_uses_apikey_scheme() {
        let config = Config::defaults();
        let headers = auth_headers(&config, "secret");
        let auth = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("apikey secret"));
    }

    #[test]
    fn caps_keep_only_reported_fields() {
        let info = Value::parse_str(
            r#"{"max_input_tokens":131072,"max_output_tokens":65536,
                "supports_vision":true,"input_cost_per_token":0,
                "output_cost_per_token":0.000002}"#,
        )
        .unwrap();
        let caps = bob_caps(&info);
        assert_eq!(caps.get("context_window").unwrap().as_i64(), Some(131072));
        assert_eq!(
            caps.get("max_output_tokens").unwrap().as_i64(),
            Some(65536)
        );
        assert_eq!(
            caps.get("supports_vision").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            caps.get("output_cost_per_token").unwrap().as_f64(),
            Some(0.000002)
        );
        assert!(caps.get("supports_prompt_caching").is_none());
    }
}
