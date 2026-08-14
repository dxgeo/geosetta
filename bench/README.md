# Benchmarks

Two complementary benchmarks:

- **`tests/perf.rs`** — geosetta-only throughput (read/write per format), driving
  the real binary end-to-end. Run with:

  ```sh
  cargo test --release --test perf -- --ignored --nocapture
  ```

  Tune with `GEOSETTA_BENCH_N` (feature count) and `GEOSETTA_BENCH_COLS`
  (wide-table column count).

- **`bench/compare.py`** — geosetta vs **ogr2ogr (GDAL)** vs **duckdb**, best-of-3
  whole-process wall-clock. Rather than the full N×M pair matrix (redundant
  under the hub-and-spoke IR — cost is `read(X) + write(Y)` with no pairwise
  interaction, except CRS translation), it covers a read and a write benchmark
  for every format, plus three EPSG:3857-reprojected pairs
  (`gpkg(3857)->parquet`, `shp(3857)->parquet`, `fgb(3857)->gpkg`) that chain
  two non-GeoJSON formats — GeoJSON is always WGS 84, so a geojson-anchored
  conversion never touches the WKT/PROJJSON CRS-translation path the way these
  do — and three wide-column cases (many typed properties per feature, narrow
  geometry) that isolate `schema::infer_columns`'s per-column cost, which the
  narrow 4-property cases above barely touch. Needs `ogr2ogr` and `duckdb` on
  `PATH`.

  ```sh
  cargo build --release
  python3 bench/compare.py           # N=100000 features, wide cases at 50000 x 200 cols
  N=200000 python3 bench/compare.py
  WIDE_N=50000 WIDE_COLS=200 python3 bench/compare.py
  ```

## Reading `compare.py` results fairly

- Timing is **whole-process**: it includes binary/library startup and file I/O,
  the true cost of a one-shot CLI conversion. Startup is very asymmetric
  (geosetta ~2 ms vs ogr2ogr / duckdb+spatial ~100 ms), which flatters geosetta
  on the fastest conversions — it still wins clearly once startup is subtracted,
  and the tiny startup is itself a dependency-free advantage.
- **DuckDB's vector I/O** (GeoJSON / FlatGeobuf / GeoPackage) is GDAL-backed;
  only its Parquet path is native, so its non-Parquet numbers track GDAL's.
- The `parquet->geojson` read input is DuckDB-written Parquet, since all three
  tools read it (geosetta also reads GDAL-written Parquet now — see
  [plans/arbitrary-geoparquet.org](../plans/arbitrary-geoparquet.org)'s
  covering-bbox/nested-schema milestone — but DuckDB's output stays the
  common input here for a same-file comparison).
- All tools produce equivalent output (same feature count), so this measures
  speed on the same work — not one tool doing less.

## Indicative numbers

100,000 mixed features (~80% points, 15% lines, 5% polygons, 4 props; points
only for the Shapefile rows, since `.shp` can't mix geometry types), on an
Apple-silicon laptop, geosetta 0.21.1:

| conversion           | geosetta | ogr2ogr | duckdb | vs ogr | vs duck |
|----------------------|---------:|--------:|-------:|-------:|--------:|
| geojson→parquet      |    89 ms |  656 ms | 662 ms |  7.4×  |  7.4×   |
| geojson→fgb          |   120 ms |  682 ms | 728 ms |  5.7×  |  6.0×   |
| geojson→gpkg         |   147 ms |  777 ms | 759 ms |  5.3×  |  5.2×   |
| parquet→geojson      |    49 ms |  420 ms | 613 ms |  8.7×  | 12.6×   |
| fgb→geojson          |    45 ms |  419 ms | 712 ms |  9.3×  | 15.7×   |
| gpkg→geojson         |    55 ms |  397 ms | 622 ms |  7.2×  | 11.4×   |
| geojson→csv          |   140 ms |  859 ms | 700 ms |  6.1×  |  5.0×   |
| csv→geojson          |    85 ms |  288 ms | 658 ms |  3.4×  |  7.7×   |
| geojson→shp          |   121 ms |  822 ms |1073 ms |  6.8×  |  8.9×   |
| shp→geojson          |    49 ms |  413 ms | 747 ms |  8.4×  | 15.3×   |
| gpkg(3857)→parquet   |    50 ms |  155 ms | 102 ms |  3.1×  |  2.1×   |
| shp(3857)→parquet    |    49 ms |  215 ms | 252 ms |  4.4×  |  5.1×   |
| fgb(3857)→gpkg       |    95 ms |  266 ms | 297 ms |  2.8×  |  3.1×   |
| geojson→parquet (wide, 200 cols) | 1178 ms | 10792 ms | 11766 ms |  9.2×  | 10.0×  |
| geojson→fgb (wide, 200 cols)     | 1212 ms | 10849 ms | 12126 ms |  8.9×  | 10.0×  |
| parquet→geojson (wide, 200 cols) |  536 ms |  2812 ms |  2342 ms |  5.2×  |  4.4×  |

The three `(3857)` rows chain two non-GeoJSON formats through a real
EPSG:3857 CRS (built via `ogr2ogr -t_srs`), rather than GeoJSON's fixed
WGS 84, so they exercise the WKT/PROJJSON CRS-translation path a
geojson-anchored conversion never touches; take the smaller margin there as
one data point, not a general rule — it moves with dataset/format choice like
everything else here.

The three `(wide, 200 cols)` rows use 50,000 points with 200 typed properties
each instead of the narrow 4-property fixture — this is where geosetta's
margin actually *widens* (8–10×, vs 5–9× on the narrow cases above): both
ogr2ogr and duckdb scale far worse with column count than geosetta does.

Hardware, tool versions, and dataset all move these figures; re-run locally for
your own baseline.
