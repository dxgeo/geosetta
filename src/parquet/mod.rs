//! A minimal, dependency-free GeoParquet writer.

mod geo;
mod gzip;
mod lz4;
mod reader;
mod schema;
mod snappy;
mod thrift;
mod types;
mod writer;
mod zstd;

pub use geo::metadata as geo_metadata;
pub use reader::read_geoparquet;
pub use schema::infer_columns;
pub use writer::write_geoparquet;
