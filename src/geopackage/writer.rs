//! Write GeoPackage files: build the required GPKG metadata tables plus one
//! feature table per layer, and serialize the whole thing with the from-scratch
//! [`crate::sqlite`] writer. Append is read-modify-write — read the existing
//! layers, upsert, and rewrite the complete file (see `plans/geopackage.org`).

use crate::crs::Crs;
use crate::error::Result;
use crate::feature::FeatureCollection;
use crate::geometry::{to_wkb, Bbox, Geometry};
use crate::schema::{columns_for, Cell, Column, ColumnType, DeclType};
use crate::format::Format;
use crate::native::{GpkgConstraint, GpkgDataColumn, GpkgNative, NativePayload};
use crate::sqlite::{MasterEntry, TableSpec, Value, autoindex_records, write_database};

use super::reader::read_layers;
use super::rtree;

const APPLICATION_ID: u32 = 0x4750_4B47; // "GPKG"
const USER_VERSION: u32 = 10200; // 1.2.0
/// srs_id of the WGS 84 default row (the well-known GeoPackage value).
const WGS84_SRS_ID: i64 = 4326;
/// srs_id of the "undefined geographic" row, used for layers whose source
/// recorded no CRS at all (e.g. from CSV or WKT input).
const UNDEFINED_SRS_ID: i64 = 0;
/// First srs_id handed out to a CRS that carries no usable authority code.
const SYNTHETIC_SRS_BASE: i64 = 100_000;
const LAST_CHANGE: &str = "1970-01-01T00:00:00.000Z";

// The registry tables' DDL, as the GeoPackage specification defines them —
// **including the table constraints**, which is what makes each of these a
// statement about the schema rather than a loose approximation of it.
//
// Every `UNIQUE` and every non-INTEGER `PRIMARY KEY` below obliges this writer
// to emit a `sqlite_autoindex_*` b-tree alongside the table; declaring one
// without the other produces a database SQLite reports as malformed. The
// obligation is carried by `crate::sqlite::TableSpec::autoindexes`, and each
// table's construction site names which constraint each index is for.
//
// **These were emitted constraint-free until 2026-08-23** — *defect 6* in
// `plans/same-format-fidelity.org` § THE DEFECTS. That was never a decision:
// `sqlite.rs` could not write an index b-tree, so the DDL that needed one was
// never written, and `gpkg_spatial_ref_sys` looked like the exception when it
// was really the only table whose key (`INTEGER PRIMARY KEY`, a rowid alias)
// needs no index at all.

/// `gpkg_spatial_ref_sys` — `srs_id INTEGER PRIMARY KEY` with **no
/// `NOT NULL`**: that is what the specification states, and `ogr2ogr` adds a
/// `NOT NULL` the spec does not ask for. Either way it is a rowid alias and
/// owns no index b-tree, which is why this was the one table already correct
/// when defect 6 was found.
const SPATIAL_REF_SYS_DDL: &str = "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER PRIMARY KEY, organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL, definition TEXT NOT NULL, description TEXT)";

/// `gpkg_contents` — the specification's definition, constraints included.
const CONTENTS_DDL: &str = "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL PRIMARY KEY, data_type TEXT NOT NULL, identifier TEXT UNIQUE, description TEXT DEFAULT '', last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER, CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id))";

/// `gpkg_geometry_columns` — two keys and two foreign keys.
const GEOMETRY_COLUMNS_DDL: &str = "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL, m TINYINT NOT NULL, CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name), CONSTRAINT uk_gc_table_name UNIQUE (table_name), CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name), CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";

/// `gpkg_data_columns` — the Schema extension's column metadata. **Two**
/// constraints: `ogr2ogr` writes only `pk_gdc`, and the specification also
/// requires `gdc_tn`.
const DATA_COLUMNS_DDL: &str = "CREATE TABLE gpkg_data_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, name TEXT, title TEXT, description TEXT, mime_type TEXT, constraint_name TEXT, CONSTRAINT pk_gdc PRIMARY KEY (table_name, column_name), CONSTRAINT gdc_tn UNIQUE (table_name, name))";

/// `gpkg_data_column_constraints` — the Schema extension's constraint
/// definitions. `ogr2ogr` writes this one with no constraint at all.
const DATA_COLUMN_CONSTRAINTS_DDL: &str = "CREATE TABLE gpkg_data_column_constraints (constraint_name TEXT NOT NULL, constraint_type TEXT NOT NULL, value TEXT, min NUMERIC, min_is_inclusive BOOLEAN, max NUMERIC, max_is_inclusive BOOLEAN, description TEXT, CONSTRAINT gdcc_ntv UNIQUE (constraint_name, constraint_type, value))";

/// `gpkg_extensions` — one row per extension in use.
const EXTENSIONS_DDL: &str = "CREATE TABLE gpkg_extensions (table_name TEXT, column_name TEXT, extension_name TEXT NOT NULL, definition TEXT NOT NULL, scope TEXT NOT NULL, CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name))";

const WGS84_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4326\"]]";

/// Upsert `new_layers` into an existing GeoPackage (or a fresh one if `existing`
/// is `None`), returning the complete file. Layers of the same name are
/// replaced. With `rtree`, every layer gets a SQLite R*Tree spatial index (the
/// opt-in GeoPackage RTree extension). With `envelope`, every geometry blob
/// carries a cached GPB envelope (see `gpb_envelope`); without it the blobs
/// are byte-identical to what this writer has always produced.
pub fn write_layers(
    existing: Option<&[u8]>,
    new_layers: &[(String, FeatureCollection)],
    rtree: bool,
    envelope: bool,
) -> Result<Vec<u8>> {
    let mut layers: Vec<(String, FeatureCollection)> = match existing {
        Some(bytes) => read_layers(bytes)?,
        None => Vec::new(),
    };
    for (name, fc) in new_layers {
        layers.retain(|(n, _)| n != name);
        layers.push((name.clone(), fc.clone()));
    }
    build(&layers, rtree, envelope)
}

fn build(layers: &[(String, FeatureCollection)], rtree: bool, envelope: bool) -> Result<Vec<u8>> {
    // Resolve every layer's srs_id up front (registering any non-default SRS),
    // so gpkg_contents, gpkg_geometry_columns, and each geometry blob all agree.
    let srs = resolve_srs(layers);

    let mut specs = vec![
        spatial_ref_sys(&srs.registrations),
        gpkg_contents(layers, &srs.per_layer),
        geometry_columns(layers, &srs.per_layer),
    ];
    // A GeoPackage has one `gpkg_extensions` table regardless of how many
    // extensions are in use, so every extension contributes rows to the same
    // combined table rather than each pushing its own `TableSpec`.
    let mut extension_rows: Vec<ExtensionRow> = Vec::new();
    if rtree {
        let names: Vec<String> = layers.iter().map(|(n, _)| n.clone()).collect();
        extension_rows.extend(rtree::extension_rows(&names));
    }
    if !extension_rows.is_empty() {
        specs.push(extensions_table(extension_rows));
    }
    // The Schema extension's two tables, replayed from the layers' natives.
    // Written only when some layer carried rows: an empty `gpkg_data_columns`
    // is not something a producer would have left behind, and D3 says a spoke
    // that recovered nothing writes nothing.
    specs.extend(schema_extension_tables(layers));
    for ((name, fc), &srs_id) in layers.iter().zip(&srs.per_layer) {
        specs.push(feature_table(name, fc, srs_id, envelope));
    }

    // The spatial indexes (shadow tables) and their virtual-table + trigger
    // schema entries, if requested. Shadow tables are appended after the
    // feature tables so their b-trees are built alongside the rest.
    let mut master: Vec<MasterEntry> = Vec::new();
    if rtree {
        for (name, fc) in layers {
            let index = rtree::build(name, fc);
            specs.extend(index.tables);
            master.extend(index.master);
        }
    }

    write_database(&specs, &master, APPLICATION_ID, USER_VERSION)
}

