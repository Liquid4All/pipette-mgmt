#!/usr/bin/env python3
"""Keep the HTTP API's bodies in `pipette_plan_types`, not `pipette-httpapi`.

A route reads and answers typed bodies, and plan-types is where they live, one
file per resource, shared with the clients that send and read them. A struct or
enum deriving `Serialize`, `Deserialize` or `JsonSchema` in `pipette-httpapi`'s
`src/` is either a body in the wrong crate or a shape built beside the one the
route already has; the `From` impls beside mgmt's records fill the bodies
instead. Clippy refuses the other half, a body built by hand (`json!`,
`serde_json::to_value`, `Map::new`, `Value::as_object_mut`; see the crate's
`clippy.toml`).

Test code (from a top-level `#[cfg(test)]` on) is exempt. `EXEMPT` names the
types that derive serde for something that is not an HTTP body; an entry that
no longer matches is reported, so the list cannot outlive its reason.
"""

import re
import subprocess
import sys
from pathlib import Path

SRC = "crates/pipette-httpapi/src/"

# (file, type): why it derives serde without being a body.
EXEMPT = {
    (SRC + "clerk.rs", "Header"): "a Clerk token's JWT header, decoded to verify it",
    (SRC + "clerk.rs", "Claims"): "a Clerk token's claims, decoded to verify them",
    (SRC + "clerk.rs", "Audience"): "a Clerk token's `aud` claim",
    (SRC + "ui.rs", "ClerkPage"): "the sign-in page's template context, rendered to HTML",
}

DERIVE = re.compile(r"^[ \t]*#\[derive\(([^)]*)\)\]", re.MULTILINE)
SERDE = re.compile(r"\b(Serialize|Deserialize|JsonSchema)\b")
ITEM = re.compile(
    r"^[ \t]*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+(\w+)", re.MULTILINE
)
CFG_TEST = re.compile(r"^#\[cfg\(test\)\]", re.MULTILINE)


def tracked() -> list[str]:
    result = subprocess.run(
        ["git", "ls-files", SRC + "*.rs"], capture_output=True, text=True, check=True
    )
    return [line for line in result.stdout.splitlines() if line]


def serde_types(path: str) -> list[tuple[str, int]]:
    src = Path(path).read_text(encoding="utf-8")
    test = CFG_TEST.search(src)
    if test is not None:
        src = src[: test.start()]
    found = []
    for derive in DERIVE.finditer(src):
        if not SERDE.search(derive.group(1)):
            continue
        item = ITEM.search(src, derive.end())
        if item is not None:
            found.append((item.group(1), src.count("\n", 0, item.start()) + 1))
    return found


def main() -> int:
    files = tracked()
    misplaced, seen = [], set()
    for path in files:
        for name, line in serde_types(path):
            if (path, name) in EXEMPT:
                seen.add((path, name))
            else:
                misplaced.append(f"{path}:{line}: {name}")
    stale = [f"{path}: {name}" for path, name in EXEMPT if (path, name) not in seen]
    if misplaced:
        print(
            "error: a pipette-httpapi type derives serde; an HTTP body belongs in pipette_plan_types.",
            file=sys.stderr,
        )
        print(
            "Move it to the resource's file in crates/pipette-plan-types/src/ and "
            "fill it from the record; if it is not an HTTP body, add it to EXEMPT in "
            "ci/checks/wire-bodies.py with the reason.",
            file=sys.stderr,
        )
        print(file=sys.stderr)
        print("\n".join(misplaced), file=sys.stderr)
    if stale:
        print(
            "error: EXEMPT in ci/checks/wire-bodies.py names types that no longer "
            "derive serde there; drop them:",
            file=sys.stderr,
        )
        print("\n".join(stale), file=sys.stderr)
    if misplaced or stale:
        return 1
    print(f"wire bodies OK ({len(files)} Rust files checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
