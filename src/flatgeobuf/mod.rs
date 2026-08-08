//! FlatGeobuf (FGB) reader, built on the dependency-free [`crate::flatbuffers`]
//! reader. Decodes an FGB file into the shared [`FeatureCollection`] IR.
//!
//! Layout: an 8-byte magic, then `[u32 len][Header]` (a FlatBuffers table), an
//! optional packed Hilbert R-tree spatial index (skipped here — a full scan
//! doesn't need it), then a sequence of `[u32 len][Feature]` tables. See
//! `plans/flatgeobuf.org`. The `.fbs` schema field indices are encoded below.

mod reader;

pub use reader::read;
