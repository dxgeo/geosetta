//! Arbitrary collections for the table's property tests: seeded, so a failure
//! names the seed that reproduces it.

use crate::feature::Feature;
use crate::format::Format;
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::native::{Native, NativePayload};
use std::rc::Rc;

/// xorshift64*: small, and enough to vary shapes.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// In `0..n`.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub(crate) fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    pub(crate) fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            items.swap(i, self.below(i + 1));
        }
    }
}

/// How a collection's rows choose their keys: the shapes the block forms and
/// the seal-time choice must each get right.
#[derive(Clone, Copy)]
enum Style {
    /// One key order for every row, a few rows short a key.
    Regular,
    /// Each row a random subset of a small pool, in random order.
    Ragged,
    /// Each row a key or two from a large pool.
    Sparse,
    /// Two regular stretches with different keys, as concatenated exports.
    Stretches,
    /// Every row the same keys, each in its own order.
    OwnOrder,
}

pub(crate) fn value(rng: &mut Rng, depth: u32) -> JsonValue {
    match rng.below(if depth > 1 { 8 } else { 10 }) {
        0 => JsonValue::Null,
        1 => JsonValue::Bool(rng.one_in(2)),
        2 | 3 => JsonValue::Int(match rng.below(4) {
            0 => i64::MIN,
            1 => i64::MAX,
            _ => rng.below(2000) as i64 - 1000,
        }),
        4 => JsonValue::Float(match rng.below(4) {
            0 => -0.0,
            1 => 1.0,
            _ => (rng.below(2_000_000) as f64 - 1_000_000.0) / 7.0,
        }),
        5 | 6 => JsonValue::String(["", "a", "Zürich", "line\nbreak", "1"][rng.below(5)].into()),
        7 => JsonValue::Binary((0..rng.below(4)).map(|_| rng.next() as u8).collect()),
        8 => JsonValue::Array((0..rng.below(3)).map(|_| value(rng, depth + 1)).collect()),
        _ => JsonValue::Object((0..rng.below(3)).map(|i| (format!("m{i}"), value(rng, depth + 1))).collect()),
    }
}

/// A value that stays one type down its column most of the time, so typed
/// buffers get built as well as `Mixed`.
fn column_value(rng: &mut Rng, key: usize) -> JsonValue {
    if rng.one_in(6) {
        return value(rng, 0);
    }
    match key % 5 {
        0 => JsonValue::Int(rng.below(1000) as i64),
        1 => JsonValue::Float(rng.below(1000) as f64 / 4.0),
        2 => JsonValue::String(format!("s{}", rng.below(50))),
        3 => JsonValue::Bool(rng.one_in(2)),
        _ => JsonValue::Null,
    }
}

/// A collection of up to a few hundred rows, now and then one of many
/// blocks of `block_rows`.
/// An ordinate: ordinary now and then, and now and then a value `==` gets
/// wrong -- `-0.0`, or with `nan`, a NaN with a payload of its own -- or an
/// infinity.
fn ordinate(rng: &mut Rng, nan: bool) -> f64 {
    match rng.below(12) {
        0 => -0.0,
        1 if nan => f64::from_bits(0x7ff8_0000_0000_0000 | (rng.next() & 0xffff)),
        2 => f64::INFINITY,
        _ => (rng.below(2_000) as f64 - 1_000.0) / 8.0,
    }
}

/// A position whose Z and M are each there or not, independently of its
/// neighbours'.
fn position(rng: &mut Rng, nan: bool, z: bool, m: bool) -> Position {
    Position {
        x: ordinate(rng, nan),
        y: ordinate(rng, nan),
        z: (z && !rng.one_in(6) || rng.one_in(20)).then(|| ordinate(rng, nan)),
        m: (m && !rng.one_in(6) || rng.one_in(20)).then(|| ordinate(rng, nan)),
    }
}

fn run(rng: &mut Rng, nan: bool, z: bool, m: bool) -> Vec<Position> {
    (0..rng.below(6)).map(|_| position(rng, nan, z, m)).collect()
}

fn rings(rng: &mut Rng, nan: bool, z: bool, m: bool) -> Vec<Vec<Position>> {
    (0..rng.below(4)).map(|_| run(rng, nan, z, m)).collect()
}

