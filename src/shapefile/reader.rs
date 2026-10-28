//! Assemble a Shapefile's already-loaded sibling file bytes into a
//! [`FeatureCollection`]. Locating the sibling files on disk (by swapping the
//! `.shp` path's extension) is main.rs's job, mirroring how GeoPackage's
//! `read_layers` takes raw bytes and leaves file I/O to the caller.

use crate::crs::Crs;
use crate::error::{Error, Result};
use crate::feature::{Feature, FeatureCollection};
use crate::shapefile::{dbf, geometry};

/// Read a Shapefile from its sibling files' bytes. `.dbf` is required (a
/// shapefile with no attributes still ships a zero-field `.dbf`); `.prj`
/// (CRS) and `.cpg` (attribute text encoding) are optional.
///
/// `warnings` receives what the read could not decode exactly, as message
/// bodies — the same sink [`crate::convert::read_features`] takes.
pub fn read(
    shp: &[u8],
    dbf_bytes: &[u8],
    prj: Option<&str>,
    cpg: Option<&str>,
    warnings: &mut Vec<String>,
) -> Result<FeatureCollection> {
    let geometries = geometry::read(shp)?;
    let encoding = dbf::encoding_from_cpg(cpg);
    let mut losses = dbf::TextLosses::default();
    let records = dbf::read(dbf_bytes, encoding, cpg.is_some(), &mut losses)?;
    report_text_losses(&losses, cpg, warnings);

    if geometries.len() != records.len() {
        return Err(Error::Convert(format!(
            "shapefile: .shp has {} record(s) but .dbf has {} — the sibling files disagree",
            geometries.len(),
            records.len()
        )));
    }

    // .dbf and .shp are zipped by row position; a deleted .dbf record drops
    // its paired .shp record too (.shp has no deletion flag of its own).
    let mut features = Vec::with_capacity(geometries.len());
    for (geometry, record) in geometries.into_iter().zip(records) {
        if record.deleted {
            continue;
        }
        features.push(Feature { geometry, properties: record.properties, native: None });
    }

    let mut fc = FeatureCollection::new(features);
    // The `.dbf` header's own declarations, kept alongside the values so a
    // same-format write puts back the field widths it was handed rather than
    // re-deriving them from the rows (see `crate::schema::FieldDecl`).
    fc.schema = dbf::read_schema(dbf_bytes)?;
    // The definition is the `.prj`'s text; the newline a writer may have framed
    // it with is not part of it. Trimming here is the same rule
    // `crate::json::raw_at` applies to a PROJJSON definition nested in `geo`
    // metadata, so both dialects reach `NamedCrs` as the source's own bytes and
    // nothing else — which is what lets `--print-crs` add exactly one trailing
    // newline. A `.prj` holding only whitespace records no definition at all.
    if let Some(wkt) = prj.map(str::trim).filter(|w| !w.is_empty()) {
        fc.crs = Some(Crs::from_authority_code(None, None, Some(wkt.to_string()), None));
    }
    Ok(fc)
}

