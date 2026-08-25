//! Container content that belongs to no layer, read verbatim so a
//! `gpkg -> gpkg` write can put it back.
//!
//! [`crate::Native`] carries what a *layer* recorded and the IR does not model.
//! This is the other half: a GeoPackage is a database, and a database holds
//! things no layer owns — a tile pyramid, a user's own table, `gpkg_metadata`.
//! Those belong to the container, which is why [`crate::NativePayload::Gpkg`]
//! deliberately does not hold them (see its doc, and
//! `plans/same-format-fidelity.org` § THE DOMAIN OF THE RULE).
//!
//! # What is carried, and what decides it
//!
//! M3b's three-class contract sorts container content by *what it depends on*:
//!
//! - **independent** — survives any edit to any layer, so it is carried
//!   unconditionally. A tile pyramid; a user's own table.
//! - **layer-coupled** — keyed by a layer, so it must be validated against what
//!   is actually written rather than replayed blindly.
//! - **derived** — recomputed, never carried. An R*Tree, a row-count cache.
//!
//! This module carries the *first* class, and reaches two members of it: the
//! tile pyramid (D8's case in `plans/same-format-fidelity.org`, 2026-08-24)
//! and a user's own table (2026-08-25). **Nothing here is raster-specific**,
//! which was D8's own argument for carrying rather than rejecting — a pyramid
//! is carried because it is independent content, and `side_notes` arrives by
//! the same operation, as that argument predicted it would.
//!
//! *What the second member cost was one distinction the first had blurred.* A
//! pyramid's key is Requirement 54's, so it could be **built**; an arbitrary
//! user table declares whatever it likes, so its indexes must be **re-packed**
//! from the source. Building for both was the shipped bug that re-packing for
//! both now fixes — `handoff.org` § BUGS FOUND entry 20.
//!
//! # Why the pyramid is where it starts
//!
//! Not size — it is the case that *forces* the two hardest requirements at
//! once. Its table declares `AUTOINCREMENT` **and** owns a `UNIQUE`, so it
//! needs `sqlite_sequence` bookkeeping and an autoindex b-tree or the file is
//! malformed (`handoff.org` § BUGS FOUND entries 15 and the `sqlite_sequence`
//! gap). And its `gpkg_contents` row carries an `srs_id` **no feature layer
//! accounts for**, which is § MILESTONES M3b requirement 1 — the reconciliation
//! `writer::resolve_srs` performs on the way out.
//!
//! # What this module does not do
//!
//! It reads *one* container. Merging a carried container into a destination
//! that already has one is `plans/container-reconciliation.org`, which gates
//! M3b's completion and is not started here — a destination's own container
//! content is still destroyed, and still warned about, exactly as before.
//!
//! And it does not yet carry the **metadata pair**. `gpkg_metadata` and
//! `gpkg_metadata_reference` travel together or not at all (Requirement 95: a
//! GeoPackage containing the first SHALL contain the second), and carrying the
//! second correctly needs feature ids this writer does not preserve
//! (Requirement 99, and `handoff.org` § BUGS FOUND entry 19). See
//! `user_tables` for the full list of what is deliberately left behind.

use crate::error::Result;
use crate::sqlite::{Autoincrement, Database, Table, Value};

/// Independent container content read out of one source GeoPackage.
///
/// Empty is the ordinary case: a GeoPackage holding nothing but its layers and
/// the registry tables they require — which is every file this crate itself
/// writes — carries nothing, and an empty [`Carried`] costs the writer nothing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Carried {
    /// Tile pyramids, in `gpkg_contents` order.
    pub pyramids: Vec<Pyramid>,
    /// Tables the container held that belong to no layer and to no registry
    /// this writer constructs, in `sqlite_master` order.
    pub tables: Vec<UserTable>,
}

impl Carried {
    /// Nothing was carried, so the write proceeds exactly as it did before this
    /// channel existed.
    pub fn is_empty(&self) -> bool {
        self.pyramids.is_empty() && self.tables.is_empty()
    }

