#!/usr/bin/env python3
"""Verify that Rust paths named outside the Rust tree still resolve.

Two spellings rot the same way and both are checked: a module path
(`pipette_core::warehouse::MetricRow`) and a file path
(`crates/pipette-store/src/lib.rs`).

This check covers every workspace member. It reads crate names from the root
manifest's `members` list. New members receive the same coverage.

The docs cite their implementation by one or the other so a reader can find the
code a section describes. Nothing compiles those mentions, so a rename or a move
leaves them pointing at nothing, and neither rustdoc nor clippy reads a line of
Markdown. The `cargo doc` gate in `rust-lint` covers the same rot inside doc
comments; this covers it in the prose those comments point at.

This check runs in `conventions`, which has no path filter, so it sees every PR,
including a docs-only one. It resolves the module segments against the crate's
`src/`, then looks for the trailing item in the module it resolved to — as a
declaration or a re-export, so a path through `pub use` still counts. Item
matching is a file-wide text search, not a parse: it answers "is this name
declared here", which is what a stale reference gets wrong.
"""

import re
import subprocess
import sys
import tempfile
from pathlib import Path

# The root manifest's member list, as a literal array of quoted paths. The
# checks run on python 3.10, which has no `tomllib`, and the one key needed here
# is a flat array of strings.
MEMBERS = re.compile(r"^members\s*=\s*\[(?P<body>[^]]*)\]", re.MULTILINE)


def crate_roots(manifest: str, base: Path = Path()) -> dict[str, Path]:
    """Map each workspace member's Rust path spelling to its `src/` directory.

    Rust paths use underscores in crate names. Directory names use hyphens.
    The lookup normalizes both forms to the same key.
    """
    body = MEMBERS.search(manifest)
    if body is None:
        raise ValueError("the root manifest has no [workspace] members array")
    roots = {}
    for member in re.findall(r'"([^"]+)"', body.group("body")):
        directory = Path(member)
        roots[directory.name.replace("-", "_")] = base / directory / "src"
    return roots


def reference_re(roots: dict[str, Path]) -> re.Pattern[str]:
    """Return a pattern for references to each workspace crate.

    Both spellings of the crate name turn up in prose, so each hyphen position
    accepts either character. Item segments may be upper- or lowercase. The
    longest name comes first so a member whose name prefixes another still
    matches whole.
    """
    names = sorted(roots, key=len, reverse=True)
    alternatives = "|".join(
        "[_-]".join(re.escape(part) for part in name.split("_")) for name in names
    )
    return re.compile(rf"\b({alternatives})((?:::[A-Za-z_][A-Za-z0-9_]*)+)")


# A citation by file path — `crates/pipette-store/src/lib.rs` in a doc link
# or a code reference. Module paths and file paths rot the same way, and a moved
# file is the more common of the two here.
#
# The crate prefix is optional so that a bare `src/...`, which no longer names a
# file, fails here rather than being skipped as someone else's path.
#
# Anchored at a path boundary, so a longer path that merely ends this way is not
# read as one of ours: the peak-memory notes cite
# `pipette-clients/crates/pipette-memprobe/src/probes/metal.rs`, a file in
# another repository, and a bare `\b` matched its tail.
FILE_REFERENCE = re.compile(
    r"(?<![\w/-])(?:crates/[A-Za-z0-9_-]+/)?(?:src|tests|examples)/[A-Za-z0-9_/-]+\.rs\b"
)

# Trees outside the Rust sources, where a path mention is prose the compiler
# never sees. `*.md` covers the root files (README, CONTRIBUTING); `docs/*` also
# catches anything non-Markdown that lands there.
SEARCH_GLOBS = ["docs/*", "examples/*", "*.md"]

# Generated, and not ours to fix: a stale path inside it is a signal to
# regenerate, which the `attribution` job already enforces.
SKIP = {Path("THIRD-PARTY-LICENSES.md")}


