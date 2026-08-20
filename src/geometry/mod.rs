//! Geometry model shared by the readers/writers, plus bounding-box support.
//!
//! Coordinates are X/Y (longitude/latitude) plus optional Z (elevation) and
//! M (linear-referencing measure) ordinates, present only where a source
//! format actually declares them.

mod wkb;
mod wkt;

pub use wkb::decode as from_wkb;
pub use wkb::encode as to_wkb;
pub use wkt::decode as from_wkt;
pub use wkt::encode as to_wkt;

/// A single coordinate. `x`/`y` are always present; `z` (elevation) and `m`
/// (measure) are per-position, since some formats (e.g. Shapefile's M
/// "no-data" sentinel) allow an individual point within an otherwise
/// Z/M-bearing geometry to omit one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub z: Option<f64>,
    pub m: Option<f64>,
}

impl Position {
    /// A 2D position — no Z, no M. The common case.
    pub fn new(x: f64, y: f64) -> Self {
        Position { x, y, z: None, m: None }
    }

    /// An X/Y/Z position (no M).
    pub fn with_z(x: f64, y: f64, z: f64) -> Self {
        Position { x, y, z: Some(z), m: None }
    }

    /// An X/Y/M position (no Z).
    pub fn with_m(x: f64, y: f64, m: f64) -> Self {
        Position { x, y, z: None, m: Some(m) }
    }

