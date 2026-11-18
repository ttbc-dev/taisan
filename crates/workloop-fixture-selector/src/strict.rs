//! Strict JSON framing scan for the selector request wire contract
//! (`docs/selector-protocol.md`). `serde_json` parses the grammar but
//! cannot express three normative requirements — duplicate object
//! keys, a hard container-depth bound, and the protocol's
//! control-character string alphabet — so this scanner walks the raw
//! text and enforces them before `serde_json` performs the typed
//! parse. Grammar details serde owns (number ranges, literal
//! boundaries it already enforces) are left to it; this scanner only
//! guarantees the framing rules, in the same order the reference
//! adapter applies them.

use std::collections::HashSet;

/// The protocol's nesting bound: the root value sits at depth 1, so
/// at most 16 containers may be open at once.
const MAX_DEPTH: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanError {
    /// Not UTF-8, or a byte-order mark was present.
    Encoding,
    /// The bytes do not form one well-formed JSON value.
    Malformed,
    /// An object repeated a key.
    DuplicateKey,
    /// Nesting exceeded the protocol bound.
    Depth,
    /// Non-whitespace bytes followed the single JSON value.
    Trailing,
}

/// Enforce the wire framing rules over `bytes`: UTF-8 without a BOM,
/// exactly one JSON value followed only by whitespace, no container
/// deeper than [`MAX_DEPTH`], no duplicate object key at any depth,
/// and no control character — raw or escaped — inside any string.
pub fn scan(bytes: &[u8]) -> Result<(), ScanError> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Err(ScanError::Encoding);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| ScanError::Encoding)?;
    let mut s = Scanner { text, pos: 0 };
    s.ws();
    s.value(1)?;
    s.ws();
    if s.pos != text.len() {
        return Err(ScanError::Trailing);
    }
    Ok(())
}

