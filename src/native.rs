//! Format-native passthrough — what a reader recovered but the IR does not
//! model, kept verbatim so a same-format run can put it back.
//!
//! The IR is deliberately small ([`crate::Feature`] is geometry + properties;
//! [`crate::FeatureCollection`] adds a CRS and a name), so every format records
//! things it has nowhere to go: GeoJSON's `id` and foreign members, a
//! GeoPackage's unmodelled tables, a FlatGeobuf header's `title`. Dropping
//! those is defensible when the *target* cannot record them and indefensible
//! when the target is the source's own format — see
//! `plans/same-format-fidelity.org` for the rule this exists to keep, and
//! `plans/native-inventory.org` for what each spoke actually discards.
//!
//! # The contract, in three lines
//!
//! - A **reader** stashes what it did not model, verbatim, tagged with its own
//!   [`Format`]. It stashes nothing when it modelled everything — `None` is the
//!   common case and costs nothing.
//! - A **writer** replays the payload *if and only if* [`Native::format`]
//!   matches the format it is writing ([`Native::payload_for`]).
//! - On a mismatch it ignores the payload. That is not an error; it is an
//!   ordinary cross-format conversion, and the loss is the target's incapacity
//!   rather than a bug.
//!
//! The tag is what makes this unable to corrupt a cross-format run: a GeoJSON
//! foreign member can never leak into a FlatGeobuf writer, because the writer
//! never looks at a payload that is not its own. GDAL takes the same approach
//! and stores the tag alongside the blob (`NATIVE_MEDIA_TYPE` beside
//! `NATIVE_DATA`), which is where this design comes from.
//!
//! # What does *not* belong here
//!
//! - **Derived data** — a `bbox`, a cached extent, an R*Tree. Replaying a
//!   derived value is how it goes stale, and a stale GeoPackage envelope
//!   silently drops features from a spatial filter. Recompute those instead.
//! - **Members several formats model first-class** — the collection's name, a
//!   column's declared width. Those are IR fields, not payloads, because a
//!   native never crosses a process boundary: only a modelled field can reach
//!   an external tool in a pipeline and be edited by it.

use crate::format::Format;
use crate::json::JsonValue;
use std::rc::Rc;

/// Verbatim, unmodelled content recovered by a reader, tagged with the format
/// that produced it.
///
/// Construct with [`Native::new`] and read back with [`Native::payload_for`],
/// which enforces the tag match rather than leaving it to each writer.
#[derive(Debug, Clone, PartialEq)]
pub struct Native {
    /// The format whose reader produced [`Self::payload`]. Only that format's
    /// writer may replay it.
    pub format: Format,
    /// The unmodelled content itself. Opaque to the hub: nothing outside the
    /// owning spoke interprets it.
    pub payload: NativePayload,
}

impl Native {
    /// Tag `payload` as belonging to `format`.
    pub fn new(format: Format, payload: NativePayload) -> Native {
        Native { format, payload }
    }

    /// The payload, but only when this native came from `target` — the tag
    /// check every writer must make, in one place so no writer can forget it.
    ///
    /// Returns `None` on a mismatch, which is the ordinary cross-format case.
    pub fn payload_for(&self, target: Format) -> Option<&NativePayload> {
        (self.format == target).then_some(&self.payload)
    }
}

