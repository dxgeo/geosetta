//! Zstandard decompression, implemented from RFC 8878 rather than a crate
//! (keeping the project dependency-free). Decoder only — enough to read the
//! ZSTD-compressed Parquet pages that DuckDB and friends emit.
//!
//! Provenance / licensing: this is an independent implementation written from
//! the RFC 8878 specification. It is **not** derived from Meta's reference
//! `zstd` sources. The constant tables below (predefined FSE distributions and
//! the length/offset baseline tables) are the format-defined values from the
//! RFC — there is exactly one correct set, and RFC code components carry the
//! IETF Trust's BSD license, compatible with this project's MIT license. The
//! `zstd` CLI is used only as a test oracle (compress input, verify we
//! reproduce it), which does not make this code a derivative of that program.
//! Note also that libzstd itself is dual-licensed BSD-3-Clause OR GPLv2; the
//! format is an open standard regardless.
//!
//! The pieces, bottom-up: two bit readers (a forward LSB-first one for FSE
//! table descriptions, a backward MSB-first one for the entropy-coded data
//! streams), an FSE (tANS) decoder, a Huffman decoder for literals, and the
//! sequence machine that interleaves literal copies with matches. Frames are a
//! sequence of blocks (raw / RLE / compressed).

use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Parquet(format!("zstd: {msg}")))
}

/// Decompress a single ZSTD frame. `expected_size` is the known output length
/// (from the Parquet page header) and bounds the output.
pub fn decompress(input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let mut d = FrameDecoder::new(input, expected_size);
    d.run()
}

// --- bit readers -----------------------------------------------------------

/// Forward, LSB-first bit reader — used for FSE table descriptions, which are
/// bit-packed low-to-high across increasing byte addresses.
struct ForwardBits<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> ForwardBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        ForwardBits { data, bit: 0 }
    }

    fn read(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for i in 0..n {
            let idx = self.bit;
            let b = self.data.get(idx / 8).map_or(0, |&byte| (byte >> (idx % 8)) & 1);
            v |= (b as u32) << i;
            self.bit += 1;
        }
        v
    }

    /// Whole bytes consumed so far (rounded up) — where the next section starts.
    fn bytes_consumed(&self) -> usize {
        self.bit.div_ceil(8)
    }
}

/// Backward, MSB-first bit reader — used for FSE/Huffman data streams, which
/// ZSTD writes as a bit stack read from the last byte down. The highest set bit
/// of the last byte is a sentinel marking the start and is skipped.
struct BackwardBits<'a> {
    data: &'a [u8],
    /// Absolute index (bit 0 = LSB of byte 0) of the next bit to consume.
    pos: i64,
}

impl<'a> BackwardBits<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        let last = *data.last().ok_or_else(|| Error::Parquet("zstd: empty bitstream".into()))?;
        if last == 0 {
            return err("bitstream final byte is zero (missing sentinel)");
        }
        let high = 7 - last.leading_zeros() as i64; // highest set bit (0..7)
        let pos = (data.len() as i64 - 1) * 8 + high - 1;
        Ok(BackwardBits { data, pos })
    }

    /// Read `n` bits, first bit read becoming the most significant.
    fn read(&mut self, n: u32) -> u64 {
        let mut v = 0u64;
        for _ in 0..n {
            v <<= 1;
            if self.pos >= 0 {
                let byte = self.data[(self.pos / 8) as usize];
                v |= ((byte >> (self.pos % 8)) & 1) as u64;
            }
            self.pos -= 1;
        }
        v
    }

    /// Peek `n` bits without consuming.
    fn peek(&self, n: u32) -> u64 {
        let mut v = 0u64;
        let mut p = self.pos;
        for _ in 0..n {
            v <<= 1;
            if p >= 0 {
                let byte = self.data[(p / 8) as usize];
                v |= ((byte >> (p % 8)) & 1) as u64;
            }
            p -= 1;
        }
        v
    }

    fn consume(&mut self, n: u32) {
        self.pos -= n as i64;
    }

    /// True once a read has consumed *past* the start of the stream (i.e. used
    /// padding). A read that lands exactly on the first bit (`pos == -1`) is a
    /// clean finish, not an overflow — ZSTD's FSE decode flushes its final
    /// state symbols on the step whose update first overflows.
    fn overflowed(&self) -> bool {
        self.pos < -1
    }
}

