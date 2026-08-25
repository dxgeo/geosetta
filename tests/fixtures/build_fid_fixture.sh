#!/bin/sh
# Build sqlite_fid_gap.gpkg, added 2026-08-25, from the ogr2ogr base.
# Recorded in tests/fixtures/HOSTILE.org; re-run from the geosetta/ root.
#
# Every other .gpkg in this tree holds feature ids that are dense 1..n, so the
# whole corpus round-trips through a renumbering unchanged and is blind to it.
# That is how handoff.org § BUGS FOUND entry 19 stayed hidden. This fixture is
# the one file where the ids are not the positions.
set -e
cd "$(dirname "$0")/../.."
BASE=tests/fixtures/gdal_hostile.gpkg
OUT=tests/fixtures/sqlite_fid_gap.gpkg

rm -f "$OUT"
cp "$BASE" "$OUT"

# ogr2ogr guards its layers with ST_* triggers that sqlite3 cannot evaluate
# (no spatialite loaded), so they are dropped before the rows are edited. They
# are validation only -- the geometries written here are copied unchanged from
# rows that already passed them.
for t in $(sqlite3 "$OUT" "SELECT name FROM sqlite_master WHERE type='trigger';"); do
  sqlite3 "$OUT" "DROP TRIGGER \"$t\";"
done

sqlite3 "$OUT" <<'SQL'
-- Leave a gap at both ends of the range and in the middle: fids 1, 9, 40 for
-- three rows, so a writer numbering by position produces 1, 2, 3 and every
-- one of the three is wrong in a different way (right, shifted, far off).
INSERT INTO cities (fid, name, zip, price, note, flag, geom)
  SELECT 9, 'nine', '009', 9.0, '', 1, geom FROM cities WHERE fid = 1;
INSERT INTO cities (fid, name, zip, price, note, flag, geom)
  SELECT 40, 'forty', '040', 40.0, '', 0, geom FROM cities WHERE fid = 1;
DELETE FROM cities WHERE fid = 2;

-- `second` keeps its dense ids, so one file holds both cases and a fix that
-- carries ids unconditionally is distinguishable from one that carries them
-- only where they differ.
SQL

echo "built:"
printf '  %s  ' "$OUT"
sqlite3 "$OUT" "PRAGMA integrity_check;"
printf '  cities fids: '; sqlite3 "$OUT" "SELECT group_concat(fid, ', ') FROM cities;"
printf '  second fids: '; sqlite3 "$OUT" "SELECT group_concat(fid, ', ') FROM second;"
