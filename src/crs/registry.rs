//! CRS resolution against the embedded registry.
//!
//! The registry *data*, `GCR1` decode, and the trusted-id lookup
//! (`(authority, code) -> definition`) live in the sibling `geoscribe` crate
//! behind its public API (`geoscribe::resolve`/`resolve_by_name`) — moved
//! there in R6 (`geoscribe/public-api.org`) so a second consumer (`nazca`)
//! doesn't need private access to this crate's internals. What stays *here*
//! is identity-*recovery* policy: turning an id-less WKT into a trusted
//! `(authority, code)` candidate, and deciding how much to trust it before
//! snapping — that's specific to how this crate reads format-specific
//! inputs, not something the registry crate should own (see
//! `geoscribe/public-api.org` § BOUNDARY).
//!
//! # Status: R1, R2, R5 done (R3/R4 are consumers of R1 in the GeoParquet and
//! # Shapefile spokes, not further work here)
//! [`def_projjson`] / [`def_wkt`] resolve a trusted `(authority, code)` to its
//! authoritative definition (R1), now thin call-throughs to
//! [`geoscribe::resolve`]. [`resolve_geographic_by_name`] and
//! [`resolve_projected_by_name`] add R2's "name → code" identity recovery —
//! an id-less WKT's outer name is looked up via [`geoscribe::resolve_by_name`]
//! and validated against its ellipsoid before snapping (`crs-registry.org` §
//! Validation), for both geographic and projected CRSes. Both stop at the
//! ellipsoid check, not full structural (method/parameter) param-match — that
//! plan's recovery step 3 — since a prior attempt at Esri method/parameter
//! *translation* hit unproven spelling gaps (see `handoff.org` § KEY FINDINGS,
//! "TRIED AND REVERTED"); each resolver's own bulk oracle (over real ESRI
//! fixtures) is the empirical check on whether the ellipsoid-only bar holds at
//! scale. [`def_wkt2`] adds R5's WKT2:2019 emission for CRSes WKT1 can't
//! structurally express (datum ensembles, dynamic CRSes, some compound
//! systems).

#![allow(dead_code)]

use super::{tokenize_wkt, WktTok};
use crate::json::JsonValue;

/// Number of CRS definitions available from the embedded registry (0 until the
/// generator populates `geoscribe`).
pub(crate) fn embedded_crs_count() -> usize {
    geoscribe::CRS_COUNT
}

/// The authoritative PROJJSON for `(authority, code)` — e.g. `("EPSG", "3857")`.
/// Always present when the pair resolves (PROJJSON is never omitted). `None` if
/// the registry is unavailable or the pair isn't in it.
pub(crate) fn def_projjson(auth: &str, code: &str) -> Option<&'static str> {
    geoscribe::resolve(auth, code).map(|r| r.projjson)
}

/// The authoritative GDAL-flavor WKT1 for `(authority, code)`. `None` if the
/// registry is unavailable, the pair isn't in it, or the ~4% of CRSes (datum
/// ensembles, dynamic/compound) that have no faithful WKT1 representation;
/// [`def_wkt2`] covers that gap instead.
pub(crate) fn def_wkt(auth: &str, code: &str) -> Option<&'static str> {
    geoscribe::resolve(auth, code).and_then(|r| r.wkt)
}

/// The authoritative WKT2:2019 for `(authority, code)` (R5). `None` if the
/// registry is unavailable or the pair isn't in it — but essentially never
/// `None` for a pair that *is* in it: WKT2:2019 can express everything
/// PROJJSON can, so it's present for effectively every entry, *including* all
/// of `def_wkt`'s WKT1-less gap (datum ensembles, dynamic CRSes, some
/// compound) — verified per-generation by `geoscribe`'s own oracle, not
/// assumed to hold forever as `proj.db` grows.
pub(crate) fn def_wkt2(auth: &str, code: &str) -> Option<&'static str> {
    geoscribe::resolve(auth, code).and_then(|r| r.wkt2)
}

// --- R2: name → code recovery (geographic + projected CRSes) ---------------

/// Relative tolerance for validating a name/structural match before snapping
/// to it, per `crs-registry.org` § Validation ("relative 1e-6 on values").
/// Applies to recovery methods weaker than a trusted inline id; the id path
/// (`def_projjson` called directly) never uses this.
const SNAP_TOLERANCE: f64 = 1e-6;

