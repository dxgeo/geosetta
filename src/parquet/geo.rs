//! Build the GeoParquet `geo` file-metadata value (a JSON string stored under
//! the `geo` key in Parquet's key/value metadata).
//!
//! See the GeoParquet specification: it records the version, the primary
//! geometry column, and per-column encoding / geometry types / bbox.

use crate::crs::Crs;
use crate::geometry::Bbox;
use crate::json::JsonValue;

/// The column name Geosetta uses for geometry.
pub const GEOMETRY_COLUMN: &str = "geometry";

const GEOPARQUET_VERSION: &str = "1.1.0";

/// Render the `geo` metadata JSON.
///
/// `geometry_types` is the sorted, de-duplicated set of geometry type names
/// present; `bbox` is included only when non-empty. `crs` is written as the
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
        s.push(',');
        s.push_str(&fmt_num(bbox.max_x));
        s.push(',');
        s.push_str(&fmt_num(bbox.max_y));
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
/// 1. A source that carried verbatim PROJJSON is re-emitted unchanged.
/// 2. A WKT definition is translated to PROJJSON when Geosetta can
///    ([`crate::crs::wkt_to_projjson`], geographic CRSes) — a *resolvable*
///    definition that PROJ/GDAL/QGIS read back correctly.
/// 3. Failing both, an authority + code is rendered as a minimal PROJJSON `id`
///    reference (`{"id":{"authority":"EPSG","code":7844}}`), the inverse of the
///    `id` lifting in [`parse_crs`]. This preserves the identity for Geosetta's
///    own round trip but is *not* resolvable by PROJ-backed readers — the CLI
///    warns (see [`crate::crs::Crs::downgrade_warning`]) when it comes to this.
fn crs_projjson(named: &crate::crs::NamedCrs) -> Option<String> {
    if let Some(projjson) = &named.projjson {
        return Some(projjson.clone());
    }
    if let Some(wkt) = &named.wkt
        && let Some(projjson) = crate::crs::wkt_to_projjson(wkt)
    {
        return Some(projjson);
    }
    let (authority, code) = (named.authority.clone()?, named.code?);
    Some(format!(
        "{{\"id\":{{\"authority\":{},\"code\":{code}}}}}",
        JsonValue::String(authority).to_json_string()
    ))
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
        Some(crs) => {
            let id = crs.get("id");
            let authority = id
                .and_then(|i| i.get("authority"))
                .and_then(JsonValue::as_str)
                .map(String::from);
            let code = id.and_then(|i| i.get("code")).and_then(JsonValue::as_f64).map(|c| c as i64);
            Some(Crs::from_authority_code(
                authority,
                code,
                None,
                Some(crs.to_json_string()),
            ))
        }
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

    #[test]
    fn includes_types_and_bbox() {
        let mut bbox = Bbox::empty();
        bbox.add([1.0, 2.0]);
        bbox.add([3.0, 4.5]);
        let json = metadata(&["Point".to_string()], &bbox, Some(&Crs::Wgs84));
        assert!(json.contains("\"encoding\":\"WKB\""));
        assert!(json.contains("\"primary_column\":\"geometry\""));
        assert!(json.contains("\"geometry_types\":[\"Point\"]"));
        assert!(json.contains("\"bbox\":[1,2,3,4.5]"));
        // The default CRS is left implicit (no `crs` member).
        assert!(!json.contains("crs"));
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
            code: Some(3857),
            wkt: None,
            projjson: Some(projjson.into()),
        });
        let json = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        assert!(json.contains("\"crs\":{\"type\":\"ProjectedCRS\""));

        // Round-trips back to an equivalent Crs, with the id lifted out.
        match parse_crs(&json).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code, Some(3857));
                assert!(n.projjson.is_some());
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn absent_crs_parses_as_wgs84() {
        let json = metadata(&[], &Bbox::empty(), None);
        assert_eq!(parse_crs(&json), Some(Crs::Wgs84));
    }

    #[test]
    fn authority_code_without_projjson_is_emitted_as_id_reference() {
        use crate::crs::NamedCrs;
        // What GeoPackage / FlatGeobuf hand us: an authority + code (here
        // EPSG:7844, GDA2020) and maybe WKT, but no PROJJSON. Must not be
        // dropped — an omitted `crs` would read back as the WGS 84 default.
        let crs = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some(7844),
            wkt: Some("GEOGCRS[\"GDA2020\"]".into()),
            projjson: None,
        });
        let json = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        assert!(json.contains("\"crs\":{\"id\":{\"authority\":\"EPSG\",\"code\":7844}}"));

        match parse_crs(&json).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code, Some(7844));
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
