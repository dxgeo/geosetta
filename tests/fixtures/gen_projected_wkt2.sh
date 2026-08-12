#!/usr/bin/env bash
# Regenerate tests/fixtures/projected_wkt2.tsv — the crosswalk oracle corpus for
# milestone C (WKT→PROJJSON, projected CRSes). For a spread of real EPSG
# projected CRSes it emits `EPSG_code<TAB>WKT2:2019_string` lines; the ignored
# test `crs::wkt_projjson::bulk_oracle_wkt2_projected_identifies` translates each
# and gates the output through `projinfo --identify` at 100% confidence.
#
# Requires PROJ (`projinfo`) and its `proj.db`. Pin/record the PROJ version:
#   projinfo --version
#
# Usage: tests/fixtures/gen_projected_wkt2.sh [per_method_cap]
set -euo pipefail

cap="${1:-50}"
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/projected_wkt2.tsv"

# Locate proj.db: $PROJ_DB, then PROJ_LIB/PROJ_DATA, then next to projinfo, then
# common prefixes.
db="${PROJ_DB:-}"
if [ -z "$db" ]; then
  for d in "${PROJ_DATA:-}" "${PROJ_LIB:-}" \
           "$(dirname "$(command -v projinfo)")/../share/proj" \
           /opt/homebrew/share/proj /usr/local/share/proj /usr/share/proj; do
    [ -n "$d" ] && [ -f "$d/proj.db" ] && { db="$d/proj.db"; break; }
  done
fi
[ -f "$db" ] || { echo "proj.db not found; set PROJ_DB=/path/to/proj.db" >&2; exit 1; }

# Sample up to `cap` non-deprecated projected CRSes per projection method, across
# *all* methods, so every structural shape (grad units, feet, polar meridians,
# non-Greenwich prime meridians, LAEA axis order, …) is exercised.
codes="$(sqlite3 "$db" "
  with ranked as (
    select p.code c, cv.method_name m,
           row_number() over (partition by cv.method_name order by p.code) rn
    from projected_crs p
    join conversion cv
      on p.conversion_auth_name = cv.auth_name and p.conversion_code = cv.code
    where p.auth_name = 'EPSG' and p.deprecated = 0
  )
  select 'EPSG:' || c from ranked where rn <= $cap order by c;
")"

: > "$out"
n=0
while IFS= read -r crs; do
  [ -n "$crs" ] || continue
  wkt2="$(projinfo -o WKT2:2019 -q "$crs" 2>/dev/null | tr -d '\n' | tr -s ' ')"
  [ -n "$wkt2" ] || continue
  printf '%s\t%s\n' "${crs#EPSG:}" "$wkt2" >> "$out"
  n=$((n + 1))
done <<< "$codes"

echo "wrote $n fixtures to $out (per-method cap $cap; $(projinfo 2>&1 1>/dev/null | grep -iE '^Rel' | head -1 || true))"
