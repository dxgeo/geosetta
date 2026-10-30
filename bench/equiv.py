#!/usr/bin/env python3
"""The equivalence corpus: every input through a pinned geosetta and a candidate, compared byte for byte.

A change meant to alter how geosetta works, not what it writes -- each
milestone of plans/columnar.org (its C0), a performance item, a refactor --
must leave every conversion here byte-identical: the output files, stderr and
the exit status. This is the check Mem1--Mem5 and performance.org's items 8
and 9 ran by hand.

The corpus:
  * every fixture in tests/fixtures a reader takes;
  * generated GeoJSON, each shape a way a table can differ from a row:
      narrow    compare.py's mixed-geometry, four-property collection
      wide      compare.py's points with 200 typed properties
      sparse    2 000 distinct keys, five a feature, types mixed per key
                (1 000 features: from a typed tabular source every KML
                feature writes all 2 000 columns, so 20 000 would be a
                1.5 GB file a run)
      ragged    every guarantee at once: one row in four in its own key
                order, absent beside null, 1 beside 1.0, a duplicate key, an
                integer past 2^53, nested values, a string in an integer
                column, a late column
      ownorder  every feature's keys in an order of its own
      stretch   two differently-keyed exports concatenated
      overlap   the same two, overlapping in space, so --sort-hilbert
                interleaves them
  * each generated file converted by the pinned binary to GeoParquet,
    FlatGeobuf, GeoPackage, CSV and KML, so the tabular and KML readers see
    the same shapes.
Each input goes to GeoJSON, KML, KMZ, CSV, WKT, FlatGeobuf, GeoParquet,
GeoPackage and Shapefile, plain and with --sort-hilbert, and with each option
that changes that format's output: --compact (GeoJSON, KML, KMZ),
--kml-version 2.3 with and without --compact (KML, KMZ), --rtree and
--envelope (GeoPackage). A conversion that fails must fail the same way; one that runs past TIMEOUT
seconds (default 300) in either binary is reported as unchecked.

Usage:
    python3 bench/equiv.py                       # candidate: the working tree; pin: HEAD
    PIN=v0.27.0 python3 bench/equiv.py           # pin another revision
    PIN_BIN=/path/to/geosetta python3 bench/equiv.py
    BIN=/path/to/geosetta python3 bench/equiv.py  # a candidate binary, used as is
    WIDE=1 python3 bench/equiv.py                # the wide table at compare.py's 50 000 rows
    SCALE=0.25 python3 bench/equiv.py            # generated sizes (default 1)
    JOBS=8 KEEP=1 python3 bench/equiv.py         # parallelism; keep every run's files
    TIMEOUT=600 python3 bench/equiv.py           # seconds a conversion may take

The pinned binary is built once per revision from a clean `git archive`, with
the crate's own release profile, and cached under the system temp dir; the
candidate is built in place with `cargo build --release`. Runs that differ
are kept for inspection, and their paths printed; exit status is 1 if any do,
2 if none differ but some went unchecked.
"""
import ast
import concurrent.futures
import filecmp
import json
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
FIXTURES = os.path.join(REPO, "tests", "fixtures")
SCALE = float(os.environ.get("SCALE", "1"))
WIDE = os.environ.get("WIDE", "0") != "0"
JOBS = int(os.environ.get("JOBS", str(os.cpu_count() or 4)))
KEEP = os.environ.get("KEEP", "0") != "0"
TIMEOUT = float(os.environ.get("TIMEOUT", "300"))
READABLE = (".geojson", ".json", ".kml", ".kmz", ".csv", ".tsv", ".fgb", ".parquet",
            ".gpkg", ".shp", ".wkt", ".zip")
TARGETS = ["geojson", "kml", "kmz", "csv", "wkt", "fgb", "parquet", "gpkg", "shp"]
DERIVED = ["parquet", "fgb", "gpkg", "csv", "kml"]


def size(n):
    return max(1, int(n * SCALE))


def options(target):
    """The option sets that change `target`'s output, each run on its own."""
    runs = [[], ["--sort-hilbert"]]
    if target in ("geojson", "kml", "kmz"):
        runs.append(["--compact"])
    if target in ("kml", "kmz"):
        runs += [["--kml-version", "2.3"], ["--kml-version", "2.3", "--compact"]]
    if target == "gpkg":
        runs += [["--rtree"], ["--envelope"]]
    return runs


# ---- binaries ----------------------------------------------------------------

def sh(cmd, cwd=None, **kw):
    return subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, **kw)


def candidate_bin():
    """The working tree's release binary. geosetta is a workspace member, so it
    lands in the workspace's target dir, not REPO/target: ask cargo (as
    compare.py does) rather than guess, since a stale REPO/target binary can
    linger."""
    if os.environ.get("BIN"):
        return os.environ["BIN"]
    sh(["cargo", "build", "--release", "--bin", "geosetta"], cwd=REPO)
    meta = json.loads(sh(["cargo", "metadata", "--format-version", "1", "--no-deps"], cwd=REPO).stdout)
    return os.path.join(meta["target_directory"], "release", "geosetta")


