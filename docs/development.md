# Development

## 1. CI

CI runs format check, clippy, and tests on every push and pull request
(see `.github/workflows/ci.yml`):

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Run these exact invocations locally. The flags are not cosmetic: without
`--all` rustfmt skips other crate targets, without `--all-targets` clippy
skips tests and benches, and without `--locked` the build is free to
resolve dependencies differently than CI does. A looser local command can
pass while CI fails.

The Python examples are linted and formatted with
[ruff](https://docs.astral.sh/ruff/). Both checks are scoped to
`examples/`, not the repository root:

```bash
uv sync --group dev                   # install the dev tooling
uv run ruff check examples
uv run ruff format --check examples
```

## 2. Benchmarks and sample data

`examples/benchmarks/` contains the benchmark catalog (TOML definitions).
Seed a storage root with it and start the server:

```bash
mkdir -p sample_data                                   # storage root (gitignored)
cp -r examples/benchmarks sample_data/benchmarks       # seed the catalog
cargo run --bin pipette-mgmt -- --config examples/config.toml serve
```

The warehouse starts empty and fills as submissions are scored. See
[visualization.md](visualization.md) for ways to explore it and
`examples/notebooks/` for a ready-made notebook.
