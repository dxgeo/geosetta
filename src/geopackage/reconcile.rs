//! Merging two source containers' worth of carried content into one.
//!
//! `plans/container-reconciliation.org`, all of it — set-union semantics per
//! table on the right key (§ 3), index construction for a merged table (§ 4),
//! and a refusal where no union exists (§ 5). The rules it implements are
//! § 6's written contract and nothing else; where that contract declines to
//! decide, this module returns an error rather than guessing.
//!
//! # Where this arises
//!
//! Exactly one configuration: `geosetta a.gpkg out.gpkg` where `out.gpkg`
//! already exists. `main.rs` reads the destination and hands it to the writer,
//! which keeps its layers and upserts the incoming ones over by name — so the
//! *layers* have always merged, and everything else about the destination was
//! destroyed (defect 3 applied to the destination). This module is what makes
//! the container behave the way the layers already do.
//!
//! # The trap this module exists to avoid
//!
//! **SQLite treats NULLs as distinct under `UNIQUE`, so a key with a nullable
//! member does not make a set.** Three of the four `UNIQUE` keys on the
//! mergeable registry tables have one, which means the dedup cannot be
//! delegated to the database on any of them: it has to compare the columns § 6
//! names, treating NULL as a value that equals itself. Measured 2026-10-13
//! against GDAL 3.13.3 — two `gpkg_data_column_constraints` rows of
//! `constraint_type = 'range'` with the same `constraint_name` and different
//! bounds coexist happily under `gdcc_ntv`, and `PRAGMA integrity_check`
//! reports `ok`.
//!
//! # What is deliberately not here
//!
//! *The registry tables built from layer payloads* — `gpkg_data_columns` and
//! `gpkg_data_column_constraints` reach the output through
//! [`super::writer::schema_extension_tables`], which composes them from every
//! layer's [`crate::GpkgNative`]. Their § 6 identity is enforced there, where
//! the rows are, rather than reproduced here.
//!
//! *`sqlite_sequence` recomputation* needs nothing from this module for the
//! metadata pair, because [`super::writer::metadata_tables`] has computed that
//! mark from the rows being written since the pair shipped. It is this module's
//! job only for a **merged tile pyramid**, where the rowids move and a carried
//! high-water mark would understate the table — see [`merge_tiles`].
//!
//! # The one table whose index is built rather than carried
//!
//! A carried index is *re-packed* from the source's own b-tree and needs no key
//! at all, which is why D10 was cheap. **A merged table's rows are in neither
//! source's order**, so its indexes have to be constructed — the reversal D2
//! took on measurement. [`merge_tiles`] is the only place that happens, because
//! a tile pyramid is the only mergeable object here that owns an index: D5
//! refuses a merged user table, and the registry tables are built from the
//! layers by the writer rather than carried.

use super::carry::{Carried, Metadata, MetadataRecord, MetadataReference, Pyramid, UserTable};
use crate::error::{Error, Result};
use crate::sqlite::{Autoincrement, Value};

/// The container content a write will put back, given a destination that
/// already had some and a source that carries some.
///
/// `destination` is the content read out of the output file as it stands;
/// `source` is what the input carries. Neither side "wins": where names do not
/// collide every rule keeps both, and the collisions are what § 6's contract
/// decides. Returns `Err` for a collision the contract declares undecidable
/// (§ 5), which is an acceptable answer under
/// `plans/same-format-fidelity.org` § THE RULE — a refusal is not a silent
/// loss — and is only unacceptable when it fires on the *ordinary* path, which
/// is why the registry tables that two ordinary files always collide on are
/// merged rather than refused.
///
/// # The order is forced and cannot be chosen freely
///
/// `gpkg_metadata` identity excludes `id` and `gpkg_metadata_reference`
/// identity *includes* the mapped `md_file_id`, so the two cannot be deduped
/// in either order: the record dedup yields an id map, the map is applied to
/// the references, and only then can the references be deduped. § 6 states
/// this, and it is the thing an implementation gets wrong if it treats § 3 as
/// one pass over all tables.
pub fn reconcile(destination: Carried, source: &Carried) -> Result<Carried> {
    // A short-circuit that is also the ordinary case: appending into a
    // destination that carries nothing is the one-container path, and running
    // the merge over it would be the same work for the same answer.
    if destination.is_empty() {
        return Ok(source.clone());
    }
    if source.is_empty() {
        return Ok(destination);
    }

    let mut merged = Carried {
        pyramids: destination.pyramids,
        tables: destination.tables,
        metadata: None,
    };
    // Names first, across *both* kinds. `gpkg_contents.table_name` is a primary
    // key and `sqlite_master.name` is unique, so a pyramid colliding with a
    // table is the same collision as either colliding with its own kind — and
    // checking each kind against only itself would let one through.
    refuse_cross_kind_collisions(&merged, source)?;
    // A merged pyramid reissues the *source* side's rowids, and
    // `gpkg_metadata_reference.row_id_value` is normatively a rowid into the
    // table it names. So the maps come back here and go into the metadata
    // merge: D5's hazard is a property of **renumbering**, and this is the one
    // place in the merge where a rowid moves.
    let rowids = merge_pyramids(&mut merged.pyramids, &source.pyramids)?;
    merge_tables(&mut merged.tables, &source.tables)?;
    merged.metadata = merge_metadata(destination.metadata, source.metadata.as_ref(), &rowids);
    Ok(merged)
}

/// A source object naming a destination object of the *other* kind.
///
/// Refused on exactly [`super::writer::build`]'s grounds for a carried name
/// colliding with a layer: one of the two would have to win, and
/// `gpkg_contents.table_name` is a primary key, so writing both produces a
/// database SQLite reports as malformed.
fn refuse_cross_kind_collisions(merged: &Carried, source: &Carried) -> Result<()> {
    let destination_tables: Vec<&str> = merged.tables.iter().map(|t| t.name.as_str()).collect();
    let destination_pyramids: Vec<&str> = merged.pyramids.iter().map(|p| p.name.as_str()).collect();
    for pyramid in &source.pyramids {
        if destination_tables.contains(&pyramid.name.as_str()) {
            return Err(collision(
                "tile pyramid",
                &pyramid.name,
                "a table of the same name",
            ));
        }
    }
    for table in &source.tables {
        if destination_pyramids.contains(&table.name.as_str()) {
            return Err(collision("table", &table.name, "a tile pyramid of the same name"));
        }
    }
    Ok(())
}

