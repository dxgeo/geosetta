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
//!
//! Not every target can express every CRS, though. When a non-WGS 84 identity
//! meets a format with no way to record it — GeoJSON is always WGS 84, CSV/WKT
//! carry no CRS at all — the loss is real and unavoidable (Geosetta does not
//! reproject), so [`Crs::downgrade_warning`] produces the `stderr` warning the
//! CLI emits rather than silently mislabeling or dropping the CRS.

use crate::format::Format;

mod wkt1_tables;
mod wkt_projjson;
pub(crate) use wkt_projjson::wkt_to_projjson;

// Embedded CRS registry (code -> authoritative definition). Feature-gated so the
// default build stays dependency-free; the data lives in the sibling
// `geosetta-crs-data` crate. Skeleton — see `../../geosetta-crs-data/crs-registry.org`.
#[cfg(feature = "crs-registry")]
mod registry;

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
    /// Authority code within `authority`, e.g. `"3857"`. A string rather than a
    /// number: most authorities (EPSG, ESRI, IAU_2015) use numeric codes, but
    /// IGNF/OGC/PROJ/NKG use alphanumeric ones (`"LAMB93"`, `"CRS84"`), and one
    /// code type has to serve every authority.
    pub code: Option<String>,
    /// Verbatim WKT (WKT1 or WKT2) definition, if the source recorded one.
    pub wkt: Option<String>,
    /// Verbatim PROJJSON definition, if the source recorded one.
    pub projjson: Option<String>,
}

/// The registry's WKT2:2019 for `EPSG:4326` — for [`Crs::Wgs84`] sources,
/// which (unlike [`Crs::Named`]) have no [`NamedCrs`] to call
/// [`NamedCrs::registry_wkt2`] on. `None` without `crs-registry`.
#[cfg(feature = "crs-registry")]
pub(crate) fn wgs84_registry_wkt2() -> Option<&'static str> {
    registry::def_wkt2("EPSG", "4326")
}

#[cfg(not(feature = "crs-registry"))]
pub(crate) fn wgs84_registry_wkt2() -> Option<&'static str> {
    None
}

