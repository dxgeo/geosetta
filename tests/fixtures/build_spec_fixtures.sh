#!/bin/sh
# Build spec_conformant.gpkg -- a GeoPackage whose registry DDL comes from the
# standard's own normative text, with no GDAL anywhere in its provenance.
#
# Why this is a separate script from build_sqlite_fixtures.sh: that one starts
# `cp gdal_hostile.gpkg`, and a fixture derived from GDAL cannot test
# conformance to a standard GDAL does not follow exactly. Nothing below reads
# an existing file.
#
# The DDL is verbatim from OGC 12-128r19, transcribed mechanically rather than
# by hand. Re-verify any statement against the source with:
#
#   SHA=fe83fdd5949a3faba09117e6e1dcc003fdb5a900
#   curl -s https://raw.githubusercontent.com/opengeospatial/geopackage/$SHA/spec/core/annexes/ddl.adoc
#   curl -s .../spec/core/annexes/extension_schema.adoc
#   curl -s .../spec/core/annexes/extension_metadata.adoc
#
# Two edits are applied to that text, both recorded here because "verbatim"
# has to mean something precise:
#   1. `gpkg_data_column_constraints` carries `//` line comments in the annex
#      (`constraint_type TEXT NOT NULL, // 'range' | 'enum' | 'glob'`). That is
#      asciidoc prose, not SQL, and SQLite rejects it. Stripped.
#   2. That same block ends without a semicolon in the annex. Added.
# Nothing else differs, including the annex's double space in `definition  TEXT
# NOT NULL` and its inconsistent spacing around `REFERENCES ... (srs_id)`.
# Both are preserved: they are what the standard says.
set -e
cd "$(dirname "$0")/../.."
OUT=tests/fixtures/spec_conformant.gpkg
rm -f "$OUT"

sqlite3 "$OUT" <<'SQL'
PRAGMA application_id = 1196444487;  -- 0x47504B47, "GPKG"
PRAGMA user_version = 10200;         -- 1.2.0, matching this crate's writer

-- ---------------------------------------------------------------------------
-- Annex C and the Schema/Metadata extension annexes, verbatim.
-- ---------------------------------------------------------------------------
CREATE TABLE gpkg_spatial_ref_sys (
  srs_name TEXT NOT NULL,
  srs_id INTEGER PRIMARY KEY,
  organization TEXT NOT NULL,
  organization_coordsys_id INTEGER NOT NULL,
  definition  TEXT NOT NULL,
  description TEXT
);

CREATE TABLE gpkg_contents (
  table_name TEXT NOT NULL PRIMARY KEY,
  data_type TEXT NOT NULL,
  identifier TEXT UNIQUE,
  description TEXT DEFAULT '',
  last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  min_x DOUBLE,
  min_y DOUBLE,
  max_x DOUBLE,
  max_y DOUBLE,
  srs_id INTEGER,
  CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id)
);

CREATE TABLE gpkg_geometry_columns (
  table_name TEXT NOT NULL,
  column_name TEXT NOT NULL,
  geometry_type_name TEXT NOT NULL,
  srs_id INTEGER NOT NULL,
  z TINYINT NOT NULL,
  m TINYINT NOT NULL,
  CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name),
  CONSTRAINT uk_gc_table_name UNIQUE (table_name),
  CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),
  CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id)
);

CREATE TABLE gpkg_extensions (
  table_name TEXT,
  column_name TEXT,
  extension_name TEXT NOT NULL,
  definition TEXT NOT NULL,
  scope TEXT NOT NULL,
  CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name)
);

CREATE TABLE gpkg_data_columns (
  table_name TEXT NOT NULL,
  column_name TEXT NOT NULL,
  name TEXT,
  title TEXT,
  description TEXT,
  mime_type TEXT,
  constraint_name TEXT,
  CONSTRAINT pk_gdc PRIMARY KEY (table_name, column_name),
  CONSTRAINT gdc_tn UNIQUE (table_name, name)
);

CREATE TABLE gpkg_data_column_constraints (
  constraint_name TEXT NOT NULL,
  constraint_type TEXT NOT NULL,
  value TEXT,
  min NUMERIC,
  min_is_inclusive BOOLEAN,
  max NUMERIC,
  max_is_inclusive BOOLEAN,
  description TEXT,
  CONSTRAINT gdcc_ntv UNIQUE (constraint_name, constraint_type, value)
);

-- The two tile-pyramid registry tables, added 2026-08-24 when the writer
-- gained the ability to emit them (M3b's carrying half). Same annex, same
-- pinned commit as everything above. Left empty: this fixture is an oracle for
-- what the standard *declares*, and a populated pyramid would make it a
-- fixture about tiles -- which is gdal_tiles.gpkg's job.
CREATE TABLE gpkg_tile_matrix_set (
  table_name TEXT NOT NULL PRIMARY KEY,
  srs_id INTEGER NOT NULL,
  min_x DOUBLE NOT NULL,
  min_y DOUBLE NOT NULL,
  max_x DOUBLE NOT NULL,
  max_y DOUBLE NOT NULL,
  CONSTRAINT fk_gtms_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),
  CONSTRAINT fk_gtms_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id)
);

