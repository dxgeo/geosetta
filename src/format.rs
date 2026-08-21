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
use crate::schema::DeclType;

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

    /// Whether this spoke *currently* carries a
    /// [`crate::FieldDecl`] — a column's declared width, precision, scale or
    /// constraints — through to its output.
    ///
    /// Unlike [`Self::supports_m`], which is a statement about each format's
    /// *spec*, this is a statement about this crate's *implementation*, and it
    /// will shrink as the spokes land. GeoPackage and GeoParquet both joined
    /// the list with M4, so **all four declaring spokes now carry one** and
    /// this predicate has stopped shrinking. GeoJSON, CSV, WKT and KML have no
    /// slot for a declaration at all and will always answer `false`.
    ///
    /// The distinction matters because the two cases warn for different
    /// reasons: one is the target's incapacity, the other is geosetta's, and
    /// only the second has a fix pending. Both drop the same information, so
    /// both warn — see `plans/lossy-conversion-warnings.org` § OWED WARNING.
    pub fn carries_column_schema(self) -> bool {
        matches!(self, Format::FlatGeobuf | Format::Shapefile | Format::Gpkg | Format::Parquet)
    }

    /// What a column declared `ty` comes back as after being written to this
    /// format and read again — the *composition* of this spoke's writer and
    /// reader type mappings.
    ///
    /// `None` when the format carries no declaration at all; that loss is
    /// [`crate::FeatureCollection::schema_drop_warning`]'s, not this one's.
    ///
    /// # This table is a claim, and a test proves it
    ///
    /// The mapping lives in the spokes, in eight separate `match` arms (a
    /// reader and a writer for each of the four declaring formats). Stating it
    /// a ninth time here would be exactly the duplication that let `REAL` mean
    /// 32 bits in one place and 64 in another for a day. It is safe only
    /// because `convert.rs`'s `every_declared_type_survives_or_is_admitted`
    /// round-trips *every* variant through *every* declaring format and
    /// asserts the real answer equals this one. Change a spoke's mapping
    /// without changing this and that test fails; change this without the
    /// spoke and it fails too.
    ///
    /// # Where the answers come from
    ///
    /// - **Shapefile** has one numeric field type, sized by width/decimals, so
    ///   every integer widens to `Int64` and every float to `Float64`. That is
    ///   dBase's design, not a defect.
    /// - **FlatGeobuf** has no `DATE`, only `DATETIME`, so a date lands there:
    ///   still a recast, but temporal rather than text, and it matches what
    ///   `ogr2ogr` writes for the same hop.
    /// - **`Binary` degrades only in Shapefile now.** It used to degrade
    ///   everywhere but GeoPackage, because the IR had no byte-level cell and
    ///   binary arrived as hex text no writer could tell from a string. Adding
    ///   `JsonValue::Binary` (2026-08-21) retired that for GeoParquet and
    ///   FlatGeobuf; dBase genuinely has no blob field type, and neither does
    ///   GDAL's shapefile driver.
    /// - **GeoParquet** answers here for well-formed values. A `Date` or
    ///   `DateTime` whose *text does not parse* falls back to UTF8 and comes
    ///   back `String`; that is value-dependent, so it belongs to the
    ///   post-write mechanism (`plans/lossy-conversion-warnings.org` § 2) and
    ///   is not predicted here.
    pub fn recast(self, ty: DeclType) -> Option<DeclType> {
        use DeclType::*;
        if !self.carries_column_schema() {
            return None;
        }
        Some(match self {
            Format::Shapefile => match ty {
                Bool | Date | String => ty,
                Int8 | UInt8 | Int16 | UInt16 | Int32 | UInt32 | Int64 | UInt64 => Int64,
                Float32 | Float64 => Float64,
                // dBase has no JSON, no timestamp and no blob field type.
                Json | DateTime | Binary => String,
            },
            // FlatGeobuf names every variant but `Date`, which it writes as
            // `DATETIME` — temporal rather than text, which is what `ogr2ogr`
            // does with the same hop.
            Format::FlatGeobuf => match ty {
                Date => DateTime,
                other => other,
            },
            // GeoPackage and GeoParquet both name every variant. GeoPackage's
            // unsigned spellings are its signed ones widened a step, since the
            // format has no unsigned type: `UInt16` is written `MEDIUMINT` and
            // so returns `Int32`, which admits it.
            Format::Gpkg => match ty {
                UInt8 => Int16,
                UInt16 => Int32,
                UInt32 | UInt64 => Int64,
                other => other,
            },
            Format::Parquet => ty,
            // `carries_column_schema` already returned for these.
            Format::GeoJson | Format::Csv | Format::Wkt | Format::Kml | Format::Kmz => {
                unreachable!("no column schema")
            }
        })
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
