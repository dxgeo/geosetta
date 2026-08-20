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
//! CLI emits rather than silently mislabeling or dropping the CRS. It returns
//! the message *body* only — `main.rs`'s `print_warnings` adds the `warning: `
//! prefix for every check alike, so no individual check can drift from it.

use crate::error::{Error, Result};
use crate::format::Format;
use crate::json::JsonValue;

mod wkt1_tables;
mod wkt_projjson;
pub(crate) use wkt_projjson::{projjson_to_wkt, wkt_to_projjson};

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
    /// Verbatim PROJJSON definition, if the source recorded one — the source's
    /// own bytes, not a re-serialization: indentation, member order, and number
    /// spelling all survive, since geosetta never interprets a definition and
    /// so has no business restyling one. `--print-crs` rests on this.
    pub projjson: Option<String>,
}

/// The canonical `AUTHORITY:CODE` spelling reported for [`Crs::Wgs84`].
///
/// The variant is fieldless by design — `EPSG:4326` and `OGC:CRS84` both
/// collapse into it in [`Crs::from_authority_code`] — so reporting an identity
/// for it means *choosing* a spelling. `OGC:CRS84` is the one that matches what
/// the variant actually asserts: WGS 84 in longitude/latitude order (`EPSG:4326`
/// is authoritatively latitude/longitude). It also round-trips to a no-op —
/// resolve it with some external tool, feed the definition back through `--crs`,
/// and [`Crs::from_authority_code`]'s id lift collapses it straight back to
/// [`Crs::Wgs84`].
pub const WGS84_CRS_CODE: &str = "OGC:CRS84";

/// The organization name a format writes when a CRS has no real authority —
/// GeoPackage's `gpkg_spatial_ref_sys` convention, and what
/// `geopackage::writer` emits alongside a synthetic `srs_id`.
///
/// It is a placeholder, not an authority: no registry has ever issued a code
/// under it. [`NamedCrs::authority_code`] treats it as absent so a synthesized
/// pair is never reported as a resolvable identity.
pub(crate) const SYNTHETIC_AUTHORITY: &str = "NONE";

impl Crs {
    /// Interpret an authority + code as a [`Crs`], collapsing the well-known
    /// WGS 84 geographic spellings (`EPSG:4326`, `OGC:CRS84`) to [`Crs::Wgs84`]
    /// so every format renders the default consistently. Anything else becomes
    /// a [`Crs::Named`] carrying the given fields.
    ///
    /// When a rich-format source supplies *neither* an authority nor a code but
    /// *does* carry a WKT definition, the CRS's own `AUTHORITY`/`ID` node is
    /// lifted out of the WKT (see `wkt_authority_code`) and used as the
    /// authority+code — the same lexical move PROJJSON's `id` gets in
    /// `parquet::geo::parse_crs`. That makes the identity portable to
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

    /// Parse a CRS definition supplied as *text* — the `--crs` override.
    ///
    /// Geosetta neither produces nor fetches this text and knows nothing about
    /// what did: PROJ, GDAL, a web service, a registry crate, or a definition
    /// typed by hand are all the same to it, and it never runs any of them (see
    /// `plans/crs-external-resolution.org` for why that is a hard rule). Its job
    /// begins and ends at "accept valid WKT or PROJJSON and use it".
    ///
    /// The dialect is decided by the first non-whitespace byte — `{` is
    /// PROJJSON, anything else must be WKT — and then handed to the parser that
    /// already exists for it; there is no new parsing here. The recovered
    /// identity goes through [`Crs::from_authority_code`] exactly like a CRS
    /// read from a file, so an override spelling `EPSG:4326` collapses to
    /// [`Crs::Wgs84`] just as a source's own would.
    ///
    /// Strict, per the crate's standing convention: text that is neither dialect
    /// is an error, never a silently ignored override.
    pub fn from_definition_text(text: &str) -> Result<Crs> {
        let text = text.trim();
        match text.as_bytes().first() {
            None => Err(Error::Usage(
                "--crs: the CRS definition is empty; expected WKT or PROJJSON text".into(),
            )),
            Some(b'{') => crs_from_projjson_text(text),
            Some(_) if is_crs_wkt(text) => {
                Ok(Crs::from_authority_code(None, None, Some(text.to_string()), None))
            }
            Some(_) => Err(Error::Usage(format!(
                "--crs: expected a CRS definition — PROJJSON (a JSON object) or WKT \
                 (GEOGCS/PROJCS/GEOGCRS/PROJCRS/…) — but the text starts with \"{}\"",
                text.chars().take(24).collect::<String>(),
            ))),
        }
    }

