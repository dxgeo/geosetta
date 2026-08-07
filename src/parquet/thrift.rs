//! A writer for the Thrift *compact* protocol — the encoding Parquet uses for
//! its `FileMetaData` and per-page `PageHeader` structures.
//!
//! Only the pieces Parquet needs are implemented: structs, i32/i64 (zigzag
//! varint), doubles, binary/strings, and lists of those. Field ids are
//! delta-encoded against the previous field within the current struct, so the
//! writer keeps a stack of "last field id" frames.
//!
//! A few protocol primitives (e.g. `field_bool`, `field_double`) are provided
//! for completeness even though the current writer doesn't emit them.
#![allow(dead_code)]

/// Compact-protocol type codes (the low nibble of a field header, and the
/// element type of a list header).
pub mod ct {
    pub const STOP: u8 = 0x00;
    pub const BOOL_TRUE: u8 = 0x01;
    pub const BOOL_FALSE: u8 = 0x02;
    pub const I32: u8 = 0x05;
    pub const I64: u8 = 0x06;
    pub const DOUBLE: u8 = 0x07;
    pub const BINARY: u8 = 0x08;
    pub const LIST: u8 = 0x09;
    pub const STRUCT: u8 = 0x0C;
}

/// Accumulates compact-protocol bytes.
pub struct CompactWriter {
    buf: Vec<u8>,
    /// Stack of the last field id written in each open struct.
    frames: Vec<i16>,
}

impl CompactWriter {
    pub fn new() -> Self {
        CompactWriter {
            buf: Vec::new(),
            frames: Vec::new(),
        }
    }

    /// Consume the writer, returning the encoded bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    // --- struct framing ---------------------------------------------------

    /// Open a struct: start a fresh field-id delta frame.
    pub fn struct_begin(&mut self) {
        self.frames.push(0);
    }

    /// Close a struct: emit the STOP byte and pop the frame.
    pub fn struct_end(&mut self) {
        self.buf.push(ct::STOP);
        self.frames.pop();
    }

    // --- fields -----------------------------------------------------------

    pub fn field_i32(&mut self, id: i16, v: i32) {
        self.field_header(id, ct::I32);
        self.write_zigzag(v as i64);
    }

    pub fn field_i64(&mut self, id: i16, v: i64) {
        self.field_header(id, ct::I64);
        self.write_zigzag(v);
    }

    pub fn field_double(&mut self, id: i16, v: f64) {
        self.field_header(id, ct::DOUBLE);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn field_bool(&mut self, id: i16, v: bool) {
        // The value lives in the field-header type nibble.
        let ty = if v { ct::BOOL_TRUE } else { ct::BOOL_FALSE };
        self.field_header(id, ty);
    }

    pub fn field_binary(&mut self, id: i16, v: &[u8]) {
        self.field_header(id, ct::BINARY);
        self.write_binary_raw(v);
    }

    pub fn field_string(&mut self, id: i16, v: &str) {
        self.field_binary(id, v.as_bytes());
    }

    /// Begin a struct-typed field. Follow with fields, then `struct_end`.
    pub fn field_struct(&mut self, id: i16) {
        self.field_header(id, ct::STRUCT);
        self.struct_begin();
    }

    /// Begin a list-typed field with `len` elements of compact type `elem`.
    /// Follow with `len` raw element writes.
    pub fn field_list(&mut self, id: i16, elem: u8, len: usize) {
        self.field_header(id, ct::LIST);
        self.list_header(elem, len);
    }

    // --- raw list elements (no field header) ------------------------------

    pub fn raw_i32(&mut self, v: i32) {
        self.write_zigzag(v as i64);
    }

    pub fn raw_string(&mut self, v: &str) {
        self.write_binary_raw(v.as_bytes());
    }

    // --- internals --------------------------------------------------------

    fn field_header(&mut self, id: i16, ty: u8) {
        let last = *self.frames.last().expect("field written outside a struct");
        let delta = id - last;
        if delta > 0 && delta <= 15 {
            self.buf.push(((delta as u8) << 4) | ty);
        } else {
            self.buf.push(ty);
            self.write_zigzag(id as i64);
        }
        *self.frames.last_mut().unwrap() = id;
    }

    fn list_header(&mut self, elem: u8, len: usize) {
        if len <= 14 {
            self.buf.push(((len as u8) << 4) | elem);
        } else {
            self.buf.push(0xF0 | elem);
            self.write_varint(len as u64);
        }
    }

    fn write_binary_raw(&mut self, v: &[u8]) {
        self.write_varint(v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    /// Unsigned LEB128 varint.
    fn write_varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                break;
            } else {
                self.buf.push(byte | 0x80);
            }
        }
    }

    /// Zigzag-encode then write as an unsigned varint.
    fn write_zigzag(&mut self, v: i64) {
        let zz = ((v << 1) ^ (v >> 63)) as u64;
        self.write_varint(zz);
    }
}

