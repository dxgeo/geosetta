//! Geosetta — convert between open vector geospatial formats.
//!
//! Every format is routed through a shared, format-neutral feature IR
//! ([`FeatureCollection`]): `read(from) -> FeatureCollection -> write(to)`, so
//! any input format converts to any output format the writers support.
//!
//! The whole crate is standard-library only — every wire format (GeoJSON,
//! GeoParquet, FlatGeobuf, CSV, WKT, GeoPackage, Shapefile) is implemented from
//! its specification, with no third-party dependencies.
//!
//! # Example
//!
//! ```no_run
//! use geosetta::{read_features, write_features, Format};
//!
//! let input = std::fs::read("in.geojson")?;
//! let fc = read_features(Format::GeoJson, &input)?;
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
pub(crate) mod xml;
pub(crate) mod zip;

// The curated entry points most callers want, hoisted to the crate root.
pub use convert::{convert, read_features, reorder_hilbert, write_features};
pub use crs::{Crs, NamedCrs};
pub use error::{Error, Result};
pub use feature::{Feature, FeatureCollection};
pub use format::Format;
pub use geometry::{Bbox, Geometry, Position};
