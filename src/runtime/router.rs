//! Edge router: health, model catalogue, excluded-model guard, and backend
//! dispatch in the oracle's order.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::bob;
use crate::cc;
use crate::client::{HttpClient, Url};
use crate::config::{Config, DEFAULT_EXCLUDED_MODELS};
use crate::http::{Request, Response};
use crate::json::Value;
use crate::kilo;
use crate::mistral;
use crate::opencode;
use crate::util::{logi, uuid_from_string};

pub struct App {
    pub config: Config,
    pub client: HttpClient,
}

impl App {
    pub fn new(config: Config) -> Self {
        App {
            config,
            client: HttpClient::new(Duration::from_secs(180)),
        }
    }

    pub fn handle(&self, req: &mut Request, res: &mut Response) {
        let path = req.path.split('?').next().unwrap_or(&req.path);
        if req.method == "GET" && (path == "/" || path == "/health") {
            let mut body = Value::obj();
            body.insert("ok", Value::Bool(true));
            body.insert("service", Value::string("paig"));
            let _ = res.send_json(200, &body);
            return;
        }
        if req.method == "GET" && path.ends_with("/models") {
            self.handle_models(res);
            return;
        }
        if req.method == "POST" && path.ends_with("/chat/completions") {
            self.handle_chat(req, res);
            return;
        }
        let mut error = Value::obj();
        error.insert(
            "message",
            Value::string(format!("Unknown route: {} {}", req.method, path)),
        );
        error.insert("type", Value::string("invalid_request_error"));
        let mut root = Value::obj();
        root.insert("error", error);
        let _ = res.send_json(404, &root);
    }

    fn handle_chat(&self, req: &mut Request, res: &mut Response) {
        let body = match req.json() {
            Ok(value) => value,
            Err(error) => {
                let _ = res.send_openai_error(400, &error, "invalid_request_error", None);
                return;
            }
        };
        if !matches!(body, Value::Object(_)) {
            let _ = res.send_openai_error(
                400,
                "Expected a JSON object body",
                "invalid_request_error",
                None,
            );
            return;
        }

        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let cc_target = cc::cc_model_id(&model);
        let routed_model = cc_target.unwrap_or(model.as_str());
        if excluded(&self.config).contains(routed_model) {
            let message = format!(
                "Model {model} is not available here: it requires a higher plan (403 MODEL_NOT_IN_PLAN) or is dead upstream (404/502). See commandCode.excludedModels."
            );
            let _ = res.send_openai_error(404, &message, "invalid_request_error", Some("model_excluded"));
            return;
        }

        if let Some(target) = opencode::opencode_model_id(&model) {
            let result = opencode::handle(&body, res, &target, &self.config, &self.client);
            finish_or_error(res, result);
            return;
        }
        if let Some(model_id) = kilo::kilo_model_id(&model) {
            let result = kilo::handle(&body, res, &model_id, &self.config, &self.client);
            finish_or_error(res, result);
            return;
        }
        if let Some(model_id) = bob::bob_model_id(&model) {
            let result = bob::handle(&body, res, &model_id, &self.config, &self.client);
            finish_or_error(res, result);
            return;
        }
        if let Some(model_id) = mistral::mistral_model_id(&model) {
            let result = mistral::handle(&body, res, &model_id, &self.config, &self.client);
            finish_or_error(res, result);
            return;
        }
        if cc_target.is_some() {
            let result = cc::handle(&body, res, &self.config, &self.client);
            finish_or_error(res, result);
            return;
        }
        let message = if model.is_empty() {
            "model is required; use cc/<id>, opencode/<id>, kilo/<id>, bob/<id>, or mistral/<id>"
                .to_string()
        } else {
            format!(
                "Model {model} has no backend prefix; use cc/<id>, opencode/<id>, kilo/<id>, bob/<id>, or mistral/<id>"
            )
        };
        let _ = res.send_openai_error(
            400,
            &message,
            "invalid_request_error",
            Some("model_unrouted"),
        );
    }

