#!/usr/bin/env python3
"""Generate the children KML 2.3 admits, in src/kml/raise.rs, from ogckml23.xsd.

Usage: python3 -I gen_kml23_children.py SCHEMA_DIR > children.rs

SCHEMA_DIR is the directory the KML oracle reads (plans/kml-standard.org
§ ORACLES): this script needs 2.3/ogckml23.xsd beneath it. The output replaces
the region of src/kml/raise.rs between its "Generated" and "End of generated
code" markers, by hand; the schema's SHA-256 goes in that module's doc.

What it emits:

- children_2_3(parent): every KML element 2.3 admits as a child of `parent`,
  through its type's content model flattened through base types, with every
  element that substitutes for an abstract group. 2.3 orders nothing -- its 79
  content models are all <all> groups -- so membership is all there is.

Atom and xAL are left out: the model carries them as foreign content, and
2.3's open content admits every other namespace anywhere.

The standard library is enough: xml.etree reads a schema fine.
"""

import sys
import xml.etree.ElementTree as ET

XS = "{http://www.w3.org/2001/XMLSchema}"
UPPER = {"Snippet", "Scale"}


def variant(name):
    if name in UPPER:
        return name + "Upper"
    return name[0].upper() + name[1:]


def local(qname):
    """The local name of a kml: reference, or None for another namespace."""
    prefix, _, name = qname.rpartition(":")
    return name if prefix == "kml" else None


def main(schema_dir):
    root = ET.parse(schema_dir + "/2.3/ogckml23.xsd").getroot()
    els = {e.get("name"): e for e in root.findall(XS + "element")}
    types = {t.get("name"): t for t in root.findall(XS + "complexType")}
    groups = {g.get("name"): g for g in root.findall(XS + "group")}
    assert not root.findall(f".//{XS}sequence"), "2.3 now orders something: this table is not enough"

    def concrete(name):
        out = [name] if els[name].get("abstract") != "true" else []
        for n, e in els.items():
            sg = e.get("substitutionGroup")
            if sg and local(sg) == name:
                out.extend(concrete(n))
        return out

    def children(type_name):
        t = types.get(type_name)
        if t is None:
            return []
        out = []
        body = t
        ext = t.find(f"{XS}complexContent/{XS}extension")
        if ext is not None:
            out.extend(children(local(ext.get("base")) or ""))
            body = ext
        for group in body.findall(XS + "all"):
            members(group, out)
        return out

    def members(group, out):
        """An <all>'s elements, through any named group it references."""
        for item in group:
            if item.tag == XS + "element" and item.get("name"):
                # A local declaration -- IconStyle's Icon, the only one --
                # qualified in the KML namespace (elementFormDefault).
                if item.get("name") not in out:
                    out.append(item.get("name"))
            elif item.tag == XS + "element":
                name = local(item.get("ref") or "")
                if name:
                    out.extend(c for c in concrete(name) if c not in out)
            elif item.tag == XS + "group":
                for inner in groups[local(item.get("ref"))].findall(XS + "all"):
                    members(inner, out)

    table = []
    for name in sorted(els, key=variant):
        e = els[name]
        type_name = local(e.get("type") or "")
        if e.get("abstract") == "true" or not type_name:
            continue
        kids = children(type_name)
        if kids:
            table.append((variant(name), [variant(k) for k in kids]))

    o = []
    o.append("// --- Generated from ogckml23.xsd by tests/fixtures/gen_kml23_children.py; do not edit by hand. ---\n\n")
    o.append("/// Every KML element 2.3 admits as a child of `parent`. Empty for an\n")
    o.append("/// element with no element content of its own.\n")
    o.append("pub(crate) fn children_2_3(parent: KmlElement) -> &'static [KmlElement] {\n")
    o.append("    use KmlElement as K;\n    match parent {\n")
    for parent, kids in table:
        o.append(f"        K::{parent} => &[\n")
        for k in kids:
            o.append(f"            K::{k},\n")
        o.append("        ],\n")
    o.append("        _ => &[],\n    }\n}\n// --- End of generated code. ---\n")
    sys.stdout.write("".join(o))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
