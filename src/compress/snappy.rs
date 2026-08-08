//! Snappy compression, implemented from the format description rather than a
//! crate (keeping the project dependency-free).
//!
//! This is the *raw* Snappy block format Parquet uses — an unsigned-varint
//! uncompressed length followed by a stream of literal and copy elements — not
//! the framed/streaming format. The encoder follows the reference Snappy
//! algorithm (a hash-table match finder over 64 KiB fragments); [`decompress`]
//! exists for round-trip testing.
//!
//! Element tags (low 2 bits of the tag byte):
//! - `00` literal: upper 6 bits are `len-1`, or 60..63 meaning 1..4 following
//!   little-endian length bytes; then the literal bytes.
//! - `01` copy, 1-byte offset: bits 2..4 are `len-4`, bits 5..7 are the high
//!   bits of an 11-bit offset whose low 8 bits are the next byte.
//! - `10` copy, 2-byte offset: upper 6 bits are `len-1`, offset is the next 2
//!   bytes little-endian.
//! - `11` copy, 4-byte offset: like `10` but a 4-byte offset.

const TAG_LITERAL: u8 = 0x00;
const TAG_COPY1: u8 = 0x01;
const TAG_COPY2: u8 = 0x02;

/// Fragment size the reference encoder works in; also bounds every copy offset
/// to 16 bits, so the encoder never needs the 4-byte-offset form.
const MAX_BLOCK: usize = 65536;
/// Trailing bytes the inner loop keeps in hand so its 4/8-byte reads stay in
/// bounds; also the smallest block worth match-finding.
const INPUT_MARGIN: usize = 15;
const MIN_NON_LITERAL_BLOCK: usize = 1 + 1 + INPUT_MARGIN;
const MAX_TABLE_SIZE: usize = 1 << 14;
const TABLE_MASK: u32 = (MAX_TABLE_SIZE - 1) as u32;

/// Compress `input` into a self-contained raw Snappy block.
pub fn compress(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(max_compressed_len(input.len()));
    write_uvarint(&mut out, input.len() as u64);

    let mut src = input;
    while !src.is_empty() {
        let n = src.len().min(MAX_BLOCK);
        let (block, rest) = src.split_at(n);
        if block.len() < MIN_NON_LITERAL_BLOCK {
            emit_literal(&mut out, block);
        } else {
            encode_block(block, &mut out);
        }
        src = rest;
    }
    out
}

/// Upper bound on the compressed size, per the Snappy spec.
pub fn max_compressed_len(source_len: usize) -> usize {
    32 + source_len + source_len / 6
}

fn load32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn load64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes([
        b[i],
        b[i + 1],
        b[i + 2],
        b[i + 3],
        b[i + 4],
        b[i + 5],
        b[i + 6],
        b[i + 7],
    ])
}

fn hash(u: u32, shift: u32) -> u32 {
    u.wrapping_mul(0x1e35a7bd) >> shift
}

/// Emit one literal element covering `lit` (must be non-empty).
fn emit_literal(out: &mut Vec<u8>, lit: &[u8]) {
    let n = lit.len() - 1;
    if n < 60 {
        out.push(((n as u8) << 2) | TAG_LITERAL);
    } else if n < 1 << 8 {
        out.push((60 << 2) | TAG_LITERAL);
        out.push(n as u8);
    } else if n < 1 << 16 {
        out.push((61 << 2) | TAG_LITERAL);
        out.push(n as u8);
        out.push((n >> 8) as u8);
    } else if n < 1 << 24 {
        out.push((62 << 2) | TAG_LITERAL);
        out.push(n as u8);
        out.push((n >> 8) as u8);
        out.push((n >> 16) as u8);
    } else {
        out.push((63 << 2) | TAG_LITERAL);
        out.push(n as u8);
        out.push((n >> 8) as u8);
        out.push((n >> 16) as u8);
        out.push((n >> 24) as u8);
    }
    out.extend_from_slice(lit);
}

