//! Decode a GeoPackage into its layers.

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::geometry::from_wkb;
use crate::json::JsonValue;
use crate::format::Format;
use crate::native::{
    GpkgConstraint, GpkgDataColumn, GpkgFeatureNative, GpkgNative, Native, NativePayload,
};
use crate::schema::{DeclType, FieldDecl};
use crate::sqlite::{Database, Table, Value};
use std::rc::Rc;

/// Read every feature layer from a GeoPackage as `(layer_name, features)`.
pub fn read_layers(bytes: &[u8]) -> Result<Vec<(String, FeatureCollection)>> {
    let db = Database::open(bytes)?;
    let tables = db.tables()?;

    // gpkg_contents lists every dataset; keep the ones with data_type='features'.
    let feature_tables = feature_layer_names(&db, &tables)?;
    // gpkg_geometry_columns says which column holds geometry (and its SRS) per
    // table; gpkg_spatial_ref_sys defines each SRS.
    let geom_columns = geometry_columns(&db, &tables)?;
    let srs = spatial_ref_systems(&db, &tables)?;

    // The layer-level unmodelled members (M3's GeoPackage spoke): the
    // `gpkg_contents` title/description and the `gpkg_data_columns` rows. Both
    // tables are read once for the whole file rather than per layer.
    let contents_meta = contents_metadata(&db, &tables)?;
    let data_columns = data_columns(&db, &tables)?;
    let constraints = data_column_constraints(&db, &tables)?;

    let mut layers = Vec::new();
    for name in feature_tables {
        let Some(table) = tables.iter().find(|t| t.name == name) else {
            continue;
        };
        let geom = geom_columns.iter().find(|g| g.table == name);
        let geom_idx =
            geom.and_then(|g| table.columns.iter().position(|c| *c == g.column));

        let mut fc = read_layer(&db, table, geom_idx)?;
        // Carry the layer's coordinate reference system through unchanged.
        fc.crs = geom.and_then(|g| resolve_crs(g.srs_id, &srs));
        // The table name *is* the layer's identity, so it belongs in the IR
        // rather than being reconstructed by the caller. Without this a
        // `gpkg -> fgb | tool | gpkg` edit loop writes its result into a new
        // table called `layer` and leaves the pre-edit features standing in the
        // original — see `plans/edit-composability.org` D4.
        fc.name = Some(name.clone());
        fc.native = layer_native(&name, &contents_meta, &data_columns, &constraints);
        layers.push((name, fc));
    }
    Ok(layers)
}

/// A row of `gpkg_geometry_columns`: which column holds geometry, in which SRS.
struct GeomColumn {
    table: String,
    column: String,
    srs_id: i64,
}

/// A row of `gpkg_spatial_ref_sys`.
struct SrsRow {
    srs_id: i64,
    organization: String,
    organization_coordsys_id: i64,
    definition: String,
}

/// Turn a layer's `srs_id` into a [`Crs`], looking its definition up in
/// `gpkg_spatial_ref_sys`. The two GeoPackage "undefined" SRSes (`0`, `-1`) and
/// an unknown `srs_id` mean no CRS; EPSG:4326 collapses to [`Crs::Wgs84`]; every
/// other row is carried through as a [`Crs::Named`] with its WKT `definition`.
fn resolve_crs(srs_id: i64, srs: &[SrsRow]) -> Option<Crs> {
    if srs_id == 0 || srs_id == -1 {
        return None;
    }
    let row = srs.iter().find(|s| s.srs_id == srs_id)?;
    let wkt = (row.definition != "undefined" && !row.definition.is_empty())
        .then(|| row.definition.clone());
    Some(Crs::from_authority_code(
        Some(row.organization.clone()),
        Some(row.organization_coordsys_id.to_string()),
        wkt,
        None,
    ))
}

