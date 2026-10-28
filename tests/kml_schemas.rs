//! The KML schema oracle: every kind of KML write geosetta builds, validated
//! against the version's own schema (`plans/kml-standard.org` § ORACLES, K5's
//! "XSD validation of every write").
//!
//! - *2.2* against `ogckml22.xsd` with `kml22gx.xsd`, by `xmllint`. The `gx:`
//!   schema is loaded too because the `gx:` elements are in substitution
//!   groups, not open content: without it a valid `gx:Track` is unexpected.
//! - *2.3* against `ogckml23.xsd`, by Python's `xmlschema`: the schema is XSD
//!   1.1, which libxml2 does not implement.
//!
//! **Ignored, and never skipped silently**, like `gdal_types.rs`: the schemas
//! are not in the tree (decided 2026-10-23), so the run needs
//! `GEOSETTA_KML_SCHEMAS`, a directory holding `2.2.0/ogckml22.xsd`,
//! `2.3/ogckml23.xsd`, `kml22gx.xsd`, `atom-author-link.xsd` and `xAL.xsd` —
//! the last two beside the schema that imports them or at the top. The
//! published schemas import one another by URL, so the test copies them into
//! a directory of its own with each import pointed at the local copy, and the
//! validators never touch the network. `GEOSETTA_KML_PYTHON` names the Python
//! to run (default `python3`), split on spaces, so
//! `uv run --no-project --with xmlschema python` works.
//!
//! *What is not validated*: a same-version write of a source that is itself
//! invalid. D-K5 gives such a document back as it was read, so
//! `hand_hostile.kml` — invalid 2.2 in its order, and read as 2.2 — is
//! written back invalid on purpose. Raised to 2.3, which orders nothing, it
//! is valid, and is checked.
//!
//! *KMZ writes are validated document by document*: every `.kml` entry of
//! each archive written, a layered archive's root and layers alike, read back
//! by walking the stored entries' local headers.

use std::path::{Path, PathBuf};
use std::process::Command;

use geosetta::convert::write_kml_features;
use geosetta::{FeatureCollection, Format, JsonValue, KmlVersion};

/// The five files, each with where it is published.
const SCHEMAS: [(&str, &str); 5] = [
    ("2.2.0/ogckml22.xsd", "https://schemas.opengis.net/kml/2.2.0/ogckml22.xsd"),
    ("2.3/ogckml23.xsd", "https://schemas.opengis.net/kml/2.3/ogckml23.xsd"),
    ("kml22gx.xsd", "https://developers.google.com/kml/schema/kml22gx.xsd"),
    ("atom-author-link.xsd", "https://schemas.opengis.net/kml/2.2.0/atom-author-link.xsd"),
    ("xAL.xsd", "http://docs.oasis-open.org/election/external/xAL.xsd"),
];

const XAL_URL: &str = "http://docs.oasis-open.org/election/external/xAL.xsd";
const KML22_URL: &str = "http://schemas.opengis.net/kml/2.2.0/ogckml22.xsd";

