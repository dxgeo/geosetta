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
}