/// One `gpkg_extensions` registration row. Every extension in use (RTree,
/// R5's CRS-WKT) contributes rows of this shape, combined by [`build`] into a
/// single `gpkg_extensions` table.
pub(crate) struct ExtensionRow {
    pub(crate) table_name: String,
    pub(crate) column_name: String,
    pub(crate) extension_name: String,
    pub(crate) definition: String,
    pub(crate) scope: String,
}

/// This layer's GeoPackage payload, if it carried one. `None` for every
/// collection that came from another format — the tag check in
/// [`crate::Native::payload_for`] is what makes a cross-format run drop it
/// rather than replay someone else's members.
fn gpkg_native(fc: &FeatureCollection) -> Option<&GpkgNative> {
    match fc.native.as_ref()?.payload_for(Format::Gpkg)? {
        NativePayload::Gpkg(native) => Some(native),
        // The tag matched, so this is unreachable unless a second GeoPackage
        // payload variant is added without updating this arm.
        _ => None,
    }
}

/// `gpkg_data_columns` and `gpkg_data_column_constraints`, rebuilt from every
/// layer's payload. Empty when no layer carried either.
///
/// Constraints are carried *per layer* (a layer that names one keeps its own
/// copy) but the table is container-scoped, so identical rows shared by two
/// layers are de-duplicated here on the way out. Rows describing a column the
/// layer no longer has are dropped: this is the same "an opaque member can
/// also be derived" ruling decision 14 made for `ARROW:schema`, since a
/// dangling `gpkg_data_columns` row is a description of nothing and GDAL
/// surfaces it as an alias on a field that is not there.
fn schema_extension_tables(layers: &[(String, FeatureCollection)]) -> Vec<TableSpec> {
    let mut columns: Vec<(String, GpkgDataColumn)> = Vec::new();
    let mut constraints: Vec<GpkgConstraint> = Vec::new();
    for (name, fc) in layers {
        let Some(native) = gpkg_native(fc) else { continue };
        let present = columns_for(fc);
        for column in &native.data_columns {
            if present.iter().any(|c| c.name == column.column_name) {
                columns.push((name.clone(), column.clone()));
            }
        }
        for constraint in &native.constraints {
            if !constraints.contains(constraint) {
                constraints.push(constraint.clone());
            }
        }
    }
    // A constraint whose every referring column was dropped above describes
    // nothing, and would be a row GDAL reports as an unused domain.
    constraints.retain(|constraint| {
        columns.iter().any(|(_, column)| {
            column.constraint_name.as_deref() == Some(constraint.constraint_name.as_str())
        })
    });
    if columns.is_empty() && constraints.is_empty() {
        return Vec::new();
    }

    let data_column_rows: Vec<(i64, Vec<Value>)> = columns
            .into_iter()
            .enumerate()
            .map(|(i, (table, column))| {
                (
                    (i + 1) as i64,
                    vec![
                        Value::Text(table),
                        Value::Text(column.column_name),
                        text_or_null(column.name),
                        text_or_null(column.title),
                        text_or_null(column.description),
                        text_or_null(column.mime_type),
                        text_or_null(column.constraint_name),
                    ],
                )
            })
            .collect();
    let mut specs = vec![TableSpec {
        name: "gpkg_data_columns".into(),
        sql: DATA_COLUMNS_DDL.into(),
        // `pk_gdc` over (table_name, column_name), then `gdc_tn` over
        // (table_name, name) — `name` is nullable, and SQLite sorts NULLs
        // first, which `compare_values` handles.
        autoindexes: vec![
            autoindex_records(&data_column_rows, &[0, 1]),
            autoindex_records(&data_column_rows, &[0, 2]),
        ],
        rows: data_column_rows,
    }];
    if !constraints.is_empty() {
        specs.push(TableSpec {
            name: "gpkg_data_column_constraints".into(),
            sql: DATA_COLUMN_CONSTRAINTS_DDL.into(),
            rows: constraints
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    (
                        (i + 1) as i64,
                        vec![
                            Value::Text(c.constraint_name),
                            Value::Text(c.constraint_type),
                            text_or_null(c.value),
                            number_or_null(c.min),
                            bool_or_null(c.min_is_inclusive),
                            number_or_null(c.max),
                            bool_or_null(c.max_is_inclusive),
                            text_or_null(c.description),
                        ],
                    )
                })
                .collect(),
            autoindexes: Vec::new(),
        });
        // `gdcc_ntv UNIQUE (constraint_name, constraint_type, value)`. Built
        // from the rows just pushed, since `constraints` was consumed above.
        let rows = &specs[specs.len() - 1].rows;
        let records = autoindex_records(rows, &[0, 1, 2]);
        specs.last_mut().expect("just pushed").autoindexes = vec![records];
    }
    specs
}

/// An absent optional member is a NULL cell, not an empty string: the reader
/// distinguishes them and so must the writer.
fn text_or_null(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
}

fn number_or_null(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Real)
}

/// GeoPackage `BOOLEAN` is `INTEGER` 0/1 — the same storage M4's declared-type
/// work reads back as a boolean.
fn bool_or_null(value: Option<bool>) -> Value {
    value.map_or(Value::Null, |b| Value::Int(b.into()))
}

fn extensions_table(rows: Vec<ExtensionRow>) -> TableSpec {
    let rows: Vec<(i64, Vec<Value>)> = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    (i + 1) as i64,
                    vec![
                        Value::Text(r.table_name),
                        Value::Text(r.column_name),
                        Value::Text(r.extension_name),
                        Value::Text(r.definition),
                        Value::Text(r.scope),
                    ],
                )
            })
            .collect();
    TableSpec {
        name: "gpkg_extensions".into(),
        sql: EXTENSIONS_DDL.into(),
        // `ge_tce UNIQUE (table_name, column_name, extension_name)`.
        autoindexes: vec![autoindex_records(&rows, &[0, 1, 2])],
        rows,
    }
}

/// A `gpkg_spatial_ref_sys` row for a CRS beyond the three mandatory defaults.
struct SrsReg {
    srs_id: i64,
    name: String,
    organization: String,
    organization_coordsys_id: i64,
    definition: String,
}

/// The srs_id chosen for each layer (aligned with `layers`), plus the extra
/// `gpkg_spatial_ref_sys` rows those choices require.
struct ResolvedSrs {
    per_layer: Vec<i64>,
    registrations: Vec<SrsReg>,
}

