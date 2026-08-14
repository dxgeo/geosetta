//! Translate a CRS **WKT** definition into a **PROJJSON** object, so the CRS a
//! WKT-dialect source records (GeoPackage, FlatGeobuf) can be written into
//! GeoParquet — whose only CRS channel is PROJJSON — in a form PROJ-backed
//! readers (GDAL, QGIS, DuckDB) actually resolve.
//!
//! This is milestone C of `plans/crs.org`, in the direction the common case
//! needs (WKT → PROJJSON). It is a *structural* translation, not projection
//! math: every field is copied verbatim from the WKT the source already carried.
//! PROJ trusts the object it is handed (it does not re-derive the definition
//! from an embedded `id`), so the translation must be faithful — which it is,
//! because the WKT holds the datum name, the ellipsoid's axis + flattening, and
//! the authority id already.
//!
//! Scope: **geographic** CRSes (WKT1 `GEOGCS`, WKT2 `GEOGCRS`), which covers the
//! reported GeoPackage case (GDA2020, EPSG:7844) and most non-projected sources.
//! Anything else — projected CRSes, datum *ensembles*, geocentric CRSes —
//! returns `None`, so the writer falls back to the (unresolvable but
//! identity-preserving) id reference and the CLI warns. Emitting a *wrong*
//! translation would be worse than none precisely because PROJ trusts it.

use super::{tokenize_wkt, WktTok};
use crate::json::JsonValue;

/// Translate a CRS WKT string to a PROJJSON object string, or `None` when the
/// WKT is not a CRS this module can *faithfully* render (fall back to an id
/// reference + warning, never a guess — PROJ trusts the object it is handed).
pub(crate) fn wkt_to_projjson(wkt: &str) -> Option<String> {
    let toks = tokenize_wkt(wkt);
    let mut pos = 0;
    let root = parse_node(&toks, &mut pos)?;
    match root.keyword.to_ascii_uppercase().as_str() {
        // WKT1 geographic, WKT2 geographic (2D). Geocentric GEODCRS/GEODETICCRS
        // (Cartesian XYZ) are intentionally not handled here.
        "GEOGCS" | "GEOGCRS" => render_geographic(&root),
        // WKT2 *projected* (`PROJCRS`): `METHOD`/`PARAMETER` carry canonical names
        // *and* inline EPSG ids, so the render is mechanical.
        "PROJCRS" | "PROJECTEDCRS" => render_projected(&root),
        // WKT1 *projected* (`PROJCS`): GDAL projection/parameter names with no ids,
        // resolved through the crosswalk-generated tables. Ambiguous or unknown
        // methods are absent from the tables and fall through to `None`.
        "PROJCS" => render_projected_wkt1(&root),
        _ => None,
    }
}

/// Translate a PROJJSON CRS object into a WKT1 (GDAL-flavor) string — the
/// mirror of [`wkt_to_projjson`], for the direction it needs. Needed when a
/// GeoParquet source's PROJJSON carries no authority `id`: the registry can't
/// help there (its lookups are keyed by `(authority, code)`, and an id-less
/// PROJJSON has nothing to key on), so this is the only way such a CRS can
/// still reach a WKT-dialect target (FlatGeobuf, GeoPackage, Shapefile's
/// `.prj`). See `plans/projjson-to-wkt.org`.
///
/// `ProjectedCRS` goes through the `wkt1_tables.rs` crosswalk *reversed*
/// (EPSG method/parameter code → GDAL WKT1 name); a method code the crosswalk
/// doesn't have — one no WKT1 source ever exercised — declines rather than
/// guessing, the same as the forward direction's own unresolved case.
pub(crate) fn projjson_to_wkt(json: &str) -> Option<String> {
    let pj = crate::json::parse(json).ok()?;
    match pj.get("type").and_then(JsonValue::as_str)? {
        "GeographicCRS" => render_wkt1_geographic(&pj),
        "ProjectedCRS" => render_wkt1_projected(&pj),
        _ => None,
    }
}

/// The unit family of a projection parameter, recorded in the crosswalk tables.
/// A WKT1 parameter carries no unit of its own; its kind selects which PROJJSON
/// unit to attach — the base CRS's angular unit, the projected linear unit, or a
/// dimensionless scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnitKind {
    Angular,
    Linear,
    Scale,
}

/// A parsed WKT node: `KEYWORD[item, item, ...]`.
struct Node {
    keyword: String,
    items: Vec<Item>,
}

/// One item inside a node: a nested node, a quoted string, or a bare word
/// (a number, or an unquoted token like an axis direction).
enum Item {
    Node(Node),
    Str(String),
    Word(String),
}

impl Node {
    /// The first nested child whose keyword matches any of `keywords`.
    fn child(&self, keywords: &[&str]) -> Option<&Node> {
        self.items.iter().find_map(|it| match it {
            Item::Node(n) if keywords.iter().any(|k| n.keyword.eq_ignore_ascii_case(k)) => Some(n),
            _ => None,
        })
    }

    /// The node's first quoted string (a keyword's name is always its first).
    fn first_str(&self) -> Option<&str> {
        self.items.iter().find_map(|it| match it {
            Item::Str(s) => Some(s.as_str()),
            _ => None,
        })
    }

    /// The node's first bare word, as its *raw* token (e.g. an accuracy `"2.0"`
    /// kept verbatim rather than reformatted through `f64`).
    fn first_word(&self) -> Option<&str> {
        self.items.iter().find_map(|it| match it {
            Item::Word(w) => Some(w.as_str()),
            _ => None,
        })
    }

    /// The node's bare-word items parsed as numbers, in order.
    fn numbers(&self) -> Vec<f64> {
        self.items
            .iter()
            .filter_map(|it| match it {
                Item::Word(w) => w.parse::<f64>().ok(),
                _ => None,
            })
            .collect()
    }
}

/// Parse one `KEYWORD[...]` node starting at `*pos`, advancing past it.
fn parse_node(toks: &[WktTok], pos: &mut usize) -> Option<Node> {
    let keyword = match toks.get(*pos)? {
        WktTok::Word(w) => w.clone(),
        _ => return None,
    };
    *pos += 1;
    if !matches!(toks.get(*pos)?, WktTok::Open) {
        return None;
    }
    *pos += 1;

    let mut items = Vec::new();
    loop {
        match toks.get(*pos)? {
            WktTok::Close => {
                *pos += 1;
                break;
            }
            WktTok::Comma => *pos += 1,
            WktTok::Str(s) => {
                items.push(Item::Str(s.clone()));
                *pos += 1;
            }
            WktTok::Word(w) => {
                // A word followed by `[` opens a nested node; otherwise it is a
                // number or bare token.
                if matches!(toks.get(*pos + 1), Some(WktTok::Open)) {
                    items.push(Item::Node(parse_node(toks, pos)?));
                } else {
                    items.push(Item::Word(w.clone()));
                    *pos += 1;
                }
            }
            // A stray open bracket (malformed) — skip it rather than loop forever.
            WktTok::Open => *pos += 1,
        }
    }
    Some(Node { keyword, items })
}

/// The geographic 2D coordinate system, in the *authoritative* axis order —
/// latitude, longitude — matching how EPSG defines geographic 2D CRSes (and
/// PROJ's own PROJJSON). This order is what lets PROJ/GDAL/QGIS *identify* the CRS
/// as its EPSG code (a lon,lat order drops identification confidence to ~25% and
/// QGIS then shows a bare name, not the EPSG entry). GeoParquet always stores
/// coordinates longitude, latitude (x, y); readers honor that convention
/// independently of the CRS axis order, so the coordinates are not flipped.
///
/// The angular unit is the CRS's own — usually `degree`, but `grad` for the
/// legacy French/African CRSes (Paris meridian), where assuming degree drops
/// identification to 25%.
fn ellipsoidal_cs(geo: &Node, dim: usize) -> String {
    let unit = angular_unit(geo);
    let mut axes = format!(
        "{{\"name\":\"Geodetic latitude\",\"abbreviation\":\"Lat\",\"direction\":\"north\",\"unit\":{unit}}},\
{{\"name\":\"Geodetic longitude\",\"abbreviation\":\"Lon\",\"direction\":\"east\",\"unit\":{unit}}}"
    );
    if dim >= 3 {
        axes.push_str(
            ",{\"name\":\"Ellipsoidal height\",\"abbreviation\":\"h\",\"direction\":\"up\",\"unit\":\"metre\"}",
        );
    }
    format!("{{\"subtype\":\"ellipsoidal\",\"axis\":[{axes}]}}")
}

