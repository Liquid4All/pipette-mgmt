#!/usr/bin/env python3
"""Verify that each `pipette-plan-types` module imports only from modules above it.

`lib.rs` declares the crate's modules in layer order, from the values every
other module is written in down to what becomes of a cell. The compiler accepts
an import in either direction, so a module can reach down a layer and nothing
goes red until the two ends form a cycle. This check reads the order from
`lib.rs` and reports each import of a module declared at or after the importer.

A module reaches another in two spellings, and both are read:

- a module path, `crate::submission::RuntimeOutput`;
- a root re-export, `use crate::{Sha256}`. The explicit `pub use` lists in
  `lib.rs` map each re-exported name back to the module that defines it.

Either may climb to the root through `super::` rather than name `crate::`:
`super::` in `cell/mod.rs`, `super::super::` in `cell/reserved_flags.rs`.

Comments, string literals and `#[cfg(test)]` items are skipped: a doc link is
not a dependency, and a test fixture may build a whole document from the
layers below its own.
"""

import re
import subprocess
import sys
import tempfile
from pathlib import Path

SRC = Path("crates") / "pipette-plan-types" / "src"

MOD = re.compile(r"^(?:pub )?mod (\w+);", re.MULTILINE)
REEXPORT = re.compile(r"^pub use (\w+)::(\{[^}]*\}|\w+);", re.MULTILINE)
CFG_TEST = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
CFG_TEST_FILE = re.compile(r"#!\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
# `r"..."`, `r#"..."#`, `br"..."`; `r#ident` has no quote and stays code.
RAW_STRING = re.compile(r'b?r(#*)"')
# A char or byte literal. A lifetime (`'a`) has no closing quote after one char.
CHAR = re.compile(r"b?'(?:[^'\\\n]|\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]+\}|.))'")


def layers(lib: str) -> tuple[list[str], dict[str, str]]:
    """The module order `lib.rs` declares, and which module each re-export names."""
    order = MOD.findall(lib)
    owner = {}
    for module, names in REEXPORT.findall(lib):
        for name in re.findall(r"\w+", names):
            owner[name] = module
    return order, owner


def blank_comments_and_literals(source: str) -> str:
    """`source` with comments and literal bodies blanked; newlines are kept."""
    out = list(source)
    size = len(source)

    def blank(start: int, end: int) -> None:
        for index in range(start, min(end, size)):
            if out[index] != "\n":
                out[index] = " "

    index = 0
    while index < size:
        # A literal's `b`/`r` prefix cannot follow an identifier character.
        prefix_ok = index == 0 or not (
            source[index - 1].isalnum() or source[index - 1] == "_"
        )
        raw = (
            RAW_STRING.match(source, index)
            if prefix_ok and source[index] in "br"
            else None
        )
        char = (
            CHAR.match(source, index) if prefix_ok and source[index] in "b'" else None
        )
        if source.startswith("//", index):
            end = source.find("\n", index)
            end = size if end < 0 else end
        elif source.startswith("/*", index):
            depth, end = 1, index + 2
            while end < size and depth:
                if source.startswith("/*", end):
                    depth, end = depth + 1, end + 2
                elif source.startswith("*/", end):
                    depth, end = depth - 1, end + 2
                else:
                    end += 1
        elif raw is not None:
            close = source.find('"' + raw.group(1), raw.end())
            end = size if close < 0 else close + 1 + len(raw.group(1))
        elif source[index] == '"':
            end = index + 1
            while end < size and source[end] != '"':
                end += 2 if source[end] == "\\" else 1
            end += 1
        elif char is not None:
            end = char.end()
        else:
            index += 1
            continue
        blank(index, end)
        index = end
    return "".join(out)


def production_code(source: str) -> str:
    """`source` without comments, literals or `#[cfg(test)]` items."""
    if CFG_TEST_FILE.search(source):
        return ""
    code = blank_comments_and_literals(source)
    out = list(code)
    for gate in CFG_TEST.finditer(code):
        # The gated item ends at its first `;` or at the `}` closing its first
        # `{`, whichever comes first outside parentheses and brackets.
        nesting, index, end = 0, gate.end(), len(code)
        while index < len(code):
            ch = code[index]
            if ch in "([":
                nesting += 1
            elif ch in ")]":
                nesting -= 1
            elif ch == ";" and nesting == 0:
                end = index + 1
                break
            elif ch == "{" and nesting == 0:
                depth = 0
                while index < len(code):
                    depth += {"{": 1, "}": -1}.get(code[index], 0)
                    index += 1
                    if depth == 0:
                        break
                end = index
                break
            index += 1
        for blanked in range(gate.start(), end):
            if out[blanked] != "\n":
                out[blanked] = " "
    return "".join(out)


def root_path(file: Path) -> re.Pattern[str]:
    """`crate::`, or the `super::` chain that climbs from `file` to the crate root."""
    # `cell/mod.rs` is the module `cell`, one level down like `text.rs`.
    depth = len(file.parts) - (file.name == "mod.rs")
    supers = "::".join(["super"] * depth)
    return re.compile(rf"\b(crate|{supers})::(\{{|\w+)")


def references(
    code: str, root: re.Pattern[str], order: list[str], owner: dict[str, str]
) -> list[tuple[int, str, str]]:
    """`(line, module, spelling)` for each module `code` reaches through `root`."""
    found = []
    for path in root.finditer(code):
        line = code.count("\n", 0, path.start()) + 1
        if path.group(2) != "{":
            names = [path.group(2)]
        else:
            depth, end = 1, path.end()
            while end < len(code) and depth:
                depth += {"{": 1, "}": -1}.get(code[end], 0)
                end += 1
            group, depth, entries, start = code[path.end() : end - 1], 0, [], 0
            for index, ch in enumerate(group):
                depth += {"{": 1, "}": -1}.get(ch, 0)
                if ch == "," and depth == 0:
                    entries.append(group[start:index])
                    start = index + 1
            entries.append(group[start:])
            names = [m.group(1) for e in entries if (m := re.match(r"\s*(\w+)", e))]
        for name in names:
            module = name if name in order else owner.get(name)
            if module is not None:
                found.append((line, module, f"{path.group(1)}::{name}"))
    return found


def violations(src: Path, files: list[Path]) -> list[str]:
    """Each import of a module declared at or after the importing module."""
    order, owner = layers((src / "lib.rs").read_text(encoding="utf-8"))
    found = []
    for path in files:
        relative = path.relative_to(src)
        module = relative.parts[0].removesuffix(".rs")
        if module not in order:
            continue
        code = production_code(path.read_text(encoding="utf-8"))
        root = root_path(relative)
        for line, target, spelling in references(code, root, order, owner):
            if target != module and order.index(target) > order.index(module):
                found.append(
                    f"{path}:{line}: `{module}` imports `{spelling}` from `{target}`, "
                    "which `lib.rs` declares below it"
                )
    return found


def self_test() -> None:
    """Fixtures for the scanner. Raises AssertionError on failure."""
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp)
        (src / "lib.rs").write_text(
            "mod low;\npub use low::{Low, low_fn};\n\nmod high;\npub use high::High;\n"
        )
        (src / "low.rs").write_text(
            "use crate::High;\n"
            "use crate::{\n    low_fn,\n    high::Other,\n};\n"
            "// crate::High in a comment\n"
            'const S: &str = "crate::High";\n'
            "/// [`High`](crate::High)\n"
            "pub struct Low<'a>(&'a str);\n"
            "#[cfg(test)]\nuse crate::High as _;\n"
            "#[cfg(test)]\nmod tests {\n    use crate::High;\n"
            "    fn f() { let _ = '{'; }\n}\n"
            "pub fn low_fn() {}\n"
            "use super::High;\n"
        )
        (src / "low").mkdir()
        (src / "low" / "deep.rs").write_text(
            "use super::super::High;\nuse super::Low;\n"
        )
        (src / "high.rs").write_text(
            "use crate::{Low, low::low_fn};\npub struct High;\n"
        )
        files = [src / "low.rs", src / "low" / "deep.rs", src / "high.rs"]
        got = violations(src, files)

        def error(file: Path, line: int, spelling: str) -> str:
            return (
                f"{file}:{line}: `low` imports `{spelling}` from `high`, "
                "which `lib.rs` declares below it"
            )

        expected = [
            error(src / "low.rs", 1, "crate::High"),
            error(src / "low.rs", 2, "crate::high"),
            error(src / "low.rs", 18, "super::High"),
            error(src / "low" / "deep.rs", 1, "super::super::High"),
        ]
        if got != expected:
            raise AssertionError(
                f"plan-types-layering self-test: got {got}, expected {expected}"
            )


