#!/bin/sh
# Build gdal_tiles.gpkg -- a real tile pyramid, two zoom levels, beside a
# feature layer from THE COMMON SOURCE.
#
# Why a script rather than a recorded ogr2ogr line like the other GDAL-produced
# fixtures: the input is a *raster*, and no raster in this tree is small enough
# to keep as a source. The pixels are generated, so the recipe has to be.
#
# Designed in PLANNED.org on 2026-08-23 and built 2026-08-24. Two choices in
# here were measured rather than assumed:
#
#   1. *A burned raster, not gdal_create alone.* An all-nodata source yields a
#      gpkg with the full tile DDL and ZERO tile rows -- GDAL skips blank
#      tiles -- and a fixture built that way exercises nothing.
#   2. *Smooth structure, not noise.* Every tile has to differ and none may be
#      blank, but PNG/JPEG on noise would make this fixture megabytes. Sine
#      bands plus a ring gives 80 distinct tiles in ~370 KB.
#
# The upper-left is snapped to the *overview* level's tile grid, so both zoom
# levels come out whole: 8x8 = 64 tiles at z13 and 4x4 = 16 at z12. Snapping to
# z13's grid instead yields a ragged 5x4 overview -- correct, but it reads as
# an accident when someone counts the rows later.
#
# Everything geosetta is meant to notice about this file is in HOSTILE.org.
set -e
cd "$(dirname "$0")/../.."
OUT=tests/fixtures/gdal_tiles.gpkg
SRC=$(mktemp -d)/src.tif
rm -f "$OUT"

# 2048x2048, 3 bands, EPSG:3857 over Copenhagen at zoom 13's resolution.
python3 - "$SRC" <<'PY'
import sys
import numpy as np
from osgeo import gdal, osr

osr.UseExceptions()
out = sys.argv[1]
size, zoom = 2048, 13

ORIGIN = -20037508.342789244
res = 2 * -ORIGIN / (256 * 2 ** zoom)
tile = 256 * res * 2  # the overview level's tile, so both zooms are whole

src, dst = osr.SpatialReference(), osr.SpatialReference()
src.ImportFromEPSG(4326)
src.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
dst.ImportFromEPSG(3857)
dst.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
# Copenhagen -- THE COMMON SOURCE's first row, so the pyramid sits where the
# features do.
x, y, _ = osr.CoordinateTransformation(src, dst).TransformPoint(12.5683, 55.6761)
ulx = ORIGIN + np.floor((x - ORIGIN) / tile) * tile
uly = -ORIGIN - np.floor((-ORIGIN - y) / tile) * tile

yy, xx = np.mgrid[0:size, 0:size] / size
r = 128 + 127 * np.sin(6 * np.pi * xx) * np.cos(4 * np.pi * yy)
g = 255 * yy
b = 255 * (1 - xx)
ring = (lambda d: (d > 0.28) & (d < 0.33))(np.hypot(xx - 0.5, yy - 0.5))
r[ring], g[ring], b[ring] = 250, 20, 20

ds = gdal.GetDriverByName("GTiff").Create(out, size, size, 3, gdal.GDT_Byte)
ds.SetGeoTransform((ulx, res, 0, uly, 0, -res))
ds.SetProjection(dst.ExportToWkt())
for i, band in enumerate((r, g, b), start=1):
    ds.GetRasterBand(i).WriteArray(band.astype(np.uint8))
ds = None
PY

# The pyramid. TILE_FORMAT is left at its default on purpose: what a real
# producer emits is the point, and GDAL's default for opaque RGB is JPEG.
gdal_translate -q -of GPKG -co TILING_SCHEME=GoogleMapsCompatible \
    -co RASTER_TABLE=imagery "$SRC" "$OUT"
gdaladdo -q -r average "$OUT" 2
rm -rf "$(dirname "$SRC")"

# A feature layer beside it, from gdal_hostile.gpkg so the fixture inherits THE
# COMMON SOURCE rather than inventing a second one. Two reasons this is not
# optional decoration:
#   - HOSTILE.org's standing rule is that geosetta reads every fixture without
#     error, and a tiles-only GeoPackage is rejected: "GeoPackage has no
#     feature layers". Measured 2026-08-24.
#   - The layer is EPSG:4326 and the pyramid EPSG:3857, so the file holds a
#     srs_id that no feature layer accounts for -- which is the case
#     same-format-fidelity.org M3b section 1 has to reconcile.
ogr2ogr -update -f GPKG "$OUT" tests/fixtures/gdal_hostile.gpkg cities

sqlite3 "$OUT" "PRAGMA integrity_check; PRAGMA foreign_key_check;"
sqlite3 "$OUT" "SELECT zoom_level, count(*) FROM imagery GROUP BY zoom_level;"
