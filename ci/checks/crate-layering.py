#!/usr/bin/env python3
"""Verify that each crate depends only on the tiers below it.

The workspace is a stack of tiers, described in the root `Cargo.toml` comment:

- `pipette-core` and `pipette-plan-types` are pure data crates.
- `pipette-planner` expands plans with no IO.
- `pipette-store` holds storage traits alone.
- The two backends never name each other.
- Only the backends and `pipette-store-init` link `object_store` and
  `aws-sdk-s3`.

Cargo enforces none of it. A single line in a manifest, or one new edge two
crates away, collapses a tier boundary and nothing goes red.

The table below names, for each crate, the dependencies that break its tier.
Two modes read it:

- The default mode reads each member's `[dependencies]` and `[dependencies.x]`
  tables and reports a forbidden name written there. It runs no cargo and needs
  no toolchain, so it belongs in `conventions` with the other checks here, where
  a manifest-only pull request still meets it. `--resolved` is the authority on
  any declaration shape this reader does not parse, and both modes gate the same
  pull requests.
- `--resolved` asks cargo for each crate's resolved graph and reports a
  forbidden crate the edges reach through any chain. A direct edge is the rare
  case; the regression that motivated this check added one crate that already
  depended on another that linked the S3 SDK. It needs a toolchain, so it runs
  in its own CI job, not under `ci/lint.sh`.

The last rule covers every testkit, and it defends attribution rather than
layering. `cargo about` reads every member's normal dependencies, so a testkit
reached by a normal edge puts its dependency tree into
`THIRD-PARTY-LICENSES.md` and fails the `attribution` job with a diff that says
nothing about the edge that caused it. Each testkit is a dev-dependency of every
consumer today.

The member list comes from the root manifest, so a new member is a subject of
the last rule with no edit here.
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

# The three crates that link the S3 SDK and the object store it sits behind. A
# tier that names one of these links a cloud client into a build that has no use
# for it, and puts its licenses into the attribution file.
S3_STACK = ["aws-sdk-s3", "aws-config", "object_store"]

# The store implementations. `pipette-store` holds the traits, and the tiers
# above it reach storage through those traits alone, so the backend a
# deployment runs stays a `[storage]` decision that `pipette-store-init` makes.
BACKENDS = [
    "pipette-store-fs",
    "pipette-store-s3",
    "pipette-store-sql",
    "pipette-store-init",
]

# The HTTP tier: it sits on the store traits and on `pipette-pipeline`, and it
# names no backend.
WEB_TIER = ["pipette-httpapi"]

# Shared test fixtures. Every consumer takes one as a dev-dependency.
TESTKITS = ["pipette-hub-testkit", "pipette-store-testkit", "pipette-web-testkit"]


def crate_names(manifest: str) -> list[str]:
    """The workspace members, by package name, in manifest order."""
    body = MEMBERS.search(manifest)
    if body is None:
        raise ValueError("the root manifest has no [workspace] members array")
    return [Path(member).name for member in re.findall(r'"([^"]+)"', body.group("body"))]


def tier_rules(members: list[str]) -> dict[str, list[str]]:
    """The tier rule for each crate that has one, keyed by crate name.

    `members` supplies the three bottom-tier rules, which name every crate above
    them. The remaining keys are the tiers themselves, so a key that names no
    member is a rule left behind by a rename. `stale_subjects` reports those.
    """
    # `pipette-core` may name `pipette-plan-types` and nothing else: a bound the
    # server checks a catalog entry against and the client re-checks the served
    # JSON against is one declaration or it drifts. The vocabulary names no
    # member in turn, so that edge cannot pull a tier down behind it.
    bottom = ["pipette-core", "pipette-plan-types"]
    machinery = S3_STACK + ["sqlx", "axum", "tokio", "reqwest"]

    table: dict[str, list[str]] = {
        "pipette-core": [name for name in members if name not in bottom] + machinery,
        "pipette-plan-types": [
            name for name in members if name != "pipette-plan-types"
        ]
        + machinery,
        # The planner expands a plan document into job bodies and does no IO.
        # It names the vocabulary it expands and no other member, so every tier
        # above can call it.
        "pipette-planner": [
            name
            for name in members
            if name not in ("pipette-planner", "pipette-plan-types")
        ]
        + machinery,
        # RFC 9421 signing and verifying, on the `http` types: the vocabulary
        # for its ids and no server framework, so a client can sign with it.
        "pipette-httpsig": [
            name
            for name in members
            if name not in ("pipette-httpsig", "pipette-plan-types")
        ]
        + machinery,
        # The traits and the types they take and return. Nothing here opens a
        # file or a socket, so an async runtime has nothing to drive.
        "pipette-store": BACKENDS + S3_STACK + ["sqlx", "axum", "tokio"],
        "pipette-store-fs": ["pipette-store-s3", "pipette-store-sql"]
        + S3_STACK
        + ["sqlx", "axum"],
        "pipette-store-s3": ["pipette-store-fs", "pipette-store-sql", "sqlx", "axum"],
        # It reaches the object-store halves it keeps (the catalog, vault blobs)
        # through the traits, so it links no object backend and no AWS SDK.
        "pipette-store-sql": [
            "pipette-store-fs",
            "pipette-store-s3",
            "pipette-store-init",
        ]
        + S3_STACK
        + ["axum"],
        # The one crate that depends on both backends and reads `[storage]` to
        # choose between them. It is below the HTTP tiers, not beside them.
        "pipette-store-init": ["axum"],
        # The processing tier. It reaches storage through `pipette-store`, so it
        # names no backend and links no HTTP server framework.
        "pipette-pipeline": BACKENDS + S3_STACK + ["axum"],
    }
    for crate in WEB_TIER:
        table[crate] = BACKENDS + S3_STACK
    return table


def stale_subjects(members: list[str]) -> list[str]:
    """Tier rules and testkit names that name no workspace member.

    A rename drops the rule it left behind, and the tier that rule defends goes
    unchecked while the run stays green. Report the stale name instead.

    `TESTKITS` is held to the same rule. Those names reach the table as
    forbidden values rather than as subjects, so a renamed testkit leaves every
    crate checked against a crate that no longer exists, and the attribution
    rule passes without reading anything.
    """
    return sorted((set(tier_rules(members)) | set(TESTKITS)) - set(members))


def layering_table(members: list[str]) -> dict[str, list[str]]:
    """Map each release crate to the dependencies that break its tier.

    The testkits are subjects of the last rule alone. They sit above the tiers
    they build fixtures for: `pipette-web-testkit` depends on `pipette-store-fs`
    on purpose, so holding it to the web tier's rule would report a violation
    for the edge that makes it useful.
    """
    rules = tier_rules(members)
    table: dict[str, list[str]] = {}
    for crate in members:
        if crate in TESTKITS:
            continue
        # Preserve order and drop the repeats `pipette-core`'s rule introduces.
        table[crate] = list(dict.fromkeys(rules.get(crate, []) + TESTKITS))
    return table


def comment_free(line: str) -> str:
    """`line` with any trailing comment removed, respecting quoted strings."""
    quote = None
    for index, char in enumerate(line):
        if quote is not None:
            if char == quote:
                quote = None
        elif char in "\"'":
            quote = char
        elif char == "#":
            return line[:index]
    return line


def normal_dependencies(manifest: str) -> set[str]:
    """The keys of a manifest's `[dependencies]` table.

    `[dev-dependencies]` is deliberately absent. A testkit, a mock transport and
    a backend a test drives directly are all dev edges, and every rule here is a
    rule about what a release build links.

    Values are read as opaque text: only a key written at the top level of the
    table counts, so the inner keys of a multi-line inline table are not read as
    dependencies of their own.
    """
    names: set[str] = set()
    in_table = False
    depth = 0
    for raw in manifest.splitlines():
        line = comment_free(raw)
        header = re.match(r"\s*\[+([^]]+)\]+\s*$", line)
        if header is not None and depth == 0:
            section = header.group(1).strip()
            in_table = section == "dependencies"
            if section.startswith("dependencies."):
                # A `[dependencies.name]` table is one dependency named by its
                # header rather than by a key.
                names.add(section.split(".", 1)[1].strip().strip('"'))
                in_table = False
            continue
        if not in_table:
            continue
        key = re.match(r"\s*([A-Za-z0-9_-]+)\s*=", line)
        if key is not None and depth == 0:
            names.add(key.group(1))
        depth += line.count("{") + line.count("[")
        depth -= line.count("}") + line.count("]")
        depth = max(depth, 0)
    return names


def manifest_violations(table: dict[str, list[str]], base: Path = Path()) -> list[str]:
    """Forbidden names written directly in a member's `[dependencies]`."""
    violations = []
    for crate, forbidden in table.items():
        path = base / "crates" / crate / "Cargo.toml"
        declared = normal_dependencies(path.read_text(encoding="utf-8"))
        for name in forbidden:
            if name in declared:
                violations.append(
                    f"crates/{crate}/Cargo.toml: `{crate}` declares `{name}` "
                    f"in [dependencies]"
                )
    return violations


