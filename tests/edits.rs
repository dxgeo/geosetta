//! **Every edit a caller can make today, it can still make**
//! (`plans/columnar.org` D-C16, C6 step 2).
//!
//! Editing what was read, through the API, is a goal of the crate, for every
//! attribute of a feature. These pin the edits a caller makes -- to geometry:
//! replace one, change its type, add and remove parts and rings, give one
//! position a Z or M its neighbours lack and take one away, edit inside a
//! collection; to properties: rename a key in its place, reorder, insert
//! anywhere, repeat a key, change a value's sign bit; to a whole feature at
//! once; each after a filter and a reorder -- against the same edits made to
//! plain `Vec<Feature>`s, the row IR's own data. Written against the storage before C6, they pass unchanged
//! after it: whatever holds a block's geometry, the collection after an edit
//! is the one the row IR would hold, and so is every file written from it,
//! under every layout.
//!
//! In `tests/` on purpose, like `tests/public_api.rs`: only the API a
//! dependent sees. A geometry is read back through `to_feature`, which C6
//! keeps, rather than `geometry()`, whose return type it changes.

use geosetta::{Feature, FeatureCollection, Format, Geometry, JsonValue, Layout, Position, ReadOptions, Value};

fn xy(x: f64, y: f64) -> Position {
    Position::new(x, y)
}

fn ring(x: f64, y: f64, d: f64) -> Vec<Position> {
    vec![xy(x, y), xy(x + d, y), xy(x + d, y + d), xy(x, y + d), xy(x, y)]
}

/// One of each geometry, and none; a Z line, an M line and a ZM point among
/// them. Each feature's `n` names it however the rows are reordered.
fn base() -> Vec<Feature> {
    let geometries = [
        Some(Geometry::Point(xy(1.0, 2.0))),
        Some(Geometry::LineString(vec![
            Position::with_z(0.0, 0.0, 10.0),
            Position::with_z(1.0, 1.0, 11.0),
            Position::with_z(2.0, 0.0, 12.0),
        ])),
        Some(Geometry::Polygon(vec![ring(0.0, 0.0, 10.0)])),
        Some(Geometry::MultiPolygon(vec![vec![ring(0.0, 0.0, 1.0)], vec![ring(5.0, 5.0, 1.0)]])),
        Some(Geometry::GeometryCollection(vec![
            Geometry::Point(xy(3.0, 3.0)),
            Geometry::LineString(vec![xy(0.0, 0.0), xy(3.0, 4.0)]),
        ])),
        None,
        Some(Geometry::LineString(vec![Position::with_m(0.0, 0.0, 1.0), Position::with_m(3.0, 0.0, 2.0)])),
        Some(Geometry::MultiPoint(vec![xy(7.0, 7.0), xy(8.0, 8.0)])),
        Some(Geometry::Point(Position::with_zm(4.0, 5.0, 6.0, 7.0))),
    ];
    geometries
        .into_iter()
        .enumerate()
        .map(|(n, geometry)| Feature {
            geometry,
            properties: vec![
                ("n".into(), JsonValue::Int(n as i64)),
                ("name".into(), JsonValue::String(format!("f{n}"))),
                ("v".into(), JsonValue::Float(n as f64 * 1.5 - 6.0)),
            ],
            native: None,
        })
        .collect()
}

