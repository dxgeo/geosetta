//! A minimal FlatBuffers reader, implemented from the FlatBuffers binary format
//! rather than a crate (keeping the project dependency-free). Read only.
//!
//! Provenance / licensing: this is an independent implementation of the
//! FlatBuffers *binary encoding* (vtables, offsets, alignment, vectors,
//! strings, tables), written from the format description. It is **not** derived
//! from Google's `flatbuffers` library. FlatBuffers is Apache-2.0 licensed,
//! which is permissive and compatible with this project's MIT license; the wire
//! format itself is an interface, not a copyrightable work.
//!
//! Layout recap: a table is located by a `uoffset` (u32, forward). It starts
//! with an `soffset` (i32) pointing *backward* to its vtable. The vtable is
//! `[u16 vtable_size][u16 table_size][u16 field_offset]*`; a field's value sits
//! at `table_pos + field_offset` (offset 0 ⇒ field absent). Scalars are inline;
//! strings, vectors, and sub-tables are reached through a further `uoffset`.

use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::FlatGeobuf(format!("flatbuffers: {msg}")))
}

fn read_u16(buf: &[u8], at: usize) -> Result<u16> {
    let b = buf.get(at..at + 2).ok_or_else(oob)?;
    Ok(u16::from_le_bytes(b.try_into().unwrap()))
}

fn read_u32(buf: &[u8], at: usize) -> Result<u32> {
    let b = buf.get(at..at + 4).ok_or_else(oob)?;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

fn read_i32(buf: &[u8], at: usize) -> Result<i32> {
    let b = buf.get(at..at + 4).ok_or_else(oob)?;
    Ok(i32::from_le_bytes(b.try_into().unwrap()))
}

fn read_u64(buf: &[u8], at: usize) -> Result<u64> {
    let b = buf.get(at..at + 8).ok_or_else(oob)?;
    Ok(u64::from_le_bytes(b.try_into().unwrap()))
}

fn read_f64(buf: &[u8], at: usize) -> Result<f64> {
    let b = buf.get(at..at + 8).ok_or_else(oob)?;
    Ok(f64::from_le_bytes(b.try_into().unwrap()))
}

fn oob() -> Error {
    Error::FlatGeobuf("flatbuffers: read out of bounds".into())
}

/// A view onto a FlatBuffers table.
#[derive(Clone, Copy)]
pub struct Table<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Table<'a> {
    /// The root table of a buffer (root `uoffset` sits at `buf[0..4]`).
    pub fn root(buf: &'a [u8]) -> Result<Table<'a>> {
        let off = read_u32(buf, 0)? as usize;
        Table::at(buf, off)
    }

    fn at(buf: &'a [u8], pos: usize) -> Result<Table<'a>> {
        if pos + 4 > buf.len() {
            return err("table position out of range");
        }
        Ok(Table { buf, pos })
    }

    /// Absolute byte position of field `i`'s value, or `None` if absent.
    fn field(&self, i: usize) -> Result<Option<usize>> {
        let soffset = read_i32(self.buf, self.pos)?;
        let vtable = (self.pos as i64 - soffset as i64) as usize;
        let vtable_size = read_u16(self.buf, vtable)? as usize;
        let slot = 4 + i * 2;
        if slot >= vtable_size {
            return Ok(None); // field beyond this vtable
        }
        let voffset = read_u16(self.buf, vtable + slot)? as usize;
        if voffset == 0 {
            return Ok(None); // field not set
        }
        Ok(Some(self.pos + voffset))
    }

    pub fn read_u8(&self, i: usize, default: u8) -> Result<u8> {
        match self.field(i)? {
            Some(p) => self.buf.get(p).copied().ok_or_else(oob),
            None => Ok(default),
        }
    }

    pub fn read_u16(&self, i: usize, default: u16) -> Result<u16> {
        match self.field(i)? {
            Some(p) => read_u16(self.buf, p),
            None => Ok(default),
        }
    }

    /// Read a signed 32-bit scalar field.
    pub fn read_i32(&self, i: usize, default: i32) -> Result<i32> {
        match self.field(i)? {
            Some(p) => read_u32(self.buf, p).map(|v| v as i32),
            None => Ok(default),
        }
    }

    pub fn read_u64(&self, i: usize, default: u64) -> Result<u64> {
        match self.field(i)? {
            Some(p) => read_u64(self.buf, p),
            None => Ok(default),
        }
    }

    /// Read a sub-table field.
    pub fn read_table(&self, i: usize) -> Result<Option<Table<'a>>> {
        match self.field(i)? {
            Some(p) => {
                let off = read_u32(self.buf, p)? as usize;
                Ok(Some(Table::at(self.buf, p + off)?))
            }
            None => Ok(None),
        }
    }

    /// Read a string field as UTF-8.
    pub fn read_str(&self, i: usize) -> Result<Option<&'a str>> {
        match self.field(i)? {
            Some(p) => {
                let off = read_u32(self.buf, p)? as usize;
                let sp = p + off;
                let len = read_u32(self.buf, sp)? as usize;
                let bytes = self.buf.get(sp + 4..sp + 4 + len).ok_or_else(oob)?;
                std::str::from_utf8(bytes)
                    .map(Some)
                    .map_err(|_| Error::FlatGeobuf("flatbuffers: invalid utf-8 string".into()))
            }
            None => Ok(None),
        }
    }

    /// Read a vector field; `None` if absent.
    pub fn read_vector(&self, i: usize) -> Result<Option<Vector<'a>>> {
        match self.field(i)? {
            Some(p) => {
                let off = read_u32(self.buf, p)? as usize;
                let vp = p + off;
                let len = read_u32(self.buf, vp)? as usize;
                Ok(Some(Vector {
                    buf: self.buf,
                    start: vp + 4,
                    len,
                }))
            }
            None => Ok(None),
        }
    }
}

