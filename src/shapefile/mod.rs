//! Esri Shapefile — geometry (`.shp`/`.shx`), attributes (`.dbf`), and CRS
//! (`.prj`) split across sibling files sharing one basename.
//!
//! Every prior spoke is either single-file/single-layer or single-file/
//! multi-layer (GeoPackage); a Shapefile is the opposite shape — *one*
//! logical layer split across *several* files. So, like GeoPackage, it gets
//! its own entry points ([`read`]/[`write`]) instead of routing through
//! [`crate::convert`]'s single-buffer `read_features`/`write_features`;
//! locating/writing the sibling files on disk is main.rs's job. See
//! `plans/shapefile.org`.

mod dbf;
mod geometry;
mod reader;
mod writer;

pub use reader::read;
pub use writer::{write, Encoded};
