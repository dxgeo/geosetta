//! Decode a FlatGeobuf byte buffer into a [`FeatureCollection`].

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::flatbuffers::{Table, Vector};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::format::Format;
use crate::native::{FgbColumnNative, FgbNative, Native, NativePayload};
use crate::schema::{DeclType, FieldDecl};
use std::rc::Rc;

const MAGIC_LEN: usize = 8;
const NODE_ITEM_SIZE: usize = 40; // 4 * f64 bbox + u64 offset

// GeometryType enum (ubyte).
mod gtype {
    pub const POINT: u8 = 1;
    pub const LINESTRING: u8 = 2;
    pub const POLYGON: u8 = 3;
    pub const MULTIPOINT: u8 = 4;
    pub const MULTILINESTRING: u8 = 5;
    pub const MULTIPOLYGON: u8 = 6;
    pub const GEOMETRYCOLLECTION: u8 = 7;
}

// ColumnType enum (ubyte).
mod ctype {
    pub const BYTE: u8 = 0;
    pub const UBYTE: u8 = 1;
    pub const BOOL: u8 = 2;
    pub const SHORT: u8 = 3;
    pub const USHORT: u8 = 4;
    pub const INT: u8 = 5;
    pub const UINT: u8 = 6;
    pub const LONG: u8 = 7;
    pub const ULONG: u8 = 8;
    pub const FLOAT: u8 = 9;
    pub const DOUBLE: u8 = 10;
    pub const STRING: u8 = 11;
    pub const JSON: u8 = 12;
    pub const DATETIME: u8 = 13;
    pub const BINARY: u8 = 14;
}

// Field indices within each FlatBuffers table.
// Note: no `HAS_Z`/`HAS_M` constants here. The header carries those as a
// file-level hint (confirmed against real `ogr2ogr -f FlatGeobuf` output:
// field index 3 is `hasZ`), but this reader determines Z/M presence
// directly from each `Geometry` table's own `z`/`m` vector fields instead —
// the same source of truth `xy`'s presence already uses, and more robust
// than trusting a summary flag on a file whose per-geometry dimensionality
// might not be perfectly uniform.
mod header {
    pub const NAME: usize = 0;
    pub const GEOMETRY_TYPE: usize = 2;
    pub const COLUMNS: usize = 7;
    pub const FEATURES_COUNT: usize = 8;
    pub const INDEX_NODE_SIZE: usize = 9;
    pub const CRS: usize = 10;
    pub const TITLE: usize = 11;
    pub const DESCRIPTION: usize = 12;
    pub const METADATA: usize = 13;
}
mod crs {
    pub const ORG: usize = 0;
    pub const CODE: usize = 1;
    pub const NAME: usize = 2;
    pub const DESCRIPTION: usize = 3;
    pub const WKT: usize = 4;
    pub const CODE_STRING: usize = 5;
}
// Column's declaration slots, verified by hand-decoding `ogr2ogr`-written
// FlatGeobuf vtables (a `.dbf` `String (80.0)` lands in slot 4 as `80`; a
// `Real (23.15)` lands in slots 5/6 as `23`/`15` with slot 4 absent; a
// `NOT NULL UNIQUE` column lands in slots 7/8 as `0`/`1`). Slots 2/3
// (`title`/`description`) and 10 (`metadata`) are not read yet — they are
// per-column natives, not declarations, and belong with the M3 FlatGeobuf
// spoke rather than here.
mod column {
    pub const NAME: usize = 0;
    pub const TYPE: usize = 1;
    pub const TITLE: usize = 2;
    pub const DESCRIPTION: usize = 3;
    pub const WIDTH: usize = 4;
    pub const PRECISION: usize = 5;
    pub const SCALE: usize = 6;
    pub const NULLABLE: usize = 7;
    pub const UNIQUE: usize = 8;
    pub const PRIMARY_KEY: usize = 9;
    pub const METADATA: usize = 10;
}
mod feature {
    pub const GEOMETRY: usize = 0;
    pub const PROPERTIES: usize = 1;
}
mod geometry {
    pub const ENDS: usize = 0;
    pub const XY: usize = 1;
    pub const Z: usize = 2;
    pub const M: usize = 3;
    pub const TYPE: usize = 6;
    pub const PARTS: usize = 7;
}

struct Column {
    name: String,
    ty: u8,
    /// What the header declared about this column, or `None` when it declared
    /// nothing beyond the name and type.
    decl: Option<FieldDecl>,
    /// This column's `title`/`description`/`metadata` — descriptive rather
    /// than declarative, so they ride in the native rather than in
    /// [`FieldDecl`]. `None` when the column carried none.
    native: Option<FgbColumnNative>,
}

