//! models.dev fallback metadata. The backend endpoints stay authoritative; this
//! only fills capability fields a backend did not report.
//!
//! The full catalogue is ~6 MB and downloads slowly through the TLS shim, so
//! the live gateway refreshes it in the background and keeps a disk copy. A
//! stale copy is served immediately while the refresh runs; the first request
//! after a cold start simply has no enrichment yet.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::client::{HttpClient, Url};
use crate::config::Config;
use crate::json::Value;

pub const DEFAULT_URL: &str = "https://models.dev/api.json";
const TTL: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY: Duration = Duration::from_secs(5 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

pub type Catalogue = HashMap<String, HashMap<String, MdCaps>>;

/// Public prefix -> models.dev provider. The router uses this same table, so
/// the catalogue filter cannot drift from dispatch. The catalogue holds 9k+
/// models across 200+ providers; converting the rest would only burn CPU and
/// memory on entries no lookup can reach.
pub const PROVIDER_PREFIXES: &[(&str, &str)] = &[
    ("cc", "commandcode"),
    ("oc", "opencode"),
    ("opencode", "opencode"),
    ("oc-go", "opencode-go"),
    ("opencode-go", "opencode-go"),
    ("kilocode", "kilo"),
    ("kilo", "kilo"),
    ("mistral", "mistral"),
];

pub fn provider_for(prefix: &str) -> Option<&'static str> {
    PROVIDER_PREFIXES
        .iter()
        .find_map(|(name, provider)| (*name == prefix).then_some(*provider))
}

fn wanted_provider(name: &str) -> bool {
    PROVIDER_PREFIXES
        .iter()
        .any(|(_, provider)| *provider == name)
}

#[derive(Debug, Clone, Default)]
pub struct MdCaps {
    pub context_window: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub supports_function_calling: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub supports_vision: Option<bool>,
    pub input_cost_per_token: Option<f64>,
    pub output_cost_per_token: Option<f64>,
}

static CACHE: OnceLock<Mutex<Option<(Instant, Option<Arc<Catalogue>>)>>> = OnceLock::new();
static FETCHING: AtomicBool = AtomicBool::new(false);

fn cache() -> &'static Mutex<Option<(Instant, Option<Arc<Catalogue>>)>> {
    CACHE.get_or_init(|| Mutex::new(None))
}

fn cached() -> Option<Option<Arc<Catalogue>>> {
    let guard = cache().lock().ok()?;
    let (at, list) = guard.as_ref()?;
    let ttl = if list.is_some() { TTL } else { RETRY };
    if at.elapsed() < ttl {
        return Some(list.clone());
    }
    None
}

fn store(list: Option<Arc<Catalogue>>) {
    if let Ok(mut guard) = cache().lock() {
        *guard = Some((Instant::now(), list));
    }
}

/// Keep serving a stale disk copy only until the refresh has had a chance to
/// finish; after that the next request retries instead of trusting it forever.
fn store_stale(list: Arc<Catalogue>) {
    if let Ok(mut guard) = cache().lock() {
        let at = Instant::now()
            .checked_sub(TTL - RETRY)
            .unwrap_or_else(Instant::now);
        *guard = Some((at, Some(list)));
    }
}

fn disk_path() -> PathBuf {
    crate::util::paig_home().join("models-dev.json")
}

pub fn catalogue(config: &Config, client: &HttpClient) -> Option<Arc<Catalogue>> {
    if let Some(hit) = cached() {
        return hit;
    }
    let url = config.models_dev.url.trim();
    if url.is_empty() {
        return None;
    }
    if config.models_dev.blocking {
        let found = fetch(url, client).map(|(list, _)| Arc::new(list));
        store(found.clone());
        return found;
    }
    if url == DEFAULT_URL {
        if let Some((fresh, list)) = load_disk() {
            let list = Arc::new(list);
            if fresh {
                store(Some(list.clone()));
                return Some(list);
            }
            store_stale(list.clone());
            refresh(url);
            return Some(list);
        }
    }
    refresh(url);
    None
}

fn refresh(url: &str) {
    if FETCHING.swap(true, Ordering::SeqCst) {
        return;
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        let client = HttpClient::new(Duration::from_secs(60));
        let found = fetch(&url, &client);
        match found {
            Some((list, body)) => {
                if url == DEFAULT_URL {
                    write_disk(&body);
                }
                store(Some(Arc::new(list)));
            }
            None => store(None),
        }
        FETCHING.store(false, Ordering::SeqCst);
    });
}

fn load_disk() -> Option<(bool, Catalogue)> {
    load_disk_at(&disk_path())
}

