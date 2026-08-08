//! Shared spatial-ordering primitives: a Hilbert-curve encoder and helpers to
//! order features by spatial locality. Used to build the FlatGeobuf packed
//! Hilbert R-tree (and, optionally, to cluster rows in other formats).
//!
//! The Hilbert mapping uses a fixed 16-bit resolution over the dataset extent,
//! matching FlatGeobuf's convention so our index agrees with GDAL's reader.

use crate::geometry::Bbox;

const HILBERT_MAX: u32 = (1 << 16) - 1;

/// The Hilbert-curve distance of integer grid coordinates `(x, y)` in a
/// 2^16 × 2^16 grid. A standard bit-twiddling implementation (matches the
/// FlatGeobuf convention).
pub fn hilbert(x: u32, y: u32) -> u32 {
    let mut a = x ^ y;
    let mut b = 0xFFFF ^ a;
    let mut c = 0xFFFF ^ (x | y);
    let mut d = x & (y ^ 0xFFFF);

    let mut aa = a | (b >> 1);
    let mut bb = (a >> 1) ^ a;
    let mut cc = ((c >> 1) ^ (b & (d >> 1))) ^ c;
    let mut dd = ((a & (c >> 1)) ^ (d >> 1)) ^ d;
    a = aa;
    b = bb;
    c = cc;
    d = dd;

    aa = (a & (a >> 2)) ^ (b & (b >> 2));
    bb = (a & (b >> 2)) ^ (b & ((a ^ b) >> 2));
    cc ^= (a & (c >> 2)) ^ (b & (d >> 2));
    dd ^= (b & (c >> 2)) ^ ((a ^ b) & (d >> 2));
    a = aa;
    b = bb;
    c = cc;
    d = dd;

    aa = (a & (a >> 4)) ^ (b & (b >> 4));
    bb = (a & (b >> 4)) ^ (b & ((a ^ b) >> 4));
    cc ^= (a & (c >> 4)) ^ (b & (d >> 4));
    dd ^= (b & (c >> 4)) ^ ((a ^ b) & (d >> 4));
    a = aa;
    b = bb;
    c = cc;
    d = dd;

    cc ^= (a & (c >> 8)) ^ (b & (d >> 8));
    dd ^= (b & (c >> 8)) ^ ((a ^ b) & (d >> 8));

    a = cc ^ (cc >> 1);
    b = dd ^ (dd >> 1);

    let mut i0 = x ^ y;
    let mut i1 = b | (0xFFFF ^ (i0 | a));

    i0 = (i0 | (i0 << 8)) & 0x00FF_00FF;
    i0 = (i0 | (i0 << 4)) & 0x0F0F_0F0F;
    i0 = (i0 | (i0 << 2)) & 0x3333_3333;
    i0 = (i0 | (i0 << 1)) & 0x5555_5555;

    i1 = (i1 | (i1 << 8)) & 0x00FF_00FF;
    i1 = (i1 | (i1 << 4)) & 0x0F0F_0F0F;
    i1 = (i1 | (i1 << 2)) & 0x3333_3333;
    i1 = (i1 | (i1 << 1)) & 0x5555_5555;

    (i1 << 1) | i0
}

/// Hilbert value of a bounding box's center, quantized to the 16-bit grid over
/// `extent`.
pub fn hilbert_bbox(b: &Bbox, extent: &Bbox) -> u32 {
    let width = extent.max_x - extent.min_x;
    let height = extent.max_y - extent.min_y;
    let quantize = |v: f64, min: f64, span: f64| -> u32 {
        if span > 0.0 {
            (HILBERT_MAX as f64 * (v - min) / span).floor() as u32
        } else {
            0
        }
    };
    let x = quantize((b.min_x + b.max_x) / 2.0, extent.min_x, width);
    let y = quantize((b.min_y + b.max_y) / 2.0, extent.min_y, height);
    hilbert(x, y)
}

/// The order in which to visit `bboxes` for spatial locality: indices sorted by
/// the Hilbert value of each bbox over their combined extent. A stable sort
/// keeps ties in input order.
pub fn hilbert_order(bboxes: &[Bbox]) -> Vec<usize> {
    let mut extent = Bbox::empty();
    for b in bboxes {
        if !b.is_empty() {
            extent.add([b.min_x, b.min_y]);
            extent.add([b.max_x, b.max_y]);
        }
    }
    let mut keyed: Vec<(u32, usize)> = bboxes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let h = if b.is_empty() { 0 } else { hilbert_bbox(b, &extent) };
            (h, i)
        })
        .collect();
    keyed.sort_by_key(|(h, _)| *h);
    keyed.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hilbert_is_a_bijection_on_a_small_region() {
        // Distinct grid points get distinct Hilbert values.
        let mut seen = std::collections::HashSet::new();
        for y in 0..32u32 {
            for x in 0..32u32 {
                assert!(seen.insert(hilbert(x, y)), "collision at ({x},{y})");
            }
        }
        assert_eq!(hilbert(0, 0), 0); // the curve starts at the origin
    }

    #[test]
    fn orders_points_by_locality() {
        // Two clusters far apart: each cluster's members should be adjacent in
        // the Hilbert order.
        let bb = |x: f64, y: f64| {
            let mut b = Bbox::empty();
            b.add([x, y]);
            b
        };
        let boxes = vec![bb(0.0, 0.0), bb(100.0, 100.0), bb(1.0, 1.0), bb(101.0, 99.0)];
        let order = hilbert_order(&boxes);
        // The two near-origin boxes (indices 0, 2) are neighbors in the order,
        // as are the two far boxes (1, 3).
        let pos = |i: usize| order.iter().position(|&x| x == i).unwrap();
        assert_eq!((pos(0) as isize - pos(2) as isize).abs(), 1);
        assert_eq!((pos(1) as isize - pos(3) as isize).abs(), 1);
    }
}