// --- FSE -------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct FseEntry {
    symbol: u8,
    nb_bits: u8,
    new_state: u16,
}

struct FseTable {
    log: u32,
    entries: Vec<FseEntry>,
}

impl FseTable {
    /// Build a decoding table from a normalized distribution (`-1` marks a
    /// low-probability symbol given one state at the top of the table).
    fn from_distribution(dist: &[i16], log: u32) -> Result<FseTable> {
        let size = 1usize << log;
        let mut symbols = vec![0u8; size];
        let mut next = vec![0u16; dist.len()];
        let mut high = size; // reserved region grows downward from the top

        for (s, &c) in dist.iter().enumerate() {
            if c == -1 {
                high -= 1;
                symbols[high] = s as u8;
                next[s] = 1;
            } else if c >= 0 {
                next[s] = c as u16;
            }
        }

        let mask = size - 1;
        let step = (size >> 1) + (size >> 3) + 3;
        let mut pos = 0usize;
        for (s, &c) in dist.iter().enumerate() {
            for _ in 0..c.max(0) {
                symbols[pos] = s as u8;
                pos = (pos + step) & mask;
                while pos >= high {
                    pos = (pos + step) & mask;
                }
            }
        }
        if pos != 0 {
            return err("FSE distribution does not fill the table");
        }

        let mut entries = vec![FseEntry::default(); size];
        for (u, slot) in entries.iter_mut().enumerate() {
            let sym = symbols[u];
            let ns = next[sym as usize];
            next[sym as usize] += 1;
            let nb = log - high_bit(ns as u32);
            let new_state = ((ns as u32) << nb) - size as u32;
            *slot = FseEntry {
                symbol: sym,
                nb_bits: nb as u8,
                new_state: new_state as u16,
            };
        }
        Ok(FseTable { log, entries })
    }

    /// A trivial single-symbol table (RLE mode).
    fn rle(symbol: u8) -> FseTable {
        FseTable {
            log: 0,
            entries: vec![FseEntry {
                symbol,
                nb_bits: 0,
                new_state: 0,
            }],
        }
    }
}

