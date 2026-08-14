//! CRS resolution against the embedded registry.
//!
//! The registry *data* (proj.db-derived CRS definitions + a name index) lives in
//! the sibling `geosetta-crs-data` crate, isolated so its third-party dataset
//! terms don't attach to the core crate. Decompression (zstd) and lookup happen
//! *here*, using our own [`crate::compress`] codecs — so no runtime dependency is
//! introduced beyond the static data blob.
//!
//! The wire format decoded here (`GCR1`) is specified in
//! `../../geosetta-crs-data/registry-format.org`; read that alongside this file.
//! The image is `(authority, code) -> {PROJJSON, WKT1, WKT2}`, sorted by
//! `(auth_id, code_bytes)` so lookup is a binary search + slice — no per-entry
//! allocation, no map build.
//!
//! # Status: R1, R2, R5 done (R3/R4 are consumers of R1 in the GeoParquet and
//! # Shapefile spokes, not further work here)
//! [`def_projjson`] / [`def_wkt`] resolve a trusted `(authority, code)` to its
//! authoritative definition (R1). [`resolve_geographic_by_name`] and
//! [`resolve_projected_by_name`] add R2's "name → code" identity recovery —
//! an id-less WKT's outer name is looked up in [`geosetta_crs_data::NAMES`]
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

use std::sync::OnceLock;

use super::{tokenize_wkt, WktTok};
use crate::json::JsonValue;

/// Number of CRS definitions available from the embedded registry (0 until the
/// generator populates `geosetta-crs-data`).
pub(crate) fn embedded_crs_count() -> usize {
    geosetta_crs_data::CRS_COUNT
}

const MAGIC: &[u8; 4] = b"GCR1";
const HEADER_SIZE: usize = 64;
/// v2 (R5) 32-byte record: v1's 24 bytes plus `wkt2_off`/`wkt2_len`, with the
/// v1 `reserved` byte repurposed as `has_wkt2` — see `registry-format.org` §
/// V1 → V2. Only v2 blobs are produced or read; there was never a released v1
/// blob to keep parsing.
const RECORD_SIZE: usize = 32;
/// Highest `format_version` this reader understands. A blob stamped higher
/// declines cleanly (`None`) rather than partially reading — forward-compat per
/// `registry-format.org` § VERSIONING.
const MAX_FORMAT_VERSION: u16 = 2;

