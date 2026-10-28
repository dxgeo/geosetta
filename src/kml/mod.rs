//! KML/KMZ format spoke — see `plans/kml.org` for the design writeup.
//!
//! `.kml` (plain XML) read/write routes through [`crate::xml`]; `.kmz` wraps
//! the same `.kml` bytes in a [`crate::zip`] container.

use crate::error::{Error, Result};
use crate::feature::FeatureCollection;
use crate::format::{Format, KmlVersion};
use crate::native::{KmzArchive, KmzEntry};
use std::rc::Rc;

pub(crate) mod model;
pub(crate) mod document;
mod lower;
mod project;
mod reader;
mod place;
mod raise;
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

/// What writing `fc` as `target` cannot carry of its KML features' elements:
/// a `<Model>` or a track's per-vertex data when the model is not given back,
/// and a track whose vertex count an edit changed when it is. Either KML
/// version gives the model back. And of a `.kmz` read, the archive's other
/// files, which only a `.kmz` write has anywhere to put.
pub(crate) fn drop_warnings(fc: &FeatureCollection, target: Format) -> Vec<String> {
    let carried = matches!(target, Format::Kml | Format::Kmz) && writer::carried_skeleton(fc).is_some();
    let mut out = reconcile::drop_warnings(fc, target, carried);
    if target != Format::Kmz {
        out.extend(archive_warning(fc, target));
    }
    out
}

/// The warning a write of a `.kmz` read as `target`, not a `.kmz`, owes for
/// the archive's files: an overlay's image, an icon, a model, written nowhere.
/// A directory entry holds nothing, and is not counted.
fn archive_warning(fc: &FeatureCollection, target: Format) -> Option<String> {
    let archive = writer::source_document(fc)?.archive.as_deref()?;
    let lost: Vec<&str> = archive
        .entries
        .iter()
        .enumerate()
        .filter(|&(i, e)| i != archive.document && !e.name.ends_with('/'))
        .map(|(_, e)| e.name.as_str())
        .collect();
    if lost.is_empty() {
        return None;
    }
    const SHOWN: usize = 3;
    let mut names = lost.iter().take(SHOWN).copied().collect::<Vec<_>>().join(", ");
    if lost.len() > SHOWN {
        names.push_str(&format!(", and {} more", lost.len() - SHOWN));
    }
    Some(format!(
        "kmz: {} archive entr{} beside the KML document ({names}) {} not written; a {} write holds no files",
        lost.len(),
        if lost.len() == 1 { "y" } else { "ies" },
        if lost.len() == 1 { "is" } else { "are" },
        target.display_name()
    ))
}

/// The warnings a write of `fc` as `version` owes for changing its version
/// (`plans/kml-standard.org` D-K7): what the change respells or renames and,
/// lowering to 2.2, what 2.2 has no place for, each counted over the document
/// as read. Changing only the version stamp says nothing.
pub(crate) fn version_change_warnings(fc: &FeatureCollection, version: KmlVersion) -> Vec<String> {
    let Some(source) = writer::source_document(fc) else {
        return Vec::new();
    };
    let Some(skeleton) = source.document.as_deref().filter(|_| source.version != version) else {
        return Vec::new();
    };
    use model::{KmlElement, Spelling};
    let mut report = lower::Changes::default();
    // A feature sits in a container; every container admits it alike.
    let features = || fc.features.iter().filter_map(reconcile::feature_model).map(|m| &m.node);
    match version {
        KmlVersion::V2_2 => {
            for node in skeleton.doc.root.children.iter() {
                lower::survey(node, (KmlElement::Kml, Spelling::Kml), &mut report);
            }
            for node in features() {
                lower::survey(node, (KmlElement::Document, Spelling::Kml), &mut report);
            }
        }
        KmlVersion::V2_3 => {
            for node in skeleton.doc.root.children.iter() {
                raise::survey(node, KmlElement::Kml, &mut report);
            }
            for node in features() {
                raise::survey(node, KmlElement::Document, &mut report);
            }
        }
    }
    report.respelled_warning(source.version, version).into_iter().chain(report.dropped_warning()).collect()
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
    let kml_entries: Vec<_> = entries.iter().filter(|e| is_kml_entry(&e.name)).collect();
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
    let (mut native, skeleton) = first.expect("at least one .kml entry was read");
    let skeleton = (kml_entries.len() == 1).then_some(skeleton);
    let facts = facts.expect("at least one .kml entry was read");
    drop(kml_entries);
    native.archive = Some(Rc::new(archive_of(entries)));
    Ok(reader::finish(features, native, skeleton, facts, Format::Kmz))
}

fn is_kml_entry(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".kml")
}

/// The archive as a `.kmz` write gives it back (M3c): every entry in order,
/// the first `.kml` one standing for the document. The archive's other `.kml`
/// documents are left out: their features are in the IR, and kept verbatim
/// they would be written twice.
fn archive_of(entries: Vec<crate::zip::ZipEntry>) -> KmzArchive {
    let mut out = Vec::with_capacity(entries.len());
    let mut document = None;
    for e in entries {
        let data = if is_kml_entry(&e.name) {
            if document.is_some() {
                continue;
            }
            document = Some(out.len());
            Vec::new()
        } else {
            e.data
        };
        out.push(KmzEntry { name: e.name, data, modified: e.modified });
    }
    KmzArchive { entries: out, document: document.expect("the archive has a .kml entry") }
}

