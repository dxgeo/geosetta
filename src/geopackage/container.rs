//! What a GeoPackage write destroys *around* the layers, and the warning that
//! says so.
//!
//! [`super::writer::write_layers`] is read-modify-write: it decodes the layers,
//! upserts, and re-emits a complete file built from the tables [`super::writer`]
//! knows how to construct, **plus whatever `super::carry` hands it**. What is
//! neither built nor carried does not survive: `gpkg_metadata` and its
//! references, extension registrations, spatial indexes, views. That is
//! *defect 3* in `plans/same-format-fidelity.org`, whose fix travels with the
//! container milestone (M3b) and is *partly landed* — a tile pyramid survives
//! as of 2026-08-24 and a user's own tables as of 2026-08-25, so this warning
//! now reports a shrinking remainder rather than everything.
//!
//! **Which means the `carried` argument is load-bearing rather than
//! decorative.** A table this module still names while the writer puts it back
//! is a warning that lies, and the coupling has no compiler behind it — see
//! [`REPRODUCED`] and `super::carry::Carried::reproduced_names`.
//!
//! This module is the *warning*, shipped ahead of the fix per that plan's
//! § SCOPE ("defect 3's warning now and its fix with the container work"). The
//! standing rule in `plans/lossy-conversion-warnings.org` says a silent drop of
//! information the source actually carries is a bug regardless of whether the
//! fix is ready, so the loss was reported first and repaired after.
//!
//! **Most of it is now repaired**, which changes what this module is for. A
//! tile pyramid survives a `gpkg -> gpkg` run as of 2026-08-24; a user's own
//! tables, the `gpkg_metadata` pair and both extensions' `gpkg_extensions`
//! registrations as of 2026-08-25, and nine of the twelve `.gpkg` fixtures
//! round-trip with this warning silent. What it still reports is a genuine
//! remainder — a `WITHOUT ROWID` table, an `attributes` dataset, an extension
//! this crate does not implement — plus the whole of a *destination's* own
//! container on the append path, which is `container-reconciliation.org`.
//!
//! /So the failure mode has inverted. This warning began as the honest report
//! of a writer that destroyed nearly everything; it is now mostly silent, and
//! the risk is **naming something that survives**. Three such false reports
//! were found and fixed on 2026-08-25 alone./
//!
//! **This file states what the writer reproduces, so it has to move when the
//! writer does.** [`REPRODUCED`] is that statement; a new always-emitted table
//! in [`super::writer::build`] belongs in it, or this module starts warning
//! about a table that in fact survives.

use crate::error::Result;
use crate::sqlite::{Database, MasterObject, Value};

/// The tables [`super::writer::build`] emits unconditionally, and so the
/// container content that survives a write today. `gpkg_extensions` is *not*
/// here: it is written only when some extension is in use, and is handled by
/// row rather than by table — see the `gpkg_extensions` arm of [`survey`],
/// which counts only the rows nothing puts back.
///
/// **Unconditional is the operative word.** Content that survives only when it
/// was carried does not belong here — it arrives per call, as `carried`, from
/// [`super::carry::Carried::reproduced_names`].
const REPRODUCED: [&str; 3] =
    ["gpkg_spatial_ref_sys", "gpkg_contents", "gpkg_geometry_columns"];

/// The registry tables [`super::writer::build`] constructs *conditionally* —
/// it emits each only when something needs it. Together with [`REPRODUCED`]
/// these are the tables the writer builds rather than carries, which is what
/// makes a **trigger attached to one of them derived**: see
/// [`is_derived_trigger`].
const CONSTRUCTED: [&str; 3] =
    ["gpkg_extensions", "gpkg_tile_matrix", "gpkg_tile_matrix_set"];

/// Content that is *derived* under M3b's three-class contract: recomputable
/// from what survives, so its absence is not a loss of information.
///
/// `gpkg_ogr_contents` is `(table_name TEXT PRIMARY KEY, feature_count
/// INTEGER)` — a **row-count cache**, and GDAL's own extension rather than the
/// standard's. Counting it as destroyed content was *bug 13*, filed 2026-08-22
/// and fixed 2026-08-24: it is the same mistake as counting an R*Tree
/// registration in `gpkg_extensions`, which `foreign_extension_rows` already
/// avoids. Reported silently rather than in the index message, on that
/// precedent: the R*Tree earns a message because `--rtree` is a remedy, and a
/// cache with no remedy and no lost information would be furniture on every
/// `ogr2ogr` file.
///
/// *Under editing this is the sharpest case in the container* — carry the
/// cache after a middle stage deletes a feature and the output states a count
/// that is a lie — which is why it is classed derived rather than merely
/// ignored.
const DERIVED: [&str; 1] = ["gpkg_ogr_contents"];

/// The Schema extension's tables, which are *layer* content living in
/// container-scoped tables: their rows are keyed by `(table_name,
/// column_name)`, and M3's GeoPackage spoke carries them on each layer's
/// [`crate::GpkgNative`], so a write puts them back.
///
/// Excluded from the survey on the same grounds as the layers themselves: this
/// warning is about the container *around* the layers, and a row belonging to
/// a layer is not the container's. That also keeps `--layer` out of it — rows
/// for a layer the user deliberately did not select are no more a loss than
/// the layer is.
const LAYER_SCOPED: [&str; 2] = ["gpkg_data_columns", "gpkg_data_column_constraints"];

/// The `gpkg_extensions.extension_name` the RTree extension registers under,
/// as [`super::rtree::extension_rows`] writes it.
const RTREE_EXTENSION: &str = "gpkg_rtree_index";