/// Read an FSE table description (accuracy log + normalized counts) from a
/// forward bit reader, returning the table and the number of bytes consumed.
fn read_fse_table(data: &[u8], max_log: u32) -> Result<(FseTable, usize)> {
    let mut r = ForwardBits::new(data);
    let accuracy_log = r.read(4) + 5;
    if accuracy_log > max_log {
        return err("FSE accuracy log too large");
    }
    let table_size = 1i32 << accuracy_log;
    let mut remaining = table_size + 1;
    let mut threshold = table_size;
    let mut nb_bits = accuracy_log as i32 + 1;

    let mut dist: Vec<i16> = Vec::new();
    let mut previous_zero = false;

    while remaining > 1 && dist.len() < 256 {
        if previous_zero {
            // A run of zero-probability symbols, encoded 2 bits at a time.
            let mut repeat = 0usize;
            loop {
                let flags = r.read(2) as usize;
                repeat += flags;
                if flags != 3 {
                    break;
                }
            }
            dist.extend(std::iter::repeat_n(0i16, repeat));
            previous_zero = false;
            continue;
        }

        let max = (2 * threshold - 1) - remaining;
        let low = r.read((nb_bits - 1) as u32) as i32;
        let count = if low < max {
            low
        } else {
            let extra = r.read(1) as i32;
            let val = low + (extra << (nb_bits - 1));
            if val >= threshold {
                val - max
            } else {
                val
            }
        };

        let value = count - 1; // stored biased by 1; -1 means low-probability
        remaining -= value.abs();
        dist.push(value as i16);
        previous_zero = value == 0;

        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 {
        return err("FSE table description did not sum to table size");
    }

    let table = FseTable::from_distribution(&dist, accuracy_log)?;
    Ok((table, r.bytes_consumed()))
}

struct FseState {
    state: u16,
}

impl FseState {
    fn init(table: &FseTable, bits: &mut BackwardBits) -> FseState {
        FseState {
            state: bits.read(table.log) as u16,
        }
    }

    fn symbol(&self, table: &FseTable) -> u8 {
        table.entries[self.state as usize].symbol
    }

    fn update(&mut self, table: &FseTable, bits: &mut BackwardBits) {
        let e = table.entries[self.state as usize];
        let add = bits.read(e.nb_bits as u32) as u16;
        self.state = e.new_state + add;
    }
}

/// Two-state FSE decode of a whole backward bitstream (used for Huffman
/// weights), decoding until the stream is exhausted.
fn fse_decompress(table: &FseTable, data: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut bits = BackwardBits::new(data)?;
    let mut s1 = FseState::init(table, &mut bits);
    let mut s2 = FseState::init(table, &mut bits);
    let mut out = Vec::new();
    loop {
        out.push(s1.symbol(table));
        s1.update(table, &mut bits);
        if bits.overflowed() {
            out.push(s2.symbol(table));
            break;
        }
        out.push(s2.symbol(table));
        s2.update(table, &mut bits);
        if bits.overflowed() {
            out.push(s1.symbol(table));
            break;
        }
        if out.len() > max {
            return err("FSE weight stream overran");
        }
    }
    Ok(out)
}

// --- Huffman ---------------------------------------------------------------

#[derive(Clone, Copy, Default)]
struct HufEntry {
    symbol: u8,
    nb_bits: u8,
}

struct HufTable {
    log: u32,
    entries: Vec<HufEntry>,
}

impl HufTable {
    /// Build a Huffman decode table from per-symbol weights.
    fn from_weights(weights: &[u8]) -> Result<HufTable> {
        // Sum of 2^(w-1) over present symbols; the last symbol's weight fills
        // the distribution up to the next power of two.
        let mut weight_sum = 0u32;
        for &w in weights {
            if w > 0 {
                weight_sum += 1 << (w - 1);
            }
        }
        if weight_sum == 0 {
            return err("empty Huffman table");
        }
        let log = high_bit(weight_sum) + 1;
        let left = (1u32 << log) - weight_sum;
        if left == 0 || (left & (left - 1)) != 0 {
            return err("invalid Huffman weight total");
        }
        let last_weight = high_bit(left) + 1;

        let mut all: Vec<u8> = weights.to_vec();
        all.push(last_weight as u8);

        // rank_start[w] = first table index for weight w.
        let mut counts = [0u32; 13];
        for &w in &all {
            if w > 0 {
                counts[w as usize] += 1;
            }
        }
        let mut rank_start = [0u32; 13];
        let mut next = 0u32;
        for w in 1..=log as usize {
            rank_start[w] = next;
            next += counts[w] << (w - 1);
        }

        let size = 1usize << log;
        let mut entries = vec![HufEntry::default(); size];
        for (sym, &w) in all.iter().enumerate() {
            if w == 0 {
                continue;
            }
            let length = 1usize << (w as usize - 1);
            let nb_bits = (log + 1 - w as u32) as u8;
            let start = rank_start[w as usize] as usize;
            for slot in entries.iter_mut().skip(start).take(length) {
                *slot = HufEntry {
                    symbol: sym as u8,
                    nb_bits,
                };
            }
            rank_start[w as usize] += length as u32;
        }
        Ok(HufTable { log, entries })
    }

    /// Decode exactly `count` symbols from one backward bitstream.
    fn decode_stream(&self, data: &[u8], count: usize) -> Result<Vec<u8>> {
        let mut bits = BackwardBits::new(data)?;
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            let idx = bits.peek(self.log) as usize;
            let e = self.entries[idx];
            out.push(e.symbol);
            bits.consume(e.nb_bits as u32);
        }
        Ok(out)
    }
}

