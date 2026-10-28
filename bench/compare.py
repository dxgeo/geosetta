#!/usr/bin/env python3
"""Compare geosetta against ogr2ogr (GDAL), duckdb and terrio on equivalent conversions.

Whole-process, best-of-3 wall-clock timing (and peak RSS, from the same runs)
over a mixed-geometry dataset
(~80% points, ~15% lines, ~5% polygons, 4 typed properties), plus a
wide-column dataset (points with WIDE_COLS typed properties) that isolates
per-column schema-inference cost — the same shapes as the in-repo
`tests/perf.rs` benchmarks, but comparing four tools side by side.

Usage:
    cargo build --release           # build the geosetta binary first (the
                                    # workspace target dir is used by default)
    python3 bench/compare.py        # N=100000 features by default
    N=200000 python3 bench/compare.py
    BIN=/path/to/geosetta python3 bench/compare.py
    TERRIO=/path/to/terrio python3 bench/compare.py
    GEOSCRIBE=/path/to/geoscribe python3 bench/compare.py
    WIDE_N=50000 WIDE_COLS=200 python3 bench/compare.py  # wide-column cases

Requirements: `ogr2ogr` (GDAL), `duckdb` (with the `spatial` extension
available to install), `terrio` (`cargo install terrio`) and `geoscribe` on PATH. Any tool/conversion that isn't available is
reported as `n/a` rather than aborting the run.

Fairness notes (so the numbers aren't misread):
  * geosetta carries no CRS registry, so for the EPSG:3857 cases its source CRS
    (which it can only reference by code) is resolved the way its own warning
    says: `geoscribe EPSG:3857 --projjson` writes the definition once, untimed,
    and geosetta gets `--crs <file>`. The other tools resolve 3857 internally;
    that one-time lookup is the cost geosetta's design moves out of the binary.
  * Timing is whole-process, so it includes binary/library startup and file I/O
    — the real cost of a one-shot CLI conversion. Startup is very asymmetric
    (geosetta ~2 ms, terrio ~4 ms, ogr2ogr ~80 ms, duckdb+spatial ~65 ms),
    which flatters geosetta and terrio on the fastest conversions; geosetta
    still wins clearly once startup is subtracted, and the small startup is
    itself a dependency-free advantage.
  * DuckDB's vector I/O (GeoJSON / FlatGeobuf / GeoPackage) is GDAL-backed; only
    its Parquet path is native, so its non-Parquet numbers track GDAL's.
  * geosetta cannot yet read GDAL-written Parquet (a known level-encoding gap),
    so the parquet->geojson read input is DuckDB-written Parquet (which all
    three tools read).
  * All tools produce equivalent output; this measures speed on the same work,
    not one tool doing less. After timing, every output is read back through
    ogr2ogr and checked against the case's input: feature count, geometry type
    and coordinates (abs tol COORD_TOL), and every input property's value
    (numbers by value, so CSV strings compare equal). Extra output columns
    (an FID, a WKT column) are ignored; a missing or wrong one fails. Set
    VERIFY=0 to skip, which also skips the ogr2ogr read-back's cost on big N.
"""
import json
import math
import os
import re
import subprocess
import sys
import tempfile
import time

N = int(os.environ.get("N", "100000"))
WIDE_N = int(os.environ.get("WIDE_N", "50000"))
WIDE_COLS = int(os.environ.get("WIDE_COLS", "200"))
REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def default_bin():
    """The release binary cargo builds for this crate. geosetta is a workspace
    member, so it lands in the workspace's target dir, not REPO/target: ask
    cargo rather than guess (a stale REPO/target binary can linger)."""
    r = subprocess.run(["cargo", "metadata", "--format-version", "1", "--no-deps"],
                       cwd=REPO, capture_output=True)
    target = json.loads(r.stdout)["target_directory"] if r.returncode == 0 else os.path.join(REPO, "target")
    return os.path.join(target, "release", "geosetta")