    /// This CRS's portable `AUTHORITY:CODE` identity, or `None` when it has none
    /// to report — what `--print-crs-code` writes to stdout so that an external
    /// resolver can be handed something scriptable, rather than a code buried in
    /// a human-readable warning sentence.
    ///
    /// Note the distinction the *caller's* `Option<Crs>` already draws, which
    /// this must not flatten. A collection with no CRS at all (`None`: CSV and
    /// WKT have no CRS channel, so nothing was ever recorded) is a different
    /// thing from one carrying [`Crs::Wgs84`]. The latter includes GeoJSON and
    /// KML/KMZ, whose *specs* fix them at WGS 84 and which therefore encode no
    /// CRS in the file itself — the identity is real and known, merely not
    /// written down, so it is reported like any other rather than treated as
    /// absent. See [`WGS84_CRS_CODE`] for which spelling it gets.
    ///
    /// A [`Crs::Named`] needs *both* halves of the pair: a resolver is handed
    /// `AUTHORITY:CODE`, and half of that names nothing.
    pub fn authority_code(&self) -> Option<String> {
        match self {
            Crs::Wgs84 => Some(WGS84_CRS_CODE.to_string()),
            Crs::Named(n) => n.authority_code().map(|(a, c)| format!("{a}:{c}")),
        }
    }

    /// The CRS *definition text* this source recorded, verbatim — what
    /// `--print-crs` writes to stdout, and `None` when there is nothing recorded
    /// to write.
    ///
    /// **PROJJSON wins when a source carries both.** A stated rule rather than
    /// incidental field order: it is the strictly less ambiguous of the two (no
    /// WKT1-vs-WKT2 dialect question, no Esri-flavor spelling problem), and it
    /// is the form the recovery tools on the other end of a pipe parse most
    /// readily.
    ///
    /// [`Crs::Wgs84`] returns `None`, which is the opposite answer
    /// [`Crs::authority_code`] gives it — deliberately. That variant is
    /// fieldless because the formats that imply it write no definition at all,
    /// so there is a real identity to report (`OGC:CRS84`) and no text to quote.
    /// Synthesizing one would mean inventing a definition, and geosetta has no
    /// registry and no business inventing one; the tool on the other end of the
    /// pipe is what has both. `--print-crs-code` answers "what is this?" and
    /// `--print-crs` answers "what did the file literally say?" — for WGS 84 the
    /// file said nothing.
    ///
    /// The returned text is the source's own bytes (see [`NamedCrs::projjson`]),
    /// so a caller may hand it onward unmodified.
    pub fn definition_body(&self) -> Option<&str> {
        match self {
            Crs::Wgs84 => None,
            Crs::Named(n) => n.projjson.as_deref().or(n.wkt.as_deref()),
        }
    }