/// Parse a Huffman tree description, returning the table and bytes consumed.
fn read_huffman_table(data: &[u8]) -> Result<(HufTable, usize)> {
    let header = *data.first().ok_or_else(|| Error::Parquet("zstd: no huffman header".into()))?;
    if header < 128 {
        // FSE-compressed weights: `header` bytes of FSE stream follow.
        let fse_bytes = header as usize;
        let body = data
            .get(1..1 + fse_bytes)
            .ok_or_else(|| Error::Parquet("zstd: truncated huffman weights".into()))?;
        let (table, table_bytes) = read_fse_table(body, 6)?;
        let stream = &body[table_bytes..];
        let weights = fse_decompress(&table, stream, 256)?;
        let huf = HufTable::from_weights(&weights)?;
        Ok((huf, 1 + fse_bytes))
    } else {
        // Direct: (header - 127) weights, 4 bits each, high nibble first.
        let n = (header - 127) as usize;
        let bytes = n.div_ceil(2);
        let packed = data
            .get(1..1 + bytes)
            .ok_or_else(|| Error::Parquet("zstd: truncated direct weights".into()))?;
        let mut weights = Vec::with_capacity(n);
        for i in 0..n {
            let byte = packed[i / 2];
            let w = if i % 2 == 0 { byte >> 4 } else { byte & 0x0f };
            weights.push(w);
        }
        let huf = HufTable::from_weights(&weights)?;
        Ok((huf, 1 + bytes))
    }
}

// --- literals --------------------------------------------------------------

/// Decode a literals section, returning the literals and the bytes it spanned.
/// `prev_huf` carries the Huffman table forward for Treeless blocks.
fn decode_literals(
    block: &[u8],
    prev_huf: &mut Option<HufTable>,
) -> Result<(Vec<u8>, usize)> {
    let b0 = *block.first().ok_or_else(|| Error::Parquet("zstd: empty literals".into()))?;
    let lit_type = b0 & 0x3;
    let size_format = (b0 >> 2) & 0x3;

    match lit_type {
        0 | 1 => {
            // Raw (0) or RLE (1).
            let (regen, header_bytes) = match size_format {
                0 | 2 => ((b0 >> 3) as usize, 1),
                1 => {
                    let b1 = *block.get(1).ok_or_else(bad_lit)?;
                    (((b0 as usize) >> 4) | ((b1 as usize) << 4), 2)
                }
                _ => {
                    let b1 = *block.get(1).ok_or_else(bad_lit)?;
                    let b2 = *block.get(2).ok_or_else(bad_lit)?;
                    (
                        ((b0 as usize) >> 4) | ((b1 as usize) << 4) | ((b2 as usize) << 12),
                        3,
                    )
                }
            };
            if lit_type == 0 {
                let lit = block
                    .get(header_bytes..header_bytes + regen)
                    .ok_or_else(bad_lit)?
                    .to_vec();
                Ok((lit, header_bytes + regen))
            } else {
                let byte = *block.get(header_bytes).ok_or_else(bad_lit)?;
                Ok((vec![byte; regen], header_bytes + 1))
            }
        }
        2 | 3 => {
            // Compressed (2) or Treeless (3, reuses previous table).
            let (regen, comp, streams, header_bytes) = parse_compressed_lit_header(block, size_format)?;
            let section = block.get(header_bytes..header_bytes + comp).ok_or_else(bad_lit)?;

            let (huf_owned, tree_bytes) = if lit_type == 2 {
                let (t, n) = read_huffman_table(section)?;
                (Some(t), n)
            } else {
                (None, 0)
            };
            let huf = match (&huf_owned, prev_huf.as_ref()) {
                (Some(t), _) => t,
                (None, Some(t)) => t,
                (None, None) => return err("treeless literals without a prior Huffman table"),
            };

            let streams_data = &section[tree_bytes..];
            let literals = if streams == 1 {
                huf.decode_stream(streams_data, regen)?
            } else {
                decode_four_streams(huf, streams_data, regen)?
            };

            if let Some(t) = huf_owned {
                *prev_huf = Some(t);
            }
            Ok((literals, header_bytes + comp))
        }
        _ => unreachable!(),
    }
}