/// Emit a back-reference of `length` bytes at `offset`, splitting into as many
/// copy elements as the format's per-element length cap requires.
fn emit_copy(out: &mut Vec<u8>, offset: usize, mut length: usize) {
    while length >= 68 {
        out.push((63 << 2) | TAG_COPY2);
        out.push(offset as u8);
        out.push((offset >> 8) as u8);
        length -= 64;
    }
    if length > 64 {
        out.push((59 << 2) | TAG_COPY2);
        out.push(offset as u8);
        out.push((offset >> 8) as u8);
        length -= 60;
    }
    if length >= 12 || offset >= 2048 {
        out.push((((length - 1) as u8) << 2) | TAG_COPY2);
        out.push(offset as u8);
        out.push((offset >> 8) as u8);
    } else {
        out.push((((offset >> 8) as u8) << 5) | (((length - 4) as u8) << 2) | TAG_COPY1);
        out.push(offset as u8);
    }
}

/// Match-find and encode a single fragment (`src.len() >= MIN_NON_LITERAL_BLOCK`).
fn encode_block(src: &[u8], out: &mut Vec<u8>) {
    let mut shift: u32 = 32 - 8;
    let mut table_size = 1usize << 8;
    while table_size < MAX_TABLE_SIZE && table_size < src.len() {
        table_size <<= 1;
        shift -= 1;
    }
    let mut table = [0u16; MAX_TABLE_SIZE];

    // Positions in `0..=s_limit` can be probed with a 4-byte read to spare.
    let s_limit = src.len() - INPUT_MARGIN;
    let mut next_emit = 0usize;

    // The stream must open with a literal (nothing precedes byte 0 to copy),
    // so match finding starts at 1.
    let mut s = 1usize;
    let mut next_hash = hash(load32(src, s), shift);

    'main: loop {
        // Search for a 4-byte match, probing less densely the longer we go
        // without a hit (the reference "skip" heuristic).
        let mut skip = 32usize;
        let mut next_s = s;
        let mut candidate;
        loop {
            s = next_s;
            let bytes_between = skip >> 5;
            next_s = s + bytes_between;
            skip += bytes_between;
            if next_s > s_limit {
                break 'main;
            }
            candidate = table[(next_hash & TABLE_MASK) as usize] as usize;
            table[(next_hash & TABLE_MASK) as usize] = s as u16;
            next_hash = hash(load32(src, next_s), shift);
            if load32(src, s) == load32(src, candidate) {
                break;
            }
        }

        // src[next_emit..s] precede the match and are unmatched.
        emit_literal(out, &src[next_emit..s]);

        // Emit this copy, then keep emitting copies as long as the bytes right
        // after each match also match; otherwise fall back to the main loop to
        // emit the next literal.
        loop {
            let base = s;
            s += 4;
            let mut i = candidate + 4;
            while s < src.len() && src[i] == src[s] {
                i += 1;
                s += 1;
            }
            emit_copy(out, base - candidate, s - base);
            next_emit = s;
            if s >= s_limit {
                break 'main;
            }

            // Refresh the table at s-1 and s before probing again; on a miss,
            // also prime next_hash for s+1 and rejoin the search loop.
            let x = load64(src, s - 1);
            let prev_hash = hash(x as u32, shift);
            table[(prev_hash & TABLE_MASK) as usize] = (s - 1) as u16;
            let curr_hash = hash((x >> 8) as u32, shift);
            candidate = table[(curr_hash & TABLE_MASK) as usize] as usize;
            table[(curr_hash & TABLE_MASK) as usize] = s as u16;
            if (x >> 8) as u32 != load32(src, candidate) {
                next_hash = hash((x >> 16) as u32, shift);
                s += 1;
                break;
            }
        }
    }

    if next_emit < src.len() {
        emit_literal(out, &src[next_emit..]);
    }
}

/// Unsigned LEB128 varint.
fn write_uvarint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