/// Read `gpkg_spatial_ref_sys` into rows. Absent (a malformed GeoPackage) is
/// treated as no known SRS definitions.
fn spatial_ref_systems(db: &Database, tables: &[Table]) -> Result<Vec<SrsRow>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_spatial_ref_sys") else {
        return Ok(Vec::new());
    };
    let id_col = column_index(t, "srs_id")?;
    let org_col = column_index(t, "organization")?;
    let org_id_col = column_index(t, "organization_coordsys_id")?;
    let def_col = column_index(t, "definition")?;

    let mut out = Vec::new();
    for row in db.read_rows(t)? {
        // srs_id is an INTEGER PRIMARY KEY (rowid alias); the reader has already
        // backfilled the rowid into that column, so it reads as a plain Int.
        let srs_id = match row.get(id_col) {
            Some(Value::Int(n)) => *n,
            _ => continue,
        };
        let text = |i: usize| match row.get(i) {
            Some(Value::Text(s)) => s.clone(),
            _ => String::new(),
        };
        let org_id = match row.get(org_id_col) {
            Some(Value::Int(n)) => *n,
            _ => srs_id,
        };
        out.push(SrsRow {
            srs_id,
            organization: text(org_col),
            organization_coordsys_id: org_id,
            definition: text(def_col),
        });
    }
    Ok(out)
}

/// The feature id as a [`Native`], or `None` when the writer would emit it
/// anyway.
///
/// **D16 in one line:** a member equal to what the writer produces on its own
/// is *derived*, not carried. `super::writer` numbers features densely from 1,
/// so a layer nobody has deleted a row out of carries nothing here — which is
/// every file this crate wrote and most files anyone else did.
///
/// The value is real content when it differs: Requirement 29's note makes the
/// feature id the target of `gpkg_metadata_reference`'s row-level links, so
/// renumbering breaks a reference the standard defines. Destroying it silently
/// was `handoff.org` § BUGS FOUND entry 19.
fn carried_fid(row: &[Value], rowid_alias: Option<usize>, position: usize) -> Option<Native> {
    let fid = match rowid_alias.and_then(|i| row.get(i)) {
        Some(Value::Int(n)) => *n,
        // No rowid-alias column, so the source exposes no id to carry. The
        // table still has rowids; they are simply not addressable as data, and
        // nothing can reference one it cannot name.
        _ => return None,
    };
    if fid == (position + 1) as i64 {
        return None;
    }
    Some(Native::new(
        Format::Gpkg,
        NativePayload::GpkgFeature(GpkgFeatureNative { fid: Some(fid) }),
    ))
}

fn read_layer(db: &Database, table: &Table, geom_idx: Option<usize>) -> Result<FeatureCollection> {
    let rowid_alias = table.rowid_alias();
    let mut features = Vec::new();
    // Intern each column's key once so every row shares one `Rc`.
    let keys: Vec<Rc<str>> = table.columns.iter().map(|n| Rc::from(n.as_str())).collect();
    // Resolved once per layer rather than per cell: the DDL does not change
    // between rows, and `decl_type` does string work.
    let decl_types: Vec<Option<DeclType>> = (0..table.columns.len())
        .map(|i| table.column_types.get(i).and_then(|t| t.as_deref()).and_then(decl_type))
        .collect();

    for (position, row) in db.read_rows(table)?.into_iter().enumerate() {
        let geometry = match geom_idx.and_then(|i| row.get(i)) {
            Some(Value::Blob(b)) => Some(from_wkb(strip_gpkg_header(b)?)?),
            _ => None,
        };
        // Every column except the geometry and the rowid-alias becomes a property.
        let properties = keys
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != geom_idx && Some(*i) != rowid_alias)
            .map(|(i, key)| (Rc::clone(key), value_to_json(&row[i], decl_types[i])))
            .collect();
        features.push(Feature {
            geometry,
            properties,
            native: carried_fid(&row, rowid_alias, position),
        });
    }
    // Carry each column's declared type into the IR, so it survives to a
    // target that can express it. The geometry and rowid-alias columns are
    // excluded for the same reason they are not properties.
    let schema = keys
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != geom_idx && Some(*i) != rowid_alias)
        .filter_map(|(i, key)| {
            let decl = FieldDecl { name: key.to_string(), ty: decl_types[i], ..Default::default() };
            (!decl.is_empty()).then_some(decl)
        })
        .collect();

    // The caller fills in `crs` from the layer's srs_id, and `name` from the
    // table name — both live in `read_layers`, which is where that context is.
    let mut fc = FeatureCollection::new(features);
    fc.schema = schema;
    Ok(fc)
}