struct Scanner<'a> {
    text: &'a str,
    /// Byte offset into `text`; always on a char boundary.
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<char> {
        self.text.get(self.pos..)?.chars().next()
    }

    fn bump(&mut self) {
        if let Some(ch) = self.peek() {
            self.pos += ch.len_utf8();
        }
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\n' | '\r')) {
            self.bump();
        }
    }

    fn value(&mut self, depth: usize) -> Result<(), ScanError> {
        match self.peek() {
            Some('{') => self.object(depth),
            Some('[') => self.array(depth),
            Some('"') => self.string().map(|_| ()),
            Some('t') => self.literal("true"),
            Some('f') => self.literal("false"),
            Some('n') => self.literal("null"),
            Some('-') | Some('0'..='9') => self.number(),
            _ => Err(ScanError::Malformed),
        }
    }

    fn literal(&mut self, word: &str) -> Result<(), ScanError> {
        if self.text[self.pos..].starts_with(word) {
            self.pos += word.len();
            Ok(())
        } else {
            Err(ScanError::Malformed)
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), ScanError> {
        if depth > MAX_DEPTH {
            return Err(ScanError::Depth);
        }
        self.bump(); // '{'
        let mut keys = HashSet::new();
        self.ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(());
        }
        loop {
            self.ws();
            if self.peek() != Some('"') {
                return Err(ScanError::Malformed);
            }
            let key = self.string()?;
            if !keys.insert(key) {
                return Err(ScanError::DuplicateKey);
            }
            self.ws();
            if self.peek() != Some(':') {
                return Err(ScanError::Malformed);
            }
            self.bump();
            self.ws();
            self.value(depth + 1)?;
            self.ws();
            match self.peek() {
                Some(',') => self.bump(),
                Some('}') => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(ScanError::Malformed),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), ScanError> {
        if depth > MAX_DEPTH {
            return Err(ScanError::Depth);
        }
        self.bump(); // '['
        self.ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(());
        }
        loop {
            self.ws();
            self.value(depth + 1)?;
            self.ws();
            match self.peek() {
                Some(',') => self.bump(),
                Some(']') => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(ScanError::Malformed),
            }
        }
    }

    /// Consume one JSON string and return its decoded contents —
    /// object keys are compared post-unescape, so the decode has to
    /// happen here. Every character, raw or unescaped, must satisfy
    /// the protocol's control-free alphabet.
    fn string(&mut self) -> Result<String, ScanError> {
        debug_assert_eq!(self.peek(), Some('"'));
        self.bump();
        let mut out = String::new();
        loop {
            let ch = self.peek().ok_or(ScanError::Malformed)?;
            self.bump();
            match ch {
                '"' => return Ok(out),
                '\\' => {
                    let esc = self.peek().ok_or(ScanError::Malformed)?;
                    self.bump();
                    let decoded = match esc {
                        '"' => '"',
                        '\\' => '\\',
                        '/' => '/',
                        'b' => '\u{0008}',
                        'f' => '\u{000C}',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        'u' => self.unicode_escape()?,
                        _ => return Err(ScanError::Malformed),
                    };
                    if decoded.is_control() {
                        return Err(ScanError::Malformed);
                    }
                    out.push(decoded);
                }
                ch if ch.is_control() => return Err(ScanError::Malformed),
                ch => out.push(ch),
            }
        }
    }

    /// Decode the code point a `\u` escape introduces, including the
    /// surrogate-pair rule: a high surrogate must be immediately
    /// followed by a low surrogate, and a lone low surrogate is
    /// malformed.
    fn unicode_escape(&mut self) -> Result<char, ScanError> {
        let cp = self.hex4()?;
        if (0xD800..0xDC00).contains(&cp) {
            if !self.text[self.pos..].starts_with("\\u") {
                return Err(ScanError::Malformed);
            }
            self.pos += 2;
            let lo = self.hex4()?;
            if !(0xDC00..0xE000).contains(&lo) {
                return Err(ScanError::Malformed);
            }
            let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
            char::from_u32(c).ok_or(ScanError::Malformed)
        } else if (0xDC00..0xE000).contains(&cp) {
            Err(ScanError::Malformed)
        } else {
            char::from_u32(cp).ok_or(ScanError::Malformed)
        }
    }

    fn hex4(&mut self) -> Result<u32, ScanError> {
        let mut value = 0u32;
        for _ in 0..4 {
            let ch = self.peek().ok_or(ScanError::Malformed)?;
            self.bump();
            let d = ch.to_digit(16).ok_or(ScanError::Malformed)?;
            value = value * 16 + d;
        }
        Ok(value)
    }

    /// Skip one JSON number's shape. Range and representation are
    /// serde's job; the scanner only needs correct token boundaries.
    fn number(&mut self) -> Result<(), ScanError> {
        if self.peek() == Some('-') {
            self.bump();
        }
        match self.peek() {
            Some('0') => self.bump(),
            Some('1'..='9') => {
                while matches!(self.peek(), Some('0'..='9')) {
                    self.bump();
                }
            }
            _ => return Err(ScanError::Malformed),
        }
        if self.peek() == Some('.') {
            self.bump();
            if !matches!(self.peek(), Some('0'..='9')) {
                return Err(ScanError::Malformed);
            }
            while matches!(self.peek(), Some('0'..='9')) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !matches!(self.peek(), Some('0'..='9')) {
                return Err(ScanError::Malformed);
            }
            while matches!(self.peek(), Some('0'..='9')) {
                self.bump();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_requests() {
        assert!(scan(br#"{"a":[1,2.5e3,"x"],"b":{"c":null},"d":true}"#).is_ok());
        assert!(scan(b"  [ ] \n").is_ok());
        assert!(scan(b"\"s\\u0041\\n\"".as_slice()).is_err()); // \n decodes to a control char
        assert!(scan(b"\"s\\u0041\"").is_ok());
        assert!(scan("\"héllo wörld\"".as_bytes()).is_ok());
    }

    #[test]
    fn rejects_duplicate_keys_at_any_depth() {
        assert_eq!(scan(br#"{"a":1,"a":2}"#), Err(ScanError::DuplicateKey));
        assert_eq!(
            scan(br#"{"a":{"b":1,"b":2}}"#),
            Err(ScanError::DuplicateKey)
        );
        assert_eq!(
            scan(br#"{"a":[{"x":1,"x":2}]}"#),
            Err(ScanError::DuplicateKey)
        );
        // Distinct keys that decode to the same string still collide.
        assert_eq!(scan(br#"{"a":1,"\u0061":2}"#), Err(ScanError::DuplicateKey));
    }

    #[test]
    fn rejects_depth_past_the_bound() {
        // 16 open containers is the limit; the 17th fails.
        let mut ok = String::new();
        for _ in 0..16 {
            ok.push('[');
        }
        for _ in 0..16 {
            ok.push(']');
        }
        assert!(scan(ok.as_bytes()).is_ok());
        let mut deep = String::new();
        for _ in 0..17 {
            deep.push('[');
        }
        for _ in 0..17 {
            deep.push(']');
        }
        assert_eq!(scan(deep.as_bytes()), Err(ScanError::Depth));
        // Objects count the same way.
        let mut obj = String::from("{\"a\":");
        for i in 0..16 {
            obj.push_str(&format!("{{\"k{i}\":"));
        }
        obj.push('1');
        for _ in 0..16 {
            obj.push('}');
        }
        obj.push('}');
        assert_eq!(scan(obj.as_bytes()), Err(ScanError::Depth));
    }

    #[test]
    fn rejects_control_characters_raw_and_escaped() {
        assert_eq!(scan(b"\"a\tb\""), Err(ScanError::Malformed)); // raw tab
        assert_eq!(scan(b"\"a\\u0009b\""), Err(ScanError::Malformed)); // escaped tab
        assert_eq!(scan(b"\"a\\u007fb\""), Err(ScanError::Malformed)); // DEL
        assert_eq!(scan(b"\"a\\u0085b\""), Err(ScanError::Malformed)); // NEL
        assert_eq!(scan("\"a\u{85}b\"".as_bytes()), Err(ScanError::Malformed)); // raw NEL
        assert!(scan(b"\"a\\u00e9b\"").is_ok()); // non-control escape fine
    }

    #[test]
    fn rejects_trailing_values_and_bad_framing() {
        assert_eq!(scan(b"{} {}"), Err(ScanError::Trailing));
        assert_eq!(scan(b"{}x"), Err(ScanError::Trailing));
        assert_eq!(scan(b""), Err(ScanError::Malformed));
        assert_eq!(scan(b"{"), Err(ScanError::Malformed));
        assert_eq!(scan(b"[1,]"), Err(ScanError::Malformed));
        assert_eq!(scan(b"01"), Err(ScanError::Trailing)); // '0' then stray '1'
        assert_eq!(scan(b"\"\\ud800\""), Err(ScanError::Malformed)); // lone high surrogate
        assert_eq!(scan(b"\"\\udc00\""), Err(ScanError::Malformed)); // lone low surrogate
        assert!(scan(b"\"\\ud83d\\ude00\"").is_ok()); // valid pair
        assert_eq!(
            scan(&[0xEF, 0xBB, 0xBF, b'{', b'}']),
            Err(ScanError::Encoding)
        );
        assert_eq!(scan(&[0xFF, b'{', b'}']), Err(ScanError::Encoding));
    }
}
