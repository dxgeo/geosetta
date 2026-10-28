# Geosetta

Geosetta converts between vector geospatial formats. It is a Rust library
and CLI with no dependencies outside the standard library, and every format
it supports can be converted to every other. The idea is similar to
[Pandoc](https://pandoc.org/).

Current version: 0.27.0.


## PHILOSOPHY

-   **Nothing is lost silently.** A conversion either carries something
    through or says on stderr what it dropped. Where the target format can
    hold something, Geosetta writes the code to carry it rather than
    settling for a warning.
-   **No dependencies.** Every format, codec and container (Parquet, SQLite,
    ZIP, ZSTD) is implemented here from its specification, using only Rust's
    standard library.
-   **One model in the middle.** Every reader produces one feature model and
    every writer consumes it (`read(from) → FeatureCollection → write(to)`),
    so a new format converts to and from every existing one with no
    per-pair code.
-   **Compose rather than embed.** Geosetta does not reproject or look up
    CRS codes, and it never runs another program. It reads and writes stdin
    and stdout so that you can put tools that do those things in the
    pipeline yourself.
-   **Checked against the reference tools.** Output is tested with GDAL,
    DuckDB, SQLite and PROJ, and against fixtures those tools wrote.

In order of priority, Geosetta aims to be extensible, performant, universal,
lightweight and dependency-free.


## SETUP

Geosetta needs Rust 1.85 or later (edition 2024).

Install the CLI from crates.io:

```sh
cargo install geosetta
```

or from a checkout:

```sh
cargo install --path .
```

To use it as a library:

```sh
cargo add geosetta
```

There is nothing else to install. The tests run with `cargo test`; a few
cross-checks against GDAL, SQLite and PROJ are `#[ignore]`d and need those
tools on `PATH` (`cargo test -- --ignored`).


## USAGE

### The CLI

The input and output formats are taken from the file extensions:

    geosetta input.geojson output.parquet   # GeoJSON    -> GeoParquet
    geosetta input.parquet output.geojson   # GeoParquet -> GeoJSON
    geosetta input.fgb     output.geojson   # FlatGeobuf -> GeoJSON
    geosetta input.csv     output.parquet   # CSV (WKT)  -> GeoParquet
    geosetta input.fgb     output.csv       # FlatGeobuf -> CSV (WKT)
    geosetta parcels.shp   parcels.geojson  # Shapefile  -> GeoJSON (reads sibling .shx/.dbf/.prj)
    geosetta roads.geojson roads.shp        # GeoJSON    -> Shapefile
    geosetta places.kmz    places.parquet   # KMZ        -> GeoParquet
    geosetta places.geojson places.kml      # GeoJSON    -> KML

or given explicitly:

    geosetta in.txt out.bin --from geojson --to parquet

GeoPackage files hold several layers:

    geosetta input.gpkg    output.geojson     # one layer (--layer NAME to pick it)
    geosetta input.gpkg    out/ --to geojson  # one file per layer
    geosetta data.gpkg     out/ --to shp      # one .shp set per layer
    geosetta roads.geojson data.gpkg          # create data.gpkg with layer "roads"
    geosetta rivers.csv    data.gpkg          # add layer "rivers" to data.gpkg

Options:

    --rtree             write GeoPackage's R*Tree spatial index
    --envelope          store each geometry's bbox in its GeoPackage header
    --sort-hilbert      order features by spatial locality
    --kml-version 2.3   write OGC KML 2.3 (default 2.2)
    --compact           write GeoJSON or KML on one line
    --progress          report each stage on stderr
    --quiet             no warnings on stderr

### Pipes

`-` is stdin or stdout. There is no extension to read the format from, so
`--from` or `--to` is required. This lets another tool run between the read
and the write, for example to reproject:

    reproject-tool --to EPSG:3857 < in.geojson | geosetta --from geojson --to fgb - out.fgb
    geosetta in.shp - --to fgb | reproject-tool --to EPSG:3857 | geosetta - out.shp --from fgb

### Editing in the middle

When a pipeline converts to a transit format, edits, and converts back, it
loses whatever the transit format cannot hold: `gpkg → fgb → tool → fgb →
gpkg` loses what FlatGeobuf cannot carry. Using the same format at both ends
avoids that:

    geosetta in.fgb - --to fgb | your-editor | geosetta - out.fgb --from fgb

Input and output may be the same format but not the same file, since a failed
write would destroy the input. What a same-format run still loses is listed
under LIMITATIONS.

### Coordinate reference systems

Each format's CRS is carried through and written in the target's own form
(authority and code, WKT, or PROJJSON). WKT and PROJJSON definitions are
translated into each other. A CRS given only as a code (`EPSG:7844`) cannot be
expanded without a registry, which Geosetta does not have, so another tool
does the lookup and Geosetta takes the result with `--crs`:

    geosetta parcels.fgb --print-crs-code                 # -> EPSG:7844
    geosetta parcels.fgb parcels.shp --crs gda2020.wkt    # WKT or PROJJSON, from a file
    geoscribe EPSG:7844 --wkt | geosetta parcels.fgb parcels.shp --crs -
    projinfo -o WKT1_GDAL -q EPSG:7844 | geosetta parcels.fgb parcels.shp --crs -

A definition with no code at all (an Esri-style `.prj`, say) has to be
identified by name. `--print-crs` prints the definition a file stores, exactly
as stored:

    geosetta parcels.parquet --print-crs | geoscribe --identify --projjson \
      | geosetta parcels.parquet parcels.fgb --crs -

Geosetta never runs these tools itself. [`geoscribe`](https://github.com/dxgeo/geoscribe)
is about 1 MB and prints nothing if more than one CRS fits; `projinfo` and
`gdalsrsinfo` work too. A crafted CRS name can contain terminal escape
sequences, so use `--print-crs --escape` to read an untrusted file at a
terminal; it is not safe to pipe. See
[plans/crs-external-resolution.org](plans/crs-external-resolution.org).

### The library

```rust
use geosetta::{read_features, write_features, Format};

let input = std::fs::read("places.geojson")?;
let mut warnings = Vec::new();
let mut fc = read_features(Format::GeoJson, &input, &mut warnings)?;
fc.for_each_position_mut(|p| p.x += 1.0);    // edit coordinates in place
let output = write_features(Format::Parquet, &fc)?;
```

`warnings` receives what the read dropped. GeoPackage and Shapefile span more
than one buffer and have their own entry points: `geopackage::read_layers`,
`geopackage::writer::write_layers`, `shapefile::read` and `shapefile::write`.
A collection held in memory between a read and a write is never serialised,
so none of the same-format losses below apply to it.


## SUPPORTED

| Format | Read | Write | Notes |
|---|---|---|---|
| GeoJSON | ✓ | ✓ | Indented by default; `--compact` for one line |
| GeoParquet | ✓ | ✓ | Reads what DuckDB, GDAL and Arrow write (see below) |
| FlatGeobuf | ✓ | ✓ | Written with a packed Hilbert R-tree index |
| CSV | ✓ | ✓ | WKT geometry column |
| WKT | ✓ | ✓ | One geometry per line |
| GeoPackage | ✓ | ✓ | Every layer; create, append, merge |
| Shapefile | ✓ | ✓ | `.shp`/`.shx`/`.dbf`/`.cpg`/`.prj` |
| KML | ✓ | ✓ | OGC 2.2 and 2.3 |
| KMZ | ✓ | ✓ | Every `.kml` in the archive |

Z goes through every format. M goes through every format that has it: WKB,
WKT and CSV, GeoPackage, FlatGeobuf, Shapefile and GeoParquet.

When the target cannot hold something in the source (a CRS, M values, a
`.dbf` field name that is too long), Geosetta still converts and prints a
warning on stderr. See
[plans/lossy-conversion-warnings.org](plans/lossy-conversion-warnings.org).

**GeoJSON.** Written with the collection, each feature and each geometry one
member per line, and values such as coordinates on one line each.

**GeoParquet.** Output is Snappy-compressed and queryable by DuckDB's spatial
extension, and geojson→parquet→geojson→parquet is byte-for-byte stable. The
reader handles:

-   SNAPPY, GZIP, ZSTD, LZ4\_RAW or no compression; PLAIN and dictionary
    encodings; `DATA_PAGE_V2`; multiple pages and row groups.
-   `BOOLEAN`, `INT32`/`INT64`, `FLOAT`/`DOUBLE`, strings, `DATE` and
    `TIMESTAMP` (as ISO 8601), `DECIMAL` (as an exact decimal string),
    `INT96` timestamps, `JSON`, and single-level lists of scalars.
-   Columns nested in groups, such as GDAL's `geometry_bbox` covering column,
    which is recognised and not exposed as a property.
-   Parquet's native `GEOMETRY`/`GEOGRAPHY` type, for files without `geo`
    metadata.

The `geo` metadata's `bbox` is written as GDAL writes it, with one difference:
on input that mixes 2D and 3D features, GDAL writes the 2D geometries with
Z = 0 but takes the bbox's Z range from the 3D ones only, so the bbox can be
smaller than the data and `ogrinfo -spat` can miss features. Geosetta keeps 2D
geometries 2D and computes the Z range from the coordinates that have Z. See
[plans/envelope.org](plans/envelope.org).

**CSV.** A cell is given a type only when that type writes back as the same
text, so `007` stays `007` and `1.50` stays `1.50`.

**GeoPackage.** Output passes SQLite's `integrity_check` and
`foreign_key_check`, and GDAL reads and rewrites it. The registry tables
follow the SQL in the OGC standard's annexes.

-   A GeoPackage-to-GeoPackage run keeps what sits around the layers: tile
    pyramids, user tables, `attributes` tables, metadata, and extension
    registrations.
-   Writing into an existing file merges the two. Metadata is deduplicated,
    tables from both sides are kept, and tile pyramids with the same name are
    combined. If the two files conflict (two different tables with one name,
    two different images at one tile), nothing is written.
-   `--rtree` writes the RTree index extension, which passes `rtreecheck()`
    and which GDAL and DuckDB use. `--envelope` stores each geometry's bbox
    as GDAL does.

**Shapefile.** Polygon rings are grouped by winding order. `.cpg` is always
written as UTF-8. Writing never deletes a file it did not write; a leftover
`.qix`, `.sbn` or `.prj` at the output path is named on stderr.

**KML and KMZ.**

-   Output keeps the source's version on a KML → KML run, and is 2.2 from any
    other format, since that is what Google Earth, GDAL and CesiumJS read.
    Converting between versions renames elements as needed and lists the
    renames on stderr.
-   A KML → KML run gives back the document it read, including styles,
    folders, overlays, whitespace and comments, with any edits to the features
    applied.
-   Other formats see a feature's `name`, `description`, time (`timestamp` or
    `begin`/`end`), altitude mode, `extrude`, `tessellate`, and
    `<ExtendedData>`, typed by the document's `<Schema>`. A `<GroundOverlay>`
    becomes the polygon its image covers and a `<PhotoOverlay>` a point.
    Written back to KML, those properties become elements again.
-   A document whose heights are all `absolute` reads as EPSG:9707 (WGS 84 +
    EGM96 height).
-   A `.kmz` written back from a `.kmz` keeps every file in the archive with
    its timestamps, permissions and comments.

See [plans/kml-standard.org](plans/kml-standard.org).


## LIMITATIONS

-   **Vector only.** Raster formats are not supported.
-   **No reprojection, and no CRS registry.** A CRS given only as a code has
    to be looked up by another tool (see USAGE).
-   **Everything is held in memory.** A conversion reads the whole input
    before writing. Reading KML uses the most, about 6 KB per feature on a
    file of points.
-   **Text encodings.** GeoJSON, CSV and WKT must be UTF-8, and KML must be
    UTF-8 or an encoding Geosetta has a table for; anything else is refused.
    There is no flag yet to name the input's encoding. A `.dbf` whose `.cpg`
    names an unrecognised codepage is read as ISO-8859-1, with a warning.
-   **GeoParquet.** Not read: Brotli compression, lists of structs, nested
    lists, and more than one geometry column; each is reported as an error.
    The writer emits one row group and does not write the covering-bbox
    column.
-   **KML.** Versions 2.0 and 2.1 are refused. Converting 2.3 to 2.2 drops the
    few 2.3 elements 2.2 has no place for, with a warning.
-   **KMZ.** Written uncompressed, since there is no DEFLATE encoder yet.
    Archives that would need zip64 (more than 65,535 entries or 4 GiB) are
    refused on read and on write.
-   **Shapefile.** Cannot be piped through `-`, since it is several files.
    `.qix` and `.sbn` indexes are not written.
-   **`--progress`** reports each stage, not a percentage within one.
-   **Same-format losses.** A same-format run reports these on stderr:
    -   GeoJSON drops `bbox` members and foreign members on geometries, and
        rewrites numbers (`1.50` as `1.5`).
    -   CSV rewrites the coordinates in the geometry column, and loses
        precision on integers past 2^53 in a column that also holds decimals.
    -   WKT rewrites coordinates and the `MULTIPOINT` form.
    -   FlatGeobuf narrows the declared geometry type to what is present.
    -   GeoParquet drops the covering-bbox column and renames the geometry
        column to `geometry`.
    -   KML writes a non-UTF-8 document back as UTF-8.
    -   KMZ also writes its files uncompressed.
    -   Shapefile and GeoPackage lose nothing; a GeoPackage reports anything
        around the layers it cannot carry.

Design notes and open work are in [plans/](plans/README.org), and the open
items in [plans/todo.org](plans/todo.org).


## IMPLEMENTATION

Every format is implemented from its specification:

-   **`feature.rs`:** the feature model (`Feature`, `FeatureCollection`).
    Every format depends on it and none on each other.
-   **`format.rs`:** the `Format` enum and each format's capabilities. Its
    module doc lists what adding a format requires.
-   **`json/`, `geojson/`:** a JSON parser and GeoJSON.
-   **`schema.rs`:** property column type inference for the Parquet and
    FlatGeobuf writers.
-   **`flatbuffers.rs`, `flatgeobuf/`:** a FlatBuffers reader and builder, and
    FlatGeobuf.
-   **`geometry/`:** the geometry model, with WKB and WKT codecs.
-   **`csv.rs`:** CSV.
-   **`spatial.rs`:** Hilbert-curve ordering.
-   **`sqlite.rs`, `geopackage/`:** a SQLite reader and whole-file writer, and
    GeoPackage on top of it.
-   **`compress/`:** Snappy, GZIP/DEFLATE, ZSTD and LZ4 decoders, and a Snappy
    encoder.
-   **`parquet/`:** Parquet and GeoParquet.
-   **`shapefile/`:** the `.shp`/`.shx` and `.dbf` codecs.
-   **`kml/`, `xml.rs`, `zip.rs`:** KML and KMZ, and the XML and ZIP codecs.
-   **`crs.rs`, `crs/`:** the CRS model and WKT↔PROJJSON translation.
-   **`cli.rs`, `convert.rs`:** argument parsing and the conversion pipeline.


## PRIOR ART

-   [GDAL/OGR](https://gdal.org/) (`ogr2ogr`) converts 80+ vector formats
    through a common feature model, the same design Geosetta uses. DuckDB
    spatial, GeoPandas, Fiona, pyogrio and QGIS all build on it.
-   [geozero](https://github.com/georust/geozero) is the closest Rust
    equivalent. `geoarrow-rs` covers GeoArrow and GeoParquet; `georust/gdal`
    binds GDAL.

The difference is dependencies. GDAL needs a C/C++ toolchain and PROJ/GEOS,
and the Rust crates depend on other crates, with Parquet support coming from
Apache Arrow. Geosetta uses only the standard library, which makes it small
and easy to embed and audit (including in WASM), at the cost of
reimplementing what GDAL and Arrow already do well.


## LICENSE

Released under the [MIT License](LICENSE).