/// A GeoPackage column's declared SQL type in the IR's vocabulary.
///
/// GeoPackage names a closed set (spec table 1: *GeoPackage Data Types*), but
/// real files also carry SQLite's own spellings and OGR's, so the match is on
/// the leading keyword with any size suffix (`VARCHAR(80)`) ignored — SQLite
/// itself resolves affinity by substring, and being stricter here would drop
/// declarations that every other tool honours.
fn decl_type(sql: &str) -> Option<DeclType> {
    let upper = sql.trim().to_ascii_uppercase();
    let head = upper.split(['(', ' ']).next().unwrap_or("");
    Some(match head {
        "BOOLEAN" | "BOOL" => DeclType::Bool,
        "TINYINT" => DeclType::Int8,
        "SMALLINT" | "INT2" => DeclType::Int16,
        "MEDIUMINT" | "INT4" => DeclType::Int32,
        "INT" | "INTEGER" | "BIGINT" | "INT8" => DeclType::Int64,
        "FLOAT" => DeclType::Float32,
        // `REAL` is 64-bit, in both the GeoPackage spec's table 1 and SQLite's
        // own storage classes — it belongs with `DOUBLE`, not with `FLOAT`.
        // It was grouped with `FLOAT` until 2026-08-21, which narrowed a
        // `price REAL` column to 32 bits and made the writer emit `FLOAT`, so
        // the loss was visible on disk and to GDAL (`Real` in, `Real(Float32)`
        // out) on a *same-format* run. Found while measuring the GeoPackage
        // spoke's round trip; it is defect 5's shape, one spelling later.
        "REAL" | "DOUBLE" | "DOUBLEPRECISION" | "NUMERIC" | "DECIMAL" => DeclType::Float64,
        "TEXT" | "VARCHAR" | "CHARACTER" | "CLOB" => DeclType::String,
        "JSON" => DeclType::Json,
        "DATETIME" | "TIMESTAMP" => DeclType::DateTime,
        "DATE" => DeclType::Date,
        "BLOB" | "BINARY" => DeclType::Binary,
        _ => return None,
    })
}

/// A `gpkg_contents` row's descriptive columns, by table name.
struct ContentsMeta {
    table: String,
    identifier: Option<String>,
    description: Option<String>,
    last_change: Option<String>,
}

/// Assemble one layer's [`GpkgNative`], or `None` when it carries nothing
/// (D3 — the common case, and what every file this crate writes produces).
///
/// `identifier` is dropped when it merely repeats the layer name: GDAL writes
/// that for every layer it creates, so keeping it would put a payload on
/// essentially every GeoPackage and replay a stale title through a rename. See
/// [`GpkgNative`]'s own doc for the D4 reasoning.
fn layer_native(
    name: &str,
    contents: &[ContentsMeta],
    data_columns: &[(String, GpkgDataColumn)],
    constraints: &[(String, GpkgConstraint)],
) -> Option<Native> {
    let meta = contents.iter().find(|c| c.table == name);
    let columns: Vec<GpkgDataColumn> = data_columns
        .iter()
        .filter(|(table, _)| table == name)
        .map(|(_, column)| column.clone())
        .collect();
    // Only the constraints this layer's columns actually name. A constraint
    // nothing refers to describes nothing, and carrying it would make the
    // payload a copy of a container-level table rather than a layer's content.
    let named: Vec<GpkgConstraint> = constraints
        .iter()
        .filter(|(constraint_name, _)| {
            columns.iter().any(|c| c.constraint_name.as_deref() == Some(constraint_name.as_str()))
        })
        .map(|(_, constraint)| constraint.clone())
        .collect();
    let native = GpkgNative {
        identifier: meta
            .and_then(|m| m.identifier.clone())
            .filter(|identifier| identifier != name),
        description: meta
            .and_then(|m| m.description.clone())
            .filter(|description| !description.is_empty()),
        // Dropped when it is the sentinel this crate writes, on the
        // `identifier` precedent: a value the writer regenerates anyway is not
        // worth a payload, so our own output still carries no native here.
        last_change: meta
            .and_then(|m| m.last_change.clone())
            .filter(|stamp| stamp != crate::geopackage::LAST_CHANGE),
        data_columns: columns,
        constraints: named,
    };
    (!native.is_empty()).then(|| Native::new(Format::Gpkg, NativePayload::Gpkg(native)))
}

