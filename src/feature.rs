//! The shared feature model — the intermediate representation every format
//! converts to and from. It is deliberately format-neutral: `geojson` and
//! `parquet` both depend on it, not on each other.

use crate::crs::Crs;
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use std::rc::Rc;

/// A vector feature: an optional geometry plus ordered properties.
///
/// Property keys are `Rc<str>` rather than `String` so that a column name
/// repeated across every row is allocated once and shared (a refcount bump per
/// cell), not re-allocated per feature — the dominant cost on wide tables.
#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub geometry: Option<Geometry>,
    /// Property members, in order. Empty when there are no properties.
    pub properties: Vec<(Rc<str>, JsonValue)>,
}

/// A collection of features.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureCollection {
    pub features: Vec<Feature>,
    /// The coordinate reference system the geometries are expressed in, as
    /// recovered from the source. `None` means the source recorded no CRS.
    /// Geosetta never reprojects — this is carried through to the output
    /// unchanged (see [`crate::crs`]).
    pub crs: Option<Crs>,
}

impl FeatureCollection {
    /// A collection with no recorded CRS. The common constructor for readers of
    /// formats that carry no coordinate-reference metadata (CSV, WKT) and for
    /// tests; set [`FeatureCollection::crs`] afterwards when a CRS is known.
    pub fn new(features: Vec<Feature>) -> FeatureCollection {
        FeatureCollection {
            features,
            crs: None,
        }
    }

    /// Visit every coordinate across every feature's geometry by mutable
    /// reference (features with no geometry are skipped).
    ///
    /// Geosetta itself never calls this — it is the seam an external
    /// reprojection crate plugs into between [`crate::read_features`] and
    /// [`crate::write_features`]: rewrite coordinates in place here, then set
    /// [`Self::crs`] to the new identity. Which library computes the
    /// transform (PROJ bindings, a pure-Rust crate, a hand-rolled Helmert
    /// shift, ...) is entirely up to the caller — geosetta only carries a
    /// [`Crs`] through, it never interprets or transforms one (see
    /// [`crate::crs`]).
    pub fn for_each_position_mut(&mut self, mut f: impl FnMut(&mut Position)) {
        for feature in &mut self.features {
            if let Some(g) = &mut feature.geometry {
                g.for_each_position_mut(&mut f);
            }
        }
    }

    /// Visit every contiguous run of coordinates across every feature's
    /// geometry by mutable slice (features with no geometry are skipped) —
    /// the batch-friendly counterpart to [`Self::for_each_position_mut`]; see
    /// [`crate::Geometry::for_each_position_run_mut`] for what counts as a
    /// "run" and why a reprojection backend would want one.
    pub fn for_each_position_run_mut(&mut self, mut f: impl FnMut(&mut [Position])) {
        for feature in &mut self.features {
            if let Some(g) = &mut feature.geometry {
                g.for_each_position_run_mut(&mut f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry;

    #[test]
    fn for_each_position_mut_rewrites_every_feature_and_skips_geometryless_ones() {
        let mut fc = FeatureCollection::new(vec![
            Feature { geometry: Some(Geometry::Point([1.0, 2.0])), properties: vec![] },
            Feature { geometry: None, properties: vec![] },
            Feature { geometry: Some(Geometry::Point([3.0, 4.0])), properties: vec![] },
        ]);
        fc.for_each_position_mut(|p| {
            p[0] *= 10.0;
            p[1] *= 10.0;
        });
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point([10.0, 20.0])));
        assert_eq!(fc.features[1].geometry, None);
        assert_eq!(fc.features[2].geometry, Some(Geometry::Point([30.0, 40.0])));
    }

    #[test]
    fn for_each_position_run_mut_rewrites_every_feature_and_skips_geometryless_ones() {
        let mut fc = FeatureCollection::new(vec![
            Feature {
                geometry: Some(Geometry::LineString(vec![[1.0, 2.0], [3.0, 4.0]])),
                properties: vec![],
            },
            Feature { geometry: None, properties: vec![] },
            Feature { geometry: Some(Geometry::Point([5.0, 6.0])), properties: vec![] },
        ]);
        let mut run_lengths = Vec::new();
        fc.for_each_position_run_mut(|ps| {
            run_lengths.push(ps.len());
            for p in ps {
                p[0] *= 10.0;
                p[1] *= 10.0;
            }
        });
        assert_eq!(run_lengths, vec![2, 1]);
        assert_eq!(
            fc.features[0].geometry,
            Some(Geometry::LineString(vec![[10.0, 20.0], [30.0, 40.0]]))
        );
        assert_eq!(fc.features[1].geometry, None);
        assert_eq!(fc.features[2].geometry, Some(Geometry::Point([50.0, 60.0])));
    }
}