def invert(crate: str, spec: str) -> subprocess.CompletedProcess[str]:
    """Ask cargo which edges carry `spec` into `crate`.

    `--target all` keeps the answer the same on every machine. The default
    resolves for the host, which hides an edge that a single platform selects
    and lets CI and a developer machine disagree. `-e normal` is what separates
    a dev edge from the edges a release build links.
    """
    return subprocess.run(
        [
            "cargo",
            "tree",
            "--locked",
            "-e",
            "normal",
            "--target",
            "all",
            "-p",
            crate,
            "-i",
            spec,
        ],
        capture_output=True,
        text=True,
    )


def reaches(crate: str, forbidden: str) -> str | None:
    """The dependency chain from `crate` to `forbidden`, or `None`.

    `cargo tree` answers in one of four ways, and stdout alone does not
    separate them:

    - A reachable crate prints the chain.
    - A crate that dev edges alone reach prints nothing and exits 0.
    - A crate absent from the graph prints nothing and exits 101, and names
      itself in a `did not match any packages` error.
    - A name the lock holds at two versions exits 101 with an `is ambiguous`
      error that lists the exact specs. Each spec is a separate question, and
      the tier is broken if any one of them answers with a chain.

    Any other failure means cargo answered no question at all, so this function
    raises. To report such a failure as "not reachable" turns a stale lock file
    or an unreadable manifest into a green run of a check whose whole purpose
    is to find an edge.
    """
    result = invert(crate, forbidden)
    chain = result.stdout.strip()
    if chain:
        return chain
    if result.returncode == 0:
        return None
    # Name the crate in the match. The same sentence covers a subject that `-p`
    # does not resolve, and that pair deserves the raise below.
    if f"`{forbidden}` did not match any packages" in result.stderr:
        return None
    if f"specification `{forbidden}` is ambiguous" in result.stderr:
        return ambiguous_chains(crate, forbidden, result.stderr)
    raise RuntimeError(
        f"cargo tree failed for {crate} -i {forbidden} "
        f"(exit {result.returncode}):\n{result.stderr.strip()}"
    )


