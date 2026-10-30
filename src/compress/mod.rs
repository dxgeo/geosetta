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

/// How many output bytes to reserve up front for a block that declares
/// `expected` bytes from `input_len` bytes of input: the declaration, but no
/// more than the codec could produce from that input at `max_ratio`.
///
/// The declared size comes from the file (a Parquet page header, a zip entry,
/// a Snappy length prefix), so reserving it outright let a few bytes of input
/// request gigabytes: an allocation failure on wasm32, and for Snappy's
/// 64-bit prefix a native abort. Past this cap the output still grows with
/// the data actually decoded, and every codec checks its final length against
/// the declaration.
pub(crate) fn reserve(expected: usize, input_len: usize, max_ratio: usize) -> usize {
    expected.min(input_len.saturating_mul(max_ratio))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_is_the_declaration_capped_by_the_input() {
        assert_eq!(reserve(100, 10, 22), 100);
        assert_eq!(reserve(1 << 30, 10, 22), 220);
        assert_eq!(reserve(usize::MAX, usize::MAX, 32768), usize::MAX);
    }

    #[test]
    fn a_huge_declared_size_is_an_error_not_an_allocation_failure() {
        // A few bytes of input declaring the largest size a file can state
        // for each codec. Each used to reserve the declaration outright: an
        // allocation failure on wasm32 (gzip, LZ4, zstd, at Parquet's
        // i32::MAX page size), and for Snappy's 64-bit prefix a native abort.
        let page_max = i32::MAX as usize;
        assert!(gzip::decompress(&[0x03, 0x00], page_max).is_err());
        assert!(lz4::decompress(&[0x00], page_max).is_err());
        assert!(zstd::decompress(&[0x28, 0xb5, 0x2f, 0xfd], page_max).is_err());
        // Snappy's own length prefix, as a varint: 2^62.
        assert!(snappy::decompress(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x40]).is_none());
    }
}
