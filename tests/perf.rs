//! Throughput benchmarks for the read/write paths.
//!
//! These drive the real `geosetta` binary end-to-end (whole-pipeline timing,
//! including process startup and file I/O, not just the library functions). They
//! are marked
//! `#[ignore]` so a normal `cargo test` stays fast; run them with:
//!
//! ```sh
//! cargo test --release --test perf -- --ignored --nocapture
//! ```
//!
//! Set `GEOSETTA_BENCH_N` to change the feature count (default 200_000). Each
//! benchmark prints wall-clock, input MB/s, and features/s (best of a few
//! runs); they assert only correctness, not timing, so they are not flaky.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const GEOSETTA: &str = env!("CARGO_BIN_EXE_geosetta");

fn feature_count() -> usize {
    std::env::var("GEOSETTA_BENCH_N").ok().and_then(|s| s.parse().ok()).unwrap_or(200_000)
}

/// Property count for the wide-table benchmark (default 30).
fn wide_cols() -> usize {
    std::env::var("GEOSETTA_BENCH_COLS").ok().and_then(|s| s.parse().ok()).unwrap_or(30)
}

/// A private scratch directory for the run's fixtures.
fn scratch() -> PathBuf {
    scratch_named("main")
}

/// A private scratch directory tagged with `name`, so benchmarks that run in
/// parallel don't share (and delete) each other's fixtures.
fn scratch_named(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("geosetta-perf-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Generate a GeoJSON FeatureCollection with `n` features: a mix of point,
/// linestring, and polygon geometries with a few typed properties, and
/// fractional coordinates that exercise the number formatter.
fn gen_geojson(n: usize) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(n * 160);
    s.push_str("{\"type\":\"FeatureCollection\",\"features\":[");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let x = -180.0 + (i as f64 * 0.001) % 360.0;
        let y = -90.0 + (i as f64 * 0.0007) % 180.0;
        let geom = match i % 20 {
            // ~5% polygons, ~15% linestrings, ~80% points.
            0 => format!(
                "{{\"type\":\"Polygon\",\"coordinates\":[[[{x:.6},{y:.6}],[{:.6},{y:.6}],[{:.6},{:.6}],[{x:.6},{y:.6}]]]}}",
                x + 0.01, x + 0.01, y + 0.01
            ),
            1..=3 => format!(
                "{{\"type\":\"LineString\",\"coordinates\":[[{x:.6},{y:.6}],[{:.6},{:.6}],[{:.6},{:.6}]]}}",
                x + 0.02, y + 0.02, x + 0.04, y - 0.01
            ),
            _ => format!("{{\"type\":\"Point\",\"coordinates\":[{x:.6},{y:.6}]}}"),
        };
        let _ = write!(
            s,
            "{{\"type\":\"Feature\",\"geometry\":{geom},\"properties\":{{\"id\":{i},\"name\":\"f{i}\",\"val\":{:.3},\"flag\":{}}}}}",
            (i as f64) * 1.5,
            i % 2 == 0
        );
    }
    s.push_str("]}");
    s
}

/// Generate a FeatureCollection of `n` points, each carrying `cols` typed
/// properties (int / float / string / bool, cycling by column). Geometry is a
/// trivial point, so property handling dominates: this isolates the cost of
/// `schema::infer_columns` (which scales with the *square* of the column count)
/// and the per-row property key/value (de)construction — neither of which the
/// narrow four-property `gen_geojson` exercises.
fn gen_wide_geojson(n: usize, cols: usize) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(n * (48 + cols * 16));
    s.push_str("{\"type\":\"FeatureCollection\",\"features\":[");
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        let x = -180.0 + (i as f64 * 0.001) % 360.0;
        let y = -90.0 + (i as f64 * 0.0007) % 180.0;
        let _ = write!(
            s,
            "{{\"type\":\"Feature\",\"geometry\":{{\"type\":\"Point\",\"coordinates\":[{x:.6},{y:.6}]}},\"properties\":{{"
        );
        for j in 0..cols {
            if j > 0 {
                s.push(',');
            }
            match j % 4 {
                0 => { let _ = write!(s, "\"c{j}\":{}", i as i64 + j as i64); }
                1 => { let _ = write!(s, "\"c{j}\":{:.3}", (i + j) as f64 * 1.5); }
                2 => { let _ = write!(s, "\"c{j}\":\"v{i}_{j}\""); }
                _ => { let _ = write!(s, "\"c{j}\":{}", (i + j) % 2 == 0); }
            }
        }
        s.push_str("}}");
    }
    s.push_str("]}");
    s
}