    /// Every `sqlite_master` name this content will put back into the output.
    ///
    /// [`super::container::container_drop_warnings`] subtracts these from its
    /// survey: a table that is carried is not destroyed, and the warning whose
    /// whole job is to say what a rewrite loses must stop naming them the
    /// moment they survive. **This is the coupling that has to move whenever
    /// the writer's carrying does**, on the same terms as that module's
    /// `REPRODUCED`.
    pub fn reproduced_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for pyramid in &self.pyramids {
            names.push(pyramid.name.clone());
            names.extend(pyramid.triggers.iter().map(|(name, _)| name.clone()));
        }
        if !self.pyramids.is_empty() {
            names.push("gpkg_tile_matrix_set".into());
            names.push("gpkg_tile_matrix".into());
        }
        for table in &self.tables {
            names.push(table.name.clone());
            names.extend(table.triggers.iter().map(|(name, _)| name.clone()));
            names.extend(table.indexes.iter().map(|(name, _, _)| name.clone()));
        }
        names
    }
}

/// One tile pyramid: its `gpkg_contents` registration, the two tile registry
/// tables' rows describing it, and the tile table itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Pyramid {
    /// The tile table's name, which is also its `gpkg_contents.table_name`.
    pub name: String,
    pub identifier: String,
    pub description: String,
    /// Carried, never re-stamped — the same rule the layer channel follows for
    /// this column, and for the same reason: the stamp follows whoever last
    /// changed the content and that is never geosetta. See
    /// `writer::LAST_CHANGE`.
    pub last_change: String,
    /// The `gpkg_contents` bounding box, as stored. Carried rather than
    /// recomputed: this crate never decodes a tile, so it has nothing to
    /// recompute one *from*, and the pyramid's extent is not derived from
    /// anything else in the file.
    pub bbox: [Value; 4],
    /// The pyramid's `srs_id` **in the source's numbering**, which is not the
    /// output's — see `writer::resolve_srs`.
    pub srs_id: i64,
    /// The source's `gpkg_spatial_ref_sys` row for [`Self::srs_id`], so the
    /// output can register a CRS no feature layer accounts for. `None` when the
    /// source itself had no such row, which is a dangling reference this crate
    /// did not create and does not repair.
    pub srs: Option<SrsDef>,
    /// `gpkg_tile_matrix_set`: the pyramid's extent in its own CRS.
    pub matrix_set: [Value; 4],
    /// `gpkg_tile_matrix`, one row per zoom level, in zoom order.
    ///
    /// **Carried whole, not reduced to the levels that hold tiles.** These rows
    /// define the tiling *scheme*, and a real pyramid defines far more levels
    /// than it populates — `gdal_tiles.gpkg` declares 14 for the 2 it stores.
    /// Recomputing them from the tiles present would write a different file and
    /// silently narrow the scheme.
    pub matrix: Vec<MatrixRow>,
    /// The tile table's `CREATE TABLE` text, carried verbatim.
    pub sql: String,
    /// The tile rows as `(rowid, values)`, with the rowid-alias column left
    /// `Null` — the shape `crate::sqlite::TableSpec` wants.
    pub rows: Vec<(i64, Vec<Value>)>,
    /// The `sqlite_autoindex_*` b-trees this table's DDL obliges, re-packed
    /// from the source exactly as [`UserTable::autoindexes`] describes.
    ///
    /// **Read from the source rather than built from Requirement 54's key, and
    /// that distinction is load-bearing.** Until 2026-08-25 this was a list of
    /// column positions found *by name*, and the writer built an index over
    /// them — which writes `sqlite_autoindex_<name>_1` for any table holding
    /// columns called `zoom_level`, `tile_column` and `tile_row` whether or not
    /// its DDL declares the `UNIQUE`. A table with the index and no constraint
    /// to justify it is a **malformed database**: SQLite reports
    /// `orphan index`, and every query against the file fails. See
    /// `handoff.org` § BUGS FOUND entry 20, and note that it is entry 15
    /// exactly inverted — that one supplied a constraint with no index.
    pub autoindexes: Vec<Vec<Vec<u8>>>,
    /// The `sqlite_sequence` state for this table, when its DDL declares
    /// `AUTOINCREMENT`.
    pub autoincrement: Option<Autoincrement>,
    /// The table's triggers as `(name, sql)`, carried verbatim because there is
    /// nothing else to reconstruct one from. A GeoPackage tile table carries
    /// six, enforcing `zoom_level`, `tile_column` and `tile_row` on insert and
    /// on update.
    pub triggers: Vec<(String, String)>,
}

