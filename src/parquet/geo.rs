//! Build the GeoParquet `geo` file-metadata value (a JSON string stored under
//! the `geo` key in Parquet's key/value metadata).
//!
//! See the GeoParquet specification: it records the version, the primary
//! geometry column, and per-column encoding / geometry types / bbox.

use crate::crs::{crs_from_projjson, crs_from_projjson_text, Crs};
use crate::geometry::Bbox;
use crate::json::JsonValue;

/// The column name Geosetta uses for geometry.
pub const GEOMETRY_COLUMN: &str = "geometry";

const GEOPARQUET_VERSION: &str = "1.1.0";

/// Render the `geo` metadata JSON.
///
/// `geometry_types` is the sorted, de-duplicated set of geometry type names
/// present; `bbox` is included only when non-empty.
///
/// The `bbox` is written in GeoParquet's 3D form —
/// `[minx, miny, minz, maxx, maxy, maxz]`, all mins then all maxes — whenever
/// any position carried a Z, and in the 2D four-element form otherwise. That
/// length switch matches what GDAL writes. M is never included: GeoParquet's
/// bbox has no M form, and an envelope bounds *space*, not a linear-referencing
/// measure (see `plans/envelope.org`).
///
/// The Z range covers only positions that actually carried a Z, which is
/// [`Bbox`]'s standing mixed-dimensionality rule. On mixed input this
/// deliberately differs from GDAL, which promotes 2D geometries to Z=0 on write
/// but folds its `minz` from the originally-3D features alone — leaving a bbox
/// that does not cover the Z=0 values it just wrote. Geosetta preserves 2D
/// geometry as 2D, so folding real Zs only is exact here. The invariant to hold
/// on to, whichever way a future writer goes, is that the Z range must cover
/// every Z ordinate actually written: a too-large bbox costs only speed, while
/// a too-small one makes a trusting reader silently drop matching rows.
///
/// `crs` is written as the
/// GeoParquet `crs` member (a PROJJSON object): it is *omitted* for the default
/// (`Crs::Wgs84`/`None`), which GeoParquet reads as OGC:CRS84, and emitted
/// verbatim when the source carried PROJJSON.
///
/// A non-default CRS that arrived without PROJJSON (e.g. from GeoPackage or
/// FlatGeobuf, which record an authority + code) is emitted as a minimal
/// PROJJSON object carrying just that authority/code `id`, e.g.
/// `{"id":{"authority":"EPSG","code":7844}}`. That preserves the CRS *identity*
/// through the hub without Geosetta ever interpreting it — the same
/// authority+code the other spokes pass through — and PROJ-backed readers
/// (DuckDB, GDAL) resolve the code to the full definition. Only when there is no
/// authority+code *and* no PROJJSON is the CRS omitted, since there is then
/// nothing GeoParquet can express.
pub fn metadata(geometry_types: &[String], bbox: &Bbox, crs: Option<&Crs>) -> String {
    let mut s = String::new();
    s.push_str("{\"version\":\"");
    s.push_str(GEOPARQUET_VERSION);
    s.push_str("\",\"primary_column\":\"");
    s.push_str(GEOMETRY_COLUMN);
    s.push_str("\",\"columns\":{\"");
    s.push_str(GEOMETRY_COLUMN);
    s.push_str("\":{\"encoding\":\"WKB\",\"geometry_types\":[");
    for (i, t) in geometry_types.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        s.push_str(t);
        s.push('"');
    }
    s.push(']');
    if !bbox.is_empty() {
        s.push_str(",\"bbox\":[");
        s.push_str(&fmt_num(bbox.min_x));
        s.push(',');
        s.push_str(&fmt_num(bbox.min_y));
        // The 3D form interleaves: all mins, then all maxes. Verified against
        // real GDAL output, not inferred from the 2D case — and deliberately
        // *not* GPB's per-dimension `minx,maxx,miny,maxy,minz,maxz` pairing.
        if let Some((min_z, _)) = bbox.z {
            s.push(',');
            s.push_str(&fmt_num(min_z));
        }
        s.push(',');
        s.push_str(&fmt_num(bbox.max_x));
        s.push(',');
        s.push_str(&fmt_num(bbox.max_y));
        if let Some((_, max_z)) = bbox.z {
            s.push(',');
            s.push_str(&fmt_num(max_z));
        }
        s.push(']');
    }
    if let Some(Crs::Named(named)) = crs
        && let Some(crs_json) = crs_projjson(named)
    {
        s.push_str(",\"crs\":");
        s.push_str(&crs_json);
    }
    s.push_str("}}}");
    s
}