/// Parse a FlatGeobuf file into a feature collection.
pub fn read(data: &[u8]) -> Result<FeatureCollection> {
    if data.len() < MAGIC_LEN + 4 || &data[0..3] != b"fgb" || &data[4..7] != b"fgb" {
        return Err(Error::FlatGeobuf("not a flatgeobuf file (bad magic)".into()));
    }

    // Header: [u32 len][Header table].
    let header_len = u32_at(data, MAGIC_LEN)? as usize;
    let header_start = MAGIC_LEN + 4;
    let header_buf = data
        .get(header_start..header_start + header_len)
        .ok_or_else(|| Error::FlatGeobuf("truncated header".into()))?;
    let header = Table::root(header_buf)?;

    let default_geom_type = header.read_u8(header::GEOMETRY_TYPE, 0)?;
    let features_count = header.read_u64(header::FEATURES_COUNT, 0)?;
    let index_node_size = header.read_u16(header::INDEX_NODE_SIZE, 16)?;
    let (crs, crs_extras) = read_crs_and_extras(&header)?;
    let columns = read_columns(&header)?;
    // Intern each column's key once so every feature shares one `Rc`.
    let keys: Vec<Rc<str>> = columns.iter().map(|c| Rc::from(c.name.as_str())).collect();

    // Skip the packed R-tree spatial index, if present.
    let index_bytes = index_size(features_count as usize, index_node_size as usize);
    let mut pos = header_start + header_len + index_bytes;

    let mut features = Vec::new();
    while pos < data.len() {
        let feat_len = u32_at(data, pos)? as usize;
        pos += 4;
        let feat_buf = data
            .get(pos..pos + feat_len)
            .ok_or_else(|| Error::FlatGeobuf("truncated feature".into()))?;
        pos += feat_len;

        let table = Table::root(feat_buf)?;
        let geometry = match table.read_table(feature::GEOMETRY)? {
            Some(g) => Some(decode_geometry(&g, default_geom_type)?),
            None => None,
        };
        // An absent properties vector is not "no properties": a row whose
        // every cell is null encodes as an *empty* blob, and the writer omits
        // an empty vector altogether. The columns still stand in the header,
        // so decode against them either way and let the omission mean null.
        let properties = match table.read_vector(feature::PROPERTIES)? {
            Some(v) => decode_properties(v.bytes()?, &columns, &keys)?,
            None => decode_properties(&[], &columns, &keys)?,
        };
        features.push(Feature {
            geometry,
            properties,
            native: None,
        });
    }

    let schema = columns.iter().filter_map(|c| c.decl.clone()).collect();
    let native = FgbNative {
        title: owned(header.read_str(header::TITLE)?),
        description: owned(header.read_str(header::DESCRIPTION)?),
        metadata: owned(header.read_str(header::METADATA)?),
        crs_name: crs_extras.0,
        crs_description: crs_extras.1,
        crs_code_string: crs_extras.2,
        columns: columns.iter().filter_map(|c| c.native.clone()).collect(),
    };
    Ok(FeatureCollection {
        features,
        crs,
        // The header's own `name`, which the writer used to overwrite with a
        // constant. Modelled rather than stashed (D1) because it is what makes
        // a layer name survive a pipe — see `FeatureCollection::name`.
        name: owned(header.read_str(header::NAME)?),
        schema,
        native: (!native.is_empty())
            .then(|| Native::new(Format::FlatGeobuf, NativePayload::FlatGeobuf(native))),
    })
}