/// A view onto a FlatBuffers vector. Element access is typed by the caller.
#[derive(Clone, Copy)]
pub struct Vector<'a> {
    buf: &'a [u8],
    start: usize,
    len: usize,
}

impl<'a> Vector<'a> {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A vector of `u8` as a byte slice (e.g. a `[ubyte]` properties blob).
    pub fn bytes(&self) -> Result<&'a [u8]> {
        self.buf.get(self.start..self.start + self.len).ok_or_else(oob)
    }

    pub fn get_u32(&self, i: usize) -> Result<u32> {
        self.check(i)?;
        read_u32(self.buf, self.start + i * 4)
    }

    pub fn get_f64(&self, i: usize) -> Result<f64> {
        self.check(i)?;
        read_f64(self.buf, self.start + i * 8)
    }

    /// Element `i` of a vector of tables (each element is a `uoffset`).
    pub fn get_table(&self, i: usize) -> Result<Table<'a>> {
        self.check(i)?;
        let p = self.start + i * 4;
        let off = read_u32(self.buf, p)? as usize;
        Table::at(self.buf, p + off)
    }

    fn check(&self, i: usize) -> Result<()> {
        if i >= self.len {
            return err("vector index out of range");
        }
        Ok(())
    }
}

// --- writer -----------------------------------------------------------------

/// A minimal FlatBuffers builder. Builds back-to-front (objects are written
/// before the tables that reference them), tracking each object's position as a
/// "rev-offset" (bytes written so far); a `uoffset` from a later field to an
/// earlier object is `field_rev - target_rev`.
pub struct Builder {
    /// The logical buffer is `buf[head..]`; new bytes are prepended (head--).
    buf: Vec<u8>,
    head: usize,
    min_align: usize,
    /// Field rev-offsets for the table currently being built.
    vtable: Vec<u16>,
    object_end: usize,
}