    fn handle_models(&self, res: &mut Response) {
        let created = crate::util::now_unix() as i64;
        let mut data: Vec<Value> = self
            .config
            .command_code
            .models
            .clone()
            .unwrap_or_default()
            .into_iter()
            .filter(|id| !id.trim().is_empty())
            .map(|id| {
                let mut model = Value::obj();
                model.insert("id", Value::string(cc_public_id(&id)));
                model.insert("object", Value::string("model"));
                model.insert("created", Value::Int(created));
                model.insert("owned_by", Value::string("command-code"));
                model
            })
            .collect();

        if !self.config.command_code.api_key.is_empty() {
            let url = format!(
                "{}/provider/v1/models",
                self.config.command_code.base_url.trim_end_matches('/')
            );
            if let Ok(url) = Url::parse(&url) {
                let headers = command_code_headers(&self.config);
                if let Ok(mut response) = self
                    .client
                    .request("GET", &url, &headers, None, Some(Duration::from_secs(3)))
                {
                    if response.status == 200 {
                        if let Ok(body) = response.read_all() {
                            if let Ok(value) = Value::parse(&body) {
                                if let Some(list) = value.get("data").and_then(Value::as_array) {
                                    if !list.is_empty() {
                                        data = list
                                            .iter()
                                            .filter_map(|model| {
                                                let id = model.get("id").and_then(Value::as_str)?;
                                                let mut entry = Value::obj();
                                                entry.insert("id", Value::string(cc_public_id(id)));
                                                entry.insert("object", Value::string("model"));
                                                entry.insert(
                                                    "created",
                                                    Value::Int(
                                                        model
                                                            .get("created")
                                                            .and_then(Value::as_i64)
                                                            .unwrap_or(created),
                                                    ),
                                                );
                                                entry.insert(
                                                    "owned_by",
                                                    Value::string(
                                                        model
                                                            .get("owned_by")
                                                            .and_then(Value::as_str)
                                                            .unwrap_or("command-code"),
                                                    ),
                                                );
                                                Some(entry)
                                            })
                                            .collect();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        data.extend(opencode::list_models(&self.config));
        data.extend(kilo::list_models(&self.config, &self.client));
        data.extend(bob::list_models(&self.config, &self.client));
        data.extend(mistral::list_models(&self.config, &self.client));

        let excluded = excluded(&self.config);
        if !excluded.is_empty() {
            data.retain(|model| {
                model
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| {
                        let bare = cc::cc_model_id(id).unwrap_or(id);
                        !excluded.contains(id) && !excluded.contains(bare)
                    })
                    .unwrap_or(true)
            });
        }
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut deduped: Vec<Value> = Vec::with_capacity(data.len());
        for model in data {
            let Some(id) = model.get("id").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            // TODO(paig): keep this last-wins to match dispatch order, but
            // revisit the Mistral/Command-Code id collision if both copies ever
            // need to remain addressable through PAIG.
            if let Some(index) = by_id.get(&id) {
                deduped[*index] = model;
            } else {
                by_id.insert(id, deduped.len());
                deduped.push(model);
            }
        }
        let mut root = Value::obj();
        root.insert("object", Value::string("list"));
        root.insert("data", Value::Array(deduped));
        let _ = res.send_json(200, &root);
    }
}

fn cc_public_id(id: &str) -> String {
    match cc::cc_model_id(id) {
        Some(bare) => format!("cc/{bare}"),
        None => format!("cc/{}", id.trim()),
    }
}

fn excluded(config: &Config) -> HashSet<String> {
    match &config.command_code.excluded_models {
        Some(list) => list.iter().cloned().collect(),
        None => DEFAULT_EXCLUDED_MODELS
            .iter()
            .map(|value| value.to_string())
            .collect(),
    }
}

fn finish_or_error(res: &mut Response, result: Result<(), String>) {
    if let Err(error) = result {
        if !res.headers_sent {
            let _ = res.send_openai_error(502, &error, "upstream_error", None);
        } else {
            let _ = res.sse_end();
        }
    }
}

fn command_code_headers(config: &Config) -> Vec<(String, String)> {
    let mut headers = config.command_code.headers.clone().unwrap_or_default();
    cc::set_header(
        &mut headers,
        "content-type",
        "application/json".to_string(),
    );
    cc::set_header(
        &mut headers,
        "authorization",
        format!("Bearer {}", config.command_code.api_key),
    );
    cc::set_header(
        &mut headers,
        "x-project-slug",
        if config.command_code.project_slug.is_empty() {
            "paig".to_string()
        } else {
            config.command_code.project_slug.clone()
        },
    );
    cc::set_header(
        &mut headers,
        "x-session-id",
        uuid_from_string("paig-models"),
    );
    headers
}

pub fn handler(app: Arc<App>) -> impl Fn(&mut Request, &mut Response) + Send + Sync + 'static {
    move |req, res| {
        app.handle(req, res);
        if app.config.debug {
            logi!("request {} {}", req.method, req.path);
        }
    }
}
