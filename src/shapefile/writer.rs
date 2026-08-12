//! Encode a [`FeatureCollection`] into a Shapefile's sibling files. Writing
//! the sibling set to disk under a shared basename is main.rs's job, mirroring
//! how GeoPackage's `write_layers` returns bytes and leaves file I/O to the
//! caller.

use crate::crs::Crs;
use crate::error::Result;
use crate::feature::FeatureCollection;
use crate::geometry::Geometry;
use crate::schema::infer_columns;
use crate::shapefile::{dbf, geometry};

/// The encoded sibling files of a Shapefile. `.shp`/`.shx`/`.dbf` are always
/// produced; `.prj` is `None` when the collection's CRS can't be expressed as
/// WKT (see [`crate::crs::Crs::downgrade_warning`]'s `Format::Shapefile` arm)
/// — `.prj` is Shapefile's only CRS slot, so an inexpressible CRS is omitted
/// rather than guessed, matching the project's never-guess convention.
pub struct Encoded {
    pub shp: Vec<u8>,
    pub shx: Vec<u8>,
    pub dbf: Vec<u8>,
    pub prj: Option<String>,
}

/// Encode a feature collection as a Shapefile. Errors when the geometries mix
/// incompatible Shapefile shape families, or contain a `GeometryCollection`
/// (Shapefile cannot represent either) — see [`geometry::write`].
pub fn write(fc: &FeatureCollection) -> Result<Encoded> {
    let geometries: Vec<Option<Geometry>> = fc.features.iter().map(|f| f.geometry.clone()).collect();
    let (shp, shx) = geometry::write(&geometries)?;

    let columns = infer_columns(&fc.features);
    let dbf_bytes = dbf::write(&columns);

    let prj = fc.crs.as_ref().and_then(prj_wkt);

    Ok(Encoded { shp, shx, dbf: dbf_bytes, prj })
}

/// The WKT text to write to `.prj` for this CRS, or `None` when it can't be
/// expressed: prefer the source's own verbatim WKT, else — with the
/// `crs-registry` feature — the registry's authoritative WKT for a known
/// authority+code (R1's `def_wkt`, this spoke is its first consumer).
fn prj_wkt(crs: &Crs) -> Option<String> {
    match crs {
        Crs::Wgs84 => Some(WGS84_WKT1.to_string()),
        Crs::Named(n) => n.wkt.clone().or_else(|| n.registry_wkt().map(str::to_string)),
    }
}

/// The canonical Esri-flavor WGS 84 `.prj` text — the exact spelling countless
/// real-world shapefiles carry verbatim, so the common case needs no registry
/// lookup or translation to write.
const WGS84_WKT1: &str = "GEOGCS[\"GCS_WGS_1984\",DATUM[\"D_WGS_1984\",SPHEROID[\"WGS_1984\",6378137.0,298.257223563]],PRIMEM[\"Greenwich\",0.0],UNIT[\"Degree\",0.0174532925199433]]";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::NamedCrs;
    use crate::feature::Feature;

    fn one_point_fc(crs: Option<Crs>) -> FeatureCollection {
        let mut fc = FeatureCollection::new(vec![Feature {
            geometry: Some(Geometry::Point([1.0, 2.0])),
            properties: vec![],
        }]);
        fc.crs = crs;
        fc
    }

    #[test]
    fn wgs84_writes_the_canonical_prj() {
        let encoded = write(&one_point_fc(Some(Crs::Wgs84))).unwrap();
        assert_eq!(encoded.prj.as_deref(), Some(WGS84_WKT1));
    }

    #[test]
    fn no_crs_writes_no_prj() {
        let encoded = write(&one_point_fc(None)).unwrap();
        assert_eq!(encoded.prj, None);
    }

    #[test]
    fn named_crs_with_wkt_writes_it_verbatim() {
        let wkt = "PROJCS[\"custom\"]";
        let crs = Crs::Named(NamedCrs { wkt: Some(wkt.into()), ..Default::default() });
        let encoded = write(&one_point_fc(Some(crs))).unwrap();
        assert_eq!(encoded.prj.as_deref(), Some(wkt));
    }

    #[test]
    fn bare_code_without_wkt_writes_no_prj_without_the_registry_feature() {
        // Shapefile's .prj has no code slot of its own (unlike FlatGeobuf/
        // GeoPackage); without crs-registry there is nothing to translate a
        // bare code into, so no .prj is written.
        let crs = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("3857".into()),
            ..Default::default()
        });
        let encoded = write(&one_point_fc(Some(crs))).unwrap();
        #[cfg(not(feature = "crs-registry"))]
        assert_eq!(encoded.prj, None);
        // With the registry on, this now resolves via def_wkt.
        #[cfg(feature = "crs-registry")]
        assert!(encoded.prj.is_some());
    }
}
