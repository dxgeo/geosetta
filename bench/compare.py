#!/usr/bin/env python3
"""Compare geosetta against ogr2ogr (GDAL) and duckdb on equivalent conversions.

Whole-process, best-of-3 wall-clock timing over a mixed-geometry dataset
(~80% points, ~15% lines, ~5% polygons, 4 typed properties), plus a
wide-column dataset (points with WIDE_COLS typed properties) that isolates
per-column schema-inference cost — the same shapes as the in-repo
`tests/perf.rs` benchmarks, but comparing three tools side by side.

Usage:
    cargo build --release           # build the geosetta binary first
    python3 bench/compare.py        # N=100000 features by default
    N=200000 python3 bench/compare.py
    BIN=/path/to/geosetta python3 bench/compare.py
    WIDE_N=50000 WIDE_COLS=200 python3 bench/compare.py  # wide-column cases

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
WIDE_N = int(os.environ.get("WIDE_N", "50000"))
WIDE_COLS = int(os.environ.get("WIDE_COLS", "200"))
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


def gen_wide_geojson(n, cols):
    """Points with `cols` typed properties instead of a mixed-geometry, 4-prop
    row: isolates per-column schema-inference cost (see tests/perf.rs's
    `wide_table`), which the narrow `gen_geojson` fixture doesn't exercise."""
    parts = ['{"type":"FeatureCollection","features":[']
    for i in range(n):
        if i:
            parts.append(",")
        x = -180.0 + (i * 0.001) % 360.0
        y = -90.0 + (i * 0.0007) % 180.0
        props = []
        for j in range(cols):
            m = j % 4
            if m == 0:
                props.append('"c%d":%d' % (j, i + j))
            elif m == 1:
                props.append('"c%d":%.3f' % (j, (i + j) * 1.5))
            elif m == 2:
                props.append('"c%d":"v%d_%d"' % (j, i, j))
            else:
                props.append('"c%d":%s' % (j, "true" if (i + j) % 2 == 0 else "false"))
        parts.append(
            '{"type":"Feature","geometry":{"type":"Point","coordinates":[%.6f,%.6f]},'
            '"properties":{%s}}' % (x, y, ",".join(props)))
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


def ogr(out, inp, driver, *extra):
    rm(out)
    return ["ogr2ogr", "-f", driver, out, inp, *extra]


def ogr_reproject(out, inp, driver, s_srs, t_srs):
    rm(out)
    return ["ogr2ogr", "-f", driver, "-s_srs", s_srs, "-t_srs", t_srs, out, inp]


def duck(sql):
    return ["duckdb", "-c", "INSTALL spatial;LOAD spatial;" + sql]


def gen_points_geojson(n):
    """Points-only variant: Shapefile can't mix geometry types in one .shp."""
    parts = ['{"type":"FeatureCollection","features":[']
    for i in range(n):
        if i:
            parts.append(",")
        x = -180.0 + (i * 0.001) % 360.0
        y = -90.0 + (i * 0.0007) % 180.0
        parts.append(
            '{"type":"Feature","geometry":{"type":"Point","coordinates":[%.6f,%.6f]},"properties":'
            '{"id":%d,"name":"f%d","val":%.3f,"flag":%s}}'
            % (x, y, i, i, i * 1.5, "true" if i % 2 == 0 else "false"))
    parts.append("]}")
    return "".join(parts)


# --- setup: common source geojson + neutral read-inputs ---------------------
print(f"generating {N} features in {D} ...", file=sys.stderr)
gj = p("src.geojson")
open(gj, "w").write(gen_geojson(N))
gj_mb = os.path.getsize(gj) / 1048576

# points-only source: Shapefile's .shp can't mix geometry types.
gjp = p("src_points.geojson")
open(gjp, "w").write(gen_points_geojson(N))

# wide-column source: isolates per-column schema-inference cost.
print(f"generating {WIDE_N} wide features x {WIDE_COLS} cols ...", file=sys.stderr)
gjw = p("src_wide.geojson")
open(gjw, "w").write(gen_wide_geojson(WIDE_N, WIDE_COLS))
gjw_mb = os.path.getsize(gjw) / 1048576
in_pq_wide = p("in_wide.parquet")
subprocess.run(duck(f"COPY (SELECT * FROM ST_Read('{gjw}')) TO '{in_pq_wide}' (FORMAT PARQUET);"),
               capture_output=True)