impl Crs {
    /// Interpret an authority + code as a [`Crs`], collapsing the well-known
    /// WGS 84 geographic spellings (`EPSG:4326`, `OGC:CRS84`) to [`Crs::Wgs84`]
    /// so every format renders the default consistently. Anything else becomes
    /// a [`Crs::Named`] carrying the given fields.
    ///
    /// When a rich-format source supplies *neither* an authority nor a code but
    /// *does* carry a WKT definition, the CRS's own `AUTHORITY`/`ID` node is
    /// lifted out of the WKT (see [`wkt_authority_code`]) and used as the
    /// authority+code — the same lexical move PROJJSON's `id` gets in
    /// [`crate::parquet::geo::parse_crs`]. That makes the identity portable to
    /// every authority+code target instead of stranding it in a string, and it
    /// feeds the WGS 84 collapse below (a WKT-only `EPSG:4326` still becomes
    /// [`Crs::Wgs84`]). A caller that already knows the authority or code is left
    /// untouched — the recovery only fills a total blank.
    pub fn from_authority_code(
        authority: Option<String>,
        code: Option<String>,
        wkt: Option<String>,
        projjson: Option<String>,
    ) -> Crs {
        let (authority, code) = match (authority, code) {
            (None, None) => match wkt.as_deref().and_then(wkt_authority_code) {
                Some((a, c)) => (Some(a), Some(c)),
                None => (None, None),
            },
            pair => pair,
        };

        let auth = authority.as_deref().map(str::to_ascii_uppercase);
        let is_wgs84 = matches!(
            (auth.as_deref(), code.as_deref()),
            (Some("EPSG"), Some("4326")) | (Some("OGC"), Some("CRS84"))
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

    /// A warning to print when converting a collection in this CRS to `target`,
    /// or `None` when `target` can faithfully record it.
    ///
    /// Geosetta labels but never reprojects, so writing a non-WGS 84 dataset to
    /// a format that cannot express its CRS is genuinely lossy. Two kinds of
    /// loss are announced, both write-side only (nothing is ever lost on the way
    /// *in* — reading GeoJSON always yields [`Crs::Wgs84`] and CSV/WKT yield no
    /// CRS):
    ///
    /// 1. *CRS-less targets.* GeoJSON forces WGS 84 (the coordinates would be
    ///    *mislabeled*), and CSV/WKT have no CRS slot at all (the label is
    ///    *dropped*). Any non-WGS 84 CRS triggers this.
    /// 2. *Dialect-mismatched rich targets.* GeoParquet records CRS as PROJJSON;
    ///    FlatGeobuf and GeoPackage record it as WKT. A CRS that carries only a
    ///    *definition string in the other dialect, with no authority code to
    ///    fall back on* cannot cross the gap — Geosetta lifts an embedded id when
    ///    there is one (see [`Crs::from_authority_code`]) but does not translate
    ///    WKT↔PROJJSON structurally. Such a CRS is dropped, so it warns.
    ///
    /// [`Crs::Wgs84`] never warns: it is representable (or the idiomatic default)
    /// in every target. A rich CRS that carries a portable authority+code, or a
    /// definition already in the target's dialect, is faithful and stays silent.
    pub fn downgrade_warning(&self, target: Format) -> Option<String> {
        let named = match self {
            Crs::Named(n) => n,
            Crs::Wgs84 => return None,
        };
        let label = named.label();
        match target {
            Format::GeoJson => Some(format!(
                "warning: source CRS is {label}; GeoJSON is always WGS 84 — output \
                 coordinates will be mislabeled. Reproject to EPSG:4326 before converting."
            )),
            Format::Csv | Format::Wkt => Some(format!(
                "warning: {} cannot record a CRS; {label} will be dropped from the output.",
                target.extension().to_ascii_uppercase()
            )),
            // GeoParquet speaks PROJJSON: faithful with verbatim PROJJSON or a
            // WKT definition Geosetta can translate (geographic, WKT2 projected,
            // and the common unambiguous WKT1 projected methods). A bare code, or
            // a WKT outside that set (an ambiguous/unsupported WKT1 projection, a
            // south/west-oriented grid whose WKT1 omits its axes), yields only an
            // unresolvable id reference.
            Format::Parquet if !named.parquet_expressible() => {
                let detail = if named.wkt.is_some() {
                    "Geosetta cannot translate this WKT definition to PROJJSON (its projection \
                     is unsupported or ambiguous in WKT1)"
                } else {
                    "Geosetta has only an authority code, not a full definition to translate"
                };
                // Only truthful without the registry: with it on, an
                // authority+code that reaches this branch already failed
                // `registry_projjson` too (not just missing), so rebuilding
                // would not change the outcome.
                #[cfg(not(feature = "crs-registry"))]
                let hint = " Rebuild with `--features crs-registry` for full CRS resolution.";
                #[cfg(feature = "crs-registry")]
                let hint = "";
                Some(format!(
                    "warning: source CRS {label} will not be resolvable in the GeoParquet output — \
                     {detail}, so it is written only as an id reference that PROJ/GDAL/QGIS read as unknown.{hint}"
                ))
            }
            // FlatGeobuf/GeoPackage speak WKT: faithful with a WKT definition or
            // a usable authority code; a PROJJSON-only CRS has neither.
            Format::FlatGeobuf | Format::Gpkg if !named.wkt_expressible() => Some(format!(
                "warning: source CRS is a {} definition with no authority code, and {} \
                 records CRS as WKT; Geosetta does not translate between the two, so the \
                 CRS will be dropped from the output.",
                named.definition_dialect(),
                rich_format_name(target),
            )),
            // Shapefile's .prj is pure WKT text with no separate code slot (unlike
            // FlatGeobuf/GeoPackage), so a bare authority code is only expressible
            // via the registry's def_wkt, not natively as it is for those two.
            Format::Shapefile if !named.shapefile_expressible() => {
                let detail = if named.wkt.is_some() {
                    "Geosetta has only a non-WKT (PROJJSON) definition, and .prj records CRS as WKT text"
                } else {
                    "Geosetta has only an authority code, not a WKT definition to write"
                };
                #[cfg(not(feature = "crs-registry"))]
                let hint = " Rebuild with `--features crs-registry` for full CRS resolution.";
                #[cfg(feature = "crs-registry")]
                let hint = "";
                Some(format!(
                    "warning: source CRS {label} will not be recorded in the Shapefile output — \
                     {detail}, so no .prj will be written.{hint}"
                ))
            }
            Format::Parquet | Format::FlatGeobuf | Format::Gpkg | Format::Shapefile => None,
        }
    }
}

/// A human-readable name for a rich target format, for warning messages.
fn rich_format_name(target: Format) -> &'static str {
    match target {
        Format::Parquet => "GeoParquet",
        Format::FlatGeobuf => "FlatGeobuf",
        Format::Gpkg => "GeoPackage",
        Format::Shapefile => "Shapefile",
        // The CRS-less formats never reach this helper.
        Format::GeoJson | Format::Csv | Format::Wkt => target.extension(),
    }
}

impl NamedCrs {
    /// A short human label for warning messages: `EPSG:7844` when both authority
    /// and code are known, else whichever single field is present, else a
    /// generic fallback for a CRS carried only as a WKT/PROJJSON string.
    fn label(&self) -> String {
        match (self.authority.as_deref(), self.code.as_deref()) {
            (Some(a), Some(c)) => format!("{a}:{c}"),
            (None, Some(c)) => format!("code {c}"),
            (Some(a), None) => format!("authority {a}"),
            (None, None) => "the source CRS".into(),
        }
    }

