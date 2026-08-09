//! Coordinate reference system identity, carried opaquely through the IR.
//!
//! Geosetta is a format translator, not a projection engine: it never
//! interprets a CRS or transforms coordinates. It only *passes a CRS through*
//! from the input to the output. Because each format records CRS differently
//! (an authority code, a WKT string, a PROJJSON object), a [`Crs`] holds every
//! representation a reader was able to recover, and each writer emits whichever
//! form its format speaks — falling back to "unspecified" rather than guessing
//! when it cannot express what it was given.
//!
//! Adding a new format is the same shape as everything else in the crate: its
//! reader fills in whatever CRS fields it can recover, and its writer emits the
//! representation its wire format uses. New encodings just add a field to
//! [`NamedCrs`]; nothing else has to change.

/// The coordinate reference system a [`crate::feature::FeatureCollection`] is
/// expressed in.
///
/// `None` on the collection means the source recorded no CRS at all (e.g. bare
/// CSV or WKT); it is distinct from [`Crs::Wgs84`], which means the source
/// specified — implicitly or explicitly — WGS 84 longitude/latitude.
#[derive(Debug, Clone, PartialEq)]
pub enum Crs {
    /// WGS 84 geographic coordinates in longitude/latitude order — OGC:CRS84,
    /// the implicit default of GeoJSON (RFC 7946) and GeoParquet. Writers emit
    /// it in each format's idiomatic spelling: nothing at all in GeoJSON,
    /// an omitted `crs` in GeoParquet, and EPSG:4326 in GeoPackage /
    /// FlatGeobuf.
    Wgs84,
    /// Any other reference system, carried opaquely by whatever the source
    /// recorded. Geosetta never parses these strings; they exist only to be
    /// handed back out to a writer.
    Named(NamedCrs),
}

/// The recovered identity of a non-default CRS. Every field is optional because
/// different formats record different subsets: GeoPackage and FlatGeobuf carry
/// an authority + code (and sometimes WKT), while GeoParquet carries PROJJSON.
/// A writer uses the richest representation its format accepts and omits the
/// CRS when it has none of the fields it needs.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NamedCrs {
    /// Authority / organization name, e.g. `"EPSG"`.
    pub authority: Option<String>,
    /// Authority code within `authority`, e.g. `3857`.
    pub code: Option<i64>,
    /// Verbatim WKT (WKT1 or WKT2) definition, if the source recorded one.
    pub wkt: Option<String>,
    /// Verbatim PROJJSON definition, if the source recorded one.
    pub projjson: Option<String>,
}

impl Crs {
    /// Interpret an authority + code as a [`Crs`], collapsing the well-known
    /// WGS 84 geographic spellings (`EPSG:4326`, `OGC:CRS84`) to [`Crs::Wgs84`]
    /// so every format renders the default consistently. Anything else becomes
    /// a [`Crs::Named`] carrying the given fields.
    pub fn from_authority_code(
        authority: Option<String>,
        code: Option<i64>,
        wkt: Option<String>,
        projjson: Option<String>,
    ) -> Crs {
        let auth = authority.as_deref().map(str::to_ascii_uppercase);
        let is_wgs84 = matches!(
            (auth.as_deref(), code),
            (Some("EPSG"), Some(4326)) | (Some("OGC"), Some(4326))
        );
        if is_wgs84 {
            Crs::Wgs84
        } else {
            Crs::Named(NamedCrs {
                authority,
                code,
                wkt,
                projjson,
            })
        }
    }
}
