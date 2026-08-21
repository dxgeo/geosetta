//! In-memory JSON value model.
//!
//! Object members are stored in an ordered `Vec` (not a hash map) so that
//! column order in the eventual Parquet output is deterministic and matches
//! the source document.

/// A parsed JSON value, and the IR's property-value type.
///
/// # `#[non_exhaustive]`
///
/// Consumers must include a wildcard arm when matching. That is deliberate and
/// it is not hypothetical: `Binary` was added on 2026-08-21, and because this
/// type was unnameable outside the crate at the time, a change that would have
/// broken every downstream `match` broke nothing and was noticed by no one.
/// The attribute makes the next such addition explicitly non-breaking instead
/// of accidentally so.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum JsonValue {
    Null,
    Bool(bool),
    /// A JSON number the source wrote without a fraction or exponent, held
    /// **exactly**.
    ///
    /// This was `Number { value: f64, is_int: bool }` until 2026-08-21, and the
    /// flag was the tell: it existed to remember something the representation
    /// had already thrown away. An `f64` carries integers exactly only to
    /// 2^53, so a *same-format* `gpkg -> gpkg` run turned `9007199254740993`
    /// into `9007199254740992` — a plain 64-bit `INTEGER` column, which both
    /// GeoPackage and GeoParquet define as 64-bit and neither could round-trip.
    /// Past `i64::MAX` the old serializer's `as i64` saturated, so
    /// `12345678901234567890` came back `9223372036854775807`: a specific wrong
    /// number rather than a nearby one.
    ///
    /// A JSON integer too large for an `i64` parses as [`Self::Float`] — lossy,
    /// but honestly lossy, and never a different integer.
    Int(i64),
    /// A JSON number with a fraction or exponent.
    ///
    /// Rendered with a `.0` when it is whole, so a float that happens to be
    /// round stays a float: `1.0` in, `1.0` out. That matches `ogr2ogr`, which
    /// round-trips `{"whole":1.0,"int":7}` unchanged.
    Float(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
    /// Raw bytes, which JSON has no type for.
    ///
    /// **Yes, this makes the name a slight lie, and the alternative was worse.**
    /// This enum is the IR's property-value type as well as the JSON parser's
    /// output, and three formats have a byte-level column: GeoPackage's `BLOB`,
    /// FlatGeobuf's `BINARY` and Parquet's unannotated `BYTE_ARRAY`. Without a
    /// variant for them the bytes had nowhere to live, and the GeoPackage
    /// reader simply returned `Null` — *a same-format run destroyed every blob
    /// in the file, silently* (measured 2026-08-21: `X'0102'` in, `NULL` out).
    ///
    /// The precedent is already here: `Number { is_int }` is not a JSON
    /// distinction either — JSON has one number type — and it exists because
    /// the IR needs to know. Same reasoning, one type further.
    ///
    /// Rendered as **hex** by the JSON serializer, matching what
    /// `parquet/reader.rs` already produced for byte arrays it could not
    /// declare. A JSON document therefore round-trips it back as a `String`,
    /// which is correct: GeoJSON has no binary, and the loss is the target's.
    Binary(Vec<u8>),
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

    /// The numeric value as an `f64`, or `None` if this is not a number.
    ///
    /// Lossy for an [`Self::Int`] beyond 2^53 — use [`Self::as_i64`] where the
    /// exact integer matters, which is what every column materialization does.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            JsonValue::Int(n) => Some(*n as f64),
            JsonValue::Float(v) => Some(*v),
            _ => None,
        }
    }

    /// The exact integer, or `None` if this is not an integer.
    ///
    /// The reason the number model was split: a 64-bit column's value has to
    /// reach the writer without passing through an `f64`.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            JsonValue::Int(n) => Some(*n),
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
            // Format straight into `out` (Display for i64/f64) rather than
            // allocating a temporary String per number.
            JsonValue::Int(n) => {
                let _ = write!(out, "{n}");
            }
            JsonValue::Float(v) => {
                // Rust's `Display` prints `1.0f64` as `1`, which would turn a
                // float back into an integer on every round trip. A non-finite
                // value has no JSON spelling at all; `null` is what the parser
                // would have to read back anyway.
                write_json_f64(*v, out);
            }
            JsonValue::String(s) => write_json_string(s, out),
            // Hex, in a JSON string: the only shape a JSON document has for
            // bytes, and the one this crate already used for undeclarable
            // Parquet byte arrays.
            JsonValue::Binary(bytes) => {
                out.push('"');
                for b in bytes {
                    let _ = write!(out, "{b:02x}");
                }
                out.push('"');
            }
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

/// Append `v` as a JSON number, keeping a whole float a *float*.
///
/// Rust's `Display` prints `1.0f64` as `1`, which would turn a float back into
/// an integer on every round trip now that the two are distinct. `ogr2ogr`
/// writes `1.0` here too, for both properties and coordinates — which is why
/// `geojson::write_position` calls this rather than formatting its own, after
/// the two disagreed for exactly as long as it took a test to notice.
///
/// A non-finite value has no JSON spelling; `null` is what a reader would have
/// to produce anyway.
pub(crate) fn write_json_f64(v: f64, out: &mut String) {
    use std::fmt::Write;
    if !v.is_finite() {
        out.push_str("null");
    } else if v.fract() == 0.0 && v.abs() < 1e16 {
        let _ = write!(out, "{v:.1}");
    } else {
        let _ = write!(out, "{v}");
    }
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