fn tmp_dir() -> PathBuf {
    let label = std::thread::current().name().unwrap_or("main").replace("::", "-");
    let dir = std::env::temp_dir().join(format!("geosetta-kml-schemas-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn missing_schemas(why: &str) -> ! {
    let list: Vec<String> = SCHEMAS.iter().map(|(f, url)| format!("  {f}  ({url})")).collect();
    panic!(
        "{why}\nset GEOSETTA_KML_SCHEMAS to a directory holding:\n{}\n\
         atom-author-link.xsd and xAL.xsd may sit beside the schema importing them or at the top",
        list.join("\n")
    )
}

/// `name` beside `schema` in `root`, else at `root`'s top.
fn find(root: &Path, schema: &str, name: &str) -> PathBuf {
    let beside = root.join(schema).parent().unwrap().join(name);
    if beside.is_file() {
        return beside;
    }
    let top = root.join(name);
    if top.is_file() {
        return top;
    }
    missing_schemas(&format!("{name} is in neither {} nor {}", beside.display(), top.display()))
}

/// `from` copied to `to` with `url` pointed at `local`: once, or the schema is
/// not the one this test was written against.
fn copy_rewritten(from: &Path, to: &Path, url: &str, local: &str) {
    let text = std::fs::read_to_string(from).unwrap_or_else(|e| missing_schemas(&format!("{}: {e}", from.display())));
    assert_eq!(text.matches(url).count(), 1, "{} should import {url} once", from.display());
    std::fs::write(to, text.replace(url, local)).unwrap();
}

/// The two oracles' entry schemas, in a directory of their own: 2.2's
/// (`kml22gx.xsd`, importing `ogckml22.xsd`) and 2.3's.
fn oracles() -> (PathBuf, PathBuf) {
    let root = PathBuf::from(
        std::env::var("GEOSETTA_KML_SCHEMAS").unwrap_or_else(|_| missing_schemas("GEOSETTA_KML_SCHEMAS is not set")),
    );
    let dir = tmp_dir();
    let (v22, v23) = (dir.join("2.2"), dir.join("2.3"));
    for (out, schema) in [(&v22, "2.2.0/ogckml22.xsd"), (&v23, "2.3/ogckml23.xsd")] {
        std::fs::create_dir_all(out).unwrap();
        for import in ["atom-author-link.xsd", "xAL.xsd"] {
            std::fs::copy(find(&root, schema, import), out.join(import)).unwrap();
        }
        let name = Path::new(schema).file_name().unwrap();
        copy_rewritten(&root.join(schema), &out.join(name), XAL_URL, "xAL.xsd");
    }
    copy_rewritten(&root.join("kml22gx.xsd"), &v22.join("kml22gx.xsd"), KML22_URL, "ogckml22.xsd");
    (v22.join("kml22gx.xsd"), v23.join("ogckml23.xsd"))
}

fn read(format: Format, path: &str) -> FeatureCollection {
    let bytes = std::fs::read(path).unwrap();
    geosetta::read_features(format, &bytes, &mut Vec::new()).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn set(fc: &mut FeatureCollection, i: usize, key: &str, value: Option<&str>) {
    let props = &mut fc.features[i].properties;
    let at = props.iter().position(|(k, _)| &**k == key);
    match (value, at) {
        (Some(v), Some(j)) => props[j].1 = JsonValue::String(v.into()),
        (Some(v), None) => props.push((key.into(), JsonValue::String(v.into()))),
        (None, Some(j)) => {
            props.remove(j);
        }
        (None, None) => {}
    }
}

/// An edit of each kind the reconcile handles: a renamed feature, a new
/// property, a sea-floor mode, a removed time, a moved geometry, a dropped
/// feature, and a feature added from another collection.
fn edited(mut fc: FeatureCollection, extra: &FeatureCollection) -> FeatureCollection {
    set(&mut fc, 0, "name", Some("renamed & edited"));
    set(&mut fc, 0, "added", Some("a new <Data>"));
    set(&mut fc, 0, "seaFloorAltitudeMode", Some("clampToSeaFloor"));
    set(&mut fc, 0, "timestamp", None);
    if let Some(g) = fc.features.last_mut().and_then(|f| f.geometry.as_mut()) {
        shift(g);
    }
    if fc.features.len() > 2 {
        fc.features.remove(1);
    }
    fc.features.push(extra.features[0].clone());
    fc
}

fn shift(g: &mut geosetta::Geometry) {
    use geosetta::Geometry as G;
    let nudge = |p: &mut geosetta::Position| p.x += 0.001;
    match g {
        G::Point(p) => nudge(p),
        G::LineString(ps) | G::MultiPoint(ps) => ps.iter_mut().for_each(nudge),
        G::Polygon(rs) | G::MultiLineString(rs) => rs.iter_mut().flatten().for_each(nudge),
        G::MultiPolygon(ps) => ps.iter_mut().flatten().flatten().for_each(nudge),
        G::GeometryCollection(gs) => gs.iter_mut().for_each(shift),
    }
}

/// Every kind of write, as (label, version written, bytes).
fn writes() -> Vec<(String, KmlVersion, Vec<u8>)> {
    let fresh = [
        ("hand_hostile.geojson", read(Format::GeoJson, "tests/fixtures/hand_hostile.geojson")),
        ("gdal_hostile.fgb", read(Format::FlatGeobuf, "tests/fixtures/gdal_hostile.fgb")),
    ];
    let extra = &fresh[0].1;
    let v22 = KmlVersion::V2_2;
    let v23 = KmlVersion::V2_3;
    let mut out = Vec::new();
    let mut add = |label: String, fc: &FeatureCollection, version: KmlVersion| {
        let bytes = write_kml_features(Format::Kml, fc, version).unwrap_or_else(|e| panic!("{label}: {e}"));
        out.push((label, version, bytes));
    };
    // Built from the feature model: a source in another format.
    for (name, fc) in &fresh {
        add(format!("{name} as 2.2"), fc, v22);
        add(format!("{name} as 2.3"), fc, v23);
    }
    // A valid source in its own version, as read and edited.
    for (path, version) in [
        ("tests/fixtures/sample.kml", v22),
        ("tests/fixtures/duckdb_geoms.kml", v22),
        ("tests/fixtures/hand_hostile_2_3.kml", v23),
    ] {
        let fc = read(Format::Kml, path);
        add(format!("{path} as itself"), &fc, version);
        add(format!("{path} edited"), &edited(fc, extra), version);
    }
    // The lowering (K5): as read, edited, and reordered.
    let source = read(Format::Kml, "tests/fixtures/hand_hostile_2_3.kml");
    add("hand_hostile_2_3.kml lowered".into(), &source, v22);
    add("hand_hostile_2_3.kml edited and lowered".into(), &edited(source.clone(), extra), v22);
    let mut sorted = source;
    geosetta::reorder_hilbert(&mut sorted);
    add("hand_hostile_2_3.kml sorted and lowered".into(), &sorted, v22);
    // The raising (K5): valid 2.2 sources, and hand_hostile.kml, invalid
    // 2.2 only in its order, which 2.3 does not fix; as read and edited.
    add("sample.kml raised".into(), &read(Format::Kml, "tests/fixtures/sample.kml"), v23);
    add("duckdb_geoms.kml raised".into(), &read(Format::Kml, "tests/fixtures/duckdb_geoms.kml"), v23);
    let hostile = read(Format::Kml, "tests/fixtures/hand_hostile.kml");
    add("hand_hostile.kml raised".into(), &hostile, v23);
    add("hand_hostile.kml edited and raised".into(), &edited(hostile, extra), v23);
    // A gx:-heavy 2.2 source: the 2.3 fixture lowered, read, and raised.
    let lowered = write_kml_features(Format::Kml, &read(Format::Kml, "tests/fixtures/hand_hostile_2_3.kml"), v22).unwrap();
    let lowered = geosetta::read_features(Format::Kml, &lowered, &mut Vec::new()).unwrap();
    add("hand_hostile_2_3.kml lowered and raised".into(), &lowered, v23);
    // KMZ (M3c): every .kml entry of each archive written, the layered
    // archive's network-link root and its layer alike. hand_hostile.kmz's
    // document is hand_hostile.kml, invalid 2.2 in its order, so only its
    // raise is checked.
    let networked = read(Format::Kmz, "tests/fixtures/gdal_networklink.kmz");
    let hostile_kmz = read(Format::Kmz, "tests/fixtures/hand_hostile.kmz");
    let from_kml = read(Format::Kml, "tests/fixtures/hand_hostile_2_3.kml");
    for (label, fc, version) in [
        ("gdal_networklink.kmz as itself", networked.clone(), v22),
        ("gdal_networklink.kmz raised", networked.clone(), v23),
        ("gdal_networklink.kmz edited", edited(networked, extra), v22),
        ("hand_hostile.kmz raised", hostile_kmz.clone(), v23),
        ("hand_hostile.kmz edited and raised", edited(hostile_kmz, extra), v23),
        ("hand_hostile_2_3.kml as a .kmz", from_kml.clone(), v23),
        ("hand_hostile_2_3.kml as a lowered .kmz", from_kml, v22),
    ] {
        let kmz = write_kml_features(Format::Kmz, &fc, version).unwrap_or_else(|e| panic!("{label}: {e}"));
        let documents = kml_entries(&kmz);
        assert!(!documents.is_empty(), "{label}: no .kml entry");
        for (name, bytes) in documents {
            out.push((format!("{label}, {name}"), version, bytes));
        }
    }
    out
}

/// The `.kml` entries of a `.kmz` geosetta wrote, by walking its local file
/// headers. geosetta stores every entry (method 0) with its sizes in the
/// local header, so nothing needs inflating; anything else is a test failure,
/// not a case to handle.
fn kml_entries(kmz: &[u8]) -> Vec<(String, Vec<u8>)> {
    let u16_at = |at: usize| u16::from_le_bytes([kmz[at], kmz[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(kmz[at..at + 4].try_into().unwrap()) as usize;
    let mut out = Vec::new();
    let mut at = 0;
    while kmz.len() >= at + 4 && u32_at(at) == 0x0403_4b50 {
        assert_eq!(u16_at(at + 8), 0, "geosetta writes stored entries");
        let size = u32_at(at + 18);
        let (name_len, extra_len) = (u16_at(at + 26), u16_at(at + 28));
        let name = String::from_utf8(kmz[at + 30..at + 30 + name_len].to_vec()).unwrap();
        let data = at + 30 + name_len + extra_len;
        if name.to_ascii_lowercase().ends_with(".kml") {
            out.push((name, kmz[data..data + size].to_vec()));
        }
        at = data + size;
    }
    out
}

/// The validator's verdict on `files`: each line `path: reason` for a file it
/// rejected.
fn rejected(mut cmd: Command, what: &str) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("could not run {what}: {e}"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    if out.status.success() {
        return String::new();
    }
    // xmllint also reports each file that passed.
    format!("{stdout}{stderr}").lines().filter(|l| !l.ends_with(" validates")).map(|l| format!("{l}\n")).collect()
}

const XMLSCHEMA_CHECK: &str = r#"
import sys
try:
    import xmlschema
except ImportError:
    sys.exit("xmlschema is not importable: install it, or set GEOSETTA_KML_PYTHON to a Python that has it")
schema = xmlschema.XMLSchema11(sys.argv[1])
bad = 0
for path in sys.argv[2:]:
    for e in list(schema.iter_errors(path))[:3]:
        print(f"{path}: {e.reason} at {e.path}")
        bad += 1
sys.exit(1 if bad else 0)
"#;

#[test]
#[ignore = "needs the KML schemas, xmllint and xmlschema; set GEOSETTA_KML_SCHEMAS and run with --ignored"]
fn every_kml_write_is_valid_in_its_version() {
    let (oracle22, oracle23) = oracles();
    let dir = tmp_dir();
    let mut files: [Vec<(String, PathBuf)>; 2] = [Vec::new(), Vec::new()];
    for (i, (label, version, bytes)) in writes().into_iter().enumerate() {
        let path = dir.join(format!("w{i:02}.kml"));
        std::fs::write(&path, bytes).unwrap();
        let slot = match version {
            KmlVersion::V2_2 => 0,
            KmlVersion::V2_3 => 1,
            other => panic!("KML {} has no schema oracle", other.as_str()),
        };
        files[slot].push((label, path));
    }
    let mut failures = String::new();
    let [v22, v23] = &files;

    let mut xmllint = Command::new("xmllint");
    xmllint.args(["--noout", "--nonet", "--schema"]).arg(&oracle22);
    xmllint.args(v22.iter().map(|(_, p)| p));
    failures += &rejected(xmllint, "xmllint");

    let python = std::env::var("GEOSETTA_KML_PYTHON").unwrap_or_else(|_| "python3".into());
    let mut words = python.split_whitespace();
    let mut xmlschema = Command::new(words.next().expect("GEOSETTA_KML_PYTHON is empty"));
    xmlschema.args(words).args(["-I", "-c", XMLSCHEMA_CHECK]).arg(&oracle23);
    xmlschema.args(v23.iter().map(|(_, p)| p));
    failures += &rejected(xmlschema, "the 2.3 validator");

    // Name each failing write by what it was, not its temp file.
    for (label, path) in v22.iter().chain(v23) {
        failures = failures.replace(&*path.to_string_lossy(), label);
    }
    assert!(failures.is_empty(), "writes the schemas reject:\n{failures}");
    assert_eq!((v22.len(), v23.len()), (14, 14), "the write list changed: update this count with it");
}
