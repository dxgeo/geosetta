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

use crate::feature::{Feature, FeatureCollection};
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

/// A column type *as the source declared it*, in a vocabulary wide enough to
/// carry what the declaring spokes name.
///
/// Distinct from [`ColumnType`], and deliberately so. `ColumnType` is the
/// hub's four-value lattice — what a cell is *stored as* in memory, inferred
/// from values. `DeclType` is what a *file* said the column is, which is a
/// richer thing: FlatGeobuf's `Column.type` names fifteen types, GeoPackage
/// declares SQL types, and a `.dbf` field descriptor names five.
///
/// Collapsing the second into the first is defect 5 in
/// `plans/same-format-fidelity.org`: it is why a FlatGeobuf `Integer(Int16)`
/// column used to come back `Integer64`, a `Float32` as float64, and — the one
/// that is a semantic downgrade rather than a widening — a `DateTime` as
/// `String`.
///
/// Every variant here is named by at least one spoke's own on-disk vocabulary.
/// Nothing is invented for symmetry: there is no `Decimal` or `List` because
/// no spoke this crate reads declares one today, and a variant no reader
/// produces is a variant every writer must handle for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclType {
    Bool,
    Int8,
    UInt8,
    Int16,
    UInt16,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Float32,
    Float64,
    String,
    /// Text that is known to be JSON. FlatGeobuf and GeoPackage both say so.
    Json,
    /// A date and time. Carried as text in every spoke this crate writes, which
    /// is exactly why it needs declaring — text is what it degrades *into*.
    DateTime,
    /// A date with no time part.
    Date,
    Binary,
}

impl DeclType {
    /// The hub type a value of this declared type is stored as.
    ///
    /// Many-to-one: every integer width collapses to [`ColumnType::Int64`] and
    /// every text-ish type to [`ColumnType::String`], because that is all the
    /// IR's cells can hold. The declaration is what remembers the difference.
    pub fn hub(self) -> ColumnType {
        use DeclType::*;
        match self {
            Bool => ColumnType::Bool,
            Int8 | UInt8 | Int16 | UInt16 | Int32 | UInt32 | Int64 | UInt64 => ColumnType::Int64,
            Float32 | Float64 => ColumnType::Double,
            String | Json | DateTime | Date | Binary => ColumnType::String,
        }
    }

    /// The inclusive range an integer type admits, if it is one.
    ///
    /// Used to decline a declaration the data has outgrown — see
    /// [`FieldDecl::type_for`].
    pub fn int_range(self) -> Option<(i64, i64)> {
        use DeclType::*;
        Some(match self {
            Int8 => (i8::MIN as i64, i8::MAX as i64),
            UInt8 => (0, u8::MAX as i64),
            Int16 => (i16::MIN as i64, i16::MAX as i64),
            UInt16 => (0, u16::MAX as i64),
            Int32 => (i32::MIN as i64, i32::MAX as i64),
            UInt32 => (0, u32::MAX as i64),
            Int64 => (i64::MIN, i64::MAX),
            // A u64 above i64::MAX cannot be held by `Cell::Int` at all, so the
            // representable range is the one that matters here.
            UInt64 => (0, i64::MAX),
            _ => return None,
        })
    }
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
    /// What the *source* declared this column to be, when it declared
    /// anything — see [`FieldDecl`]. `None` when nothing was declared, which
    /// is every format that has no schema of its own (GeoJSON, CSV, WKT, KML)
    /// and every collection built in memory.
    ///
    /// A writer treats this as a *floor*, never a ceiling: it may widen a
    /// declaration to fit the data it was handed, but must not narrow one to
    /// fit it. See [`declared_or`].
    pub decl: Option<FieldDecl>,
}

impl Column {
    /// The declared type to write this column as, once reconciled against the
    /// cells actually present — see [`FieldDecl::type_for`].
    ///
    /// `None` means "write whatever [`Column::ty`] implies": either nothing was
    /// declared, or what was declared no longer describes the data.
    pub fn declared_type(&self) -> Option<DeclType> {
        self.decl.as_ref().and_then(|d| d.type_for(self.ty, &self.values))
    }
}