/// The `gpkg_extensions.extension_name` the Metadata extension registers
/// under, for both of its rows (Requirement 140's record table).
///
/// Its rows are *reproduced* whenever the pair is carried — the writer emits
/// them from `writer::metadata_extension_rows` — so counting them as destroyed
/// would be **bug 13's mistake in a second table**: reporting content that
/// survives. Unlike [`RTREE_EXTENSION`] the exclusion is conditional, because
/// the rows come back only when there is a pair to register.
const METADATA_EXTENSION: &str = "gpkg_metadata";

/// The `gpkg_extensions.extension_name` the Schema extension registers under,
/// as `writer::schema_extension_rows` writes it.
///
/// Excluded **unconditionally**, and that is [`LAYER_SCOPED`]'s reasoning
/// rather than [`METADATA_EXTENSION`]'s: the Schema tables' rows belong to the
/// layers, so this warning excludes the tables themselves whether or not a
/// given write produces them. A registration of those tables is the same
/// content one level up and gets the same treatment — anything else would
/// report the registration as destroyed while saying nothing about the table
/// it registers.
const SCHEMA_EXTENSION: &str = "gpkg_schema";

/// How many objects of a kind are named individually before the message
/// switches to a count. Six is enough to name every registry table a normal
/// `ogr2ogr` GeoPackage carries; past that the list stops informing and starts
/// scrolling (`plans/lossy-conversion-warnings.org` § THE RULE, on noise).
const MAX_NAMED: usize = 6;

/// Warnings for everything in `bytes` that a GeoPackage write will not
/// reproduce. `label` names the container in the message (a path, or `stdin`);
/// `written` is the layers the write will produce, which survive as feature
/// tables — and which also bound the index report, since an index on a layer
/// that is not being written cannot be rebuilt by writing one. With `rtree`, an
/// index is being built, so the source's own index is replaced rather than
/// lost.
///
/// Returns up to two messages: the container content that is destroyed, and —
/// separately, because it is *derived* content with a one-flag remedy rather
/// than an outright loss (D4 in `plans/same-format-fidelity.org`) — the spatial
/// index that is not rebuilt.
///
/// Silent on a container holding nothing but its layers and the required
/// registry tables, which is every GeoPackage this crate itself writes.
///
/// This is the pre-write predictive shape of
/// `plans/lossy-conversion-warnings.org` § THE TWO EXISTING MECHANISMS, with
/// one deviation it names here rather than hiding: the other three checks take
/// a `FeatureCollection` and a target `Format`, because what they report is a
/// capability gap in the target. This one takes *bytes*, because the loss is
/// not in the target's capabilities at all — the target is GeoPackage, which
/// represents all of this perfectly well — but in this crate's writer. Same
/// contract, different question, so it collects in `main.rs` beside them
/// instead of growing a path of its own.
pub fn container_drop_warnings(
    bytes: &[u8],
    label: &str,
    written: &[String],
    carried: &[String],
    rtree: bool,
) -> Vec<String> {
    match survey(bytes, written, carried) {
        Ok(s) => s.into_warnings(label, rtree),
        // Not the error path. Every caller has already read this container (or
        // is about to), so a file this cannot open is reported — with a better
        // message than a warning could give — by the read that follows.
        //
        // Only a file that is not a readable database at all arrives here: a
        // *table* this cannot decode is classified by `survey` instead, because
        // swallowing that one would drop the warning for the whole container.
        Err(_) => Vec::new(),
    }
}

/// What a survey of the container found, already classified.
#[derive(Default)]
struct Survey {
    /// Tables holding rows that the write will not reproduce, as
    /// `(name, rows)` — the actual data loss. A `None` count is a table whose
    /// rows this crate cannot decode (a `WITHOUT ROWID` table is stored as an
    /// index b-tree, which [`crate::sqlite`] does not walk): it is still
    /// content that will not survive, and is reported without a number rather
    /// than not reported at all.
    populated: Vec<(String, Option<usize>)>,
    /// Tables the write will not reproduce that hold no rows: registry tables
    /// a producer created and never filled. Their *existence* is information
    /// (it records which extensions a file was prepared for) but no row of a
    /// user's data goes with them, so they are counted, not named.
    empty: usize,
    /// Triggers, views and indexes that will not be reproduced.
    schema: usize,
    /// Feature tables carrying an R*Tree index in the source.
    indexed: Vec<String>,
}

