//! The whole conversion, from source bytes to output bytes, in one ordered
//! pipeline the `geosetta` binary and a library caller both run
//! (`plans/library-pipeline.org`).
//!
//! [`prepare`] decodes the source and does everything that has to happen
//! before an encode — layer selection, the Hilbert sort, a CRS override, every
//! lossy-conversion check, and releasing what the target will not read — in
//! that order. The only way to encode is through the [`Prepared`] it returns,
//! so the checks have always run on exactly what will be written.
//!
//! What the pipeline has to say arrives through an [`Event`] callback, in the
//! order it happens: warnings as message bodies, and the stage boundaries a
//! progress display needs. The library formats no progress text and prints
//! nothing.
//!
//! File I/O stays with the caller, as everywhere in this crate: a [`Source`]
//! is bytes, and an [`Output`] is bytes.

use std::collections::BTreeSet;

use crate::convert::{self, ReadOptions, WriteOptions};
use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::FeatureCollection;
use crate::table::Layout;
use crate::format::Format;
use crate::{geopackage, shapefile};

/// A conversion's input, already read.
#[derive(Debug, Clone, Copy)]
pub enum Source<'a> {
    /// Every single-buffer format, and GeoPackage.
    Bytes(&'a [u8]),
    /// A Shapefile's sibling files. `.dbf` is required; `.prj` and `.cpg` are
    /// optional, as for [`shapefile::read`].
    Shapefile { shp: &'a [u8], dbf: &'a [u8], prj: Option<&'a str>, cpg: Option<&'a str> },
}

/// What a conversion does beyond the encode itself. `Default` is a plain
/// conversion.
#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// How each layer is held once read (`plans/columnar.org` D-C14).
    pub read: ReadOptions,
    /// How each single-buffer layer is written — `--kml-version`, `--compact`.
    pub write: WriteOptions,
    /// Relabel every layer with this CRS — `--crs`. Assign, not enrich: it
    /// replaces whatever the source carried, or its lack of one, `ogr2ogr
    /// -a_srs` style, and touches no coordinate.
    pub crs: Option<Crs>,
    /// Reorder features by Hilbert-curve locality — `--sort-hilbert`.
    pub sort_hilbert: bool,
    /// `--layer`: the one layer of a GeoPackage source to convert, and the name
    /// of a single-collection source. That name is the collection's own from
    /// then on, so an output that records one ([`Format::records_name`]) has it,
    /// and it is what [`Prepared::layers`] reports.
    pub layer: Option<String>,
    /// The name for that GeoPackage layer when neither [`Self::layer`] nor the
    /// collection's own name gives one. The binary passes the input's file
    /// stem; without it the layer is called `layer`.
    pub default_layer_name: Option<String>,
    /// Write a GeoPackage output with an R*Tree spatial index — `--rtree`.
    /// Refused for any other output ([`Self::check`]).
    pub rtree: bool,
    /// Cache each geometry's envelope in a GeoPackage output — `--envelope`.
    /// Refused for any other output ([`Self::check`]).
    pub envelope: bool,
    /// How the GeoPackage survey messages name the source container (a path,
    /// or `stdin`). "the source" without one.
    pub source_label: Option<String>,
    /// The same for the destination container. "the destination" without one.
    pub destination_label: Option<String>,
}