/// Decode a raw Snappy block. Returns `None` on malformed input.
pub fn decompress(input: &[u8]) -> Option<Vec<u8>> {
    let (expected_len, mut pos) = read_uvarint(input)?;
    let mut out = Vec::with_capacity(expected_len as usize);

    while pos < input.len() {
        let tag = input[pos];
        match tag & 0x03 {
            TAG_LITERAL => {
                let mut len = (tag >> 2) as usize;
                pos += 1;
                if len >= 60 {
                    let extra = len - 59;
                    if pos + extra > input.len() {
                        return None;
                    }
                    let mut l = 0usize;
                    for k in 0..extra {
                        l |= (input[pos + k] as usize) << (8 * k);
                    }
                    pos += extra;
                    len = l;
                }
                len += 1;
                if pos + len > input.len() {
                    return None;
                }
                out.extend_from_slice(&input[pos..pos + len]);
                pos += len;
            }
            TAG_COPY1 => {
                if pos + 2 > input.len() {
                    return None;
                }
                let len = 4 + ((tag >> 2) & 0x07) as usize;
                let offset = (((tag >> 5) as usize) << 8) | input[pos + 1] as usize;
                pos += 2;
                copy_match(&mut out, offset, len)?;
            }
            TAG_COPY2 => {
                if pos + 3 > input.len() {
                    return None;
                }
                let len = 1 + (tag >> 2) as usize;
                let offset = input[pos + 1] as usize | ((input[pos + 2] as usize) << 8);
                pos += 3;
                copy_match(&mut out, offset, len)?;
            }
            _ => {
                if pos + 5 > input.len() {
                    return None;
                }
                let len = 1 + (tag >> 2) as usize;
                let offset = input[pos + 1] as usize
                    | ((input[pos + 2] as usize) << 8)
                    | ((input[pos + 3] as usize) << 16)
                    | ((input[pos + 4] as usize) << 24);
                pos += 5;
                copy_match(&mut out, offset, len)?;
            }
        }
    }

    (out.len() as u64 == expected_len).then_some(out)
}

/// Append a back-reference, byte by byte so overlapping copies (offset < len)
/// read what earlier iterations just wrote.
fn copy_match(out: &mut Vec<u8>, offset: usize, len: usize) -> Option<()> {
    if offset == 0 || offset > out.len() {
        return None;
    }
    let start = out.len() - offset;
    for k in 0..len {
        out.push(out[start + k]);
    }
    Some(())
}

fn read_uvarint(input: &[u8]) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0u32;
    let mut pos = 0usize;
    loop {
        if pos >= input.len() {
            return None;
        }
        let byte = input[pos];
        result |= ((byte & 0x7f) as u64) << shift;
        pos += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    Some((result, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(data: &[u8]) {
        let compressed = compress(data);
        assert!(compressed.len() <= max_compressed_len(data.len()));
        let restored = decompress(&compressed).expect("valid snappy");
        assert_eq!(restored, data, "round trip mismatch (len {})", data.len());
    }

    #[test]
    fn empty() {
        round_trip(&[]);
        assert_eq!(compress(&[]), vec![0x00]); // just varint(0)
    }

    #[test]
    fn short_inputs_are_literal() {
        for n in 1..MIN_NON_LITERAL_BLOCK {
            round_trip(&vec![0xabu8; n]);
        }
        round_trip(b"hello");
    }

    #[test]
    fn highly_repetitive_compresses() {
        let data = vec![0x42u8; 10_000];
        let compressed = compress(&data);
        assert!(
            compressed.len() < data.len() / 10,
            "expected strong compression, got {} for {}",
            compressed.len(),
            data.len()
        );
        round_trip(&data);
    }

    #[test]
    fn repeated_pattern() {
        let mut data = Vec::new();
        while data.len() < 5000 {
            data.extend_from_slice(b"the quick brown fox jumps over the lazy dog. ");
        }
        round_trip(&data);
    }

    #[test]
    fn pseudo_random() {
        // Simple LCG so the test is deterministic without a dependency.
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut data = Vec::with_capacity(20_000);
        for _ in 0..20_000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            data.push((state >> 33) as u8);
        }
        round_trip(&data);
    }

    #[test]
    fn spans_multiple_fragments() {
        // Larger than one 64 KiB fragment, with structure to match on.
        let mut data = Vec::new();
        for i in 0..200_000u32 {
            data.extend_from_slice(&(i % 997).to_le_bytes());
        }
        round_trip(&data);
    }

    #[test]
    fn boundary_lengths() {
        for n in [15, 16, 17, 18, 59, 60, 61, 63, 64, 65, 66, 67, 68, 69, 255, 256, 257] {
            let mut data = Vec::with_capacity(n);
            for i in 0..n {
                data.push((i * 7 + 3) as u8);
            }
            round_trip(&data);
        }
    }
}