fn approx_eq(a: f64, b: f64) -> bool {
    (a - b).abs() <= SNAP_TOLERANCE * a.abs().max(b.abs()).max(1.0)
}

/// The outermost CRS's own name — the first quoted string right after the
/// root keyword, e.g. `"GCS_WGS_1984"` in `GEOGCS["GCS_WGS_1984", ...]`. This
/// is exactly the spelling `proj.db`'s `alias_name` (Esri) or `name` (every
/// other authority) columns carry — what the generator built `NAMES` from —
/// so no normalization is needed before looking it up.
fn wkt_crs_name(wkt: &str) -> Option<String> {
    wkt_crs_name_toks(&tokenize_wkt(wkt))
}

/// [`wkt_crs_name`] over pre-tokenized input, so callers that also need
/// [`wkt_ellipsoid_params_toks`] can tokenize the WKT once and share it.
fn wkt_crs_name_toks(toks: &[WktTok]) -> Option<String> {
    match (toks.first(), toks.get(1), toks.get(2)) {
        (Some(WktTok::Word(_)), Some(WktTok::Open), Some(WktTok::Str(name))) => Some(name.clone()),
        _ => None,
    }
}

/// The CRS's ellipsoid as `(semi_major_axis, inverse_flattening)`, from its
/// first `SPHEROID`/`ELLIPSOID` node (WKT1 and WKT2 spell the leading
/// `name, semi-major axis, inverse flattening` triple identically). This is
/// the only structural fact [`resolve_geographic_by_name`] validates: a name
/// is weaker evidence than a trusted id, so it is confirmed against a real
/// geodetic constant before snapping, not just trusted outright.
fn wkt_ellipsoid_params(wkt: &str) -> Option<(f64, f64)> {
    wkt_ellipsoid_params_toks(&tokenize_wkt(wkt))
}

/// [`wkt_ellipsoid_params`] over pre-tokenized input — see
/// [`wkt_crs_name_toks`].
fn wkt_ellipsoid_params_toks(toks: &[WktTok]) -> Option<(f64, f64)> {
    for i in 0..toks.len() {
        let WktTok::Word(w) = &toks[i] else { continue };
        if !(w.eq_ignore_ascii_case("SPHEROID") || w.eq_ignore_ascii_case("ELLIPSOID")) {
            continue;
        }
        if let (Some(WktTok::Open), Some(WktTok::Str(_)), Some(WktTok::Comma)) =
            (toks.get(i + 1), toks.get(i + 2), toks.get(i + 3))
        {
            let a = match toks.get(i + 4) {
                Some(WktTok::Word(v)) | Some(WktTok::Str(v)) => v.parse::<f64>().ok(),
                _ => None,
            };
            let rf = match toks.get(i + 5) {
                Some(WktTok::Comma) => match toks.get(i + 6) {
                    Some(WktTok::Word(v)) | Some(WktTok::Str(v)) => v.parse::<f64>().ok(),
                    _ => None,
                },
                _ => None,
            };
            if let (Some(a), Some(rf)) = (a, rf) {
                return Some((a, rf));
            }
        }
    }
    None
}

/// A GeographicCRS or ProjectedCRS PROJJSON's ellipsoid as `(semi_major_axis,
/// inverse_flattening)`, from its `datum` (a plain single-realization datum)
/// or `datum_ensemble` (the modern form WGS 84 and similar use — the
/// ellipsoid sits once at the ensemble level, not per-member). A ProjectedCRS
/// nests these under `base_crs` (its underlying GeographicCRS) rather than at
/// the top level, so that's checked first — falling through to the top level
/// covers GeographicCRS callers unchanged. PROJJSON expresses the ellipsoid's
/// flattening either as `inverse_flattening` directly or as `semi_minor_axis`
/// (e.g. Clarke 1866, NAD27's ellipsoid); the latter is converted (`a / (a -
/// b)`) so callers always compare the same quantity.
fn projjson_ellipsoid(pj: &JsonValue) -> Option<(f64, f64)> {
    let base = pj.get("base_crs").unwrap_or(pj);
    let datum = base.get("datum").or_else(|| base.get("datum_ensemble"))?;
    let ellipsoid = datum.get("ellipsoid")?;
    let a = ellipsoid.get("semi_major_axis")?.as_f64()?;
    let rf = match ellipsoid.get("inverse_flattening").and_then(JsonValue::as_f64) {
        Some(rf) => rf,
        None => {
            let b = ellipsoid.get("semi_minor_axis")?.as_f64()?;
            a / (a - b)
        }
    };
    Some((a, rf))
}

