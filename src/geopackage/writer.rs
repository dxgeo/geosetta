//! Write GeoPackage files: build the required GPKG metadata tables plus one
//! feature table per layer, and serialize the whole thing with the from-scratch
//! [`crate::sqlite`] writer. Append is read-modify-write — read the existing
//! layers, upsert, and rewrite the complete file (see `plans/geopackage.org`).

use crate::crs::Crs;
use crate::error::Result;
use crate::feature::FeatureCollection;
use crate::geometry::{to_wkb, Bbox, Geometry};
use crate::schema::{infer_columns, Cell, ColumnType};
use crate::sqlite::{MasterEntry, TableSpec, Value, write_database};

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

fn extensions_table(rows: Vec<ExtensionRow>) -> TableSpec {
    TableSpec {
        name: "gpkg_extensions".into(),
        sql: "CREATE TABLE gpkg_extensions (table_name TEXT, column_name TEXT, extension_name TEXT NOT NULL, definition TEXT NOT NULL, scope TEXT NOT NULL)".into(),
        rows: rows
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
            .collect(),
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
    let sql = "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER NOT NULL PRIMARY KEY, organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL, definition TEXT NOT NULL, description TEXT)";
    TableSpec { name: "gpkg_spatial_ref_sys".into(), sql: sql.into(), rows }
}

fn gpkg_contents(layers: &[(String, FeatureCollection)], per_layer_srs: &[i64]) -> TableSpec {
    let rows = layers
        .iter()
        .zip(per_layer_srs)
        .enumerate()
        .map(|(i, ((name, fc), &srs_id))| {
            let (minx, miny, maxx, maxy) = bbox(fc);
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("features".into()),
                    Value::Text(name.clone()),
                    Value::Text(String::new()),
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
        // No TEXT PRIMARY KEY / UNIQUE: those would require auto-indexes we do
        // not build. The columns are otherwise the GPKG-standard ones.
        sql: "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL, data_type TEXT NOT NULL, identifier TEXT, description TEXT, last_change DATETIME NOT NULL, min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER)".into(),
        rows,
    }
}

fn geometry_columns(layers: &[(String, FeatureCollection)], per_layer_srs: &[i64]) -> TableSpec {
    let rows = layers
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
        sql: "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL, m TINYINT NOT NULL)".into(),
        rows,
    }
}

fn feature_table(name: &str, fc: &FeatureCollection, srs_id: i64, envelope: bool) -> TableSpec {
    let columns = infer_columns(&fc.features);

    // DDL: fid (rowid), geom, then one column per property.
    let mut sql = format!("CREATE TABLE {} (\"fid\" INTEGER PRIMARY KEY NOT NULL, \"geom\" GEOMETRY", quote(name));
    for col in &columns {
        sql.push_str(&format!(", {} {}", quote(&col.name), sqlite_type(col.ty)));
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

fn sqlite_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Int64 => "INTEGER",
        ColumnType::Double => "REAL",
        ColumnType::String => "TEXT",
    }
}

fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Int(*b as i64),
        Cell::Int(n) => Value::Int(*n),
        Cell::Double(d) => Value::Real(*d),
        Cell::Str(s) => Value::Text(s.clone()),
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

    #[test]
    fn write_then_read_round_trips() {
        let layer = fc(vec![
            Feature {
                geometry: point(1.0, 2.0),
                properties: vec![
                    ("name".into(), JsonValue::String("a".into())),
                    ("n".into(), JsonValue::Number { value: 5.0, is_int: true }),
                ],
            },
            Feature {
                geometry: point(3.0, 4.0),
                properties: vec![
                    ("name".into(), JsonValue::String("b".into())),
                    ("n".into(), JsonValue::Null),
                ],
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
            .map(|i| Feature { geometry: point(i as f64, (i % 7) as f64), properties: vec![] })
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
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![] }]);
        layer.crs = Some(Crs::Wgs84);
        let bytes = write_layers(None, &[("p".into(), layer)], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.crs, Some(Crs::Wgs84));
    }

    #[test]
    fn no_crs_layer_round_trips_as_none() {
        // CSV/WKT-style input with no CRS lands on the "undefined" SRS and reads
        // back as no CRS, not a mislabeled 4326.
        let layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![] }]);
        let bytes = write_layers(None, &[("p".into(), layer)], false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.crs, None);
    }

    #[test]
    fn named_crs_is_registered_and_round_trips() {
        use crate::crs::NamedCrs;
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![] }]);
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
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![] }]);
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
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![] }]);
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
        let a = fc(vec![Feature { geometry: point(0.0, 0.0), properties: vec![] }]);
        let b = fc(vec![
            Feature { geometry: point(1.0, 1.0), properties: vec![] },
            Feature { geometry: point(2.0, 2.0), properties: vec![] },
        ]);

        let g1 = write_layers(None, &[("a".into(), a)], false, false).unwrap();
        let g2 = write_layers(Some(&g1), &[("b".into(), b)], false, false).unwrap();
        let layers = read_layers(&g2).unwrap();
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().any(|(n, _)| n == "a"));
        assert!(layers.iter().any(|(n, l)| n == "b" && l.features.len() == 2));

        // Upsert "a" with three features -> still two layers, "a" replaced.
        let a2 = fc(vec![Feature { geometry: point(9.0, 9.0), properties: vec![] }; 3]);
        let g3 = write_layers(Some(&g2), &[("a".into(), a2)], false, false).unwrap();
        let layers = read_layers(&g3).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers.iter().find(|(n, _)| n == "a").unwrap().1.features.len(), 3);
    }
}
