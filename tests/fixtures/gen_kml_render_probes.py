#!/usr/bin/env python3
"""Build the KML renderer probes measured in plans/kml-standard.org
§ MEASURED IN A RENDERER (2026-10-23).

Usage: python3 -I gen_kml_render_probes.py OUT_DIR

Writes, into OUT_DIR:

  probe_rotation_gxtrack.kmz  -- a GroundOverlay rotated 45 degrees at 60N,
      with pins at the corners each rotation hypothesis predicts, plus a
      gx:Track and a control LineString. KML 2.2 + gx.
  probe_kml23_track.kml       -- a kml:Track in a version="2.3" document,
      plus a control LineString.
  probe_kml23_gxtrack.kml     -- a gx:Track in a version="2.3" document,
      plus a control LineString: open question 9's option (a).
  probe_rotation_plane.kmz    -- open question 10: a GroundOverlay 40x30
      degrees across, rotated 30 degrees, with pins at the corners each of
      four "on the ground" hypotheses predicts (2026-10-28).

These are not test fixtures: nothing in the suite reads them. They are the
input to a manual measurement in a renderer (Google Earth Web), kept so the
measurement can be rerun when a renderer changes rather than re-derived.

Why this shape, so nobody simplifies it back:

- *60N, not the equator.* The two rotation hypotheses -- turn the corners in
  degree space, or on a local metric plane -- agree at the equator and diverge
  with latitude. At 60N a 4x2-degree box is a ~222 km square on the ground and
  the predicted corners land ~80 km apart.
- *4x2 degrees, not a square in degrees.* cos(60) = 0.5, so this is the box
  that is square *on the ground*; the image's circle then stays a circle under
  a ground-plane rotation and visibly becomes an ellipse under a degree-space
  one, which is a second, independent read of the same answer.
- *Red arm to image-top, blue arm to image-right.* Fixes the direction of the
  turn: counter-clockwise puts the red arm at the NW corner.
- *A control LineString in each file*, at a known latitude, so "the track did
  not draw" can be told apart from "the file did not load" or "the camera is
  elsewhere".
- *Every <when> before every <coord>.* kml22gx.xsd declares gx:Track's content
  as a sequence, so an interleaved track is invalid 2.2 + gx (and is reported
  not to draw in Google Earth Pro); 2.3 declares kml:Track's as xs:all, which
  allows either. The probe uses the order both accept so that only the
  namespace differs between the two tracks.
- *The rotation-plane probe is 40x30 degrees, centred on 10E 45N.* The first
  probe settled degree space against the ground, but at its size every way of
  turning "on the ground" agrees to within 4 km. Turned 30 degrees, this box
  puts each pair of the four ground hypotheses at least 41 km apart at some
  corner and 140-310 km apart at another -- ten or more pixels with the whole
  box in view. The hypotheses: G, a local plane with longitude scaled by
  cos(centre latitude), which kml/project.rs implements; M1, Web Mercator
  turned about the box's Mercator midpoint; M2, Web Mercator turned about its
  midpoint in degrees; S, the sphere, each corner's bearing from the centre
  turned and its distance kept.
- *Three track files, one variable each.* 2.2 + gx:Track against 2.3 +
  gx:Track differs only in the version stamp; 2.3 + gx:Track against 2.3 +
  kml:Track differs only in the namespace.

The PNG is written by hand (zlib + struct) so the script needs nothing outside
the standard library.
"""

import math
import os
import struct
import sys
import zipfile
import zlib

LON0, LAT0 = 10.0, 60.0  # overlay centre
DLON, DLAT = 2.0, 1.0  # half-extents in degrees
ROT = 45.0  # LatLonBox rotation, degrees counter-clockwise


def overlay_png():
    """256x256: white, black border, black circle, red arm centre->top,
    blue arm centre->right."""
    w = h = 256
    rows = []
    for y in range(h):
        row = bytearray(b"\x00")  # filter type None
        for x in range(w):
            r = math.hypot(x - 127.5, y - 127.5)
            if x < 6 or y < 6 or x >= w - 6 or y >= h - 6 or abs(r - 100) < 3:
                px = (0, 0, 0)
            elif abs(x - 127.5) < 3 and y < 128:
                px = (220, 0, 0)
            elif abs(y - 127.5) < 3 and x > 128:
                px = (0, 0, 220)
            else:
                px = (255, 255, 255)
            row += bytes(px)
        rows.append(bytes(row))

    def chunk(tag, data):
        crc = zlib.crc32(tag + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", crc)

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(b"".join(rows)))
        + chunk(b"IEND", b"")
    )


def degree_space(ox, oy):
    """Hypothesis D: rotate the (lon, lat) offsets as plain numbers."""
    t = math.radians(ROT)
    return (
        LON0 + ox * math.cos(t) - oy * math.sin(t),
        LAT0 + ox * math.sin(t) + oy * math.cos(t),
    )


