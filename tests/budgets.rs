//! **Do the memory and speed wins in `plans/performance.org` stay won?**
//!
//! Two kinds of guard, both run by a plain `cargo test`, both built so that
//! they do not flake:
//!
//! - *Memory and allocations* are counted, not timed. A counting
//!   `#[global_allocator]` (the `bench/memprobe` one) watches each conversion
//!   through `geosetta::pipeline` — the path the binary runs — and its peak,
//!   the IR it leaves resident, and its allocation count, each per feature,
//!   must stay under a budget set a little above what was measured when the
//!   budget was written. The count is deterministic, so a budget only fails
//!   when the code allocates more.
//! - *Speed* is checked as *shape*, not as a number: four times the rows, or
//!   four times the columns, must cost well under the sixteen times a
//!   quadratic pass would. Every speed regression `performance.org` records
//!   was that shape (items 1, 11, and Mem3's FlatGeobuf write), and a ratio
//!   measured on one machine within one run does not depend on the machine.
//!
//! Absolute throughput is `tests/perf.rs`'s, against floors, in release.
//!
//! **These numbers never outrank accuracy.** A budget is met by doing the same
//! work more cheaply — output byte-identical, which `bench/equiv.py` checks —
//! never by keeping less of the data. When fidelity work costs memory or time,
//! raise the budget and say why in `plans/performance.org` § BUDGETS.
//!
//! **A new format joins both sweeps by construction:** `cases()` names every
//! `Format` through an exhaustive match, so a new one does not compile until it
//! has a budget, and the scaling sweep runs `Format::ALL` on both sides.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use geosetta::pipeline::{self, ConvertOptions, Event, Source};
use geosetta::{shapefile, Format};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static ALLOCS: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grow(layout.size());
            ALLOCS.fetch_add(1, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        unsafe { System.dealloc(p, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, layout, new) };
        if !q.is_null() {
            LIVE.fetch_sub(layout.size(), Relaxed);
            grow(new);
            ALLOCS.fetch_add(1, Relaxed);
        }
        q
    }
}

fn grow(by: usize) {
    let now = LIVE.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The counters are process-wide, so the tests in this file take turns.
static SERIAL: Mutex<()> = Mutex::new(());

/// `n` features, ~80% points, ~15% linestrings, ~5% polygons, with four typed
/// properties — `tests/perf.rs`'s narrow shape.
fn narrow_geojson(n: usize) -> Vec<u8> {
    use std::fmt::Write;
    let mut s = String::from("{\"type\":\"FeatureCollection\",\"features\":[");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let x = -180.0 + (i as f64 * 0.001) % 360.0;
        let y = -90.0 + (i as f64 * 0.0007) % 180.0;
        let geometry = match i % 20 {
            0 => format!(
                "{{\"type\":\"Polygon\",\"coordinates\":[[[{x:.6},{y:.6}],[{:.6},{y:.6}],[{:.6},{:.6}],[{x:.6},{y:.6}]]]}}",
                x + 0.01,
                x + 0.01,
                y + 0.01
            ),
            1..=3 => format!("{{\"type\":\"LineString\",\"coordinates\":[[{x:.6},{y:.6}],[{:.6},{:.6}]]}}", x + 0.02, y + 0.02),
            _ => format!("{{\"type\":\"Point\",\"coordinates\":[{x:.6},{y:.6}]}}"),
        };
        write!(
            s,
            "{{\"type\":\"Feature\",\"geometry\":{geometry},\"properties\":{{\"id\":{i},\"name\":\"feature {i}\",\"value\":{:.3},\"flag\":{}}}}}",
            i as f64 * 1.5,
            i % 2 == 0
        )
        .unwrap();
    }
    s.push_str("]}");
    s.into_bytes()
}

/// `n` points with `cols` properties each, alternating integer, float and
/// string — the shape that exposes per-column cost.
fn wide_geojson(n: usize, cols: usize) -> Vec<u8> {
    use std::fmt::Write;
    let mut s = String::from("{\"type\":\"FeatureCollection\",\"features\":[");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        write!(s, "{{\"type\":\"Feature\",\"geometry\":{{\"type\":\"Point\",\"coordinates\":[{:.6},{:.6}]}},\"properties\":{{", (i % 360) as f64 - 180.0, (i % 180) as f64 - 90.0).unwrap();
        for c in 0..cols {
            if c > 0 {
                s.push(',');
            }
            match c % 3 {
                0 => write!(s, "\"c{c}\":{}", i * c),
                1 => write!(s, "\"c{c}\":{:.2}", (i + c) as f64 * 0.5),
                _ => write!(s, "\"c{c}\":\"v{}\"", i % 97),
            }
            .unwrap();
        }
        s.push_str("}}");
    }
    s.push_str("]}");
    s.into_bytes()
}

