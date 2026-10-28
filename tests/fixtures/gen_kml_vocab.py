#!/usr/bin/env python3
"""Generate the KML 2.3 vocabulary in src/kml/model.rs from ogckml23.xsd.

Usage: python3 -I gen_kml_vocab.py SCHEMA_DIR > vocab.rs

SCHEMA_DIR holds the schemas the KML oracle reads (plans/kml-standard.org
§ ORACLES): this script needs only 2.3/ogckml23.xsd beneath it, from
https://schemas.opengis.net/kml/2.3/ogckml23.xsd. The output replaces the
region of src/kml/model.rs between its "Generated" and "End of generated code"
markers, by hand; the schema's SHA-256 goes in that module's doc.

What it emits, all from the schema's global <element> declarations:

- KmlElement: one variant per concrete (non-abstract) element, 168 of them.
  KML 2.2's 145 are a subset (checked 2026-10-23), so 2.3's vocabulary is the
  model's.
- NAMES: (local name, variant), sorted bytewise for a binary search.
- KmlElement::name, and one is_* predicate per abstract group the model reads,
  each member found by following substitutionGroup to the group.

Variant names are the element names with the first letter upper-cased. Two
pairs collide that way -- snippet/Snippet and scale/Scale, different elements
-- and the capitalised one of each gets an "Upper" suffix.

The standard library is enough: xml.etree reads a schema fine.
"""

import sys
import xml.etree.ElementTree as ET

XS = "{http://www.w3.org/2001/XMLSchema}"

# Why each capitalised collision exists, for its doc comment.
UPPER = {
    "Snippet": "the feature summary's capitalised spelling, beside `<snippet>`",
    "Scale": "`<Model>`'s three-axis scale, beside the sub-styles' `<scale>`",
}

# The abstract groups the model asks about, as (predicate, group).
GROUPS = [
    ("is_geometry", "AbstractGeometryGroup"),
    ("is_feature", "AbstractFeatureGroup"),
    ("is_container", "AbstractContainerGroup"),
    ("is_overlay", "AbstractOverlayGroup"),
    ("is_style_selector", "AbstractStyleSelectorGroup"),
    ("is_sub_style", "AbstractSubStyleGroup"),
    ("is_time_primitive", "AbstractTimePrimitiveGroup"),
    ("is_view", "AbstractViewGroup"),
    ("is_tour_primitive", "AbstractTourPrimitiveGroup"),
]


def variant(name):
    if name in UPPER:
        return name + "Upper"
    return name[0].upper() + name[1:]


def main(schema_dir):
    root = ET.parse(schema_dir + "/2.3/ogckml23.xsd").getroot()
    els = {e.get("name"): e for e in root.findall(XS + "element")}
    concrete = sorted((n for n, e in els.items() if e.get("abstract") != "true"), key=lambda s: (s.lower(), s))
    variants = [variant(n) for n in concrete]
    assert len(set(variants)) == len(variants), "two elements map to one variant"

    def groups_of(name):
        out = []
        while sg := els[name].get("substitutionGroup"):
            name = sg.split(":")[1]
            out.append(name)
        return out

    o = []
    o.append("// --- Generated from ogckml23.xsd by tests/fixtures/gen_kml_vocab.py; do not edit by hand. ---\n\n")
    o.append("/// Every concrete element KML 2.3 defines: the 168 global, non-abstract\n")
    o.append("/// elements of `ogckml23.xsd`. KML 2.2's 145 are a subset.\n")
    o.append("#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]\n")
    o.append("pub(crate) enum KmlElement {\n")
    for n, v in zip(concrete, variants):
        if n in UPPER:
            o.append(f"    /// `<{n}>`: {UPPER[n]}.\n")
        o.append(f"    {v},\n")
    o.append("}\n\n")
    o.append("/// `(local name, element)`, sorted bytewise for [`KmlElement::from_name`].\n")
    o.append(f"const NAMES: [(&str, KmlElement); {len(concrete)}] = [\n")
    for n, v in sorted(zip(concrete, variants)):
        o.append(f'    ("{n}", KmlElement::{v}),\n')
    o.append("];\n\n")
    o.append("// Every group predicate is generated, whether or not the model reads it yet.\n")
    o.append("#[allow(dead_code)]\n")
    o.append("impl KmlElement {\n")
    o.append("    /// The element a KML-namespace local name denotes, if 2.3 defines one.\n")
    o.append("    pub fn from_name(name: &str) -> Option<KmlElement> {\n")
    o.append("        NAMES.binary_search_by(|(n, _)| (*n).cmp(name)).ok().map(|i| NAMES[i].1)\n")
    o.append("    }\n\n")
    o.append("    /// The local name, as the KML namespace spells it.\n")
    o.append("    pub fn name(self) -> &'static str {\n        match self {\n")
    for n, v in zip(concrete, variants):
        o.append(f'            KmlElement::{v} => "{n}",\n')
    o.append("        }\n    }\n")
    for fn, group in GROUPS:
        members = [variant(n) for n in concrete if group in groups_of(n)]
        o.append(f"\n    /// Whether this substitutes for `kml:{group}`.\n")
        o.append(f"    pub fn {fn}(self) -> bool {{\n        matches!(\n            self,\n            ")
        o.append("\n                | ".join(f"KmlElement::{m}" for m in members))
        o.append("\n        )\n    }\n")
    o.append("}\n// --- End of generated code. ---\n")
    sys.stdout.write("".join(o))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