    /// A full X/Y/Z/M position.
    pub fn with_zm(x: f64, y: f64, z: f64, m: f64) -> Self {
        Position { x, y, z: Some(z), m: Some(m) }
    }
}

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

    /// Whether any position in this geometry carries an M (measure)
    /// ordinate. Used by [`crate::feature::FeatureCollection::has_m`] to
    /// decide whether converting to a format with no M concept (GeoJSON,
    /// KML/KMZ) would silently drop data — see
    /// [`crate::feature::FeatureCollection::m_downgrade_warning`].
    pub fn has_m(&self) -> bool {
        match self {
            Geometry::Point(p) => p.m.is_some(),
            Geometry::LineString(ps) | Geometry::MultiPoint(ps) => ps.iter().any(|p| p.m.is_some()),
            Geometry::Polygon(rings) | Geometry::MultiLineString(rings) => {
                rings.iter().flatten().any(|p| p.m.is_some())
            }
            Geometry::MultiPolygon(polys) => polys.iter().flatten().flatten().any(|p| p.m.is_some()),
            Geometry::GeometryCollection(geoms) => geoms.iter().any(Geometry::has_m),
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
    ///
    /// **The Z/M contract**: `Position::z`/`Position::m`, when present, are
    /// handed to the callback exactly like `x`/`y` — this seam makes no
    /// promise about what a purely horizontal reprojection does with them.
    /// A 2D-only backend (e.g. `wbprojection`, the dev-dependency
    /// `tests/reproject_pipe.rs` verifies against) will typically leave `z`
    /// untouched, which is correct per Geosetta's own "label, never
    /// reproject" posture — Z passes through unexamined by default, the
    /// same as every other ordinate this crate doesn't interpret. A caller
    /// wanting a full 3D transform (e.g. a vertical-datum shift alongside
    /// the horizontal one) is free to read and rewrite `p.z` itself inside
    /// the closure; nothing here prevents it. `m` is never touched by any
    /// reprojection concern at all — it's a linear-referencing measure, not
    /// a spatial coordinate — so a backend has no reason to alter it.
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
    ///
    /// Same Z/M contract as [`Self::for_each_position_mut`]: both ordinates
    /// are handed to the callback unexamined, and it's the caller's choice
    /// whether to touch `z` at all.
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
///
/// X and Y are always tracked. Z and M are tracked *only when some position
/// carries them* — see [`Bbox::z`]/[`Bbox::m`] — which is what makes this
/// usable for GeoPackage's dimension-specific envelope (`plans/envelope.org`)
/// without disturbing the XY-only consumers (the Hilbert ordering and the
/// FlatGeobuf/GeoPackage R-tree indexes, which are 2D by definition).
#[derive(Debug, Clone, Copy)]
pub struct Bbox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
    /// `(min, max)` Z across every position that carried one, or `None` if
    /// none did. Mixed input (some positions 3D, some 2D) folds in whichever
    /// positions have a Z rather than erroring — matching the WKB codec's
    /// "fall back gracefully, never panic" posture for the same case.
    pub z: Option<(f64, f64)>,
    /// `(min, max)` M across every position that carried one, or `None`.
    /// Same mixed-input rule as [`Bbox::z`].
    pub m: Option<(f64, f64)>,
}

impl Bbox {
    /// An empty box (inverted bounds) ready to absorb points.
    pub fn empty() -> Self {
        Bbox {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
            z: None,
            m: None,
        }
    }

    /// Grow the box to include `p`, in every ordinate `p` actually carries.
    pub fn add(&mut self, p: Position) {
        self.min_x = self.min_x.min(p.x);
        self.min_y = self.min_y.min(p.y);
        self.max_x = self.max_x.max(p.x);
        self.max_y = self.max_y.max(p.y);
        if let Some(z) = p.z {
            self.z = Some(match self.z {
                Some((lo, hi)) => (lo.min(z), hi.max(z)),
                None => (z, z),
            });
        }
        if let Some(m) = p.m {
            self.m = Some(match self.m {
                Some((lo, hi)) => (lo.min(m), hi.max(m)),
                None => (m, m),
            });
        }
    }

    /// True until at least one point has been added.
    ///
    /// Judged on X/Y alone: a box with no Z is not empty, it is 2D.
    pub fn is_empty(&self) -> bool {
        self.min_x > self.max_x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Bbox: Z/M folding (plans/envelope.org V1) ---------------------------
    //
    // `Bbox::add` has always taken a whole `Position`, so since zm-geometry.org's
    // M1 it has been handed Z and M and thrown them away. These pin down what it
    // keeps now.

    #[test]
    fn bbox_of_2d_positions_has_no_z_or_m() {
        let mut b = Bbox::empty();
        b.add(Position::new(1.0, 2.0));
        b.add(Position::new(5.0, -3.0));
        assert_eq!((b.min_x, b.max_x, b.min_y, b.max_y), (1.0, 5.0, -3.0, 2.0));
        assert_eq!(b.z, None);
        assert_eq!(b.m, None);
    }

    #[test]
    fn bbox_folds_z_and_m_independently() {
        let mut b = Bbox::empty();
        b.add(Position::with_zm(0.0, 0.0, 10.0, 100.0));
        b.add(Position::with_zm(1.0, 1.0, -4.0, 250.0));
        assert_eq!(b.z, Some((-4.0, 10.0)));
        assert_eq!(b.m, Some((100.0, 250.0)));
    }

    #[test]
    fn bbox_tracks_z_only_or_m_only() {
        let mut z = Bbox::empty();
        z.add(Position::with_z(0.0, 0.0, 7.0));
        assert_eq!(z.z, Some((7.0, 7.0)));
        assert_eq!(z.m, None, "a Z-only position must not invent an M range");

        let mut m = Bbox::empty();
        m.add(Position::with_m(0.0, 0.0, 7.0));
        assert_eq!(m.m, Some((7.0, 7.0)));
        assert_eq!(m.z, None, "an M-only position must not invent a Z range");
    }

    #[test]
    fn bbox_mixed_dimensionality_folds_whichever_ordinates_are_present() {
        // Some positions 3D, some 2D — fold the ones that have a Z rather than
        // erroring or discarding, matching the WKB codec's "fall back
        // gracefully, never panic" posture for the same case.
        let mut b = Bbox::empty();
        b.add(Position::new(0.0, 0.0));
        b.add(Position::with_z(10.0, 10.0, 5.0));
        b.add(Position::new(20.0, 20.0));
        b.add(Position::with_z(30.0, 30.0, -5.0));
        assert_eq!((b.min_x, b.max_x), (0.0, 30.0));
        assert_eq!(b.z, Some((-5.0, 5.0)));
        assert_eq!(b.m, None);
    }

    #[test]
    fn an_empty_bbox_stays_empty_and_a_2d_one_does_not() {
        let empty = Bbox::empty();
        assert!(empty.is_empty());
        assert_eq!(empty.z, None);

        // Emptiness is judged on X/Y alone: a box with no Z is 2D, not empty.
        let mut flat = Bbox::empty();
        flat.add(Position::new(1.0, 1.0));
        assert!(!flat.is_empty());
        assert_eq!(flat.z, None);
    }

    #[test]
    fn geometry_bbox_folds_z_through_the_whole_tree() {
        // `extend_bbox` walks every variant; confirm Z survives the nesting,
        // not just a bare Point.
        let g = Geometry::MultiPolygon(vec![vec![vec![
            Position::with_z(0.0, 0.0, 1.0),
            Position::with_z(4.0, 4.0, 9.0),
            Position::with_z(0.0, 4.0, 5.0),
        ]]]);
        let b = g.bbox();
        assert_eq!((b.min_x, b.max_x, b.min_y, b.max_y), (0.0, 4.0, 0.0, 4.0));
        assert_eq!(b.z, Some((1.0, 9.0)));
    }

    // A negate-both-ordinates stand-in for a real reprojection: cheap to
    // verify every position was actually visited (and none double-visited).
    fn negate(p: &mut Position) {
        p.x = -p.x;
        p.y = -p.y;
    }

    #[test]
    fn for_each_position_mut_visits_a_point() {
        let mut g = Geometry::Point(Position::new(1.0, 2.0));
        g.for_each_position_mut(negate);
        assert_eq!(g, Geometry::Point(Position::new(-1.0, -2.0)));
    }

    #[test]
    fn for_each_position_mut_visits_polygon_rings_including_holes() {
        let mut g = Geometry::Polygon(vec![
            vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 0.0)],
            vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 1.0)],
        ]);
        g.for_each_position_mut(negate);
        assert_eq!(
            g,
            Geometry::Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(-4.0, 0.0), Position::new(-4.0, -4.0), Position::new(0.0, 0.0)],
                vec![Position::new(-1.0, -1.0), Position::new(-2.0, -1.0), Position::new(-2.0, -2.0), Position::new(-1.0, -1.0)],
            ])
        );
    }

    #[test]
    fn for_each_position_mut_visits_multipolygon_parts() {
        let mut g = Geometry::MultiPolygon(vec![
            vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0), Position::new(1.0, 1.0)]],
            vec![vec![Position::new(5.0, 5.0), Position::new(6.0, 5.0), Position::new(6.0, 6.0)]],
        ]);
        g.for_each_position_mut(negate);
        assert_eq!(
            g,
            Geometry::MultiPolygon(vec![
                vec![vec![Position::new(0.0, 0.0), Position::new(-1.0, 0.0), Position::new(-1.0, -1.0)]],
                vec![vec![Position::new(-5.0, -5.0), Position::new(-6.0, -5.0), Position::new(-6.0, -6.0)]],
            ])
        );
    }

    #[test]
    fn for_each_position_mut_recurses_into_geometry_collections() {
        let mut g = Geometry::GeometryCollection(vec![
            Geometry::Point(Position::new(1.0, 2.0)),
            Geometry::LineString(vec![Position::new(3.0, 4.0), Position::new(5.0, 6.0)]),
            Geometry::GeometryCollection(vec![Geometry::Point(Position::new(7.0, 8.0))]),
        ]);
        g.for_each_position_mut(negate);
        assert_eq!(
            g,
            Geometry::GeometryCollection(vec![
                Geometry::Point(Position::new(-1.0, -2.0)),
                Geometry::LineString(vec![Position::new(-3.0, -4.0), Position::new(-5.0, -6.0)]),
                Geometry::GeometryCollection(vec![Geometry::Point(Position::new(-7.0, -8.0))]),
            ])
        );
    }

    #[test]
    fn for_each_position_mut_counts_every_position_exactly_once() {
        // MultiLineString + MultiPoint exercise the remaining variants; a
        // counting closure catches over- or under-visiting that an equality
        // check on symmetric negation could miss.
        let mut g = Geometry::GeometryCollection(vec![
            Geometry::MultiPoint(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 2.0)]),
            Geometry::MultiLineString(vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0)], vec![Position::new(2.0, 0.0), Position::new(3.0, 0.0), Position::new(4.0, 0.0)]]),
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
        let mut g = Geometry::Point(Position::new(1.0, 2.0));
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![1]);
        g.for_each_position_run_mut(negate_slice);
        assert_eq!(g, Geometry::Point(Position::new(-1.0, -2.0)));
    }

    #[test]
    fn for_each_position_run_mut_yields_the_whole_linestring_as_one_run() {
        let mut g = Geometry::LineString(vec![Position::new(0.0, 0.0), Position::new(1.0, 1.0), Position::new(2.0, 2.0)]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![3]);
    }

    #[test]
    fn for_each_position_run_mut_yields_one_run_per_polygon_ring() {
        let mut g = Geometry::Polygon(vec![
            vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 0.0)],
            vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 1.0)],
        ]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![4, 4]);
        g.for_each_position_run_mut(negate_slice);
        assert_eq!(
            g,
            Geometry::Polygon(vec![
                vec![Position::new(0.0, 0.0), Position::new(-4.0, 0.0), Position::new(-4.0, -4.0), Position::new(0.0, 0.0)],
                vec![Position::new(-1.0, -1.0), Position::new(-2.0, -1.0), Position::new(-2.0, -2.0), Position::new(-1.0, -1.0)],
            ])
        );
    }

    #[test]
    fn for_each_position_run_mut_yields_one_run_per_multipolygon_ring() {
        // Two polygons, the second with a hole: 1 + 2 rings = 3 runs total,
        // each still keyed to its own polygon's ring, not flattened across
        // polygons.
        let mut g = Geometry::MultiPolygon(vec![
            vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0), Position::new(1.0, 1.0)]],
            vec![
                vec![Position::new(0.0, 0.0), Position::new(4.0, 0.0), Position::new(4.0, 4.0), Position::new(0.0, 0.0)],
                vec![Position::new(1.0, 1.0), Position::new(2.0, 1.0), Position::new(2.0, 2.0), Position::new(1.0, 1.0)],
            ],
        ]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![3, 4, 4]);
    }

    #[test]
    fn for_each_position_run_mut_recurses_into_geometry_collections() {
        let mut g = Geometry::GeometryCollection(vec![
            Geometry::Point(Position::new(1.0, 2.0)),
            Geometry::LineString(vec![Position::new(3.0, 4.0), Position::new(5.0, 6.0)]),
            Geometry::GeometryCollection(vec![Geometry::MultiPoint(vec![Position::new(7.0, 8.0), Position::new(9.0, 10.0)])]),
        ]);
        let mut runs = Vec::new();
        g.for_each_position_run_mut(|ps| runs.push(ps.len()));
        assert_eq!(runs, vec![1, 2, 2]);
        g.for_each_position_run_mut(negate_slice);
        assert_eq!(
            g,
            Geometry::GeometryCollection(vec![
                Geometry::Point(Position::new(-1.0, -2.0)),
                Geometry::LineString(vec![Position::new(-3.0, -4.0), Position::new(-5.0, -6.0)]),
                Geometry::GeometryCollection(vec![Geometry::MultiPoint(vec![Position::new(-7.0, -8.0), Position::new(-9.0, -10.0)])]),
            ])
        );
    }

    #[test]
    fn for_each_position_run_mut_and_pointwise_mut_agree_on_total_coverage() {
        // Same coordinate set, visited two different ways: the total number
        // of coordinates touched must match regardless of run grouping.
        let mut g = Geometry::MultiPolygon(vec![
            vec![vec![Position::new(0.0, 0.0), Position::new(1.0, 0.0), Position::new(1.0, 1.0)]],
            vec![vec![Position::new(5.0, 5.0), Position::new(6.0, 5.0), Position::new(6.0, 6.0), Position::new(5.0, 6.0)]],
        ]);
        let mut by_point = 0;
        g.for_each_position_mut(|_| by_point += 1);
        let mut by_run = 0;
        g.for_each_position_run_mut(|ps| by_run += ps.len());
        assert_eq!(by_point, by_run);
    }
}