fn survey(bytes: &[u8], written: &[String], carried: &[String]) -> Result<Survey> {
    let db = Database::open(bytes)?;
    let objects = db.master_objects()?;
    let tables = db.tables()?;

    // Layer content is out of scope here: this warning is about the container
    // *around* the layers, and what happens to a layer's own unmodelled members
    // is M3's business. Excluding them also keeps `--layer` out of it — a
    // deliberately selected subset must not read as an accidental loss.
    let layers = feature_layers(&db, &objects, &tables);

    let mut survey = Survey::default();
    for object in &objects {
        // SQLite's own bookkeeping (`sqlite_sequence`, the `sqlite_autoindex_*`
        // entries a UNIQUE constraint creates). These follow the DDL rather
        // than carrying anything, so re-emitting the DDL re-creates whichever
        // of them the new file needs; reporting them would be reporting noise.
        if object.name.starts_with("sqlite_") {
            continue;
        }
        if is_rtree(&object.name) {
            // Derived, and reported as an index rather than as a loss.
            continue;
        }
        if REPRODUCED.contains(&object.name.as_str())
            || LAYER_SCOPED.contains(&object.name.as_str())
            || DERIVED.contains(&object.name.as_str())
            || layers.contains(&object.name)
            || written.contains(&object.name)
            // Carried by `super::carry`, so it survives the write and is not a
            // loss to report. A trigger on a carried table arrives here too,
            // which is why this test is by *name* rather than by kind: the six
            // triggers a tile table owns would otherwise still be counted
            // among the schema objects said to be destroyed.
            || carried.contains(&object.name)
            || is_derived_trigger(object, &layers, carried)
        {
            continue;
        }
        if object.kind != "table" || object.root_page == 0 {
            survey.schema += 1;
            continue;
        }
        // `gpkg_extensions` is the one table whose rows are counted rather
        // than its existence: an R*Tree registration is *derived* — it travels
        // with the index it declares, which `indexed` already reports, and
        // counting it here would report one index loss twice. Rows for any
        // other extension are a real loss and are what gets named.
        if object.name == "gpkg_extensions" {
            match tables.iter().find(|t| t.name == object.name) {
                Some(table) => match foreign_extension_rows(&db, table, carried) {
                    // Nothing but R*Tree registrations (or nothing at all):
                    // wholly derived, so not a loss of its own in either the
                    // populated or the empty sense.
                    Ok(0) => {}
                    Ok(n) => survey.populated.push((object.name.clone(), Some(n))),
                    Err(_) => survey.populated.push((object.name.clone(), None)),
                },
                None => survey.schema += 1,
            }
            continue;
        }
        match tables.iter().find(|t| t.name == object.name) {
            // Reading rows costs a b-tree walk, which is why it happens only
            // for tables already known not to survive: registry tables and the
            // occasional user table, never the layers.
            // A table this cannot decode is *not* an error here. It is
            // container content like any other, and aborting the survey would
            // trade one unreadable table for silence about the whole file —
            // which is what `container_drop_warnings` would do with the `Err`.
            Some(table) => match db.read_rows(table).map(|r| r.len()) {
                // ***An empty registry table this writer constructs on demand
                // is not destroyed content.*** It is written whenever
                // something needs it, so its absence from an output that needs
                // nothing loses exactly nothing — and reporting it made the
                // drop warning fire on the most ordinary file there is: a
                // plain `ogr2ogr` GeoPackage with no pyramid says
                // "2 empty tables", meaning `gpkg_tile_matrix` and
                // `gpkg_tile_matrix_set`.
                //
                // **Conditional on being empty, and that is the whole
                // subtlety.** These same two tables are a *real* loss when
                // they are populated and the pyramid they describe is not
                // carried — which is exactly what the append path reports
                // against a destination holding tiles. Excluding `CONSTRUCTED`
                // outright, the way `REPRODUCED` and `LAYER_SCOPED` are
                // excluded above, would have silenced that. /Measured before
                // the change: a `points.gpkg` write into a tiled destination
                // names `gpkg_tile_matrix (14 rows)`, and still does./
                Ok(0) if CONSTRUCTED.contains(&object.name.as_str()) => {}
                Ok(0) => survey.empty += 1,
                Ok(n) => survey.populated.push((object.name.clone(), Some(n))),
                // A table the *table* walk cannot decode is a `WITHOUT ROWID`
                // table, which SQLite stores as an index b-tree — so ask the
                // index walk instead before giving up on a count. Until
                // 2026-08-23 this reported `(rows not counted)`, which was the
                // honest answer while `sqlite.rs` could not read an index.
                Err(_) => {
                    let counted = db.read_index(object.root_page).map(|r| r.len()).ok();
                    match counted {
                        Some(0) => survey.empty += 1,
                        other => survey.populated.push((object.name.clone(), other)),
                    }
                }
            },
            None => survey.schema += 1,
        }
    }
    survey.indexed = indexed_layers(&objects, written);
    Ok(survey)
}

impl Survey {
    fn into_warnings(self, label: &str, rtree: bool) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Some(w) = self.loss_warning(label) {
            warnings.push(w);
        }
        if !rtree && !self.indexed.is_empty() {
            warnings.push(format!(
                "{label} carries an R*Tree spatial index on {}; the output is written without \
                 one — pass --rtree to rebuild it.",
                join_names(&self.indexed)
            ));
        }
        warnings
    }

    fn loss_warning(&self, label: &str) -> Option<String> {
        if self.populated.is_empty() && self.empty == 0 && self.schema == 0 {
            return None;
        }
        let mut parts = Vec::new();
        if !self.populated.is_empty() {
            let mut text: Vec<String> = self
                .populated
                .iter()
                .take(MAX_NAMED)
                .map(|(name, rows)| match rows {
                    Some(n) => format!("{name} ({n} {})", plural(*n, "row")),
                    None => format!("{name} (rows not counted)"),
                })
                .collect();
            if self.populated.len() > MAX_NAMED {
                text.push(format!("and {} more", self.populated.len() - MAX_NAMED));
            }
            parts.push(text.join(", "));
        }
        if self.empty > 0 {
            parts.push(format!("{} empty {}", self.empty, plural(self.empty, "table")));
        }
        if self.schema > 0 {
            parts.push(format!(
                "{} other schema {} (triggers, views, indexes)",
                self.schema,
                plural(self.schema, "object")
            ));
        }
        Some(format!(
            "writing a GeoPackage rewrites the whole file, and this writer reproduces only the \
             feature layers and the tables they require — so the rest of {label} is not carried: \
             {}. The layers themselves are written in full. \
             (plans/same-format-fidelity.org, defect 3.)",
            parts.join("; ")
        ))
    }
}

