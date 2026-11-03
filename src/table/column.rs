//! One property column of a column block (`plans/columnar.org` D-C2, D-C3).

use super::bitmap::Bitmap;
use super::kinds::Kinds;
use super::value::Value;
use crate::json::JsonValue;
use std::rc::Rc;

/// A column: a typed buffer with a slot for every row of its block, which
/// rows are null (`valid`), and which rows hold the key at all (`present`).
///
/// `present` is `None` while every row holds the key, which is every column a
/// tabular source produces; the first absent row creates it. Absent and null
/// are different bitmaps because they are different documents (guarantee 1).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Column {
    pub(crate) name: Rc<str>,
    data: ColumnData,
    valid: Bitmap,
    present: Option<Bitmap>,
}

/// D-C2's buffers, plus `Null`: a column whose every cell so far is null or
/// absent has no type to choose yet, and holds nothing.
///
/// An absent or null row still takes a slot in a typed buffer -- `false`,
/// `0`, an empty string -- so row `i` is slot `i`.
#[derive(Debug, Clone, PartialEq)]
enum ColumnData {
    Null,
    Bool(Bitmap),
    /// Integers and floats together (D-C3): each slot's bits are an `i64` or
    /// an `f64`, and `is_int` says which, so `1` and `1.0` stay two values.
    Number { bits: Vec<u64>, is_int: Bitmap },
    /// Text as one `String`, so reading a slot checks only its two ends
    /// fall on character boundaries, not the whole slot.
    Str { offsets: Vec<u32>, text: String },
    Binary { offsets: Vec<u32>, bytes: Vec<u8> },
    /// The escape hatch: anything the narrower buffers cannot hold exactly.
    Mixed(Vec<JsonValue>),
}

/// The buffer a value would start a column in.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Number,
    Str,
    Binary,
    Mixed,
}

fn kind_of(value: &JsonValue) -> Option<Kind> {
    match value {
        JsonValue::Null => None,
        JsonValue::Bool(_) => Some(Kind::Bool),
        JsonValue::Int(_) | JsonValue::Float(_) => Some(Kind::Number),
        JsonValue::String(_) => Some(Kind::Str),
        JsonValue::Binary(_) => Some(Kind::Binary),
        JsonValue::Array(_) | JsonValue::Object(_) => Some(Kind::Mixed),
    }
}

impl ColumnData {
    fn kind(&self) -> Option<Kind> {
        match self {
            ColumnData::Null => None,
            ColumnData::Bool(_) => Some(Kind::Bool),
            ColumnData::Number { .. } => Some(Kind::Number),
            ColumnData::Str { .. } => Some(Kind::Str),
            ColumnData::Binary { .. } => Some(Kind::Binary),
            ColumnData::Mixed(_) => Some(Kind::Mixed),
        }
    }

    /// An empty buffer of `kind` with `len` placeholder slots.
    fn placeholders(kind: Kind, len: usize) -> ColumnData {
        match kind {
            Kind::Bool => ColumnData::Bool(Bitmap::filled(len, false)),
            Kind::Number => ColumnData::Number { bits: vec![0; len], is_int: Bitmap::filled(len, false) },
            Kind::Str => ColumnData::Str { offsets: vec![0; len + 1], text: String::new() },
            Kind::Binary => ColumnData::Binary { offsets: vec![0; len + 1], bytes: Vec::new() },
            Kind::Mixed => ColumnData::Mixed(vec![JsonValue::Null; len]),
        }
    }

    /// Append one slot. `value` is `Null` for a null or absent row, and
    /// otherwise of this buffer's kind.
    fn push(&mut self, value: JsonValue) {
        match (self, value) {
            (ColumnData::Null, _) => {}
            (ColumnData::Bool(b), JsonValue::Bool(v)) => b.push(v),
            (ColumnData::Bool(b), _) => b.push(false),
            (ColumnData::Number { bits, is_int }, JsonValue::Int(v)) => {
                bits.push(v as u64);
                is_int.push(true);
            }
            (ColumnData::Number { bits, is_int }, JsonValue::Float(v)) => {
                bits.push(v.to_bits());
                is_int.push(false);
            }
            (ColumnData::Number { bits, is_int }, _) => {
                bits.push(0);
                is_int.push(false);
            }
            (ColumnData::Str { offsets, text }, JsonValue::String(v)) => {
                text.push_str(&v);
                push_end(offsets, text.len());
            }
            (ColumnData::Str { offsets, text }, _) => push_end(offsets, text.len()),
            (ColumnData::Binary { offsets, bytes }, JsonValue::Binary(v)) => {
                bytes.extend_from_slice(&v);
                push_end(offsets, bytes.len());
            }
            (ColumnData::Binary { offsets, bytes }, _) => push_end(offsets, bytes.len()),
            (ColumnData::Mixed(values), v) => values.push(v),
        }
    }
}