/// Say what decoding the `.dbf`'s text approximated or replaced, once per
/// cause. Silent when every value decoded exactly — an unrecognised `.cpg` over
/// all-ASCII text loses nothing, and saying so on every such file would be
/// noise (`plans/lossy-conversion-warnings.org` § THE RULE).
fn report_text_losses(losses: &dbf::TextLosses, cpg: Option<&str>, warnings: &mut Vec<String>) {
    let cpg = cpg.map(str::trim).unwrap_or_default();
    if losses.approximated > 0 {
        warnings.push(format!(
            "shapefile: .cpg names codepage \"{cpg}\", which this build has no table for; {} text value(s) with \
             non-ASCII bytes were decoded as ISO-8859-1 and may be wrong (Windows-1252, for one, differs in bytes \
             0x80-0x9F)",
            losses.approximated
        ));
    }
    if losses.replaced > 0 {
        warnings.push(format!(
            "shapefile: .cpg declares UTF-8, but {} text value(s) are not valid UTF-8; their invalid bytes were \
             replaced with U+FFFD",
            losses.replaced
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Geometry, Position};
    use crate::schema::infer_columns;
    use crate::shapefile::writer;

    fn one_point_fc() -> FeatureCollection {
        FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![("name".into(), crate::json::JsonValue::String("a".into()))],
            native: None,
        }])
    }

    /// A minimal valid `.shp`/`.dbf` pair, for tests that only care about `.prj`.
    fn shp_and_dbf() -> (Vec<u8>, Vec<u8>) {
        let encoded = writer::write(&one_point_fc()).unwrap();
        (encoded.shp, encoded.dbf)
    }

    #[test]
    fn a_prj_reaches_the_ir_as_its_own_bytes_without_its_framing() {
        // A `.prj` written with a trailing newline records the same definition
        // as one written without; the newline is the file's framing, not the
        // CRS's text. Storing it would make `--print-crs` emit two.
        let (shp, dbf) = shp_and_dbf();
        let wkt = "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",\
                   SPHEROID[\"WGS_1984\",6378137.0,298.257223563]]]";
        for framed in [wkt.to_string(), format!("{wkt}\n"), format!("\n{wkt}\r\n")] {
            let back = read(&shp, &dbf, Some(&framed), None, &mut Vec::new()).unwrap();
            match back.crs {
                Some(Crs::Named(n)) => assert_eq!(n.wkt.as_deref(), Some(wkt), "for {framed:?}"),
                other => panic!("expected Named, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_blank_prj_records_no_crs_rather_than_an_empty_definition() {
        // Otherwise the IR carries a definition that is the empty string, and
        // "nothing to report" becomes indistinguishable from "reported nothing".
        let (shp, dbf) = shp_and_dbf();
        for blank in ["", "   ", "\n\t\n"] {
            assert!(
                read(&shp, &dbf, Some(blank), None, &mut Vec::new()).unwrap().crs.is_none(),
                "for {blank:?}"
            );
        }
    }

    #[test]
    fn reads_back_a_written_shapefile() {
        let fc = one_point_fc();
        let encoded = writer::write(&fc).unwrap();
        let back = read(&encoded.shp, &encoded.dbf, encoded.prj.as_deref(), None, &mut Vec::new()).unwrap();
        assert_eq!(back.features.len(), 1);
        assert_eq!(back.features[0].geometry, Some(Geometry::Point(Position::new(1.0, 2.0))));
    }

    #[test]
    fn mismatched_record_counts_error() {
        let fc = one_point_fc();
        let columns = infer_columns(&fc.features);
        // A .dbf with zero records paired against a one-record .shp.
        let (empty_dbf, _warnings) = dbf::write(&columns[..0]).unwrap();
        let (shp, _shx) = crate::shapefile::geometry::write(&[Some(Geometry::Point(Position::new(0.0, 0.0)))]).unwrap();
        assert!(read(&shp, &empty_dbf, None, None, &mut Vec::new()).is_err());
    }

    #[test]
    fn prj_recovers_a_named_crs() {
        let mut fc = one_point_fc();
        fc.crs = Some(Crs::Named(crate::crs::NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            wkt: Some("PROJCS[\"WGS_1984_Web_Mercator\",AUTHORITY[\"EPSG\",\"3857\"]]".into()),
            projjson: None,
        }));
        let encoded = writer::write(&fc).unwrap();
        let back = read(&encoded.shp, &encoded.dbf, encoded.prj.as_deref(), None, &mut Vec::new()).unwrap();
        match back.crs {
            Some(Crs::Named(n)) => assert_eq!(n.code.as_deref(), Some("3857")),
            other => panic!("expected Named EPSG:3857, got {other:?}"),
        }
    }

    /// A one-feature Shapefile whose `name` field holds `text`'s bytes,
    /// re-encoded: the writer emits UTF-8, so `é` is swapped for `raw` in place
    /// (the field is space-padded, so a shorter byte run keeps its width).
    fn shapefile_with_name_bytes(raw: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: vec![("name".into(), crate::json::JsonValue::String("café".into()))],
            native: None,
        }]);
        let encoded = writer::write(&fc).unwrap();
        let mut dbf = encoded.dbf;
        let at = dbf.windows(2).position(|w| w == "é".as_bytes()).unwrap();
        let mut replacement = raw.to_vec();
        replacement.resize(2, b' ');
        dbf[at..at + 2].copy_from_slice(&replacement);
        (encoded.shp, dbf)
    }

    fn name_and_warnings(shp: &[u8], dbf: &[u8], cpg: &str) -> (String, Vec<String>) {
        let mut warnings = Vec::new();
        let fc = read(shp, dbf, None, Some(cpg), &mut warnings).unwrap();
        (fc.features[0].properties[0].1.as_str().unwrap().to_string(), warnings)
    }

    #[test]
    fn an_unrecognised_cpg_over_non_ascii_text_warns_that_it_was_approximated() {
        // 0xE9 is é in both ISO-8859-1 and Windows-1252, so the value is right
        // here — but the reader cannot know that, and says so.
        let (shp, dbf) = shapefile_with_name_bytes(&[0xE9]);
        let (name, warnings) = name_and_warnings(&shp, &dbf, "1252\n");
        assert_eq!(name, "café");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("\"1252\""), "{warnings:?}");
        assert!(warnings[0].contains("1 text value(s)"), "{warnings:?}");
    }

    #[test]
    fn an_unrecognised_cpg_over_ascii_text_is_exact_and_silent() {
        let (shp, dbf) = shapefile_with_name_bytes(b"e");
        let (name, warnings) = name_and_warnings(&shp, &dbf, "CP1252");
        assert_eq!(name, "cafe");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_declared_latin_1_cpg_is_exact_and_silent() {
        for cpg in ["ISO-8859-1", "88591", "latin1"] {
            let (shp, dbf) = shapefile_with_name_bytes(&[0xE9]);
            let (name, warnings) = name_and_warnings(&shp, &dbf, cpg);
            assert_eq!(name, "café", "for {cpg}");
            assert!(warnings.is_empty(), "for {cpg}: {warnings:?}");
        }
    }

    #[test]
    fn a_utf8_cpg_over_invalid_bytes_warns_that_they_were_replaced() {
        let (shp, dbf) = shapefile_with_name_bytes(&[0xE9]);
        let (name, warnings) = name_and_warnings(&shp, &dbf, "UTF-8");
        assert_eq!(name, "caf\u{FFFD}");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("U+FFFD"), "{warnings:?}");
    }
}
