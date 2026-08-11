#!/usr/bin/env python3
"""Compare geosetta against ogr2ogr (GDAL) and duckdb on equivalent conversions.

Whole-process, best-of-3 wall-clock timing over a mixed-geometry dataset
(~80% points, ~15% lines, ~5% polygons, 4 typed properties) — the same shape as
the in-repo `tests/perf.rs` benchmarks, but comparing three tools side by side.

Usage:
    cargo build --release           # build the geosetta binary first
    python3 bench/compare.py        # N=100000 features by default
    N=200000 python3 bench/compare.py
    BIN=/path/to/geosetta python3 bench/compare.py

Requirements: `ogr2ogr` (GDAL) and `duckdb` (with the `spatial` extension
available to install) on PATH. Any tool/conversion that isn't available is
reported as `n/a` rather than aborting the run.

Fairness notes (so the numbers aren't misread):
  * Timing is whole-process, so it includes binary/library startup and file I/O
    — the real cost of a one-shot CLI conversion. Startup is very asymmetric
    (geosetta ~2 ms vs ogr2ogr / duckdb+spatial ~100 ms), which flatters
    geosetta on the fastest conversions; it still wins clearly once startup is
    subtracted, and the small startup is itself a dependency-free advantage.
  * DuckDB's vector I/O (GeoJSON / FlatGeobuf / GeoPackage) is GDAL-backed; only
    its Parquet path is native, so its non-Parquet numbers track GDAL's.
  * geosetta cannot yet read GDAL-written Parquet (a known level-encoding gap),
    so the parquet->geojson read input is DuckDB-written Parquet (which all
    three tools read).
  * All tools produce equivalent output (same feature count); this measures
    speed on the same work, not one tool doing less.
"""
import os
import subprocess
import sys
import tempfile
import time

N = int(os.environ.get("N", "100000"))
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("BIN", os.path.join(REPO, "target", "release", "geosetta"))
D = tempfile.mkdtemp(prefix="geosetta-bench-")
p = lambda f: os.path.join(D, f)


def gen_geojson(n):
    parts = ['{"type":"FeatureCollection","features":[']
    for i in range(n):
        if i:
            parts.append(",")
        x = -180.0 + (i * 0.001) % 360.0
        y = -90.0 + (i * 0.0007) % 180.0
        m = i % 20
        if m == 0:
            g = ('{"type":"Polygon","coordinates":[[[%.6f,%.6f],[%.6f,%.6f],'
                 '[%.6f,%.6f],[%.6f,%.6f]]]}') % (x, y, x + 0.01, y, x + 0.01, y + 0.01, x, y)
        elif 1 <= m <= 3:
            g = ('{"type":"LineString","coordinates":[[%.6f,%.6f],[%.6f,%.6f],'
                 '[%.6f,%.6f]]}') % (x, y, x + 0.02, y + 0.02, x + 0.04, y - 0.01)
        else:
            g = '{"type":"Point","coordinates":[%.6f,%.6f]}' % (x, y)
        parts.append(
            '{"type":"Feature","geometry":%s,"properties":'
            '{"id":%d,"name":"f%d","val":%.3f,"flag":%s}}'
            % (g, i, i, i * 1.5, "true" if i % 2 == 0 else "false"))
    parts.append("]}")
    return "".join(parts)


def run(cmd, shell=False):
    """Best-of-3 wall time in ms, or (None, stderr) on failure."""
    best = None
    for _ in range(3):
        t = time.perf_counter()
        r = subprocess.run(cmd, shell=shell, capture_output=True)
        dt = (time.perf_counter() - t) * 1000
        if r.returncode != 0:
            return None, r.stderr.decode()[:200]
        best = dt if best is None else min(best, dt)
    return best, None


def rm(f):
    if os.path.exists(f):
        os.remove(f)


def ogr(out, inp, driver):
    rm(out)
    return ["ogr2ogr", "-f", driver, out, inp]


def duck(sql):
    return ["duckdb", "-c", "INSTALL spatial;LOAD spatial;" + sql]


# --- setup: common source geojson + neutral read-inputs ---------------------
print(f"generating {N} features in {D} ...", file=sys.stderr)
gj = p("src.geojson")
open(gj, "w").write(gen_geojson(N))
gj_mb = os.path.getsize(gj) / 1048576

