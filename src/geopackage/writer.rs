//! Write GeoPackage files: build the required GPKG metadata tables plus one
//! feature table per layer, and serialize the whole thing with the from-scratch
//! [`crate::sqlite`] writer. Append is read-modify-write — read the existing
//! layers, upsert, and rewrite the complete file (see `plans/geopackage.org`).

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::FeatureCollection;
use crate::geometry::{to_wkb, Bbox, Geometry};
use crate::schema::{columns_for, Cell, Column, ColumnType, DeclType};
use crate::format::Format;
use crate::native::{GpkgConstraint, GpkgDataColumn, GpkgNative, NativePayload};
use crate::sqlite::{Autoincrement, MasterEntry, TableSpec, Value, autoindex_records, write_database};

use super::carry::{Carried, Metadata, MetadataReference, Pyramid, SrsDef, Storage, UserTable};
use super::reader::read_layers;
use super::rtree;

const APPLICATION_ID: u32 = 0x4750_4B47; // "GPKG"
const USER_VERSION: u32 = 10200; // 1.2.0
/// srs_id of the WGS 84 default row (the well-known GeoPackage value).
const WGS84_SRS_ID: i64 = 4326;
/// srs_id of the "undefined geographic" row, used for layers whose source
/// recorded no CRS at all (e.g. from CSV or WKT input).
const UNDEFINED_SRS_ID: i64 = 0;
/// First srs_id handed out to a CRS that carries no usable authority code.
const SYNTHETIC_SRS_BASE: i64 = 100_000;
/// The value written into `gpkg_contents.last_change`, for every layer, on
/// every run.
///
/// **A fixed epoch, so this writer's output is byte-identical across runs.**
/// The alternative is `strftime('%Y-%m-%dT%H:%M:%fZ','now')`, which is what the
/// column's DEFAULT expression holds and what `ogr2ogr` writes, and which would
/// make two runs over the same input produce two different files.
///
/// *This is conformant, and worth recording why rather than leaving it to be
/// rediscovered.* The standard's only requirement touching the value is that it
/// be **in DATETIME format** — "Values of the `gpkg_contents` table
/// `last_change` column SHALL be in a DATETIME format as per Requirement 5",
/// and Requirement 5 is about data types. The epoch is a well-formed ISO-8601
/// UTC string, so it passes, and the abstract test suite checks format alone.
///
/// *But note what it costs, because the standard does define this column's
/// meaning:* `gpkg_contents.last_change` is described as "timestamp of last
/// change to content". A fixed value does not carry that meaning, and it is the
/// only registry column this writer fills with something chosen for the
/// writer's convenience rather than read from the data. The neighbouring
/// `gpkg_metadata_reference.timestamp` is the contrasting case — the standard
/// gives it no semantics at all, only a format — which is argued in
/// `plans/container-reconciliation.org` § 6 rule 3.
///
/// **Decided, and this constant is the interim behaviour rather than the
/// intended one.** A `gpkg -> gpkg` run overwrites the source's `last_change`
/// with this constant, which is a *loss*: the rule is to **carry** the source's
/// value, and to write a fresh stamp only when there is none to carry — a
/// cross-format source. See `plans/same-format-fidelity.org` § OPEN QUESTIONS.
///
/// *The rule is short because of an invariant this crate already holds:* the
/// stamp follows whoever last changed the content, and **that is never
/// geosetta**. It does not reproject (see this crate's `main` module doc and
/// [`crate::crs`]); a reprojection tool composes through a pipe or the mutation
/// seam, and the change happens there. `--crs` is *assign, not enrich* — it
/// relabels a declared CRS without moving a coordinate — so it is not a content
/// change either.
///
/// So it is a field on [`crate::native::GpkgNative`] beside `identifier` and
/// `description`, classed *independent* for exactly that reason.
///
/// ***The carry half shipped 2026-08-24 and this constant is now the fallback
/// rather than the answer.*** A layer that carried a `last_change` writes it
/// back; one that did not — a cross-format source, or a file this crate wrote —
/// gets this. /It needed nothing from M3b: `gpkg_contents`'s descriptive
/// columns were already in the layer natives channel. An earlier draft of this
/// comment said it travelled with M3b's container work, which was wrong./
///
/// **The stamp half is deliberately not built.** Writing a fresh timestamp
/// where there is nothing to carry would be more honest than this sentinel and
/// would cost byte-reproducibility on nearly every run, since almost everything
/// this tool does is cross-format. The test
/// `two_writes_of_the_same_input_are_byte_identical` in this module exists so
/// that trade cannot be made by accident.
pub(crate) const LAST_CHANGE: &str = "1970-01-01T00:00:00.000Z";

// The registry tables' DDL, as the GeoPackage specification defines them —
// **including the table constraints**, which is what makes each of these a
// statement about the schema rather than a loose approximation of it.
//
// Every `UNIQUE` and every non-INTEGER `PRIMARY KEY` below obliges this writer
// to emit a `sqlite_autoindex_*` b-tree alongside the table; declaring one
// without the other produces a database SQLite reports as malformed. The
// obligation is carried by `crate::sqlite::TableSpec::autoindexes`, and each
// table's construction site names which constraint each index is for.
//
// **These were emitted constraint-free until 2026-08-23** — *defect 6* in
// `plans/same-format-fidelity.org` § THE DEFECTS. That was never a decision:
// `sqlite.rs` could not write an index b-tree, so the DDL that needed one was
// never written, and `gpkg_spatial_ref_sys` looked like the exception when it
// was really the only table whose key (`INTEGER PRIMARY KEY`, a rowid alias)
// needs no index at all.

/// `gpkg_spatial_ref_sys` — `srs_id INTEGER PRIMARY KEY` with **no
/// `NOT NULL`**: that is what the specification states, and `ogr2ogr` adds a
/// `NOT NULL` the spec does not ask for. Either way it is a rowid alias and
/// owns no index b-tree, which is why this was the one table already correct
/// when defect 6 was found.
const SPATIAL_REF_SYS_DDL: &str = "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT NOT NULL, srs_id INTEGER PRIMARY KEY, organization TEXT NOT NULL, organization_coordsys_id INTEGER NOT NULL, definition TEXT NOT NULL, description TEXT)";

/// `gpkg_contents` — the specification's definition, constraints included.
const CONTENTS_DDL: &str = "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL PRIMARY KEY, data_type TEXT NOT NULL, identifier TEXT UNIQUE, description TEXT DEFAULT '', last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), min_x DOUBLE, min_y DOUBLE, max_x DOUBLE, max_y DOUBLE, srs_id INTEGER, CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id))";

/// `gpkg_geometry_columns` — two keys and two foreign keys.
const GEOMETRY_COLUMNS_DDL: &str = "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, geometry_type_name TEXT NOT NULL, srs_id INTEGER NOT NULL, z TINYINT NOT NULL, m TINYINT NOT NULL, CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name), CONSTRAINT uk_gc_table_name UNIQUE (table_name), CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name), CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";

/// `gpkg_data_columns` — the Schema extension's column metadata. **Two**
/// constraints: `ogr2ogr` writes only `pk_gdc`, and the specification also
/// requires `gdc_tn`.
const DATA_COLUMNS_DDL: &str = "CREATE TABLE gpkg_data_columns (table_name TEXT NOT NULL, column_name TEXT NOT NULL, name TEXT, title TEXT, description TEXT, mime_type TEXT, constraint_name TEXT, CONSTRAINT pk_gdc PRIMARY KEY (table_name, column_name), CONSTRAINT gdc_tn UNIQUE (table_name, name))";

/// `gpkg_data_column_constraints` — the Schema extension's constraint
/// definitions. `ogr2ogr` writes this one with no constraint at all.
const DATA_COLUMN_CONSTRAINTS_DDL: &str = "CREATE TABLE gpkg_data_column_constraints (constraint_name TEXT NOT NULL, constraint_type TEXT NOT NULL, value TEXT, min NUMERIC, min_is_inclusive BOOLEAN, max NUMERIC, max_is_inclusive BOOLEAN, description TEXT, CONSTRAINT gdcc_ntv UNIQUE (constraint_name, constraint_type, value))";

/// `gpkg_tile_matrix_set` — one row per tile pyramid, giving its extent in its
/// own CRS.
///
/// Transcribed from OGC 12-128r19 Annex C at the pinned commit `fe83fdd`, the
/// same source and the same commit as the four core tables above. **GDAL emits
/// this one identically**, which is worth recording rather than assuming: the
/// four core tables are where `ogr2ogr` deviates, and this is a case where
/// checking the standard confirmed the producer instead of correcting it. The
/// annex's inconsistent spacing — `gpkg_contents(table_name)` against
/// `gpkg_spatial_ref_sys (srs_id)` — is preserved, because it is what the
/// standard says.
const TILE_MATRIX_SET_DDL: &str = "CREATE TABLE gpkg_tile_matrix_set (table_name TEXT NOT NULL PRIMARY KEY, srs_id INTEGER NOT NULL, min_x DOUBLE NOT NULL, min_y DOUBLE NOT NULL, max_x DOUBLE NOT NULL, max_y DOUBLE NOT NULL, CONSTRAINT fk_gtms_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name), CONSTRAINT fk_gtms_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";

/// `gpkg_tile_matrix` — one row per zoom level of a pyramid, defining the
/// tiling scheme. Same source and commit as [`TILE_MATRIX_SET_DDL`].
const TILE_MATRIX_DDL: &str = "CREATE TABLE gpkg_tile_matrix (table_name TEXT NOT NULL, zoom_level INTEGER NOT NULL, matrix_width INTEGER NOT NULL, matrix_height INTEGER NOT NULL, tile_width INTEGER NOT NULL, tile_height INTEGER NOT NULL, pixel_x_size DOUBLE NOT NULL, pixel_y_size DOUBLE NOT NULL, CONSTRAINT pk_ttm PRIMARY KEY (table_name, zoom_level), CONSTRAINT fk_tmm_table_name FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name))";

/// `gpkg_extensions` — one row per extension in use.
const EXTENSIONS_DDL: &str = "CREATE TABLE gpkg_extensions (table_name TEXT, column_name TEXT, extension_name TEXT NOT NULL, definition TEXT NOT NULL, scope TEXT NOT NULL, CONSTRAINT ge_tce UNIQUE (table_name, column_name, extension_name))";

/// `gpkg_metadata` — the Metadata extension's document table.
///
/// Transcribed from OGC 12-128r19 `spec/core/annexes/extension_metadata.adoc`
/// at the pinned commit `fe83fdd`, the same source and commit as the tile
/// registry tables above.
///
/// **Constructed rather than carried, and Requirement 93 is why**: "A
/// GeoPackage MAY contain a table named `gpkg_metadata`. If present it SHALL be
/// defined per clauses … `gpkg_metadata_sql`." The DDL is normative, so a
/// source's own spelling is not this crate's to reproduce — GDAL 3.13.2 writes
/// `id INTEGER CONSTRAINT m_pk PRIMARY KEY ASC NOT NULL`, which is a rowid
/// alias like the standard's but is not what the clause says. The *rows* are
/// carried; only the table definition is this crate's.
///
/// /Declaring `AUTOINCREMENT` obliges the matching `sqlite_sequence` state, and
/// a source that never declared it has none to carry — see
/// [`metadata_tables`], where the mark is computed rather than carried. A
/// declared `AUTOINCREMENT` with no sequence row is the malformed shape of
/// `handoff.org` § BUGS FOUND entry 15./
const METADATA_DDL: &str = "CREATE TABLE gpkg_metadata (id INTEGER PRIMARY KEY AUTOINCREMENT, md_scope TEXT NOT NULL DEFAULT 'dataset', md_standard_uri TEXT NOT NULL, mime_type TEXT NOT NULL DEFAULT 'text/xml', metadata TEXT NOT NULL DEFAULT '')";

/// `gpkg_metadata_reference` — what each metadata document is *about*.
///
/// Same source and commit as [`METADATA_DDL`]. **The two `crmr_*_fk` foreign
/// keys are the reason constructing beats carrying**: they are what lets
/// `foreign_key_check` see a reference whose `md_file_id` no longer resolves
/// (Requirement 101), and five of this tree's eight fixtures holding the pair
/// declare neither, so a verbatim carry would produce output no oracle can
/// check. /Those five inherit the omission from `gdal_hostile.gpkg`'s
/// hand-built container content; GDAL's own output declares both — measured
/// 2026-08-25, and `tests/fixtures/HOSTILE.org` recorded the opposite until
/// that day./
const METADATA_REFERENCE_DDL: &str = "CREATE TABLE gpkg_metadata_reference (reference_scope TEXT NOT NULL, table_name TEXT, column_name TEXT, row_id_value INTEGER, timestamp DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), md_file_id INTEGER NOT NULL, md_parent_id INTEGER, CONSTRAINT crmr_mfi_fk FOREIGN KEY (md_file_id) REFERENCES gpkg_metadata(id), CONSTRAINT crmr_mpi_fk FOREIGN KEY (md_parent_id) REFERENCES gpkg_metadata(id))";

/// The `gpkg_extensions` rows Requirement 140 obliges for the Metadata
/// extension, with the values the annex's *Extension Table Records* fixes:
/// both rows under `extension_name` `gpkg_metadata`, `column_name` NULL, scope
/// `read-write`.
///
/// The `definition` cell is the one the annex leaves to the implementation —
/// "a hyperlink that describes the current implementation of this extension" —
/// and names `http://www.geopackage.org/spec121/#extension_metadata` as the
/// permalink pattern for 1.2.1. That is the version whose text this crate's
/// DDL is transcribed from, so it is the link written here. /GDAL writes the
/// `spec120` form./
const METADATA_EXTENSION_DEFINITION: &str =
    "http://www.geopackage.org/spec121/#extension_metadata";

/// The `definition` permalink for the Schema extension's registrations, from
/// its annex's note — same pattern and same version as
/// [`METADATA_EXTENSION_DEFINITION`], because the two annexes were read at the
/// same pinned commit and this crate's DDL for both is transcribed from it.
const SCHEMA_EXTENSION_DEFINITION: &str = "http://www.geopackage.org/spec121/#extension_schema";

const WGS84_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AUTHORITY[\"EPSG\",\"4326\"]]";

/// Upsert `new_layers` into an existing GeoPackage (or a fresh one if `existing`
/// is `None`), returning the complete file. Layers of the same name are
/// replaced. With `rtree`, every layer gets a SQLite R*Tree spatial index (the
/// opt-in GeoPackage RTree extension). With `envelope`, every geometry blob
/// carries a cached GPB envelope (see `gpb_envelope`); without it the blobs
/// are byte-identical to what this writer has always produced.
pub fn write_layers(
    existing: Option<&[u8]>,
    new_layers: &[(String, FeatureCollection)],
    carried: &Carried,
    rtree: bool,
    envelope: bool,
) -> Result<Vec<u8>> {
    build(&compose_layers(existing, new_layers)?, carried, rtree, envelope)
}

/// The layer set the output will actually hold: the destination's, with
/// `new_layers` upserted over it by name.
///
/// **Extracted so that nothing else has to reproduce it.** A caller that wants
/// to know what a write *will* do — [`metadata_drop_warnings`] is the one that
/// does — has to reason about the same layer set the write reasons about, and
/// an append makes those two different: the destination supplies layers the
/// caller never passed. /Reproducing the upsert at the call site is how the
/// metadata warning shipped claiming "3 of 3 metadata references could not be
/// carried" about three references the same run then wrote./
fn compose_layers(
    existing: Option<&[u8]>,
    new_layers: &[(String, FeatureCollection)],
) -> Result<Vec<(String, FeatureCollection)>> {
    let mut layers: Vec<(String, FeatureCollection)> = match existing {
        Some(bytes) => read_layers(bytes)?,
        None => Vec::new(),
    };
    for (name, fc) in new_layers {
        layers.retain(|(n, _)| n != name);
        layers.push((name.clone(), fc.clone()));
    }
    Ok(layers)
}

fn build(
    layers: &[(String, FeatureCollection)],
    carried: &Carried,
    rtree: bool,
    envelope: bool,
) -> Result<Vec<u8>> {
    // A carried table cannot share a name with a layer: one of the two would
    // have to win, and `gpkg_contents.table_name` is a primary key, so writing
    // both produces a database SQLite reports as malformed. Refused rather than
    // resolved, on D5's grounds in `plans/container-reconciliation.org` — a
    // silent wrong answer here passes `integrity_check` in some shapes and
    // fails it in others, and neither is something to guess at.
    let carried_names = carried
        .pyramids
        .iter()
        .map(|p| ("tile pyramid", &p.name))
        .chain(carried.tables.iter().map(|t| ("table", &t.name)));
    for (kind, name) in carried_names {
        if layers.iter().any(|(layer, _)| layer == name) {
            return Err(Error::Convert(format!(
                "gpkg: the source carries a {kind} named \"{name}\" and a feature layer of \
                 the same name; refusing to write a container where one would overwrite the \
                 other"
            )));
        }
    }

    // Resolve every layer's srs_id up front (registering any non-default SRS),
    // so gpkg_contents, gpkg_geometry_columns, and each geometry blob all agree
    // — and, since 2026-08-24, so does every *carried* object that references
    // one. See `resolve_srs`.
    let srs = resolve_srs(layers, carried);

    let mut specs = vec![
        spatial_ref_sys(&srs.registrations),
        gpkg_contents(layers, &srs.per_layer, carried, &srs.per_pyramid, &srs.per_table)?,
        geometry_columns(layers, &srs.per_layer),
    ];
    // The two tile registry tables exist to describe pyramids, so they are
    // emitted for pyramids and not merely for "something was carried" — a
    // container carrying only a user table must not gain two empty registry
    // tables it never had.
    if !carried.pyramids.is_empty() {
        specs.push(tile_matrix_set(carried, &srs.per_pyramid));
        specs.push(tile_matrix(carried));
    }
    // A GeoPackage has one `gpkg_extensions` table regardless of how many
    // extensions are in use, so every extension contributes rows to the same
    // combined table rather than each pushing its own `TableSpec`.
    let mut extension_rows: Vec<ExtensionRow> = Vec::new();
    if rtree {
        let names: Vec<String> = layers.iter().map(|(n, _)| n.clone()).collect();
        extension_rows.extend(rtree::extension_rows(&names));
    }
    if carried.metadata.is_some() {
        extension_rows.extend(metadata_extension_rows());
    }
    // The Schema extension's tables, replayed from the layers' natives. Written
    // only when some layer carried rows: an empty `gpkg_data_columns` is not
    // something a producer would have left behind, and D3 says a spoke that
    // recovered nothing writes nothing.
    //
    // Built *before* `gpkg_extensions` is pushed because it decides what goes
    // in it — Requirement 141 registers the tables that exist — but pushed
    // after, which is the position it has always had.
    let schema_tables = schema_extension_tables(layers)?;
    extension_rows.extend(schema_extension_rows(&schema_tables));
    if !extension_rows.is_empty() {
        specs.push(extensions_table(extension_rows));
    }
    specs.extend(schema_tables);
    // Computed once per layer and shared: the feature table's rowids and the
    // R*Tree's leaf entries have to be the same numbers or the index points at
    // rows that do not exist.
    let ids: Vec<Vec<i64>> = layers.iter().map(|(_, fc)| feature_ids(fc)).collect();
    // The Metadata extension's pair, after `ids` because a `row`-scoped
    // reference is checked against the very numbers the feature tables are
    // about to be written with. What is dropped here is reported by
    // `super::metadata_drop_warnings`, which runs the same rule.
    if let Some(metadata) = &carried.metadata {
        let (references, _dropped) = retained_references(metadata, layers, &ids, carried);
        specs.extend(metadata_tables(metadata, &references));
    }
    for (((name, fc), &srs_id), ids) in layers.iter().zip(&srs.per_layer).zip(&ids) {
        specs.push(feature_table(name, fc, srs_id, envelope, ids));
    }

    // The spatial indexes (shadow tables) and their virtual-table + trigger
    // schema entries, if requested. Shadow tables are appended after the
    // feature tables so their b-trees are built alongside the rest.
    let mut master: Vec<MasterEntry> = Vec::new();
    if rtree {
        for ((name, fc), ids) in layers.iter().zip(&ids) {
            let index = rtree::build(name, fc, ids);
            specs.extend(index.tables);
            master.extend(index.master);
        }
    }

    // Carried tables last, so their b-trees are built after everything this
    // writer constructs — the order has no meaning to SQLite, and keeping the
    // constructed tables in the positions they have always had keeps this
    // crate's own output byte-identical for a file that carries nothing.
    for pyramid in &carried.pyramids {
        specs.push(tile_table(pyramid));
        master.extend(triggers(&pyramid.name, &pyramid.triggers));
    }
    for table in &carried.tables {
        match &table.storage {
            Storage::Rows(_) => specs.push(user_table(table)),
            // A `WITHOUT ROWID` table *is* an index b-tree, so it is written as
            // one — `write_database` builds a real rootpage from any
            // `MasterEntry` carrying records and never asks what kind of entry
            // it is. It takes no `TableSpec` because it has no rows to key by
            // rowid, no rowid alias to blank, and (SQLite forbids it) no
            // `AUTOINCREMENT` to book-keep.
            Storage::Records(records) => master.push(MasterEntry {
                kind: "table".into(),
                name: table.name.clone(),
                tbl_name: table.name.clone(),
                sql: table.sql.clone(),
                records: Some(records.clone()),
            }),
        }
        master.extend(triggers(&table.name, &table.triggers));
        // An explicit `CREATE INDEX` owns a b-tree, so unlike a trigger it
        // supplies records — re-packed from the source, never compared. See
        // `super::carry::UserTable::autoindexes`.
        master.extend(table.indexes.iter().map(|(name, sql, records)| MasterEntry {
            kind: "index".into(),
            name: name.clone(),
            tbl_name: table.name.clone(),
            sql: sql.clone(),
            records: Some(records.clone()),
        }));
    }

    write_database(&specs, &master, APPLICATION_ID, USER_VERSION)
}

/// One `gpkg_extensions` registration row.
///
/// A GeoPackage has one `gpkg_extensions` table however many extensions are in
/// use, so each contributes rows of this shape and [`build`] combines them into
/// a single table. Three contribute today: the R\*Tree under `--rtree`
/// ([`rtree::extension_rows`]), the Metadata extension when a pair is carried
/// ([`metadata_extension_rows`], Requirement 140), and the Schema extension
/// whenever a layer's column declarations are written
/// ([`schema_extension_rows`], Requirement 141).
pub(crate) struct ExtensionRow {
    pub(crate) table_name: String,
    /// NULL for a *table*-scoped registration, which is what the Metadata
    /// extension's two rows are (Requirement 140's record table writes them
    /// null, and GDAL stores NULL — measured 2026-08-25). An empty string
    /// would be a different value: `ge_tce UNIQUE (table_name, column_name,
    /// extension_name)` distinguishes them, and so does every consumer.
    pub(crate) column_name: Option<String>,
    pub(crate) extension_name: String,
    pub(crate) definition: String,
    pub(crate) scope: String,
}

/// This layer's GeoPackage payload, if it carried one. `None` for every
/// collection that came from another format — the tag check in
/// [`crate::Native::payload_for`] is what makes a cross-format run drop it
/// rather than replay someone else's members.
fn gpkg_native(fc: &FeatureCollection) -> Option<&GpkgNative> {
    match fc.native.as_ref()?.payload_for(Format::Gpkg)? {
        NativePayload::Gpkg(native) => Some(native),
        // The tag matched, so this is unreachable unless a second GeoPackage
        // payload variant is added without updating this arm.
        _ => None,
    }
}

/// `gpkg_data_columns` and `gpkg_data_column_constraints`, rebuilt from every
/// layer's payload. Empty when no layer carried either.
///
/// Constraints are carried *per layer* (a layer that names one keeps its own
/// copy) but the table is container-scoped, so identical rows shared by two
/// layers are de-duplicated here on the way out. Rows describing a column the
/// layer no longer has are dropped: this is the same "an opaque member can
/// also be derived" ruling decision 14 made for `ARROW:schema`, since a
/// dangling `gpkg_data_columns` row is a description of nothing and GDAL
/// surfaces it as an alias on a field that is not there.
fn schema_extension_tables(layers: &[(String, FeatureCollection)]) -> Result<Vec<TableSpec>> {
    let mut columns: Vec<(String, GpkgDataColumn)> = Vec::new();
    let mut constraints: Vec<GpkgConstraint> = Vec::new();
    for (name, fc) in layers {
        let Some(native) = gpkg_native(fc) else { continue };
        let present = columns_for(fc);
        for column in &native.data_columns {
            if present.iter().any(|c| c.name == column.column_name) {
                columns.push((name.clone(), column.clone()));
            }
        }
        for constraint in &native.constraints {
            match constraints.iter().find(|held| same_constraint(held, constraint)) {
                // The same constraint, already held: one row. Identity is § 6's
                // rather than the whole row's, which is the entire point — see
                // `same_constraint`.
                Some(held) if held == constraint => {}
                // The same constraint saying something different. No union
                // exists: a column referring to this name would get bounds
                // neither source declared, so this is
                // `plans/container-reconciliation.org` § 5's refusal.
                //
                // /`ogr2ogr -update -append` resolves it silently in the
                // destination's favour (measured 2026-10-13), which leaves the
                // appended layer's domain quietly replaced. § THE ORACLE RULE:
                // that is evidence of what a producer does, not of what is
                // right./
                Some(_) => {
                    return Err(Error::Convert(format!(
                        "gpkg: the output would carry two constraints named \"{}\" of type \
                         \"{}\" that define different things; refusing to write a \
                         gpkg_data_column_constraints the declared gdcc_ntv key cannot \
                         separate (plans/container-reconciliation.org § 5)",
                        constraint.constraint_name, constraint.constraint_type
                    )));
                }
                None => constraints.push(constraint.clone()),
            }
        }
    }
    // A constraint whose every referring column was dropped above describes
    // nothing, and would be a row GDAL reports as an unused domain.
    constraints.retain(|constraint| {
        columns.iter().any(|(_, column)| {
            column.constraint_name.as_deref() == Some(constraint.constraint_name.as_str())
        })
    });
    if columns.is_empty() && constraints.is_empty() {
        return Ok(Vec::new());
    }

    let data_column_rows: Vec<(i64, Vec<Value>)> = columns
            .into_iter()
            .enumerate()
            .map(|(i, (table, column))| {
                (
                    (i + 1) as i64,
                    vec![
                        Value::Text(table),
                        Value::Text(column.column_name),
                        text_or_null(column.name),
                        text_or_null(column.title),
                        text_or_null(column.description),
                        text_or_null(column.mime_type),
                        text_or_null(column.constraint_name),
                    ],
                )
            })
            .collect();
    let mut specs = vec![TableSpec {
        name: "gpkg_data_columns".into(),
        sql: DATA_COLUMNS_DDL.into(),
        // `pk_gdc` over (table_name, column_name), then `gdc_tn` over
        // (table_name, name) — `name` is nullable, and SQLite sorts NULLs
        // first, which `compare_values` handles.
        autoindexes: vec![
            autoindex_records(&data_column_rows, &[0, 1]),
            autoindex_records(&data_column_rows, &[0, 2]),
        ],
        rows: data_column_rows,
        autoincrement: None,
    }];
    if !constraints.is_empty() {
        specs.push(TableSpec {
            name: "gpkg_data_column_constraints".into(),
            sql: DATA_COLUMN_CONSTRAINTS_DDL.into(),
            rows: constraints
                .into_iter()
                .enumerate()
                .map(|(i, c)| {
                    (
                        (i + 1) as i64,
                        vec![
                            Value::Text(c.constraint_name),
                            Value::Text(c.constraint_type),
                            text_or_null(c.value),
                            number_or_null(c.min),
                            bool_or_null(c.min_is_inclusive),
                            number_or_null(c.max),
                            bool_or_null(c.max_is_inclusive),
                            text_or_null(c.description),
                        ],
                    )
                })
                .collect(),
            autoindexes: Vec::new(),
            autoincrement: None,
        });
        // `gdcc_ntv UNIQUE (constraint_name, constraint_type, value)`. Built
        // from the rows just pushed, since `constraints` was consumed above.
        let rows = &specs[specs.len() - 1].rows;
        let records = autoindex_records(rows, &[0, 1, 2]);
        specs.last_mut().expect("just pushed").autoindexes = vec![records];
    }
    Ok(specs)
}

/// Whether two `gpkg_data_column_constraints` rows describe the *same*
/// constraint, per `plans/container-reconciliation.org` § 6.
///
/// ***This is the one table whose identity is type-dependent, and saying so is
/// the honest resolution of § 3's NULL trap rather than a workaround for it.***
///
/// | `constraint_type` | identity |
/// |---|---|
/// | `enum`, `glob` | `(constraint_name, constraint_type, value)` |
/// | `range` | `(constraint_name, constraint_type)` |
///
/// A `range` row's `value` is NULL *by definition* — the column is for `enum`
/// and `glob` — and the standard admits one range row per constraint name, so
/// it carries no information to key on. The trap and the contract are the same
/// fact seen twice: `gdcc_ntv UNIQUE (constraint_name, constraint_type, value)`
/// includes `value` because it must cover `enum`, where the value is the row's
/// whole point, and a `range` row has none — so **the index cannot separate two
/// of them and identity must not try to**.
///
/// /Measured 2026-10-13, GDAL 3.13.3 and SQLite 3.51.0: two `range` rows with
/// the same `constraint_name` and different bounds coexist under `gdcc_ntv`,
/// because SQLite treats NULLs as distinct under `UNIQUE`, and
/// `PRAGMA integrity_check` reports `ok`. Deduping on the declared key, or
/// assuming the index makes a set, is wrong./
///
/// # Why an unrecognised type gets the three-column key
///
/// Requirement 104 defines the three, so a fourth is the source's
/// non-conformance and this crate does not interpret a constraint. Keying it on
/// `value` as well is the conservative half of the choice: it can only ever
/// *fail* to merge two rows, where dropping `value` could merge two rows whose
/// values mean something to a reader that understands the type.
fn same_constraint(a: &GpkgConstraint, b: &GpkgConstraint) -> bool {
    a.constraint_name == b.constraint_name
        && a.constraint_type == b.constraint_type
        && (a.constraint_type == "range" || a.value == b.value)
}

/// An absent optional member is a NULL cell, not an empty string: the reader
/// distinguishes them and so must the writer.
fn text_or_null(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::Text)
}

