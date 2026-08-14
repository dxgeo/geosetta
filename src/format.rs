//! The set of supported vector formats.
//!
//! This is a core, CLI-neutral type: the conversion API in [`crate::convert`]
//! is keyed by [`Format`], and callers (the `geosetta` binary or any library
//! consumer) select formats through it. Extension/name parsing lives here too so
//! both the CLI and library users can reuse it.

use crate::error::{Error, Result};

/// A supported vector format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    GeoJson,
    Parquet,
    FlatGeobuf,
    Csv,
    Wkt,
    Gpkg,
    Shapefile,
    Kml,
    Kmz,
}

impl Format {
    /// Parse an explicit format name (e.g. a `--from`/`--to` value).
    pub fn parse(name: &str) -> Result<Format> {
        match name.to_ascii_lowercase().as_str() {
            "geojson" | "json" => Ok(Format::GeoJson),
            "parquet" | "geoparquet" => Ok(Format::Parquet),
            "flatgeobuf" | "fgb" => Ok(Format::FlatGeobuf),
            "csv" => Ok(Format::Csv),
            "wkt" => Ok(Format::Wkt),
            "gpkg" | "geopackage" => Ok(Format::Gpkg),
            "shapefile" | "shp" | "esri shapefile" => Ok(Format::Shapefile),
            "kml" => Ok(Format::Kml),
            "kmz" => Ok(Format::Kmz),
            other => Err(Error::Usage(format!("unknown format \"{other}\""))),
        }
    }

    /// Infer a format from a file path's extension.
    pub fn from_path(path: &str) -> Option<Format> {
        let ext = path.rsplit('.').next()?.to_ascii_lowercase();
        match ext.as_str() {
            "geojson" | "json" => Some(Format::GeoJson),
            "parquet" => Some(Format::Parquet),
            "fgb" => Some(Format::FlatGeobuf),
            "csv" => Some(Format::Csv),
            "wkt" => Some(Format::Wkt),
            "gpkg" => Some(Format::Gpkg),
            "shp" => Some(Format::Shapefile),
            "kml" => Some(Format::Kml),
            "kmz" => Some(Format::Kmz),
            _ => None,
        }
    }

    /// The canonical file extension, used to name fan-out outputs.
    pub fn extension(self) -> &'static str {
        match self {
            Format::GeoJson => "geojson",
            Format::Parquet => "parquet",
            Format::FlatGeobuf => "fgb",
            Format::Csv => "csv",
            Format::Wkt => "wkt",
            Format::Gpkg => "gpkg",
            Format::Shapefile => "shp",
            Format::Kml => "kml",
            Format::Kmz => "kmz",
        }
    }
}
