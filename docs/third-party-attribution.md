# Third-party attribution

`THIRD-PARTY-LICENSES.md` reproduces the license notices of the third-party
code in the release binary: every crate linked into it, and every npm package
bundled into the console app it serves. It is generated, and it ships with both
release artifacts.

## Why the file exists

The permissive licenses covering the dependency tree (MIT, Apache-2.0, the BSD
family, ISC, Unicode-3.0, Zlib, MPL-2.0) all grant their permissions on the
condition that their copyright and license notices be reproduced when the
covered code is redistributed. `pipette-mgmt` redistributes that code in
compiled form, publicly, through two channels:

- the `liquidai/pipette-mgmt` image on Docker Hub, and
- the binary tarballs attached to each GitHub release.

Both carry the notice. The Dockerfile copies it to
`/usr/share/doc/pipette-mgmt/THIRD-PARTY-LICENSES.md`, and the CI packaging
step places it beside the binary in the tarball. A release that omits it
redistributes the dependencies without the notices their licenses require.

## Regenerating

```bash
# `cli` gates the binary — without it, cargo installs nothing and only warns.
cargo install cargo-about --version 0.9.1 --locked --features cli
cargo about generate about.hbs -o THIRD-PARTY-LICENSES.md
```

The version is pinned to the one CI uses. Generation has to be reproducible to
the byte, so upgrading the generator is its own change: bump it here and in
`.github/workflows/ci.yml` together, then commit whatever the new version
produces.

`about.toml` holds the configuration and `about.hbs` the Markdown template.
Regenerate whenever the dependency graph changes. Any edit to `[dependencies]`
or to a dependency's enabled features, and any `Cargo.lock` update that adds,
removes, or bumps a crate. Feature changes count: a feature can pull an entire
subtree that needs crediting.

Commit the regenerated file in the same change as the dependency edit. CI
regenerates it and fails if the committed copy differs, so a stale notice
surfaces as a build failure rather than an incomplete release.

## Pinning a crate's license text

`cargo about` looks a crate's license text up on clearlydefined.io unless it is
told otherwise, and what comes back is not always the text the crate ships. The
generator groups crates by the **exact** text, so a crate whose two spellings
differ only in indentation lands under a different heading depending on whether
the lookup answered. That makes the notice depend on the network rather than on
the dependency graph, and makes CI's regenerate-and-compare fail against a file
generated on a machine whose lookup went the other way.

When that happens, pin the file in `about.toml`:

```toml
[<crate>.clarify]
license = "Apache-2.0"

[[<crate>.clarify.files]]
path = "LICENSE"
checksum = "<sha256 of that file>"
```

`shasum -a 256 ~/.cargo/registry/src/*/<crate>-<version>/LICENSE` prints the
checksum. A mismatch after a version bump is a warning, not an error, and the
generator quietly falls back to the lookup, so the check that catches a stale
pin is the same CI step: it will move the crate's heading again.

## What the generated file covers

Attribution tracks what is actually distributed, so `about.toml` scopes
generation to the two Linux targets CI builds (`x86_64-unknown-linux-gnu` and
`aarch64-unknown-linux-gnu`) and excludes build- and dev-dependencies, which are
not linked into the binary.

`cargo about` excludes the workspace's own crates the same way: every member
inherits `publish = false` from `[workspace.package]` through
`publish.workspace = true`, which marks it private, so the generated file
carries third-party notices alone. The generator reads that key per crate. A new
member that omits it therefore adds an entry for first-party code the project already
licenses through its own `LICENSE`, and the regenerate-and-compare step fails
on the difference. Dependencies reachable only on other platforms are outside
that scope: a macOS-only or wasm-only crate is absent from the shipped artifact
and therefore from the notice.

Where a crate offers a choice of license, `about.toml` elects one rather than
reproducing all of them. The `accepted` list is ordered by preference, with
`Apache-2.0` ahead of `MIT` so a crate published as `MIT OR Apache-2.0` is taken
under Apache-2.0 for its explicit patent grant. Reordering that list changes
which terms the project relies on, so treat it as a licensing decision.

A crate that carries a license outside `accepted` fails generation instead of
being silently omitted. That failure is the intended signal to review a new
dependency's terms before shipping it: extend `accepted` only after deciding the
license is one this project can distribute under.

## npm packages in the console app

The console app (`ui/`) is compiled into the binary, so the npm packages its
bundle carries are redistributed too. `cargo about` sees only crates, so they
are credited by hand in a static section of `about.hbs`, one
`- <name> <version> — <repository>` line and its license text each, then
regenerated into `THIRD-PARTY-LICENSES.md` as above.

`npm run build` lists the packages the bundle carries
(`node_modules/.cache/bundled-packages.txt`, written by `ui/vite.config.ts`).
CI's `console-ui` job fails while one of them is not credited in `about.hbs` at its
installed version, so a new or bumped package needs its credit in the same
change.

## MPL-2.0

`option-ext`, reached through `dirs`, is the one weak-copyleft dependency.
MPL-2.0 is file-level: it obliges nothing of `pipette-mgmt`'s own source, but
recipients of the binary must be able to obtain the covered source. The notice
lists the crate's upstream repository, which satisfies that. Modifying
`option-ext`'s own files (as opposed to merely linking it) would add an
obligation to publish those modifications, so vendor-and-patch is the case to
avoid.
