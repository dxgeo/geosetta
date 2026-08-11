# Benchmarks

Two complementary benchmarks:

- **`tests/perf.rs`** — geosetta-only throughput (read/write per format), driving
  the real binary end-to-end. Run with:

  ```sh
  cargo test --release --test perf -- --ignored --nocapture
  ```

  Tune with `GEOSETTA_BENCH_N` (feature count) and `GEOSETTA_BENCH_COLS`
  (wide-table column count).

- **`bench/compare.py`** — geosetta vs **ogr2ogr (GDAL)** vs **duckdb** on the
  same conversions, best-of-3 whole-process wall-clock. Needs `ogr2ogr` and
  `duckdb` on `PATH`.

  ```sh
  cargo build --release
  python3 bench/compare.py           # N=100000 features
  N=200000 python3 bench/compare.py
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

200,000 mixed features (~80% points, 15% lines, 5% polygons, 4 props), on an
Apple-silicon laptop, geosetta 0.19.0:

| conversion       | geosetta | ogr2ogr | duckdb | vs ogr | vs duck |
|------------------|---------:|--------:|-------:|-------:|--------:|
| geojson→parquet  |   190 ms | 1382 ms | 1281 ms|  7.3×  |  6.7×   |
| geojson→fgb      |   254 ms | 1440 ms | 1441 ms|  5.7×  |  5.7×   |
| geojson→gpkg     |   336 ms | 1644 ms | 1523 ms|  4.9×  |  4.5×   |
| parquet→geojson  |   108 ms |  924 ms | 1248 ms|  8.6×  | 11.6×   |
| fgb→geojson      |    96 ms |  778 ms | 1428 ms|  8.1×  | 14.9×   |
| gpkg→geojson     |   117 ms |  762 ms | 1255 ms|  6.5×  | 10.7×   |

Hardware, tool versions, and dataset all move these figures; re-run locally for
your own baseline.
