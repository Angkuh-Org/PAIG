//! Minimal owned JSON DOM. No serde, no allocation tricks we cannot explain.
//! Objects keep insertion order in a Vec; lookups are linear because PAIG's
//! objects are small. Parsing scans small objects by hand and only builds an
//! index past a threshold, so duplicate keys stay last-wins without quadratic
//! behavior on adversarial objects.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

impl Value {
    pub fn obj() -> Self {
        Value::Object(Vec::new())
    }

    pub fn string<S: Into<String>>(s: S) -> Self {
        Value::Str(s.into())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(items) => items.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match self {
            Value::Object(items) => items.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Float(v) if v.is_finite() => Some(*v as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Float(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_array_mut(&mut self) -> Option<&mut Vec<Value>> {
        match self {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Value)]> {
        match self {
            Value::Object(items) => Some(items),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn insert<K: Into<String>>(&mut self, key: K, value: Value) {
        let key = key.into();
        if let Value::Object(items) = self {
            if let Some((_, slot)) = items.iter_mut().find(|(k, _)| *k == key) {
                *slot = value;
            } else {
                items.push((key, value));
            }
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        if let Value::Object(items) = self {
            if let Some(index) = items.iter().position(|(k, _)| k == key) {
                return Some(items.remove(index).1);
            }
        }
        None
    }

    pub fn stringify(&self) -> String {
        let mut out = Vec::with_capacity(self.serialized_len_hint());
        self.write_to(&mut out).expect("Vec writes cannot fail");
        // SAFETY: every branch of write_to emits valid UTF-8: string bodies are
        // copied from `&str`, while escapes, keys, and numbers are ASCII.
        unsafe { String::from_utf8_unchecked(out) }
    }

    /// Lower-bound size of `stringify()` output. Numbers reserve their widest
    /// form and escapes reserve none, so the writer may still grow once; the
    /// point is to avoid the 8/16/32/... allocation ladder before the copy.
    fn serialized_len_hint(&self) -> usize {
        match self {
            Value::Null => 4,
            Value::Bool(true) => 4,
            Value::Bool(false) => 5,
            Value::Int(_) => 20,
            Value::Float(_) => 24,
            Value::Str(s) => s.len() + 2,
            Value::Array(items) => {
                2 + items
                    .iter()
                    .map(|item| item.serialized_len_hint() + 1)
                    .sum::<usize>()
            }
            Value::Object(items) => {
                2 + items
                    .iter()
                    .map(|(key, value)| key.len() + 3 + value.serialized_len_hint())
                    .sum::<usize>()
            }
        }
    }

    pub fn write_to<W: Write>(&self, out: &mut W) -> io::Result<()> {
        match self {
            Value::Null => out.write_all(b"null"),
            Value::Bool(true) => out.write_all(b"true"),
            Value::Bool(false) => out.write_all(b"false"),
            Value::Int(v) => write_i64(out, *v),
            Value::Float(v) => {
                if !v.is_finite() {
                    out.write_all(b"null")
                } else {
                    write!(out, "{v}")
                }
            }
            Value::Str(s) => write_json_string(out, s),
            Value::Array(items) => {
                out.write_all(b"[")?;
                for (index, item) in items.iter().enumerate() {
                    if index != 0 {
                        out.write_all(b",")?;
                    }
                    item.write_to(out)?;
                }
                out.write_all(b"]")
            }
            Value::Object(items) => {
                out.write_all(b"{")?;
                for (index, (key, value)) in items.iter().enumerate() {
                    if index != 0 {
                        out.write_all(b",")?;
                    }
                    write_json_string(out, key)?;
                    out.write_all(b":")?;
                    value.write_to(out)?;
                }
                out.write_all(b"}")
            }
        }
    }

    pub fn parse(input: &[u8]) -> Result<Value, JsonError> {
        let mut parser = Parser {
            bytes: input,
            pos: 0,
            depth: 0,
        };
        let value = parser.parse_value()?;
        parser.skip_ws();
        if parser.pos != input.len() {
            return Err(parser.error("trailing characters"));
        }
        Ok(value)
    }

    pub fn parse_str(input: &str) -> Result<Value, JsonError> {
        Value::parse(input.as_bytes())
    }
}

const SWAR_ONES: u64 = 0x0101_0101_0101_0101;
const SWAR_HIGHS: u64 = 0x8080_8080_8080_8080;
const HEX: &[u8; 16] = b"0123456789abcdef";

#[inline]
fn zero_byte(word: u64) -> u64 {
    word.wrapping_sub(SWAR_ONES) & !word & SWAR_HIGHS
}

/// First byte JSON cannot copy verbatim: `"`, `\`, or a control byte below
/// 0x20. Eight bytes per iteration. A borrow from a low byte only sets flags
/// above it, and the first flagged byte is the first true hit, so the lowest
/// set bit is the byte to stop on.
#[inline]
fn find_special(bytes: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 8 <= bytes.len() {
        let word = u64::from_le_bytes(bytes[index..index + 8].try_into().unwrap());
        let quote = zero_byte(word ^ 0x2222_2222_2222_2222);
        let backslash = zero_byte(word ^ 0x5c5c_5c5c_5c5c_5c5c);
        let control = word.wrapping_sub(0x2020_2020_2020_2020) & !word & SWAR_HIGHS;
        let hits = quote | backslash | control;
        if hits != 0 {
            return Some(index + (hits.trailing_zeros() as usize >> 3));
        }
        index += 8;
    }
    while index < bytes.len() {
        let byte = bytes[index];
        if byte < 0x20 || byte == b'"' || byte == b'\\' {
            return Some(index);
        }
        index += 1;
    }
    None
}

#[inline]
pub(crate) fn write_i64<W: Write>(out: &mut W, value: i64) -> io::Result<()> {
    let mut buf = [0u8; 20];
    let negative = value < 0;
    let mut magnitude = value.unsigned_abs();
    let mut index = buf.len();
    loop {
        index -= 1;
        buf[index] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        index -= 1;
        buf[index] = b'-';
    }
    out.write_all(&buf[index..])
}

pub(crate) fn write_json_string<W: Write>(out: &mut W, value: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    let bytes = value.as_bytes();
    let mut start = 0;
    while let Some(offset) = find_special(&bytes[start..]) {
        let index = start + offset;
        if index > start {
            out.write_all(&bytes[start..index])?;
        }
        let byte = bytes[index];
        match byte {
            b'"' => out.write_all(b"\\\"")?,
            b'\\' => out.write_all(b"\\\\")?,
            b'\x08' => out.write_all(b"\\b")?,
            b'\x0c' => out.write_all(b"\\f")?,
            b'\n' => out.write_all(b"\\n")?,
            b'\r' => out.write_all(b"\\r")?,
            b'\t' => out.write_all(b"\\t")?,
            _ => {
                let escape = [
                    b'\\',
                    b'u',
                    b'0',
                    b'0',
                    HEX[(byte >> 4) as usize],
                    HEX[(byte & 0xf) as usize],
                ];
                out.write_all(&escape)?;
            }
        }
        start = index + 1;
    }
    if start < bytes.len() {
        out.write_all(&bytes[start..])?;
    }
    out.write_all(b"\"")
}

#[derive(Debug)]
pub struct JsonError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at byte {}: {}", self.offset, self.message)
    }
}

impl std::error::Error for JsonError {}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> JsonError {
        JsonError {
            message: message.to_string(),
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn parse_value(&mut self) -> Result<Value, JsonError> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(Value::Str),
            Some(b't') => {
                self.expect_literal(b"true")?;
                Ok(Value::Bool(true))
            }
            Some(b'f') => {
                self.expect_literal(b"false")?;
                Ok(Value::Bool(false))
            }
            Some(b'n') => {
                self.expect_literal(b"null")?;
                Ok(Value::Null)
            }
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    fn expect_literal(&mut self, literal: &[u8]) -> Result<(), JsonError> {
        if self.bytes.len() - self.pos < literal.len()
            || &self.bytes[self.pos..self.pos + literal.len()] != literal
        {
            return Err(self.error("invalid literal"));
        }
        self.pos += literal.len();
        Ok(())
    }

    fn parse_object(&mut self) -> Result<Value, JsonError> {
        self.enter()?;
        self.pos += 1; // {
        let mut items = Vec::new();
        // Objects are small, so scanning by hand is cheaper than SipHash. The
        // index only exists to keep pathological key counts linear; it is
        // built once the object proves it is not small.
        let mut index: Option<HashMap<String, usize>> = None;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            self.leave();
            return Ok(Value::Object(items));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected object key"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.bump() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            let value = self.parse_value()?;
            if let Some(map) = &mut index {
                if let Some(&slot) = map.get(&key) {
                    items[slot].1 = value;
                } else {
                    map.insert(key.clone(), items.len());
                    items.push((key, value));
                }
            } else if let Some((_, slot)) = items.iter_mut().find(|(k, _)| *k == key) {
                *slot = value;
            } else {
                items.push((key, value));
                if items.len() == 16 {
                    let mut map = HashMap::with_capacity(32);
                    for (slot, (key, _)) in items.iter().enumerate() {
                        map.insert(key.clone(), slot);
                    }
                    index = Some(map);
                }
            }
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
        self.leave();
        Ok(Value::Object(items))
    }

    fn parse_array(&mut self) -> Result<Value, JsonError> {
        self.enter()?;
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            self.leave();
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
        self.leave();
        Ok(Value::Array(items))
    }

    fn enter(&mut self) -> Result<(), JsonError> {
        self.depth += 1;
        if self.depth > 128 {
            return Err(self.error("nesting too deep"));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        if self.bump() != Some(b'"') {
            return Err(self.error("expected string"));
        }
        let mut out = Vec::new();
        loop {
            let start = self.pos;
            let Some(offset) = find_special(&self.bytes[start..]) else {
                self.pos = self.bytes.len();
                return Err(self.error("unterminated string"));
            };
            let index = start + offset;
            if index > start {
                out.extend_from_slice(&self.bytes[start..index]);
            }
            let byte = self.bytes[index];
            self.pos = index + 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escape = self.bump().ok_or_else(|| self.error("unterminated escape"))?;
                    match escape {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let first = self.parse_hex4()?;
                            let code = if (0xd800..=0xdbff).contains(&first) {
                                if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                                    return Err(self.error("missing low surrogate"));
                                }
                                let second = self.parse_hex4()?;
                                if !(0xdc00..=0xdfff).contains(&second) {
                                    return Err(self.error("invalid low surrogate"));
                                }
                                0x10000 + ((first - 0xd800) << 10) + (second - 0xdc00)
                            } else {
                                first
                            };
                            let ch = char::from_u32(code)
                                .ok_or_else(|| self.error("invalid unicode scalar"))?;
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                _ => return Err(self.error("control character in string")),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("invalid UTF-8 in string"))
    }

    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let byte = self.bump().ok_or_else(|| self.error("short unicode escape"))?;
            let digit = match byte {
                b'0'..=b'9' => (byte - b'0') as u32,
                b'a'..=b'f' => (byte - b'a' + 10) as u32,
                b'A'..=b'F' => (byte - b'A' + 10) as u32,
                _ => return Err(self.error("invalid unicode escape")),
            };
            value = (value << 4) | digit;
        }
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("leading zero"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("invalid number")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected digit after decimal point"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected exponent digit"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.error("invalid number bytes"))?;
        if !is_float {
            if let Ok(value) = text.parse::<i64>() {
                return Ok(Value::Int(value));
            }
        }
        let value = text
            .parse::<f64>()
            .map_err(|_| self.error("invalid number"))?;
        if !value.is_finite() {
            return Err(self.error("number out of range"));
        }
        Ok(Value::Float(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let input = br#"{"a":1,"b":[true,false,null],"c":"x\n\u00e9"}"#;
        let value = Value::parse(input).unwrap();
        assert_eq!(value.get("a").and_then(Value::as_i64), Some(1));
        assert_eq!(value.get("c").and_then(Value::as_str), Some("x\né"));
        let again = Value::parse_str(&value.stringify()).unwrap();
        assert_eq!(value, again);
    }

    #[test]
    fn surrogate_pair() {
        let value = Value::parse_str(r#""\ud83d\ude00""#).unwrap();
        assert_eq!(value.as_str(), Some("😀"));
    }

    #[test]
    fn preserve_unknown_fields() {
        let mut value = Value::parse_str(r#"{"keep":{"deep":[1,2,3]},"model":"x"}"#).unwrap();
        value.insert("model", Value::string("y"));
        assert_eq!(
            value.get("keep").unwrap().get("deep").unwrap().as_array().unwrap().len(),
            3
        );
        assert_eq!(value.get("model").unwrap().as_str(), Some("y"));
    }

    #[test]
    fn duplicate_keys_are_last_wins() {
        let value = Value::parse_str(r#"{"model":"kilo/A","model":"kilo/B","stream":false,"stream":true}"#)
            .unwrap();
        assert_eq!(value.get("model").unwrap().as_str(), Some("kilo/B"));
        assert_eq!(value.get("stream").unwrap().as_bool(), Some(true));
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(
            value.stringify(),
            r#"{"model":"kilo/B","stream":true}"#
        );
    }

    #[test]
    fn rejects_trailing_garbage() {
        assert!(Value::parse_str("{}x").is_err());
        assert!(Value::parse_str("{\"a\":01}").is_err());
        assert!(Value::parse_str("\"unterminated").is_err());
    }

    #[test]
    fn escapes_match_the_canonical_form() {
        let value = Value::string("a\"b\\c\n\r\t\u{8}\u{c}\u{1}é😀");
        assert_eq!(
            value.stringify(),
            r#""a\"b\\c\n\r\t\b\f\u0001é😀""#
        );
        assert_eq!(Value::parse_str(&value.stringify()).unwrap(), value);
    }

    #[test]
    fn special_bytes_are_found_at_every_run_offset() {
        for offset in 0..24 {
            let mut text = "x".repeat(offset);
            text.push_str("q\\\"\n");
            let encoded = Value::string(text.as_str()).stringify();
            assert_eq!(Value::parse_str(&encoded).unwrap().as_str(), Some(text.as_str()));
        }
    }

    #[test]
    fn find_special_matches_a_scalar_scan() {
        fn scalar(bytes: &[u8]) -> Option<usize> {
            bytes
                .iter()
                .position(|&byte| byte < 0x20 || byte == b'"' || byte == b'\\')
        }
        for byte in 0u8..=255 {
            let bytes = [b'a', b'b', byte, b'c'];
            assert_eq!(find_special(&bytes), scalar(&bytes), "{bytes:?}");
        }
        let alphabet = [
            0x00u8, 0x09, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x5b, 0x5c, 0x5d, 0x7f, 0x80, 0xc3,
            0xa9, 0xff,
        ];
        let mut state = 0x1234_5678u32;
        for len in 0..=64 {
            for _ in 0..200 {
                let mut bytes = Vec::with_capacity(len);
                for _ in 0..len {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    bytes.push(alphabet[(state as usize) % alphabet.len()]);
                }
                assert_eq!(find_special(&bytes), scalar(&bytes), "{bytes:?}");
            }
        }
    }

    #[test]
    fn error_offsets_survive_the_run_scanner() {
        assert_eq!(Value::parse_str("\"a\u{1}\"").unwrap_err().offset, 3);
        assert_eq!(Value::parse_str("\"ab").unwrap_err().offset, 3);
        assert_eq!(Value::parse_str(r#""\x""#).unwrap_err().offset, 3);
        assert_eq!(Value::parse_str(r#""\ud83d""#).unwrap_err().offset, 8);
    }

    #[test]
    fn integer_writer_matches_display() {
        for value in [0i64, 7, -7, 123_456_789_012_345, i64::MAX, i64::MIN] {
            let mut out = Vec::new();
            write_i64(&mut out, value).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), format!("{value}"));
        }
    }

    #[test]
    fn large_objects_stay_last_wins() {
        let mut json = String::from("{");
        for index in 0..40 {
            if index != 0 {
                json.push(',');
            }
            json.push_str(&format!("\"k{index}\":{index}"));
        }
        json.push('}');
        let value = Value::parse_str(&json).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 40);
        let duplicate = format!("{}{}", &json[..json.len() - 1], ",\"k7\":\"late\"}");
        assert_eq!(
            Value::parse_str(&duplicate).unwrap().get("k7").unwrap().as_str(),
            Some("late")
        );
    }
}