/// Tile pyramids: a set union by name, and a real merge of the tiles where the
/// names collide.
///
/// Returns the reissued rowids per merged pyramid name, which
/// [`merge_metadata`] needs: a `row`-scoped `gpkg_metadata_reference` into a
/// merged pyramid names a rowid this function has moved.
///
/// # This is step 5, and a pyramid is the only table that can reach it
///
/// § 6 gives a tile table a *normative* identity — Requirement 54 marks
/// `zoom_level`, `tile_column` and `tile_row` **UK**, so `(z, x, y)` is the
/// declared unique key and `id` is excluded by rule 1. What the rows should do
/// was never the open question. The index was: **a merged table's rows are in
/// neither source's order, so its indexes cannot be re-packed and have to be
/// built**, which is the machinery D10 scoped out and D2 reopened on
/// measurement. A pyramid is the only mergeable object in [`Carried`] that
/// both declares `AUTOINCREMENT` and owns an index — D5 refuses a merged user
/// table — so it is also the only caller
/// [`crate::sqlite::autoindex_key_columns`] will ever have.
///
/// # What is refused, and why each refusal is not a hedge
///
/// Every one of these is a case where no union exists or where this crate
/// cannot reproduce SQLite's own answer. Writing something anyway would
/// produce a database `SELECT` may still answer — the failure mode § 5 exists
/// to refuse rather than risk.
///
/// | refused | because |
/// |---|---|
/// | the two DDLs differ | two declarations of one table have no union (§ 5) |
/// | the CRS, the tile matrix set or the matrix differ | these define the tiling *scheme*; tiles under two schemes do not share a coordinate at all |
/// | a key the parse cannot honour, or an autoindex count that disagrees with the source's | the parse did not understand this DDL, whatever else is true of it |
/// | the table has no `zoom_level` / `tile_column` / `tile_row` | Requirement 54's key is what identity rests on, and without it there is none |
/// | the same `(z, x, y)` carrying different bytes | **D4**, and it fires only on a true overlap: adjacent pyramids union cleanly |
///
/// *The count check is the one that earns its keep without being obvious.*
/// [`crate::sqlite::autoindex_key_columns`] reads the DDL and the carried
/// `autoindexes` were read from the source's own b-trees, so the two are
/// independent answers to "how many implicit indexes does this table have".
/// Where they disagree the parse is wrong, and building from it would write
/// either a fabricated index (`handoff.org` entry 20) or a declared constraint
/// with no b-tree behind it (entry 15) — the two halves of the same defect,
/// one of which this tree has shipped.
fn merge_pyramids(
    destination: &mut Vec<Pyramid>,
    source: &[Pyramid],
) -> Result<Vec<(String, IdMap)>> {
    let mut rowids = Vec::new();
    for pyramid in source {
        match destination.iter_mut().find(|p| p.name == pyramid.name) {
            Some(into) => rowids.push((pyramid.name.clone(), merge_tiles(into, pyramid)?)),
            None => destination.push(pyramid.clone()),
        }
    }
    Ok(rowids)
}

/// One pyramid merged into another: its tiles unioned on Requirement 54's key,
/// its rowids reissued where they collide, and its indexes **built** from the
/// result.
fn merge_tiles(into: &mut Pyramid, from: &Pyramid) -> Result<IdMap> {
    let refuse = |what: &str| {
        Err(Error::Convert(format!(
            "gpkg: the source and the destination both carry a tile pyramid named \"{}\", and \
             {what}; refusing to merge two pyramids that are not the same pyramid \
             (plans/container-reconciliation.org § 5)",
            into.name
        )))
    };
    if collapse_whitespace(&into.sql) != collapse_whitespace(&from.sql) {
        return refuse("their CREATE TABLE statements differ");
    }
    // The scheme, not the tiles. Two pyramids whose matrix or CRS differ do not
    // share a coordinate system, so `(z, x, y)` does not identify the same tile
    // on both sides and the union would silently interleave two griddings.
    if into.srs_id != from.srs_id || into.srs != from.srs {
        return refuse("they declare different coordinate reference systems");
    }
    if into.matrix_set != from.matrix_set || into.matrix != from.matrix {
        return refuse("they declare different tile matrices");
    }

    let columns = crate::sqlite::table_columns(&into.sql);
    let keys = crate::sqlite::autoindex_key_columns(&into.sql)
        .map_err(|why| Error::Convert(format!(
            "gpkg: cannot merge the tile pyramid \"{}\" because {why}; refusing rather than \
             building an index whose order this crate cannot reproduce \
             (plans/container-reconciliation.org § 5)",
            into.name
        )))?;
    if keys.len() != into.autoindexes.len() {
        return Err(Error::Convert(format!(
            "gpkg: the tile pyramid \"{}\" declares {} implicit index(es) by its DDL and the \
             source file holds {}; refusing to merge a table whose schema this crate has not \
             understood (plans/container-reconciliation.org § 5)",
            into.name,
            keys.len(),
            into.autoindexes.len()
        )));
    }
    // Requirement 54 fixes the *names*; only the DDL knows the positions.
    let key_columns: Option<Vec<usize>> = ["zoom_level", "tile_column", "tile_row"]
        .iter()
        .map(|want| columns.iter().position(|c| c.eq_ignore_ascii_case(want)))
        .collect();
    let Some(key_columns) = key_columns else {
        return refuse("one of them has no zoom_level/tile_column/tile_row to identify a tile by");
    };
    let tile_key = |values: &[Value]| -> Vec<Value> {
        key_columns.iter().map(|&c| values.get(c).cloned().unwrap_or(Value::Null)).collect()
    };

    // A reissued rowid clears everything already allocated, on `merge_records`'
    // reasoning: the destination's rowids need not be dense, and a hole is not
    // free — `gpkg_metadata_reference` may still point into it.
    let mut next = into.rows.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0);
    let mut map = IdMap::default();
    for (rowid, values) in &from.rows {
        match into.rows.iter().find(|(_, held)| tile_key(held) == tile_key(values)) {
            Some((existing, held)) => {
                // Same tile, same bytes: one tile, and the destination's rowid
                // is the one that survives. D4 refuses the other case — a true
                // overlap disagreeing on `tile_data` has no union, and it is
                // the only collision a pair of adjacent pyramids cannot have.
                if held != values {
                    return refuse("they hold different tiles at the same (zoom_level, \
                                   tile_column, tile_row)");
                }
                map.entries.push((*rowid, *existing));
            }
            None => {
                next += 1;
                map.entries.push((*rowid, next));
                into.rows.push((next, values.clone()));
            }
        }
    }

    // ***Built, not re-packed.*** This is the line step 5 exists for: the
    // merged rows are in neither source's order, so the source's b-tree records
    // describe an order the output does not have.
    into.autoindexes =
        keys.iter().map(|key| crate::sqlite::autoindex_records(&into.rows, key)).collect();
    // And requirement 2's inversion, where it is finally reachable: carrying a
    // high-water mark verbatim is right for a carried table and wrong the
    // moment its ids move. Recomputed from the merged rows.
    if into.autoincrement.is_some() {
        into.autoincrement = Some(match into.rows.iter().map(|(rowid, _)| *rowid).max() {
            Some(high) => Autoincrement::HighWater(high),
            None => Autoincrement::Declared,
        });
    }
    // The registration describes a pyramid that now covers both extents. Every
    // other member of the row is the destination's: its `identifier` and
    // `description` are what the file being appended to says about itself, and
    // `last_change` follows `writer::LAST_CHANGE`'s rule — the stamp belongs to
    // whoever last changed the content, and geosetta does not get to claim it.
    into.bbox = union_bbox(&into.bbox, &from.bbox);
    Ok(map)
}