/// Reconcile a source's declared size with the size this column's data
/// actually needs: the declaration wins when it is wider, the data when it is.
///
/// That asymmetry is the whole reason declarations are carried. An 80-character
/// `.dbf` field currently holding one-character values must come back out 80
/// characters wide, because re-deriving it as 1 produces a file that holds
/// today's rows and cannot hold tomorrow's. The `max` in the other direction is
/// not merely defensive: a route (a) caller may lengthen a value between
/// [`crate::read_features`] and [`crate::write_features`], and honouring a
/// now-too-narrow declaration would truncate what they wrote.
pub fn declared_or(declared: Option<u32>, required: usize) -> usize {
    match declared {
        Some(w) => required.max(w as usize),
        None => required,
    }
}

/// What a source format declared about one property column, as distinct from
/// what its values happen to be.
///
/// **Modelled in the IR rather than carried as a [`crate::Native`]**, which is
/// the exception D1 in `plans/same-format-fidelity.org` names alongside the
/// collection's name. Two reasons, and the second is the load-bearing one:
///
/// 1. Four spokes give these a *first-class slot* — Shapefile's `.dbf` field
///    descriptor, FlatGeobuf's `Column` table, GeoPackage's column
///    declarations, GeoParquet's `ARROW:schema`. A member four independent
///    format specs each chose to model is not one wire format's quirk.
/// 2. A native does not cross a pipe (D5), so a Shapefile-tagged width payload
///    never reaches a middle stage. A *modelled* field rides in the transit
///    format's own slot, so in
///    `geosetta a.shp - --to fgb | tool | geosetta - b.shp --from fgb` the
///    middle tool can read the declaration, widen `width: 80` to `120`
///    alongside the value it lengthened, and have the Shapefile writer honour
///    it. No payload could do that.
///
/// Field names and meanings follow FlatGeobuf's `Column` table, which is the
/// only one of the four that names all six: `width` is the encoded width of a
/// value (a string's length), `precision` the number of significant digits and
/// `scale` the number of digits after the decimal point (numbers only).
/// GDAL follows the same split — verified against `ogr2ogr`-written FlatGeobuf,
/// where a `.dbf` `String (80.0)` becomes `width: 80` and a `Real (23.15)`
/// becomes `precision: 23, scale: 15`, with no `width` at all.
///
/// Every field is `Option` because "the source declared nothing" and "the
/// source declared zero" are different facts, and only the first may be
/// re-derived from the data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FieldDecl {
    /// The property this describes, matched to [`Column::name`] exactly.
    pub name: String,
    /// The type the source declared this column to be, in that source's own
    /// vocabulary rather than the hub's — see [`DeclType`], and
    /// [`FieldDecl::type_for`] for the rule that decides whether it still
    /// applies to the data at hand.
    pub ty: Option<DeclType>,
    /// Encoded width of a value — a string field's declared length.
    pub width: Option<u32>,
    /// Significant digits, numbers only. This is what OGR calls a numeric
    /// field's *width*, and what a `.dbf` numeric descriptor stores as its
    /// field length.
    pub precision: Option<u32>,
    /// Digits after the decimal point, numbers only. OGR's *precision*, and a
    /// `.dbf` numeric descriptor's `decimals`.
    pub scale: Option<u32>,
    /// Whether the column admits nulls. `Some(false)` is a `NOT NULL`
    /// constraint the source actually declared.
    pub nullable: Option<bool>,
    pub unique: Option<bool>,
    pub primary_key: Option<bool>,
}

impl FieldDecl {
    /// A declaration for `name` that declares nothing yet.
    pub fn new(name: impl Into<String>) -> FieldDecl {
        FieldDecl { name: name.into(), ..FieldDecl::default() }
    }

    /// Whether this carries no declaration at all, in which case there is
    /// nothing to preserve and it need not be recorded.
    pub fn is_empty(&self) -> bool {
        self.ty.is_none()
            && self.width.is_none()
            && self.precision.is_none()
            && self.scale.is_none()
            && self.nullable.is_none()
            && self.unique.is_none()
            && self.primary_key.is_none()
    }

