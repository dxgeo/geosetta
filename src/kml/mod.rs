//! KML/KMZ format spoke — see `plans/kml.org` for the design writeup.
//!
//! `.kml` (plain XML) read/write routes through [`crate::xml`]; `.kmz` wraps
//! the same `.kml` bytes in a [`crate::zip`] container.

use crate::error::{Error, Result};
use crate::feature::FeatureCollection;

mod reader;
mod writer;

pub(crate) use reader::read as read_kml;
pub(crate) use writer::write as write_kml;

/// `.kmz` bytes -> Feature IR: unwrap the zip container and flatten every
/// `*.kml` entry's `Placemark`s into one collection — not just the first.
/// Conventionally a single `doc.kml` at the archive root holds everything,
/// but a real multi-layer producer (GDAL/LIBKML's `ogr2ogr -f LIBKML`, e.g.)
/// instead writes a root `doc.kml` that's only a `<NetworkLink>` pointing at
/// the actual per-layer data in `layers/*.kml`; reading only the first entry
/// would silently produce zero features for archives shaped that way.
pub(crate) fn read_kmz(bytes: &[u8]) -> Result<FeatureCollection> {
    let entries = crate::zip::read(bytes)?;
    let mut kml_entries = entries.iter().filter(|e| e.name.to_ascii_lowercase().ends_with(".kml")).peekable();
    if kml_entries.peek().is_none() {
        return Err(Error::Convert("kmz: no .kml entry found in the archive".into()));
    }
    let mut features = Vec::new();
    for entry in kml_entries {
        let text = std::str::from_utf8(&entry.data)
            .map_err(|_| Error::Convert(format!("kmz: entry \"{}\" is not valid utf-8", entry.name)))?;
        reader::collect_features(text, &mut features)?;
    }
    Ok(reader::finish(features))
}

/// Feature IR -> `.kmz` bytes: the `.kml` writer's output, wrapped as one
/// stored zip entry named `doc.kml` (the conventional name; see
/// `crate::zip`'s module doc for why compression isn't attempted).
pub(crate) fn write_kmz(fc: &FeatureCollection) -> Vec<u8> {
    let kml_bytes = write_kml(fc);
    crate::zip::write(&[("doc.kml", &kml_bytes)])
}