/// The PROJJSON to write for a non-default CRS, or `None` when GeoParquet has no
/// way to express it. In priority order:
///
/// 1. A source that carried verbatim PROJJSON is re-emitted unchanged — which
///    includes a definition the user supplied with `--crs`, since that is
///    installed on the IR exactly like one read from a file.
/// 2. A WKT definition is translated to PROJJSON when Geosetta can
///    ([`crate::crs::wkt_to_projjson`], geographic CRSes) — a *resolvable*
///    definition that PROJ/GDAL/QGIS read back correctly.
/// 3. Failing both of the above, an authority + code is rendered as a minimal
///    PROJJSON `id` reference (`{"id":{"authority":"EPSG","code":7844}}`), the
///    inverse of the `id` lifting in [`parse_crs`]. This preserves the identity
///    for Geosetta's own round trip but is *not* resolvable by PROJ-backed
///    readers — the CLI warns (see [`crate::crs::Crs::downgrade_warning`]) when
///    it comes to this.
fn crs_projjson(named: &crate::crs::NamedCrs) -> Option<String> {
    if let Some(projjson) = &named.projjson {
        return Some(projjson.clone());
    }
    if let Some(wkt) = &named.wkt
        && let Some(projjson) = crate::crs::wkt_to_projjson(wkt)
    {
        return Some(projjson);
    }
    let (authority, code) = (named.authority.clone()?, named.code.clone()?);
    Some(format!(
        "{{\"id\":{{\"authority\":{},\"code\":{}}}}}",
        JsonValue::String(authority).to_json_string(),
        code_json_literal(&code),
    ))
}

/// PROJJSON's `id.code` is spec'd as an integer *or* a string; emit whichever
/// the code actually is — bare for a purely-numeric code (the common
/// EPSG/ESRI/IAU_2015 case, and what real PROJJSON emits), quoted for anything
/// else (IGNF/OGC/PROJ/NKG's alphanumeric codes, e.g. `"LAMB93"`).
fn code_json_literal(code: &str) -> String {
    if !code.is_empty() && code.bytes().all(|b| b.is_ascii_digit()) {
        code.to_string()
    } else {
        JsonValue::String(code.to_string()).to_json_string()
    }
}

/// Recover the CRS from a `geo` metadata JSON string (the inverse of the `crs`
/// half of [`metadata`]). GeoParquet's `crs` is a PROJJSON object on the primary
/// geometry column; an absent or null `crs` means the OGC:CRS84 default. The
/// PROJJSON is carried through verbatim, and its `id` (authority + code), when
/// present, is lifted out so non-Parquet writers can reference it.
pub fn parse_crs(geo: &str) -> Option<Crs> {
    let doc = crate::json::parse(geo).ok()?;
    let primary = doc.get("primary_column").and_then(JsonValue::as_str)?;
    let column = doc.get("columns").and_then(|c| c.get(primary))?;
    match column.get("crs") {
        // Absent or explicit null → the GeoParquet default, OGC:CRS84.
        None | Some(JsonValue::Null) => Some(Crs::Wgs84),
        // The parsed value supplies the `id`; the definition itself is sliced
        // straight out of `geo` so it survives byte-for-byte. `raw_at` walks
        // the same path the navigation above did and so finds the same value —
        // the fallback exists only so a disagreement would cost formatting
        // rather than the whole CRS.
        Some(crs) => {
            let raw = crate::json::raw_at(geo, &["columns", primary, "crs"])
                .ok()
                .flatten();
            Some(match raw {
                Some(raw) => crs_from_projjson(crs, raw),
                None => crs_from_projjson(crs, &crs.to_json_string()),
            })
        }
    }
}

/// Recover the CRS from Parquet's native `GEOMETRY`/`GEOGRAPHY` logical
/// type's own `crs` field (`SchemaElement.logicalType`, parsed in
/// `parquet/reader.rs`'s `parse_geometry_logical_type`) — a raw PROJJSON
/// string, unlike `geo` metadata's `crs`, which is nested under
/// `columns.<primary>.crs`. Absent means the format's own default, OGC:CRS84
/// (mirrors `geo`'s absent/null `crs` convention in [`parse_crs`]) — this is
/// the on-schema fallback for writers (e.g. `ogr2ogr -lco
/// USE_PARQUET_GEO_TYPES=ONLY`) that drop `geo` entirely in favor of the
/// native type.
pub fn parse_native_geometry_crs(crs_projjson: Option<&str>) -> Option<Crs> {
    match crs_projjson {
        None => Some(Crs::Wgs84),
        Some(s) => crs_from_projjson_text(s).ok(),
    }
}

