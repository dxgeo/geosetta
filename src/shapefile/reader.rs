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
    if let Some(wkt) = prj {
        fc.crs = Some(Crs::from_authority_code(None, None, Some(wkt.to_string()), None));
    }
    Ok(fc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Geometry;
    use crate::schema::infer_columns;
    use crate::shapefile::writer;

    fn one_point_fc() -> FeatureCollection {
        FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point([1.0, 2.0])),
            properties: vec![("name".into(), crate::json::JsonValue::String("a".into()))],
        }])
    }

    #[test]
    fn reads_back_a_written_shapefile() {
        let fc = one_point_fc();
        let encoded = writer::write(&fc).unwrap();
        let back = read(&encoded.shp, &encoded.dbf, encoded.prj.as_deref(), None).unwrap();
        assert_eq!(back.features.len(), 1);
        assert_eq!(back.features[0].geometry, Some(Geometry::Point([1.0, 2.0])));
    }

    #[test]
    fn mismatched_record_counts_error() {
        let fc = one_point_fc();
        let columns = infer_columns(&fc.features);
        // A .dbf with zero records paired against a one-record .shp.
        let empty_dbf = dbf::write(&columns[..0]);
        let (shp, _shx) = crate::shapefile::geometry::write(&[Some(Geometry::Point([0.0, 0.0]))]).unwrap();
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
