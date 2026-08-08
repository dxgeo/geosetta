//! LZ4 block decompression (the `LZ4_RAW` Parquet codec), implemented from the
//! LZ4 block format spec rather than a crate — keeping the project
//! dependency-free. Decoder only.
//!
//! A block is a series of sequences. Each begins with a token byte: the high
//! nibble is the literal length, the low nibble is the match length minus the
//! 4-byte minimum. A nibble value of 15 means "read more length bytes" (each
//! 0..255, summed, terminated by a byte < 255). After the literals comes a
//! 2-byte little-endian match offset (distance back into the output) unless the
//! sequence is the final, literals-only one at the end of the block.

use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Parquet(format!("lz4: {msg}")))
}

const MIN_MATCH: usize = 4;

/// Decompress one raw LZ4 block. `expected_size` is the known output length
/// from the Parquet page header and bounds the output.
pub fn decompress(input: &[u8], expected_size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected_size);
    let mut ip = 0usize;

    while ip < input.len() {
        let token = input[ip];
        ip += 1;

        // Literals.
        let mut lit_len = (token >> 4) as usize;
        if lit_len == 15 {
            lit_len += read_length(input, &mut ip)?;
        }
        let lits = input
            .get(ip..ip + lit_len)
            .ok_or_else(|| Error::Parquet("lz4: literal run out of range".into()))?;
        out.extend_from_slice(lits);
        ip += lit_len;

        // The final sequence is literals only and ends the block.
        if ip == input.len() {
            break;
        }

        // Match: 2-byte little-endian offset, then the length.
        let offset = *input.get(ip).ok_or_else(bad)? as usize
            | (*input.get(ip + 1).ok_or_else(bad)? as usize) << 8;
        ip += 2;
        if offset == 0 || offset > out.len() {
            return err("match offset out of range");
        }

        let mut match_len = (token & 0x0f) as usize + MIN_MATCH;
        if (token & 0x0f) == 15 {
            match_len += read_length(input, &mut ip)?;
        }

        // Copy byte-by-byte so overlapping matches (offset < length) work.
        let start = out.len() - offset;
        for k in 0..match_len {
            let b = out[start + k];
            out.push(b);
        }
    }

    if out.len() != expected_size {
        return err("decompressed size does not match the page header");
    }
    Ok(out)
}

fn bad() -> Error {
    Error::Parquet("lz4: truncated block".into())
}

/// Read a variable-length extension: bytes of 255 accumulate, a final byte
/// below 255 terminates. Advances `ip`.
fn read_length(input: &[u8], ip: &mut usize) -> Result<usize> {
    let mut extra = 0usize;
    loop {
        let b = *input.get(*ip).ok_or_else(bad)?;
        *ip += 1;
        extra += b as usize;
        if b != 255 {
            return Ok(extra);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a literals-only block (no matches) for a simple round-trip check.
    fn literals_only_block(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let n = data.len();
        let token = if n >= 15 { 0xf0 } else { (n as u8) << 4 };
        out.push(token);
        if n >= 15 {
            let mut rem = n - 15;
            while rem >= 255 {
                out.push(255);
                rem -= 255;
            }
            out.push(rem as u8);
        }
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn decodes_literals_only() {
        for n in [0usize, 1, 5, 14, 15, 16, 270, 600] {
            let data: Vec<u8> = (0..n).map(|i| (i * 7 + 1) as u8).collect();
            let block = literals_only_block(&data);
            assert_eq!(decompress(&block, n).unwrap(), data);
        }
    }

    #[test]
    fn decodes_a_match() {
        // "abcabcabc": 3 literals "abc", then a match of length 6 at offset 3
        // (overlapping copy).
        // token: litlen=3 (0x30), matchlen = 6-4 = 2 (0x02) -> 0x32
        let block = [0x32u8, b'a', b'b', b'c', 0x03, 0x00];
        let out = decompress(&block, 9).unwrap();
        assert_eq!(&out, b"abcabcabc");
    }

    #[test]
    fn rejects_bad_offset() {
        // Offset 5 with only 3 bytes of output so far.
        let block = [0x32u8, b'a', b'b', b'c', 0x05, 0x00];
        assert!(decompress(&block, 9).is_err());
    }
}
