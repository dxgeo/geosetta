//! Geometry model shared by the readers/writers, plus bounding-box support.
//!
//! Coordinates are 2D (X = longitude, Y = latitude). Any additional ordinates
//! in the source (e.g. elevation) are dropped for this first version.

mod wkb;
mod wkt;

pub use wkb::decode as from_wkb;
pub use wkb::encode as to_wkb;
pub use wkt::decode as from_wkt;
pub use wkt::encode as to_wkt;

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

    /// This geometry's own bounding box.
    pub fn bbox(&self) -> Bbox {
        let mut b = Bbox::empty();
        self.extend_bbox(&mut b);
        b
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

    /// Visit every coordinate in the geometry by mutable reference, recursing
    /// into rings/parts/collections the same way [`Self::extend_bbox`] does.
    ///
    /// This is the seam a reprojection crate plugs into: geosetta itself never
    /// calls this (see the [`crate::crs`] module docs — it only ever carries a
    /// CRS through, never transforms coordinates), but an external crate can
    /// pair it with [`crate::FeatureCollection::for_each_position_mut`] to
    /// rewrite every coordinate in place without hand-rolling a match over
    /// every `Geometry` variant.
    pub fn for_each_position_mut(&mut self, mut f: impl FnMut(&mut Position)) {
        self.visit_positions_mut(&mut f);
    }

    /// Generic-`F` recursion helper for [`Self::for_each_position_mut`] — takes
    /// `&mut F` rather than `impl FnMut` so the closure reference threads
    /// through `GeometryCollection` recursion without re-boxing at each level.
    fn visit_positions_mut<F: FnMut(&mut Position)>(&mut self, f: &mut F) {
        match self {
            Geometry::Point(p) => f(p),
            Geometry::LineString(ps) | Geometry::MultiPoint(ps) => {
                ps.iter_mut().for_each(&mut *f);
            }
            Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
                rings.iter_mut().flatten().for_each(&mut *f);
            }
            Geometry::MultiPolygon(polys) => {
                polys.iter_mut().flatten().flatten().for_each(&mut *f);
            }
            Geometry::GeometryCollection(geoms) => {
                geoms.iter_mut().for_each(|g| g.visit_positions_mut(f));
            }
        }
    }

    /// Visit every *contiguous run* of coordinates in the geometry by mutable
    /// slice — a single point, a whole `LineString`/`MultiPoint`, or one ring
    /// of a `Polygon`/`MultiLineString`/`MultiPolygon` per call — rather than
    /// one coordinate at a time.
    ///
    /// The IR already stores each of those runs as one contiguous `Vec`, so
    /// this exposes that shape instead of flattening it away. It exists
    /// alongside [`Self::for_each_position_mut`] for reprojection backends
    /// that batch (PROJ's `proj_trans_array`, a SIMD kernel, ...): handing
    /// over a slice lets them transform a whole run in one call instead of
    /// paying per-coordinate call/FFI overhead. A caller that only wants
    /// pointwise access can just iterate the slice itself.
    pub fn for_each_position_run_mut(&mut self, mut f: impl FnMut(&mut [Position])) {
        self.visit_position_runs_mut(&mut f);
    }

    /// Generic-`F` recursion helper for [`Self::for_each_position_run_mut`],
    /// mirroring [`Self::visit_positions_mut`].
    fn visit_position_runs_mut<F: FnMut(&mut [Position])>(&mut self, f: &mut F) {
        match self {
            Geometry::Point(p) => f(std::slice::from_mut(p)),
            Geometry::LineString(ps) | Geometry::MultiPoint(ps) => f(ps.as_mut_slice()),
            Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
                rings.iter_mut().for_each(|ring| f(ring.as_mut_slice()));
            }
            Geometry::MultiPolygon(polys) => {
                polys.iter_mut().flatten().for_each(|ring| f(ring.as_mut_slice()));
            }
            Geometry::GeometryCollection(geoms) => {
                geoms.iter_mut().for_each(|g| g.visit_position_runs_mut(f));
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

#[cfg(test)]
mod tests {
    use super::*;

    // A negate-both-ordinates stand-in for a real reprojection: cheap to
    // verify every position was actually visited (and none double-visited).
    fn negate(p: &mut Position) {
        p[0] = -p[0];
        p[1] = -p[1];
    }

    #[test]
    fn for_each_position_mut_visits_a_point() {
        let mut g = Geometry::Point([1.0, 2.0]);
        g.for_each_position_mut(negate);
        assert_eq!(g, Geometry::Point([-1.0, -2.0]));
    }

    #[test]
    fn for_each_position_mut_visits_polygon_rings_including_holes() {
        let mut g = Geometry::Polygon(vec![
            vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 0.0]],
            vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 1.0]],
        ]);
        g.for_each_position_mut(negate);
        assert_eq!(
            g,
            Geometry::Polygon(vec![
                vec![[0.0, 0.0], [-4.0, 0.0], [-4.0, -4.0], [0.0, 0.0]],
                vec![[-1.0, -1.0], [-2.0, -1.0], [-2.0, -2.0], [-1.0, -1.0]],
            ])
        );
    }

    #[test]
    fn for_each_position_mut_visits_multipolygon_parts() {
        let mut g = Geometry::MultiPolygon(vec![
            vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]],
            vec![vec![[5.0, 5.0], [6.0, 5.0], [6.0, 6.0]]],
        ]);
        g.for_each_position_mut(negate);
        assert_eq!(
            g,
            Geometry::MultiPolygon(vec![
                vec![vec![[0.0, 0.0], [-1.0, 0.0], [-1.0, -1.0]]],
                vec![vec![[-5.0, -5.0], [-6.0, -5.0], [-6.0, -6.0]]],
            ])
        );
    }

    #[test]
    fn for_each_position_mut_recurses_into_geometry_collections() {
        let mut g = Geometry::GeometryCollection(vec![
            Geometry::Point([1.0, 2.0]),
            Geometry::LineString(vec![[3.0, 4.0], [5.0, 6.0]]),
            Geometry::GeometryCollection(vec![Geometry::Point([7.0, 8.0])]),
        ]);
        g.for_each_position_mut(negate);
        assert_eq!(
            g,
            Geometry::GeometryCollection(vec![
                Geometry::Point([-1.0, -2.0]),
                Geometry::LineString(vec![[-3.0, -4.0], [-5.0, -6.0]]),
                Geometry::GeometryCollection(vec![Geometry::Point([-7.0, -8.0])]),
            ])
        );
    }

    #[test]
    fn for_each_position_mut_counts_every_position_exactly_once() {
        // MultiLineString + MultiPoint exercise the remaining variants; a
        // counting closure catches over- or under-visiting that an equality
        // check on symmetric negation could miss.
        let mut g = Geometry::GeometryCollection(vec![
            Geometry::MultiPoint(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]]),
            Geometry::MultiLineString(vec![vec![[0.0, 0.0], [1.0, 0.0]], vec![[2.0, 0.0], [3.0, 0.0], [4.0, 0.0]]]),
        ]);
        let mut count = 0;
        g.for_each_position_mut(|_| count += 1);
        assert_eq!(count, 3 + 2 + 3);
    }

    fn negate_slice(ps: &mut [Position]) {
        for p in ps {
            negate(p);
        }
    }

    #[test]
    fn for_each_position_run_mut_treats_a_point_as_a_length_one_run() {
        let mut g = Geometry::Point([1.0, 2.0]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![1]);
        g.for_each_position_run_mut(negate_slice);
        assert_eq!(g, Geometry::Point([-1.0, -2.0]));
    }

    #[test]
    fn for_each_position_run_mut_yields_the_whole_linestring_as_one_run() {
        let mut g = Geometry::LineString(vec![[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![3]);
    }

    #[test]
    fn for_each_position_run_mut_yields_one_run_per_polygon_ring() {
        let mut g = Geometry::Polygon(vec![
            vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 0.0]],
            vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 1.0]],
        ]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![4, 4]);
        g.for_each_position_run_mut(negate_slice);
        assert_eq!(
            g,
            Geometry::Polygon(vec![
                vec![[0.0, 0.0], [-4.0, 0.0], [-4.0, -4.0], [0.0, 0.0]],
                vec![[-1.0, -1.0], [-2.0, -1.0], [-2.0, -2.0], [-1.0, -1.0]],
            ])
        );
    }

    #[test]
    fn for_each_position_run_mut_yields_one_run_per_multipolygon_ring() {
        // Two polygons, the second with a hole: 1 + 2 rings = 3 runs total,
        // each still keyed to its own polygon's ring, not flattened across
        // polygons.
        let mut g = Geometry::MultiPolygon(vec![
            vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]],
            vec![
                vec![[0.0, 0.0], [4.0, 0.0], [4.0, 4.0], [0.0, 0.0]],
                vec![[1.0, 1.0], [2.0, 1.0], [2.0, 2.0], [1.0, 1.0]],
            ],
        ]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![3, 4, 4]);
    }

    #[test]
    fn for_each_position_run_mut_recurses_into_geometry_collections() {
        let mut g = Geometry::GeometryCollection(vec![
            Geometry::Point([1.0, 2.0]),
            Geometry::LineString(vec![[3.0, 4.0], [5.0, 6.0]]),
            Geometry::GeometryCollection(vec![Geometry::MultiPoint(vec![[7.0, 8.0], [9.0, 10.0]])]),
        ]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![1, 2, 2]);
        g.for_each_position_run_mut(negate_slice);
        assert_eq!(
            g,
            Geometry::GeometryCollection(vec![
                Geometry::Point([-1.0, -2.0]),
                Geometry::LineString(vec![[-3.0, -4.0], [-5.0, -6.0]]),
                Geometry::GeometryCollection(vec![Geometry::MultiPoint(vec![[-7.0, -8.0], [-9.0, -10.0]])]),
            ])
        );
    }

    #[test]
    fn for_each_position_run_mut_and_pointwise_mut_agree_on_total_coverage() {
        // Same coordinate set, visited two different ways: the total number
        // of coordinates touched must match regardless of run grouping.
        let mut g = Geometry::MultiPolygon(vec![
            vec![vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]],
            vec![vec![[5.0, 5.0], [6.0, 5.0], [6.0, 6.0], [5.0, 6.0]]],
        ]);
        let mut by_point = 0;
        g.for_each_position_mut(|_| by_point += 1);
        let mut by_run = 0;
        g.for_each_position_run_mut(|ps| by_run += ps.len());
        assert_eq!(by_point, by_run);
    }
}
