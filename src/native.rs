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
#[derive(Debug, Clone, PartialEq)]
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
                JsonValue::Number { value: 42.0, is_int: true },
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
        assert_eq!(members[0].1, JsonValue::Number { value: 42.0, is_int: true });
    }
}
