#!/usr/bin/env python3
"""Generate KML 2.2's child order in src/kml/lower.rs from ogckml22.xsd and
kml22gx.xsd.

Usage: python3 -I gen_kml22_order.py SCHEMA_DIR > order.rs

SCHEMA_DIR is the directory the KML oracle reads (plans/kml-standard.org
§ ORACLES): this script needs 2.2.0/ogckml22.xsd and kml22gx.xsd beneath it.
The output replaces the region of src/kml/lower.rs between its "Generated"
and "End of generated code" markers, by hand; the schemas' SHA-256s go in that
module's doc.

What it emits:

- order_2_2(parent, spelling): for each element 2.2 gives element content, in
  either namespace, every child it admits as (element, spelling, slot). The
  slot is the child's position in the parent type's sequence, flattened
  through its base types, so sorting children by slot is 2.2's order. A
  reference to an abstract group admits every concrete element that
  substitutes for it, from either schema -- which is how gx:Track reaches a
  Placemark's geometry slot and gx:altitudeMode a Point's altitude-mode slot.
  A choice is one slot.
- foreign_slot_2_2(parent, spelling, ns, local): the slot of a child the
  model carries as foreign content but the schemas name -- atom:author,
  xal:AddressDetails, a gx: element 2.3 did not adopt -- so it is ordered
  like any other.
- in_2_2(element): whether ogckml22.xsd itself defines the element.

order_2_2 lists what the model identifies: a KML element of the 2.3
vocabulary, or a gx: element 2.3 adopted (GX_ADOPTED in src/kml/model.rs,
transcribed below). Everything else a sequence names is foreign_slot_2_2's.

The standard library is enough: xml.etree reads a schema fine.
"""

import sys
import xml.etree.ElementTree as ET

XS = "{http://www.w3.org/2001/XMLSchema}"
KML = "http://www.opengis.net/kml/2.2"
GX = "http://www.google.com/kml/ext/2.2"

# src/kml/model.rs GX_ADOPTED: gx local name -> 2.3 element name.
GX_ADOPTED = {
    "AnimatedUpdate": "AnimatedUpdate", "FlyTo": "FlyTo", "LatLonQuad": "LatLonQuad",
    "MultiTrack": "MultiTrack", "Playlist": "Playlist", "SimpleArrayData": "SimpleArrayData",
    "SimpleArrayField": "SimpleArrayField", "SoundCue": "SoundCue", "TimeSpan": "TimeSpan",
    "TimeStamp": "TimeStamp", "Tour": "Tour", "TourControl": "TourControl", "Track": "Track",
    "Wait": "Wait", "altitudeMode": "seaFloorAltitudeMode", "altitudeOffset": "altitudeOffset",
    "angles": "angles", "balloonVisibility": "balloonVisibility", "coord": "coord",
    "delayedStart": "delayedStart", "duration": "duration", "flyToMode": "flyToMode",
    "horizFov": "horizFov", "interpolate": "interpolate", "playMode": "playMode", "value": "value",
}

UPPER = {"Snippet", "Scale"}


def variant(name):
    if name in UPPER:
        return name + "Upper"
    return name[0].upper() + name[1:]


class Schemas:
    def __init__(self, schema_dir):
        self.elements = {}  # (ns, name) -> <element>
        self.types = {}  # (ns, name) -> <complexType>
        self.prefixes = {}  # (schema ns, prefix) -> uri
        for path, ns in [("2.2.0/ogckml22.xsd", KML), ("kml22gx.xsd", GX)]:
            full = f"{schema_dir}/{path}"
            for _, (prefix, uri) in ET.iterparse(full, events=["start-ns"]):
                self.prefixes[(ns, prefix)] = uri
            root = ET.parse(full).getroot()
            for e in root.findall(XS + "element"):
                self.elements[(ns, e.get("name"))] = e
            for t in root.findall(XS + "complexType"):
                self.types[(ns, t.get("name"))] = t

    def resolve(self, ns, qname):
        prefix, _, local = qname.rpartition(":")
        return (self.prefixes[(ns, prefix)], local)

    def concrete(self, key):
        """The concrete elements `key` admits: itself, or what substitutes."""
        out = []
        el = self.elements.get(key)
        if el is not None and el.get("abstract") != "true":
            out.append(key)
        for k, e in self.elements.items():
            sg = e.get("substitutionGroup")
            if sg and self.resolve(k[0], sg) == key:
                out.extend(self.concrete(k))
        return out

    def sequence(self, ns, type_qname):
        """The type's slots, base first: each a list of element keys."""
        key = self.resolve(ns, type_qname)
        t = self.types.get(key)
        if t is None:
            return []
        slots = []
        body = t
        ext = t.find(f"{XS}complexContent/{XS}extension")
        if ext is not None:
            slots.extend(self.sequence(key[0], ext.get("base")))
            body = ext
        seq = body.find(XS + "sequence")
        if seq is None:
            return slots
        for item in seq:
            if item.tag == XS + "element":
                slots.append([self.ref(key[0], item)])
            elif item.tag == XS + "choice":
                slots.append([self.ref(key[0], c) for c in item.findall(XS + "element")])
        return slots

    def ref(self, ns, item):
        """A sequence member's element: a reference, or a local declaration,
        which both schemas qualify (elementFormDefault)."""
        if item.get("ref"):
            return self.resolve(ns, item.get("ref"))
        return (ns, item.get("name"))