/// The envelope covering both, with a non-numeric bound in either treated as
/// unknown and carried from the destination.
///
/// Requirement 13 makes the four columns informative for a `tiles` row, so this
/// cannot make the file non-conformant — but a union that quietly narrowed the
/// extent would describe a pyramid smaller than the tiles it holds.
fn union_bbox(into: &[Value; 4], from: &[Value; 4]) -> [Value; 4] {
    let pick = |a: &Value, b: &Value, lower: bool| match (as_f64(a), as_f64(b)) {
        (Some(x), Some(y)) => {
            Value::Real(if lower { x.min(y) } else { x.max(y) })
        }
        _ => a.clone(),
    };
    [
        pick(&into[0], &from[0], true),
        pick(&into[1], &from[1], true),
        pick(&into[2], &from[2], false),
        pick(&into[3], &from[3], false),
    ]
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Real(n) => Some(*n),
        Value::Int(n) => Some(*n as f64),
        _ => None,
    }
}

/// `sql` with every run of whitespace outside a quoted string collapsed to one
/// space, so two producers' spelling of the same declaration compares equal.
///
/// *Whitespace is the only difference this forgives*, and that is deliberate:
/// `gdal_tiles.gpkg`'s tile table is one long line and a hand-built one is
/// indented over five, and they declare the same table. Anything else — a
/// differing column, constraint or type — is a different declaration and
/// [`merge_tiles`] refuses it.
fn collapse_whitespace(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut quote: Option<char> = None;
    let mut pending_space = false;
    for c in sql.chars() {
        match quote {
            Some(q) => {
                out.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c.is_whitespace() => pending_space = !out.is_empty(),
            None => {
                if pending_space {
                    out.push(' ');
                    pending_space = false;
                }
                if matches!(c, '"' | '\'' | '`') {
                    quote = Some(c);
                } else if c == '[' {
                    quote = Some(']');
                }
                out.push(c);
            }
        }
    }
    out
}

/// User tables: a set union by name, and a refusal where the names collide.
///
/// **D5, decided 2026-08-23, reopened, and re-decided.** A user table has no
/// general notion of row identity — it is somebody else's table, and this crate
/// does not interpret it — so there is no key to union on and nothing is
/// invented. The hazard that settled it is sharper than the missing key:
/// `gpkg_metadata_reference.row_id_value` is normatively a rowid into the table
/// but carries **no foreign key**, so merging renumbers rowids and every
/// row-scoped reference rots *silently* — past `integrity_check` and past
/// `foreign_key_check`.
fn merge_tables(destination: &mut Vec<UserTable>, source: &[UserTable]) -> Result<()> {
    for table in source {
        if destination.iter().any(|t| t.name == table.name) {
            return Err(collision("table", &table.name, "a table of the same name"));
        }
        destination.push(table.clone());
    }
    Ok(())
}

/// The refusal message for a name that exists on both sides, phrased the way
/// [`super::writer::build`]'s sibling refusal is.
fn collision(kind: &str, name: &str, against: &str) -> Error {
    Error::Convert(format!(
        "gpkg: the source carries a {kind} named \"{name}\" and the destination already holds \
         {against}; refusing to write a container where one would overwrite the other \
         (plans/container-reconciliation.org D5)"
    ))
}

/// The Metadata extension's pair, merged.
///
/// Infallible, and that is a statement about the contract rather than about the
/// implementation: § 6 gives `gpkg_metadata` and `gpkg_metadata_reference` the
/// only two identities in the table that admit **no conflict**. Two records
/// with different content are a different record, and two references with a
/// different mapped parent are a different reference — so there is nothing for
/// a merge to be undecided about, and every § 5 refusal belongs to some other
/// table.
///
/// `rowids` carries [`merge_pyramids`]' reissued rowids per merged pyramid
/// name. ***A `row`-scoped reference into a merged pyramid is remapped, not
/// dropped***, and that choice is decision 27's precedent rather than a new
/// one: when this crate could keep a reference pointing at the feature it
/// meant, it did. The alternative — dropping it, as a *renumbered layer's*
/// reference is dropped — is right there because `feature_ids` reissues a whole
/// layer and no map survives the IR; here the map is in hand, so there is
/// nothing to lose.
fn merge_metadata(
    destination: Option<Metadata>,
    source: Option<&Metadata>,
    rowids: &[(String, IdMap)],
) -> Option<Metadata> {
    // Requirement 95 makes the pair travel together, so a side that has it
    // brings both tables or neither, and where only one side does that side is
    // the answer unchanged.
    let (mut merged, source) = match (destination, source) {
        (Some(destination), Some(source)) => (destination, source),
        (Some(destination), None) => return Some(destination),
        (None, source) => return source.cloned(),
    };

    // 1. Dedupe the records by content, which yields the id map.
    let ids = merge_records(&mut merged.records, &source.records);
    // 2. Apply the map, then 3. dedupe the references on the mapped values.
    //    Steps 2 and 3 are one pass because a mapped reference is the only
    //    thing step 3 can compare: § 6's identity includes the *mapped*
    //    `md_file_id`, not the source's.
    for reference in &source.references {
        // The rowid remap comes **first**, because § 6 puts `row_id_value` in a
        // reference's identity: deduping on the source's rowid would compare a
        // number the output does not contain.
        let row_id_value = match (&reference.table_name, reference.row_id_value) {
            (Some(table), Some(rowid)) => rowids
                .iter()
                .find(|(name, _)| name == table)
                .map_or(Some(rowid), |(_, map)| Some(map.map(rowid))),
            _ => reference.row_id_value,
        };
        let mapped = MetadataReference {
            row_id_value,
            md_file_id: ids.map(reference.md_file_id),
            md_parent_id: reference.md_parent_id.map(|parent| ids.map(parent)),
            ..reference.clone()
        };
        // A reference the destination already states is one reference. The
        // destination's row is kept, which keeps **its** `timestamp`: § 6 rule
        // 3 makes a write-time stamp provenance rather than identity, and the
        // record existed before this append created nothing. Two independently
        // built files always disagree on the stamp, so including it in identity
        // would mean nothing ever dedupes.
        if !merged.references.iter().any(|existing| same_reference(existing, &mapped)) {
            merged.references.push(mapped);
        }
    }
    Some(merged)
}

/// `gpkg_metadata`, deduped by content, and the map from the source's ids to
/// the merged table's.
///
/// § 6 rule 1: **a surrogate id is allocation, not identity.** The id exists to
/// be pointed at, not to say what a row *is*, so two records differing only in
/// it are one record — which is what makes the remapping safe rather than
/// destructive. Identity is therefore `(md_scope, md_standard_uri, mime_type,
/// metadata)`, the four columns [`MetadataRecord::values`] holds, with `id`
/// excluded.
///
/// *A source id is kept where it is free*, and reissued only where the
/// destination already uses it for different content. That is not a
/// requirement — any allocation satisfies the contract — but it keeps the
/// common case (two files whose records do not overlap at all) from renumbering
/// rows nothing forced it to touch.
fn merge_records(destination: &mut Vec<MetadataRecord>, source: &[MetadataRecord]) -> IdMap {
    let mut map = IdMap::default();
    for record in source {
        if let Some(existing) = destination.iter().find(|d| d.values == record.values) {
            // The same document, already present: one record, and every
            // reference to the source's id now names the destination's.
            map.entries.push((record.id, existing.id));
            continue;
        }
        let taken = destination.iter().any(|d| d.id == record.id);
        let id = if taken {
            // The high-water mark of what is already allocated, plus one. Not
            // `len()`: the destination's ids need not be dense, and a deleted
            // row leaves a hole that reusing would fill with a *different*
            // document under an id something may still point at.
            destination.iter().map(|d| d.id).max().unwrap_or(0).max(record.id) + 1
        } else {
            record.id
        };
        map.entries.push((record.id, id));
        destination.push(MetadataRecord { id, values: record.values.clone() });
    }
    map
}

/// Source `gpkg_metadata.id` to merged `gpkg_metadata.id`.
///
/// A list rather than a `HashMap` deliberately: this crate carries no map type
/// for this and a `gpkg_metadata` has single-digit row counts in every file
/// measured, so the lookup is not worth a dependency or a hash.
#[derive(Default)]
struct IdMap {
    entries: Vec<(i64, i64)>,
}

impl IdMap {
    /// The merged id for a source id, or the source id unchanged.
    ///
    /// Unchanged is the right answer for an id this map never saw, which is an
    /// `md_file_id` naming no `gpkg_metadata` row. That is Requirement 101's
    /// case and [`super::writer::retained_references`] is where it is caught —
    /// inventing a mapping for it here would turn a reference the write is
    /// about to *drop* into one pointing at an unrelated document.
    fn map(&self, id: i64) -> i64 {
        self.entries.iter().find(|(from, _)| *from == id).map_or(id, |(_, to)| *to)
    }
}

/// Whether two `gpkg_metadata_reference` rows are the same reference, per § 6.
///
/// Identity is `(reference_scope, table_name, column_name, row_id_value)` plus
/// the **mapped** `md_file_id` and `md_parent_id`; `timestamp` is excluded by
/// rule 3. `Option`'s own `PartialEq` is what makes rule 2 — NULL equals NULL,
/// and a NULL is unequal to every non-NULL — hold without saying it again at
/// this call site.
fn same_reference(a: &MetadataReference, b: &MetadataReference) -> bool {
    a.scope == b.scope
        && a.table_name == b.table_name
        && a.column_name == b.column_name
        && a.row_id_value == b.row_id_value
        && a.md_file_id == b.md_file_id
        && a.md_parent_id == b.md_parent_id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geopackage::carry::{Storage, UserTable};
    use crate::sqlite::Value;

    /// `sqlite_constraints.gpkg` — four user tables (`tags`, `registry`,
    /// `events`, `people`), an explicit index, a trigger on a user's own table,
    /// and the metadata pair.
    const CONSTRAINTS: &[u8] = include_bytes!("../../tests/fixtures/sqlite_constraints.gpkg");
    /// `sqlite_contents.gpkg` — a tile pyramid (`imagery`), an `attributes`
    /// dataset (`notes_aspatial`), and the metadata pair.
    const CONTENTS: &[u8] = include_bytes!("../../tests/fixtures/sqlite_contents.gpkg");
    /// `gdal_tiles.gpkg` — a **real** 80-tile pyramid over two zoom levels,
    /// whose DDL declares the `UNIQUE (zoom_level, tile_column, tile_row)` of
    /// Requirement 54's informative example. The only fixture in the tree whose
    /// tile table owns an index, and so the only one step 5 can be tested on.
    const TILES: &[u8] = include_bytes!("../../tests/fixtures/gdal_tiles.gpkg");

    fn read(bytes: &[u8]) -> Carried {
        super::super::read_container(bytes).unwrap()
    }

    fn record(id: i64, document: &str) -> MetadataRecord {
        MetadataRecord {
            id,
            values: [
                Value::Text("dataset".into()),
                Value::Text("http://example.org/lineage".into()),
                Value::Text("text/xml".into()),
                Value::Text(document.into()),
            ],
        }
    }

    fn reference(table: &str, md_file_id: i64, stamp: &str) -> MetadataReference {
        MetadataReference {
            scope: "table".into(),
            table_name: Some(table.into()),
            column_name: None,
            row_id_value: None,
            timestamp: Value::Text(stamp.into()),
            md_file_id,
            md_parent_id: None,
        }
    }

    fn pair(records: Vec<MetadataRecord>, references: Vec<MetadataReference>) -> Carried {
        Carried { metadata: Some(Metadata { records, references }), ..Carried::default() }
    }

    fn table(name: &str) -> UserTable {
        UserTable {
            name: name.into(),
            sql: format!("CREATE TABLE {name} (id INTEGER PRIMARY KEY)"),
            storage: Storage::Rows(Vec::new()),
            autoindexes: Vec::new(),
            indexes: Vec::new(),
            autoincrement: None,
            triggers: Vec::new(),
            registration: None,
        }
    }

    /// ***Where nothing collides, every rule keeps both sides*** — the ordinary
    /// case, and the one the whole plan is in service of: before 2026-10-13 the
    /// destination's container was simply destroyed.
    ///
    /// /`side_notes` is dropped from the source first, and the reason is worth
    /// keeping: **both fixtures are built on `gdal_hostile.gpkg`, so they share
    /// it** and an unmodified pair refuses. That is § THE QUESTION IS NARROWER
    /// AND HARDER THAN "WHICH WINS" demonstrated inside this tree — collisions
    /// are the *common* case, not the rare one, and two files with a producer in
    /// common collide without being asked to./
    #[test]
    fn content_that_does_not_collide_is_a_union() {
        let destination = read(CONSTRAINTS);
        let mut source = read(CONTENTS);
        source.tables.retain(|t| t.name != "side_notes");
        // The premise: disjoint names now, and a pyramid on only one side.
        assert!(destination.pyramids.is_empty() && !source.pyramids.is_empty());

        let merged = reconcile(destination, &source).unwrap();

        let mut names: Vec<&str> = merged.tables.iter().map(|t| t.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["events", "notes_aspatial", "people", "registry", "side_notes", "tags"],
            "every table from both sides survives"
        );
        assert_eq!(
            merged.pyramids.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["imagery"],
            "the source's pyramid comes along"
        );
    }

    /// ***A same-named user table is refused*** — D5, and the hazard that
    /// settled it is `row_id_value`: it is normatively a rowid into the table
    /// and carries **no foreign key**, so merging renumbers rowids and every
    /// row-scoped reference rots past both pragmas.
    #[test]
    fn a_same_named_user_table_is_refused() {
        let destination = read(CONSTRAINTS);
        let error = reconcile(destination.clone(), &destination)
            .expect_err("a user table has no general notion of row identity");
        let message = error.to_string();
        assert!(message.contains("D5"), "the refusal must cite the decision: {message}");
    }

    /// ***Two pyramids with disjoint tiles union, and the index is built.***
    ///
    /// This is step 5's whole point. `(z, x, y)` identifies a tile
    /// (Requirement 54) and `id` does not (§ 6 rule 1), so the source's rows
    /// arrive with reissued rowids — and **the index cannot be re-packed from
    /// either side's b-tree**, because the merged rows are in neither side's
    /// order. It is constructed by `crate::sqlite::autoindex_records`, which is
    /// the one place in that module that orders anything.
    #[test]
    fn two_pyramids_with_disjoint_tiles_union_and_the_index_is_built() {
        let destination = read(TILES);
        let held = destination.pyramids[0].clone();
        assert_eq!(held.autoindexes.len(), 1, "the premise: gdal_tiles declares one UNIQUE");
        let before = held.rows.len();

        // The same pyramid one zoom level further out: a real adjacent gridding
        // rather than a contrived key, so every row is new and none collides.
        let mut source = Carried { pyramids: vec![held.clone()], ..Carried::default() };
        for (rowid, values) in &mut source.pyramids[0].rows {
            values[1] = Value::Int(99);
            *rowid += 1000;
        }

        let merged = reconcile(destination, &source).unwrap();
        let pyramid = &merged.pyramids[0];
        assert_eq!(pyramid.rows.len(), before * 2, "disjoint tiles union");
        // Reissued above the destination's high-water mark, and `sqlite_sequence`
        // recomputed behind them — requirement 2's inversion, reachable at last.
        let rowids: Vec<i64> = pyramid.rows.iter().map(|(r, _)| *r).collect();
        assert_eq!(
            rowids.iter().copied().collect::<std::collections::BTreeSet<_>>().len(),
            rowids.len(),
            "a reissued rowid must collide with nothing: {rowids:?}"
        );
        assert_eq!(
            pyramid.autoincrement,
            Some(Autoincrement::HighWater(*rowids.iter().max().unwrap())),
            "carrying the source's mark would understate the merged table"
        );
        // The index is the merged rows' own, not either source's records.
        assert_eq!(
            pyramid.autoindexes,
            vec![crate::sqlite::autoindex_records(&pyramid.rows, &[1, 2, 3])],
            "the index must be built from the merged rows"
        );
        assert_ne!(
            pyramid.autoindexes[0], held.autoindexes[0],
            "re-packing the destination's records would describe an order the output lacks"
        );
    }

    /// ***The same tile on both sides is one tile.*** Identity is `(z, x, y)`
    /// with `id` excluded, so a pyramid merged with itself is unchanged — and
    /// the rowids do not move, which is what keeps a reference into the
    /// destination valid without any remapping.
    #[test]
    fn the_same_pyramid_merged_with_itself_is_unchanged() {
        let destination = read(TILES);
        let source = Carried {
            pyramids: destination.pyramids.clone(),
            ..Carried::default()
        };
        let before = destination.pyramids[0].clone();

        let merged = reconcile(destination, &source).unwrap();

        assert_eq!(merged.pyramids[0].rows, before.rows, "no tile is new and none moves");
        assert_eq!(
            merged.pyramids[0].autoindexes,
            vec![crate::sqlite::autoindex_records(&before.rows, &[1, 2, 3])],
            "the index is built over the same rows, so it keys the same tiles"
        );
        // ***And it is compared against the construction, not against the
        // source's bytes.*** Those differ, and the difference is not an order:
        // `encode_record` writes every integer as an 8-byte serial type where
        // SQLite picks the narrowest that fits, so a built index is wider than
        // a carried one over identical rows. /Valid either way — SQLite reads
        // both and `integrity_check` passes on every index this crate has ever
        // built — but it means a merged pyramid's index is **not** byte-equal to
        // what SQLite would have written, which is worth knowing before anyone
        // asserts byte equality against a GDAL-produced file./
        assert_ne!(
            merged.pyramids[0].autoindexes, before.autoindexes,
            "the premise of the comparison above: built and carried bytes differ"
        );
    }

    /// ***D4: the same `(z, x, y)` carrying different bytes is refused.***
    ///
    /// It fires only on a *true* overlap — adjacent pyramids union cleanly on
    /// the declared key — so it is not a refusal on the ordinary path. And
    /// there is no union to pick: this crate never decodes a tile, so it has no
    /// basis whatever for preferring one image to the other.
    #[test]
    fn the_same_tile_with_different_bytes_is_refused() {
        let destination = read(TILES);
        let mut source = Carried {
            pyramids: destination.pyramids.clone(),
            ..Carried::default()
        };
        let data = source.pyramids[0].rows[0].1.len() - 1;
        source.pyramids[0].rows[0].1[data] = Value::Blob(b"a different image".to_vec());

        let error = reconcile(destination, &source).expect_err("two tiles, one coordinate");
        let message = error.to_string();
        assert!(
            message.contains("different tiles at the same"),
            "the refusal must say what overlapped: {message}"
        );
    }

    /// Two declarations of one table have no union, so a differing DDL or a
    /// differing tiling *scheme* is refused rather than resolved. /The scheme
    /// matters as much as the DDL: under two different matrices `(z, x, y)` does
    /// not name the same ground, so the union would interleave two griddings
    /// while passing every structural oracle./
    #[test]
    fn pyramids_that_are_not_the_same_pyramid_are_refused() {
        let base = read(TILES);
        type Break = fn(&mut Pyramid);
        let cases: [(&str, Break); 3] = [
            ("their CREATE TABLE statements differ", |p| {
                p.sql = p.sql.replace("tile_data BLOB NOT NULL", "tile_data BLOB");
            }),
            ("they declare different coordinate reference systems", |p| p.srs_id += 1),
            ("they declare different tile matrices", |p| {
                p.matrix.truncate(1);
            }),
        ];
        for (expected, break_it) in cases {
            let mut source =
                Carried { pyramids: base.pyramids.clone(), ..Carried::default() };
            break_it(&mut source.pyramids[0]);
            let error = reconcile(base.clone(), &source)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "expected {expected:?}, got {error}");
        }
    }

    /// ***The merged registration describes a pyramid covering both
    /// extents.***
    ///
    /// A `tiles` row's four bounds are *carried* rather than recomputed — this
    /// crate never decodes a tile, so it has nothing to recompute them from
    /// (`native-inventory.org`) — which means a merge has to union them or the
    /// row describes a pyramid smaller than the tiles the file holds.
    ///
    /// /Requirement 13 makes the columns informative for this `data_type`, so a
    /// narrowed extent cannot make the file non-conformant. It can still make
    /// it wrong, and a consumer deciding what to fetch reads exactly this row./
    #[test]
    fn a_merged_pyramids_registration_covers_both_extents() {
        let mut destination = read(TILES);
        destination.pyramids[0].bbox =
            [Value::Real(0.0), Value::Real(0.0), Value::Real(10.0), Value::Real(10.0)];
        let mut source = Carried { pyramids: destination.pyramids.clone(), ..Carried::default() };
        source.pyramids[0].bbox =
            [Value::Real(-5.0), Value::Real(2.0), Value::Real(4.0), Value::Real(20.0)];

        let merged = reconcile(destination, &source).unwrap();

        assert_eq!(
            merged.pyramids[0].bbox,
            [Value::Real(-5.0), Value::Real(0.0), Value::Real(10.0), Value::Real(20.0)],
            "each bound must move outwards, never inwards"
        );
    }

    /// A bound that is not a number is **unknown**, not zero, so the
    /// destination's is carried rather than compared against something that
    /// cannot be ordered. /Requirement 13 makes all four nullable for a `tiles`
    /// row, and `min(NULL, x)` is not `x`: it is the absence of an answer./
    #[test]
    fn a_non_numeric_bound_is_carried_rather_than_compared() {
        let mut destination = read(TILES);
        destination.pyramids[0].bbox =
            [Value::Null, Value::Real(0.0), Value::Real(10.0), Value::Real(10.0)];
        let mut source = Carried { pyramids: destination.pyramids.clone(), ..Carried::default() };
        source.pyramids[0].bbox =
            [Value::Real(-5.0), Value::Real(0.0), Value::Real(10.0), Value::Real(10.0)];

        let merged = reconcile(destination, &source).unwrap();

        assert_eq!(
            merged.pyramids[0].bbox[0],
            Value::Null,
            "an unknown bound stays unknown rather than borrowing the other side's"
        );
    }

    /// ***A pyramid whose DDL and whose file disagree about how many implicit
    /// indexes it has is refused.***
    ///
    /// The self-check, and **the mutation that found this test survived without
    /// it**: in every fixture the parse and the file agree, so removing the
    /// check broke nothing. /The blind corpus's signature for the fifth time —
    /// the one example we have happens to be the agreeing case./
    ///
    /// The two numbers are independent answers to one question.
    /// `sqlite::autoindex_key_columns` reads the DDL;
    /// `carry::autoindex_records` read the source's own b-trees. Where they
    /// disagree the parse did not understand this table, and building from it
    /// writes one of the two halves of the same defect: an index with no
    /// constraint behind it (`handoff.org` entry 20) or a constraint with no
    /// index (entry 15). **This tree has shipped one of them.**
    #[test]
    fn a_pyramid_whose_ddl_and_file_disagree_about_its_indexes_is_refused() {
        let base = read(TILES);
        // The DDL declares `UNIQUE (zoom_level, tile_column, tile_row)` and the
        // file holds its b-tree. A source whose b-tree is missing is a file
        // that is already malformed, or a DDL this parse read wrongly — and
        // from here there is no telling which.
        let mut destination = base.clone();
        destination.pyramids[0].autoindexes.clear();
        let error = reconcile(destination, &base).unwrap_err().to_string();
        assert!(
            error.contains("declares 1 implicit index(es) by its DDL and the source file holds 0"),
            "the refusal must state both counts: {error}"
        );
    }

    /// ***A reissued tile rowid clears every rowid already allocated, not just
    /// the row count.***
    ///
    /// `merge_records`' rule, and the same mutation survived here for the same
    /// reason it would have there: `gdal_tiles.gpkg`'s ids are a dense `1..80`,
    /// so `len()` and `max()` coincide and either implementation passes. **A
    /// gap is what distinguishes them**, and a pyramid with one is not exotic —
    /// a tile deleted from a pyramid leaves exactly this.
    ///
    /// /Reusing a hole would be worse than untidy: `gpkg_metadata_reference`
    /// may still point into it, so a reissued rowid landing in a gap silently
    /// redirects a reference to a different tile — D5's hazard, caused rather
    /// than inherited./
    #[test]
    fn a_reissued_tile_rowid_clears_every_rowid_already_allocated() {
        let mut destination = read(TILES);
        // A hole: the destination holds 2 tiles at rowids 1 and 40, so `len()`
        // is 2 and the high-water mark is 40.
        let pyramid = &mut destination.pyramids[0];
        pyramid.rows.truncate(2);
        pyramid.rows[1].0 = 40;

        // One incoming tile, at a key neither of those two has.
        let mut source = Carried { pyramids: destination.pyramids.clone(), ..Carried::default() };
        source.pyramids[0].rows.truncate(1);
        source.pyramids[0].rows[0].0 = 1;
        source.pyramids[0].rows[0].1[2] = Value::Int(9999);

        let merged = reconcile(destination, &source).unwrap();
        let reissued = merged.pyramids[0]
            .rows
            .iter()
            .find(|(_, values)| values[2] == Value::Int(9999))
            .expect("the incoming tile is in the output")
            .0;
        assert_eq!(reissued, 41, "a hole at 2..39 is not free: got {reissued}");
    }

    /// ***A `row`-scoped reference into a merged pyramid follows its tile.***
    ///
    /// `gpkg_metadata_reference.row_id_value` is normatively a rowid into the
    /// table it names and carries **no foreign key**, so a stale one rots
    /// silently — past `integrity_check` and past `foreign_key_check`. That is
    /// D5's hazard, and the 2026-08-25 correction says it belongs wherever a
    /// **rowid changes** rather than wherever a merge happens. This is the one
    /// place in the merge where one does.
    ///
    /// /Remapped rather than dropped, on decision 27's precedent. Nothing else
    /// would catch it: the reference stays a valid integer either way, and the
    /// tile it points at is simply a different image./
    #[test]
    fn a_row_scoped_reference_into_a_merged_pyramid_follows_its_tile() {
        let destination = read(TILES);
        let name = destination.pyramids[0].name.clone();
        // A source whose single tile is new, so its rowid must move.
        let mut pyramid = destination.pyramids[0].clone();
        pyramid.rows.truncate(1);
        pyramid.rows[0].0 = 1;
        pyramid.rows[0].1[1] = Value::Int(99);
        let source = Carried {
            pyramids: vec![pyramid],
            metadata: Some(Metadata {
                records: vec![record(1, "<a/>")],
                references: vec![MetadataReference {
                    scope: "row".into(),
                    table_name: Some(name.clone()),
                    row_id_value: Some(1),
                    ..reference(&name, 1, "T")
                }],
            }),
            ..Carried::default()
        };

        let merged = reconcile(destination, &source).unwrap();

        let moved_to = merged.pyramids[0]
            .rows
            .iter()
            .find(|(_, values)| values[1] == Value::Int(99))
            .expect("the source's tile is in the output")
            .0;
        let reference = merged
            .metadata
            .as_ref()
            .unwrap()
            .references
            .iter()
            .find(|r| r.scope == "row")
            .expect("the reference survives");
        assert_ne!(moved_to, 1, "the premise: the rowid had to move");
        assert_eq!(
            reference.row_id_value,
            Some(moved_to),
            "a reference left on the source's rowid names whichever tile now holds it"
        );
    }

    /// ***SQLite enforces the `UNIQUE` behind an index this crate constructed
    /// from merged rows.***
    ///
    /// # Why `integrity_check` is not enough on its own
    ///
    /// It is necessary and it is not sufficient. `integrity_check` validates
    /// that every row has an index entry and that no entry is non-unique, which
    /// catches a **missing** or **duplicated** record — but what step 5 risks is
    /// an order, and a b-tree whose keys are misordered still holds exactly one
    /// entry per row. /That is why `plans/same-format-fidelity.org` § THE ORACLE
    /// RULE's sibling lesson applies here: the oracle you did not run is the one
    /// that would have caught you./
    ///
    /// So this asks the question only the engine can answer: insert a tile whose
    /// `(zoom_level, tile_column, tile_row)` already exists, and require SQLite
    /// to **find** the conflict through the index and refuse. A mis-sorted
    /// b-tree descends to the wrong page and the duplicate goes in — passing
    /// both pragmas on the way.
    #[test]
    #[ignore = "needs the sqlite3 binary; run with --ignored"]
    fn sqlite_enforces_the_unique_behind_a_merged_pyramids_built_index() {
        use std::process::Command;

        let destination = read(TILES);
        let layers = super::super::read_layers(TILES).unwrap();
        let held = destination.pyramids[0].rows.len();

        // The same pyramid ten columns east: same DDL, same CRS, same tile
        // matrix, not one tile in common — the adjacent gridding D4 says unions
        // cleanly, rather than a contrived key.
        let mut source = destination.clone();
        for (_, values) in &mut source.pyramids[0].rows {
            let Value::Int(column) = values[2] else { panic!("tile_column is not an integer") };
            values[2] = Value::Int(column + 10);
        }
        let merged = reconcile(destination, &source).expect("two adjacent griddings union");
        assert_eq!(merged.pyramids[0].rows.len(), held * 2, "the premise: every tile is new");

        let bytes =
            super::super::write_layers(None, &layers, &merged, false, false).unwrap();
        let path = std::env::temp_dir().join("geosetta_merged_pyramid.gpkg");
        std::fs::write(&path, &bytes).unwrap();

        let ask = |sql: &str| -> String {
            let out = Command::new("sqlite3").arg(&path).arg(sql).output().expect("sqlite3");
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if stderr.is_empty() { stdout } else { stderr }
        };

        assert_eq!(ask("PRAGMA integrity_check;"), "ok", "necessary, and not sufficient");
        assert_eq!(ask("PRAGMA foreign_key_check;"), "", "no reference left dangling");
        // The planner must reach for the index at all, or the lookup below
        // proves nothing about it.
        let plan = ask(
            "EXPLAIN QUERY PLAN SELECT id FROM imagery \
             WHERE zoom_level = 13 AND tile_column = 4390 AND tile_row = 2566;",
        );
        assert!(
            plan.contains("sqlite_autoindex_imagery_1"),
            "the built index must be the one SQLite searches: {plan:?}"
        );
        // And it must agree with the table it indexes.
        let indexed = ask(
            "SELECT id FROM imagery \
             WHERE zoom_level = 13 AND tile_column = 4390 AND tile_row = 2566;",
        );
        let scanned = ask(
            "SELECT id FROM imagery NOT INDEXED \
             WHERE zoom_level = 13 AND tile_column = 4390 AND tile_row = 2566;",
        );
        assert_eq!(indexed, scanned, "the index points at a different row than the table has");
        assert!(!indexed.is_empty(), "the shifted tile should be findable");

        // ***The sufficient question.*** `tile_data` is NOT NULL, so the
        // statement supplies one; a NOT NULL failure would be a different
        // error, which is why the message itself is matched.
        let duplicate = ask(
            "INSERT INTO imagery (zoom_level, tile_column, tile_row, tile_data) \
             SELECT zoom_level, tile_column, tile_row, tile_data FROM imagery LIMIT 1;",
        );
        assert!(
            duplicate.contains("UNIQUE constraint failed"),
            "SQLite must find the conflict through the index this crate built: {duplicate:?}"
        );
        // Requirement 2's inversion, measured on the file: carrying the mark
        // verbatim would understate a table whose rowids have moved.
        assert_eq!(
            ask("SELECT seq FROM sqlite_sequence WHERE name = 'imagery';"),
            ask("SELECT max(id) FROM imagery;"),
            "sqlite_sequence must describe the merged rows"
        );
    }

    /// A source *table* colliding with a destination *pyramid* is the same
    /// collision as either colliding with its own kind: `gpkg_contents` makes
    /// `table_name` a primary key, so checking each kind against only itself
    /// would let this one through.
    #[test]
    fn a_table_colliding_with_a_pyramid_of_the_other_side_is_refused() {
        let destination = read(CONTENTS);
        let source = Carried { tables: vec![table("imagery")], ..Carried::default() };
        let error = reconcile(destination, &source)
            .expect_err("a table and a pyramid cannot share a gpkg_contents row");
        assert!(error.to_string().contains("imagery"), "{error}");

        // And the mirror image, which a one-directional check would miss.
        let destination = Carried { tables: vec![table("imagery")], ..Carried::default() };
        let error = reconcile(destination, &read(CONTENTS))
            .expect_err("the collision is symmetric");
        assert!(error.to_string().contains("imagery"), "{error}");
    }

    /// ***Two records with the same id and different content are two records,
    /// and the reference follows the renumbering.***
    ///
    /// § 6 rule 1: a surrogate id is allocation, not identity. This is the
    /// collision that occurs with no coaxing — two independently built files
    /// both number `gpkg_metadata` from 1 — and getting it wrong does not
    /// produce a malformed file, it produces a reference pointing at *the other
    /// file's document*.
    #[test]
    fn two_documents_sharing_an_id_are_two_records_and_the_reference_follows() {
        let destination = pair(vec![record(1, "<a/>")], vec![reference("cities", 1, "T1")]);
        let source = pair(vec![record(1, "<b/>")], vec![reference("towns", 1, "T2")]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        assert_eq!(merged.records.len(), 2, "different content is a different record");
        let new_id = merged.records.iter().find(|r| r.values[3] == Value::Text("<b/>".into()));
        let new_id = new_id.expect("the source's document survives").id;
        assert_ne!(new_id, 1, "it cannot keep an id the destination holds for other content");
        let towns = merged.references.iter().find(|r| r.table_name.as_deref() == Some("towns"));
        assert_eq!(
            towns.expect("the source's reference survives").md_file_id,
            new_id,
            "a reference rewritten to the wrong id points at the other file's document"
        );
        let cities = merged.references.iter().find(|r| r.table_name.as_deref() == Some("cities"));
        assert_eq!(cities.unwrap().md_file_id, 1, "the destination's own id does not move");
    }

    /// ***The same document on both sides is one record***, and both references
    /// name it. The content key is `(md_scope, md_standard_uri, mime_type,
    /// metadata)` with `id` excluded, so the source's differing id is no
    /// obstacle to recognising its document.
    #[test]
    fn the_same_document_under_different_ids_is_one_record() {
        let destination = pair(vec![record(1, "<a/>")], vec![reference("cities", 1, "T1")]);
        let source = pair(vec![record(7, "<a/>")], vec![reference("towns", 7, "T2")]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        assert_eq!(merged.records.len(), 1, "one document is one record");
        assert_eq!(merged.records[0].id, 1, "the destination's allocation stands");
        assert!(
            merged.references.iter().all(|r| r.md_file_id == 1),
            "both references name the one surviving record: {:?}",
            merged.references.iter().map(|r| r.md_file_id).collect::<Vec<_>>()
        );
    }

    /// ***A reference the destination already states is one reference, and the
    /// destination's `timestamp` is what survives.***
    ///
    /// § 6 rule 3: a write-time stamp is provenance, not identity. Two
    /// independently built files *always* disagree on it — the column defaults
    /// to `strftime('now')` — so including it in identity would mean nothing
    /// ever dedupes. Keeping the destination's is the choice the standard
    /// leaves open: Requirement 100 constrains only the format, and the
    /// record existed before this append, which created nothing.
    #[test]
    fn a_duplicate_reference_dedupes_and_keeps_the_destinations_stamp() {
        let destination = pair(vec![record(1, "<a/>")], vec![reference("cities", 1, "ORIGINAL")]);
        let source = pair(vec![record(1, "<a/>")], vec![reference("cities", 1, "APPENDED")]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        assert_eq!(merged.references.len(), 1, "the same reference stated twice is one row");
        assert_eq!(
            merged.references[0].timestamp,
            Value::Text("ORIGINAL".into()),
            "the stamp follows whoever last changed the content, and that is not this append"
        );
    }

    /// ***A different mapped parent is a different reference.***
    ///
    /// Added after mutation-checking: dropping `md_parent_id` from
    /// [`same_reference`] broke no test, because every reference in every
    /// fixture in this tree roots its own hierarchy — `md_parent_id` is NULL
    /// everywhere — so the column could be left out of identity and nothing
    /// would notice. /The blind corpus's signature again: the one example we
    /// have happens to be the exception./
    ///
    /// § 6 puts both id columns in the identity and says no conflict is
    /// possible, which is precisely because a differing parent makes it a
    /// *different* reference rather than the same one disagreeing.
    #[test]
    fn two_references_differing_only_in_parent_are_two_references() {
        let rooted = reference("cities", 1, "T1");
        let parented = MetadataReference { md_parent_id: Some(2), ..reference("cities", 1, "T2") };
        let records = vec![record(1, "<a/>"), record(2, "<parent/>")];
        let destination = pair(records.clone(), vec![rooted]);
        let source = pair(records, vec![parented]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        assert_eq!(
            merged.references.len(),
            2,
            "a reference under a parent is not the same reference as one without: {:?}",
            merged.references.iter().map(|r| r.md_parent_id).collect::<Vec<_>>()
        );
    }

    /// ***A reissued id goes above the high-water mark, not into a hole.***
    ///
    /// Not `len()`, and not the first free integer: the destination's ids need
    /// not be dense, and a deleted row leaves a gap that something may still
    /// point at — `gpkg_metadata_reference` declares `crmr_mfi_fk` but
    /// `sqlite_fid_gap.gpkg` is the standing reminder that a tree full of dense
    /// ids cannot tell a carried id from a reissued one.
    #[test]
    fn a_reissued_id_clears_every_id_already_allocated() {
        let destination = pair(vec![record(1, "<a/>"), record(40, "<b/>")], Vec::new());
        let source = pair(vec![record(1, "<c/>")], vec![reference("towns", 1, "T")]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        let reissued = merged
            .records
            .iter()
            .find(|r| r.values[3] == Value::Text("<c/>".into()))
            .expect("the source's document survives")
            .id;
        assert_eq!(reissued, 41, "a hole at 2..39 is not free: {reissued}");
        assert_eq!(merged.references[0].md_file_id, 41, "the reference follows");
    }

    /// A free id is kept. Not a requirement — any allocation satisfies § 6 —
    /// but it keeps the common case, two files whose documents do not overlap,
    /// from renumbering rows nothing forced it to touch.
    #[test]
    fn a_source_id_nothing_has_taken_is_kept() {
        let destination = pair(vec![record(1, "<a/>")], Vec::new());
        let source = pair(vec![record(5, "<b/>")], vec![reference("towns", 5, "T")]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        assert_eq!(merged.records.iter().map(|r| r.id).collect::<Vec<_>>(), [1, 5]);
        assert_eq!(merged.references[0].md_file_id, 5, "nothing forced a rewrite");
    }

    /// Requirement 95 makes the pair travel together, so a side that holds it
    /// brings both tables or neither — and where only one side does, that side
    /// is the answer unchanged rather than an empty merge.
    #[test]
    fn a_pair_on_one_side_only_survives_whole() {
        let held = pair(vec![record(1, "<a/>")], vec![reference("cities", 1, "T")]);
        let bare = Carried { tables: vec![table("side_notes")], ..Carried::default() };

        let from_destination = reconcile(held.clone(), &bare).unwrap();
        let from_source = reconcile(bare, &held).unwrap();
        for merged in [from_destination, from_source] {
            let metadata = merged.metadata.expect("the pair is not dropped");
            assert_eq!(metadata.records.len(), 1);
            assert_eq!(metadata.references.len(), 1);
        }
    }

    /// An `md_file_id` naming no `gpkg_metadata` row is left as it is rather
    /// than mapped. Requirement 101's case, and
    /// [`super::super::writer::retained_references`] is where it is caught —
    /// inventing a mapping here would turn a reference the write is about to
    /// *drop* into one pointing at an unrelated document.
    #[test]
    fn a_reference_to_no_document_is_not_given_one() {
        let destination = pair(vec![record(1, "<a/>")], Vec::new());
        let source = pair(vec![record(1, "<b/>")], vec![reference("towns", 99, "T")]);

        let merged = reconcile(destination, &source).unwrap().metadata.unwrap();

        assert_eq!(
            merged.references[0].md_file_id, 99,
            "a dangling id must stay dangling so the write can drop the row"
        );
    }
}
