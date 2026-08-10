# Contributing

Thanks for your interest in `pipette-mgmt`. This document is the front door:
how to get a local build running, what to check before opening a pull
request, and where things live. It deliberately does not duplicate the
build and test reference — [docs/development.md](docs/development.md) is
canonical for that.

## 1. Getting started

You need a stable Rust toolchain (edition 2024) with the `rustfmt` and
`clippy` components. [uv](https://docs.astral.sh/uv/) is only needed if you
touch the Python examples.

```bash
cargo build --release

# Copy the example config and seed the benchmark catalog
cp examples/config.toml config.toml
mkdir -p sample_data
cp -r examples/benchmarks sample_data/benchmarks

# Start the HTTP server
./target/release/pipette-mgmt --config config.toml serve

# Process pending measurement submissions (fast pass; run via cron in production)
./target/release/pipette-mgmt --config config.toml process-submissions

# Score eval submissions (slow pass; run via cron when eval benchmarks are enabled)
./target/release/pipette-mgmt --config config.toml score-eval
```

`examples/config.toml` points both `[storage]` and `[auth_storage]` at
`./sample_data`, which is why the catalog is seeded there. The config path
can also come from the `PIPETTE_MGMT_CONFIG` environment variable instead of
`--config`.

`score` is a visible alias for the `process-submissions` fast pass; the slow
`score-eval` pass is a separate subcommand. See [docs/cli.md](docs/cli.md)
for the full subcommand and config reference, and
[docs/operations.md](docs/operations.md) for how the two scoring passes are
scheduled.

The warehouse starts empty and fills as submissions are scored — see
[docs/development.md](docs/development.md) §2 and
[docs/visualization.md](docs/visualization.md).

This repository is the server only. Benchmarks are run by a separate client;
see the [pipette-clients repository](https://github.com/Liquid4All/pipette-clients).

## 2. Before you open a PR

Run the same checks CI runs. The commands live in
[docs/development.md](docs/development.md) §1, which is the canonical list —
run them from there rather than from memory, since the exact flags
(`--all`, `--all-targets`, `--locked`) are what make a local pass mean
anything.

In short: rustfmt, clippy, and `cargo test` for Rust; Python uses
`uv run ruff check examples` and `uv run ruff format --check examples`.

New behaviour should come with tests. The test suite lives in `tests/`
alongside unit tests in `src/`, and the same `cargo test --locked` covers
both.

## 3. How CI is structured

CI is a single workflow, [`.github/workflows/ci.yml`](.github/workflows/ci.yml),
running on pushes to `main` and on pull requests targeting `main`.

A `changes` job runs first and uses `dorny/paths-filter` to decide what a
pull request actually touched:

- **rust** — `**/*.rs`, `**/Cargo.toml`, `Cargo.lock`, `docker/**`,
  `.github/**`
- **python** — `**/*.py`, `pyproject.toml`, `uv.lock`, `.github/**`

Jobs gated on those filters are skipped on pull requests that miss them, so
a docs-only PR does not pay for a full compile. Push events to `main` ignore
the filters and always run everything.

| Job | Runs | What it does |
|-----|------|--------------|
| `changes` | always | Paths filter feeding the gates below |
| `release-metadata` | always | Computes a `YYYY.MM.DD-<short-sha>` version |
| `rust-lint` | rust filter | `cargo fmt` and `cargo clippy` |
| `rust-test` | rust filter | `cargo test` |
| `python-check` | python filter | `uv sync --group dev`, then `uv run ruff check examples` and `uv run ruff format --check examples` |
| `build-artifact` | rust filter | Release binaries for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, packaged as tarballs with `.sha256` checksums |
| `docker-image-build` | after `build-artifact` | Per-arch image build on a native runner, copying in the prebuilt binary rather than recompiling |
| `docker-image-merge` | push to `main` only | Stitches the per-arch digests into one multi-arch tag |
| `github-release` | push to `main` only | Publishes the tarballs and checksums as a GitHub release |

Two things are worth knowing when you read a PR's checks:

- `docker-image-build` runs on pull requests, but only builds — its registry
  login, digest export, and push steps are conditioned on push-to-main, so
  nothing is published from a PR.
- `docker-image-merge` and `github-release` are skipped entirely on pull
  requests. Publishing only ever happens from `main`.

## 4. Where documentation lives

Most project documentation lives under `docs/`. The [README](README.md#documentation)
carries the index of the primary documents — start there rather than browsing
the directory.

The convention is one document per concern: `architecture.md` for system
design and the processing lifecycle, `httpapi.md` for the endpoint
reference, `storage.md` for the directory structure and file formats, and
`operations.md` for deployment and cron. If your change alters one of those
concerns, update the corresponding document in the same PR — an API change
belongs in `httpapi.md`, a new on-disk layout in `storage.md`, and so on.

`docs/` also holds documents the README index does not list, including
`client-integration.md`, `planner.md`, `plan-ingestion.md`,
`scoring-service.md`, `visualization.md`, and a `methodology/` subdirectory
with a spec per benchmark family. Follow the same rule: change the
behaviour, update its document.

## 5. Commit and PR conventions

For new contributions, use
[Conventional Commits](https://www.conventionalcommits.org/) subjects:

```
type(scope): imperative summary
```

- **Common types:** `feat`, `fix`, `docs`, `refactor`, `ci`, `style`,
  `chore`, `perf`.
- **Scope** is optional and names the subsystem — `plans`, `ingestion`,
  `stores`, `matching`, `planner`, `queue-maintenance`, `todo`, `mgmt`.
- **Summary** is lowercase, imperative mood, no trailing period.
- Do **not** append the PR number yourself. GitHub adds the `(#225)` suffix
  when the PR is squash-merged.

Example: `feat(plans): add plan admin CLI (ingest/list/status/cancel)`

Use this three-heading template for substantive commit bodies and PR
descriptions:

```
**Problem**
What was broken or missing, and why it mattered.

**Solution**
What changed, as a short bullet list. Call out non-obvious decisions.

**Testing**
How you verified it — test counts, manual runs, what you exercised.

Closes #123
```

Keep pull requests focused on one change. Maintainers squash-merge PRs into
the linear `main` history, so the PR description becomes the commit message.

## 6. License

This project is licensed under the [Apache License 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project is submitted under the Apache
License 2.0, as described in Section 5 of the license.
