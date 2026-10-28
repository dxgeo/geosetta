//! KML/KMZ format spoke — see `plans/kml.org` for the design writeup, and
//! `plans/kml-standard.org`, which rescopes it to the OGC standard.
//!
//! `.kml` (plain XML) read/write routes through [`crate::xml`] and the KML
//! document model. A `.kmz` is a [`crate::zip`] archive of one or more `.kml`
//! documents and the files they reference; one written back keeps its
//! archive — every entry, each document from its own model
//! (`plans/same-format-fidelity.org` M3c).

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
    let carried = writer::carries_documents(fc, target);
    let mut out = reconcile::drop_warnings(fc, target, carried);
    if target == Format::Kml {
        out.extend(layers_warning(fc));
    }
    if target != Format::Kmz {
        out.extend(archive_warning(fc, target));
    }
    out
}

/// The warning a layered `.kmz` read written as one `.kml` owes: a `.kml`
/// holds one document, so the archive's are built into one from the IR.
fn layers_warning(fc: &FeatureCollection) -> Option<String> {
    let source = writer::source_document(fc)?;
    let archive = source.archive.as_deref()?;
    if source.layers.is_empty() {
        return None;
    }
    let names: Vec<&str> = std::iter::once(archive.document)
        .chain(source.layers.iter().map(|l| l.entry))
        .map(|i| archive.entries[i].name.as_str())
        .collect();
    Some(format!(
        "kmz: the archive holds {} KML documents ({}), and a KML write holds one; written as one document built \
         from the feature model alone, without their styles, folders or network links, and with ground and photo \
         overlays as Placemarks",
        names.len(),
        names.join(", ")
    ))
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
        .filter(|(_, e)| !is_kml_entry(&e.name) && !e.name.ends_with('/'))
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
pub(crate) fn version_change_warnings(fc: &FeatureCollection, target: Format, version: KmlVersion) -> Vec<String> {
    let Some(source) = writer::source_document(fc).filter(|_| writer::carries_documents(fc, target)) else {
        return Vec::new();
    };
    let Some(first) = source.document.as_deref() else {
        return Vec::new();
    };
    use model::{KmlElement, Spelling};
    let documents = std::iter::once((first, source.version))
        .chain(source.layers.iter().map(|l| (&*l.skeleton, l.version)))
        .enumerate()
        .filter(|(_, (_, own))| *own != version);
    let mut report = lower::Changes::default();
    let mut from = None;
    for (index, (skeleton, own)) in documents {
        from = Some(own);
        // A feature sits in a container; every container admits it alike.
        let features = fc.features.iter().filter_map(reconcile::stored).filter(|(_, d)| *d == index).map(|(m, _)| &m.node);
        let nodes = skeleton.doc.root.children.iter().map(|n| (n, KmlElement::Kml)).chain(features.map(|n| (n, KmlElement::Document)));
        for (node, parent) in nodes {
            match version {
                KmlVersion::V2_2 => lower::survey(node, (parent, Spelling::Kml), &mut report),
                KmlVersion::V2_3 => raise::survey(node, parent, &mut report),
            }
        }
    }
    let Some(from) = from else {
        return Vec::new();
    };
    report.respelled_warning(from, version).into_iter().chain(report.dropped_warning()).collect()
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
    let kml_entries: Vec<_> = entries.iter().enumerate().filter(|(_, e)| is_kml_entry(&e.name)).collect();
    if kml_entries.is_empty() {
        return Err(Error::Convert("kmz: no .kml entry found in the archive".into()));
    }
    let mut features = Vec::new();
    // The collection's provenance is the first document's — conventionally
    // the root `doc.kml`, which a layered archive (GDAL's) fills with network
    // links to the rest. Every other document is a layer, with its own
    // skeleton, so a `.kmz` write gives each back (M3c); each feature records
    // which it came from.
    let mut first = None;
    let mut layers = Vec::new();
    let mut facts: Option<reader::Facts> = None;
    for (index, (entry_at, entry)) in kml_entries.iter().enumerate() {
        let text = crate::xml::decode(&entry.data).map_err(|e| match e {
            Error::Convert(m) => Error::Convert(format!("kmz: entry \"{}\": {m}", entry.name)),
            other => other,
        })?;
        let document = reader::read_document(&text, Format::Kmz, index)?;
        features.extend(document.features);
        facts = Some(match facts {
            Some(f) => f.merge(document.facts),
            None => document.facts,
        });
        if first.is_none() {
            first = Some((document.native, document.skeleton));
        } else {
            layers.push(document::Layer {
                entry: *entry_at,
                version: document.native.version,
                skeleton: Rc::new(document.skeleton),
            });
        }
    }
    let (mut native, skeleton) = first.expect("at least one .kml entry was read");
    let facts = facts.expect("at least one .kml entry was read");
    drop(kml_entries);
    native.layers = layers;
    native.archive = Some(Rc::new(archive_of(entries)));
    Ok(reader::finish(features, native, Some(skeleton), facts, Format::Kmz))
}

fn is_kml_entry(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".kml")
}

