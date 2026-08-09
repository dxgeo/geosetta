//! In-memory JSON value model.
//!
//! Object members are stored in an ordered `Vec` (not a hash map) so that
//! column order in the eventual Parquet output is deterministic and matches
//! the source document.

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    /// A JSON number. `is_int` records whether the source token had no
    /// fraction or exponent, which lets us prefer integer Parquet columns.
    Number { value: f64, is_int: bool },
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

impl JsonValue {
    /// Members of an object, or `None` if this is not an object.
    pub fn as_object(&self) -> Option<&[(String, JsonValue)]> {
        match self {
            JsonValue::Object(m) => Some(m),
            _ => None,
        }
    }

    /// Elements of an array, or `None` if this is not an array.
    pub fn as_array(&self) -> Option<&[JsonValue]> {
        match self {
            JsonValue::Array(a) => Some(a),
            _ => None,
        }
    }

    /// The string contents, or `None` if this is not a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            JsonValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// The numeric value, or `None` if this is not a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Number { value, .. } => Some(*value),
            _ => None,
        }
    }

    /// Look up a member by key (object only). First match wins.
    pub fn get(&self, key: &str) -> Option<&JsonValue> {
        self.as_object()?
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// Serialize back to compact JSON text (no insignificant whitespace).
    /// Used to stringify heterogeneous or nested property values into a
    /// single Parquet string column.
    pub fn to_json_string(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    /// Append this value's compact JSON to `out` (the public entry to the
    /// serializer, so other spokes can stream values without a `JsonValue`
    /// wrapper or an intermediate `String`).
    pub fn write_json_to(&self, out: &mut String) {
        self.write_json(out);
    }

    fn write_json(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            JsonValue::Null => out.push_str("null"),
            JsonValue::Bool(true) => out.push_str("true"),
            JsonValue::Bool(false) => out.push_str("false"),
            JsonValue::Number { value, is_int } => {
                // Format straight into `out` (Display for i64/f64) rather than
                // allocating a temporary String per number.
                if *is_int {
                    let _ = write!(out, "{}", *value as i64); // no fractional part
                } else {
                    let _ = write!(out, "{value}");
                }
            }
            JsonValue::String(s) => write_json_string(s, out),
            JsonValue::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_json(out);
                }
                out.push(']');
            }
            JsonValue::Object(members) => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(k, out);
                    out.push(':');
                    v.write_json(out);
                }
                out.push('}');
            }
        }
    }
}

/// Write `s` as a quoted, escaped JSON string (public so other serializers can
/// emit keys/values without a `JsonValue` wrapper).
pub fn escape_into(s: &str, out: &mut String) {
    write_json_string(s, out);
}

/// Write `s` as a quoted, escaped JSON string.
///
/// Bulk-copies runs of ordinary bytes with one `push_str` and only stops on a
/// byte that must be escaped (`"`, `\`, or a control char `< 0x20`) — the mirror
/// of what the parser does on read, and far cheaper than decoding and
/// re-pushing one `char` at a time. Every escape-triggering byte is ASCII, so
/// run boundaries always fall on char boundaries and each slice is valid UTF-8.
fn write_json_string(s: &str, out: &mut String) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'"' && b != b'\\' && b >= 0x20 {
            continue;
        }
        if start < i {
            out.push_str(&s[start..i]);
        }
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x08 => out.push_str("\\b"),
            0x0C => out.push_str("\\f"),
            // Any other control char: \u00XX (b < 0x20, so the high byte is 00).
            _ => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.push_str("\\u00");
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
        start = i + 1;
    }
    if start < bytes.len() {
        out.push_str(&s[start..]);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escaped(s: &str) -> String {
        let mut out = String::new();
        escape_into(s, &mut out);
        out
    }

    #[test]
    fn escapes_specials_and_copies_clean_runs() {
        assert_eq!(escaped("plain"), "\"plain\"");
        assert_eq!(escaped("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(escaped("tab\tnl\ncr\r"), "\"tab\\tnl\\ncr\\r\"");
        assert_eq!(escaped("\u{08}\u{0c}"), "\"\\b\\f\"");
        // Other control chars use lowercase \u00XX.
        assert_eq!(escaped("\u{00}\u{1f}"), "\"\\u0000\\u001f\"");
        // Multi-byte UTF-8 is copied verbatim (bytes >= 0x80 are never escaped).
        assert_eq!(escaped("café ☕"), "\"café ☕\"");
        assert_eq!(escaped(""), "\"\"");
    }
}
