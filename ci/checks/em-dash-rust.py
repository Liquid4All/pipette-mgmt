#!/usr/bin/env python3
"""Reject em dashes in user-visible Rust copy.

An em dash is uncommon to type by hand, so it reads as machine-written in the
text this server prints. The CLI is the surface that matters: `println!` output,
table cells and error messages all reach a terminal a person reads. Markdown and
Python are each guarded separately (`em-dash-docs.py`, `em-dash-python.py`).

Running here, in `conventions`, means the rule is checked with no toolchain and
no path filter, so it costs seconds and no compile.

Exempt:

- **Comments**, line and block, block comments nesting.
- **Logging.** `tracing::` macros: developer text that never reaches a screen a
  user reads. `println!` and `eprintln!` are deliberately NOT exempt, because
  they print to a terminal a person reads.
- **`#[cfg(test)]` modules.** Assertion messages are developer-facing, and
  fixture strings can be load-bearing: a scoring fixture may carry an em dash
  because the recorded model output did.
- **A string literal that is exactly one em dash**, the empty-value glyph.

Two details are load-bearing and were paid for in real bugs upstream, in the
clients repo this check is ported from:

1. A log call's arguments are blanked by CHARACTER, not by skipping the line. A
   line-level exemption lets `tracing::warn!("x"); return Err("a - b".into());`
   through, and that line's paren delta is zero, so no amount of depth
   bookkeeping catches it either.
2. String literals are tracked while scanning, char literals included. A `'"'`
   flips string state on and never off, after which every later comment in the
   file is scanned as copy.

Fix a failure by rewriting the sentence, not by swapping in an en dash or a
double hyphen: those are the same tell wearing a hat. A period, comma, colon, or
parentheses almost always carries the same clause break.
"""

import re
import subprocess
import sys
from pathlib import Path

EM_DASH = chr(0x2014)

# Only the shipping sources, in any crate of the workspace. `tests/` is
# developer-facing, for the same reason `#[cfg(test)]` is exempt below.
RUST_SOURCES = re.compile(r"^crates/[^/]+/src/.*\.rs$")

# A broken walk would make this check vacuously green, so it fails instead when
# it finds implausibly few files. The tree holds ~50; this floor only has to be
# far enough below that to survive ordinary deletions.
MIN_FILES = 30

# Always fully qualified in this repo, which is what lets a prefix match stand in
# for a real macro-path parse.
LOG_MACROS = (
    "tracing::trace!",
    "tracing::debug!",
    "tracing::info!",
    "tracing::warn!",
    "tracing::error!",
)


def log_call(line: str, i: int) -> int | None:
    """Width of a `tracing::level!(` prefix at `i`, or None."""
    for macro in LOG_MACROS:
        if (
            line.startswith(macro, i)
            and line[i + len(macro) : i + len(macro) + 1] == "("
        ):
            return len(macro) + 1
    return None


def string_open(line: str, i: int) -> tuple[int, str, bool] | None:
    """A Rust string opening at `i`, as (width, closing delimiter, escapes)."""
    if line[i] == "r" and not (i and (line[i - 1].isalnum() or line[i - 1] == "_")):
        hashes = 0
        while line[i + 1 + hashes : i + 2 + hashes] == "#":
            hashes += 1
        if line[i + 1 + hashes : i + 2 + hashes] == '"':
            # Raw strings carry their hash count because their contents routinely
            # include bare quotes.
            return hashes + 2, '"' + "#" * hashes, False
    if line[i] == '"':
        return 1, '"', True
    return None


def char_literal_width(line: str, i: int) -> int | None:
    """Width of a Rust char literal at `i`, or None.

    A char literal such as `'"'` or `'('` must not shift string state or paren
    depth. A lifetime (`'a`) has no closing quote, so only skip when the shape
    really is a one-or-two-character literal.
    """
    if line[i] != "'":
        return None
    if line[i + 2 : i + 3] == "'":
        return 3
    if line[i + 1 : i + 2] == "\\" and line[i + 3 : i + 4] == "'":
        return 4
    return None


def keep(code: list[str], ch: str, blank: bool) -> None:
    """Append `ch`, or a space when inside a log call's argument span.

    Module level, and told explicitly whether to blank, rather than a closure over
    the scanner's state: a closure would capture the loop variables late, which is
    the same class of bug the scanner exists to avoid.
    """
    code.append(" " if blank else ch)


