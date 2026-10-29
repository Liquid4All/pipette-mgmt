#!/usr/bin/env python3
"""Reject docs section numbers (`docs/httpapi.md` §7.16) in Rust comments.

A section number moves whenever a doc is reorganized, and every comment citing
it then has to move with it. Name the doc file if the sentence needs it, never
the section. An RFC's sections are stable, so `RFC 9421 §2.2` is fine.
"""

import re
import subprocess
import sys
from pathlib import Path

RFC = re.compile(r"RFC \d+,? §\s?[\d.]+|§\s?[\d.]+ of RFC \d+")
MIN_FILES = 100


def comments(source: str) -> list[tuple[int, str]]:
    """Each line's comment text: `//` and `/* */`, never inside a string literal.

    Raw strings and escapes are only approximated; a `"` in a char literal would
    desync it, so `'"'` is skipped explicitly.
    """
    out = []
    block = 0
    for n, line in enumerate(source.split("\n"), 1):
        text = []
        i = 0
        in_string = False
        while i < len(line):
            if block:
                if line.startswith("/*", i):
                    block += 1
                    i += 2
                elif line.startswith("*/", i):
                    block -= 1
                    i += 2
                else:
                    text.append(line[i])
                    i += 1
            elif in_string:
                if line[i] == "\\":
                    i += 2
                    continue
                in_string = line[i] != '"'
                i += 1
            elif line.startswith("'\"'", i):
                i += 3
            elif line[i] == '"':
                in_string = True
                i += 1
            elif line.startswith("//", i):
                text.append(line[i:])
                break
            elif line.startswith("/*", i):
                block = 1
                i += 2
            else:
                i += 1
        if text:
            out.append((n, "".join(text)))
    return out


def flagged(source: str) -> list[tuple[int, str]]:
    return [(n, text) for n, text in comments(source) if "§" in RFC.sub("", text)]


def selftest() -> list[str]:
    cases = [
        ("/// See `docs/httpapi.md` §7.16.", True),
        ("// ruled out (§5.10)", True),
        ("let x = 1; // see §5", True),
        ("/* the gate, §5.2 */", True),
        ("/*\n * across lines, §5.2\n */", True),
        ("//! RFC 9421 §2: the covered components", False),
        ("// RFC 9421 §2 and RFC 9530 §2", False),
        ("// §2 of RFC 9421", False),
        ('let s = "(`docs/httpapi.md` §7)";', False),
        ('let s = "a // b §7";', False),
        ("/// See `docs/httpapi.md`.", False),
    ]
    return [
        f"selftest: {source!r} flagged={not want}, want {want}"
        for source, want in cases
        if bool(flagged(source)) != want
    ]


def main() -> int:
    failures = selftest()
    if failures:
        print("\n".join(failures))
        return 1
    files = [
        Path(p)
        for p in subprocess.run(
            ["git", "ls-files", "*.rs"], capture_output=True, text=True, check=True
        ).stdout.split()
    ]
    if len(files) < MIN_FILES:
        print(f"expected to walk the Rust sources, found {len(files)} files")
        return 1
    hits = [
        f"{path}:{n}: {text.strip()[:110]}"
        for path in files
        for n, text in flagged(path.read_text(encoding="utf-8"))
    ]
    if not hits:
        print(f"doc section refs OK ({len(files)} Rust files scanned)")
        return 0
    print(f"docs section numbers in Rust comments ({len(hits)}):")
    print("\n".join(f"  {h}" for h in hits))
    print("\nName the doc file if the sentence needs it, not the section.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
