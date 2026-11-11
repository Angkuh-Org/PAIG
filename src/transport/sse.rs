//! Server-Sent Events parsing. PAIG uses it in two ways: Command Code events
//! are translated, while passthrough backends forward frames and observe
//! finish/usage metadata.

use crate::json::Value;

#[derive(Debug, Clone)]
pub struct Event {
    pub data: String,
}

pub struct Parser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl Parser {
    pub fn new() -> Self {
        Parser {
            buf: Vec::new(),
            data: Vec::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            let Some(pos) = self.buf.iter().position(|&b| b == b'\n') else {
                break;
            };
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let mut line = &line[..line.len().saturating_sub(1)];
            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }
            if let Some(event) = self.line(line) {
                events.push(event);
            }
        }
        events
    }

    pub fn finish(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            let mut line = line.as_slice();
            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }
            if let Some(event) = self.line(line) {
                events.push(event);
            }
        }
        if let Some(event) = self.dispatch() {
            events.push(event);
        }
        events
    }

    fn line(&mut self, line: &[u8]) -> Option<Event> {
        if line.is_empty() {
            return self.dispatch();
        }
        let text = String::from_utf8_lossy(line);
        if text.starts_with(':') {
            return None;
        }
        let (field, value) = text.split_once(':').unwrap_or((&text, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        if field == "data" {
            self.data.push(value.to_string());
        }
        None
    }

    fn dispatch(&mut self) -> Option<Event> {
        if self.data.is_empty() {
            return None;
        }
        let event = Event {
            data: self.data.join("\n"),
        };
        self.data.clear();
        Some(event)
    }
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the JSON payload of a `data:` line. Returns None for `[DONE]` and
/// malformed frames.
pub fn data_json(line: &str) -> Option<Value> {
    let text = line.trim();
    let text = text.strip_prefix("data:").unwrap_or(text).trim();
    if text.is_empty() || text == "[DONE]" {
        return None;
    }
    Value::parse_str(text).ok()
}