def scan_lines(source: str) -> list[tuple[str, bool]]:
    """Blank every span that is not user-visible copy.

    Returns one `(code, in_test)` per line. What survives in `code` is real code
    and the string literals a person can actually read.

    Kept as one function on purpose: it is a character-level state machine, and
    splitting it would spread the string, comment, brace-depth, and log-depth
    state across call boundaries, making exactly the desync bugs it exists to
    prevent easier to reintroduce.
    """
    out: list[tuple[str, bool]] = []
    # Rust block comments NEST, so this counts depth rather than holding a flag.
    # With a flag, the inner `*/` of `/* a /* b */ still */` ends the comment
    # early and the tail gets scanned as copy.
    block_depth = 0
    closer: str | None = None  # Non-None while inside a string literal.
    escapes = False
    log_depth: int | None = None
    brace_depth = 0
    pending_test = False
    test_exit_depth: int | None = None

    for line in source.split("\n"):
        code: list[str] = []
        i = 0
        line_starts_in_test = test_exit_depth is not None

        while i < len(line):
            c = line[i]
            blank = log_depth is not None

            if block_depth > 0:
                if line.startswith("/*", i):
                    block_depth += 1
                    i += 2
                elif line.startswith("*/", i):
                    block_depth -= 1
                    i += 2
                else:
                    i += 1
                continue

            if closer is not None:
                if escapes and c == "\\":
                    keep(code, c, blank)
                    if i + 1 < len(line):
                        keep(code, line[i + 1], blank)
                    i += 2
                    continue
                if line.startswith(closer, i):
                    for k in range(len(closer)):
                        keep(code, line[i + k], blank)
                    i += len(closer)
                    closer = None
                    continue
                keep(code, c, blank)
                i += 1
                continue

            if line.startswith("/*", i):
                block_depth = 1
                i += 2
                continue
            if line.startswith("//", i):
                break

            if c == "#" and line.startswith("#[cfg(test)]", i):
                # Arms the next `{` as the start of a test block.
                pending_test = True

            if log_depth is None:
                width = log_call(line, i)
                if width is not None:
                    code.append(" " * width)
                    log_depth = 1
                    i += width
                    continue

            width = char_literal_width(line, i)
            if width is not None:
                for k in range(width):
                    keep(code, line[i + k], blank)
                i += width
                continue

            opened = string_open(line, i)
            if opened is not None:
                width, closer, escapes = opened
                for k in range(width):
                    keep(code, line[i + k], blank)
                i += width
                continue

            # Brace bookkeeping in code context only, so braces inside strings
            # and comments cannot shift it.
            if c == "{":
                if pending_test:
                    pending_test = False
                    test_exit_depth = brace_depth
                brace_depth += 1
            elif c == "}":
                brace_depth -= 1
                if test_exit_depth == brace_depth:
                    test_exit_depth = None

            if log_depth is not None:
                if c == "(":
                    log_depth += 1
                elif c == ")":
                    log_depth -= 1
                    if log_depth <= 0:
                        log_depth = None
                code.append(" ")
                i += 1
                continue

            code.append(c)
            i += 1

        # A line is test code if it was inside the block on entry or opened it, so
        # the `#[cfg(test)]` and `mod tests {` lines are covered too.
        in_test = line_starts_in_test or test_exit_depth is not None or pending_test
        out.append(("".join(code), in_test))
    return out


def contains_padded(haystack: str, prefix: str, digits: str) -> bool:
    """True when `prefix` appears followed by optional zeros and then `digits`."""
    at = haystack.find(prefix)
    while at != -1:
        if haystack[at + len(prefix) :].lstrip("0").startswith(digits):
            return True
        at = haystack.find(prefix, at + 1)
    return False


def has_em_dash(line: str) -> bool:
    """Every rendering this guard rejects.

    The literal character is not enough: an escape or an HTML entity renders as one
    too. The needles are built from the code point so this file does not contain
    its own.
    """
    # A lone em dash is the empty-value placeholder, not punctuation.
    stripped = line.replace(f'"{EM_DASH}"', '""')
    if EM_DASH in stripped:
        return True
    lower = stripped.lower()
    # Rust's `\u{...}` accepts leading zeros, so a padded spelling renders
    # identically and must be caught.
    if contains_padded(lower, "\\u{", f"{0x2014:x}}}"):
        return True
    # Lowercased because `&MDASH;` renders the same as `&mdash;`.
    if "&" + "mdash" + ";" in lower:
        return True
    # HTML numeric references DO allow leading zeros, and a padded form renders
    # exactly like its unpadded twin, so skip any padding run before the digits.
    return any(
        contains_padded(lower, prefix, digits)
        for prefix, digits in (("&#", f"{8212};"), ("&#x", f"{0x2014:x};"))
    )


def flagged(source: str) -> list[str]:
    """Run the real detector over an in-memory source and return what it flags."""
    return [
        code.strip()
        for code, in_test in scan_lines(source)
        if not in_test and has_em_dash(code)
    ]


def sources() -> list[Path]:
    out = subprocess.run(
        ["git", "ls-files"], capture_output=True, text=True, check=True
    ).stdout.split("\n")
    return [Path(p) for p in out if p and RUST_SOURCES.match(p)]


def offenders(path: Path) -> list[str]:
    text = path.read_text(errors="replace")
    return [
        f"{path}:{n}: {code.strip()[:110]}"
        for n, (code, in_test) in enumerate(scan_lines(text), 1)
        if not in_test and has_em_dash(code)
    ]