/// The geographic CRS's angular unit: a direct `UNIT`/`ANGLEUNIT` (WKT1 `GEOGCS`),
/// else the `PRIMEM`'s angular unit (WKT2 `BASEGEOGCRS`, where the CS is often
/// implicit), else `degree`.
fn angular_unit(geo: &Node) -> String {
    geo.child(&["ANGLEUNIT", "UNIT"])
        .or_else(|| {
            geo.child(&["PRIMEM", "PRIMEMERIDIAN"])
                .and_then(|pm| pm.child(&["ANGLEUNIT", "UNIT"]))
        })
        .and_then(unit_fragment)
        .unwrap_or_else(|| "\"degree\"".to_string())
}

/// A JSON string literal, quoted and escaped.
fn q(s: &str) -> String {
    JsonValue::String(s.to_string()).to_json_string()
}

/// A node's *own* authority id — its direct-child `AUTHORITY` (WKT1) or `ID`
/// (WKT2) node — as `(authority, numeric code)`. Nested ids on the datum,
/// ellipsoid, method, etc. are ignored because they are not direct children;
/// this is what lets a projected CRS lift *both* its own code and its base's.
fn node_authority_code(node: &Node) -> Option<(String, i64)> {
    let id = node.child(&["AUTHORITY", "ID"])?;
    let mut fields = id.items.iter().filter_map(|it| match it {
        Item::Str(s) => Some(s.as_str()),
        Item::Word(w) => Some(w.as_str()),
        _ => None,
    });
    let authority = fields.next()?.to_string();
    let code = fields.next()?.parse::<i64>().ok()?;
    Some((authority, code))
}

/// Render a geographic CRS node (`GEOGCS`/`GEOGCRS`, or a projected CRS's
/// `BASEGEOGCRS`) as a PROJJSON `GeographicCRS` object string. Reused verbatim as
/// a projected CRS's `base_crs`, so it lifts the *node's own* id.
fn render_geographic(node: &Node) -> Option<String> {
    render_geographic_dim(node, geo_dimension(node))
}

/// The geographic CRS's own dimension (2 or 3), from its `CS[ellipsoidal,N]`;
/// 2 when the CS is implicit.
fn geo_dimension(node: &Node) -> usize {
    node.child(&["CS"])
        .and_then(|cs| cs.numbers().first().copied())
        .map(|n| n as usize)
        .unwrap_or(2)
}

/// Render a geographic CRS with an explicit axis count. A projected CRS's base
/// omits its own `CS`, so its dimension follows the *projected* CRS's (a 3D
/// projected CRS ⇒ a 3D base carrying an ellipsoidal-height axis).
fn render_geographic_dim(node: &Node, dim: usize) -> Option<String> {
    let name = node.first_str()?;
    let mut out = String::new();
    out.push_str("{\"type\":\"GeographicCRS\",\"name\":");
    out.push_str(&q(name));
    out.push(',');
    out.push_str(&render_datum(node)?);
    out.push_str(",\"coordinate_system\":");
    out.push_str(&ellipsoidal_cs(node, dim));
    if let Some((authority, code)) = node_authority_code(node) {
        out.push_str(",\"id\":{\"authority\":");
        out.push_str(&q(&authority));
        out.push_str(",\"code\":");
        out.push_str(&code.to_string());
        out.push('}');
    }
    out.push('}');
    Some(out)
}

/// Render the `"datum"` (plain `DATUM`) or `"datum_ensemble"` (`ENSEMBLE`, e.g.
/// WGS 84) member of a geographic node. Ensemble *members* carry no ids in WKT2
/// and are not needed to identify — name + ellipsoid is enough (verified with
/// `projinfo --identify`, 100%), so they are omitted.
fn render_datum(geo: &Node) -> Option<String> {
    if let Some(datum) = geo.child(&["DATUM", "GEODETICDATUM", "TRF"]) {
        let name = datum.first_str().unwrap_or("unknown");
        let ellipsoid = render_ellipsoid(datum)?;
        // A non-Greenwich prime meridian (Madrid, Ferro, Paris, …) nests *inside*
        // the datum in PROJJSON, even though WKT2 lists `PRIMEM` at the geographic
        // CRS level. Dropping it makes PROJ assume Greenwich — a different CRS.
        Some(format!(
            "\"datum\":{{\"type\":\"GeodeticReferenceFrame\",\"name\":{},{}{}}}",
            q(name),
            ellipsoid,
            prime_meridian(geo).unwrap_or_default()
        ))
    } else if let Some(ensemble) = geo.child(&["ENSEMBLE"]) {
        let name = ensemble.first_str().unwrap_or("unknown");
        let ellipsoid = render_ellipsoid(ensemble)?;
        // PROJJSON *requires* `accuracy` on a `datum_ensemble` (members, by
        // contrast, are optional and omitted). GDAL WKT2 always records it as
        // `ENSEMBLEACCURACY`, kept here as its raw string token. If it is ever
        // absent, fall back to a plain datum — which still identifies at 100%.
        match ensemble.child(&["ENSEMBLEACCURACY"]).and_then(Node::first_word) {
            Some(accuracy) => Some(format!(
                "\"datum_ensemble\":{{\"name\":{},{},\"accuracy\":{}}}",
                q(name),
                ellipsoid,
                q(accuracy)
            )),
            None => Some(format!(
                "\"datum\":{{\"type\":\"GeodeticReferenceFrame\",\"name\":{},{}}}",
                q(name),
                ellipsoid
            )),
        }
    } else {
        None
    }
}

/// The `,"prime_meridian":{…}` fragment for a geographic node whose `PRIMEM` is
/// *not* Greenwich (longitude 0 in degrees). Greenwich is the PROJJSON default
/// and is omitted. When the meridian is given in a non-degree unit (e.g. Paris
/// in grad), the longitude must be the `{value, unit}` object form — a bare
/// number would be read as degrees, the wrong meridian.
fn prime_meridian(geo: &Node) -> Option<String> {
    let pm = geo.child(&["PRIMEM", "PRIMEMERIDIAN"])?;
    let longitude = *pm.numbers().first()?;
    let unit = pm.child(&["ANGLEUNIT", "UNIT"]).and_then(unit_fragment);
    let non_degree = matches!(unit.as_deref(), Some(u) if u != "\"degree\"");
    if longitude == 0.0 && !non_degree {
        return None;
    }
    let longitude_json = match &unit {
        Some(u) if non_degree => format!("{{\"value\":{},\"unit\":{}}}", number(longitude), u),
        _ => number(longitude),
    };
    let name = pm.first_str().unwrap_or("unknown");
    Some(format!(
        ",\"prime_meridian\":{{\"name\":{},\"longitude\":{}}}",
        q(name),
        longitude_json
    ))
}

/// Render the `"ellipsoid"` member from a datum or ensemble node.
fn render_ellipsoid(datum_or_ensemble: &Node) -> Option<String> {
    let ellipsoid = datum_or_ensemble.child(&["SPHEROID", "ELLIPSOID"])?;
    let name = ellipsoid.first_str().unwrap_or("unknown");
    let dims = ellipsoid.numbers();
    let semi_major = *dims.first()?;
    let inverse_flattening = *dims.get(1)?;
    // `semi_major_axis` is a length: when the ellipsoid's unit is not metre
    // (Clarke's foot, US survey foot, …) it must carry that unit as a `{value,
    // unit}` object, or PROJ reads the number as metres — a different ellipsoid,
    // hence a different datum. `inverse_flattening` is a dimensionless ratio.
    let unit = ellipsoid.child(&["LENGTHUNIT", "UNIT"]).and_then(unit_fragment);
    let semi_major_json = match &unit {
        Some(u) if u != "\"metre\"" => {
            format!("{{\"value\":{},\"unit\":{}}}", number(semi_major), u)
        }
        _ => number(semi_major),
    };
    Some(format!(
        "\"ellipsoid\":{{\"name\":{},\"semi_major_axis\":{},\"inverse_flattening\":{}}}",
        q(name),
        semi_major_json,
        number(inverse_flattening)
    ))
}

