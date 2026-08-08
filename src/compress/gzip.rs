//! GZIP / DEFLATE decompression, implemented from RFC 1952 (gzip container)
//! and RFC 1951 (the DEFLATE stream) rather than a crate — keeping the project
//! dependency-free. Decoder only.
//!
//! Parquet's GZIP codec wraps a DEFLATE stream in a gzip container. For
//! robustness this also accepts a bare zlib stream (RFC 1950) or raw DEFLATE,
//! detected from the leading bytes.

use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Parquet(format!("gzip: {msg}")))
}

/// Decompress a GZIP (or zlib / raw DEFLATE) stream. `expected_size` is the
/// known output length from the Parquet page header and bounds the output.
pub fn decompress(input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let deflate = strip_container(input)?;
    let mut out = Vec::with_capacity(expected_size);
    Inflater::new(deflate, &mut out).run()?;
    if out.len() != expected_size {
        return err("inflated size does not match the page header");
    }
    Ok(out)
}

/// Find where the raw DEFLATE stream begins, skipping any gzip or zlib wrapper.
fn strip_container(input: &[u8]) -> Result<&[u8]> {
    if input.len() >= 2 && input[0] == 0x1f && input[1] == 0x8b {
        return strip_gzip(input);
    }
    // zlib: CMF/FLG with CM=8 and a checkable header.
    if input.len() >= 2
        && input[0] & 0x0f == 8
        && ((input[0] as u16) << 8 | input[1] as u16).is_multiple_of(31)
    {
        let fdict = input[1] & 0x20 != 0;
        let start = if fdict { 6 } else { 2 };
        return input.get(start..).ok_or_else(|| Error::Parquet("gzip: short zlib header".into()));
    }
    // Otherwise assume a bare DEFLATE stream.
    Ok(input)
}

fn strip_gzip(input: &[u8]) -> Result<&[u8]> {
    // Fixed 10-byte header, then optional fields per the flag byte.
    if input.len() < 10 {
        return err("truncated gzip header");
    }
    if input[2] != 8 {
        return err("unsupported gzip compression method");
    }
    let flags = input[3];
    let mut pos = 10;

    if flags & 0x04 != 0 {
        // FEXTRA: 2-byte length then that many bytes.
        let xlen = *input.get(pos).ok_or_else(bad_hdr)? as usize
            | (*input.get(pos + 1).ok_or_else(bad_hdr)? as usize) << 8;
        pos += 2 + xlen;
    }
    if flags & 0x08 != 0 {
        pos = skip_cstring(input, pos)?; // FNAME
    }
    if flags & 0x10 != 0 {
        pos = skip_cstring(input, pos)?; // FCOMMENT
    }
    if flags & 0x02 != 0 {
        pos += 2; // FHCRC
    }
    input.get(pos..).ok_or_else(bad_hdr)
}

fn bad_hdr() -> Error {
    Error::Parquet("gzip: truncated header".into())
}

fn skip_cstring(input: &[u8], mut pos: usize) -> Result<usize> {
    loop {
        let b = *input.get(pos).ok_or_else(bad_hdr)?;
        pos += 1;
        if b == 0 {
            return Ok(pos);
        }
    }
}

// --- DEFLATE bit reader ----------------------------------------------------

/// Forward, LSB-first bit reader (RFC 1951 packing order).
struct BitReader<'a> {
    data: &'a [u8],
    byte: usize,
    bit: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, byte: 0, bit: 0 }
    }

    fn read_bit(&mut self) -> Result<u32> {
        let b = *self
            .data
            .get(self.byte)
            .ok_or_else(|| Error::Parquet("gzip: unexpected end of deflate stream".into()))?;
        let v = ((b >> self.bit) & 1) as u32;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.byte += 1;
        }
        Ok(v)
    }

    /// Read `n` bits, first bit read is the least significant.
    fn read_bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.read_bit()? << i;
        }
        Ok(v)
    }

    /// Discard bits up to the next byte boundary.
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.byte += 1;
        }
    }

    fn read_byte(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.byte)
            .ok_or_else(|| Error::Parquet("gzip: unexpected end of stored block".into()))?;
        self.byte += 1;
        Ok(b)
    }
}