/// An owned copy of a FlatBuffers string field, preserving the distinction
/// between *absent* and *empty*: a writer that emitted `title: ""` said
/// something, and re-emitting it as absent would not be putting the file back.
fn owned(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

/// Read the header's `Crs` sub-table into a [`Crs`], carrying the org/code/WKT
/// through unchanged; absent (or empty) means the file recorded no CRS. Plus
/// the three members [`Crs`] has no slot for: `name(2)`, `description(3)` and
/// `code_string(5)`.
///
/// They come back beside the CRS rather than inside it because they are
/// *opaque* under D4 — `NamedCrs` models an identity and a definition, and a
/// human-readable name is neither. `name` is populated in every
/// `ogr2ogr`-written file observed, so this is not a theoretical slot.
type CrsExtras = (Option<String>, Option<String>, Option<String>);
fn read_crs_and_extras(header: &Table) -> Result<(Option<Crs>, CrsExtras)> {
    let Some(t) = header.read_table(header::CRS)? else {
        return Ok((None, (None, None, None)));
    };
    let extras = (
        owned(t.read_str(crs::NAME)?),
        owned(t.read_str(crs::DESCRIPTION)?),
        owned(t.read_str(crs::CODE_STRING)?),
    );
    let org = t.read_str(crs::ORG)?.map(str::to_string);
    let code = match t.read_i32(crs::CODE, 0)? {
        0 => None,
        c => Some(c.to_string()),
    };
    let wkt = t.read_str(crs::WKT)?.filter(|s| !s.is_empty()).map(str::to_string);
    if org.is_none() && code.is_none() && wkt.is_none() {
        return Ok((None, extras));
    }
    Ok((Some(Crs::from_authority_code(org, code, wkt, None)), extras))
}

fn read_columns(header: &Table) -> Result<Vec<Column>> {
    let mut columns = Vec::new();
    if let Some(vec) = header.read_vector(header::COLUMNS)? {
        for i in 0..vec.len() {
            let c = vec.get_table(i)?;
            let name = c.read_str(column::NAME)?.unwrap_or_default().to_string();
            let ty = c.read_u8(column::TYPE, ctype::STRING)?;
            let decl = read_decl(&c, &name, ty)?;
            let native = FgbColumnNative {
                name: name.clone(),
                title: owned(c.read_str(column::TITLE)?),
                description: owned(c.read_str(column::DESCRIPTION)?),
                metadata: owned(c.read_str(column::METADATA)?),
            };
            columns.push(Column {
                name,
                ty,
                decl,
                native: (!native.is_empty()).then_some(native),
            });
        }
    }
    Ok(columns)
}

/// The six declaration slots of one `Column` table, or `None` when the writer
/// left all of them at their defaults.
///
/// The spec's defaults are `-1` for the three sizes and `true`/`false`/`false`
/// for the three flags, so a slot at its default carries no declaration.
/// A size of *zero* is treated the same way: GDAL writes `width: 0` for a
/// GeoPackage `TEXT` column, which declares no width at all rather than a
/// zero-wide field, and re-emitting it as a real declaration would pin every
/// downstream field to width 0.
fn read_decl(c: &Table, name: &str, ty: u8) -> Result<Option<FieldDecl>> {
    let size = |slot: usize| -> Result<Option<u32>> {
        Ok(match c.read_i32(slot, -1)? {
            n if n > 0 => Some(n as u32),
            _ => None,
        })
    };
    // `scale` is meaningful at zero (an integer column declares 0 decimals),
    // but only alongside a precision — on its own it describes nothing.
    let precision = size(column::PRECISION)?;
    let scale = match precision {
        Some(_) => match c.read_i32(column::SCALE, -1)? {
            n if n >= 0 => Some(n as u32),
            _ => None,
        },
        None => size(column::SCALE)?,
    };
    let flag = |slot: usize, default: u8| -> Result<Option<bool>> {
        Ok(match c.read_u8(slot, default)? {
            n if n == default => None,
            n => Some(n != 0),
        })
    };
    let decl = FieldDecl {
        name: name.to_string(),
        // Unlike the six slots below, the type is *always* a declaration: the
        // `Column` table has no "unspecified" type, so every column states one.
        // Recording it is what stops an `Int16` returning as `Int64` and a
        // `DateTime` as `String` (defect 5), and what lets an all-null column
        // keep its type at all, since there is nothing left to re-infer from.
        ty: decl_type(ty),
        width: size(column::WIDTH)?,
        precision,
        scale,
        nullable: flag(column::NULLABLE, 1)?,
        unique: flag(column::UNIQUE, 0)?,
        primary_key: flag(column::PRIMARY_KEY, 0)?,
    };
    Ok((!decl.is_empty()).then_some(decl))
}

/// FlatGeobuf's `Column.type` in the IR's declared-type vocabulary.
///
/// Total but for `BINARY`, which the reader cannot materialize as a cell (it
/// decodes to `Null`), so declaring it would promise a fidelity the value side
/// does not keep.
fn decl_type(ty: u8) -> Option<DeclType> {
    Some(match ty {
        ctype::BOOL => DeclType::Bool,
        ctype::BYTE => DeclType::Int8,
        ctype::UBYTE => DeclType::UInt8,
        ctype::SHORT => DeclType::Int16,
        ctype::USHORT => DeclType::UInt16,
        ctype::INT => DeclType::Int32,
        ctype::UINT => DeclType::UInt32,
        ctype::LONG => DeclType::Int64,
        ctype::ULONG => DeclType::UInt64,
        ctype::FLOAT => DeclType::Float32,
        ctype::DOUBLE => DeclType::Float64,
        ctype::STRING => DeclType::String,
        ctype::JSON => DeclType::Json,
        ctype::DATETIME => DeclType::DateTime,
        _ => return None,
    })
}

/// Byte size of the packed Hilbert R-tree, so we can skip it.
fn index_size(num_items: usize, node_size: usize) -> usize {
    if num_items == 0 || node_size == 0 {
        return 0;
    }
    let node_size = node_size.max(2);
    let mut n = num_items;
    let mut num_nodes = n;
    loop {
        n = n.div_ceil(node_size);
        num_nodes += n;
        if n == 1 {
            break;
        }
    }
    num_nodes * NODE_ITEM_SIZE
}

// --- geometry --------------------------------------------------------------

fn decode_geometry(g: &Table, default_type: u8) -> Result<Geometry> {
    let ty = g.read_u8(geometry::TYPE, default_type)?;
    let xy = g.read_vector(geometry::XY)?;
    let z = g.read_vector(geometry::Z)?;
    let m = g.read_vector(geometry::M)?;
    let ends = g.read_vector(geometry::ENDS)?;

    let geom = match ty {
        gtype::POINT => Geometry::Point(point_at(&positions(xy, z, m)?, 0)?),
        gtype::LINESTRING => Geometry::LineString(positions(xy, z, m)?),
        gtype::MULTIPOINT => Geometry::MultiPoint(positions(xy, z, m)?),
        gtype::POLYGON => Geometry::Polygon(split_rings(&positions(xy, z, m)?, ends)?),
        gtype::MULTILINESTRING => Geometry::MultiLineString(split_rings(&positions(xy, z, m)?, ends)?),
        gtype::MULTIPOLYGON => {
            let mut polys = Vec::new();
            for part in parts(g)? {
                match decode_geometry(&part, gtype::POLYGON)? {
                    Geometry::Polygon(rings) => polys.push(rings),
                    _ => return Err(Error::FlatGeobuf("MultiPolygon part is not a Polygon".into())),
                }
            }
            Geometry::MultiPolygon(polys)
        }
        gtype::GEOMETRYCOLLECTION => {
            let mut geoms = Vec::new();
            for part in parts(g)? {
                geoms.push(decode_geometry(&part, 0)?);
            }
            Geometry::GeometryCollection(geoms)
        }
        other => return Err(Error::FlatGeobuf(format!("unsupported geometry type {other}"))),
    };
    Ok(geom)
}

/// Flat `xy` doubles, plus optional parallel `z`/`m` vectors (same length as
/// the point count), into positions.
fn positions(xy: Option<Vector>, z: Option<Vector>, m: Option<Vector>) -> Result<Vec<Position>> {
    let xy = match xy {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    if xy.len() % 2 != 0 {
        return Err(Error::FlatGeobuf("odd number of xy coordinates".into()));
    }
    let n = xy.len() / 2;
    for (name, v) in [("z", z), ("m", m)] {
        if let Some(v) = v
            && v.len() != n
        {
            return Err(Error::FlatGeobuf(format!(
                "{name} vector length ({}) does not match point count ({n})",
                v.len()
            )));
        }
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let x = xy.get_f64(2 * i)?;
        let y = xy.get_f64(2 * i + 1)?;
        let z = z.map(|v| v.get_f64(i)).transpose()?;
        let m = m.map(|v| v.get_f64(i)).transpose()?;
        out.push(Position { x, y, z, m });
    }
    Ok(out)
}

fn point_at(ps: &[Position], i: usize) -> Result<Position> {
    ps.get(i)
        .copied()
        .ok_or_else(|| Error::FlatGeobuf("point has no coordinates".into()))
}

/// Split a flat position list into rings/lines at the `ends` boundaries
/// (each `end` is a cumulative point count). No `ends` ⇒ a single ring.
fn split_rings(ps: &[Position], ends: Option<Vector>) -> Result<Vec<Vec<Position>>> {
    let ends = match ends {
        Some(v) if !v.is_empty() => v,
        _ => return Ok(vec![ps.to_vec()]),
    };
    let mut rings = Vec::with_capacity(ends.len());
    let mut start = 0usize;
    for i in 0..ends.len() {
        let end = ends.get_u32(i)? as usize;
        let ring = ps
            .get(start..end)
            .ok_or_else(|| Error::FlatGeobuf("ring end out of range".into()))?;
        rings.push(ring.to_vec());
        start = end;
    }
    Ok(rings)
}

fn parts<'a>(g: &Table<'a>) -> Result<Vec<Table<'a>>> {
    let mut out = Vec::new();
    if let Some(v) = g.read_vector(geometry::PARTS)? {
        for i in 0..v.len() {
            out.push(v.get_table(i)?);
        }
    }
    Ok(out)
}