/// Every `gpkg_contents` row's `identifier`, `description` and `last_change`.
fn contents_metadata(db: &Database, tables: &[Table]) -> Result<Vec<ContentsMeta>> {
    let Some(contents) = tables.iter().find(|t| t.name == "gpkg_contents") else {
        return Ok(Vec::new());
    };
    let table_col = column_index(contents, "table_name")?;
    // Both are optional in the spec, so a file missing either column is not
    // malformed — it just has nothing here to carry.
    let identifier_col = contents.columns.iter().position(|c| c == "identifier");
    let description_col = contents.columns.iter().position(|c| c == "description");
    let last_change_col = contents.columns.iter().position(|c| c == "last_change");
    Ok(db
        .read_rows(contents)?
        .iter()
        .filter_map(|row| match row.get(table_col) {
            Some(Value::Text(table)) => Some(ContentsMeta {
                table: table.clone(),
                identifier: identifier_col.and_then(|i| text_at(row, i)),
                description: description_col.and_then(|i| text_at(row, i)),
                last_change: last_change_col.and_then(|i| text_at(row, i)),
            }),
            _ => None,
        })
        .collect())
}

/// Every `gpkg_data_columns` row, paired with the table it describes. Absent
/// from most GeoPackages: the table exists only where the Schema extension is
/// in use.
fn data_columns(db: &Database, tables: &[Table]) -> Result<Vec<(String, GpkgDataColumn)>> {
    let Some(table) = tables.iter().find(|t| t.name == "gpkg_data_columns") else {
        return Ok(Vec::new());
    };
    let table_col = column_index(table, "table_name")?;
    let column_col = column_index(table, "column_name")?;
    let at = |name: &str| table.columns.iter().position(|c| c == name);
    let (name, title, description) = (at("name"), at("title"), at("description"));
    let (mime_type, constraint_name) = (at("mime_type"), at("constraint_name"));
    Ok(db
        .read_rows(table)?
        .iter()
        .filter_map(|row| match (row.get(table_col), row.get(column_col)) {
            (Some(Value::Text(owner)), Some(Value::Text(column))) => Some((
                owner.clone(),
                GpkgDataColumn {
                    column_name: column.clone(),
                    name: name.and_then(|i| text_at(row, i)),
                    title: title.and_then(|i| text_at(row, i)),
                    description: description.and_then(|i| text_at(row, i)),
                    mime_type: mime_type.and_then(|i| text_at(row, i)),
                    constraint_name: constraint_name.and_then(|i| text_at(row, i)),
                },
            )),
            _ => None,
        })
        .collect())
}