    /// A warning to print when converting a collection in this CRS to `target`,
    /// or `None` when `target` can faithfully record it. The returned string is
    /// the message *body*: `main.rs`'s `print_warnings` adds the `warning: `
    /// prefix, uniformly for every check.
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
            // GeoJSON and KML/KMZ have no CRS channel at all — all *always*
            // WGS 84 by spec, so a non-WGS-84 source doesn't get dropped, it
            // gets silently relabeled: the same numbers are re-emitted as if
            // they were already WGS 84 lon/lat.
            Format::GeoJson | Format::Kml | Format::Kmz => {
                let target_name = target.display_name();
                Some(format!(
                    "{label} is not WGS 84; {target_name} is always WGS 84 — output \
                     coordinates will be mislabeled. Reproject to EPSG:4326 before converting."
                ))
            }
            Format::Csv | Format::Wkt => Some(format!(
                "{} cannot record a CRS; {label} will be dropped from the output.",
                target.display_name()
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
                // Geosetta resolves structurally or not at all — it never
                // guesses a definition it wasn't given — so the way out is for
                // the user to supply one, and the hint names the flag that
                // takes it.
                let hint = " Supply the definition with `--crs <path|->` \
                             (`--print-crs-code` prints the code to resolve).";
                Some(format!(
                    "{label} will not be resolvable in the GeoParquet output — \
                     {detail}, so it is written only as an id reference that PROJ/GDAL/QGIS read as unknown.{hint}"
                ))
            }
            // FlatGeobuf/GeoPackage speak WKT: faithful with a WKT definition, a
            // usable authority code, or a PROJJSON-only CRS Geosetta can
            // structurally translate (geographic, currently — see
            // `NamedCrs::structural_wkt`). A PROJJSON-only CRS outside that
            // scope (projected, a datum ensemble, a non-metre ellipsoid axis, …)
            // has no way to reach a WKT-dialect target.
            Format::FlatGeobuf | Format::Gpkg if !named.wkt_expressible() => Some(format!(
                "source CRS is a {} definition with no authority code, and {} records CRS as \
                 WKT; Geosetta cannot translate this particular definition, so the CRS will be \
                 dropped from the output.",
                named.definition_dialect(),
                target.display_name(),
            )),
            // A narrower loss than the branch above: WKT *is* expressible (so
            // the CRS as a whole isn't dropped), but FlatGeobuf's `Crs.code`
            // and GeoPackage's `srs_id`/`organization_coordsys_id` are both
            // native integer columns — genuinely numeric on disk, unlike the
            // IR's string `code` field, which also has to hold IGNF/OGC/
            // PROJ/NKG's alphanumeric codes (e.g. `"LAMB93"`). Such a code
            // can't be the native id: FlatGeobuf drops it outright (falls
            // back to the same "unset" sentinel as no code at all — see
            // `flatgeobuf/writer.rs::build_crs`); GeoPackage instead hands
            // out a synthetic numeric id (see `geopackage/writer.rs::
            // resolve_srs`), so what round-trips back is a *different* code
            // from the one recorded here, not the original. Both are real,
            // silent losses this project's own tests already exercised
            // (`alphanumeric_code_is_dropped_but_org_and_wkt_survive`,
            // `alphanumeric_code_falls_back_to_a_synthetic_srs_id`) without
            // ever warning about them until this branch was added.
            Format::FlatGeobuf | Format::Gpkg
                if named.code.as_deref().is_some_and(|c| !matches!(c.parse::<i64>(), Ok(n) if n > 0)) =>
            {
                Some(format!(
                    "{label}'s authority code is not a positive integer; {} records CRS codes \
                     as a native integer id, so the output will carry a \
                     synthetic/placeholder id instead of the original code (the authority and \
                     WKT definition still carry through).",
                    target.display_name(),
                ))
            }
            // Shapefile's .prj is pure WKT text with no separate code slot (unlike
            // FlatGeobuf/GeoPackage), so a bare authority code is only expressible
            // via the registry's def_wkt, not natively as it is for those two.
            Format::Shapefile if !named.shapefile_expressible() => {
                let detail = if named.projjson.is_some() {
                    "Geosetta has only a non-WKT (PROJJSON) definition it cannot translate for \
                     this CRS, and .prj records CRS as WKT text"
                } else {
                    "Geosetta has only an authority code, not a WKT definition to write"
                };
                let hint = " Supply the definition with `--crs <path|->` \
                             (`--print-crs-code` prints the code to resolve).";
                Some(format!(
                    "{label} will not be recorded in the Shapefile output — \
                     {detail}, so no .prj will be written.{hint}"
                ))
            }
            Format::Parquet | Format::FlatGeobuf | Format::Gpkg | Format::Shapefile => None,
        }
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