/// Feature IR -> `.kmz` bytes: the `.kml` writer's output, in the archive a
/// `.kmz` read came from — every entry back in its place, the document under
/// its own name — or else as one entry named `doc.kml`, the conventional
/// name. Entries are stored, uncompressed (see `crate::zip`'s module doc).
pub(crate) fn write_kmz(fc: &FeatureCollection, version: Option<KmlVersion>) -> Vec<u8> {
    let kml_bytes = write_kml(fc, version);
    let Some(archive) = writer::source_document(fc).and_then(|s| s.archive.as_deref()) else {
        return crate::zip::write(&[("doc.kml", &kml_bytes)]);
    };
    let entries: Vec<(&str, &[u8], crate::zip::DosTime)> = archive
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.name.as_str(), if i == archive.document { &kml_bytes[..] } else { &e.data[..] }, e.modified))
        .collect();
    crate::zip::write_timed(&entries)
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
        let warnings = drop_warnings(&fc, Format::Kmz);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].starts_with("kml: 2 feature(s) hold <Model>"), "{warnings:?}");
    }

    #[test]
    fn a_single_document_kmz_carries_its_models() {
        let doc = br#"<kml xmlns="http://www.opengis.net/kml/2.2"><Placemark><Model/></Placemark></kml>"#;
        let fc = read_kmz(&crate::zip::write(&[("doc.kml", &doc[..])]), &mut Vec::new()).unwrap();
        assert!(drop_warnings(&fc, Format::Kmz).is_empty());
        assert!(drop_warnings(&fc, Format::Kml).is_empty(), "a .kml write gives the same document back");
    }

    const DOC: &[u8] = br#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Placemark><name>p</name></Placemark></Document></kml>"#;

    /// An archive shaped the way real ones are: a directory entry, files
    /// before and after the document, the document not named `doc.kml`, and
    /// times recorded.
    fn archive() -> Vec<u8> {
        crate::zip::write_timed(&[
            ("files/", b"", 0x5B5C_0000),
            ("files/overlay.png", b"\x89PNG\r\n", 0x5B5C_6421),
            ("places.kml", DOC, 0x5B5C_6422),
            ("files/notes.txt", b"notes", 0x5B5C_6423),
        ])
    }

    #[test]
    fn a_kmz_written_back_keeps_every_entry_in_its_place() {
        let fc = read_kmz(&archive(), &mut Vec::new()).unwrap();
        let back = crate::zip::read(&write_kmz(&fc, None)).unwrap();
        let shape: Vec<_> = back.iter().map(|e| (e.name.as_str(), e.modified)).collect();
        assert_eq!(
            shape,
            [
                ("files/", 0x5B5C_0000),
                ("files/overlay.png", 0x5B5C_6421),
                ("places.kml", 0x5B5C_6422),
                ("files/notes.txt", 0x5B5C_6423)
            ]
        );
        assert_eq!(back[1].data, b"\x89PNG\r\n");
        assert_eq!(back[3].data, b"notes");
        // The document is written from the model, edits and all.
        let mut edited = fc.clone();
        edited.features[0].properties[0].1 = crate::json::JsonValue::String("renamed".into());
        let back = crate::zip::read(&write_kmz(&edited, Some(KmlVersion::V2_3))).unwrap();
        let text = String::from_utf8(back[2].data.clone()).unwrap();
        assert!(text.contains("<name>renamed</name>") && text.contains(r#"version="2.3""#), "{text}");
        assert!(drop_warnings(&fc, Format::Kmz).is_empty());
    }

    #[test]
    fn a_kmz_written_as_anything_else_names_the_files_it_leaves() {
        let fc = read_kmz(&archive(), &mut Vec::new()).unwrap();
        for target in [Format::Kml, Format::GeoJson] {
            assert_eq!(
                drop_warnings(&fc, target),
                [format!(
                    "kmz: 2 archive entries beside the KML document (files/overlay.png, files/notes.txt) are not \
                     written; a {} write holds no files",
                    target.display_name()
                )]
            );
        }
        // A .kml read has no archive, and a .kml written as .kmz is one entry.
        let fc = crate::kml::read_kml(DOC, &mut Vec::new()).unwrap();
        assert!(drop_warnings(&fc, Format::Kml).is_empty());
        let names: Vec<_> = crate::zip::read(&write_kmz(&fc, None)).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["doc.kml"]);
    }

    #[test]
    fn a_layered_kmzs_other_documents_are_not_carried_twice() {
        let kmz = crate::zip::write(&[("doc.kml", DOC), ("layers/a.kml", DOC), ("icon.png", b"i")]);
        let fc = read_kmz(&kmz, &mut Vec::new()).unwrap();
        assert_eq!(fc.features.len(), 2);
        let back = crate::zip::read(&write_kmz(&fc, None)).unwrap();
        let names: Vec<_> = back.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["doc.kml", "icon.png"], "the layer's features are in doc.kml now");
    }

    #[test]
    fn a_kmz_entry_that_cannot_be_decoded_is_named() {
        let kmz = crate::zip::write(&[("doc.kml", &b"<kml>\xFF</kml>"[..])]);
        let err = read_kmz(&kmz, &mut Vec::new()).unwrap_err().to_string();
        assert!(err.contains("\"doc.kml\""), "{err}");
    }
}