fn load_disk_at(path: &Path) -> Option<(bool, Catalogue)> {
    let meta = fs::metadata(path).ok()?;
    let age = meta.modified().ok()?.elapsed().unwrap_or(Duration::MAX);
    let body = fs::read(path).ok()?;
    let value = Value::parse(&body).ok()?;
    let list = parse_catalogue(&value)?;
    Some((age < TTL, list))
}

fn write_disk(body: &[u8]) {
    write_disk_at(&disk_path(), body);
}

fn write_disk_at(path: &Path, body: &[u8]) {
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, body).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

fn fetch(url: &str, client: &HttpClient) -> Option<(Catalogue, Vec<u8>)> {
    let url = Url::parse(url).ok()?;
    let started = Instant::now();
    let mut response = client
        .request("GET", &url, &[], None, Some(FETCH_TIMEOUT))
        .ok()?;
    let connected = started.elapsed();
    if !(200..300).contains(&response.status) {
        return None;
    }
    let body = response.read_all().ok()?;
    let downloaded = started.elapsed();
    let value = Value::parse(&body).ok()?;
    let parsed = started.elapsed();
    crate::util::logi!(
        "models.dev: connect {:?}, download {:?}, parse {:?}, bytes {}",
        connected,
        downloaded - connected,
        parsed - downloaded,
        body.len()
    );
    let list = parse_catalogue(&value)?;
    Some((list, body))
}

fn parse_catalogue(value: &Value) -> Option<Catalogue> {
    let providers = value.as_object()?;
    let mut out = HashMap::with_capacity(PROVIDER_PREFIXES.len());
    for (provider, entry) in providers {
        if !wanted_provider(provider) {
            continue;
        }
        let Some(models) = entry.get("models").and_then(Value::as_object) else {
            continue;
        };
        let mut by_id = HashMap::with_capacity(models.len());
        for (id, model) in models {
            by_id.insert(id.clone(), md_caps(model));
        }
        out.insert(provider.clone(), by_id);
    }
    if out.is_empty() {
        return None;
    }
    Some(out)
}

pub fn lookup<'a>(catalogue: &'a Catalogue, provider: &str, id: &str) -> Option<&'a MdCaps> {
    catalogue.get(provider)?.get(id)
}

fn md_caps(model: &Value) -> MdCaps {
    let mut caps = MdCaps::default();
    if let Some(limit) = model.get("limit") {
        caps.context_window = limit.get("context").and_then(Value::as_i64);
        caps.max_output_tokens = limit.get("output").and_then(Value::as_i64);
    }
    caps.supports_function_calling = model.get("tool_call").and_then(Value::as_bool);
    caps.supports_reasoning = model.get("reasoning").and_then(Value::as_bool);
    caps.supports_vision = model.get("vision").and_then(Value::as_bool);
    if caps.supports_vision.is_none() {
        if let Some(inputs) = model
            .get("modalities")
            .and_then(|value| value.get("input"))
            .and_then(Value::as_array)
        {
            caps.supports_vision = Some(inputs.iter().any(|value| {
                value
                    .as_str()
                    .map(|name| name == "image" || name.starts_with("image/"))
                    .unwrap_or(false)
            }));
        }
    }
    if let Some(cost) = model.get("cost") {
        caps.input_cost_per_token = cost
            .get("input")
            .and_then(Value::as_f64)
            .map(|value| value / 1_000_000.0);
        caps.output_cost_per_token = cost
            .get("output")
            .and_then(Value::as_f64)
            .map(|value| value / 1_000_000.0);
    }
    caps
}

