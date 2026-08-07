//! Parquet enum constants used by the writer, as defined in
//! `parquet.thrift`. Each Thrift enum is an `i32` on the wire.
//!
//! The full enum tables are kept for reference and future formats even though
//! the current GeoJSON→GeoParquet path only touches a subset.
#![allow(dead_code)]

/// Physical column types (`Type`).
pub mod ptype {
    pub const BOOLEAN: i32 = 0;
    pub const INT32: i32 = 1;
    pub const INT64: i32 = 2;
    pub const INT96: i32 = 3;
    pub const FLOAT: i32 = 4;
    pub const DOUBLE: i32 = 5;
    pub const BYTE_ARRAY: i32 = 6;
    pub const FIXED_LEN_BYTE_ARRAY: i32 = 7;
}

/// Field repetition (`FieldRepetitionType`).
pub mod repetition {
    pub const REQUIRED: i32 = 0;
    pub const OPTIONAL: i32 = 1;
    pub const REPEATED: i32 = 2;
}

/// Value/level encodings (`Encoding`).
pub mod encoding {
    pub const PLAIN: i32 = 0;
    /// Deprecated dictionary-index encoding; still emitted by e.g. DuckDB.
    pub const PLAIN_DICTIONARY: i32 = 2;
    pub const RLE: i32 = 3;
    /// Modern dictionary-index encoding (same on-wire form as PLAIN_DICTIONARY).
    pub const RLE_DICTIONARY: i32 = 8;
}

/// Compression codecs (`CompressionCodec`).
pub mod codec {
    pub const UNCOMPRESSED: i32 = 0;
    pub const SNAPPY: i32 = 1;
    pub const ZSTD: i32 = 6;
}

/// Page types (`PageType`).
pub mod page {
    pub const DATA_PAGE: i32 = 0;
    pub const DICTIONARY_PAGE: i32 = 2;
    pub const DATA_PAGE_V2: i32 = 3;
}

/// Legacy converted types (`ConvertedType`). Kept for broad reader
/// compatibility alongside the newer logical types.
pub mod converted {
    pub const UTF8: i32 = 0;
}
