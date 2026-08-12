#!/usr/bin/env python3
"""Generate the WKT1->PROJJSON crosswalk tables + fixtures for milestone C phase 2.

Build-time only (needs PROJ's `projinfo` + `proj.db`). GDAL/PROJ is used as a
*codegen tool*, never a runtime dependency — the emitted tables ship as static
Rust data.

For a spread of real EPSG projected CRSes it reads two encodings of each:

  * PROJJSON        — canonical method/param *names*, EPSG *codes*, and *units*,
                      already structured (no WKT2 parsing needed).
  * WKT1_GDAL       — GDAL's own projection/parameter *names*, which carry no ids.

Walking the two parameter lists in parallel (position-aligned, value-checked)
reads off the mapping WKT1 lacks:

  methods:  gdal_projection_name              -> (canonical name, EPSG method code)
  params:   (method_code, gdal_param_name)    -> (canonical name, EPSG code, unit kind)

Outputs:
  * src/crs/wkt1_tables.rs        — generated Rust tables (do not hand-edit)
  * tests/fixtures/projected_wkt1.tsv  — `code<TAB>WKT1_GDAL` oracle corpus

Usage: tests/fixtures/gen_wkt1_crosswalk.py [per_method_cap]
"""
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
CAP = int(sys.argv[1]) if len(sys.argv) > 1 else 200

PROJ_DB = os.environ.get("PROJ_DB") or next(
    (p for p in (
        os.path.join(os.environ.get("PROJ_DATA", ""), "proj.db"),
        "/opt/homebrew/share/proj/proj.db",
        "/usr/local/share/proj/proj.db",
        "/usr/share/proj/proj.db",
    ) if p and os.path.isfile(p)),
    None,
)
if not PROJ_DB:
    sys.exit("proj.db not found; set PROJ_DB=/path/to/proj.db")


def projinfo(code, fmt):
    return subprocess.run(
        ["projinfo", "-o", fmt, "-q", f"EPSG:{code}"],
        capture_output=True, text=True,
    ).stdout.strip()


def unit_kind(u):
    if isinstance(u, str):
        return {"degree": "Angular", "unity": "Scale", "metre": "Linear"}.get(u, "Linear")
    t = u.get("type", "")
    return "Angular" if "Angular" in t else "Scale" if "Scale" in t else "Linear"


# code -> WKT1 projection name; PARAMETER names in document order; AXIS directions.
PROJ_RE = re.compile(r'PROJECTION\["([^"]+)"')
PARAM_RE = re.compile(r'PARAMETER\["([^"]+)",\s*([-0-9.eE]+)')
AXIS_RE = re.compile(r'AXIS\["[^"]*",\s*([A-Za-z]+)\]')

codes = subprocess.run(
    ["sqlite3", PROJ_DB,
     f"""with ranked as (
           select p.code c, cv.method_name m,
                  row_number() over (partition by cv.method_name order by p.code) rn
           from projected_crs p
           join conversion cv
             on p.conversion_auth_name = cv.auth_name and p.conversion_code = cv.code
           where p.auth_name = 'EPSG' and p.deprecated = 0)
         select c from ranked where rn <= {CAP} order by c;"""],
    capture_output=True, text=True,
).stdout.split()

from collections import defaultdict

# Observations across all sampled CRSes. A WKT1 name is *ambiguous* when it is
# seen mapping to more than one EPSG code — WKT1 (GDAL flavor) cannot tell those
# apart from the name alone (e.g. Polar_Stereographic variant A vs B). Ambiguous
# methods/params are excluded from the tables: the translator returns `None` for
# them and the writer falls back to an id reference + warning, never a guess.
method_obs = defaultdict(set)      # gdal_proj_name -> {(canonical, code)}
param_obs = defaultdict(set)       # (method_code, gdal_param_name) -> {(canonical, code, kind)}
candidates = []                    # (code, gdal_proj_name, method_code, wkt1, translatable)
axis_unsafe = set()                # methods whose WKT1 AXIS disagrees with the true CS
not_en_default = set()             # methods with a no-AXIS CRS whose true CS is not E/N
skipped = 0

for code in codes:
    pj_text = projinfo(code, "PROJJSON")
    w1 = projinfo(code, "WKT1_GDAL")
    if not pj_text or not w1.startswith("PROJCS"):
        skipped += 1
        continue
    pj = json.loads(pj_text)
    conv = pj.get("conversion", {})
    method = conv.get("method", {})
    mname, mid = method.get("name"), method.get("id", {}).get("code")
    pj_params = conv.get("parameters", [])
    if mid is None:
        skipped += 1
        continue

    gdal_proj = PROJ_RE.search(w1)
    gdal_params = PARAM_RE.findall(w1)
    if not gdal_proj or len(gdal_params) != len(pj_params):
        skipped += 1
        continue

    # Value-checked positional alignment: guards against any order mismatch.
    ok = True
    for (gname, gval), pp in zip(gdal_params, pj_params):
        try:
            if abs(float(gval) - float(pp["value"])) > 1e-6 * max(1.0, abs(float(pp["value"]))):
                ok = False
                break
        except (TypeError, ValueError):
            ok = False
            break
    if not ok:
        skipped += 1
        continue

    gname_proj = gdal_proj.group(1)

    # Coordinate-system reconstructability. The translator uses WKT1 `AXIS` nodes
    # when present, else defaults to easting, northing. So:
    #   * a CRS *with* AXIS is translatable iff those axes match the true CS
    #     (else the method is axis-unsafe — GDAL's AXIS can't be trusted);
    #   * a CRS *without* AXIS is translatable only if its true CS is E/N — GDAL
    #     WKT1 omits AXIS even for south/west-oriented grids, silently dropping
    #     the orientation, and the runtime (seeing only the method) can't tell.
    #     A method with any such no-AXIS non-EN CRS is not `en_default_safe`, but
    #     stays usable for its AXIS-bearing CRSes.
    proj_dirs = [a["direction"] for a in pj.get("coordinate_system", {}).get("axis", [])]
    w1_dirs = [d.lower() for d in AXIS_RE.findall(w1)]
    if w1_dirs:
        translatable = w1_dirs == proj_dirs
        if not translatable:
            axis_unsafe.add(gname_proj)
    else:
        translatable = proj_dirs == ["east", "north"]
        if not translatable:
            not_en_default.add(gname_proj)

    method_obs[gname_proj].add((mname, mid))
    for (gname, _), pp in zip(gdal_params, pj_params):
        param_obs[(mid, gname)].add((pp["name"], pp["id"]["code"], unit_kind(pp["unit"])))
    candidates.append((code, gname_proj, mid, " ".join(w1.split()), translatable, bool(w1_dirs)))