/// End the slot just pushed at `end`, the buffer's new length.
fn push_end(offsets: &mut Vec<u32>, end: usize) {
    // A block past 4 GiB of text is not a case designed for (D-C10); the
    // reader seals early before one could form.
    offsets.push(u32::try_from(end).expect("a block's text exceeds 4 GiB"));
}

fn slice(offsets: &[u32], row: usize) -> std::ops::Range<usize> {
    offsets[row] as usize..offsets[row + 1] as usize
}

impl Column {
    pub(crate) fn new(name: Rc<str>) -> Column {
        Column { name, data: ColumnData::Null, valid: Bitmap::default(), present: None }
    }

    pub(crate) fn len(&self) -> usize {
        self.valid.len()
    }

    /// Append row `len()`: `None` when the row does not hold the key.
    ///
    /// A value the buffer cannot hold exactly promotes the column once
    /// (D-C3): from `Null` to the value's own buffer, from anything else to
    /// `Mixed`, copying what it holds.
    pub(crate) fn push(&mut self, cell: Option<JsonValue>) {
        let len = self.len();
        match (&mut self.present, cell.is_some()) {
            (Some(p), present) => p.push(present),
            (None, false) => {
                let mut p = Bitmap::filled(len, true);
                p.push(false);
                self.present = Some(p);
            }
            (None, true) => {}
        }
        let value = cell.unwrap_or(JsonValue::Null);
        // `Mixed` holds anything: promoting it again would copy the column on
        // every push, which made a mixed column quadratic to build.
        match (self.data.kind(), kind_of(&value)) {
            (None, Some(kind)) => self.promote(kind),
            (Some(held), Some(kind)) if held != kind && held != Kind::Mixed => self.promote(Kind::Mixed),
            _ => {}
        }
        self.valid.push(!matches!(value, JsonValue::Null));
        self.data.push(value);
    }

    fn promote(&mut self, to: Kind) {
        #[cfg(test)]
        tests::PROMOTIONS.with(|n| n.set(n.get() + 1));
        let len = self.len();
        self.data = match to {
            Kind::Mixed => ColumnData::Mixed((0..len).map(|row| self.get(row)).collect()),
            // Only a `Null` column promotes to anything narrower, and every
            // slot it holds is a placeholder.
            _ => ColumnData::placeholders(to, len),
        };
    }

    /// Whether `row` holds this column's key.
    pub(crate) fn is_present(&self, row: usize) -> bool {
        self.present.as_ref().is_none_or(|p| p.get(row))
    }

