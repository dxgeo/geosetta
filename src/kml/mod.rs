//! KML/KMZ format spoke — see `plans/kml.org` for the design writeup.
//!
//! `.kml` (plain XML) read/write routes through [`crate::xml`]; `.kmz` wraps
//! the same `.kml` bytes in a [`crate::zip`] container.

use crate::error::{Error, Result};
use crate::feature::FeatureCollection;
use crate::format::{Format, KmlVersion};

pub(crate) mod model;
pub(crate) mod document;
mod lower;
mod project;
mod reader;
mod place;
mod reconcile;
mod writer;

pub(crate) use reader::read as read_kml;
pub(crate) use writer::write as write_kml;

/// Whether `key` is one of the properties a feature's time projects to.
pub(crate) fn is_time_key(key: &str) -> bool {
    matches!(key, project::TIMESTAMP | project::BEGIN | project::END)
}

/// What a column declared `ty` reads back as from a KML write: the field type
/// the writer gives it, as the reader declares that (`Format::recast`).
pub(crate) fn field_type_recast(ty: crate::schema::DeclType) -> crate::schema::DeclType {
    project::FieldType::for_decl(ty).decl()
}

/// What writing `fc` as `target`, in `version` when the caller chose one,
/// cannot carry of its KML features' elements: a `<Model>` or a track's
/// per-vertex data when the model is not given back, and a track whose
/// vertex count an edit changed when it is.
pub(crate) fn drop_warnings(fc: &FeatureCollection, target: Format, version: Option<KmlVersion>) -> Vec<String> {
    let carried = matches!(target, Format::Kml | Format::Kmz) && writer::carried_skeleton(fc, version).is_some();
    reconcile::drop_warnings(fc, target, carried)
}

/// The warnings a write of `fc` as `version` owes for changing its version
/// (`plans/kml-standard.org` D-K7). Lowering 2.3 to 2.2 reports what it
/// respells and what 2.2 has no place for, each counted over the document as
/// read; changing only the version stamp says nothing. A 2.2 source written
/// as 2.3 is built from the IR alone until the raising exists, and says what
/// that leaves behind.
pub(crate) fn version_change_warnings(fc: &FeatureCollection, version: KmlVersion) -> Vec<String> {
    let Some(source) = writer::source_document(fc) else {
        return Vec::new();
    };
    let Some(skeleton) = source.document.as_deref().filter(|_| source.version != version) else {
        return Vec::new();
    };
    if writer::lowers(source.version, Some(version)) {
        use model::{KmlElement, Spelling};
        let mut report = lower::Lowering::default();
        for child in &skeleton.doc.root.children {
            lower::survey(child, (KmlElement::Kml, Spelling::Kml), &mut report);
        }
        // A feature sits in a container; every container admits it alike.
        for m in fc.features.iter().filter_map(reconcile::feature_model) {
            lower::survey(&m.node, (KmlElement::Document, Spelling::Kml), &mut report);
        }
        return report.respelled_warning().into_iter().chain(report.dropped_warning()).collect();
    }
    vec![format!(
        "kml: the source is KML {} and the output KML {}; a version change is written from the feature model \
         alone, so it does not preserve the source's styles, folders, network links, views or screen overlays, \
         and writes its ground and photo overlays as Placemarks",
        source.version.as_str(),
        version.as_str()
    )]
}

/// `.kmz` bytes -> Feature IR: unwrap the zip container and flatten every
/// `*.kml` entry's `Placemark`s into one collection — not just the first.
/// Conventionally a single `doc.kml` at the archive root holds everything,
/// but a real multi-layer producer (GDAL/LIBKML's `ogr2ogr -f LIBKML`, e.g.)
/// instead writes a root `doc.kml` that's only a `<NetworkLink>` pointing at
/// the actual per-layer data in `layers/*.kml`; reading only the first entry
/// would silently produce zero features for archives shaped that way.
pub(crate) fn read_kmz(bytes: &[u8], _warnings: &mut Vec<String>) -> Result<FeatureCollection> {
    let entries = crate::zip::read(bytes)?;
    let kml_entries: Vec<_> = entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".kml")).collect();
    if kml_entries.is_empty() {
        return Err(Error::Convert("kmz: no .kml entry found in the archive".into()));
    }
    let mut features = Vec::new();
    // The document's provenance is the first entry's — conventionally the
    // root `doc.kml`, the one the archive is written back as. Its skeleton is
    // kept only when it is the archive's one document: a layered archive's
    // structure is the container's to carry (`plans/same-format-fidelity.org`
    // M3c), and a `doc.kml` written back alone would point at layers that are
    // no longer there.
    let mut first = None;
    let mut facts: Option<reader::Facts> = None;
    for entry in &kml_entries {
        let text = crate::xml::decode(&entry.data).map_err(|e| match e {
            Error::Convert(m) => Error::Convert(format!("kmz: entry \"{}\": {m}", entry.name)),
            other => other,
        })?;
        let document = reader::read_document(&text, Format::Kmz)?;
        features.extend(document.features);
        facts = Some(match facts {
            Some(f) => f.merge(document.facts),
            None => document.facts,
        });
        first.get_or_insert((document.native, document.skeleton));
    }
    let (native, skeleton) = first.expect("at least one .kml entry was read");
    let skeleton = (kml_entries.len() == 1).then_some(skeleton);
    let facts = facts.expect("at least one .kml entry was read");
    Ok(reader::finish(features, native, skeleton, facts, Format::Kmz))
}

/// Feature IR -> `.kmz` bytes: the `.kml` writer's output, wrapped as one
/// stored zip entry named `doc.kml` (the conventional name; see
/// `crate::zip`'s module doc for why compression isn't attempted).
pub(crate) fn write_kmz(fc: &FeatureCollection, version: Option<KmlVersion>) -> Vec<u8> {
    let kml_bytes = write_kml(fc, version);
    crate::zip::write(&[("doc.kml", &kml_bytes)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_layered_kmz_keeps_no_document_and_counts_its_losses_once() {
        let layer = br#"<kml xmlns="http://www.opengis.net/kml/2.2"><Placemark><Model/></Placemark></kml>"#;
        let kmz = crate::zip::write(&[("doc.kml", &layer[..]), ("layers/a.kml", &layer[..])]);
        let fc = read_kmz(&kmz, &mut Vec::new()).unwrap();
        assert_eq!(fc.features.len(), 2);
        // Written back, the archive is one document: the models go with the
        // layers, so even a KMZ write says so.
        let warnings = drop_warnings(&fc, Format::Kmz, None);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("kml: 2 feature(s) hold <Model>"), "{warnings:?}");
    }

    #[test]
    fn a_single_document_kmz_carries_its_models() {
        let doc = br#"<kml xmlns="http://www.opengis.net/kml/2.2"><Placemark><Model/></Placemark></kml>"#;
        let fc = read_kmz(&crate::zip::write(&[("doc.kml", &doc[..])]), &mut Vec::new()).unwrap();
        assert!(drop_warnings(&fc, Format::Kmz, None).is_empty());
        assert!(drop_warnings(&fc, Format::Kml, None).is_empty(), "a .kml write gives the same document back");
    }

    #[test]
    fn a_kmz_entry_that_cannot_be_decoded_is_named() {
        let kmz = crate::zip::write(&[("doc.kml", &b"<kml>\xFF</kml>"[..])]);
        let err = read_kmz(&kmz, &mut Vec::new()).unwrap_err().to_string();
        assert!(err.contains("\"doc.kml\""), "{err}");
    }
}
