//! The shared feature model — the intermediate representation every format
//! converts to and from. It is deliberately format-neutral: `geojson` and
//! `parquet` both depend on it, not on each other.

use crate::crs::Crs;
use crate::format::Format;
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::native::Native;
use crate::schema::FieldDecl;
use crate::table::{ColumnRef, Layout, Properties, RowRef, RowTableBuilder, Sizes, Table, TableBuilder, Value};
use std::rc::Rc;

/// A vector feature: an optional geometry plus ordered properties.
///
/// Property keys are `Rc<str>` rather than `String` so that a column name
/// repeated across every row is allocated once and shared (a refcount bump per
/// cell), not re-allocated per feature — the dominant cost on wide tables.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Feature {
    pub geometry: Option<Geometry>,
    /// Property members, in order. Empty when there are no properties.
    pub properties: Vec<(Rc<str>, JsonValue)>,
    /// What this feature's source recorded that the two fields above cannot
    /// hold — a GeoJSON `id`, a foreign member — kept verbatim and tagged with
    /// the format that produced it, so only that format's writer replays it.
    /// `None` whenever the reader modelled everything, which is the common
    /// case. See [`crate::native`].
    ///
    /// Boxed because it is so rarely present: inline, the `Option` made every
    /// feature 168 bytes larger than one carrying a pointer
    /// (`plans/performance.org`, *Memory*).
    pub native: Option<Box<Native>>,
}

/// A collection of features.
///
/// The features are held in blocks, each as columns or as rows of shared
/// shapes (`plans/columnar.org` D-C10, D-C13), and are read in place through
/// [`Self::row`], [`Self::rows`] and [`Self::column`], built with
/// [`Self::from_features`] and [`Self::push`], and edited through
/// [`Self::set`] and the other edits. How the blocks hold them is never
/// visible: every [`Layout`] reads and writes the same features.
#[derive(Debug, Clone, Default)]
pub struct FeatureCollection {
    table: Table,
    /// The coordinate reference system the geometries are expressed in, as
    /// recovered from the source. `None` means the source recorded no CRS.
    /// Geosetta never reprojects — this is carried through to the output
    /// unchanged (see [`crate::crs`]).
    pub crs: Option<Crs>,
    /// The collection's own name, when the source recorded one — GeoJSON's
    /// `name`, a FlatGeobuf header's `NAME`, a GeoPackage layer's table name,
    /// a Shapefile's stem.
    ///
    /// Modelled here rather than carried as a [`Native`] because four spokes
    /// give it a first-class slot, and because a native never crosses a process
    /// boundary: a name that rides in the transit format's own header survives
    /// `geosetta a.gpkg - --to fgb | tool | geosetta - a.gpkg --from fgb`,
    /// where a payload would not. Losing it is what makes that pipeline append
    /// a duplicate layer instead of replacing one.
    pub name: Option<String>,
    /// What the source declared about its property columns — widths,
    /// precisions, `NOT NULL`, and so on — for the columns it declared
    /// anything about. Empty when the source has no schema of its own
    /// (GeoJSON, CSV, WKT) or declared nothing.
    ///
    /// Modelled here for the same reason as [`Self::name`], and see
    /// [`crate::schema::FieldDecl`] for the argument in full: four spokes give
    /// these first-class slots, and only a modelled field can be *edited by an
    /// external tool mid-pipeline*, which is what lets a middle stage widen a
    /// declaration alongside the value it lengthened.
    ///
    /// Entries are matched to properties by exact name; one that names no
    /// property is inert. This is a declaration *about* the data, never a
    /// substitute for it — the crate-internal `columns_for` attaches these to
    /// inferred columns, and a writer treats a declared size as a floor rather
    /// than a ceiling.
    pub schema: Vec<FieldDecl>,
    /// Collection-level unmodelled content, tagged by source format — see
    /// [`Feature::native`] and [`crate::native`].
    pub native: Option<Native>,
}

