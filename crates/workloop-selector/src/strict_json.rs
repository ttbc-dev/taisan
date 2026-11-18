//! Strict JSON reader for the selector protocol wire contract
//! (`docs/selector-protocol.md` framing rules). `serde_json` alone
//! cannot express three normative requirements — duplicate object
//! keys, a hard container-depth bound, and rejection of trailing
//! values — so this parser produces a `serde_json::Value` under all of
//! them before any typed decode runs. It is deliberately small: the
//! grammar is JSON, the checks are the protocol's.
//!
//! Enforced here, before typed validation:
//! - UTF-8 input, no BOM, exactly one JSON value followed only by
//!   JSON whitespace;
//! - duplicate object keys rejected at every depth;
//! - container nesting bounded (`depth` counts the root object as 1);
//! - strings reject unescaped control characters and, after
//!   unescaping, U+007F–U+009F — the protocol's string alphabet.

use serde_json::Value;

/// The protocol's nesting bound: the root object sits at depth 1, so
/// at most 16 containers may be open at once.
pub const MAX_JSON_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonError {
    /// Bytes were not valid UTF-8 or a BOM was present.
    Encoding,
    /// Malformed JSON at a byte offset (diagnostic only — never
    /// payload text).
    Malformed { offset: usize },
    /// A duplicate object key was found.
    DuplicateKey { offset: usize },
    /// Nesting exceeded [`MAX_JSON_DEPTH`].
    Depth,
    /// Trailing non-whitespace bytes after the single JSON value.
    Trailing,
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

/// Parse `bytes` as exactly one JSON value under the protocol's
/// strictness rules. The returned `Value` has already survived
/// duplicate-key, depth, control-character, and single-value checks;
/// callers still owe byte-length, count, and cross-field validation.
pub fn parse_strict(bytes: &[u8]) -> Result<Value, JsonError> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(JsonError::Encoding);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| JsonError::Encoding)?;
    let mut p = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    p.ws();
    let value = p.value(1)?;
    p.ws();
    if p.pos != p.bytes.len() {
        return Err(JsonError::Trailing);
    }
    Ok(value)
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(JsonError::Malformed { offset: self.pos }),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value, JsonError> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(JsonError::Malformed { offset: self.pos })
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(JsonError::Depth);
        }
        self.pos += 1; // '{'
        let mut map = serde_json::Map::new();
        let mut keys = std::collections::HashSet::new();
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(map));
        }
        loop {
            self.ws();
            if self.peek() != Some(b'"') {
                return Err(JsonError::Malformed { offset: self.pos });
            }
            let key_offset = self.pos;
            let key = self.string()?;
            if !keys.insert(key.clone()) {
                return Err(JsonError::DuplicateKey { offset: key_offset });
            }
            self.ws();
            if self.peek() != Some(b':') {
                return Err(JsonError::Malformed { offset: self.pos });
            }
            self.pos += 1;
            self.ws();
            let value = self.value(depth + 1)?;
            map.insert(key, value);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(map));
                }
                _ => return Err(JsonError::Malformed { offset: self.pos }),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Value, JsonError> {
        if depth > MAX_JSON_DEPTH {
            return Err(JsonError::Depth);
        }
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(JsonError::Malformed { offset: self.pos }),
            }
        }
    }

    /// Parse one JSON string starting at the opening quote. The
    /// protocol's string alphabet forbids every Unicode control
    /// character — U+0000–U+001F and U+007F–U+009F — however it is
    /// encoded, so decoded escapes pass the same check as raw bytes.
    fn string(&mut self) -> Result<String, JsonError> {
        debug_assert_eq!(self.peek(), Some(b'"'));
        self.pos += 1;
        let mut out = String::new();
        loop {
            let b = *self
                .bytes
                .get(self.pos)
                .ok_or(JsonError::Malformed { offset: self.pos })?;
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = *self
                        .bytes
                        .get(self.pos)
                        .ok_or(JsonError::Malformed { offset: self.pos })?;
                    self.pos += 1;
                    let ch = match esc {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{0008}',
                        b'f' => '\u{000C}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let cp = self.hex4()?;
                            // Surrogate pairs: a high surrogate must be
                            // followed by \uDC00–\uDFFF.
                            if (0xD800..0xDC00).contains(&cp) {
                                if self.bytes.get(self.pos) == Some(&b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    if !(0xDC00..0xE000).contains(&lo) {
                                        return Err(JsonError::Malformed { offset: self.pos });
                                    }
                                    let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                    char::from_u32(c)
                                        .ok_or(JsonError::Malformed { offset: self.pos })?
                                } else {
                                    return Err(JsonError::Malformed { offset: self.pos });
                                }
                            } else if (0xDC00..0xE000).contains(&cp) {
                                return Err(JsonError::Malformed { offset: self.pos });
                            } else {
                                char::from_u32(cp)
                                    .ok_or(JsonError::Malformed { offset: self.pos })?
                            }
                        }
                        _ => return Err(JsonError::Malformed { offset: self.pos }),
                    };
                    if ch.is_control() {
                        return Err(JsonError::Malformed { offset: self.pos });
                    }
                    out.push(ch);
                }
                0x00..=0x1F => return Err(JsonError::Malformed { offset: self.pos }),
                _ => {
                    // Copy the full UTF-8 sequence verbatim.
                    let start = self.pos - 1;
                    let len = utf8_len(b).ok_or(JsonError::Malformed { offset: start })?;
                    let end = start + len;
                    if end > self.bytes.len() {
                        return Err(JsonError::Malformed { offset: start });
                    }
                    let s = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| JsonError::Malformed { offset: start })?;
                    if s.chars().any(char::is_control) {
                        return Err(JsonError::Malformed { offset: start });
                    }
                    out.push_str(s);
                    self.pos = end;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = *self
                .bytes
                .get(self.pos)
                .ok_or(JsonError::Malformed { offset: self.pos })?;
            self.pos += 1;
            let d = match b {
                b'0'..=b'9' => b - b'0',
                b'a'..=b'f' => b - b'a' + 10,
                b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(JsonError::Malformed { offset: self.pos }),
            };
            value = value * 16 + u32::from(d);
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError::Malformed { offset: self.pos }),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(JsonError::Malformed { offset: self.pos });
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
                return Err(JsonError::Malformed { offset: self.pos });
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| JsonError::Malformed { offset: start })?;
        if is_float {
            text.parse::<f64>()
                .map(serde_json::Number::from_f64)
                .ok()
                .flatten()
                .map(Value::Number)
                .ok_or(JsonError::Malformed { offset: start })
        } else if let Ok(n) = text.parse::<i64>() {
            Ok(Value::Number(n.into()))
        } else if let Ok(n) = text.parse::<u64>() {
            Ok(Value::Number(n.into()))
        } else {
            Err(JsonError::Malformed { offset: start })
        }
    }
}

fn utf8_len(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}