def ambiguous_specs(forbidden: str, stderr: str) -> list[str]:
    """The `name@version` specs cargo lists under its `is ambiguous` error.

    cargo prints one spec per line, indented, below a `help:` line. Only a
    spec for `forbidden` counts, so an unrelated line in the same output
    cannot add a question.
    """
    return re.findall(rf"^\s+({re.escape(forbidden)}@\S+)$", stderr, re.MULTILINE)


def ambiguous_chains(crate: str, forbidden: str, stderr: str) -> str | None:
    """The chains from `crate` to every version of `forbidden` the lock holds.

    Asking again per spec keeps the version in the failure message, which is
    what tells a reader whether the bump or the edge is the thing to undo.
    """
    specs = ambiguous_specs(forbidden, stderr)
    if not specs:
        raise RuntimeError(
            f"cargo tree called `{forbidden}` ambiguous for {crate} and listed "
            f"no versions:\n{stderr.strip()}"
        )
    chains = []
    for spec in specs:
        result = invert(crate, spec)
        if result.returncode != 0 and not result.stdout.strip():
            raise RuntimeError(
                f"cargo tree failed for {crate} -i {spec} "
                f"(exit {result.returncode}):\n{result.stderr.strip()}"
            )
        chain = result.stdout.strip()
        if chain:
            chains.append(chain)
    return "\n".join(chains) or None