/// One feature of a collection, read in place (`plans/columnar.org` D-C8):
/// what [`FeatureCollection::row`] and [`FeatureCollection::rows`] give.
/// Nothing is copied to read one; [`Self::to_feature`] makes an owned copy.
#[derive(Clone, Copy)]
pub struct FeatureRef<'a>(RowRef<'a>);

impl<'a> FeatureRef<'a> {
    pub fn geometry(self) -> Option<&'a Geometry> {
        self.0.geometry()
    }

    /// What the feature's source recorded beyond its geometry and
    /// properties; see [`Feature::native`].
    pub fn native(self) -> Option<&'a Native> {
        self.0.native()
    }

    /// The feature's properties, in its own order, a repeated key as many
    /// times as it holds it.
    pub fn properties(self) -> FeatureProperties<'a> {
        FeatureProperties(self.0.properties())
    }

    /// The feature's first value for `key`, or `None` when it has none.
    pub fn get(self, key: &str) -> Option<Value<'a>> {
        self.0.get(key)
    }

    /// The feature as an owned [`Feature`], copied.
    pub fn to_feature(self) -> Feature {
        self.0.to_feature()
    }
}

impl std::fmt::Debug for FeatureRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureRef")
            .field("geometry", &self.geometry())
            .field("properties", &self.properties().collect::<Vec<_>>())
            .field("native", &self.native())
            .finish()
    }
}

/// [`FeatureRef::properties`]: each key and its value, borrowed.
pub struct FeatureProperties<'a>(Properties<'a>);

impl<'a> Iterator for FeatureProperties<'a> {
    type Item = (&'a Rc<str>, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// [`FeatureCollection::rows`].
pub struct FeatureRefs<'a> {
    table: &'a Table,
    rows: std::ops::Range<usize>,
}

impl<'a> Iterator for FeatureRefs<'a> {
    type Item = FeatureRef<'a>;

    fn next(&mut self) -> Option<FeatureRef<'a>> {
        self.rows.next().map(|i| FeatureRef(self.table.row(i)))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl ExactSizeIterator for FeatureRefs<'_> {}

/// What a reader with no schema fills (C3): its features, one at a time,
/// into blocks sealed under the layout asked for.
pub(crate) struct Collecting(RowTableBuilder);

impl Collecting {
    pub(crate) fn new(layout: Layout) -> Collecting {
        Collecting(RowTableBuilder::new(layout))
    }

    pub(crate) fn push(&mut self, feature: Feature) {
        self.0.push(feature);
    }

    pub(crate) fn finish(self) -> FeatureCollection {
        FeatureCollection::from_table(self.0.finish())
    }
}

/// What a tabular reader fills (C2): every row holds every key, in declared
/// order, and goes straight into column blocks.
pub(crate) struct Tabular(TableBuilder);

impl Tabular {
    pub(crate) fn new(keys: Vec<Rc<str>>, layout: Layout) -> Tabular {
        Tabular(TableBuilder::new(keys, layout))
    }

    /// Append a row: its geometry, a value for each key in declared order,
    /// and its native.
    pub(crate) fn push(
        &mut self,
        geometry: Option<Geometry>,
        values: impl IntoIterator<Item = JsonValue>,
        native: Option<Box<Native>>,
    ) {
        self.0.push(geometry, values, native);
    }

    pub(crate) fn finish(self) -> FeatureCollection {
        FeatureCollection::from_table(self.0.finish())
    }