BIN = os.environ.get("BIN") or default_bin()
GEOSCRIBE = os.environ.get("GEOSCRIBE", "geoscribe")
TERRIO = os.environ.get("TERRIO", "terrio")
VERIFY = os.environ.get("VERIFY", "1") != "0"
COORD_TOL = float(os.environ.get("COORD_TOL", "1e-6"))
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
    """Best-of-3 wall time in ms and lowest peak RSS in MB, as (ms, err, mb);
    (None, stderr, None) on failure. Peak RSS is the child's own ru_maxrss from
    wait4, so it is per-run, not the running max RUSAGE_CHILDREN would give."""
    best = rss = None
    for _ in range(3):
        with tempfile.TemporaryFile() as err:
            t = time.perf_counter()
            proc = subprocess.Popen(cmd, shell=shell, stdout=subprocess.DEVNULL, stderr=err)
            _, status, ru = os.wait4(proc.pid, 0)
            dt = (time.perf_counter() - t) * 1000
            proc.returncode = os.waitstatus_to_exitcode(status)
            if proc.returncode != 0:
                err.seek(0)
                return None, err.read().decode()[:200], None
        # ru_maxrss is bytes on macOS, kilobytes on Linux
        mb = ru.ru_maxrss / (1048576 if sys.platform == "darwin" else 1024)
        best = dt if best is None else min(best, dt)
        rss = mb if rss is None else min(rss, mb)
    return best, None, rss


def rm(f):
    if os.path.exists(f):
        os.remove(f)


def ogr(out, inp, driver, *extra):
    rm(out)
    return ["ogr2ogr", "-f", driver, out, inp, *extra]


def ogr_reproject(out, inp, driver, s_srs, t_srs):
    rm(out)
    return ["ogr2ogr", "-f", driver, "-s_srs", s_srs, "-t_srs", t_srs, out, inp]


def terrio(inp, out):
    """`terrio convert` of the same input to a terrio-private output path, so
    it never races or overwrites geosetta's. Output format comes from the
    extension; CSV needs WKT geometry to match the other tools' output."""
    out = os.path.join(os.path.dirname(out), "t_" + os.path.basename(out))
    rm(out)
    extra = ["--lco", "GEOMETRY=AS_WKT"] if out.endswith(".csv") else []
    return [TERRIO, "convert", inp, out, *extra]


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


def read_features(path):
    """Features of any dataset, as GeoJSON dicts in the file's own CRS."""
    oo = ["-oo", "GEOM_POSSIBLE_NAMES=geometry,WKT"] if path.endswith(".csv") else []
    r = subprocess.run(["ogr2ogr", "-f", "GeoJSONSeq", "-preserve_fid", "/vsistdout/", path, *oo],
                       capture_output=True)
    if r.returncode != 0:
        return None, r.stderr.decode()[:120]
    fts = [json.loads(l) for l in r.stdout.splitlines() if l.strip()]
    for ft in fts:
        # an integer `id` stored as a GeoPackage/GDAL FID reads back as the
        # feature's own id, not a property: it's the same value
        # (the wide fixture has no id column; its FIDs are just row numbers)
        if "id" not in ft["properties"] and "c0" not in ft["properties"] and ft.get("id") is not None:
            ft["properties"]["id"] = ft["id"]
    return fts, None


def num(v):
    """A property as a float if it is numeric, boolean or numeric text, else as
    text: tools disagree on whether true is a bool, 1, or "true"."""
    if isinstance(v, bool):
        return float(v)
    if isinstance(v, (int, float)):
        return float(v)
    if isinstance(v, str):
        if v.lower() in ("true", "false"):
            return float(v.lower() == "true")
        try:
            return float(v)
        except ValueError:
            return v
    return v


def same_value(a, b):
    a, b = num(a), num(b)
    if isinstance(a, float) and isinstance(b, float):
        return math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-9)
    return a == b


def flat(c):
    return [c] if isinstance(c, (int, float)) else [x for part in c for x in flat(part)]


def same_geometry(a, b):
    if not a or not b or a.get("type") != b.get("type"):
        return False
    fa, fb = flat(a["coordinates"]), flat(b["coordinates"])
    return len(fa) == len(fb) and all(abs(x - y) <= COORD_TOL for x, y in zip(fa, fb))