/// Whether `object` is a trigger whose loss is *derived* rather than content.
///
/// A trigger is attached to a table. When that table is one the write
/// **rebuilds** — a layer, or a registry table [`super::writer::build`]
/// constructs — the trigger is not container content that vanishes; it is
/// enforcement or maintenance of a table that comes back, and what it enforced
/// either still holds or is recomputed.
///
/// ***The case that made this necessary, and it is a large one.*** `ogr2ogr`
/// writes **ten** validation triggers on `gpkg_tile_matrix` alone, enforcing
/// the value constraints of Requirements 46--52 — `zoom_level` not negative,
/// the widths and pixel sizes greater than zero — on insert and on update. So
/// on any GeoPackage holding a pyramid, ten of the twelve "other schema
/// objects" this warning reported as destroyed were those. /Measured on
/// `gdal_tiles.gpkg`, 2026-08-24, the day the pyramid started surviving./
///
/// **The standard does not mandate them**, which is what settles the
/// classification rather than a judgement about how useful they are:
/// Requirements 46--52 constrain the *values in the table*, and the tiles
/// clause (OGC 12-128r19 § 2.2, read at pinned `fe83fdd`) does not mention a
/// trigger anywhere. GDAL's ten are one implementation of enforcing a value
/// requirement, so a file without them is conformant as long as its values are
/// — and this writer carries those values verbatim. Producer practice, not the
/// standard: `plans/same-format-fidelity.org` § THE ORACLE RULE.
///
/// /What is genuinely given up is future enforcement: an `INSERT` of a bad row
/// into this crate's output is caught by neither. That is a loss of a guard
/// rather than of information, and it is not what a warning about **what a
/// rewrite destroys** is counting./
fn is_derived_trigger(object: &MasterObject, layers: &[String], carried: &[String]) -> bool {
    object.kind == "trigger"
        && (layers.contains(&object.tbl_name)
            || carried.contains(&object.tbl_name)
            || REPRODUCED.contains(&object.tbl_name.as_str())
            || CONSTRUCTED.contains(&object.tbl_name.as_str())
            || LAYER_SCOPED.contains(&object.tbl_name.as_str())
            || DERIVED.contains(&object.tbl_name.as_str()))
}

/// How many `gpkg_extensions` rows register something other than the R*Tree
/// index. A table whose every row is an R*Tree registration holds nothing this
/// writer is losing independently of the index itself.
fn foreign_extension_rows(
    db: &Database,
    table: &crate::sqlite::Table,
    carried: &[String],
) -> Result<usize> {
    let Some(idx) = table.columns.iter().position(|c| c == "extension_name") else {
        // No such column: not a table this can reason about, so every row counts.
        return Ok(db.read_rows(table)?.len());
    };
    // The Metadata extension's registrations survive exactly when its pair
    // does, which `carried` is the record of.
    let metadata_survives = carried.iter().any(|name| name == "gpkg_metadata");
    Ok(db
        .read_rows(table)?
        .iter()
        .filter(|row| {
            let Some(Value::Text(name)) = row.get(idx) else { return true };
            if name == RTREE_EXTENSION || name == SCHEMA_EXTENSION {
                return false;
            }
            !(name == METADATA_EXTENSION && metadata_survives)
        })
        .count())
}

