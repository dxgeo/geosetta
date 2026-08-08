//! The shared feature model — the intermediate representation every format
//! converts to and from. It is deliberately format-neutral: `geojson` and
//! `parquet` both depend on it, not on each other.

use crate::geometry::Geometry;
use crate::json::JsonValue;

/// A vector feature: an optional geometry plus ordered properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub geometry: Option<Geometry>,
    /// Property members, in order. Empty when there are no properties.
    pub properties: Vec<(String, JsonValue)>,
}

/// A collection of features.
#[derive(Debug, Clone, PartialEq)]
pub struct FeatureCollection {
    pub features: Vec<Feature>,
}
