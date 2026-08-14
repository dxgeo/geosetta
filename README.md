# Geosetta


## OVERVIEW

Geosetta is a library for exporting any vector geospatial format to any other vector geospatial format.
The basic idea is to have one library, with no other dependencies, that can take any input vector geospatial file and output any other conceivable vector format, provided it is an open standard.
If you've heard of [Pandoc](https://pandoc.org/), the idea is similar here.

Vector is the focus today. **Raster** formats are a planned future direction —
they need a second intermediate representation alongside the feature IR (a
coverage/grid model rather than `FeatureCollection`); see the roadmap.


## ARCHITECTURE

This is the list of priorities for this project in terms of design/architecture.
The project aims to be:

-   Extensible
-   Performant
-   Universal
-   Lightweight
-   Dependency-free


## STATUS

Current version: **0.22.0**.

Eight formats are supported, all routed through a shared feature IR
(`read(from) → FeatureCollection → write(to)`), so every format composes with
every other automatically — no per-pair code:

-   **GeoJSON** (read + write)
-   **GeoParquet** (read + write; reads common foreign files — see below)
-   **FlatGeobuf** (read + write) on a from-scratch FlatBuffers reader and builder;
    written with a **packed Hilbert R-tree** spatial index (GDAL bbox queries work)
-   **CSV** with a WKT geometry column (read + write), types lightly inferred
-   **WKT** — one geometry per line (read + write)
-   **GeoPackage** (read + write) on a from-scratch SQLite reader **and** writer. A
    `.gpkg` is multi-layer, so reading fans out over all feature tables and writing
    a layer creates the file or appends to it (upserting by name); the `--layer`
    option and directory-vs-single-file output rules handle the fan-out. Our output
    passes SQLite's `integrity_check` and is read by GDAL. An opt-in `--rtree` flag
    also writes the GeoPackage RTree extension (SQLite's R\*Tree virtual table,
    built from scratch); the index passes `rtreecheck()` and answers GDAL/DuckDB
    bbox queries.
-   **Shapefile** (read + write) — the first multi-file spoke, sibling
    `.shp`/`.shx`/`.dbf`/`.prj`(/`.cpg`) files sharing a basename. A from-scratch
    mixed-endian `.shp`/`.shx` geometry codec (with real ring-classification
    geometry — shoelace winding + appearance-order grouping — for the
    Polygon/MultiPolygon disambiguation) and a dBase III/IV `.dbf` attribute codec.
    Composes with GeoPackage's multi-layer fan-out for free (one `layer.shp`
    sibling set per `.gpkg` layer, in either direction). Validated against real
    DuckDB-generated fixtures and GDAL/DuckDB's own readers.
-   **KML** and **KMZ** (read + write) on a from-scratch XML reader/writer
    (`src/xml.rs`) and a minimal ZIP container codec (`src/zip.rs`). CRS is
    always WGS 84 (KML has no other CRS concept). `.kmz` reading flattens
    every internal `*.kml` entry rather than trusting just the first, since a
    real multi-layer producer's root `doc.kml` can be nothing but a
    `<NetworkLink>` pointing at the actual data; writing packs the `.kml`
    bytes as one stored (uncompressed) zip entry — no DEFLATE encoder in the
    crate yet, so `.kmz` output is bigger than a compressed one but
    universally readable. Validated against real GDAL (`ogrinfo`,
    `ogr2ogr -f LIBKML`) and DuckDB-generated fixtures, plus a real Info-ZIP
    archive for the deflate read path.

Any input converts to any output: e.g. CSV→GeoParquet, FlatGeobuf→GeoJSON,
GeoPackage→GeoParquet, GeoJSON→GeoPackage, GeoPackage↔Shapefile all work.
Validated against DuckDB / GDAL.

