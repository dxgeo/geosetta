# Geosetta

## OVERVIEW

Geosetta exports any vector geospatial format to any other: one library, no
dependencies, taking any open-standard vector input to any conceivable vector
output. If you've heard of [Pandoc](https://pandoc.org/), the idea is similar.

Vector is the focus today. **Raster** is a planned future direction — it needs a
second intermediate representation beside the feature IR (a coverage/grid model
rather than `FeatureCollection`); see the roadmap.


## ARCHITECTURE

Design priorities for the project, in order. Geosetta aims to be:

-   Extensible
-   Performant
-   Universal
-   Lightweight
-   Dependency-free


## STATUS

Current version: **0.24.0**.

Eight formats are supported, all routed through a shared feature IR
(`read(from) → FeatureCollection → write(to)`), so every format composes with
every other automatically — no per-pair code. Any input converts to any output
(CSV→GeoParquet, FlatGeobuf→GeoJSON, GeoPackage↔Shapefile, KMZ→GeoParquet, …),
validated against DuckDB / GDAL.

-   **GeoJSON** (read + write)
-   **GeoParquet** (read + write; reads common foreign files — see below)
-   **FlatGeobuf** (read + write) on a from-scratch FlatBuffers reader and builder;
    written with a **packed Hilbert R-tree** spatial index (GDAL bbox queries work)
-   **CSV** with a WKT geometry column (read + write), types lightly inferred
-   **WKT** — one geometry per line (read + write)
-   **GeoPackage** (read + write) on a from-scratch SQLite reader **and** writer. A
    `.gpkg` is multi-layer, so reading fans out over all feature tables and writing
    a layer creates the file or appends to it (upserting by name); the `--layer`
    option and the directory-vs-single-file output rules drive the fan-out. Our
    output passes SQLite's `integrity_check` and is read by GDAL. An opt-in
    `--rtree` flag also writes the GeoPackage RTree extension (SQLite's R\*Tree
    virtual table, built from scratch); the index passes `rtreecheck()` and
    answers GDAL/DuckDB bbox queries.
-   **Shapefile** (read + write) — the first multi-file spoke: sibling
    `.shp`/`.shx`/`.dbf`/`.prj`(/`.cpg`) files sharing a basename. A from-scratch
    mixed-endian `.shp`/`.shx` geometry codec (with real ring classification —
    shoelace winding + appearance-order grouping — for the Polygon/MultiPolygon
    disambiguation) and a dBase III/IV `.dbf` attribute codec. Composes with
    GeoPackage's multi-layer fan-out for free (one `layer.shp` sibling set per
    `.gpkg` layer, either direction). Validated against real DuckDB-generated
    fixtures and GDAL/DuckDB's own readers.
-   **KML** and **KMZ** (read + write) on a from-scratch XML reader/writer
    (`src/xml.rs`) and a minimal ZIP container codec (`src/zip.rs`). CRS is always
    WGS 84 (KML has no other CRS concept). `.kmz` reading flattens **every**
    internal `*.kml` entry rather than just the first, since a real multi-layer
    producer's root `doc.kml` can be nothing but a `<NetworkLink>`; writing packs
    one stored (uncompressed) entry — no DEFLATE encoder in the crate yet, so
    `.kmz` output is bigger than a compressed one but universally readable.
    Validated against real GDAL (`ogrinfo`, `ogr2ogr -f LIBKML`), DuckDB
    fixtures, and a real Info-ZIP archive for the deflate read path.