# A method survives only if its WKT1 name is unambiguous *and* none of its
# parameters are ambiguous — otherwise translating it could produce a wrong CRS.
ambiguous_methods = {g for g, s in method_obs.items() if len({c for _, c in s}) > 1}
ambiguous_params = {k for k, s in param_obs.items() if len({c for _, c, _ in s}) > 1}
bad_method_codes = {mid for (mid, _) in ambiguous_params}
# A method is kept when its name is unambiguous, none of its parameters are
# ambiguous, and its WKT1 AXIS nodes (when present) are trustworthy.
excluded_methods = {
    g for g, s in method_obs.items()
    if g in ambiguous_methods or g in axis_unsafe
    or any(mid in bad_method_codes for _, mid in s)
}
methods = {
    g: (canon, mid, g not in not_en_default)   # (canonical, code, en_default_safe)
    for g, s in method_obs.items() if g not in excluded_methods
    for (canon, mid) in [next(iter(s))]
}
kept_codes = {mid for _, (_, mid, _) in methods.items()}
params = {k: next(iter(s)) for k, s in param_obs.items() if k[0] in kept_codes}
# Fixtures: exactly the CRSes the runtime resolves — kept method, CS rebuildable,
# and (AXIS-bearing OR the method is en_default_safe). Mirrors the runtime gate in
# `render_projected_wkt1` so the oracle never expects a CRS the translator declines.
fixtures = [
    (c, w1) for (c, g, mid, w1, ok, has_axis) in candidates
    if g in methods and ok and (has_axis or methods[g][2])
]

print(f"excluded (ambiguous/axis-unsafe, fall back + warn): "
      f"{', '.join(sorted(excluded_methods)) or 'none'}", file=sys.stderr)
print(f"kept-but-AXIS-only (no-AXIS non-EN falls back): "
      f"{', '.join(sorted(g for g in methods if g in not_en_default)) or 'none'}", file=sys.stderr)

proj_ver = subprocess.run(["projinfo"], capture_output=True, text=True).stderr.split("\n")[0]


def rs_str(s):
    return '"' + s.replace('\\', '\\\\').replace('"', '\\"') + '"'


lines = [
    "//! GENERATED by tests/fixtures/gen_wkt1_crosswalk.py — do not edit by hand.",
    f"//! Source: PROJ EPSG registry via projinfo ({len(fixtures)} CRSes, cap {CAP}/method).",
    "//!",
    "//! WKT1 (GDAL flavor) carries GDAL's projection/parameter names with no EPSG",
    "//! ids; these tables restore the ids PROJJSON needs. Keyed data only — the",
    "//! translation logic lives in `wkt_projjson.rs`.",
    "",
    "use super::wkt_projjson::UnitKind;",
    "",
    "/// WKT1 GDAL `PROJECTION` name -> (canonical PROJJSON method name, EPSG code,",
    "/// en_default_safe). When `en_default_safe` is false the method has CRSes whose",
    "/// true CS is not easting/northing yet whose WKT1 omits `AXIS`; the translator",
    "/// then resolves only AXIS-bearing CRSes and falls back on the rest.",
    "pub(super) static METHODS: &[(&str, &str, i64, bool)] = &[",
]
for gname, (canon, code, en_safe) in sorted(methods.items()):
    lines.append(f"    ({rs_str(gname)}, {rs_str(canon)}, {code}, {str(en_safe).lower()}),")
lines += [
    "];",
    "",
    "/// (EPSG method code, WKT1 GDAL parameter name)",
    "///     -> (canonical PROJJSON parameter name, EPSG code, unit kind).",
    "pub(super) static PARAMS: &[(i64, &str, &str, i64, UnitKind)] = &[",
]
for (mid, gname), (canon, code, kind) in sorted(params.items()):
    lines.append(f"    ({mid}, {rs_str(gname)}, {rs_str(canon)}, {code}, UnitKind::{kind}),")
lines += ["];", ""]

with open(os.path.join(ROOT, "src/crs/wkt1_tables.rs"), "w") as f:
    f.write("\n".join(lines))

with open(os.path.join(HERE, "projected_wkt1.tsv"), "w") as f:
    for code, w1 in fixtures:
        f.write(f"{code}\t{w1}\n")

print(f"{proj_ver}")
print(f"methods: {len(methods)}, params: {len(params)}, fixtures: {len(fixtures)}, skipped: {skipped}")