/// The structural edits, each to one geometry: the same closure is given a
/// plain feature's geometry and the collection's, so what is compared is
/// where the edit lands and what the storage keeps of it.
fn structural(n: i64) -> Option<fn(&mut Option<Geometry>)> {
    Some(match n {
        // A type change.
        0 => |g| *g = Some(Geometry::Polygon(vec![ring(1.0, 2.0, 0.5)])),
        // A ring added.
        2 => |g| {
            let Some(Geometry::Polygon(rings)) = g else { panic!("{g:?}") };
            rings.push(ring(2.0, 2.0, 1.0));
        },
        // A part taken away.
        3 => |g| {
            let Some(Geometry::MultiPolygon(parts)) = g else { panic!("{g:?}") };
            parts.remove(0);
        },
        // One position loses its Z; its neighbours keep theirs.
        1 => |g| {
            let Some(Geometry::LineString(ps)) = g else { panic!("{g:?}") };
            ps[1].z = None;
        },
        // One position gains a Z its neighbour lacks, beside its M.
        6 => |g| {
            let Some(Geometry::LineString(ps)) = g else { panic!("{g:?}") };
            ps[0].z = Some(-0.0);
        },
        // Inside a collection: a member moved, a member added.
        4 => |g| {
            let Some(Geometry::GeometryCollection(members)) = g else { panic!("{g:?}") };
            members[0] = Geometry::Point(xy(-3.0, -3.0));
            members.push(Geometry::Polygon(vec![ring(9.0, 9.0, 1.0)]));
        },
        // A geometry given to a feature that had none: an empty part, and
        // an empty polygon ring, among real ones.
        5 => |g| {
            *g = Some(Geometry::GeometryCollection(vec![
                Geometry::MultiLineString(vec![vec![xy(0.0, 0.0), xy(1.0, 0.0)], vec![]]),
                Geometry::Polygon(vec![ring(0.0, 0.0, 2.0), vec![]]),
            ]))
        },
        // A geometry taken away.
        8 => |g| *g = None,
        _ => return None,
    })
}

/// Every position: moved, and the one at x = 3 given a Z where it has none
/// and its M taken away where it has one.
fn positionwise(p: &mut Position) {
    p.y += 1000.0;
    if p.x == 3.0 {
        p.z.get_or_insert(0.5);
        p.m = None;
    }
}

/// A line appended to, per feature, by index: what a bulk structural edit
/// does.
fn bulk(g: &mut Option<Geometry>) {
    if let Some(Geometry::LineString(ps)) = g {
        let last = *ps.last().unwrap();
        ps.push(Position { x: last.x + 1.0, ..last });
    }
}

/// The feature's `n`, wherever an edit has moved it; -1 once one has taken
/// it away.
fn n_of(feature: &Feature) -> i64 {
    n_in(&feature.properties).unwrap_or(-1)
}

fn pushed() -> Feature {
    Feature {
        geometry: Some(Geometry::LineString(vec![xy(0.0, 0.0), xy(1.0, 1.0)])),
        properties: vec![
            ("n".into(), JsonValue::Int(9)),
            ("name".into(), JsonValue::String("f9".into())),
            ("v".into(), JsonValue::Float(7.5)),
        ],
        native: None,
    }
}

/// The edits as the row IR makes them: ordinary Rust on a `Vec<Feature>`.
fn edited_rows() -> Vec<Feature> {
    let mut rows = base();
    rows.reverse();
    rows.retain(|f| n_of(f) != 7);
    for f in &mut rows {
        if let Some(edit) = structural(n_of(f)) {
            edit(&mut f.geometry);
        }
    }
    for f in &mut rows {
        bulk(&mut f.geometry);
    }
    for f in &mut rows {
        if let Some(g) = &mut f.geometry {
            g.for_each_position_mut(positionwise);
        }
    }
    for f in &mut rows {
        if let Some(g) = &mut f.geometry {
            g.for_each_position_run_mut(|run| run.iter_mut().for_each(|p| p.x *= 2.0));
        }
    }
    rows.push(pushed());
    if let Some(Geometry::LineString(ps)) = &mut rows.last_mut().unwrap().geometry {
        ps.insert(0, Position::with_z(-1.0, -1.0, 3.0));
    }
    rows
}

/// The same edits through the collection's API.
fn edit(fc: &mut FeatureCollection) {
    let order: Vec<usize> = (0..fc.len()).rev().collect();
    fc.permute(&order);
    fc.retain(|f| f.get("n") != Some(Value::Int(7)));
    for i in 0..fc.len() {
        let Some(Value::Int(n)) = fc.row(i).get("n") else { unreachable!() };
        match (n, structural(n)) {
            // Through `set_geometry`, as a caller replacing one would.
            (0 | 5 | 8, Some(edit)) => {
                let mut g = fc.row(i).to_feature().geometry;
                edit(&mut g);
                fc.set_geometry(i, g);
            }
            (_, Some(edit)) => fc.edit_geometry(i, edit),
            (_, None) => {}
        }
    }
    let len = fc.len();
    let mut seen = Vec::new();
    fc.for_each_geometry_mut(|i, g| {
        seen.push(i);
        bulk(g);
    });
    assert_eq!(seen, (0..len).collect::<Vec<_>>(), "every feature, once, in order");
    fc.for_each_position_mut(positionwise);
    fc.for_each_position_run_mut(|run| run.iter_mut().for_each(|p| p.x *= 2.0));
    // An appended feature, edited where it landed.
    fc.push(pushed());
    let last = fc.len() - 1;
    let had = fc.edit_geometry(last, |g| {
        let Some(Geometry::LineString(ps)) = g else { return false };
        ps.insert(0, Position::with_z(-1.0, -1.0, 3.0));
        true
    });
    assert!(had, "edit_geometry returns what its closure does");
}

