//! A small recursive-descent JSON parser over UTF-8 bytes.
//!
//! Standard-library only. Produces a [`JsonValue`]. Errors carry the byte
//! offset where parsing failed.

use super::value::JsonValue;
use crate::error::{Error, Result};

/// Parse a complete JSON document. Trailing content (after optional
/// whitespace) is rejected.
pub fn parse(input: &str) -> Result<JsonValue> {
    let mut p = Parser {
        bytes: input.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let value = p.parse_value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("unexpected trailing characters"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, message: &str) -> Error {
        Error::Json {
            offset: self.pos,
            message: message.to_string(),
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    /// Consume `expected` exactly, or error.
    fn expect(&mut self, expected: u8) -> Result<()> {
        match self.peek() {
            Some(b) if b == expected => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.err(&format!("expected '{}'", expected as char))),
        }
    }

    fn parse_value(&mut self) -> Result<JsonValue> {
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => Ok(JsonValue::String(self.parse_string()?)),
            Some(b't') | Some(b'f') => self.parse_bool(),
            Some(b'n') => self.parse_null(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.err("unexpected character")),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue> {
        self.expect(b'{')?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(JsonValue::Object(members));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("expected string key"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            members.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        Ok(JsonValue::Object(members))
    }

    fn parse_array(&mut self) -> Result<JsonValue> {
        self.expect(b'[')?;
        let mut elems = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(JsonValue::Array(elems));
        }
        loop {
            let value = self.parse_value()?;
            elems.push(value);
            self.skip_ws();
            match self.bump() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(self.err("expected ',' or ']'")),
            }
        }
        Ok(JsonValue::Array(elems))
    }

    fn parse_bool(&mut self) -> Result<JsonValue> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(JsonValue::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(JsonValue::Bool(false))
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_null(&mut self) -> Result<JsonValue> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(JsonValue::Null)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn parse_number(&mut self) -> Result<JsonValue> {
        let start = self.pos;
        let mut is_int = true;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_int = false;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;
        let value: f64 = text.parse().map_err(|_| Error::Json {
            offset: start,
            message: "invalid number".to_string(),
        })?;
        Ok(JsonValue::Number { value, is_int })
    }

    /// Parse a JSON string, with the opening quote at the cursor.
    fn parse_string(&mut self) -> Result<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => self.parse_escape(&mut out)?,
                Some(b) if b < 0x20 => {
                    return Err(self.err("control character in string"));
                }
                Some(b) if b < 0x80 => out.push(b as char),
                Some(b) => {
                    // Multi-byte UTF-8: collect the continuation bytes and
                    // decode. We already consumed the lead byte `b`.
                    let len = utf8_len(b);
                    let start = self.pos - 1;
                    for _ in 1..len {
                        if self.bump().is_none() {
                            return Err(self.err("truncated utf-8 sequence"));
                        }
                    }
                    let s = std::str::from_utf8(&self.bytes[start..self.pos])
                        .map_err(|_| self.err("invalid utf-8"))?;
                    out.push_str(s);
                }
            }
        }
        Ok(out)
    }

    /// Handle the character following a backslash.
    fn parse_escape(&mut self, out: &mut String) -> Result<()> {
        match self.bump() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{08}'),
            Some(b'f') => out.push('\u{0C}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => {
                let hi = self.parse_hex4()?;
                let cp = if (0xD800..=0xDBFF).contains(&hi) {
                    // High surrogate: must be followed by \uXXXX low surrogate.
                    if self.bump() != Some(b'\\') || self.bump() != Some(b'u') {
                        return Err(self.err("expected low surrogate"));
                    }
                    let lo = self.parse_hex4()?;
                    if !(0xDC00..=0xDFFF).contains(&lo) {
                        return Err(self.err("invalid low surrogate"));
                    }
                    0x10000 + (((hi - 0xD800) as u32) << 10) + (lo - 0xDC00) as u32
                } else if (0xDC00..=0xDFFF).contains(&hi) {
                    return Err(self.err("unexpected low surrogate"));
                } else {
                    hi as u32
                };
                match char::from_u32(cp) {
                    Some(c) => out.push(c),
                    None => return Err(self.err("invalid code point")),
                }
            }
            _ => return Err(self.err("invalid escape")),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u16> {
        let mut v: u16 = 0;
        for _ in 0..4 {
            let d = match self.bump() {
                Some(b @ b'0'..=b'9') => (b - b'0') as u16,
                Some(b @ b'a'..=b'f') => (b - b'a' + 10) as u16,
                Some(b @ b'A'..=b'F') => (b - b'A' + 10) as u16,
                _ => return Err(self.err("invalid \\u escape")),
            };
            v = (v << 4) | d;
        }
        Ok(v)
    }
}

/// Expected total byte length of a UTF-8 sequence from its lead byte.
fn utf8_len(lead: u8) -> usize {
    if lead >= 0xF0 {
        4
    } else if lead >= 0xE0 {
        3
    } else {
        2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null").unwrap(), JsonValue::Null);
        assert_eq!(parse("true").unwrap(), JsonValue::Bool(true));
        assert_eq!(parse("  false ").unwrap(), JsonValue::Bool(false));
        assert_eq!(parse("\"hi\"").unwrap(), JsonValue::String("hi".into()));
    }

    #[test]
    fn distinguishes_int_and_float() {
        match parse("42").unwrap() {
            JsonValue::Number { value, is_int } => {
                assert_eq!(value, 42.0);
                assert!(is_int);
            }
            _ => panic!(),
        }
        match parse("-3.5e2").unwrap() {
            JsonValue::Number { value, is_int } => {
                assert_eq!(value, -350.0);
                assert!(!is_int);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn parses_nested_and_preserves_order() {
        let v = parse(r#"{"b": [1, 2], "a": {"x": null}}"#).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj[0].0, "b");
        assert_eq!(obj[1].0, "a");
        assert_eq!(v.get("b").unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn handles_escapes_and_unicode() {
        assert_eq!(parse(r#""a\nb""#).unwrap().as_str().unwrap(), "a\nb");
        assert_eq!(parse(r#""é""#).unwrap().as_str().unwrap(), "é");
        // Surrogate pair for U+1F600.
        assert_eq!(parse(r#""😀""#).unwrap().as_str().unwrap(), "😀");
        // Raw multi-byte UTF-8 passes through.
        assert_eq!(parse("\"café\"").unwrap().as_str().unwrap(), "café");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("nul").is_err());
        assert!(parse("1 2").is_err());
    }
}