/// An explicit `CREATE INDEX` as `(name, sql, records)`: its `sqlite_master`
/// name, the statement that declared it, and its records in index order.
pub type CarriedIndex = (String, String, Vec<Vec<u8>>);

/// One table the container held that belongs to no layer and to no registry
/// this writer constructs — `side_notes` and its kind.
///
/// This is the *plainest* member of the independent class and the one
/// `plans/same-format-fidelity.org` § MILESTONES M3b names when it says
/// carrying an arbitrary user table verbatim "is already an expressible
/// operation". Everything here is read back out of the source and handed to
/// the writer untouched; nothing interprets a column.
///
/// # Why its rowids survive when a feature layer's do not
///
/// A carried table's rows keep the rowids SQLite gave them, because the whole
/// point is to put back what was there. A *feature layer* is rebuilt from the
/// IR instead, and `super::writer` reissues its ids dense from 1 — which is
/// `handoff.org` § BUGS FOUND entry 19 and is **not** a property this module
/// shares. The distinction matters here for one concrete reason: an index
/// record embeds the rowid it points at, so a carried index is only
/// re-packable while the rowids under it are unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct UserTable {
    pub name: String,
    /// The `CREATE TABLE` text, carried verbatim.
    pub sql: String,
    /// Rows as `(rowid, values)` with the rowid-alias column blanked, the shape
    /// `crate::sqlite::TableSpec` wants. See `rows_blanking_alias` for why.
    pub rows: Vec<(i64, Vec<Value>)>,
    /// The `sqlite_autoindex_*` b-trees the DDL obliges, **re-packed from the
    /// source** rather than rebuilt: each entry is that index's records in
    /// index order, exactly as `crate::sqlite::Database::read_index`
    /// returned them, ordered by the `_1`, `_2`, … suffix SQLite assigns.
    ///
    /// *This is the case D10 scoped the index work for.* An index on disk is
    /// already sorted, so re-packing needs no comparison semantics at all —
    /// no type ordering, no collation. The pyramid takes the other path
    /// (`super::writer::tile_table` builds its autoindex, because its key is
    /// three integers); an arbitrary user table may declare `UNIQUE` over
    /// anything, so it takes this one.
    pub autoindexes: Vec<Vec<Vec<u8>>>,
    /// Explicit `CREATE INDEX` objects as `(name, sql, records)`, re-packed on
    /// the same terms as [`Self::autoindexes`].
    pub indexes: Vec<CarriedIndex>,
    /// The `sqlite_sequence` state, when the DDL declares `AUTOINCREMENT`.
    pub autoincrement: Option<Autoincrement>,
    /// The table's triggers as `(name, sql)`, carried verbatim.
    pub triggers: Vec<(String, String)>,
}

/// A `gpkg_spatial_ref_sys` row, as the source stored it.
#[derive(Debug, Clone, PartialEq)]
pub struct SrsDef {
    pub srs_name: String,
    pub organization: String,
    pub organization_coordsys_id: i64,
    pub definition: String,
}

/// One `gpkg_tile_matrix` row, minus its `table_name`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixRow {
    pub zoom_level: Value,
    pub matrix_width: Value,
    pub matrix_height: Value,
    pub tile_width: Value,
    pub tile_height: Value,
    pub pixel_x_size: Value,
    pub pixel_y_size: Value,
}

