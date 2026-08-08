# Pantograph


## OVERVIEW

Pantograph is a library for exporting any vector geospatial format to any other vector geospatial format.
The basic idea is to have one library, with no other dependencies, that can take any input vector geospatial file and output any other conceivable vector format, provided it is an open standard.
If you've heard of [Pandoc](https://pandoc.org/), the idea is similar here.


## ARCHITECTURE

This is the list of priorities for this project in terms of design/architecture.
The project aims to be:

-   Extensible
-   Performant
-   Universal
-   Lightweight
-   Dependency-free


## STATUS

Current version: **0.12.0**.

Five formats are supported, all routed through a shared feature IR
(`read(from) → FeatureCollection → write(to)`), so every format composes with
every other automatically — no per-pair code:

-   **GeoJSON** (read + write)
-   **GeoParquet** (read + write; reads common foreign files — see below)
-   **FlatGeobuf** (read + write) on a from-scratch FlatBuffers reader and builder;
    written index-less (`index_node_size = 0`), which GDAL/DuckDB read fine
-   **CSV** with a WKT geometry column (read + write), types lightly inferred
-   **WKT** — one geometry per line (read + write)

Any input converts to any output: e.g. CSV→GeoParquet, FlatGeobuf→GeoJSON,
GeoParquet→FlatGeobuf all work. Validated against DuckDB / GDAL.

Both directions of the **GeoJSON ⇄ GeoParquet** path are implemented and working,
written in Rust using only the standard library (zero external crates, in
keeping with the dependency-free goal). Write output is Snappy-compressed and
validated by DuckDB's spatial engine as genuine, queryable GeoParquet; the
reader recovers a GeoParquet file Pantograph wrote back to equivalent GeoJSON,
and the geojson→parquet→geojson→parquet round trip is byte-for-byte stable.

The reader also handles common **foreign** GeoParquet: DuckDB's default output
(dictionary-encoded columns, multiple row groups) reads back correctly,
geometry included, under SNAPPY, GZIP, ZSTD, LZ4<sub>RAW</sub>, or no compression — the
ZSTD, GZIP/DEFLATE, and LZ4 decoders are implemented from scratch in
`parquet/zstd.rs`, `parquet/gzip.rs`, and `parquet/lz4.rs`. Property columns may
be `BOOLEAN`, `INT32=/=INT64`, `FLOAT=/=DOUBLE`, or `BYTE_ARRAY` strings, with
`DATE` and `TIMESTAMP` columns rendered as ISO 8601 strings. Remaining gaps —
`DATA_PAGE_V2`, `DECIMAL=/=INT96`, nested columns, 3D geometry, Brotli — are
reported as clear errors and tracked in
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
-   **`flatgeobuf/`:** FlatGeobuf reader and writer — magic + header, spatial-index
    skip on read (index-less on write), and feature/geometry/property coding via
    `flatbuffers.rs`
-   **`geometry/`:** geometry model, bounding box, and the shared geometry codecs —
    WKB (`wkb.rs`) and WKT (`wkt.rs`), each encoder + decoder
-   **`csv.rs`:** CSV spoke — RFC 4180 rows with a WKT geometry column and
    type-inferred property columns
-   **`compress/`:** format-agnostic `bytes -> bytes` codecs implemented from spec —
    Snappy, GZIP/DEFLATE (RFC 1951/1952), ZSTD (RFC 8878, FSE + Huffman +
    sequences), and LZ4 block. Not Parquet-specific, so reusable by future formats
-   **`parquet/`:** Thrift compact-protocol writer, schema inference, GeoParquet
    `geo` metadata, and the Parquet file writer
-   **`parquet/reader.rs`:** the inverse — footer/schema parsing, page iteration
    (dictionary + data pages), decompression via `compress/`, RLE/bit-pack levels,
    PLAIN and dictionary value decoding, across multiple row groups
-   **`cli.rs` / `convert.rs`:** argument parsing and the hub-and-spoke conversion
    pipeline (`read(from) → FeatureCollection → write(to)`)

Design choices for the writer: Snappy-compressed Parquet pages (the codec
implemented from scratch in `compress/snappy.rs`, keeping the zero-crate goal),
2D coordinates, one row group, and property columns typed by scanning all
features (heterogeneous or nested values fall back to a JSON string).


## USAGE

    panto input.geojson output.parquet   # GeoJSON    -> GeoParquet
    panto input.parquet output.geojson   # GeoParquet -> GeoJSON
    panto input.fgb     output.geojson   # FlatGeobuf -> GeoJSON
    panto input.fgb     output.parquet   # FlatGeobuf -> GeoParquet
    panto input.geojson output.fgb       # GeoJSON    -> FlatGeobuf
    panto input.parquet output.fgb       # GeoParquet -> FlatGeobuf
    panto input.csv     output.parquet   # CSV (WKT)  -> GeoParquet
    panto input.fgb     output.csv       # FlatGeobuf -> CSV (WKT)
    # formats may also be given explicitly:
    panto in.txt out.bin --from geojson --to parquet

A runnable example lives in [examples/sample.geojson](examples/sample.geojson).


## PRIOR ART / POSITIONING

Format conversion itself is a solved problem; Pantograph's wager is **how** it
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
Apache Arrow (C++ or the `arrow` and `parquet` crates). Pantograph's distinguishing
constraint is exactly that: pure standard-library Rust, every wire format
implemented from its specification. The payoff is a small, embeddable,
audit-friendly, no-supply-chain build (WASM, constrained environments); the cost
is reimplementing — from scratch — codecs, encodings, CRS handling, and the long
tail of formats that GDAL and Arrow already handle robustly.


## ROADMAP

The GeoParquet reader now covers essentially every file DuckDB / Arrow / GDAL
produce in practice: dictionary encoding, multiple row groups, the
SNAPPY/GZIP/ZSTD/LZ4 codecs, =BOOLEAN=/=INT32=/=INT64=/=FLOAT=/=DOUBLE=/string
columns, and DATE/TIMESTAMP rendering. The next steps, in rough priority:

-   **More format spokes** — GeoPackage is the natural next one (a from-scratch
    SQLite reader/writer; multi-layer, so it's a layer-aware wrapper around the
    single-layer IR) — scoped in [plans/geopackage.org](plans/geopackage.org).
    Shapefile is another classic.
-   **FlatGeobuf spatial index** (optional) — write the packed Hilbert R-tree so
    other tools get fast spatial queries; we currently write index-less files.
    See [plans/flatgeobuf.org](plans/flatgeobuf.org).
-   **Deferred, lower-priority GeoParquet milestones** (parked in
    [plans/arbitrary-geoparquet.org](plans/arbitrary-geoparquet.org); diminishing returns / testing friction):
    -   `DATA_PAGE_V2` — DuckDB doesn't emit it; a test fixture needs pyarrow.
    -   Brotli — a full from-scratch codec on the scale of ZSTD, rarely used here.
    -   `DECIMAL` / `INT96` / `FIXED_LEN_BYTE_ARRAY`, and 3D (Z/M) geometry — niche.
-   **CRS handling** beyond the CRS84 default, once a second CRS-bearing format lands.

Detailed design notes live in [plans/](plans/README.md).


## LICENSE

Released under the [MIT License](LICENSE).