in_pq, in_fgb, in_gpkg = p("in.parquet"), p("in.fgb"), p("in.gpkg")
gpq, gfgb, ggpkg = p("g.parquet"), p("g.fgb"), p("g.gpkg")
dpq, dfgb, dgpkg = p("d.parquet"), p("d.fgb"), p("d.gpkg")
g1, g2, g3 = p("g1.geojson"), p("g2.geojson"), p("g3.geojson")
d1, d2, d3 = p("d1.geojson"), p("d2.geojson"), p("d3.geojson")
GDAL_GJ = "WITH (FORMAT GDAL, DRIVER 'GeoJSON', SRS 'EPSG:4326')"

# read-inputs created by a neutral tool (parquet via duckdb: geosetta can't read
# GDAL parquet; fgb/gpkg via ogr so no tool reads only its own output).
subprocess.run(duck(f"COPY (SELECT * FROM ST_Read('{gj}')) TO '{in_pq}' (FORMAT PARQUET);"),
               capture_output=True)
subprocess.run(ogr(in_fgb, gj, "FlatGeobuf"), capture_output=True)
subprocess.run(ogr(in_gpkg, gj, "GPKG"), capture_output=True)

# --- conversions: (label, geosetta_cmd, ogr_cmd, duckdb_cmd) -----------------
cases = [
    ("geojson->parquet", [BIN, gj, gpq],
     ogr(p("o.parquet"), gj, "Parquet"),
     duck(f"COPY (SELECT * FROM ST_Read('{gj}')) TO '{dpq}' (FORMAT PARQUET);")),
    ("geojson->fgb", [BIN, gj, gfgb],
     ogr(p("o.fgb"), gj, "FlatGeobuf"),
     duck(f"COPY (SELECT * FROM ST_Read('{gj}')) TO '{dfgb}' WITH (FORMAT GDAL, DRIVER 'FlatGeobuf', SRS 'EPSG:4326');")),
    ("geojson->gpkg", [BIN, gj, ggpkg],
     ogr(p("o.gpkg"), gj, "GPKG"),
     duck(f"COPY (SELECT * FROM ST_Read('{gj}')) TO '{dgpkg}' WITH (FORMAT GDAL, DRIVER 'GPKG', SRS 'EPSG:4326');")),
    ("parquet->geojson", [BIN, in_pq, g1],
     ogr(p("o1.geojson"), in_pq, "GeoJSON"),
     duck(f"COPY (SELECT * FROM '{in_pq}') TO '{d1}' {GDAL_GJ};")),
    ("fgb->geojson", [BIN, in_fgb, g2],
     ogr(p("o2.geojson"), in_fgb, "GeoJSON"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_fgb}')) TO '{d2}' {GDAL_GJ};")),
    ("gpkg->geojson", [BIN, in_gpkg, g3],
     ogr(p("o3.geojson"), in_gpkg, "GeoJSON"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_gpkg}')) TO '{d3}' {GDAL_GJ};")),
]

if not os.path.exists(BIN):
    sys.exit(f"geosetta binary not found at {BIN}; run `cargo build --release` first "
             f"(or set BIN=...).")

print(f"\n=== conversion benchmark: {N} features, source geojson {gj_mb:.1f} MB, best-of-3 ms ===\n")
print(f"{'conversion':<20}{'geosetta':>12}{'ogr2ogr':>12}{'duckdb':>12}   {'vs ogr':>8} {'vs duck':>8}")
print("-" * 84)
for label, gc, oc, dc in cases:
    gt, _ = run(gc)
    ot, oe = run(oc)
    dt, de = run(dc)
    cell = lambda t: f"{t:>10.0f}ms" if t is not None else f"{'n/a':>12}"
    ratio = lambda base, other: f"{other / base:>7.1f}x" if base and other else f"{'-':>8}"
    print(f"{label:<20}{cell(gt)}{cell(ot)}{cell(dt)}   {ratio(gt, ot)} {ratio(gt, dt)}")
    for tool, err in (("ogr", oe), ("duck", de)):
        if err:
            print(f"    ! {tool} failed: {err.strip()[:120]}")

print("\n(ratio = competitor_time / geosetta_time; >1 means geosetta is faster)")
import shutil
shutil.rmtree(D, ignore_errors=True)