/// Run `geosetta in out [extra…]`, returning elapsed time and output size.
fn run(input: &Path, output: &Path, extra: &[&str]) -> (Duration, u64) {
    let start = Instant::now();
    let out = Command::new(GEOSETTA)
        .arg(input)
        .arg(output)
        .args(extra)
        .output()
        .expect("spawn geosetta");
    let elapsed = start.elapsed();
    assert!(
        out.status.success(),
        "geosetta {input:?} -> {output:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    (elapsed, size)
}

/// Best-of-3 timing of one conversion, with a throughput line printed.
fn bench(label: &str, input: &Path, in_bytes: u64, n: usize, output: &Path, extra: &[&str]) -> u64 {
    let mut best = Duration::MAX;
    let mut size = 0;
    for _ in 0..3 {
        let (dt, s) = run(input, output, extra);
        best = best.min(dt);
        size = s;
    }
    let secs = best.as_secs_f64();
    let mb = in_bytes as f64 / 1_048_576.0;
    println!(
        "{label:<22} {:>7.1} ms   {:>7.1} MB/s in   {:>8.0} feat/s   (out {} bytes)",
        secs * 1000.0,
        mb / secs,
        n as f64 / secs,
        size
    );
    size
}

/// Build the shared source fixtures (geojson + the binary formats derived from
/// it) once, returning the scratch dir and the geojson byte size.
fn setup(n: usize) -> (PathBuf, u64) {
    let dir = scratch();
    let geojson = dir.join("bench.geojson");
    let text = gen_geojson(n);
    std::fs::write(&geojson, &text).unwrap();
    let bytes = text.len() as u64;

    // Derive the binary inputs used by the read benchmarks.
    run(&geojson, &dir.join("bench.parquet"), &[]);
    run(&geojson, &dir.join("bench.fgb"), &[]);
    run(&geojson, &dir.join("bench.csv"), &[]);
    (dir, bytes)
}

#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn throughput() {
    let n = feature_count();
    let (dir, geo_bytes) = setup(n);
    println!("\n=== geosetta throughput: {n} features, geojson {} bytes ===", geo_bytes);

    let g = |f: &str| dir.join(f);
    let sz = |p: PathBuf| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let pq = sz(g("bench.parquet"));
    let fgb = sz(g("bench.fgb"));
    let csv = sz(g("bench.csv"));

    // Reads (input format is what we're stressing; output is cheap-ish).
    bench("geojson->wkt (read gj)", &g("bench.geojson"), geo_bytes, n, &g("out.wkt"), &[]);
    bench("parquet->geojson", &g("bench.parquet"), pq, n, &g("out1.geojson"), &[]);
    bench("fgb->geojson", &g("bench.fgb"), fgb, n, &g("out2.geojson"), &[]);
    bench("csv->geojson", &g("bench.csv"), csv, n, &g("out3.geojson"), &[]);

    // Writes (output format is what we're stressing).
    bench("geojson->parquet", &g("bench.geojson"), geo_bytes, n, &g("out.parquet"), &[]);
    bench("geojson->fgb", &g("bench.geojson"), geo_bytes, n, &g("out.fgb"), &[]);
    bench("geojson->gpkg", &g("bench.geojson"), geo_bytes, n, &g("out.gpkg"), &[]);
    bench("gpkg->geojson", &g("out.gpkg"), sz(g("out.gpkg")), n, &g("out4.geojson"), &[]);

    // Sanity: a full round trip preserves the feature count.
    let back = std::fs::read_to_string(g("out1.geojson")).unwrap();
    assert_eq!(back.matches("\"Feature\"").count(), n, "feature count preserved");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Wide-table benchmark: the same feature count, but many property columns, to
/// expose the per-column costs the narrow `throughput` run hides. On the write
/// side this stresses `schema::infer_columns` (currently O(rows × cols²)); on
/// the read side, the per-row rebuild of each feature's properties (one key
/// `String` allocation and one value clone per cell). Watch how the reported
/// feat/s degrades as `GEOSETTA_BENCH_COLS` grows — a linear-in-cols schema pass
/// should keep it roughly flat per cell.
#[test]
#[ignore = "benchmark; run with --release --ignored --nocapture"]
fn wide_table() {
    let n = feature_count();
    let cols = wide_cols();
    let dir = scratch_named("wide");
    let geojson = dir.join("wide.geojson");
    let text = gen_wide_geojson(n, cols);
    std::fs::write(&geojson, &text).unwrap();
    let geo_bytes = text.len() as u64;
    println!(
        "\n=== geosetta wide-table: {n} features x {cols} props, geojson {geo_bytes} bytes ===",
    );

    // Write path: schema inference + columnar materialization dominate.
    bench("geojson->parquet (wide)", &geojson, geo_bytes, n, &dir.join("wide.parquet"), &[]);
    bench("geojson->fgb (wide)", &geojson, geo_bytes, n, &dir.join("wide.fgb"), &[]);

    // Read path: per-row property reconstruction (key clone + value clone).
    let pq = std::fs::metadata(dir.join("wide.parquet")).map(|m| m.len()).unwrap_or(0);
    bench("parquet->geojson (wide)", &dir.join("wide.parquet"), pq, n, &dir.join("wide_out.geojson"), &[]);

    let back = std::fs::read_to_string(dir.join("wide_out.geojson")).unwrap();
    assert_eq!(back.matches("\"Feature\"").count(), n, "feature count preserved");

    let _ = std::fs::remove_dir_all(&dir);
}
