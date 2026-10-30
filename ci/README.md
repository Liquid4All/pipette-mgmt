# ci/

Repo-convention checks that run in CI and locally.

Run them all before pushing:

```sh
./ci/lint.sh
```

## What's here

- `lint.sh` + `checks/`: the convention checks, run by the `conventions` job.
  No toolchain required; see "Adding a check" below.
- `pr-body.py`: the Problem / Solution / Testing structure required of a pull
  request description.
- `install-tool.sh`: downloads a pinned release tarball, verifies its SHA-256
  and installs the binary. It also holds the pins. Used by the `security`
  workflow for `cargo-deny`, `cargo-machete` and `gitleaks`, and by `ci.yml`
  for the `cargo-deny` that checks licenses.

`lint.sh` runs every executable file in `ci/checks/` via its own shebang, so a
check can be any language (bash, python, ...). CI calls the same script (one step
in the `conventions` job), so a green local run matches the PR check.

The checks, and the rule each one defends:

| check | rule |
| --- | --- |
| `anyhow-imports` | only `use anyhow::Context;` is imported from anyhow; write `anyhow::Result` and friends qualified |
| `plan-types-no-anyhow` | `pipette-plan-types` links no `anyhow` outside `#[cfg(test)]` code; `anyhow` stays a dev-dependency |
| `plan-types-layering` | a `pipette-plan-types` module imports only from the modules `lib.rs` declares above it; test code is exempt |
| `crate-layering` | a crate names no dependency that breaks its tier, such as the S3 SDK below `pipette-store-init` or a testkit on a normal edge |
| `import-groups` | `use` blocks grouped std / third-party / workspace crate (`pipette_*`) / crate-local, blank-line separated |
| `rust-path-refs` | a Rust module or file path cited in the docs still resolves |
| `error-type-names` | a type deriving `thiserror::Error` ends in `Error`: `Invalid<X>` becomes `<X>Error`, a condition `<Condition>Error` |
| `wire-bodies` | no `pipette-httpapi` type derives serde: an HTTP body lives in `pipette_plan_types`, unless `EXEMPT` names the type as no body; clippy refuses the hand-built half (`clippy.toml`) |
| `secret-newtypes` | a secret-named field is a redacting newtype, and those newtypes keep their redaction |
| `doc-section-refs` | no docs section numbers (`storage.md` §5) in Rust comments: they move with every docs reorganization; name the file if the sentence needs it |
| `em-dash-rust` | no em dashes in user-visible Rust copy |
| `em-dash-docs` | no em dashes in Markdown prose |
| `md-links` | a relative Markdown link names a file that exists and, with a `#anchor` into a `.md` file, one of its headings |
| `em-dash-python` | no em dashes in Python strings a person reads |
| `test-targets` | every `crates/*/tests/*.rs`, and every module beside a `tests/<dir>/main.rs` target, is wired into a test binary, since `autotests = false` means an unlisted one never runs |

`crate-layering` is the one check with a second mode. Run with no arguments it
reads the manifests, which is what `lint.sh` does. Given `--resolved` it asks
cargo for each crate's whole graph, and so reports a forbidden crate reached
through a chain rather than a direct edge. That mode needs a toolchain, so the
`layering` job in [ci.yml](../.github/workflows/ci.yml) runs it and `lint.sh`
does not. See "Adding a check" below for why the checks here avoid cargo.

`pr-body.py` sits in `ci/` rather than under `checks/` for a different reason:
`lint.sh` runs each check with no arguments against `git ls-files`, and a PR
description is not a tracked file. There is no body to read in a clone, so it
takes one (`--pr`, `--env`, a file path, or stdin). You run it by hand before
you ask for review. The rules, and the reason headings are bold rather than
`##`, are in
[CONTRIBUTING.md](../CONTRIBUTING.md#5-commit-and-pr-conventions).

```sh
./ci/pr-body.py --pr 1234
```

`install-tool.sh` is not a check either, and is here for the same reason: it is
CI's, not a developer's. It exists so the download, checksum and install of a
pinned tool is written once rather than once per job, since a verification step
copy-pasted three times only has to be weakened in one copy to stop verifying.
It runs on Linux runners only (it wants `sha256sum`).

It takes a tool name and nothing else, because the version and digest of each
tool are a table inside it. `cargo-deny` is installed by two workflows here, and
the same three tools gate the other Pipette repositories; with the pins at the
call sites, two of those copies drift and nobody notices. The file is meant to
be identical across the repositories. [CONTRIBUTING.md](../CONTRIBUTING.md#securityyml) covers why the
tools are pinned at all.

```sh
ci/install-tool.sh cargo-deny
```

## Adding a check

Drop a self-contained, executable `ci/checks/<name>.{sh,py,...}` (with a shebang,
`chmod +x`) that:

- inspects tracked files (`git ls-files`),
- exits non-zero on violation with a message stating the rule and how to fix it,
- exits zero otherwise.

`lint.sh` picks it up; no workflow edit. Reserve these for conventions that
Rust's own tooling (rustfmt, clippy, rustdoc) can't express; otherwise prefer
that. `conventions` needs no Rust toolchain, and a check that invoked cargo here
would download and compile the whole dependency tree. Doc-link rot is the worked
example: it is the `cargo doc` step in `rust-lint`, not a check here, because
resolving an intra-doc link needs the compiler.

A check that wants cargo for part of its rule can still keep the cheap part
here. `crate-layering` does this: `lint.sh` runs it with no arguments, and that
run touches no toolchain. Put the cargo half behind a flag and give it a job
of its own.

Checks live here rather than as steps in the Rust job because `conventions` has
no path filter. It runs on every PR, which is what lets `rust-path-refs` catch a
Rust-only change breaking a reference from `docs/`, and what lets a docs-only PR
still have its prose checked.