def pinned_bin():
    """PIN's binary, built from a clean export with the crate's own profile and
    cached by commit."""
    if os.environ.get("PIN_BIN"):
        return os.environ["PIN_BIN"]
    rev = os.environ.get("PIN", "HEAD")
    sha = sh(["git", "rev-parse", "--verify", rev + "^{commit}"], cwd=REPO, text=True).stdout.strip()
    root = os.path.join(tempfile.gettempdir(), "geosetta-equiv", "pin-" + sha[:12])
    binary = os.path.join(root, "target", "release", "geosetta")
    if not os.path.exists(binary):
        src = os.path.join(root, "src-tree")
        shutil.rmtree(root, ignore_errors=True)
        os.makedirs(src)
        archive = sh(["git", "archive", sha], cwd=REPO).stdout
        subprocess.run(["tar", "-x", "-C", src], input=archive, check=True)
        # Standalone, so the crate's own [profile.release] applies.
        build = ["cargo", "build", "--release", "--bin", "geosetta", "--target-dir", os.path.join(root, "target")]
        if subprocess.run(build + ["--offline"], cwd=src, capture_output=True).returncode != 0:
            sh(build, cwd=src)
    return binary


# ---- the corpus --------------------------------------------------------------

def compare_generators():
    """compare.py's own generators, taken without running its benchmark: the
    script does its work at top level, so its two functions are lifted from
    its syntax tree."""
    path = os.path.join(REPO, "bench", "compare.py")
    tree = ast.parse(open(path).read(), path)
    ns = {}
    wanted = [n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name in ("gen_geojson", "gen_wide_geojson")]
    exec(compile(ast.Module(wanted, []), path, "exec"), ns)
    return ns["gen_geojson"], ns["gen_wide_geojson"]


def collection(features):
    return '{"type":"FeatureCollection","features":[' + ",".join(features) + "]}"


def feature(i, props):
    """A point feature; `props` is (key, JSON text) pairs, so a duplicate key and
    an exact number's spelling both survive."""
    x = -180.0 + (i * 0.001) % 360.0
    y = -90.0 + (i * 0.0007) % 180.0
    body = ",".join(f"{json.dumps(k)}:{v}" for k, v in props)
    return ('{"type":"Feature","geometry":{"type":"Point","coordinates":[%.6f,%.6f]},'
            '"properties":{%s}}' % (x, y, body))


def gen_sparse(n, rng):
    keys = [f"tag{k}" for k in range(2000)]
    return collection(feature(i, [(k, str(i) if j % 2 else json.dumps(f"v{i}"))
                                  for j, k in enumerate(rng.sample(keys, 5))]) for i in range(n))


def gen_ragged(n, rng):
    def row(i):
        p = [("a", str(i)), ("b", json.dumps(f"s{i}")), ("c", f"{i * 0.5}"), ("d", "true" if i % 2 else "false")]
        if i % 7 == 0:
            p[1] = ("b", "null")                       # null, not absent
        if i % 5 == 0:
            p.pop(2)                                   # absent
        if i % 3 == 0:
            p.append(("e", "1" if i % 2 else "1.0"))   # 1 beside 1.0
        if i % 11 == 0:
            p.append(("a", '"dup"'))                   # a duplicate key
        if i % 13 == 0:
            p.append(("big", "9007199254740993"))      # past 2^53
        if i % 17 == 0:
            p.append(("nest", '{"x":[1,2,{"y":null}]}'))
        if i % 19 == 0:
            p[0] = ("a", '"str"')                      # a string in an integer column
        if i > n * 3 // 4:
            p.append(("late", str(i)))                 # a late column
        if i % 4 == 0:
            rng.shuffle(p)                             # its own key order
        return feature(i, p)
    return collection(row(i) for i in range(n))


def gen_ownorder(n, rng):
    keys = [f"k{k}" for k in range(8)]
    def row(i):
        ks = keys[:]
        rng.shuffle(ks)
        return feature(i, [(k, str(i + j)) for j, k in enumerate(ks)])
    return collection(row(i) for i in range(n))


def gen_stretches(n, overlap):
    a = [f"a{k}" for k in range(10)]
    b = [f"b{k}" for k in range(8)][::-1]
    first = [feature(i, [(k, str(i + j)) for j, k in enumerate(a)]) for i in range(n)]
    second = [feature(i - n if overlap else i, [(k, json.dumps(f"x{i}")) for k in b]) for i in range(n, 2 * n)]
    return collection(first + second)


def generate(out):
    gen_geojson, gen_wide_geojson = compare_generators()
    rng = random.Random(7)
    files = {
        "narrow": gen_geojson(size(20000)),
        "wide": gen_wide_geojson(50000 if WIDE else size(2000), 200),
        "sparse": gen_sparse(size(1000), rng),
        "ragged": gen_ragged(size(20000), rng),
        "ownorder": gen_ownorder(size(5000), rng),
        "stretch": gen_stretches(size(10000), overlap=False),
        "overlap": gen_stretches(size(10000), overlap=True),
    }
    paths = []
    for name, text in files.items():
        path = os.path.join(out, name + ".geojson")
        with open(path, "w") as f:
            f.write(text)
        paths.append(path)
    return paths