impl Builder {
    pub fn new() -> Builder {
        Builder {
            buf: vec![0; 1024],
            head: 1024,
            min_align: 1,
            vtable: Vec::new(),
            object_end: 0,
        }
    }

    /// Bytes written so far (the rev-offset of the current head).
    fn offset(&self) -> usize {
        self.buf.len() - self.head
    }

    fn ensure(&mut self, n: usize) {
        if self.head < n {
            let used = self.offset();
            let new_len = (self.buf.len() * 2).max(used + n);
            let mut nb = vec![0u8; new_len];
            nb[new_len - used..].copy_from_slice(&self.buf[self.head..]);
            self.buf = nb;
            self.head = new_len - used;
        }
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        self.ensure(bytes.len());
        self.head -= bytes.len();
        self.buf[self.head..self.head + bytes.len()].copy_from_slice(bytes);
    }

    /// Pad so a following `size`-byte write (after `additional` more bytes) is
    /// aligned to `size`.
    fn prep(&mut self, size: usize, additional: usize) {
        if size > self.min_align {
            self.min_align = size;
        }
        let pad = (self.offset() + additional).wrapping_neg() & (size - 1);
        self.ensure(pad + size + additional);
        for _ in 0..pad {
            self.head -= 1;
            self.buf[self.head] = 0;
        }
    }

    fn push_u8(&mut self, v: u8) {
        self.prep(1, 0);
        self.write_raw(&[v]);
    }
    fn push_u16(&mut self, v: u16) {
        self.prep(2, 0);
        self.write_raw(&v.to_le_bytes());
    }
    fn push_u32(&mut self, v: u32) {
        self.prep(4, 0);
        self.write_raw(&v.to_le_bytes());
    }
    fn push_i32(&mut self, v: i32) {
        self.prep(4, 0);
        self.write_raw(&v.to_le_bytes());
    }
    fn push_u64(&mut self, v: u64) {
        self.prep(8, 0);
        self.write_raw(&v.to_le_bytes());
    }

    fn push_uoffset(&mut self, target: usize) {
        self.prep(4, 0);
        let value = (self.offset() + 4 - target) as u32;
        self.write_raw(&value.to_le_bytes());
    }

    // --- strings & vectors (return their rev-offset) ---

    pub fn create_string(&mut self, s: &str) -> usize {
        let bytes = s.as_bytes();
        self.push_u8(0); // null terminator
        self.prep(4, bytes.len());
        self.write_raw(bytes); // bytes[0] lands at the lowest address
        self.push_u32(bytes.len() as u32);
        self.offset()
    }

    pub fn create_byte_vector(&mut self, bytes: &[u8]) -> usize {
        self.prep(4, bytes.len());
        self.write_raw(bytes);
        self.push_u32(bytes.len() as u32);
        self.offset()
    }

    pub fn create_f64_vector(&mut self, vals: &[f64]) -> usize {
        let mut block = Vec::with_capacity(vals.len() * 8);
        for &v in vals {
            block.extend_from_slice(&v.to_le_bytes());
        }
        self.prep(4, block.len());
        self.prep(8, block.len());
        self.write_raw(&block);
        self.push_u32(vals.len() as u32);
        self.offset()
    }

    pub fn create_u32_vector(&mut self, vals: &[u32]) -> usize {
        let mut block = Vec::with_capacity(vals.len() * 4);
        for &v in vals {
            block.extend_from_slice(&v.to_le_bytes());
        }
        self.prep(4, block.len());
        self.write_raw(&block);
        self.push_u32(vals.len() as u32);
        self.offset()
    }

    /// A vector of table offsets (each `target` is a rev-offset).
    pub fn create_offset_vector(&mut self, targets: &[usize]) -> usize {
        self.prep(4, targets.len() * 4);
        for &t in targets.iter().rev() {
            self.push_uoffset(t);
        }
        self.push_u32(targets.len() as u32);
        self.offset()
    }

    // --- tables ---

