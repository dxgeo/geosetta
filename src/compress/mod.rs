//! Byte-oriented decompression codecs, each implemented from its specification
//! rather than a crate (keeping the project dependency-free).
//!
//! These are format-agnostic `bytes -> bytes` codecs. Parquet is the current
//! consumer, but nothing here is Parquet-specific, so they live outside the
//! `parquet` module and are reusable by any future format.

pub mod gzip;
pub mod lz4;
pub mod snappy;
pub mod zstd;