    /// Whether GeoParquet can record this CRS *resolvably*. It speaks PROJJSON,
    /// so it needs verbatim PROJJSON, a WKT definition Geosetta can translate
    /// into PROJJSON (see [`wkt_to_projjson`]), or — with the `crs-registry`
    /// feature — an authority+code the embedded registry resolves (see
    /// [`Self::registry_projjson`]). Without the feature a bare authority+code
    /// is *not* enough: an id-only reference is invalid PROJJSON that
    /// PROJ/GDAL/QGIS read as unknown. Mirrors [`crate::parquet::geo`]'s
    /// `crs_projjson`.
    fn parquet_expressible(&self) -> bool {
        self.projjson.is_some()
            || self.wkt.as_deref().is_some_and(|w| wkt_to_projjson(w).is_some())
            || self.registry_projjson().is_some()
    }

    /// The authoritative PROJJSON for this CRS, from the embedded registry.
    /// Two paths, mutually exclusive:
    ///
    /// 1. *Trusted inline id* (R1): an authority+code the source already
    ///    declared is looked up directly — no structural validation, since an
    ///    authority code *is* the declared identity. If the registry doesn't
    ///    have that exact pair, this does *not* fall through to name-based
    ///    recovery below: guessing a different code than the one the source
    ///    declared would be a mislabel, not a recovery.
    /// 2. *Name → code recovery* (R2), only when the source declared *neither*
    ///    an authority nor a code: the WKT's outer name is looked up and
    ///    validated against its own ellipsoid before snapping (see
    ///    [`registry::resolve_geographic_by_name`]). Scoped to geographic
    ///    CRSes for now — see that function's doc comment.
    ///
    /// `None` when the `crs-registry` feature is off, or neither path resolves
    /// (a genuinely custom/non-catalog CRS, or a projected id-less WKT — not
    /// yet handled).
    #[cfg(feature = "crs-registry")]
    pub(crate) fn registry_projjson(&self) -> Option<&'static str> {
        if let (Some(auth), Some(code)) = (self.authority.as_deref(), self.code.as_deref()) {
            return registry::def_projjson(auth, code);
        }
        self.wkt.as_deref().and_then(registry::resolve_geographic_by_name)
    }

