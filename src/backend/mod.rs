use crate::json::Value;

pub mod bob;
pub mod cc;
pub mod kilo;
pub mod mistral;
pub mod models_dev;
pub mod opencode;

/// Copy an upstream integer into the normalized capability object.
pub fn cap_int(src: &Value, from: &str, caps: &mut Value, to: &str) {
    if let Some(value) = src.get(from).and_then(Value::as_i64) {
        caps.insert(to, Value::Int(value));
    }
}

/// Copy an upstream boolean. Absent keys stay absent: "not reported" is not
/// the same claim as "not supported".
pub fn cap_bool(src: &Value, from: &str, caps: &mut Value, to: &str) {
    if let Some(value) = src.get(from).and_then(Value::as_bool) {
        caps.insert(to, Value::Bool(value));
    }
}

pub fn cap_f64(src: &Value, from: &str, caps: &mut Value, to: &str) {
    if let Some(value) = src.get(from).and_then(Value::as_f64) {
        caps.insert(to, Value::Float(value));
    }
}

/// Attach capabilities only when the backend actually reported some.
pub fn attach_caps(entry: &mut Value, caps: Value) {
    if caps
        .as_object()
        .map(|fields| !fields.is_empty())
        .unwrap_or(false)
    {
        entry.insert("capabilities", caps);
    }
}