/// A spoke's own unmodelled content.
///
/// One variant per format that needs one; a format with nothing unmodelled
/// never constructs a [`Native`] at all. Deliberately an enum rather than a
/// `Box<dyn Any>`: [`crate::Feature`] and [`crate::FeatureCollection`] derive
/// `Debug`/`Clone`/`PartialEq` and `dyn Any` satisfies none of them, and an
/// enum keeps the compiler enumerating the blast radius when a spoke is added.
///
/// Payload contents are the spoke's business — this type only says which spoke
/// owns them.
///
/// # `#[non_exhaustive]`
///
/// Consumers must include a wildcard arm when matching. This is the one
/// exported enum that grows *by construction*: it gains a variant per spoke,
/// and M3 has four still to build (CSV, WKT, KML, Shapefile) plus two
/// containers (M3b, M3c). Without the attribute each of those six would be a
/// breaking change for every downstream `match`, which is D7's argument for
/// [`crate::JsonValue`] and [`crate::DeclType`] applied to the type it fits
/// best. Inside the crate nothing changes: the compiler still enumerates the
/// blast radius when a spoke lands, which is what the enum is for.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum NativePayload {
    /// GeoJSON members the reader skipped, in source order: `id`, `bbox`, and
    /// any foreign member, at whichever level the [`Native`] is attached to.
    ///
    /// Keys are `Rc<str>` to match [`crate::Feature::properties`], since a
    /// foreign member repeated across every feature is the common shape.
    GeoJson(Vec<(Rc<str>, JsonValue)>),
    /// FlatGeobuf header and sub-table members with no IR slot. Collection
    /// level only — FlatGeobuf's per-*feature* unmodelled surface is the
    /// `Feature.columns` schema override, which nothing observed emits.
    FlatGeobuf(FgbNative),
    /// GeoParquet file-level metadata with no IR slot. Collection level only.
    Parquet(ParquetNative),
    /// A GeoPackage *layer*'s unmodelled members. Collection level only, and
    /// deliberately layer-scoped: the container's own unmodelled content (user
    /// tables, `gpkg_metadata`, extension registrations) belongs to no single
    /// layer and is M3b's, not this payload's — see
    /// `plans/same-format-fidelity.org` § THE DOMAIN OF THE RULE.
    Gpkg(GpkgNative),
}

/// A GeoPackage layer's descriptive members: its `gpkg_contents` title and
/// description, and the `gpkg_data_columns` rows describing its columns.
///
/// **`identifier` is absent when it merely repeats the table name.** GDAL
/// writes `identifier = table_name` for every layer it creates, so carrying
/// that verbatim would put a native on essentially every GeoPackage ever read
/// and make [`Self::is_empty`] permanently false — and worse, would replay a
/// *stale* title into a layer that had since been renamed. An identifier equal
/// to the layer name is **derived** under D4 and regenerated by the writer; one
/// that differs is content somebody authored, and is carried. This is decision
/// 14's shape ("an opaque member can also be derived") applied to a second
/// spoke.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpkgNative {
    /// `gpkg_contents.identifier`, when it says something the layer name does
    /// not. A human-readable title; [`crate::FeatureCollection::name`] models
    /// the *identity*, which is `table_name`.
    ///
    /// Not promoted to an IR field under D1: only FlatGeobuf (`title`) and
    /// GeoPackage have a slot for one, and the FlatGeobuf spoke already ruled
    /// on this exact split for `Crs.name` — two spokes is not four.
    pub identifier: Option<String>,
    /// `gpkg_contents.description`, when it is not empty.
    ///
    /// An empty description is dropped for the same reason a redundant
    /// `identifier` is: it is exactly what this writer emits when there is
    /// nothing to say, and `ogr2ogr` writes `''` (not NULL) for every layer it
    /// creates — so carrying it would put a payload on every GeoPackage in the
    /// world to record the absence of a description.
    ///
    /// *This does not follow [`FgbNative`]'s "absent and empty are different"
    /// rule, deliberately.* That rule holds there because the FlatGeobuf writer
    /// omits an absent field entirely, so the distinction survives into the
    /// file. `gpkg_contents.description` is a column in a row that is always
    /// written, so NULL and `''` are the same output either way, and the
    /// distinction has nowhere to live.
    pub description: Option<String>,
    /// `gpkg_contents.last_change` — the standard's "timestamp of last change
    /// to content", carried verbatim so a same-format run does not destroy it.
    ///
    /// *Dropped when it is the epoch sentinel this crate itself writes*, on
    /// exactly the [`Self::identifier`] precedent: a value the writer would
    /// regenerate anyway is not worth a payload. So a GeoPackage this crate
    /// produced carries nothing here, and D3's "the common case costs nothing"
    /// survives for our own output.
    ///
    /// **This deliberately does not follow [`Self::description`]'s argument**,
    /// and the difference is worth stating because the two look alike. Dropping
    /// an empty `description` avoids putting a payload on every GeoPackage in
    /// the world *to record the absence of information*. `last_change` is
    /// present and meaningful in every file another tool wrote, so carrying it
    /// records something real — and the alternative is destroying it, which is
    /// what this crate did until 2026-08-24.
    ///
    /// *Never stamped fresh here.* The rule decided in
    /// `plans/same-format-fidelity.org` § OPEN QUESTIONS is that the stamp
    /// follows whoever last changed the content, and that is never geosetta —
    /// it does not reproject, and `--crs` relabels without moving a coordinate.
    /// A cross-format source simply has none to carry, and the writer's own
    /// constant stands in. /Writing a fresh stamp there is the other half of
    /// that rule and is deliberately not implemented; see the plan./
    pub last_change: Option<String>,
    /// `gpkg_data_columns` rows for this layer, in column order. GDAL surfaces
    /// these as a field's alternative name, comment and domain, so they are
    /// visible content rather than inert bookkeeping.
    pub data_columns: Vec<GpkgDataColumn>,
    /// The `gpkg_data_column_constraints` rows [`Self::data_columns`] refer to
    /// by `constraint_name`.
    ///
    /// Container-scoped on disk (constraints are named, and two layers may
    /// share one) but carried per layer, because a constraint with no data
    /// column referring to it describes nothing. A file whose two layers share
    /// a constraint therefore carries it twice and the writer de-duplicates on
    /// the way out.
    pub constraints: Vec<GpkgConstraint>,
}