    #[cfg(not(feature = "crs-registry"))]
    pub(crate) fn registry_projjson(&self) -> Option<&'static str> {
        None
    }

    /// Whether the WKT-dialect targets (FlatGeobuf, GeoPackage) can record this
    /// CRS. They store a WKT `definition` and/or an authority code, so they need
    /// a WKT string or a usable code; a PROJJSON-only definition has neither.
    /// Mirrors `flatgeobuf::writer`'s `build_crs` and `geopackage::writer`'s
    /// `resolve_srs`.
    fn wkt_expressible(&self) -> bool {
        self.wkt.is_some() || self.code.is_some()
    }

    /// Whether Shapefile's `.prj` (pure WKT text, no separate code slot) can
    /// record this CRS: a WKT definition, or — with the `crs-registry` feature —
    /// an authority+code the embedded registry can render as WKT (see
    /// [`Self::registry_wkt`]). Unlike [`Self::wkt_expressible`], a bare code is
    /// *not* enough on its own, since `.prj` has nowhere to put a code without a
    /// WKT wrapper. Mirrors `shapefile::writer`'s `.prj` writer.
    fn shapefile_expressible(&self) -> bool {
        self.wkt.is_some() || self.registry_wkt().is_some()
    }

    /// The authoritative WKT for this CRS from the embedded registry, keyed by
    /// authority+code (R1's `def_wkt`). `None` when the `crs-registry` feature is
    /// off, either field is missing, or the registry has no WKT for that code
    /// (`has_wkt=0`, R1's ~3.3% PROJJSON-only slice).
    #[cfg(feature = "crs-registry")]
    pub(crate) fn registry_wkt(&self) -> Option<&'static str> {
        let (auth, code) = (self.authority.as_deref()?, self.code.as_deref()?);
        registry::def_wkt(auth, code)
    }

    #[cfg(not(feature = "crs-registry"))]
    pub(crate) fn registry_wkt(&self) -> Option<&'static str> {
        None
    }

    /// The authoritative WKT2:2019 for this CRS from the embedded registry,
    /// keyed by authority+code (R5's `def_wkt2`). `None` when the
    /// `crs-registry` feature is off or either field is missing — but
    /// essentially never `None` for a pair that *does* resolve, since
    /// WKT2:2019 is present for effectively every entry (R5), including all
    /// of `registry_wkt`'s `has_wkt=0` gap.
    #[cfg(feature = "crs-registry")]
    pub(crate) fn registry_wkt2(&self) -> Option<&'static str> {
        let (auth, code) = (self.authority.as_deref()?, self.code.as_deref()?);
        registry::def_wkt2(auth, code)
    }

    #[cfg(not(feature = "crs-registry"))]
    pub(crate) fn registry_wkt2(&self) -> Option<&'static str> {
        None
    }

    /// The dialect of the definition string this CRS carries, for warning
    /// messages — the form a dialect-mismatched rich target cannot translate.
    fn definition_dialect(&self) -> &'static str {
        if self.wkt.is_some() {
            "WKT"
        } else if self.projjson.is_some() {
            "PROJJSON"
        } else {
            "CRS"
        }
    }
}

/// A lexical WKT token. WKT (both WKT1 and WKT2) is a tree of
/// `KEYWORD[value, value, ...]` nodes; recovering a CRS's identifier needs only
/// this shallow tokenization, never a full parse of the projection it describes.
enum WktTok {
    Open,
    Close,
    Comma,
    Str(String),
    Word(String),
}

/// Tokenize a WKT string into brackets, commas, quoted strings, and bare words
/// (keywords like `AUTHORITY`, or numbers like `7844`). Deliberately lenient:
/// `[`/`(` and `]`/`)` are treated alike, and anything that isn't a delimiter or
/// whitespace runs into a single `Word`.
fn tokenize_wkt(s: &str) -> Vec<WktTok> {
    let b = s.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'[' | b'(' => {
                toks.push(WktTok::Open);
                i += 1;
            }
            b']' | b')' => {
                toks.push(WktTok::Close);
                i += 1;
            }
            b',' => {
                toks.push(WktTok::Comma);
                i += 1;
            }
            b'"' => {
                i += 1;
                let start = i;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                toks.push(WktTok::Str(s[start..i].to_string()));
                i += 1; // skip the closing quote (a no-op if the string was unterminated)
            }
            c if c.is_ascii_whitespace() => i += 1,
            _ => {
                let start = i;
                while i < b.len()
                    && !matches!(b[i], b'[' | b']' | b'(' | b')' | b',' | b'"')
                    && !b[i].is_ascii_whitespace()
                {
                    i += 1;
                }
                toks.push(WktTok::Word(s[start..i].to_string()));
            }
        }
    }
    toks
}