def ground_plane(ox, oy):
    """Hypothesis G: rotate on a local metric plane (longitude scaled by
    cos(lat0))."""
    t = math.radians(ROT)
    c = math.cos(math.radians(LAT0))
    x, y = ox * c, oy
    return (
        LON0 + (x * math.cos(t) - y * math.sin(t)) / c,
        LAT0 + x * math.sin(t) + y * math.cos(t),
    )


def pin(name, lon, lat, style):
    return (
        f"<Placemark><name>{name}</name><styleUrl>#{style}</styleUrl>"
        f"<Point><coordinates>{lon:.6f},{lat:.6f}</coordinates></Point></Placemark>"
    )


def wiggle(lon0, lat0):
    return [(lon0 + i * 0.5, lat0 + 0.3 * math.sin(i)) for i in range(8)]


def line_string(name, pts):
    coords = " ".join(f"{x},{y},0" for x, y in pts)
    return (
        f"<Placemark><name>{name}</name><styleUrl>#line</styleUrl>"
        f"<LineString><coordinates>{coords}</coordinates></LineString></Placemark>"
    )


def track(name, prefix, pts):
    whens = "".join(f"<when>2026-01-01T00:0{i}:00Z</when>" for i in range(len(pts)))
    coords = "".join(f"<{prefix}coord>{x} {y} 0</{prefix}coord>" for x, y in pts)
    return (
        f"<Placemark><name>{name}</name><styleUrl>#line</styleUrl>"
        f"<{prefix}Track>{whens}{coords}</{prefix}Track></Placemark>"
    )


STYLES = (
    '<Style id="deg"><IconStyle><color>ff0000ff</color><scale>0.8</scale></IconStyle>'
    "<LabelStyle><color>ff0000ff</color></LabelStyle></Style>"
    '<Style id="gnd"><IconStyle><color>ff00ff00</color><scale>0.8</scale></IconStyle>'
    "<LabelStyle><color>ff00ff00</color></LabelStyle></Style>"
    '<Style id="line"><LineStyle><color>ffff00ff</color><width>4</width></LineStyle></Style>'
)


