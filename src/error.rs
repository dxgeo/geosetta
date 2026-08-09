//! Error type for Geosetta. No external crates: a plain enum implementing
//! `std::error::Error` plus a crate-wide `Result` alias.

use std::fmt;

/// The one error type used throughout Geosetta.
#[derive(Debug)]
pub enum Error {
    /// An I/O failure reading input or writing output.
    Io(std::io::Error),
    /// The input could not be parsed as JSON (with a byte offset).
    Json { offset: usize, message: String },
    /// The JSON was valid but not a well-formed GeoJSON document.
    GeoJson(String),
    /// A malformed or unsupported Parquet input (bad footer, unexpected
    /// encoding/codec, truncated page, etc.).
    Parquet(String),
    /// A malformed or unsupported FlatGeobuf input (bad magic, FlatBuffers
    /// structure, geometry, etc.).
    FlatGeobuf(String),
    /// A CLI usage problem (bad/missing arguments, unknown format).
    Usage(String),
    /// Anything that goes wrong while building the Parquet output. Reserved
    /// for encoders that can fail (the current writer is infallible).
    #[allow(dead_code)]
    Convert(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {e}"),
            Error::Json { offset, message } => {
                write!(f, "json parse error at byte {offset}: {message}")
            }
            Error::GeoJson(m) => write!(f, "invalid geojson: {m}"),
            Error::Parquet(m) => write!(f, "invalid parquet: {m}"),
            Error::FlatGeobuf(m) => write!(f, "invalid flatgeobuf: {m}"),
            Error::Usage(m) => write!(f, "{m}"),
            Error::Convert(m) => write!(f, "conversion error: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;