/// Every `gpkg_data_column_constraints` row, keyed by `constraint_name`.
fn data_column_constraints(
    db: &Database,
    tables: &[Table],
) -> Result<Vec<(String, GpkgConstraint)>> {
    let Some(table) = tables.iter().find(|t| t.name == "gpkg_data_column_constraints") else {
        return Ok(Vec::new());
    };
    let name_col = column_index(table, "constraint_name")?;
    let type_col = column_index(table, "constraint_type")?;
    let at = |name: &str| table.columns.iter().position(|c| c == name);
    let (value, description) = (at("value"), at("description"));
    let (min, min_inc) = (at("min"), at("min_is_inclusive"));
    let (max, max_inc) = (at("max"), at("max_is_inclusive"));
    Ok(db
        .read_rows(table)?
        .iter()
        .filter_map(|row| match (row.get(name_col), row.get(type_col)) {
            (Some(Value::Text(name)), Some(Value::Text(kind))) => Some((
                name.clone(),
                GpkgConstraint {
                    constraint_name: name.clone(),
                    constraint_type: kind.clone(),
                    value: value.and_then(|i| text_at(row, i)),
                    min: min.and_then(|i| number_at(row, i)),
                    min_is_inclusive: min_inc.and_then(|i| bool_at(row, i)),
                    max: max.and_then(|i| number_at(row, i)),
                    max_is_inclusive: max_inc.and_then(|i| bool_at(row, i)),
                    description: description.and_then(|i| text_at(row, i)),
                },
            )),
            _ => None,
        })
        .collect())
}

