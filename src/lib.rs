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
//! use geosetta::{convert_with, Format, WriteOptions};
//!
//! let input = std::fs::read("in.geojson")?;
//! let out = convert_with(Format::GeoJson, Format::Parquet, &input, WriteOptions::default())?;
//! for w in &out.warnings {
//!     eprintln!("warning: {w}"); // what the conversion could not keep, if anything
//! }
//! std::fs::write("out.parquet", out.bytes)?;
//! # Ok::<(), geosetta::Error>(())
//! ```
//!
//! Every other conversion — a GeoPackage's layers, a Shapefile's sibling
//! files, a CRS override, a Hilbert sort — goes through [`pipeline`], which is
//! what [`convert_with`] and the `geosetta` binary both run. Its
//! [`pipeline::prepare`] does every step before the encode in the order the
//! checks need, and reports warnings and progress through one callback:
//!
//! ```no_run
//! use geosetta::pipeline::{self, ConvertOptions, Event, Source};
//! use geosetta::Format;
//!
//! let input = std::fs::read("in.gpkg")?;
//! let mut events = |e: Event<'_>| {
//!     if let Event::Warning(w) = e {
//!         eprintln!("warning: {w}");
//!     }
//! };
//! let existing = std::fs::read("out.gpkg").ok();
//! let prepared = pipeline::prepare(Format::Gpkg, Format::Gpkg, Source::Bytes(&input), ConvertOptions::default(), &mut events)?;
//! std::fs::write("out.gpkg", prepared.into_geopackage(existing.as_deref(), &mut events)?)?;
//! # Ok::<(), geosetta::Error>(())
//! ```
//!
//! The stages are public too: [`read_features`] takes a sink for what the read
//! drops, [`conversion_warnings`] predicts what a write will, and
//! [`write_features`] encodes — warning about nothing, so a caller composing
//! the stages runs the check between them.
//!
//! Conversions are byte-in / byte-out; file I/O is left to the caller. The
//! `geosetta` binary is that caller: it moves bytes between files or stdio and
//! the pipeline, and presents what the pipeline reports
//! (`plans/library-pipeline.org`). GeoPackage is a multi-layer container, and
//! Shapefile the opposite shape — one logical layer split across sibling files
//! (`.shp`/`.shx`/`.dbf`/`.prj`) — so each also has its own entry points
//! ([`geopackage::read_layers`] / [`geopackage::write_layers`],
//! [`shapefile::read`] / [`shapefile::write`]) for a caller who wants the
//! format alone.

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
pub mod pipeline;
pub mod shapefile;

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
pub(crate) mod table;
pub(crate) mod sqlite;
pub(crate) mod tally;
pub(crate) mod xml;
pub(crate) mod zip;

// The curated entry points most callers want, hoisted to the crate root.
pub use convert::{
    conversion_warnings, convert, convert_with, read_features, read_features_with, reorder_hilbert, write_features,
    write_features_with, Converted, ReadOptions, WriteOptions,
};
pub use table::{ColumnRef, ColumnValues, GeometryRef, Layout, Members, Parts, Rings, Run, Shape, Value};
pub use crs::{Crs, NamedCrs, WGS84_CRS_CODE};
pub use error::{Error, Result};
pub use feature::{Feature, FeatureCollection, FeatureProperties, FeatureRef, FeatureRefs};
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