CREATE TABLE gpkg_tile_matrix (
  table_name TEXT NOT NULL,
  zoom_level INTEGER NOT NULL,
  matrix_width INTEGER NOT NULL,
  matrix_height INTEGER NOT NULL,
  tile_width INTEGER NOT NULL,
  tile_height INTEGER NOT NULL,
  pixel_x_size DOUBLE NOT NULL,
  pixel_y_size DOUBLE NOT NULL,
  CONSTRAINT pk_ttm PRIMARY KEY (table_name, zoom_level),
  CONSTRAINT fk_tmm_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name)
);

CREATE TABLE gpkg_metadata (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  md_scope TEXT NOT NULL DEFAULT 'dataset',
  md_standard_uri TEXT NOT NULL,
  mime_type TEXT NOT NULL DEFAULT 'text/xml',
  metadata TEXT NOT NULL DEFAULT ''
);

CREATE TABLE gpkg_metadata_reference (
  reference_scope TEXT NOT NULL,
  table_name TEXT,
  column_name TEXT,
  row_id_value INTEGER,
  timestamp DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  md_file_id INTEGER NOT NULL,
  md_parent_id INTEGER,
  CONSTRAINT crmr_mfi_fk FOREIGN KEY (md_file_id) REFERENCES gpkg_metadata(id),
  CONSTRAINT crmr_mpi_fk FOREIGN KEY (md_parent_id) REFERENCES gpkg_metadata(id)
);

-- ---------------------------------------------------------------------------
-- Content. The three gpkg_spatial_ref_sys records the standard requires: one
-- real CRS plus the two "undefined" reservations, which exist so that a
-- geometry with no meaningful CRS still has an srs_id to point at.
-- ---------------------------------------------------------------------------
INSERT INTO gpkg_spatial_ref_sys VALUES
  ('WGS 84 geodetic', 4326, 'EPSG', 4326,
   'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563,AUTHORITY["EPSG","7030"]],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4326"]]',
   'longitude/latitude coordinates in decimal degrees on the WGS 84 spheroid'),
  ('Undefined cartesian SRS', -1, 'NONE', -1, 'undefined',
   'undefined cartesian coordinate reference system'),
  ('Undefined geographic SRS', 0, 'NONE', 0, 'undefined',
   'undefined geographic coordinate reference system');

-- One feature table, deliberately plain: this fixture's load-bearing content
-- is the registry DDL above, and the layer exists only so the reader has a
-- dataset to find -- geopackage/reader.rs errors without gpkg_contents and
-- filters on data_type='features'. Its DDL follows Annex C's
-- sample_feature_table shape.
CREATE TABLE sample_features (
  fid INTEGER PRIMARY KEY AUTOINCREMENT,
  geom POINT,
  name TEXT
);

-- Text and geometry go in as hex rather than as literals, per HOSTILE.org's
-- "hand-decode the actual bytes" rule -- an earlier fixture generation was
-- silently mangled passing UTF-8 through a shell heredoc, and this sidesteps
-- the question entirely. 4BC3B862656E6861766E is 'København' and
-- 5AC3BC72696368 is 'Zürich', the same two names the common source uses.
-- The blobs are GeoPackageBinary: 'GP', version 0, flags 0x01 (little-endian
-- header, no envelope), LE srs_id 4326, then little-endian WKB point.
INSERT INTO sample_features (fid, geom, name) VALUES
  (1, X'47500001E6100000010100000034A2B437F8222940AD69DE718AD64B40',
      CAST(X'4BC3B862656E6861766E' AS TEXT)),
  (2, X'47500001E61000000101000000857CD0B35915214063EE5A423EB04740',
      CAST(X'5AC3BC72696368' AS TEXT));

INSERT INTO gpkg_contents
  (table_name, data_type, identifier, description, last_change,
   min_x, min_y, max_x, max_y, srs_id)
VALUES
  ('sample_features', 'features', 'sample_features', '',
   '2026-08-23T00:00:00.000Z', 8.5417, 47.3769, 12.5683, 55.6761, 4326);

INSERT INTO gpkg_geometry_columns VALUES
  ('sample_features', 'geom', 'POINT', 4326, 0, 0);
SQL

printf 'built:   %s\n' "$OUT"
printf 'integrity_check:   '; sqlite3 "$OUT" 'PRAGMA integrity_check;'
printf 'foreign_key_check: '; sqlite3 "$OUT" 'PRAGMA foreign_key_check;' && echo 'clean'
printf 'application_id:    '; sqlite3 "$OUT" 'PRAGMA application_id;'
printf 'user_version:      '; sqlite3 "$OUT" 'PRAGMA user_version;'
