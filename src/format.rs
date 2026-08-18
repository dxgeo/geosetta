//! The set of supported vector formats.
//!
//! This is a core, CLI-neutral type: the conversion API in [`mod@crate::convert`]
//! is keyed by [`Format`], and callers (the `geosetta` binary or any library
//! consumer) select formats through it. Extension/name parsing lives here too so
//! both the CLI and library users can reuse it.
//!
//! # Adding a format
//!
//! A new [`Format`] variant has obligations in two flavours, and only one of
//! them is enforced for you:
//!
//! - *The compiler catches these.* They match on the variant exhaustively, so
//!   the build breaks until each is filled in: [`Format::extension`],
//!   [`Format::display_name`], [`Format::supports_m`], the read/write dispatch
//!   in [`mod@crate::convert`], and [`crate::crs::Crs::downgrade_warning`] (which
//!   is where you decide what the new format *cannot* record — see
//!   `plans/lossy-conversion-warnings.org`; a spoke that silently drops
//!   something is a bug, not a gap).
//! - *Nobody catches these.* [`Format::parse`] and [`Format::from_path`] match
//!   on strings with a catch-all arm, so a variant missing from them compiles
//!   perfectly and is simply unreachable from the CLI — no `--from`/`--to`
//!   name, no extension inference. Add both.
//!
//! Beyond this module, a spoke needs its reader/writer against the IR and, if
//! its on-disk shape is not one file (GeoPackage's many layers, Shapefile's
//! sibling files), I/O handling in `main.rs`.

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

    /// The format's canonical human-readable name, for messages a user reads
    /// (warnings, errors). Distinct from [`Self::extension`], which names the
    /// file suffix: upper-casing an extension gets `GEOJSON`/`GPKG`/`SHP`,
    /// none of which is how anyone spells these formats. Every warning that
    /// names a target format goes through here, so one format has one spelling
    /// across the whole crate.
    pub fn display_name(self) -> &'static str {
        match self {
            Format::GeoJson => "GeoJSON",
            Format::Parquet => "GeoParquet",
            Format::FlatGeobuf => "FlatGeobuf",
            Format::Csv => "CSV",
            Format::Wkt => "WKT",
            Format::Gpkg => "GeoPackage",
            Format::Shapefile => "Shapefile",
            Format::Kml => "KML",
            Format::Kmz => "KMZ",
        }
    }

    /// Whether this format's own spec has any way to represent an M
    /// (measure) ordinate. Only GeoJSON (RFC 7946 positions are `[x,y,z?]`,
    /// nothing more) and KML/KMZ (`<coordinates>lon,lat,alt</coordinates>`
    /// has no measure slot at all) lack an M concept entirely; every other
    /// format spoke carries it through fully — see `plans/zm-geometry.org`.
    /// Z isn't checked here: every format spoke supports Z, so there is
    /// currently no Z-drop case; add one if that ever stops being true.
    pub fn supports_m(self) -> bool {
        !matches!(self, Format::GeoJson | Format::Kml | Format::Kmz)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the spellings themselves: these strings are user-visible, and the
    /// whole point of the helper is that a format reads the same everywhere.
    #[test]
    fn display_name_uses_each_format_s_own_spelling() {
        let expected = [
            (Format::GeoJson, "GeoJSON"),
            (Format::Parquet, "GeoParquet"),
            (Format::FlatGeobuf, "FlatGeobuf"),
            (Format::Csv, "CSV"),
            (Format::Wkt, "WKT"),
            (Format::Gpkg, "GeoPackage"),
            (Format::Shapefile, "Shapefile"),
            (Format::Kml, "KML"),
            (Format::Kmz, "KMZ"),
        ];
        for (format, name) in expected {
            assert_eq!(format.display_name(), name, "{format:?}");
        }
    }
}