// --- canonical Huffman -----------------------------------------------------

const MAX_BITS: usize = 15;

/// A canonical Huffman decoder built from per-symbol code lengths (the "puff"
/// count/symbol representation).
struct Huffman {
    counts: [u16; MAX_BITS + 1],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Result<Huffman> {
        let mut counts = [0u16; MAX_BITS + 1];
        for &l in lengths {
            if l as usize > MAX_BITS {
                return err("code length exceeds 15");
            }
            counts[l as usize] += 1;
        }
        counts[0] = 0;

        // Offsets of each length's first symbol within the sorted symbol list.
        let mut offsets = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offsets[len + 1] = offsets[len] + counts[len];
        }

        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Ok(Huffman { counts, symbols })
    }

    /// Decode one symbol, reading bits most-significant-first into the code.
    fn decode(&self, bits: &mut BitReader) -> Result<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..=MAX_BITS {
            code |= bits.read_bit()? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        err("invalid Huffman code")
    }
}

// --- inflate ---------------------------------------------------------------

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order in which the code-length code lengths are stored (RFC 1951 §3.2.7).
const CL_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

struct Inflater<'a> {
    bits: BitReader<'a>,
    out: &'a mut Vec<u8>,
}

impl<'a> Inflater<'a> {
    fn new(data: &'a [u8], out: &'a mut Vec<u8>) -> Self {
        Inflater {
            bits: BitReader::new(data),
            out,
        }
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let final_block = self.bits.read_bit()? == 1;
            let btype = self.bits.read_bits(2)?;
            match btype {
                0 => self.stored()?,
                1 => {
                    let (lit, dist) = fixed_tables()?;
                    self.inflate_block(&lit, &dist)?;
                }
                2 => {
                    let (lit, dist) = self.dynamic_tables()?;
                    self.inflate_block(&lit, &dist)?;
                }
                _ => return err("reserved DEFLATE block type"),
            }
            if final_block {
                return Ok(());
            }
        }
    }

    fn stored(&mut self) -> Result<()> {
        self.bits.align();
        let len = self.bits.read_byte()? as usize | (self.bits.read_byte()? as usize) << 8;
        let nlen = self.bits.read_byte()? as usize | (self.bits.read_byte()? as usize) << 8;
        if len ^ 0xffff != nlen {
            return err("stored block length check failed");
        }
        for _ in 0..len {
            let b = self.bits.read_byte()?;
            self.out.push(b);
        }
        Ok(())
    }

    fn dynamic_tables(&mut self) -> Result<(Huffman, Huffman)> {
        let hlit = self.bits.read_bits(5)? as usize + 257;
        let hdist = self.bits.read_bits(5)? as usize + 1;
        let hclen = self.bits.read_bits(4)? as usize + 4;

        // Code-length code lengths, in their scrambled order.
        let mut cl_lengths = [0u8; 19];
        for i in 0..hclen {
            cl_lengths[CL_ORDER[i]] = self.bits.read_bits(3)? as u8;
        }
        let cl_huf = Huffman::new(&cl_lengths)?;

        // Decode the literal/length and distance code lengths together.
        let total = hlit + hdist;
        let mut lengths = Vec::with_capacity(total);
        while lengths.len() < total {
            let sym = cl_huf.decode(&mut self.bits)?;
            match sym {
                0..=15 => lengths.push(sym as u8),
                16 => {
                    // Repeat the previous length 3–6 times.
                    let prev = *lengths.last().ok_or_else(|| {
                        Error::Parquet("gzip: repeat with no previous length".into())
                    })?;
                    let n = 3 + self.bits.read_bits(2)?;
                    for _ in 0..n {
                        lengths.push(prev);
                    }
                }
                17 => {
                    let n = 3 + self.bits.read_bits(3)?;
                    lengths.extend(std::iter::repeat_n(0u8, n as usize));
                }
                18 => {
                    let n = 11 + self.bits.read_bits(7)?;
                    lengths.extend(std::iter::repeat_n(0u8, n as usize));
                }
                _ => return err("invalid code-length symbol"),
            }
        }
        if lengths.len() != total {
            return err("code-length run overran the table");
        }

        let lit = Huffman::new(&lengths[..hlit])?;
        let dist = Huffman::new(&lengths[hlit..])?;
        Ok((lit, dist))
    }

    fn inflate_block(&mut self, lit: &Huffman, dist: &Huffman) -> Result<()> {
        loop {
            let sym = lit.decode(&mut self.bits)?;
            match sym {
                0..=255 => self.out.push(sym as u8),
                256 => return Ok(()),
                257..=285 => {
                    let i = (sym - 257) as usize;
                    let length =
                        LENGTH_BASE[i] as usize + self.bits.read_bits(LENGTH_EXTRA[i])? as usize;
                    let dsym = dist.decode(&mut self.bits)? as usize;
                    if dsym >= DIST_BASE.len() {
                        return err("invalid distance symbol");
                    }
                    let distance =
                        DIST_BASE[dsym] as usize + self.bits.read_bits(DIST_EXTRA[dsym])? as usize;
                    self.copy_match(distance, length)?;
                }
                _ => return err("invalid literal/length symbol"),
            }
        }
    }

    fn copy_match(&mut self, distance: usize, length: usize) -> Result<()> {
        if distance == 0 || distance > self.out.len() {
            return err("match distance before start of output");
        }
        let start = self.out.len() - distance;
        for k in 0..length {
            let b = self.out[start + k];
            self.out.push(b);
        }
        Ok(())
    }
}

