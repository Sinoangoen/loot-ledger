//! A small JSON writer and reader.
//!
//! loot-ledger takes no dependencies, so `serde_json` is not available. What the
//! application actually needs is narrow: emit compact objects for an HTTP
//! response, and read back the journal's own line format at startup. That fits
//! in a few hundred lines, and owning it means no version skew between the
//! serialiser and what we log.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// A JSON value.
///
/// Object keys are kept in an ordered map so output is deterministic, which
/// makes both the HTTP responses and the journal diffable and testable.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// An empty object.
    pub fn object() -> Json {
        Json::Object(BTreeMap::new())
    }

    /// Build an object from key/value pairs.
    pub fn obj<K: Into<String>, I: IntoIterator<Item = (K, Json)>>(pairs: I) -> Json {
        Json::Object(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// Build an array.
    pub fn arr<I: IntoIterator<Item = Json>>(items: I) -> Json {
        Json::Array(items.into_iter().collect())
    }

    /// Borrow a string value.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Read an integer value.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(n) => Some(*n),
            Json::Float(f) if f.fract() == 0.0 => Some(*f as i64),
            _ => None,
        }
    }

    /// Read a boolean value.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Borrow a nested array.
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Read a nested object.
    pub fn as_object(&self) -> Option<&BTreeMap<String, Json>> {
        match self {
            Json::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Read a nested value by key.
    pub fn get(&self, key: &str) -> Option<&Json> {
        self.as_object()?.get(key)
    }

    /// True for `Json::Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    /// Serialise to compact JSON.
    fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Json::Float(f) => {
                // JSON has no literal for non-finite numbers.
                if f.is_finite() {
                    let _ = write!(out, "{f}");
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_string(s, out),
            Json::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            Json::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

/// Write a JSON string literal, escaping what the grammar requires.
impl std::fmt::Display for Json {
    /// Serialise to compact JSON.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_json_string())
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Control characters must be escaped; everything else, including
            // non-ASCII player names, goes through as UTF-8.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Why a document could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

/// Maximum nesting depth accepted by the parser.
///
/// The journal only ever nests two levels. A limit turns a hostile or corrupt
/// file into a clean error instead of a stack overflow, which matters because
/// this runs on whatever the user happens to point it at.
const MAX_DEPTH: usize = 64;

/// Parse one complete JSON document.
pub fn parse(input: &str) -> Result<Json, ParseError> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_whitespace();
    let value = p.value(0)?;
    p.skip_whitespace();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing data after JSON value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_owned(),
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), ParseError> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected '{}'", b as char)))
        }
    }

    fn literal(&mut self, text: &str) -> Result<(), ParseError> {
        if self.bytes[self.pos..].starts_with(text.as_bytes()) {
            self.pos += text.len();
            Ok(())
        } else {
            Err(self.err(&format!("expected {text}")))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.err("nesting too deep"));
        }

        match self.peek() {
            Some(b'n') => {
                self.literal("null")?;
                Ok(Json::Null)
            }
            Some(b't') => {
                self.literal("true")?;
                Ok(Json::Bool(true))
            }
            Some(b'f') => {
                self.literal("false")?;
                Ok(Json::Bool(false))
            }
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b'[') => self.array(depth),
            Some(b'{') => self.object(depth),
            Some(c) if c == b'-' || c.is_ascii_digit() => self.number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.expect(b'[')?;
        let mut items = Vec::new();

        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }

        loop {
            self.skip_whitespace();
            items.push(self.value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.expect(b'{')?;
        let mut map = BTreeMap::new();

        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(map));
        }

        loop {
            self.skip_whitespace();
            let key = self.string()?;
            self.skip_whitespace();
            self.expect(b':')?;
            self.skip_whitespace();
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(map));
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.expect(b'"')?;
        let mut out = String::new();

        loop {
            let Some(c) = self.peek() else {
                return Err(self.err("unterminated string"));
            };
            self.pos += 1;

            match c {
                b'"' => return Ok(out),
                b'\\' => {
                    let Some(esc) = self.peek() else {
                        return Err(self.err("unterminated escape"));
                    };
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{08}'),
                        b'f' => out.push('\u{0c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                c if c < 0x20 => return Err(self.err("raw control character in string")),
                c if c < 0x80 => out.push(c as char),
                _ => {
                    // Multi-byte UTF-8: copy the whole sequence through.
                    let start = self.pos - 1;
                    let len = utf8_len(c);
                    if start + len > self.bytes.len() {
                        return Err(self.err("truncated UTF-8 sequence"));
                    }
                    match std::str::from_utf8(&self.bytes[start..start + len]) {
                        Ok(s) => out.push_str(s),
                        Err(_) => return Err(self.err("invalid UTF-8")),
                    }
                    self.pos = start + len;
                }
            }
        }
    }

    fn unicode_escape(&mut self) -> Result<char, ParseError> {
        let high = self.hex4()?;

        // Surrogate pairs arrive as two escapes and must be recombined.
        if (0xd800..0xdc00).contains(&high) {
            if self.bytes[self.pos..].starts_with(b"\\u") {
                self.pos += 2;
                let low = self.hex4()?;
                if (0xdc00..0xe000).contains(&low) {
                    let c = 0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00);
                    return char::from_u32(c).ok_or_else(|| self.err("invalid surrogate pair"));
                }
            }
            return Err(self.err("lone high surrogate"));
        }

        char::from_u32(high).ok_or_else(|| self.err("invalid unicode escape"))
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.err("truncated \\u escape"));
        }
        let s = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| self.err("invalid \\u escape"))?;
        let v = u32::from_str_radix(s, 16).map_err(|_| self.err("invalid \\u escape"))?;
        self.pos += 4;
        Ok(v)
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
            self.pos += 1;
        }

        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                self.pos += 1;
            }
        }

        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;

        if is_float {
            text.parse::<f64>()
                .map(Json::Float)
                .map_err(|_| self.err("invalid float"))
        } else {
            text.parse::<i64>()
                .map(Json::Int)
                .map_err(|_| self.err("integer out of range"))
        }
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_scalars() {
        assert_eq!(Json::Null.to_string(), "null");
        assert_eq!(Json::Bool(true).to_string(), "true");
        assert_eq!(Json::Int(-7).to_string(), "-7");
        assert_eq!(Json::Str("hi".into()).to_string(), "\"hi\"");
    }

    #[test]
    fn escapes_control_and_quote_characters() {
        let s = Json::Str("a\"b\\c\nd\te\u{1}f".into()).to_string();
        assert_eq!(s, r#""a\"b\\c\nd\te\u0001f""#);
    }

    #[test]
    fn passes_through_non_ascii() {
        // Player and guild names routinely contain these.
        let s = Json::Str("Grüße 日本語 🎮".into()).to_string();
        assert_eq!(s, "\"Grüße 日本語 🎮\"");
    }

    #[test]
    fn writes_nested_structures() {
        let v = Json::obj([
            ("n", Json::Int(3)),
            ("xs", Json::arr([Json::Str("a".into()), Json::Bool(false)])),
        ]);
        assert_eq!(v.to_string(), r#"{"n":3,"xs":["a",false]}"#);
    }

    #[test]
    fn object_keys_are_deterministic() {
        let v = Json::obj([("z", Json::Int(1)), ("a", Json::Int(2))]);
        assert_eq!(v.to_string(), r#"{"a":2,"z":1}"#);
    }

    #[test]
    fn round_trips_a_document() {
        let src = r#"{"a":1,"b":[true,null,"x"],"c":{"d":-2.5}}"#;
        let parsed = parse(src).unwrap();
        assert_eq!(parsed.to_string(), src);
    }

    #[test]
    fn reads_strings_with_escapes() {
        let v = parse(r#""a\nb\u0047""#).unwrap();
        assert_eq!(v.as_str(), Some("a\nbG"));
    }

    #[test]
    fn reads_surrogate_pairs() {
        // U+1F3AE VIDEO GAME, as a surrogate pair.
        let v = parse(r#""🎮""#).unwrap();
        assert_eq!(v.as_str(), Some("\u{1f3ae}"));
    }

    #[test]
    fn rejects_malformed_documents() {
        for bad in [
            "{",
            "[1,]",
            "{\"a\"}",
            "{\"a\":}",
            "\"unterminated",
            "tru",
            "{\"a\":1}extra",
            "",
            "\"\\q\"",
            "\"\\ud800\"",
        ] {
            assert!(parse(bad).is_err(), "should have rejected: {bad:?}");
        }
    }

    #[test]
    fn rejects_runaway_nesting() {
        let deep = "[".repeat(200) + &"]".repeat(200);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn accessors_behave() {
        let v = parse(r#"{"n":5,"s":"x","b":true,"f":2.0,"z":null}"#).unwrap();
        assert_eq!(v.get("n").and_then(Json::as_i64), Some(5));
        assert_eq!(v.get("s").and_then(Json::as_str), Some("x"));
        assert_eq!(v.get("b").and_then(Json::as_bool), Some(true));
        assert_eq!(v.get("f").and_then(Json::as_i64), Some(2));
        assert!(v.get("z").unwrap().is_null());
        assert!(v.get("missing").is_none());
    }
}