/// The `gpkg_contents.data_type` a tile pyramid registers under.
const TILES_DATA_TYPE: &str = "tiles";

/// Read every carryable piece of container content out of `bytes`.
///
/// A source that holds none returns an empty [`Carried`] rather than an error,
/// and so does one this cannot decode *in part*: a pyramid missing a piece it
/// needs is skipped, not fatal. That is the same ruling
/// `container::survey` makes about an unreadable table — refusing to
/// write because one object could not be carried would trade a partial carry
/// for no output at all, and the drop warning still names whatever was skipped.
pub fn read(bytes: &[u8]) -> Result<Carried> {
    let db = Database::open(bytes)?;
    let tables = db.tables()?;
    let objects = db.master_objects()?;
    let sequences = db.sequence_numbers().unwrap_or_default();

    let mut pyramids = Vec::new();
    for (name, identifier, description, last_change, bbox, srs_id) in tile_contents(&db, &tables)? {
        let Some(table) = tables.iter().find(|t| t.name == name) else { continue };
        let Some(rowid_col) = table.rowid_alias() else {
            // Requirement 54 gives a tile table an `INTEGER PRIMARY KEY`, so a
            // table without one is not a tile table this crate recognises.
            // Skipping keeps the rowids honest: without an alias column there
            // is no rowid in what `read_rows` returns to carry.
            continue;
        };
        let Some(matrix_set) = matrix_set_row(&db, &tables, &name)? else { continue };
        let rows = rows_blanking_alias(&db, table, rowid_col)?;
        pyramids.push(Pyramid {
            identifier,
            description,
            last_change,
            bbox,
            srs_id,
            srs: srs_definition(&db, &tables, srs_id)?,
            matrix_set,
            matrix: matrix_rows(&db, &tables, &name)?,
            sql: table_sql(&objects, &name).unwrap_or_default(),
            autoindexes: autoindex_records(&db, &objects, &name)?,
            autoincrement: autoincrement_of(&name, &table_sql(&objects, &name), &sequences),
            triggers: triggers_of(&objects, &name),
            rows,
            name,
        });
    }
    let tables = user_tables(&db, &tables, &objects, &sequences, &pyramids)?;
    Ok(Carried { pyramids, tables })
}

/// Registry tables this writer builds for itself, so carrying one would put
/// back a stale copy of something reconstructed from the layers.
///
/// **Kept in step with `super::container`'s `REPRODUCED`, `CONSTRUCTED`,
/// `LAYER_SCOPED` and `DERIVED` by the test at the bottom of this file**,
/// which fails if either list grows without the other. The lists cannot simply
/// be shared: that module is classifying *what a write destroys* and this one
/// is choosing *what to carry*, and the two agree today only because nothing
/// is yet carried that the warning still reports.
pub(crate) const CONSTRUCTED_TABLES: [&str; 9] = [
    "gpkg_spatial_ref_sys",
    "gpkg_contents",
    "gpkg_geometry_columns",
    "gpkg_extensions",
    "gpkg_tile_matrix",
    "gpkg_tile_matrix_set",
    "gpkg_data_columns",
    "gpkg_data_column_constraints",
    "gpkg_ogr_contents",
];