// --- reader ----------------------------------------------------------------

use crate::error::{Error, Result};

/// One field header read from a struct: either the STOP marker or a field.
pub enum Field {
    Stop,
    Begin { id: i16, ty: u8 },
}

/// Reads the compact-protocol structures Parquet writes (the inverse of
/// [`CompactWriter`]). The caller drives it against a known schema: open a
/// struct with [`struct_begin`], read fields with [`read_field`] until
/// [`Field::Stop`], then [`struct_end`]; skip fields it doesn't care about with
/// [`skip`].
///
/// [`struct_begin`]: CompactReader::struct_begin
/// [`read_field`]: CompactReader::read_field
/// [`struct_end`]: CompactReader::struct_end
/// [`skip`]: CompactReader::skip
pub struct CompactReader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Stack of the last field id read in each open struct (delta base).
    last_ids: Vec<i16>,
}

impl<'a> CompactReader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        CompactReader {
            buf,
            pos: 0,
            last_ids: vec![0],
        }
    }

    /// Bytes consumed so far (used to locate a page body after its header).
    pub fn position(&self) -> usize {
        self.pos
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or_else(Self::eof)?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(Self::eof)?;
        let s = self.buf.get(self.pos..end).ok_or_else(Self::eof)?;
        self.pos = end;
        Ok(s)
    }

    fn eof() -> Error {
        Error::Parquet("unexpected end of thrift data".into())
    }

    pub fn read_varint(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return Err(Error::Parquet("varint too long".into()));
            }
        }
        Ok(result)
    }

    pub fn read_zigzag(&mut self) -> Result<i64> {
        let u = self.read_varint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }

    pub fn struct_begin(&mut self) {
        self.last_ids.push(0);
    }

    pub fn struct_end(&mut self) {
        self.last_ids.pop();
    }

    /// Read the next field header within the current struct.
    pub fn read_field(&mut self) -> Result<Field> {
        let b = self.byte()?;
        if b == ct::STOP {
            return Ok(Field::Stop);
        }
        let ty = b & 0x0f;
        let delta = (b >> 4) as i16;
        let id = if delta == 0 {
            self.read_zigzag()? as i16
        } else {
            *self.last_ids.last().unwrap() + delta
        };
        *self.last_ids.last_mut().unwrap() = id;
        Ok(Field::Begin { id, ty })
    }

    pub fn read_i32(&mut self) -> Result<i32> {
        Ok(self.read_zigzag()? as i32)
    }

    pub fn read_i64(&mut self) -> Result<i64> {
        self.read_zigzag()
    }

    pub fn read_double(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn read_binary(&mut self) -> Result<Vec<u8>> {
        let n = self.read_varint()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    pub fn read_string(&mut self) -> Result<String> {
        String::from_utf8(self.read_binary()?)
            .map_err(|_| Error::Parquet("invalid utf-8 string".into()))
    }

    /// Read a list header, returning `(element_type, len)`.
    pub fn read_list_header(&mut self) -> Result<(u8, usize)> {
        let b = self.byte()?;
        let elem = b & 0x0f;
        let size = (b >> 4) as usize;
        let len = if size == 15 {
            self.read_varint()? as usize
        } else {
            size
        };
        Ok((elem, len))
    }

    /// Consume a value of compact type `ty` without interpreting it.
    pub fn skip(&mut self, ty: u8) -> Result<()> {
        match ty {
            ct::BOOL_TRUE | ct::BOOL_FALSE => {} // value lives in the header
            ct::I32 | ct::I64 => {
                self.read_varint()?;
            }
            ct::DOUBLE => {
                self.take(8)?;
            }
            ct::BINARY => {
                let n = self.read_varint()? as usize;
                self.take(n)?;
            }
            ct::LIST => {
                let (elem, len) = self.read_list_header()?;
                for _ in 0..len {
                    self.skip(elem)?;
                }
            }
            ct::STRUCT => {
                self.struct_begin();
                loop {
                    match self.read_field()? {
                        Field::Stop => break,
                        Field::Begin { ty, .. } => self.skip(ty)?,
                    }
                }
                self.struct_end();
            }
            other => return Err(Error::Parquet(format!("unknown compact type {other}"))),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(f: impl FnOnce(&mut CompactWriter)) -> Vec<u8> {
        let mut w = CompactWriter::new();
        f(&mut w);
        w.into_bytes()
    }

    #[test]
    fn varint_and_zigzag_known_values() {
        assert_eq!(bytes_of(|w| w.write_varint(0)), vec![0x00]);
        assert_eq!(bytes_of(|w| w.write_varint(127)), vec![0x7f]);
        assert_eq!(bytes_of(|w| w.write_varint(128)), vec![0x80, 0x01]);
        assert_eq!(bytes_of(|w| w.write_varint(300)), vec![0xac, 0x02]);
        // zigzag: 1 -> 2, -1 -> 1, -2 -> 3
        assert_eq!(bytes_of(|w| w.raw_i32(1)), vec![0x02]);
        assert_eq!(bytes_of(|w| w.raw_i32(-1)), vec![0x01]);
        assert_eq!(bytes_of(|w| w.raw_i32(-2)), vec![0x03]);
    }

    #[test]
    fn small_struct_i32_field() {
        let out = bytes_of(|w| {
            w.struct_begin();
            w.field_i32(1, 5);
            w.struct_end();
        });
        // header (delta 1 << 4 | I32=5)=0x15, value zigzag(5)=10=0x0a, STOP
        assert_eq!(out, vec![0x15, 0x0a, 0x00]);
    }

    #[test]
    fn string_field() {
        let out = bytes_of(|w| {
            w.struct_begin();
            w.field_string(1, "hi");
            w.struct_end();
        });
        // header (1<<4|BINARY=8)=0x18, len 2, 'h','i', STOP
        assert_eq!(out, vec![0x18, 0x02, 0x68, 0x69, 0x00]);
    }

    #[test]
    fn i32_list_field() {
        let out = bytes_of(|w| {
            w.struct_begin();
            w.field_list(1, ct::I32, 2);
            w.raw_i32(0);
            w.raw_i32(1);
            w.struct_end();
        });
        // field header (1<<4|LIST=9)=0x19, list header (2<<4|I32=5)=0x25,
        // then zigzag(0)=0, zigzag(1)=2, STOP
        assert_eq!(out, vec![0x19, 0x25, 0x00, 0x02, 0x00]);
    }

    #[test]
    fn long_field_id_uses_zigzag_form() {
        let out = bytes_of(|w| {
            w.struct_begin();
            // Jump by more than 15 so the short delta form can't be used.
            w.field_i32(20, 0);
            w.struct_end();
        });
        // type byte I32=0x05, field id zigzag(20)=40=0x28, value 0, STOP
        assert_eq!(out, vec![0x05, 0x28, 0x00, 0x00]);
    }

    #[test]
    fn reader_round_trips_writer() {
        // A small struct exercising i32, i64, string, a nested struct, and a
        // list — the shapes the footer/page parsers walk.
        let bytes = bytes_of(|w| {
            w.struct_begin();
            w.field_i32(1, 7);
            w.field_i64(3, -100);
            w.field_string(4, "geo");
            w.field_struct(5); // nested struct at id 5
            w.field_i32(1, 42);
            w.struct_end();
            w.field_list(6, ct::I32, 3);
            w.raw_i32(10);
            w.raw_i32(20);
            w.raw_i32(30);
            w.struct_end();
        });

        let mut r = CompactReader::new(&bytes);
        r.struct_begin();
        let mut seen = Vec::new();
        loop {
            match r.read_field().unwrap() {
                Field::Stop => break,
                Field::Begin { id, ty } => match id {
                    1 => seen.push(("i32", r.read_i32().unwrap() as i64)),
                    3 => seen.push(("i64", r.read_i64().unwrap())),
                    4 => {
                        assert_eq!(r.read_string().unwrap(), "geo");
                        seen.push(("str", 0));
                    }
                    5 => {
                        // Nested struct: one i32 field then STOP.
                        r.struct_begin();
                        if let Field::Begin { ty, .. } = r.read_field().unwrap() {
                            assert_eq!(ty, ct::I32);
                            seen.push(("nested", r.read_i32().unwrap() as i64));
                        }
                        assert!(matches!(r.read_field().unwrap(), Field::Stop));
                        r.struct_end();
                    }
                    6 => {
                        let (elem, len) = r.read_list_header().unwrap();
                        assert_eq!(elem, ct::I32);
                        let sum: i64 = (0..len).map(|_| r.read_i32().unwrap() as i64).sum();
                        seen.push(("list_sum", sum));
                    }
                    _ => r.skip(ty).unwrap(),
                },
            }
        }
        r.struct_end();
        assert_eq!(
            seen,
            vec![
                ("i32", 7),
                ("i64", -100),
                ("str", 0),
                ("nested", 42),
                ("list_sum", 60),
            ]
        );
        assert_eq!(r.position(), bytes.len());
    }

    #[test]
    fn skip_consumes_whole_value() {
        let bytes = bytes_of(|w| {
            w.struct_begin();
            w.field_string(1, "skip me");
            w.field_i32(2, 99);
            w.struct_end();
        });
        let mut r = CompactReader::new(&bytes);
        r.struct_begin();
        // Skip the string field, then read the i32.
        match r.read_field().unwrap() {
            Field::Begin { ty, .. } => r.skip(ty).unwrap(),
            Field::Stop => panic!(),
        }
        match r.read_field().unwrap() {
            Field::Begin { id, .. } => assert_eq!((id, r.read_i32().unwrap()), (2, 99)),
            Field::Stop => panic!(),
        }
        assert!(matches!(r.read_field().unwrap(), Field::Stop));
    }
}