/// The archive as a `.kmz` write gives it back (M3c): every entry in order,
/// each `.kml` one with no bytes of its own, since its document is written
/// from the model — the first is the collection's document, the rest its
/// layers.
fn archive_of(entries: Vec<crate::zip::ZipEntry>) -> KmzArchive {
    let document = entries.iter().position(|e| is_kml_entry(&e.name)).expect("the archive has a .kml entry");
    let entries = entries
        .into_iter()
        .map(|e| {
            let data = if is_kml_entry(&e.name) { Vec::new() } else { e.data };
            KmzEntry { name: e.name, data, modified: e.modified }
        })
        .collect();
    KmzArchive { entries, document }
}

/// Feature IR -> `.kmz` bytes: the archive a `.kmz` read came from, every
/// entry back in its place — each `.kml` one written from its own document
/// with its own features, a layered archive's as much as a single one's — or
/// else the `.kml` writer's output as one entry named `doc.kml`, the
/// conventional name. Entries are stored, uncompressed (see `crate::zip`'s
/// module doc).
pub(crate) fn write_kmz(fc: &FeatureCollection, version: Option<KmlVersion>) -> Vec<u8> {
    let source = writer::source_document(fc);
    let Some(archive) = source.and_then(|s| s.archive.as_deref()).filter(|_| writer::carries_documents(fc, Format::Kmz))
    else {
        return crate::zip::write(&[("doc.kml", &write_kml(fc, version))]);
    };
    let layers = &source.expect("an archive has a source").layers;
    let written: Vec<Option<Vec<u8>>> = (0..archive.entries.len())
        .map(|i| {
            let index = match i {
                _ if i == archive.document => 0,
                _ => 1 + layers.iter().position(|l| l.entry == i)?,
            };
            Some(writer::write_document(fc, index, version))
        })
        .collect();
    let entries: Vec<(&str, &[u8], crate::zip::DosTime)> = archive
        .entries
        .iter()
        .zip(&written)
        .map(|(e, w)| (e.name.as_str(), w.as_deref().unwrap_or(&e.data), e.modified))
        .collect();
    crate::zip::write_timed(&entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feature::Feature;

    #[test]
    fn a_layered_kmz_carries_its_models_and_a_kml_write_says_what_it_cannot() {
        let layer = br#"<kml xmlns="http://www.opengis.net/kml/2.2"><Placemark><Model/></Placemark></kml>"#;
        let kmz = crate::zip::write(&[("doc.kml", &layer[..]), ("layers/a.kml", &layer[..])]);
        let fc = read_kmz(&kmz, &mut Vec::new()).unwrap();
        assert_eq!(fc.features.len(), 2);
        // A .kmz holds both documents, models and all.
        assert!(drop_warnings(&fc, Format::Kmz).is_empty());
        // A .kml holds one, so it is built from the IR, and both say so.
        let warnings = drop_warnings(&fc, Format::Kml);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("kml: 2 feature(s) hold <Model>"), "{warnings:?}");
        assert!(warnings[1].starts_with("kmz: the archive holds 2 KML documents (doc.kml, layers/a.kml)"), "{warnings:?}");
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

    fn layer_doc(name: &str) -> Vec<u8> {
        format!(
            "<kml xmlns=\"http://www.opengis.net/kml/2.2\"><Document><Style id=\"s\"/><Folder><name>{name}</name>\
             <Placemark><name>{name}</name><styleUrl>#s</styleUrl></Placemark></Folder></Document></kml>"
        )
        .into_bytes()
    }

    #[test]
    fn a_layered_kmz_gives_each_document_back_with_its_own_features() {
        let (a, b) = (layer_doc("a"), layer_doc("b"));
        let kmz = crate::zip::write(&[("doc.kml", DOC), ("layers/a.kml", &a), ("icon.png", b"i"), ("layers/b.kml", &b)]);
        let fc = read_kmz(&kmz, &mut Vec::new()).unwrap();
        assert_eq!(fc.features.len(), 3);
        let back = crate::zip::read(&write_kmz(&fc, None)).unwrap();
        let names: Vec<_> = back.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["doc.kml", "layers/a.kml", "icon.png", "layers/b.kml"]);
        // Unedited, each document is the one read.
        for (i, source) in [(0, DOC), (1, &a[..]), (3, &b[..])] {
            let written = crate::xml::parse_document(std::str::from_utf8(&back[i].data).unwrap()).unwrap();
            assert_eq!(written, crate::xml::parse_document(std::str::from_utf8(source).unwrap()).unwrap(), "{}", back[i].name);
        }
        // An edit lands in its own document; a removed feature leaves only
        // its own; a feature added since joins the first.
        let mut edited = fc.clone();
        edited.features[1].properties[0].1 = crate::json::JsonValue::String("a, renamed".into());
        let added = edited.features.remove(2);
        edited.features.push(Feature { native: None, ..added });
        let back = crate::zip::read(&write_kmz(&edited, None)).unwrap();
        let text = |i: usize| String::from_utf8(back[i].data.clone()).unwrap();
        assert!(text(1).contains("<Folder><name>a</name><Placemark><name>a, renamed</name><styleUrl>#s</styleUrl>"), "{}", text(1));
        assert!(!text(3).contains("<Placemark>"), "{}", text(3));
        assert!(text(0).contains("<name>b</name>"), "{}", text(0));
    }

    #[test]
    fn gdals_layered_kmz_round_trips_as_a_kmz() {
        let source = include_bytes!("../../tests/fixtures/gdal_networklink.kmz");
        let fc = read_kmz(source, &mut Vec::new()).unwrap();
        let again = read_kmz(&write_kmz(&fc, None), &mut Vec::new()).unwrap();
        assert_eq!(again.native, fc.native, "the archive, its documents and their layers");
        assert_eq!(again.features, fc.features);
        // Each document written is the one read, network links and all.
        let (before, after) = (crate::zip::read(source).unwrap(), crate::zip::read(&write_kmz(&fc, None)).unwrap());
        for (b, a) in before.iter().zip(&after).filter(|(b, _)| b.name.ends_with(".kml")) {
            let parse = |bytes: &[u8]| crate::xml::parse_document(&crate::xml::decode(bytes).unwrap()).unwrap();
            assert_eq!(parse(&a.data), parse(&b.data), "{}", b.name);
        }
        // And each document changes version on its own.
        let raised = read_kmz(&write_kmz(&fc, Some(KmlVersion::V2_3)), &mut Vec::new()).unwrap();
        let source = writer::source_document(&raised).unwrap();
        assert_eq!((source.version, source.layers[0].version), (KmlVersion::V2_3, KmlVersion::V2_3));
        assert_eq!(raised.features, fc.features);
    }

    #[test]
    fn a_kmz_entry_that_cannot_be_decoded_is_named() {
        let kmz = crate::zip::write(&[("doc.kml", &b"<kml>\xFF</kml>"[..])]);
        let err = read_kmz(&kmz, &mut Vec::new()).unwrap_err().to_string();
        assert!(err.contains("\"doc.kml\""), "{err}");
    }
}