/// Pull the top-level `AUTHORITY["EPSG","7844"]` (WKT1) or `ID["EPSG",7844]`
/// (WKT2) identifier out of a CRS WKT string as an `(authority, code)` pair.
///
/// A WKT definition nests identifiers on its datum, ellipsoid, prime meridian,
/// axes, and units; the CRS's *own* id is the shallowest one — a direct child of
/// the outermost keyword — and, when several sit at that depth (a bound/compound
/// CRS), the last one. This is a purely lexical extraction: Geosetta never
/// interprets the projection the WKT describes. The code is captured verbatim
/// (the IR's code is a string), so an alphanumeric authority code (e.g.
/// `OGC:CRS84`) round-trips just as well as a numeric one.
fn wkt_authority_code(wkt: &str) -> Option<(String, String)> {
    let toks = tokenize_wkt(wkt);
    let mut depth: i32 = 0;
    // (depth, authority, code) of the shallowest id seen; ties resolve to the
    // later one, so `<=` replaces on equal depth.
    let mut best: Option<(i32, String, String)> = None;
    for i in 0..toks.len() {
        match &toks[i] {
            WktTok::Open => depth += 1,
            WktTok::Close => depth -= 1,
            WktTok::Word(w) if w.eq_ignore_ascii_case("AUTHORITY") || w.eq_ignore_ascii_case("ID") => {
                // The keyword sits at the current depth; its node is
                // `Open Str(authority) Comma <code> ...`.
                if let (Some(WktTok::Open), Some(WktTok::Str(authority)), Some(WktTok::Comma)) =
                    (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3))
                {
                    let code = match toks.get(i + 4) {
                        // WKT1 quotes the code (`"7844"`); WKT2 leaves it bare.
                        Some(WktTok::Str(c)) | Some(WktTok::Word(c)) => Some(c.clone()),
                        _ => None,
                    };
                    if let Some(code) = code
                        && best.as_ref().is_none_or(|(bd, _, _)| depth <= *bd)
                    {
                        best = Some((depth, authority.clone(), code));
                    }
                }
            }
            _ => {}
        }
    }
    best.map(|(_, a, c)| (a, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gda2020() -> Crs {
        Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("7844".into()),
            wkt: None,
            projjson: None,
        })
    }

    #[test]
    fn wgs84_never_warns() {
        for target in [
            Format::GeoJson,
            Format::Csv,
            Format::Wkt,
            Format::Parquet,
            Format::FlatGeobuf,
            Format::Gpkg,
        ] {
            assert_eq!(Crs::Wgs84.downgrade_warning(target), None);
        }
    }

    #[test]
    fn wkt_dialect_targets_dont_warn_for_a_portable_code() {
        // FlatGeobuf and GeoPackage store an authority code natively, so a
        // code-only non-WGS 84 CRS to either is silent.
        for target in [Format::FlatGeobuf, Format::Gpkg] {
            assert_eq!(gda2020().downgrade_warning(target), None);
        }
    }

    #[test]
    #[cfg(not(feature = "crs-registry"))]
    fn parquet_warns_for_a_code_only_crs() {
        // Without the registry, GeoParquet has no verbatim PROJJSON and no WKT
        // to translate, so a bare code can only be written as an unresolvable id
        // reference — warn, with the rebuild hint.
        let w = gda2020().downgrade_warning(Format::Parquet).unwrap();
        assert!(w.contains("EPSG:7844"), "{w}");
        assert!(w.contains("resolv"), "{w}");
        assert!(w.contains("id reference"), "{w}");
        assert!(w.contains("--features crs-registry"), "{w}");
    }

    #[test]
    #[cfg(feature = "crs-registry")]
    fn parquet_resolves_a_code_only_crs_via_registry() {
        // With the registry on, the same bare EPSG:7844 (a real registry entry)
        // now resolves to the authoritative PROJJSON — no warning, no hint. This
        // is R1's headline behavior change (crs-registry.org, "the hint stops
        // firing").
        assert_eq!(gda2020().downgrade_warning(Format::Parquet), None);
    }

    #[test]
    #[cfg(feature = "crs-registry")]
    fn parquet_still_warns_for_a_code_the_registry_does_not_have() {
        // A code genuinely absent from the registry (custom/non-catalog) still
        // warns — the registry only resolves what's actually in proj.db, and the
        // hint is correctly absent (rebuilding would not help).
        let custom = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("999999999".into()),
            wkt: None,
            projjson: None,
        });
        let w = custom.downgrade_warning(Format::Parquet).unwrap();
        assert!(w.contains("id reference"), "{w}");
        assert!(!w.contains("--features crs-registry"), "{w}");
    }

    #[test]
    #[cfg(feature = "crs-registry")]
    fn parquet_resolves_an_id_less_esri_geographic_wkt_via_name_recovery() {
        // R2: a shapefile-.prj-shaped source — Esri WKT1, no AUTHORITY node at
        // all, non-standard datum spelling — carries no authority/code
        // (from_authority_code's id-lift finds nothing, since there's no id to
        // lift). Structural translation alone would decline or mis-identify
        // this (crs-registry.org measured NAD83 -> wrong EPSG:9309); name
        // recovery gets the real code and resolves it, no warning.
        let esri_nad83 = "GEOGCS[\"GCS_North_American_1983\",DATUM[\"D_North_American_1983\",\
             SPHEROID[\"GRS_1980\",6378137.0,298.257222101]],PRIMEM[\"Greenwich\",0.0],\
             UNIT[\"Degree\",0.0174532925199433]]";
        let crs = Crs::from_authority_code(None, None, Some(esri_nad83.into()), None);
        match &crs {
            Crs::Named(n) => assert_eq!((n.authority.as_deref(), n.code.as_deref()), (None, None)),
            other => panic!("expected an id-less Named CRS (nothing to lift), got {other:?}"),
        }
        assert_eq!(crs.downgrade_warning(Format::Parquet), None);
    }

    #[test]
    fn parquet_does_not_warn_for_a_translatable_wkt() {
        // A geographic WKT definition is translatable to resolvable PROJJSON, so
        // GeoParquet output is faithful — no warning.
        let crs = Crs::Named(NamedCrs {
            wkt: Some(
                "GEOGCS[\"GDA2020\",DATUM[\"GDA2020\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],\
                 AUTHORITY[\"EPSG\",\"7844\"]]"
                    .into(),
            ),
            ..Default::default()
        });
        assert_eq!(crs.downgrade_warning(Format::Parquet), None);
    }

    fn wkt_only() -> Crs {
        // A WKT with no datum/ellipsoid: not translatable, so it stands in for
        // the "can't express" case.
        Crs::Named(NamedCrs { wkt: Some("GEOGCRS[\"custom\"]".into()), ..Default::default() })
    }

    fn projjson_only() -> Crs {
        Crs::Named(NamedCrs {
            projjson: Some("{\"type\":\"GeographicCRS\",\"name\":\"custom\"}".into()),
            ..Default::default()
        })
    }

    #[test]
    fn parquet_warns_for_an_untranslatable_wkt() {
        // A WKT with no datum/ellipsoid can't be translated to resolvable
        // PROJJSON, so GeoParquet output warns.
        let w = wkt_only().downgrade_warning(Format::Parquet).unwrap();
        assert!(w.contains("WKT definition"), "{w}");
        assert!(w.contains("resolv"), "{w}");
        // A PROJJSON-only CRS is fine for GeoParquet — it carries through.
        assert_eq!(projjson_only().downgrade_warning(Format::Parquet), None);
    }

    #[test]
    fn wkt_targets_warn_for_a_projjson_only_crs() {
        // PROJJSON definition, no code → FlatGeobuf/GeoPackage (WKT) can't express it.
        for (target, name) in [(Format::FlatGeobuf, "FlatGeobuf"), (Format::Gpkg, "GeoPackage")] {
            let w = projjson_only().downgrade_warning(target).unwrap();
            assert!(w.contains("PROJJSON definition"), "{w}");
            assert!(w.contains(name), "{w}");
            assert!(w.contains("dropped"), "{w}");
            // A WKT-only CRS is fine for these — the definition carries through.
            assert_eq!(wkt_only().downgrade_warning(target), None);
        }
    }

    #[test]
    fn geojson_warns_about_mislabeling() {
        let w = gda2020().downgrade_warning(Format::GeoJson).unwrap();
        assert!(w.contains("EPSG:7844"), "{w}");
        assert!(w.contains("WGS 84"), "{w}");
        assert!(w.contains("Reproject"), "{w}");
    }

    #[test]
    fn csv_and_wkt_warn_about_a_dropped_label() {
        for (target, name) in [(Format::Csv, "CSV"), (Format::Wkt, "WKT")] {
            let w = gda2020().downgrade_warning(target).unwrap();
            assert!(w.contains(name), "{w}");
            assert!(w.contains("EPSG:7844"), "{w}");
            assert!(w.contains("dropped"), "{w}");
        }
    }

    #[test]
    fn label_falls_back_when_fields_are_missing() {
        let code_only =
            Crs::Named(NamedCrs { authority: None, code: Some("3857".into()), ..Default::default() });
        assert!(code_only.downgrade_warning(Format::GeoJson).unwrap().contains("code 3857"));

        let wkt_only = Crs::Named(NamedCrs {
            wkt: Some("PROJCS[..]".into()),
            ..Default::default()
        });
        assert!(wkt_only
            .downgrade_warning(Format::GeoJson)
            .unwrap()
            .contains("the source CRS"));
    }

    // A realistic WKT2 definition (EPSG:7844, GDA2020) with nested ids on the
    // ellipsoid — the extractor must return the *root* CRS id, not a nested one.
    const GDA2020_WKT2: &str = r#"GEOGCRS["GDA2020",
        DATUM["Geocentric Datum of Australia 2020",
            ELLIPSOID["GRS 1980",6378137,298.257222101,
                LENGTHUNIT["metre",1],
                ID["EPSG",7019]]],
        CS[ellipsoidal,2],
            AXIS["geodetic latitude (Lat)",north],
            AXIS["geodetic longitude (Lon)",east],
            ANGLEUNIT["degree",0.0174532925199433],
        ID["EPSG",7844]]"#;

    // The WKT1 spelling of the same CRS: several `AUTHORITY` nodes at varying
    // depths (spheroid, datum, primem, unit) plus the root one.
    const GDA2020_WKT1: &str = r#"GEOGCS["GDA2020",DATUM["GDA2020",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6283"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","7844"]]"#;

    #[test]
    fn extracts_root_id_from_wkt2() {
        assert_eq!(wkt_authority_code(GDA2020_WKT2), Some(("EPSG".into(), "7844".into())));
    }

    #[test]
    fn extracts_root_authority_from_wkt1() {
        // Not the spheroid's 7019, the datum's 6283, or the unit's 9122.
        assert_eq!(wkt_authority_code(GDA2020_WKT1), Some(("EPSG".into(), "7844".into())));
    }

    #[test]
    fn no_id_in_wkt_yields_none() {
        assert_eq!(wkt_authority_code("GEOGCRS[\"anonymous\",DATUM[\"d\"]]"), None);
        assert_eq!(wkt_authority_code(""), None);
    }

    #[test]
    fn alphanumeric_code_is_captured() {
        // The code is a string, so a non-numeric authority code (e.g. OGC:CRS84,
        // or an IGNF-flavored id) round-trips just like a numeric one — it is no
        // longer dropped.
        assert_eq!(
            wkt_authority_code("GEOGCRS[\"x\",ID[\"OGC\",\"CRS84\"]]"),
            Some(("OGC".into(), "CRS84".into()))
        );
    }

    #[test]
    fn from_authority_code_recovers_a_wkt_only_crs() {
        // Neither authority nor code supplied, only WKT: the id is lifted so the
        // identity becomes portable to every authority+code target.
        match Crs::from_authority_code(None, None, Some(GDA2020_WKT2.into()), None) {
            Crs::Named(n) => {
                assert_eq!(n.authority.as_deref(), Some("EPSG"));
                assert_eq!(n.code.as_deref(), Some("7844"));
                assert!(n.wkt.is_some()); // the definition is still carried through
            }
            other => panic!("expected Named EPSG:7844, got {other:?}"),
        }
    }

    #[test]
    fn wkt_only_wgs84_collapses_to_the_default() {
        // A WKT-only definition whose lifted id is EPSG:4326 still collapses to
        // the shared WGS 84 default rather than a Named CRS.
        let wkt = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],AUTHORITY[\"EPSG\",\"4326\"]]";
        assert_eq!(Crs::from_authority_code(None, None, Some(wkt.into()), None), Crs::Wgs84);
    }

    #[test]
    fn wkt_only_ogc_crs84_collapses_to_the_default() {
        // The OGC spelling of WGS 84 lon/lat is the alphanumeric code CRS84, not
        // a numeric 4326 — this only collapses correctly now that the code is a
        // string the WGS 84 check can compare against "CRS84" directly.
        let wkt = "GEOGCRS[\"WGS 84 (CRS84)\",DATUM[\"World Geodetic System 1984\",ELLIPSOID[\"WGS 84\",6378137,298.257223563]],ID[\"OGC\",\"CRS84\"]]";
        assert_eq!(Crs::from_authority_code(None, None, Some(wkt.into()), None), Crs::Wgs84);
    }

    #[test]
    fn supplied_authority_code_is_not_overridden_by_wkt() {
        // When the caller already knows the code, the WKT is left as-is (even a
        // mismatched embedded id does not override the explicit pair).
        match Crs::from_authority_code(
            Some("EPSG".into()),
            Some("3857".into()),
            Some(GDA2020_WKT2.into()),
            None,
        ) {
            Crs::Named(n) => assert_eq!(n.code.as_deref(), Some("3857")),
            other => panic!("expected Named EPSG:3857, got {other:?}"),
        }
    }
}
