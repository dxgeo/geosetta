//! GeoPackage (GPKG) reader, built on the from-scratch `crate::sqlite` reader.
//!
//! A `.gpkg` is a SQLite database that can hold several feature tables
//! ("layers"). Reading fans out over all of them: [`read_layers`] returns each
//! layer's name and [`crate::feature::FeatureCollection`]. Geometry is stored as
//! GeoPackage Binary — a small header wrapping standard WKB, which we already
//! decode. See `plans/geopackage.org`.

mod reader;
mod rtree;
mod writer;

pub use reader::read_layers;
pub use writer::write_layers;