/// Tables belonging to the independent class: everything the container holds
/// that is not a layer, not a registry table this writer constructs, not
/// derived, and not already carried as part of a pyramid.
///
/// # What is deliberately left behind, and why each is not an oversight
///
/// - **`gpkg_metadata` and `gpkg_metadata_reference`.** Requirement 95 makes
///   them a *pair* — a GeoPackage containing the first SHALL contain the
///   second — so carrying one without the other writes a non-conformant file,
///   and carrying the second correctly needs feature ids this writer does not
///   yet preserve (Requirement 99; `handoff.org` entry 19). They travel
///   together or not at all.
/// - **A dataset registered in `gpkg_contents`** under `attributes`. The table
///   is carryable on exactly these terms, but its registry row is not carried
///   with it, and a table present without its `gpkg_contents` row is content
///   that no reader will find. Skipped whole rather than carried half.
/// - **R\*Tree shadow tables and `gpkg_ogr_contents`**, which are *derived*:
///   `super::container`'s `DERIVED` and the `--rtree` remedy own them.
fn user_tables(
    db: &Database,
    tables: &[Table],
    objects: &[crate::sqlite::MasterObject],
    sequences: &[(String, i64)],
    pyramids: &[Pyramid],
) -> Result<Vec<UserTable>> {
    let registered = contents_names(db, tables)?;
    let mut out = Vec::new();
    for object in objects {
        if object.kind != "table" || object.root_page == 0 {
            continue;
        }
        let name = object.name.as_str();
        if name.starts_with("sqlite_")
            || name.starts_with("rtree_")
            || CONSTRUCTED_TABLES.contains(&name)
            || name == "gpkg_metadata"
            || name == "gpkg_metadata_reference"
            || registered.iter().any(|n| n == name)
            || pyramids.iter().any(|p| p.name == name)
        {
            continue;
        }
        let Some(table) = tables.iter().find(|t| t.name == name) else { continue };
        let Some(sql) = object.sql.clone() else { continue };
        // A table this cannot decode is skipped rather than fatal, on
        // `read`'s own ruling: a partial carry beats no output at all, and
        // `super::container` still names whatever was skipped.
        let Ok(rows) = carried_rows(db, table) else { continue };
        out.push(UserTable {
            autoindexes: autoindex_records(db, objects, name)?,
            indexes: explicit_indexes(db, objects, name)?,
            autoincrement: autoincrement_of(name, &Some(sql.clone()), sequences),
            triggers: triggers_of(objects, name),
            name: name.to_string(),
            sql,
            rows,
        });
    }
    Ok(out)
}

/// Every `gpkg_contents.table_name`, whatever its `data_type`.
///
/// Used as an exclusion: a registered dataset is either a layer the writer
/// rebuilds, a pyramid carried elsewhere, or an `attributes` table whose
/// registry row this pass does not carry. None of the three is a plain user
/// table.
fn contents_names(db: &Database, tables: &[Table]) -> Result<Vec<String>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_contents") else {
        return Ok(Vec::new());
    };
    let Some(name_col) = t.columns.iter().position(|c| c == "table_name") else {
        return Ok(Vec::new());
    };
    Ok(db.read_rows(t)?.iter().filter_map(|row| text_at(row, name_col)).collect())
}

/// Rows as `(rowid, values)` with the rowid alias blanked, for a table that may
/// not have one.
///
/// A table with no `INTEGER PRIMARY KEY` still has rowids — SQLite gives every
/// non-`WITHOUT ROWID` table one — they are simply not exposed as a column, so
/// there is nothing to blank and [`Database::read_rows`] backfills nothing.
/// The rowid is then the row's position, which is what SQLite itself assigns.
fn carried_rows(db: &Database, table: &Table) -> Result<Vec<(i64, Vec<Value>)>> {
    match table.rowid_alias() {
        Some(col) => rows_blanking_alias(db, table, col),
        None => Ok(db
            .read_rows(table)?
            .into_iter()
            .enumerate()
            .map(|(i, row)| ((i + 1) as i64, row))
            .collect()),
    }
}

/// The `sqlite_autoindex_<name>_N` records, ordered by `N`.
///
/// Re-packed rather than rebuilt: `read_index` walks the source's b-tree in
/// index order, so the records come out already sorted and no key is ever
/// compared. See [`UserTable::autoindexes`].
fn autoindex_records(
    db: &Database,
    objects: &[crate::sqlite::MasterObject],
    name: &str,
) -> Result<Vec<Vec<Vec<u8>>>> {
    let prefix = format!("sqlite_autoindex_{name}_");
    let mut found: Vec<(u32, u32)> = objects
        .iter()
        .filter(|o| o.kind == "index" && o.tbl_name == name && o.name.starts_with(&prefix))
        .filter_map(|o| {
            o.name[prefix.len()..].parse::<u32>().ok().map(|n| (n, o.root_page))
        })
        .collect();
    found.sort_by_key(|(n, _)| *n);
    found.iter().map(|(_, root)| db.read_index(*root)).collect()
}