    /// A collection read a column at a time: `geometry` is every row's, and
    /// `columns` yields key `i`'s value for every row when called with `i`.
    /// Each column is moved into the blocks and dropped before the next is
    /// asked for, so the reader never holds two.
    pub(crate) fn from_columns(
        keys: Vec<Rc<str>>,
        geometry: Vec<Option<Geometry>>,
        columns: impl FnMut(usize) -> crate::Result<Vec<JsonValue>>,
        layout: Layout,
    ) -> crate::Result<FeatureCollection> {
        let table = Table::from_columns(keys, geometry, columns, layout, Sizes::DEFAULT)?;
        Ok(FeatureCollection::from_table(table))
    }
}

/// Two collections are equal when their features are, each read in place:
/// the same geometry, native and properties, in order, however each
/// collection's blocks hold them.
impl PartialEq for FeatureCollection {
    fn eq(&self, other: &FeatureCollection) -> bool {
        self.crs == other.crs
            && self.name == other.name
            && self.schema == other.schema
            && self.native == other.native
            && self.len() == other.len()
            && self.rows().zip(other.rows()).all(|(a, b)| {
                a.geometry() == b.geometry() && a.native() == b.native() && a.properties().eq(b.properties())
            })
    }
}

impl FeatureCollection {
    /// Every feature, in order, read in place.
    pub fn rows(&self) -> FeatureRefs<'_> {
        FeatureRefs { table: &self.table, rows: 0..self.table.len() }
    }

    /// Feature `i`, read in place. Panics when `i` is not below [`Self::len`].
    pub fn row(&self, i: usize) -> FeatureRef<'_> {
        assert!(i < self.len(), "row {i} of a collection of {}", self.len());
        FeatureRef(self.table.row(i))
    }

    /// Every feature's first value for `key`, in order: a column, whichever
    /// features hold the key and however each block holds it.
    pub fn column<'a>(&'a self, key: &'a str) -> ColumnRef<'a> {
        self.table.column(key)
    }

    /// A collection holding `features`, in order, with no recorded CRS,
    /// name, schema or native: set those afterwards when they are known.
    pub fn from_features(features: Vec<Feature>) -> FeatureCollection {
        FeatureCollection::from_table(Table::from_features(features, Layout::Auto))
    }

    /// A collection holding `table`: what every reader builds.
    pub(crate) fn from_table(table: Table) -> FeatureCollection {
        FeatureCollection { table, crs: None, name: None, schema: Vec::new(), native: None }
    }

    /// The table, which every edit goes through.
    fn table_mut(&mut self) -> &mut Table {
        &mut self.table
    }

    /// Append `feature` after the last.
    pub fn push(&mut self, feature: Feature) {
        self.table_mut().push(feature);
    }

    /// Give feature `i`'s first `key` the value `value`, in its place; a
    /// feature without the key gains it last. Panics when `i` is not below
    /// [`Self::len`].
    pub fn set(&mut self, i: usize, key: &str, value: JsonValue) {
        assert!(i < self.len(), "row {i} of a collection of {}", self.len());
        self.table_mut().set(i, key, value);
    }

    /// Take every `key` out of feature `i`. Panics when `i` is not below
    /// [`Self::len`].
    pub fn remove(&mut self, i: usize, key: &str) {
        assert!(i < self.len(), "row {i} of a collection of {}", self.len());
        self.table_mut().remove(i, key);
    }

    /// Replace feature `i`'s geometry. Panics when `i` is not below
    /// [`Self::len`].
    pub fn set_geometry(&mut self, i: usize, geometry: Option<Geometry>) {
        assert!(i < self.len(), "row {i} of a collection of {}", self.len());
        self.table_mut().set_geometry(i, geometry);
    }

    /// Replace feature `i`'s native: what its source recorded beyond the
    /// geometry and properties ([`Feature::native`]). Panics when `i` is
    /// not below [`Self::len`].
    pub fn set_native(&mut self, i: usize, native: Option<Native>) {
        assert!(i < self.len(), "row {i} of a collection of {}", self.len());
        self.table_mut().set_native(i, native);
    }

