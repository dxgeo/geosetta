//! What a GeoPackage write destroys *around* the layers, and the warning that
//! says so.
//!
//! [`super::writer::write_layers`] is read-modify-write: it decodes the layers,
//! upserts, and re-emits a complete file built from the tables [`super::writer`]
//! knows how to construct. Anything else the source container held — a user's
//! own tables, `gpkg_metadata`, extension registrations, spatial indexes,
//! triggers, views — is not carried, and so does not survive. That is
//! *defect 3* in `plans/same-format-fidelity.org`, whose fix travels with the
//! container milestone (M3b).
//!
//! This module is the *warning*, shipped ahead of the fix per that plan's
//! § SCOPE ("defect 3's warning now and its fix with the container work"). Until
//! M3b lands, a `gpkg -> gpkg` run destroys container content at exit 0; the
//! standing rule in `plans/lossy-conversion-warnings.org` says a silent drop of
//! information the source actually carries is a bug regardless of whether the
//! fix is ready, so the loss is reported now and repaired later.
//!
//! **This file states what the writer reproduces, so it has to move when the
//! writer does.** [`REPRODUCED`] is that statement; a new always-emitted table
//! in [`super::writer::build`] belongs in it, or this module starts warning
//! about a table that in fact survives.

use crate::error::Result;
use crate::sqlite::{Database, MasterObject, Value};

/// The tables [`super::writer::build`] emits unconditionally, and so the
/// container content that survives a write today. `gpkg_extensions` is *not*
/// here: it is written only when an index is, which [`container_drop_warnings`]
/// handles alongside the index itself.
const REPRODUCED: [&str; 3] =
    ["gpkg_spatial_ref_sys", "gpkg_contents", "gpkg_geometry_columns"];

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
    rtree: bool,
) -> Vec<String> {
    match survey(bytes, written) {
        Ok(s) => s.into_warnings(label, rtree),
        // Not the error path. Every caller has already read this container (or
        // is about to), so a file this cannot open is reported — with a better
        // message than a warning could give — by the read that follows.
        Err(_) => Vec::new(),
    }
}

/// What a survey of the container found, already classified.
#[derive(Default)]
struct Survey {
    /// Tables holding rows that the write will not reproduce, as
    /// `(name, rows)` — the actual data loss.
    populated: Vec<(String, usize)>,
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

fn survey(bytes: &[u8], written: &[String]) -> Result<Survey> {
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
            || layers.contains(&object.name)
            || written.contains(&object.name)
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
                Some(table) => match foreign_extension_rows(&db, table)? {
                    // Nothing but R*Tree registrations (or nothing at all):
                    // wholly derived, so not a loss of its own in either the
                    // populated or the empty sense.
                    0 => {}
                    n => survey.populated.push((object.name.clone(), n)),
                },
                None => survey.schema += 1,
            }
            continue;
        }
        match tables.iter().find(|t| t.name == object.name) {
            // Reading rows costs a b-tree walk, which is why it happens only
            // for tables already known not to survive: registry tables and the
            // occasional user table, never the layers.
            Some(table) => match db.read_rows(table)?.len() {
                0 => survey.empty += 1,
                n => survey.populated.push((object.name.clone(), n)),
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
                .map(|(name, rows)| format!("{name} ({rows} {})", plural(*rows, "row")))
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

/// How many `gpkg_extensions` rows register something other than the R*Tree
/// index. A table whose every row is an R*Tree registration holds nothing this
/// writer is losing independently of the index itself.
fn foreign_extension_rows(db: &Database, table: &crate::sqlite::Table) -> Result<usize> {
    let Some(idx) = table.columns.iter().position(|c| c == "extension_name") else {
        // No such column: not a table this can reason about, so every row counts.
        return Ok(db.read_rows(table)?.len());
    };
    Ok(db
        .read_rows(table)?
        .iter()
        .filter(|row| !matches!(row.get(idx), Some(Value::Text(name)) if name == RTREE_EXTENSION))
        .count())
}

/// Every table named by a `gpkg_contents` row, whatever its `data_type`. Tile
/// pyramids are deliberately included: a raster GeoPackage's imagery is not
/// something this vector tool carries, and until § The collision's rejection
/// recommendation is settled, a populated tile table should at minimum say it
/// is being dropped rather than vanish.
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
    let Ok(rows) = db.read_rows(table) else {
        return Vec::new();
    };
    rows.iter()
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

    #[test]
    fn the_container_around_the_layers_is_reported_object_by_object() {
        let written = vec!["cities".to_string(), "second".to_string()];
        let warnings = container_drop_warnings(HOSTILE, "in.gpkg", &written, false);
        let loss = &warnings[0];
        // The user's own table is the one a reader of this message cares most
        // about, and it is named with its row count rather than counted.
        assert!(loss.contains("side_notes (1 row)"), "{loss}");
        assert!(loss.contains("gpkg_metadata (1 row)"), "{loss}");
        assert!(loss.contains("gpkg_metadata_reference (1 row)"), "{loss}");
        assert!(loss.contains("gpkg_ogr_contents (2 rows)"), "{loss}");
        // gpkg_tile_matrix and gpkg_tile_matrix_set, created and never filled.
        assert!(loss.contains("2 empty tables"), "{loss}");
        // 30 triggers in the file, 14 of them the R*Tree's own (derived, and
        // reported as the index instead).
        assert!(loss.contains("14 other schema objects"), "{loss}");
        assert!(loss.contains("in.gpkg"), "{loss}");
    }

    #[test]
    fn a_layer_is_never_reported_as_container_content() {
        // Both layers, and then just one: `--layer cities` drops `second` from
        // the output, but that is a selection the user asked for, not a loss
        // the container warning has any business reporting.
        for written in [vec!["cities".to_string(), "second".to_string()], vec!["cities".to_string()]] {
            let warnings = container_drop_warnings(HOSTILE, "in.gpkg", &written, false);
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
        let bytes = super::super::write_layers(None, &layers, false, false).unwrap();
        assert_eq!(
            container_drop_warnings(&bytes, "own.gpkg", &names(&layers), false),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_rtree_registration_is_not_a_loss_beside_the_index_it_declares() {
        let layers = vec![layer("a")];
        let bytes = super::super::write_layers(None, &layers, true, false).unwrap();

        // Written again with an index: the index is rebuilt, so nothing at all.
        assert_eq!(
            container_drop_warnings(&bytes, "own.gpkg", &names(&layers), true),
            Vec::<String>::new()
        );

        // Written again without one: exactly one warning, and it is the index
        // — `gpkg_extensions` holds nothing but that index's own registration,
        // so reporting it as destroyed content would report one loss twice.
        let warnings = container_drop_warnings(&bytes, "own.gpkg", &names(&layers), false);
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
        let bytes = super::super::write_layers(None, &layers, true, false).unwrap();

        let selected = vec!["a".to_string()];
        let warnings = container_drop_warnings(&bytes, "in.gpkg", &selected, false);
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
        let bytes = super::super::write_layers(None, &layers, true, false).unwrap();
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
            container_drop_warnings(b"not a sqlite file", "junk.gpkg", &[], false),
            Vec::<String>::new()
        );
    }
}