/// The text at `index`, or `None` where the cell is NULL or another storage
/// class. SQLite is dynamically typed, so a cell in a `TEXT` column need not
/// hold text; a non-text cell here is not something to guess at.
fn text_at(row: &[Value], index: usize) -> Option<String> {
    match row.get(index) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

/// The number at `index`. `NUMERIC` accepts either storage class, and GDAL
/// writes integers for whole bounds.
fn number_at(row: &[Value], index: usize) -> Option<f64> {
    match row.get(index) {
        Some(Value::Int(n)) => Some(*n as f64),
        Some(Value::Real(n)) => Some(*n),
        _ => None,
    }
}

/// The boolean at `index`. GeoPackage `BOOLEAN` is an `INTEGER` 0/1, the same
/// storage the declared-type work (M4, defect 2) already turns back into a
/// boolean for feature columns.
fn bool_at(row: &[Value], index: usize) -> Option<bool> {
    match row.get(index) {
        Some(Value::Int(n)) => Some(*n != 0),
        _ => None,
    }
}

/// Names of tables whose `gpkg_contents.data_type` is `features`.
fn feature_layer_names(db: &Database, tables: &[Table]) -> Result<Vec<String>> {
    let contents = tables
        .iter()
        .find(|t| t.name == "gpkg_contents")
        .ok_or_else(|| Error::Convert("geopackage: missing gpkg_contents".into()))?;
    let name_col = column_index(contents, "table_name")?;
    let type_col = column_index(contents, "data_type")?;

    let mut names = Vec::new();
    for row in db.read_rows(contents)? {
        if let (Some(Value::Text(name)), Some(Value::Text(kind))) =
            (row.get(name_col), row.get(type_col))
            && kind == "features"
        {
            names.push(name.clone());
        }
    }
    Ok(names)
}

/// Geometry column + SRS per table, from `gpkg_geometry_columns`.
fn geometry_columns(db: &Database, tables: &[Table]) -> Result<Vec<GeomColumn>> {
    let Some(gc) = tables.iter().find(|t| t.name == "gpkg_geometry_columns") else {
        return Ok(Vec::new());
    };
    let table_col = column_index(gc, "table_name")?;
    let column_col = column_index(gc, "column_name")?;
    let srs_col = column_index(gc, "srs_id")?;

    let mut out = Vec::new();
    for row in db.read_rows(gc)? {
        if let (Some(Value::Text(t)), Some(Value::Text(c))) =
            (row.get(table_col), row.get(column_col))
        {
            let srs_id = match row.get(srs_col) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            out.push(GeomColumn {
                table: t.clone(),
                column: c.clone(),
                srs_id,
            });
        }
    }
    Ok(out)
}

fn column_index(table: &Table, name: &str) -> Result<usize> {
    table
        .columns
        .iter()
        .position(|c| c == name)
        .ok_or_else(|| Error::Convert(format!("geopackage: {} has no {name} column", table.name)))
}

/// Strip the GeoPackage Binary header, returning the wrapped WKB. Layout:
/// `"GP"`, version, flags, srs_id (4), then an optional envelope, then WKB.
fn strip_gpkg_header(blob: &[u8]) -> Result<&[u8]> {
    if blob.len() < 8 || &blob[0..2] != b"GP" {
        return Err(Error::Convert("geopackage: bad geometry blob header".into()));
    }
    let flags = blob[3];
    let envelope_bytes = match (flags >> 1) & 0x07 {
        0 => 0,
        1 => 32, // XY
        2 => 48, // XYZ
        3 => 48, // XYM
        4 => 64, // XYZM
        _ => return Err(Error::Convert("geopackage: invalid envelope flag".into())),
    };
    let header_len = 8 + envelope_bytes;
    blob.get(header_len..)
        .ok_or_else(|| Error::Convert("geopackage: truncated geometry blob".into()))
}

/// One stored value as JSON, informed by what the column was *declared* to be.
///
/// The declaration matters because SQLite is dynamically typed and GeoPackage
/// leans on that: the spec requires a `BOOLEAN` column to store 0 or 1 as an
/// `INTEGER`, so a reader that looks only at the storage class cannot tell a
/// boolean from a small number. That was defect 2 — this crate's own writer
/// declared `BOOLEAN` correctly and its reader handed back `0`/`1`, so a
/// boolean did not survive its own format.
fn value_to_json(v: &Value, declared: Option<DeclType>) -> JsonValue {
    if let (Some(DeclType::Bool), Value::Int(n)) = (declared, v) {
        return JsonValue::Bool(*n != 0);
    }
    match v {
        Value::Null => JsonValue::Null,
        // Exactly, not through an `f64`. A GeoPackage `INTEGER` is 64-bit, and
        // routing it through a double turned `9007199254740993` into
        // `...992` on a *same-format* run until 2026-08-21.
        Value::Int(n) => JsonValue::Int(*n),
        Value::Real(f) => JsonValue::Float(*f),
        Value::Text(s) => JsonValue::String(s.clone()),
        // Carried as bytes. This returned `Null` until 2026-08-21, which meant
        // a `gpkg -> gpkg` run *destroyed every blob in the file* while its
        // BLOB declaration survived intact — so the schema looked right and the
        // data was gone. Measured: `X'0102'` in, `NULL` out, no warning.
        Value::Blob(b) => JsonValue::Binary(b.clone()),
    }
}

#[cfg(test)]
mod tests {
    /// **Every SQL spelling this reader claims to accept, and what it means.**
    ///
    /// Not "does it parse" — *how wide is it*. This is the test that was
    /// missing when `REAL` was grouped with `FLOAT` as 32-bit: the arm was
    /// written, reviewed and shipped, and nothing anywhere asserted that `REAL`
    /// is a double. Every entry below cites its width so a future edit has to
    /// argue with the spec rather than with a name that looks plausible.
    ///
    /// Widths are GeoPackage spec table 1 (*GeoPackage Data Types*), which
    /// agrees with SQLite's own storage classes on the two that matter here:
    /// `REAL` is 8 bytes, `FLOAT` is 4.
    #[test]
    fn every_accepted_sql_spelling_declares_the_width_the_spec_gives_it() {
        use DeclType::*;
        let cases: &[(&str, DeclType)] = &[
            ("BOOLEAN", Bool),
            ("BOOL", Bool),
            ("TINYINT", Int8),
            ("SMALLINT", Int16),
            ("INT2", Int16),
            ("MEDIUMINT", Int32),
            ("INT4", Int32),
            ("INT", Int64),
            ("INTEGER", Int64),
            ("BIGINT", Int64),
            ("INT8", Int64),
            // 4-byte.
            ("FLOAT", Float32),
            // 8-byte, all five of them. `REAL` sat in the line above until
            // 2026-08-21 and narrowed every column declared with it.
            ("REAL", Float64),
            ("DOUBLE", Float64),
            ("DOUBLEPRECISION", Float64),
            ("NUMERIC", Float64),
            ("DECIMAL", Float64),
            ("TEXT", String),
            ("VARCHAR", String),
            ("CHARACTER", String),
            ("CLOB", String),
            ("JSON", Json),
            ("DATETIME", DateTime),
            ("TIMESTAMP", DateTime),
            ("DATE", Date),
            ("BLOB", Binary),
            ("BINARY", Binary),
        ];
        for (sql, expected) in cases {
            assert_eq!(decl_type(sql), Some(*expected), "{sql}");
            // The size suffix and case-insensitivity the doc comment promises.
            assert_eq!(decl_type(&sql.to_ascii_lowercase()), Some(*expected), "{sql} lowercased");
            assert_eq!(decl_type(&format!("{sql}(80)")), Some(*expected), "{sql}(80)");
            assert_eq!(decl_type(&format!("  {sql}  ")), Some(*expected), "{sql} padded");
        }
        // SQLite permits a column with no declared type at all, and a
        // declaration nobody recognises must not be guessed at.
        assert_eq!(decl_type(""), None);
        assert_eq!(decl_type("GEOMETRY"), None);
        assert_eq!(decl_type("WHATEVER"), None);
    }

    /// The other half of the same claim: what the *writer* spells each declared
    /// type, read back through the table above, must land on a type that admits
    /// the original. Pairs with `every_declared_type_survives_or_is_admitted`
    /// in `convert.rs`, but names the on-disk spelling, which is what a person
    /// inspecting the file with `sqlite3` actually sees.
    #[test]
    fn every_declared_type_is_written_with_a_spelling_that_reads_back_wide_enough() {
        for ty in DeclType::ALL {
            let spelling = super::super::writer::sql_type_for(ty);
            let back = decl_type(spelling).unwrap_or_else(|| {
                panic!("writer emits {spelling:?} for {ty:?}, which this reader does not accept")
            });
            assert!(
                back.admits(ty),
                "{ty:?} is written {spelling:?}, which reads back as {back:?} — a narrowing"
            );
        }
    }

    use super::*;
    use crate::geometry::{Geometry, Position};

    /// A row that predates an `ALTER TABLE … ADD COLUMN` keeps a *short*
    /// record, so the added column's value exists only as the `DEFAULT` in the
    /// DDL. Padding those rows with NULL — which `read_rows` did until
    /// 2026-08-22 — reads back a value SQLite never stored.
    ///
    /// This is the type-vs-value blind spot again, one turn further out: the
    /// column's *declaration* survived perfectly, so every schema assertion in
    /// the suite stayed green while the values came back empty. `sqlite3` and
    /// `ogrinfo` both report `B` on this fixture; geosetta reported nothing.
    #[test]
    fn a_column_added_by_alter_table_reads_back_its_declared_default() {
        let bytes = include_bytes!("../../tests/fixtures/sqlite_altered.gpkg");
        let layers = read_layers(bytes).unwrap();
        let (_, fc) = &layers[0];
        assert_eq!(fc.features.len(), 2);
        for f in &fc.features {
            let get = |k: &str| f.properties.iter().find(|(n, _)| &**n == k).map(|(_, v)| v);
            assert_eq!(get("grade"), Some(&JsonValue::String("B".into())));
            assert_eq!(get("rank"), Some(&JsonValue::Int(5)));
        }
    }

    #[test]
    fn reads_a_geopackage_layer() {
        // DuckDB-written GPKG, layer "places" (fid/geom/name/pop/score).
        let bytes = include_bytes!("../../tests/fixtures/points.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers.len(), 1);
        let (name, fc) = &layers[0];
        assert_eq!(name, "places");
        assert_eq!(fc.features.len(), 2);

        // Geometry: GPKG-binary header stripped, WKB decoded.
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::new(10.0, 20.0))));
        // Properties exclude the geometry column and the fid rowid-alias.
        let props: Vec<&str> = fc.features[0].properties.iter().map(|(k, _)| &**k).collect();
        assert_eq!(props, vec!["name", "pop", "score"]);
        assert_eq!(fc.features[1].properties[0].1.as_str(), Some("Beta"));
        assert_eq!(fc.features[1].properties[1].1.as_f64(), Some(200.0));
    }

    #[test]
    fn reads_a_real_envelope_bearing_z_fixture() {
        // Real `ogr2ogr -f GPKG` output, found after `strip_gpkg_header`'s
        // envelope-size branch turned out to have no fixture coverage at
        // all: a `Point`'s degenerate bbox gets no envelope from GDAL, but
        // a real `LineString Z` does — flags=0x05, envelope indicator 2
        // (XYZ, 48 bytes), envelope values (0,20,0,10,0,10) confirmed by
        // hand-decoding the raw file to exactly match the geometry's true
        // bounds before committing this fixture. This is the real-fixture
        // exercise of the envelope-skip path the synthetic test below
        // stood in for.
        let bytes = include_bytes!("../../tests/fixtures/gdal_linestring_z.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(
            layers[0].1.features[0].geometry,
            Some(Geometry::LineString(vec![
                Position::with_z(0.0, 0.0, 0.0),
                Position::with_z(10.0, 10.0, 10.0),
                Position::with_z(20.0, 5.0, 3.0),
            ]))
        );
    }

    #[test]
    fn reads_a_real_m_bearing_fixture() {
        // GeoPackage was M5's "zero production code changes" milestone —
        // `strip_gpkg_header`/`from_wkb` are dimension-agnostic wrappers, so
        // M should already work for free the same way Z did, purely because
        // the WKB codec underneath (M2) is dimension-generic. Never had a
        // fixture confirming that until now. Sourced via `ogr2ogr -f GPKG`
        // from a `LINESTRING ZM (...)` WKT/CSV source (GDAL has no direct
        // GeoJSON-to-M path, since GeoJSON itself has no M concept — see
        // `plans/zm-geometry.org`'s M7 M follow-up for the same sourcing
        // trick applied to Shapefile). Confirmed via direct SQLite blob
        // inspection before trusting the fixture: the GPB header's envelope
        // indicator is 2 (XYZ, 48 bytes — GDAL's envelope only ever tracks
        // Z, not M, even for a ZM geometry) and the inner WKB's own type
        // code is `0x0BBA` = 3002 = ISO SFA `LineString` + 3000 (ZM),
        // matching `ogrinfo`/DuckDB both independently reading it back as
        // `LINESTRING ZM (0 0 1 10,10 10 2 20,20 5 3 30)`.
        let bytes = include_bytes!("../../tests/fixtures/gdal_linestring_zm.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(
            layers[0].1.features[0].geometry,
            Some(Geometry::LineString(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(10.0, 10.0, 2.0, 20.0),
                Position::with_zm(20.0, 5.0, 3.0, 30.0),
            ]))
        );
    }

    #[test]
    fn strip_gpkg_header_skips_every_envelope_size_per_the_spec_table() {
        // GeoPackage §2.1.3's envelope contents indicator code: 0 = none (0
        // bytes), 1 = XY (32), 2 = XYZ (48), 3 = XYM (48), 4 = XYZM (64).
        // Indicator 2 (XYZ) is now also covered by a real fixture (above);
        // this test still exercises every code, including the two (XYM,
        // XYZM) no real tool tried here happened to produce.
        let wkb = crate::geometry::to_wkb(&Geometry::Point(Position::with_z(1.0, 2.0, 3.0)));
        for (indicator, envelope_len) in [(0u8, 0usize), (1, 32), (2, 48), (3, 48), (4, 64)] {
            let mut blob = Vec::new();
            blob.extend_from_slice(b"GP");
            blob.push(0); // version
            blob.push(0x01 | (indicator << 1)); // flags: LE + this envelope size
            blob.extend_from_slice(&0i32.to_le_bytes()); // srs_id
            blob.resize(blob.len() + envelope_len, 0); // dummy envelope bytes
            blob.extend_from_slice(&wkb);
            let stripped = strip_gpkg_header(&blob).unwrap();
            assert_eq!(stripped, wkb.as_slice(), "indicator {indicator} (envelope {envelope_len} bytes)");
        }
    }
}