    /// Keep the features `keep` says to, in order.
    pub fn retain(&mut self, keep: impl FnMut(FeatureRef<'_>) -> bool) {
        let mut keep = keep;
        self.table_mut().retain(|r| keep(FeatureRef(r)));
    }

    /// [`Self::retain`] by position: keep feature `i` when `keep(i)`.
    #[cfg(test)]
    pub(crate) fn retain_at(&mut self, mut keep: impl FnMut(usize) -> bool) {
        let mut i = 0;
        self.retain(|_| {
            i += 1;
            keep(i - 1)
        });
    }

    /// The features as owned rows, copied: what a test inside the crate
    /// compares.
    #[cfg(test)]
    pub(crate) fn to_features(&self) -> Vec<Feature> {
        self.rows().map(FeatureRef::to_feature).collect()
    }

    /// Reorder the features: feature `i` becomes the one that was
    /// `order[i]`. Panics unless `order` names every feature exactly once.
    pub fn permute(&mut self, order: &[usize]) {
        let mut seen = vec![false; self.len()];
        assert_eq!(order.len(), seen.len(), "a permutation names every feature once");
        for &i in order {
            assert!(i < seen.len() && !std::mem::replace(&mut seen[i], true), "a permutation names every feature once");
        }
        self.reorder(order);
    }

    /// Every feature's `key`: set to the value where `values` gives `Some`,
    /// taken out where it gives `None`. Panics unless it gives one per
    /// feature.
    pub fn set_column(&mut self, key: &str, values: impl IntoIterator<Item = Option<JsonValue>>) {
        self.table_mut().set_column(key, values);
    }

    /// [`Self::set_column`] with each feature's new value computed from its
    /// index and its old one.
    pub fn map_column(&mut self, key: &str, f: impl FnMut(usize, Option<Value<'_>>) -> Option<JsonValue>) {
        self.table_mut().map_column(key, f);
    }

    /// Take `key` out of every feature.
    pub fn drop_column(&mut self, key: &str) {
        self.table_mut().drop_column(key);
    }

    /// [`Self::permute`], for an order known to be one: recorded as a
    /// selection, and nothing moves (D-C11).
    pub(crate) fn reorder(&mut self, order: &[usize]) {
        self.table.permute(order);
    }

    /// Drop each per-feature native `drop` says to.
    pub(crate) fn drop_natives(&mut self, drop: impl FnMut(&Native) -> bool) {
        self.table.drop_natives(drop);
    }

    /// The table the features are held in.
    pub(crate) fn table(&self) -> &Table {
        &self.table
    }

    /// How many features the collection holds.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether any feature's geometry carries an M (measure) ordinate.
    pub fn has_m(&self) -> bool {
        self.rows().filter_map(FeatureRef::geometry).any(Geometry::has_m)
    }

    /// A warning to print when converting this collection to `target`, or
    /// `None` when `target` can faithfully record every geometry's
    /// ordinates. Like [`crate::crs::Crs::downgrade_warning`], this returns the
    /// message *body* — `main.rs`'s `print_warnings` adds the `warning: `
    /// prefix.
    ///
    /// Mirrors [`crate::crs::Crs::downgrade_warning`]'s shape (a pure,
    /// pre-write predictive check) and is the same standing rule that
    /// prompted it: any format-capability gap that would silently drop
    /// information on conversion must warn instead of just succeeding — see
    /// `plans/lossy-conversion-warnings.org`. Only M is checked: every
    /// format spoke supports Z today, so there is no Z-drop case to warn
    /// about (see [`Format::supports_m`]).
    pub fn m_downgrade_warning(&self, target: Format) -> Option<String> {
        if target.supports_m() || !self.has_m() {
            return None;
        }
        Some(format!(
            "source geometry carries M (measure) values; {} has no way to represent them — \
             M will be dropped from the output.",
            target.display_name()
        ))
    }

    /// A warning to print when converting this collection to `target` and the
    /// column declarations it carries would not survive, or `None` when there
    /// are none or `target` keeps them.
    ///
    /// Same shape and same standing rule as [`Self::m_downgrade_warning`]: a
    /// capability gap that silently drops information must say so. This one
    /// only became sayable once declarations were read at all — before that
    /// nothing was dropped because nothing was recovered — and it is the loss
    /// most likely to matter downstream, since a re-derived schema produces a
    /// file that holds today's rows and cannot hold tomorrow's.
    ///
    /// Silent whenever [`Self::schema`] is empty, which is every source that
    /// declares nothing (GeoJSON, CSV, WKT, a KML with no `<Schema>` and no
    /// times) and so the overwhelmingly common case — the rule against warning
    /// noise in `plans/lossy-conversion-warnings.org` § THE RULE.
    ///
    /// A target that declares a column's type and nothing else — KML — keeps
    /// the types, so it warns only about the declarations that say more: a
    /// width, a precision or scale, or a constraint.
    pub fn schema_drop_warning(&self, target: Format) -> Option<String> {
        if self.schema.is_empty() {
            return None;
        }
        if target.declares_type_only() {
            let mut names: Vec<&str> = self
                .schema
                .iter()
                .filter(|d| !FieldDecl { ty: None, ..(*d).clone() }.is_empty())
                .map(|d| d.name.as_str())
                .collect();
            if names.is_empty() {
                return None;
            }
            names.sort_unstable();
            return Some(format!(
                "source declares a field width/precision/constraints for {}; {} declares a column's type \
                 only, so the rest is not carried — a field sized to hold more than it currently holds comes \
                 back narrower.",
                names.join(", "),
                target.display_name()
            ));
        }
        if target.carries_column_schema() {
            return None;
        }
        let mut names: Vec<&str> = self.schema.iter().map(|d| d.name.as_str()).collect();
        names.sort_unstable();
        Some(format!(
            "source declares a column schema (field width/precision/constraints) for {}; \
             it is not carried to {}, so the output's schema is re-derived from the values \
             — a field sized to hold more than it currently holds comes out narrower.",
            names.join(", "),
            target.display_name()
        ))
    }

    /// The declared column types `target` cannot hold, and what it will
    /// declare instead.
    ///
    /// Silent when every declaration survives, or is replaced by one that
    /// [`crate::DeclType::admits`] it — a Shapefile writing every integer as its one
    /// numeric field type loses nothing, and neither does a GeoPackage writing
    /// `UInt16` as `MEDIUMINT`. It fires on anything `admits` does not
    /// cover, which is a narrowing (a `DateTime` becoming text, a `Json` column
    /// losing the fact that it is JSON) *or* a lateral move (a `Date` written
    /// to FlatGeobuf, which has only `DATETIME`). The message shows both types
    /// rather than claiming which is worse, because `Date → DateTime` is not
    /// narrower and is still not what went in.
    ///
    /// Silent, too, when the target carries no declaration at all — that whole
    /// loss belongs to [`Self::schema_drop_warning`], and reporting it twice
    /// would make the more specific message the noisier one.
    ///
    /// This closes the gap that let three recasts ship silently: the checks
    /// beside it ask whether the target has *a* schema, and none asked whether
    /// the type it holds is still the type that went in. See
    /// `plans/lossy-conversion-warnings.org`.
    pub fn decl_recast_warning(&self, target: Format) -> Option<String> {
        if !target.carries_column_schema() {
            return None;
        }
        let mut recast: Vec<String> = Vec::new();
        for decl in &self.schema {
            let Some(ty) = decl.ty else { continue };
            let Some(landed) = target.recast_column(&decl.name, ty) else { continue };
            if !landed.admits(ty) {
                recast.push(format!("{} ({ty:?} → {landed:?})", decl.name));
            }
        }
        if recast.is_empty() {
            return None;
        }
        recast.sort_unstable();
        Some(format!(
            "{} cannot hold the declared type of {}; the output declares the type shown \
             instead, which is not the type that went in.",
            target.display_name(),
            recast.join(", ")
        ))
    }

    /// Visit every coordinate across every feature's geometry by mutable
    /// reference (features with no geometry are skipped).
    ///
    /// Geosetta itself never calls this — it is the seam an external
    /// reprojection crate plugs into between [`crate::read_features`] and
    /// [`crate::write_features`]: rewrite coordinates in place here, then set
    /// [`Self::crs`] to the new identity. Which library computes the
    /// transform (PROJ bindings, a pure-Rust crate, a hand-rolled Helmert
    /// shift, ...) is entirely up to the caller — geosetta only carries a
    /// [`Crs`] through, it never interprets or transforms one (see
    /// [`crate::crs`]). See [`crate::Geometry::for_each_position_mut`] for
    /// the Z/M contract: both ordinates, when present, are handed to the
    /// callback unexamined — a purely horizontal backend is free to leave
    /// `z` untouched (matches Geosetta's own posture), or a caller wanting
    /// a full 3D transform can read/rewrite it inside the closure.
    pub fn for_each_position_mut(&mut self, mut f: impl FnMut(&mut Position)) {
        self.table_mut().for_each_geometry_mut(|g| g.for_each_position_mut(&mut f));
    }

    /// Visit every contiguous run of coordinates across every feature's
    /// geometry by mutable slice (features with no geometry are skipped) —
    /// the batch-friendly counterpart to [`Self::for_each_position_mut`]; see
    /// [`crate::Geometry::for_each_position_run_mut`] for what counts as a
    /// "run" and why a reprojection backend would want one.
    pub fn for_each_position_run_mut(&mut self, mut f: impl FnMut(&mut [Position])) {
        self.table_mut().for_each_geometry_mut(|g| g.for_each_position_run_mut(&mut f));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry;

    #[test]
    fn m_downgrade_warning_fires_only_for_m_incapable_targets_with_real_m() {
        let fc = FeatureCollection::from_features(vec![Feature {
            geometry: Some(Geometry::Point(Position::with_m(1.0, 2.0, 5.0))),
            properties: vec![],
            native: None,
        }]);
        for target in [Format::GeoJson, Format::Kml, Format::Kmz] {
            let w = fc.m_downgrade_warning(target).unwrap();
            assert!(w.contains('M'), "{w}");
            assert!(w.contains("dropped"), "{w}");
            // Names the target the way that format is actually spelled
            // (`GeoJSON`, not the upper-cased extension `GEOJSON`).
            assert!(w.contains(target.display_name()), "{w}");
        }
        // Every M-capable format spoke stays silent.
        for target in [Format::Parquet, Format::FlatGeobuf, Format::Csv, Format::Wkt, Format::Gpkg, Format::Shapefile]
        {
            assert_eq!(fc.m_downgrade_warning(target), None, "{target:?} should not warn");
        }
    }

    #[test]
    fn m_downgrade_warning_stays_silent_for_2d_and_z_only_sources() {
        let two_d = FeatureCollection::from_features(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
            native: None,
        }]);
        let z_only = FeatureCollection::from_features(vec![Feature {
            geometry: Some(Geometry::Point(Position::with_z(1.0, 2.0, 3.0))),
            properties: vec![],
            native: None,
        }]);
        for target in [Format::GeoJson, Format::Kml, Format::Kmz] {
            assert_eq!(two_d.m_downgrade_warning(target), None);
            assert_eq!(z_only.m_downgrade_warning(target), None);
        }
    }

    #[test]
    fn schema_drop_warning_fires_only_where_a_declaration_is_actually_lost() {
        let mut fc = FeatureCollection::from_features(vec![]);
        fc.schema = vec![FieldDecl { name: "name".into(), width: Some(80), ..FieldDecl::default() }];
        for target in [Format::GeoJson, Format::Csv, Format::Wkt, Format::Kml, Format::Kmz] {
            let w = fc.schema_drop_warning(target).unwrap();
            assert!(w.contains("name"), "{w}");
            assert!(w.contains(target.display_name()), "{w}");
        }
        // The spokes that carry declarations stay silent. GeoPackage joined
        // them in M4 — it reads and writes a column's declared type — so a
        // declaration crossing it is no longer lost and no longer warns.
        for target in [Format::FlatGeobuf, Format::Shapefile, Format::Gpkg, Format::Parquet] {
            assert_eq!(fc.schema_drop_warning(target), None, "{target:?} should not warn");
        }
    }

    #[test]
    fn the_recast_message_states_the_change_without_ranking_it() {
        // A `Date` written to FlatGeobuf lands as `DATETIME`, which is *wider*
        // — the message may not call it a narrowing. It fires because it is not
        // what went in, which is a different claim and the only one both cases
        // support. (`Json -> String` in Shapefile is a genuine narrowing, and
        // gets the same wording: the message shows both types and lets the
        // reader judge.)
        let mut fc = FeatureCollection::from_features(vec![]);
        fc.schema = vec![FieldDecl {
            name: "when".into(),
            ty: Some(crate::schema::DeclType::Date),
            ..FieldDecl::default()
        }];
        let w = fc.decl_recast_warning(Format::FlatGeobuf).unwrap();
        assert!(w.contains("when (Date → DateTime)"), "{w}");
        assert!(!w.contains("narrower"), "a lateral recast must not be called a narrowing: {w}");
    }

    #[test]
    fn schema_drop_warning_stays_silent_when_nothing_was_declared() {
        // The common case by a wide margin — a source with no schema of its
        // own must not add a line to every conversion.
        let fc = FeatureCollection::from_features(vec![Feature::default()]);
        for target in [Format::GeoJson, Format::Csv, Format::Gpkg, Format::Parquet, Format::Kml] {

            assert_eq!(fc.schema_drop_warning(target), None, "{target:?}");
        }
    }

    #[test]
    fn for_each_position_mut_rewrites_every_feature_and_skips_geometryless_ones() {
        let mut fc = FeatureCollection::from_features(vec![
            Feature { geometry: Some(Geometry::Point(Position::new(1.0, 2.0))), properties: vec![], native: None },
            Feature { geometry: None, properties: vec![], native: None },
            Feature { geometry: Some(Geometry::Point(Position::new(3.0, 4.0))), properties: vec![], native: None },
        ]);
        fc.for_each_position_mut(|p| {
            p.x *= 10.0;
            p.y *= 10.0;
        });
        assert_eq!(fc.to_features()[0].geometry, Some(Geometry::Point(Position::new(10.0, 20.0))));
        assert_eq!(fc.to_features()[1].geometry, None);
        assert_eq!(fc.to_features()[2].geometry, Some(Geometry::Point(Position::new(30.0, 40.0))));
    }

    #[test]
    fn for_each_position_run_mut_rewrites_every_feature_and_skips_geometryless_ones() {
        let mut fc = FeatureCollection::from_features(vec![
            Feature {
                geometry: Some(Geometry::LineString(vec![Position::new(1.0, 2.0), Position::new(3.0, 4.0)])),
                properties: vec![],
                native: None,
            },
            Feature { geometry: None, properties: vec![], native: None },
            Feature { geometry: Some(Geometry::Point(Position::new(5.0, 6.0))), properties: vec![], native: None },
        ]);
        let mut run_lengths = Vec::new();
        fc.for_each_position_run_mut(|ps| {
            run_lengths.push(ps.len());
            for p in ps {
                p.x *= 10.0;
                p.y *= 10.0;
            }
        });
        assert_eq!(run_lengths, vec![2, 1]);
        assert_eq!(
            fc.to_features()[0].geometry,
            Some(Geometry::LineString(vec![Position::new(10.0, 20.0), Position::new(30.0, 40.0)]))
        );
        assert_eq!(fc.to_features()[1].geometry, None);
        assert_eq!(fc.to_features()[2].geometry, Some(Geometry::Point(Position::new(50.0, 60.0))));
    }
}