def verify(expected, got):
    """None if `got` matches `expected`, else a short reason."""
    if got is None:
        return "unreadable"
    if len(got) != len(expected):
        return f"count {len(got)} != {len(expected)}"
    key = next(k for k in ("c0", "id") if k in expected[0]["properties"])
    by_key = {}
    for ft in got:
        by_key[num(ft["properties"].get(key))] = ft
    bad_geom = bad_prop = 0
    first = None
    for ex in expected:
        ft = by_key.get(num(ex["properties"][key]))
        if ft is None:
            return f"missing feature {key}={ex['properties'][key]}"
        if not same_geometry(ex["geometry"], ft["geometry"]):
            bad_geom += 1
            first = first or f"geometry of {key}={ex['properties'][key]}"
        for k, v in ex["properties"].items():
            if not same_value(v, ft["properties"].get(k)):
                bad_prop += 1
                first = first or f"{k} of {key}={ex['properties'][key]}: {ft['properties'].get(k)!r} != {v!r}"
    if bad_geom or bad_prop:
        return f"{bad_geom} geometries, {bad_prop} values differ (first: {first})"
    return None


def newest_source_mtime():
    newest = os.path.getmtime(os.path.join(REPO, "Cargo.toml"))
    for root, _, files in os.walk(os.path.join(REPO, "src")):
        for name in files:
            newest = max(newest, os.path.getmtime(os.path.join(root, name)))
    return newest


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

# EPSG:3857's definition, resolved once by geoscribe (see the fairness notes).
crs_3857 = p("epsg3857.projjson")
r = subprocess.run([GEOSCRIBE, "EPSG:3857", "--projjson"], capture_output=True)
if r.returncode != 0:
    sys.exit(f"{GEOSCRIBE} EPSG:3857 failed: {r.stderr.decode()[:200]}")
open(crs_3857, "wb").write(r.stdout)