    pub fn start_table(&mut self, num_fields: usize) {
        self.vtable = vec![0u16; num_fields];
        self.object_end = self.offset();
    }

    fn slot(&mut self, field: usize) {
        self.vtable[field] = self.offset() as u16;
    }

    pub fn add_u8(&mut self, field: usize, v: u8, default: u8) {
        if v != default {
            self.push_u8(v);
            self.slot(field);
        }
    }
    pub fn add_u16(&mut self, field: usize, v: u16, default: u16) {
        if v != default {
            self.push_u16(v);
            self.slot(field);
        }
    }
    pub fn add_u64(&mut self, field: usize, v: u64, default: u64) {
        if v != default {
            self.push_u64(v);
            self.slot(field);
        }
    }
    pub fn add_i32(&mut self, field: usize, v: i32, default: i32) {
        if v != default {
            self.push_i32(v);
            self.slot(field);
        }
    }
    /// Add a reference field (string / vector / sub-table) by its rev-offset.
    pub fn add_offset(&mut self, field: usize, target: usize) {
        if target != 0 {
            self.push_uoffset(target);
            self.slot(field);
        }
    }

    /// Finish the current table, writing its vtable; returns its rev-offset.
    pub fn end_table(&mut self) -> usize {
        self.push_i32(0); // soffset placeholder (patched below)
        let table_rev = self.offset();

        let mut n = self.vtable.len();
        while n > 0 && self.vtable[n - 1] == 0 {
            n -= 1;
        }
        for i in (0..n).rev() {
            let vo = if self.vtable[i] != 0 {
                (table_rev - self.vtable[i] as usize) as u16
            } else {
                0
            };
            self.push_u16(vo);
        }
        self.push_u16((table_rev - self.object_end) as u16); // table inline size
        self.push_u16(((n + 2) * 2) as u16); // vtable size

        let vtable_rev = self.offset();
        let soffset = (vtable_rev as i32) - (table_rev as i32);
        let pos = self.buf.len() - table_rev;
        self.buf[pos..pos + 4].copy_from_slice(&soffset.to_le_bytes());
        table_rev
    }

    /// Finish the buffer with `root` (a table rev-offset) as the root, returning
    /// the finished bytes.
    pub fn finish(mut self, root: usize) -> Vec<u8> {
        self.prep(self.min_align.max(4), 4);
        self.push_uoffset(root);
        self.buf[self.head..].to_vec()
    }
}

