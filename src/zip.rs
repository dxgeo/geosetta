//! A minimal ZIP container codec. Not KML-specific — format-agnostic
//! bytes-in/bytes-out, like `compress/`, so it's a reasonable building block
//! for any future ZIP-based format; `.kmz` is its first consumer.
//!
//! Implements just enough of the PKWARE APPNOTE to round-trip archives real
//! tools produce: the End Of Central Directory record, central directory file
//! headers, and local file headers. No zip64, no encryption, no multi-disk
//! archives, no data descriptors — a `.kmz` producer uses none of those.
//!
//! Read: stored (method 0) entries pass through as-is; deflate (method 8)
//! entries decompress via the existing [`crate::compress::gzip::decompress`],
//! whose `strip_container` already treats a bare DEFLATE stream (no gzip/zlib
//! wrapper) as the default case — exactly what a ZIP method-8 entry is, so no
//! new decompression code is needed here. Each entry's CRC-32 is checked
//! against the central directory's recorded value.
//!
//! Write: stored-only. There is no DEFLATE *encoder* in the crate (`gzip.rs`
//! is decoder-only) — a deliberate, deferred trade-off, not an oversight; see
//! `plans/kml.org`'s open questions.

use crate::compress::gzip;
use crate::error::{Error, Result};

fn bad(message: &str) -> Error {
    Error::Convert(format!("zip: {message}"))
}

fn err<T>(message: &str) -> Result<T> {
    Err(bad(message))
}

const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// One archive entry: its stored name and decompressed bytes.
pub(crate) struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
}

/// Read and decompress every entry in a ZIP archive, via its central
/// directory (the authoritative index — entries aren't decoded by scanning
/// local headers sequentially).
pub(crate) fn read(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    let (total_entries, cd_offset) = find_eocd(bytes)?;
    let mut entries = Vec::with_capacity(total_entries as usize);
    let mut pos = cd_offset as usize;
    for _ in 0..total_entries {
        let (entry, next) = read_central_entry(bytes, pos)?;
        entries.push(entry);
        pos = next;
    }
    Ok(entries)
}

/// Write a ZIP archive containing `entries`, each stored (method 0,
/// uncompressed) — see the module doc for why there's no deflate encoder.
pub(crate) fn write(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut local_info = Vec::with_capacity(entries.len()); // (crc, local offset)

    for (name, data) in entries {
        let crc = crc32(data);
        local_info.push((crc, out.len() as u32));
        write_local_header(&mut out, name, data.len(), crc);
        out.extend_from_slice(data);
    }

    let cd_start = out.len() as u32;
    let mut central = Vec::new();
    for ((name, data), &(crc, local_offset)) in entries.iter().zip(&local_info) {
        write_central_header(&mut central, name, data.len(), crc, local_offset);
    }
    out.extend_from_slice(&central);
    write_eocd(&mut out, entries.len() as u16, central.len() as u32, cd_start);
    out
}

/// Locate the End Of Central Directory record by scanning backward for its
/// signature — a trailing comment field, legal per spec, can push it earlier
/// than the fixed 22-byte tail (the same "trailing structure, scan from the
/// end" shape as locating GeoParquet's Thrift footer). Returns `(total
/// entries, central directory offset)`.
fn find_eocd(bytes: &[u8]) -> Result<(u16, u32)> {
    if bytes.len() < 22 {
        return err("too short to be a zip archive");
    }
    // A comment is at most 65535 bytes; never search further back than that.
    let search_from = bytes.len().saturating_sub(22 + 65535);
    let sig = EOCD_SIG.to_le_bytes();
    let pos = bytes[search_from..].windows(4).rposition(|w| w == sig).map(|i| search_from + i);
    let Some(pos) = pos else {
        return err("no end-of-central-directory record found");
    };
    let total_entries = u16_le(bytes, pos + 10)?;
    let cd_offset = u32_le(bytes, pos + 16)?;
    Ok((total_entries, cd_offset))
}

/// Read one central directory file header at `pos`: decompress its data
/// (located via the header's local-file offset) and verify its CRC-32.
/// Returns the entry plus the position of the next central directory header.
fn read_central_entry(bytes: &[u8], pos: usize) -> Result<(ZipEntry, usize)> {
    if u32_le(bytes, pos)? != CENTRAL_SIG {
        return err("bad central directory file header signature");
    }
    let method = u16_le(bytes, pos + 10)?;
    let crc = u32_le(bytes, pos + 16)?;
    let compressed_size = u32_le(bytes, pos + 20)? as usize;
    let uncompressed_size = u32_le(bytes, pos + 24)? as usize;
    let name_len = u16_le(bytes, pos + 28)? as usize;
    let extra_len = u16_le(bytes, pos + 30)? as usize;
    let comment_len = u16_le(bytes, pos + 32)? as usize;
    let local_offset = u32_le(bytes, pos + 42)? as usize;

    let name_start = pos + 46;
    let name_bytes = bytes
        .get(name_start..name_start + name_len)
        .ok_or_else(|| bad("truncated central directory file name"))?;
    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| bad("central directory file name is not valid utf-8"))?
        .to_string();

    let data = read_local_data(bytes, local_offset, method, compressed_size, uncompressed_size)?;
    if crc32(&data) != crc {
        return err(&format!("crc-32 mismatch for entry \"{name}\" (corrupt archive)"));
    }

    let next = name_start + name_len + extra_len + comment_len;
    Ok((ZipEntry { name, data }, next))
}

