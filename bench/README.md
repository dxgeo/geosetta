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
- geosetta cannot yet read **GDAL-written Parquet** (a known level-encoding
  gap), so the `parquet->geojson` read input is DuckDB-written Parquet, which all
  three tools read.
- All tools produce equivalent output (same feature count), so this measures
  speed on the same work — not one tool doing less.

## Indicative numbers

100,000 mixed features (~80% points, 15% lines, 5% polygons, 4 props; points
only for the Shapefile rows, since `.shp` can't mix geometry types), on an
Apple-silicon laptop, geosetta 0.21.0:

| conversion           | geosetta | ogr2ogr | duckdb | vs ogr | vs duck |
|----------------------|---------:|--------:|-------:|-------:|--------:|
| geojson→parquet      |   107 ms |  670 ms | 641 ms |  6.2×  |  6.0×   |
| geojson→fgb          |   121 ms |  705 ms | 728 ms |  5.8×  |  6.0×   |
| geojson→gpkg         |   148 ms |  793 ms | 781 ms |  5.3×  |  5.3×   |
| parquet→geojson      |    49 ms |  434 ms | 635 ms |  8.8×  | 12.9×   |
| fgb→geojson          |    47 ms |  430 ms | 748 ms |  9.1×  | 15.8×   |
| gpkg→geojson         |    58 ms |  465 ms | 653 ms |  8.1×  | 11.3×   |
| geojson→csv          |   144 ms |  885 ms | 703 ms |  6.2×  |  4.9×   |
| csv→geojson          |    87 ms |  318 ms | 692 ms |  3.7×  |  8.0×   |
| geojson→shp          |   123 ms |  872 ms |1103 ms |  7.1×  |  9.0×   |
| shp→geojson          |    51 ms |  424 ms | 777 ms |  8.3×  | 15.2×   |
| gpkg(3857)→parquet   |    52 ms |  161 ms | 106 ms |  3.1×  |  2.1×   |
| shp(3857)→parquet    |    50 ms |  223 ms | 269 ms |  4.5×  |  5.4×   |
| fgb(3857)→gpkg       |   102 ms |  280 ms | 313 ms |  2.7×  |  3.1×   |
| geojson→parquet (wide, 200 cols) | 1457 ms | 11836 ms | 12229 ms |  8.1×  |  8.4×  |
| geojson→fgb (wide, 200 cols)     | 1221 ms | 11213 ms | 12549 ms |  9.2×  | 10.3×  |
| parquet→geojson (wide, 200 cols) |  578 ms |  2875 ms |  2431 ms |  5.0×  |  4.2×  |

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
