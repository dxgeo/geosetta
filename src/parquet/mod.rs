//! A minimal, dependency-free GeoParquet writer.

mod geo;
mod reader;
mod schema;
mod snappy;
mod thrift;
mod types;
mod writer;

pub use geo::metadata as geo_metadata;
pub use reader::read_geoparquet;
pub use schema::infer_columns;
pub use writer::write_geoparquet;