/// A collection whose features are `rows`, carrying `like`'s
/// collection-level fields.
fn built(rows: Vec<Feature>, like: &FeatureCollection) -> FeatureCollection {
    let mut fc = FeatureCollection::from_features(rows);
    fc.crs = like.crs.clone();
    fc.name = like.name.clone();
    fc.schema = like.schema.clone();
    fc.native = like.native.clone();
    fc
}

/// Every way to write `fc`, as what a byte comparison can check: the
/// single-buffer formats as bytes or the error, a Shapefile as its files,
/// and a GeoPackage (whose header records when it was written) as what it
/// reads back as.
fn written(fc: &FeatureCollection) -> Vec<(Format, Result<Vec<Vec<u8>>, String>)> {
    Format::ALL
        .into_iter()
        .map(|format| {
            let out = match format {
                Format::Shapefile => geosetta::shapefile::write(fc).map(|e| vec![e.shp, e.shx, e.dbf]),
                Format::Gpkg => {
                    geosetta::geopackage::write_layers(None, &[("t".into(), fc.clone())], &Default::default(), false, false)
                        .and_then(|bytes| geosetta::geopackage::read_layers(&bytes))
                        .map(|layers| {
                            layers[0].1.rows().map(|f| format!("{:?}", f.to_feature()).into_bytes()).collect()
                        })
                }
                _ => geosetta::write_features(format, fc).map(|bytes| vec![bytes]),
            };
            (format, out.map_err(|e| e.to_string()))
        })
        .collect()
}

/// The source the edits start from, read under `layout`, through a format
/// that holds every one of [`base`]'s geometries.
fn read(layout: Layout) -> FeatureCollection {
    let bytes = geosetta::write_features(Format::Parquet, &FeatureCollection::from_features(base())).unwrap();
    let options = ReadOptions { layout, ..ReadOptions::default() };
    let fc = geosetta::read_features_with(Format::Parquet, &bytes, &mut Vec::new(), options).unwrap();
    let rows: Vec<Feature> = fc.rows().map(|f| f.to_feature()).collect();
    assert_eq!(rows, base(), "the source holds the fixture exactly");
    fc
}

#[test]
fn geometry_edits_through_the_api_are_the_row_irs_edits() {
    for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
        let mut fc = read(layout);
        edit(&mut fc);
        let rows: Vec<Feature> = fc.rows().map(|f| f.to_feature()).collect();
        assert_eq!(rows, edited_rows(), "{layout:?}");
    }
}

#[test]
fn an_edited_collection_writes_what_one_built_edited_writes() {
    for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
        let mut fc = read(layout);
        edit(&mut fc);
        let reference = built(edited_rows(), &fc);
        for ((format, got), (_, want)) in written(&fc).into_iter().zip(written(&reference)) {
            assert_eq!(got, want, "{format:?} under {layout:?}");
        }
    }
}

/// The edited collection reads what was edited even before anything folds
/// it, and a second edit of a geometry already edited starts from the first.
#[test]
fn an_edit_reads_back_at_once_and_edits_compose() {
    let mut fc = read(Layout::Columns);
    fc.edit_geometry(0, |g| *g = Some(Geometry::Point(xy(1.0, 1.0))));
    assert_eq!(fc.row(0).to_feature().geometry, Some(Geometry::Point(xy(1.0, 1.0))));
    fc.edit_geometry(0, |g| {
        let Some(Geometry::Point(p)) = g else { panic!("{g:?}") };
        p.m = Some(2.0);
    });
    fc.for_each_position_mut(|p| p.x += 1.0);
    assert_eq!(fc.row(0).to_feature().geometry, Some(Geometry::Point(Position { x: 2.0, y: 1.0, z: None, m: Some(2.0) })));
    // Untouched rows are as read.
    let rows: Vec<Feature> = fc.rows().skip(1).map(|f| f.to_feature()).collect();
    let mut want = base()[1..].to_vec();
    want.iter_mut().filter_map(|f| f.geometry.as_mut()).for_each(|g| g.for_each_position_mut(|p| p.x += 1.0));
    assert_eq!(rows, want);
}

