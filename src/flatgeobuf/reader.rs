//! Decode a FlatGeobuf byte buffer into a [`FeatureCollection`].

use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::flatbuffers::{Table, Vector};
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;

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
mod header {
    pub const GEOMETRY_TYPE: usize = 2;
    pub const COLUMNS: usize = 7;
    pub const FEATURES_COUNT: usize = 8;
    pub const INDEX_NODE_SIZE: usize = 9;
}
mod column {
    pub const NAME: usize = 0;
    pub const TYPE: usize = 1;
}
mod feature {
    pub const GEOMETRY: usize = 0;
    pub const PROPERTIES: usize = 1;
}
mod geometry {
    pub const ENDS: usize = 0;
    pub const XY: usize = 1;
    pub const TYPE: usize = 6;
    pub const PARTS: usize = 7;
}

struct Column {
    name: String,
    ty: u8,
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
    let columns = read_columns(&header)?;

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
        let properties = match table.read_vector(feature::PROPERTIES)? {
            Some(v) => decode_properties(v.bytes()?, &columns)?,
            None => Vec::new(),
        };
        features.push(Feature {
            geometry,
            properties,
        });
    }

    Ok(FeatureCollection { features })
}

fn read_columns(header: &Table) -> Result<Vec<Column>> {
    let mut columns = Vec::new();
    if let Some(vec) = header.read_vector(header::COLUMNS)? {
        for i in 0..vec.len() {
            let c = vec.get_table(i)?;
            columns.push(Column {
                name: c.read_str(column::NAME)?.unwrap_or_default().to_string(),
                ty: c.read_u8(column::TYPE, ctype::STRING)?,
            });
        }
    }
    Ok(columns)
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
    let ends = g.read_vector(geometry::ENDS)?;

    let geom = match ty {
        gtype::POINT => Geometry::Point(point_at(&positions(xy)?, 0)?),
        gtype::LINESTRING => Geometry::LineString(positions(xy)?),
        gtype::MULTIPOINT => Geometry::MultiPoint(positions(xy)?),
        gtype::POLYGON => Geometry::Polygon(split_rings(&positions(xy)?, ends)?),
        gtype::MULTILINESTRING => Geometry::MultiLineString(split_rings(&positions(xy)?, ends)?),
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

/// Flat `xy` doubles into `[x, y]` positions.
fn positions(xy: Option<Vector>) -> Result<Vec<Position>> {
    let xy = match xy {
        Some(v) => v,
        None => return Ok(Vec::new()),
    };
    if xy.len() % 2 != 0 {
        return Err(Error::FlatGeobuf("odd number of xy coordinates".into()));
    }
    let mut out = Vec::with_capacity(xy.len() / 2);
    for i in 0..xy.len() / 2 {
        out.push([xy.get_f64(2 * i)?, xy.get_f64(2 * i + 1)?]);
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
fn decode_properties(blob: &[u8], columns: &[Column]) -> Result<Vec<(String, JsonValue)>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < blob.len() {
        let idx = u16_at(blob, pos)? as usize;
        pos += 2;
        let col = columns
            .get(idx)
            .ok_or_else(|| Error::FlatGeobuf("property column index out of range".into()))?;
        let value = read_property_value(blob, &mut pos, col.ty)?;
        out.push((col.name.clone(), value));
    }
    Ok(out)
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
