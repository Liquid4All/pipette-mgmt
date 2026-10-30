# Development

## 1. CI

CI runs the convention checks, the Rust gates, and the tests on every push and
pull request. How the workflows are laid out, and why `security.yml` is
separate, is [CONTRIBUTING.md §3](../CONTRIBUTING.md#3-how-ci-is-structured).
These are the gates, as CI runs them:

```bash
./ci/lint.sh                          # repo conventions; no toolchain needed
cargo fmt --all -- --check
cargo deny check licenses advisories bans sources  # needs cargo-deny 0.19+
cargo machete --with-metadata         # needs cargo-machete
cargo clippy --workspace --all-targets --locked
cargo clippy -p pipette-plan-types --all-targets --locked
cargo doc --workspace --no-deps --locked
cargo nextest run --workspace --locked  # or: cargo test --workspace --locked
cargo nextest run -p pipette-plan-types --locked
cargo test --workspace --doc --locked
(cd ui && npm ci && npm run check && npm test && npm run build)
cargo run -p pipette-mgmt --locked -- openapi --site target/api-docs  # the API reference page
```

Run these exact invocations locally. The flags are not cosmetic: without
`--all` rustfmt skips other crate targets, without `--all-targets` clippy
skips tests, and without `--locked` the build is free to resolve
dependencies differently than CI does. `--workspace` names the member set
explicitly. The root manifest is virtual and declares no `default-members`, so
a gate run from a crate directory would cover that crate alone. A looser local
command can pass while CI fails.

Two test sets need more than the command:

- The tests that run on PostgreSQL, `pipette-store-sql`'s and those built on
  `seeded_sql_state` in `pipette-web-testkit`, need a server at `TEST_DATABASE_URL`
  (e.g. `postgres://postgres@127.0.0.1:5432/postgres`). Unset, they skip
  locally and fail in CI. Each test makes and drops a schema of its own in a
  `pipette_test` database, which the first test creates and later runs reuse.
- The `pipette-store-s3` counter tests read process-wide counters, so run them
  under nextest or with `cargo test -p pipette-store-s3 -- --test-threads=1`;
  plain `cargo test` fails them.

What each gate is for:

- `./ci/lint.sh` runs every check in `ci/checks/`: the repo conventions
  that rustfmt, clippy and rustdoc cannot express (import grouping, the
  anyhow import rule, the `Error` suffix on error types, redacting secret
  newtypes, Rust paths cited in the docs, and the ban on em dashes in prose
  and user-visible copy). It needs
  no Rust toolchain and runs on every PR, including a docs-only one. See
  [ci/README.md](../ci/README.md), which also covers adding a check.
- The `ui/` commands type-check, unit-test and build the
  Svelte app ([§3](#3-the-console-app)). The release binary embeds that build.
- The two `-p pipette-plan-types` runs build `pipette-plan-types` without its
  `schemars` feature, which derives a JSON Schema for `ClientRunSpec` and the
  types it reaches. `pipette-pipeline` turns it on for the app's generated
  TypeScript, so every `--workspace` run builds it on, and these two cover the
  default a client builds.
- **No `-D warnings` anywhere.** Lint levels live in `[workspace.lints]`
  in the root `Cargo.toml`, which each crate opts into with `[lints]
  workspace = true`, so a local `cargo clippy` enforces exactly what CI
  does. Those tables also ban the panicking constructs: `unwrap`,
  `expect`, `panic!`, `todo!` and `unreachable!` are denied in every crate
  here, tests included. A reachable failure returns a typed error, and a
  test is written `-> anyhow::Result<()>` and propagates with `?`. Where a
  value really is valid by construction, `types::id_from_valid` is the one
  sanctioned place that says so.
- The HTTP tier (`pipette-httpapi`) also bans `serde_json::json!`, through
  its `clippy.toml`: a response is a `#[derive(Serialize)]` type, so
  the wire shape is written down once. Tests are exempt, since a test
  builds the request a client would send, malformed ones included.
- `cargo deny` is the dependency policy, all of it in
  [`deny.toml`](../deny.toml). An unlisted license fails the build; so does
  a RUSTSEC advisory, a crate banned by name, a `version = "*"`
  requirement, and a dependency resolved from anywhere but crates.io. The
  file records why each ignored advisory is out of reach.
- `cargo doc` is a gate, not a build step: `[workspace.lints.rustdoc]`
  denies broken intra-doc links, which nothing else in the build reads.
- Dead code is gated in two halves, because no single tool sees both.
  Within a crate, `warnings = "deny"` makes rustc's `dead_code` a build
  error, and `unreachable_pub` keeps an item from escaping that lint by being
  `pub` where `pub(crate)` would do. `dead_code` stops at a crate's `pub`
  surface, since rustc counts a `pub` item as used downstream; nothing here is
  published, so prefer private modules and narrow visibility. Whole
  dependencies are `cargo machete`'s half: it fails on a crate kept past its
  last caller, which otherwise costs build time, a `THIRD-PARTY-LICENSES.md`
  entry and an advisory surface. It reads sources textually, so a dependency
  reached only through a macro expansion is its blind spot: list such a crate
  under `ignored` in `[package.metadata.cargo-machete]` in its own manifest,
  with a comment naming what reaches it, as
  [`crates/pipette-plan-types/Cargo.toml`](../crates/pipette-plan-types/Cargo.toml)
  does for `regex`.

`gitleaks` reads commits rather than the working tree, so a credential committed
once and deleted in the next commit is still found. Over the whole history it
takes minutes, so it is not worth running before every push. The push to `main`
and the daily cron scan everything; a pull request scans `base..HEAD`:

```bash
gitleaks git --redact --verbose --config .gitleaks.toml .
gitleaks git --redact --verbose --config .gitleaks.toml --log-opts="origin/main..HEAD" .
```

`--redact` keeps the matched text out of the output, so a CI log is never a
second copy of the secret. A finding that is not a credential belongs in
[`.gitleaks.toml`](../.gitleaks.toml). History cannot be rewritten once it is
pushed, so every entry has to say why the string is not a secret, as `deny.toml`
says why it ignores an advisory.

The Python examples are linted and formatted with
[ruff](https://docs.astral.sh/ruff/), scoped to `examples/`:

```bash
uv sync --group dev                   # install the dev tooling
uv run ruff check examples
uv run ruff format --check examples
```

Check the pull request description before you ask for review
([CONTRIBUTING.md §5](../CONTRIBUTING.md#5-commit-and-pr-conventions)):

```bash
./ci/pr-body.py --pr 1234             # or a file path, or - for stdin
```

## 2. Benchmarks and sample data

`examples/benchmarks/` contains the benchmark catalog (TOML definitions).
The ASR entries are not in it: they live in the deployment's catalog, and
[benchmarks.md](benchmarks.md#the-deployments-ladder) shows their shape for a
local `sample_data/benchmarks/`. Seed a storage root with it and start the
server:

```bash
mkdir -p sample_data                                   # storage root (gitignored)
cp -r examples/benchmarks sample_data/benchmarks       # seed the catalog
cargo run --bin pipette-mgmt -- --config examples/config.toml serve
```

The asr_eval entries are organization data and are not in this repository
([benchmarks.md §2.12](benchmarks.md#212-asr-eval-error-rate)): a deployment
puts each into its organization's private catalog with
`pipette-mgmt orgs benchmarks put <org> <file>`, never into the public one.

The warehouse starts empty and fills as submissions are scored.
`examples/notebooks/` holds a ready-made notebook for exploring it.

## 3. The console app

`/app/` serves the Svelte app in `ui/`; [ui/README.md](../ui/README.md) covers
how it is served, how it signs in, and its pages. A checkout that has not run
`npm run build` still compiles, and `/app/` then serves a page saying the app
was not built, so the Rust gates never need Node. `PIPETTE_REQUIRE_UI=1` turns
that into a build error; the release build and `docker/build.sh` set it.

To work on it, run a local `pipette-mgmt serve` and the Vite dev server, which
proxies the API routes to it (`PIPETTE_MGMT_URL`, default
`http://127.0.0.1:3000`):

```bash
cd ui
npm ci
npm run dev        # http://localhost:5173/app/
```

WebCrypto signs only in a secure context, which `localhost` is.

`npm run check` type-checks with TypeScript 7 (`svelte-check --tsgo`).
`svelte-check` still loads TypeScript 6 itself, so both are installed, 7 under
the alias `@typescript/native`.

The app's request and response types are generated, not written.
`pipette_httpapi::api_schema` derives JSON Schemas from the Rust bodies in
`pipette-plan-types` into `ui/src/lib/api-schema.json` (a request from its
deserialize side, so a field with a default is optional), `npm run types` turns
that into `ui/src/lib/api-types.ts`, and both files are committed. After
changing a type the app reads:

```bash
UPDATE_API_SCHEMA=1 cargo test -p pipette-httpapi --lib api_schema
(cd ui && npm run types)
```

A Rust test fails while the JSON is stale and the `console-ui` job while the
TypeScript is. The HTTP tests also check the bodies the app reads against the
schemas, which catches a hand-written `Serialize` that drifts from its declared
shape.

The bundle refuses a name two types share, which schemars would otherwise split
as `Foo` and `Foo2`, and a request and a response of one name with different
schemas. Two types are told apart with `#[schemars(rename = "…")]` on one. One
type the app both sends and reads, whose deserialize side differs (a field with
`#[serde(default)]`), needs a request-side mirror type in `api_schema` under its
own name, since a rename would move both sides.

The npm packages the bundle carries are credited in `THIRD-PARTY-LICENSES.md`,
and CI checks it ([third-party-attribution.md](third-party-attribution.md#npm-packages-in-the-console-app)).

The operator API is also an OpenAPI 3.1 document
([httpapi.md §1.5](httpapi.md#15-the-openapi-document)), built from the same
body types. Its route table,
`crates/pipette-httpapi/src/api_schema/openapi/table.rs`, is held to `router.rs`
by a unit test, and `test_app` checks every `/api/v1` answer the HTTP tests
receive against the document, so an undeclared status (a `5xx` aside) or a body
its schema refuses fails the test that met it.