def tracked(prefix: Path) -> list[Path]:
    """Tracked `.rs` files under `prefix`, relative to the repo root."""
    result = subprocess.run(
        ["git", "ls-files", "--", str(prefix)],
        capture_output=True,
        text=True,
        check=True,
    )
    return [Path(line) for line in result.stdout.splitlines() if line.endswith(".rs")]


def main(argv: list[str] | None = None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv not in ([], ["--self-test"]):
        print(f"usage: {sys.argv[0]} [--self-test]", file=sys.stderr)
        return 2
    try:
        self_test()
    except AssertionError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    if argv == ["--self-test"]:
        print("plan-types-layering self-test OK")
        return 0

    files = tracked(SRC)
    found = violations(SRC, files)
    if found:
        print(
            "error: a `pipette-plan-types` module imports from a module "
            "declared below it.",
            file=sys.stderr,
        )
        print(
            "Each module imports only from the modules `lib.rs` declares "
            "above it. Move the",
            file=sys.stderr,
        )
        print(
            "item up to a module declared higher, or declare the importing "
            "module lower.",
            file=sys.stderr,
        )
        print(file=sys.stderr)
        for violation in found:
            print(f"  {violation}", file=sys.stderr)
        return 1
    print(f"plan-types-layering OK ({len(files)} source files checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