/// Render a WKT2 projected CRS (`PROJCRS`) as a PROJJSON `ProjectedCRS`: the base
/// geographic CRS, the map-projection `conversion` (method + parameters, each
/// carrying its inline EPSG id), and a Cartesian coordinate system. Returns
/// `None` on anything missing an inline id (e.g. WKT1), so the writer falls back
/// rather than emitting a CRS PROJ cannot identify.
fn render_projected(root: &Node) -> Option<String> {
    let name = root.first_str()?;
    let base = root.child(&["BASEGEOGCRS", "BASEGEODCRS", "GEOGCS", "GEOGCRS"])?;
    // A 3D projected CRS (easting, northing, height) has a 3D geographic base,
    // even though the base omits its own CS — so its dimension follows the count
    // of the projected CRS's own AXIS nodes.
    let projected_axes = root
        .items
        .iter()
        .filter(|it| matches!(it, Item::Node(n) if n.keyword.eq_ignore_ascii_case("AXIS")))
        .count();
    let base_json = render_geographic_dim(base, projected_axes.max(2))?;

    let conversion = root.child(&["CONVERSION", "DERIVINGCONVERSION"])?;
    let conversion_name = conversion.first_str().unwrap_or("unnamed");
    let method = conversion.child(&["METHOD", "PROJECTION"])?;
    let method_name = method.first_str()?;
    // WKT2 carries the method's EPSG id inline; its absence means WKT1 (deferred).
    let (method_auth, method_code) = node_authority_code(method)?;

    let mut parameters = String::new();
    for item in &conversion.items {
        let Item::Node(param) = item else { continue };
        if !param.keyword.eq_ignore_ascii_case("PARAMETER") {
            continue;
        }
        let pname = param.first_str()?;
        let pvalue = *param.numbers().first()?;
        let punit = param_unit(param)?;
        let (pauth, pcode) = node_authority_code(param)?;
        if !parameters.is_empty() {
            parameters.push(',');
        }
        parameters.push_str(&format!(
            "{{\"name\":{},\"value\":{},\"unit\":{},\"id\":{{\"authority\":{},\"code\":{}}}}}",
            q(pname),
            number(pvalue),
            punit,
            q(&pauth),
            pcode
        ));
    }
    if parameters.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("{\"type\":\"ProjectedCRS\",\"name\":");
    out.push_str(&q(name));
    out.push_str(",\"base_crs\":");
    out.push_str(&base_json);
    out.push_str(",\"conversion\":{\"name\":");
    out.push_str(&q(conversion_name));
    out.push_str(",\"method\":{\"name\":");
    out.push_str(&q(method_name));
    out.push_str(",\"id\":{\"authority\":");
    out.push_str(&q(&method_auth));
    out.push_str(",\"code\":");
    out.push_str(&method_code.to_string());
    out.push_str("}},\"parameters\":[");
    out.push_str(&parameters);
    out.push_str("]},\"coordinate_system\":");
    out.push_str(&render_cartesian(root));
    if let Some((authority, code)) = node_authority_code(root) {
        out.push_str(",\"id\":{\"authority\":");
        out.push_str(&q(&authority));
        out.push_str(",\"code\":");
        out.push_str(&code.to_string());
        out.push('}');
    }
    out.push('}');
    Some(out)
}

/// Render a WKT1 projected CRS (`PROJCS`) as a PROJJSON `ProjectedCRS`.
///
/// WKT1 (GDAL flavor) names the projection method and parameters with GDAL's own
/// strings and no EPSG ids, so the canonical names + ids + unit *kinds* come from
/// the crosswalk-generated tables (`wkt1_tables`, built by
/// `tests/fixtures/gen_wkt1_crosswalk.py`). An unknown/ambiguous method, or any
/// parameter absent from the tables, returns `None` — the writer then falls back
/// to an id reference + warning rather than emit a possibly-wrong CRS.
fn render_projected_wkt1(root: &Node) -> Option<String> {
    let name = root.first_str()?;
    let base = root.child(&["GEOGCS", "GEOGCRS"])?;

    let projection = root.child(&["PROJECTION"])?;
    let gdal_method = projection.first_str()?;
    let &(_, canonical_method, method_code, en_default_safe) = super::wkt1_tables::METHODS
        .iter()
        .find(|(gdal, _, _, _)| gdal.eq_ignore_ascii_case(gdal_method))?;

    // The Cartesian CS comes from WKT1 `AXIS` nodes when present. When they are
    // absent the translator must default to easting/northing — safe only for a
    // method with no south/west-oriented members (`en_default_safe`). Otherwise
    // the orientation is unrecoverable from this WKT1, so fall back rather than
    // risk a mislabeled CS.
    let has_axis = root
        .items
        .iter()
        .any(|it| matches!(it, Item::Node(n) if n.keyword.eq_ignore_ascii_case("AXIS")));
    if !has_axis && !en_default_safe {
        return None;
    }

    // The projected linear unit (metre, US survey foot, …) applies to the
    // Cartesian axes and to the linear (false easting/northing) parameters; the
    // base CRS's angular unit applies to the angular parameters.
    let linear_unit = root
        .child(&["UNIT"])
        .and_then(unit_fragment)
        .unwrap_or_else(|| "\"metre\"".to_string());
    let angular_base = angular_unit(base);

    let projected_axes = root
        .items
        .iter()
        .filter(|it| matches!(it, Item::Node(n) if n.keyword.eq_ignore_ascii_case("AXIS")))
        .count();
    let base_json = render_geographic_dim(base, projected_axes.max(2))?;

    let mut parameters = String::new();
    for item in &root.items {
        let Item::Node(param) = item else { continue };
        if !param.keyword.eq_ignore_ascii_case("PARAMETER") {
            continue;
        }
        let pname = param.first_str()?;
        let pvalue = *param.numbers().first()?;
        let &(_, _, canonical, code, kind) = super::wkt1_tables::PARAMS
            .iter()
            .find(|(mc, gdal, _, _, _)| *mc == method_code && gdal.eq_ignore_ascii_case(pname))?;
        let unit = match kind {
            UnitKind::Angular => angular_base.clone(),
            UnitKind::Linear => linear_unit.clone(),
            UnitKind::Scale => "\"unity\"".to_string(),
        };
        if !parameters.is_empty() {
            parameters.push(',');
        }
        parameters.push_str(&format!(
            "{{\"name\":{},\"value\":{},\"unit\":{},\"id\":{{\"authority\":\"EPSG\",\"code\":{}}}}}",
            q(canonical),
            number(pvalue),
            unit,
            code
        ));
    }
    if parameters.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("{\"type\":\"ProjectedCRS\",\"name\":");
    out.push_str(&q(name));
    out.push_str(",\"base_crs\":");
    out.push_str(&base_json);
    // WKT1 `PROJCS` records no separate conversion name (unlike WKT2's
    // `CONVERSION[...]`); PROJ uses "unnamed" here.
    out.push_str(",\"conversion\":{\"name\":");
    out.push_str(&q("unnamed"));
    out.push_str(",\"method\":{\"name\":");
    out.push_str(&q(canonical_method));
    out.push_str(&format!(",\"id\":{{\"authority\":\"EPSG\",\"code\":{method_code}}}}}"));
    out.push_str(",\"parameters\":[");
    out.push_str(&parameters);
    out.push_str("]},\"coordinate_system\":");
    out.push_str(&render_cartesian_wkt1(root, &linear_unit));
    if let Some((authority, code)) = node_authority_code(root) {
        out.push_str(",\"id\":{\"authority\":");
        out.push_str(&q(&authority));
        out.push_str(",\"code\":");
        out.push_str(&code.to_string());
        out.push('}');
    }
    out.push('}');
    Some(out)
}

/// The Cartesian coordinate system for a WKT1 projected CRS. WKT1 `AXIS` nodes
/// carry a bare uppercase direction (`EAST`) and no per-axis unit — the linear
/// unit is the shared projected `UNIT`. Falls back to easting, northing.
fn render_cartesian_wkt1(root: &Node, linear_unit: &str) -> String {
    let axes: Vec<&Node> = root
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Node(n) if n.keyword.eq_ignore_ascii_case("AXIS") => Some(n),
            _ => None,
        })
        .collect();
    let rendered: Vec<String> = axes
        .iter()
        .filter_map(|ax| {
            let direction = ax.first_word()?.to_ascii_lowercase();
            let (name, abbreviation) = axis_name_abbreviation(ax.first_str().unwrap_or(""), &direction);
            Some(format!(
                "{{\"name\":{},\"abbreviation\":{},\"direction\":{},\"unit\":{linear_unit}}}",
                q(&name),
                q(&abbreviation),
                q(&direction)
            ))
        })
        .collect();
    if rendered.len() >= 2 && rendered.len() == axes.len() {
        format!("{{\"subtype\":\"Cartesian\",\"axis\":[{}]}}", rendered.join(","))
    } else {
        format!(
            "{{\"subtype\":\"Cartesian\",\"axis\":[\
{{\"name\":\"Easting\",\"abbreviation\":\"E\",\"direction\":\"east\",\"unit\":{linear_unit}}},\
{{\"name\":\"Northing\",\"abbreviation\":\"N\",\"direction\":\"north\",\"unit\":{linear_unit}}}]}}"
        )
    }
}

