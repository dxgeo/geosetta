//! A minimal ZIP container codec. Not KML-specific — format-agnostic
//! bytes-in/bytes-out, like `compress/`, so it's a reasonable building block
//! for any future ZIP-based format; `.kmz` is its first consumer.
//!
//! Implements just enough of the PKWARE APPNOTE to round-trip archives real
//! tools produce: the End Of Central Directory record, central directory file
//! headers, and local file headers. No zip64, no encryption, no multi-disk
//! archives, no data descriptors — a `.kmz` producer uses none of those.
//!
//! *Zip64 is refused, never mistaken.* Past 65,535 entries or 4 GiB a zip
//! needs zip64's records; this codec has none. A read that meets them — the
//! zip64 locator, or a field holding its `0xFFFF…` placeholder — is refused
//! by name, and so is a central directory with more entries than its end
//! record counts, which is what a writer that wraps the 16-bit count leaves.
//! A write past either limit is refused rather than written with its sizes
//! truncated. A zip64 field in a *local* header alone is read: the sizes come
//! from the central directory. `plans/same-format-fidelity.org` M3c records
//! why zip64 itself is deferred.
//!
//! Read: stored (method 0) entries pass through as-is; deflate (method 8)
//! entries decompress via the existing [`crate::compress::gzip::decompress`],
//! whose `strip_container` already treats a bare DEFLATE stream (no gzip/zlib
//! wrapper) as the default case — exactly what a ZIP method-8 entry is, so no
//! new decompression code is needed here. Each entry's CRC-32 is checked
//! against the central directory's recorded value.
//!
//! Write: stored-only. There is no DEFLATE *encoder* in the crate (`gzip.rs`
//! is decoder-only) — a deliberate, deferred trade-off, not an oversight;
//! `plans/compression-encoders.org` plans the encoder, `.kmz` its consumer.

use crate::compress::gzip;
use crate::error::{Error, Result};

fn bad(message: &str) -> Error {
    Error::Convert(format!("zip: {message}"))
}

fn err<T>(message: &str) -> Result<T> {
    Err(bad(message))
}

const EOCD_SIG: u32 = 0x0605_4b50;
const ZIP64_LOCATOR_SIG: u32 = 0x0706_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;

const METHOD_STORED: u16 = 0;
const METHOD_DEFLATE: u16 = 8;

/// The largest entry count, and size or offset, a zip without zip64 states.
/// One below each field's maximum, which is zip64's placeholder.
const MAX_ENTRIES: usize = 0xFFFE;
const MAX_U32: usize = 0xFFFF_FFFE;

/// General-purpose bit 11, the language encoding flag: set, the entry's name
/// is UTF-8; clear, it is IBM CP437 (APPNOTE 6.3.0 § 4.4.4, Appendix D).
const FLAG_UTF8_NAME: u16 = 1 << 11;

/// One archive entry: its stored name, decompressed bytes, and last
/// modification time as the archive recorded it.
pub(crate) struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub modified: DosTime,
}

/// An entry's last modification time in MS-DOS form, as the local and central
/// headers both hold it: the date in the high 16 bits, the time in the low.
/// Kept as recorded, never interpreted, so a rewritten archive says what the
/// original did. Zero, the writer's default, reads as no time at all.
pub(crate) type DosTime = u32;

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
    if u32_le(bytes, pos).is_ok_and(|sig| sig == CENTRAL_SIG) {
        return err(&format!(
            "the archive's end record counts {total_entries} entries but its central directory holds more \
             (past 65,535 entries, written without zip64); not read rather than read in part"
        ));
    }
    Ok(entries)
}

fn zip64() -> Error {
    bad("zip64 archive (past 65,535 entries or 4 GiB); zip64 is not supported")
}

/// Write a ZIP archive containing `entries`, each stored (method 0,
/// uncompressed) — see the module doc for why there's no deflate encoder —
/// with no modification time.
pub(crate) fn write(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let timed: Vec<(&str, &[u8], DosTime)> = entries.iter().map(|&(name, data)| (name, data, 0)).collect();
    write_timed(&timed)
}