fn number_or_null(value: Option<f64>) -> Value {
    value.map_or(Value::Null, Value::Real)
}

/// GeoPackage `BOOLEAN` is `INTEGER` 0/1 — the same storage M4's declared-type
/// work reads back as a boolean.
fn bool_or_null(value: Option<bool>) -> Value {
    value.map_or(Value::Null, |b| Value::Int(b.into()))
}

fn extensions_table(rows: Vec<ExtensionRow>) -> TableSpec {
    let rows: Vec<(i64, Vec<Value>)> = rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| {
                (
                    (i + 1) as i64,
                    vec![
                        Value::Text(r.table_name),
                        r.column_name.map_or(Value::Null, Value::Text),
                        Value::Text(r.extension_name),
                        Value::Text(r.definition),
                        Value::Text(r.scope),
                    ],
                )
            })
            .collect();
    TableSpec {
        name: "gpkg_extensions".into(),
        sql: EXTENSIONS_DDL.into(),
        // `ge_tce UNIQUE (table_name, column_name, extension_name)`.
        autoindexes: vec![autoindex_records(&rows, &[0, 1, 2])],
        autoincrement: None,
        rows,
    }
}

/// One carried metadata reference that does not reach the output, and why.
///
/// Kept as data rather than formatted on the spot so the same computation
/// serves both the write and `metadata_drop_warnings` — a rule
/// evaluated in two places is a rule that drifts, and this one decides what is
/// destroyed.
pub(crate) struct DroppedReference {
    pub(crate) scope: String,
    pub(crate) table_name: Option<String>,
    pub(crate) reason: &'static str,
}

/// Which carried `gpkg_metadata_reference` rows reach the output, and why the
/// rest do not.
///
/// **This is the layer-coupled rule, and it is the whole difference between
/// this member and everything else `carry.rs` holds.** A pyramid or a user
/// table is valid whatever the write produces; a reference names a table, a
/// column and a rowid, so it is valid only against *this* write. Decision 23
/// forbids tracking that — `FeatureCollection::features` is a public field a
/// consumer mutates directly — so it is computed here, at write, by comparing
/// each reference with what is actually going into the file.
///
/// # What each scope is checked against
///
/// | scope | check | requirement |
/// |---|---|---|
/// | `geopackage` | nothing; the three locating columns are forced NULL | 97, 98, 99 |
/// | `table` | `table_name` is registered in the output's `gpkg_contents` | 97 |
/// | `column` | that, and the column exists in what is written | 97, 98 |
/// | `row` | that, and the rowid exists in what is written | 97, 99 |
/// | `row/col` | all of the above | 97, 98, 99 |
///
/// ***The authority for `table_name` is `gpkg_contents`, not the layer set.***
/// Requirement 97 says the value "references a value in the `gpkg_contents`
/// `table_name` column", and since 2026-08-24 this crate registers *carried
/// tile pyramids* there too. Checking against the layers alone would drop
/// `gdal_tiles.gpkg`'s reference to `imagery` from a file that still contains
/// `imagery` — a valid reference destroyed by a check that was stricter than
/// the standard.
///
/// # Why a reference into carried content is kept without inspecting it
///
/// A pyramid is carried **verbatim** — its DDL, its rows and its rowids — so
/// the table the reference identifies is, column for column and rowid for
/// rowid, the table the source had. The reference is therefore exactly as
/// valid in the output as it was in the source. Where the source itself was
/// wrong, that is a dangling reference this crate did not create and does not
/// repair, on the same terms as [`super::carry::Pyramid::srs`].
///
/// ***That premise has one exception, and it is handled upstream rather than
/// here.*** A **merged** pyramid's rowids are not the source's: step 5 keeps
/// the destination's and reissues the incoming side's
/// (`plans/container-reconciliation.org`). So
/// [`super::reconcile::merge_metadata`] remaps every incoming `row_id_value`
/// through that reissue *before* this function sees it — which is why the rule
/// below still holds unchanged, and why the remap could not be deferred to
/// here: by this point there is one merged pyramid and no record of which side
/// a reference arrived from.
///
/// A written *layer* is the opposite case: its columns come from
/// [`crate::schema::columns_for`] and its rowids from [`feature_ids`], and
/// neither need match what the source held.
pub(crate) fn retained_references(
    metadata: &Metadata,
    layers: &[(String, FeatureCollection)],
    ids: &[Vec<i64>],
    carried: &Carried,
) -> (Vec<MetadataReference>, Vec<DroppedReference>) {
    let record_ids: Vec<i64> = metadata.records.iter().map(|r| r.id).collect();
    let mut kept = Vec::new();
    let mut dropped = Vec::new();

    for reference in &metadata.references {
        let mut r = reference.clone();
        let drop = |dropped: &mut Vec<DroppedReference>, reason| {
            dropped.push(DroppedReference {
                scope: reference.scope.clone(),
                table_name: reference.table_name.clone(),
                reason,
            });
        };

        // Requirement 96: five values, lowercase. Anything else names no scope
        // this can check, so there is no way to carry it correctly.
        if !matches!(r.scope.as_str(), "geopackage" | "table" | "column" | "row" | "row/col") {
            drop(&mut dropped, "its reference_scope is not one the standard defines");
            continue;
        }
        // Requirement 101. Kept ahead of the scope work because a reference to
        // no document is not a reference, whatever it points at.
        if !record_ids.contains(&r.md_file_id) {
            drop(&mut dropped, "its md_file_id names no gpkg_metadata row");
            continue;
        }
        // Requirement 102: a parent that is not a record, or is this row's own
        // `md_file_id`, is normalised to NULL rather than costing the whole
        // reference. NULL is the standard's own spelling for "roots a
        // hierarchy", so the result is conformant and loses only the part that
        // was invalid — the reference still says which document is about what.
        if let Some(parent) = r.md_parent_id
            && (!record_ids.contains(&parent) || parent == r.md_file_id)
        {
            r.md_parent_id = None;
        }

        // Requirements 97, 98 and 99 each say which columns SHALL be NULL for
        // which scopes. Normalised rather than propagated: a stray value in a
        // column the standard requires empty is the source's non-conformance,
        // and copying it forward would make it this crate's output's.
        let (wants_column, wants_row) = match r.scope.as_str() {
            "geopackage" => {
                r.table_name = None;
                r.column_name = None;
                r.row_id_value = None;
                kept.push(r);
                continue;
            }
            "table" => (false, false),
            "column" => (true, false),
            "row" => (false, true),
            _ => (true, true),
        };
        if !wants_column {
            r.column_name = None;
        }
        if !wants_row {
            r.row_id_value = None;
        }

        // Requirement 97 for every non-`geopackage` scope.
        let Some(table) = r.table_name.clone() else {
            drop(&mut dropped, "it names no table");
            continue;
        };
        let layer = layers.iter().position(|(name, _)| *name == table);
        // Requirement 97's authority is the output's `gpkg_contents`, not its
        // layer set — so every kind of entry that table gains has to appear
        // here. A carried table qualifies exactly when it brought a
        // registration: an unregistered one (`side_notes` and its kind) is in
        // the file but in no registry, so a reference naming it would dangle
        // under a requirement that says it SHALL NOT.
        let registered = carried.pyramids.iter().any(|p| p.name == table)
            || carried
                .tables
                .iter()
                .any(|t| t.name == table && t.registration.is_some());
        if layer.is_none() && !registered {
            drop(&mut dropped, "the table it names is not in the output");
            continue;
        }
        if wants_column && r.column_name.is_none() {
            drop(&mut dropped, "its scope requires a column_name and it has none");
            continue;
        }
        if wants_row && r.row_id_value.is_none() {
            drop(&mut dropped, "its scope requires a row_id_value and it has none");
            continue;
        }
        // Carried content is the source's table unchanged, so there is nothing
        // further to check — see this function's doc comment.
        if let Some(i) = layer {
            let (_, fc) = &layers[i];
            if let Some(column) = &r.column_name {
                // `fid` and `geom` are written by `feature_table` ahead of the
                // properties, so they are columns of the output even though no
                // property is named for them.
                let known = column == "fid"
                    || column == "geom"
                    || crate::schema::columns_for(fc).iter().any(|c| c.name == *column);
                if !known {
                    drop(&mut dropped, "the column it names is not in the output");
                    continue;
                }
            }
            if let Some(rowid) = r.row_id_value
                && !ids.get(i).is_some_and(|ids| ids.contains(&rowid))
            {
                drop(&mut dropped, "the feature it names is not in the output");
                continue;
            }
        }
        kept.push(r);
    }
    (kept, dropped)
}

/// What a carried metadata pair loses to *this* write, as warnings.
///
/// ***The first content in this crate that can be carried in **part**.*** The
/// container survey's vocabulary is carried-or-destroyed: a table either comes
/// back or it does not. A reference is finer than that — three may survive and
/// one be dropped because the layer it named was not selected — and neither
/// existing precedent covers the case. `gpkg_ogr_contents` is reported
/// silently because it is *derived* and nothing is lost; the R\*Tree earns a
/// message because `--rtree` is a **remedy**. A dropped reference has no
/// remedy and does lose information, so it is reported and the reason is
/// named: the drop follows from a choice the caller made, which is what makes
/// the message worth reading.
///
/// Recomputes `feature_ids` rather than being handed them, so that one rule —
/// `retained_references` — decides both what is written and what is
/// reported. `feature_ids` is a pure function of the collection, so the two
/// calls cannot disagree.
pub fn metadata_drop_warnings(
    existing: Option<&[u8]>,
    new_layers: &[(String, FeatureCollection)],
    carried: &Carried,
) -> Vec<String> {
    let Some(metadata) = &carried.metadata else {
        return Vec::new();
    };
    // The *output's* layer set, not the caller's — see `compose_layers`. On an
    // append the destination supplies layers this run never passed, and a
    // reference naming one of them survives.
    let Ok(layers) = compose_layers(existing, new_layers) else {
        // An unreadable destination is the write's error to report, not this
        // warning's to guess at.
        return Vec::new();
    };
    let layers = &layers[..];
    let ids: Vec<Vec<i64>> = layers.iter().map(|(_, fc)| feature_ids(fc)).collect();
    let (kept, dropped) = retained_references(metadata, layers, &ids, carried);
    if dropped.is_empty() {
        return Vec::new();
    }
    let mut named: Vec<String> = dropped
        .iter()
        .take(MAX_NAMED_REFERENCES)
        .map(|d| match &d.table_name {
            Some(table) => format!("{} scope on \"{table}\" ({})", d.scope, d.reason),
            None => format!("{} scope ({})", d.scope, d.reason),
        })
        .collect();
    if dropped.len() > MAX_NAMED_REFERENCES {
        named.push(format!("and {} more", dropped.len() - MAX_NAMED_REFERENCES));
    }
    vec![format!(
        "the source's gpkg_metadata records are carried, but {} of {} metadata \
         reference{} could not be: {}. The documents themselves are kept — a \
         gpkg_metadata row nothing points at is still conformant — so what is lost is \
         what each dropped reference said the document was about. \
         (plans/same-format-fidelity.org, § The metadata pair.)",
        dropped.len(),
        dropped.len() + kept.len(),
        if dropped.len() + kept.len() == 1 { "" } else { "s" },
        named.join(", ")
    )]
}

/// How many dropped references are named individually before the message
/// switches to a count, on the same reasoning as `super::container`'s
/// `MAX_NAMED`: past a handful the list stops informing and starts scrolling.
const MAX_NAMED_REFERENCES: usize = 4;

/// The Metadata extension's two tables, built from carried rows.
///
/// Both are emitted whenever either would be, because Requirement 95 makes
/// them a pair — including the case where every reference was dropped, which
/// yields `gpkg_metadata_reference` with no rows rather than no table.
///
/// Neither carries an index: `gpkg_metadata`'s only key is its rowid alias and
/// `gpkg_metadata_reference` declares none at all, which is measured across
/// every fixture in this tree and is why nothing here re-packs anything.
fn metadata_tables(metadata: &Metadata, references: &[MetadataReference]) -> [TableSpec; 2] {
    let records: Vec<(i64, Vec<Value>)> = metadata
        .records
        .iter()
        .map(|record| {
            let mut values = vec![Value::Null]; // `id`, carried by the rowid.
            values.extend(record.values.iter().cloned());
            (record.id, values)
        })
        .collect();
    // The DDL this crate writes declares `AUTOINCREMENT`, so the matching
    // `sqlite_sequence` state is not optional — see `METADATA_DDL`. It is
    // *computed* rather than carried: a GDAL source declared no AUTOINCREMENT
    // and so has no mark to carry, and the high-water mark of the rows being
    // written is the honest value for a table this writer is constructing.
    let autoincrement = Some(match records.iter().map(|(id, _)| *id).max() {
        Some(high) => Autoincrement::HighWater(high),
        None => Autoincrement::Declared,
    });
    let reference_rows: Vec<(i64, Vec<Value>)> = references
        .iter()
        .enumerate()
        .map(|(i, r)| {
            (
                (i + 1) as i64,
                vec![
                    Value::Text(r.scope.clone()),
                    r.table_name.clone().map_or(Value::Null, Value::Text),
                    r.column_name.clone().map_or(Value::Null, Value::Text),
                    r.row_id_value.map_or(Value::Null, Value::Int),
                    r.timestamp.clone(),
                    Value::Int(r.md_file_id),
                    r.md_parent_id.map_or(Value::Null, Value::Int),
                ],
            )
        })
        .collect();
    [
        TableSpec {
            name: "gpkg_metadata".into(),
            sql: METADATA_DDL.into(),
            autoindexes: Vec::new(),
            autoincrement,
            rows: records,
        },
        TableSpec {
            name: "gpkg_metadata_reference".into(),
            sql: METADATA_REFERENCE_DDL.into(),
            autoindexes: Vec::new(),
            autoincrement: None,
            rows: reference_rows,
        },
    ]
}

/// The `gpkg_extensions` rows Requirement 141 obliges for the Schema
/// extension — one per Schema table actually written.
///
/// ***Registering an extension's tables is not bookkeeping; Requirement 59
/// makes it the thing that says what the file is.*** "In an Extended
/// GeoPackage, every extension SHALL be registered in a corresponding row in
/// the `gpkg_extensions` table … the absence of rows in the `gpkg_extensions`
/// table SHALL indicate that the file is a GeoPackage (as opposed to an
/// Extended GeoPackage)." This crate has written `gpkg_data_columns` since
/// 2026-08-21 and registered nothing, so its output declared itself a plain
/// GeoPackage while carrying an extension's tables. **Defect 6's shape
/// exactly**: output that does not state what the standard says it states.
///
/// # Why this registers what is written rather than the annex's two rows
///
/// Requirement 141's record table lists `gpkg_data_columns` *and*
/// `gpkg_data_column_constraints`, and `schema_extension_tables` writes the
/// second only when some layer carried a constraint. Registering an absent
/// table would point at nothing: Requirement 60 lets `table_name` "reference
/// new tables required by that extension", and a table the file does not
/// contain is not required by anything.
///
/// **The Schema annex permits that, and the Metadata annex would not**, which
/// is the distinction worth keeping. Requirements 103 and 107 each say a
/// GeoPackage *MAY* contain their table, independently — there is no analogue
/// of Requirement 95, which makes `gpkg_metadata` and
/// `gpkg_metadata_reference` a pair that travels together or not at all. So
/// `gpkg_data_columns` alone is a conformant file and one registration is the
/// honest description of it.
fn schema_extension_rows(tables: &[TableSpec]) -> Vec<ExtensionRow> {
    tables
        .iter()
        .map(|spec| ExtensionRow {
            table_name: spec.name.clone(),
            // NULL: the registration is of the table, not of a column in it
            // (Requirement 61 permits NULL, and the annex's record table
            // writes it).
            column_name: None,
            extension_name: "gpkg_schema".into(),
            definition: SCHEMA_EXTENSION_DEFINITION.into(),
            scope: "read-write".into(),
        })
        .collect()
}

/// The two `gpkg_extensions` rows Requirement 140 obliges when the pair is
/// written.
fn metadata_extension_rows() -> Vec<ExtensionRow> {
    ["gpkg_metadata", "gpkg_metadata_reference"]
        .into_iter()
        .map(|table_name| ExtensionRow {
            table_name: table_name.into(),
            // NULL, per the annex's record table — a table-scoped
            // registration names no column. This is the first such row this
            // crate writes, and why `ExtensionRow::column_name` is an
            // `Option`.
            column_name: None,
            extension_name: "gpkg_metadata".into(),
            definition: METADATA_EXTENSION_DEFINITION.into(),
            scope: "read-write".into(),
        })
        .collect()
}

/// A `gpkg_spatial_ref_sys` row for a CRS beyond the three mandatory defaults.
struct SrsReg {
    srs_id: i64,
    name: String,
    organization: String,
    organization_coordsys_id: i64,
    definition: String,
}