impl GpkgNative {
    /// Whether the layer recorded none of this, in which case no [`Native`]
    /// need be constructed (D3). True of every GeoPackage this crate writes
    /// that had nothing to carry, and of ordinary `ogr2ogr` output.
    pub fn is_empty(&self) -> bool {
        self.identifier.is_none()
            && self.description.is_none()
            && self.last_change.is_none()
            && self.data_columns.is_empty()
            && self.constraints.is_empty()
    }
}

/// One `gpkg_data_columns` row: what a GeoPackage records *about* a column,
/// as opposed to [`crate::FieldDecl`]'s declaration of what it can hold.
///
/// Field names are the GeoPackage spec's own. Note that `name` here is the
/// column's *alternative* name (GDAL's "alternative name"), not its identity —
/// the identity is `column_name`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpkgDataColumn {
    /// The column this describes, matched to the property key on replay.
    pub column_name: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    /// Names a row in `gpkg_data_column_constraints`; GDAL reports it as the
    /// field's domain.
    pub constraint_name: Option<String>,
}

/// One `gpkg_data_column_constraints` row, carried verbatim. Nothing in this
/// crate interprets a constraint — it is not applied to values on read or
/// enforced on write, only put back where it was found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GpkgConstraint {
    pub constraint_name: String,
    /// `"range"`, `"enum"` or `"glob"` per the spec; unvalidated here.
    pub constraint_type: String,
    pub value: Option<String>,
    pub min: Option<f64>,
    pub min_is_inclusive: Option<bool>,
    pub max: Option<f64>,
    pub max_is_inclusive: Option<bool>,
    pub description: Option<String>,
}

/// A GeoParquet file's unmodelled metadata: the `key_value_metadata` entries
/// beside `geo`, and the `geo` members this crate does not model.
///
/// Two file-level members are *deliberately absent*, both for the reason the
/// inventory already gives for `version`: they describe **the file geosetta is
/// writing**, not the one it read. `created_by` names the application that
/// wrote the file, and geosetta did; `version` states the GeoParquet version
/// of the document it just produced. Replaying either would make the output
/// claim something untrue about itself.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParquetNative {
    /// `key_value_metadata` entries other than `geo`, in file order —
    /// `ARROW:schema`, `gdal:metadata`, and anything else a producer wrote.
    /// `geo` is excluded because geosetta regenerates it from the geometry it
    /// actually wrote.
    pub key_value: Vec<(String, String)>,
    /// The primary geometry column's `orientation` member inside `geo`.
    /// Present in real `ogr2ogr` output (`"counterclockwise"`), and never read
    /// or written before this spoke.
    pub orientation: Option<String>,
    /// The property column names this metadata was read alongside, in order.
    ///
    /// Not itself a recovered member — it is the *validity check* for
    /// [`Self::schema_derived`] keys. `ARROW:schema` restates the file's own
    /// column types, so replaying it into a file whose columns have changed
    /// would leave an Arrow reader with a schema contradicting the Parquet
    /// one. D4 forbids replaying derived data blindly; this records just
    /// enough to tell whether it is still true.
    pub columns: Vec<String>,
}

