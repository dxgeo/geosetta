//! Proves the read seam -- [`geosetta::GeometryRef::shape`] and the views in
//! [`geosetta::geometry`] -- lets a caller read coordinates without the copy
//! [`geosetta::GeometryRef::to_geometry`] makes, against the consumer it was
//! made public for: nazca, whose [`nazca::Sink`] names "`geosetta`'s IR" as
//! one of the sources an adapter drives it from (`nazca/src/sink.rs`).
//!
//! The adapter below is what that caller writes. Its oracle is independent of
//! it: nazca's own WKB reader, fed geosetta's WKB of the same geometry, must
//! drive a sink through the same events. Each case runs under every layout,
//! so both of a block's forms are walked (`plans/columnar.org` D-C6), and the
//! 2D case checks the flat form hands nazca its coordinates as slices, in
//! place, rather than a position at a time.
//!
//! nazca is a dev-dependency of this test only (`Cargo.toml`): the libraries
//! stay peers with no edge between them (`../architecture.org`).

// The wildcard arm in `drive` is reachable only because `Shape` is
// `#[non_exhaustive]`, as `tests/public_api.rs` enforces for the other open
// vocabularies.
#![deny(unreachable_patterns)]

use geosetta::geometry::{Rings, Run, Shape};
use geosetta::{Format, GeometryRef, Layout, ReadOptions};
use nazca::Sink;

/// Drive `sink` from one geometry, read in place: what a renderer embedding
/// geosetta writes, and all it needs.
fn drive(g: GeometryRef<'_>, sink: &mut impl Sink) {
    match g.shape() {
        Shape::Point(p) => sink.point([p.x, p.y]),
        Shape::MultiPoint(ps) => ps.iter().for_each(|p| sink.point([p.x, p.y])),
        Shape::LineString(ps) => line(ps, sink),
        Shape::MultiLineString(lines) => lines.iter().for_each(|ps| line(ps, sink)),
        Shape::Polygon(rings) => polygon(rings, sink),
        Shape::MultiPolygon(parts) => parts.iter().for_each(|rings| polygon(rings, sink)),
        Shape::GeometryCollection(members) => members.iter().for_each(|m| drive(m, sink)),
        other => panic!("a shape this adapter does not know: {other:?}"),
    }
}

fn line(ps: Run<'_>, sink: &mut impl Sink) {
    sink.begin_line();
    run(ps, sink);
    sink.end_line();
}

fn polygon(rings: Rings<'_>, sink: &mut impl Sink) {
    sink.begin_polygon();
    for ps in rings {
        sink.begin_ring();
        run(ps, sink);
        sink.end_ring();
    }
    sink.end_polygon();
}

/// A run's coordinates: the flat buffer itself when there is one, else
/// gathered a chunk at a time, as nazca's own WKB reader splits a run.
fn run(ps: Run<'_>, sink: &mut impl Sink) {
    if let Some(xy) = ps.xy_2d() {
        return sink.points(xy);
    }
    let mut chunk = Vec::with_capacity(64);
    for p in ps {
        chunk.push([p.x, p.y]);
        if chunk.len() == 64 {
            sink.points(&chunk);
            chunk.clear();
        }
    }
    if !chunk.is_empty() {
        sink.points(&chunk);
    }
}

#[derive(Debug, PartialEq)]
enum Event {
    Point([f64; 2]),
    /// One run of coordinates, however many `points` calls carried it.
    Run(Vec<[f64; 2]>),
    BeginLine,
    EndLine,
    BeginPolygon,
    EndPolygon,
    BeginRing,
    EndRing,
}

/// Records the events, joining consecutive `points` calls into one run as
/// the [`Sink`] contract says a sink must, and counting the calls.
#[derive(Default)]
struct Recorder {
    events: Vec<Event>,
    points_calls: usize,
}

impl Sink for Recorder {
    fn point(&mut self, xy: [f64; 2]) {
        self.events.push(Event::Point(xy));
    }

    fn points(&mut self, xy: &[[f64; 2]]) {
        self.points_calls += 1;
        match self.events.last_mut() {
            Some(Event::Run(run)) => run.extend_from_slice(xy),
            _ => self.events.push(Event::Run(xy.to_vec())),
        }
    }

    fn begin_line(&mut self) {
        self.events.push(Event::BeginLine);
    }

    fn end_line(&mut self) {
        self.events.push(Event::EndLine);
    }

    fn begin_polygon(&mut self) {
        self.events.push(Event::BeginPolygon);
    }

    fn end_polygon(&mut self) {
        self.events.push(Event::EndPolygon);
    }

    fn begin_ring(&mut self) {
        self.events.push(Event::BeginRing);
    }

    fn end_ring(&mut self) {
        self.events.push(Event::EndRing);
    }
}