fn bad_lit() -> Error {
    Error::Parquet("zstd: truncated literals section".into())
}

fn parse_compressed_lit_header(
    block: &[u8],
    size_format: u8,
) -> Result<(usize, usize, usize, usize)> {
    // Returns (regenerated_size, compressed_size, num_streams, header_bytes).
    let b = |i: usize| block.get(i).copied().map(|v| v as usize).ok_or_else(bad_lit);
    match size_format {
        0 | 1 => {
            let (b0, b1, b2) = (b(0)?, b(1)?, b(2)?);
            let regen = (b0 >> 4) | ((b1 & 0x3f) << 4);
            let comp = (b1 >> 6) | (b2 << 2);
            let streams = if size_format == 0 { 1 } else { 4 };
            Ok((regen, comp, streams, 3))
        }
        2 => {
            let (b0, b1, b2, b3) = (b(0)?, b(1)?, b(2)?, b(3)?);
            let regen = (b0 >> 4) | (b1 << 4) | ((b2 & 0x3) << 12);
            let comp = (b2 >> 2) | (b3 << 6);
            Ok((regen, comp, 4, 4))
        }
        _ => {
            let (b0, b1, b2, b3, b4) = (b(0)?, b(1)?, b(2)?, b(3)?, b(4)?);
            let regen = (b0 >> 4) | (b1 << 4) | ((b2 & 0x3f) << 12);
            let comp = (b2 >> 6) | (b3 << 2) | (b4 << 10);
            Ok((regen, comp, 4, 5))
        }
    }
}

fn decode_four_streams(huf: &HufTable, data: &[u8], regen: usize) -> Result<Vec<u8>> {
    // 6-byte jump table: sizes of the first three streams (LE u16 each).
    if data.len() < 6 {
        return err("truncated 4-stream jump table");
    }
    let s1 = u16::from_le_bytes([data[0], data[1]]) as usize;
    let s2 = u16::from_le_bytes([data[2], data[3]]) as usize;
    let s3 = u16::from_le_bytes([data[4], data[5]]) as usize;
    let rest = &data[6..];
    if s1 + s2 + s3 > rest.len() {
        return err("4-stream sizes exceed section");
    }
    let s4 = rest.len() - s1 - s2 - s3;

    let seg = regen.div_ceil(4);
    let sizes = [s1, s2, s3, s4];
    let counts = [seg, seg, seg, regen - 3 * seg];

    let mut out = Vec::with_capacity(regen);
    let mut off = 0usize;
    for k in 0..4 {
        let stream = &rest[off..off + sizes[k]];
        off += sizes[k];
        out.extend(huf.decode_stream(stream, counts[k])?);
    }
    Ok(out)
}

// --- sequences -------------------------------------------------------------