// --- properties ------------------------------------------------------------

/// Decode the packed properties blob against the column schema:
/// `[u16 col_index][typed value]…`, little-endian.
///
/// Every column in the header yields a property, in header order. A cell the
/// row omits comes back `Null`, because *omission is how FlatGeobuf encodes a
/// null* — there is no other encoding for one. Materializing only the cells
/// present was defect 1: a column null in every row then had no key in any
/// feature, so nothing downstream could tell it had ever existed and the
/// column vanished from a same-format round trip.
///
/// This makes the output rectangular, which is what every other spoke in this
/// crate already produces (GeoParquet, GeoPackage and CSV are byte-identical
/// to `ogr2ogr` here) and what `ogr2ogr` itself writes from a FlatGeobuf.
fn decode_properties(
    blob: &[u8],
    columns: &[Column],
    keys: &[Rc<str>],
) -> Result<Vec<(Rc<str>, JsonValue)>> {
    let mut values: Vec<JsonValue> = vec![JsonValue::Null; columns.len()];
    let mut pos = 0usize;
    while pos < blob.len() {
        let idx = u16_at(blob, pos)? as usize;
        pos += 2;
        let col = columns
            .get(idx)
            .ok_or_else(|| Error::FlatGeobuf("property column index out of range".into()))?;
        values[idx] = read_property_value(blob, &mut pos, col.ty)?;
    }
    Ok(keys.iter().cloned().zip(values).collect())
}