def selftest() -> list[str]:
    """Fixtures for the exemptions, run on every invocation.

    An exemption that is too eager still exits 0, so nothing surfaces it. These
    cases are the ones that have actually been wrong at some point.
    """
    d = EM_DASH
    # Composed rather than written out, for the same reason the needles are: a
    # fixture holding the entity literally would make this file trip its own check.
    ent = "&" + "mdash" + ";"
    ent_upper = "&" + "MDASH" + ";"
    num_padded = "&#" + f"0{8212};"
    hex_ent = "&#x" + f"{0x2014:x};"
    brace_escape = "\\u{" + f"{0x2014:x}" + "}"
    brace_escape_padded = "\\u{" + f"000{0x2014:x}" + "}"

    cases: list[tuple[str, int, str]] = [
        (f'let m = "port in use {d} try again";', 1, "plain offender"),
        (f"// a comment {d} here\nlet x = 1;", 0, "line comment"),
        (f"/// a doc comment {d} here\nlet x = 1;", 0, "doc comment"),
        (f"/* spans\n * a {d} break\n */\nlet x = 1;", 0, "block comment"),
        (
            f"/* outer /* inner */ still a {d} comment */\nlet x = 1;",
            0,
            "NESTED block comment stays exempt",
        ),
        (
            f'/* outer /* inner */ */\nlet s = "a {d} b";',
            1,
            "code AFTER a nested comment is still checked",
        ),
        # --- logging is developer text -----------------------------------------
        (f'tracing::warn!("retrying {d} backoff");', 0, "log macro"),
        (
            f'tracing::info!(\n    "wrapped {d} message"\n);',
            0,
            "multi-line log macro",
        ),
        (
            f'tracing::warn!("x"); return Err("a {d} b".into());',
            1,
            "a log call must not exempt the REST of its line",
        ),
        (f'println!("a {d} b");', 1, "println reaches a terminal a person reads"),
        (f'eprintln!("a {d} b");', 1, "eprintln likewise"),
        # --- the empty-value glyph ---------------------------------------------
        (f'let cell = value.unwrap_or("{d}");', 0, "lone glyph"),
        (
            f'let a = "{d}"; let b = "real {d} prose";',
            1,
            "the glyph does not excuse prose on the same line",
        ),
        # --- strings that must not desync the scan -----------------------------
        (
            f'let q = \x27"\x27;\n// a comment {d} here\n',
            0,
            "a CHAR LITERAL holding a quote must not flip string state",
        ),
        (
            f'let s = "// not a comment {d}";',
            1,
            "a // inside a string does not start a comment",
        ),
        (f'let s = r#"raw {d} here"#;', 1, "raw string"),
        (
            f'let s = r#"has " quote {d}"#;',
            1,
            "a bare quote inside a raw string does not close it",
        ),
        # --- test code is exempt ------------------------------------------------
        (
            f'#[cfg(test)]\nmod tests {{\n    let body = "a {d} b";\n}}\n',
            0,
            "cfg(test) fixtures can be load-bearing",
        ),
        (
            '#[cfg(test)]\nmod tests {\n    let s = "{}";\n}\n'
            f'let real = "a {d} b";\n',
            1,
            "a brace in a string must not end the test module early",
        ),
        # --- every rendering ----------------------------------------------------
        (f'let s = "a {ent} b";', 1, "html entity"),
        (f'let s = "a {ent_upper} b";', 1, "entity, uppercase"),
        (f'let s = "a {num_padded} b";', 1, "zero-padded entity"),
        (f'let s = "a {hex_ent} b";', 1, "hex entity"),
        (f'let s = "a {brace_escape} b";', 1, "brace escape"),
        (
            f'let s = "a {brace_escape_padded} b";',
            1,
            "brace escape accepts leading zeros",
        ),
        # --- clean source ---------------------------------------------------------
        ('let s = "plain";', 0, "no glyph"),
    ]

    failures = []
    for source, want, why in cases:
        got = len(flagged(source))
        if got != want:
            failures.append(f"selftest: expected {want} hit(s), got {got} for {why}")
    return failures


def main() -> int:
    failures = selftest()
    if failures:
        print("the guard's own fixtures failed, so its verdict cannot be trusted:")
        for f in failures:
            print(f"  {f}")
        return 1

    files = sources()
    if len(files) < MIN_FILES:
        print(f"expected to walk the Rust sources, found {len(files)} files")
        return 1
    hits = [h for f in files for h in offenders(f)]

    if not hits:
        print(f"em dash check OK ({len(files)} Rust files scanned)")
        return 0
    print(f"em dashes in user-visible copy ({len(hits)}):")
    for h in hits:
        print(f"  {h}")
    print()
    print("Rewrite the sentence instead of swapping in an en dash or '--'.")
    print("Comments, tracing:: logging, #[cfg(test)] code, and a string that is")
    print("exactly one em dash are exempt.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