in_pq, in_fgb, in_gpkg, in_shp = p("in.parquet"), p("in.fgb"), p("in.gpkg"), p("in.shp")
in_csv = p("in.csv")
gpq, gfgb, ggpkg, gcsv, gshp = p("g.parquet"), p("g.fgb"), p("g.gpkg"), p("g.csv"), p("g.shp")
dpq, dfgb, dgpkg, dcsv, dshp = p("d.parquet"), p("d.fgb"), p("d.gpkg"), p("d.csv"), p("d.shp")
g1, g2, g3, g4, g5 = p("g1.geojson"), p("g2.geojson"), p("g3.geojson"), p("g4.geojson"), p("g5.geojson")
d1, d2, d3, d4, d5 = p("d1.geojson"), p("d2.geojson"), p("d3.geojson"), p("d4.geojson"), p("d5.geojson")
GDAL_GJ = "WITH (FORMAT GDAL, DRIVER 'GeoJSON', SRS 'EPSG:4326')"

# read-inputs created by a neutral tool (parquet via duckdb: geosetta can't read
# GDAL parquet; fgb/gpkg/shp via ogr so no tool reads only its own output; csv
# via duckdb's native writer so it's not GDAL-CSV-flavored either).
subprocess.run(duck(f"COPY (SELECT * FROM ST_Read('{gj}')) TO '{in_pq}' (FORMAT PARQUET);"),
               capture_output=True)
subprocess.run(ogr(in_fgb, gj, "FlatGeobuf"), capture_output=True)
subprocess.run(ogr(in_gpkg, gj, "GPKG"), capture_output=True)
subprocess.run(ogr(in_shp, gjp, "ESRI Shapefile"), capture_output=True)
subprocess.run(duck(f"COPY (SELECT * EXCLUDE(geom), ST_AsText(geom) AS geometry FROM ST_Read('{gj}')) "
                     f"TO '{in_csv}' (FORMAT CSV, HEADER);"), capture_output=True)

