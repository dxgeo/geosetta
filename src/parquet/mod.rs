//! A minimal, dependency-free GeoParquet writer.

mod geo;
mod schema;
mod thrift;
mod types;
mod writer;

pub use geo::metadata as geo_metadata;
pub use schema::infer_columns;
pub use writer::write_geoparquet;