def resolved_violations(table: dict[str, list[str]]) -> list[str]:
    """Forbidden crates the resolved graph reaches, through a chain of any length."""
    violations = []
    for crate, forbidden in table.items():
        for name in forbidden:
            chain = reaches(crate, name)
            if chain is not None:
                indented = "\n".join(f"       {line}" for line in chain.splitlines())
                violations.append(f"`{crate}` reaches `{name}`:\n{indented}")
    return violations


MANIFEST_FIXTURE = """[workspace]
members = [
    "crates/pipette-core",
    "crates/pipette-plan-types",
    "crates/pipette-planner",
    "crates/pipette-store",
    "crates/pipette-store-fs",
    "crates/pipette-web-testkit",
]
resolver = "3"
"""

DEPENDENCIES_FIXTURE = """[package]
name = "pipette-core"

[dependencies]
# axum = { workspace = true }
anyhow = { workspace = true }
aws-sdk-s3 = { version = "1.93", default-features = false, features = [
    "sigv4a",
    "rt-tokio",
] }

[dependencies.tabled]
workspace = true

[dev-dependencies]
pipette-web-testkit = { workspace = true }
"""

# cargo's answer when one name in `-i` matches two versions in the lock. The
# `did not match any packages` line is cargo's answer when a name matches none.
AMBIGUOUS_STDERR = """error: specification `object_store` is ambiguous
help: re-run this command with one of the following specifications
  object_store@0.11.2
  object_store@0.12.1
"""

ABSENT_STDERR = "error: package ID specification `aws-sdk-s3` did not match any packages\n"


