# Visualization

The warehouse stores scored benchmark results as Parquet files partitioned by
benchmark, client, and day, with frozen legacy month partitions still readable
(see [storage.md](storage.md) for the schema and partition layout). Because the
format is standard Apache Parquet, it can be queried with any tool that reads
Parquet.

## 1. What you can explore

- **Leaderboards** — latest or best value per client for a given benchmark
- **Trends over time** — how throughput, latency, or accuracy change across
  days, months, and runtime versions
- **Client comparison** — side-by-side performance across hardware platforms
- **Benchmark coverage** — which clients have run which benchmarks
- **Resource profiling** — memory usage patterns across models and hardware

## 2. Included examples

A ready-made example lives under `examples/notebooks/`. It reads Parquet
files directly from disk — no server required. See the subdirectory's README
for setup.

| Example | Path | Best for |
|---------|------|----------|
| Jupyter notebook | `examples/notebooks/` | Ad-hoc exploration with full control over queries and plots. One-off investigations and custom charts. |

## 3. Other tools

The Parquet files can be loaded by any compatible tool, for example:

| Tool | Example |
|------|---------|
| pandas | `pd.read_parquet("sample_data/warehouse/")` |
| DuckDB | `SELECT * FROM read_parquet('sample_data/warehouse/**/*.parquet')` |
| Polars | `pl.scan_parquet("sample_data/warehouse/**/*.parquet")` |
| Grafana / Metabase / Superset | Connect via a Parquet-capable data source |
