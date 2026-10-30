#!/usr/bin/env python3
"""Keep `anyhow` out of `pipette-plan-types`' release build.

`pipette-plan-types` is a pure data crate: the vocabulary every tier shares.
Its errors are typed (`thiserror`), so a signature returning `anyhow::Result`
would hide which failure a caller gets. Tests may still return
`anyhow::Result<()>` for `?`-chaining, with `anyhow` as a dev-dependency only.

Nothing in the compiler defends that arrangement: `anyhow` in `[dependencies]`
builds fine, and an `anyhow::` path in non-test code compiles under
`cargo test` (where dev-dependencies are linked) and only fails the release
build with an error that says nothing about the tier rule. So this check reads
the declarations instead and reports two violations:

1. `crates/pipette-plan-types/Cargo.toml` names `anyhow` anywhere but under
   `[dev-dependencies]`. Only a key written at the top level of a dependencies
   table counts, so the inner keys of a multi-line inline table are not read
   as dependencies of their own.
2. A `.rs` source under `crates/pipette-plan-types/src/` mentions `anyhow`
   outside `#[cfg(test)]`-gated code. The scan tracks brace depth with a small
   lexer (line/block comments, strings, chars, raw strings) so a `{` inside a
   format string cannot close a test module early.
"""

import re
import subprocess
import sys
from pathlib import Path

CRATE = "pipette-plan-types"
MANIFEST = Path("crates") / CRATE / "Cargo.toml"
SRC = Path("crates") / CRATE / "src"

CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
ANYHOW = re.compile(r"\banyhow\b")


def tracked(prefix: str) -> list[Path]:
    """Tracked files under `prefix`, relative to the repo root."""
    result = subprocess.run(
        ["git", "ls-files", "--", prefix],
        capture_output=True,
        text=True,
        check=True,
    )
    return [Path(line) for line in result.stdout.splitlines() if line]


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


def manifest_violations() -> list[str]:
    """`anyhow` keys under any dependencies table but `[dev-dependencies]`."""
    violations = []
    in_scope = False
    depth = 0
    for lineno, raw in enumerate(MANIFEST.read_text(encoding="utf-8").splitlines(), 1):
        line = comment_free(raw)
        header = re.match(r"\s*\[+([^]]+)\]+\s*$", line)
        if header is not None and depth == 0:
            section = header.group(1).strip()
            # `[target.'cfg(...)'.dependencies]` is a normal edge;
            # `[target.'cfg(...)'.dev-dependencies]` is not.
            in_scope = section == "dependencies"
            if re.fullmatch(r"dependencies\.[^\s=]+", section) is not None:
                in_scope = True
                name = section.split(".", 1)[1].strip().strip("\"'")
                if name == "anyhow":
                    violations.append(f"{MANIFEST}:{lineno}: `{CRATE}` names `anyhow`")
            continue
        if not in_scope:
            continue
        # The key may be dotted (`anyhow.workspace = true`), so either `=`
        # or `.` may follow the name.
        key = re.match(r"\s*([A-Za-z0-9_-]+)\s*(?:=|\.)", line)
        if key is not None and depth == 0 and key.group(1) == "anyhow":
            violations.append(f"{MANIFEST}:{lineno}: `{CRATE}` names `anyhow`")
        depth += line.count("{") + line.count("[")
        depth -= line.count("}") + line.count("]")
        depth = max(depth, 0)
    return violations