/// A parameter's `"unit"` value: the bare PROJJSON string for the standard units
/// (`degree`/`metre`/`unity`), else a `{type,name,conversion_factor}` object for
/// a non-standard unit (e.g. US survey foot). `None` if the parameter has no unit.
fn param_unit(param: &Node) -> Option<String> {
    let unit = param.child(&["ANGLEUNIT", "LENGTHUNIT", "SCALEUNIT", "UNIT"])?;
    unit_fragment(unit)
}

/// Render a WKT unit node as a PROJJSON unit value — a bare string for the three
/// standard units, else a typed object carrying the conversion factor.
fn unit_fragment(unit: &Node) -> Option<String> {
    let name = unit.first_str().unwrap_or("");
    match name.to_ascii_lowercase().as_str() {
        "degree" => Some("\"degree\"".to_string()),
        "metre" | "meter" => Some("\"metre\"".to_string()),
        "unity" => Some("\"unity\"".to_string()),
        _ => {
            let factor = unit.numbers().first().copied()?;
            let unit_type = match unit.keyword.to_ascii_uppercase().as_str() {
                "ANGLEUNIT" => "AngularUnit",
                "SCALEUNIT" => "ScaleUnit",
                _ => "LinearUnit",
            };
            Some(format!(
                "{{\"type\":\"{}\",\"name\":{},\"conversion_factor\":{}}}",
                unit_type,
                q(name),
                number(factor)
            ))
        }
    }
}

/// Render the projected Cartesian coordinate system from the WKT2 `AXIS` nodes.
///
/// In WKT2 the `AXIS` nodes are *direct children* of the `PROJCRS` (siblings of
/// `CS[Cartesian,2]`), already in order. Every part is identity-critical: a wrong
/// axis *order* (LAEA is northing-first), a missing non-metre *unit* (US-survey-
/// foot state planes), or a dropped polar *meridian* each drops PROJ's
/// identification to ~25%. So the axes are rendered faithfully, not assumed.
fn render_cartesian(root: &Node) -> String {
    let axes: Vec<&Node> = root
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Node(n) if n.keyword.eq_ignore_ascii_case("AXIS") => Some(n),
            _ => None,
        })
        .collect();
    let rendered: Vec<String> = axes.iter().filter_map(|ax| render_axis(ax)).collect();
    if rendered.len() >= 2 && rendered.len() == axes.len() {
        format!("{{\"subtype\":\"Cartesian\",\"axis\":[{}]}}", rendered.join(","))
    } else {
        // Fallback: the conventional easting, northing / metre pair.
        "{\"subtype\":\"Cartesian\",\"axis\":[\
{\"name\":\"Easting\",\"abbreviation\":\"E\",\"direction\":\"east\",\"unit\":\"metre\"},\
{\"name\":\"Northing\",\"abbreviation\":\"N\",\"direction\":\"north\",\"unit\":\"metre\"}]}"
            .to_string()
    }
}

/// Render one WKT2 `AXIS` node as a PROJJSON axis object, preserving direction,
/// an optional polar `MERIDIAN` qualifier, and the axis's own linear unit.
fn render_axis(axis: &Node) -> Option<String> {
    let direction = axis.first_word()?;
    let (name, abbreviation) = axis_name_abbreviation(axis.first_str().unwrap_or(""), direction);
    let unit = axis
        .child(&["LENGTHUNIT", "UNIT"])
        .and_then(unit_fragment)
        .unwrap_or_else(|| "\"metre\"".to_string());

    let mut out = format!(
        "{{\"name\":{},\"abbreviation\":{},\"direction\":{}",
        q(&name),
        q(&abbreviation),
        q(direction)
    );
    if let Some(longitude) = axis.child(&["MERIDIAN"]).and_then(|m| m.numbers().first().copied()) {
        out.push_str(&format!(",\"meridian\":{{\"longitude\":{}}}", number(longitude)));
    }
    out.push_str(&format!(",\"unit\":{unit}}}"));
    Some(out)
}

/// Split a WKT2 axis label — `"name (ABBR)"` or the short `"(ABBR)"` — into the
/// PROJJSON `(name, abbreviation)`, deriving either from the direction when the
/// label omits it (e.g. UTM's `"(E)"` ⇒ name `"Easting"`).
fn axis_name_abbreviation(label: &str, direction: &str) -> (String, String) {
    let (name_part, abbr_part) = match (label.find('('), label.rfind(')')) {
        (Some(open), Some(close)) if close > open => {
            (label[..open].trim(), label[open + 1..close].trim())
        }
        _ => (label.trim(), ""),
    };
    let name = if name_part.is_empty() {
        default_axis_name(direction)
    } else {
        let mut chars = name_part.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => name_part.to_string(),
        }
    };
    let abbreviation = if abbr_part.is_empty() {
        default_axis_abbreviation(direction)
    } else {
        abbr_part.to_string()
    };
    (name, abbreviation)
}

fn default_axis_name(direction: &str) -> String {
    match direction.to_ascii_lowercase().as_str() {
        "east" => "Easting",
        "north" => "Northing",
        "west" => "Westing",
        "south" => "Southing",
        other => other,
    }
    .to_string()
}

fn default_axis_abbreviation(direction: &str) -> String {
    match direction.to_ascii_lowercase().as_str() {
        "east" => "E",
        "north" => "N",
        "west" => "W",
        "south" => "S",
        other => &other[..1.min(other.len())],
    }
    .to_string()
}

/// Format a coordinate/measure as a JSON number, avoiding a trailing `.0` on
/// integer-valued values (e.g. `6378137`, not `6378137.0`). Mirrors
/// `parquet::geo::fmt_num`.
fn number(v: f64) -> String {
    if v == v.trunc() && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ============================================================================
// REVERSE DIRECTION: PROJJSON -> WKT1, for `projjson_to_wkt` above.
//
// The input side needs no new parser: `crate::json::parse` already exists and
// is already used to read PROJJSON fields elsewhere (`crs/registry.rs`). The
// output side is new — WKT1 syntax, quoting, and node nesting differ enough
// from JSON's that the forward direction's `q()`/render_* functions (which
// build JSON strings) aren't reusable here.
// ============================================================================

/// Render a PROJJSON `GeographicCRS` object as a WKT1 `GEOGCS[...]` string.
/// Narrower than the forward direction's `render_geographic`: WKT1's
/// `SPHEROID`/`PRIMEM` carry no unit node (unlike WKT2's `ELLIPSOID`/
/// `PRIMEM`), so a non-metre ellipsoid axis or non-degree prime meridian has
/// no faithful WKT1 rendering — those return `None` rather than silently
/// mislabeling a unit. Datum *ensembles* are out of scope for the same reason
/// WKT1 has no `ENSEMBLE` keyword at all (see `plans/projjson-to-wkt.org`).
fn render_wkt1_geographic(pj: &JsonValue) -> Option<String> {
    let name = pj.get("name").and_then(JsonValue::as_str)?;
    let datum = pj.get("datum")?;
    let datum_name = datum.get("name").and_then(JsonValue::as_str).unwrap_or("unknown");
    let ellipsoid = wkt1_ellipsoid(datum)?;

    let mut out = format!("GEOGCS[{}", wkt1_str(name));
    out.push_str(&format!(",DATUM[{},{}]", wkt1_str(datum_name), ellipsoid));
    out.push_str(&wkt1_prime_meridian(datum)?);
    out.push_str(",UNIT[\"degree\",0.0174532925199433]");
    if let Some((authority, code)) = pj_id(pj) {
        out.push_str(&format!(",AUTHORITY[{},{}]", wkt1_str(&authority), wkt1_str(&code.to_string())));
    }
    out.push(']');
    Some(out)
}

/// `SPHEROID["name",semi_major_axis,inverse_flattening]`. See
/// [`render_wkt1_geographic`] for why a non-metre `semi_major_axis` returns
/// `None` rather than a wrong-unit number.
fn wkt1_ellipsoid(datum: &JsonValue) -> Option<String> {
    let ellipsoid = datum.get("ellipsoid")?;
    let name = ellipsoid.get("name").and_then(JsonValue::as_str).unwrap_or("unknown");
    let a = pj_length_metres(ellipsoid.get("semi_major_axis")?)?;
    let rf = match ellipsoid.get("inverse_flattening").and_then(JsonValue::as_f64) {
        Some(rf) => rf,
        // Mirrors `registry.rs`'s `projjson_ellipsoid`: some sources (Clarke
        // 1866, NAD27's ellipsoid) give `semi_minor_axis` instead.
        None => {
            let b = pj_length_metres(ellipsoid.get("semi_minor_axis")?)?;
            a / (a - b)
        }
    };
    Some(format!("SPHEROID[{},{},{}]", wkt1_str(name), number(a), number(rf)))
}

/// A PROJJSON length field's value in metres. A bare number is already
/// metres (PROJJSON's default unit for length fields); a `{value,unit}`
/// object is accepted only when its unit is `metre` — any other unit (US
/// survey foot, Clarke's foot, …) has no unit node to carry it faithfully in
/// WKT1's unit-less `SPHEROID`, so that returns `None`.
fn pj_length_metres(v: &JsonValue) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    let unit = v.get("unit").and_then(JsonValue::as_str)?;
    if unit != "metre" && unit != "meter" {
        return None;
    }
    v.get("value").and_then(JsonValue::as_f64)
}