    /// This CRS's authority and code, when it carries both — the portable
    /// identity every format can speak, and the only form worth handing to an
    /// external resolver. Unlike `label`, which produces prose for
    /// warnings (`code 3857`, `the source CRS`), this is a contract: `None`
    /// rather than a partial answer.
    ///
    /// **A `NONE` authority is not an identity.** GeoPackage's
    /// `gpkg_spatial_ref_sys` requires an `srs_id` on every row, so a CRS that
    /// arrives with only a WKT definition still has to be given one to be
    /// written at all — `geopackage::writer` records organization `NONE` and a
    /// synthetic id (`SYNTHETIC_SRS_BASE`, 100000 upward). Reading that file
    /// back recovers the pair verbatim, and reporting `NONE:100000` to a
    /// resolver would hand it a code no registry can ever match, because
    /// geosetta invented it.
    ///
    /// So it reads as absent here, which puts such a CRS in the same bucket as
    /// an id-less WKT: nothing for `--print-crs-code`, and the definition body
    /// for `--print-crs`, which is the flag that actually helps. The *fields*
    /// are untouched — the writers still round-trip the synthetic id through a
    /// GeoPackage — since this is about what may be reported as an identity,
    /// not about what is stored.
    pub fn authority_code(&self) -> Option<(&str, &str)> {
        let authority = self.authority.as_deref()?;
        if authority.eq_ignore_ascii_case(SYNTHETIC_AUTHORITY) {
            return None;
        }
        Some((authority, self.code.as_deref()?))
    }

    /// Whether GeoParquet can record this CRS *resolvably*. It speaks PROJJSON,
    /// so it needs verbatim PROJJSON or a WKT definition Geosetta can translate
    /// into PROJJSON (see [`wkt_to_projjson`]). A bare authority+code is *not*
    /// enough: an id-only reference is invalid PROJJSON that PROJ/GDAL/QGIS read
    /// as unknown — supplying a real definition for such a code is what `--crs`
    /// is for. Mirrors [`crate::parquet::geo`]'s `crs_projjson`.
    fn parquet_expressible(&self) -> bool {
        self.projjson.is_some()
            || self.wkt.as_deref().is_some_and(|w| wkt_to_projjson(w).is_some())
    }

    /// Whether the WKT-dialect targets (FlatGeobuf, GeoPackage) can record this
    /// CRS. They store a WKT `definition` and/or an authority code, so they need
    /// a WKT string or a usable code — or, for a PROJJSON-only definition with
    /// neither (the case a registry lookup can't help with, since it has no
    /// `(authority, code)` to key on), a structural PROJJSON→WKT translation
    /// (see [`Self::structural_wkt`]). Mirrors `flatgeobuf::writer`'s
    /// `build_crs` and `geopackage::writer`'s `resolve_srs`.
    fn wkt_expressible(&self) -> bool {
        self.wkt.is_some() || self.code.is_some() || self.structural_wkt().is_some()
    }

    /// Whether Shapefile's `.prj` (pure WKT text, no separate code slot) can
    /// record this CRS: a WKT definition, or a structural PROJJSON→WKT
    /// translation (see [`Self::structural_wkt`]). Unlike
    /// [`Self::wkt_expressible`], a bare code alone is *not* enough, since
    /// `.prj` has nowhere to put a code without a WKT wrapper — a code-only CRS
    /// bound for `.prj` needs a definition supplied via `--crs`. Mirrors
    /// `shapefile::writer`'s `.prj` writer.
    fn shapefile_expressible(&self) -> bool {
        self.wkt.is_some() || self.structural_wkt().is_some()
    }