/// Explicit `CREATE INDEX` objects on `name`, as `(name, sql, records)`.
fn explicit_indexes(
    db: &Database,
    objects: &[crate::sqlite::MasterObject],
    name: &str,
) -> Result<Vec<CarriedIndex>> {
    let mut out = Vec::new();
    for object in objects {
        if object.kind != "index" || object.tbl_name != name || object.root_page == 0 {
            continue;
        }
        // An autoindex has no `sql` — SQLite created it on its own behalf — and
        // is carried by `autoindex_records` instead.
        let Some(sql) = object.sql.clone() else { continue };
        out.push((object.name.clone(), sql, db.read_index(object.root_page)?));
    }
    Ok(out)
}

/// The `gpkg_contents` rows registering a tile pyramid, in stored order.
#[allow(clippy::type_complexity)]
fn tile_contents(
    db: &Database,
    tables: &[Table],
) -> Result<Vec<(String, String, String, String, [Value; 4], i64)>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_contents") else {
        return Ok(Vec::new());
    };
    let idx = |name: &str| t.columns.iter().position(|c| c == name);
    let (Some(name_col), Some(type_col), Some(srs_col)) =
        (idx("table_name"), idx("data_type"), idx("srs_id"))
    else {
        return Ok(Vec::new());
    };

    let mut out = Vec::new();
    for row in db.read_rows(t)? {
        if text_at(&row, type_col).as_deref() != Some(TILES_DATA_TYPE) {
            continue;
        }
        let Some(name) = text_at(&row, name_col) else { continue };
        let srs_id = match row.get(srs_col) {
            Some(Value::Int(n)) => *n,
            _ => continue,
        };
        let cell = |column: &str| {
            idx(column).and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null)
        };
        out.push((
            name,
            idx("identifier").and_then(|i| text_at(&row, i)).unwrap_or_default(),
            idx("description").and_then(|i| text_at(&row, i)).unwrap_or_default(),
            idx("last_change").and_then(|i| text_at(&row, i)).unwrap_or_default(),
            [cell("min_x"), cell("min_y"), cell("max_x"), cell("max_y")],
            srs_id,
        ));
    }
    Ok(out)
}

/// This pyramid's `gpkg_tile_matrix_set` extent. Absent means the file does not
/// describe the pyramid well enough to carry it.
fn matrix_set_row(db: &Database, tables: &[Table], name: &str) -> Result<Option<[Value; 4]>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_tile_matrix_set") else {
        return Ok(None);
    };
    let idx = |column: &str| t.columns.iter().position(|c| c == column);
    let Some(name_col) = idx("table_name") else { return Ok(None) };
    for row in db.read_rows(t)? {
        if text_at(&row, name_col).as_deref() != Some(name) {
            continue;
        }
        let cell = |column: &str| {
            idx(column).and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null)
        };
        return Ok(Some([cell("min_x"), cell("min_y"), cell("max_x"), cell("max_y")]));
    }
    Ok(None)
}

/// This pyramid's `gpkg_tile_matrix` rows, in stored order.
fn matrix_rows(db: &Database, tables: &[Table], name: &str) -> Result<Vec<MatrixRow>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_tile_matrix") else {
        return Ok(Vec::new());
    };
    let idx = |column: &str| t.columns.iter().position(|c| c == column);
    let Some(name_col) = idx("table_name") else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for row in db.read_rows(t)? {
        if text_at(&row, name_col).as_deref() != Some(name) {
            continue;
        }
        let cell = |column: &str| {
            idx(column).and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null)
        };
        out.push(MatrixRow {
            zoom_level: cell("zoom_level"),
            matrix_width: cell("matrix_width"),
            matrix_height: cell("matrix_height"),
            tile_width: cell("tile_width"),
            tile_height: cell("tile_height"),
            pixel_x_size: cell("pixel_x_size"),
            pixel_y_size: cell("pixel_y_size"),
        });
    }
    Ok(out)
}