/// [`write`], each entry with the modification time given. Refused past
/// what a zip without zip64 holds (see the module doc).
pub(crate) fn write_timed(entries: &[(&str, &[u8], DosTime)]) -> Result<Vec<u8>> {
    let count = fits(entries.len(), MAX_ENTRIES, || format!("{} entries", entries.len()))?;
    let mut out = Vec::new();
    let mut local_info = Vec::with_capacity(entries.len()); // (crc, local offset)

    for &(name, data, modified) in entries {
        fits(name.len(), 0xFFFF, || format!("entry name of {} bytes", name.len()))?;
        fits(data.len(), MAX_U32, || format!("entry \"{name}\" of {} bytes", data.len()))?;
        let offset = fits(out.len(), MAX_U32, || format!("entry \"{name}\" starting past 4 GiB"))?;
        let crc = crc32(data);
        local_info.push((crc, offset as u32));
        write_local_header(&mut out, name, data.len(), crc, modified);
        out.extend_from_slice(data);
    }

    let cd_start = fits(out.len(), MAX_U32, || "a central directory starting past 4 GiB".into())?;
    let mut central = Vec::new();
    for (&(name, data, modified), &(crc, local_offset)) in entries.iter().zip(&local_info) {
        write_central_header(&mut central, name, data.len(), crc, modified, local_offset);
    }
    let cd_size = fits(central.len(), MAX_U32, || format!("a central directory of {} bytes", central.len()))?;
    out.extend_from_slice(&central);
    write_eocd(&mut out, count as u16, cd_size as u32, cd_start as u32);
    Ok(out)
}

/// `n`, if a zip without zip64 can state it (at most `max`); else refused,
/// naming what overflowed.
fn fits(n: usize, max: usize, what: impl FnOnce() -> String) -> Result<usize> {
    if n <= max {
        return Ok(n);
    }
    err(&format!("{} is past what a zip without zip64 holds, and zip64 is not written", what()))
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
    let cd_size = u32_le(bytes, pos + 12)?;
    let cd_offset = u32_le(bytes, pos + 16)?;
    let locator = pos.checked_sub(20).is_some_and(|at| u32_le(bytes, at).is_ok_and(|sig| sig == ZIP64_LOCATOR_SIG));
    if locator || cd_size == u32::MAX || cd_offset == u32::MAX {
        return Err(zip64());
    }
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
    let modified = u32_le(bytes, pos + 12)?;
    let crc = u32_le(bytes, pos + 16)?;
    let compressed_size = u32_le(bytes, pos + 20)? as usize;
    let uncompressed_size = u32_le(bytes, pos + 24)? as usize;
    let name_len = u16_le(bytes, pos + 28)? as usize;
    let extra_len = u16_le(bytes, pos + 30)? as usize;
    let comment_len = u16_le(bytes, pos + 32)? as usize;
    let local_offset = u32_le(bytes, pos + 42)? as usize;
    if [compressed_size, uncompressed_size, local_offset].contains(&(u32::MAX as usize)) {
        return Err(zip64());
    }

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
    Ok((ZipEntry { name, data, modified }, next))
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

/// The general-purpose flags a written entry declares. Names are written as
/// UTF-8, so a non-ASCII one sets bit 11; an ASCII name reads the same under
/// CP437 and leaves the flags clear, as every archive written before did.
fn name_flags(name: &str) -> u16 {
    if name.is_ascii() { 0 } else { FLAG_UTF8_NAME }
}

fn write_local_header(out: &mut Vec<u8>, name: &str, len: usize, crc: u32, modified: DosTime) {
    out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract (2.0)
    out.extend_from_slice(&name_flags(name).to_le_bytes()); // general purpose bit flag
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&modified.to_le_bytes()); // last mod file time, then date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes()); // compressed size
    out.extend_from_slice(&(len as u32).to_le_bytes()); // uncompressed size
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    out.extend_from_slice(name.as_bytes());
}