def tracked_files() -> list[Path]:
    out = subprocess.run(
        ["git", "ls-files", *SEARCH_GLOBS],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    return [Path(f) for f in out if Path(f) not in SKIP]


def module_source(node: Path) -> Path | None:
    """The file backing module `node`.

    `lib.rs` only exists at the crate root, so offering it as a candidate costs
    nothing elsewhere and lets a crate-root re-export (`pipette_mgmt::AppState`)
    resolve.
    """
    for candidate in (node.with_suffix(".rs"), node / "mod.rs", node / "lib.rs"):
        if candidate.exists():
            return candidate
    return None


def declares(source: Path, item: str) -> bool:
    """True when `source` declares `item` or re-exports it.

    A re-export counts: a path routed through `pub use` is one a reader can
    follow. Searches the whole file, since `pub use` lists wrap across lines.
    """
    text = source.read_text(encoding="utf-8")
    declared = re.search(
        rf"\b(fn|struct|enum|trait|type|const|static|mod)\s+{re.escape(item)}\b", text
    )
    if declared:
        return True
    return any(
        re.search(rf"\b{re.escape(item)}\b", statement)
        for statement in re.findall(r"pub use [^;]+;", text)
    )


def workspace_reexport(
    source: Path, item: str, roots: dict[str, Path]
) -> tuple[Path, list[str]] | None:
    """Find a workspace target in a direct or flat grouped `pub use` statement."""
    text = source.read_text(encoding="utf-8")
    for crate, body in re.findall(r"\bpub\s+use\s+(\w+)::([^;]+);", text):
        if crate not in roots:
            continue
        body = body.strip()
        prefix, grouped, entries = body.partition("{")
        if grouped:
            if not entries.endswith("}") or "{" in entries:
                continue
            paths = entries[:-1].split(",")
        else:
            prefix, paths = "", [body]
        for path in paths:
            parts = re.split(r"\s+as\s+", path.strip(), maxsplit=1)
            target = [segment.strip() for segment in (prefix + parts[0]).split("::")]
            if not all(re.fullmatch(r"[A-Za-z_]\w*", segment) for segment in target):
                continue
            if target[-1] == "self":
                target.pop()
            name = parts[1] if len(parts) == 2 else (target[-1] if target else crate)
            if name == item:
                return roots[crate], target
    return None


def resolves(
    src: Path,
    segments: list[str],
    roots: dict[str, Path] | None = None,
    seen: frozenset[tuple[Path, str]] = frozenset(),
) -> bool:
    """True when the path names a real module chain and a real item in it."""
    node = src
    for index, segment in enumerate(segments):
        if module_source(node / segment) is not None:
            node = node / segment
            continue
        source = module_source(node)
        if source is None:
            return False
        target = workspace_reexport(source, segment, roots or {})
        if target is not None:
            edge = (source, segment)
            if edge in seen:
                return False
            target_root, target_segments = target
            return resolves(
                target_root,
                target_segments + segments[index + 1 :],
                roots,
                seen | {edge},
            )
        # Not a module, so this and everything after it name an item. Only the
        # first is checkable — a field or variant below it needs a real parse.
        return declares(source, segment)
    return True


MANIFEST_FIXTURE = """[workspace]
members = ["crates/pipette-mgmt", "crates/pipette-core"]
resolver = "3"
"""


def multi_crate_self_test() -> None:
    """Fixtures for the crate lookup. Raises AssertionError on failure.

    A two-crate workspace, where each crate declares one module the other does
    not. A reference has to reach the crate it names and no other.
    """
    with tempfile.TemporaryDirectory() as tmp:
        base = Path(tmp)
        for crate, module in (("pipette-mgmt", "handlers"), ("pipette-core", "plan")):
            src = base / "crates" / crate / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text(f"pub mod {module};\n")
            (src / f"{module}.rs").write_text("pub struct Marker {}\n")

        roots = crate_roots(MANIFEST_FIXTURE, base)
        (roots["pipette_mgmt"] / "lib.rs").write_text(
            "pub mod handlers;\n"
            "pub use pipette_core::{\n    plan, plan as plans,\n};\n"
            "pub use pipette_core::plan as direct;\n"
            "pub use pipette_core::plan::Marker;\n"
            "pub use pipette_core::cycle;\n"
        )
        (roots["pipette_core"] / "lib.rs").write_text(
            "pub mod plan;\npub use pipette_mgmt::cycle;\n"
        )
        (roots["pipette_core"] / "plan.rs").write_text(
            "pub struct Marker {}\npub enum State { Ready }\n"
        )
        expected_roots = {
            "pipette_mgmt": base / "crates/pipette-mgmt/src",
            "pipette_core": base / "crates/pipette-core/src",
        }
        if roots != expected_roots:
            raise AssertionError(
                f"rust-path-refs self-test failed: crate roots are {roots}, "
                f"expected {expected_roots}"
            )

        reference = reference_re(roots)
        # (name, prose, expected match, expected resolution)
        cases = [
            ("hyphen spelling", "pipette-core::plan::Marker", "pipette_core", True),
            ("underscore spelling", "pipette_core::plan::Marker", "pipette_core", True),
            ("second crate", "pipette_mgmt::handlers::Marker", "pipette_mgmt", True),
            ("module of the other crate", "pipette_core::handlers", "pipette_core", False),
            ("unknown item", "pipette_core::plan::Missing", "pipette_core", False),
            ("facade module", "pipette_mgmt::plan", "pipette_mgmt", True),
            ("facade item", "pipette_mgmt::plan::Marker", "pipette_mgmt", True),
            ("missing facade item", "pipette_mgmt::plan::Missing", "pipette_mgmt", False),
            ("grouped alias", "pipette_mgmt::plans::Marker", "pipette_mgmt", True),
            ("missing grouped alias item", "pipette_mgmt::plans::Missing", "pipette_mgmt", False),
            ("direct alias", "pipette_mgmt::direct::Marker", "pipette_mgmt", True),
            ("missing direct alias item", "pipette_mgmt::direct::Missing", "pipette_mgmt", False),
            ("re-exported item", "pipette_mgmt::Marker", "pipette_mgmt", True),
            ("enum variant", "pipette_mgmt::plan::State::Ready", "pipette_mgmt", True),
            ("re-export cycle", "pipette_mgmt::cycle::Missing", "pipette_mgmt", False),
        ]
        for name, prose, expected_crate, expected in cases:
            match = reference.search(prose)
            if match is None:
                raise AssertionError(
                    f"rust-path-refs self-test failed: {name} ({prose}) matched nothing"
                )
            crate = match.group(1).replace("-", "_")
            if crate != expected_crate:
                raise AssertionError(
                    f"rust-path-refs self-test failed: {name} ({prose}) named "
                    f"crate {crate}, expected {expected_crate}"
                )
            segments = match.group(2).lstrip(":").split("::")
            got = resolves(roots[crate], segments, roots)
            if got is not expected:
                raise AssertionError(
                    f"rust-path-refs self-test failed: {name} ({prose}) "
                    f"resolved to {got}, expected {expected}"
                )

        # A crate outside the workspace keeps its own prose: the peak-memory
        # notes name `pipette_memprobe::probes`, which lives in another
        # repository and has no tree here to resolve against.
        if reference.search("pipette_memprobe::probes::metal") is not None:
            raise AssertionError(
                "rust-path-refs self-test failed: a crate outside the workspace "
                "matched the reference pattern"
            )


def self_test() -> None:
    """Regression fixtures for the resolver. Raises AssertionError on failure."""
    multi_crate_self_test()
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "src"
        (src / "stores").mkdir(parents=True)
        (src / "lib.rs").write_text(
            "pub mod stores;\npub mod storage;\npub use router::AppState;\n"
        )
        (src / "storage.rs").write_text("pub struct Warehouse {}\n")
        (src / "stores" / "mod.rs").write_text("pub use local_fs::LocalFsStore;\n")

        # (name, path segments, expected)
        cases = [
            ("module only", ["storage"], True),
            ("item in module", ["storage", "Warehouse"], True),
            ("nested module", ["stores"], True),
            # A crate-root re-export resolves through lib.rs, not a module file.
            ("crate-root re-export", ["AppState"], True),
            ("item via pub use", ["stores", "LocalFsStore"], True),
            ("unknown module", ["nope"], False),
            ("unknown item", ["storage", "no_such_item"], False),
            ("unknown item under a module", ["stores", "no_such_item"], False),
        ]
        for name, segments, expected in cases:
            got = resolves(src, segments)
            if got is not expected:
                path = "::".join(segments)
                raise AssertionError(
                    f"rust-path-refs self-test failed: {name} ({path}) "
                    f"resolved to {got}, expected {expected}"
                )


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    only_self_test = argv == ["--self-test"]
    if argv and not only_self_test:
        print(f"usage: {sys.argv[0]} [--self-test]", file=sys.stderr)
        return 2

    # Fixtures always run first so CI catches resolver regressions.
    try:
        self_test()
    except AssertionError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    if only_self_test:
        print("rust-path-refs self-test OK")
        return 0

    roots = crate_roots(Path("Cargo.toml").read_text(encoding="utf-8"))
    reference = reference_re(roots)

    violations: list[str] = []
    for path in tracked_files():
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        for line_no, line in enumerate(text.splitlines(), start=1):
            for match in reference.finditer(line):
                root = roots[match.group(1).replace("-", "_")]
                segments = match.group(2).lstrip(":").split("::")
                if not resolves(root, segments, roots):
                    violations.append(f"{path}:{line_no}: {match.group(0)}")
            for match in FILE_REFERENCE.finditer(line):
                if not Path(match.group(0)).is_file():
                    violations.append(f"{path}:{line_no}: {match.group(0)}")

    if violations:
        print("error: a Rust path named outside the Rust tree no longer resolves.")
        print("Update the reference to the module's current path, or drop it.")
        print()
        for v in violations:
            print(f"  {v}")
        return 1

    print("cross-language Rust path references OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