impl Default for Builder {
    fn default() -> Self {
        Builder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A hand-built buffer for a table with:
    //   field 0: u16 = 0x1234
    //   field 1: string = "hi"
    //   field 2: [double] = [1.5, 2.5]
    // Referenced objects (string, vector) come *after* the table, since
    // FlatBuffers uoffsets only point forward.
    fn sample() -> Vec<u8> {
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(&[0, 0, 0, 0]); // 0: root uoffset (patched below)

        // vtable at 4. Table inline layout: soffset(+0), field0 u16(+4), pad(+6),
        // field1 uoffset(+8), field2 uoffset(+12); inline size 16.
        b.extend_from_slice(&10u16.to_le_bytes()); // 4: vtable_size (2 hdr + 3 fields)
        b.extend_from_slice(&16u16.to_le_bytes()); // 6: table inline size
        b.extend_from_slice(&4u16.to_le_bytes()); // 8: field0 at table+4
        b.extend_from_slice(&8u16.to_le_bytes()); // 10: field1 at table+8
        b.extend_from_slice(&12u16.to_le_bytes()); // 12: field2 at table+12

        // table at 14.
        let table_pos = b.len();
        b.extend_from_slice(&((table_pos as i32 - 4).to_le_bytes())); // 14: soffset -> vtable
        b.extend_from_slice(&0x1234u16.to_le_bytes()); // 18: field0
        b.extend_from_slice(&[0, 0]); // 20: pad
        let f1 = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]); // 22: field1 uoffset (patched)
        let f2 = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]); // 26: field2 uoffset (patched)

        // string "hi" after the table.
        let str_pos = b.len();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(b"hi\0\0");

        // [double] vector after the string.
        let vec_pos = b.len();
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&1.5f64.to_le_bytes());
        b.extend_from_slice(&2.5f64.to_le_bytes());

        b[0..4].copy_from_slice(&(table_pos as u32).to_le_bytes());
        b[f1..f1 + 4].copy_from_slice(&((str_pos - f1) as u32).to_le_bytes());
        b[f2..f2 + 4].copy_from_slice(&((vec_pos - f2) as u32).to_le_bytes());
        b
    }

    #[test]
    fn reads_scalar_string_and_vector() {
        let buf = sample();
        let t = Table::root(&buf).unwrap();
        assert_eq!(t.read_u16(0, 0).unwrap(), 0x1234);
        assert_eq!(t.read_str(1).unwrap(), Some("hi"));
        let v = t.read_vector(2).unwrap().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v.get_f64(0).unwrap(), 1.5);
        assert_eq!(v.get_f64(1).unwrap(), 2.5);
        // An absent field falls back to its default.
        assert_eq!(t.read_u16(7, 42).unwrap(), 42);
        assert!(t.read_str(5).unwrap().is_none());
    }

    #[test]
    fn writer_round_trips_through_reader() {
        // Build a table: field0 u8, field1 u64, field2 string, field3 [f64],
        // field4 [u32], plus a field5 sub-table with one u16.
        let mut b = Builder::new();
        let sub = {
            b.start_table(1);
            b.add_u16(0, 0xbeef, 0);
            b.end_table()
        };
        let s = b.create_string("héllo ☕");
        let xy = b.create_f64_vector(&[1.0, -2.5, 3.25]);
        let ends = b.create_u32_vector(&[7, 42]);

        b.start_table(6);
        b.add_u8(0, 200, 0);
        b.add_u64(1, 0x0123_4567_89ab_cdef, 0);
        b.add_offset(2, s);
        b.add_offset(3, xy);
        b.add_offset(4, ends);
        b.add_offset(5, sub);
        let root = b.end_table();
        let buf = b.finish(root);

        let t = Table::root(&buf).unwrap();
        assert_eq!(t.read_u8(0, 0).unwrap(), 200);
        assert_eq!(t.read_u64(1, 0).unwrap(), 0x0123_4567_89ab_cdef);
        assert_eq!(t.read_str(2).unwrap(), Some("héllo ☕"));
        let xy = t.read_vector(3).unwrap().unwrap();
        assert_eq!(
            (xy.get_f64(0).unwrap(), xy.get_f64(1).unwrap(), xy.get_f64(2).unwrap()),
            (1.0, -2.5, 3.25)
        );
        let ends = t.read_vector(4).unwrap().unwrap();
        assert_eq!((ends.get_u32(0).unwrap(), ends.get_u32(1).unwrap()), (7, 42));
        let sub = t.read_table(5).unwrap().unwrap();
        assert_eq!(sub.read_u16(0, 0).unwrap(), 0xbeef);
    }

    #[test]
    fn writer_offset_vector_of_tables() {
        // A vector of two sub-tables, each carrying a distinct u8.
        let mut b = Builder::new();
        let a = {
            b.start_table(1);
            b.add_u8(0, 11, 0);
            b.end_table()
        };
        let c = {
            b.start_table(1);
            b.add_u8(0, 22, 0);
            b.end_table()
        };
        let vec = b.create_offset_vector(&[a, c]);
        b.start_table(1);
        b.add_offset(0, vec);
        let root = b.end_table();
        let buf = b.finish(root);

        let t = Table::root(&buf).unwrap();
        let v = t.read_vector(0).unwrap().unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v.get_table(0).unwrap().read_u8(0, 0).unwrap(), 11);
        assert_eq!(v.get_table(1).unwrap().read_u8(0, 0).unwrap(), 22);
    }
}
