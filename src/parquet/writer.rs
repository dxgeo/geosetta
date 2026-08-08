//! Assemble a complete GeoParquet file in memory.
//!
//! Layout produced:
//! ```text
//! "PAR1"                       magic
//! <data page> * N              one Snappy-compressed DATA_PAGE per column
//! <FileMetaData>               Thrift compact protocol
//! <u32 le>                     length of FileMetaData
//! "PAR1"                       magic
//! ```
//! Every column is OPTIONAL (max definition level 1, no repetition levels),
//! so each page body is `[u32 len][RLE definition levels][PLAIN values]`.

use super::geo::GEOMETRY_COLUMN;
use crate::schema::{Cell, Column, ColumnType};
use crate::compress::snappy;
use super::thrift::{ct, CompactWriter};
use super::types::{codec, converted, encoding, page, ptype, repetition};

const MAGIC: &[u8; 4] = b"PAR1";
/// Format version recorded in `FileMetaData` (data pages are v1).
const FORMAT_VERSION: i32 = 1;

/// A column reduced to its physical type plus per-row definition levels and
/// PLAIN-encoded present values.
struct Prepared {
    name: String,
    ptype: i32,
    num_values: usize,
    /// One byte per row: 1 = present, 0 = null.
    def_levels: Vec<u8>,
    /// PLAIN-encoded values, present rows only.
    values: Vec<u8>,
}

/// Where a written column chunk landed, for the footer. Sizes include the page
/// header (which is never compressed) and the page body.
struct ChunkInfo {
    name: String,
    ptype: i32,
    data_page_offset: i64,
    compressed_size: i64,
    uncompressed_size: i64,
    num_values: i64,
}

/// Build a GeoParquet file from property `columns`, a per-row `geometry` WKB
/// column, and the pre-rendered `geo_metadata` JSON.
pub fn write_geoparquet(
    columns: &[Column],
    geometry: &[Option<Vec<u8>>],
    geo_metadata: &str,
) -> Vec<u8> {
    let num_rows = geometry.len() as i64;

    // Property columns first, geometry last (GeoParquet's primary column).
    let mut prepared: Vec<Prepared> = columns.iter().map(prepare_property).collect();
    prepared.push(prepare_geometry(geometry));

    let mut file: Vec<u8> = Vec::new();
    file.extend_from_slice(MAGIC);

    let mut infos: Vec<ChunkInfo> = Vec::with_capacity(prepared.len());
    for pc in &prepared {
        infos.push(write_column_page(&mut file, pc));
    }

    let total_byte_size: i64 = infos.iter().map(|c| c.uncompressed_size).sum();
    let footer = build_file_metadata(&infos, num_rows, total_byte_size, geo_metadata);

    file.extend_from_slice(&footer);
    file.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    file.extend_from_slice(MAGIC);
    file
}

// --- column preparation ---------------------------------------------------

fn prepare_property(col: &Column) -> Prepared {
    let ptype = match col.ty {
        ColumnType::Bool => ptype::BOOLEAN,
        ColumnType::Int64 => ptype::INT64,
        ColumnType::Double => ptype::DOUBLE,
        ColumnType::String => ptype::BYTE_ARRAY,
    };

    let mut def = Vec::with_capacity(col.values.len());
    let mut values = Vec::new();
    // Booleans are bit-packed together, so collect them separately.
    let mut bits: Vec<bool> = Vec::new();

    for cell in &col.values {
        match (col.ty, cell) {
            (_, Cell::Null) => def.push(0),
            (ColumnType::Bool, Cell::Bool(b)) => {
                def.push(1);
                bits.push(*b);
            }
            (ColumnType::Int64, Cell::Int(n)) => {
                def.push(1);
                values.extend_from_slice(&n.to_le_bytes());
            }
            (ColumnType::Double, Cell::Double(d)) => {
                def.push(1);
                values.extend_from_slice(&d.to_le_bytes());
            }
            (ColumnType::String, Cell::Str(s)) => {
                def.push(1);
                plain_byte_array(&mut values, s.as_bytes());
            }
            // Type/cell mismatches cannot arise from `infer_columns`, but be
            // safe and treat them as null.
            _ => def.push(0),
        }
    }

    if col.ty == ColumnType::Bool {
        values = pack_bits(&bits);
    }

    Prepared {
        name: col.name.clone(),
        ptype,
        num_values: col.values.len(),
        def_levels: def,
        values,
    }
}