/// The srs_id chosen for each layer (aligned with `layers`) and for each
/// carried pyramid (aligned with `carried.pyramids`), plus the extra
/// `gpkg_spatial_ref_sys` rows those choices require.
struct ResolvedSrs {
    per_layer: Vec<i64>,
    /// **The output's** srs_id for each carried object, which is not the
    /// source's — see [`reconcile_carried_srs`].
    per_pyramid: Vec<i64>,
    /// The same, for each entry of `carried.tables` — `None` where the table
    /// carries no [`Registration`] (so no `gpkg_contents` row is written for
    /// it and the value is never read) *or* where its registration left
    /// `srs_id` NULL, which Requirement 13 permits. The two cases are not
    /// distinguished because nothing downstream distinguishes them: both write
    /// no srs_id.
    per_table: Vec<Option<i64>>,
    registrations: Vec<SrsReg>,
}

/// Map every layer's [`Crs`] to a GeoPackage srs_id, registering a
/// `gpkg_spatial_ref_sys` row for any non-default system. Geosetta never
/// reprojects, so this only *labels* each layer with the CRS it arrived in:
/// `None` → undefined, WGS 84 → 4326, and any other CRS → its authority code
/// (or a synthetic id when it has none), carrying the WKT definition through.
///
/// GeoPackage's `srs_id`/`organization_coordsys_id` are SQLite `INTEGER`
/// columns — genuinely numeric on disk, unlike the IR's string `code` (which
/// also has to hold IGNF/OGC/PROJ/NKG's alphanumeric codes). A code that
/// parses as a positive integer is used directly, same as always; a code that
/// doesn't (non-numeric, e.g. `"LAMB93"`) falls into the same synthetic-id path
/// as "no code at all" — the WKT definition, if any, still carries through, so
/// nothing is silently mislabeled, it just can't be the native id.
fn resolve_srs(layers: &[(String, FeatureCollection)], carried: &Carried) -> ResolvedSrs {
    let mut per_layer = Vec::with_capacity(layers.len());
    let mut registrations: Vec<SrsReg> = Vec::new();
    let mut next_synthetic = SYNTHETIC_SRS_BASE;

    for (_, fc) in layers {
        let (srs_id, reg) = match &fc.crs {
            None => (UNDEFINED_SRS_ID, None),
            Some(Crs::Wgs84) => (WGS84_SRS_ID, None),
            Some(Crs::Named(n)) => {
                let definition =
                    n.wkt.clone().or_else(|| n.structural_wkt()).unwrap_or_else(|| "undefined".into());
                let numeric_code = n.code.as_deref().and_then(|c| c.parse::<i64>().ok());
                match numeric_code {
                    // EPSG:4326 is exactly the default row.
                    Some(4326) => (WGS84_SRS_ID, None),
                    // A usable numeric authority code becomes the srs_id directly.
                    Some(code) if code > 0 => {
                        let organization =
                            n.authority.clone().unwrap_or_else(|| "EPSG".into());
                        (
                            code,
                            Some(SrsReg {
                                srs_id: code,
                                name: format!("{organization}:{code}"),
                                organization,
                                organization_coordsys_id: code,
                                definition,
                            }),
                        )
                    }
                    // No code, or a code GeoPackage's INTEGER srs_id can't
                    // represent: hand out a synthetic id and record whatever
                    // definition we have (WKT, or "undefined").
                    _ => {
                        let srs_id = next_synthetic;
                        next_synthetic += 1;
                        (
                            srs_id,
                            Some(SrsReg {
                                srs_id,
                                name: format!("srs {srs_id}"),
                                organization: n
                                    .authority
                                    .clone()
                                    .unwrap_or_else(|| crate::crs::SYNTHETIC_AUTHORITY.into()),
                                organization_coordsys_id: numeric_code.unwrap_or(srs_id),
                                definition,
                            }),
                        )
                    }
                }
            }
        };
        if let Some(reg) = reg
            && !registrations.iter().any(|r| r.srs_id == reg.srs_id)
        {
            registrations.push(reg);
        }
        per_layer.push(srs_id);
    }
    // Carried content is resolved *after* every layer, so a layer's numbering
    // is exactly what it was before this channel existed and a file carrying
    // nothing is byte-identical.
    let per_pyramid: Vec<i64> = carried
        .pyramids
        .iter()
        .map(|pyramid| {
            reconcile_carried_srs(
                pyramid.srs_id,
                pyramid.srs.as_ref(),
                &mut registrations,
                &mut next_synthetic,
            )
        })
        .collect();
    // Registered carried tables last, so a pyramid's numbering is unchanged by
    // the arrival of this channel — the same rule that puts carried content
    // after every layer.
    let per_table = carried
        .tables
        .iter()
        .map(|table| {
            let registration = table.registration.as_ref()?;
            let srs_id = registration.srs_id?;
            Some(reconcile_carried_srs(
                srs_id,
                registration.srs.as_ref(),
                &mut registrations,
                &mut next_synthetic,
            ))
        })
        .collect();

    ResolvedSrs { per_layer, per_pyramid, per_table, registrations }
}

/// Map a carried object's `srs_id` from the **source's** numbering into the
/// output's, registering its CRS when the output does not already have one.
///
/// This is § MILESTONES M3b requirement 1, and the reason it is needed at all
/// is that [`resolve_srs`] recomputes every id from the layer set on *every*
/// write, with no "existing destination" condition: an authority code becomes
/// the id directly and a codeless CRS is handed a synthetic id **in layer
/// order**. So a carried `gpkg_contents` or `gpkg_tile_matrix_set` row points
/// into a numbering the writer has just rebuilt, and left alone it dangles —
/// past `integrity_check`, which has no opinion, and caught by
/// `PRAGMA foreign_key_check` through `fk_gc_r_srs_id`.
///
/// Three cases, in the order they are tested:
///
/// 1. **The output already has this id, for this same CRS.** Reuse it. This is
///    the ordinary case — a pyramid in EPSG:3857 beside a layer in EPSG:3857,
///    or two pyramids sharing one.
/// 2. **The output already has this id, for a *different* CRS.** The id
///    collides, so the carried object is renumbered onto a fresh synthetic id
///    and its definition registered there. Position-dependent synthetic ids are
///    exactly how this arises: two files both numbering from
///    [`SYNTHETIC_SRS_BASE`] mean the same integer names two different systems.
/// 3. **The output does not have this id.** Register the source's own row under
///    its own id, so the reference stays the number it already was.
///
/// A carried object whose source held *no* definition for its id is mapped to
/// [`UNDEFINED_SRS_ID`] rather than carried through: the reference was already
/// dangling in the source, and reproducing it would put this crate's name on a
/// file that fails `foreign_key_check`. The CRS is not knowable from anything
/// else in the file — a tile is an opaque blob here, and an `attributes`
/// dataset has no geometry at all — so "undefined" is the true statement.
///
/// Called for a [`Pyramid`] and for a carried table's
/// [`super::carry::Registration`], which is why it takes the two fields rather
/// than either struct. **Requirement 16 is what makes it owed in both cases**:
/// a non-NULL `gpkg_contents.srs_id` SHALL reference a `gpkg_spatial_ref_sys`
/// row, and `resolve_srs` rebuilds that numbering from the layer set on every
/// write.
fn reconcile_carried_srs(
    srs_id: i64,
    def: Option<&SrsDef>,
    registrations: &mut Vec<SrsReg>,
    next_synthetic: &mut i64,
) -> i64 {
    // The three rows `spatial_ref_sys` always writes. A carried reference to
    // one of them needs no registration and no renumbering.
    if srs_id == UNDEFINED_SRS_ID || srs_id == -1 {
        return srs_id;
    }
    let Some(def) = def else {
        return UNDEFINED_SRS_ID;
    };
    if srs_id == WGS84_SRS_ID {
        return WGS84_SRS_ID;
    }

    if let Some(existing) = registrations.iter().find(|r| r.srs_id == srs_id) {
        let same = existing.organization == def.organization
            && existing.organization_coordsys_id == def.organization_coordsys_id
            && existing.definition == def.definition;
        if same {
            return srs_id;
        }
        // Case 2: renumber onto an id nothing has taken. Stepping past taken
        // ids matters because a *carried* id may itself sit in the synthetic
        // range, which is the collision this whole function exists for.
        while registrations.iter().any(|r| r.srs_id == *next_synthetic) {
            *next_synthetic += 1;
        }
        let srs_id = *next_synthetic;
        *next_synthetic += 1;
        registrations.push(SrsReg {
            srs_id,
            name: def.srs_name.clone(),
            organization: def.organization.clone(),
            organization_coordsys_id: def.organization_coordsys_id,
            definition: def.definition.clone(),
        });
        return srs_id;
    }

    registrations.push(SrsReg {
        srs_id,
        name: def.srs_name.clone(),
        organization: def.organization.clone(),
        organization_coordsys_id: def.organization_coordsys_id,
        definition: def.definition.clone(),
    });
    srs_id
}

fn spatial_ref_sys(registrations: &[SrsReg]) -> TableSpec {
    // srs_id is INTEGER PRIMARY KEY, so its column value is Null and the rowid
    // carries it; rows must be rowid-ordered.
    let row = |srs_id: i64, name: &str, org: &str, org_id: i64, def: &str, desc: &str| {
        let vals = vec![
            Value::Text(name.into()),
            Value::Null,
            Value::Text(org.into()),
            Value::Int(org_id),
            Value::Text(def.into()),
            Value::Text(desc.into()),
        ];
        (srs_id, vals)
    };
    let mut rows = vec![
        row(-1, "Undefined cartesian SRS", "NONE", -1, "undefined", "undefined cartesian"),
        row(0, "Undefined geographic SRS", "NONE", 0, "undefined", "undefined geographic"),
        row(WGS84_SRS_ID, "WGS 84 geodetic", "EPSG", 4326, WGS84_WKT, "WGS 84"),
    ];
    for reg in registrations {
        rows.push(row(
            reg.srs_id,
            &reg.name,
            &reg.organization,
            reg.organization_coordsys_id,
            &reg.definition,
            "",
        ));
    }
    // Rows must be rowid- (srs_id-) ordered for the b-tree writer.
    rows.sort_by_key(|(srs_id, _)| *srs_id);
    let sql = SPATIAL_REF_SYS_DDL;
    // `srs_id INTEGER NOT NULL PRIMARY KEY` is a rowid alias, so this table
    // owns no index b-tree — which is why it was the only registry table this
    // writer already emitted exactly as specified (defect 6).
    TableSpec { name: "gpkg_spatial_ref_sys".into(), sql: sql.into(), rows, autoindexes: Vec::new(), autoincrement: None }
}

/// `gpkg_contents` — one row per feature layer, then one per carried pyramid,
/// then one per carried table that arrived with a [`super::carry::Registration`].
///
/// The rowid order is layers, pyramids, datasets, and it is positional rather
/// than meaningful: keeping the constructed rows in the positions they have
/// always had is what leaves this crate's output byte-identical for a file
/// that carries nothing.
fn gpkg_contents(
    layers: &[(String, FeatureCollection)],
    per_layer_srs: &[i64],
    carried: &Carried,
    per_pyramid_srs: &[i64],
    per_table: &[Option<i64>],
) -> Result<TableSpec> {
    let rows: Vec<(i64, Vec<Value>)> = layers
        .iter()
        .zip(per_layer_srs)
        .enumerate()
        .map(|(i, ((name, fc), &srs_id))| {
            let (minx, miny, maxx, maxy) = bbox(fc);
            let native = gpkg_native(fc);
            // The layer's own title and description when it recorded any; the
            // layer name and an empty string otherwise, which is what GDAL
            // writes for a layer created without them. An identifier equal to
            // the name is not carried at all (see `GpkgNative`), so this
            // regenerates it and a rename stays consistent.
            let identifier = native
                .and_then(|n| n.identifier.clone())
                .unwrap_or_else(|| name.clone());
            let description =
                native.and_then(|n| n.description.clone()).unwrap_or_default();
            // Carried when the source had one; the sentinel otherwise. Never a
            // clock reading — see `LAST_CHANGE`.
            let last_change = native
                .and_then(|n| n.last_change.clone())
                .unwrap_or_else(|| LAST_CHANGE.to_string());
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("features".into()),
                    Value::Text(identifier),
                    Value::Text(description),
                    Value::Text(last_change),
                    minx,
                    miny,
                    maxx,
                    maxy,
                    Value::Int(srs_id),
                ],
            )
        })
        .collect();
    let mut rows = rows;
    // ***The same Requirement 13 uniqueness the two carried arms below enforce,
    // and the layers had never been checked against each other.*** Found
    // 2026-10-13 while measuring `plans/container-reconciliation.org` § 3: a
    // single source cannot hold two colliding identifiers, because its own
    // `UNIQUE` forbade it — but an **append composes two sources**, and
    // `ogr2ogr -nln` copies the identifier verbatim when it renames a layer, so
    // two files with differently-named layers and the same identifier are what
    // a real producer hands you. The output was a database `integrity_check`
    // reports as `non-unique entry in index sqlite_autoindex_gpkg_contents_2`.
    //
    // /`ogr2ogr -update -append` refuses the same case outright —
    // `UNIQUE constraint failed: gpkg_contents.identifier`, and it terminates
    // the translation — so a refusal here is not merely the conservative
    // answer, it is the one the only other implementation gives./
    for (position, (_, values)) in rows.iter().enumerate() {
        let identifier = values.get(2);
        if rows[..position].iter().any(|(_, earlier)| earlier.get(2) == identifier) {
            let Some(Value::Text(identifier)) = identifier else { continue };
            return Err(Error::Convert(format!(
                "gpkg: two feature layers in the output would share the gpkg_contents \
                 identifier \"{identifier}\", which Requirement 13 makes UNIQUE; refusing to \
                 write a database SQLite reports as malformed — rename one layer's identifier, \
                 or write to a new file"
            )));
        }
    }
    for (i, (pyramid, &srs_id)) in carried.pyramids.iter().zip(per_pyramid_srs).enumerate() {
        // `identifier` is UNIQUE across the whole table, so a carried pyramid
        // whose identifier a layer already took would build an index with two
        // equal keys — a database SQLite reports as malformed. Refused for the
        // same reason as the table-name collision in `build`.
        if rows.iter().any(|(_, values)| values.get(2) == Some(&Value::Text(pyramid.identifier.clone())))
        {
            return Err(Error::Convert(format!(
                "gpkg: the source carries a tile pyramid whose gpkg_contents identifier \"{}\" \
                 is already used by another entry in the output; refusing to write a container \
                 with a duplicate identifier",
                pyramid.identifier
            )));
        }
        rows.push((
            (layers.len() + i + 1) as i64,
            vec![
                Value::Text(pyramid.name.clone()),
                Value::Text("tiles".into()),
                Value::Text(pyramid.identifier.clone()),
                Value::Text(pyramid.description.clone()),
                // Carried, on the same rule as a layer's: the stamp follows
                // whoever last changed the content, and that is never geosetta.
                Value::Text(pyramid.last_change.clone()),
                pyramid.bbox[0].clone(),
                pyramid.bbox[1].clone(),
                pyramid.bbox[2].clone(),
                pyramid.bbox[3].clone(),
                Value::Int(srs_id),
            ],
        ));
    }
    for (i, (table, srs_id)) in carried.tables.iter().zip(per_table).enumerate() {
        let Some(registration) = &table.registration else { continue };
        // Requirement 13 makes `identifier` UNIQUE, so the same refusal the
        // pyramid arm makes applies here — and a NULL identifier is exempt,
        // because two NULLs do not collide under a SQL UNIQUE. That is the
        // whole reason `Registration::identifier` is an `Option`.
        if let Some(identifier) = &registration.identifier
            && rows
                .iter()
                .any(|(_, values)| values.get(2) == Some(&Value::Text(identifier.clone())))
        {
            return Err(Error::Convert(format!(
                "gpkg: the source carries a dataset whose gpkg_contents identifier \"{identifier}\" \
                 is already used by another entry in the output; refusing to write a container \
                 with a duplicate identifier"
            )));
        }
        rows.push((
            (layers.len() + carried.pyramids.len() + i + 1) as i64,
            vec![
                Value::Text(table.name.clone()),
                Value::Text(registration.data_type.clone()),
                text_or_null(registration.identifier.clone()),
                text_or_null(registration.description.clone()),
                Value::Text(registration.last_change.clone()),
                registration.bbox[0].clone(),
                registration.bbox[1].clone(),
                registration.bbox[2].clone(),
                registration.bbox[3].clone(),
                match srs_id {
                    Some(id) => Value::Int(*id),
                    None => Value::Null,
                },
            ],
        ));
    }
    let rows = rows;
    Ok(TableSpec {
        name: "gpkg_contents".into(),
        sql: CONTENTS_DDL.into(),
        // `table_name` is the primary key and `identifier` is UNIQUE, so this
        // table owns two implicit indexes — `_1` for the key, `_2` for the
        // UNIQUE, which is the order SQLite assigns them.
        autoindexes: vec![
            autoindex_records(&rows, &[0]),
            autoindex_records(&rows, &[2]),
        ],
        rows,
        autoincrement: None,
    })
}