def identify(key):
    """The model's (variant, spelling) for an element key, or None."""
    ns, name = key
    if ns == KML:
        return (variant(name), "Kml")
    if ns == GX and name in GX_ADOPTED:
        return (variant(GX_ADOPTED[name]), "Gx")
    return None


def main(schema_dir):
    s = Schemas(schema_dir)
    table = []
    foreign = []
    for key, el in sorted(s.elements.items(), key=lambda kv: (identify(kv[0]) or ("", ""))):
        parent = identify(key)
        if parent is None or el.get("abstract") == "true" or not el.get("type"):
            continue
        children = []
        for slot, refs in enumerate(s.sequence(key[0], el.get("type"))):
            for ref in refs:
                # A reference into a schema not loaded here (Atom, xAL) is
                # the element itself; an abstract group nothing substitutes
                # for admits nothing.
                for c in s.concrete(ref) if ref in s.elements else [ref]:
                    child = identify(c)
                    if child is None:
                        if (parent, c) not in [(p, (n, l)) for p, n, l, _ in foreign]:
                            foreign.append((parent, c[0], c[1], slot))
                    elif child not in [(v, sp) for v, sp, _ in children]:
                        children.append((child[0], child[1], slot))
        if children:
            table.append((parent, children))
    in22 = sorted(variant(n) for (ns, n), e in s.elements.items() if ns == KML and e.get("abstract") != "true")

    o = []
    o.append("// --- Generated from ogckml22.xsd and kml22gx.xsd by tests/fixtures/gen_kml22_order.py; do not edit by hand. ---\n\n")
    o.append("/// Every child 2.2 admits under `parent` spelled `spelling`, as\n")
    o.append("/// `(element, spelling, slot)`: sorted by slot, children are in the\n")
    o.append("/// sequence `ogckml22.xsd` and `kml22gx.xsd` declare. Empty for an element\n")
    o.append("/// with no element content, or none in 2.2.\n")
    o.append("pub(crate) fn order_2_2(parent: KmlElement, spelling: Spelling) -> &'static [(KmlElement, Spelling, u8)] {\n")
    o.append("    use KmlElement as K;\n    use Spelling::{Gx, Kml};\n")
    o.append("    match (parent, spelling) {\n")
    for (pv, ps), children in table:
        o.append(f"        (K::{pv}, {ps}) => &[\n")
        for cv, cs, slot in children:
            o.append(f"            (K::{cv}, {cs}, {slot}),\n")
        o.append("        ],\n")
    o.append("        _ => &[],\n    }\n}\n\n")
    o.append("/// The slot of a child the model carries as foreign content, by its\n")
    o.append("/// namespace and local name, when the schemas name it under `parent`.\n")
    o.append("pub(crate) fn foreign_slot_2_2(parent: KmlElement, spelling: Spelling, ns: &str, local: &str) -> Option<u8> {\n")
    o.append("    use KmlElement as K;\n    use Spelling::{Gx, Kml};\n")
    o.append("    const ATOM: &str = \"http://www.w3.org/2005/Atom\";\n")
    o.append("    const XAL: &str = \"urn:oasis:names:tc:ciq:xsdschema:xAL:2.0\";\n")
    o.append("    const GX: &str = \"http://www.google.com/kml/ext/2.2\";\n")
    o.append("    const FOREIGN: &[(KmlElement, Spelling, &str, &str, u8)] = &[\n")
    names = {"http://www.w3.org/2005/Atom": "ATOM", "urn:oasis:names:tc:ciq:xsdschema:xAL:2.0": "XAL", GX: "GX"}
    for (pv, ps), ns, local, slot in foreign:
        o.append(f'        (K::{pv}, {ps}, {names[ns]}, "{local}", {slot}),\n')
    o.append("    ];\n")
    o.append("    FOREIGN.iter().find(|(p, s, n, l, _)| *p == parent && *s == spelling && *n == ns && *l == local).map(|f| f.4)\n}\n\n")
    o.append("/// Whether `ogckml22.xsd` defines `element` in the KML namespace.\n")
    o.append("pub(crate) fn in_2_2(element: KmlElement) -> bool {\n    use KmlElement as K;\n    matches!(\n        element,\n        ")
    o.append("\n            | ".join(f"K::{v}" for v in in22))
    o.append("\n    )\n}\n// --- End of generated code. ---\n")
    sys.stdout.write("".join(o))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