/// Map every layer's [`Crs`] to a GeoPackage srs_id, registering a
/// `gpkg_spatial_ref_sys` row for any non-default system. Geosetta never
/// reprojects, so this only *labels* each layer with the CRS it arrived in:
/// `None` → undefined, WGS 84 → 4326, and any other CRS → its authority code
/// (or a synthetic id when it has none), carrying the WKT definition through.
///
/// GeoPackage's `srs_id`/`organization_coordsys_id` are SQLite `INTEGER`
/// columns — genuinely numeric on disk, unlike the IR's string `code` (which
/// also has to hold IGNF/OGC/PROJ/NKG's alphanumeric codes). A code that
/// parses as a positive integer is used directly, same as always; a code that
/// doesn't (non-numeric, e.g. `"LAMB93"`) falls into the same synthetic-id path
/// as "no code at all" — the WKT definition, if any, still carries through, so
/// nothing is silently mislabeled, it just can't be the native id.
fn resolve_srs(layers: &[(String, FeatureCollection)]) -> ResolvedSrs {
    let mut per_layer = Vec::with_capacity(layers.len());
    let mut registrations: Vec<SrsReg> = Vec::new();
    let mut next_synthetic = SYNTHETIC_SRS_BASE;

    for (_, fc) in layers {
        let (srs_id, reg) = match &fc.crs {
            None => (UNDEFINED_SRS_ID, None),
            Some(Crs::Wgs84) => (WGS84_SRS_ID, None),
            Some(Crs::Named(n)) => {
                let definition =
                    n.wkt.clone().or_else(|| n.structural_wkt()).unwrap_or_else(|| "undefined".into());
                let numeric_code = n.code.as_deref().and_then(|c| c.parse::<i64>().ok());
                match numeric_code {
                    // EPSG:4326 is exactly the default row.
                    Some(4326) => (WGS84_SRS_ID, None),
                    // A usable numeric authority code becomes the srs_id directly.
                    Some(code) if code > 0 => {
                        let organization =
                            n.authority.clone().unwrap_or_else(|| "EPSG".into());
                        (
                            code,
                            Some(SrsReg {
                                srs_id: code,
                                name: format!("{organization}:{code}"),
                                organization,
                                organization_coordsys_id: code,
                                definition,
                            }),
                        )
                    }
                    // No code, or a code GeoPackage's INTEGER srs_id can't
                    // represent: hand out a synthetic id and record whatever
                    // definition we have (WKT, or "undefined").
                    _ => {
                        let srs_id = next_synthetic;
                        next_synthetic += 1;
                        (
                            srs_id,
                            Some(SrsReg {
                                srs_id,
                                name: format!("srs {srs_id}"),
                                organization: n
                                    .authority
                                    .clone()
                                    .unwrap_or_else(|| crate::crs::SYNTHETIC_AUTHORITY.into()),
                                organization_coordsys_id: numeric_code.unwrap_or(srs_id),
                                definition,
                            }),
                        )
                    }
                }
            }
        };
        if let Some(reg) = reg
            && !registrations.iter().any(|r| r.srs_id == reg.srs_id)
        {
            registrations.push(reg);
        }
        per_layer.push(srs_id);
    }
    ResolvedSrs { per_layer, registrations }
}

fn spatial_ref_sys(registrations: &[SrsReg]) -> TableSpec {
    // srs_id is INTEGER PRIMARY KEY, so its column value is Null and the rowid
    // carries it; rows must be rowid-ordered.
    let row = |srs_id: i64, name: &str, org: &str, org_id: i64, def: &str, desc: &str| {
        let vals = vec![
            Value::Text(name.into()),
            Value::Null,
            Value::Text(org.into()),
            Value::Int(org_id),
            Value::Text(def.into()),
            Value::Text(desc.into()),
        ];
        (srs_id, vals)
    };
    let mut rows = vec![
        row(-1, "Undefined cartesian SRS", "NONE", -1, "undefined", "undefined cartesian"),
        row(0, "Undefined geographic SRS", "NONE", 0, "undefined", "undefined geographic"),
        row(WGS84_SRS_ID, "WGS 84 geodetic", "EPSG", 4326, WGS84_WKT, "WGS 84"),
    ];
    for reg in registrations {
        rows.push(row(
            reg.srs_id,
            &reg.name,
            &reg.organization,
            reg.organization_coordsys_id,
            &reg.definition,
            "",
        ));
    }
    // Rows must be rowid- (srs_id-) ordered for the b-tree writer.
    rows.sort_by_key(|(srs_id, _)| *srs_id);
    let sql = SPATIAL_REF_SYS_DDL;
    // `srs_id INTEGER NOT NULL PRIMARY KEY` is a rowid alias, so this table
    // owns no index b-tree — which is why it was the only registry table this
    // writer already emitted exactly as specified (defect 6).
    TableSpec { name: "gpkg_spatial_ref_sys".into(), sql: sql.into(), rows, autoindexes: Vec::new() }
}