/// `gpkg_tile_matrix_set` — one row per carried pyramid, in the **output's**
/// srs numbering.
fn tile_matrix_set(carried: &Carried, per_pyramid_srs: &[i64]) -> TableSpec {
    let rows: Vec<(i64, Vec<Value>)> = carried
        .pyramids
        .iter()
        .zip(per_pyramid_srs)
        .enumerate()
        .map(|(i, (pyramid, &srs_id))| {
            (
                (i + 1) as i64,
                vec![
                    Value::Text(pyramid.name.clone()),
                    Value::Int(srs_id),
                    pyramid.matrix_set[0].clone(),
                    pyramid.matrix_set[1].clone(),
                    pyramid.matrix_set[2].clone(),
                    pyramid.matrix_set[3].clone(),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_tile_matrix_set".into(),
        sql: TILE_MATRIX_SET_DDL.into(),
        // `table_name TEXT NOT NULL PRIMARY KEY` is not a rowid alias, so it
        // owns an autoindex.
        autoindexes: vec![autoindex_records(&rows, &[0])],
        autoincrement: None,
        rows,
    }
}

/// `gpkg_tile_matrix` — every carried pyramid's zoom levels, carried whole.
fn tile_matrix(carried: &Carried) -> TableSpec {
    let mut rows: Vec<(i64, Vec<Value>)> = Vec::new();
    for pyramid in &carried.pyramids {
        for level in &pyramid.matrix {
            rows.push((
                rows.len() as i64 + 1,
                vec![
                    Value::Text(pyramid.name.clone()),
                    level.zoom_level.clone(),
                    level.matrix_width.clone(),
                    level.matrix_height.clone(),
                    level.tile_width.clone(),
                    level.tile_height.clone(),
                    level.pixel_x_size.clone(),
                    level.pixel_y_size.clone(),
                ],
            ));
        }
    }
    TableSpec {
        name: "gpkg_tile_matrix".into(),
        sql: TILE_MATRIX_DDL.into(),
        // `CONSTRAINT pk_ttm PRIMARY KEY (table_name, zoom_level)`.
        autoindexes: vec![autoindex_records(&rows, &[0, 1])],
        autoincrement: None,
        rows,
    }
}

/// The tile table itself: the source's DDL and rows, unchanged.
///
/// **Nothing here decodes a tile.** `tile_data` is a blob this crate moves and
/// never inspects, which is D8's point — carrying a pyramid is the same
/// operation as carrying any other independent table, with no raster-specific
/// code anywhere in it.
fn tile_table(pyramid: &Pyramid) -> TableSpec {
    TableSpec {
        name: pyramid.name.clone(),
        sql: pyramid.sql.clone(),
        // Whatever the pyramid carries, and **what that is depends on how it
        // got here**: re-packed from the source for a carried pyramid (D10),
        // or *built* from the merged rows for a merged one
        // (`super::reconcile::merge_tiles`, step 5). Either way this function
        // does not decide it, which is the point — until 2026-08-25 it built an
        // index over whichever of Requirement 54's columns it found *by name*,
        // which fabricates an autoindex for a table whose DDL declares no
        // `UNIQUE`, and that is a malformed database rather than a harmless
        // extra (`handoff.org` entry 20). The DDL is the only authority for
        // whether an index exists at all, and the merge consults it through
        // `sqlite::autoindex_key_columns` rather than guessing again. See
        // `super::carry::Pyramid`.
        autoindexes: pyramid.autoindexes.clone(),
        autoincrement: pyramid.autoincrement,
        rows: pyramid.rows.clone(),
    }
}

/// A carried table, put back exactly as it was read.
///
/// Nothing here interprets a column: the DDL is the source's text, the rows are
/// its rows with their original rowids, and the autoindexes are its own b-trees
/// re-packed. That is the whole of the independent class's contract — see
/// `super::carry::UserTable`.
fn user_table(table: &UserTable) -> TableSpec {
    TableSpec {
        name: table.name.clone(),
        sql: table.sql.clone(),
        autoindexes: table.autoindexes.clone(),
        autoincrement: table.autoincrement,
        rows: match &table.storage {
            Storage::Rows(rows) => rows.clone(),
            // Unreachable: `build` sends a `Records` table down the
            // `MasterEntry` path and never calls this. Empty rather than a
            // panic, because the cost of being wrong here is an empty table
            // and the cost of a panic is no output at all.
            Storage::Records(_) => Vec::new(),
        },
    }
}

/// `MasterEntry` for each of a carried table's triggers.
///
/// A trigger owns no b-tree; it is pure schema, and there is nothing to
/// reconstruct one from but the text SQLite stored.
fn triggers<'a>(
    tbl_name: &'a str,
    list: &'a [(String, String)],
) -> impl Iterator<Item = MasterEntry> + 'a {
    list.iter().map(move |(name, sql)| MasterEntry {
        kind: "trigger".into(),
        name: name.clone(),
        tbl_name: tbl_name.to_string(),
        sql: sql.clone(),
        records: None,
    })
}

fn geometry_columns(layers: &[(String, FeatureCollection)], per_layer_srs: &[i64]) -> TableSpec {
    let rows: Vec<(i64, Vec<Value>)> = layers
        .iter()
        .zip(per_layer_srs)
        .enumerate()
        .map(|(i, ((name, _), &srs_id))| {
            (
                (i + 1) as i64,
                vec![
                    Value::Text(name.clone()),
                    Value::Text("geom".into()),
                    Value::Text("GEOMETRY".into()),
                    Value::Int(srs_id),
                    Value::Int(0),
                    Value::Int(0),
                ],
            )
        })
        .collect();
    TableSpec {
        name: "gpkg_geometry_columns".into(),
        sql: GEOMETRY_COLUMNS_DDL.into(),
        // `pk_geom_cols` over (table_name, column_name), then
        // `uk_gc_table_name` over table_name alone.
        autoindexes: vec![
            autoindex_records(&rows, &[0, 1]),
            autoindex_records(&rows, &[0]),
        ],
        rows,
        autoincrement: None,
    }
}

/// The rowids a layer's features are written under.
///
/// Dense `1..n` unless the source carried ids that differ — see
/// `super::reader`'s `carried_fid` and [`crate::GpkgFeatureNative::fid`]. A
/// feature carrying none takes its 1-based position, which is exactly the id
/// it had in a source whose ids the reader classed derived, so the two halves
/// reconstruct the original set between them.
///
/// # Why this validates rather than replays
///
/// **A carried id is only valid against the features actually being written**,
/// which is the layer-coupled rule in `handoff.org` § M3B NEEDS A THIRD CLASS,
/// and validity has to be *computed here* because `FeatureCollection::features`
/// is a public field a consumer mutates directly — there is no setter to hook.
/// Delete a feature from the middle and the survivors' carried ids no longer
/// line up with their positions; two of them can collide, or run backwards.
///
/// Either would be fatal rather than untidy: `fid` is `INTEGER PRIMARY KEY`, so
/// a duplicate is a malformed database, and `write_database` requires rows in
/// ascending rowid order to pack a b-tree at all. So the sequence is checked
/// for both, and **the whole layer falls back to dense `1..n`** when it fails.
/// Falling back per feature would be worse than useless: it would hand back a
/// set that is neither the source's numbering nor a fresh one.
fn feature_ids(fc: &FeatureCollection) -> Vec<i64> {
    let ids: Vec<i64> = fc
        .features
        .iter()
        .enumerate()
        .map(|(i, feat)| match feat.native.as_ref().and_then(|n| n.payload_for(Format::Gpkg)) {
            Some(NativePayload::GpkgFeature(g)) => g.fid.unwrap_or((i + 1) as i64),
            _ => (i + 1) as i64,
        })
        .collect();
    // Strictly ascending covers both hazards at once: it rules out a duplicate
    // and it is what the b-tree writer needs.
    if ids.windows(2).all(|w| w[0] < w[1]) && ids.first().is_none_or(|&first| first > 0) {
        ids
    } else {
        (1..=fc.features.len() as i64).collect()
    }
}

fn feature_table(
    name: &str,
    fc: &FeatureCollection,
    srs_id: i64,
    envelope: bool,
    ids: &[i64],
) -> TableSpec {
    // `columns_for`, not `infer_columns`: a column the source declared a type
    // for is declared as *that* type here, which is what lets a `BOOLEAN`, a
    // `DATETIME` or a `SMALLINT` survive rather than collapsing to the four
    // types the hub can infer.
    let columns = columns_for(fc);

    // DDL: fid (rowid), geom, then one column per property.
    let mut sql = format!("CREATE TABLE {} (\"fid\" INTEGER PRIMARY KEY NOT NULL, \"geom\" GEOMETRY", quote(name));
    for col in &columns {
        sql.push_str(&format!(", {} {}", quote(&col.name), sqlite_type(col)));
    }
    sql.push(')');

    let rows = fc
        .features
        .iter()
        .enumerate()
        .map(|(i, feat)| {
            let mut values = Vec::with_capacity(columns.len() + 2);
            values.push(Value::Null); // fid, carried by the rowid
            values.push(match &feat.geometry {
                Some(g) => Value::Blob(gpkg_geometry(g, srs_id, envelope)),
                None => Value::Null,
            });
            for col in &columns {
                values.push(cell_to_value(&col.cell(i)));
            }
            // The rowid is the carried id when there was one — see
            // `feature_ids`, which is also what the R*Tree is built against, so
            // an index entry cannot point at a row that is not there.
            (ids[i], values)
        })
        .collect();

    TableSpec {
        name: name.to_string(),
        sql,
        rows,
        // The feature table is keyed by `"fid" INTEGER PRIMARY KEY`, a rowid
        // alias, so it owns no index b-tree of its own. Its *spatial* index is
        // a separate opt-in structure — see `super::rtree`.
        autoindexes: Vec::new(),
        autoincrement: None,
    }
}

/// GeoPackage Binary: an 8-byte header ("GP", version, flags, LE srs_id),
/// optionally followed by a cached envelope, wrapping standard WKB.
///
/// With `envelope` false the header is exactly what it always was — flags
/// `0x01`, envelope indicator 0, no envelope bytes — so default output is
/// unchanged byte for byte.
fn gpkg_geometry(g: &Geometry, srs_id: i64, envelope: bool) -> Vec<u8> {
    let env = if envelope { gpb_envelope(g) } else { None };
    let (indicator, env_bytes) = match &env {
        Some((i, b)) => (*i, b.as_slice()),
        None => (0u8, &[][..]),
    };
    let wkb = to_wkb(g);
    let mut out = Vec::with_capacity(8 + env_bytes.len() + wkb.len());
    out.extend_from_slice(b"GP");
    out.push(0); // version
    // flags: bit 0 = little-endian header ints, bits 1-3 = envelope indicator.
    out.push(0x01 | (indicator << 1));
    out.extend_from_slice(&(srs_id as i32).to_le_bytes());
    out.extend_from_slice(env_bytes);
    out.extend_from_slice(&wkb);
    out
}

/// GPB §2.1.3's cached envelope: an indicator code plus a little-endian `f64`
/// sequence, or `None` for a geometry with no coordinates (indicator 0).
///
/// The layout is min/max *pairs per dimension* — `minx, maxx, miny, maxy`, then
/// `minz, maxz` for indicator 2 — **not** all mins followed by all maxes.
/// Confirmed by hand-decoding real `ogr2ogr -f GPKG` output rather than read
/// off the spec: `tests/fixtures/gdal_linestring_2d.gpkg` (indicator 1, 32
/// bytes) and `gdal_linestring_z.gpkg` (indicator 2, 48 bytes).
///
/// **The envelope stops at Z: only indicators 1 and 2 are produced, never 3
/// (XYM) or 4 (XYZM), even for an M-bearing geometry.** Two reasons, the first
/// of which is the real one.
///
/// *An envelope is a spatial bound, and M is not a spatial dimension.* The
/// envelope exists to accelerate spatial queries, and it is genuinely used for
/// that: with a deliberately falsified envelope, `ogrinfo -spat 0 0 25 15`
/// returns **zero** features for a geometry that plainly intersects the filter
/// box — GDAL trusts the cache over the coordinates. But a spatial filter is
/// over X/Y (and conceivably Z); M is a linear-referencing measure (mile
/// markers, timestamps), with no query to accelerate. GeoPackage's own RTree is
/// strictly 2D for the same reason. Indicators 3 and 4 exist in the spec's
/// table for completeness, not because anything consumes them.
///
/// *And nothing produces them to check against.* GDAL 3.13.2 emits indicator 1
/// for `LINESTRING M` and indicator 2 for `LINESTRING ZM` — the latter caching
/// the *Z* range, not M. Verified across eight geometry families (LineString,
/// Polygon, MultiLineString, MultiPoint, Point, GeometryCollection,
/// CircularString, in M and ZM forms); no creation option or `OGR_GPKG_*`
/// config switch changes it. So an XYM/XYZM byte order could only be *assumed*
/// symmetric with XYZ's, which `plans/envelope.org` explicitly forbids — and
/// given the filter behaviour above, a wrongly-encoded envelope silently drops
/// real query hits. A missing envelope only costs speed.
///
/// GDAL's *reader* does accept indicators 3 and 4 (it skips the bytes), so
/// emitting them would not break readers — it simply would not buy anything,
/// at the cost of a layout no reference implementation could confirm.
fn gpb_envelope(g: &Geometry) -> Option<(u8, Vec<u8>)> {
    let b = g.bbox();
    if b.is_empty() {
        return None;
    }
    let mut buf = Vec::with_capacity(48);
    for v in [b.min_x, b.max_x, b.min_y, b.max_y] {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    let indicator = match b.z {
        Some((lo, hi)) => {
            buf.extend_from_slice(&lo.to_le_bytes());
            buf.extend_from_slice(&hi.to_le_bytes());
            2
        }
        None => 1,
    };
    Some((indicator, buf))
}

fn bbox(fc: &FeatureCollection) -> (Value, Value, Value, Value) {
    let mut b = Bbox::empty();
    for f in &fc.features {
        if let Some(g) = &f.geometry {
            g.extend_bbox(&mut b);
        }
    }
    if b.is_empty() {
        (Value::Null, Value::Null, Value::Null, Value::Null)
    } else {
        (
            Value::Real(b.min_x),
            Value::Real(b.min_y),
            Value::Real(b.max_x),
            Value::Real(b.max_y),
        )
    }
}

/// The GeoPackage spelling for a declared type, split out from
/// [`sqlite_type`] so the reader's tests can assert the two directions agree
/// without constructing a [`Column`] — see
/// `every_declared_type_is_written_with_a_spelling_that_reads_back_wide_enough`.
pub(super) fn sql_type_for(d: DeclType) -> &'static str {
    match d {
        DeclType::Bool => "BOOLEAN",
        // GeoPackage has no unsigned type, so each unsigned width is
        // written as the *next signed width up* — the one that admits its
        // whole range. Pairing them with the same-width signed spelling
        // (`UInt32` as `MEDIUMINT`) was a silent narrowing: the value
        // survived SQLite's dynamic typing, but the declaration was a lie
        // and GDAL read 3000000000 back as -1294967296. Found 2026-08-21 by
        // the recast matrix; `Format::recast` states the result and
        // `every_declared_type_survives_or_is_admitted` pins it.
        DeclType::Int8 => "TINYINT",
        DeclType::UInt8 | DeclType::Int16 => "SMALLINT",
        DeclType::UInt16 | DeclType::Int32 => "MEDIUMINT",
        DeclType::UInt32 | DeclType::Int64 | DeclType::UInt64 => "INTEGER",
        DeclType::Float32 => "FLOAT",
        DeclType::Float64 => "DOUBLE",
        DeclType::String => "TEXT",
        DeclType::Json => "JSON",
        DeclType::DateTime => "DATETIME",
        DeclType::Date => "DATE",
        DeclType::Binary => "BLOB",
    }
}

/// The SQL type to declare a column as: what the source declared, when that
/// still describes the data, else what the values inferred.
///
/// Every name here is from GeoPackage spec table 1 (*GeoPackage Data Types*),
/// so a declaration carried in from another spoke's richer vocabulary lands on
/// a spelling this format actually sanctions.
fn sqlite_type(col: &Column) -> &'static str {
    if let Some(d) = col.declared_type() {
        return sql_type_for(d);
    }
    match col.ty {
        ColumnType::Bool => "BOOLEAN",
        ColumnType::Int64 => "INTEGER",
        ColumnType::Double => "REAL",
        ColumnType::String => "TEXT",
        ColumnType::Binary => "BLOB",
    }
}

fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Int(*b as i64),
        Cell::Int(n) => Value::Int(*n),
        Cell::Double(d) => Value::Real(*d),
        Cell::Str(s) => Value::Text(s.to_string()),
        Cell::Bytes(b) => Value::Blob(b.to_vec()),
    }
}

