//! Build the GeoParquet `geo` file-metadata value (a JSON string stored under
//! the `geo` key in Parquet's key/value metadata).
//!
//! See the GeoParquet specification: it records the version, the primary
//! geometry column, and per-column encoding / geometry types / bbox.

use crate::geometry::Bbox;

/// The column name Geosetta uses for geometry.
pub const GEOMETRY_COLUMN: &str = "geometry";

const GEOPARQUET_VERSION: &str = "1.1.0";

/// Render the `geo` metadata JSON.
///
/// `geometry_types` is the sorted, de-duplicated set of geometry type names
/// present; `bbox` is included only when non-empty.
pub fn metadata(geometry_types: &[String], bbox: &Bbox) -> String {
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
    s.push_str("}}}");
    s
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
        let json = metadata(&["Point".to_string()], &bbox);
        assert!(json.contains("\"encoding\":\"WKB\""));
        assert!(json.contains("\"primary_column\":\"geometry\""));
        assert!(json.contains("\"geometry_types\":[\"Point\"]"));
        assert!(json.contains("\"bbox\":[1,2,3,4.5]"));
    }

    #[test]
    fn omits_empty_bbox() {
        let json = metadata(&[], &Bbox::empty());
        assert!(!json.contains("bbox"));
        assert!(json.contains("\"geometry_types\":[]"));
    }
}
