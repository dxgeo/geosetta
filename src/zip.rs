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

/// One archive entry: its stored name, decompressed bytes, and what its
/// headers say besides.
pub(crate) struct ZipEntry {
    pub name: String,
    pub data: Vec<u8>,
    pub meta: EntryMeta,
}

/// An archive: its entries in central-directory order, and its comment.
pub(crate) struct Archive {
    pub entries: Vec<ZipEntry>,
    pub comment: Vec<u8>,
}

/// What an entry's headers record beyond its name, data and the fields the
/// writer derives (flags, method, sizes, CRC, offsets): kept as read, never
/// interpreted, so a rewritten archive says what the original did.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EntryMeta {
    pub modified: DosTime,
    /// "Version made by": the host in the high byte, which says how
    /// `external_attrs` reads (0 MS-DOS, 3 Unix), the spec version in the low.
    pub made_by: u16,
    /// Bit 0 says the entry is text.
    pub internal_attrs: u16,
    /// The host's attributes: under Unix, the file mode in the high 16 bits
    /// and MS-DOS attributes in the low byte.
    pub external_attrs: u32,
    /// The extra fields, local header's and central directory's apart: a
    /// producer may write them differently (Info-ZIP's local timestamp holds
    /// more than its central one). Zip64's field is not kept — see
    /// [`without_zip64`].
    pub local_extra: Vec<u8>,
    pub central_extra: Vec<u8>,
    pub comment: Vec<u8>,
}

/// Unix, spec version 2.0: what an entry this codec creates says made it.
const MADE_BY_UNIX: u16 = (3 << 8) | 20;

impl EntryMeta {
    /// An entry this codec creates, rather than carries: no time, a Unix
    /// file of mode `0644` (a directory, named with a trailing `/`, `0755`
    /// and MS-DOS's directory bit), as Info-ZIP and Python write one.
    pub(crate) fn created(name: &str) -> EntryMeta {
        let external_attrs = if name.ends_with('/') { (0o040_755 << 16) | 0x10 } else { 0o100_644 << 16 };
        EntryMeta {
            modified: 0,
            made_by: MADE_BY_UNIX,
            internal_attrs: 0,
            external_attrs,
            local_extra: Vec::new(),
            central_extra: Vec::new(),
            comment: Vec::new(),
        }
    }
}

/// An entry's last modification time in MS-DOS form, as the local and central
/// headers both hold it: the date in the high 16 bits, the time in the low.
/// Kept as recorded, never interpreted, so a rewritten archive says what the
/// original did. Zero, the writer's default, reads as no time at all.
pub(crate) type DosTime = u32;

/// Read and decompress every entry in a ZIP archive, via its central
/// directory (the authoritative index — entries aren't decoded by scanning
/// local headers sequentially).
#[cfg(test)]
pub(crate) fn read(bytes: &[u8]) -> Result<Vec<ZipEntry>> {
    Ok(read_archive(bytes)?.entries)
}

/// [`read`], with the archive's comment.
pub(crate) fn read_archive(bytes: &[u8]) -> Result<Archive> {
    let (total_entries, cd_offset, comment) = find_eocd(bytes)?;
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
    Ok(Archive { entries, comment: comment.to_vec() })
}

fn zip64() -> Error {
    bad("zip64 archive (past 65,535 entries or 4 GiB); zip64 is not supported")
}

/// Write a ZIP archive containing `entries`, each stored (method 0,
/// uncompressed) — see the module doc for why there's no deflate encoder —
/// and each as this codec creates one ([`EntryMeta::created`]).
pub(crate) fn write(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    let metas: Vec<EntryMeta> = entries.iter().map(|&(name, _)| EntryMeta::created(name)).collect();
    let entries: Vec<_> = entries.iter().zip(&metas).map(|(&(name, data), meta)| (name, data, meta)).collect();
    write_archive(&entries, b"")
}

