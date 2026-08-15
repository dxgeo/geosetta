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
    /// Non-fatal warnings from lossy `.dbf` truncation (a property value or
    /// name too long for dBase's fixed-width fields) — see [`dbf::write`].
    /// Empty in the overwhelmingly common case where nothing was truncated.
    pub warnings: Vec<String>,
}

/// Encode a feature collection as a Shapefile. Errors when the geometries mix
/// incompatible Shapefile shape families, or contain a `GeometryCollection`
/// (Shapefile cannot represent either) — see [`geometry::write`] — or when the
/// `.dbf` can't represent the properties without silently corrupting or
/// merging data — see [`dbf::write`].
pub fn write(fc: &FeatureCollection) -> Result<Encoded> {
    let geometries: Vec<Option<Geometry>> = fc.features.iter().map(|f| f.geometry.clone()).collect();
    let (shp, shx) = geometry::write(&geometries)?;

    let columns = infer_columns(&fc.features);
    let (dbf_bytes, warnings) = dbf::write(&columns)?;

    let prj = fc.crs.as_ref().and_then(prj_wkt);

    Ok(Encoded { shp, shx, dbf: dbf_bytes, prj, warnings })
}

/// The WKT text to write to `.prj` for this CRS, or `None` when it can't be
/// expressed: prefer the source's own verbatim WKT, else — with the
/// `crs-registry` feature — the registry's authoritative WKT for a known
/// authority+code (R1's `def_wkt`, this spoke is its first consumer), else a
/// structural PROJJSON→WKT translation for an id-less PROJJSON-only source
/// (see `NamedCrs::structural_wkt`).
fn prj_wkt(crs: &Crs) -> Option<String> {
    match crs {
        Crs::Wgs84 => Some(WGS84_WKT1.to_string()),
        Crs::Named(n) => n
            .wkt
            .clone()
            .or_else(|| n.registry_wkt().map(str::to_string))
            .or_else(|| n.structural_wkt()),
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

    #[test]
    fn id_less_projjson_writes_prj_via_structural_translation() {
        // The gap `plans/projjson-to-wkt.org` closes: a GeoParquet source
        // whose PROJJSON carries no authority code at all — the registry
        // can't help (nothing to key a lookup on) — still reaches `.prj` via
        // `NamedCrs::structural_wkt`, independent of the registry feature.
        let pj = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"custom datum","ellipsoid":{"name":"custom ellipsoid","semi_major_axis":6378137,"inverse_flattening":298.257223563}}}"#;
        let crs = Crs::Named(NamedCrs { projjson: Some(pj.into()), ..Default::default() });
        let encoded = write(&one_point_fc(Some(crs))).unwrap();
        let prj = encoded.prj.expect(".prj written via structural translation");
        assert!(prj.starts_with("GEOGCS[\"custom\""), "{prj}");
        assert!(prj.contains("SPHEROID[\"custom ellipsoid\",6378137,298.257223563]"), "{prj}");
    }
}