pub fn enrich_model(entry: &mut Value, md: &MdCaps) {
    let mut caps = entry
        .get("capabilities")
        .cloned()
        .unwrap_or_else(Value::obj);
    let mut filled = false;
    if caps.get("context_window").is_none() {
        if let Some(value) = md.context_window {
            caps.insert("context_window", Value::Int(value));
            filled = true;
        }
    }
    if caps.get("max_output_tokens").is_none() {
        if let Some(value) = md.max_output_tokens {
            caps.insert("max_output_tokens", Value::Int(value));
            filled = true;
        }
    }
    if caps.get("supports_function_calling").is_none() {
        if let Some(value) = md.supports_function_calling {
            caps.insert("supports_function_calling", Value::Bool(value));
            filled = true;
        }
    }
    if caps.get("supports_reasoning").is_none() {
        if let Some(value) = md.supports_reasoning {
            caps.insert("supports_reasoning", Value::Bool(value));
            filled = true;
        }
    }
    if caps.get("supports_vision").is_none() {
        if let Some(value) = md.supports_vision {
            caps.insert("supports_vision", Value::Bool(value));
            filled = true;
        }
    }
    if caps.get("input_cost_per_token").is_none() {
        if let Some(value) = md.input_cost_per_token {
            caps.insert("input_cost_per_token", Value::Float(value));
            filled = true;
        }
    }
    if caps.get("output_cost_per_token").is_none() {
        if let Some(value) = md.output_cost_per_token {
            caps.insert("output_cost_per_token", Value::Float(value));
            filled = true;
        }
    }
    if filled {
        caps.insert("estimated", Value::Bool(true));
    }
    crate::backend::attach_caps(entry, caps);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalogue {
        let value = Value::parse_str(
            r#"{
                "commandcode":{"models":{"test-model":{
                    "limit":{"context":1000000,"output":128000},
                    "tool_call":true,"reasoning":true,"vision":null,
                    "modalities":{"input":["text","image"]},
                    "cost":{"input":3,"output":15}
                }}},
                "opencode":{"models":{"beam-free":{
                    "limit":{"context":1048576,"output":131072},
                    "tool_call":true,"reasoning":true,
                    "modalities":{"input":["text"]},
                    "cost":{"input":0,"output":0}
                }}},
                "unrouted":{"models":{"hidden":{
                    "limit":{"context":1}
                }}}
            }"#,
        )
        .unwrap();
        parse_catalogue(&value).unwrap()
    }

    #[test]
    fn provider_table_matches_dispatch_prefixes() {
        for (prefix, provider) in PROVIDER_PREFIXES {
            assert_eq!(provider_for(prefix), Some(*provider));
            assert!(wanted_provider(provider));
        }
        assert_eq!(provider_for("other"), None);
    }

    #[test]
    fn catalogue_without_known_providers_is_rejected() {
        let value = Value::parse_str(
            r#"{"other":{"models":{"hidden":{"limit":{"context":1}}}}}"#,
        )
        .unwrap();
        assert!(parse_catalogue(&value).is_none());
    }

    #[test]
    fn parses_provider_model_keys() {
        let cat = sample();
        let cc = lookup(&cat, "commandcode", "test-model").unwrap();
        assert_eq!(cc.context_window, Some(1000000));
        assert_eq!(cc.max_output_tokens, Some(128000));
        assert_eq!(cc.supports_function_calling, Some(true));
        assert_eq!(cc.supports_vision, Some(true));
        assert_eq!(cc.input_cost_per_token, Some(0.000003));
        let opencode = lookup(&cat, "opencode", "beam-free").unwrap();
        assert_eq!(opencode.context_window, Some(1048576));
        assert_eq!(opencode.supports_vision, Some(false));
        assert!(lookup(&cat, "unrouted", "hidden").is_none());
    }

    #[test]
    fn enrichment_never_overwrites_upstream() {
        let cat = sample();
        let mut entry = Value::parse_str(
            r#"{"id":"cc/test-model","capabilities":{"context_window":999,"supports_reasoning":false}}"#,
        )
        .unwrap();
        let md = lookup(&cat, "commandcode", "test-model").unwrap();
        enrich_model(&mut entry, md);
        let caps = entry.get("capabilities").unwrap();
        assert_eq!(caps.get("context_window").unwrap().as_i64(), Some(999));
        assert_eq!(caps.get("max_output_tokens").unwrap().as_i64(), Some(128000));
        assert_eq!(
            caps.get("supports_function_calling").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(
            caps.get("supports_reasoning").unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn enrichment_creates_capabilities_when_absent() {
        let cat = sample();
        let mut entry = Value::parse_str(r#"{"id":"opencode/beam-free"}"#).unwrap();
        let md = lookup(&cat, "opencode", "beam-free").unwrap();
        enrich_model(&mut entry, md);
        let caps = entry.get("capabilities").unwrap();
        assert_eq!(caps.get("context_window").unwrap().as_i64(), Some(1048576));
        assert_eq!(
            caps.get("supports_function_calling").unwrap().as_bool(),
            Some(true)
        );
        assert_eq!(caps.get("estimated").unwrap().as_bool(), Some(true));
    }

    #[test]
    fn disk_cache_roundtrips_and_expires() {
        let dir = std::env::temp_dir().join(format!(
            "paig-md-test-{}-{}",
            std::process::id(),
            crate::util::now_unix()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("models-dev.json");
        let body = br#"{"commandcode":{"models":{"m":{
            "limit":{"context":7},
            "modalities":{"input":["text","image"]}
        }}}}"#;
        write_disk_at(&path, body);
        let (fresh, cat) = load_disk_at(&path).unwrap();
        assert!(fresh);
        let entry = lookup(&cat, "commandcode", "m").unwrap();
        assert_eq!(entry.context_window, Some(7));
        assert_eq!(entry.supports_vision, Some(true));

        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(25 * 60 * 60);
        file.set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        let (fresh, _) = load_disk_at(&path).unwrap();
        assert!(!fresh);
        let _ = fs::remove_dir_all(&dir);
    }
}