/// Every warning is dropped: these tests measure the conversion, not its
/// messages.
fn quiet(_: Event<'_>) {}

/// A source, already encoded: one buffer, or a Shapefile's siblings.
enum Input {
    Bytes(Vec<u8>),
    Shapefile(shapefile::Encoded),
}

impl Input {
    fn source(&self) -> Source<'_> {
        match self {
            Input::Bytes(b) => Source::Bytes(b),
            Input::Shapefile(e) => Source::Shapefile { shp: &e.shp, dbf: &e.dbf, prj: e.prj.as_deref(), cpg: Some(e.cpg) },
        }
    }
}

/// `input` converted from `from` to `to` through the pipeline, as the binary
/// would: every layer encoded, or one GeoPackage written. Returns the feature
/// count.
fn convert(from: Format, to: Format, input: &Input) -> usize {
    let prepared = pipeline::prepare(from, to, input.source(), ConvertOptions::default(), &mut quiet).unwrap();
    let features = prepared.layers().iter().map(|(_, fc)| fc.features.len()).sum();
    if to == Format::Gpkg {
        prepared.into_geopackage(None, &mut quiet).unwrap();
    } else {
        for i in 0..prepared.layers().len() {
            prepared.encode(i, &mut quiet).unwrap();
        }
    }
    features
}

/// The GeoJSON `geojson` written as `format`, to be read back as a source.
fn source_as(format: Format, geojson: &[u8]) -> Input {
    if format == Format::GeoJson {
        return Input::Bytes(geojson.to_vec());
    }
    let prepared =
        pipeline::prepare(Format::GeoJson, format, Source::Bytes(geojson), ConvertOptions::default(), &mut quiet).unwrap();
    if format == Format::Gpkg {
        return Input::Bytes(prepared.into_geopackage(None, &mut quiet).unwrap());
    }
    match prepared.encode(0, &mut quiet).unwrap() {
        pipeline::Output::Bytes(b) => Input::Bytes(b),
        pipeline::Output::Shapefile(e) => Input::Shapefile(e),
    }
}

/// The narrow table for a conversion: mixed geometry, unless Shapefile is on
/// either side, which holds one shape type per file and gets points with as
/// many properties instead.
fn narrow_for(from: Format, to: Format, n: usize) -> Vec<u8> {
    if from == Format::Shapefile || to == Format::Shapefile { wide_geojson(n, 4) } else { narrow_geojson(n) }
}

/// What one conversion cost, per feature.
#[derive(Debug, Clone, Copy)]
struct Cost {
    /// The highest live heap above the input, during the whole conversion.
    peak: usize,
    allocs: usize,
}

fn measure(from: Format, to: Format, input: &Input) -> Cost {
    let base = LIVE.load(Relaxed);
    PEAK.store(base, Relaxed);
    let allocs = ALLOCS.load(Relaxed);
    let features = convert(from, to, input).max(1);
    Cost {
        peak: (PEAK.load(Relaxed) - base) / features,
        allocs: (ALLOCS.load(Relaxed) - allocs) / features,
    }
}

/// A budget: per-feature ceilings on a conversion's peak heap and its
/// allocation count.
struct Budget {
    from: Format,
    to: Format,
    wide: bool,
    peak: usize,
    allocs: usize,
}

/// One case per format on each side, narrow, plus the wide cases
/// `performance.org` tracks. Ceilings are 20% over what was measured on
/// 2026-10-31 (at least two allocations over), at `NARROW` features, or
/// `WIDE_ROWS` × `WIDE_COLS`. **Lower one when a change wins memory back
/// with byte-identical output**, so the win is kept too; raise one for a
/// change that is worth its cost, fidelity above all, and say so in
/// `performance.org`.
///
/// **Exhaustive on purpose:** every `Format` appears in `source_budget` and
/// `target_budget`, and a new one does not compile until it has both.
/// Shapefile on either side measures a points table, since one `.shp` holds
/// one shape type.
fn cases() -> Vec<Budget> {
    fn source_budget(from: Format) -> Option<(usize, usize)> {
        // (peak bytes / feature, allocations / feature), read into GeoJSON.
        match from {
            // The target sweep's GeoJSON case is this one.
            Format::GeoJson => None,
            Format::Parquet => Some((832, 5)),
            Format::FlatGeobuf => Some((893, 6)),
            Format::Csv => Some((910, 20)),
            Format::Wkt => Some((435, 3)),
            Format::Gpkg => Some((893, 9)),
            Format::Shapefile => Some((791, 5)),
            // The read keeps each element for a KML write to replay
            // (`performance.org`, "KML is now the heaviest spoke").
            Format::Kml => Some((6791, 64)),
            Format::Kmz => Some((7395, 64)),
        }
    }
    fn target_budget(to: Format) -> (usize, usize) {
        // (peak bytes / feature, allocations / feature), written from GeoJSON.
        match to {
            Format::GeoJson => (915, 16),
            Format::Parquet => (743, 18),
            Format::FlatGeobuf => (934, 27),
            Format::Csv => (560, 38),
            Format::Wkt => (512, 23),
            // Every row is built as a `Vec<Value>` first (`performance.org`,
            // "Also seen, not yet acted on").
            Format::Gpkg => (1383, 50),
            Format::Shapefile => (552, 28),
            Format::Kml => (1067, 23),
            Format::Kmz => (2080, 23),
        }
    }
    let mut out = Vec::new();
    for format in Format::ALL {
        if let Some((peak, allocs)) = source_budget(format) {
            out.push(Budget { from: format, to: Format::GeoJson, wide: false, peak, allocs });
        }
        let (peak, allocs) = target_budget(format);
        out.push(Budget { from: Format::GeoJson, to: format, wide: false, peak, allocs });
    }
    // The wide table, the case the memory work was about.
    out.push(Budget { from: Format::GeoJson, to: Format::Parquet, wide: true, peak: 16_061, allocs: 336 });
    out.push(Budget { from: Format::GeoJson, to: Format::FlatGeobuf, wide: true, peak: 19_685, allocs: 347 });
    out.push(Budget { from: Format::Parquet, to: Format::GeoJson, wide: true, peak: 19_247, allocs: 86 });
    out
}