/// Resolve a geographic CRS's `(authority, code)` from the outer name of a
/// WKT definition, validated against the WKT's own ellipsoid before snapping
/// — R2's "name → code" recovery (`crs-registry.org` § Identity recovery,
/// step 2), scoped to geographic CRSes (see the module doc). Multiple
/// authorities can share a name (e.g. an Esri alias matching an EPSG official
/// name); candidates are tried in [`geoscribe::resolve_by_name`]'s order
/// (`NAMES` sorted by `(name, authority, code)`, so EPSG generally wins ties)
/// and the first one whose own ellipsoid matches wins. Returns the
/// authoritative PROJJSON on a validated match; `None` if the WKT has no
/// extractable name/ellipsoid, no candidate exists, or no candidate
/// validates.
pub(crate) fn resolve_geographic_by_name(wkt: &str) -> Option<&'static str> {
    let toks = tokenize_wkt(wkt);
    let name = wkt_crs_name_toks(&toks)?;
    let (wkt_a, wkt_rf) = wkt_ellipsoid_params_toks(&toks)?;
    geoscribe::resolve_by_name(&name).find_map(|(auth, code)| {
        let pj = def_projjson(auth, code)?;
        let parsed = crate::json::parse(pj).ok()?;
        if parsed.get("type").and_then(JsonValue::as_str) != Some("GeographicCRS") {
            return None;
        }
        let (reg_a, reg_rf) = projjson_ellipsoid(&parsed)?;
        (approx_eq(wkt_a, reg_a) && approx_eq(wkt_rf, reg_rf)).then_some(pj)
    })
}

