//! Small shared helpers: UTC stamps, UUID-shaped ids, path/env parsing, and
//! logging. No chrono, no uuid crate.

use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Howard Hinnant's civil_from_days, valid for the proleptic Gregorian range
/// we care about.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + if m <= 2 { 1 } else { 0 }, m as u32, d as u32)
}

pub fn utc_parts() -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = now_unix() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let millis = (now_nanos() % 1_000_000_000) as u32 / 1_000_000;
    (
        year,
        month,
        day,
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
        millis,
    )
}

pub fn utc_date() -> String {
    let (y, m, d, _, _, _, _) = utc_parts();
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn utc_iso() -> String {
    let (y, m, d, hh, mm, ss, ms) = utc_parts();
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{ms:03}Z")
}

pub fn stamp() -> String {
    let (y, m, d, hh, mm, ss, ms) = utc_parts();
    format!("[{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}.{ms:03}]")
}

pub fn log_line(level: char, message: &str) {
    let stderr = level != 'I';
    let color = color_enabled(stderr);
    let style = match level {
        'I' => "1;32",
        'W' => "1;33",
        'E' => "1;31",
        _ => "0",
    };
    let line = if color {
        format!(
            "\x1b[2m{}\x1b[0m \x1b[{style}m[{level}]\x1b[0m {message}\n",
            stamp()
        )
    } else {
        format!("{} [{level}] {message}\n", stamp())
    };
    if stderr {
        eprint!("{line}");
    } else {
        print!("{line}");
    }
}

pub fn color_enabled(stderr: bool) -> bool {
    if std::env::var_os("NO_COLOR")
        .map(|value| !value.is_empty())
        .unwrap_or(false)
    {
        return false;
    }
    if std::env::var_os("TERM")
        .map(|value| value.to_string_lossy() == "dumb")
        .unwrap_or(false)
    {
        return false;
    }
    if stderr {
        std::io::stderr().is_terminal()
    } else {
        std::io::stdout().is_terminal()
    }
}

pub fn paint(text: &str, style: &str, stderr: bool) -> String {
    if color_enabled(stderr) {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn icon(pretty: &'static str, ascii: &'static str, stderr: bool) -> &'static str {
    if color_enabled(stderr) {
        pretty
    } else {
        ascii
    }
}

macro_rules! logi {
    ($($arg:tt)*) => { $crate::util::log_line('I', &format!($($arg)*)) };
}
macro_rules! logw {
    ($($arg:tt)*) => { $crate::util::log_line('W', &format!($($arg)*)) };
}
macro_rules! loge {
    ($($arg:tt)*) => { $crate::util::log_line('E', &format!($($arg)*)) };
}
pub(crate) use {loge, logi, logw};

/// v5-shaped UUID derived from SHA-256.
pub fn uuid_from_string(value: &str) -> String {
    let digest = crate::crypto::sha256(value.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn mask_secret(value: &str) -> String {
    let start = match value.char_indices().rev().nth(3) {
        Some((index, _)) if value.len() > 8 => index,
        _ => return "********".to_string(),
    };
    format!("****{}", &value[start..])
}

fn next_char_boundary(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while index < value.len() && !value.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Best-effort secret scrub for logs. Keeps the shape, removes the value.
pub fn scrub(text: &str) -> String {
    let mut out = text.to_string();
    let mut search_from = 0usize;
    loop {
        let lower = out.to_ascii_lowercase();
        let Some(rel) = lower[search_from..].find("bearer ") else {
            break;
        };
        let start = search_from + rel + "bearer ".len();
        let end = out[start..]
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
            .map(|n| start + n)
            .unwrap_or(out.len());
        if end > start {
            out.replace_range(start..end, "****");
        }
        search_from = next_char_boundary(&out, start + 4);
        if search_from >= out.len() {
            break;
        }
    }
    for marker in ["api_key", "api-key", "apikey", "authorization"] {
        let mut from = 0usize;
        loop {
            let lower = out.to_ascii_lowercase();
            let Some(rel) = lower[from..].find(marker) else { break };
            let after = from + rel + marker.len();
            let tail = &out[after..];
            let Some(sep) = tail.find(['=', ':']) else { break };
            let mut start = after + sep + 1;
            while start < out.len() && matches!(out.as_bytes()[start], b' ' | b'"' | b'\'' | b'\t') {
                start += 1;
            }
            let end = out[start..]
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',' || c == '}')
                .map(|n| start + n)
                .unwrap_or(out.len());
            if end > start {
                out.replace_range(start..end, "****");
            }
            from = next_char_boundary(&out, start + 4);
            if from >= out.len() {
                break;
            }
        }
    }
    out
}

pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    if let Ok(home) = std::env::var("USERPROFILE") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    PathBuf::from("/")
}

pub fn paig_home() -> PathBuf {
    home_dir().join(".paig")
}

pub fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(value)
}

pub fn resolve_path(cwd: &Path, value: &str) -> PathBuf {
    if value.is_empty() {
        return PathBuf::new();
    }
    let expanded = expand_home(value);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

pub fn parse_bool(value: Option<&str>, fallback: bool) -> bool {
    match value.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => true,
        Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off" | "") => false,
        _ => fallback,
    }
}

pub fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

pub fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

pub fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.mode() & 0o777)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_masking_respects_utf8_boundaries() {
        let masked = mask_secret("abcde😀x");
        assert!(masked.starts_with("****"));
        assert!(masked.ends_with('x'));
        assert_eq!(mask_secret("short"), "********");
        assert_eq!(mask_secret("😀😀😀"), "********");
        assert_eq!(mask_secret("秘密秘"), "********");
    }

    #[test]
    fn scrubbing_respects_utf8_boundaries() {
        let text = "Authorization: Bearer abc😀def api_key=😀secret";
        let scrubbed = scrub(text);
        assert!(!scrubbed.contains("abc😀def"));
        assert!(!scrubbed.contains("😀secret"));
        assert!(scrubbed.contains("****"));
    }
}