    /// The declared type to write this column as, or `None` to fall back to
    /// whatever the data inferred.
    ///
    /// **The declaration wins where the data is silent; the data wins where it
    /// contradicts.** That is the same asymmetry [`declared_or`] applies to
    /// widths, for the same reason: a declaration describes what the source
    /// *could* hold, and honouring a stale one must never destroy a value a
    /// later stage actually produced.
    ///
    /// So an all-null column keeps its declared `Int16` (the data says
    /// nothing, and re-inferring would call it `String`), but a column a middle
    /// stage refilled with text declines it. The integer case is checked
    /// against the *values*, not just the inferred type: a middle stage that
    /// writes `40000` into a declared `Int16` column has outgrown the
    /// declaration, and writing it as a 16-bit field would corrupt the number.
    pub fn type_for(&self, inferred: ColumnType, values: &[Cell]) -> Option<DeclType> {
        let declared = self.ty?;
        // A declaration only speaks for cells the hub stores the same way. An
        // all-null column infers as `String` but has no value to contradict
        // anything, so it keeps whatever it was declared.
        let all_null = values.iter().all(|c| matches!(c, Cell::Null));
        if !all_null && declared.hub() != inferred {
            return None;
        }
        if let Some((lo, hi)) = declared.int_range() {
            let fits = values.iter().all(|c| match c {
                Cell::Int(v) => *v >= lo && *v <= hi,
                _ => true,
            });
            if !fits {
                return None;
            }
        }
        Some(declared)
    }
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
/// Two passes, neither doing per-row work proportional to the *total* column
/// count (unlike a design that back-fills every known column on every row):
///
/// 1. Discover the column set (a `HashMap` assigns each property key a stable
///    first-seen index) and merge each column's type, without materializing
///    any per-row values yet.
/// 2. Append each column's cells in row order as they're encountered. A
///    column not touched by the current row is left alone rather than
///    eagerly padded — the gap is back-filled with `Null` only when (a) that
///    column is touched again later, or (b) the pass ends and it never is.
///    So a fully dense table (every row has every key, the common case) costs
///    one push per cell and never touches a column it isn't writing to; only
///    genuinely sparse data pays for back-filling, proportional to the gaps
///    it actually has.
pub fn infer_columns(features: &[Feature]) -> Vec<Column> {
    let mut index: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    let mut types: Vec<Option<Infer>> = Vec::new();

    for feature in features {
        for (key, value) in &feature.properties {
            let idx = *index.entry(&**key).or_insert_with(|| {
                let i = order.len();
                order.push(&**key);
                types.push(None);
                i
            });
            if let Some(inf) = Infer::of(value) {
                types[idx] = Some(match types[idx] {
                    Some(existing) => existing.merge(inf),
                    None => inf,
                });
            }
        }
    }

    // Columns whose values were all null default to String.
    let resolved: Vec<ColumnType> =
        types.iter().map(|t| t.map(Infer::resolve).unwrap_or(ColumnType::String)).collect();

    let n = features.len();
    let mut columns: Vec<Vec<Cell>> = resolved.iter().map(|_| Vec::with_capacity(n)).collect();

    for (row, feature) in features.iter().enumerate() {
        for (key, value) in &feature.properties {
            let idx = *index.get(&**key).expect("every key was indexed in the first pass");
            let col = &mut columns[idx];
            match col.len().cmp(&row) {
                // Common case: this column was written last row too, so it's
                // exactly caught up — just append.
                std::cmp::Ordering::Equal => col.push(materialize(resolved[idx], Some(value))),
                // This column skipped one or more rows since its last
                // appearance: back-fill the gap, then append.
                std::cmp::Ordering::Less => {
                    col.resize(row, Cell::Null);
                    col.push(materialize(resolved[idx], Some(value)));
                }
                // Already written this row: a duplicate key within one row's
                // properties, where the first occurrence wins (matching the
                // old `find`-based lookup).
                std::cmp::Ordering::Greater => {}
            }
        }
    }
    // Any column whose last appearance wasn't the final row is still short.
    for col in columns.iter_mut() {
        col.resize(n, Cell::Null);
    }

    order
        .into_iter()
        .zip(resolved)
        .zip(columns)
        .map(|((name, ty), values)| Column { name: name.to_string(), ty, values, decl: None })
        .collect()
}

/// [`infer_columns`] over `fc`'s features, with each column's [`FieldDecl`]
/// attached from [`FeatureCollection::schema`].
///
/// Every writer that materializes property columns should call this rather
/// than [`infer_columns`] directly — inference alone re-derives the schema from
/// today's values, which is exactly the narrowing the declarations exist to
/// prevent.
///
/// A declaration naming a property the features do not have is dropped: it
/// describes a column that will not be written, and inventing an empty column
/// for it would add data rather than preserve it. Matching is by exact name,
/// since a property key is a key.
pub fn columns_for(fc: &FeatureCollection) -> Vec<Column> {
    let mut columns = infer_columns(&fc.features);
    if fc.schema.is_empty() {
        return columns;
    }
    let by_name: HashMap<&str, &FieldDecl> =
        fc.schema.iter().map(|d| (d.name.as_str(), d)).collect();
    for col in &mut columns {
        col.decl = by_name.get(col.name.as_str()).map(|d| (*d).clone());
    }
    columns
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
                native: None,
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

    #[test]
    fn columns_for_attaches_declarations_by_name() {
        let mut fc = FeatureCollection::new(features(&[r#"{"a":"x","b":1}"#]));
        fc.schema = vec![FieldDecl { name: "a".into(), width: Some(80), ..FieldDecl::default() }];
        let cols = columns_for(&fc);
        assert_eq!(cols[0].decl.as_ref().unwrap().width, Some(80));
        // A property the source declared nothing about carries no declaration
        // rather than an empty one.
        assert_eq!(cols[1].decl, None);
    }

    #[test]
    fn a_declaration_for_an_absent_property_is_dropped() {
        // It describes a column that will not be written; inventing an empty
        // column for it would add data rather than preserve it.
        let mut fc = FeatureCollection::new(features(&[r#"{"a":"x"}"#]));
        fc.schema = vec![FieldDecl { name: "gone".into(), width: Some(80), ..FieldDecl::default() }];
        let cols = columns_for(&fc);
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].name, "a");
        assert_eq!(cols[0].decl, None);
    }

    #[test]
    fn a_collection_with_no_schema_is_unchanged_by_columns_for() {
        let fc = FeatureCollection::new(features(&[r#"{"a":"x"}"#]));
        let inferred = infer_columns(&fc.features);
        let attached = columns_for(&fc);
        assert_eq!(attached.len(), inferred.len());
        assert!(attached.iter().all(|c| c.decl.is_none()));
    }

    /// The declaration is a floor, never a ceiling — the property the whole
    /// mechanism exists for, in both directions.
    #[test]
    fn declared_or_widens_but_never_narrows() {
        // The source declared 80 for data that currently needs 1: honour 80,
        // so the output file can still hold tomorrow's rows.
        assert_eq!(declared_or(Some(80), 1), 80);
        // The data needs 120 against a declaration of 80 — an external tool
        // lengthened a value mid-pipeline. The data wins; truncating it to the
        // declaration would lose what the caller just wrote.
        assert_eq!(declared_or(Some(80), 120), 120);
        // Nothing declared: the data is the only source of truth.
        assert_eq!(declared_or(None, 7), 7);
    }

    #[test]
    fn a_declaration_that_declares_nothing_is_empty() {
        assert!(FieldDecl::new("a").is_empty());
        assert!(!FieldDecl { name: "a".into(), scale: Some(0), ..FieldDecl::default() }.is_empty());
    }

    #[test]
    fn duplicate_key_in_one_row_keeps_the_first_value() {
        // The JSON object model preserves duplicate keys as separate members
        // (it's a Vec, not a map), so a row can legitimately carry the same
        // property twice; the first occurrence should win.
        let f = features(&[r#"{"a":1,"a":2}"#]);
        let cols = infer_columns(&f);
        assert_eq!(cols[0].values[0], Cell::Int(1));
    }
}