/// Resolve a projected CRS's `(authority, code)` from the outer name of a
/// WKT definition, validated against its *base* geographic CRS's ellipsoid
/// before snapping — the projected counterpart of
/// [`resolve_geographic_by_name`]. Deliberately does *not* validate the
/// projection method/parameters (central meridian, false easting, etc.):
/// `crs-registry.org`'s R2 section notes that building a GDAL↔Esri
/// method/parameter crosswalk for validation risks the same spelling gaps
/// that sank the earlier structural-*translation* attempt (`handoff.org` §
/// KEY FINDINGS, "TRIED AND REVERTED"). Instead this leans on the same
/// guarantee the geographic path already relies on: a `NAMES` entry is a
/// catalog name unique to its own `(authority, code)`, so a name match is
/// already strong evidence, and the ellipsoid check catches a same-spelled
/// name landing on the wrong datum. The bulk oracle
/// (`bulk_oracle_esri_projected_name_recovery`) is the empirical check on
/// whether that's enough — if it ever finds a wrong (not just declined)
/// resolution, this needs a real method/parameter check added, not before.
pub(crate) fn resolve_projected_by_name(wkt: &str) -> Option<&'static str> {
    let toks = tokenize_wkt(wkt);
    let name = wkt_crs_name_toks(&toks)?;
    let (wkt_a, wkt_rf) = wkt_ellipsoid_params_toks(&toks)?;
    geoscribe::resolve_by_name(&name).find_map(|(auth, code)| {
        let pj = def_projjson(auth, code)?;
        let parsed = crate::json::parse(pj).ok()?;
        if parsed.get("type").and_then(JsonValue::as_str) != Some("ProjectedCRS") {
            return None;
        }
        let (reg_a, reg_rf) = projjson_ellipsoid(&parsed)?;
        (approx_eq(wkt_a, reg_a) && approx_eq(wkt_rf, reg_rf)).then_some(pj)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_epsg_3857() {
        let pj = def_projjson("EPSG", "3857").expect("EPSG:3857 present");
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":3857}"), "{pj}");
        assert!(crate::json::parse(pj).is_ok(), "{pj}");
    }

    // Real `projinfo -o WKT1_ESRI` exports — id-less, Esri-flavor spellings
    // (`GCS_WGS_1984`, `D_North_American_1983`), exactly the shapefile-`.prj`
    // shape `crs-registry.org` § WHY measured as mis-identifying (WGS 84 70%,
    // NAD83/NAD27 outright wrong) under structural translation alone.
    const ESRI_WGS84: &str = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    const ESRI_NAD83: &str = r#"GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    // Esri's inverse_flattening (294.978698213898) is derived from the same
    // ellipsoid the registry expresses as semi_minor_axis (6356583.8) —
    // exercises the semi_minor_axis -> inverse_flattening conversion path in
    // `projjson_ellipsoid`.
    const ESRI_NAD27: &str = r#"GEOGCS["GCS_North_American_1927",DATUM["D_North_American_1927",SPHEROID["Clarke_1866",6378206.4,294.978698213898]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;

    #[test]
    fn resolve_geographic_by_name_recovers_esri_wgs84() {
        let pj = resolve_geographic_by_name(ESRI_WGS84).expect("resolves to EPSG:4326");
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":4326}"), "{pj}");
    }

    #[test]
    fn resolve_geographic_by_name_recovers_esri_nad83() {
        // Structural translation alone gets this wrong (EPSG:9309, per
        // crs-registry.org); name recovery gets the real code.
        let pj = resolve_geographic_by_name(ESRI_NAD83).expect("resolves to EPSG:4269");
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":4269}"), "{pj}");
    }

    #[test]
    fn resolve_geographic_by_name_recovers_esri_nad27() {
        // Structural translation alone gets this wrong (EPSG:4169, per
        // crs-registry.org); name recovery gets the real code.
        let pj = resolve_geographic_by_name(ESRI_NAD27).expect("resolves to EPSG:4267");
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":4267}"), "{pj}");
    }

    #[test]
    fn resolve_geographic_by_name_rejects_a_lying_name() {
        // Same name as WGS 84, but a fabricated ellipsoid that matches no real
        // registry entry: the name is weaker evidence than an id, so a
        // mismatched structure must not snap — strict fallback, never guess.
        let lying = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6300000.0,290.0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
        assert_eq!(resolve_geographic_by_name(lying), None);
    }

    #[test]
    fn resolve_geographic_by_name_declines_an_unknown_name() {
        let wkt = r#"GEOGCS["Totally Made Up Datum XYZ",DATUM["d",SPHEROID["e",6378137.0,298.257223563]]]"#;
        assert_eq!(resolve_geographic_by_name(wkt), None);
    }

    // Real `projinfo -o WKT1_ESRI` export, ESRI:102057 (`geoscribe/
    // tools/gen_esri_projected_fixtures.py`). ESRI:102057 itself is
    // deprecated (`proj.db`) with EPSG:6339 as its live replacement sharing
    // the same Esri-style catalog name — the same "exact numeric duplicate"
    // ambiguity the R1 oracle documented for geographic (`handoff.org` § KEY
    // FINDINGS), here for projected. `NAMES`' tie-break (sorted by `(name,
    // authority, code)`, EPSG generally wins) resolves to the live EPSG twin
    // rather than the deprecated ESRI code — both self-identify at 100%, so
    // either would pass the bulk oracle's hard bar, but EPSG:6339 is the
    // better pick to actually snap to.
    const ESRI_NAD83_2011_UTM10N: &str = r#"PROJCS["NAD_1983_2011_UTM_Zone_10N",GEOGCS["GCS_NAD_1983_2011",DATUM["D_NAD_1983_2011",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-123.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;

    #[test]
    fn resolve_projected_by_name_recovers_esri_nad83_2011_utm10n() {
        let pj = resolve_projected_by_name(ESRI_NAD83_2011_UTM10N).expect("resolves to EPSG:6339");
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":6339}"), "{pj}");
    }

    #[test]
    fn resolve_projected_by_name_rejects_a_lying_ellipsoid() {
        // Same name, but a fabricated base ellipsoid matching no real entry —
        // must not snap, same strict-fallback discipline as the geographic path.
        let lying = r#"PROJCS["NAD_1983_2011_UTM_Zone_10N",GEOGCS["GCS_NAD_1983_2011",DATUM["D_NAD_1983_2011",SPHEROID["GRS_1980",6300000.0,290.0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-123.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
        assert_eq!(resolve_projected_by_name(lying), None);
    }

    #[test]
    fn resolve_projected_by_name_declines_an_unknown_name() {
        let wkt = r#"PROJCS["Totally Made Up Projection XYZ",GEOGCS["g",DATUM["d",SPHEROID["e",6378137.0,298.257223563]]],PROJECTION["Transverse_Mercator"]]"#;
        assert_eq!(resolve_projected_by_name(wkt), None);
    }

    #[test]
    fn wkt_ellipsoid_params_reads_the_first_spheroid() {
        assert_eq!(wkt_ellipsoid_params(ESRI_WGS84), Some((6378137.0, 298.257223563)));
        assert_eq!(wkt_ellipsoid_params("GEOGCRS[\"x\"]"), None);
    }

    #[test]
    fn wkt_crs_name_reads_the_outer_name() {
        assert_eq!(wkt_crs_name(ESRI_WGS84).as_deref(), Some("GCS_WGS_1984"));
        assert_eq!(wkt_crs_name(""), None);
    }

    /// Entries where `projinfo --identify` cannot return `100 %` for reasons
    /// proven to be limitations of `projinfo` itself, not the registry data or
    /// pipeline. **Duplicated from `geoscribe/src/registry.rs`'s own
    /// `KNOWN_IDENTIFY_GAPS`** (same list, same provenance — see that file's
    /// doc comment for the two classes it covers) because
    /// `bulk_oracle_esri_projected_name_recovery` below needs it and it's
    /// `cfg(test)`-private to `geoscribe`'s own test binary, not something a
    /// dependent crate can import. Keep the two lists in sync by hand if
    /// either changes — same documented-duplication discipline as this
    /// crate's own copy of the zstd decoder (`geoscribe/public-api.org` §
    /// MODULE LAYOUT).
    const KNOWN_IDENTIFY_GAPS: &[(&str, &str)] = &[
        ("EPSG", "5800"),
        ("EPSG", "5801"),
        ("EPSG", "5802"),
        ("EPSG", "5803"),
        ("EPSG", "5808"),
        ("EPSG", "5809"),
        ("EPSG", "5810"),
        ("EPSG", "5811"),
        ("EPSG", "5812"),
        ("EPSG", "5813"),
        ("EPSG", "5814"),
        ("EPSG", "5815"),
        ("EPSG", "5816"),
        ("EPSG", "5817"),
        ("EPSG", "6715"),
        ("ESRI", "102124"),
        ("ESRI", "102125"),
        ("ESRI", "102126"),
        ("ESRI", "102127"),
        ("ESRI", "102128"),
        ("ESRI", "102129"),
        ("ESRI", "102130"),
        ("ESRI", "102131"),
        ("ESRI", "102570"),
        ("ESRI", "102571"),
        ("ESRI", "102572"),
        ("ESRI", "102573"),
        ("ESRI", "102574"),
        ("ESRI", "102575"),
        ("ESRI", "102576"),
        ("ESRI", "102577"),
        ("ESRI", "102578"),
        ("ESRI", "102579"),
        ("ESRI", "102580"),
    ];

    // The R2 name-recovery bulk oracle: every native ESRI geographic 2D CRS in
    // proj.db (431), exported as real id-less `WKT1_ESRI` text (generated by
    // `geoscribe/tools/gen_esri_geographic_fixtures.py`), fed through
    // `resolve_geographic_by_name`. The hard bar (matching the project's
    // strict-fallback convention, `crs-registry.org` § Validation): *anything
    // resolved must self-identify at 100%* via `projinfo --identify` — a wrong
    // snap is never acceptable. Coverage (resolved vs. declined) is reported
    // but not asserted on a specific bar: declining is the correct, safe
    // outcome whenever validation can't confirm a match, not a failure.
    // Ignored by default: shells out to `projinfo` once per resolved entry.
    #[test]
    #[ignore = "manual: R2 name-recovery bulk oracle over 431 native ESRI geographic CRSes, needs `projinfo` (PROJ) on PATH"]
    fn bulk_oracle_esri_geographic_name_recovery() {
        use std::process::Command;
        let data = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/esri_geographic_wkt1.tsv"
        ))
        .expect("fixtures present");

        let (mut resolved, mut declined, mut wrong) = (0u32, 0u32, Vec::new());
        for line in data.lines().filter(|l| !l.trim().is_empty()) {
            let (esri_code, wkt) = line.split_once('\t').expect("code<TAB>wkt");
            let Some(pj) = resolve_geographic_by_name(wkt) else {
                declined += 1;
                continue;
            };
            let out = Command::new("projinfo")
                .arg("--identify")
                .arg(pj)
                .output()
                .expect("run projinfo");
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains(": 100 %") {
                resolved += 1;
            } else {
                let got = text.lines().find(|l| l.contains('%')).unwrap_or("(no match)");
                wrong.push(format!("ESRI:{esri_code}: resolved but didn't self-identify at 100%: {}", got.trim()));
            }
        }
        eprintln!(
            "R2 geographic name-recovery oracle: {resolved} resolved@100%, {declined} declined (no validated match), {} wrong",
            wrong.len()
        );
        assert!(
            wrong.is_empty(),
            "{} resolved-but-wrong (a snap that doesn't self-identify — must never happen):\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }

    // Projected counterpart to `bulk_oracle_esri_geographic_name_recovery`,
    // over `esri_projected_wkt1.tsv` (all 2 274 native ESRI projected CRSes,
    // real id-less `WKT1_ESRI` exports —
    // `geoscribe/tools/gen_esri_projected_fixtures.py`). Same hard
    // bar: zero wrong, resolution rate reported not asserted.
    // `resolve_projected_by_name` validates only the base ellipsoid, not the
    // projection method/parameters (see its doc comment) — this oracle is
    // what actually checks whether that's sufficient.
    #[test]
    #[ignore = "manual: R2 name-recovery bulk oracle over 2274 native ESRI projected CRSes, needs `projinfo` (PROJ) on PATH"]
    fn bulk_oracle_esri_projected_name_recovery() {
        use std::process::Command;
        let data = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/esri_projected_wkt1.tsv"
        ))
        .expect("fixtures present");

        let (mut resolved, mut declined, mut known_gap, mut wrong) = (0u32, 0u32, 0u32, Vec::new());
        for line in data.lines().filter(|l| !l.trim().is_empty()) {
            let (esri_code, wkt) = line.split_once('\t').expect("code<TAB>wkt");
            let Some(pj) = resolve_projected_by_name(wkt) else {
                declined += 1;
                continue;
            };
            // The resolved candidate's *own* (authority, code) — not
            // necessarily ESRI:{esri_code}, since `NAMES` can tie-break to a
            // live EPSG twin of a deprecated ESRI entry (same as R1's "exact
            // numeric duplicate" finding, see the unit test above). Used to
            // check against `KNOWN_IDENTIFY_GAPS` below: R1's full-registry
            // oracle already proved these specific codes are `projinfo
            // --identify` limitations, not registry defects, so a resolution
            // landing on one of them isn't a *new* finding here.
            let parsed = crate::json::parse(pj).expect("registry PROJJSON parses");
            let cand_auth = parsed.get("id").and_then(|id| id.get("authority")).and_then(JsonValue::as_str);
            let cand_code = parsed.get("id").and_then(|id| id.get("code"));
            let cand_code = cand_code
                .and_then(JsonValue::as_str)
                .map(str::to_string)
                .or_else(|| cand_code.and_then(JsonValue::as_f64).map(|c| format!("{c:.0}")));
            let expected_gap = matches!(
                (cand_auth, cand_code.as_deref()),
                (Some(a), Some(c)) if KNOWN_IDENTIFY_GAPS.contains(&(a, c))
            );

            let out = Command::new("projinfo")
                .arg("--identify")
                .arg(pj)
                .output()
                .expect("run projinfo");
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains(": 100 %") {
                resolved += 1;
            } else if expected_gap {
                known_gap += 1;
            } else {
                let got = text.lines().find(|l| l.contains('%')).unwrap_or("(no match)");
                wrong.push(format!("ESRI:{esri_code}: resolved but didn't self-identify at 100%: {}", got.trim()));
            }
        }
        eprintln!(
            "R2 projected name-recovery oracle: {resolved} resolved@100%, {declined} declined (no validated match), {known_gap} known projinfo --identify gaps, {} wrong",
            wrong.len()
        );
        assert!(
            wrong.is_empty(),
            "{} resolved-but-wrong (a snap that doesn't self-identify — must never happen):\n{}",
            wrong.len(),
            wrong.join("\n")
        );
    }
}