/// [`write`], each entry with the metadata given, and the archive with
/// `comment`. Refused past what a zip without zip64 holds (see the module
/// doc).
pub(crate) fn write_archive(entries: &[(&str, &[u8], &EntryMeta)], comment: &[u8]) -> Result<Vec<u8>> {
    let count = fits(entries.len(), MAX_ENTRIES, || format!("{} entries", entries.len()))?;
    fits(comment.len(), 0xFFFF, || format!("an archive comment of {} bytes", comment.len()))?;
    let mut out = Vec::new();
    let mut local_info = Vec::with_capacity(entries.len()); // (crc, local offset)

    for &(name, data, meta) in entries {
        fits(name.len(), 0xFFFF, || format!("entry name of {} bytes", name.len()))?;
        for (field, len) in [("local extra field", meta.local_extra.len()), ("central extra field", meta.central_extra.len()), ("comment", meta.comment.len())] {
            fits(len, 0xFFFF, || format!("entry \"{name}\"'s {field} of {len} bytes"))?;
        }
        fits(data.len(), MAX_U32, || format!("entry \"{name}\" of {} bytes", data.len()))?;
        let offset = fits(out.len(), MAX_U32, || format!("entry \"{name}\" starting past 4 GiB"))?;
        let crc = crc32(data);
        local_info.push((crc, offset as u32));
        write_local_header(&mut out, name, data.len(), crc, meta);
        out.extend_from_slice(data);
    }

    let cd_start = fits(out.len(), MAX_U32, || "a central directory starting past 4 GiB".into())?;
    let mut central = Vec::new();
    for (&(name, data, meta), &(crc, local_offset)) in entries.iter().zip(&local_info) {
        write_central_header(&mut central, name, data.len(), crc, meta, local_offset);
    }
    let cd_size = fits(central.len(), MAX_U32, || format!("a central directory of {} bytes", central.len()))?;
    out.extend_from_slice(&central);
    write_eocd(&mut out, count as u16, cd_size as u32, cd_start as u32, comment);
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
/// end" shape as locating GeoParquet's Thrift footer). The record is the one
/// whose comment runs exactly to the end, so a comment that happens to hold
/// the signature is not mistaken for it. Returns `(total entries, central
/// directory offset, comment)`.
fn find_eocd(bytes: &[u8]) -> Result<(u16, u32, &[u8])> {
    if bytes.len() < 22 {
        return err("too short to be a zip archive");
    }
    // A comment is at most 65535 bytes; never search further back than that.
    let search_from = bytes.len().saturating_sub(22 + 65535);
    let sig = EOCD_SIG.to_le_bytes();
    let ends_the_archive = |at: usize| u16_le(bytes, at + 20).is_ok_and(|len| at + 22 + len as usize == bytes.len());
    let pos = (search_from..=bytes.len() - 22).rev().find(|&at| bytes[at..at + 4] == sig && ends_the_archive(at));
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
    Ok((total_entries, cd_offset, &bytes[pos + 22..]))
}

/// Read one central directory file header at `pos`: decompress its data
/// (located via the header's local-file offset) and verify its CRC-32.
/// Returns the entry plus the position of the next central directory header.
fn read_central_entry(bytes: &[u8], pos: usize) -> Result<(ZipEntry, usize)> {
    if u32_le(bytes, pos)? != CENTRAL_SIG {
        return err("bad central directory file header signature");
    }
    let made_by = u16_le(bytes, pos + 4)?;
    let method = u16_le(bytes, pos + 10)?;
    let modified = u32_le(bytes, pos + 12)?;
    let crc = u32_le(bytes, pos + 16)?;
    let compressed_size = u32_le(bytes, pos + 20)? as usize;
    let uncompressed_size = u32_le(bytes, pos + 24)? as usize;
    let name_len = u16_le(bytes, pos + 28)? as usize;
    let extra_len = u16_le(bytes, pos + 30)? as usize;
    let comment_len = u16_le(bytes, pos + 32)? as usize;
    let internal_attrs = u16_le(bytes, pos + 36)?;
    let external_attrs = u32_le(bytes, pos + 38)?;
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

    let (data, local_extra) = read_local_data(bytes, local_offset, method, compressed_size, uncompressed_size)?;
    if crc32(&data) != crc {
        return err(&format!("crc-32 mismatch for entry \"{name}\" (corrupt archive)"));
    }
    let extra_start = name_start + name_len;
    let central_extra = bytes.get(extra_start..extra_start + extra_len).ok_or_else(|| bad("truncated extra field"))?;
    let comment_start = extra_start + extra_len;
    let comment = bytes.get(comment_start..comment_start + comment_len).ok_or_else(|| bad("truncated entry comment"))?;

    let meta = EntryMeta {
        modified,
        made_by,
        internal_attrs,
        external_attrs,
        local_extra: without_zip64(local_extra),
        central_extra: without_zip64(central_extra),
        comment: comment.to_vec(),
    };
    Ok((ZipEntry { name, data, meta }, comment_start + comment_len))
}

/// Zip64's extra field (id `0x0001`) dropped from `extra`: it holds sizes
/// and offsets for a header whose own fields say `0xFFFFFFFF`, and the
/// writer states every size in the header (no zip64), so carried it would
/// describe nothing. Every other field is kept, in order. An extra area that
/// does not parse as id/length blocks is kept whole.
fn without_zip64(extra: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(extra.len());
    let mut at = 0;
    while at < extra.len() {
        let (Ok(id), Ok(len)) = (u16_le(extra, at), u16_le(extra, at + 2)) else { return extra.to_vec() };
        let end = at + 4 + len as usize;
        if end > extra.len() {
            return extra.to_vec();
        }
        if id != 0x0001 {
            kept.extend_from_slice(&extra[at..end]);
        }
        at = end;
    }
    kept
}

/// An entry's data at its local file header offset, decompressed per
/// `method`, and the local header's extra field. The local header's own
/// name/extra-field lengths (not the central directory's) locate the data
/// start — real-world writers keep the names identical, but the spec doesn't
/// require it, and Info-ZIP's extra fields differ.
fn read_local_data(
    bytes: &[u8],
    local_offset: usize,
    method: u16,
    compressed_size: usize,
    uncompressed_size: usize,
) -> Result<(Vec<u8>, &[u8])> {
    if u32_le(bytes, local_offset)? != LOCAL_SIG {
        return err("bad local file header signature");
    }
    let name_len = u16_le(bytes, local_offset + 26)? as usize;
    let extra_len = u16_le(bytes, local_offset + 28)? as usize;
    let extra_start = local_offset + 30 + name_len;
    let extra = bytes.get(extra_start..extra_start + extra_len).ok_or_else(|| bad("truncated local extra field"))?;
    let data_start = extra_start + extra_len;
    let compressed = bytes
        .get(data_start..data_start + compressed_size)
        .ok_or_else(|| bad("truncated entry data"))?;
    let data = match method {
        METHOD_STORED => compressed.to_vec(),
        METHOD_DEFLATE => gzip::decompress(compressed, uncompressed_size)
            .map_err(|_| bad("entry failed to inflate (corrupt or unsupported deflate stream)"))?,
        other => return err(&format!("unsupported zip compression method {other} (only stored/deflate)")),
    };
    Ok((data, extra))
}

/// The general-purpose flags a written entry declares. Names are written as
/// UTF-8, so a non-ASCII one sets bit 11; an ASCII name reads the same under
/// CP437 and leaves the flags clear, as every archive written before did.
fn name_flags(name: &str) -> u16 {
    if name.is_ascii() { 0 } else { FLAG_UTF8_NAME }
}

fn write_local_header(out: &mut Vec<u8>, name: &str, len: usize, crc: u32, meta: &EntryMeta) {
    out.extend_from_slice(&LOCAL_SIG.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract (2.0)
    out.extend_from_slice(&name_flags(name).to_le_bytes()); // general purpose bit flag
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&meta.modified.to_le_bytes()); // last mod file time, then date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes()); // compressed size
    out.extend_from_slice(&(len as u32).to_le_bytes()); // uncompressed size
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&(meta.local_extra.len() as u16).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&meta.local_extra);
}