/// Format a coordinate as a JSON number (finite values only reach here).
fn fmt_num(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Position;

    #[test]
    fn includes_types_and_bbox() {
        let mut bbox = Bbox::empty();
        bbox.add(Position::new(1.0, 2.0));
        bbox.add(Position::new(3.0, 4.5));
        let json = metadata(&["Point".to_string()], &bbox, Some(&Crs::Wgs84));
        assert!(json.contains("\"encoding\":\"WKB\""));
        assert!(json.contains("\"primary_column\":\"geometry\""));
        assert!(json.contains("\"geometry_types\":[\"Point\"]"));
        assert!(json.contains("\"bbox\":[1,2,3,4.5]"));
        // The default CRS is left implicit (no `crs` member).
        assert!(!json.contains("crs"));
    }

    #[test]
    fn writes_six_element_bbox_when_z_is_present() {
        let mut bbox = Bbox::empty();
        bbox.add(Position::with_z(0.0, 0.0, 0.0));
        bbox.add(Position::with_z(20.0, 10.0, 10.0));
        let json = metadata(&["LineString Z".to_string()], &bbox, None);
        // All mins, then all maxes — the layout real GDAL output was checked
        // against, and *not* GPB's per-dimension pairing.
        assert!(json.contains("\"bbox\":[0,0,0,20,10,10]"), "{json}");
    }

    #[test]
    fn m_never_reaches_the_bbox() {
        // GeoParquet's bbox has no M form at all, so an M-carrying collection
        // still writes the plain 2D four-element box.
        let mut bbox = Bbox::empty();
        bbox.add(Position::with_m(1.0, 2.0, 100.0));
        bbox.add(Position::with_m(3.0, 4.0, 200.0));
        let json = metadata(&["LineString M".to_string()], &bbox, None);
        assert!(json.contains("\"bbox\":[1,2,3,4]"), "{json}");
        assert!(!json.contains("100"), "M leaked into the bbox: {json}");
    }

    #[test]
    fn mixed_dimensionality_bbox_covers_every_z_actually_written() {
        // The invariant, asserted as an invariant rather than as literal
        // numbers: the Z range must contain every Z ordinate in the data. This
        // is what makes folding real Zs only (rather than GDAL's promote-to-0)
        // safe, and it is what would trip if a future writer started promoting
        // 2D geometry to Z=0 without widening the fold to match.
        let positions = [
            Position::new(0.0, 0.0),
            Position::new(10.0, 10.0),
            Position::with_z(100.0, 100.0, 50.0),
            Position::with_z(110.0, 110.0, 60.0),
        ];
        let mut bbox = Bbox::empty();
        for p in positions {
            bbox.add(p);
        }
        let json = metadata(
            &["LineString".to_string(), "LineString Z".to_string()],
            &bbox,
            None,
        );
        assert!(json.contains("\"bbox\":[0,0,50,110,110,60]"), "{json}");

        let (min_z, max_z) = bbox.z.expect("some position carried a Z");
        for p in positions {
            if let Some(z) = p.z {
                assert!(
                    z >= min_z && z <= max_z,
                    "z {z} escapes the written bbox range {min_z}..{max_z}"
                );
            }
        }
    }

    #[test]
    fn omits_empty_bbox() {
        let json = metadata(&[], &Bbox::empty(), None);
        assert!(!json.contains("bbox"));
        assert!(json.contains("\"geometry_types\":[]"));
    }

    #[test]
    fn emits_and_recovers_projjson_crs() {
        use crate::crs::NamedCrs;
        let projjson = "{\"type\":\"ProjectedCRS\",\"id\":{\"authority\":\"EPSG\",\"code\":3857}}";
        let crs = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: None,
            projjson: Some(projjson.into()),
        });
        let json = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        assert!(json.contains("\"crs\":{\"type\":\"ProjectedCRS\""));

        // Round-trips back to an equivalent Crs, with the id lifted out.
        match parse_crs(&json).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("3857"));
                assert!(n.projjson.is_some());
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn a_pretty_printed_definition_survives_byte_for_byte() {
        // The verbatim contract, and the reason `parse_crs` slices `geo` rather
        // than re-printing the parsed value: this crate's serializer is compact,
        // so a round trip through it would silently reformat a definition
        // geosetta never interprets and has no business restyling.
        let projjson = "{\n  \"type\": \"GeographicCRS\",\n  \"name\": \"GDA2020\",\n  \
                        \"id\": {\n    \"authority\": \"EPSG\",\n    \"code\": 7844\n  }\n}";
        let geo = format!(
            "{{\"version\":\"1.1.0\",\"primary_column\":\"geometry\",\"columns\":\
             {{\"geometry\":{{\"encoding\":\"WKB\",\"crs\":{projjson}}}}}}}"
        );
        match parse_crs(&geo).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.projjson.as_deref(), Some(projjson), "not byte-identical");
                // The id is still lifted from the parse — verbatim storage does
                // not cost the identity.
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("7844"));
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn a_definitions_own_spelling_of_its_numbers_survives() {
        // Number formatting is where a re-print diverges most quietly: `6378137.0`
        // and `1e2` both parse to an f64 whose `Display` spells it differently.
        // Verbatim means the file's spelling, not Rust's.
        let projjson = r#"{"type":"GeographicCRS","a":6378137.0,"rf":2.982572e2}"#;
        let geo = format!(
            "{{\"version\":\"1.1.0\",\"primary_column\":\"geometry\",\"columns\":\
             {{\"geometry\":{{\"crs\":{projjson}}}}}}}"
        );
        match parse_crs(&geo).unwrap() {
            Crs::Named(n) => assert_eq!(n.projjson.as_deref(), Some(projjson)),
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn a_verbatim_definition_re_emits_unchanged_through_the_writer() {
        // Read → write is a no-op on the definition text, which is what makes
        // a geosetta-to-geosetta GeoParquet round trip lossless on the CRS.
        let projjson = "{\n  \"type\": \"ProjectedCRS\",\n  \"name\": \"custom\"\n}";
        let geo = format!(
            "{{\"version\":\"1.1.0\",\"primary_column\":\"geometry\",\"columns\":\
             {{\"geometry\":{{\"crs\":{projjson}}}}}}}"
        );
        let crs = parse_crs(&geo).unwrap();
        let rewritten = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        assert!(
            rewritten.contains(projjson),
            "definition was restyled: {rewritten}"
        );
        assert_eq!(
            parse_crs(&rewritten),
            Some(crs),
            "and it survives a second pass"
        );
    }

    #[test]
    fn the_native_logical_type_keeps_its_definition_verbatim_too() {
        // The other PROJJSON entry point: here the field already *is* the
        // definition text, so verbatim costs nothing but must still be honored.
        let projjson = "{\n  \"type\": \"GeographicCRS\",\n  \"name\": \"WGS 84 / custom\"\n}";
        match parse_native_geometry_crs(Some(projjson)).unwrap() {
            Crs::Named(n) => assert_eq!(n.projjson.as_deref(), Some(projjson)),
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn absent_crs_parses_as_wgs84() {
        let json = metadata(&[], &Bbox::empty(), None);
        assert_eq!(parse_crs(&json), Some(Crs::Wgs84));
    }

    #[test]
    fn alphanumeric_code_round_trips_as_a_json_string() {
        use crate::crs::NamedCrs;
        // IGNF/OGC/PROJ/NKG-style codes are alphanumeric; PROJJSON's id.code
        // must be quoted for these (unlike EPSG's bare-number codes), or the
        // value comes back malformed. A made-up authority keeps this
        // independent of whether the registry happens to resolve it.
        let crs = Crs::Named(NamedCrs {
            authority: Some("CUSTOM".into()),
            code: Some("GRID-1".into()),
            wkt: None,
            projjson: None,
        });
        let json = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        assert!(json.contains("\"crs\":{\"id\":{\"authority\":\"CUSTOM\",\"code\":\"GRID-1\"}}"), "{json}");

        match parse_crs(&json).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("CUSTOM"));
                assert_eq!(n.code.as_deref(), Some("GRID-1"));
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn authority_code_without_projjson_is_emitted_as_id_reference() {
        use crate::crs::NamedCrs;
        // What GeoPackage / FlatGeobuf hand us: an authority + code (here
        // EPSG:7844, GDA2020) and maybe WKT, but no PROJJSON. Must not be
        // dropped — an omitted `crs` would read back as the WGS 84 default.
        // An id reference is the best GeoParquet can do from a code alone; the
        // CLI warns and points at `--crs`, which is how a real definition for
        // this code gets in (see `crs::Crs::downgrade_warning`).
        let crs = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("7844".into()),
            wkt: Some("GEOGCRS[\"GDA2020\"]".into()),
            projjson: None,
        });
        let json = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        assert!(json.contains("\"crs\":{\"id\":{\"authority\":\"EPSG\",\"code\":7844}}"));

        match parse_crs(&json).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("7844"));
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn crs_with_neither_code_nor_projjson_is_omitted() {
        use crate::crs::NamedCrs;
        // WKT only, no authority/code and no PROJJSON: GeoParquet cannot express
        // it, so it is omitted rather than guessed.
        let crs = Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: Some("GEOGCRS[\"something\"]".into()),
            projjson: None,
        });
        let json = metadata(&[], &Bbox::empty(), Some(&crs));
        assert!(!json.contains("crs"));
    }
}