/// Any geometry: every type, empty parts and rings among full ones, Z and M
/// per position, and collections nested in collections. `nan` lets an
/// ordinate be a NaN, which only a bitwise comparison can check.
pub(crate) fn geometry(rng: &mut Rng, nan: bool, depth: u32) -> Geometry {
    let (z, m) = (rng.one_in(3), rng.one_in(4));
    match rng.below(if depth < 2 { 8 } else { 7 }) {
        0 | 1 => Geometry::Point(position(rng, nan, z, m)),
        2 => Geometry::LineString(run(rng, nan, z, m)),
        3 => Geometry::Polygon(rings(rng, nan, z, m)),
        4 => Geometry::MultiPoint(run(rng, nan, z, m)),
        5 => Geometry::MultiLineString(rings(rng, nan, z, m)),
        6 => Geometry::MultiPolygon((0..rng.below(3)).map(|_| rings(rng, nan, z, m)).collect()),
        _ => Geometry::GeometryCollection((0..rng.below(4)).map(|_| geometry(rng, nan, depth + 1)).collect()),
    }
}

pub(crate) fn collection(rng: &mut Rng, block_rows: usize) -> Vec<Feature> {
    let rows = match rng.below(10) {
        0 => 0,
        1 => 10 * block_rows + rng.below(300),
        _ => rng.below(300),
    };
    collection_of(rng, rows)
}

pub(crate) fn collection_of(rng: &mut Rng, rows: usize) -> Vec<Feature> {
    let style = [Style::Regular, Style::Ragged, Style::Sparse, Style::Stretches, Style::OwnOrder][rng.below(5)];
    let pool: Vec<Rc<str>> = (0..if matches!(style, Style::Sparse) { 80 } else { 8 })
        .map(|i| Rc::from(format!("k{i}")))
        .collect();
    (0..rows)
        .map(|row| {
            let mut keys: Vec<usize> = match style {
                Style::Regular => (0..6).filter(|_| !rng.one_in(10)).collect(),
                Style::Ragged => (0..8).filter(|_| rng.one_in(2)).collect(),
                Style::Sparse => (0..1 + rng.below(2)).map(|_| rng.below(80)).collect(),
                Style::Stretches if row < rows / 2 => vec![0, 1, 2],
                Style::Stretches => vec![3, 4, 5, 6],
                Style::OwnOrder => (0..5).collect(),
            };
            if matches!(style, Style::Ragged | Style::OwnOrder) {
                rng.shuffle(&mut keys);
            }
            // Guarantee 3: now and then a key repeats.
            if !keys.is_empty() && rng.one_in(15) {
                keys.push(keys[rng.below(keys.len())]);
            }
            Feature {
                geometry: match rng.below(10) {
                    0 => None,
                    1..=3 => Some(geometry(rng, false, 0)),
                    _ => Some(Geometry::Point(Position::new(row as f64, -(row as f64)))),
                },
                properties: keys.into_iter().map(|k| (pool[k].clone(), column_value(rng, k))).collect(),
                native: rng.one_in(20).then(|| {
                    Box::new(Native {
                        format: Format::GeoJson,
                        payload: NativePayload::GeoJson(vec![("id".into(), JsonValue::Int(row as i64))]),
                    })
                }),
            }
        })
        .collect()
}

/// A tabular reader's rows: every row holds every key of a header, in the
/// header's order, which now and then declares a key twice. Returned with
/// the header.
pub(crate) fn tabular(rng: &mut Rng, rows: usize) -> (Vec<Rc<str>>, Vec<Feature>) {
    let mut keys: Vec<usize> = (0..rng.below(9)).collect();
    if !keys.is_empty() && rng.one_in(6) {
        keys.insert(rng.below(keys.len() + 1), keys[rng.below(keys.len())]);
    }
    let header: Vec<Rc<str>> = keys.iter().map(|k| Rc::from(format!("k{k}"))).collect();
    let features = (0..rows)
        .map(|row| Feature {
            geometry: match rng.below(10) {
                0 => None,
                1..=3 => Some(geometry(rng, false, 0)),
                _ => Some(Geometry::Point(Position::new(row as f64, 0.0))),
            },
            properties: keys.iter().zip(&header).map(|(&k, key)| (key.clone(), column_value(rng, k))).collect(),
            native: rng.one_in(20).then(|| {
                Box::new(Native {
                    format: Format::GeoJson,
                    payload: NativePayload::GeoJson(vec![("fid".into(), JsonValue::Int(row as i64))]),
                })
            }),
        })
        .collect();
    (header, features)
}