fn u16_at(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn u32_at(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Parsed header offsets (all byte offsets from the start of the decoded image).
struct Header {
    entry_count: usize,
    auth_count: usize,
    authorities_off: usize,
    index_off: usize,
    keys_off: usize,
    payload_off: usize,
    versions_off: usize,
}

fn parse_header(image: &[u8]) -> Option<Header> {
    if image.len() < HEADER_SIZE || &image[0..4] != MAGIC {
        return None;
    }
    if u16_at(image, 4) > MAX_FORMAT_VERSION {
        return None;
    }
    Some(Header {
        entry_count: u32_at(image, 8) as usize,
        auth_count: u16_at(image, 12) as usize,
        authorities_off: u32_at(image, 16) as usize,
        index_off: u32_at(image, 20) as usize,
        keys_off: u32_at(image, 24) as usize,
        payload_off: u32_at(image, 28) as usize,
        versions_off: u32_at(image, 32) as usize,
    })
}

/// One decoded index record — see `registry-format.org` § Index.
struct Record {
    auth_id: u8,
    key_off: usize,
    key_len: usize,
    has_wkt: bool,
    has_wkt2: bool,
    proj_off: usize,
    proj_len: usize,
    wkt_off: usize,
    wkt_len: usize,
    wkt2_off: usize,
    wkt2_len: usize,
}

fn parse_record(buf: &[u8]) -> Record {
    Record {
        auth_id: buf[0],
        key_len: buf[1] as usize,
        has_wkt: buf[2] != 0,
        has_wkt2: buf[3] != 0,
        key_off: u32_at(buf, 4) as usize,
        proj_off: u32_at(buf, 8) as usize,
        proj_len: u32_at(buf, 12) as usize,
        wkt_off: u32_at(buf, 16) as usize,
        wkt_len: u32_at(buf, 20) as usize,
        wkt2_off: u32_at(buf, 24) as usize,
        wkt2_len: u32_at(buf, 28) as usize,
    }
}

/// The decoded `GCR1` image plus its parsed header. Lookups slice directly into
/// `image`; nothing is copied or allocated per query.
struct Registry {
    image: Vec<u8>,
    header: Header,
}

impl Registry {
    fn record(&self, i: usize) -> Record {
        let off = self.header.index_off + i * RECORD_SIZE;
        parse_record(&self.image[off..off + RECORD_SIZE])
    }

    fn key_bytes(&self, rec: &Record) -> &[u8] {
        let start = self.header.keys_off + rec.key_off;
        &self.image[start..start + rec.key_len]
    }

    fn slice_str(&self, off: usize, len: usize) -> &str {
        let start = self.header.payload_off + off;
        // Generator guarantees UTF-8 (`registry-format.org` § LOOKUP ALGORITHM
        // step 5); trust it rather than paying a validation pass per lookup.
        std::str::from_utf8(&self.image[start..start + len]).unwrap_or_default()
    }

    /// Authority string -> its `auth_id` (position in the authority table).
    /// Linear scan; the table has ~10 entries.
    fn lookup_authority(&self, auth: &str) -> Option<u8> {
        let mut p = self.header.authorities_off;
        for id in 0..self.header.auth_count {
            let len = self.image[p] as usize;
            p += 1;
            if &self.image[p..p + len] == auth.as_bytes() {
                return Some(id as u8);
            }
            p += len;
        }
        None
    }

    /// `auth_id` -> its authority string. Same linear scan, the other direction.
    fn authority_name(&self, auth_id: u8) -> Option<&str> {
        let mut p = self.header.authorities_off;
        for id in 0..self.header.auth_count {
            let len = self.image[p] as usize;
            p += 1;
            if id as u8 == auth_id {
                return std::str::from_utf8(&self.image[p..p + len]).ok();
            }
            p += len;
        }
        None
    }

    /// Binary search the sorted `(auth_id, code_bytes)` index. Comparison order
    /// matches the generator's sort key exactly (`registry-format.org` § Index).
    fn find(&self, auth: &str, code: &str) -> Option<Record> {
        let auth_id = self.lookup_authority(auth)?;
        let code_b = code.as_bytes();
        let (mut lo, mut hi) = (0usize, self.header.entry_count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let rec = self.record(mid);
            let key = (rec.auth_id, self.key_bytes(&rec));
            match key.cmp(&(auth_id, code_b)) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(rec),
            }
        }
        None
    }
}

/// Decode the embedded blob once (only when the registry is actually used) and
/// parse its header. `None` if the sibling crate has no data (generator hasn't
/// run) or the image fails to validate — callers degrade to the structural path.
fn registry() -> Option<&'static Registry> {
    static REGISTRY: OnceLock<Option<Registry>> = OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let zstd = geosetta_crs_data::REGISTRY_BLOB_ZSTD;
            if zstd.is_empty() {
                return None;
            }
            let image =
                crate::compress::zstd::decompress(zstd, geosetta_crs_data::REGISTRY_BLOB_RAW_SIZE)
                    .ok()?;
            let header = parse_header(&image)?;
            Some(Registry { image, header })
        })
        .as_ref()
}

/// The authoritative PROJJSON for `(authority, code)` — e.g. `("EPSG", "3857")`.
/// Always present when the pair resolves (PROJJSON is never omitted). `None` if
/// the registry is unavailable or the pair isn't in it.
pub(crate) fn def_projjson(auth: &str, code: &str) -> Option<&'static str> {
    let reg = registry()?;
    let rec = reg.find(auth, code)?;
    Some(reg.slice_str(rec.proj_off, rec.proj_len))
}

/// The authoritative GDAL-flavor WKT1 for `(authority, code)`. `None` if the
/// registry is unavailable, the pair isn't in it, or the ~4% of CRSes (datum
/// ensembles, dynamic/compound) that have no faithful WKT1 representation
/// (`has_wkt=0`); [`def_wkt2`] covers that gap instead.
pub(crate) fn def_wkt(auth: &str, code: &str) -> Option<&'static str> {
    let reg = registry()?;
    let rec = reg.find(auth, code)?;
    rec.has_wkt.then(|| reg.slice_str(rec.wkt_off, rec.wkt_len))
}

/// The authoritative WKT2:2019 for `(authority, code)` (R5). `None` if the
/// registry is unavailable or the pair isn't in it — but essentially never
/// `None` for a pair that *is* in it: WKT2:2019 can express everything
/// PROJJSON can, so it's present for effectively every entry, *including* all
/// of `def_wkt`'s `has_wkt=0` gap (datum ensembles, dynamic CRSes, some
/// compound) — verified per-generation by the oracle, not assumed to hold
/// forever as `proj.db` grows.
pub(crate) fn def_wkt2(auth: &str, code: &str) -> Option<&'static str> {
    let reg = registry()?;
    let rec = reg.find(auth, code)?;
    rec.has_wkt2.then(|| reg.slice_str(rec.wkt2_off, rec.wkt2_len))
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