const NARROW: usize = 10_000;
const WIDE_ROWS: usize = 1_000;
const WIDE_COLS: usize = 200;

/// Peak heap and allocations per feature, for every format on each side, stay
/// under their budgets. `GEOSETTA_BUDGET_REPORT=1` prints what each measured,
/// for setting a budget after a deliberate change.
#[test]
fn memory_and_allocations_stay_within_budget() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let report = std::env::var_os("GEOSETTA_BUDGET_REPORT").is_some();
    let wide = wide_geojson(WIDE_ROWS, WIDE_COLS);
    let mut over = Vec::new();
    for b in cases() {
        let geojson = if b.wide { wide.clone() } else { narrow_for(b.from, b.to, NARROW) };
        let input = source_as(b.from, &geojson);
        let cost = measure(b.from, b.to, &input);
        if report {
            eprintln!("{:?} -> {:?}{}: {cost:?}", b.from, b.to, if b.wide { " (wide)" } else { "" });
        }
        if cost.peak > b.peak || cost.allocs > b.allocs {
            over.push(format!(
                "{:?} -> {:?}{}: {} B and {} allocations a feature, against a budget of {} B and {}",
                b.from,
                b.to,
                if b.wide { " (wide)" } else { "" },
                cost.peak,
                cost.allocs,
                b.peak,
                b.allocs,
            ));
        }
    }
    assert!(over.is_empty(), "over budget (plans/performance.org § BUDGETS):\n{}", over.join("\n"));
}

/// The best of three runs of `f`, which keeps a stray pause from deciding.
fn best_of_3(mut f: impl FnMut()) -> Duration {
    (0..3)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

/// How many times longer the large input took than the small one. Linear work
/// is ~4; a quadratic pass is ~16.
fn growth(from: Format, to: Format, small: &Input, large: &Input) -> f64 {
    let small = best_of_3(|| {
        convert(from, to, small);
    });
    let large = best_of_3(|| {
        convert(from, to, large);
    });
    large.as_secs_f64() / small.as_secs_f64().max(1e-9)
}

/// The ceiling on that ratio: room for cache effects and noise above 4, well
/// below a quadratic's 16.
const MAX_GROWTH: f64 = 8.0;

/// Every format, read and written, is linear in its rows; and the writers
/// that build columns are linear in the column count.
#[test]
fn conversions_scale_linearly() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut slow = Vec::new();

    for format in Format::ALL {
        let read = |n| source_as(format, &narrow_for(format, Format::GeoJson, n));
        let ratio = growth(format, Format::GeoJson, &read(2_000), &read(8_000));
        if ratio > MAX_GROWTH {
            slow.push(format!("reading {format:?}: 4x the rows took {ratio:.1}x as long"));
        }
        let write = |n| Input::Bytes(narrow_for(Format::GeoJson, format, n));
        let ratio = growth(Format::GeoJson, format, &write(2_000), &write(8_000));
        if ratio > MAX_GROWTH {
            slow.push(format!("writing {format:?}: 4x the rows took {ratio:.1}x as long"));
        }
    }

    // Columns, at a fixed row count: `performance.org` items 1 and 11 were
    // O(columns²) on the write side.
    let (narrow, wide) = (Input::Bytes(wide_geojson(300, 50)), Input::Bytes(wide_geojson(300, 200)));
    for to in [Format::Parquet, Format::FlatGeobuf, Format::Gpkg, Format::Csv, Format::Kml] {
        let ratio = growth(Format::GeoJson, to, &narrow, &wide);
        if ratio > MAX_GROWTH {
            slow.push(format!("writing {to:?}: 4x the columns took {ratio:.1}x as long"));
        }
    }
    assert!(slow.is_empty(), "super-linear (plans/performance.org § BUDGETS):\n{}", slow.join("\n"));
}
