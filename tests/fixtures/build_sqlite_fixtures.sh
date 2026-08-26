#!/bin/sh
# Build the two sqlite_* fixtures added 2026-08-23, from the ogr2ogr base.
# Recorded in tests/fixtures/HOSTILE.org; re-run from the geosetta/ root.
set -e
cd "$(dirname "$0")/../.."
BASE=tests/fixtures/gdal_hostile.gpkg

# --- sqlite_contents.gpkg: gpkg_contents rows that are not feature layers ----
rm -f tests/fixtures/sqlite_contents.gpkg
cp "$BASE" tests/fixtures/sqlite_contents.gpkg
sqlite3 tests/fixtures/sqlite_contents.gpkg <<'SQL'
-- An aspatial table, registered the way ogr2ogr registers one.
CREATE TABLE notes_aspatial (id INTEGER PRIMARY KEY, note TEXT);
-- Ids 1 and 9, not 1 and 2 (2026-08-25). Requirement 119's NOTE gives an
-- attributes table's rowids the same stated purpose Requirement 29's NOTE
-- gives a feature table's: linking to gpkg_metadata by rowid through
-- gpkg_metadata_reference. A dense 1,2 cannot tell a carried rowid from a
-- reissued one -- the blindness that hid handoff.org entry 19 -- and 9 is the
-- id a reissue would silently move.
INSERT INTO notes_aspatial VALUES (1, 'aspatial'), (9, 'still aspatial');
INSERT INTO gpkg_contents
  VALUES ('notes_aspatial','attributes','notes_aspatial','',
          '2026-08-23T00:00:00.000Z',NULL,NULL,NULL,NULL,0);

-- The gpkg_ogr_contents row and the two maintenance triggers ogr2ogr attaches
-- to an aspatial table, spelled as GDAL 3.13.2 spells them (added 2026-08-25).
-- Carrying the table verbatim would carry these, and gpkg_ogr_contents is the
-- one member of container.rs's DERIVED class -- this writer never emits it. A
-- trigger whose body names a missing table is created without complaint,
-- PRAGMA integrity_check answers ok and foreign_key_check says nothing, and
-- then every INSERT fails with "no such table: main.gpkg_ogr_contents". So the
-- carry drops them, and without these two triggers that rule can be deleted
-- and no test in the suite notices.
INSERT INTO gpkg_ogr_contents VALUES ('notes_aspatial', 2);
CREATE TRIGGER "trigger_insert_feature_count_notes_aspatial"
AFTER INSERT ON "notes_aspatial"
BEGIN UPDATE gpkg_ogr_contents SET feature_count = feature_count + 1
  WHERE lower(table_name) = lower('notes_aspatial'); END;
CREATE TRIGGER "trigger_delete_feature_count_notes_aspatial"
AFTER DELETE ON "notes_aspatial"
BEGIN UPDATE gpkg_ogr_contents SET feature_count = feature_count - 1
  WHERE lower(table_name) = lower('notes_aspatial'); END;

-- A table-scoped metadata reference naming the aspatial dataset (2026-08-25).
-- Requirement 97 makes the output's gpkg_contents the authority for a
-- reference's table_name, so this row is dropped while the dataset is not
-- carried and SHALL survive once it is. Nothing else in the tree references
-- anything but a feature layer and a tile pyramid.
INSERT INTO gpkg_metadata_reference
  VALUES ('table','notes_aspatial',NULL,NULL,'2026-08-23T00:00:00.000Z',1,NULL);
-- And a row-scoped one at the gap id, which is Requirement 99 pointed at an
-- attributes dataset: the row_id_value hazard container-reconciliation.org D5
-- describes, in the one place outside a feature layer it can arise.
INSERT INTO gpkg_metadata_reference
  VALUES ('row','notes_aspatial',NULL,9,'2026-08-23T00:00:00.000Z',1,NULL);

-- A tile pyramid: one tile, and the registry rows a tiles dataset requires.
CREATE TABLE imagery (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  zoom_level INTEGER NOT NULL,
  tile_column INTEGER NOT NULL,
  tile_row INTEGER NOT NULL,
  tile_data BLOB NOT NULL);
INSERT INTO imagery VALUES (1,0,0,0,X'89504E470D0A1A0A');
INSERT INTO gpkg_contents
  VALUES ('imagery','tiles','imagery','',
          '2026-08-23T00:00:00.000Z',0.0,0.0,1.0,1.0,4326);
INSERT INTO gpkg_tile_matrix_set VALUES ('imagery',4326,0.0,0.0,1.0,1.0);
INSERT INTO gpkg_tile_matrix VALUES ('imagery',0,1,1,256,256,0.00390625,0.00390625);
SQL

# --- sqlite_constraints.gpkg: constraints that need an index b-tree ---------
rm -f tests/fixtures/sqlite_constraints.gpkg
cp "$BASE" tests/fixtures/sqlite_constraints.gpkg
sqlite3 tests/fixtures/sqlite_constraints.gpkg <<'SQL'
-- UNIQUE on an ordinary column: one sqlite_autoindex_*.
CREATE TABLE tags (id INTEGER PRIMARY KEY, tag TEXT UNIQUE, body TEXT);
INSERT INTO tags VALUES (1,'alpha','a'), (2,'beta','b'), (3,'gamma','c');

-- A TEXT PRIMARY KEY: not a rowid alias, so also an index b-tree. This is the
-- shape gpkg_contents itself has in the specification.
CREATE TABLE registry (key TEXT PRIMARY KEY, value TEXT);
INSERT INTO registry VALUES ('a','1'), ('b','2');

-- An explicit CREATE INDEX, which is a sqlite_master 'index' entry with a name
-- of its own rather than an autoindex.
CREATE TABLE events (id INTEGER PRIMARY KEY, kind TEXT, at TEXT);
INSERT INTO events VALUES (1,'open','2026-01-01'), (2,'close','2026-01-02');
CREATE INDEX idx_events_kind ON events(kind);

-- A UNIQUE carrying an explicit collation: the case D10 says must be *carried*
-- rather than constructed, because constructing it would need NOCASE.
CREATE TABLE people (id INTEGER PRIMARY KEY, email TEXT UNIQUE COLLATE NOCASE);
INSERT INTO people VALUES (1,'A@example.com'), (2,'b@example.com');

-- A trigger on a *user's own* table, added 2026-08-24. Every trigger in every
-- other fixture is attached to a table the write rebuilds -- a layer, or a
-- registry table -- and those are classed *derived* by
-- container.rs::is_derived_trigger, because what they enforce comes back with
-- the table. This one is attached to a table that is genuinely destroyed, so
-- it must still be reported. Without it the negative half of that rule is
-- untested, and a widening of it would go unnoticed.
CREATE TRIGGER events_stamp AFTER INSERT ON events
BEGIN
  UPDATE events SET at = '2026-01-01' WHERE id = NEW.id AND NEW.at IS NULL;
END;
SQL

echo "built:"
for f in tests/fixtures/sqlite_contents.gpkg tests/fixtures/sqlite_constraints.gpkg; do
  printf '  %s  ' "$f"
  sqlite3 "$f" "PRAGMA integrity_check;"
done
