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
pub fn read(shp: &[u8], dbf_bytes: &[u8], prj: Option<&str>, cpg: Option<&str>) -> Result<FeatureCollection> {
    let geometries = geometry::read(shp)?;
    let encoding = dbf::encoding_from_cpg(cpg);
    let records = dbf::read(dbf_bytes, encoding, cpg.is_some())?;

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
        features.push(Feature { geometry, properties: record.properties });
    }

    let mut fc = FeatureCollection::new(features);
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
            let back = read(&shp, &dbf, Some(&framed), None).unwrap();
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
                read(&shp, &dbf, Some(blank), None).unwrap().crs.is_none(),
                "for {blank:?}"
            );
        }
    }

    #[test]
    fn reads_back_a_written_shapefile() {
        let fc = one_point_fc();
        let encoded = writer::write(&fc).unwrap();
        let back = read(&encoded.shp, &encoded.dbf, encoded.prj.as_deref(), None).unwrap();
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
        assert!(read(&shp, &empty_dbf, None, None).is_err());
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
        let back = read(&encoded.shp, &encoded.dbf, encoded.prj.as_deref(), None).unwrap();
        match back.crs {
            Some(Crs::Named(n)) => assert_eq!(n.code.as_deref(), Some("3857")),
            other => panic!("expected Named EPSG:3857, got {other:?}"),
        }
    }
}