/// The source's definition of `srs_id`, or `None` when it has no row for it.
fn srs_definition(db: &Database, tables: &[Table], srs_id: i64) -> Result<Option<SrsDef>> {
    let Some(t) = tables.iter().find(|t| t.name == "gpkg_spatial_ref_sys") else {
        return Ok(None);
    };
    let idx = |column: &str| t.columns.iter().position(|c| c == column);
    let Some(id_col) = idx("srs_id") else { return Ok(None) };
    for row in db.read_rows(t)? {
        if !matches!(row.get(id_col), Some(Value::Int(n)) if *n == srs_id) {
            continue;
        }
        let text = |column: &str| {
            idx(column).and_then(|i| text_at(&row, i)).unwrap_or_default()
        };
        let org_id = idx("organization_coordsys_id")
            .and_then(|i| row.get(i))
            .and_then(|v| match v {
                Value::Int(n) => Some(*n),
                _ => None,
            })
            .unwrap_or(srs_id);
        return Ok(Some(SrsDef {
            srs_name: text("srs_name"),
            organization: text("organization"),
            organization_coordsys_id: org_id,
            definition: text("definition"),
        }));
    }
    Ok(None)
}

/// Rows as `(rowid, values)`, with the rowid-alias column blanked.
///
/// [`Database::read_rows`] backfills the rowid into that column so a reader
/// sees the value SQLite reports; `crate::sqlite::TableSpec` wants the
/// inverse, because the rowid is the cell's key rather than part of its record.
/// Writing the id back into the record as well would store it twice and change
/// every row's bytes.
fn rows_blanking_alias(db: &Database, table: &Table, rowid_col: usize) -> Result<Vec<(i64, Vec<Value>)>> {
    let mut out = Vec::new();
    for mut row in db.read_rows(table)? {
        let rowid = match row.get(rowid_col) {
            Some(Value::Int(n)) => *n,
            _ => continue,
        };
        row[rowid_col] = Value::Null;
        out.push((rowid, row));
    }
    Ok(out)
}

/// This table's `CREATE TABLE` text as `sqlite_master` stored it.
fn table_sql(objects: &[crate::sqlite::MasterObject], name: &str) -> Option<String> {
    objects
        .iter()
        .find(|o| o.kind == "table" && o.name == name)
        .and_then(|o| o.sql.clone())
}

/// The `sqlite_sequence` state to carry for `name`.
///
/// `None` when the DDL does not declare `AUTOINCREMENT`; [`Autoincrement::Declared`]
/// when it does and SQLite holds no mark, which is a table that has never been
/// inserted into and still needs the `sqlite_sequence` *table* to exist.
fn autoincrement_of(
    name: &str,
    sql: &Option<String>,
    sequences: &[(String, i64)],
) -> Option<Autoincrement> {
    let declared = sql.as_deref()?.to_ascii_uppercase().contains("AUTOINCREMENT");
    if !declared {
        return None;
    }
    Some(match sequences.iter().find(|(t, _)| t == name) {
        Some((_, seq)) => Autoincrement::HighWater(*seq),
        None => Autoincrement::Declared,
    })
}

/// Every trigger attached to `name`, as `(name, sql)` in `sqlite_master` order.
fn triggers_of(objects: &[crate::sqlite::MasterObject], name: &str) -> Vec<(String, String)> {
    objects
        .iter()
        .filter(|o| o.kind == "trigger" && o.tbl_name == name)
        .filter_map(|o| o.sql.clone().map(|sql| (o.name.clone(), sql)))
        .collect()
}

fn text_at(row: &[Value], index: usize) -> Option<String> {
    match row.get(index) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}