/// An entry's data at its local file header offset, decompressed per
/// `method`. The local header's own name/extra-field lengths (not the
/// central directory's) locate the data start — real-world writers keep them
/// identical, but the spec doesn't require it.
fn read_local_data(
    bytes: &[u8],
    local_offset: usize,
    method: u16,
    compressed_size: usize,
    uncompressed_size: usize,
) -> Result<Vec<u8>> {
    if u32_le(bytes, local_offset)? != LOCAL_SIG {
        return err("bad local file header signature");
    }
    let name_len = u16_le(bytes, local_offset + 26)? as usize;
    let extra_len = u16_le(bytes, local_offset + 28)? as usize;
    let data_start = local_offset + 30 + name_len + extra_len;
    let compressed = bytes
        .get(data_start..data_start + compressed_size)
        .ok_or_else(|| bad("truncated entry data"))?;
    match method {
        METHOD_STORED => Ok(compressed.to_vec()),
        METHOD_DEFLATE => gzip::decompress(compressed, uncompressed_size)
            .map_err(|_| bad("entry failed to inflate (corrupt or unsupported deflate stream)")),
        other => err(&format!("unsupported zip compression method {other} (only stored/deflate)")),
    }
}

fn write_local_header(out: &mut Vec<u8>, name: &str, len: usize, crc: u32) {
    out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract (2.0)
    out.extend_from_slice(&0u16.to_le_bytes()); // general purpose bit flag
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // last mod file time
    out.extend_from_slice(&0u16.to_le_bytes()); // last mod file date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes()); // compressed size
    out.extend_from_slice(&(len as u32).to_le_bytes()); // uncompressed size
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    out.extend_from_slice(name.as_bytes());
}

fn write_central_header(out: &mut Vec<u8>, name: &str, len: usize, crc: u32, local_offset: u32) {
    out.extend_from_slice(&CENTRAL_SIG.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version made by
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract
    out.extend_from_slice(&0u16.to_le_bytes()); // general purpose bit flag
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // last mod file time
    out.extend_from_slice(&0u16.to_le_bytes()); // last mod file date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes()); // compressed size
    out.extend_from_slice(&(len as u32).to_le_bytes()); // uncompressed size
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    out.extend_from_slice(&0u16.to_le_bytes()); // file comment length
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number start
    out.extend_from_slice(&0u16.to_le_bytes()); // internal file attributes
    out.extend_from_slice(&0u32.to_le_bytes()); // external file attributes
    out.extend_from_slice(&local_offset.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
}

fn write_eocd(out: &mut Vec<u8>, num_entries: u16, cd_size: u32, cd_offset: u32) {
    out.extend_from_slice(&EOCD_SIG.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // number of this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk where central directory starts
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length
}

fn u16_le(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(b.get(at..at + 2).ok_or_else(|| bad("truncated"))?.try_into().unwrap()))
}

fn u32_le(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(b.get(at..at + 4).ok_or_else(|| bad("truncated"))?.try_into().unwrap()))
}

/// The standard reflected CRC-32 (IEEE 802.3 / "CRC-32/ISO-HDLC") ZIP entry
/// headers use — the same algorithm as gzip's and PNG's — from a 256-entry
/// table built at compile time.
fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = build_crc32_table();
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        let idx = ((crc ^ b as u32) & 0xFF) as usize;
        crc = TABLE[idx] ^ (crc >> 8);
    }
    !crc
}

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_standard_check_value() {
        // The canonical CRC-32/ISO-HDLC (zip/gzip/png) check value for "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn write_then_read_round_trips_multiple_entries() {
        let entries: [(&str, &[u8]); 3] =
            [("doc.kml", b"<kml/>"), ("empty.txt", b""), ("images/icon.png", b"\x89PNG\r\n"),];
        let bytes = write(&entries);
        let read_back = read(&bytes).unwrap();
        assert_eq!(read_back.len(), entries.len());
        for ((name, data), entry) in entries.iter().zip(read_back.iter()) {
            assert_eq!(&entry.name, name);
            assert_eq!(&entry.data, data);
        }
    }

    #[test]
    fn rejects_a_truncated_archive() {
        assert!(read(b"not a zip file").is_err());
        assert!(read(&write(&[("a", b"x")])[..10]).is_err());
    }

    #[test]
    fn detects_a_corrupted_entry_via_crc() {
        let mut bytes = write(&[("a", b"hello")]);
        // Flip a byte inside the (stored) entry data, after the local header.
        let corrupt_at = bytes.len() - 3;
        bytes[corrupt_at] ^= 0xFF;
        assert!(read(&bytes).is_err());
    }

    #[test]
    fn reads_a_real_unzip_produced_deflate_entry() {
        // A real archive built by the system `zip` tool (macOS/Info-ZIP),
        // deflate by default — exercises the METHOD_DEFLATE path, which our
        // own `write` never produces (stored-only), so a real external
        // archive is the only way to cover it.
        let bytes = include_bytes!("../tests/fixtures/sample.zip");
        let entries = read(bytes).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "doc.kml");
        assert_eq!(entries[0].data, include_bytes!("../tests/fixtures/sample.kml"));
    }
}