    /// The value at `row`, which must be present; `Null` when it is null.
    pub(crate) fn value(&self, row: usize) -> Value<'_> {
        if !self.valid.get(row) {
            return Value::Null;
        }
        match &self.data {
            ColumnData::Null => Value::Null,
            ColumnData::Bool(b) => Value::Bool(b.get(row)),
            ColumnData::Number { bits, is_int } => match is_int.get(row) {
                true => Value::Int(bits[row] as i64),
                false => Value::Float(f64::from_bits(bits[row])),
            },
            ColumnData::Str { offsets, text } => Value::Str(&text[slice(offsets, row)]),
            ColumnData::Binary { offsets, bytes } => Value::Binary(&bytes[slice(offsets, row)]),
            ColumnData::Mixed(values) => Value::of(&values[row]),
        }
    }

    /// The kinds of value the column holds, read off its buffer where its
    /// type says them. Every typed buffer holds at least one valid value: a
    /// column takes its type from the first.
    pub(crate) fn kinds(&self) -> Kinds {
        match &self.data {
            ColumnData::Null => Kinds::NONE,
            ColumnData::Bool(_) => Kinds::BOOL,
            ColumnData::Number { is_int, .. } => {
                let mut kinds = Kinds::NONE;
                if self.valid.any_where(is_int, true) {
                    kinds |= Kinds::INT;
                }
                if self.valid.any_where(is_int, false) {
                    kinds |= Kinds::FLOAT;
                }
                kinds
            }
            ColumnData::Str { .. } => Kinds::STR,
            ColumnData::Binary { .. } => Kinds::BINARY,
            ColumnData::Mixed(values) => values.iter().fold(Kinds::NONE, |k, v| k | Kinds::of(Value::of(v))),
        }
    }

    /// [`Self::value`], owned.
    pub(crate) fn get(&self, row: usize) -> JsonValue {
        self.value(row).to_owned()
    }

    /// [`Self::get`], moving a `Mixed` cell out rather than cloning it. The
    /// slot is left `Null`; only a column being dismantled calls this.
    pub(crate) fn take(&mut self, row: usize) -> JsonValue {
        match &mut self.data {
            ColumnData::Mixed(values) if self.valid.get(row) => std::mem::replace(&mut values[row], JsonValue::Null),
            _ => self.get(row),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Promotions on this thread, so a test can hold D-C3's "promoted
        /// once" to account rather than timing it.
        pub(super) static PROMOTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn a_column_promotes_at_most_twice_however_its_values_alternate() {
        PROMOTIONS.with(|n| n.set(0));
        let kinds = [JsonValue::Bool(true), JsonValue::Int(1), JsonValue::String("s".into()), JsonValue::Null];
        let cells: Vec<Option<JsonValue>> = (0..1_000).map(|i| Some(kinds[i % kinds.len()].clone())).collect();
        let c = column(&cells);
        // `Null` to `Bool`, then `Bool` to `Mixed`, and never again.
        assert_eq!(PROMOTIONS.with(|n| n.get()), 2);
        assert_eq!(read_back(&c), cells);
    }

    fn column(cells: &[Option<JsonValue>]) -> Column {
        let mut c = Column::new("k".into());
        for cell in cells {
            c.push(cell.clone());
        }
        c
    }

    fn read_back(c: &Column) -> Vec<Option<JsonValue>> {
        (0..c.len()).map(|r| c.is_present(r).then(|| c.get(r))).collect()
    }

    #[test]
    fn absent_and_null_stay_apart() {
        let cells = [Some(JsonValue::Int(1)), None, Some(JsonValue::Null), Some(JsonValue::Int(2))];
        let c = column(&cells);
        assert_eq!(read_back(&c), cells);
    }

    #[test]
    fn ints_and_floats_share_a_number_column_exactly() {
        let cells = [
            Some(JsonValue::Int(1)),
            Some(JsonValue::Float(1.0)),
            Some(JsonValue::Int(i64::MAX)),
            Some(JsonValue::Float(-0.0)),
        ];
        let c = column(&cells);
        assert!(matches!(c.data, ColumnData::Number { .. }));
        let back = read_back(&c);
        assert_eq!(back, cells);
        assert!(matches!(back[3], Some(JsonValue::Float(z)) if z.is_sign_negative()));
    }

    #[test]
    fn a_disagreeing_value_promotes_to_mixed_keeping_what_was_held() {
        let cells = [
            None,
            Some(JsonValue::Null),
            Some(JsonValue::String("a".into())),
            Some(JsonValue::Int(3)),
            Some(JsonValue::Array(vec![JsonValue::Bool(true)])),
        ];
        let c = column(&cells);
        assert!(matches!(c.data, ColumnData::Mixed(_)));
        assert_eq!(read_back(&c), cells);
    }

    #[test]
    fn a_column_that_starts_null_takes_the_first_value_s_type() {
        let cells = [Some(JsonValue::Null), None, Some(JsonValue::Bool(true)), Some(JsonValue::Bool(false))];
        let c = column(&cells);
        assert!(matches!(c.data, ColumnData::Bool(_)));
        assert_eq!(read_back(&c), cells);
        let bytes = [Some(JsonValue::Binary(vec![1, 2])), Some(JsonValue::Binary(vec![]))];
        assert_eq!(read_back(&column(&bytes)), bytes);
    }
}