/// Something the pipeline reports as it runs, in the order it happens.
#[non_exhaustive]
#[derive(Debug)]
pub enum Event<'a> {
    /// What the conversion cannot keep, as a message body without the
    /// `warning: ` prefix the binary adds.
    Warning(String),
    /// The source is decoded: every layer it holds, before `layer` selects
    /// one. A single-collection source is one layer.
    Read { layers: &'a [(String, FeatureCollection)] },
    /// The layers this conversion will write, selected and named.
    Selected { layers: &'a [(String, FeatureCollection)] },
    /// The Hilbert sort ran, over this many features in all.
    Sorted { features: usize },
    /// Every check has run and the encode starts.
    Encoding,
}

impl ConvertOptions {
    /// Whether these options can be written as `target` at all: the one place
    /// that decides, so the binary's argument check and [`prepare`] refuse
    /// alike, in the same words. See [`WriteOptions::check`], which it extends.
    pub fn check(&self, target: Format) -> Result<()> {
        self.write.check(target)?;
        check_layer_name(self)?;
        for (on, flag) in [(self.rtree, "--rtree"), (self.envelope, "--envelope")] {
            if on && target != Format::Gpkg {
                return Err(Error::Usage(format!(
                    "{flag} is a GeoPackage option, and the output here is {}",
                    target.display_name()
                )));
            }
        }
        Ok(())
    }
}

/// A table or a file can hold no empty name, so it cannot be what was meant.
fn check_layer_name(options: &ConvertOptions) -> Result<()> {
    if options.layer.as_deref() == Some("") {
        return Err(Error::Usage("--layer needs a name, and this one is empty".into()));
    }
    Ok(())
}

/// A conversion that has run every step but the encode. See [`prepare`].
#[derive(Debug)]
pub struct Prepared {
    to: Format,
    options: ConvertOptions,
    layers: Vec<(String, FeatureCollection)>,
    /// Independent container content a GeoPackage source holds, for a
    /// GeoPackage output to put back. Empty otherwise.
    carried: geopackage::Carried,
}

/// One layer, encoded for a target that is not GeoPackage.
#[derive(Debug)]
pub enum Output {
    Bytes(Vec<u8>),
    /// The sibling files. Their [`shapefile::Encoded::warnings`] have already
    /// been reported as events.
    Shapefile(shapefile::Encoded),
}

/// Decode `source` as `from` and do everything a write as `to` needs first.
///
/// In order: [`read`], which decodes, reports what the read dropped, and
/// selects or names the layers; for a GeoPackage output from a GeoPackage,
/// read the source container and say what of it will not be carried; sort; install the CRS override; run
/// [`convert::conversion_warnings`] on every layer, saying each distinct
/// message once; and drop the natives the target will not read.
///
/// The override comes before the checks because it is as authoritative as a
/// CRS read from the file, and the natives go after them because the KML
/// checks inspect them (`plans/library-warnings.org` D5).
pub fn prepare(
    from: Format,
    to: Format,
    source: Source<'_>,
    options: ConvertOptions,
    events: &mut dyn FnMut(Event<'_>),
) -> Result<Prepared> {
    // Before anything is read: options that cannot apply to the target fail
    // the conversion outright, as the binary's argument check does.
    options.check(to)?;
    // The target is known here, so a read keeps only what its writer reads.
    let mut layers = read_as(from, source, &options, events, options.read.layout, Some(to))?;
    let mut carried = geopackage::Carried::default();
    if let (Format::Gpkg, Format::Gpkg, Source::Bytes(bytes)) = (from, to, source) {
        // Carried *before* the survey, because the survey's job is to name
        // what does not survive and this is what does. `layer` does not narrow
        // it: a pyramid is not a layer, so selecting one layer is not a
        // statement about the container around it.
        carried = geopackage::read_container(bytes)?;
        for w in geopackage::container_drop_warnings(
            bytes,
            options.source_label.as_deref().unwrap_or("the source"),
            &names_of(&layers),
            &carried.reproduced_names(),
            options.rtree,
        ) {
            events(Event::Warning(w));
        }
    } else if layers.is_empty() {
        return Err(Error::Usage("GeoPackage has no feature layers".into()));
    }
    events(Event::Selected { layers: &layers });

    if options.sort_hilbert {
        for (_, fc) in &mut layers {
            convert::reorder_hilbert(fc);
        }
        events(Event::Sorted { features: layers.iter().map(|(_, fc)| fc.len()).sum() });
    }
    if let Some(crs) = &options.crs {
        for w in apply_crs_override(&mut layers, crs) {
            events(Event::Warning(w));
        }
    }
    for w in layer_warnings(&layers, from, to, options.write) {
        events(Event::Warning(w));
    }
    for (_, fc) in &mut layers {
        release_foreign_natives(fc, to);
    }
    Ok(Prepared { to, options, layers, carried })
}

/// The first stage of [`prepare`] alone: decode `source` as `from` into the
/// layers a conversion would write, reporting what the read dropped.
///
/// A GeoPackage is each of its layers, narrowed to [`ConvertOptions::layer`]
/// when one is given. Anything else is one collection, named as a GeoPackage
/// output would name it — [`ConvertOptions::layer`], then the name the
/// *source* recorded, then [`ConvertOptions::default_layer_name`]. The middle
/// step is what makes an edit loop replace a GeoPackage layer instead of
/// appending a duplicate: with `-` for input there is no stem to draw on, so
/// before collections carried a name every piped write landed in a layer
/// called `layer` (`plans/edit-composability.org` D4).
///
/// For a caller that wants the collections without converting them — the
/// binary's `--print-crs*` diagnostics read through this.
pub fn read(
    from: Format,
    source: Source<'_>,
    options: &ConvertOptions,
    events: &mut dyn FnMut(Event<'_>),
) -> Result<Vec<(String, FeatureCollection)>> {
    check_layer_name(options)?;
    read_as(from, source, options, events, options.read.layout, options.read.target)
}

/// [`read`], building each collection in `layout` for a write as `target`.
fn read_as(
    from: Format,
    source: Source<'_>,
    options: &ConvertOptions,
    events: &mut dyn FnMut(Event<'_>),
    layout: Layout,
    target: Option<Format>,
) -> Result<Vec<(String, FeatureCollection)>> {
    let mut warnings = Vec::new();
    let mut fc = match (from, source) {
        (Format::Gpkg, Source::Bytes(bytes)) => {
            let mut layers = geopackage::read_layers_as(bytes, layout)?;
            events(Event::Read { layers: &layers });
            if let Some(name) = &options.layer {
                select_layer(&mut layers, name)?;
            }
            return Ok(layers);
        }
        (Format::Shapefile, Source::Shapefile { shp, dbf, prj, cpg }) => {
            shapefile::read_as(shp, dbf, prj, cpg, &mut warnings, layout)?
        }
        (Format::Shapefile, Source::Bytes(_)) => {
            return Err(Error::Usage("Shapefile input is read from its sibling files, not one buffer".into()));
        }
        (_, Source::Shapefile { .. }) => {
            return Err(Error::Usage(format!(
                "sibling files are a Shapefile source, and the source here is {}",
                from.display_name()
            )));
        }
        (_, Source::Bytes(bytes)) => convert::read_features_as(from, bytes, &mut warnings, layout, target)?,
    };
    for w in warnings {
        events(Event::Warning(w));
    }
    // Only for a name that was asked for: a default name is the file's stem,
    // which no output should start recording unasked.
    if let Some(layer) = &options.layer {
        fc.name = Some(layer.clone());
    }
    let name = options
        .layer
        .clone()
        .or_else(|| fc.name.clone())
        .or_else(|| options.default_layer_name.clone())
        .unwrap_or_else(|| "layer".into());
    let layers = vec![(name, fc)];
    events(Event::Read { layers: &layers });
    Ok(layers)
}

impl Prepared {
    /// The layers that will be written, in order, each with its name. One for
    /// a single-collection source.
    ///
    /// A name comes from the source, so it is untrusted: a caller building a path
    /// from one must first see that it is one plain file name. It may be
    /// `../../x`, or absolute (`plans/cli-review.org` C9).
    pub fn layers(&self) -> &[(String, FeatureCollection)] {
        &self.layers
    }

    /// The options this conversion was prepared with.
    pub fn options(&self) -> &ConvertOptions {
        &self.options
    }

    /// Encode layer `index` for a target that is not GeoPackage. A Shapefile's
    /// `.dbf` warnings are reported as events, after the encode that found
    /// them.
    pub fn encode(&self, index: usize, events: &mut dyn FnMut(Event<'_>)) -> Result<Output> {
        let (_, fc) = self
            .layers
            .get(index)
            .ok_or_else(|| Error::Usage(format!("no layer {index}: this conversion has {}", self.layers.len())))?;
        match self.to {
            Format::Gpkg => Err(Error::Usage(
                "GeoPackage output is one container for every layer: use into_geopackage".into(),
            )),
            Format::Shapefile => {
                events(Event::Encoding);
                let encoded = shapefile::write(fc)?;
                for w in &encoded.warnings {
                    events(Event::Warning(w.clone()));
                }
                Ok(Output::Shapefile(encoded))
            }
            to => {
                events(Event::Encoding);
                Ok(Output::Bytes(convert::write_features_with(to, fc, self.options.write)?))
            }
        }
    }

    /// Write every layer into a GeoPackage: upserted into `existing` when
    /// there is one, a new container otherwise.
    ///
    /// The destination's own container content is merged with what the source
    /// carries (`plans/container-reconciliation.org` § 3) *before* the
    /// destination is surveyed, because what the destination keeps is no
    /// longer destroyed, and a survey that still named it would report
    /// surviving content as lost. The metadata survey comes last, against
    /// `existing` too, because an append supplies layers this conversion does
    /// not, and a reference to one of those still survives.
    pub fn into_geopackage(self, existing: Option<&[u8]>, events: &mut dyn FnMut(Event<'_>)) -> Result<Vec<u8>> {
        if self.to != Format::Gpkg {
            return Err(Error::Usage(format!(
                "into_geopackage writes a GeoPackage, and the target here is {}: use encode",
                self.to.display_name()
            )));
        }
        let mut carried = self.carried;
        if let Some(bytes) = existing {
            carried = geopackage::reconcile_containers(geopackage::read_container(bytes)?, &carried)?;
            for w in geopackage::container_drop_warnings(
                bytes,
                self.options.destination_label.as_deref().unwrap_or("the destination"),
                &names_of(&self.layers),
                &carried.reproduced_names(),
                self.options.rtree,
            ) {
                events(Event::Warning(w));
            }
        }
        for w in geopackage::metadata_drop_warnings(existing, &self.layers, &carried) {
            events(Event::Warning(w));
        }
        events(Event::Encoding);
        geopackage::write_layers(existing, &self.layers, &carried, self.options.rtree, self.options.envelope)
    }
}

/// Narrow `layers` to the one called `name`, or say there is none.
///
/// An error rather than an empty conversion, so a mistyped `--layer` cannot
/// succeed by writing nothing.
pub fn select_layer(layers: &mut Vec<(String, FeatureCollection)>, name: &str) -> Result<()> {
    layers.retain(|(n, _)| n == name);
    if layers.is_empty() {
        return Err(Error::Usage(format!("no layer named \"{name}\"")));
    }
    Ok(())
}

/// Install a CRS override on every layer, plus the one warning only a
/// multi-layer source can earn.
///
/// Overriding is the caller's explicit instruction, so a single CRS being
/// replaced has nothing to report. But when the layers *disagreed*, one
/// identity now stands in for several — a real relabel the user may not have
/// pictured when they pointed `--crs` at a container rather than a file.
/// Announcing it rather than performing it quietly is the same convention
/// every other lossy path in this crate follows
/// (`plans/lossy-conversion-warnings.org`).
fn apply_crs_override(layers: &mut [(String, FeatureCollection)], crs: &Crs) -> Vec<String> {
    let mut distinct: Vec<Option<Crs>> = Vec::new();
    for (_, fc) in layers.iter() {
        if !distinct.contains(&fc.crs) {
            distinct.push(fc.crs.clone());
        }
    }
    for (_, fc) in layers.iter_mut() {
        fc.crs = Some(crs.clone());
    }
    if distinct.len() < 2 {
        return Vec::new();
    }
    vec![format!(
        "--crs relabels all {} layers as {}, but the source declared {} different CRSes; \
         the override replaces every one of them.",
        layers.len(),
        crs.authority_code().unwrap_or_else(|| "the supplied definition".into()),
        distinct.len(),
    )]
}

/// [`convert::conversion_warnings`] across every layer, de-duplicating
/// identical messages so a single-CRS `.gpkg` (the usual case) warns just once
/// even when it fans out to many output files.
fn layer_warnings(
    layers: &[(String, FeatureCollection)],
    source: Format,
    target: Format,
    options: WriteOptions,
) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (_, fc) in layers {
        for w in convert::conversion_warnings(fc, source, target, options) {
            if seen.insert(w.clone()) {
                out.push(w);
            }
        }
    }
    out
}

/// Drop every native the target's writer will not read, once the warnings
/// that inspect them have run.
///
/// Only a memory saving: a writer replays a payload only when its tag matches
/// (`Native::payload_for`), so this changes no output. It matters for KML,
/// whose natives are each feature's whole element — on a large `kml -> fgb`
/// run they would otherwise be held through the write for nothing. KML and KMZ
/// read each other's natives, since the two hold one document.
fn release_foreign_natives(fc: &mut FeatureCollection, target: Format) {
    let kept = |format: Format| {
        format == target || matches!((format, target), (Format::Kml, Format::Kmz) | (Format::Kmz, Format::Kml))
    };
    if fc.native.as_ref().is_some_and(|n| !kept(n.format)) {
        fc.native = None;
    }
    fc.drop_natives(|n| !kept(n.format));
}

/// The layer names of a `(name, collection)` list, which is the shape both
/// GeoPackage surveys need.
fn names_of(layers: &[(String, FeatureCollection)]) -> Vec<String> {
    layers.iter().map(|(n, _)| n.clone()).collect()
}