    /// A WKT1 rendering of this CRS's verbatim PROJJSON, when it has one and no
    /// `id` to resolve some other way — the reverse of [`wkt_to_projjson`] (see
    /// [`projjson_to_wkt`]), for a GeoParquet source reaching a WKT-dialect
    /// target. `None` when there is no PROJJSON, or the translation can't be
    /// made faithfully (a projected CRS, a datum ensemble, a non-metre
    /// ellipsoid axis, …) — see `plans/projjson-to-wkt.org`.
    pub(crate) fn structural_wkt(&self) -> Option<String> {
        self.projjson.as_deref().and_then(projjson_to_wkt)
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

/// Lift a PROJJSON object's `id.authority`/`id.code`, if present, alongside the
/// verbatim PROJJSON text.
///
/// Shared by the GeoParquet reader (which meets PROJJSON both in `geo` metadata
/// and in the native `GEOMETRY` logical type) and by
/// [`Crs::from_definition_text`], so a `--crs` override and a file's own CRS
/// recover their identity by one rule rather than two.
///
/// `raw` is the definition's own source text, which becomes
/// [`NamedCrs::projjson`] untouched — *not* `crs.to_json_string()`. The parsed
/// value is read for the `id` and nothing else. Splitting the two is what makes
/// the stored definition byte-identical to what the source wrote: this crate's
/// serializer is compact, so re-printing a pretty-printed definition would
/// silently strip its formatting, and `--print-crs` promises the opposite.
pub(crate) fn crs_from_projjson(crs: &JsonValue, raw: &str) -> Crs {
    let id = crs.get("id");
    let authority = id
        .and_then(|i| i.get("authority"))
        .and_then(JsonValue::as_str)
        .map(String::from);
    let code = id.and_then(|i| i.get("code")).and_then(json_code_as_string);
    Crs::from_authority_code(authority, code, None, Some(raw.to_string()))
}

/// Recover a [`Crs`] from PROJJSON *text* — the shape every caller that already
/// holds the definition as a standalone string wants (a `--crs` override, the
/// Parquet `GEOMETRY` logical type's own `crs` field).
///
/// The stored definition is the input's own bytes, less any whitespace framing
/// it, rather than a re-print of the parse: [`crate::json::raw_at`] with an
/// empty path measures the root value exactly. Callers whose PROJJSON is nested
/// inside a larger document pass the enclosing text and a path to
/// [`crate::json::raw_at`] themselves, then use [`crs_from_projjson`].
pub(crate) fn crs_from_projjson_text(text: &str) -> Result<Crs> {
    let value = crate::json::parse(text)?;
    let raw = crate::json::raw_at(text, &[])?.unwrap_or(text);
    Ok(crs_from_projjson(&value, raw))
}

/// A PROJJSON `id.code` value read back as a string, whichever JSON type it
/// arrived as (the inverse of `parquet::geo`'s `code_json_literal`): a JSON
/// string is used verbatim (IGNF/OGC/PROJ/NKG-style alphanumeric codes), a JSON
/// number is formatted without a spurious fractional part (authority codes are
/// always integers).
fn json_code_as_string(v: &JsonValue) -> Option<String> {
    match v.as_str() {
        Some(s) => Some(s.to_string()),
        None => v.as_f64().map(wkt_projjson::number),
    }
}

/// Whether `text` opens as a WKT CRS node: a recognized CRS keyword followed by
/// a bracket.
///
/// Deliberately shallow — this decides a *dialect*, not validity, in the same
/// spirit as [`wkt_authority_code`]: the crate never parses WKT for meaning.
/// WKT1 and WKT2 keywords are both accepted, since a `--crs` override may be
/// written in either and geosetta has no business preferring one.
fn is_crs_wkt(text: &str) -> bool {
    const CRS_KEYWORDS: [&str; 16] = [
        // WKT1 (GDAL / Esri flavor)
        "GEOGCS", "PROJCS", "GEOCCS", "LOCAL_CS", "VERT_CS", "COMPD_CS",
        // WKT2 (:2015 and :2019)
        "GEOGCRS", "GEODCRS", "PROJCRS", "VERTCRS", "ENGCRS", "COMPOUNDCRS", "BOUNDCRS",
        "TIMECRS", "PARAMETRICCRS", "DERIVEDPROJCRS",
    ];
    let mut toks = tokenize_wkt(text).into_iter();
    match (toks.next(), toks.next()) {
        (Some(WktTok::Word(kw)), Some(WktTok::Open)) => {
            CRS_KEYWORDS.contains(&kw.to_ascii_uppercase().as_str())
        }
        _ => false,
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
    fn parquet_warns_for_a_code_only_crs() {
        // GeoParquet has no verbatim PROJJSON and no WKT to translate, so a bare
        // code can only be written as an unresolvable id reference — warn, and
        // point at the flag that accepts a real definition for it.
        let w = gda2020().downgrade_warning(Format::Parquet).unwrap();
        assert!(w.contains("EPSG:7844"), "{w}");
        assert!(w.contains("resolv"), "{w}");
        assert!(w.contains("id reference"), "{w}");
        assert!(w.contains("--crs"), "{w}");
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

    #[test]
    fn definition_text_accepts_both_dialects() {
        // WKT1, WKT2 and PROJJSON all parse, and each lands in the field its
        // own dialect belongs in — the same shape a reader would have produced.
        let wkt1 = "GEOGCS[\"GDA2020\",DATUM[\"GDA2020\",SPHEROID[\"GRS 1980\",6378137,298.257222101]],\
                    AUTHORITY[\"EPSG\",\"7844\"]]";
        match Crs::from_definition_text(wkt1).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority_code(), Some(("EPSG", "7844")));
                assert!(n.wkt.is_some() && n.projjson.is_none());
            }
            other => panic!("expected Named, got {other:?}"),
        }

        let wkt2 = "PROJCRS[\"custom grid\",BASEGEOGCRS[\"x\"],ID[\"EPSG\",\"3857\"]]";
        match Crs::from_definition_text(wkt2).unwrap() {
            Crs::Named(n) => assert_eq!(n.authority_code(), Some(("EPSG", "3857"))),
            other => panic!("expected Named, got {other:?}"),
        }

        // Leading whitespace/newlines are what a tool's stdout actually looks
        // like, so the dialect sniff has to see past them.
        let projjson = "\n  {\"type\":\"GeographicCRS\",\"name\":\"GDA2020\",\
                        \"id\":{\"authority\":\"EPSG\",\"code\":7844}}\n";
        match Crs::from_definition_text(projjson).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.authority_code(), Some(("EPSG", "7844")));
                assert!(n.projjson.is_some() && n.wkt.is_none());
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn a_synthetic_none_authority_is_not_reported_as_an_identity() {
        // GeoPackage must put *some* srs_id on every row, so a WKT-only CRS gets
        // organization NONE and an invented id. Reporting that pair would hand a
        // resolver a code geosetta made up, which no registry can ever match.
        let synthetic = Crs::Named(NamedCrs {
            authority: Some(SYNTHETIC_AUTHORITY.into()),
            code: Some("100000".into()),
            wkt: Some("GEOGCS[\"Datum A\"]".into()),
            projjson: None,
        });
        assert_eq!(
            synthetic.authority_code(),
            None,
            "not a resolvable identity"
        );
        // The definition body is what actually helps here, and it is still there.
        assert_eq!(synthetic.definition_body(), Some("GEOGCS[\"Datum A\"]"));

        // Spelling is not the point; the placeholder is.
        let lowercase = Crs::Named(NamedCrs {
            authority: Some("none".into()),
            code: Some("100000".into()),
            ..Default::default()
        });
        assert_eq!(lowercase.authority_code(), None);
    }

    #[test]
    fn a_real_authority_is_still_reported() {
        // The change must not reach anything but the placeholder.
        let real = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("7844".into()),
            ..Default::default()
        });
        assert_eq!(real.authority_code().as_deref(), Some("EPSG:7844"));
        // And the fields themselves are untouched, so writers still round-trip
        // a synthetic id through a GeoPackage.
        let synthetic = NamedCrs {
            authority: Some(SYNTHETIC_AUTHORITY.into()),
            code: Some("100000".into()),
            ..Default::default()
        };
        assert_eq!(synthetic.authority.as_deref(), Some("NONE"));
        assert_eq!(synthetic.code.as_deref(), Some("100000"));
    }

    #[test]
    fn definition_body_prefers_projjson_when_both_dialects_are_present() {
        // A stated rule, not incidental field order: PROJJSON is the less
        // ambiguous dialect (no WKT1-vs-WKT2 question, no Esri spelling problem)
        // and the form the recovery tools downstream parse.
        //
        // Asserted here rather than through the CLI because no reader produces
        // this state — GeoParquet records PROJJSON only, FlatGeobuf/GeoPackage
        // and a Shapefile `.prj` record WKT only. The rule still has to hold for
        // whatever fills both next, which is exactly what a unit test is for.
        let both = Crs::Named(NamedCrs {
            authority: None,
            code: None,
            wkt: Some("GEOGCS[\"D\"]".into()),
            projjson: Some(r#"{"type":"GeographicCRS","name":"D"}"#.into()),
        });
        assert_eq!(
            both.definition_body(),
            Some(r#"{"type":"GeographicCRS","name":"D"}"#)
        );
    }

    #[test]
    fn definition_body_falls_back_to_wkt_and_reports_nothing_when_there_is_none() {
        let wkt_only = Crs::Named(NamedCrs {
            wkt: Some("GEOGCS[\"D\"]".into()),
            ..Default::default()
        });
        assert_eq!(wkt_only.definition_body(), Some("GEOGCS[\"D\"]"));

        // A code is an identity, not a definition: `--print-crs-code` answers
        // for this input and `--print-crs` correctly has nothing to say.
        let code_only = Crs::Named(NamedCrs {
            authority: Some("EPSG".into()),
            code: Some("7844".into()),
            ..Default::default()
        });
        assert_eq!(code_only.definition_body(), None);
        assert_eq!(code_only.authority_code().as_deref(), Some("EPSG:7844"));

        // And WGS 84 is the deliberate disagreement between the two flags: a
        // real identity, no recorded text.
        assert_eq!(Crs::Wgs84.definition_body(), None);
        assert_eq!(Crs::Wgs84.authority_code().as_deref(), Some(WGS84_CRS_CODE));
    }

    #[test]
    fn an_override_keeps_the_definition_the_user_supplied() {
        // `--crs` text is stored as the user wrote it, framing whitespace aside:
        // the whole point of the flag is that geosetta accepts a definition some
        // other tool produced and passes it on without an opinion. Piping a
        // pretty-printed definition in and getting a compacted one out would be
        // geosetta editing a definition it never interprets.
        let pretty = "{\n  \"type\": \"GeographicCRS\",\n  \"name\": \"GDA2020\",\n  \
                      \"id\": {\n    \"authority\": \"EPSG\",\n    \"code\": 7844\n  }\n}";
        let padded = format!("\n  {pretty}\n\n");
        match Crs::from_definition_text(&padded).unwrap() {
            Crs::Named(n) => {
                assert_eq!(n.projjson.as_deref(), Some(pretty), "not byte-identical");
                assert_eq!(n.authority_code(), Some(("EPSG", "7844")));
            }
            other => panic!("expected Named, got {other:?}"),
        }
    }

    #[test]
    fn definition_text_lifts_identity_exactly_like_a_reader() {
        // An override is not a special kind of CRS: it goes through
        // `from_authority_code` like every reader path, so the WGS 84 collapse
        // applies to it too, in either dialect.
        let wkt = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],\
                   AUTHORITY[\"EPSG\",\"4326\"]]";
        assert_eq!(Crs::from_definition_text(wkt).unwrap(), Crs::Wgs84);
        let projjson = "{\"type\":\"GeographicCRS\",\"id\":{\"authority\":\"OGC\",\"code\":\"CRS84\"}}";
        assert_eq!(Crs::from_definition_text(projjson).unwrap(), Crs::Wgs84);
    }

    #[test]
    fn definition_text_rejects_what_is_neither_dialect() {
        // Strict fallback: a malformed override is an error, never a silently
        // ignored one — the user asked for a specific CRS and must not get a
        // conversion labeled with something else.
        for bad in ["", "   \n ", "EPSG:7844", "<gml:ProjectedCRS/>", "not wkt at all"] {
            assert!(
                matches!(Crs::from_definition_text(bad), Err(Error::Usage(_))),
                "should have rejected {bad:?}",
            );
        }
        // A JSON *document* that isn't PROJJSON still parses as JSON; it simply
        // yields a CRS with no identity, which is the user's problem, not a
        // parse failure. What must not happen is a panic or a silent WGS 84.
        let odd = Crs::from_definition_text("{\"hello\":\"world\"}").unwrap();
        assert!(matches!(&odd, Crs::Named(n) if n.authority_code().is_none()));
    }

    #[test]
    fn authority_code_reports_wgs84_by_its_lon_lat_spelling() {
        // `Crs::Wgs84` is fieldless, so reporting an identity for it means
        // choosing one; OGC:CRS84 is what the variant asserts (lon/lat order).
        // This is the GeoJSON/KML case: a spec-mandated CRS that the file
        // itself never encodes is still an identity worth reporting.
        assert_eq!(Crs::Wgs84.authority_code().as_deref(), Some(WGS84_CRS_CODE));
        assert_eq!(Crs::Wgs84.authority_code().as_deref(), Some("OGC:CRS84"));
        // ...and it round-trips to a no-op through the override path.
        assert_eq!(
            Crs::from_definition_text(
                "{\"type\":\"GeographicCRS\",\"id\":{\"authority\":\"OGC\",\"code\":\"CRS84\"}}"
            )
            .unwrap(),
            Crs::Wgs84,
        );
    }

    #[test]
    fn authority_code_needs_both_halves() {
        // `AUTHORITY:CODE` is what gets handed to a resolver; half of it names
        // nothing, so a partial identity reports none at all rather than
        // something unusable. Contrast `label`, which is prose for warnings.
        assert_eq!(gda2020().authority_code().as_deref(), Some("EPSG:7844"));
        let code_only = Crs::Named(NamedCrs { code: Some("3857".into()), ..Default::default() });
        assert_eq!(code_only.authority_code(), None);
        let auth_only = Crs::Named(NamedCrs { authority: Some("EPSG".into()), ..Default::default() });
        assert_eq!(auth_only.authority_code(), None);
        // An id-less WKT (the Esri .prj shape) has nothing to report either.
        assert_eq!(wkt_only().authority_code(), None);
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
    fn wkt_targets_warn_for_an_alphanumeric_code_even_with_wkt_present() {
        // IGNF/OGC/PROJ/NKG-style codes (e.g. "LAMB93") aren't dropped
        // outright by FlatGeobuf/GeoPackage (WKT still carries through — the
        // branch above this one doesn't fire), but neither format's native
        // integer code slot can hold the original code, which is a real,
        // narrower loss (see `flatgeobuf::writer::tests::
        // alphanumeric_code_is_dropped_but_org_and_wkt_survive` and
        // `geopackage::writer::tests::
        // alphanumeric_code_falls_back_to_a_synthetic_srs_id`) that had no
        // warning before this test existed.
        let lamb93 = Crs::Named(NamedCrs {
            authority: Some("IGNF".into()),
            code: Some("LAMB93".into()),
            wkt: Some("PROJCS[\"RGF93 Lambert 93\"]".into()),
            projjson: None,
        });
        for (target, name) in [(Format::FlatGeobuf, "FlatGeobuf"), (Format::Gpkg, "GeoPackage")] {
            let w = lamb93.downgrade_warning(target).unwrap();
            assert!(w.contains("not a positive integer"), "{w}");
            assert!(w.contains(name), "{w}");
            assert!(w.contains("synthetic"), "{w}");
        }
        // A numeric code (however unusual the authority) doesn't warn.
        let numeric = Crs::Named(NamedCrs {
            authority: Some("ESRI".into()),
            code: Some("102100".into()),
            wkt: Some("PROJCS[\"WGS_1984_Web_Mercator\"]".into()),
            projjson: None,
        });
        assert_eq!(numeric.downgrade_warning(Format::FlatGeobuf), None);
        assert_eq!(numeric.downgrade_warning(Format::Gpkg), None);
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