def main(out):
    corners = {"NE": (DLON, DLAT), "NW": (-DLON, DLAT), "SW": (-DLON, -DLAT), "SE": (DLON, -DLAT)}
    pins = []
    for k, (ox, oy) in corners.items():
        pins.append(pin(f"D-{k}", *degree_space(ox, oy), "deg"))
        pins.append(pin(f"G-{k}", *ground_plane(ox, oy), "gnd"))
    pins.append(pin("D-top", *degree_space(0, DLAT), "deg"))
    pins.append(pin("G-top", *ground_plane(0, DLAT), "gnd"))

    overlay = (
        f"<GroundOverlay><name>rotated overlay {ROT:g}deg @{LAT0:g}N</name>"
        "<Icon><href>overlay.png</href></Icon>"
        f"<LatLonBox><north>{LAT0 + DLAT}</north><south>{LAT0 - DLAT}</south>"
        f"<east>{LON0 + DLON}</east><west>{LON0 - DLON}</west>"
        f"<rotation>{ROT}</rotation></LatLonBox></GroundOverlay>"
    )
    doc22 = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2">'
        f"<Document><name>geosetta probe: rotation + gx:Track</name>{STYLES}\n"
        f"{overlay}\n{''.join(pins)}\n"
        f"{line_string('CONTROL LineString (lat 50)', wiggle(0, 50))}\n"
        f"{track('gx:Track (lat 48)', 'gx:', wiggle(0, 48))}\n"
        "</Document></kml>"
    )
    doc23 = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<kml xmlns="http://www.opengis.net/kml/2.2" version="2.3">'
        f"<Document><name>geosetta probe: kml:Track (KML 2.3)</name>{STYLES}\n"
        f"{line_string('CONTROL LineString (lat 44)', wiggle(0, 44))}\n"
        f"{track('kml:Track 2.3 (lat 46)', '', wiggle(0, 46))}\n"
        "</Document></kml>"
    )

    doc23gx = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<kml xmlns="http://www.opengis.net/kml/2.2" xmlns:gx="http://www.google.com/kml/ext/2.2" version="2.3">'
        f"<Document><name>geosetta probe: gx:Track (KML 2.3)</name>{STYLES}\n"
        f"{line_string('CONTROL LineString (lat 40)', wiggle(0, 40))}\n"
        f"{track('gx:Track 2.3 (lat 42)', 'gx:', wiggle(0, 42))}\n"
        "</Document></kml>"
    )

    with zipfile.ZipFile(os.path.join(out, "probe_rotation_gxtrack.kmz"), "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("doc.kml", doc22)
        z.writestr("overlay.png", overlay_png())
    with open(os.path.join(out, "probe_kml23_track.kml"), "w") as f:
        f.write(doc23)
    with open(os.path.join(out, "probe_kml23_gxtrack.kml"), "w") as f:
        f.write(doc23gx)

    for k, (ox, oy) in corners.items():
        d, g = degree_space(ox, oy), ground_plane(ox, oy)
        print(f"{k}: degree-space {d[0]:.3f},{d[1]:.3f}  ground-plane {g[0]:.3f},{g[1]:.3f}")

    plane = rotation_plane_probe()
    with zipfile.ZipFile(os.path.join(out, "probe_rotation_plane.kmz"), "w", zipfile.ZIP_DEFLATED) as z:
        z.writestr("doc.kml", plane)
        z.writestr("overlay.png", overlay_png())
    for name, corners in PLANE_HYPOTHESES.items():
        print(f"{name}: " + "  ".join(f"{k} {x:.3f},{y:.3f}" for k, (x, y) in corners.items()))


# --- open question 10: the rotation plane -------------------------------

P_LON0, P_LAT0 = 10.0, 45.0  # centre
P_DLON, P_DLAT = 20.0, 15.0  # half-extents: the box spans -10..30E, 30..60N
P_ROT = 30.0


def _merc_y(lat):
    return math.degrees(math.log(math.tan(math.pi / 4 + math.radians(lat) / 2)))


def _merc_lat(y):
    return math.degrees(2 * math.atan(math.exp(math.radians(y))) - math.pi / 2)


def _turn(x, y, t):
    return x * math.cos(t) - y * math.sin(t), x * math.sin(t) + y * math.cos(t)


def plane_hypotheses():
    """Each hypothesis's corners, {name: {corner: (lon, lat)}}."""
    t = math.radians(P_ROT)
    n, s, e, w = P_LAT0 + P_DLAT, P_LAT0 - P_DLAT, P_LON0 + P_DLON, P_LON0 - P_DLON
    corners = {"NE": (e, n), "NW": (w, n), "SW": (w, s), "SE": (e, s)}
    out = {}

    c = math.cos(math.radians(P_LAT0))
    g = {}
    for k, (x, y) in corners.items():
        rx, ry = _turn((x - P_LON0) * c, y - P_LAT0, t)
        g[k] = (P_LON0 + rx / c, P_LAT0 + ry)
    out["G"] = g

    for name, cy in [("M1", (_merc_y(n) + _merc_y(s)) / 2), ("M2", _merc_y(P_LAT0))]:
        m = {}
        for k, (x, y) in corners.items():
            rx, ry = _turn(x - P_LON0, _merc_y(y) - cy, t)
            m[k] = (P_LON0 + rx, _merc_lat(cy + ry))
        out[name] = m

    p1 = math.radians(P_LAT0)
    sph = {}
    for k, (x, y) in corners.items():
        p2, dl = math.radians(y), math.radians(x - P_LON0)
        d = math.acos(math.sin(p1) * math.sin(p2) + math.cos(p1) * math.cos(p2) * math.cos(dl))
        b = math.atan2(math.sin(dl) * math.cos(p2), math.cos(p1) * math.sin(p2) - math.sin(p1) * math.cos(p2) * math.cos(dl))
        b -= t  # a bearing runs clockwise; the turn is counter-clockwise
        q = math.asin(math.sin(p1) * math.cos(d) + math.cos(p1) * math.sin(d) * math.cos(b))
        lon = math.radians(P_LON0) + math.atan2(math.sin(b) * math.sin(d) * math.cos(p1), math.cos(d) - math.sin(p1) * math.sin(q))
        sph[k] = (math.degrees(lon), math.degrees(q))
    out["S"] = sph
    return out


PLANE_HYPOTHESES = plane_hypotheses()

# One colour per hypothesis, as KML's aabbggrr.
PLANE_COLOURS = {"G": "ff00ff00", "M1": "ffff0000", "M2": "ffffff00", "S": "ff00a5ff"}


def rotation_plane_probe():
    styles = "".join(
        f'<Style id="{h}"><IconStyle><color>{c}</color><scale>0.9</scale></IconStyle>'
        f"<LabelStyle><color>{c}</color></LabelStyle></Style>"
        for h, c in PLANE_COLOURS.items()
    )
    pins = "".join(
        pin(f"{h}-{k}", x, y, h) for h, corners in PLANE_HYPOTHESES.items() for k, (x, y) in corners.items()
    )
    overlay = (
        f"<GroundOverlay><name>rotated overlay {P_ROT:g}deg, 40x30deg @{P_LAT0:g}N</name>"
        "<Icon><href>overlay.png</href></Icon>"
        f"<LatLonBox><north>{P_LAT0 + P_DLAT}</north><south>{P_LAT0 - P_DLAT}</south>"
        f"<east>{P_LON0 + P_DLON}</east><west>{P_LON0 - P_DLON}</west>"
        f"<rotation>{P_ROT}</rotation></LatLonBox></GroundOverlay>"
    )
    return (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        '<kml xmlns="http://www.opengis.net/kml/2.2">'
        f"<Document><name>geosetta probe: rotation plane (G green, M1 blue, M2 cyan, S orange)</name>"
        f"{styles}\n{overlay}\n{pins}\n</Document></kml>"
    )


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