fn read_property_value(blob: &[u8], pos: &mut usize, ty: u8) -> Result<JsonValue> {
    let mut scalar = |width: usize| -> Result<&[u8]> {
        let s = blob
            .get(*pos..*pos + width)
            .ok_or_else(|| Error::FlatGeobuf("truncated property value".into()))?;
        *pos += width;
        Ok(s)
    };
    let int = |v: i64| JsonValue::Number {
        value: v as f64,
        is_int: true,
    };

    Ok(match ty {
        ctype::BOOL => JsonValue::Bool(scalar(1)?[0] != 0),
        ctype::BYTE => int(scalar(1)?[0] as i8 as i64),
        ctype::UBYTE => int(scalar(1)?[0] as i64),
        ctype::SHORT => int(i16::from_le_bytes(scalar(2)?.try_into().unwrap()) as i64),
        ctype::USHORT => int(u16::from_le_bytes(scalar(2)?.try_into().unwrap()) as i64),
        ctype::INT => int(i32::from_le_bytes(scalar(4)?.try_into().unwrap()) as i64),
        ctype::UINT => int(u32::from_le_bytes(scalar(4)?.try_into().unwrap()) as i64),
        ctype::LONG => int(i64::from_le_bytes(scalar(8)?.try_into().unwrap())),
        ctype::ULONG => int(u64::from_le_bytes(scalar(8)?.try_into().unwrap()) as i64),
        ctype::FLOAT => JsonValue::Number {
            value: f32::from_le_bytes(scalar(4)?.try_into().unwrap()) as f64,
            is_int: false,
        },
        ctype::DOUBLE => JsonValue::Number {
            value: f64::from_le_bytes(scalar(8)?.try_into().unwrap()),
            is_int: false,
        },
        ctype::STRING | ctype::JSON | ctype::DATETIME => {
            let len = u32::from_le_bytes(scalar(4)?.try_into().unwrap()) as usize;
            let bytes = blob
                .get(*pos..*pos + len)
                .ok_or_else(|| Error::FlatGeobuf("truncated property string".into()))?;
            *pos += len;
            JsonValue::String(
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::FlatGeobuf("property string is not utf-8".into()))?
                    .to_string(),
            )
        }
        ctype::BINARY => {
            let len = u32::from_le_bytes(scalar(4)?.try_into().unwrap()) as usize;
            *pos += len; // skip binary payload (no GeoJSON representation)
            JsonValue::Null
        }
        other => return Err(Error::FlatGeobuf(format!("unsupported column type {other}"))),
    })
}

// --- little-endian helpers -------------------------------------------------

fn u16_at(b: &[u8], at: usize) -> Result<u16> {
    Ok(u16::from_le_bytes(
        b.get(at..at + 2)
            .ok_or_else(|| Error::FlatGeobuf("read past end".into()))?
            .try_into()
            .unwrap(),
    ))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32> {
    Ok(u32::from_le_bytes(
        b.get(at..at + 4)
            .ok_or_else(|| Error::FlatGeobuf("read past end".into()))?
            .try_into()
            .unwrap(),
    ))
}
