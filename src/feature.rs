//! The shared feature model — the intermediate representation every format
//! converts to and from. It is deliberately format-neutral: `geojson` and
//! `parquet` both depend on it, not on each other.

use crate::crs::Crs;
use crate::geometry::Geometry;
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
}