/// Quote a SQL identifier, doubling any embedded quotes.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Position;
    use crate::feature::Feature;
    use crate::json::JsonValue;

    fn point(x: f64, y: f64) -> Option<Geometry> {
        Some(Geometry::Point(Position::new(x, y)))
    }
    fn fc(features: Vec<Feature>) -> FeatureCollection {
        FeatureCollection::new(features)
    }

    /// Every registry table is written as the specification defines it, with
    /// the index each of its constraints requires.
    ///
    /// This is *defect 6*. Until 2026-08-23 all of these were emitted with no
    /// table constraints at all, so a `gpkg -> gpkg` run read a conformant
    /// container and returned one whose registry DDL had lost every key it
    /// arrived with. **Nothing observable broke** — GDAL read the output and
    /// `PRAGMA integrity_check` passed — which is why no existing test caught
    /// it: they compare values and declared column types, and a table
    /// constraint is neither.
    ///
    /// Two halves, and both are needed. The DDL half asserts the constraint is
    /// *declared*; the index half asserts the b-tree it obliges is *there*,
    /// because declaring one without the other is precisely the malformed
    /// database of `handoff.org` § BUGS FOUND entry 15.
    ///
    /// **The DDL half below is a weaker check than it looks**, and
    /// `the_registry_ddl_is_what_the_standard_says` is the one that closes the
    /// gap: a substring assertion can only confirm what somebody thought to
    /// assert. It cannot see a constraint nobody knew was missing — which is
    /// exactly how `gdc_tn` survived this test's first pass — and it cannot
    /// see an *extra* column or constraint at all, though the standard forbids
    /// those in the same sentence.
    #[test]
    fn the_registry_tables_are_written_as_the_specification_defines_them() {
        // Declared: the constraints the specification states, by name where it
        // names them.
        assert!(CONTENTS_DDL.contains("table_name TEXT NOT NULL PRIMARY KEY"));
        assert!(CONTENTS_DDL.contains("identifier TEXT UNIQUE"));
        assert!(CONTENTS_DDL.contains("CONSTRAINT fk_gc_r_srs_id FOREIGN KEY"));
        assert!(CONTENTS_DDL.contains("description TEXT DEFAULT ''"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT pk_geom_cols PRIMARY KEY"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT uk_gc_table_name UNIQUE"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT fk_gc_tn FOREIGN KEY"));
        assert!(GEOMETRY_COLUMNS_DDL.contains("CONSTRAINT fk_gc_srs FOREIGN KEY"));
        assert!(DATA_COLUMNS_DDL.contains("CONSTRAINT pk_gdc PRIMARY KEY"));
        // `gdc_tn` is the constraint `ogr2ogr` omits and the specification
        // requires — missed on this test's first pass for exactly that reason,
        // since GDAL's own file was the reference being read.
        assert!(DATA_COLUMNS_DDL.contains("CONSTRAINT gdc_tn UNIQUE (table_name, name)"));
        assert!(DATA_COLUMN_CONSTRAINTS_DDL.contains("CONSTRAINT gdcc_ntv UNIQUE"));
        assert!(EXTENSIONS_DDL.contains("CONSTRAINT ge_tce UNIQUE"));
        // The specification states `srs_id INTEGER PRIMARY KEY`; `ogr2ogr`
        // writes `INTEGER NOT NULL PRIMARY KEY`. Follow the specification.
        assert!(SPATIAL_REF_SYS_DDL.contains("srs_id INTEGER PRIMARY KEY"));
        assert!(!SPATIAL_REF_SYS_DDL.contains("NOT NULL PRIMARY KEY"));

        // Present: with `--rtree`, so `gpkg_extensions` is written too.
        let layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: vec![("name".into(), JsonValue::String("a".into()))],
            native: None,
        }]);
        let bytes =
            write_layers(None, &[("places".into(), layer)], &Carried::default(), true, false)
                .unwrap();
        let objects = crate::sqlite::Database::open(&bytes).unwrap().master_objects().unwrap();

        // Named and ordered as SQLite names them: `_1` for the primary key,
        // `_2` for the UNIQUE beside it.
        for (table, count) in [("gpkg_contents", 2), ("gpkg_geometry_columns", 2), ("gpkg_extensions", 1)] {
            for n in 1..=count {
                let want = format!("sqlite_autoindex_{table}_{n}");
                let found = objects
                    .iter()
                    .find(|o| o.name == want)
                    .unwrap_or_else(|| panic!("missing {want}"));
                assert_eq!(found.kind, "index", "{want}");
                assert_eq!(found.tbl_name, table, "{want}");
                assert_ne!(found.root_page, 0, "{want} must own a real b-tree");
            }
        }

        // And `gpkg_spatial_ref_sys` owns none: `INTEGER PRIMARY KEY` is a
        // rowid alias, which is the whole reason that one table was already
        // correct while the other five were not.
        assert!(
            !objects.iter().any(|o| o.name.starts_with("sqlite_autoindex_gpkg_spatial_ref_sys")),
            "a rowid alias needs no index of its own"
        );
    }

    /// Normalise DDL for comparison: layout differs between the standard's
    /// wrapped text and this crate's one-line constants, and layout is the one
    /// thing that is genuinely not being tested.
    ///
    /// Whitespace *inside* a string literal is data and is left alone —
    /// `DEFAULT ''` and `strftime('%Y-%m-%dT%H:%M:%fZ','now')` both appear in
    /// the registry DDL, and collapsing runs blindly would rewrite them.
    fn normalize_ddl(sql: &str) -> String {
        let mut out = String::with_capacity(sql.len());
        let mut chars = sql.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                out.push(c);
                // Copy the literal through verbatim, honouring the `''` escape.
                while let Some(q) = chars.next() {
                    out.push(q);
                    if q == '\'' {
                        if chars.peek() == Some(&'\'') {
                            out.push(chars.next().unwrap());
                            continue;
                        }
                        break;
                    }
                }
            } else if c.is_whitespace() {
                while chars.peek().is_some_and(|n| n.is_whitespace()) {
                    chars.next();
                }
                // The wrapped form breaks the line after `(`; the one-line
                // form does not. Dropping the space here rather than by a
                // `replace` over the finished string is what keeps a literal
                // containing `( ` safe.
                if !out.ends_with('(') {
                    out.push(' ');
                }
            } else {
                // Its closing partner: `\n)` against `)`.
                if c == ')' && out.ends_with(' ') {
                    out.pop();
                }
                out.push(c);
            }
        }
        // Everything else stays significant, including the standard's own
        // inconsistency between `gpkg_spatial_ref_sys(srs_id)` and
        // `gpkg_spatial_ref_sys (srs_id)` — that is what it says.
        out.trim().trim_end_matches(';').trim().to_string()
    }

    /// The normaliser collapses layout and *only* layout.
    ///
    /// Worth its own test because the property is easy to lose: the obvious
    /// implementation is a whitespace `replace` over the finished string, and
    /// it silently rewrites `DEFAULT '  x  '` while every DDL comparison
    /// keeps passing. Both literals below are real —
    /// `strftime('%Y-%m-%dT%H:%M:%fZ','now')` is `gpkg_contents.last_change`.
    #[test]
    fn normalizing_ddl_does_not_reach_inside_a_string_literal() {
        let wrapped = "CREATE TABLE t (\n  a DATETIME DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),\n  b TEXT DEFAULT '  two  spaces  ',\n  c TEXT DEFAULT 'it''s ( quoted )'\n);";
        let inline = "CREATE TABLE t (a DATETIME DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')), b TEXT DEFAULT '  two  spaces  ', c TEXT DEFAULT 'it''s ( quoted )')";
        assert_eq!(normalize_ddl(wrapped), normalize_ddl(inline));
        // Not merely equal to each other — equal to the text, spaces intact.
        assert_eq!(normalize_ddl(wrapped), inline);
    }

    use super::super::carry::SrsDef;
    use crate::NamedCrs;

    /// **A tile pyramid survives a `gpkg -> gpkg` write**, which is D8's
    /// carrying half and the largest single piece of defect 3's fix.
    ///
    /// Everything asserted here is a thing that was destroyed before
    /// 2026-08-24: the tile rows and their blobs, the tiling scheme, the
    /// `sqlite_sequence` mark, the six triggers, and the pyramid's own
    /// `gpkg_contents` registration.
    ///
    /// *The blob comparison is the load-bearing one.* Row counts survive a
    /// carry that mangles every payload; comparing the bytes is what makes this
    /// a statement about the tiles rather than about the table.
    #[test]
    fn a_tile_pyramid_survives_a_geopackage_write() {
        const TILES: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");
        let carried = crate::geopackage::read_container(TILES).unwrap();
        assert_eq!(carried.pyramids.len(), 1, "the fixture holds one pyramid");

        let layers = read_layers(TILES).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();

        let source = crate::sqlite::Database::open(TILES).unwrap();
        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let rows = |db: &crate::sqlite::Database, name: &str| {
            let tables = db.tables().unwrap();
            let t = tables.iter().find(|t| t.name == name).unwrap_or_else(|| {
                panic!("{name} is missing from the output");
            });
            db.read_rows(t).unwrap()
        };

        // The tiles themselves, blob for blob and key for key.
        assert_eq!(rows(&ours, "imagery"), rows(&source, "imagery"));

        // The scheme, carried whole: 14 levels for the 2 that hold tiles.
        assert_eq!(rows(&ours, "gpkg_tile_matrix").len(), 14);
        assert_eq!(rows(&ours, "gpkg_tile_matrix_set"), rows(&source, "gpkg_tile_matrix_set"));

        // The AUTOINCREMENT mark, which no DDL regenerates and whose absence is
        // a database that passes `integrity_check` and fails every `INSERT`.
        assert_eq!(
            ours.sequence_numbers().unwrap().into_iter().find(|(t, _)| t == "imagery"),
            Some(("imagery".to_string(), 80)),
        );

        // The six triggers, which are schema objects on a carried table that
        // are neither index nor DDL.
        let objects = ours.master_objects().unwrap();
        let triggers = objects.iter().filter(|o| o.kind == "trigger" && o.tbl_name == "imagery");
        assert_eq!(triggers.count(), 6);

        // And the registration that makes it a pyramid rather than a table of
        // blobs, beside the feature layer rather than instead of it.
        let contents = rows(&ours, "gpkg_contents");
        let kinds: Vec<_> = contents
            .iter()
            .map(|r| (text(&r[0]), text(&r[1])))
            .collect();
        assert!(kinds.contains(&("imagery".into(), "tiles".into())), "{kinds:?}");
        assert!(kinds.contains(&("cities".into(), "features".into())), "{kinds:?}");
    }

    /// ***A feature id survives a `gpkg -> gpkg` write*** — `handoff.org`
    /// § BUGS FOUND entry 19, and the property Requirement 29's note makes
    /// content: other rows point at these values by rowid.
    ///
    /// `sqlite_fid_gap.gpkg` is the only file in the tree whose ids are not
    /// their positions — `cities` holds 1, 9, 40 — which is why the whole
    /// corpus was blind to the renumbering until it was built.
    #[test]
    fn a_feature_id_survives_a_geopackage_write() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();

        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = ours.tables().unwrap();
        let cities = tables.iter().find(|t| t.name == "cities").unwrap();
        let ids: Vec<i64> = ours
            .read_rows(cities)
            .unwrap()
            .iter()
            .map(|row| match row[cities.rowid_alias().unwrap()] {
                Value::Int(n) => n,
                ref other => panic!("a non-integer fid: {other:?}"),
            })
            .collect();
        assert_eq!(ids, [1, 9, 40], "the source's own numbering, not the positions");
    }

    /// ***And a dense sequence carries no payload at all*** — D16, and the
    /// containment that makes carrying ids free.
    ///
    /// `second` in the same fixture holds 1, 2. A reader that carried ids
    /// unconditionally would put a `Native` on every feature of every
    /// GeoPackage ever written; this asserts it puts one on none of them.
    /// /That is the `is_empty` mistake `last_change` made on 2026-08-24,
    /// avoided here on purpose./
    #[test]
    fn a_dense_feature_id_sequence_is_derived_and_carries_nothing() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();

        let carried_in = |name: &str| -> usize {
            let (_, fc) = layers.iter().find(|(n, _)| n == name).unwrap();
            fc.features
                .iter()
                .filter(|f| {
                    matches!(
                        f.native.as_ref().and_then(|n| n.payload_for(Format::Gpkg)),
                        Some(NativePayload::GpkgFeature(_))
                    )
                })
                .count()
        };
        // `second` is 1, 2 — exactly what the writer emits, so nothing is carried.
        assert_eq!(carried_in("second"), 0, "a dense layer must cost no payload");
        // `cities` is 1, 9, 40 — the first is what the writer would emit anyway
        // and is derived; the other two are not.
        assert_eq!(carried_in("cities"), 2, "only the ids that actually differ");
    }

    /// ***Carried ids that no longer describe the features being written are
    /// discarded, and the whole layer falls back to a dense sequence.***
    ///
    /// This is the layer-coupled rule doing its job. `Feature`'s fields are
    /// public, so a consumer deletes from the middle of a collection with no
    /// method call to intercept — after which the survivors' ids may collide or
    /// run backwards. A duplicate `INTEGER PRIMARY KEY` is a malformed
    /// database and an unsorted one cannot be packed into a b-tree at all, so
    /// neither may be written.
    ///
    /// /Reversing the order is the mutation: replaying the ids blindly writes
    /// 40, 9, 1 and `write_database` has no ascending sequence to pack./
    #[test]
    fn carried_ids_that_no_longer_ascend_fall_back_to_a_dense_sequence() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let mut layers = read_layers(SRC).unwrap();
        let (_, fc) = layers.iter_mut().find(|(n, _)| n == "cities").unwrap();
        // The edit a consumer makes directly, with nothing to hook.
        fc.features.reverse();

        let bytes =
            write_layers(None, &layers, &crate::geopackage::Carried::default(), false, false)
                .unwrap();
        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = ours.tables().unwrap();
        let cities = tables.iter().find(|t| t.name == "cities").unwrap();
        let ids: Vec<i64> = ours
            .read_rows(cities)
            .unwrap()
            .iter()
            .map(|row| match row[cities.rowid_alias().unwrap()] {
                Value::Int(n) => n,
                ref other => panic!("a non-integer fid: {other:?}"),
            })
            .collect();
        assert_eq!(ids, [1, 2, 3], "a fresh dense sequence, not the stale numbering");
    }

    /// Build a layer carrying a `gpkg_data_columns` native, with or without a
    /// constraint — which is what decides whether the *second* Schema table is
    /// written, and so how many registrations Requirement 141 obliges.
    fn layer_with_schema_native(constrained: bool) -> FeatureCollection {
        let mut layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: vec![("name".into(), JsonValue::String("a".into()))],
            native: None,
        }]);
        layer.native = Some(crate::Native::new(
            Format::Gpkg,
            crate::NativePayload::Gpkg(crate::GpkgNative {
                identifier: None,
                description: None,
                last_change: None,
                data_columns: vec![crate::GpkgDataColumn {
                    column_name: "name".into(),
                    name: Some("Name".into()),
                    title: None,
                    description: None,
                    mime_type: None,
                    constraint_name: constrained.then(|| "nonempty".to_string()),
                }],
                constraints: if constrained {
                    vec![crate::GpkgConstraint {
                        constraint_name: "nonempty".into(),
                        constraint_type: "glob".into(),
                        value: Some("*".into()),
                        min: None,
                        min_is_inclusive: None,
                        max: None,
                        max_is_inclusive: None,
                        description: None,
                    }]
                } else {
                    Vec::new()
                },
            }),
        ));
        layer
    }

    /// Every `gpkg_extensions` row in `bytes` as
    /// `(table_name, column_name, extension_name, scope)`.
    fn extension_rows_in(bytes: &[u8]) -> Vec<(String, String, String, String)> {
        let db = crate::sqlite::Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();
        let Some(t) = tables.iter().find(|t| t.name == "gpkg_extensions") else {
            return Vec::new();
        };
        let at = |c: &str| t.columns.iter().position(|n| n == c).unwrap();
        let (tn, cn, en, sc) =
            (at("table_name"), at("column_name"), at("extension_name"), at("scope"));
        db.read_rows(t)
            .unwrap()
            .iter()
            .map(|row| {
                let cell = |i: usize| match &row[i] {
                    Value::Text(s) => s.clone(),
                    Value::Null => "-".to_string(),
                    other => panic!("unexpected cell {other:?}"),
                };
                (cell(tn), cell(cn), cell(en), cell(sc))
            })
            .collect()
    }

    /// ***Writing an extension's tables without registering them makes the file
    /// lie about what it is*** — Requirement 59, and this crate did it from
    /// 2026-08-21 until 2026-08-25.
    ///
    /// "In an Extended GeoPackage, every extension SHALL be registered in a
    /// corresponding row in the `gpkg_extensions` table … the absence of rows
    /// in the `gpkg_extensions` table SHALL indicate that the file is a
    /// GeoPackage (as opposed to an Extended GeoPackage)." So an output holding
    /// `gpkg_data_columns` and no registration declares itself *plain* while
    /// carrying an extension's table. **Defect 6's shape**: output that does
    /// not state what the standard says it states, and no oracle in this tree
    /// objected because the file is perfectly well-formed SQLite.
    ///
    /// Requirement 141 fixes the values; this asserts them, `column_name`
    /// included — NULL, not the empty string (Requirement 61 permits NULL and
    /// the annex's record table writes it).
    #[test]
    fn writing_the_schema_extension_registers_it() {
        let layer = layer_with_schema_native(true);
        let bytes = write_layers(
            None,
            &[("places".into(), layer)],
            &crate::geopackage::Carried::default(),
            false,
            false,
        )
        .unwrap();

        let rows = extension_rows_in(&bytes);
        let schema: Vec<_> = rows.iter().filter(|(_, _, name, _)| name == "gpkg_schema").collect();
        assert_eq!(schema.len(), 2, "both Schema tables are registered: {rows:?}");
        for (table, column, _, scope) in &schema {
            assert_eq!(column, "-", "a table-scoped registration names no column");
            assert_eq!(scope, "read-write");
            assert!(
                table == "gpkg_data_columns" || table == "gpkg_data_column_constraints",
                "{table}"
            );
        }
    }

    /// ***Only the Schema tables actually written are registered, and that is
    /// where this extension parts company with the Metadata one.***
    ///
    /// `schema_extension_tables` writes `gpkg_data_column_constraints` only
    /// when some layer carried a constraint, so a layer with a plain column
    /// declaration produces one table — and registering the absent one would
    /// point at nothing (Requirement 60 lets `table_name` reference "new tables
    /// required by that extension", and a table the file does not contain is
    /// not required).
    ///
    /// **The annexes differ and that is the whole reason this is not the
    /// metadata rule.** Requirements 103 and 107 each say a GeoPackage *MAY*
    /// contain their table, independently; Requirement 95 makes `gpkg_metadata`
    /// and `gpkg_metadata_reference` a pair that travels together or not at
    /// all. Reading one annex and applying it to the other would have been
    /// wrong in both directions.
    #[test]
    fn an_unwritten_schema_table_is_not_registered() {
        let layer = layer_with_schema_native(false);
        let bytes = write_layers(
            None,
            &[("places".into(), layer)],
            &crate::geopackage::Carried::default(),
            false,
            false,
        )
        .unwrap();

        // The premise: one Schema table, not two.
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let names: Vec<String> = db.tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "gpkg_data_columns"), "{names:?}");
        assert!(
            !names.iter().any(|n| n == "gpkg_data_column_constraints"),
            "no constraint was carried, so the table is not written: {names:?}"
        );

        let schema: Vec<_> = extension_rows_in(&bytes)
            .into_iter()
            .filter(|(_, _, name, _)| name == "gpkg_schema")
            .collect();
        assert_eq!(
            schema.iter().map(|(t, _, _, _)| t.as_str()).collect::<Vec<_>>(),
            ["gpkg_data_columns"],
            "a registration must not name a table the file does not contain"
        );
    }

    // --- § 3's NULL trap, and the identity that resolves it ----------------

    /// A one-feature layer whose `price` column refers to `constraint`.
    fn layer_with_constraint(constraint: GpkgConstraint) -> FeatureCollection {
        let mut layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: vec![("price".into(), JsonValue::Float(1.0))],
            native: None,
        }]);
        layer.native = Some(crate::Native::new(
            Format::Gpkg,
            crate::NativePayload::Gpkg(crate::GpkgNative {
                identifier: None,
                description: None,
                last_change: None,
                data_columns: vec![crate::GpkgDataColumn {
                    column_name: "price".into(),
                    name: None,
                    title: None,
                    description: None,
                    mime_type: None,
                    constraint_name: Some(constraint.constraint_name.clone()),
                }],
                constraints: vec![constraint],
            }),
        ));
        layer
    }

    /// A `range` constraint named `price_range`, with `value` NULL as the
    /// standard defines it for that type.
    fn range_constraint(max: f64, description: &str) -> GpkgConstraint {
        GpkgConstraint {
            constraint_name: "price_range".into(),
            constraint_type: "range".into(),
            value: None,
            min: Some(0.0),
            min_is_inclusive: Some(true),
            max: Some(max),
            max_is_inclusive: Some(true),
            description: Some(description.into()),
        }
    }

    /// Every `gpkg_data_column_constraints` row in the output, as
    /// `(constraint_name, constraint_type, value)`.
    fn constraints_in(bytes: &[u8]) -> Vec<(String, String, String)> {
        let db = crate::sqlite::Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();
        let Some(t) = tables.iter().find(|t| t.name == "gpkg_data_column_constraints") else {
            return Vec::new();
        };
        let at = |c: &str| t.columns.iter().position(|n| n == c).unwrap();
        let (n, ty, v) = (at("constraint_name"), at("constraint_type"), at("value"));
        db.read_rows(t)
            .unwrap()
            .iter()
            .map(|row| {
                let cell = |i: usize| match &row[i] {
                    Value::Text(s) => s.clone(),
                    Value::Null => "-".to_string(),
                    other => panic!("unexpected cell {other:?}"),
                };
                (cell(n), cell(ty), cell(v))
            })
            .collect()
    }

    /// ***Two `range` rows for one constraint name are refused, and the
    /// declared key cannot see the problem.***
    ///
    /// `plans/container-reconciliation.org` § 3's trap, and before 2026-10-13
    /// this wrote both rows. `gdcc_ntv UNIQUE (constraint_name,
    /// constraint_type, value)` does not separate them, because a `range` row's
    /// `value` is NULL *by definition* and SQLite treats NULLs as distinct
    /// under `UNIQUE` — so the output held two contradictory definitions of one
    /// domain and `PRAGMA integrity_check` reported `ok`. /Measured against
    /// GDAL 3.13.3 and SQLite 3.51.0; `tests/fixtures/sqlite_nullkey.gpkg` is
    /// the file./
    ///
    /// **The refusal is § 5's, and it is what honours the rule**: a column
    /// referring to this name would otherwise get bounds neither layer
    /// declared. `ogr2ogr -update -append` resolves the same case silently in
    /// the destination's favour, which is evidence of what a producer does and
    /// not of what is right.
    #[test]
    fn two_range_constraints_for_one_name_are_refused() {
        let layers = [
            ("cities".into(), layer_with_constraint(range_constraint(100.0, "Survey band."))),
            ("towns".into(), layer_with_constraint(range_constraint(500.0, "Another band."))),
        ];
        let error = write_layers(None, &layers, &Carried::default(), false, false)
            .expect_err("two definitions of one domain have no union");
        let message = error.to_string();
        assert!(
            message.contains("price_range") && message.contains("gdcc_ntv"),
            "the refusal must name the constraint and the key that cannot separate it: {message}"
        );
    }

    /// ***The same `range` constraint on both sides is one row, not a
    /// refusal.*** The noise bar in `plans/same-format-fidelity.org` § THE RULE
    /// is what makes this the load-bearing half: a refusal is an acceptable
    /// answer only while it stays off the ordinary path, and two layers sharing
    /// a field domain *is* the ordinary path — it is what every `ogr2ogr`
    /// append produces.
    #[test]
    fn one_range_constraint_shared_by_two_layers_is_one_row() {
        let layers = [
            ("cities".into(), layer_with_constraint(range_constraint(100.0, "Survey band."))),
            ("towns".into(), layer_with_constraint(range_constraint(100.0, "Survey band."))),
        ];
        let bytes = write_layers(None, &layers, &Carried::default(), false, false).unwrap();
        assert_eq!(
            constraints_in(&bytes),
            [("price_range".to_string(), "range".to_string(), "-".to_string())],
            "one domain declared twice is one row"
        );
    }

    /// ***A `range` row's `value` is not part of its identity, and this is the
    /// only input that can tell.***
    ///
    /// Added after mutation-checking: deleting the `range` arm of
    /// [`same_constraint`] broke no test, because a *conformant* range row has
    /// `value` NULL on both sides and `None == None` — so comparing `value`
    /// or not comparing it gives the same answer on every fixture in the tree.
    /// **The correlation was satisfied by both implementations**, which this
    /// tree has now learned to look for before the mutation survives rather
    /// than after.
    ///
    /// The distinguishing input is two `range` rows that disagree on a *non-NULL*
    /// `value`. Keyed per § 6 they are one constraint saying two things, and so
    /// a refusal; keyed on the declared `gdcc_ntv` triple they are two
    /// constraints and both get written, which the standard admits only one of.
    /// /The input is non-conformant — `value` is for `enum` and `glob` — and
    /// that is the point: the branch only ever earns its keep here./
    #[test]
    fn two_range_constraints_differing_only_in_value_are_still_one_constraint() {
        let valued = |value: &str| GpkgConstraint {
            value: Some(value.into()),
            ..range_constraint(100.0, "Survey band.")
        };
        let layers = [
            ("cities".into(), layer_with_constraint(valued("x"))),
            ("towns".into(), layer_with_constraint(valued("y"))),
        ];
        let error = write_layers(None, &layers, &Carried::default(), false, false)
            .expect_err("one range domain cannot have two values");
        assert!(error.to_string().contains("price_range"), "{error}");
    }

    /// ***Two `enum` rows differing only in `value` are two constraints.***
    ///
    /// The negative half of [`same_constraint`], and what stops the rule from
    /// being written as "drop `value` from the key". For an `enum` the value is
    /// the row's whole point, so these must both survive — the same identity
    /// that merges two `range` rows must keep two `enum` rows apart.
    ///
    /// /A rule with one branch tested is a rule half tested, which this tree
    /// has now learned three times./
    #[test]
    fn two_enum_constraints_differing_only_in_value_are_two_constraints() {
        let enum_constraint = |value: &str| GpkgConstraint {
            constraint_name: "grade".into(),
            constraint_type: "enum".into(),
            value: Some(value.into()),
            min: None,
            min_is_inclusive: None,
            max: None,
            max_is_inclusive: None,
            description: None,
        };
        let layers = [
            ("cities".into(), layer_with_constraint(enum_constraint("a"))),
            ("towns".into(), layer_with_constraint(enum_constraint("b"))),
        ];
        let bytes = write_layers(None, &layers, &Carried::default(), false, false).unwrap();
        let mut rows = constraints_in(&bytes);
        rows.sort();
        assert_eq!(
            rows,
            [
                ("grade".to_string(), "enum".to_string(), "a".to_string()),
                ("grade".to_string(), "enum".to_string(), "b".to_string()),
            ],
            "an enum's value is its identity, so neither row may be merged away"
        );
    }

    /// ***Two layers sharing a `gpkg_contents.identifier` are refused***, which
    /// Requirement 13 makes `UNIQUE`.
    ///
    /// **The carried arms of [`gpkg_contents`] had enforced this since
    /// 2026-08-24 and the layers had never been checked against each other.**
    /// A single source cannot hold the collision — its own `UNIQUE` forbade it
    /// — but an append composes *two* sources, and `ogr2ogr -nln` copies the
    /// identifier verbatim when it renames a layer, so two files with
    /// differently-named layers and one identifier is what a real producer
    /// hands you. The output was a database `integrity_check` reports as
    /// `non-unique entry in index sqlite_autoindex_gpkg_contents_2`.
    ///
    /// /`ogr2ogr -update -append` refuses the same case and terminates the
    /// translation, so this is not merely the conservative answer./
    #[test]
    fn two_layers_sharing_a_gpkg_contents_identifier_are_refused() {
        let titled = |title: &str| {
            let mut layer = fc(vec![Feature {
                geometry: point(1.0, 2.0),
                properties: Vec::new(),
                native: None,
            }]);
            layer.native = Some(crate::Native::new(
                Format::Gpkg,
                crate::NativePayload::Gpkg(crate::GpkgNative {
                    identifier: Some(title.into()),
                    ..crate::GpkgNative::default()
                }),
            ));
            layer
        };
        let layers = [("cities".into(), titled("Survey")), ("towns".into(), titled("Survey"))];
        let error = write_layers(None, &layers, &Carried::default(), false, false)
            .expect_err("a duplicate identifier is a malformed database");
        assert!(
            error.to_string().contains("Survey"),
            "the refusal must name the identifier: {error}"
        );

        // The same two layers with distinct identifiers are perfectly ordinary,
        // so the check must not fire on them.
        let distinct = [("cities".into(), titled("Survey")), ("towns".into(), titled("Other"))];
        write_layers(None, &distinct, &Carried::default(), false, false)
            .expect("distinct identifiers are not a collision");
    }

    // --- the metadata pair -------------------------------------------------

    /// Every `gpkg_metadata_reference` row in the output, as
    /// `(scope, table_name, column_name, row_id_value)`.
    fn references_in(bytes: &[u8]) -> Vec<(String, String, String, String)> {
        let db = crate::sqlite::Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();
        let Some(t) = tables.iter().find(|t| t.name == "gpkg_metadata_reference") else {
            return Vec::new();
        };
        let at = |t: &crate::sqlite::Table, c: &str| t.columns.iter().position(|n| n == c).unwrap();
        let (s, tn, cn, ri) = (
            at(t, "reference_scope"),
            at(t, "table_name"),
            at(t, "column_name"),
            at(t, "row_id_value"),
        );
        db.read_rows(t)
            .unwrap()
            .iter()
            .map(|row| {
                let cell = |i: usize| match &row[i] {
                    Value::Text(s) => s.clone(),
                    Value::Int(n) => n.to_string(),
                    Value::Null => "-".to_string(),
                    other => panic!("unexpected cell {other:?}"),
                };
                (cell(s), cell(tn), cell(cn), cell(ri))
            })
            .collect()
    }

    /// How many `gpkg_metadata` rows the output holds.
    fn metadata_rows(bytes: &[u8]) -> usize {
        let db = crate::sqlite::Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();
        tables
            .iter()
            .find(|t| t.name == "gpkg_metadata")
            .map(|t| db.read_rows(t).unwrap().len())
            .unwrap_or(0)
    }

    /// ***A `row`-scoped reference still names the feature it named.***
    ///
    /// The two halves of this have to hold *together*, which is why one test
    /// asserts both: decision 27 carries `cities`'s ids as 1, 9, 40, and the
    /// reference into it points at **9**. Carry the ids without the reference
    /// and the file forgets what the document was about; carry the reference
    /// without the ids and it points at a row that is no longer the one it
    /// meant. /Before 2026-08-25 no fixture in this tree held a row-scoped
    /// reference at all, so neither failure was observable./
    #[test]
    fn a_row_scoped_reference_still_names_the_feature_it_named() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();

        let refs = references_in(&bytes);
        assert!(
            refs.contains(&("row".into(), "cities".into(), "-".into(), "9".into())),
            "the row-scoped reference did not survive: {refs:?}"
        );
        // And the feature it names is in the output under that same id.
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let cities = tables.iter().find(|t| t.name == "cities").unwrap();
        let ids: Vec<i64> = db
            .read_rows(cities)
            .unwrap()
            .iter()
            .map(|row| match row[cities.rowid_alias().unwrap()] {
                Value::Int(n) => n,
                ref other => panic!("a non-integer fid: {other:?}"),
            })
            .collect();
        assert!(ids.contains(&9), "the reference names a rowid the table does not have: {ids:?}");
    }

    /// ***A reference into a layer whose ids were reissued is dropped.***
    ///
    /// **Decision 27 did not close this and the write-up nearly said it had.**
    /// `feature_ids` still falls back to a dense sequence for the whole layer
    /// when a consumer's edit leaves the carried ids not strictly ascending —
    /// and *that* is a renumbering with live row-scoped references pointing
    /// into it, on the plain one-container path.
    ///
    /// `container-reconciliation.org` D5 states the general form: the
    /// `row_id_value` guard belongs wherever a **rowid changes**, not wherever
    /// a merge happens. This is that guard.
    ///
    /// /Nothing else would catch it. The DDL declares no foreign key on
    /// `row_id_value`, so `foreign_key_check` cannot see the dangle and
    /// `integrity_check` cannot either — the file is not malformed, merely
    /// lying./
    #[test]
    fn a_row_scoped_reference_is_dropped_when_the_layer_is_renumbered() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let mut layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        // The edit a consumer makes directly, with nothing to hook: after this
        // the carried ids run 40, 9, 1 and the whole layer is reissued 1, 2, 3.
        let (_, fc) = layers.iter_mut().find(|(n, _)| n == "cities").unwrap();
        fc.features.reverse();

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let refs = references_in(&bytes);
        assert!(
            !refs.iter().any(|(scope, _, _, _)| scope == "row"),
            "a row-scoped reference survived a renumbering that invalidated it: {refs:?}"
        );
        // The `table`- and `column`-scoped references are untouched: renumbering
        // says nothing about which table exists or which columns it has.
        assert!(
            refs.iter().any(|(scope, _, _, _)| scope == "table")
                && refs.iter().any(|(scope, _, _, _)| scope == "column"),
            "renumbering dropped references that do not depend on a rowid: {refs:?}"
        );
    }

    /// ***A `column`-scoped reference to a column the write does not produce is
    /// dropped*** — Requirement 98's other branch.
    ///
    /// The fixture's own column reference names `name`, which survives, so it
    /// exercises only the *keeping* half. **A rule with one branch tested is a
    /// rule half tested**, and this one is the D14 precedent applied to a
    /// second table: `ARROW:schema` is replayed only while the column names
    /// still match, and a metadata reference is kept on the same terms.
    ///
    /// /Added after mutation-checking found that deleting the column check
    /// broke no test at all./
    #[test]
    fn a_column_scoped_reference_to_a_missing_column_is_dropped() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let mut carried = crate::geopackage::read_container(SRC).unwrap();
        let metadata = carried.metadata.as_mut().unwrap();
        let reference =
            metadata.references.iter_mut().find(|r| r.scope == "column").unwrap();
        assert_eq!(reference.column_name.as_deref(), Some("name"), "the premise");
        reference.column_name = Some("a_column_no_layer_has".into());

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let refs = references_in(&bytes);
        assert!(
            !refs.iter().any(|(scope, _, _, _)| scope == "column"),
            "a reference to a column that is not written survived: {refs:?}"
        );
        // The other two do not depend on a column and are untouched.
        assert_eq!(refs.len(), 2, "only the column reference should go: {refs:?}");
    }

    /// ***A reference to a carried tile pyramid survives, and the authority is
    /// `gpkg_contents`.***
    ///
    /// Requirement 97 says `table_name` "references a value in the
    /// `gpkg_contents` `table_name` column" — not "names a feature layer".
    /// Since 2026-08-24 this crate registers carried pyramids there, so
    /// `gdal_tiles.gpkg`'s reference to `imagery` is valid in the output and
    /// must be kept.
    ///
    /// /This test exists because the plan's first draft wrote the check against
    /// the layer set, which would have destroyed this reference from a file
    /// that still contains `imagery`. Caught by reading the normative text
    /// rather than by anything failing./
    #[test]
    fn a_reference_to_a_carried_pyramid_is_not_a_reference_to_nothing() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        assert!(
            !layers.iter().any(|(name, _)| name == "imagery"),
            "the point of this fixture is that `imagery` is not a layer"
        );

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let refs = references_in(&bytes);
        assert!(
            refs.iter().any(|(scope, table, _, _)| scope == "table" && table == "imagery"),
            "the pyramid's own metadata reference was dropped: {refs:?}"
        );
    }

    /// ***The document outlives the reference that named it*** — D12.
    ///
    /// Dropping a reference must not cascade into the `gpkg_metadata` row it
    /// pointed at. Requirement 95 makes the two tables a pair and says nothing
    /// about every record being referenced; `spec_conformant.gpkg` holds the
    /// pair with zero references and is the fixture built from the normative
    /// text. The foreign keys agree: both run reference → metadata, so
    /// deleting a record can break a surviving reference and deleting a
    /// reference can never break a record.
    #[test]
    fn dropping_every_reference_keeps_the_metadata_it_referenced() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        // Write only `second`: every reference in the fixture names `cities`.
        let only_second: Vec<_> =
            layers.iter().filter(|(n, _)| n == "second").cloned().collect();
        assert_eq!(only_second.len(), 1);

        let bytes = write_layers(None, &only_second, &carried, false, false).unwrap();
        assert_eq!(references_in(&bytes).len(), 0, "the references should all be gone");
        assert_eq!(metadata_rows(&bytes), 1, "the document itself must survive");
        // Requirement 95: the pair travels together, so the reference table is
        // still there — empty, not absent.
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let names: Vec<String> =
            db.tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "gpkg_metadata"), "{names:?}");
        assert!(names.iter().any(|n| n == "gpkg_metadata_reference"), "{names:?}");
    }

    /// ***And the drop is reported*** — D13, the first content this crate can
    /// carry in *part*.
    #[test]
    fn a_partially_carried_pair_says_what_it_lost() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let only_second: Vec<_> =
            layers.iter().filter(|(n, _)| n == "second").cloned().collect();

        let warnings =
            crate::geopackage::metadata_drop_warnings(None, &only_second, &carried);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("3 of 3"), "{}", warnings[0]);
        assert!(warnings[0].contains("cities"), "{}", warnings[0]);

        // And a write that loses nothing says nothing — the noise bar in
        // `plans/lossy-conversion-warnings.org` § THE RULE.
        assert!(crate::geopackage::metadata_drop_warnings(None, &layers, &carried).is_empty());
    }

    /// ***And an append counts the destination's layers, because the write
    /// does.***
    ///
    /// `write_layers` upserts `new_layers` over whatever the destination holds,
    /// so a reference naming a layer only the *destination* has still survives.
    /// A warning computed from `new_layers` alone therefore reports losses the
    /// same run does not incur — which is exactly what this shipped doing:
    /// "3 of 3 metadata references could not be" about three references that
    /// were written into the file.
    ///
    /// /Both callers now go through `compose_layers`, so the message and the
    /// output cannot disagree without the shared function being wrong./
    #[test]
    fn an_append_does_not_report_losses_the_destination_prevents() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let pick = |want: &str| -> Vec<(String, FeatureCollection)> {
            layers.iter().filter(|(n, _)| n == want).cloned().collect()
        };

        // A destination holding `cities` — which every reference names.
        let destination =
            write_layers(None, &pick("cities"), &Carried::default(), false, false).unwrap();
        // Appending `second` into it: the output will hold both layers.
        let appending = pick("second");

        let warnings = crate::geopackage::metadata_drop_warnings(
            Some(&destination),
            &appending,
            &carried,
        );
        assert!(warnings.is_empty(), "nothing is lost on this append: {warnings:?}");

        // And the file agrees, which is the half that was already correct.
        let bytes =
            write_layers(Some(&destination), &appending, &carried, false, false).unwrap();
        assert_eq!(references_in(&bytes).len(), 3, "all three references are written");
    }

    /// ***Requirement 140's two registration rows are written, with a NULL
    /// `column_name`.***
    ///
    /// The annex's record table writes both rows' `column_name` as null and
    /// GDAL stores NULL (measured 2026-08-25). An empty string is a different
    /// value — `ge_tce UNIQUE (table_name, column_name, extension_name)` tells
    /// them apart, and so does every consumer — which is why
    /// `ExtensionRow::column_name` became an `Option`.
    #[test]
    fn writing_the_metadata_pair_registers_the_extension() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();

        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let ext = tables.iter().find(|t| t.name == "gpkg_extensions").expect("registered");
        let at = |c: &str| ext.columns.iter().position(|n| n == c).unwrap();
        let rows = db.read_rows(ext).unwrap();
        let registered: Vec<&Vec<Value>> = rows
            .iter()
            .filter(|r| matches!(&r[at("extension_name")], Value::Text(n) if n == "gpkg_metadata"))
            .collect();
        assert_eq!(registered.len(), 2, "both tables are registered: {rows:?}");
        for row in registered {
            assert!(
                matches!(row[at("column_name")], Value::Null),
                "a table-scoped registration names no column: {row:?}"
            );
            assert!(matches!(&row[at("scope")], Value::Text(s) if s == "read-write"));
        }
    }

    /// ***A reference whose `md_file_id` names no document is dropped***, per
    /// Requirement 101 — and this is the check the constructed DDL's
    /// `crmr_mfi_fk` also gives `foreign_key_check`, which five of this tree's
    /// eight metadata-bearing fixtures could not supply because their
    /// hand-built DDL declares no such key.
    #[test]
    fn a_reference_to_no_document_is_not_carried() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let mut carried = crate::geopackage::read_container(SRC).unwrap();
        let metadata = carried.metadata.as_mut().unwrap();
        // Point the row-scoped reference at a document that is not there.
        let reference = metadata.references.iter_mut().find(|r| r.scope == "row").unwrap();
        reference.md_file_id = 404;

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let refs = references_in(&bytes);
        assert!(
            !refs.iter().any(|(scope, _, _, _)| scope == "row"),
            "a reference to a nonexistent gpkg_metadata row was written: {refs:?}"
        );
        assert_eq!(refs.len(), 2, "and only that one was dropped: {refs:?}");
    }

    /// The R*Tree is built against the ids the feature table is written under,
    /// not against positions.
    ///
    /// An index whose leaf entries name rowids that no row has is not
    /// something `integrity_check` objects to — the shadow tables are
    /// well-formed and merely wrong — so nothing but this would catch it.
    #[test]
    fn the_spatial_index_points_at_the_ids_actually_written() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_fid_gap.gpkg");
        let layers = read_layers(SRC).unwrap();
        let bytes =
            write_layers(None, &layers, &crate::geopackage::Carried::default(), true, false)
                .unwrap();

        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = ours.tables().unwrap();
        let rowid_map = tables.iter().find(|t| t.name == "rtree_cities_geom_rowid").unwrap();
        let mut indexed: Vec<i64> = ours
            .read_rows(rowid_map)
            .unwrap()
            .iter()
            .filter_map(|row| match row[0] {
                Value::Int(n) => Some(n),
                _ => None,
            })
            .collect();
        indexed.sort_unstable();
        assert_eq!(indexed, [1, 9, 40], "the index must name the rows that exist");
    }

    /// A user's own tables survive a `gpkg -> gpkg` write, with their DDL,
    /// their rows, their rowids, their triggers and their indexes.
    ///
    /// `sqlite_constraints.gpkg` is the hostile fixture for this: five tables
    /// no layer owns, between them a `UNIQUE`, a `TEXT PRIMARY KEY`, an
    /// explicit `CREATE INDEX`, a trigger and a `COLLATE NOCASE`.
    #[test]
    fn a_users_own_tables_survive_a_geopackage_write() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_constraints.gpkg");
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let names: Vec<&str> = carried.tables.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["side_notes", "tags", "registry", "events", "people"], "{names:?}");

        let layers = read_layers(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let source = crate::sqlite::Database::open(SRC).unwrap();
        let ours = crate::sqlite::Database::open(&bytes).unwrap();

        let rows = |db: &crate::sqlite::Database, name: &str| {
            let tables = db.tables().unwrap();
            let t = tables
                .iter()
                .find(|t| t.name == name)
                .unwrap_or_else(|| panic!("{name} is missing from the output"));
            db.read_rows(t).unwrap()
        };
        for name in &names {
            assert_eq!(rows(&ours, name), rows(&source, name), "{name}'s rows");
        }

        // The DDL is the source's text, not a re-rendering of it: a carried
        // table is put back, not rebuilt, so anything this crate cannot model
        // (a `CHECK`, a `COLLATE`, a `DEFAULT`) rides along in the statement.
        let ddl = |db: &crate::sqlite::Database, name: &str| {
            db.master_objects()
                .unwrap()
                .into_iter()
                .find(|o| o.kind == "table" && o.name == name)
                .and_then(|o| o.sql)
                .unwrap_or_else(|| panic!("no DDL for {name}"))
        };
        for name in &names {
            assert_eq!(ddl(&ours, name), ddl(&source, name), "{name}'s DDL");
        }

        // The schema objects hanging off them. **Counts, not names**: the
        // 2026-08-24 lesson is that asserting a phrase a fixture satisfies by
        // accident proves nothing, and a count fails the moment either one
        // stops being carried.
        let objects = ours.master_objects().unwrap();
        let of = |kind: &str, tbl: &str| {
            objects.iter().filter(|o| o.kind == kind && o.tbl_name == tbl).count()
        };
        assert_eq!(of("trigger", "events"), 1, "events_stamp");
        assert_eq!(of("index", "events"), 1, "idx_events_kind");
        assert_eq!(of("index", "people"), 1, "the UNIQUE COLLATE NOCASE autoindex");
        assert_eq!(of("index", "registry"), 1, "the TEXT PRIMARY KEY autoindex");
        assert_eq!(of("index", "side_notes"), 0, "no constraint, so no index");
    }

    /// ***An autoindex is written only for a DDL that declares one*** — the
    /// regression test for `handoff.org` § BUGS FOUND entry 20, and the exact
    /// inverse of entry 15.
    ///
    /// Entry 15: a DDL declaring `UNIQUE` with no index behind it is a
    /// malformed database. **This is the other direction**, and it is just as
    /// fatal: an index SQLite cannot attribute to a constraint is an
    /// `orphan index`, and the whole file stops opening.
    ///
    /// `sqlite_contents.gpkg`'s `imagery` is a tile table whose DDL declares
    /// **no** `UNIQUE`, though it has all three of the columns Requirement 54
    /// names. Carrying it by *building* an index over the columns found by name
    /// — which is what shipped on 2026-08-24 — produced a file `sqlite3` will
    /// not open at all. /Restoring that construction fails this test; nothing
    /// else in the suite notices, because the other tile fixture is
    /// conformant and does declare the constraint./
    #[test]
    fn no_autoindex_is_written_for_a_carried_ddl_that_declares_no_unique() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let source = crate::sqlite::Database::open(SRC).unwrap();
        // The premise, asserted rather than assumed: the source's own `imagery`
        // owns no index, because its DDL declares no constraint needing one.
        let src_indexes = source
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "index" && o.tbl_name == "imagery")
            .count();
        assert_eq!(src_indexes, 0, "the fixture's premise");

        let carried = crate::geopackage::read_container(SRC).unwrap();
        assert_eq!(carried.pyramids.len(), 1, "imagery is carried as a pyramid");
        let layers = read_layers(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();

        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let ours_indexes = ours
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "index" && o.tbl_name == "imagery")
            .count();
        assert_eq!(ours_indexes, 0, "an index here is an orphan and the file will not open");
    }

    /// A carried index keeps its collation, and **no collation code exists to
    /// make that true**.
    ///
    /// `people` declares `UNIQUE COLLATE NOCASE`. Re-packing reads the source's
    /// b-tree in index order, so the records arrive already sorted by SQLite's
    /// own `NOCASE` ordering and no key is ever compared — which is exactly the
    /// bargain D10 struck when it scoped the index work to re-packing, and the
    /// first case that actually depends on it.
    #[test]
    fn a_carried_index_keeps_a_collation_this_crate_cannot_compute() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_constraints.gpkg");
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let people = carried.tables.iter().find(|t| t.name == "people").unwrap();
        assert!(people.sql.contains("NOCASE"), "the fixture's premise: {}", people.sql);

        // One autoindex, holding one record per row — re-packed, not rebuilt.
        assert_eq!(people.autoindexes.len(), 1);
        let Storage::Rows(rows) = &people.storage else { panic!("people has a rowid") };
        assert_eq!(people.autoindexes[0].len(), rows.len());

        let source = crate::sqlite::Database::open(SRC).unwrap();
        let root = source
            .master_objects()
            .unwrap()
            .into_iter()
            .find(|o| o.name == "sqlite_autoindex_people_1")
            .expect("the source's own autoindex");
        assert_eq!(people.autoindexes[0], source.read_index(root.root_page).unwrap());
    }

    /// ***A carried pyramid's CRS is registered although no feature layer uses
    /// it*** — M3b requirement 1, and the reason it is a requirement at all.
    ///
    /// `resolve_srs` builds `gpkg_spatial_ref_sys` from the *layer set*, so a
    /// pyramid in EPSG:3857 beside a layer in EPSG:4326 would otherwise point
    /// at a row the output never writes. That dangles past `integrity_check`,
    /// which has no opinion about it, and is caught by `foreign_key_check`
    /// through `fk_gc_r_srs_id`.
    ///
    /// /Deleting `reconcile_carried_srs`'s registration makes this fail, which
    /// is the mutation it exists to catch./
    #[test]
    fn a_carried_pyramids_crs_is_registered_though_no_layer_uses_it() {
        const TILES: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");
        let carried = crate::geopackage::read_container(TILES).unwrap();
        let layers = read_layers(TILES).unwrap();
        // The premise: the only feature layer is 4326, so nothing but the
        // pyramid accounts for 3857.
        assert!(layers.iter().all(|(_, fc)| fc.crs != Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: None,
            projjson: None,
        }))));

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let srs = tables.iter().find(|t| t.name == "gpkg_spatial_ref_sys").unwrap();
        let ids: Vec<i64> = db
            .read_rows(srs)
            .unwrap()
            .iter()
            // srs_id is column 1 and an INTEGER PRIMARY KEY, so `read_rows`
            // has backfilled the rowid into it.
            .filter_map(|r| match r.get(1) {
                Some(Value::Int(n)) => Some(*n),
                _ => None,
            })
            .collect();
        assert!(ids.contains(&3857), "the pyramid's CRS was not registered: {ids:?}");

        // And the pyramid's row points at it.
        let contents = tables.iter().find(|t| t.name == "gpkg_contents").unwrap();
        let row = db
            .read_rows(contents)
            .unwrap()
            .into_iter()
            .find(|r| text(&r[0]) == "imagery")
            .expect("the pyramid is registered");
        assert_eq!(row[9], Value::Int(3857));
    }

    /// A carried `srs_id` that **collides with a different CRS** is renumbered
    /// rather than reused, and both systems survive.
    ///
    /// This is the position-dependent case the requirement was written for:
    /// synthetic ids are handed out in layer order from `SYNTHETIC_SRS_BASE`,
    /// so two files both numbering from it mean one integer names two
    /// different coordinate systems. Reusing it would relabel the pyramid with
    /// somebody else's CRS — a wrong answer that no oracle in this tree
    /// catches, because the file is perfectly well formed.
    #[test]
    fn a_carried_srs_id_that_collides_with_a_layers_is_renumbered() {
        // A layer with an unusable code takes the first synthetic id.
        let mut layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: Vec::new(),
            native: None,
        }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: Some("IGNF".into()),
            code: Some("LAMB93".into()),
            wkt: Some("PROJCS[\"RGF93 Lambert 93\"]".into()),
            projjson: None,
        }));

        // And a pyramid whose source numbered *its* CRS the same way.
        let carried = Carried {
            pyramids: vec![pyramid_at_srs(
                SYNTHETIC_SRS_BASE,
                SrsDef {
                    srs_name: "some other system".into(),
                    organization: "NONE".into(),
                    organization_coordsys_id: SYNTHETIC_SRS_BASE,
                    definition: "PROJCS[\"Not Lambert at all\"]".into(),
                },
            )],
            tables: Vec::new(),
            metadata: None,
        };

        let bytes = write_layers(None, &[("l".into(), layer)], &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let srs = tables.iter().find(|t| t.name == "gpkg_spatial_ref_sys").unwrap();
        let definitions: Vec<String> =
            db.read_rows(srs).unwrap().iter().map(|r| text(&r[4])).collect();
        assert!(definitions.iter().any(|d| d.contains("RGF93")), "{definitions:?}");
        assert!(definitions.iter().any(|d| d.contains("Not Lambert")), "{definitions:?}");

        let contents = tables.iter().find(|t| t.name == "gpkg_contents").unwrap();
        let row = db
            .read_rows(contents)
            .unwrap()
            .into_iter()
            .find(|r| text(&r[0]) == "tiles_table")
            .expect("the pyramid is registered");
        assert_ne!(
            row[9],
            Value::Int(SYNTHETIC_SRS_BASE),
            "the pyramid kept an id the layer had already taken",
        );
    }

    /// A carried table whose name a layer already has is **refused**, not
    /// resolved.
    ///
    /// `gpkg_contents.table_name` is a primary key, so writing both would build
    /// an index with two equal keys — a malformed database. D5 in
    /// `plans/container-reconciliation.org` takes the same position for the
    /// same reason: refusing makes the wrong file impossible rather than
    /// guarded against.
    #[test]
    fn a_carried_table_named_like_a_layer_is_refused() {
        let carried = Carried {
            pyramids: vec![pyramid_at_srs(4326, SrsDef {
                srs_name: "WGS 84".into(),
                organization: "EPSG".into(),
                organization_coordsys_id: 4326,
                definition: WGS84_WKT.into(),
            })],
            tables: Vec::new(),
            metadata: None,
        };
        let layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: Vec::new(),
            native: None,
        }]);
        let err = write_layers(None, &[("tiles_table".into(), layer)], &carried, false, false)
            .expect_err("a name collision must not be written");
        assert!(format!("{err}").contains("tiles_table"), "{err}");
    }

    /// A minimal pyramid registered at `srs_id`, for the reconciliation tests.
    /// Deliberately hand-built rather than read from a fixture: these tests are
    /// about the *numbering*, and a fixture would pin one real CRS.
    fn pyramid_at_srs(srs_id: i64, srs: SrsDef) -> super::Pyramid {
        super::Pyramid {
            name: "tiles_table".into(),
            identifier: "tiles_table".into(),
            description: String::new(),
            last_change: LAST_CHANGE.into(),
            bbox: [Value::Real(0.0), Value::Real(0.0), Value::Real(1.0), Value::Real(1.0)],
            srs_id,
            srs: Some(srs),
            matrix_set: [Value::Real(0.0), Value::Real(0.0), Value::Real(1.0), Value::Real(1.0)],
            matrix: Vec::new(),
            sql: "CREATE TABLE tiles_table (id INTEGER PRIMARY KEY AUTOINCREMENT, zoom_level \
                  INTEGER NOT NULL, tile_column INTEGER NOT NULL, tile_row INTEGER NOT NULL, \
                  tile_data BLOB NOT NULL, UNIQUE (zoom_level, tile_column, tile_row))"
                .into(),
            rows: Vec::new(),
            // The DDL above declares `UNIQUE`, so the table owes exactly one
            // autoindex — empty, because it holds no rows. Supplying none
            // would be the entry-15 shape; supplying one for a DDL without the
            // constraint would be entry 20's.
            autoindexes: vec![Vec::new()],
            autoincrement: Some(crate::sqlite::Autoincrement::Declared),
            triggers: Vec::new(),
        }
    }

    fn text(v: &Value) -> String {
        match v {
            Value::Text(s) => s.clone(),
            other => format!("{other:?}"),
        }
    }

    /// Every registry table this crate writes declares *exactly* what the
    /// GeoPackage standard says it declares — no constraint missing, and none
    /// added.
    ///
    /// The oracle is `tests/fixtures/spec_conformant.gpkg`, built by `sqlite3`
    /// from the normative SQL of OGC 12-128r19 with **no GDAL anywhere in its
    /// provenance**. That is the whole point of it, and the reason this test
    /// is not simply more assertions about the constants: defect 6's first fix
    /// was cross-checked against `ogr2ogr`'s own output and therefore
    /// inherited GDAL's four measured deviations from the standard, three of
    /// them ours until the spec text was read. A differential test against a
    /// reference implementation measures agreement, not conformance — see
    /// `plans/same-format-fidelity.org` § THE ORACLE RULE.
    ///
    /// Comparing whole normalised DDL rather than substrings is what makes
    /// this stronger than its sibling above. The standard's clause is a
    /// **SHALL** in both directions: *"All `gpkg_*` tables and views ... SHALL
    /// have only the specified columns and table constraints."* An extra
    /// column passes every substring assertion ever written and fails here.
    #[test]
    fn the_registry_ddl_is_what_the_standard_says() {
        const SPEC: &[u8] = include_bytes!("../../tests/fixtures/spec_conformant.gpkg");
        const TILES: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");
        let spec_db = crate::sqlite::Database::open(SPEC).unwrap();
        let spec_ddl: Vec<_> = spec_db
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "table" && o.name.starts_with("gpkg_"))
            .filter_map(|o| o.sql.map(|s| (o.name, normalize_ddl(&s))))
            .collect();

        // `--rtree` so `gpkg_extensions` is written, and a layer carrying a
        // schema native so the two Schema extension tables are too. Between
        // them that reaches every registry table this writer can emit.
        let mut layer = fc(vec![Feature {
            geometry: point(1.0, 2.0),
            properties: vec![("name".into(), JsonValue::String("a".into()))],
            native: None,
        }]);
        layer.native = Some(crate::Native::new(
            crate::Format::Gpkg,
            crate::NativePayload::Gpkg(crate::GpkgNative {
                identifier: None,
                description: None,
                last_change: None,
                data_columns: vec![crate::GpkgDataColumn {
                    column_name: "name".into(),
                    name: Some("Name".into()),
                    title: None,
                    description: None,
                    mime_type: None,
                    constraint_name: Some("nonempty".into()),
                }],
                // Non-empty on purpose: `gpkg_data_column_constraints` is
                // written only when a constraint exists, so an empty vector
                // here would silently drop that table out of the comparison.
                constraints: vec![crate::GpkgConstraint {
                    constraint_name: "nonempty".into(),
                    constraint_type: "glob".into(),
                    value: Some("*".into()),
                    min: None,
                    min_is_inclusive: None,
                    max: None,
                    max_is_inclusive: None,
                    description: None,
                }],
            }),
        ));
        // Carrying a pyramid is what makes the writer emit the two tile
        // registry tables at all, so the oracle reaches eight tables rather
        // than six. `gdal_tiles.gpkg` supplies a real one, because a
        // hand-built pyramid would be this crate asserting against its own
        // idea of the shape.
        let carried = crate::geopackage::read_container(TILES).unwrap();
        let bytes =
            write_layers(None, &[("places".into(), layer)], &carried, true, false).unwrap();
        let ours = crate::sqlite::Database::open(&bytes).unwrap();
        let ours_ddl: std::collections::HashMap<String, String> = ours
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "table" && o.name.starts_with("gpkg_"))
            .filter_map(|o| o.sql.map(|s| (o.name, normalize_ddl(&s))))
            .collect();

        // Intersection only, which since 2026-08-25 is every `gpkg_` table
        // `spec_conformant.gpkg` holds: the fixture's `gpkg_metadata` pair used
        // to fall outside it because this crate wrote neither, and the carrying
        // half now brings them in. /That is what this test is for — a new
        // registry table joins the comparison by being written, not by anyone
        // remembering to add an assertion./
        let mut compared = 0;
        for (table, want) in &spec_ddl {
            let Some(got) = ours_ddl.get(table) else {
                continue;
            };
            assert_eq!(got, want, "{table} does not match the standard's DDL");
            compared += 1;
        }

        // Without this the test passes vacuously the day a rename means
        // nothing intersects. Ten tables: the four core ones, the Schema
        // extension's two, the two tile registry tables the carrying half emits
        // (2026-08-24), and the Metadata extension's pair (2026-08-25).
        assert_eq!(compared, 10, "compared {compared} tables, expected 10");

        // And the writer's constants are the same text, which is what makes
        // the six assertions above statements about this crate's output rather
        // than about one file it happened to produce.
        for ddl in [
            SPATIAL_REF_SYS_DDL,
            CONTENTS_DDL,
            GEOMETRY_COLUMNS_DDL,
            EXTENSIONS_DDL,
            DATA_COLUMNS_DDL,
            DATA_COLUMN_CONSTRAINTS_DDL,
            TILE_MATRIX_SET_DDL,
            TILE_MATRIX_DDL,
            METADATA_DDL,
            METADATA_REFERENCE_DDL,
        ] {
            let name = ddl.trim_start_matches("CREATE TABLE ").split_whitespace().next().unwrap();
            let want = spec_ddl.iter().find(|(n, _)| n == name).map(|(_, s)| s);
            assert_eq!(Some(&normalize_ddl(ddl)), want, "{name} constant");
        }
    }

    #[test]
    fn write_then_read_round_trips() {
        let layer = fc(vec![
            Feature {
                geometry: point(1.0, 2.0),
                properties: vec![
                    ("name".into(), JsonValue::String("a".into())),
                    ("n".into(), JsonValue::Int(5)),
                ],
                native: None,
            },
            Feature {
                geometry: point(3.0, 4.0),
                properties: vec![
                    ("name".into(), JsonValue::String("b".into())),
                    ("n".into(), JsonValue::Null),
                ],
                native: None,
            },
        ]);
        let bytes = write_layers(None, &[("places".into(), layer)], &Carried::default(), false, false).unwrap();
        let back = read_layers(&bytes).unwrap();

        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "places");
        assert_eq!(back[0].1.features.len(), 2);
        assert_eq!(back[0].1.features[0].geometry, point(1.0, 2.0));
        // fid is excluded from properties.
        let names: Vec<&str> = back[0].1.features[0].properties.iter().map(|(k, _)| &**k).collect();
        assert_eq!(names, vec!["name", "n"]);
        assert_eq!(back[0].1.features[0].properties[1].1.as_f64(), Some(5.0));
    }

    /// The GeoPackage half of `edit-composability.org` D4's layer-duplication
    /// bug: a layer read back must carry the *table name* as its identity, or
    /// an edit loop cannot tell the writer which layer it is replacing.
    #[test]
    fn a_layer_read_back_knows_its_own_name() {
        let bytes =
            write_layers(None, &[("roads".into(), fc(vec![]))], &Carried::default(), false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.name.as_deref(), Some("roads"));
    }

    /// The failure that bug was actually about, end to end: a middle stage
    /// *deletes* a feature, and the deletion has to stick. Before the reader
    /// set `fc.name`, the edited collection landed in a second table called
    /// `layer` and the pre-edit features stayed standing in `roads` — so a
    /// delete silently became a no-op, which is the worst shape this could
    /// take. Asserted on the surviving rows, not just the table name, because
    /// the table name is the mechanism and this is the consequence.
    #[test]
    fn an_edit_loop_that_deletes_a_feature_does_not_duplicate_the_layer() {
        let feature = |n: f64| Feature {
            geometry: point(n, n),
            properties: vec![("id".into(), JsonValue::Int((n) as i64))],
            native: None,
        };
        let original = fc(vec![feature(1.0), feature(2.0), feature(3.0)]);
        let bytes = write_layers(None, &[("roads".into(), original)], &Carried::default(), false, false).unwrap();

        // Read it back the way a pipeline would, drop the middle feature, and
        // write it into the same GeoPackage using only the name the IR carries.
        let mut edited = read_layers(&bytes).unwrap().remove(0).1;
        edited.features.retain(|f| f.properties[0].1.as_f64() != Some(2.0));
        let name = edited.name.clone().expect("the reader names the layer");
        let after = write_layers(Some(&bytes), &[(name, edited)], &Carried::default(), false, false).unwrap();

        let layers = read_layers(&after).unwrap();
        assert_eq!(layers.len(), 1, "the edit loop appended a duplicate layer");
        assert_eq!(layers[0].0, "roads");
        let ids: Vec<Option<f64>> =
            layers[0].1.features.iter().map(|f| f.properties[0].1.as_f64()).collect();
        assert_eq!(ids, vec![Some(1.0), Some(3.0)], "the deletion did not stick");
    }

    /// **Two writes of the same input produce byte-identical files**, and the
    /// mechanism that makes them so is [`LAST_CHANGE`] being a fixed sentinel
    /// rather than a clock reading.
    ///
    /// *This test exists to be broken deliberately rather than by accident.*
    /// `plans/same-format-fidelity.org` § OPEN QUESTIONS decided that
    /// `gpkg_contents.last_change` should be **carried** from the source, and
    /// stamped fresh only where there is nothing to carry — a cross-format
    /// source. Implementing that removes this property from essentially every
    /// write, because same-format runs only became possible on 2026-08-23 and
    /// nearly everything this tool does is cross-format.
    ///
    /// That is a fine trade to make and a bad one to make *silently*. Without
    /// this test the whole suite stays green while the property disappears, and
    /// it gets rediscovered later as noise in a `cmp` of two outputs. **Whoever
    /// implements the carry-and-stamp rule has to come here and change this
    /// test on purpose**, which is the only point of it.
    ///
    /// /Measured before it was written:/ two release-binary runs two seconds
    /// apart hashed identically, so the property was real rather than assumed.
    ///
    /// *The two assertions are not redundant.* Byte-equality guards the broad
    /// property, but it cannot be trusted alone as a guard against a clock:
    /// two calls in one process can land in the same millisecond and agree by
    /// luck. The sentinel assertion is the deterministic half — a wall-clock
    /// stamp fails it however fast the machine is — and it pins the *literal*
    /// epoch rather than [`LAST_CHANGE`], so that changing the constant is
    /// also a change someone has to make here on purpose.
    ///
    /// **Both halves confirmed by mutation**, which is how the first draft's
    /// bug was found: writing `t == LAST_CHANGE` compared the output against
    /// the constant that produced it and passed happily when the constant was
    /// changed — defect 6's fixture mistake, reproduced in a test written to
    /// guard against that class.
    #[test]
    fn two_writes_of_the_same_input_are_byte_identical() {
        let layer = || {
            fc(vec![Feature {
                geometry: point(1.0, 2.0),
                properties: vec![("name".into(), JsonValue::String("a".into()))],
                native: None,
            }])
        };
        let first = write_layers(None, &[("places".into(), layer())], &Carried::default(), false, false).unwrap();
        let second = write_layers(None, &[("places".into(), layer())], &Carried::default(), false, false).unwrap();
        assert_eq!(first, second, "two writes of the same input differ");

        // The mechanism, asserted separately so a failure says which half went.
        let db = crate::sqlite::Database::open(&first).unwrap();
        let contents = db
            .tables()
            .unwrap()
            .into_iter()
            .find(|t| t.name == "gpkg_contents")
            .expect("gpkg_contents");
        let rows = db.read_rows(&contents).unwrap();
        let stamps: Vec<&crate::sqlite::Value> = rows.iter().map(|r| &r[4]).collect();
        // The literal, deliberately — *not* `LAST_CHANGE`. Comparing the written
        // value against the constant that produced it proves only that the
        // constant equals itself, which is defect 6's fixture mistake in
        // miniature. Confirmed by mutation: with `LAST_CHANGE` on the right the
        // sentinel could be changed to any other value and this still passed.
        assert!(
            stamps
                .iter()
                .all(|v| matches!(v, crate::sqlite::Value::Text(t) if t == "1970-01-01T00:00:00.000Z")),
            "last_change is no longer the fixed epoch sentinel: {stamps:?}"
        );
    }

    #[test]
    fn write_then_read_round_trips_z_and_m() {
        // GeoPackage needed zero new format-specific code for this (see
        // plans/zm-geometry.org's M5) — `gpkg_geometry` just wraps
        // `to_wkb(g)`, and `strip_gpkg_header` already skips whichever
        // envelope size the flags byte declares, so M2's WKB codec alone
        // should carry Z/M through untouched. Confirmed here, not assumed.
        let layer = fc(vec![Feature {
            geometry: Some(Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0))),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("places".into(), layer)], &Carried::default(), false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(
            back[0].1.features[0].geometry,
            Some(Geometry::Point(Position::with_zm(1.0, 2.0, 3.0, 4.0)))
        );
    }

    // --- GPB envelope (plans/envelope.org V2/V3) -----------------------------
    //
    // Layout facts here are confirmed against real `ogr2ogr -f GPKG` output
    // (GDAL 3.13.2 / PROJ 9.8.1), not read off the spec — see `gpb_envelope`.

    /// Pull the GPB header pieces out of the first geometry blob in the file:
    /// the flags byte, the envelope indicator, and the envelope's decoded
    /// doubles. Scans every table rather than taking a name, so it works
    /// unchanged on our own output and on GDAL fixtures whose layer names
    /// differ.
    fn gpb_parts(bytes: &[u8]) -> (u8, u8, Vec<f64>) {
        use crate::sqlite::Database;
        let db = Database::open(bytes).unwrap();
        let blob = db
            .tables()
            .unwrap()
            .iter()
            .filter_map(|t| db.read_rows(t).ok())
            .flatten()
            .find_map(|row| {
                row.iter().find_map(|c| match c {
                    crate::sqlite::Value::Blob(b) if b.starts_with(b"GP") => Some(b.clone()),
                    _ => None,
                })
            })
            .expect("a GPB geometry blob");
        let flags = blob[3];
        let indicator = (flags >> 1) & 0x07;
        let len = match indicator {
            0 => 0,
            1 => 32,
            2 | 3 => 48,
            4 => 64,
            _ => panic!("invalid envelope indicator {indicator}"),
        };
        let doubles = blob[8..8 + len]
            .chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        (flags, indicator, doubles)
    }

    fn linestring(ps: Vec<Position>) -> Option<Geometry> {
        Some(Geometry::LineString(ps))
    }

    #[test]
    fn without_the_flag_output_is_byte_identical_to_before() {
        // The envelope is opt-in precisely so the default path does not move.
        // Same layer, written twice: `--envelope` off must reproduce the
        // historical header exactly (flags 0x01, indicator 0, no bytes).
        let layer = fc(vec![Feature {
            geometry: linestring(vec![Position::new(0.0, 0.0), Position::new(20.0, 10.0)]),
            properties: vec![],
            native: None,
        }]);
        let plain = write_layers(None, &[("l".into(), layer)], &Carried::default(), false, false).unwrap();
        let (flags, indicator, env) = gpb_parts(&plain);
        assert_eq!(flags, 0x01);
        assert_eq!(indicator, 0);
        assert!(env.is_empty());
    }

    #[test]
    fn envelope_flag_writes_indicator_1_for_2d() {
        let layer = fc(vec![Feature {
            geometry: linestring(vec![
                Position::new(0.0, 0.0),
                Position::new(10.0, 10.0),
                Position::new(20.0, 5.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), layer)], &Carried::default(), false, true).unwrap();
        let (flags, indicator, env) = gpb_parts(&bytes);
        assert_eq!(flags, 0x03, "bit 0 little-endian + indicator 1 in bits 1-3");
        assert_eq!(indicator, 1);
        // min/max PAIRS PER DIMENSION, not all-mins-then-all-maxes.
        assert_eq!(env, vec![0.0, 20.0, 0.0, 10.0]);
    }

    #[test]
    fn envelope_flag_writes_indicator_2_for_z() {
        let layer = fc(vec![Feature {
            geometry: linestring(vec![
                Position::with_z(0.0, 0.0, 0.0),
                Position::with_z(10.0, 10.0, 10.0),
                Position::with_z(20.0, 5.0, 3.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), layer)], &Carried::default(), false, true).unwrap();
        let (flags, indicator, env) = gpb_parts(&bytes);
        assert_eq!(flags, 0x05);
        assert_eq!(indicator, 2);
        assert_eq!(env, vec![0.0, 20.0, 0.0, 10.0, 0.0, 10.0]);
    }

    #[test]
    fn m_is_never_encoded_in_the_envelope_matching_gdal() {
        // The envelope is a *spatial* bound, and M is not a spatial dimension —
        // there is no "filter by M" for it to accelerate, which is why GDAL
        // stops at Z and why GeoPackage's own RTree is 2D. Verified across
        // eight geometry families with real `ogr2ogr`: `LINESTRING M` gets
        // indicator 1, `LINESTRING ZM` gets indicator 2 caching the *Z* range.
        // See `gpb_envelope` for the full rationale.
        let m_only = fc(vec![Feature {
            geometry: linestring(vec![
                Position::with_m(0.0, 0.0, 10.0),
                Position::with_m(20.0, 10.0, 30.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), m_only)], &Carried::default(), false, true).unwrap();
        let (_, indicator, env) = gpb_parts(&bytes);
        assert_eq!(
            indicator, 1,
            "M alone must not promote the envelope past XY"
        );
        assert_eq!(env, vec![0.0, 20.0, 0.0, 10.0]);

        let zm = fc(vec![Feature {
            geometry: linestring(vec![
                Position::with_zm(0.0, 0.0, 1.0, 10.0),
                Position::with_zm(20.0, 10.0, 3.0, 30.0),
            ]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), zm)], &Carried::default(), false, true).unwrap();
        let (_, indicator, env) = gpb_parts(&bytes);
        assert_eq!(indicator, 2, "ZM tracks Z only, exactly as GDAL does");
        assert_eq!(
            env,
            vec![0.0, 20.0, 0.0, 10.0, 1.0, 3.0],
            "the Z range, not the M range"
        );
    }

    #[test]
    fn an_empty_geometry_gets_no_envelope_even_with_the_flag() {
        // Nothing to bound: indicator 0, as if the flag were off. GDAL declines
        // an envelope for a degenerate case too (it writes none for a `Point`).
        let layer = fc(vec![Feature {
            geometry: linestring(vec![]),
            properties: vec![],
            native: None,
        }]);
        let bytes = write_layers(None, &[("l".into(), layer)], &Carried::default(), false, true).unwrap();
        let (flags, indicator, env) = gpb_parts(&bytes);
        assert_eq!(flags, 0x01);
        assert_eq!(indicator, 0);
        assert!(env.is_empty());
    }

    #[test]
    fn our_own_envelope_bearing_output_reads_back() {
        // `strip_gpkg_header` skips whichever envelope size the flags byte
        // declares. It has always been exercised by GDAL's files; this proves
        // it against our own, for both indicator sizes.
        for geom in [
            linestring(vec![Position::new(0.0, 0.0), Position::new(20.0, 10.0)]),
            linestring(vec![
                Position::with_z(0.0, 0.0, 1.0),
                Position::with_z(20.0, 10.0, 9.0),
            ]),
        ] {
            let layer = fc(vec![Feature {
                geometry: geom.clone(),
                properties: vec![],
                native: None,
            }]);
            let bytes = write_layers(None, &[("l".into(), layer)], &Carried::default(), false, true).unwrap();
            let back = read_layers(&bytes).unwrap();
            assert_eq!(back[0].1.features[0].geometry, geom);
        }
    }

    #[test]
    fn envelope_bytes_match_gdals_on_the_same_geometry() {
        // The V3 oracle: read a real `ogr2ogr` GeoPackage, rewrite it through
        // our writer with envelopes on, and require the envelope we compute to
        // equal the one GDAL computed — numerically, ordinate for ordinate.
        // A *missing* envelope is only a lost optimization; a *wrong* one is a
        // correctness bug, which is what this catches.
        for (name, fixture) in [
            (
                "2d",
                &include_bytes!("../../tests/fixtures/gdal_linestring_2d.gpkg")[..],
            ),
            (
                "z",
                &include_bytes!("../../tests/fixtures/gdal_linestring_z.gpkg")[..],
            ),
        ] {
            let (_, gdal_indicator, gdal_env) = gpb_parts(fixture);
            assert_ne!(
                gdal_indicator, 0,
                "{name}: fixture must actually carry an envelope"
            );

            let layers = read_layers(fixture).unwrap();
            let ours = write_layers(None, &layers, &Carried::default(), false, true).unwrap();
            let (_, our_indicator, our_env) = gpb_parts(&ours);

            assert_eq!(our_indicator, gdal_indicator, "{name}: indicator");
            assert_eq!(our_env, gdal_env, "{name}: envelope ordinates");
        }
    }

    #[test]
    fn rtree_index_is_emitted_and_round_trips() {
        // A layer big enough to force interior rtree nodes (> one node of cells)
        // and a multi-page sqlite_master (many trigger rows).
        let feats: Vec<Feature> = (0..120)
            .map(|i| Feature { geometry: point(i as f64, (i % 7) as f64), properties: vec![], native: None })
            .collect();
        let bytes = write_layers(None, &[("grid".into(), fc(feats))], &Carried::default(), true, false).unwrap();

        // The virtual table, its shadow tables, and gpkg_extensions are present.
        use crate::sqlite::Database;
        let db = Database::open(&bytes).unwrap();
        let names: Vec<String> = db.tables().unwrap().into_iter().map(|t| t.name).collect();
        assert!(names.iter().any(|n| n == "gpkg_extensions"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_node"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_rowid"));
        assert!(names.iter().any(|n| n == "rtree_grid_geom_parent"));
        // The rtree virtual table itself has rootpage 0, so our table reader
        // (which needs a real root) skips it — a layer read still sees only the
        // feature layer.
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].0, "grid");
        assert_eq!(back[0].1.features.len(), 120);
    }

    #[test]
    fn wgs84_layer_round_trips_as_wgs84() {
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Wgs84);
        let bytes = write_layers(None, &[("p".into(), layer)], &Carried::default(), false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.crs, Some(Crs::Wgs84));
    }

    #[test]
    fn no_crs_layer_round_trips_as_none() {
        // CSV/WKT-style input with no CRS lands on the "undefined" SRS and reads
        // back as no CRS, not a mislabeled 4326.
        let layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        let bytes = write_layers(None, &[("p".into(), layer)], &Carried::default(), false, false).unwrap();
        let back = read_layers(&bytes).unwrap();
        assert_eq!(back[0].1.crs, None);
    }

    #[test]
    fn named_crs_is_registered_and_round_trips() {
        use crate::crs::NamedCrs;
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: Some("PROJCS[\"WGS 84 / Pseudo-Mercator\"]".into()),
            projjson: None,
        }));
        let bytes = write_layers(None, &[("web".into(), layer)], &Carried::default(), false, false).unwrap();

        // A valid, integrity-checkable GeoPackage that DuckDB/GDAL can open.
        use crate::sqlite::Database;
        assert!(Database::open(&bytes).is_ok());

        let back = read_layers(&bytes).unwrap();
        match &back[0].1.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("3857"));
                assert!(n.wkt.is_some());
            }
            other => panic!("expected Named EPSG:3857, got {other:?}"),
        }
    }

    #[test]
    fn alphanumeric_code_falls_back_to_a_synthetic_srs_id() {
        use crate::crs::NamedCrs;
        // A non-numeric authority code (IGNF/OGC/PROJ/NKG-style) can't be
        // GeoPackage's native INTEGER srs_id; it must fall into the same
        // synthetic-id path as "no code at all", never mislabel or fail to
        // write, and the WKT still carries through.
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: Some("IGNF".into()),
            code: Some("LAMB93".into()),
            wkt: Some("PROJCS[\"RGF93 Lambert 93\"]".into()),
            projjson: None,
        }));
        let bytes = write_layers(None, &[("fr".into(), layer)], &Carried::default(), false, false).unwrap();

        use crate::sqlite::Database;
        assert!(Database::open(&bytes).is_ok());

        let back = read_layers(&bytes).unwrap();
        match &back[0].1.crs {
            Some(Crs::Named(n)) => {
                assert_eq!(n.authority.as_deref(), Some("IGNF"));
                assert!(n.wkt.is_some());
                // The read-back path resolves a synthetic organization_coordsys_id
                // (not the original "LAMB93" — GeoPackage's INTEGER column can't
                // hold it), so the recovered code is numeric, not the original.
                assert!(n.code.as_deref().is_some_and(|c| c.parse::<i64>().is_ok()));
            }
            other => panic!("expected Named IGNF, got {other:?}"),
        }
    }

    #[test]
    fn id_less_projjson_writes_definition_via_structural_translation() {
        use crate::crs::NamedCrs;
        // The gap `plans/projjson-to-wkt.org` closes: a GeoParquet source
        // whose PROJJSON carries no authority code at all — the registry
        // can't help (nothing to key a lookup on) — still reaches
        // `gpkg_spatial_ref_sys.definition` via `NamedCrs::structural_wkt`,
        // instead of falling to a bare "undefined".
        let pj = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"custom datum","ellipsoid":{"name":"custom ellipsoid","semi_major_axis":6378137,"inverse_flattening":298.257223563}}}"#;
        let mut layer = fc(vec![Feature { geometry: point(1.0, 2.0), properties: vec![], native: None }]);
        layer.crs = Some(Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: None,
            projjson: Some(pj.into()),
        }));
        let bytes = write_layers(None, &[("custom".into(), layer)], &Carried::default(), false, false).unwrap();

        use crate::sqlite::Database;
        assert!(Database::open(&bytes).is_ok());

        let back = read_layers(&bytes).unwrap();
        match &back[0].1.crs {
            Some(Crs::Named(n)) => {
                let wkt = n.wkt.as_deref().expect("structural translation produced a WKT string");
                assert!(wkt.starts_with("GEOGCS[\"custom\""), "{wkt}");
                assert!(wkt.contains("SPHEROID[\"custom ellipsoid\",6378137,298.257223563]"), "{wkt}");
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn real_fixture_reads_its_declared_srs() {
        // This DuckDB-written fixture declares srs_id 0 ("undefined geographic")
        // on the layer, so faithful pass-through reports no CRS rather than
        // inventing one.
        let bytes = include_bytes!("../../tests/fixtures/points.gpkg");
        let layers = read_layers(bytes).unwrap();
        assert_eq!(layers[0].1.crs, None);
    }

    #[test]
    fn append_adds_a_layer_then_upsert_replaces() {
        let a = fc(vec![Feature { geometry: point(0.0, 0.0), properties: vec![], native: None }]);
        let b = fc(vec![
            Feature { geometry: point(1.0, 1.0), properties: vec![], native: None },
            Feature { geometry: point(2.0, 2.0), properties: vec![], native: None },
        ]);

        let g1 = write_layers(None, &[("a".into(), a)], &Carried::default(), false, false).unwrap();
        let g2 = write_layers(Some(&g1), &[("b".into(), b)], &Carried::default(), false, false).unwrap();
        let layers = read_layers(&g2).unwrap();
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().any(|(n, _)| n == "a"));
        assert!(layers.iter().any(|(n, l)| n == "b" && l.features.len() == 2));

        // Upsert "a" with three features -> still two layers, "a" replaced.
        let a2 = fc(vec![Feature { geometry: point(9.0, 9.0), properties: vec![], native: None }; 3]);
        let g3 = write_layers(Some(&g2), &[("a".into(), a2)], &Carried::default(), false, false).unwrap();
        let layers = read_layers(&g3).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers.iter().find(|(n, _)| n == "a").unwrap().1.features.len(), 3);
    }

    /// ***An `attributes` dataset is carried, and its `gpkg_contents` row with
    /// it*** — Requirement 118, and the second of defect 3's three remaining
    /// capabilities.
    ///
    /// The registration is not decoration on the table: Requirement 118 says
    /// the `gpkg_contents` table SHALL hold a row with `data_type` of
    /// `attributes` for each attributes data table, so the row is what makes
    /// the table a dataset at all. Carrying one without the other is the
    /// half-carry `carry::user_tables` refused to do until 2026-08-25 — and
    /// the reason it gave, that the registry row could not come along, had
    /// been false since the pyramid started carrying its own a day earlier.
    #[test]
    fn an_attributes_dataset_is_carried_with_the_row_requirement_118_obliges() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let notes = carried
            .tables
            .iter()
            .find(|t| t.name == "notes_aspatial")
            .expect("the attributes dataset is carried, not skipped");
        let registration = notes.registration.as_ref().expect("with its registration");
        assert_eq!(registration.data_type, "attributes");
        assert_eq!(registration.identifier.as_deref(), Some("notes_aspatial"));

        // A plain user table in the same file carries none, which is the other
        // arm of the rule: `side_notes` is in the file and in no registry.
        let side = carried.tables.iter().find(|t| t.name == "side_notes").unwrap();
        assert!(side.registration.is_none(), "an unregistered table stays unregistered");

        let layers = read_layers(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();

        // The registration reaches the output, spelled as the source spelled it.
        let contents = tables.iter().find(|t| t.name == "gpkg_contents").unwrap();
        let name_col = contents.columns.iter().position(|c| c == "table_name").unwrap();
        let type_col = contents.columns.iter().position(|c| c == "data_type").unwrap();
        let row = db
            .read_rows(contents)
            .unwrap()
            .into_iter()
            .find(|r| r.get(name_col) == Some(&Value::Text("notes_aspatial".into())))
            .expect("a gpkg_contents row for the carried dataset");
        assert_eq!(row.get(type_col), Some(&Value::Text("attributes".into())));

        // And the table itself, with its rows.
        let notes_out = tables.iter().find(|t| t.name == "notes_aspatial").unwrap();
        assert_eq!(db.read_rows(notes_out).unwrap().len(), 2);
    }

    /// ***A carried dataset keeps the rowids Requirement 119's NOTE gives a
    /// purpose*** — and the property arrives free, which is why it needs a
    /// test.
    ///
    /// Requirement 119 obliges an attributes table to have an INTEGER primary
    /// key acting as a `rowid` alias, and its NOTE says what for: linking to
    /// row-level records in `gpkg_metadata` **by rowid value** through
    /// `gpkg_metadata_reference`. That is word for word what Requirement 29's
    /// NOTE says about a feature table, which is the sentence decision 27 was
    /// taken on.
    ///
    /// `carry::UserTable` already keeps its rowids, so nothing had to be built
    /// — and nothing in the code says the property is load-bearing. The
    /// fixture's ids are **1 and 9**, not 1 and 2: a dense pair cannot tell a
    /// carried rowid from a reissued one, which is the blindness that hid
    /// `handoff.org` entry 19 for four days.
    #[test]
    fn a_carried_datasets_rowids_are_the_sources_own() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let notes = carried.tables.iter().find(|t| t.name == "notes_aspatial").unwrap();
        let Storage::Rows(rows) = &notes.storage else { panic!("an attributes table has a rowid") };
        let ids: Vec<i64> = rows.iter().map(|(rowid, _)| *rowid).collect();
        assert_eq!(ids, vec![1, 9], "the gap is the fixture's whole point");

        let layers = read_layers(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let out = tables.iter().find(|t| t.name == "notes_aspatial").unwrap();
        let col = out.columns.iter().position(|c| c == "id").unwrap();
        let written: Vec<Value> =
            db.read_rows(out).unwrap().into_iter().map(|r| r[col].clone()).collect();
        assert_eq!(
            written,
            vec![Value::Int(1), Value::Int(9)],
            "a reissue would write 1, 2 and every row-scoped reference at 9 would rot"
        );
    }

    /// ***Requirement 97's authority is the output's `gpkg_contents`, so
    /// registering a dataset keeps its references*** — the composition
    /// `plans/same-format-fidelity.org` predicted in the abstract and that had
    /// never had a case.
    ///
    /// Both arms are asserted, because only one of them is new. A reference
    /// naming the carried **dataset** now survives; one naming a carried but
    /// **unregistered** table is still dropped, and must be — Requirement 97
    /// says `table_name` SHALL reference a value in `gpkg_contents`, and
    /// `side_notes` is in the file and in no registry, so a reference to it
    /// would dangle under the very requirement it is being checked against.
    #[test]
    fn a_reference_to_a_carried_dataset_survives_and_one_to_a_plain_table_does_not() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let mut carried = crate::geopackage::read_container(SRC).unwrap();
        let layers = read_layers(SRC).unwrap();

        // The fixture's own reference to the dataset, plus one manufactured at
        // the plain user table beside it.
        let metadata = carried.metadata.as_mut().unwrap();
        let template = metadata.references[0].clone();
        metadata.references.push(MetadataReference {
            table_name: Some("side_notes".into()),
            ..template
        });

        let ids: Vec<Vec<i64>> = layers.iter().map(|(_, fc)| feature_ids(fc)).collect();
        let metadata = carried.metadata.clone().unwrap();
        let (kept, dropped) = retained_references(&metadata, &layers, &ids, &carried);

        let named: Vec<&str> =
            kept.iter().filter_map(|r| r.table_name.as_deref()).collect();
        assert!(named.contains(&"notes_aspatial"), "the dataset is registered: {named:?}");
        assert!(!named.contains(&"side_notes"), "an unregistered table is not: {named:?}");
        assert!(
            dropped.iter().any(|d| d.table_name.as_deref() == Some("side_notes")),
            "and the drop is reported rather than silent"
        );
    }

    /// ***A `row`-scoped reference into a carried dataset survives at the id
    /// it names*** — Requirement 99, in the one place outside a feature layer
    /// it can arise.
    ///
    /// The fixture's reference points at rowid **9**, the gap id, and the
    /// carried table keeps it. This is `container-reconciliation.org` D5's
    /// `row_id_value` hazard reaching a second kind of table: the reference
    /// carries no foreign key, so a renumbering here would rot it silently,
    /// past `integrity_check` and past `foreign_key_check` alike.
    #[test]
    fn a_row_scoped_reference_into_a_carried_dataset_keeps_its_rowid() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let layers = read_layers(SRC).unwrap();
        let ids: Vec<Vec<i64>> = layers.iter().map(|(_, fc)| feature_ids(fc)).collect();
        let metadata = carried.metadata.clone().unwrap();
        let (kept, _) = retained_references(&metadata, &layers, &ids, &carried);

        let row_scoped: Vec<&MetadataReference> = kept
            .iter()
            .filter(|r| r.scope == "row" && r.table_name.as_deref() == Some("notes_aspatial"))
            .collect();
        assert_eq!(row_scoped.len(), 1, "the fixture's row-scoped reference survives");
        assert_eq!(row_scoped[0].row_id_value, Some(9), "at the id it named");
    }

    /// A carried dataset whose `gpkg_contents.identifier` a layer already holds
    /// is **refused**, on the same grounds as the pyramid's collision.
    ///
    /// Requirement 13 makes `identifier` UNIQUE, so writing both would build an
    /// index over two equal keys — a database SQLite reports as malformed.
    #[test]
    fn a_carried_dataset_whose_identifier_is_already_taken_is_refused() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let mut carried = crate::geopackage::read_container(SRC).unwrap();
        let layers = read_layers(SRC).unwrap();
        // `second`'s layer identifier is its own name, so claiming it collides.
        let notes = carried.tables.iter_mut().find(|t| t.name == "notes_aspatial").unwrap();
        notes.registration.as_mut().unwrap().identifier = Some("second".into());

        let err = write_layers(None, &layers, &carried, false, false)
            .expect_err("a duplicate identifier is refused, not resolved");
        assert!(
            format!("{err}").contains("duplicate identifier"),
            "and the message says why: {err}"
        );
    }

    /// A registration's NULL `srs_id` is written NULL, not coerced to a number.
    ///
    /// Requirement 13's table makes the column nullable and Requirement 16
    /// makes any *non-NULL* value a reference that SHALL resolve — so NULL and
    /// `0` are different statements, and `0` is the one that claims an
    /// undefined geographic CRS. `Registration::srs_id` is an `Option` for
    /// this, and `Pyramid`'s `i64` could not express it.
    #[test]
    fn a_null_srs_id_on_a_carried_registration_is_written_null() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let mut carried = crate::geopackage::read_container(SRC).unwrap();
        let layers = read_layers(SRC).unwrap();
        // The fixture stores 0; assert that first, so the mutation below is
        // known to be a change rather than a no-op.
        let notes = carried.tables.iter_mut().find(|t| t.name == "notes_aspatial").unwrap();
        assert_eq!(notes.registration.as_ref().unwrap().srs_id, Some(0));
        notes.registration.as_mut().unwrap().srs_id = None;

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let contents = tables.iter().find(|t| t.name == "gpkg_contents").unwrap();
        let name_col = contents.columns.iter().position(|c| c == "table_name").unwrap();
        let srs_col = contents.columns.iter().position(|c| c == "srs_id").unwrap();
        let row = db
            .read_rows(contents)
            .unwrap()
            .into_iter()
            .find(|r| r.get(name_col) == Some(&Value::Text("notes_aspatial".into())))
            .unwrap();
        assert_eq!(row.get(srs_col), Some(&Value::Null));
    }

    /// ***A carried dataset's CRS is registered although no layer uses it***,
    /// exactly as a pyramid's is — Requirement 16, and the reason
    /// `reconcile_carried_srs` had to stop taking a `&Pyramid`.
    ///
    /// `resolve_srs` rebuilds `gpkg_spatial_ref_sys` from the *layer set* on
    /// every write, so a registration pointing at an id no layer accounts for
    /// dangles unless it is reconciled. That is invisible to
    /// `integrity_check` and is caught by `foreign_key_check` through
    /// `fk_gc_r_srs_id`.
    #[test]
    fn a_carried_datasets_crs_is_registered_though_no_layer_uses_it() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        let mut carried = crate::geopackage::read_container(SRC).unwrap();
        let layers = read_layers(SRC).unwrap();
        // Move the dataset onto a CRS nothing else in the file uses. 3857 is
        // not one of the three rows `spatial_ref_sys` always writes, so it has
        // to be registered or the reference dangles.
        let notes = carried.tables.iter_mut().find(|t| t.name == "notes_aspatial").unwrap();
        let registration = notes.registration.as_mut().unwrap();
        registration.srs_id = Some(3857);
        registration.srs = Some(SrsDef {
            srs_name: "WGS 84 / Pseudo-Mercator".into(),
            organization: "EPSG".into(),
            organization_coordsys_id: 3857,
            definition: "PROJCS[\"WGS 84 / Pseudo-Mercator\"]".into(),
        });

        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let tables = db.tables().unwrap();
        let srs = tables.iter().find(|t| t.name == "gpkg_spatial_ref_sys").unwrap();
        let id_col = srs.columns.iter().position(|c| c == "srs_id").unwrap();
        let ids: Vec<Value> =
            db.read_rows(srs).unwrap().into_iter().map(|r| r[id_col].clone()).collect();
        assert!(
            ids.contains(&Value::Int(3857)),
            "the dataset's CRS has to be registered too: {ids:?}"
        );
    }

    /// ***D14: a dataset registered under a `data_type` this crate does not
    /// model is left behind*** — and still reported, rather than carried as
    /// opaque bytes.
    ///
    /// Requirement 14 permits "an implementer-defined value for other data
    /// tables per clause in an Extended GeoPackage", so carrying one is
    /// expressible: the table is just bytes and the registration is just a
    /// row. **Requirement 59 is what rules it out.** An implementer-defined
    /// `data_type` comes from an extension clause, and the absence of rows in
    /// `gpkg_extensions` SHALL indicate the file is *not* an Extended
    /// GeoPackage — so registering the dataset while emitting no registration
    /// for its extension is `handoff.org` entry 21 committed deliberately, and
    /// about somebody else's extension rather than our own.
    ///
    /// Synthesised here rather than added to a fixture: no producer in this
    /// tree emits one, and putting an uncarryable dataset into
    /// `sqlite_contents.gpkg` would cost that fixture its silence.
    #[test]
    fn a_dataset_under_an_implementer_defined_data_type_is_not_carried() {
        use crate::sqlite::{TableSpec, write_database};

        let contents = TableSpec {
            name: "gpkg_contents".into(),
            sql: CONTENTS_DDL.into(),
            rows: vec![(
                1,
                vec![
                    Value::Text("coverage_stub".into()),
                    Value::Text("2d-gridded-coverage".into()),
                    Value::Text("coverage_stub".into()),
                    Value::Text("".into()),
                    Value::Text(LAST_CHANGE.into()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Int(4326),
                ],
            )],
            autoindexes: vec![Vec::new(), Vec::new()],
            autoincrement: None,
        };
        let stub = TableSpec {
            name: "coverage_stub".into(),
            sql: "CREATE TABLE coverage_stub (id INTEGER PRIMARY KEY, v BLOB)".into(),
            rows: vec![(1, vec![Value::Null, Value::Blob(vec![1, 2, 3])])],
            autoindexes: Vec::new(),
            autoincrement: None,
        };
        let bytes =
            write_database(&[contents, stub], &[], APPLICATION_ID, USER_VERSION).unwrap();

        let carried = crate::geopackage::read_container(&bytes).unwrap();
        assert!(
            !carried.tables.iter().any(|t| t.name == "coverage_stub"),
            "an unmodelled data_type is left behind, not carried as bytes"
        );
        // And left behind means *reported*: the survey still names it, which is
        // what keeps this a stated capability gap rather than a silent drop.
        let warnings =
            super::super::container_drop_warnings(&bytes, "stub.gpkg", &[], &[], false);
        assert!(
            warnings.iter().any(|w| w.contains("coverage_stub")),
            "and it is still reported: {warnings:?}"
        );
    }

    /// ***A trigger that maintains `gpkg_ogr_contents` is dropped with the
    /// table it maintains, and keeping it makes a file that cannot be written
    /// to.***
    ///
    /// `gpkg_ogr_contents` is `super::container`'s entire `DERIVED` class —
    /// recomputable from the layers, so this writer never emits it. `ogr2ogr`
    /// attaches two maintenance triggers to every table it registers,
    /// including an aspatial one, and an `attributes` dataset is the first
    /// carried table that is neither a layer nor a registry table. So the rule
    /// was always in force and this is the first time it needed code.
    ///
    /// Measured with `sqlite3` on 2026-08-25: a trigger naming a missing table
    /// is created without complaint, `integrity_check` answers `ok`,
    /// `foreign_key_check` says nothing, and every `INSERT` then fails with
    /// `no such table: main.gpkg_ogr_contents`. The behavioural half of that is
    /// `tests/gpkg_oracles.rs`; this test owns the structural half.
    ///
    /// **Both arms, because a filter that drops everything would pass the
    /// first.** `sqlite_constraints.gpkg`'s `events_stamp` is a trigger on a
    /// user's own table and must still be carried verbatim.
    #[test]
    fn a_gpkg_ogr_contents_maintenance_trigger_is_not_carried() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
        // The fixture's premise: the source really does hold them.
        let source = crate::sqlite::Database::open(SRC).unwrap();
        let src_triggers: Vec<String> = source
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "trigger" && o.tbl_name == "notes_aspatial")
            .map(|o| o.name)
            .collect();
        assert_eq!(src_triggers.len(), 2, "ogr2ogr's pair: {src_triggers:?}");

        let carried = crate::geopackage::read_container(SRC).unwrap();
        let notes = carried.tables.iter().find(|t| t.name == "notes_aspatial").unwrap();
        assert!(notes.triggers.is_empty(), "not carried: {:?}", notes.triggers);

        let layers = read_layers(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let orphaned: Vec<String> = db
            .master_objects()
            .unwrap()
            .into_iter()
            .filter(|o| o.kind == "trigger" && o.sql.as_deref().is_some_and(|s| s.contains("gpkg_ogr_contents")))
            .map(|o| o.name)
            .collect();
        assert!(orphaned.is_empty(), "these would break every INSERT: {orphaned:?}");

        // The other arm: an ordinary trigger on a user's own table survives.
        const CONSTRAINTS: &[u8] =
            include_bytes!("../../tests/fixtures/sqlite_constraints.gpkg");
        let other = crate::geopackage::read_container(CONSTRAINTS).unwrap();
        let events = other.tables.iter().find(|t| t.name == "events").unwrap();
        assert!(
            events.triggers.iter().any(|(name, _)| name == "events_stamp"),
            "a trigger this crate has no quarrel with is carried: {:?}",
            events.triggers
        );
    }

    /// ***A `WITHOUT ROWID` table survives a `gpkg -> gpkg` run***, and the
    /// writer needed no new capability to do it.
    ///
    /// Such a table has no rowid: SQLite stores it as an **index b-tree**
    /// keyed by the declared primary key, with the whole row living in the
    /// key. `handoff.org`, `todo.org` and `same-format-fidelity.org` all
    /// described this as real file-format work on the grounds that "the
    /// *writer* has never emitted one" — **measured 2026-08-26 and false**.
    /// `write_database` builds a real rootpage from any `MasterEntry` carrying
    /// records and never tests the entry's kind, so the capability arrived
    /// with the index work on 2026-08-23 and nobody tried it. The whole of the
    /// change is reading the source's b-tree with `read_index` instead of
    /// `read_rows` and emitting a `MasterEntry` instead of a `TableSpec`.
    ///
    /// /Like a carried index, the records are **re-packed** in the order the
    /// source stored them and never compared — so the primary key's collation
    /// never has to be implemented. D10's bargain, collected a second time./
    #[test]
    fn a_without_rowid_table_is_carried_as_the_index_btree_it_is() {
        const SRC: &[u8] = include_bytes!("../../tests/fixtures/sqlite_altered.gpkg");
        let carried = crate::geopackage::read_container(SRC).unwrap();
        let kv = carried
            .tables
            .iter()
            .find(|t| t.name == "kv")
            .expect("kv is carried, not skipped");
        assert!(kv.sql.contains("WITHOUT ROWID"), "the fixture's premise: {}", kv.sql);
        let Storage::Records(records) = &kv.storage else {
            panic!("a WITHOUT ROWID table is stored as records, not rows")
        };
        assert_eq!(records.len(), 2);
        // The records are the source's own bytes, re-packed rather than rebuilt.
        let source = crate::sqlite::Database::open(SRC).unwrap();
        let root = source
            .master_objects()
            .unwrap()
            .into_iter()
            .find(|o| o.name == "kv")
            .unwrap()
            .root_page;
        assert_eq!(*records, source.read_index(root).unwrap());

        // And it reaches the output as a `table` entry owning a real b-tree.
        let layers = read_layers(SRC).unwrap();
        let bytes = write_layers(None, &layers, &carried, false, false).unwrap();
        let db = crate::sqlite::Database::open(&bytes).unwrap();
        let out = db
            .master_objects()
            .unwrap()
            .into_iter()
            .find(|o| o.name == "kv")
            .expect("kv is in the output");
        assert_eq!(out.kind, "table");
        assert!(out.root_page > 0, "a schema-only entry would lose every row");
        assert!(out.sql.as_deref().is_some_and(|s| s.contains("WITHOUT ROWID")));
        assert_eq!(db.read_index(out.root_page).unwrap(), *records);
    }

    /// The storage shape is decided by the **DDL**, not by whether the table
    /// walk failed — and reading it the other way would corrupt a file.
    ///
    /// A `WITHOUT ROWID` table's table-walk returns `Err`, so that `Err` looks
    /// like a usable signal. It is not: a walk can fail for reasons unrelated
    /// to storage, and re-reading such a table as an index and writing it back
    /// as one produces bytes that say something its own DDL does not. `kv`
    /// declares the clause and every other carried table does not, which is
    /// the pair that pins the rule.
    #[test]
    fn the_storage_shape_follows_the_ddl_and_not_a_failed_read() {
        // Two fixtures, because no single one holds both arms:
        // `sqlite_altered.gpkg` carries only `kv`, and `sqlite_constraints.gpkg`
        // carries four ordinary tables and no `WITHOUT ROWID` one.
        const ALTERED: &[u8] = include_bytes!("../../tests/fixtures/sqlite_altered.gpkg");
        const CONSTRAINTS: &[u8] =
            include_bytes!("../../tests/fixtures/sqlite_constraints.gpkg");
        let mut records = 0;
        let mut rows = 0;
        for src in [ALTERED, CONSTRAINTS] {
            for table in &crate::geopackage::read_container(src).unwrap().tables {
                let declares = table.sql.to_ascii_uppercase().contains("WITHOUT ROWID");
                match &table.storage {
                    Storage::Records(_) => {
                        assert!(declares, "{} does not declare it", table.name);
                        records += 1;
                    }
                    Storage::Rows(_) => {
                        assert!(!declares, "{} declares it", table.name);
                        rows += 1;
                    }
                }
            }
        }
        // Neither assertion above is vacuous.
        assert_eq!(records, 1, "kv");
        assert!(rows >= 4, "the ordinary tables: {rows}");
    }

    /// ***A table that fails its walk while claiming a rowid is skipped, not
    /// re-read as an index*** — the half of the rule the correlation above
    /// cannot see.
    ///
    /// On every fixture, "the table walk failed" and "the DDL says
    /// `WITHOUT ROWID`" coincide, so a `storage_of` that branched on the failed
    /// read instead of on the DDL passes every other test in this suite. **The
    /// first mutation pass found exactly that**, which is why this test exists.
    ///
    /// The distinguishing input is a source where the two disagree: an index
    /// b-tree sitting under a DDL that declares an ordinary rowid table. Read
    /// by the DDL, it is uncarryable and is reported. Read by the failed walk,
    /// it is carried as records and written back as a `WITHOUT ROWID`-shaped
    /// b-tree under a DDL that says otherwise — a malformed database this crate
    /// would have authored from a source it merely received.
    #[test]
    fn a_table_that_claims_a_rowid_is_never_re_read_as_an_index() {
        use crate::sqlite::{TableSpec, write_database};

        let bytes = write_database(
            &[TableSpec {
                name: "gpkg_contents".into(),
                sql: CONTENTS_DDL.into(),
                rows: Vec::new(),
                autoindexes: vec![Vec::new(), Vec::new()],
                autoincrement: None,
            }],
            &[MasterEntry {
                kind: "table".into(),
                name: "liar".into(),
                tbl_name: "liar".into(),
                // No `WITHOUT ROWID`, so this DDL claims a table b-tree …
                sql: "CREATE TABLE liar (k TEXT PRIMARY KEY, v TEXT)".into(),
                // … while the root page it gets is an index b-tree.
                records: Some(vec![vec![3, 15, 15, b'a', b'1']]),
            }],
            APPLICATION_ID,
            USER_VERSION,
        )
        .unwrap();

        let carried = crate::geopackage::read_container(&bytes).unwrap();
        assert!(
            !carried.tables.iter().any(|t| t.name == "liar"),
            "carrying this would write bytes its own DDL contradicts"
        );
        let warnings =
            super::super::container_drop_warnings(&bytes, "liar.gpkg", &[], &[], false);
        assert!(
            warnings.iter().any(|w| w.contains("liar")),
            "and skipped means reported: {warnings:?}"
        );
    }

    /// A `WITHOUT ROWID` table owning a `sqlite_autoindex_*` is **left
    /// behind**, and that boundary is a writer limit rather than a choice.
    ///
    /// Measured 2026-08-26: only an *additional* `UNIQUE` creates one. A
    /// single-column primary key and a composite primary key each own no
    /// separate b-tree, because for these tables the primary key **is** the
    /// table — SQLite numbers the one it does create `_2`, since `_1` is
    /// notionally that key. There is no way to emit it: `MasterEntry::sql` is a
    /// `String` where SQLite stores NULL for an autoindex, and
    /// `TableSpec::autoindexes` would renumber it `_1`.
    ///
    /// Carrying the table without its index would declare a `UNIQUE` with no
    /// b-tree behind it, which is `handoff.org` entry 15 — a malformed
    /// database. So it is skipped, and `super::container` keeps reporting it.
    #[test]
    fn a_without_rowid_table_with_an_extra_unique_is_left_behind_and_reported() {
        use crate::sqlite::{TableSpec, write_database};

        // Built here because no fixture holds one and no producer emits one.
        // `records` are never read back for this table — the point is that the
        // autoindex exists at all.
        let bytes = write_database(
            &[TableSpec {
                name: "gpkg_contents".into(),
                sql: CONTENTS_DDL.into(),
                rows: Vec::new(),
                autoindexes: vec![Vec::new(), Vec::new()],
                autoincrement: None,
            }],
            &[
                MasterEntry {
                    kind: "table".into(),
                    name: "wr".into(),
                    tbl_name: "wr".into(),
                    sql: "CREATE TABLE wr (k TEXT PRIMARY KEY, e TEXT UNIQUE) WITHOUT ROWID"
                        .into(),
                    records: Some(vec![vec![3, 15, 15, b'a', b'e']]),
                },
                MasterEntry {
                    kind: "index".into(),
                    name: "sqlite_autoindex_wr_2".into(),
                    tbl_name: "wr".into(),
                    sql: String::new(),
                    records: Some(vec![vec![3, 15, 15, b'e', b'a']]),
                },
            ],
            APPLICATION_ID,
            USER_VERSION,
        )
        .unwrap();

        let carried = crate::geopackage::read_container(&bytes).unwrap();
        assert!(
            !carried.tables.iter().any(|t| t.name == "wr"),
            "carrying it without its autoindex would be a malformed database"
        );
        let warnings =
            super::super::container_drop_warnings(&bytes, "wr.gpkg", &[], &[], false);
        assert!(
            warnings.iter().any(|w| w.contains("wr")),
            "and left behind means reported: {warnings:?}"
        );
    }
}