def derive(pin, sources, out):
    """Each generated file in the other readable formats, written by the pinned
    binary; a format that cannot take a fixture is left out."""
    paths = []
    for src in sources:
        stem = os.path.splitext(os.path.basename(src))[0]
        for to in DERIVED:
            path = os.path.join(out, f"{stem}.{to}")
            if subprocess.run([pin, src, path, "--quiet"], capture_output=True).returncode == 0:
                paths.append(path)
    return paths


# ---- running and comparing ---------------------------------------------------

def run_one(binary, src, target, opts, where):
    """Run one conversion into `where`; False if it ran past TIMEOUT."""
    os.makedirs(where)
    with open(os.path.join(where, "stdout"), "wb") as out, open(os.path.join(where, "stderr"), "wb") as err:
        try:
            status = subprocess.run([binary, src, "out." + target, *opts], cwd=where, stdout=out, stderr=err,
                                    timeout=TIMEOUT).returncode
        except subprocess.TimeoutExpired:
            return False
    with open(os.path.join(where, "status"), "w") as f:
        f.write(f"{status}\n")
    return True


def first_difference(a, b):
    """None when the two run directories hold the same files, byte for byte;
    else a short description of the first that differs."""
    names_a, names_b = sorted(os.listdir(a)), sorted(os.listdir(b))
    if names_a != names_b:
        return f"files {names_a} against {names_b}"
    for name in names_a:
        pa, pb = os.path.join(a, name), os.path.join(b, name)
        if not filecmp.cmp(pa, pb, shallow=False):
            with open(pa, "rb") as fa, open(pb, "rb") as fb:
                da, db = fa.read(), fb.read()
            at = next((i for i, (x, y) in enumerate(zip(da, db)) if x != y), min(len(da), len(db)))
            return f"{name} differs at byte {at} ({len(da)} against {len(db)} bytes)"
    return None


def case(job):
    n, pin, cand, src, target, opts, work = job
    where = os.path.join(work, f"{n:05d}")
    finished = run_one(pin, src, target, opts, os.path.join(where, "pin"))
    finished &= run_one(cand, src, target, opts, os.path.join(where, "candidate"))
    if not finished:
        return f"unchecked: ran past {TIMEOUT:.0f} s", where
    diff = first_difference(os.path.join(where, "pin"), os.path.join(where, "candidate"))
    if diff is None and not KEEP:
        shutil.rmtree(where)
    return diff, where


def main():
    started = time.time()
    print("building the pinned and candidate binaries ...", file=sys.stderr)
    pin, cand = pinned_bin(), candidate_bin()
    work = tempfile.mkdtemp(prefix="geosetta-equiv-")
    corpus = os.path.join(work, "corpus")
    os.makedirs(corpus)
    print(f"generating the corpus in {corpus} ...", file=sys.stderr)
    generated = generate(corpus)
    inputs = sorted(os.path.join(FIXTURES, f) for f in os.listdir(FIXTURES) if f.lower().endswith(READABLE))
    inputs += generated + derive(pin, generated, corpus)

    runs = os.path.join(work, "runs")
    jobs = [(src, target, opts) for src in inputs for target in TARGETS for opts in options(target)]
    jobs = [(n, pin, cand, src, target, opts, runs) for n, (src, target, opts) in enumerate(jobs)]
    print(f"{len(inputs)} inputs, {len(jobs)} conversions, {JOBS} at a time ...", file=sys.stderr)
    differ, unchecked = [], []
    with concurrent.futures.ThreadPoolExecutor(JOBS) as pool:
        for job, (diff, where) in zip(jobs, pool.map(case, jobs)):
            if diff is not None:
                _, _, _, src, target, opts, _ = job
                label = f"{os.path.basename(src)} -> {target} {' '.join(opts)}".rstrip()
                (unchecked if diff.startswith("unchecked") else differ).append((label, diff, where))

    print(f"pinned:    {pin}\ncandidate: {cand}")
    print(f"{len(inputs)} inputs, {len(jobs)} conversions: {len(jobs) - len(differ) - len(unchecked)} identical, "
          f"{len(differ)} differ, {len(unchecked)} unchecked ({time.time() - started:.0f} s)")
    for label, diff, where in (differ + unchecked)[:40]:
        print(f"  {label}: {diff}\n    {where}")
    if len(differ) + len(unchecked) > 40:
        print(f"  ... and {len(differ) + len(unchecked) - 40} more")
    if not differ and not unchecked and not KEEP:
        shutil.rmtree(work)
    else:
        print(f"work dir: {work}")
    return 1 if differ else 2 if unchecked else 0


if __name__ == "__main__":
    sys.exit(main())