/// The fixed Huffman tables (RFC 1951 §3.2.6).
fn fixed_tables() -> Result<(Huffman, Huffman)> {
    let mut lit = [0u8; 288];
    for (s, l) in lit.iter_mut().enumerate() {
        *l = match s {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let dist = [5u8; 30];
    Ok((Huffman::new(&lit)?, Huffman::new(&dist)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    /// Compress via Python's gzip (level `level`); None if unavailable.
    fn gzip_cli(input: &[u8], level: u32) -> Option<Vec<u8>> {
        let code = format!(
            "import sys,gzip; sys.stdout.buffer.write(gzip.compress(sys.stdin.buffer.read(), {level}))"
        );
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(code)
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
        let Some(comp) = gzip_cli(input, level) else {
            eprintln!("skipping: python3 gzip unavailable");
            return;
        };
        let got = decompress(&comp, input.len())
            .unwrap_or_else(|e| panic!("inflate failed (len {}, level {level}): {e}", input.len()));
        assert_eq!(got, input, "mismatch at len {} level {level}", input.len());
    }

    #[test]
    fn roundtrip_repetitive() {
        // Long runs -> length/distance matches (dynamic Huffman).
        let data = vec![0x42u8; 20_000];
        for lvl in [1, 6, 9] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_text() {
        let mut data = Vec::new();
        while data.len() < 10_000 {
            data.extend_from_slice(b"the quick brown fox jumps over the lazy dog. ");
        }
        for lvl in [1, 6, 9] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_incompressible() {
        // Pseudo-random -> mostly stored / literals.
        let mut s = 0x9E3779B97F4A7C15u64;
        let data: Vec<u8> = (0..8192)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect();
        for lvl in [1, 6, 9] {
            check(&data, lvl);
        }
    }

    #[test]
    fn roundtrip_small_sizes() {
        for n in [0usize, 1, 2, 5, 255, 256, 1024] {
            let data: Vec<u8> = (0..n).map(|i| (i * 31 + 7) as u8).collect();
            check(&data, 6);
        }
    }
}
