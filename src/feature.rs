//! The shared feature model — the intermediate representation every format
//! converts to and from. It is deliberately format-neutral: `geojson` and
//! `parquet` both depend on it, not on each other.

use crate::crs::Crs;
use crate::format::Format;
use crate::geometry::{Geometry, Position};
use crate::json::JsonValue;
use crate::native::Native;
use crate::schema::FieldDecl;
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
    pub native: Option<Native>,
}

/// A collection of features.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FeatureCollection {
    pub features: Vec<Feature>,
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
    /// (GeoJSON, CSV, WKT, KML) or declared nothing.
    ///
    /// Modelled here for the same reason as [`Self::name`], and see
    /// [`crate::schema::FieldDecl`] for the argument in full: four spokes give
    /// these first-class slots, and only a modelled field can be *edited by an
    /// external tool mid-pipeline*, which is what lets a middle stage widen a
    /// declaration alongside the value it lengthened.
    ///
    /// Entries are matched to properties by exact name; one that names no
    /// property is inert. This is a declaration *about* the data, never a
    /// substitute for it — [`crate::schema::columns_for`] attaches these to
    /// inferred columns, and a writer treats a declared size as a floor rather
    /// than a ceiling.
    pub schema: Vec<FieldDecl>,
    /// Collection-level unmodelled content, tagged by source format — see
    /// [`Feature::native`] and [`crate::native`].
    pub native: Option<Native>,
}

impl FeatureCollection {
    /// A collection with no recorded CRS. The common constructor for readers of
    /// formats that carry no coordinate-reference metadata (CSV, WKT) and for
    /// tests; set [`FeatureCollection::crs`] afterwards when a CRS is known.
    pub fn new(features: Vec<Feature>) -> FeatureCollection {
        FeatureCollection {
            features,
            crs: None,
            name: None,
            schema: Vec::new(),
            native: None,
        }
    }

    /// Whether any feature's geometry carries an M (measure) ordinate.
    pub fn has_m(&self) -> bool {
        self.features.iter().filter_map(|f| f.geometry.as_ref()).any(Geometry::has_m)
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
    /// declares nothing (GeoJSON, CSV, WKT, KML) and so the overwhelmingly
    /// common case — the rule against warning noise in
    /// `plans/lossy-conversion-warnings.org` § THE RULE.
    pub fn schema_drop_warning(&self, target: Format) -> Option<String> {
        if self.schema.is_empty() || target.carries_column_schema() {
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
        for feature in &mut self.features {
            if let Some(g) = &mut feature.geometry {
                g.for_each_position_mut(&mut f);
            }
        }
    }

    /// Visit every contiguous run of coordinates across every feature's
    /// geometry by mutable slice (features with no geometry are skipped) —
    /// the batch-friendly counterpart to [`Self::for_each_position_mut`]; see
    /// [`crate::Geometry::for_each_position_run_mut`] for what counts as a
    /// "run" and why a reprojection backend would want one.
    pub fn for_each_position_run_mut(&mut self, mut f: impl FnMut(&mut [Position])) {
        for feature in &mut self.features {
            if let Some(g) = &mut feature.geometry {
                g.for_each_position_run_mut(&mut f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry;

    #[test]
    fn m_downgrade_warning_fires_only_for_m_incapable_targets_with_real_m() {
        let fc = FeatureCollection::new(vec![Feature {
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
        let two_d = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![],
            native: None,
        }]);
        let z_only = FeatureCollection::new(vec![Feature {
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
        let mut fc = FeatureCollection::new(vec![]);
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
    fn schema_drop_warning_stays_silent_when_nothing_was_declared() {
        // The common case by a wide margin — a source with no schema of its
        // own must not add a line to every conversion.
        let fc = FeatureCollection::new(vec![Feature::default()]);
        for target in [Format::GeoJson, Format::Csv, Format::Gpkg, Format::Parquet, Format::Kml] {

            assert_eq!(fc.schema_drop_warning(target), None, "{target:?}");
        }
    }

    #[test]
    fn for_each_position_mut_rewrites_every_feature_and_skips_geometryless_ones() {
        let mut fc = FeatureCollection::new(vec![
            Feature { geometry: Some(Geometry::Point(Position::new(1.0, 2.0))), properties: vec![], native: None },
            Feature { geometry: None, properties: vec![], native: None },
            Feature { geometry: Some(Geometry::Point(Position::new(3.0, 4.0))), properties: vec![], native: None },
        ]);
        fc.for_each_position_mut(|p| {
            p.x *= 10.0;
            p.y *= 10.0;
        });
        assert_eq!(fc.features[0].geometry, Some(Geometry::Point(Position::new(10.0, 20.0))));
        assert_eq!(fc.features[1].geometry, None);
        assert_eq!(fc.features[2].geometry, Some(Geometry::Point(Position::new(30.0, 40.0))));
    }

    #[test]
    fn for_each_position_run_mut_rewrites_every_feature_and_skips_geometryless_ones() {
        let mut fc = FeatureCollection::new(vec![
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
            fc.features[0].geometry,
            Some(Geometry::LineString(vec![Position::new(10.0, 20.0), Position::new(30.0, 40.0)]))
        );
        assert_eq!(fc.features[1].geometry, None);
        assert_eq!(fc.features[2].geometry, Some(Geometry::Point(Position::new(50.0, 60.0))));
    }
}