# --- conversions: (label, geosetta_cmd, ogr_cmd, duckdb_cmd); terrio's is derived -----------------
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
     ogr(p("o4.geojson"), in_csv, "GeoJSON", "-oo", "GEOM_POSSIBLE_NAMES=geometry"),
     duck(f"COPY (SELECT * EXCLUDE(geometry), ST_GeomFromText(geometry) AS geom FROM read_csv('{in_csv}')) "
          f"TO '{d4}' {GDAL_GJ};")),
    ("geojson->shp", [BIN, gjp, gshp],
     ogr(p("o.shp"), gjp, "ESRI Shapefile"),
     duck(f"COPY (SELECT * FROM ST_Read('{gjp}')) TO '{dshp}' "
          f"WITH (FORMAT GDAL, DRIVER 'ESRI Shapefile', SRS 'EPSG:4326');")),
    ("shp->geojson", [BIN, in_shp, g5],
     ogr(p("o5.geojson"), in_shp, "GeoJSON"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_shp}')) TO '{d5}' {GDAL_GJ};")),
    ("gpkg(3857)->parquet", [BIN, in_gpkg_3857, p("g_gpkg3857.parquet"), "--crs", crs_3857],
     ogr(p("o_gpkg3857.parquet"), in_gpkg_3857, "Parquet"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_gpkg_3857}')) TO '{p('d_gpkg3857.parquet')}' (FORMAT PARQUET);")),
    ("shp(3857)->parquet", [BIN, in_shp_3857, p("g_shp3857.parquet"), "--crs", crs_3857],
     ogr(p("o_shp3857.parquet"), in_shp_3857, "Parquet"),
     duck(f"COPY (SELECT * FROM ST_Read('{in_shp_3857}')) TO '{p('d_shp3857.parquet')}' (FORMAT PARQUET);")),
    ("fgb(3857)->gpkg", [BIN, in_fgb_3857, p("g_fgb3857.gpkg"), "--crs", crs_3857],
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
if os.path.getmtime(BIN) < newest_source_mtime():
    print(f"warning: {BIN} is older than the source; run `cargo build --release`.",
          file=sys.stderr)
print(f"geosetta binary: {BIN}", file=sys.stderr)

print(f"\n=== conversion benchmark: {N} features (source geojson {gj_mb:.1f} MB), "
      f"{WIDE_N} x {WIDE_COLS} cols for the wide cases, best-of-3 ms ===\n")
label_w = max(len(label) for label, *_ in cases) + 2
print(f"{'conversion':<{label_w}}{'geosetta':>12}{'ogr2ogr':>12}{'duckdb':>12}{'terrio':>12}"
      f"   {'vs ogr':>8} {'vs duck':>8} {'vs terrio':>9}")
print("-" * (label_w + 12 * 4 + 3 + 8 + 1 + 8 + 1 + 9))
outs = {}
mems = []
for label, gc, oc, dc in cases:
    gt, _, gm = run(gc)
    ot, oe, om = run(oc)
    dt, de, dm = run(dc)
    tc = terrio(gc[1], gc[2])
    tt, te, tm = run(tc)
    mems.append((label, gm, om, dm, tm))
    cell = lambda t: f"{t:>10.0f}ms" if t is not None else f"{'n/a':>12}"
    ratio = lambda base, other: f"{other / base:>7.1f}x" if base and other else f"{'-':>8}"
    print(f"{label:<{label_w}}{cell(gt)}{cell(ot)}{cell(dt)}{cell(tt)}   {ratio(gt, ot)} {ratio(gt, dt)} {ratio(gt, tt):>9}")
    for tool, err in (("ogr", oe), ("duck", de), ("terrio", te)):
        if err:
            print(f"    ! {tool} failed: {err.strip()[:120]}")
    outs[label] = (gc[1], {"geosetta": gc[2], "ogr2ogr": oc[3],
                           "duckdb": re.search(r"TO '([^']+)'", dc[2]).group(1),
                           "terrio": tc[3]})

print("\n(ratio = competitor_time / geosetta_time; >1 means geosetta is faster)")

print(f"\n=== peak memory (max RSS of the whole process, lowest of 3 runs, MB) ===\n")
print(f"{'conversion':<{label_w}}{'geosetta':>12}{'ogr2ogr':>12}{'duckdb':>12}{'terrio':>12}"
      f"   {'vs ogr':>8} {'vs duck':>8} {'vs terrio':>9}")
print("-" * (label_w + 12 * 4 + 3 + 8 + 1 + 8 + 1 + 9))
for label, gm, om, dm, tm in mems:
    cell = lambda m: f"{m:>10.1f}MB" if m is not None else f"{'n/a':>12}"
    ratio = lambda base, other: f"{other / base:>7.1f}x" if base and other else f"{'-':>8}"
    print(f"{label:<{label_w}}{cell(gm)}{cell(om)}{cell(dm)}{cell(tm)}   {ratio(gm, om)} {ratio(gm, dm)} {ratio(gm, tm):>9}")
print("\n(ratio = competitor_rss / geosetta_rss; >1 means geosetta uses less memory)")

if VERIFY:
    print("\n=== output check: count, geometry and every input property vs the input ===\n")
    tools = ("geosetta", "ogr2ogr", "duckdb", "terrio")
    print(f"{'conversion':<{label_w}}" + "".join(f"{t:>12}" for t in tools))
    print("-" * (label_w + 12 * len(tools)))
    failures = []
    expected = {}
    for label, (src, paths) in outs.items():
        # a CSV input has no CRS or typed columns to read back reliably; the
        # source geojson holds the same features
        src = gj if src.endswith(".csv") else src
        if src not in expected:
            expected[src] = read_features(src)[0]
        row = []
        for tool in tools:
            path = paths[tool]
            got = read_features(path)[0] if os.path.exists(path) else None
            why = verify(expected[src], got)
            row.append("ok" if why is None else "FAIL")
            if why:
                failures.append(f"{label} / {tool}: {why}")
        print(f"{label:<{label_w}}" + "".join(f"{c:>12}" for c in row))
    for fl in failures:
        print(f"    ! {fl}")
import shutil
shutil.rmtree(D, ignore_errors=True)