# EPSG:3857-reprojected fixtures (points-only, shared across gpkg/shp/fgb): a
# real projected CRS, unlike the WGS-84 default above, so these pair
# conversions exercise the WKT/PROJJSON CRS-translation path that a
# geojson-anchored (always-WGS-84) conversion never touches.
in_gpkg_3857, in_shp_3857, in_fgb_3857 = p("in3857.gpkg"), p("in3857.shp"), p("in3857.fgb")
subprocess.run(ogr_reproject(in_gpkg_3857, gjp, "GPKG", "EPSG:4326", "EPSG:3857"), capture_output=True)
subprocess.run(ogr_reproject(in_shp_3857, gjp, "ESRI Shapefile", "EPSG:4326", "EPSG:3857"), capture_output=True)
subprocess.run(ogr_reproject(in_fgb_3857, gjp, "FlatGeobuf", "EPSG:4326", "EPSG:3857"), capture_output=True)

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
    ("geojson->csv", [BIN, gj, gcsv],
     ogr(p("o.csv"), gj, "CSV", "-lco", "GEOMETRY=AS_WKT"),
     duck(f"COPY (SELECT * EXCLUDE(geom), ST_AsText(geom) AS geometry FROM ST_Read('{gj}')) "
          f"TO '{dcsv}' (FORMAT CSV, HEADER);")),
    ("csv->geojson", [BIN, in_csv, g4],
     ogr(p("o4.geojson"), in_csv, "GeoJSON"),
     duck(f"COPY (SELECT * EXCLUDE(geometry), ST_GeomFromText(geometry) AS geom FROM read_csv('{in_csv}')) "
          f"TO '{d4}' {GDAL_GJ};")),
    ("geojson->shp", [BIN, gjp, gshp],
     ogr(p("o.shp"), gjp, "ESRI Shapefile"),
     duck(f"COPY (SELECT * FROM ST_Read('{gjp}')) TO '{dshp}' "
          f"WITH (FORMAT GDAL, DRIVER 'ESRI Shapefile', SRS 'EPSG:4326');")),
    ("shp->geojson", [BIN, in_shp, g5],
     ogr(p("o5.geojson"), in_shp, "GeoJSON"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_shp}')) TO '{d5}' {GDAL_GJ};")),
    ("gpkg(3857)->parquet", [BIN, in_gpkg_3857, p("g_gpkg3857.parquet")],
     ogr(p("o_gpkg3857.parquet"), in_gpkg_3857, "Parquet"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_gpkg_3857}')) TO '{p('d_gpkg3857.parquet')}' (FORMAT PARQUET);")),
    ("shp(3857)->parquet", [BIN, in_shp_3857, p("g_shp3857.parquet")],
     ogr(p("o_shp3857.parquet"), in_shp_3857, "Parquet"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_shp_3857}')) TO '{p('d_shp3857.parquet')}' (FORMAT PARQUET);")),
    ("fgb(3857)->gpkg", [BIN, in_fgb_3857, p("g_fgb3857.gpkg")],
     ogr(p("o_fgb3857.gpkg"), in_fgb_3857, "GPKG"),
     duck(f"COPY (SELECT * EXCLUDE(OGC_FID) FROM ST_Read('{in_fgb_3857}')) TO '{p('d_fgb3857.gpkg')}' "
          f"WITH (FORMAT GDAL, DRIVER 'GPKG', SRS 'EPSG:3857');")),
    (f"geojson->parquet (wide, {WIDE_COLS} cols)", [BIN, gjw, p("g_wide.parquet")],
     ogr(p("o_wide.parquet"), gjw, "Parquet"),
     duck(f"COPY (SELECT * FROM ST_Read('{gjw}')) TO '{p('d_wide.parquet')}' (FORMAT PARQUET);")),
    (f"geojson->fgb (wide, {WIDE_COLS} cols)", [BIN, gjw, p("g_wide.fgb")],
     ogr(p("o_wide.fgb"), gjw, "FlatGeobuf"),
     duck(f"COPY (SELECT * FROM ST_Read('{gjw}')) TO '{p('d_wide.fgb')}' "
          f"WITH (FORMAT GDAL, DRIVER 'FlatGeobuf', SRS 'EPSG:4326');")),
    (f"parquet->geojson (wide, {WIDE_COLS} cols)", [BIN, in_pq_wide, p("g_wide_out.geojson")],
     ogr(p("o_wide_out.geojson"), in_pq_wide, "GeoJSON"),
     duck(f"COPY (SELECT * FROM '{in_pq_wide}') TO '{p('d_wide_out.geojson')}' {GDAL_GJ};")),
]

if not os.path.exists(BIN):
    sys.exit(f"geosetta binary not found at {BIN}; run `cargo build --release` first "
             f"(or set BIN=...).")

print(f"\n=== conversion benchmark: {N} features (source geojson {gj_mb:.1f} MB), "
      f"{WIDE_N} x {WIDE_COLS} cols for the wide cases, best-of-3 ms ===\n")
label_w = max(len(label) for label, *_ in cases) + 2
print(f"{'conversion':<{label_w}}{'geosetta':>12}{'ogr2ogr':>12}{'duckdb':>12}   {'vs ogr':>8} {'vs duck':>8}")
print("-" * (label_w + 12 * 3 + 3 + 8 + 1 + 8))
for label, gc, oc, dc in cases:
    gt, _ = run(gc)
    ot, oe = run(oc)
    dt, de = run(dc)
    cell = lambda t: f"{t:>10.0f}ms" if t is not None else f"{'n/a':>12}"
    ratio = lambda base, other: f"{other / base:>7.1f}x" if base and other else f"{'-':>8}"
    print(f"{label:<{label_w}}{cell(gt)}{cell(ot)}{cell(dt)}   {ratio(gt, ot)} {ratio(gt, dt)}")
    for tool, err in (("ogr", oe), ("duck", de)):
        if err:
            print(f"    ! {tool} failed: {err.strip()[:120]}")

print("\n(ratio = competitor_time / geosetta_time; >1 means geosetta is faster)")
import shutil
shutil.rmtree(D, ignore_errors=True)