/// `,PRIMEM["name",longitude]` — `Greenwich`/`0` when the source PROJJSON
/// omits `prime_meridian` entirely (PROJJSON's own default). Takes the
/// *datum* object, not the CRS: PROJJSON nests `prime_meridian` inside
/// `datum`, even though WKT1 lists `PRIMEM` as a sibling of `DATUM` (the
/// forward direction's `render_datum` has the identical asymmetry, see its
/// doc comment). WKT1's `PRIMEM` carries no unit node, so a longitude given
/// via the `{value,unit}` object form in a non-degree unit (e.g. grad, for
/// legacy French/African datums) has no faithful rendering here and returns
/// `None` — see § OPEN QUESTIONS in `plans/projjson-to-wkt.org`.
fn wkt1_prime_meridian(datum: &JsonValue) -> Option<String> {
    let Some(pm) = datum.get("prime_meridian") else {
        return Some(",PRIMEM[\"Greenwich\",0]".to_string());
    };
    let name = pm.get("name").and_then(JsonValue::as_str).unwrap_or("Greenwich");
    let lon_field = pm.get("longitude")?;
    let longitude = if let Some(n) = lon_field.as_f64() {
        n
    } else {
        let unit = lon_field.get("unit").and_then(JsonValue::as_str)?;
        if unit != "degree" {
            return None;
        }
        lon_field.get("value").and_then(JsonValue::as_f64)?
    };
    Some(format!(",PRIMEM[{},{}]", wkt1_str(name), number(longitude)))
}

/// A PROJJSON object's own top-level `id` as `(authority, code)` — a CRS's,
/// but equally a `conversion.method`'s or a `parameter`'s, since all three
/// carry the same `{"id":{"authority":...,"code":...}}` shape. Nested ids
/// further down (on a datum, an ellipsoid, ...) are not surfaced through this
/// path — the same scope as the forward direction's `node_authority_code`,
/// which only ever reads a node's *own* direct `AUTHORITY`/`ID` child.
fn pj_id(pj: &JsonValue) -> Option<(String, i64)> {
    let id = pj.get("id")?;
    let authority = id.get("authority").and_then(JsonValue::as_str)?.to_string();
    let code = id.get("code").and_then(JsonValue::as_f64)? as i64;
    Some((authority, code))
}

/// A WKT1 quoted string literal. Unlike JSON, WKT has no backslash-escape
/// convention (`tokenize_wkt`'s reader doesn't recognize one either — it
/// reads until the next `"` verbatim); CRS names in practice never carry an
/// embedded quote, so this does not attempt to escape one.
fn wkt1_str(s: &str) -> String {
    format!("\"{s}\"")
}

/// Render a PROJJSON `ProjectedCRS` object as a WKT1 `PROJCS[...]` string.
/// The method and every parameter carry an EPSG `id` in PROJJSON but a WKT1
/// `PROJECTION`/`PARAMETER` node only ever has a GDAL *name* — those come
/// from `wkt1_tables::method_by_code`/`param_by_code`, the reverse of the
/// crosswalk the forward direction (`render_projected_wkt1`) already
/// generates from. A method code absent from that table — one no WKT1 source
/// ever exercised — declines rather than guessing (see
/// `plans/projjson-to-wkt.org`), same as any parameter whose own unit doesn't
/// match what WKT1 needs (see [`wkt1_linear_unit`]).
fn render_wkt1_projected(pj: &JsonValue) -> Option<String> {
    let name = pj.get("name").and_then(JsonValue::as_str)?;
    let base_wkt = render_wkt1_geographic(pj.get("base_crs")?)?;

    let conversion = pj.get("conversion")?;
    let method = conversion.get("method")?;
    let method_code = pj_id(method)?.1;
    let method_name = super::wkt1_tables::method_name_by_code(method_code)?;

    let (unit_name, unit_factor) = wkt1_linear_unit(pj)?;

    let mut param_wkt = String::new();
    for param in conversion.get("parameters")?.as_array()? {
        let (_, epsg_param_code) = pj_id(param)?;
        let (wkt1_pname, kind) = super::wkt1_tables::param_by_code(method_code, epsg_param_code)?;
        let value = param.get("value").and_then(JsonValue::as_f64)?;
        let unit = param.get("unit")?;
        match kind {
            UnitKind::Angular if unit.as_str() == Some("degree") => {}
            UnitKind::Scale if unit.as_str() == Some("unity") => {}
            UnitKind::Linear => {
                let (n, f) = linear_unit_name_factor(unit)?;
                if n != unit_name || f != unit_factor {
                    return None;
                }
            }
            _ => return None,
        }
        param_wkt.push_str(&format!(",PARAMETER[{},{}]", wkt1_str(wkt1_pname), number(value)));
    }
    if param_wkt.is_empty() {
        return None;
    }

    let mut out = format!("PROJCS[{},", wkt1_str(name));
    out.push_str(&base_wkt);
    out.push_str(&format!(",PROJECTION[{}]", wkt1_str(method_name)));
    out.push_str(&param_wkt);
    out.push_str(&format!(",UNIT[{},{}]", wkt1_str(&unit_name), number(unit_factor)));
    // Explicit AXIS nodes, always — never the WKT1 no-AXIS convention that
    // implicitly assumes easting,northing. The forward direction
    // (`render_projected_wkt1`) deliberately distrusts that assumption for
    // `en_default_safe = false` methods (a source WKT1 omitting AXIS there
    // is refused, not guessed); emitting it unconditionally here keeps this
    // writer's own output identifiable by that same forward direction, and
    // matches what real GDAL WKT1 export always includes (see `CALIF1_WKT1`).
    out.push_str(&wkt1_axes(pj)?);
    if let Some((authority, code)) = pj_id(pj) {
        out.push_str(&format!(",AUTHORITY[{},{}]", wkt1_str(&authority), wkt1_str(&code.to_string())));
    }
    out.push(']');
    Some(out)
}

/// `,AXIS["name",DIRECTION]` for each axis of the projected CRS's Cartesian
/// coordinate system, in order — WKT1's direction token is a bare uppercase
/// word (`EAST`, `NORTH`, ...), unlike PROJJSON's lowercase string.
fn wkt1_axes(pj: &JsonValue) -> Option<String> {
    let axes = pj.get("coordinate_system")?.get("axis")?.as_array()?;
    let mut out = String::new();
    for axis in axes {
        let name = axis.get("name").and_then(JsonValue::as_str)?;
        let direction = axis.get("direction").and_then(JsonValue::as_str)?;
        out.push_str(&format!(",AXIS[{},{}]", wkt1_str(name), direction.to_ascii_uppercase()));
    }
    Some(out)
}

/// The projected CRS's governing linear unit — WKT1 stores every linear
/// parameter and the coordinate system itself in one shared unit (unlike
/// PROJJSON, where each parameter carries its own) — read from the Cartesian
/// coordinate system's first axis, the same source `render_cartesian`/
/// `render_axis` use going the other way.
fn wkt1_linear_unit(pj: &JsonValue) -> Option<(String, f64)> {
    let axis0 = pj.get("coordinate_system")?.get("axis")?.as_array()?.first()?;
    linear_unit_name_factor(axis0.get("unit")?)
}