const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LL_BITS: [u32; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: [u32; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];

const LL_DIST: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DIST: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DIST: [i16; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

struct SeqTables {
    ll: FseTable,
    of: FseTable,
    ml: FseTable,
}

/// A running frame decoder.
struct FrameDecoder<'a> {
    input: &'a [u8],
    pos: usize,
    out: Vec<u8>,
    expected: usize,
    huf: Option<HufTable>,
    prev: Option<SeqTables>,
    repeats: [u32; 3],
}

impl<'a> FrameDecoder<'a> {
    fn new(input: &'a [u8], expected: usize) -> Self {
        FrameDecoder {
            input,
            pos: 0,
            out: Vec::with_capacity(expected),
            expected,
            huf: None,
            prev: None,
            repeats: [1, 4, 8],
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .input
            .get(self.pos..self.pos + n)
            .ok_or_else(|| Error::Parquet("zstd: unexpected end of frame".into()))?;
        self.pos += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn run(&mut self) -> Result<Vec<u8>> {
        self.parse_frame_header()?;
        loop {
            let header = self.take(3)?;
            let raw = header[0] as u32 | (header[1] as u32) << 8 | (header[2] as u32) << 16;
            let last = raw & 1 != 0;
            let block_type = (raw >> 1) & 0x3;
            let block_size = (raw >> 3) as usize;

            match block_type {
                0 => {
                    let data = self.take(block_size)?.to_vec();
                    self.out.extend_from_slice(&data);
                }
                1 => {
                    let b = self.byte()?;
                    self.out.extend(std::iter::repeat_n(b, block_size));
                }
                2 => {
                    let block = self.take(block_size)?;
                    self.decode_compressed_block(block)?;
                }
                _ => return err("reserved block type"),
            }
            if last {
                break;
            }
        }
        if self.out.len() != self.expected {
            return err("decompressed size does not match the page header");
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn parse_frame_header(&mut self) -> Result<()> {
        let magic = self.take(4)?;
        if magic != [0x28, 0xB5, 0x2F, 0xFD] {
            return err("bad frame magic");
        }
        let descriptor = self.byte()?;
        let fcs_flag = descriptor >> 6;
        let single_segment = descriptor & 0x20 != 0;
        let content_checksum = descriptor & 0x04 != 0;
        let dict_id_flag = descriptor & 0x3;

        if !single_segment {
            self.byte()?; // window descriptor
        }
        let dict_bytes = [0, 1, 2, 4][dict_id_flag as usize];
        self.take(dict_bytes)?;

        let fcs_bytes = match fcs_flag {
            0 => usize::from(single_segment),
            1 => 2,
            2 => 4,
            _ => 8,
        };
        self.take(fcs_bytes)?;
        // A trailing 4-byte content checksum, if present, is ignored — we stop
        // consuming at the last block, so nothing else to do here.
        let _ = content_checksum;
        Ok(())
    }

    fn decode_compressed_block(&mut self, block: &[u8]) -> Result<()> {
        let mut huf = self.huf.take();
        let (literals, lit_bytes) = decode_literals(block, &mut huf)?;
        self.huf = huf;

        let seq_data = &block[lit_bytes..];
        self.decode_sequences(&literals, seq_data)?;
        Ok(())
    }

    fn decode_sequences(&mut self, literals: &[u8], data: &[u8]) -> Result<()> {
        let b0 = *data.first().ok_or_else(|| Error::Parquet("zstd: no sequences header".into()))?;
        let (num_seq, mut off) = if b0 < 128 {
            (b0 as usize, 1)
        } else if b0 < 255 {
            let b1 = *data.get(1).ok_or_else(bad_seq)?;
            (((b0 as usize - 128) << 8) + b1 as usize, 2)
        } else {
            let b1 = *data.get(1).ok_or_else(bad_seq)?;
            let b2 = *data.get(2).ok_or_else(bad_seq)?;
            (b1 as usize + ((b2 as usize) << 8) + 0x7F00, 3)
        };

        if num_seq == 0 {
            // No sequences: the block output is just the literals.
            self.out.extend_from_slice(literals);
            return Ok(());
        }

        let modes = *data.get(off).ok_or_else(bad_seq)?;
        off += 1;
        let ll_mode = (modes >> 6) & 0x3;
        let of_mode = (modes >> 4) & 0x3;
        let ml_mode = (modes >> 2) & 0x3;

        let ll = self.sequence_table(ll_mode, &LL_DIST, 9, data, &mut off, SeqKind::Ll)?;
        let of = self.sequence_table(of_mode, &OF_DIST, 8, data, &mut off, SeqKind::Of)?;
        let ml = self.sequence_table(ml_mode, &ML_DIST, 9, data, &mut off, SeqKind::Ml)?;
        let tables = SeqTables { ll, of, ml };

        self.run_sequences(literals, &data[off..], num_seq, &tables)?;
        self.prev = Some(tables);
        Ok(())
    }

    /// Resolve one sequence FSE table for the given compression mode.
    fn sequence_table(
        &self,
        mode: u8,
        predefined: &[i16],
        max_log: u32,
        data: &[u8],
        off: &mut usize,
        kind: SeqKind,
    ) -> Result<FseTable> {
        match mode {
            0 => FseTable::from_distribution(predefined, predefined_log(kind)),
            1 => {
                let sym = *data.get(*off).ok_or_else(bad_seq)?;
                *off += 1;
                Ok(FseTable::rle(sym))
            }
            2 => {
                let (t, used) = read_fse_table(&data[*off..], max_log)?;
                *off += used;
                Ok(t)
            }
            _ => self
                .prev
                .as_ref()
                .map(|p| clone_kind(p, kind))
                .ok_or_else(|| Error::Parquet("zstd: repeat sequence table with no prior".into())),
        }
    }

    fn run_sequences(
        &mut self,
        literals: &[u8],
        data: &[u8],
        num_seq: usize,
        tables: &SeqTables,
    ) -> Result<()> {
        let mut bits = BackwardBits::new(data)?;
        // States initialized in the order LL, OF, ML.
        let mut ll = FseState::init(&tables.ll, &mut bits);
        let mut of = FseState::init(&tables.of, &mut bits);
        let mut ml = FseState::init(&tables.ml, &mut bits);

        let mut lit_pos = 0usize;
        for i in 0..num_seq {
            let ll_code = ll.symbol(&tables.ll) as usize;
            let ml_code = ml.symbol(&tables.ml) as usize;
            let of_code = of.symbol(&tables.of) as u32;

            // Extra bits, read offset then match then literal length.
            let offset_value = (1u64 << of_code) + bits.read(of_code);
            let match_length = ML_BASE[ml_code] as usize + bits.read(ML_BITS[ml_code]) as usize;
            let literals_length = LL_BASE[ll_code] as usize + bits.read(LL_BITS[ll_code]) as usize;

            let offset = self.resolve_offset(offset_value, literals_length)?;
            self.emit_sequence(literals, &mut lit_pos, literals_length, offset, match_length)?;

            if i + 1 < num_seq {
                // State updates in the order LL, ML, OF.
                ll.update(&tables.ll, &mut bits);
                ml.update(&tables.ml, &mut bits);
                of.update(&tables.of, &mut bits);
            }
        }

        // Trailing literals after the last match.
        self.out.extend_from_slice(&literals[lit_pos..]);
        Ok(())
    }

    /// Apply the repeat-offset rules, updating the recent-offset history.
    fn resolve_offset(&mut self, offset_value: u64, literals_length: usize) -> Result<u32> {
        let r = &mut self.repeats;
        let offset = if offset_value > 3 {
            let o = (offset_value - 3) as u32;
            r[2] = r[1];
            r[1] = r[0];
            r[0] = o;
            o
        } else if literals_length != 0 {
            match offset_value {
                1 => r[0],
                2 => {
                    r.swap(0, 1);
                    r[0]
                }
                _ => {
                    let o = r[2];
                    r[2] = r[1];
                    r[1] = r[0];
                    r[0] = o;
                    o
                }
            }
        } else {
            match offset_value {
                1 => {
                    r.swap(0, 1);
                    r[0]
                }
                2 => {
                    let o = r[2];
                    r[2] = r[1];
                    r[1] = r[0];
                    r[0] = o;
                    o
                }
                _ => {
                    let o = r[0] - 1;
                    r[2] = r[1];
                    r[1] = r[0];
                    r[0] = o;
                    o
                }
            }
        };
        if offset == 0 {
            return err("zero offset");
        }
        Ok(offset)
    }

    fn emit_sequence(
        &mut self,
        literals: &[u8],
        lit_pos: &mut usize,
        literals_length: usize,
        offset: u32,
        match_length: usize,
    ) -> Result<()> {
        let lits = literals
            .get(*lit_pos..*lit_pos + literals_length)
            .ok_or_else(|| Error::Parquet("zstd: literals underrun".into()))?;
        self.out.extend_from_slice(lits);
        *lit_pos += literals_length;

        let offset = offset as usize;
        if offset > self.out.len() {
            return err("match offset before start of output");
        }
        let start = self.out.len() - offset;
        for k in 0..match_length {
            let b = self.out[start + k];
            self.out.push(b);
        }
        Ok(())
    }
}

fn bad_seq() -> Error {
    Error::Parquet("zstd: truncated sequences section".into())
}

#[derive(Clone, Copy)]
enum SeqKind {
    Ll,
    Of,
    Ml,
}

fn predefined_log(kind: SeqKind) -> u32 {
    match kind {
        SeqKind::Ll => 6,
        SeqKind::Of => 5,
        SeqKind::Ml => 6,
    }
}

fn clone_kind(p: &SeqTables, kind: SeqKind) -> FseTable {
    let src = match kind {
        SeqKind::Ll => &p.ll,
        SeqKind::Of => &p.of,
        SeqKind::Ml => &p.ml,
    };
    FseTable {
        log: src.log,
        entries: src.entries.clone(),
    }
}

/// floor(log2(x)) for x >= 1.
fn high_bit(x: u32) -> u32 {
    31 - x.leading_zeros()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn high_bit_basic() {
        assert_eq!(high_bit(1), 0);
        assert_eq!(high_bit(2), 1);
        assert_eq!(high_bit(255), 7);
        assert_eq!(high_bit(256), 8);
    }

    /// Compress `input` with the `zstd` CLI at `level`; returns None if the CLI
    /// is unavailable so the test can be skipped in environments without it.
    fn zstd_cli(input: &[u8], level: u32) -> Option<Vec<u8>> {
        let mut child = Command::new("zstd")
            .arg(format!("-{level}"))
            .arg("-q")
            .arg("-c")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child.stdin.take()?.write_all(input).ok()?;
        let out = child.wait_with_output().ok()?;
        out.status.success().then_some(out.stdout)
    }

    fn check(input: &[u8], level: u32) {
        let Some(comp) = zstd_cli(input, level) else {
            eprintln!("skipping: zstd CLI unavailable");
            return;
        };
        let got = decompress(&comp, input.len())
            .unwrap_or_else(|e| panic!("decompress failed (len {}, level {level}): {e}", input.len()));
        assert_eq!(got, input, "mismatch at len {} level {level}", input.len());
    }

    #[test]
    fn roundtrip_incompressible() {
        // Pseudo-random -> raw literals, few/no matches.
        let mut s = 0x2545F4914F6CDD1Du64;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect();
        for lvl in [1, 3, 19] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_repetitive() {
        let data = vec![0x42u8; 10_000];
        for lvl in [1, 3, 19] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_text() {
        let mut data = Vec::new();
        while data.len() < 8000 {
            data.extend_from_slice(b"the quick brown fox jumps over the lazy dog. ");
        }
        for lvl in [1, 3, 19] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_structured() {
        // WKB-like: many little-endian doubles with structure -> Huffman + seqs.
        let mut data = Vec::new();
        for i in 0..2000u32 {
            data.push(0x01);
            data.extend_from_slice(&1u32.to_le_bytes());
            data.extend_from_slice(&(-73.9 + (i % 100) as f64 * 0.001).to_le_bytes());
            data.extend_from_slice(&(40.7 + (i % 100) as f64 * 0.001).to_le_bytes());
        }
        for lvl in [3, 9, 19] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_small_sizes() {
        for n in [0usize, 1, 2, 5, 31, 32, 33, 128, 1023, 1024] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
            check(&data, 3);
        }
    }
}
