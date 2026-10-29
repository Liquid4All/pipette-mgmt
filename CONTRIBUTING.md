# Contributing

Thanks for your interest in `pipette-mgmt`. This document is the front door:
what to check before opening a pull request, how CI is laid out, and the commit
conventions. [docs/development.md](docs/development.md) is canonical for
building, running and testing.

## 1. Getting started

You need a stable Rust toolchain (edition 2024) with the `rustfmt` and
`clippy` components, Node for the console app in `ui/`, and
[uv](https://docs.astral.sh/uv/) only if you touch the Python examples. To seed
a storage root and start the server, follow
[docs/development.md §2](docs/development.md#2-benchmarks-and-sample-data).

This repository is the server only. Benchmarks are run by a separate client;
see the [pipette-clients repository](https://github.com/Liquid4All/pipette-clients).

### Where things live

The repository is a Cargo workspace with its crates under `crates/`. The
comment over the member list in the root [`Cargo.toml`](Cargo.toml) is the crate
map. [`ci/checks/crate-layering.py`](ci/checks/crate-layering.py) holds the tier
rules as a table, and CI enforces them in the manifests and in the resolved
dependency graph ([ci/README.md](ci/README.md)).

`crates/pipette-plan-types/` holds the plan vocabulary this repository shares
with
[pipette-clients](https://github.com/Liquid4All/pipette-clients):
the plan TOML a client runs, the `spec` the queue hands it, and the
`model_descriptor` / `runtime_descriptor` a result carries. This repository owns
the crate; pipette-clients pins it to a commit in its root
`Cargo.toml`. A change starts here and reaches the clients when they bump the
pin, and must stay readable by clients on an older pin, because each client
release keeps the revision it was built with. A plan can use a new `Runtime` or
`Model` kind only after three steps: a change here, a server deploy, and a pin
update in pipette-clients.

`crates/pipette-planner/` owns the capability rules and the expansion of a
scheduler plan into job bodies; pipette-clients dropped its copy
(PIP-633). `crates/pipette-planner/tests/parity.rs` compares the expansion with
output the client generated before then. A doc in pipette-clients that
quotes a capability rule may need the same edit.

`examples/plans/` holds two sets of example plans, and the tests read both:

- The local-dispatch examples, `examples/plans/*.toml`, are copies of the
  examples pipette-clients ships for `pipette-ctl`.
  `crates/pipette-plan-types/tests/example_plans.rs` parses and expands each,
  so a crate change that breaks one fails here before a client bumps its pin.
  Copy a new or changed example from pipette-clients without edits; a
  path in a copied example's comments refers to that tree.
- The scheduler examples, in [`examples/plans/scheduler/`](examples/plans/scheduler/),
  set no `expires_at`, so their jobs take ingestion's 30-day default and stay
  live for the tests that submit them. The `pipette-planner` tests check them
  against the golden bodies in `crates/pipette-planner/testdata/golden/`, so an
  example changes together with its golden bodies.

## 2. Before you open a PR

Run the same checks CI runs. The commands live in
[docs/development.md §1](docs/development.md#1-ci), which is the canonical list.
Run them from there rather than from memory, since the exact flags
(`--all`, `--all-targets`, `--locked`) are what make a local pass mean
anything.

Two of those bite in ways worth knowing before you write the code:
panicking constructs (`unwrap`, `expect`, `panic!`) are denied crate-wide,
tests included, and em dashes are rejected in prose and in any string a
user reads.

New behaviour should come with tests. The test suite lives in a crate's
`tests/` alongside unit tests in its `src/`, and the same
`cargo test --locked` covers both. Check your PR description with
`./ci/pr-body.py --pr <n>` before you ask for review
([§5](#5-commit-and-pr-conventions)).

## 3. How CI is structured

CI is [`.github/workflows/ci.yml`](.github/workflows/ci.yml). It runs on pull
requests, on pushes to `main`, and on pushes to `release/**` branches. One more
workflow sits beside it: [`security.yml`](.github/workflows/security.yml), the
supply-chain and secret gates. All jobs run on GitHub-hosted runners.

A `changes` job runs first and uses `dorny/paths-filter` to decide what a pull
request touched (the `rust`, `python`, `catalog` and `console_ui` filters in
`ci.yml`). Jobs gated on a filter are skipped on pull requests that miss it, so
a docs-only PR does not pay for a full compile, unless it touches `docs/api/`,
which the binary compiles into the OpenAPI document. Pushes ignore the filters
and run everything.

| Job | Runs | What it does |
|-----|------|--------------|
| `changes` | always | Paths filter feeding the gates below |
| `release-metadata` | always | Computes the version and image path with [`ci/version.sh`](ci/version.sh) |
| `conventions` | always | `./ci/lint.sh`: the repo-convention checks in `ci/checks/`. No path filter and no toolchain, so a docs-only PR is checked too |
| `rust-lint` | rust filter | `cargo fmt`, `cargo deny check licenses`, `cargo clippy`, and `cargo doc` |
| `rust-test` | rust filter | `cargo nextest run` plus `cargo test --doc` |
| `catalog` | catalog filter | Parses the shipped benchmark catalog, and validates the app's example plans against it |
| `console-ui` | console_ui filter | The Svelte app: generated types, type check, unit tests, build, and the credit of every bundled npm package |
| `attribution` | rust filter | Regenerates `THIRD-PARTY-LICENSES.md` with `cargo about` and fails if the committed file differs |
| `layering` | rust filter | `ci/checks/crate-layering.py --resolved`: the tier rules, read off each crate's resolved dependency graph |
| `python-check` | python filter | `uv sync --group dev`, then `ruff check` and `ruff format --check` over `examples` and `tools`, then the signing tools' tests |
| `build-artifact` | rust filter | Release binaries for `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`, packaged as tarballs with `.sha256` checksums |
| `api-docs` | after `build-artifact` | The API reference as a static page, `pipette-mgmt openapi --site` from the x86_64 binary, uploaded as the `pipette-mgmt-api-docs` artifact |
| `docker-image-build` | after `release-metadata`, `build-artifact` and `attribution` | Per-arch image build on a native runner, copying in the prebuilt binary rather than recompiling |
| `docker-image-publish` | push only | Stitches the per-arch digests into one multi-arch tag on GHCR |
| `build-complete` | always | The single aggregate gate branch protection requires, so a job added later is protected without an edit to the branch rules |
| `github-release` | push to `release/**` only | Publishes the tarballs and checksums as a GitHub release |

Two things are worth knowing when you read a run's checks:

- `docker-image-build` runs on pull requests, but only builds: its registry
  login, digest export, and push steps are conditioned on a push, so nothing
  is published from a PR.
- `docker-image-publish` is skipped on pull requests, and `github-release`
  runs only on a push to a release branch.

### Releasing

A push to `main` publishes the multi-arch image to GHCR, tagged with the commit
SHA. A push to a `release/<train>` branch, such as `release/2026.08.1`, also
creates a GitHub release with the tarballs and checksums attached. The release
and the image tag take the version `<train>-<n>-g<sha>`, where `<n>` is the
number of commits since the branch point. Run `ci/version.sh` to see what the
current commit publishes as. Two pushes to one release branch queue rather than
race on the tag.

### `security.yml`

`security.yml` is separate because nothing in it is about whether the code
compiles. It reads the dependency graph and the git history, so it needs no
toolchain cache and no path filter (a docs-only PR is scanned too), and, unlike
every job in `ci.yml`, it can go red with no change on the branch: a RUSTSEC
advisory is published, or a crate is yanked. So it runs on every pull request,
on every push to `main`, and on a daily cron that rechecks `main` between
merges.

| Job | Runs | What it does |
|-----|------|--------------|
| `supply chain (cargo-deny)` | PR, push, daily | `cargo deny check -D advisory-not-detected advisories bans sources` against [`deny.toml`](deny.toml). `check licenses` stays in `ci.yml`, next to the attribution it produces |
| `unused deps (cargo-machete)` | PR, push, daily | `cargo-machete`: a dependency kept past its last caller |
| `secrets (gitleaks)` | PR, push, daily | `gitleaks git --redact` over the commits a pull request adds, and over the whole history on `main` and on the cron. Allowlist in [`.gitleaks.toml`](.gitleaks.toml) |

The three install their tools from pinned release tarballs whose SHA-256
[`ci/install-tool.sh`](ci/install-tool.sh) verifies, rather than resolving
whatever is latest: a gate that changes version on its own can start failing,
or quietly stop finding things, on someone else's release day. Nothing bumps
those pins for you. They are a table in that script, which `ci.yml` also reads
for the `cargo-deny` behind the license check, and which the other Pipette
repositories carry verbatim with the same three jobs, so a change here is a
change to make in each of them.

The `security.yml` jobs are not inside `build-complete`, which can only
aggregate jobs in its own workflow. They gate nothing until branch protection
names them.

## 4. Where documentation lives

The [README](README.md#documentation) is the map of the documents under
`docs/`; start there. Each fact has one home. If your change alters behaviour a
document describes, update that document in the same PR, and link to a fact's
home rather than restating it. Code owns implementation: a config key is
documented on its field in `crates/pipette-core/src/config.rs`, a CLI flag in
its clap doc comment, and an `/api/v1` body field in its OpenAPI doc comment.

## 5. Commit and PR conventions

For new contributions, use
[Conventional Commits](https://www.conventionalcommits.org/) subjects:

```
type(scope): imperative summary
```

- **Common types:** `feat`, `fix`, `docs`, `refactor`, `ci`, `style`,
  `chore`, `perf`.
- **Scope** is optional and names the subsystem: `plans`, `ingestion`,
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

[`.github/PULL_REQUEST_TEMPLATE.md`](.github/PULL_REQUEST_TEMPLATE.md) opens a
description with that shape, and [`ci/pr-body.py`](ci/pr-body.py) checks it.
Three rules
about content: the sections are present, they are in that order, and each says
something (the template's prompts left in place count as saying nothing; a
fenced block of the commands you ran counts as an answer). Two about form:
exactly one heading per section, and bold. Extra sections are free, in any form
and any position, which is what `Breaking`, `Follow-up` and `Notes` already are.

**Headings are bold, not `##`.** Bold is the form this repository writes: 476
of the last 477 core-section headings use it. The check takes only bold, so a
description skimmed at speed always has its sections in the same shape.
`## Problem` fails with a message naming the fix.

Run the check before you ask for review:

```bash
./ci/pr-body.py --pr 1234   # or a file path, or - for stdin
```

Keep pull requests focused on one change. Maintainers squash-merge PRs into
the linear `main` history, so the PR description becomes the commit message.

## 6. License

This project is licensed under the [Apache License 2.0](LICENSE).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project is submitted under the Apache
License 2.0, as described in Section 5 of the license.
