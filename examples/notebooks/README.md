# Jupyter Notebook

`warehouse_playground.ipynb` walks through the device topology fields — the
hardware and OS identity carried on every warehouse row. Reads Parquet files
directly from disk with pandas — no server required.

```bash
uv python install 3.12
uv sync
PIPETTE_WAREHOUSE=/path/to/warehouse uv run jupyter notebook examples/notebooks
```

`PIPETTE_WAREHOUSE` points at a warehouse root — the directory containing
`results/`. It can be omitted when the configured storage root is
`./sample_data`.