def self_test() -> None:
    """Regression fixtures for the table and the manifest reader.

    Raises AssertionError on failure.
    """
    declared = normal_dependencies(DEPENDENCIES_FIXTURE)
    # (name, dependency, expected)
    reader_cases = [
        ("plain key", "anyhow", True),
        ("multi-line inline table", "aws-sdk-s3", True),
        ("header-named dependency", "tabled", True),
        ("commented out", "axum", False),
        ("dev-dependency", "pipette-web-testkit", False),
        ("key inside an inline table", "default-features", False),
        ("key of another table", "name", False),
    ]
    for name, dependency, expected in reader_cases:
        got = dependency in declared
        if got is not expected:
            raise AssertionError(
                f"crate-layering self-test failed: {name} ({dependency}) read as "
                f"{got}, expected {expected}"
            )

    # (name, forbidden, stderr, expected)
    stderr_cases = [
        (
            "both versions of an ambiguous name",
            "object_store",
            AMBIGUOUS_STDERR,
            ["object_store@0.11.2", "object_store@0.12.1"],
        ),
        ("another crate's specs", "aws-config", AMBIGUOUS_STDERR, []),
        ("an absent crate lists no spec", "aws-sdk-s3", ABSENT_STDERR, []),
    ]
    for name, forbidden, stderr, expected in stderr_cases:
        got = ambiguous_specs(forbidden, stderr)
        if got != expected:
            raise AssertionError(
                f"crate-layering self-test failed: {name} ({forbidden}) read as "
                f"{got}, expected {expected}"
            )

    # The absent-crate branch names the crate, so the same sentence about a
    # subject `-p` cannot resolve stays a failure rather than a clean pair.
    if "`aws-sdk-s3` did not match any packages" not in ABSENT_STDERR:
        raise AssertionError(
            "crate-layering self-test failed: the absent-crate match no longer "
            "finds cargo's message"
        )
    if "`pipette-core` did not match any packages" in ABSENT_STDERR:
        raise AssertionError(
            "crate-layering self-test failed: the absent-crate match reads a "
            "message about another crate as this crate's absence"
        )

    members = crate_names(MANIFEST_FIXTURE)
    expected_members = [
        "pipette-core",
        "pipette-plan-types",
        "pipette-planner",
        "pipette-store",
        "pipette-store-fs",
        "pipette-web-testkit",
    ]
    if members != expected_members:
        raise AssertionError(
            f"crate-layering self-test failed: members are {members}, expected "
            f"{expected_members}"
        )
    table = layering_table(members)
    # (name, crate, dependency, expected)
    table_cases = [
        ("a testkit is forbidden everywhere", "pipette-core", "pipette-web-testkit", True),
        ("the bottom tier names no crate", "pipette-core", "pipette-store-fs", True),
        ("the bottom tier links no S3 SDK", "pipette-core", "aws-sdk-s3", True),
        ("a plain third-party crate is allowed", "pipette-core", "anyhow", False),
        ("the core names no planner", "pipette-core", "pipette-planner", True),
        ("the planner names the vocabulary", "pipette-planner", "pipette-plan-types", False),
        ("the planner names no core", "pipette-planner", "pipette-core", True),
        ("the planner links no HTTP stack", "pipette-planner", "axum", True),
        ("the traits tier names no backend", "pipette-store", "pipette-store-fs", True),
        ("a backend is allowed the traits", "pipette-store-fs", "pipette-store", False),
        ("a backend names no sibling", "pipette-store-fs", "pipette-store-s3", True),
        ("a testkit is not a subject", "pipette-web-testkit", None, None),
    ]
    for name, crate, dependency, expected in table_cases:
        if dependency is None:
            if crate in table:
                raise AssertionError(
                    f"crate-layering self-test failed: {name} ({crate}) is a "
                    f"subject of the table"
                )
            continue
        got = dependency in table.get(crate, [])
        if got is not expected:
            raise AssertionError(
                f"crate-layering self-test failed: {name} ({crate} -> "
                f"{dependency}) read as {got}, expected {expected}"
            )

    # A rule for a crate the fixture does not list is the shape a rename leaves
    # behind. `pipette-httpapi` stands in for any renamed subject, and
    # `pipette-store-testkit` for a renamed testkit: the fixture lists
    # `pipette-web-testkit` and not that one.
    stale = stale_subjects(members)
    # (name, crate, expected)
    stale_cases = [
        ("a tier the fixture omits", "pipette-httpapi", True),
        ("a member the fixture lists", "pipette-core", False),
        ("a testkit the fixture omits", "pipette-store-testkit", True),
        ("a testkit the fixture lists", "pipette-web-testkit", False),
    ]
    for name, crate, expected in stale_cases:
        got = crate in stale
        if got is not expected:
            raise AssertionError(
                f"crate-layering self-test failed: {name} ({crate}) read as "
                f"{got}, expected {expected}; stale subjects are {stale}"
            )

    with tempfile.TemporaryDirectory() as tmp:
        base = Path(tmp)
        crate = base / "crates" / "pipette-core"
        crate.mkdir(parents=True)
        (crate / "Cargo.toml").write_text(DEPENDENCIES_FIXTURE, encoding="utf-8")
        found = manifest_violations({"pipette-core": ["aws-sdk-s3"]}, base)
        if len(found) != 1 or "aws-sdk-s3" not in found[0]:
            raise AssertionError(
                f"crate-layering self-test failed: a forbidden dependency in "
                f"[dependencies] reported {found}, expected one violation"
            )
        clean = manifest_violations({"pipette-core": ["pipette-web-testkit"]}, base)
        if clean:
            raise AssertionError(
                f"crate-layering self-test failed: a dev-dependency reported "
                f"{clean}, expected no violation"
            )


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    resolved = argv == ["--resolved"]
    if argv and not resolved:
        print(f"usage: {sys.argv[0]} [--resolved]", file=sys.stderr)
        return 2

    # Fixtures always run first so CI catches a regression in the reader.
    try:
        self_test()
    except AssertionError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    members = crate_names(Path("Cargo.toml").read_text(encoding="utf-8"))
    stale = stale_subjects(members)
    if stale:
        print(
            f"error: a tier rule names a crate that is not a workspace member: "
            f"{', '.join(stale)}",
            file=sys.stderr,
        )
        print("Update the table in this file to the crate's current name.", file=sys.stderr)
        return 1

    table = layering_table(members)
    try:
        violations = (
            resolved_violations(table) if resolved else manifest_violations(table)
        )
    except (RuntimeError, OSError, ValueError) as e:
        print(f"error: {e}", file=sys.stderr)
        return 1

    if violations:
        print("error: a crate depends on a tier it must not reach.")
        print("The tiers, and what each one may link, are in the root Cargo.toml.")
        print()
        for violation in violations:
            print(f"  {violation}")
        return 1

    scope = "resolved graph" if resolved else "manifests"
    print(f"crate layering OK ({scope})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