def code_points(source: str) -> list[tuple[int, str]]:
    """`source` with comments and literal bodies replaced by blanks.

    Returns `(line_number, code)` pairs where every byte still sits at its
    original column, so a brace or an `anyhow` found here reports the position
    it has in the file. Block comments and raw strings persist across lines;
    the other states reset at each newline.
    """
    out: list[tuple[int, str]] = []
    in_block = 0
    in_string = False
    in_char = False
    raw_hashes: int | None = None
    for lineno, raw in enumerate(source.splitlines(), 1):
        code = list(raw)
        index = 0
        escaped = False
        while index < len(raw):
            char = raw[index]
            if raw_hashes is not None:
                # Inside a raw string: only the closing quote plus the matching
                # hashes ends it; everything else (braces included) is payload.
                if char == '"' and raw[index + 1 : index + 1 + raw_hashes] == "#" * raw_hashes:
                    for blank in range(index, index + 1 + raw_hashes):
                        code[blank] = " "
                    index += 1 + raw_hashes
                    raw_hashes = None
                else:
                    code[index] = " "
                    index += 1
            elif in_block > 0:
                if char == "/" and raw[index + 1 : index + 2] == "*":
                    in_block += 1
                    code[index] = code[index + 1] = " "
                    index += 2
                elif char == "*" and raw[index + 1 : index + 2] == "/":
                    in_block -= 1
                    code[index] = code[index + 1] = " "
                    index += 2
                else:
                    code[index] = " "
                    index += 1
            elif in_string:
                code[index] = " "
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
                index += 1
            elif in_char:
                code[index] = " "
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == "'":
                    in_char = False
                index += 1
            elif char == "/" and raw[index + 1 : index + 2] == "/":
                for blank in range(index, len(raw)):
                    code[blank] = " "
                break
            elif char == "/" and raw[index + 1 : index + 2] == "*":
                in_block += 1
                code[index] = code[index + 1] = " "
                index += 2
            elif char == '"':
                in_string = True
                code[index] = " "
                index += 1
            elif char == "'":
                # A lifetime (`&'a`, `<'a>`) is not a char literal: only `'x'`
                # and `'\\..'` shapes open one.
                rest = raw[index:]
                if re.match(r"'(?:[^'\\]|\\.)'", rest) is not None:
                    in_char = True
                    code[index] = " "
                index += 1
            elif char == "r" and re.match(r'r#*"', raw[index:]) is not None:
                hashes = re.match(r'r(#*)"', raw[index:])
                assert hashes is not None
                raw_hashes = len(hashes.group(1))
                for blank in range(index, index + 1 + raw_hashes + 1):
                    code[blank] = " "
                index += 1 + raw_hashes + 1
            else:
                index += 1
        out.append((lineno, "".join(code)))
    return out


def source_violations(path: Path) -> list[str]:
    """`anyhow` mentions in `path` outside `#[cfg(test)]`-gated code."""
    violations = []
    depth = 0
    gated: list[int] = []
    pending = False
    for lineno, code in code_points(path.read_text(encoding="utf-8")):
        if CFG_TEST.search(code):
            pending = True
        if ANYHOW.search(code) and not gated and not pending:
            violations.append(f"{path}:{lineno}: `{CRATE}` uses `anyhow` outside `#[cfg(test)]`")
        for char in code:
            if char == "{":
                depth += 1
                if pending:
                    gated.append(depth)
                    pending = False
            elif char == "}":
                if gated and depth == gated[-1]:
                    gated.pop()
                depth = max(depth - 1, 0)
    return violations


def main() -> int:
    violations = manifest_violations()
    for path in tracked(str(SRC)):
        if path.suffix == ".rs":
            violations.extend(source_violations(path))
    if violations:
        print(f"error: `{CRATE}` must not use `anyhow` outside tests.", file=sys.stderr)
        print("Keep `anyhow` a dev-dependency and write release signatures with", file=sys.stderr)
        print("the crate's typed errors (`thiserror`); use `anyhow::Result` only in", file=sys.stderr)
        print("`#[cfg(test)]`-gated code.", file=sys.stderr)
        print(file=sys.stderr)
        for violation in violations:
            print(f"  {violation}", file=sys.stderr)
        return 1
    checked = len([path for path in tracked(str(SRC)) if path.suffix == ".rs"])
    print(f"plan-types-no-anyhow OK ({CRATE} links no anyhow; {checked} source files checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