fn gpkg_contents(layers: &[(String, FeatureCollection)], per_layer_srs: &[i64]) -> TableSpec {
    let rows: Vec<(i64, Vec<Value>)> = layers
        .iter()
        .zip(per_layer_srs)
        .enumerate()
        .map(|(i, ((name, fc), &srs_id))| {
            let (minx, miny, maxx, maxy) = bbox(fc);
            let native = gpkg_native(fc);
            // The layer's own title and description when it recorded any; the
            // layer name and an empty string otherwise, which is what GDAL
            // writes for a layer created without them. An identifier equal to
            // the name is not carried at all (see `GpkgNative`), so this
            // regenerates it and a rename stays consistent.
            let identifier = native
                .and_then(|n| n.identifier.clone())
                .unwrap_or_else(|| name.clone());
            let description =
                native.and_then(|n| n.description.clone()).unwrap_or_default();
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("features".into()),
                    Value::Text(identifier),
                    Value::Text(description),
                    Value::Text(LAST_CHANGE.into()),
                    minx,
                    miny,
                    maxx,
                    maxy,
                    Value::Int(srs_id),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_contents".into(),
        sql: CONTENTS_DDL.into(),
        // `table_name` is the primary key and `identifier` is UNIQUE, so this
        // table owns two implicit indexes — `_1` for the key, `_2` for the
        // UNIQUE, which is the order SQLite assigns them.
        autoindexes: vec![
            autoindex_records(&rows, &[0]),
            autoindex_records(&rows, &[2]),
        ],
        rows,
    }
}

fn geometry_columns(layers: &[(String, FeatureCollection)], per_layer_srs: &[i64]) -> TableSpec {
    let rows: Vec<(i64, Vec<Value>)> = layers
        .iter()
        .zip(per_layer_srs)
        .enumerate()
        .map(|(i, ((name, _), &srs_id))| {
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("geom".into()),
                    Value::Text("GEOMETRY".into()),
                    Value::Int(srs_id),
                    Value::Int(0),
                    Value::Int(0),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_geometry_columns".into(),
        sql: GEOMETRY_COLUMNS_DDL.into(),
        // `pk_geom_cols` over (table_name, column_name), then
        // `uk_gc_table_name` over table_name alone.
        autoindexes: vec![
            autoindex_records(&rows, &[0, 1]),
            autoindex_records(&rows, &[0]),
        ],
        rows,
    }
}

fn feature_table(name: &str, fc: &FeatureCollection, srs_id: i64, envelope: bool) -> TableSpec {
    // `columns_for`, not `infer_columns`: a column the source declared a type
    // for is declared as *that* type here, which is what lets a `BOOLEAN`, a
    // `DATETIME` or a `SMALLINT` survive rather than collapsing to the four
    // types the hub can infer.
    let columns = columns_for(fc);

    // DDL: fid (rowid), geom, then one column per property.
    let mut sql = format!("CREATE TABLE {} (\"fid\" INTEGER PRIMARY KEY NOT NULL, \"geom\" GEOMETRY", quote(name));
    for col in &columns {
        sql.push_str(&format!(", {} {}", quote(&col.name), sqlite_type(col)));
    }
    sql.push(')');

    let rows = fc
        .features
        .iter()
        .enumerate()
        .map(|(i, feat)| {
            let mut values = Vec::with_capacity(columns.len() + 2);
            values.push(Value::Null); // fid, carried by the rowid
            values.push(match &feat.geometry {
                Some(g) => Value::Blob(gpkg_geometry(g, srs_id, envelope)),
                None => Value::Null,
            });
            for col in &columns {
                values.push(cell_to_value(&col.values[i]));
            }
            ((i + 1) as i64, values)
        })
        .collect();

    TableSpec {
        name: name.to_string(),
        sql,
        rows,
        // The feature table is keyed by `"fid" INTEGER PRIMARY KEY`, a rowid
        // alias, so it owns no index b-tree of its own. Its *spatial* index is
        // a separate opt-in structure — see `super::rtree`.
        autoindexes: Vec::new(),
    }
}

/// GeoPackage Binary: an 8-byte header ("GP", version, flags, LE srs_id),
/// optionally followed by a cached envelope, wrapping standard WKB.
///
/// With `envelope` false the header is exactly what it always was — flags
/// `0x01`, envelope indicator 0, no envelope bytes — so default output is
/// unchanged byte for byte.
fn gpkg_geometry(g: &Geometry, srs_id: i64, envelope: bool) -> Vec<u8> {
    let env = if envelope { gpb_envelope(g) } else { None };
    let (indicator, env_bytes) = match &env {
        Some((i, b)) => (*i, b.as_slice()),
        None => (0u8, &[][..]),
    };
    let wkb = to_wkb(g);
    let mut out = Vec::with_capacity(8 + env_bytes.len() + wkb.len());
    out.extend_from_slice(b"GP");
    out.push(0); // version
    // flags: bit 0 = little-endian header ints, bits 1-3 = envelope indicator.
    out.push(0x01 | (indicator << 1));
    out.extend_from_slice(&(srs_id as i32).to_le_bytes());
    out.extend_from_slice(env_bytes);
    out.extend_from_slice(&wkb);
    out
}

/// GPB §2.1.3's cached envelope: an indicator code plus a little-endian `f64`
/// sequence, or `None` for a geometry with no coordinates (indicator 0).
///
/// The layout is min/max *pairs per dimension* — `minx, maxx, miny, maxy`, then
/// `minz, maxz` for indicator 2 — **not** all mins followed by all maxes.
/// Confirmed by hand-decoding real `ogr2ogr -f GPKG` output rather than read
/// off the spec: `tests/fixtures/gdal_linestring_2d.gpkg` (indicator 1, 32
/// bytes) and `gdal_linestring_z.gpkg` (indicator 2, 48 bytes).
///
/// **The envelope stops at Z: only indicators 1 and 2 are produced, never 3
/// (XYM) or 4 (XYZM), even for an M-bearing geometry.** Two reasons, the first
/// of which is the real one.
///
/// *An envelope is a spatial bound, and M is not a spatial dimension.* The
/// envelope exists to accelerate spatial queries, and it is genuinely used for
/// that: with a deliberately falsified envelope, `ogrinfo -spat 0 0 25 15`
/// returns **zero** features for a geometry that plainly intersects the filter
/// box — GDAL trusts the cache over the coordinates. But a spatial filter is
/// over X/Y (and conceivably Z); M is a linear-referencing measure (mile
/// markers, timestamps), with no query to accelerate. GeoPackage's own RTree is
/// strictly 2D for the same reason. Indicators 3 and 4 exist in the spec's
/// table for completeness, not because anything consumes them.
///
/// *And nothing produces them to check against.* GDAL 3.13.2 emits indicator 1
/// for `LINESTRING M` and indicator 2 for `LINESTRING ZM` — the latter caching
/// the *Z* range, not M. Verified across eight geometry families (LineString,
/// Polygon, MultiLineString, MultiPoint, Point, GeometryCollection,
/// CircularString, in M and ZM forms); no creation option or `OGR_GPKG_*`
/// config switch changes it. So an XYM/XYZM byte order could only be *assumed*
/// symmetric with XYZ's, which `plans/envelope.org` explicitly forbids — and
/// given the filter behaviour above, a wrongly-encoded envelope silently drops
/// real query hits. A missing envelope only costs speed.
///
/// GDAL's *reader* does accept indicators 3 and 4 (it skips the bytes), so
/// emitting them would not break readers — it simply would not buy anything,
/// at the cost of a layout no reference implementation could confirm.
fn gpb_envelope(g: &Geometry) -> Option<(u8, Vec<u8>)> {
    let b = g.bbox();
    if b.is_empty() {
        return None;
    }
    let mut buf = Vec::with_capacity(48);
    for v in [b.min_x, b.max_x, b.min_y, b.max_y] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    let indicator = match b.z {
        Some((lo, hi)) => {
            buf.extend_from_slice(&lo.to_le_bytes());
            buf.extend_from_slice(&hi.to_le_bytes());
            2
        }
        None => 1,
    };
    Some((indicator, buf))
}

fn bbox(fc: &FeatureCollection) -> (Value, Value, Value, Value) {
    let mut b = Bbox::empty();
    for f in &fc.features {
        if let Some(g) = &f.geometry {
            g.extend_bbox(&mut b);
        }
    }
    if b.is_empty() {
        (Value::Null, Value::Null, Value::Null, Value::Null)
    } else {
        (
            Value::Real(b.min_x),
            Value::Real(b.min_y),
            Value::Real(b.max_x),
            Value::Real(b.max_y),
        )
    }
}

/// The GeoPackage spelling for a declared type, split out from
/// [`sqlite_type`] so the reader's tests can assert the two directions agree
/// without constructing a [`Column`] — see
/// `every_declared_type_is_written_with_a_spelling_that_reads_back_wide_enough`.
pub(super) fn sql_type_for(d: DeclType) -> &'static str {
    match d {
        DeclType::Bool => "BOOLEAN",
        // GeoPackage has no unsigned type, so each unsigned width is
        // written as the *next signed width up* — the one that admits its
        // whole range. Pairing them with the same-width signed spelling
        // (`UInt32` as `MEDIUMINT`) was a silent narrowing: the value
        // survived SQLite's dynamic typing, but the declaration was a lie
        // and GDAL read 3000000000 back as -1294967296. Found 2026-08-21 by
        // the recast matrix; `Format::recast` states the result and
        // `every_declared_type_survives_or_is_admitted` pins it.
        DeclType::Int8 => "TINYINT",
        DeclType::UInt8 | DeclType::Int16 => "SMALLINT",
        DeclType::UInt16 | DeclType::Int32 => "MEDIUMINT",
        DeclType::UInt32 | DeclType::Int64 | DeclType::UInt64 => "INTEGER",
        DeclType::Float32 => "FLOAT",
        DeclType::Float64 => "DOUBLE",
        DeclType::String => "TEXT",
        DeclType::Json => "JSON",
        DeclType::DateTime => "DATETIME",
        DeclType::Date => "DATE",
        DeclType::Binary => "BLOB",
    }
}

/// The SQL type to declare a column as: what the source declared, when that
/// still describes the data, else what the values inferred.
///
/// Every name here is from GeoPackage spec table 1 (*GeoPackage Data Types*),
/// so a declaration carried in from another spoke's richer vocabulary lands on
/// a spelling this format actually sanctions.
fn sqlite_type(col: &Column) -> &'static str {
    if let Some(d) = col.declared_type() {
        return sql_type_for(d);
    }
    match col.ty {
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Int64 => "INTEGER",
        ColumnType::Double => "REAL",
        ColumnType::String => "TEXT",
        ColumnType::Binary => "BLOB",
    }
}

fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Int(*b as i64),
        Cell::Int(n) => Value::Int(*n),
        Cell::Double(d) => Value::Real(*d),
        Cell::Str(s) => Value::Text(s.clone()),
        Cell::Bytes(b) => Value::Blob(b.clone()),
    }
}

/// Quote a SQL identifier, doubling any embedded quotes.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Position;
    use crate::feature::Feature;
    use crate::json::JsonValue;

    fn point(x: f64, y: f64) -> Option<Geometry> {
        Some(Geometry::Point(Position::new(x, y)))
    }
    fn fc(features: Vec<Feature>) -> FeatureCollection {
        FeatureCollection::new(features)
    }

    /// Every registry table is written as the specification defines it, with
    /// the index each of its constraints requires.
    ///
    /// This is *defect 6*. Until 2026-08-23 all of these were emitted with no
    /// table constraints at all, so a `gpkg -> gpkg` run read a conformant
    /// container and returned one whose registry DDL had lost every key it
    /// arrived with. **Nothing observable broke** — GDAL read the output and
    /// `PRAGMA integrity_check` passed — which is why no existing test caught
    /// it: they compare values and declared column types, and a table
    /// constraint is neither.
    ///
    /// Two halves, and both are needed. The DDL half asserts the constraint is
    /// *declared*; the index half asserts the b-tree it obliges is *there*,
    /// because declaring one without the other is precisely the malformed
    /// database of `handoff.org` § BUGS FOUND entry 15.
    ///
    /// **The DDL half below is a weaker check than it looks**, and
    /// `the_registry_ddl_is_what_the_standard_says` is the one that closes the
    /// gap: a substring assertion can only confirm what somebody thought to
    /// assert. It cannot see a constraint nobody knew was missing — which is
    /// exactly how `gdc_tn` survived this test's first pass — and it cannot
    /// see an *extra* column or constraint at all, though the standard forbids
    /// those in the same sentence.
    #[test]
    fn the_registry_tables_are_written_as_the_specification_defines_them() {
        // Declared: the constraints the specification states, by name where it
        // names them.
        assert!(CONTENTS_DDL.contains("table_name TEXT NOT NULL PRIMARY KEY"));
        assert!(CONTENTS_DDL.contains("identifier TEXT UNIQUE"));
        assert!(CONTENTS_DDL.contains("CONSTRAINT fk_gc_r_srs_id FOREIGN KEY"));
        assert!(CONTENTS_DDL.contains("description TEXT DEFAULT ''"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT pk_geom_cols PRIMARY KEY"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT uk_gc_table_name UNIQUE"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT fk_gc_tn FOREIGN KEY"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT fk_gc_srs FOREIGN KEY"));
        assert!(DATA_COLUMNS_DDL.contains("CONSTRAINT pk_gdc PRIMARY KEY"));
        // `gdc_tn` is the constraint `ogr2ogr` omits and the specification
        // requires — missed on this test's first pass for exactly that reason,
        // since GDAL's own file was the reference being read.
        assert!(DATA_COLUMNS_DDL.contains("CONSTRAINT gdc_tn UNIQUE (table_name, name)"));
        assert!(DATA_COLUMN_CONSTRAINTS_DDL.contains("CONSTRAINT gdcc_ntv UNIQUE"));
        assert!(EXTENSIONS_DDL.contains("CONSTRAINT ge_tce UNIQUE"));
        // The specification states `srs_id INTEGER PRIMARY KEY`; `ogr2ogr`
        // writes `INTEGER NOT NULL PRIMARY KEY`. Follow the specification.
        assert!(SPATIAL_REF_SYS_DDL.contains("srs_id INTEGER PRIMARY KEY"));
        assert!(!SPATIAL_REF_SYS_DDL.contains("NOT NULL PRIMARY KEY"));

        // Present: with `--rtree`, so `gpkg_extensions` is written too.
        let layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: vec![("name".into(), JsonValue::String("a".into()))],
            native: None,
        }]);
        let bytes = write_layers(None, &[("places".into(), layer)], true, false).unwrap();
        let objects = crate::sqlite::Database::open(&bytes).unwrap().master_objects().unwrap();

        // Named and ordered as SQLite names them: `_1` for the primary key,
        // `_2` for the UNIQUE beside it.
        for (table, count) in [("gpkg_contents", 2), ("gpkg_geometry_columns", 2), ("gpkg_extensions", 1)] {
            for n in 1..=count {
                let want = format!("sqlite_autoindex_{table}_{n}");
                let found = objects
                    .iter()
                    .find(|o| o.name == want)
                    .unwrap_or_else(|| panic!("missing {want}"));
                assert_eq!(found.kind, "index", "{want}");
                assert_eq!(found.tbl_name, table, "{want}");
                assert_ne!(found.root_page, 0, "{want} must own a real b-tree");
            }
        }

        // And `gpkg_spatial_ref_sys` owns none: `INTEGER PRIMARY KEY` is a
        // rowid alias, which is the whole reason that one table was already
        // correct while the other five were not.
        assert!(
            !objects.iter().any(|o| o.name.starts_with("sqlite_autoindex_gpkg_spatial_ref_sys")),
            "a rowid alias needs no index of its own"
        );
    }

    /// Normalise DDL for comparison: layout differs between the standard's
    /// wrapped text and this crate's one-line constants, and layout is the one
    /// thing that is genuinely not being tested.
    ///
    /// Whitespace *inside* a string literal is data and is left alone —
    /// `DEFAULT ''` and `strftime('%Y-%m-%dT%H:%M:%fZ','now')` both appear in
    /// the registry DDL, and collapsing runs blindly would rewrite them.
    fn normalize_ddl(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        let mut chars = sql.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                out.push(c);
                // Copy the literal through verbatim, honouring the `''` escape.
                while let Some(q) = chars.next() {
                    out.push(q);
                    if q == '\'' {
                        if chars.peek() == Some(&'\'') {
                            out.push(chars.next().unwrap());
                            continue;
                        }
                        break;
                    }
                }
            } else if c.is_whitespace() {
                while chars.peek().is_some_and(|n| n.is_whitespace()) {
                    chars.next();
                }
                // The wrapped form breaks the line after `(`; the one-line
                // form does not. Dropping the space here rather than by a
                // `replace` over the finished string is what keeps a literal
                // containing `( ` safe.
                if !out.ends_with('(') {
                    out.push(' ');
                }
            } else {
                // Its closing partner: `\n)` against `)`.
                if c == ')' && out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
            }
        }
        // Everything else stays significant, including the standard's own
        // inconsistency between `gpkg_spatial_ref_sys(srs_id)` and
        // `gpkg_spatial_ref_sys (srs_id)` — that is what it says.
        out.trim().trim_end_matches(';').trim().to_string()
    }

    /// The normaliser collapses layout and *only* layout.
    ///
    /// Worth its own test because the property is easy to lose: the obvious
    /// implementation is a whitespace `replace` over the finished string, and
    /// it silently rewrites `DEFAULT '  x  '` while every DDL comparison
    /// keeps passing. Both literals below are real —
    /// `strftime('%Y-%m-%dT%H:%M:%fZ','now')` is `gpkg_contents.last_change`.
    #[test]
    fn normalizing_ddl_does_not_reach_inside_a_string_literal() {
        let wrapped = "CREATE TABLE t (\n  a DATETIME DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),\n  b TEXT DEFAULT '  two  spaces  ',\n  c TEXT DEFAULT 'it''s ( quoted )'\n);";
        let inline = "CREATE TABLE t (a DATETIME DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), b TEXT DEFAULT '  two  spaces  ', c TEXT DEFAULT 'it''s ( quoted )')";
        assert_eq!(normalize_ddl(wrapped), normalize_ddl(inline));
        // Not merely equal to each other — equal to the text, spaces intact.
        assert_eq!(normalize_ddl(wrapped), inline);
    }

    /// Every registry table this crate writes declares *exactly* what the
    /// GeoPackage standard says it declares — no constraint missing, and none
    /// added.
    ///
    /// The oracle is `tests/fixtures/spec_conformant.gpkg`, built by `sqlite3`
    /// from the normative SQL of OGC 12-128r19 with **no GDAL anywhere in its
    /// provenance**. That is the whole point of it, and the reason this test
    /// is not simply more assertions about the constants: defect 6's first fix
    /// was cross-checked against `ogr2ogr`'s own output and therefore
    /// inherited GDAL's four measured deviations from the standard, three of
    /// them ours until the spec text was read. A differential test against a
    /// reference implementation measures agreement, not conformance — see
    /// `plans/same-format-fidelity.org` § THE ORACLE RULE.
    ///
    /// Comparing whole normalised DDL rather than substrings is what makes
    /// this stronger than its sibling above. The standard's clause is a
    /// **SHALL** in both directions: *"All `gpkg_*` tables and views ... SHALL
    /// have only the specified columns and table constraints."* An extra
    /// column passes every substring assertion ever written and fails here.
    #[test]
    fn the_registry_ddl_is_what_the_standard_says() {
        const SPEC: &[u8] = include_bytes!("../../tests/fixtures/spec_conformant.gpkg");
        let spec_db = crate::sqlite::Database::open(SPEC).unwrap();
        let spec_ddl: Vec<_> = spec_db
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "table" && o.name.starts_with("gpkg_"))
            .filter_map(|o| o.sql.map(|s| (o.name, normalize_ddl(&s))))
            .collect();

        // `--rtree` so `gpkg_extensions` is written, and a layer carrying a
        // schema native so the two Schema extension tables are too. Between
        // them that reaches every registry table this writer can emit.
        let mut layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: vec![("name".into(), JsonValue::String("a".into()))],
            native: None,
        }]);
        layer.native = Some(crate::Native::new(
            crate::Format::Gpkg,
            crate::NativePayload::Gpkg(crate::GpkgNative {
                identifier: None,
                description: None,
                data_columns: vec![crate::GpkgDataColumn {
                    column_name: "name".into(),
                    name: Some("Name".into()),
                    title: None,
                    description: None,
                    mime_type: None,
                    constraint_name: Some("nonempty".into()),
                }],
                // Non-empty on purpose: `gpkg_data_column_constraints` is
                // written only when a constraint exists, so an empty vector
                // here would silently drop that table out of the comparison.
                constraints: vec![crate::GpkgConstraint {
                    constraint_name: "nonempty".into(),
                    constraint_type: "glob".into(),
                    value: Some("*".into()),
                    min: None,
                    min_is_inclusive: None,
                    max: None,
                    max_is_inclusive: None,
                    description: None,
                }],
            }),
        ));
        let bytes = write_layers(None, &[("places".into(), layer)], true, false).unwrap();
        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let ours_ddl: std::collections::HashMap<String, String> = ours
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "table" && o.name.starts_with("gpkg_"))
            .filter_map(|o| o.sql.map(|s| (o.name, normalize_ddl(&s))))
            .collect();

        // Intersection only. The fixture also carries `gpkg_metadata` and
        // `gpkg_metadata_reference`, which this crate does not write at all —
        // a milestone rather than a defect, and M3b's to close when it starts
        // carrying them. Comparing on absence would assert the wrong thing.
        let mut compared = 0;
        for (table, want) in &spec_ddl {
            let Some(got) = ours_ddl.get(table) else {
                continue;
            };
            assert_eq!(got, want, "{table} does not match the standard's DDL");
            compared += 1;
        }

        // Without this the test passes vacuously the day a rename means
        // nothing intersects. Six tables: the four core ones plus the Schema
        // extension's two.
        assert_eq!(compared, 6, "compared {compared} tables, expected 6");

        // And the writer's constants are the same text, which is what makes
        // the six assertions above statements about this crate's output rather
        // than about one file it happened to produce.
        for ddl in [
            SPATIAL_REF_SYS_DDL,
            CONTENTS_DDL,
            GEOMETRY_COLUMNS_DDL,
            EXTENSIONS_DDL,
            DATA_COLUMNS_DDL,
            DATA_COLUMN_CONSTRAINTS_DDL,
        ] {
            let name = ddl.trim_start_matches("CREATE TABLE ").split_whitespace().next().unwrap();
            let want = spec_ddl.iter().find(|(n, _)| n == name).map(|(_, s)| s);
            assert_eq!(Some(&normalize_ddl(ddl)), want, "{name} constant");
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let layer = fc(vec![
            Feature {
                geometry: point(1.0, 2.0),
                properties: vec![
                    ("name".into(), JsonValue::String("a".into())),
                    ("n".into(), JsonValue::Int(5)),
                ],
                native: None,
            },
            Feature {
                geometry: point(3.0, 4.0),
                properties: vec![
                    ("name".into(), JsonValue::String("b".into())),
                    ("n".into(), JsonValue::Null),
                ],
                native: None,
            },
        ]);
        let bytes = write_layers(None, &[("places".into(), layer)], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();

        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "places");
        assert_eq!(back[0].1.features.len(), 2);
        assert_eq!(back[0].1.features[0].geometry, point(1.0, 2.0));
        // fid is excluded from properties.
        let names: Vec<&str> = back[0].1.features[0].properties.iter().map(|(k, _)| &**k).collect();
        assert_eq!(names, vec!["name", "n"]);
        assert_eq!(back[0].1.features[0].properties[1].1.as_f64(), Some(5.0));
    }

    /// The GeoPackage half of `edit-composability.org` D4's layer-duplication
    /// bug: a layer read back must carry the *table name* as its identity, or
    /// an edit loop cannot tell the writer which layer it is replacing.
    #[test]
    fn a_layer_read_back_knows_its_own_name() {
        let bytes =
            write_layers(None, &[("roads".into(), fc(vec![]))], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.name.as_deref(), Some("roads"));
    }

    /// The failure that bug was actually about, end to end: a middle stage
    /// *deletes* a feature, and the deletion has to stick. Before the reader
    /// set `fc.name`, the edited collection landed in a second table called
    /// `layer` and the pre-edit features stayed standing in `roads` — so a
    /// delete silently became a no-op, which is the worst shape this could
    /// take. Asserted on the surviving rows, not just the table name, because
    /// the table name is the mechanism and this is the consequence.
    #[test]
    fn an_edit_loop_that_deletes_a_feature_does_not_duplicate_the_layer() {
        let feature = |n: f64| Feature {
            geometry: point(n, n),
            properties: vec![("id".into(), JsonValue::Int((n) as i64))],
            native: None,
        };
        let original = fc(vec![feature(1.0), feature(2.0), feature(3.0)]);
        let bytes = write_layers(None, &[("roads".into(), original)], false, false).unwrap();

        // Read it back the way a pipeline would, drop the middle feature, and
        // write it into the same GeoPackage using only the name the IR carries.
        let mut edited = read_layers(&bytes).unwrap().remove(0).1;
        edited.features.retain(|f| f.properties[0].1.as_f64() != Some(2.0));
        let name = edited.name.clone().expect("the reader names the layer");
        let after = write_layers(Some(&bytes), &[(name, edited)], false, false).unwrap();

        let layers = read_layers(&after).unwrap();
        assert_eq!(layers.len(), 1, "the edit loop appended a duplicate layer");
        assert_eq!(layers[0].0, "roads");
        let ids: Vec<Option<f64>> =
            layers[0].1.features.iter().map(|f| f.properties[0].1.as_f64()).collect();
        assert_eq!(ids, vec![Some(1.0), Some(3.0)], "the deletion did not stick");
    }

    #[test]
    fn write_then_read_round_trips_z_and_m() {
        // GeoPackage needed zero new format-specific code for this (see
        // plans/zm-geometry.org's M5) — `gpkg_geometry` just wraps
        // `to_wkb(g)`, and `strip_gpkg_header` already skips whichever
        // envelope size the flags byte declares, so M2's WKB codec alone
        // should carry Z/M through untouched. Confirmed here, not assumed.
        let layer = fc(vec![Feature {
            geometry: Some(Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0))),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("places".into(), layer)], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(
            back[0].1.features[0].geometry,
            Some(Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)))
        );
    }

    // --- GPB envelope (plans/envelope.org V2/V3) -----------------------------
    //
    // Layout facts here are confirmed against real `ogr2ogr -f GPKG` output
    // (GDAL 3.13.2 / PROJ 9.8.1), not read off the spec — see `gpb_envelope`.

    /// Pull the GPB header pieces out of the first geometry blob in the file:
    /// the flags byte, the envelope indicator, and the envelope's decoded
    /// doubles. Scans every table rather than taking a name, so it works
    /// unchanged on our own output and on GDAL fixtures whose layer names
    /// differ.
    fn gpb_parts(bytes: &[u8]) -> (u8, u8, Vec<f64>) {
        use crate::sqlite::Database;
        let db = Database::open(bytes).unwrap();
        let blob = db
            .tables()
            .unwrap()
            .iter()
            .filter_map(|t| db.read_rows(t).ok())
            .flatten()
            .find_map(|row| {
                row.iter().find_map(|c| match c {
                    crate::sqlite::Value::Blob(b) if b.starts_with(b"GP") => Some(b.clone()),
                    _ => None,
                })
            })
            .expect("a GPB geometry blob");
        let flags = blob[3];
        let indicator = (flags >> 1) & 0x07;
        let len = match indicator {
            0 => 0,
            1 => 32,
            2 | 3 => 48,
            4 => 64,
            _ => panic!("invalid envelope indicator {indicator}"),
        };
        let doubles = blob[8..8 + len]
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        (flags, indicator, doubles)
    }

    fn linestring(ps: Vec<Position>) -> Option<Geometry> {
        Some(Geometry::LineString(ps))
    }

    #[test]
    fn without_the_flag_output_is_byte_identical_to_before() {
        // The envelope is opt-in precisely so the default path does not move.
        // Same layer, written twice: `--envelope` off must reproduce the
        // historical header exactly (flags 0x01, indicator 0, no bytes).
        let layer = fc(vec![Feature {
            geometry: linestring(vec![Position::new(0.0, 0.0), Position::new(20.0, 10.0)]),
            properties: vec![],
            native: None,
        }]);
        let plain = write_layers(None, &[("l".into(), layer)], false, false).unwrap();
        let (flags, indicator, env) = gpb_parts(&plain);
        assert_eq!(flags, 0x01);
        assert_eq!(indicator, 0);
        assert!(env.is_empty());
    }

    #[test]
    fn envelope_flag_writes_indicator_1_for_2d() {
        let layer = fc(vec![Feature {
            geometry: linestring(vec![
                Position::new(0.0, 0.0),
                Position::new(10.0, 10.0),
                Position::new(20.0, 5.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), layer)], false, true).unwrap();
        let (flags, indicator, env) = gpb_parts(&bytes);
        assert_eq!(flags, 0x03, "bit 0 little-endian + indicator 1 in bits 1-3");
        assert_eq!(indicator, 1);
        // min/max PAIRS PER DIMENSION, not all-mins-then-all-maxes.
        assert_eq!(env, vec![0.0, 20.0, 0.0, 10.0]);
    }

    #[test]
    fn envelope_flag_writes_indicator_2_for_z() {
        let layer = fc(vec![Feature {
            geometry: linestring(vec![
                Position::with_z(0.0, 0.0, 0.0),
                Position::with_z(10.0, 10.0, 10.0),
                Position::with_z(20.0, 5.0, 3.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), layer)], false, true).unwrap();
        let (flags, indicator, env) = gpb_parts(&bytes);
        assert_eq!(flags, 0x05);
        assert_eq!(indicator, 2);
        assert_eq!(env, vec![0.0, 20.0, 0.0, 10.0, 0.0, 10.0]);
    }

    #[test]
    fn m_is_never_encoded_in_the_envelope_matching_gdal() {
        // The envelope is a *spatial* bound, and M is not a spatial dimension —
        // there is no "filter by M" for it to accelerate, which is why GDAL
        // stops at Z and why GeoPackage's own RTree is 2D. Verified across
        // eight geometry families with real `ogr2ogr`: `LINESTRING M` gets
        // indicator 1, `LINESTRING ZM` gets indicator 2 caching the *Z* range.
        // See `gpb_envelope` for the full rationale.
        let m_only = fc(vec![Feature {
            geometry: linestring(vec![
                Position::with_m(0.0, 0.0, 10.0),
                Position::with_m(20.0, 10.0, 30.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), m_only)], false, true).unwrap();
        let (_, indicator, env) = gpb_parts(&bytes);
        assert_eq!(
            indicator, 1,
            "M alone must not promote the envelope past XY"
        );
        assert_eq!(env, vec![0.0, 20.0, 0.0, 10.0]);

        let zm = fc(vec![Feature {
            geometry: linestring(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(20.0, 10.0, 3.0, 30.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), zm)], false, true).unwrap();
        let (_, indicator, env) = gpb_parts(&bytes);
        assert_eq!(indicator, 2, "ZM tracks Z only, exactly as GDAL does");
        assert_eq!(
            env,
            vec![0.0, 20.0, 0.0, 10.0, 1.0, 3.0],
            "the Z range, not the M range"
        );
    }

    #[test]
    fn an_empty_geometry_gets_no_envelope_even_with_the_flag() {
        // Nothing to bound: indicator 0, as if the flag were off. GDAL declines
        // an envelope for a degenerate case too (it writes none for a `Point`).
        let layer = fc(vec![Feature {
            geometry: linestring(vec![]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), layer)], false, true).unwrap();
        let (flags, indicator, env) = gpb_parts(&bytes);
        assert_eq!(flags, 0x01);
        assert_eq!(indicator, 0);
        assert!(env.is_empty());
    }

    #[test]
    fn our_own_envelope_bearing_output_reads_back() {
        // `strip_gpkg_header` skips whichever envelope size the flags byte
        // declares. It has always been exercised by GDAL's files; this proves
        // it against our own, for both indicator sizes.
        for geom in [
            linestring(vec![Position::new(0.0, 0.0), Position::new(20.0, 10.0)]),
            linestring(vec![
                Position::with_z(0.0, 0.0, 1.0),
                Position::with_z(20.0, 10.0, 9.0),
            ]),
        ] {
            let layer = fc(vec![Feature {
                geometry: geom.clone(),
                properties: vec![],
                native: None,
            }]);
            let bytes = write_layers(None, &[("l".into(), layer)], false, true).unwrap();
            let back = read_layers(&bytes).unwrap();
            assert_eq!(back[0].1.features[0].geometry, geom);
        }
    }

    #[test]
    fn envelope_bytes_match_gdals_on_the_same_geometry() {
        // The V3 oracle: read a real `ogr2ogr` GeoPackage, rewrite it through
        // our writer with envelopes on, and require the envelope we compute to
        // equal the one GDAL computed — numerically, ordinate for ordinate.
        // A *missing* envelope is only a lost optimization; a *wrong* one is a
        // correctness bug, which is what this catches.
        for (name, fixture) in [
            (
                "2d",
                &include_bytes!("../../tests/fixtures/gdal_linestring_2d.gpkg")[..],
            ),
            (
                "z",
                &include_bytes!("../../tests/fixtures/gdal_linestring_z.gpkg")[..],
            ),
        ] {
            let (_, gdal_indicator, gdal_env) = gpb_parts(fixture);
            assert_ne!(
                gdal_indicator, 0,
                "{name}: fixture must actually carry an envelope"
            );

            let layers = read_layers(fixture).unwrap();
            let ours = write_layers(None, &layers, false, true).unwrap();
            let (_, our_indicator, our_env) = gpb_parts(&ours);

            assert_eq!(our_indicator, gdal_indicator, "{name}: indicator");
            assert_eq!(our_env, gdal_env, "{name}: envelope ordinates");
        }
    }

    #[test]
    fn rtree_index_is_emitted_and_round_trips() {
        // A layer big enough to force interior rtree nodes (> one node of cells)
        // and a multi-page sqlite_master (many trigger rows).
        let feats: Vec<Feature> = (0..120)
            .map(|i| Feature { geometry: point(i as f64, (i % 7) as f64), properties: vec![], native: None })
            .collect();
        let bytes = write_layers(None, &[("grid".into(), fc(feats))], true, false).unwrap();

        // The virtual table, its shadow tables, and gpkg_extensions are present.
        use crate::sqlite::Database;
        let db = Database::open(&bytes).unwrap();
        let names: Vec<String> = db.tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "gpkg_extensions"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_node"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_rowid"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_parent"));
        // The rtree virtual table itself has rootpage 0, so our table reader
        // (which needs a real root) skips it — a layer read still sees only the
        // feature layer.
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "grid");
        assert_eq!(back[0].1.features.len(), 120);
    }

    #[test]
    fn wgs84_layer_round_trips_as_wgs84() {
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Wgs84);
        let bytes = write_layers(None, &[("p".into(), layer)], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.crs, Some(Crs::Wgs84));
    }

    #[test]
    fn no_crs_layer_round_trips_as_none() {
        // CSV/WKT-style input with no CRS lands on the "undefined" SRS and reads
        // back as no CRS, not a mislabeled 4326.
        let layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        let bytes = write_layers(None, &[("p".into(), layer)], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.crs, None);
    }

    #[test]
    fn named_crs_is_registered_and_round_trips() {
        use crate::crs::NamedCrs;
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: Some("PROJCS[\"WGS 84 / Pseudo-Mercator\"]".into()),
            projjson: None,
        }));
        let bytes = write_layers(None, &[("web".into(), layer)], false, false).unwrap();

        // A valid, integrity-checkable GeoPackage that DuckDB/GDAL can open.
        use crate::sqlite::Database;
        assert!(Database::open(&bytes).is_ok());

        let back = read_layers(&bytes).unwrap();
        match &back[0].1.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("3857"));
                assert!(n.wkt.is_some());
            }
            other => panic!("expected Named EPSG:3857, got {other:?}"),
        }
    }

    #[test]
    fn alphanumeric_code_falls_back_to_a_synthetic_srs_id() {
        use crate::crs::NamedCrs;
        // A non-numeric authority code (IGNF/OGC/PROJ/NKG-style) can't be
        // GeoPackage's native INTEGER srs_id; it must fall into the same
        // synthetic-id path as "no code at all", never mislabel or fail to
        // write, and the WKT still carries through.
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: Some("IGNF".into()),
            code: Some("LAMB93".into()),
            wkt: Some("PROJCS[\"RGF93 Lambert 93\"]".into()),
            projjson: None,
        }));
        let bytes = write_layers(None, &[("fr".into(), layer)], false, false).unwrap();

        use crate::sqlite::Database;
        assert!(Database::open(&bytes).is_ok());

        let back = read_layers(&bytes).unwrap();
        match &back[0].1.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("IGNF"));
                assert!(n.wkt.is_some());
                // The read-back path resolves a synthetic organization_coordsys_id
                // (not the original "LAMB93" — GeoPackage's INTEGER column can't
                // hold it), so the recovered code is numeric, not the original.
                assert!(n.code.as_deref().is_some_and(|c| c.parse::<i64>().is_ok()));
            }
            other => panic!("expected Named IGNF, got {other:?}"),
        }
    }

    #[test]
    fn id_less_projjson_writes_definition_via_structural_translation() {
        use crate::crs::NamedCrs;
        // The gap `plans/projjson-to-wkt.org` closes: a GeoParquet source
        // whose PROJJSON carries no authority code at all — the registry
        // can't help (nothing to key a lookup on) — still reaches
        // `gpkg_spatial_ref_sys.definition` via `NamedCrs::structural_wkt`,
        // instead of falling to a bare "undefined".
        let pj = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"custom datum","ellipsoid":{"name":"custom ellipsoid","semi_major_axis":6378137,"inverse_flattening":298.257223563}}}"#;
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: None,
            projjson: Some(pj.into()),
        }));
        let bytes = write_layers(None, &[("custom".into(), layer)], false, false).unwrap();

        use crate::sqlite::Database;
        assert!(Database::open(&bytes).is_ok());

        let back = read_layers(&bytes).unwrap();
        match &back[0].1.crs {
            Some(Crs::Named(n)) => {
                let wkt = n.wkt.as_deref().expect("structural translation produced a WKT string");
                assert!(wkt.starts_with("GEOGCS[\"custom\""), "{wkt}");
                assert!(wkt.contains("SPHEROID[\"custom ellipsoid\",6378137,298.257223563]"), "{wkt}");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn real_fixture_reads_its_declared_srs() {
        // This DuckDB-written fixture declares srs_id 0 ("undefined geographic")
        // on the layer, so faithful pass-through reports no CRS rather than
        // inventing one.
        let bytes = include_bytes!("../../tests/fixtures/points.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers[0].1.crs, None);
    }

    #[test]
    fn append_adds_a_layer_then_upsert_replaces() {
        let a = fc(vec![Feature { geometry: point(0.0, 0.0), properties: vec![], native: None }]);
        let b = fc(vec![
            Feature { geometry: point(1.0, 1.0), properties: vec![], native: None },
            Feature { geometry: point(2.0, 2.0), properties: vec![], native: None },
        ]);

        let g1 = write_layers(None, &[("a".into(), a)], false, false).unwrap();
        let g2 = write_layers(Some(&g1), &[("b".into(), b)], false, false).unwrap();
        let layers = read_layers(&g2).unwrap();
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().any(|(n, _)| n == "a"));
        assert!(layers.iter().any(|(n, l)| n == "b" && l.features.len() == 2));

        // Upsert "a" with three features -> still two layers, "a" replaced.
        let a2 = fc(vec![Feature { geometry: point(9.0, 9.0), properties: vec![], native: None }; 3]);
        let g3 = write_layers(Some(&g2), &[("a".into(), a2)], false, false).unwrap();
        let layers = read_layers(&g3).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers.iter().find(|(n, _)| n == "a").unwrap().1.features.len(), 3);
    }
}