/// A PROJJSON unit value's `(name, conversion_factor)` when it is a *linear*
/// unit — the bare string `"metre"` (factor `1`), or a `{type,name,
/// conversion_factor}` object for a non-metre unit (US survey foot, …).
/// `None` for `"degree"`/`"unity"` or anything not linear.
fn linear_unit_name_factor(unit: &JsonValue) -> Option<(String, f64)> {
    if let Some(s) = unit.as_str() {
        return (s == "metre").then(|| ("metre".to_string(), 1.0));
    }
    let name = unit.get("name").and_then(JsonValue::as_str)?.to_string();
    let factor = unit.get("conversion_factor").and_then(JsonValue::as_f64)?;
    Some((name, factor))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The real GeoPackage definition for EPSG:7844 (GDA2020), verbatim WKT1.
    const GDA2020_WKT1: &str = r#"GEOGCS["GDA2020",DATUM["Geocentric_Datum_of_Australia_2020",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6283"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","7844"]]"#;

    #[test]
    fn translates_wkt1_geographic_faithfully() {
        let pj = wkt_to_projjson(GDA2020_WKT1).unwrap();
        // A valid, complete GeographicCRS carrying the source's own fields…
        assert!(pj.contains("\"type\":\"GeographicCRS\""), "{pj}");
        assert!(pj.contains("\"name\":\"GDA2020\""), "{pj}");
        assert!(pj.contains("\"name\":\"Geocentric_Datum_of_Australia_2020\""), "{pj}");
        assert!(pj.contains("\"semi_major_axis\":6378137"), "{pj}");
        assert!(pj.contains("\"inverse_flattening\":298.257222101"), "{pj}");
        // …tagged with the *root* authority id, not the ellipsoid's 7019.
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":7844}"), "{pj}");
        // Parseable as JSON (well-formed object).
        assert!(crate::json::parse(&pj).is_ok(), "{pj}");
    }

    #[test]
    fn uses_authoritative_latitude_first_axis_order() {
        // Identity-critical: PROJ/GDAL/QGIS only recognize the EPSG code when the
        // axis order matches the authoritative definition (latitude, longitude).
        // A longitude-first order drops identification to ~25% and QGIS shows a
        // bare name instead of "EPSG:7844". GeoParquet's x,y storage is handled
        // by the reader convention, so this does not flip coordinates.
        let pj = wkt_to_projjson(GDA2020_WKT1).unwrap();
        let lat = pj.find("Geodetic latitude").expect("latitude axis present");
        let lon = pj.find("Geodetic longitude").expect("longitude axis present");
        assert!(lat < lon, "latitude axis must come first: {pj}");
    }

    #[test]
    fn translates_wkt2_geographic() {
        let wkt = r#"GEOGCRS["GDA2020",DATUM["GDA2020",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],CS[ellipsoidal,2],ID["EPSG",7844]]"#;
        let pj = wkt_to_projjson(wkt).unwrap();
        assert!(pj.contains("\"type\":\"GeographicCRS\""), "{pj}");
        assert!(pj.contains("\"code\":7844"), "{pj}");
        assert!(crate::json::parse(&pj).is_ok(), "{pj}");
    }

    #[test]
    fn no_id_still_produces_valid_projjson() {
        let wkt = r#"GEOGCS["custom",DATUM["custom datum",SPHEROID["custom",6378137,298.25]]]"#;
        let pj = wkt_to_projjson(wkt).unwrap();
        assert!(!pj.contains("\"id\""), "{pj}");
        assert!(pj.contains("\"semi_major_axis\":6378137"), "{pj}");
        assert!(crate::json::parse(&pj).is_ok(), "{pj}");
    }

    // Real `projinfo -o WKT2:2019` for EPSG:32755 (UTM 55S), trimmed of USAGE and
    // most ensemble members. Base is a datum *ensemble* (WGS 84).
    const UTM55S_WKT2: &str = r#"PROJCRS["WGS 84 / UTM zone 55S",BASEGEOGCRS["WGS 84",ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (Transit)"],MEMBER["World Geodetic System 1984 (G2296)"],ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ENSEMBLEACCURACY[2.0]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",4326]],CONVERSION["UTM zone 55S",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",147,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",10000000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]],ID["EPSG",32755]]"#;

    // Real `projinfo -o WKT2:2019` for EPSG:7855 (GDA2020 / MGA zone 55), trimmed.
    // Base is a plain `DATUM` (not an ensemble).
    const MGA55_WKT2: &str = r#"PROJCRS["GDA2020 / MGA zone 55",BASEGEOGCRS["GDA2020",DATUM["Geocentric Datum of Australia 2020",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",7844]],CONVERSION["Map Grid of Australia zone 55",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",147,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",10000000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]],ID["EPSG",7855]]"#;

    #[test]
    fn translates_wkt2_projected_ensemble_base() {
        let pj = wkt_to_projjson(UTM55S_WKT2).unwrap();
        assert!(pj.contains("\"type\":\"ProjectedCRS\""), "{pj}");
        assert!(pj.contains("\"name\":\"WGS 84 / UTM zone 55S\""), "{pj}");
        // Base geographic CRS rendered as a datum *ensemble*: members omitted, but
        // `accuracy` is required by PROJJSON and kept from ENSEMBLEACCURACY.
        assert!(pj.contains("\"datum_ensemble\":{\"name\":\"World Geodetic System 1984 ensemble\""), "{pj}");
        assert!(pj.contains("\"accuracy\":\"2.0\""), "accuracy required on ensemble: {pj}");
        assert!(!pj.contains("\"members\""), "members should be omitted: {pj}");
        // Conversion: method + parameters, each with its inline EPSG id.
        assert!(pj.contains("\"method\":{\"name\":\"Transverse Mercator\",\"id\":{\"authority\":\"EPSG\",\"code\":9807}}"), "{pj}");
        assert!(pj.contains("\"name\":\"Longitude of natural origin\",\"value\":147,\"unit\":\"degree\",\"id\":{\"authority\":\"EPSG\",\"code\":8802}"), "{pj}");
        assert!(pj.contains("\"name\":\"Scale factor at natural origin\",\"value\":0.9996,\"unit\":\"unity\""), "{pj}");
        assert!(pj.contains("\"name\":\"False easting\",\"value\":500000,\"unit\":\"metre\""), "{pj}");
        // Cartesian CS + the projected CRS's *own* id (not the base's 4326).
        assert!(pj.contains("\"subtype\":\"Cartesian\""), "{pj}");
        assert!(pj.ends_with("\"id\":{\"authority\":\"EPSG\",\"code\":32755}}"), "{pj}");
        assert!(crate::json::parse(&pj).is_ok(), "{pj}");
    }

    #[test]
    fn translates_wkt2_projected_plain_datum_base() {
        let pj = wkt_to_projjson(MGA55_WKT2).unwrap();
        assert!(pj.contains("\"type\":\"ProjectedCRS\""), "{pj}");
        // Plain datum base carrying its own id (7844), distinct from the root (7855).
        assert!(pj.contains("\"datum\":{\"type\":\"GeodeticReferenceFrame\",\"name\":\"Geocentric Datum of Australia 2020\""), "{pj}");
        assert!(pj.contains("\"id\":{\"authority\":\"EPSG\",\"code\":7844}"), "{pj}");
        assert!(pj.ends_with("\"id\":{\"authority\":\"EPSG\",\"code\":7855}}"), "{pj}");
        assert!(crate::json::parse(&pj).is_ok(), "{pj}");
    }

    // Real `projinfo -o WKT1_GDAL` for EPSG:2225 (NAD83 / California zone 1, US
    // survey feet) — a WKT1 `PROJCS`: LCC 2SP, feet linear unit, explicit AXIS.
    const CALIF1_WKT1: &str = r#"PROJCS["NAD83 / California zone 1 (ftUS)",GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4269"]],PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["latitude_of_origin",39.3333333333333],PARAMETER["central_meridian",-122],PARAMETER["standard_parallel_1",41.6666666666667],PARAMETER["standard_parallel_2",40],PARAMETER["false_easting",6561666.667],PARAMETER["false_northing",1640416.667],UNIT["US survey foot",0.304800609601219,AUTHORITY["EPSG","9003"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","2225"]]"#;

    #[test]
    fn translates_wkt1_projected_via_crosswalk_tables() {
        let pj = wkt_to_projjson(CALIF1_WKT1).unwrap();
        assert!(pj.contains("\"type\":\"ProjectedCRS\""), "{pj}");
        // GDAL method name resolved to canonical name + EPSG method code.
        assert!(pj.contains("\"method\":{\"name\":\"Lambert Conic Conformal (2SP)\",\"id\":{\"authority\":\"EPSG\",\"code\":9802}}"), "{pj}");
        // Angular param (degree, from the base) with its lifted canonical name + id.
        assert!(pj.contains("\"name\":\"Latitude of false origin\",\"value\":39.3333333333333,\"unit\":\"degree\",\"id\":{\"authority\":\"EPSG\",\"code\":8821}"), "{pj}");
        // Linear param carries the projected US-survey-foot unit object.
        assert!(pj.contains("\"name\":\"Easting at false origin\",\"value\":6561666.667,\"unit\":{\"type\":\"LinearUnit\",\"name\":\"US survey foot\""), "{pj}");
        // Cartesian axes use the same foot unit; root id is the PROJCS's own.
        assert!(pj.contains("\"subtype\":\"Cartesian\""), "{pj}");
        assert!(pj.ends_with("\"id\":{\"authority\":\"EPSG\",\"code\":2225}}"), "{pj}");
        assert!(crate::json::parse(&pj).is_ok(), "{pj}");
    }

    #[test]
    fn wkt1_unknown_method_falls_back() {
        // A method absent from the crosswalk tables (here an ambiguous one that is
        // deliberately excluded) must return None, never a guess.
        let wkt1 = r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic"],PARAMETER["latitude_of_origin",-71],PARAMETER["central_meridian",0],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]]"#;
        assert_eq!(wkt_to_projjson(wkt1), None);
    }

    // The crosswalk *oracle*: run the translator over a broad slice of the real
    // EPSG registry (generated by the `gen_*` fixture scripts) and gate every
    // output through `projinfo --identify`, requiring a 100%-confidence match to
    // the source's own code. This is the milestone-C "100% gate per method",
    // applied in bulk rather than one hand-written fixture at a time. Ignored by
    // default: it shells out to PROJ's `projinfo`.
    fn run_bulk_oracle(fixtures: &str) {
        use std::process::Command;
        let data = std::fs::read_to_string(fixtures).expect("fixtures present");

        let (mut ok, mut failures) = (0u32, Vec::new());
        for line in data.lines().filter(|l| !l.trim().is_empty()) {
            let (code, wkt) = line.split_once('\t').expect("code<TAB>wkt");
            let Some(pj) = wkt_to_projjson(wkt) else {
                failures.push(format!("EPSG:{code}: not translated"));
                continue;
            };
            assert!(crate::json::parse(&pj).is_ok(), "invalid JSON for EPSG:{code}: {pj}");
            let out = Command::new("projinfo")
                .arg("--identify")
                .arg(&pj)
                .output()
                .expect("run projinfo");
            let text = String::from_utf8_lossy(&out.stdout);
            if text.contains(&format!("EPSG:{code}: 100 %")) {
                ok += 1;
            } else {
                let got = text.lines().find(|l| l.contains("EPSG:")).unwrap_or("(no match)");
                failures.push(format!("EPSG:{code}: expected 100%, got `{}`", got.trim()));
            }
        }
        eprintln!("bulk oracle: {ok} identified@100%, {} failed", failures.len());
        assert!(
            failures.is_empty(),
            "{} of {} fixtures failed:\n{}",
            failures.len(),
            ok as usize + failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    #[ignore = "manual: bulk oracle, needs `projinfo` (PROJ) on PATH"]
    fn bulk_oracle_wkt2_projected_identifies() {
        run_bulk_oracle(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/projected_wkt2.tsv"));
    }

    #[test]
    #[ignore = "manual: bulk oracle, needs `projinfo` (PROJ) on PATH"]
    fn bulk_oracle_wkt1_projected_identifies() {
        run_bulk_oracle(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/projected_wkt1.tsv"));
    }

    #[test]
    fn declines_incomplete_or_unsupported_wkt() {
        // No datum/ellipsoid to render faithfully.
        assert_eq!(wkt_to_projjson("GEOGCS[\"anonymous\"]"), None);
        // WKT1 projected but incomplete (no PROJECTION/PARAMETER nodes) — nothing
        // to resolve against the crosswalk tables, so fall back rather than guess.
        assert_eq!(
            wkt_to_projjson("PROJCS[\"WGS 84 / UTM zone 55S\",GEOGCS[\"WGS 84\"]]"),
            None
        );
        // WKT2 projected but method lacks an inline id — refuse rather than emit a
        // CRS PROJ cannot identify.
        assert_eq!(
            wkt_to_projjson(r#"PROJCRS["x",BASEGEOGCRS["WGS 84",DATUM["d",ELLIPSOID["e",6378137,298.257223563]]],CONVERSION["c",METHOD["Transverse Mercator"],PARAMETER["False easting",500000,LENGTHUNIT["metre",1]]]]"#),
            None
        );
        // Not WKT at all.
        assert_eq!(wkt_to_projjson("not wkt"), None);
    }

    // --- projjson_to_wkt (reverse direction) ---

    #[test]
    fn projjson_to_wkt_renders_geographic_with_id() {
        let pj = wkt_to_projjson(GDA2020_WKT1).unwrap();
        let wkt = projjson_to_wkt(&pj).unwrap();
        assert!(wkt.starts_with("GEOGCS[\"GDA2020\""), "{wkt}");
        assert!(wkt.contains("DATUM[\"Geocentric_Datum_of_Australia_2020\""), "{wkt}");
        assert!(wkt.contains("SPHEROID[\"GRS 1980\",6378137,298.257222101]"), "{wkt}");
        assert!(wkt.contains("PRIMEM[\"Greenwich\",0]"), "{wkt}");
        assert!(wkt.contains("UNIT[\"degree\",0.0174532925199433]"), "{wkt}");
        assert!(wkt.ends_with("AUTHORITY[\"EPSG\",\"7844\"]]"), "{wkt}");
    }

    #[test]
    fn projjson_to_wkt_round_trips_id_less_geographic() {
        // No `id` anywhere — the actual target case: a custom/authority-
        // stripped PROJJSON with nothing a registry lookup could key on.
        let pj = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"custom datum","ellipsoid":{"name":"custom ellipsoid","semi_major_axis":6378137,"inverse_flattening":298.257223563}},"coordinate_system":{"subtype":"ellipsoidal","axis":[]}}"#;
        let wkt = projjson_to_wkt(pj).unwrap();
        assert!(!wkt.contains("AUTHORITY"), "no id to round-trip: {wkt}");
        assert!(wkt.contains("SPHEROID[\"custom ellipsoid\",6378137,298.257223563]"), "{wkt}");

        // Feeding the emitted WKT1 back through the forward direction should
        // recover the same datum/ellipsoid fields (not necessarily the same
        // JSON text — field order may differ).
        let pj_back = wkt_to_projjson(&wkt).unwrap();
        assert!(pj_back.contains("\"name\":\"custom datum\""), "{pj_back}");
        assert!(pj_back.contains("\"semi_major_axis\":6378137"), "{pj_back}");
        assert!(pj_back.contains("\"inverse_flattening\":298.257223563"), "{pj_back}");
    }

    #[test]
    fn projjson_to_wkt_renders_non_greenwich_prime_meridian() {
        let pj = r#"{"type":"GeographicCRS","name":"custom Paris","datum":{"type":"GeodeticReferenceFrame","name":"custom datum","ellipsoid":{"name":"Clarke 1880 (IGN)","semi_major_axis":6378249.2,"inverse_flattening":293.4660212936269},"prime_meridian":{"name":"Paris","longitude":2.5969213}}}"#;
        // (`prime_meridian` nests inside `datum` — see `wkt1_prime_meridian`'s doc.)
        let wkt = projjson_to_wkt(pj).unwrap();
        assert!(wkt.contains("PRIMEM[\"Paris\",2.5969213]"), "{wkt}");
    }

    #[test]
    fn projjson_to_wkt_declines_ensemble_and_nonmetre_ellipsoid() {
        // Datum ensembles have no WKT1 equivalent (no ENSEMBLE keyword).
        let ensemble = r#"{"type":"GeographicCRS","name":"WGS 84","datum_ensemble":{"name":"World Geodetic System 1984 ensemble","members":[],"ellipsoid":{"name":"WGS 84","semi_major_axis":6378137,"inverse_flattening":298.257223563},"accuracy":"2.0"}}"#;
        assert_eq!(projjson_to_wkt(ensemble), None);

        // A non-metre semi_major_axis has no unit node to carry it in WKT1's
        // unit-less SPHEROID — refuse rather than mislabel the ellipsoid.
        let non_metre = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"d","ellipsoid":{"name":"e","semi_major_axis":{"type":"LinearUnit","name":"US survey foot","value":20925832.16,"conversion_factor":0.304800609601219},"inverse_flattening":294.978698214}}}"#;
        assert_eq!(projjson_to_wkt(non_metre), None);

        // A non-degree prime meridian likewise has nowhere to carry its unit
        // (`prime_meridian` nests inside `datum` — see `wkt1_prime_meridian`).
        let grad_pm = r#"{"type":"GeographicCRS","name":"custom","datum":{"type":"GeodeticReferenceFrame","name":"d","ellipsoid":{"name":"e","semi_major_axis":6378249.2,"inverse_flattening":293.466021293},"prime_meridian":{"name":"Paris","longitude":{"value":2.5969213,"unit":"grad"}}}}"#;
        assert_eq!(projjson_to_wkt(grad_pm), None);

        // A ProjectedCRS missing everything a render needs (no base_crs, no
        // conversion) — incomplete input declines rather than guessing.
        let projected = r#"{"type":"ProjectedCRS","name":"x"}"#;
        assert_eq!(projjson_to_wkt(projected), None);
        // Some other/unhandled PROJJSON `type` entirely.
        assert_eq!(projjson_to_wkt(r#"{"type":"GeodeticCRS","name":"x"}"#), None);

        // Not valid JSON.
        assert_eq!(projjson_to_wkt("not json"), None);
    }

    #[test]
    #[ignore = "manual: needs `projinfo` (PROJ) on PATH"]
    fn projjson_to_wkt_oracle_identifies() {
        use std::process::Command;
        let pj = wkt_to_projjson(GDA2020_WKT1).unwrap();
        let wkt = projjson_to_wkt(&pj).unwrap();
        let out = Command::new("projinfo")
            .arg("--identify")
            .arg(&wkt)
            .output()
            .expect("run projinfo");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("EPSG:7844: 100 %"), "{text}\nwkt: {wkt}");
    }

    #[test]
    fn projjson_to_wkt_renders_projected_with_metre_units() {
        // Round trip through the forward direction's own MGA55 fixture (a
        // plain-datum base, all-metre Transverse Mercator projection).
        let pj = wkt_to_projjson(MGA55_WKT2).unwrap();
        let wkt = projjson_to_wkt(&pj).unwrap();
        assert!(wkt.starts_with("PROJCS[\"GDA2020 / MGA zone 55\""), "{wkt}");
        assert!(wkt.contains("GEOGCS[\"GDA2020\""), "{wkt}");
        assert!(wkt.contains("PROJECTION[\"Transverse_Mercator\"]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"central_meridian\",147]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"scale_factor\",0.9996]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"false_easting\",500000]"), "{wkt}");
        assert!(wkt.contains("UNIT[\"metre\",1]"), "{wkt}");
        assert!(wkt.ends_with("AUTHORITY[\"EPSG\",\"7855\"]]"), "{wkt}");
    }

    #[test]
    fn projjson_to_wkt_renders_projected_with_non_metre_units() {
        // Round trip through the forward direction's own California-zone-1
        // fixture — exercises the non-metre (US survey foot) governing linear
        // unit path, not just the common metre case.
        let pj = wkt_to_projjson(CALIF1_WKT1).unwrap();
        let wkt = projjson_to_wkt(&pj).unwrap();
        assert!(wkt.starts_with("PROJCS[\"NAD83 / California zone 1 (ftUS)\""), "{wkt}");
        assert!(wkt.contains("PROJECTION[\"Lambert_Conformal_Conic_2SP\"]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"false_easting\",6561666.667]"), "{wkt}");
        assert!(wkt.contains("UNIT[\"US survey foot\",0.304800609601219]"), "{wkt}");
        assert!(wkt.ends_with("AUTHORITY[\"EPSG\",\"2225\"]]"), "{wkt}");
    }

    #[test]
    fn projjson_to_wkt_round_trips_id_less_projected() {
        // The real target case (see `plans/projjson-to-wkt.org`'s "How common
        // is the id-less-PROJJSON case" finding): a local/custom base datum
        // with a standard, recognized projection method — real GDAL output
        // for exactly this shape has no top-level `id`, but the method and
        // every parameter still carry their EPSG ids.
        let pj = r#"{"type":"ProjectedCRS","name":"My Custom Projection","base_crs":{"type":"GeographicCRS","name":"My Custom Datum","datum":{"type":"GeodeticReferenceFrame","name":"My_Custom_Datum","ellipsoid":{"name":"My Ellipsoid","semi_major_axis":6378137,"inverse_flattening":299}}},"conversion":{"name":"UTM zone 18N","method":{"name":"Transverse Mercator","id":{"authority":"EPSG","code":9807}},"parameters":[{"name":"Latitude of natural origin","value":0,"unit":"degree","id":{"authority":"EPSG","code":8801}},{"name":"Longitude of natural origin","value":-75,"unit":"degree","id":{"authority":"EPSG","code":8802}},{"name":"Scale factor at natural origin","value":0.9996,"unit":"unity","id":{"authority":"EPSG","code":8805}},{"name":"False easting","value":500000,"unit":"metre","id":{"authority":"EPSG","code":8806}},{"name":"False northing","value":0,"unit":"metre","id":{"authority":"EPSG","code":8807}}]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"E","direction":"east","unit":"metre"},{"name":"Northing","abbreviation":"N","direction":"north","unit":"metre"}]}}"#;
        let wkt = projjson_to_wkt(pj).unwrap();
        assert!(!wkt.contains("AUTHORITY"), "no id anywhere to round-trip: {wkt}");
        assert!(wkt.contains("GEOGCS[\"My Custom Datum\""), "{wkt}");
        assert!(wkt.contains("PROJECTION[\"Transverse_Mercator\"]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"central_meridian\",-75]"), "{wkt}");
        assert!(wkt.contains("PARAMETER[\"scale_factor\",0.9996]"), "{wkt}");

        // GDAL/ogrinfo reads it back without error and reports the same
        // ellipsoid and projection parameters (the target case has no
        // registered code, so `projinfo --identify` can't confirm identity —
        // this is a syntactic/faithfulness check instead).
        let pj_back = wkt_to_projjson(&wkt).unwrap();
        assert!(pj_back.contains("\"semi_major_axis\":6378137"), "{pj_back}");
        assert!(pj_back.contains("\"method\":{\"name\":\"Transverse Mercator\""), "{pj_back}");
        assert!(pj_back.contains("\"name\":\"Longitude of natural origin\",\"value\":-75"), "{pj_back}");
    }

    #[test]
    fn projjson_to_wkt_declines_unresolvable_projected_cases() {
        // A method code absent from `wkt1_tables.rs` — no WKT1 source ever
        // exercised it, so there is no GDAL name to emit. (EPSG:1024,
        // Pseudo Mercator, is a real method not in the crosswalk.)
        let unknown_method = r#"{"type":"ProjectedCRS","name":"x","base_crs":{"type":"GeographicCRS","name":"WGS 84","datum":{"type":"GeodeticReferenceFrame","name":"d","ellipsoid":{"name":"e","semi_major_axis":6378137,"inverse_flattening":298.257223563}}},"conversion":{"name":"c","method":{"name":"Popular Visualisation Pseudo-Mercator","id":{"authority":"EPSG","code":1024}},"parameters":[{"name":"False easting","value":0,"unit":"metre","id":{"authority":"EPSG","code":8806}}]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"E","direction":"east","unit":"metre"}]}}"#;
        assert_eq!(projjson_to_wkt(unknown_method), None);

        // A linear parameter whose own unit doesn't match the CRS's governing
        // linear unit (metre param under a US-survey-foot CRS) — refuse
        // rather than silently mix units.
        let mismatched_unit = r#"{"type":"ProjectedCRS","name":"x","base_crs":{"type":"GeographicCRS","name":"WGS 84","datum":{"type":"GeodeticReferenceFrame","name":"d","ellipsoid":{"name":"e","semi_major_axis":6378137,"inverse_flattening":298.257223563}}},"conversion":{"name":"c","method":{"name":"Transverse Mercator","id":{"authority":"EPSG","code":9807}},"parameters":[{"name":"False easting","value":500000,"unit":"metre","id":{"authority":"EPSG","code":8806}}]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"E","direction":"east","unit":{"type":"LinearUnit","name":"US survey foot","conversion_factor":0.304800609601219}}]}}"#;
        assert_eq!(projjson_to_wkt(mismatched_unit), None);

        // No parameters at all — nothing to render.
        let no_params = r#"{"type":"ProjectedCRS","name":"x","base_crs":{"type":"GeographicCRS","name":"WGS 84","datum":{"type":"GeodeticReferenceFrame","name":"d","ellipsoid":{"name":"e","semi_major_axis":6378137,"inverse_flattening":298.257223563}}},"conversion":{"name":"c","method":{"name":"Transverse Mercator","id":{"authority":"EPSG","code":9807}},"parameters":[]},"coordinate_system":{"subtype":"Cartesian","axis":[{"name":"Easting","abbreviation":"E","direction":"east","unit":"metre"}]}}"#;
        assert_eq!(projjson_to_wkt(no_params), None);
    }

    #[test]
    #[ignore = "manual: needs `projinfo` (PROJ) on PATH"]
    fn projjson_to_wkt_projected_oracle_identifies() {
        use std::process::Command;
        for (fixture, expect) in [(MGA55_WKT2, "EPSG:7855"), (CALIF1_WKT1, "EPSG:2225")] {
            let pj = wkt_to_projjson(fixture).unwrap();
            let wkt = projjson_to_wkt(&pj).unwrap();
            let out = Command::new("projinfo")
                .arg("--identify")
                .arg(&wkt)
                .output()
                .expect("run projinfo");
            let text = String::from_utf8_lossy(&out.stdout);
            assert!(text.contains(&format!("{expect}: 100 %")), "{text}\nwkt: {wkt}");
        }
    }
}