/// The tables named by a `gpkg_contents` row whose `data_type` is `features` —
/// the same test [`super::reader::read_layers`] applies, and it has to be the
/// same one.
///
/// This list is what the survey treats as *out of scope because it is a layer*.
/// So anything wrongly in it is not reported as a loss **and** is not written
/// either, since the writer only emits feature layers: it vanishes at exit 0.
/// A GDAL `attributes` table (what `ogr2ogr` registers an aspatial table as)
/// and a tile pyramid are both `gpkg_contents` rows that are not layers, and
/// both used to land here.
///
/// **The version before this took every row whatever its `data_type`, and said
/// in its own doc comment that it did so deliberately so that a populated tile
/// table "should at minimum say it is being dropped rather than vanish".** It
/// achieved the exact reverse. Worth keeping in view when reading the rest of
/// this module: the intent was recorded correctly and the code did the
/// opposite, so the comment could not catch it and neither could a test that
/// trusted the comment.
fn feature_layers(db: &Database, objects: &[MasterObject], tables: &[crate::sqlite::Table]) -> Vec<String> {
    let Some(table) = tables.iter().find(|t| t.name == "gpkg_contents") else {
        return Vec::new();
    };
    // A container with no `gpkg_contents` b-tree is not a GeoPackage; leave it
    // to the reader to reject.
    if !objects.iter().any(|o| o.name == "gpkg_contents") {
        return Vec::new();
    }
    let Some(idx) = table.columns.iter().position(|c| c == "table_name") else {
        return Vec::new();
    };
    // No `data_type` column: a `gpkg_contents` this cannot classify by. Treat
    // every row as a layer, which is this function's pre-2026-08-23 behaviour
    // and errs toward under-reporting — the alternative on a malformed
    // registry is to report every feature layer in the file as destroyed.
    let kind = table.columns.iter().position(|c| c == "data_type");
    let Ok(rows) = db.read_rows(table) else {
        return Vec::new();
    };
    rows.iter()
        .filter(|row| match kind {
            Some(k) => matches!(row.get(k), Some(Value::Text(t)) if t == "features"),
            None => true,
        })
        .filter_map(|row| match row.get(idx) {
            Some(Value::Text(name)) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// The layers *being written* that the container carries an R*Tree index for.
///
/// Attribution comes from the maintenance triggers' `tbl_name`, which names the
/// indexed feature table exactly, rather than from the shadow tables'
/// `rtree_<table>_<column>` names: splitting those on `_` cannot tell layer
/// `a` with a geometry column `b_geom` from layer `a_b` with `geom`, and would
/// report an index on whichever it guessed.
///
/// Filtered against `written` rather than against every layer the source holds,
/// on the same grounds the loss survey excludes the layers themselves: an index
/// on a layer `--layer` deliberately left out is not a loss, and naming it would
/// offer `--rtree` as a remedy that cannot apply — the writer builds an index
/// for the layers it writes, and that layer is not one of them.
fn indexed_layers(objects: &[MasterObject], written: &[String]) -> Vec<String> {
    let mut indexed: Vec<String> = objects
        .iter()
        .filter(|o| is_rtree(&o.name) && o.kind == "trigger")
        .map(|o| o.tbl_name.clone())
        .filter(|table| written.contains(table))
        .collect();
    indexed.sort_unstable();
    indexed.dedup();
    indexed
}

/// The GeoPackage RTree extension's own objects: the `rtree_<table>_<column>`
/// virtual table, its `_node`/`_parent`/`_rowid` shadow tables, and the
/// triggers that maintain them. All derived — rebuilt from the geometry rather
/// than carried — which is why they are reported as an index that was not
/// rebuilt rather than as content that was destroyed (D4).
fn is_rtree(name: &str) -> bool {
    name.starts_with("rtree_")
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { word.to_string() } else { format!("{word}s") }
}

/// `a`, `a and b`, `a, b and c` — the form the CRS warnings already use.
fn join_names(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::{Feature, FeatureCollection};
    use crate::geometry::{Geometry, Position};

    /// `ogr2ogr` output carrying everything a real container carries: a user
    /// table, `gpkg_metadata`, an OGR feature-count table, tile-matrix
    /// registry tables, and an R*Tree per layer. See
    /// `tests/fixtures/HOSTILE.org`.
    const HOSTILE: &[u8] = include_bytes!("../../tests/fixtures/gdal_hostile.gpkg");

    /// The same `cities` layer plus a `WITHOUT ROWID` user table, which SQLite
    /// stores as an *index* b-tree. See `tests/fixtures/HOSTILE.org`.
    const ALTERED: &[u8] = include_bytes!("../../tests/fixtures/sqlite_altered.gpkg");

    /// The same base plus two `gpkg_contents` rows that are *not* feature
    /// layers: an `attributes` table and a tile pyramid. See
    /// `tests/fixtures/HOSTILE.org`.
    const CONTENTS: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");

    fn layer(name: &str) -> (String, FeatureCollection) {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: Vec::new(),
            native: None,
        }]);
        (name.to_string(), fc)
    }

    fn names(layers: &[(String, FeatureCollection)]) -> Vec<String> {
        layers.iter().map(|(n, _)| n.clone()).collect()
    }

    /// A table whose rows this crate cannot decode must still be *reported*.
    ///
    /// `WITHOUT ROWID` tables live in index b-trees, which `crate::sqlite`
    /// does not walk. Until 2026-08-22 the resulting `Err` propagated out of
    /// `survey` and `container_drop_warnings` swallowed it, so **one such
    /// table anywhere in the file silenced the warning for the entire
    /// container** — including the R*Tree message, which has nothing to do
    /// with it. Defect 3 went back to being exactly as silent as it was before
    /// this module shipped, on a file that converts at exit 0.
    #[test]
    fn a_table_this_cannot_decode_is_reported_rather_than_silencing_the_survey() {
        let written = vec!["cities".to_string()];
        let warnings = container_drop_warnings(ALTERED, "in.gpkg", &written, &[], false);
        let loss = &warnings[0];
        // Counted, as of 2026-08-23. A `WITHOUT ROWID` table is stored as an
        // index b-tree, and `sqlite.rs` can now read one — so the fallback this
        // test was written against ("kv (rows not counted)", the honest answer
        // while the rows were genuinely unreachable) is no longer reached here.
        // The fallback itself is still live for a table neither walk can decode.
        assert!(loss.contains("kv (2 rows)"), "{loss}");
        // The rest of the survey is unaffected: the tables it *can* read are
        // still counted, which is what the abort used to take down with it.
        // /This asserted `gpkg_ogr_contents (1 row)` until 2026-08-24, when that
        // table was reclassified as derived — bug 13. `gpkg_metadata` is the
        // same assertion about the same property, on a table that really is
        // content./
        assert!(loss.contains("gpkg_metadata (1 row)"), "{loss}");
        // And the second warning still fires. This is the one that proves the
        // bug was scope rather than formatting: an undecodable table has no
        // bearing on whether the source carries a spatial index.
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[1].contains("R*Tree spatial index on cities"), "{warnings:?}");
    }

    /// ***The two modules' lists of "tables this writer builds for itself" say
    /// the same thing***, and nothing but this test makes them.
    ///
    /// `super::carry` uses its list to decide **what to carry**; this module
    /// uses its four to decide **what a write destroys**. They are separate
    /// judgements over the same set, so they cannot simply share a constant —
    /// but a table appearing in one and not the other is a bug in whichever
    /// direction it goes: carried *and* reported is a lie in the warning,
    /// reported *and* not carried is a table that silently vanishes.
    #[test]
    fn the_two_lists_of_constructed_tables_agree() {
        let mut here: Vec<&str> = REPRODUCED
            .iter()
            .chain(CONSTRUCTED.iter())
            .chain(LAYER_SCOPED.iter())
            .chain(DERIVED.iter())
            .copied()
            .collect();
        here.sort_unstable();
        let mut there = crate::geopackage::carry::CONSTRUCTED_TABLES.to_vec();
        there.sort_unstable();
        assert_eq!(here, there);
    }

    /// A table that is *carried* stops being named as destroyed, which is the
    /// whole coupling between `super::carry` and this module.
    ///
    /// /Asserted as a **count** of what remains, not as the absence of one
    /// name: the 2026-08-24 lesson is that a message can satisfy a phrase for
    /// reasons unrelated to what is being tested./
    #[test]
    fn a_carried_table_is_no_longer_reported_as_destroyed() {
        const HOSTILE: &[u8] = include_bytes!("../../tests/fixtures/gdal_hostile.gpkg");
        let written = vec!["cities".to_string(), "second".to_string()];

        let before = container_drop_warnings(HOSTILE, "in.gpkg", &written, &[], false);
        assert!(before[0].contains("side_notes (1 row)"), "the premise: {}", before[0]);

        let after =
            container_drop_warnings(HOSTILE, "in.gpkg", &written, &["side_notes".into()], false);
        assert!(!after[0].contains("side_notes"), "{}", after[0]);
        // And nothing else moved: the two metadata tables are still reported,
        // because they are still destroyed.
        assert!(after[0].contains("gpkg_metadata (1 row)"), "{}", after[0]);
        assert!(after[0].contains("gpkg_metadata_reference (1 row)"), "{}", after[0]);
    }

    /// ***The Metadata extension's own `gpkg_extensions` rows stop being
    /// counted as destroyed once the pair is carried.***
    ///
    /// This is **bug 13's mistake in a second table** and nothing else would
    /// catch it: `gpkg_extensions` is reported by *row count*, so two rows that
    /// come back in the output would go on being reported as lost while the
    /// table's own name never appears in `carried`. /Found by mutation-checking
    /// the tests for the metadata pair — every other new rule had a test and
    /// this one did not./
    #[test]
    fn a_reproduced_extension_registration_is_not_reported_as_destroyed() {
        const TILES: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");
        let written = vec!["cities".to_string()];
        // `gdal_tiles.gpkg` holds five registrations: two Metadata, two Schema,
        // one R*Tree. The R*Tree row is always excluded (it is reported as an
        // index instead) and the Schema rows are excluded unconditionally on
        // `LAYER_SCOPED`'s reasoning, so two are counted while the pair is not
        // carried.
        let carried_pyramid: Vec<String> =
            vec!["imagery".into(), "gpkg_tile_matrix".into(), "gpkg_tile_matrix_set".into()];
        let before = container_drop_warnings(TILES, "in.gpkg", &written, &carried_pyramid, false);
        assert!(before[0].contains("gpkg_extensions (2 rows)"), "the premise: {}", before[0]);

        // Carrying the pair reproduces its two registrations, which leaves
        // nothing in `gpkg_extensions` to report at all — so the table stops
        // being named rather than being named with a smaller number.
        let mut carried_pair = carried_pyramid.clone();
        carried_pair.push("gpkg_metadata".into());
        carried_pair.push("gpkg_metadata_reference".into());
        let after = container_drop_warnings(TILES, "in.gpkg", &written, &carried_pair, false);
        assert!(!after[0].contains("gpkg_extensions"), "{}", after[0]);
    }

    /// ***And the Schema extension's own registrations are never counted***,
    /// on the same grounds as [`LAYER_SCOPED`] itself.
    ///
    /// Those rows register `gpkg_data_columns` and
    /// `gpkg_data_column_constraints`, whose *rows* travel with the layers and
    /// which this survey therefore excludes unconditionally. Counting the
    /// registration while excluding the table it registers would report half a
    /// thing — and since 2026-08-25 the writer emits those rows anyway
    /// (Requirement 141), so counting them would be reporting content that
    /// survives.
    ///
    /// /Isolated by **count** rather than by absence, because no fixture in
    /// this tree holds Schema registrations alone. `sqlite_altered.gpkg` holds
    /// all three kinds — two Metadata, two Schema, one R\*Tree — and is asked
    /// with nothing carried, so the two Metadata rows *are* a real loss and are
    /// the whole of it. Reporting four would mean the Schema pair had been
    /// counted; reporting none would mean the Metadata pair had wrongly been
    /// excluded. Only two is right, which is what makes this assertion say
    /// something the neighbouring test does not./
    #[test]
    fn a_schema_extension_registration_is_never_counted_as_destroyed() {
        const ALTERED: &[u8] = include_bytes!("../../tests/fixtures/sqlite_altered.gpkg");
        let db = Database::open(ALTERED).unwrap();
        let tables = db.tables().unwrap();
        let ext = tables.iter().find(|t| t.name == "gpkg_extensions").unwrap();
        assert_eq!(db.read_rows(ext).unwrap().len(), 5, "the premise: 2 + 2 + 1");

        let warnings =
            container_drop_warnings(ALTERED, "in.gpkg", &["cities".to_string()], &[], false);
        assert!(
            warnings[0].contains("gpkg_extensions (2 rows)"),
            "the two Metadata rows are the loss; the Schema pair is not: {}",
            warnings[0]
        );
    }

    /// ***An empty registry table this writer constructs on demand is not
    /// destroyed content -- but a populated one still is.***
    ///
    /// Both halves are asserted here because the rule is only correct as a
    /// pair, and the tempting simplification breaks the second half. Adding
    /// `CONSTRUCTED` to the unconditional exclusion list beside `REPRODUCED`
    /// and `LAYER_SCOPED` -- which is what "the survey is missing a class"
    /// suggests -- would silence a **real** loss: the append path writes into a
    /// destination whose tile registries are populated and whose pyramid is not
    /// carried, and 15 rows describing a tiling scheme would vanish at exit 0.
    ///
    /// /The false report was the ordinary case, which is why it mattered: a
    /// plain `ogr2ogr` GeoPackage with no pyramid at all said
    /// "2 empty tables" -- `gpkg_tile_matrix` and `gpkg_tile_matrix_set`,
    /// present and empty because GDAL creates them, and rebuilt by this writer
    /// the moment anything needs them. That is furniture on every file, which
    /// `plans/lossy-conversion-warnings.org` § THE RULE forbids./
    #[test]
    fn an_empty_constructed_registry_is_not_a_loss_but_a_populated_one_is() {
        const HOSTILE: &[u8] = include_bytes!("../../tests/fixtures/gdal_hostile.gpkg");
        const TILES: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");

        // The premise: `gdal_hostile.gpkg` holds both tile registry tables and
        // both are empty, because `ogr2ogr` creates them for every GeoPackage.
        let db = Database::open(HOSTILE).unwrap();
        let tables = db.tables().unwrap();
        for name in ["gpkg_tile_matrix", "gpkg_tile_matrix_set"] {
            let t = tables.iter().find(|t| t.name == name).expect(name);
            assert_eq!(db.read_rows(t).unwrap().len(), 0, "{name} should be empty");
        }
        // Everything else in that file is carried, so the warning must be
        // silent rather than quieter.
        let carried = crate::geopackage::read_container(HOSTILE).unwrap();
        let warnings = container_drop_warnings(
            HOSTILE,
            "in.gpkg",
            &["cities".to_string(), "second".to_string()],
            &carried.reproduced_names(),
            true,
        );
        assert!(warnings.is_empty(), "an empty registry is not content: {warnings:?}");

        // And the other half: populated, with the pyramid they describe not
        // carried. Reached by passing no carried names at all — which since
        // the § 3 merge landed is no longer what an append does, but is still
        // exactly the question this warning has to answer: a populated
        // registry whose pyramid is not coming along is a real loss.
        let populated = container_drop_warnings(TILES, "dest.gpkg", &["cities".into()], &[], true);
        assert!(
            populated[0].contains("gpkg_tile_matrix (14 rows)"),
            "a populated registry is a real loss and must still be named: {}",
            populated[0]
        );
    }

    /// ***`sqlite_contents.gpkg` round-trips with the drop warning silent***,
    /// which is what carrying the `attributes` dataset was for.
    ///
    /// The file holds every class this crate models at once — two feature
    /// layers, a tile pyramid, a plain user table, the `gpkg_metadata` pair and
    /// an `attributes` dataset — and the last of those was the only thing left
    /// speaking. **Asserted end to end rather than by inspecting a class list**:
    /// this module was not edited for the dataset at all, so the claim worth
    /// pinning is that the survey and the carry still agree, not that either
    /// one was changed.
    #[test]
    fn the_file_holding_every_modelled_class_at_once_is_carried_whole() {
        let carried = crate::geopackage::read_container(CONTENTS).unwrap();
        let written = vec!["cities".to_string(), "second".to_string()];
        let warnings = container_drop_warnings(
            CONTENTS,
            "in.gpkg",
            &written,
            &carried.reproduced_names(),
            true,
        );
        assert!(warnings.is_empty(), "nothing in this file is a loss now: {warnings:?}");

        // The premise, so a carry that silently stopped carrying would fail
        // here rather than pass by reporting nothing about nothing.
        assert!(carried.tables.iter().any(|t| t.name == "notes_aspatial"));
        assert!(carried.tables.iter().any(|t| t.name == "side_notes"));
        assert_eq!(carried.pyramids.len(), 1);
        assert!(carried.metadata.is_some());
    }

    /// A trigger on a table that is **genuinely destroyed** is still reported,
    /// which is the half of [`is_derived_trigger`] that nothing else exercises.
    ///
    /// Every trigger in every other fixture is attached to something the write
    /// rebuilds — a layer, or a registry table — so all of them are derived and
    /// the rule could be widened to "all triggers" without a single test
    /// noticing. `sqlite_constraints.gpkg` grew `events_stamp` on 2026-08-24
    /// for exactly that reason: `events` is a user table this writer does not
    /// reproduce, so its trigger goes down with it and the message has to say
    /// so.
    #[test]
    fn a_trigger_on_a_destroyed_table_is_still_reported() {
        const CONSTRAINTS: &[u8] = include_bytes!("../../tests/fixtures/sqlite_constraints.gpkg");
        let written = vec!["cities".to_string(), "second".to_string()];
        let warnings = container_drop_warnings(CONSTRAINTS, "in.gpkg", &written, &[], false);
        let loss = &warnings[0];
        assert!(loss.contains("events (2 rows)"), "the table itself: {loss}");
        // **The count, not the phrase.** This asserted only that the message
        // said "other schema objects", and passed under a mutation that
        // classified *every* trigger as derived — because `idx_events_kind`,
        // an explicit `CREATE INDEX` in the same fixture, satisfies the phrase
        // on its own. Two objects: that index, and `events_stamp`. Found by
        // mutation-testing this test on the day it was written.
        assert!(loss.contains("2 other schema objects"), "its trigger: {loss}");
    }

    #[test]
    fn the_container_around_the_layers_is_reported_object_by_object() {
        let written = vec!["cities".to_string(), "second".to_string()];
        let warnings = container_drop_warnings(HOSTILE, "in.gpkg", &written, &[], false);
        let loss = &warnings[0];
        // The user's own table is the one a reader of this message cares most
        // about, and it is named with its row count rather than counted.
        assert!(loss.contains("side_notes (1 row)"), "{loss}");
        assert!(loss.contains("gpkg_metadata (1 row)"), "{loss}");
        assert!(loss.contains("gpkg_metadata_reference (1 row)"), "{loss}");
        assert!(loss.contains("in.gpkg"), "{loss}");
        // ***And no "2 empty tables", corrected 2026-08-25.*** Those two were
        // `gpkg_tile_matrix` and `gpkg_tile_matrix_set`, which `ogr2ogr`
        // creates for every GeoPackage and leaves empty — and which this
        // writer constructs whenever a pyramid needs them. An empty registry
        // table that comes back on demand is not destroyed content, and saying
        // so on every ordinary file is the furniture
        // `plans/lossy-conversion-warnings.org` § THE RULE forbids.
        //
        // /This assertion previously read `contains("2 empty tables")` and was
        // pinning the bug. A populated one is still reported, which
        // `an_empty_constructed_registry_is_not_a_loss_but_a_populated_one_is`
        // holds down./
        assert!(!loss.contains("empty table"), "{loss}");

        // ***Two things this message used to say and no longer does, both
        // corrected 2026-08-24.***
        //
        // `gpkg_ogr_contents` is a row-count cache — *bug 13*, and the same
        // mistake as counting an R*Tree registration in `gpkg_extensions`.
        assert!(!loss.contains("gpkg_ogr_contents"), "{loss}");
        // And **no** schema objects are left to report here, where the message
        // used to claim 14. This file's 28 triggers are 14 R*Tree ones
        // (already derived), 10 that `ogr2ogr` puts on `gpkg_tile_matrix` to
        // enforce Requirements 46–52 — a table the writer rebuilds, and value
        // constraints the standard states without mandating a trigger — and 4
        // maintaining `gpkg_ogr_contents`. Every one of them is attached to
        // something the write reconstructs.
        assert!(!loss.contains("other schema object"), "{loss}");
    }

    #[test]
    fn a_layer_is_never_reported_as_container_content() {
        // Both layers, and then just one: `--layer cities` drops `second` from
        // the output, but that is a selection the user asked for, not a loss
        // the container warning has any business reporting.
        for written in [vec!["cities".to_string(), "second".to_string()], vec!["cities".to_string()]] {
            let warnings = container_drop_warnings(HOSTILE, "in.gpkg", &written, &[], false);
            let joined = warnings.join(" | ");
            assert!(!joined.contains("cities ("), "{joined}");
            assert!(!joined.contains("second ("), "{joined}");
        }
    }

    #[test]
    fn a_container_this_crate_wrote_itself_says_nothing() {
        // The rule against warning noise: the overwhelmingly common input is a
        // GeoPackage with nothing in it but layers and required tables, and a
        // warning that fires on that one teaches users to ignore it.
        let layers = vec![layer("a"), layer("b")];
        let bytes = super::super::write_layers(None, &layers, &crate::geopackage::Carried::default(), false, false).unwrap();
        assert_eq!(
            container_drop_warnings(&bytes, "own.gpkg", &names(&layers), &[], false),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_rtree_registration_is_not_a_loss_beside_the_index_it_declares() {
        let layers = vec![layer("a")];
        let bytes = super::super::write_layers(None, &layers, &crate::geopackage::Carried::default(), true, false).unwrap();

        // Written again with an index: the index is rebuilt, so nothing at all.
        assert_eq!(
            container_drop_warnings(&bytes, "own.gpkg", &names(&layers), &[], true),
            Vec::<String>::new()
        );

        // Written again without one: exactly one warning, and it is the index
        // — `gpkg_extensions` holds nothing but that index's own registration,
        // so reporting it as destroyed content would report one loss twice.
        let warnings = container_drop_warnings(&bytes, "own.gpkg", &names(&layers), &[], false);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("R*Tree spatial index on a"), "{warnings:?}");
        assert!(warnings[0].contains("--rtree"), "{warnings:?}");
    }

    #[test]
    fn an_index_on_a_layer_that_is_not_being_written_is_not_reported() {
        // `--layer a` drops `b` from the output, so `b`'s index is not
        // something the write loses — it goes with the layer the user chose to
        // leave out. Naming it would also offer `--rtree` as a remedy that
        // cannot apply: the writer only ever indexes the layers it writes.
        let layers = vec![layer("a"), layer("b")];
        let bytes = super::super::write_layers(None, &layers, &crate::geopackage::Carried::default(), true, false).unwrap();

        let selected = vec!["a".to_string()];
        let warnings = container_drop_warnings(&bytes, "in.gpkg", &selected, &[], false);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("R*Tree spatial index on a"), "{warnings:?}");
        assert!(!warnings[0].contains(" b"), "the unselected layer's index: {warnings:?}");
    }

    #[test]
    fn an_index_is_attributed_to_its_own_layer_when_one_name_prefixes_another() {
        // `rtree_a_b_geom` is layer `a_b`'s index, not layer `a`'s with a
        // geometry column called `b_geom` — indistinguishable by name, which
        // is why attribution reads the triggers' `tbl_name` instead.
        let layers = vec![layer("a"), layer("a_b")];
        let bytes = super::super::write_layers(None, &layers, &crate::geopackage::Carried::default(), true, false).unwrap();
        let db = Database::open(&bytes).unwrap();
        let objects = db.master_objects().unwrap();
        // Both layers are being written here, so the `written` filter admits
        // both and attribution is the only thing under test.
        assert_eq!(
            indexed_layers(&objects, &names(&layers)),
            vec!["a".to_string(), "a_b".to_string()]
        );
    }

    #[test]
    fn unreadable_bytes_warn_about_nothing_and_leave_the_error_to_the_read() {
        assert_eq!(
            container_drop_warnings(b"not a sqlite file", "junk.gpkg", &[], &[], false),
            Vec::<String>::new()
        );
    }

    /// A `gpkg_contents` row that is not a feature layer is container content,
    /// and must be surveyed like any other.
    ///
    /// `feature_layers` took *every* `gpkg_contents.table_name` until
    /// 2026-08-23, whatever its `data_type`. That list is what the survey
    /// skips as "out of scope because it is a layer" — so a GDAL `attributes`
    /// table or a tile pyramid was skipped by the survey *and* dropped by the
    /// writer, which emits feature layers only. It vanished at exit 0, and the
    /// warning whose entire job is to say what a rewrite destroys did not
    /// mention it.
    ///
    /// Asserted against `survey` rather than the message because the message
    /// stops naming past `MAX_NAMED` — the bug was that these never reached
    /// the survey at all, which is the level the assertion belongs at.
    #[test]
    fn a_gpkg_contents_row_that_is_not_a_feature_layer_is_still_surveyed() {
        let written = vec!["cities".to_string()];
        let s = survey(CONTENTS, &written, &[]).expect("fixture opens");
        let found: Vec<&str> = s.populated.iter().map(|(n, _)| n.as_str()).collect();
        assert!(found.contains(&"notes_aspatial"), "attributes table missing: {found:?}");
        assert!(found.contains(&"imagery"), "tile table missing: {found:?}");
        // And the feature layer is still out of scope on this fixture too —
        // the property `a_layer_is_never_reported_as_container_content` owns,
        // re-checked here only because the filter above is what could break it.
        assert!(!found.contains(&"cities"), "the layer is not a loss: {found:?}");
    }

}