/// The contiguous run of `geosetta_crs_data::NAMES` entries sharing `name`.
/// `NAMES` is sorted by `(name, authority, code)`
/// (`geosetta-crs-data/src/names.rs`), so its bounds are two binary searches
/// instead of a full scan of all ~20k entries per resolution.
fn names_matching(name: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    let names = geosetta_crs_data::NAMES;
    let lo = names.partition_point(|(n, _, _)| *n < name);
    let hi = lo + names[lo..].partition_point(|(n, _, _)| *n == name);
    &names[lo..hi]
}

/// Resolve a geographic CRS's `(authority, code)` from the outer name of a
/// WKT definition, validated against the WKT's own ellipsoid before snapping
/// — R2's "name → code" recovery (`crs-registry.org` § Identity recovery,
/// step 2), scoped to geographic CRSes (see the module doc). Multiple
/// authorities can share a name (e.g. an Esri alias matching an EPSG official
/// name); candidates are tried in `NAMES`' generation order (sorted by
/// `(name, authority, code)`, so EPSG generally wins ties) and the first one
/// whose own ellipsoid matches wins. Returns the authoritative PROJJSON on a
/// validated match; `None` if the WKT has no extractable name/ellipsoid, no
/// candidate exists, or no candidate validates.
pub(crate) fn resolve_geographic_by_name(wkt: &str) -> Option<&'static str> {
    let toks = tokenize_wkt(wkt);
    let name = wkt_crs_name_toks(&toks)?;
    let (wkt_a, wkt_rf) = wkt_ellipsoid_params_toks(&toks)?;
    names_matching(&name)
        .iter()
        .find_map(|(_, auth, code)| {
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
    names_matching(&name)
        .iter()
        .find_map(|(_, auth, code)| {
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

    #[test]
    fn resolves_non_epsg_authorities() {
        // String codes (IGNF/OGC/PROJ/NKG) round-trip through the same path as
        // numeric-looking EPSG/ESRI codes — the key is always compared as bytes.
        let ognf = def_projjson("IGNF", "LAMB93").expect("IGNF:LAMB93 present");
        assert!(ognf.contains("\"code\":\"LAMB93\""), "{ognf}");

        let crs84 = def_projjson("OGC", "CRS84").expect("OGC:CRS84 present");
        assert!(crs84.contains("\"code\":\"CRS84\""), "{crs84}");

        let esri = def_projjson("ESRI", "102100").expect("ESRI:102100 present");
        assert!(esri.contains("\"code\":102100") || esri.contains("\"code\":\"102100\""), "{esri}");
    }

    #[test]
    fn unknown_authority_or_code_declines() {
        assert_eq!(def_projjson("NOPE", "1"), None);
        assert_eq!(def_projjson("EPSG", "999999999"), None);
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

    // Real `projinfo -o WKT1_ESRI` export, ESRI:102057 (`geosetta-crs-data/
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

    // The R2 name-recovery bulk oracle: every native ESRI geographic 2D CRS in
    // proj.db (431), exported as real id-less `WKT1_ESRI` text (generated by
    // `geosetta-crs-data/tools/gen_esri_geographic_fixtures.py`), fed through
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
    // `geosetta-crs-data/tools/gen_esri_projected_fixtures.py`). Same hard
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

    #[test]
    fn wkt_present_and_absent_are_distinguishable() {
        // Every entry has PROJJSON; only ~96% have WKT1 (has_wkt=0 for datum
        // ensembles / dynamic / compound CRSes). def_wkt must return None for
        // those, not an empty string, and Some for the rest.
        let reg = registry().expect("registry present");
        let (mut saw_present, mut saw_absent) = (false, false);
        for i in 0..reg.header.entry_count {
            if saw_present && saw_absent {
                break;
            }
            let rec = reg.record(i);
            let auth = reg.authority_name(rec.auth_id).expect("known auth_id");
            let code = std::str::from_utf8(reg.key_bytes(&rec)).expect("utf8 code");
            if rec.has_wkt && !saw_present {
                assert!(def_wkt(auth, code).is_some_and(|w| !w.is_empty()));
                saw_present = true;
            } else if !rec.has_wkt && !saw_absent {
                assert_eq!(def_wkt(auth, code), None);
                saw_absent = true;
            }
        }
        assert!(saw_present && saw_absent, "expected both WKT-present and WKT-absent entries");
    }

    #[test]
    fn def_wkt2_covers_the_has_wkt_zero_gap() {
        // R5's actual point: EPSG:4979 (WGS 84, Geographic 3D) has no WKT1 —
        // `projinfo` itself declines ("WKT1 does not support Geographic 3D
        // CRS"), so `def_wkt` returns None for it — but WKT2:2019 can express
        // it, so `def_wkt2` must return Some.
        assert_eq!(def_wkt("EPSG", "4979"), None, "sanity: still has_wkt=0");
        let wkt2 = def_wkt2("EPSG", "4979").expect("EPSG:4979 has WKT2");
        assert!(wkt2.contains("ID[\"EPSG\",4979]"), "{wkt2}");
    }

    #[test]
    fn def_wkt2_resolves_a_present_entry() {
        let wkt2 = def_wkt2("EPSG", "3857").expect("EPSG:3857 has WKT2");
        assert!(wkt2.starts_with("PROJCRS[") || wkt2.starts_with("PROJCS["), "{wkt2}");
        assert!(wkt2.contains("ID[\"EPSG\",3857]"), "{wkt2}");
    }

    #[test]
    fn def_wkt2_unknown_declines() {
        assert_eq!(def_wkt2("NOPE", "1"), None);
        assert_eq!(def_wkt2("EPSG", "999999999"), None);
    }

    #[test]
    fn format_self_check() {
        // Guards the embed/decode pipeline itself: magic, version, declared
        // entry count, index sortedness, and every (off, len) in-bounds.
        let reg = registry().expect("registry present");
        assert_eq!(reg.header.entry_count, geosetta_crs_data::CRS_COUNT);

        let payload_end = reg.header.versions_off - reg.header.payload_off;
        let mut prev: Option<(u8, Vec<u8>)> = None;
        for i in 0..reg.header.entry_count {
            let rec = reg.record(i);
            let key = (rec.auth_id, reg.key_bytes(&rec).to_vec());
            if let Some(p) = &prev {
                assert!(*p <= key, "index not sorted at entry {i}");
            }
            prev = Some(key);

            assert!(rec.proj_len > 0, "entry {i}: empty PROJJSON");
            assert!(rec.proj_off + rec.proj_len <= payload_end, "entry {i}: PROJJSON out of bounds");
            if rec.has_wkt {
                assert!(rec.wkt_off + rec.wkt_len <= payload_end, "entry {i}: WKT1 out of bounds");
            } else {
                assert_eq!(rec.wkt_len, 0);
            }
            if rec.has_wkt2 {
                assert!(rec.wkt2_off + rec.wkt2_len <= payload_end, "entry {i}: WKT2 out of bounds");
            } else {
                assert_eq!(rec.wkt2_len, 0);
            }
            assert!(reg.authority_name(rec.auth_id).is_some(), "entry {i}: unknown auth_id");
        }
    }

    /// Entries where `projinfo --identify` cannot return `100 %` for reasons
    /// proven to be limitations of `projinfo` itself, not the registry data or
    /// pipeline — verified by feeding `projinfo`'s own unmodified, unstripped
    /// export of each code back into `--identify` and observing the identical
    /// result. Two classes, found by the first full run of this oracle:
    ///
    /// - `EPSG` `EngineeringCRS` (15/15 of that type, all of them): `--identify`
    ///   returns *zero* matches for this CRS type categorically.
    /// - `ESRI` UTM-zone / Alaska-system codes that are exact numeric duplicates
    ///   of an EPSG CRS: `--identify` reports the EPSG twin at 70–90 %
    ///   confidence rather than the ESRI code at 100 %.
    ///
    /// A code leaving this list (because a newer PROJ fixes the gap) is fine —
    /// the loop below tolerates over-performance. A code *entering* unexpected
    /// failure, or one on this list unexpectedly passing (so the list has gone
    /// stale), fails the test either way, so this stays an exhaustive guard
    /// against regressions rather than a blanket tolerance.
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

    /// R5's own `KNOWN_IDENTIFY_GAPS` counterpart, for the WKT2:2019 oracle
    /// below. *Not* the same set as `KNOWN_IDENTIFY_GAPS` above — every one of
    /// these 21 codes self-identifies at 100% via its *PROJJSON* (verified
    /// individually, e.g. `ESRI:104009`), so this is a `--identify`
    /// discrepancy specific to *WKT2 serialization*, not a broader `projinfo`
    /// limitation shared with R1's list — the two lists must stay separate,
    /// since merging would make R1's oracle wrongly expect these (PROJJSON-
    /// clean) codes to fail too. Two classes, same shape as R1's: 20 deprecated
    /// ESRI codes reporting 25% (mostly geographic CRSes with a live EPSG/ESRI
    /// successor — `--identify` apparently weighs a WKT2 candidate against
    /// deprecation/duplicate status differently than a PROJJSON candidate);
    /// `ESRI:102113` (`WGS_1984_Web_Mercator`, deprecated) reporting 70% for
    /// its aux-sphere EPSG:3857 twin, the same aux-sphere ambiguity pattern as
    /// R1's ESRI-UTM-duplicate entries. Same exhaustive-allowlist discipline:
    /// a new unexpected failure or a stale (now-passing) entry both fail loud.
    const KNOWN_WKT2_IDENTIFY_GAPS: &[(&str, &str)] = &[
        ("ESRI", "102113"),
        ("ESRI", "104009"),
        ("ESRI", "104125"),
        ("ESRI", "104144"),
        ("ESRI", "104199"),
        ("ESRI", "104256"),
        ("ESRI", "104664"),
        ("ESRI", "37201"),
        ("ESRI", "37208"),
        ("ESRI", "37212"),
        ("ESRI", "37214"),
        ("ESRI", "37216"),
        ("ESRI", "37217"),
        ("ESRI", "37227"),
        ("ESRI", "37229"),
        ("ESRI", "37231"),
        ("ESRI", "37233"),
        ("ESRI", "37234"),
        ("ESRI", "37238"),
        ("ESRI", "37242"),
        ("ESRI", "37253"),
    ];

    // The registry oracle (`crs-registry.org` R1.4): every embedded definition
    // fed through `projinfo --identify` must return its own `(authority, code)`
    // at 100%, except the documented `KNOWN_IDENTIFY_GAPS`. For an authoritative
    // definition 100% holds by construction, so this mainly guards the
    // embed/serialize/zstd-decode pipeline and dataset version drift — not the
    // definitions themselves. Ignored by default: it shells out to PROJ's
    // `projinfo` once per entry (13 790 of them, ~7 minutes).
    #[test]
    #[ignore = "manual: full registry oracle, needs `projinfo` (PROJ) on PATH, ~13.8k invocations"]
    fn bulk_oracle_every_entry_identifies() {
        use std::process::Command;

        let reg = registry().expect("registry present");
        let (mut ok, mut unexpected, mut stale) = (0u32, Vec::new(), Vec::new());
        for i in 0..reg.header.entry_count {
            let rec = reg.record(i);
            let auth = reg.authority_name(rec.auth_id).expect("known auth_id");
            let code = std::str::from_utf8(reg.key_bytes(&rec)).expect("utf8 code");
            let expected_gap = KNOWN_IDENTIFY_GAPS.contains(&(auth, code));

            let out = Command::new("projinfo")
                .arg("--identify")
                .arg(reg.slice_str(rec.proj_off, rec.proj_len))
                .output()
                .expect("run projinfo");
            let text = String::from_utf8_lossy(&out.stdout);
            let matched = text.contains(&format!("{auth}:{code}: 100 %"));

            match (matched, expected_gap) {
                (true, _) => {
                    ok += 1;
                    if expected_gap {
                        stale.push(format!("{auth}:{code}: on KNOWN_IDENTIFY_GAPS but now identifies at 100% — remove it"));
                    }
                }
                (false, true) => {} // documented, tolerated
                (false, false) => {
                    // The confidence line (e.g. "EPSG:26701: 90 %") always
                    // contains '%'; a plain-error message has none, and its
                    // reason lands on stderr, not stdout.
                    let got = text.lines().find(|l| l.contains('%')).map(str::trim).unwrap_or("(no match)");
                    let err = String::from_utf8_lossy(&out.stderr);
                    let err = err.lines().next().unwrap_or("").trim();
                    unexpected.push(format!(
                        "{auth}:{code}: expected 100%, got `{got}`{}",
                        if err.is_empty() { String::new() } else { format!(" (stderr: {err})") }
                    ));
                }
            }
        }
        eprintln!(
            "registry oracle: {ok} identified@100%, {} known gaps, {} unexpected failures, {} stale exceptions",
            KNOWN_IDENTIFY_GAPS.len(),
            unexpected.len(),
            stale.len()
        );
        assert!(
            unexpected.is_empty() && stale.is_empty(),
            "{} unexpected failure(s):\n{}\n{} stale exception(s):\n{}",
            unexpected.len(),
            unexpected.join("\n"),
            stale.len(),
            stale.join("\n"),
        );
    }

    // R5's own full-registry oracle, parallel to `bulk_oracle_every_entry_
    // identifies` but over WKT2:2019 instead of PROJJSON, plus the coverage
    // claim R5 exists to make: every `has_wkt=0` entry (the ~4% WKT1 can't
    // express) must have `has_wkt2=1`. Checks against *both* `KNOWN_IDENTIFY_
    // GAPS` and `KNOWN_WKT2_IDENTIFY_GAPS`: R1's gaps (EngineeringCRS,
    // exact-duplicate ESRI codes) turn out to be `--identify` limitations that
    // hold regardless of serialization, so they fail under WKT2 too (verified
    // by this oracle's first run); `KNOWN_WKT2_IDENTIFY_GAPS` adds the ones
    // that are *specific* to WKT2 (self-identify fine via PROJJSON — see that
    // const's doc comment).
    #[test]
    #[ignore = "manual: full registry WKT2 oracle, needs `projinfo` (PROJ) on PATH, ~13.8k invocations"]
    fn bulk_oracle_every_entry_wkt2_identifies_and_covers_wkt1_gap() {
        use std::process::Command;

        let reg = registry().expect("registry present");
        let (mut ok, mut unexpected, mut stale, mut missing_wkt2) = (0u32, Vec::new(), Vec::new(), Vec::new());
        for i in 0..reg.header.entry_count {
            let rec = reg.record(i);
            let auth = reg.authority_name(rec.auth_id).expect("known auth_id");
            let code = std::str::from_utf8(reg.key_bytes(&rec)).expect("utf8 code");

            if !rec.has_wkt2 {
                // The specific gap R5 exists to close: every entry lacking
                // WKT1 (`has_wkt=0`) must have WKT2 instead. Collected for
                // *all* entries missing WKT2, not just the WKT1-less ones —
                // a broader miss would be even more surprising.
                missing_wkt2.push(format!("{auth}:{code}"));
                continue;
            }

            let expected_gap = KNOWN_IDENTIFY_GAPS.contains(&(auth, code))
                || KNOWN_WKT2_IDENTIFY_GAPS.contains(&(auth, code));
            let out = Command::new("projinfo")
                .arg("--identify")
                .arg(reg.slice_str(rec.wkt2_off, rec.wkt2_len))
                .output()
                .expect("run projinfo");
            let text = String::from_utf8_lossy(&out.stdout);
            let matched = text.contains(&format!("{auth}:{code}: 100 %"));

            match (matched, expected_gap) {
                (true, _) => {
                    ok += 1;
                    if expected_gap {
                        stale.push(format!("{auth}:{code}: on a known-gaps list but now identifies at 100% (WKT2) — remove it"));
                    }
                }
                (false, true) => {} // documented, tolerated
                (false, false) => {
                    let got = text.lines().find(|l| l.contains('%')).map(str::trim).unwrap_or("(no match)");
                    let err = String::from_utf8_lossy(&out.stderr);
                    let err = err.lines().next().unwrap_or("").trim();
                    unexpected.push(format!(
                        "{auth}:{code}: expected 100%, got `{got}`{} (WKT2)",
                        if err.is_empty() { String::new() } else { format!(" (stderr: {err})") }
                    ));
                }
            }
        }
        eprintln!(
            "R5 WKT2 oracle: {ok} identified@100%, {} known gaps, {} unexpected failures, {} stale exceptions, {} missing WKT2 entirely",
            KNOWN_IDENTIFY_GAPS.len() + KNOWN_WKT2_IDENTIFY_GAPS.len(),
            unexpected.len(),
            stale.len(),
            missing_wkt2.len(),
        );
        assert!(
            unexpected.is_empty() && stale.is_empty() && missing_wkt2.is_empty(),
            "{} unexpected failure(s):\n{}\n{} stale exception(s):\n{}\n{} entries with no WKT2 at all (R5's coverage claim broken):\n{}",
            unexpected.len(),
            unexpected.join("\n"),
            stale.len(),
            stale.join("\n"),
            missing_wkt2.len(),
            missing_wkt2.join("\n"),
        );
    }
}
