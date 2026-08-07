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
}