fn prepare_geometry(geometry: &[Option<Vec<u8>>]) -> Prepared {
    let mut def = Vec::with_capacity(geometry.len());
    let mut values = Vec::new();
    for g in geometry {
        match g {
            Some(wkb) => {
                def.push(1);
                plain_byte_array(&mut values, wkb);
            }
            None => def.push(0),
        }
    }
    Prepared {
        name: GEOMETRY_COLUMN.to_string(),
        ptype: ptype::BYTE_ARRAY,
        num_values: geometry.len(),
        def_levels: def,
        values,
    }
}

/// PLAIN BYTE_ARRAY value: 4-byte little-endian length, then the bytes.
fn plain_byte_array(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// PLAIN BOOLEAN: one bit per value, LSB first, zero-padded to whole bytes.
fn pack_bits(bits: &[bool]) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, b) in bits.iter().enumerate() {
        if *b {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    out
}

// --- page + footer serialization ------------------------------------------

/// Append one Snappy-compressed DATA_PAGE (header + body) for `pc` and report
/// its placement.
fn write_column_page(file: &mut Vec<u8>, pc: &Prepared) -> ChunkInfo {
    let data_page_offset = file.len() as i64;

    // Body = [u32 len][RLE definition levels (bit width 1)][PLAIN values].
    let mut body = Vec::new();
    let rle = encode_rle_levels(&pc.def_levels, 1);
    body.extend_from_slice(&(rle.len() as u32).to_le_bytes());
    body.extend_from_slice(&rle);
    body.extend_from_slice(&pc.values);

    // The codec applies to the page body only; the header stays uncompressed.
    let compressed = snappy::compress(&body);
    let uncompressed_page_size = body.len() as i32;
    let compressed_page_size = compressed.len() as i32;

    // PageHeader (compact protocol).
    let mut h = CompactWriter::new();
    h.struct_begin();
    h.field_i32(1, page::DATA_PAGE); // type
    h.field_i32(2, uncompressed_page_size); // uncompressed_page_size
    h.field_i32(3, compressed_page_size); // compressed_page_size
    h.field_struct(5); // data_page_header
    h.field_i32(1, pc.num_values as i32);
    h.field_i32(2, encoding::PLAIN); // value encoding
    h.field_i32(3, encoding::RLE); // definition level encoding
    h.field_i32(4, encoding::RLE); // repetition level encoding
    h.struct_end(); // data_page_header
    h.struct_end(); // PageHeader
    let header = h.into_bytes();

    file.extend_from_slice(&header);
    file.extend_from_slice(&compressed);

    ChunkInfo {
        name: pc.name.clone(),
        ptype: pc.ptype,
        data_page_offset,
        compressed_size: (header.len() + compressed.len()) as i64,
        uncompressed_size: (header.len() + body.len()) as i64,
        num_values: pc.num_values as i64,
    }
}

fn build_file_metadata(
    infos: &[ChunkInfo],
    num_rows: i64,
    total_byte_size: i64,
    geo_metadata: &str,
) -> Vec<u8> {
    let prepared = &infos; // alias for readability
    let mut w = CompactWriter::new();
    w.struct_begin();

    w.field_i32(1, FORMAT_VERSION);

    // schema: root element followed by one leaf per column.
    w.field_list(2, ct::STRUCT, 1 + prepared.len());
    // Root.
    w.struct_begin();
    w.field_string(4, "schema");
    w.field_i32(5, prepared.len() as i32); // num_children
    w.struct_end();
    // Leaves.
    for c in *prepared {
        let converted = converted_type_for(c);
        w.struct_begin();
        w.field_i32(1, c.ptype); // type
        w.field_i32(3, repetition::OPTIONAL); // repetition_type
        w.field_string(4, &c.name); // name
        if let Some(conv) = converted {
            w.field_i32(6, conv); // converted_type (UTF8 for strings)
        }
        w.struct_end();
    }

    w.field_i64(3, num_rows);

    // row_groups: exactly one.
    w.field_list(4, ct::STRUCT, 1);
    w.struct_begin();
    // columns
    w.field_list(1, ct::STRUCT, prepared.len());
    for c in *prepared {
        w.struct_begin();
        w.field_i64(2, c.data_page_offset); // file_offset
        w.field_struct(3); // meta_data (ColumnMetaData)
        w.field_i32(1, c.ptype); // type
        w.field_list(2, ct::I32, 2); // encodings
        w.raw_i32(encoding::PLAIN);
        w.raw_i32(encoding::RLE);
        w.field_list(3, ct::BINARY, 1); // path_in_schema
        w.raw_string(&c.name);
        w.field_i32(4, codec::SNAPPY); // codec
        w.field_i64(5, c.num_values); // num_values
        w.field_i64(6, c.uncompressed_size); // total_uncompressed_size
        w.field_i64(7, c.compressed_size); // total_compressed_size
        w.field_i64(9, c.data_page_offset); // data_page_offset
        w.struct_end(); // ColumnMetaData
        w.struct_end(); // ColumnChunk
    }
    w.field_i64(2, total_byte_size);
    w.field_i64(3, num_rows);
    w.struct_end(); // RowGroup

    // key_value_metadata: the GeoParquet "geo" entry.
    w.field_list(5, ct::STRUCT, 1);
    w.struct_begin();
    w.field_string(1, "geo");
    w.field_string(2, geo_metadata);
    w.struct_end();

    w.field_string(6, "pantograph");
    w.struct_end(); // FileMetaData
    w.into_bytes()
}

/// String columns carry the UTF8 converted type; everything else none.
fn converted_type_for(c: &ChunkInfo) -> Option<i32> {
    if c.ptype == ptype::BYTE_ARRAY && c.name != GEOMETRY_COLUMN {
        Some(converted::UTF8)
    } else {
        None
    }
}

// --- RLE definition levels ------------------------------------------------

/// Encode definition `levels` with the RLE/bit-pack hybrid, using only RLE
/// runs (each maximal run of equal levels becomes one run). `bit_width` is 1
/// for our optional-only schema.
fn encode_rle_levels(levels: &[u8], bit_width: u32) -> Vec<u8> {
    let value_bytes = (bit_width as usize).div_ceil(8);
    let mut out = Vec::new();
    let mut i = 0;
    while i < levels.len() {
        let v = levels[i];
        let mut run = 1usize;
        while i + run < levels.len() && levels[i + run] == v {
            run += 1;
        }
        // RLE run header: (run_length << 1) as unsigned varint (low bit 0).
        write_uvarint(&mut out, (run as u64) << 1);
        let vv = v as u64;
        for b in 0..value_bytes {
            out.push(((vv >> (8 * b)) & 0xff) as u8);
        }
        i += run;
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rle_levels_single_run() {
        // Five present rows -> one RLE run of value 1.
        let out = encode_rle_levels(&[1, 1, 1, 1, 1], 1);
        // header varint(5<<1=10)=0x0a, value byte 0x01
        assert_eq!(out, vec![0x0a, 0x01]);
    }

    #[test]
    fn rle_levels_alternating() {
        // 1,0,1 -> three runs of length 1.
        let out = encode_rle_levels(&[1, 0, 1], 1);
        // each: varint(1<<1=2)=0x02, value byte
        assert_eq!(out, vec![0x02, 0x01, 0x02, 0x00, 0x02, 0x01]);
    }

    #[test]
    fn pack_bits_lsb_first() {
        assert_eq!(pack_bits(&[true, false, true]), vec![0b0000_0101]);
        assert_eq!(pack_bits(&[false; 9]), vec![0, 0]);
    }

    #[test]
    fn file_has_magic_bookends() {
        let cols: Vec<Column> = Vec::new();
        let geom = vec![Some(vec![0x01, 0x02])];
        let out = write_geoparquet(&cols, &geom, "{}");
        assert_eq!(&out[..4], MAGIC);
        assert_eq!(&out[out.len() - 4..], MAGIC);
        // 4-byte length precedes the trailing magic.
        let len_pos = out.len() - 8;
        let footer_len =
            u32::from_le_bytes(out[len_pos..len_pos + 4].try_into().unwrap()) as usize;
        assert!(footer_len > 0 && footer_len < out.len());
    }
}