Each format's **coordinate reference system** is carried through the IR and
re-emitted in the target's own representation (authority+code, WKT, WKT2, or
PROJJSON) — Geosetta *labels* CRS, it never reprojects. A source CRS resolves in
up to three ways, tried in order: (1) an opt-in **embedded CRS registry** —
PROJ's `proj.db`, re-encoded as a ~1 MB `(authority, code) → {PROJJSON, WKT1,
WKT2}` blob covering all 13,790 CRSes across every authority, gated behind the
`crs-registry` Cargo feature and shipped in a sibling crate
([`geosetta-crs-data`](https://github.com/dxgeo/geosetta-crs-data)) so the
default build stays dependency-free; it also recovers a code from an id-less
WKT's name (e.g. Esri-flavor Shapefile `.prj` text, which carries no authority
id), validated against the CRS's own ellipsoid before ever trusting a match; (2)
**structural translation** of a source WKT definition into a complete, resolvable
PROJJSON object — geographic and the common projected CRSes (WKT2, and WKT1 for
the unambiguous methods), compiled in by default; (3) an *id-reference +
warning* fallback. Every resolution path is verified against PROJ's `projinfo`
at 100% identification (or, for name recovery, zero wrong matches — see
`geosetta-crs-data`'s design doc for the bulk-oracle methodology). When a target
can't express the source CRS at all (GeoJSON forces WGS 84; CSV/WKT record none),
the CLI *warns* rather than silently mislabeling; `--quiet` suppresses.

The **GeoParquet** path is the most exercised. Write output is Snappy-compressed
and validated by DuckDB's spatial engine as genuine, queryable GeoParquet; the
reader recovers a GeoParquet file Geosetta wrote back to equivalent GeoJSON,
and the geojson→parquet→geojson→parquet round trip is byte-for-byte stable.

The reader also handles common **foreign** GeoParquet: DuckDB's default output
(dictionary-encoded columns, multiple row groups) reads back correctly,
geometry included, under SNAPPY, GZIP, ZSTD, LZ4_RAW, or no compression — the
ZSTD, GZIP/DEFLATE, and LZ4 decoders are implemented from scratch in
`compress/zstd.rs`, `compress/gzip.rs`, and `compress/lz4.rs`. Property columns may
be `BOOLEAN`, `INT32`/`INT64`, `FLOAT`/`DOUBLE`, or `BYTE_ARRAY` strings, with
`DATE` and `TIMESTAMP` columns rendered as ISO 8601 strings. The schema parser
walks the file's actual schema *tree* (not just each leaf's own repetition
type), so a column nested under an OPTIONAL group decodes correctly too — the
case that matters in practice is GDAL/OGR's GeoParquet 1.1 `geometry_bbox`
"covering" column, which it writes by default; Geosetta recognizes it via the
`geo` metadata and excludes it from `properties` rather than surfacing it as a
fake column (fixture: `tests/fixtures/gdal_covering_bbox.parquet`). Remaining
gaps — `DATA_PAGE_V2`, `DECIMAL`/`INT96`, genuinely nested/repeated (list-valued)
columns, 3D geometry, Brotli — are reported as clear errors and tracked in
[plans/arbitrary-geoparquet.org](plans/arbitrary-geoparquet.org).


## IMPLEMENTATION

Language: Rust (edition 2024). Every wire format is implemented from its
specification rather than pulled from a crate:

-   **`feature.rs`:** the shared, format-neutral feature model (`Feature` /
    `FeatureCollection`) — the intermediate representation every format converts
    to and from; all spokes depend on it, not on each other
-   **`json/`:** standard-library JSON parser and value model
-   **`geojson/`:** GeoJSON parsing and serialization (both directions)
-   **`schema.rs`:** property-column inference (type lattice, per-row cells) shared
    by the Parquet and FlatGeobuf writers
-   **`flatbuffers.rs`:** a minimal, from-scratch FlatBuffers reader **and** builder
    (vtables, offsets, alignment, vectors, strings, tables)
-   **`flatgeobuf/`:** FlatGeobuf reader and writer — magic + header, the packed
    Hilbert R-tree spatial index (skipped on read, built on write via
    `spatial.rs`), and feature/geometry/property coding via `flatbuffers.rs`
-   **`geometry/`:** geometry model, bounding box, and the shared geometry codecs —
    WKB (`wkb.rs`) and WKT (`wkt.rs`), each encoder + decoder
-   **`csv.rs`:** CSV spoke — RFC 4180 rows with a WKT geometry column and
    type-inferred property columns
-   **`spatial.rs`:** shared spatial-ordering primitives — a Hilbert-curve encoder
    and feature-locality sort, used to build FlatGeobuf's packed R-tree
-   **`sqlite.rs`:** a minimal, from-scratch SQLite reader **and** whole-file writer
    (header, multi-page b-tree walk/pack with overflow chains — including a
    `sqlite_master` that grows past page 1 — schema-only master rows for virtual
    tables and triggers, record coding, CREATE TABLE parsing) — the file our
    GeoPackage output passes `integrity_check` on
-   **`geopackage/`:** GeoPackage reader and writer on top of `sqlite.rs` — layer
    fan-out from `gpkg_contents`, GeoPackage Binary geometry wrapping WKB, and
    create-or-append (upsert) writes. `geopackage/rtree.rs` builds the opt-in
    RTree extension: a packed R\*Tree in SQLite's node blob format, its shadow
    tables, and the maintenance triggers
-   **`compress/`:** format-agnostic `bytes -> bytes` codecs implemented from spec —
    Snappy, GZIP/DEFLATE (RFC 1951/1952), ZSTD (RFC 8878, FSE + Huffman +
    sequences), and LZ4 block. Not Parquet-specific, so reusable by future formats
-   **`parquet/`:** Thrift compact-protocol writer, schema inference, GeoParquet
    `geo` metadata, and the Parquet file writer
-   **`parquet/reader.rs`:** the inverse — footer/schema parsing, page iteration
    (dictionary + data pages), decompression via `compress/`, RLE/bit-pack levels,
    PLAIN and dictionary value decoding, across multiple row groups
-   **`shapefile/`:** the Shapefile spoke — `geometry.rs` (mixed-endian
    `.shp`/`.shx` codec plus ring classification), `dbf.rs` (dBase III/IV
    attribute codec), `reader.rs`/`writer.rs` (assembling/splitting the
    sibling-file set); `.prj` CRS goes straight through `crs.rs` with no
    format-specific code
-   **`crs.rs` / `crs/`:** the CRS intermediate representation and its
    resolution paths — `crs/wkt_projjson.rs` (structural WKT→PROJJSON
    translation, default-compiled) and, behind the opt-in `crs-registry`
    feature, `crs/registry.rs` (the embedded-registry reader and id-less-WKT
    name recovery, backed by the sibling
    [`geosetta-crs-data`](https://github.com/dxgeo/geosetta-crs-data) crate)
-   **`cli.rs` / `convert.rs`:** argument parsing and the hub-and-spoke conversion
    pipeline (`read(from) → FeatureCollection → write(to)`)

Design choices for the writer: Snappy-compressed Parquet pages (the codec
implemented from scratch in `compress/snappy.rs`, keeping the zero-crate goal),
2D coordinates, one row group, and property columns typed by scanning all
features (heterogeneous or nested values fall back to a JSON string).


## USAGE

    geosetta input.geojson output.parquet   # GeoJSON    -> GeoParquet
    geosetta input.parquet output.geojson   # GeoParquet -> GeoJSON
    geosetta input.fgb     output.geojson   # FlatGeobuf -> GeoJSON
    geosetta input.fgb     output.parquet   # FlatGeobuf -> GeoParquet
    geosetta input.geojson output.fgb       # GeoJSON    -> FlatGeobuf
    geosetta input.parquet output.fgb       # GeoParquet -> FlatGeobuf
    geosetta input.csv     output.parquet   # CSV (WKT)  -> GeoParquet
    geosetta input.fgb     output.csv       # FlatGeobuf -> CSV (WKT)
    geosetta input.gpkg    output.geojson   # GeoPackage -> GeoJSON (--layer NAME to pick one)
    geosetta input.gpkg    out/ --to geojson # multi-layer GeoPackage -> one file per layer
    geosetta roads.geojson data.gpkg        # create data.gpkg with layer "roads"
    geosetta rivers.csv    data.gpkg        # append layer "rivers" to data.gpkg
    geosetta roads.geojson data.gpkg --rtree # …with a GeoPackage R*Tree spatial index
    geosetta big.geojson   big.parquet --sort-hilbert  # cluster rows by spatial locality
    geosetta big.geojson   big.parquet --progress      # report each stage on stderr
    geosetta parcels.shp   parcels.geojson  # Shapefile  -> GeoJSON (reads sibling .shx/.dbf/.prj)
    geosetta roads.geojson roads.shp        # GeoJSON    -> Shapefile
    geosetta data.gpkg     out/ --to shp    # multi-layer GeoPackage -> one .shp set per layer
    geosetta places.kml    places.geojson   # KML        -> GeoJSON
    geosetta places.kmz    places.parquet   # KMZ        -> GeoParquet (reads every internal .kml)
    geosetta places.geojson places.kml      # GeoJSON    -> KML
    geosetta places.geojson places.kmz      # GeoJSON    -> KMZ (stored, uncompressed)
    # formats may also be given explicitly:
    geosetta in.txt out.bin --from geojson --to parquet
    # the embedded CRS registry (name recovery for id-less WKT1, WKT2 emission, …)
    # is opt-in — build/run with:
    cargo run --features crs-registry -- parcels.shp parcels.parquet
    # or, when installing the published crate:
    cargo install geosetta --features crs-registry
    # "-" means stdin/stdout (needs --from/--to, since there's no path to infer
    # a format from) — pipe any external tool in between read and write, e.g. a
    # reprojection step, since Geosetta itself never reprojects:
    reproject-tool --to EPSG:3857 < in.geojson | geosetta --from geojson --to fgb - out.fgb
    # not supported for Shapefile (sibling .shp/.shx/.dbf/.prj files, no single
    # byte stream to pipe) — route it through a single-buffer format instead:
    geosetta in.shp - --to fgb | reproject-tool --to EPSG:3857 | geosetta - out.shp --from fgb

A runnable example lives in [examples/sample.geojson](examples/sample.geojson).


## PRIOR ART / POSITIONING

Format conversion itself is a solved problem; Geosetta's wager is **how** it
does it, not **that** it does it.

-   [GDAL/OGR](https://gdal.org/) (`ogr2ogr`) is the de facto universal translator:
    ~80+ vector formats converted through a common internal feature model — the
    same hub-and-spoke design described above, and the closest thing to "the
    Pandoc of geospatial." It is comprehensive and battle-tested, but not literally
    every-pair-losslessly: drivers have asymmetric read/write capabilities, vector
    and raster are separate, and support for newer formats (GeoParquet, the native
    Parquet `GEOMETRY` type) is still maturing. Most of the ecosystem is GDAL in
    disguise: DuckDB spatial, GeoPandas / Fiona / pyogrio, and QGIS all embed or
    bind it.
-   [geozero](https://github.com/georust/geozero) is the closest analog in Rust:
    streaming read/write across GeoJSON, FlatGeobuf, GeoPackage, WKB/WKT, MVT, and
    GeoParquet-adjacent formats. `geoarrow-rs` covers GeoArrow/GeoParquet;
    `georust/gdal` is just Rust bindings to GDAL.

What none of them are is **dependency-free**. GDAL needs a C/C++ toolchain plus
PROJ/GEOS; the Rust options pull in crates, and every Parquet path leans on
Apache Arrow (C++ or the `arrow` and `parquet` crates). Geosetta's distinguishing
constraint is exactly that: pure standard-library Rust, every wire format
implemented from its specification. The payoff is a small, embeddable,
audit-friendly, no-supply-chain build (WASM, constrained environments); the cost
is reimplementing — from scratch — codecs, encodings, CRS handling, and the long
tail of formats that GDAL and Arrow already handle robustly.


## ROADMAP

The GeoParquet reader now covers essentially every file DuckDB / Arrow / GDAL
produce in practice: dictionary encoding, multiple row groups, the
SNAPPY/GZIP/ZSTD/LZ4 codecs, `BOOLEAN`/`INT32`/`INT64`/`FLOAT`/`DOUBLE`/string
columns, and DATE/TIMESTAMP rendering. The next steps, in rough priority:

-   **Spatial indexing** — done across all three formats: the shared Hilbert
    primitives, the FlatGeobuf packed R-tree, the opt-in `--sort-hilbert` row
    clustering, and now the opt-in `--rtree` GeoPackage R\*Tree extension (scoped in
    [plans/spatial-index.org](plans/spatial-index.org)). Whether to make `--rtree` the default is the one
    remaining open question there.
-   **More format spokes** — Shapefile and KML/KMZ are both done (see above),
    scoped in [plans/kml.org](plans/kml.org). The next classic candidate is a
    further-out consideration, not yet scoped.
-   **Raster formats** (larger effort, planned) — a genuinely new axis. Raster data
    is a grid of cells, not a set of features, so it needs a second intermediate
    representation (a coverage/grid model) beside the feature IR, with its own
    read/write spokes — e.g. GeoTIFF and Cloud-Optimized GeoTIFF. Conversions
    would compose within the raster IR the same hub-and-spoke way; vector⇄raster
    (rasterize / vectorize) is a separate, further-out concern.
-   **Deferred, lower-priority GeoParquet milestones** (parked in
    [plans/arbitrary-geoparquet.org](plans/arbitrary-geoparquet.org); diminishing returns / testing friction):
    -   `DATA_PAGE_V2` — DuckDB doesn't emit it; a test fixture needs pyarrow.
    -   Brotli — a full from-scratch codec on the scale of ZSTD, rarely used here.
    -   `DECIMAL` / `INT96` / `FIXED_LEN_BYTE_ARRAY`, and 3D (Z/M) geometry — niche.
-   **CRS handling** — implemented: pass-through across all formats, CRS-loss
    warnings, structural WKT↔PROJJSON translation for geographic and common
    projected CRSes (scoped in [plans/crs.org](plans/crs.org)), and — opt-in via
    `--features crs-registry` — a full embedded `proj.db` registry
    ([`geosetta-crs-data`](https://github.com/dxgeo/geosetta-crs-data), plan in
    that repo's `crs-registry.org`) resolving any `(authority, code)`, recovering
    codes from id-less WKT names (geographic and projected, ellipsoid-validated),
    and emitting WKT2:2019. Remaining: re-scoping whether the registry's own
    resolution makes any of the structural crosswalk's hand-maintained tables
    redundant in the default (`crs-registry`-off) build without regressing it,
    and whether/when `crs-registry` should become a default-on feature.
-   **Reprojection composability** — implemented (scoped in
    [plans/reproject-composability.org](plans/reproject-composability.org)).
    Geosetta still never reprojects itself, but two seams now let an external
    tool do it as a stage between read and write: `-` as `<input>`/`<output>`
    pipes any external CLI through stdin/stdout (not supported for Shapefile,
    which is multi-file — route it through a single-buffer format instead), and
    `FeatureCollection::for_each_position_mut`/`for_each_position_run_mut`
    (`src/feature.rs`, `src/geometry/mod.rs`) let a linked-in Rust reprojection
    *library* rewrite coordinates in place, the latter yielding whole
    contiguous coordinate runs for backends that batch. Verified against three
    independent, real tools — GDAL (`ogr2ogr`), PROJ (`cs2cs`), and the
    [`wbprojection`](https://crates.io/crates/wbprojection) crate — in
    `tests/reproject_pipe.rs`.

Detailed design notes live in [plans/](plans/README.org).


## LICENSE

Released under the [MIT License](LICENSE).

