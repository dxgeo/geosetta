//! Geosetta — convert between open vector geospatial formats.
//!
//! Every format is routed through a shared, format-neutral feature IR
//! ([`FeatureCollection`]): `read(from) -> FeatureCollection -> write(to)`, so
//! any input format converts to any output format the writers support.
//!
//! The whole crate is standard-library only — every wire format (GeoJSON,
//! GeoParquet, FlatGeobuf, CSV, WKT, GeoPackage, Shapefile, KML/KMZ) is
//! implemented from its specification, with no third-party dependencies and no
//! optional ones — there is no feature that adds any (see [`crs`] for how a CRS
//! geosetta cannot resolve structurally is supplied instead).
//!
//! # Example
//!
//! ```no_run
//! use geosetta::{read_features, write_features, Format};
//!
//! let input = std::fs::read("in.geojson")?;
//! let mut warnings = Vec::new(); // what the read could not keep, if anything
//! let fc = read_features(Format::GeoJson, &input, &mut warnings)?;
//! let out = write_features(Format::Parquet, &fc)?;
//! std::fs::write("out.parquet", out)?;
//! # Ok::<(), geosetta::Error>(())
//! ```
//!
//! Conversions are byte-in / byte-out; file I/O is left to the caller (the
//! `geosetta` binary wraps this API with a CLI). GeoPackage is a multi-layer
//! container, so it has its own [`geopackage::read_layers`] /
//! [`geopackage::write_layers`] entry points. Shapefile is the opposite shape —
//! one logical layer split across sibling files (`.shp`/`.shx`/`.dbf`/`.prj`) —
//! so it has its own [`shapefile::read`] / [`shapefile::write`] entry points,
//! taking each sibling's bytes explicitly rather than a single buffer.

// Public API surface. Formats are selected via `Format`, and the conversion
// functions plus the IR types are re-exported at the crate root below.
pub mod convert;
pub mod crs;
pub mod error;
pub mod feature;
pub mod format;
pub mod geometry;
pub mod native;
pub mod geopackage;
pub mod shapefile;

// The CLI argument parser, used by the `geosetta` binary. Exposed so downstream
// tooling can reuse the same parsing if desired.
pub mod cli;

// Format spokes and shared machinery. These are crate-internal: library users
// go through `convert::{read_features, write_features}` keyed by `Format`
// rather than calling a spoke directly.
pub(crate) mod compress;
pub(crate) mod csv;
pub(crate) mod flatbuffers;
pub(crate) mod flatgeobuf;
pub(crate) mod geojson;
pub(crate) mod json;
pub(crate) mod kml;
pub(crate) mod parquet;
pub(crate) mod schema;
pub(crate) mod spatial;
pub(crate) mod sqlite;
pub(crate) mod tally;
pub(crate) mod xml;
pub(crate) mod zip;

// The curated entry points most callers want, hoisted to the crate root.
pub use convert::{convert, read_features, reorder_hilbert, write_features, write_features_with, WriteOptions};
pub use crs::{Crs, NamedCrs, WGS84_CRS_CODE};
pub use error::{Error, Result};
pub use feature::{Feature, FeatureCollection};
pub use format::{Format, KmlVersion};
pub use native::{
    FgbColumnNative, FgbNative, GpkgConstraint, GpkgDataColumn, GpkgFeatureNative, GpkgNative,
    KmlFeatureNative, KmlNative, KmzArchive, KmzEntry, Native, NativePayload, ParquetNative,
};
pub use geometry::{Bbox, Geometry, Position};
// `schema` is crate-internal (its column materialization is a writer detail),
// but `FieldDecl` is not: it is a public field of `FeatureCollection`, and a
// route (a) caller editing a schema between `read_features` and
// `write_features` is precisely the case that field exists for — see
// `plans/same-format-fidelity.org` D1.
pub use schema::{DeclType, FieldDecl};

// `json` is crate-internal (its parser and escaping helpers are reader
// details), but `JsonValue` is not: it is the type of `Feature::properties`,
// so a caller that cannot name it cannot *build* a feature — only read one.
// That gap made the route (a) editing story above true of the schema and false
// of the data it describes; see `plans/same-format-fidelity.org` D5.
pub use json::JsonValue;
