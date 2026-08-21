//! **Can a consumer actually do the thing the library says it is for?**
//!
//! `plans/same-format-fidelity.org` D5 argues that the column schema is
//! *modelled in the IR* rather than carried as an opaque native, because only a
//! modelled field lets an external tool widen a declaration mid-flight
//! alongside the value that outgrew it. `plans/edit-composability.org` makes the
//! same argument for feature data. Both arguments are about a **caller**, and
//! until 2026-08-21 neither was testable from where a caller stands:
//! `Feature::properties` is a public field of type `JsonValue`, and
//! `FieldDecl::ty` a public field of type `DeclType`, and *neither type could be
//! named outside this crate*. A consumer could read both and construct neither.
//!
//! These tests live in `tests/` on purpose. An integration test is compiled as
//! a separate crate, so it sees exactly the API a real dependent sees — a unit
//! test inside `src/` cannot check this property at all, because inside the
//! crate everything is nameable.

// So `the_exported_vocabularies_are_open` *enforces* rather than illustrates:
// a wildcard arm beside every known variant is reachable only because the enum
// is `#[non_exhaustive]`. Drop the attribute and the arm becomes unreachable,
// and this turns that lint into a build failure.
#![deny(unreachable_patterns)]

use geosetta::{
    DeclType, Feature, FeatureCollection, FieldDecl, Format, Geometry, JsonValue, NativePayload,
    Position,
};

/// Build a collection from nothing: geometry, properties, and a declared
/// column type. Every one of these lines needs a type that was unnameable.
fn built_by_hand() -> FeatureCollection {
    let mut fc = FeatureCollection::new(vec![Feature {
        geometry: Some(Geometry::Point(Position::new(12.5, 55.6))),
        properties: vec![
            ("name".into(), JsonValue::String("København".into())),
            ("count".into(), JsonValue::Int(7)),
            ("blob".into(), JsonValue::Binary(vec![0xde, 0xad])),
        ],
        native: None,
    }]);
    // `FieldDecl::new` names it and declares nothing; `..` fills the rest.
    fc.schema = vec![
        FieldDecl { width: Some(80), ..FieldDecl::new("name") },
        FieldDecl { ty: Some(DeclType::Int16), ..FieldDecl::new("count") },
        FieldDecl { ty: Some(DeclType::Binary), ..FieldDecl::new("blob") },
    ];
    fc
}

/// Route (a): construct, write, read back. The declaration a caller *authored*
/// must reach the file.
#[test]
fn a_caller_can_build_a_collection_and_its_declarations_reach_the_file() {
    let fc = built_by_hand();
    let bytes = geosetta::write_features(Format::FlatGeobuf, &fc).unwrap();
    let back = geosetta::read_features(Format::FlatGeobuf, &bytes).unwrap();

    let decl = |name: &str| back.schema.iter().find(|d| d.name == name).unwrap().clone();
    assert_eq!(decl("count").ty, Some(DeclType::Int16), "a caller-set type was lost");
    assert_eq!(decl("name").width, Some(80), "a caller-set width was lost");
    assert_eq!(decl("blob").ty, Some(DeclType::Binary));

    let prop = |name: &str| {
        back.features[0].properties.iter().find(|(k, _)| &**k == name).unwrap().1.clone()
    };
    assert_eq!(prop("name"), JsonValue::String("København".into()));
    assert_eq!(prop("blob"), JsonValue::Binary(vec![0xde, 0xad]));
}

/// Route (a) again, and the case D5 exists for: read, **widen a declaration in
/// the middle**, write. This is the "external tool widens a declaration
/// alongside the value that outgrew it" story, executed through the public API
/// for the first time.
#[test]
fn a_caller_can_widen_a_declaration_between_a_read_and_a_write() {
    let bytes = geosetta::write_features(Format::FlatGeobuf, &built_by_hand()).unwrap();
    let mut fc = geosetta::read_features(Format::FlatGeobuf, &bytes).unwrap();

    // The middle stage: a value arrives that the declared Int16 cannot hold,
    // so the caller widens the declaration to match.
    for feature in &mut fc.features {
        for (key, value) in &mut feature.properties {
            if &**key == "count" {
                *value = JsonValue::Int(100_000);
            }
        }
    }
    for decl in &mut fc.schema {
        if decl.name == "count" {
            decl.ty = Some(DeclType::Int32);
        }
    }

    let out = geosetta::write_features(Format::FlatGeobuf, &fc).unwrap();
    let back = geosetta::read_features(Format::FlatGeobuf, &out).unwrap();
    let count = back.schema.iter().find(|d| d.name == "count").unwrap();
    assert_eq!(count.ty, Some(DeclType::Int32), "the widened declaration did not survive");
    let value = back.features[0].properties.iter().find(|(k, _)| &**k == "count").unwrap().1.clone();
    assert_eq!(value, JsonValue::Int(100_000));
}

/// The exported enums are `#[non_exhaustive]`, which is only meaningful from
/// outside the crate — so this is the one place it can be shown. A wildcard arm
/// is required; without it this does not compile.
///
/// `NativePayload` is here for the same reason and a stronger one: it gains a
/// variant *per spoke*, and six are still to come (CSV, WKT, KML, Shapefile,
/// and the two containers). It was exhaustive until this test was written, so
/// each of those six would have broken every downstream `match`.
#[test]
fn the_exported_vocabularies_are_open() {
    let described = |v: &JsonValue| match v {
        JsonValue::Binary(b) => format!("{} bytes", b.len()),
        JsonValue::String(s) => s.clone(),
        // Required: adding a variant must not break a consumer.
        _ => "other".to_string(),
    };
    assert_eq!(described(&JsonValue::Binary(vec![1, 2, 3])), "3 bytes");

    let is_temporal = |t: DeclType| matches!(t, DeclType::Date | DeclType::DateTime);
    assert!(is_temporal(DeclType::Date));
    assert!(!is_temporal(DeclType::Int16));

    // The spoke vocabulary, which grows every time M3 lands one. Every variant
    // that exists is named here *and* a wildcard follows: with the crate-level
    // `deny(unreachable_patterns)` above, that combination compiles only while
    // the enum is `#[non_exhaustive]`, so this is the arm that actually holds
    // the attribute in place. (The two matches above are demonstrations — they
    // do not list every variant, so they would compile either way.)
    let spoke = |p: &NativePayload| match p {
        NativePayload::GeoJson(_) => "geojson",
        NativePayload::FlatGeobuf(_) => "fgb",
        NativePayload::Parquet(_) => "parquet",
        NativePayload::Gpkg(_) => "gpkg",
        // Required: the next spoke must not break a consumer.
        _ => "other",
    };
    assert_eq!(spoke(&NativePayload::GeoJson(Vec::new())), "geojson");
}