fn write_central_header(out: &mut Vec<u8>, name: &str, len: usize, crc: u32, meta: &EntryMeta, local_offset: u32) {
    out.extend_from_slice(&CENTRAL_SIG.to_le_bytes());
    out.extend_from_slice(&meta.made_by.to_le_bytes());
    out.extend_from_slice(&20u16.to_le_bytes()); // version needed to extract
    out.extend_from_slice(&name_flags(name).to_le_bytes()); // general purpose bit flag
    out.extend_from_slice(&METHOD_STORED.to_le_bytes());
    out.extend_from_slice(&meta.modified.to_le_bytes()); // last mod file time, then date
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&(len as u32).to_le_bytes()); // compressed size
    out.extend_from_slice(&(len as u32).to_le_bytes()); // uncompressed size
    out.extend_from_slice(&(name.len() as u16).to_le_bytes());
    out.extend_from_slice(&(meta.central_extra.len() as u16).to_le_bytes());
    out.extend_from_slice(&(meta.comment.len() as u16).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number start
    out.extend_from_slice(&meta.internal_attrs.to_le_bytes());
    out.extend_from_slice(&meta.external_attrs.to_le_bytes());
    out.extend_from_slice(&local_offset.to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    out.extend_from_slice(&meta.central_extra);
    out.extend_from_slice(&meta.comment);
}

fn write_eocd(out: &mut Vec<u8>, num_entries: u16, cd_size: u32, cd_offset: u32, comment: &[u8]) {
    out.extend_from_slice(&EOCD_SIG.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // number of this disk
    out.extend_from_slice(&0u16.to_le_bytes()); // disk where central directory starts
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&num_entries.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
    out.extend_from_slice(comment);
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
        let timed = EntryMeta { modified, ..EntryMeta::created("a.png") };
        let bytes = write_archive(&[("a.png", b"x", &timed), ("b", b"", &EntryMeta::created("b"))], b"").unwrap();
        let back = read(&bytes).unwrap();
        assert_eq!((back[0].meta.modified, back[1].meta.modified), (modified, 0));
    }

    #[test]
    fn reads_the_time_a_real_archive_recorded() {
        let entries = read(include_bytes!("../tests/fixtures/sample.zip")).unwrap();
        assert_ne!(entries[0].meta.modified, 0, "Info-ZIP records a time");
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
    fn a_created_entry_is_a_unix_file_or_directory() {
        let back = read(&write(&[("doc.kml", b"<kml/>"), ("files/", b"")]).unwrap()).unwrap();
        assert_eq!((back[0].meta.made_by, back[0].meta.external_attrs), (0x0314, 0o100_644 << 16));
        assert_eq!(back[1].meta.external_attrs, (0o040_755 << 16) | 0x10);
    }

    #[test]
    fn every_header_field_not_derived_round_trips() {
        // Info-ZIP 3.0: timestamp and Unix-owner extras, longer locally than
        // centrally; modes 0644, 0755 and a directory; text bits; entry and
        // archive comments.
        let source = read_archive(include_bytes!("../tests/fixtures/info_zip_carried.kmz")).unwrap();
        let entries: Vec<_> = source.entries.iter().map(|e| (e.name.as_str(), &e.data[..], &e.meta)).collect();
        let back = read_archive(&write_archive(&entries, &source.comment).unwrap()).unwrap();
        assert_eq!(back.comment, b"geosetta M3c fixture: Info-ZIP 3.0, extras, modes, comments");
        assert_eq!(back.comment, source.comment);
        for (a, b) in source.entries.iter().zip(&back.entries) {
            assert_eq!((&a.name, &a.data, &a.meta), (&b.name, &b.data, &b.meta));
        }
        let meta = &source.entries[3].meta;
        assert_eq!((meta.external_attrs >> 16, meta.internal_attrs, &meta.comment[..]), (0o100_755, 1, &b"executable, mode 0755"[..]));
        assert_eq!((meta.local_extra.len(), meta.central_extra.len()), (28, 24));
    }

    #[test]
    fn a_comment_holding_the_end_records_signature_is_not_mistaken_for_it() {
        let mut comment = b"see ".to_vec();
        comment.extend_from_slice(&EOCD_SIG.to_le_bytes());
        comment.extend_from_slice(&[0; 18]);
        comment.extend_from_slice(b" and more");
        let meta = EntryMeta::created("a");
        let back = read_archive(&write_archive(&[("a", b"x", &meta)], &comment).unwrap()).unwrap();
        assert_eq!((back.entries.len(), back.comment), (1, comment));
    }

    #[test]
    fn zip64s_extra_field_is_not_carried() {
        let entries = read(include_bytes!("../tests/fixtures/local_zip64.kmz")).unwrap();
        assert!(entries[0].meta.local_extra.is_empty());
        let (ut, z64) = (b"UT\x05\x00\x01abcd", b"\x01\x00\x08\x00\xff\xff\xff\xff\x00\x00\x00\x00");
        assert_eq!(without_zip64(&[&z64[..], ut].concat()), ut);
        assert_eq!(without_zip64(b"UT\xff\x00"), b"UT\xff\x00", "a malformed area is kept whole");
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