/// Every geometry type, a hole, a long ring (past a chunk), and a
/// collection nested in a collection.
fn geojson(with_z: bool) -> String {
    let c = |x: f64, y: f64| if with_z { format!("[{x},{y},{}]", x + y) } else { format!("[{x},{y}]") };
    let ring = |n: usize, r: f64| {
        let mut ps: Vec<String> = (0..n)
            .map(|i| {
                let a = i as f64 / n as f64 * std::f64::consts::TAU;
                c(10.0 + r * a.cos(), 20.0 + r * a.sin())
            })
            .collect();
        ps.push(ps[0].clone());
        format!("[{}]", ps.join(","))
    };
    let geometries = [
        format!(r#"{{"type":"Point","coordinates":{}}}"#, c(1.0, 2.0)),
        format!(r#"{{"type":"MultiPoint","coordinates":[{},{}]}}"#, c(1.0, 2.0), c(3.0, 4.0)),
        format!(r#"{{"type":"LineString","coordinates":[{},{},{}]}}"#, c(0.0, 0.0), c(1.0, 1.0), c(2.0, 0.0)),
        format!(r#"{{"type":"MultiLineString","coordinates":[[{},{}],[{},{}]]}}"#, c(0.0, 0.0), c(1.0, 1.0), c(5.0, 5.0), c(6.0, 6.0)),
        format!(r#"{{"type":"Polygon","coordinates":[{},{}]}}"#, ring(200, 5.0), ring(5, 1.0)),
        format!(r#"{{"type":"MultiPolygon","coordinates":[[{}],[{},{}]]}}"#, ring(4, 1.0), ring(6, 3.0), ring(3, 0.5)),
        format!(
            r#"{{"type":"GeometryCollection","geometries":[{{"type":"Point","coordinates":{}}},{{"type":"GeometryCollection","geometries":[{{"type":"LineString","coordinates":[{},{}]}}]}}]}}"#,
            c(7.0, 8.0),
            c(0.0, 0.0),
            c(9.0, 9.0)
        ),
    ];
    let features: Vec<String> =
        geometries.iter().map(|g| format!(r#"{{"type":"Feature","properties":{{}},"geometry":{g}}}"#)).collect();
    format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))
}

fn read(input: &str, layout: Layout) -> geosetta::FeatureCollection {
    let mut warnings = Vec::new();
    let options = ReadOptions { layout, ..ReadOptions::default() };
    let fc = geosetta::read_features_with(Format::GeoJson, input.as_bytes(), &mut warnings, options).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    fc
}

/// The adapter's events for every feature, against nazca's own WKB reader's
/// for the same geometry; and how many `points` calls the adapter made.
fn check(fc: &geosetta::FeatureCollection, what: &str) -> usize {
    let mut calls = 0;
    for (i, f) in fc.rows().enumerate() {
        let g = f.geometry().unwrap();
        let mut adapted = Recorder::default();
        drive(g, &mut adapted);
        let mut oracle = Recorder::default();
        nazca::wkb::read(&geosetta::geometry::to_wkb(&g.to_geometry()), &mut oracle).unwrap();
        assert_eq!(adapted.events, oracle.events, "{what}: feature {i} ({})", g.type_name());
        calls += adapted.points_calls;
    }
    calls
}

/// One `points` call per run, the coordinates handed over in place: what a
/// flat 2D block gives a caller that the owned form cannot.
const RUNS: usize = 1 + 2 + 2 + 3 + 1;

#[test]
fn nazca_reads_a_2d_collection_in_place_under_every_layout() {
    let input = geojson(false);
    for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
        let calls = check(&read(&input, layout), &format!("{layout:?}"));
        match layout {
            Layout::Rows => assert!(calls > RUNS, "rows hold owned geometry, gathered in chunks: {calls}"),
            _ => assert_eq!(calls, RUNS, "{layout:?}: one slice a run"),
        }
    }
}

#[test]
fn nazca_reads_a_3d_collection_under_every_layout() {
    let input = geojson(true);
    for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
        check(&read(&input, layout), &format!("{layout:?}"));
    }
}

/// An edit replaces a flat row through the overlay (D-C12, D-C16); the view
/// reads the edited geometry, and its neighbours still from the buffers.
#[test]
fn nazca_reads_an_edited_flat_collection() {
    let mut fc = read(&geojson(false), Layout::Columns);
    fc.edit_geometry(4, |g| {
        if let Some(geosetta::Geometry::Polygon(rings)) = g {
            rings.truncate(1);
            rings[0].reverse();
        }
    });
    fc.set_geometry(0, Some(geosetta::Geometry::LineString(vec![
        geosetta::Position::new(1.0, 1.0),
        geosetta::Position::new(2.0, 2.0),
    ])));
    check(&fc, "edited");
    assert_eq!(fc.row(0).geometry().unwrap().type_name(), "LineString");
}
