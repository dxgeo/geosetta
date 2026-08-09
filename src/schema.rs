//! Infer a flat, typed column schema from GeoJSON feature properties.
//!
//! Every property becomes one optional column. A column's type is the least
//! type that fits all of its non-null values:
//!   * all booleans            -> Bool
//!   * all integers            -> Int64
//!   * integers and/or floats  -> Double
//!   * anything else / mixed   -> String (values JSON-stringified)
//!
//! Columns appear in order of first appearance across the features.

use crate::feature::Feature;
use crate::json::JsonValue;
use std::collections::HashMap;

/// The resolved physical type of a property column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool,
    Int64,
    Double,
    String,
}

/// One materialized value in a column (one per feature/row).
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
}

/// A fully materialized property column.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    /// One cell per feature, in feature order.
    pub values: Vec<Cell>,
}

/// The type lattice used while scanning values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Infer {
    Bool,
    Int,
    Double,
    Str,
}

impl Infer {
    /// Least type that accommodates both `self` and `other`.
    fn merge(self, other: Infer) -> Infer {
        use Infer::*;
        match (self, other) {
            (a, b) if a == b => a,
            (Int, Double) | (Double, Int) => Double,
            _ => Str,
        }
    }

    fn of(value: &JsonValue) -> Option<Infer> {
        match value {
            JsonValue::Null => None,
            JsonValue::Bool(_) => Some(Infer::Bool),
            JsonValue::Number { is_int: true, .. } => Some(Infer::Int),
            JsonValue::Number { is_int: false, .. } => Some(Infer::Double),
            JsonValue::String(_) => Some(Infer::Str),
            // Arrays/objects can only be represented as JSON text.
            JsonValue::Array(_) | JsonValue::Object(_) => Some(Infer::Str),
        }
    }

    fn resolve(self) -> ColumnType {
        match self {
            Infer::Bool => ColumnType::Bool,
            Infer::Int => ColumnType::Int64,
            Infer::Double => ColumnType::Double,
            Infer::Str => ColumnType::String,
        }
    }
}

/// Scan all features and build the property columns.
///
/// Single pass in `O(rows × cols)`: a `HashMap` assigns each property key a
/// stable first-seen column index (no linear scan over known columns per
/// property), each key's type is merged as values are seen, and every row's
/// value is recorded per column with absent cells back-filled as `None`. A
/// second pass then resolves each column's type and materializes its cells.
pub fn infer_columns(features: &[Feature]) -> Vec<Column> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    let mut types: Vec<Option<Infer>> = Vec::new();
    // Per column, the value at each row (borrowed from `features`); `None` marks
    // a row that omitted the key. Kept row-aligned as the pass proceeds.
    let mut raw: Vec<Vec<Option<&JsonValue>>> = Vec::new();

    for (row, feature) in features.iter().enumerate() {
        for (key, value) in &feature.properties {
            let idx = *index.entry(&**key).or_insert_with(|| {
                let i = order.len();
                order.push(&**key);
                types.push(None);
                // Back-fill the rows before this key first appeared.
                let mut col = Vec::with_capacity(features.len());
                col.resize(row, None);
                raw.push(col);
                i
            });
            if let Some(inf) = Infer::of(value) {
                types[idx] = Some(match types[idx] {
                    Some(existing) => existing.merge(inf),
                    None => inf,
                });
            }
            // Record this row's value; on a duplicate key within the row the
            // first occurrence wins (matching the old `find`-based lookup).
            if raw[idx].len() == row {
                raw[idx].push(Some(value));
            }
        }
        // Any column not touched this row gets a Null cell for it.
        for col in raw.iter_mut() {
            if col.len() == row {
                col.push(None);
            }
        }
    }

    // Materialize each column, filling missing/null cells with Null.
    order
        .into_iter()
        .zip(types)
        .zip(raw)
        .map(|((name, ty), col)| {
            // Columns whose values were all null default to String.
            let ty = ty.map(Infer::resolve).unwrap_or(ColumnType::String);
            let values = col.into_iter().map(|v| materialize(ty, v)).collect();
            Column {
                name: name.to_string(),
                ty,
                values,
            }
        })
        .collect()
}

/// Convert one property value into a typed [`Cell`] for column type `ty`.
fn materialize(ty: ColumnType, value: Option<&JsonValue>) -> Cell {
    let value = match value {
        None | Some(JsonValue::Null) => return Cell::Null,
        Some(v) => v,
    };
    match ty {
        ColumnType::Bool => match value {
            JsonValue::Bool(b) => Cell::Bool(*b),
            _ => Cell::Null,
        },
        ColumnType::Int64 => match value.as_f64() {
            Some(n) => Cell::Int(n as i64),
            None => Cell::Null,
        },
        ColumnType::Double => match value.as_f64() {
            Some(n) => Cell::Double(n),
            None => Cell::Null,
        },
        ColumnType::String => match value {
            JsonValue::String(s) => Cell::Str(s.clone()),
            // Numbers, bools, arrays, objects in a string column: stringify.
            other => Cell::Str(other.to_json_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn features(docs: &[&str]) -> Vec<Feature> {
        docs.iter()
            .map(|d| Feature {
                geometry: None,
                properties: parse(d)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(k, v)| (std::rc::Rc::from(k.as_str()), v.clone()))
                    .collect(),
            })
            .collect()
    }

    #[test]
    fn infers_types_and_promotes() {
        let f = features(&[r#"{"b":true,"i":1,"x":1}"#, r#"{"b":false,"i":2,"x":1.5}"#]);
        let cols = infer_columns(&f);
        assert_eq!(cols[0].ty, ColumnType::Bool);
        assert_eq!(cols[1].ty, ColumnType::Int64);
        // int then float promotes to Double.
        assert_eq!(cols[2].ty, ColumnType::Double);
    }

    #[test]
    fn mixed_types_fall_back_to_string() {
        let f = features(&[r#"{"m":1}"#, r#"{"m":"two"}"#, r#"{"m":[1,2]}"#]);
        let cols = infer_columns(&f);
        assert_eq!(cols[0].ty, ColumnType::String);
        assert_eq!(cols[0].values[0], Cell::Str("1".into()));
        assert_eq!(cols[0].values[1], Cell::Str("two".into()));
        assert_eq!(cols[0].values[2], Cell::Str("[1,2]".into()));
    }

    #[test]
    fn missing_keys_become_null() {
        let f = features(&[r#"{"a":1}"#, r#"{"b":2}"#]);
        let cols = infer_columns(&f);
        // Column "a": present in row 0, missing in row 1.
        let a = cols.iter().find(|c| c.name == "a").unwrap();
        assert_eq!(a.values[0], Cell::Int(1));
        assert_eq!(a.values[1], Cell::Null);
    }
}
