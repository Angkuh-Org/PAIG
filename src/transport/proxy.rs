//! Shared HTTP passthrough for OpenAI-shaped backends (Kilo, Bob, OpenCode in
//! raw mode). Keeps unknown request fields, rewrites only `model`, and streams
//! SSE chunks without buffering the whole response.

use std::time::Duration;

use crate::client::{HttpClient, Response as UpstreamResponse, Url};
use crate::http::Response;
use crate::json::Value;

pub fn post_json(
    client: &HttpClient,
    url: &Url,
    headers: &[(String, String)],
    payload: &Value,
    timeout: Duration,
) -> Result<UpstreamResponse, String> {
    let body = payload.stringify();
    client
        .request("POST", url, headers, Some(body.as_bytes()), Some(timeout))
        .map_err(|error| error.to_string())
}

pub fn forward(
    res: &mut Response,
    mut upstream: UpstreamResponse,
    label: &str,
    original_model: &str,
    streaming: bool,
) -> Result<(), String> {
    if !(200..300).contains(&upstream.status) {
        let body = upstream.read_all().unwrap_or_default();
        let content_type = upstream
            .header("content-type")
            .unwrap_or("application/json")
            .to_string();
        let status = if (400..600).contains(&upstream.status) {
            upstream.status
        } else {
            502
        };
        return res
            .send_bytes(status, &content_type, &body)
            .map_err(|error| error.to_string());
    }

    if streaming {
        res.start_sse().map_err(|error| error.to_string())?;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match upstream.read_chunk(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if res.sse_write(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = res.sse_write(
                        format!("data: {{\"error\":{{\"message\":\"{} stream error\",\"type\":\"upstream_error\"}}}}\n\n", label)
                            .as_bytes(),
                    );
                    crate::util::logw!("{label} stream error: {error}");
                    break;
                }
            }
        }
        return res.sse_end().map_err(|error| error.to_string());
    }

    let body = upstream.read_all().map_err(|error| error.to_string())?;
    let mut value = match Value::parse(&body) {
        Ok(value) => value,
        Err(_) => {
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
    if let Value::Object(_) = value {
        if value.get("model").is_some() {
            value.insert("model", Value::string(original_model));
        }
    }
    res.send_json(200, &value).map_err(|error| error.to_string())
}
