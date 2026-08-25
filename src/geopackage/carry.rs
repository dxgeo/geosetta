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
//! This module carries the *first* class, and today reaches one member of it:
//! the tile pyramid, which is D8's case in `plans/same-format-fidelity.org`.
//! **Nothing here is raster-specific**, which is D8's own argument for carrying
//! rather than rejecting: a pyramid is carried because it is independent
//! content, by the same operation that will carry `side_notes`, and the tile
//! *bytes* are opaque blobs this crate never decodes.
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
}

impl Carried {
    /// Nothing was carried, so the write proceeds exactly as it did before this
    /// channel existed.
    pub fn is_empty(&self) -> bool {
        self.pyramids.is_empty()
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
    /// Column positions of the `UNIQUE (zoom_level, tile_column, tile_row)`
    /// key, for the autoindex the DDL obliges.
    pub key_columns: Vec<usize>,
    /// The `sqlite_sequence` state for this table, when its DDL declares
    /// `AUTOINCREMENT`.
    pub autoincrement: Option<Autoincrement>,
    /// The table's triggers as `(name, sql)`, carried verbatim because there is
    /// nothing else to reconstruct one from. A GeoPackage tile table carries
    /// six, enforcing `zoom_level`, `tile_column` and `tile_row` on insert and
    /// on update.
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
        let rows = tile_rows(&db, table, rowid_col)?;
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
            key_columns: tile_key_columns(table),
            autoincrement: autoincrement_of(&name, &table_sql(&objects, &name), &sequences),
            triggers: triggers_of(&objects, &name),
            rows,
            name,
        });
    }
    Ok(Carried { pyramids })
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

/// The tile rows as `(rowid, values)`, with the rowid-alias column blanked.
///
/// [`Database::read_rows`] backfills the rowid into that column so a reader
/// sees the value SQLite reports; `crate::sqlite::TableSpec` wants the
/// inverse, because the rowid is the cell's key rather than part of its record.
/// Writing the id back into the record as well would store it twice and change
/// every row's bytes.
fn tile_rows(db: &Database, table: &Table, rowid_col: usize) -> Result<Vec<(i64, Vec<Value>)>> {
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

/// Column positions of a tile table's declared key.
///
/// Requirement 54 makes `(zoom_level, tile_column, tile_row)` a tile table's
/// key, so this reads the columns by name rather than parsing the constraint —
/// and an autoindex over three integers needs no collation, which is what
/// `plans/container-reconciliation.org` § THE MEASUREMENT established when it
/// found no producer emitting a non-`BINARY` one.
///
/// Empty when the table does not have all three, which leaves the spec with no
/// autoindex — correct for a table whose DDL then declares no `UNIQUE`.
fn tile_key_columns(table: &Table) -> Vec<usize> {
    let wanted = ["zoom_level", "tile_column", "tile_row"];
    let found: Vec<usize> = wanted
        .iter()
        .filter_map(|w| table.columns.iter().position(|c| c == w))
        .collect();
    if found.len() == wanted.len() { found } else { Vec::new() }
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