type Members = Vec<(std::sync::Arc<str>, JsonValue)>;

/// The property edits, each to one feature's list, shared as
/// [`structural`] is.
fn property_edit(n: i64) -> Option<fn(&mut Members)> {
    Some(match n {
        // A key renamed in its place.
        0 => |ps| ps[1].0 = "label".into(),
        // Reordered: the last key first.
        1 => |ps| {
            let last = ps.pop().unwrap();
            ps.insert(0, last);
        },
        // A key inserted first.
        2 => |ps| ps.insert(0, ("first".into(), JsonValue::Bool(true))),
        // A key repeated, and the repeat edited.
        3 => |ps| {
            ps.push(("name".into(), JsonValue::String("again".into())));
            ps[3].1 = JsonValue::String("again, edited".into());
        },
        // 0.0 becomes -0.0, which `==` cannot tell apart.
        4 => |ps| ps[2].1 = JsonValue::Float(-0.0),
        // Every property taken away.
        6 => Vec::clear,
        _ => return None,
    })
}

/// A whole feature edited at once: geometry and properties together.
fn whole(f: &mut Feature) {
    f.geometry = Some(Geometry::Point(xy(80.0, 80.0)));
    f.properties.retain(|(k, _)| &**k != "name");
    f.properties.push(("whole".into(), JsonValue::Null));
}

/// A pass over every feature that edits the even ones and leaves the rest.
fn every(i: usize, f: &mut Feature) {
    if i % 2 == 0 {
        f.properties.push(("i".into(), JsonValue::Int(i as i64)));
    }
}

fn n_in(members: &Members) -> Option<i64> {
    members.iter().find(|(k, _)| &**k == "n").and_then(|(_, v)| match v {
        JsonValue::Int(n) => Some(*n),
        _ => None,
    })
}

fn properties_edited_rows() -> Vec<Feature> {
    let mut rows = base();
    rows.reverse();
    rows.retain(|f| n_of(f) != 7);
    for f in &mut rows {
        let n = n_of(f);
        if let Some(edit) = property_edit(n) {
            edit(&mut f.properties);
        }
        if n == 8 {
            whole(f);
        }
    }
    for (i, f) in rows.iter_mut().enumerate() {
        every(i, f);
    }
    rows
}

fn edit_properties(fc: &mut FeatureCollection) {
    let order: Vec<usize> = (0..fc.len()).rev().collect();
    fc.permute(&order);
    fc.retain(|f| f.get("n") != Some(Value::Int(7)));
    for i in 0..fc.len() {
        let n = fc.edit_properties(i, |ps| {
            let n = n_in(ps).unwrap();
            if let Some(edit) = property_edit(n) {
                edit(ps);
            }
            n
        });
        if n == 8 {
            fc.edit_feature(i, whole);
        }
    }
    fc.for_each_feature_mut(every);
}

#[test]
fn property_edits_through_the_api_are_the_row_irs_edits() {
    for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
        let mut fc = read(layout);
        edit_properties(&mut fc);
        let rows: Vec<Feature> = fc.rows().map(|f| f.to_feature()).collect();
        let want = properties_edited_rows();
        assert_eq!(rows, want, "{layout:?}");
        // `==` cannot see a sign bit; the edit made one.
        let v = |rows: &[Feature]| match rows.iter().find(|f| n_of(f) == 4).unwrap().properties[2].1 {
            JsonValue::Float(v) => v.to_bits(),
            _ => unreachable!(),
        };
        assert_eq!(v(&rows), v(&want), "{layout:?}: -0.0 kept");
    }
}

#[test]
fn a_collection_with_edited_properties_writes_what_one_built_edited_writes() {
    for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
        let mut fc = read(layout);
        edit_properties(&mut fc);
        let reference = built(properties_edited_rows(), &fc);
        for ((format, got), (_, want)) in written(&fc).into_iter().zip(written(&reference)) {
            assert_eq!(got, want, "{format:?} under {layout:?}");
        }
    }
}