impl ParquetNative {
    /// Whether `key` restates the file's own schema, and so may only be
    /// replayed while the columns still match.
    pub fn schema_derived(key: &str) -> bool {
        key == "ARROW:schema"
    }

    pub fn is_empty(&self) -> bool {
        self.key_value.is_empty() && self.orientation.is_none()
    }
}

/// The FlatGeobuf header's descriptive members, plus the two sub-tables' —
/// everything `flatgeobuf::reader` recovers but [`crate::FeatureCollection`]
/// has nowhere to put.
///
/// Field names are FlatGeobuf's own, and the slot numbers in each doc line are
/// the ones `reader.rs`/`writer.rs` use, so this type can be checked against a
/// hand-decoded file. `Option` throughout because absent and empty are
/// different: a writer that emitted `title: ""` said something, and re-emitting
/// it as absent would not be putting the file back.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FgbNative {
    /// Header `title(11)`.
    pub title: Option<String>,
    /// Header `description(12)`.
    pub description: Option<String>,
    /// Header `metadata(13)`. Real `ogr2ogr` output puts a JSON object here —
    /// a GeoPackage's `gpkg_metadata` rows, a Shapefile's
    /// `DBF_DATE_LAST_UPDATE` — so this is the member most likely to be
    /// populated in the wild, and the reader had no constant for it at all.
    pub metadata: Option<String>,
    /// `Crs.name(2)`. Present in every `ogr2ogr`-written file observed
    /// (`"WGS 84"`); [`crate::NamedCrs`] models authority, code and definition
    /// text, not the human-readable name.
    pub crs_name: Option<String>,
    /// `Crs.description(3)`.
    pub crs_description: Option<String>,
    /// `Crs.code_string(5)` — the alphanumeric-code case that a numeric
    /// `code(1)` cannot hold.
    pub crs_code_string: Option<String>,
    /// Per-column descriptive members, in header column order. Only columns
    /// that carried at least one appear.
    pub columns: Vec<FgbColumnNative>,
}

impl FgbNative {
    /// Whether the header recorded none of this, in which case there is
    /// nothing to carry and no [`Native`] need be constructed (D3).
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.metadata.is_none()
            && self.crs_name.is_none()
            && self.crs_description.is_none()
            && self.crs_code_string.is_none()
            && self.columns.is_empty()
    }
}

/// One column's descriptive members — the three of `Column`'s eleven fields
/// that are neither name/type nor part of [`crate::FieldDecl`]'s declaration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FgbColumnNative {
    /// The column this describes, matched to the property key on replay.
    pub name: String,
    /// `Column.title(2)`.
    pub title: Option<String>,
    /// `Column.description(3)`.
    pub description: Option<String>,
    /// `Column.metadata(10)`.
    pub metadata: Option<String>,
}

impl FgbColumnNative {
    pub fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none() && self.metadata.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geojson_native() -> Native {
        Native::new(
            Format::GeoJson,
            NativePayload::GeoJson(vec![(
                Rc::from("id"),
                JsonValue::Int(42),
            )]),
        )
    }

    /// The whole safety property: a payload is visible only to the format that
    /// produced it, so a cross-format run cannot replay one by accident.
    #[test]
    fn payload_is_visible_only_to_its_own_format() {
        let n = geojson_native();
        assert!(n.payload_for(Format::GeoJson).is_some());
        for other in [
            Format::Parquet,
            Format::FlatGeobuf,
            Format::Csv,
            Format::Wkt,
            Format::Gpkg,
            Format::Shapefile,
            Format::Kml,
            Format::Kmz,
        ] {
            assert!(n.payload_for(other).is_none(), "{other:?} saw a GeoJSON payload");
        }
    }

    #[test]
    fn payload_round_trips_through_the_tag_check() {
        let n = geojson_native();
        let Some(NativePayload::GeoJson(members)) = n.payload_for(Format::GeoJson) else {
            panic!("a GeoJSON-tagged native must carry a GeoJSON payload")
        };
        assert_eq!(members.len(), 1);
        assert_eq!(&*members[0].0, "id");
        assert_eq!(members[0].1, JsonValue::Int(42));
    }
}
