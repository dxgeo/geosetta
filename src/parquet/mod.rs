//! A minimal, dependency-free GeoParquet reader and writer. Compression codecs
//! live in the top-level [`crate::compress`] module.

mod geo;
mod reader;
mod thrift;
mod types;
mod writer;

pub use geo::metadata as geo_metadata;
pub use reader::read_geoparquet;
pub use writer::write_geoparquet;
