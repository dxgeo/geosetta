//! A borrowed cell (`plans/columnar.org` D-C8): what a view hands out, so
//! reading a table never copies a string to look at it.

use crate::json::JsonValue;

/// A property value, borrowed from the collection holding it: what
/// [`crate::FeatureRef`] and [`crate::ColumnRef`] hand out. A
/// [`JsonValue`] without the copy; [`Self::to_owned`] makes one.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Value<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    Binary(&'a [u8]),
    /// An array or object, which only a `Mixed` column or a row block holds.
    Json(&'a JsonValue),
}

impl<'a> Value<'a> {
    pub(crate) fn of(value: &'a JsonValue) -> Value<'a> {
        match value {
            JsonValue::Null => Value::Null,
            JsonValue::Bool(b) => Value::Bool(*b),
            JsonValue::Int(i) => Value::Int(*i),
            JsonValue::Float(f) => Value::Float(*f),
            JsonValue::String(s) => Value::Str(s),
            JsonValue::Binary(b) => Value::Binary(b),
            JsonValue::Array(_) | JsonValue::Object(_) => Value::Json(value),
        }
    }

    /// The cell as compact JSON, as [`JsonValue::write_json_to`] writes it.
    pub(crate) fn write_json_to(self, out: &mut String) {
        use std::fmt::Write;
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(true) => out.push_str("true"),
            Value::Bool(false) => out.push_str("false"),
            // Format straight into `out` (Display for i64/f64) rather than
            // allocating a temporary String per number.
            Value::Int(n) => {
                let _ = write!(out, "{n}");
            }
            // Rust's `Display` prints `1.0f64` as `1`, which would turn a
            // float back into an integer on every round trip. A non-finite
            // value has no JSON spelling at all; `null` is what the parser
            // would have to read back anyway.
            Value::Float(v) => crate::json::write_json_f64(v, out),
            Value::Str(s) => crate::json::escape_into(s, out),
            // Hex, in a JSON string: the only shape a JSON document has for
            // bytes, and the one this crate already used for undeclarable
            // Parquet byte arrays.
            Value::Binary(bytes) => {
                out.push('"');
                for b in bytes {
                    let _ = write!(out, "{b:02x}");
                }
                out.push('"');
            }
            Value::Json(v) => v.write_json_to(out),
        }
    }

    /// The value as an owned [`JsonValue`], copied.
    #[allow(clippy::wrong_self_convention, clippy::should_implement_trait)]
    pub fn to_owned(self) -> JsonValue {
        match self {
            Value::Null => JsonValue::Null,
            Value::Bool(b) => JsonValue::Bool(b),
            Value::Int(i) => JsonValue::Int(i),
            Value::Float(f) => JsonValue::Float(f),
            Value::Str(s) => JsonValue::String(s.to_owned()),
            Value::Binary(b) => JsonValue::Binary(b.to_vec()),
            Value::Json(v) => v.clone(),
        }
    }
}