**Z and M ordinates travel end to end.** `Position` carries optional `z` and `m`,
so every spoke reads and writes Z, and M is supported everywhere a format can
express it: WKB, WKT/CSV, GeoPackage, FlatGeobuf, Shapefile (both within
Z-shapes and the four M-only shape types), and GeoParquet's `" Z"`/`" M"`/`" ZM"`
`geometry_types` suffixes. GeoJSON and KML/KMZ are the only spokes without M —
neither spec has an M concept at all. Every dimension-flag ambiguity (WKB's
ISO-vs-EWKB, WKT's keyword-vs-bare-tuple) was confirmed against real DuckDB/GDAL
output before implementing ([plans/zm-geometry.org](plans/zm-geometry.org)).

**Lossy conversions warn; they never succeed silently.** Whenever the target
format cannot represent something the source carries — a CRS it has no channel
for, M ordinates bound for GeoJSON/KML, a `.dbf` field name or value past
dBase's length limits — Geosetta prints a warning to stderr and still performs
the conversion (`--quiet` suppresses). This is a standing convention with a single collection
point in `main.rs`, so a new check is one line rather than new plumbing
([plans/lossy-conversion-warnings.org](plans/lossy-conversion-warnings.org)).

Each format's **coordinate reference system** is carried through the IR and
re-emitted in the target's own representation (authority+code, WKT, or PROJJSON)
— Geosetta *labels* CRS, it never reprojects and never resolves. A source CRS
resolves in two ways, tried in order: (1) **structural translation** of a source
WKT definition into a complete, resolvable PROJJSON object, and the reverse —
geographic and the common projected CRSes (WKT2, and WKT1 for the unambiguous
methods), verified against PROJ's `projinfo` at 100% identification; (2) an
*id-reference + warning* fallback. There is no third: a bare authority code with
no definition to translate cannot be expanded by a crate that ships no registry,
so rather than guess, Geosetta says so — and offers a seam.

That seam is `--print-crs-code` and `--crs`, which turn the gap into ordinary
shell composition: the first reports the code Geosetta could not resolve, the
second accepts the WKT or PROJJSON that some *other* tool resolved it to.
Geosetta spawns nothing itself — every external step is visible in the command
the user typed — and it is entirely agnostic about which tool that is. Its code
names none; its *docs* recommend one:
[`geoscribe`](https://github.com/dxgeo/geoscribe) for a bare authority code
(PROJ's `proj.db` in ~1 MB, bare definition on stdout, pipes straight into
`--crs`), with `projinfo -o PROJJSON -q` or `gdalsrsinfo -o projjson` equally
fine if PROJ or GDAL is already installed:

```sh
geosetta in.fgb out.parquet \
  --crs <(geoscribe "$(geosetta in.fgb --print-crs-code)" --projjson)
```

An *id-less* definition (Esri-flavor `.prj`, no authority id at all) has no code
for `--print-crs-code` to report, so it needs a resolver that can **identify** a
CRS from its name and structure. `geoscribe --identify` does that — it validates
the name against the WKT's own ellipsoid, and where several real CRSes fit
equally well it prints nothing and exits nonzero rather than guessing, which
composes with `--crs`'s hard error on empty input:

```sh
geoscribe --identify --projjson parcels.prj \
  | geosetta parcels.shp parcels.parquet --crs -
```

`projinfo --identify` serves the same role with confidence percentages if you
prefer to eyeball the candidates yourself. Either way this is weaker evidence
than a stated id — see
[plans/crs-external-resolution.org](plans/crs-external-resolution.org).

The **GeoParquet** path is the most exercised. Write output is Snappy-compressed
and validated by DuckDB's spatial engine as genuine, queryable GeoParquet; the
reader recovers a GeoParquet file Geosetta wrote back to equivalent GeoJSON, and
the geojson→parquet→geojson→parquet round trip is byte-for-byte stable.

The reader also handles common **foreign** GeoParquet — in practice essentially
anything DuckDB, GDAL, or Arrow writes, each case pinned by a real fixture from
the tool that produces it:

-   SNAPPY, GZIP, ZSTD, LZ4\_RAW, or no compression (the ZSTD, GZIP/DEFLATE and
    LZ4 decoders written from scratch in `compress/`), PLAIN and dictionary
    encodings, `DATA_PAGE_V2`, multiple pages and row groups.
-   `BOOLEAN`, `INT32`/`INT64`, `FLOAT`/`DOUBLE` and `BYTE_ARRAY` strings; `DATE`
    and `TIMESTAMP` as ISO 8601; `DECIMAL` (`INT32`/`INT64`/`FIXED_LEN_BYTE_ARRAY`-backed,
    rendered as an exact base-10 string); `INT96` legacy Impala/Hive timestamps;
    the `JSON` logical type; and single-level lists of scalars (GDAL's
    `StringList`, DuckDB's `LIST(VARCHAR)`), which flow through the same JSON-text
    fallback every writer already has for array-valued properties — **no**
    write-side code needed.
-   A walk of the file's actual schema *tree*, not just each leaf's own repetition
    type, so a column nested under an OPTIONAL group decodes correctly. The case
    that matters in practice is GDAL/OGR's GeoParquet 1.1 `geometry_bbox`
    "covering" column, which it writes by default: Geosetta recognizes it via the
    `geo` metadata and excludes it from `properties` rather than surfacing it as a
    fake column.
-   Parquet's **native** `GEOMETRY`/`GEOGRAPHY` logical type, not just the classic
    `geo` key/value metadata convention that some writers now omit entirely — the
    geometry column's name and CRS are recovered straight from the schema.

Remaining gaps — a list of structs or a list nested inside another list, actually
decoding (not just detecting) multiple geometry columns, and Brotli — are
reported as clear errors and tracked in
[plans/arbitrary-geoparquet.org](plans/arbitrary-geoparquet.org) (and, for
multiple geometry columns specifically,
[plans/multi-geometry-columns.org](plans/multi-geometry-columns.org)).


## IMPLEMENTATION

Language: Rust (edition 2024). Every wire format is implemented from its
specification rather than pulled from a crate:

-   **`feature.rs`:** the shared, format-neutral feature model (`Feature` /
    `FeatureCollection`) — the IR every format converts to and from; all spokes
    depend on it, not on each other
-   **`format.rs`:** the `Format` enum every entry point is keyed by — extension
    and name parsing, plus each format's capability answers (`display_name` for
    the one spelling used in every user-facing message, `supports_m`). Its module
    doc carries the *adding a format* contract: which obligations the compiler
    enforces and which two (`parse`/`from_path`, both string-keyed) it silently
    does not
-   **`json/`:** standard-library JSON parser and value model
-   **`geojson/`:** GeoJSON parsing and serialization (both directions)
-   **`schema.rs`:** property-column inference (type lattice, per-row cells) shared
    by the Parquet and FlatGeobuf writers
-   **`flatbuffers.rs`:** a minimal, from-scratch FlatBuffers reader **and** builder
    (vtables, offsets, alignment, vectors, strings, tables)
-   **`flatgeobuf/`:** FlatGeobuf reader and writer — magic + header, the packed
    Hilbert R-tree (skipped on read, built on write via `spatial.rs`), and
    feature/geometry/property coding via `flatbuffers.rs`
-   **`geometry/`:** geometry model with optional Z/M ordinates, bounding box, and
    the shared codecs — WKB (`wkb.rs`) and WKT (`wkt.rs`), each encoder + decoder
-   **`csv.rs`:** CSV spoke — RFC 4180 rows with a WKT geometry column and
    type-inferred property columns
-   **`spatial.rs`:** shared spatial-ordering primitives — a Hilbert-curve encoder
    and feature-locality sort, used to build FlatGeobuf's packed R-tree
-   **`sqlite.rs`:** a minimal, from-scratch SQLite reader **and** whole-file writer
    (header, multi-page b-tree walk/pack with overflow chains — including a
    `sqlite_master` that grows past page 1 — schema-only master rows for virtual
    tables and triggers, record coding, CREATE TABLE parsing) — what our
    GeoPackage output passes `integrity_check` on
-   **`geopackage/`:** GeoPackage reader and writer on `sqlite.rs` — layer fan-out
    from `gpkg_contents`, GeoPackage Binary geometry wrapping WKB, and
    create-or-append (upsert) writes. `geopackage/rtree.rs` builds the opt-in RTree
    extension: a packed R\*Tree in SQLite's node blob format, its shadow tables,
    and the maintenance triggers
-   **`compress/`:** format-agnostic `bytes -> bytes` codecs implemented from spec —
    Snappy, GZIP/DEFLATE (RFC 1951/1952), ZSTD (RFC 8878, FSE + Huffman +
    sequences), and LZ4 block. Not Parquet-specific, so reusable by future formats
-   **`parquet/`:** Thrift compact-protocol writer, schema inference, GeoParquet
    `geo` metadata, and the Parquet file writer; `parquet/reader.rs` is the inverse
    — footer/schema parsing, page iteration, decompression via `compress/`,
    RLE/bit-pack levels, PLAIN and dictionary decoding, across multiple row groups
-   **`shapefile/`:** the Shapefile spoke — `geometry.rs` (mixed-endian
    `.shp`/`.shx` codec, Z/M shape types, ring classification), `dbf.rs` (dBase
    III/IV attribute codec), `reader.rs`/`writer.rs` (assembling/splitting the
    sibling-file set); `.prj` CRS goes straight through `crs.rs` with no
    format-specific code
-   **`crs.rs` / `crs/`:** the CRS intermediate representation and its resolution
    paths — `crs/wkt_projjson.rs` (structural WKT↔PROJJSON translation) plus
    `Crs::from_definition_text`, which accepts a `--crs` override as text in
    either dialect (sniffed on the first non-whitespace byte) and runs it through
    the same identity lift every reader uses, and `Crs::authority_code`, the
    scriptable `AUTHORITY:CODE` form `--print-crs-code` emits. No registry, and no
    dependency — optional or otherwise — to hold one
-   **`cli.rs` / `convert.rs`:** argument parsing and the hub-and-spoke conversion
    pipeline (`read(from) → FeatureCollection → write(to)`)

Design choices for the writer: Snappy-compressed Parquet pages (the codec
implemented from scratch in `compress/snappy.rs`, keeping the zero-crate goal),
one row group, and property columns typed by scanning all features
(heterogeneous or nested values fall back to a JSON string).


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
    geosetta aus.gpkg      aus.geojson --quiet         # silence lossy-conversion warnings (on by default)
    geosetta parcels.shp   parcels.geojson  # Shapefile  -> GeoJSON (reads sibling .shx/.dbf/.prj)
    geosetta roads.geojson roads.shp        # GeoJSON    -> Shapefile
    geosetta data.gpkg     out/ --to shp    # multi-layer GeoPackage -> one .shp set per layer
    geosetta places.kml    places.geojson   # KML        -> GeoJSON
    geosetta places.kmz    places.parquet   # KMZ        -> GeoParquet (reads every internal .kml)
    geosetta places.geojson places.kml      # GeoJSON    -> KML
    geosetta places.geojson places.kmz      # GeoJSON    -> KMZ (stored, uncompressed)
    # formats may also be given explicitly:
    geosetta in.txt out.bin --from geojson --to parquet
    # a CRS geosetta can't resolve (a bare authority code, no definition) is resolved
    # by whatever tool you like and handed back — geosetta never runs it for you:
    geosetta parcels.fgb --print-crs-code                 # -> EPSG:7844
    geosetta parcels.fgb parcels.shp --crs gda2020.wkt    # WKT or PROJJSON, either way
    geoscribe EPSG:7844 --wkt | geosetta parcels.fgb parcels.shp --crs -
    projinfo -o WKT1_GDAL -q EPSG:7844 | geosetta parcels.fgb parcels.shp --crs -
    # or in one line, with the code looked up on the fly:
    geosetta in.fgb out.parquet \
      --crs <(geoscribe "$(geosetta in.fgb --print-crs-code)" --projjson)
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
    ~80+ vector formats through a common internal feature model — the same
    hub-and-spoke design described above, and the closest thing to "the Pandoc of
    geospatial." Comprehensive and battle-tested, but not literally
    every-pair-losslessly: drivers have asymmetric read/write capabilities, vector
    and raster are separate, and newer formats (GeoParquet, the native Parquet
    `GEOMETRY` type) are still maturing. Most of the ecosystem is GDAL in disguise:
    DuckDB spatial, GeoPandas / Fiona / pyogrio, and QGIS all embed or bind it.
-   [geozero](https://github.com/georust/geozero) is the closest analog in Rust:
    streaming read/write across GeoJSON, FlatGeobuf, GeoPackage, WKB/WKT, MVT, and
    GeoParquet-adjacent formats. `geoarrow-rs` covers GeoArrow/GeoParquet;
    `georust/gdal` is just Rust bindings to GDAL.

What none of them are is **dependency-free**. GDAL needs a C/C++ toolchain plus
PROJ/GEOS; the Rust options pull in crates, and every Parquet path leans on
Apache Arrow. Geosetta's distinguishing constraint is exactly that: pure
standard-library Rust, every wire format implemented from its specification. The
payoff is a small, embeddable, audit-friendly, no-supply-chain build (WASM,
constrained environments); the cost is reimplementing — from scratch — codecs,
encodings, CRS handling, and the long tail of formats GDAL and Arrow already
handle robustly. Reprojection is not among those costs: Geosetta passes the
source CRS through untouched rather than transforming coordinates (see below).


## ROADMAP

The GeoParquet reader now covers essentially every file DuckDB / Arrow / GDAL
produce in practice (see STATUS). The next steps, in rough priority:

-   **Spatial indexing** — done across all three formats: the shared Hilbert
    primitives, the FlatGeobuf packed R-tree, the opt-in `--sort-hilbert` row
    clustering, and the opt-in `--rtree` GeoPackage R\*Tree extension (scoped in
    [plans/spatial-index.org](plans/spatial-index.org)). Whether `--rtree` should
    be the default is the one open question there.
-   **More format spokes** — Shapefile and KML/KMZ are both done (see above),
    scoped in [plans/kml.org](plans/kml.org). The next classic candidate is a
    further-out consideration, not yet scoped.
-   **CRS handling** — implemented: pass-through across all formats, lossy-CRS
    warnings, and structural WKT↔PROJJSON translation in both directions for
    geographic and common projected CRSes (scoped in [plans/crs.org](plans/crs.org)
    and [plans/projjson-to-wkt.org](plans/projjson-to-wkt.org)). What structural
    translation *can't* reach — a bare authority code with no definition — is
    closed by composition rather than by an embedded registry:
    `--print-crs-code` reports the code, `--crs <path|->` accepts the WKT or
    PROJJSON any other tool resolved it to, and geosetta spawns nothing itself
    (scoped in
    [plans/crs-external-resolution.org](plans/crs-external-resolution.org)).
    The opt-in `crs-registry` feature and its sibling-crate dependency were
    removed in 0.24.0 — they were the crate's only dependency of any kind.
    **The default build is unchanged by this**: every registry entry point was a
    `None` stub without the feature, so removing them is a no-op there, and the
    bare build's known weakness (structural translation identifies Esri-flavor
    WGS 84 at only ~70%, and NAD83/NAD27 outright wrong) is the pre-existing
    status quo rather than a new regression. Only a `--features crs-registry`
    build loses anything, and what it loses is name recovery from an id-less
    WKT — narrow in practice (geographic CRSes only, sources declaring neither
    authority nor code, GeoParquet output only). Restoring it properly belongs
    in `geoscribe`; see that plan's § CROSS-REPO FOLLOW-UP.
-   **Reprojection composability** — implemented (scoped in
    [plans/reproject-composability.org](plans/reproject-composability.org)).
    Geosetta still never reprojects itself, but two seams let an external tool do
    it as a stage between read and write: `-` as `<input>`/`<output>` pipes any
    external CLI through stdin/stdout (not supported for Shapefile, which is
    multi-file — route it through a single-buffer format instead), and
    `FeatureCollection::for_each_position_mut`/`for_each_position_run_mut`
    (`src/feature.rs`, `src/geometry/mod.rs`) let a linked-in Rust reprojection
    *library* rewrite coordinates in place, the latter yielding whole contiguous
    coordinate runs for backends that batch. Verified against three independent,
    real tools — GDAL (`ogr2ogr`), PROJ (`cs2cs`), and the
    [`wbprojection`](https://crates.io/crates/wbprojection) crate — in
    `tests/reproject_pipe.rs`.
-   **Raster formats** (larger effort, planned) — a genuinely new axis. Raster data
    is a grid of cells, not a set of features, so it needs a second intermediate
    representation (a coverage/grid model) beside the feature IR, with its own
    read/write spokes — e.g. GeoTIFF and Cloud-Optimized GeoTIFF. Conversions
    would compose within the raster IR the same hub-and-spoke way; vector⇄raster
    (rasterize / vectorize) is a separate, further-out concern.
-   **Deferred, lower-priority GeoParquet milestones** (parked in
    [plans/arbitrary-geoparquet.org](plans/arbitrary-geoparquet.org); diminishing
    returns / testing friction): Brotli (a from-scratch codec on the scale of ZSTD,
    rarely used here); a list of structs or a list nested inside another list
    (single-level lists of scalars are done); and multiple geometry columns in one
    file, today detected and rejected with a clear error — actually decoding one
    needs an IR change the reader alone can't make, scoped separately in
    [plans/multi-geometry-columns.org](plans/multi-geometry-columns.org).

Detailed design notes live in [plans/](plans/README.org).


## LICENSE

Released under the [MIT License](LICENSE).
