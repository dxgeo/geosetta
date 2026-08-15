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
/// 2. With the `crs-registry` feature, an authority + code resolves to the
///    embedded registry's authoritative PROJJSON for that code (the R1
///    trusted-id path — see [`crate::crs::NamedCrs::registry_projjson`]). A
///    no-op (always `None`) without the feature.
/// 3. A WKT definition is translated to PROJJSON when Geosetta can
///    ([`crate::crs::wkt_to_projjson`], geographic CRSes) — a *resolvable*
///    definition that PROJ/GDAL/QGIS read back correctly.
/// 4. Failing all of the above, an authority + code is rendered as a minimal
///    PROJJSON `id` reference (`{"id":{"authority":"EPSG","code":7844}}`), the
///    inverse of the `id` lifting in [`parse_crs`]. This preserves the identity
///    for Geosetta's own round trip but is *not* resolvable by PROJ-backed
///    readers — the CLI warns (see [`crate::crs::Crs::downgrade_warning`]) when
///    it comes to this.
fn crs_projjson(named: &crate::crs::NamedCrs) -> Option<String> {
    if let Some(projjson) = &named.projjson {
        return Some(projjson.clone());
    }
    if let Some(projjson) = named.registry_projjson() {
        return Some(projjson.to_string());
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
        Some(crs) => Some(crs_from_projjson_value(crs)),
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
        Some(s) => crate::json::parse(s).ok().map(|v| crs_from_projjson_value(&v)),
    }
}

/// Shared by [`parse_crs`] and [`parse_native_geometry_crs`]: lift a PROJJSON
/// object's `id.authority`/`id.code`, if present, alongside the verbatim
/// PROJJSON text.
fn crs_from_projjson_value(crs: &JsonValue) -> Crs {
    let id = crs.get("id");
    let authority = id
        .and_then(|i| i.get("authority"))
        .and_then(JsonValue::as_str)
        .map(String::from);
    let code = id.and_then(|i| i.get("code")).and_then(json_code_as_string);
    Crs::from_authority_code(authority, code, None, Some(crs.to_json_string()))
}

/// A PROJJSON `id.code` value read back as a string, whichever JSON type it
/// arrived as (the inverse of [`code_json_literal`]): a JSON string is used
/// verbatim (IGNF/OGC/PROJ/NKG-style alphanumeric codes), a JSON number is
/// formatted without a spurious fractional part (authority codes are always
/// integers).
fn json_code_as_string(v: &JsonValue) -> Option<String> {
    match v.as_str() {
        Some(s) => Some(s.to_string()),
        None => v.as_f64().map(fmt_num),
    }
}

/// Format a coordinate — or, via [`json_code_as_string`], a numeric authority
/// code — as a JSON number (finite values only reach here).
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
    #[cfg(not(feature = "crs-registry"))]
    fn authority_code_without_projjson_is_emitted_as_id_reference() {
        use crate::crs::NamedCrs;
        // What GeoPackage / FlatGeobuf hand us: an authority + code (here
        // EPSG:7844, GDA2020) and maybe WKT, but no PROJJSON. Must not be
        // dropped — an omitted `crs` would read back as the WGS 84 default.
        // Without the registry, this is the best GeoParquet can do.
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
    #[cfg(feature = "crs-registry")]
    fn authority_code_without_projjson_resolves_via_registry() {
        use crate::crs::NamedCrs;
        // Same input as the non-registry case, but with the feature on: the
        // registry resolves EPSG:7844 to its authoritative PROJJSON rather than
        // a minimal id reference — R1 step 2.
        let crs = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("7844".into()),
            wkt: Some("GEOGCRS[\"GDA2020\"]".into()),
            projjson: None,
        });
        let json = metadata(&["Point".to_string()], &Bbox::empty(), Some(&crs));
        // Not the minimal id-only form the non-registry path would emit...
        assert!(!json.contains("\"crs\":{\"id\":{\"authority\":\"EPSG\",\"code\":7844}}"), "{json}");
        // ...but the full authoritative definition, id included.
        assert!(json.contains("\"id\":{\"authority\":\"EPSG\",\"code\":7844}"), "{json}");
        assert!(json.contains("\"type\":\"GeographicCRS\""), "{json}");

        match parse_crs(&json).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("7844"));
                assert!(n.projjson.is_some());
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