fn write_central_header(out: &mut Vec<u8>, name: &str, len: usize, crc: u32, modified: DosTime, local_offset: u32) {
    out.extend_from_slice(&CENTRAL_SIG.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version made by
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract
    out.extend_from_slice(&name_flags(name).to_le_bytes()); // general purpose bit flag
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&modified.to_le_bytes()); // last mod file time, then date
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
        let bytes = write(&entries).unwrap();
        let read_back = read(&bytes).unwrap();
        assert_eq!(read_back.len(), entries.len());
        for ((name, data), entry) in entries.iter().zip(read_back.iter()) {
            assert_eq!(&entry.name, name);
            assert_eq!(&entry.data, data);
        }
    }

    #[test]
    fn an_entrys_modification_time_round_trips() {
        // 2026-10-28 12:34:56: date ((2026-1980)<<9 | 10<<5 | 28), time (12<<11 | 34<<5 | 56/2).
        let modified: DosTime = (((46 << 9) | (10 << 5) | 28) << 16) | ((12 << 11) | (34 << 5) | 28);
        let bytes = write_timed(&[("a.png", b"x", modified), ("b", b"", 0)]).unwrap();
        let back = read(&bytes).unwrap();
        assert_eq!((back[0].modified, back[1].modified), (modified, 0));
    }

    #[test]
    fn reads_the_time_a_real_archive_recorded() {
        let entries = read(include_bytes!("../tests/fixtures/sample.zip")).unwrap();
        assert_ne!(entries[0].modified, 0, "Info-ZIP records a time");
    }

    #[test]
    fn a_non_ascii_name_is_declared_utf8_in_both_headers() {
        let name = "images/Zürich.png";
        let bytes = write(&[(name, b"x")]).unwrap();
        let local_flags = u16_le(&bytes, 6).unwrap();
        let central = bytes.windows(4).position(|w| w == CENTRAL_SIG.to_le_bytes()).unwrap();
        let central_flags = u16_le(&bytes, central + 8).unwrap();
        assert_eq!((local_flags, central_flags), (FLAG_UTF8_NAME, FLAG_UTF8_NAME));
        assert_eq!(read(&bytes).unwrap()[0].name, name);
    }

    #[test]
    fn an_ascii_name_leaves_the_flags_clear() {
        let bytes = write(&[("doc.kml", b"<kml/>")]).unwrap();
        let central = bytes.windows(4).position(|w| w == CENTRAL_SIG.to_le_bytes()).unwrap();
        assert_eq!((u16_le(&bytes, 6).unwrap(), u16_le(&bytes, central + 8).unwrap()), (0, 0));
    }

    /// Where the end record of an archive `write` made starts.
    fn eocd(bytes: &[u8]) -> usize {
        bytes.len() - 22
    }

    #[test]
    fn a_wrapped_entry_count_is_refused_not_read_in_part() {
        // A writer without zip64 records 65,537 entries as 1; two stand in.
        let mut bytes = write(&[("a", b"x"), ("b", b"y")]).unwrap();
        let at = eocd(&bytes);
        bytes[at + 8..at + 12].copy_from_slice(&[1, 0, 1, 0]);
        let e = read(&bytes).err().unwrap().to_string();
        assert!(e.contains("counts 1 entries but its central directory holds more"), "{e}");
    }

    #[test]
    fn zip64_is_refused_by_name() {
        let zip64 = |bytes: Vec<u8>| read(&bytes).err().unwrap().to_string().contains("zip64 archive");
        let bytes = write(&[("a", b"x")]).unwrap();
        // The end record's offset as zip64's placeholder.
        let mut placeholder = bytes.clone();
        let at = eocd(&placeholder);
        placeholder[at + 16..at + 20].copy_from_slice(&[0xFF; 4]);
        assert!(zip64(placeholder));
        // A zip64 locator before the end record.
        let at = eocd(&bytes);
        let mut located = bytes[..at].to_vec();
        located.extend_from_slice(&ZIP64_LOCATOR_SIG.to_le_bytes());
        located.extend_from_slice(&[0; 16]);
        located.extend_from_slice(&bytes[at..]);
        assert!(zip64(located));
        // A central header's size as the placeholder.
        let mut entry = bytes.clone();
        let central = entry.windows(4).position(|w| w == CENTRAL_SIG.to_le_bytes()).unwrap();
        entry[central + 20..central + 24].copy_from_slice(&[0xFF; 4]);
        assert!(zip64(entry));
    }

    #[test]
    fn a_zip64_field_in_a_local_header_alone_is_read() {
        // Python's `force_zip64`: placeholders and a 0x0001 extra field
        // locally, the sizes in the central directory.
        let entries = read(include_bytes!("../tests/fixtures/local_zip64.kmz")).unwrap();
        assert_eq!(entries[0].data, include_bytes!("../tests/fixtures/sample.kml"));
    }

    #[test]
    fn a_write_past_the_limits_is_refused_not_truncated() {
        let long = "n".repeat(0x1_0000);
        let e = write(&[(&long, b"")]).err().unwrap().to_string();
        assert!(e.contains("entry name of 65536 bytes is past what a zip without zip64 holds"), "{e}");
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| i.to_string()).collect();
        let entries: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b""[..])).collect();
        let e = write(&entries).err().unwrap().to_string();
        assert!(e.contains("65535 entries is past"), "{e}");
        assert!(write(&entries[1..]).is_ok(), "65,534 entries fit");
    }

    #[test]
    fn rejects_a_truncated_archive() {
        assert!(read(b"not a zip file").is_err());
        assert!(read(&write(&[("a", b"x")]).unwrap()[..10]).is_err());
    }

    #[test]
    fn detects_a_corrupted_entry_via_crc() {
        let mut bytes = write(&[("a", b"hello")]).unwrap();
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
