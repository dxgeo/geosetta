//! Geometry model shared by the readers/writers, plus bounding-box support.
//!
//! Coordinates are 2D (X = longitude, Y = latitude). Any additional ordinates
//! in the source (e.g. elevation) are dropped for this first version.

mod wkb;

pub use wkb::encode as to_wkb;

/// A single 2D coordinate: `[x, y]`.
pub type Position = [f64; 2];

/// A vector geometry, mirroring the GeoJSON geometry types.
///
/// Variant names match the GeoJSON/WKB type names exactly (including
/// `GeometryCollection`), so we opt out of the variant-naming lint.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum Geometry {
    Point(Position),
    LineString(Vec<Position>),
    /// Rings; by convention the first is exterior, the rest are holes.
    Polygon(Vec<Vec<Position>>),
    MultiPoint(Vec<Position>),
    MultiLineString(Vec<Vec<Position>>),
    MultiPolygon(Vec<Vec<Vec<Position>>>),
    GeometryCollection(Vec<Geometry>),
}

impl Geometry {
    /// The GeoParquet/GeoJSON type name (e.g. `"Point"`, `"MultiPolygon"`).
    pub fn type_name(&self) -> &'static str {
        match self {
            Geometry::Point(_) => "Point",
            Geometry::LineString(_) => "LineString",
            Geometry::Polygon(_) => "Polygon",
            Geometry::MultiPoint(_) => "MultiPoint",
            Geometry::MultiLineString(_) => "MultiLineString",
            Geometry::MultiPolygon(_) => "MultiPolygon",
            Geometry::GeometryCollection(_) => "GeometryCollection",
        }
    }

    /// Fold every coordinate in the geometry into `bbox`.
    pub fn extend_bbox(&self, bbox: &mut Bbox) {
        match self {
            Geometry::Point(p) => bbox.add(*p),
            Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
                ps.iter().for_each(|p| bbox.add(*p));
            }
            Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
                rings.iter().flatten().for_each(|p| bbox.add(*p));
            }
            Geometry::MultiPolygon(polys) => {
                polys.iter().flatten().flatten().for_each(|p| bbox.add(*p));
            }
            Geometry::GeometryCollection(geoms) => {
                geoms.iter().for_each(|g| g.extend_bbox(bbox));
            }
        }
    }
}

/// An axis-aligned bounding box accumulated across geometries.
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl Bbox {
    /// An empty box (inverted bounds) ready to absorb points.
    pub fn empty() -> Self {
        Bbox {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
        }
    }

    /// Grow the box to include `p`.
    pub fn add(&mut self, p: Position) {
        self.min_x = self.min_x.min(p[0]);
        self.min_y = self.min_y.min(p[1]);
        self.max_x = self.max_x.max(p[0]);
        self.max_y = self.max_y.max(p[1]);
    }

    /// True until at least one point has been added.
    pub fn is_empty(&self) -> bool {
        self.min_x > self.max_x
    }
}
