#!/usr/bin/env python3
"""Keep secrets inside the newtypes that redact them.

Four secret values cross this server, two per principal kind: a pre-auth token
as it is minted or presented, and the secret half that token carries. Each is a
newtype whose `Debug` is hand-written to print `(redacted)`, and which derives
neither `Display` nor `Serialize` nor `Deref` (see `docs/authentication.md`).
That is what makes it safe to `{:?}` a whole request or record: the leaf hides
itself, so every struct holding one inherits the redaction.

Nothing in the compiler defends that arrangement. A field added as
`token: String` gets a derived `Debug` and is published by the first log line
that dumps its parent, and a `Debug` slipped into a secret newtype's `nutype`
derive list silently replaces the hand-written redaction. A unit test can't see
either one: it can only assert about a token it plants itself, in a field that
already exists.

So this check reads the declarations instead:

1. A struct or enum field whose name reads like a secret must be typed with a
   registered secret newtype (bare, or inside `Option`/`Vec`).
2. A registered secret newtype must hand-write a `Debug` that renders
   `(redacted)` without touching the raw value, must not derive `Debug`,
   `Display` or `Serialize`, and must not gain `Deref` — the escape hatch is
   `AsRef`, through the `expose()` method that makes every disclosure greppable.

Rule 2 covers the server's own secret types. A redacting newtype whose shape the
clients' wire fixes satisfies rule 1 and is listed in `WIRE_REDACTING_TYPES`,
where the reasoning for each entry lives.

Field-name matching deliberately treats a plural `tokens` as *not* a secret, and
a name ending in `_hash` is a stored digest rather than a credential. Only type
bodies are scanned, so a `token: &str` function parameter or a `const
TOKEN_PREFIX` naming a prefix is not a violation.
"""

import re
import subprocess
import sys
from pathlib import Path

# Registered secret newtypes: the type a secret-named field must be declared
# with, and whose own shape rule 2 enforces. Adding a secret means adding it
# here — which is the review moment this check exists to force.
SECRET_TYPES = {
    "ClientPreauthToken": Path("crates/pipette-core/src/client_preauth.rs"),
    "ClientPreauthSecret": Path("crates/pipette-core/src/client_preauth.rs"),
    "UserPreauthToken": Path("crates/pipette-core/src/user_preauth.rs"),
    "UserPreauthSecret": Path("crates/pipette-core/src/user_preauth.rs"),
    "DuckDbPassword": Path("crates/pipette-core/src/config.rs"),
    "GitHubToken": Path("crates/pipette-pipeline/src/catalog_import/llamacpp.rs"),
}

# Types that answer a secret-sounding field name but hold no credential.
# Reviewed exceptions, so a secret-named field typed `String` still fails.
#
# `bool` is here for `require_client_preauth_key`, a config switch whose name
# ends in `preauth_key` while holding nothing but a policy flag.
NON_SECRET_TYPES = {"bool"}

# Redacting newtypes whose shape the clients' wire fixes. They satisfy rule 1
# (the field is not a bare `String`) but are exempt from rule 2.
#
# `AuthToken` is `pipette-plan-types`' HuggingFace access token, which
# pipette-clients takes from here by git. It redacts the way the types above do, with a hand-written
# `Debug`, a redacting `Display` and `AsRef` rather than `Deref`, and its own
# tests pin both renderings. Rule 2 would still reject it on three counts, none
# of which are defects: it derives `Serialize` because the token travels to the
# client inside `spec.model`, it renders `<redacted>` rather than `(redacted)`,
# and it has a `Display` at all (deliberately, so `{token}` compiles to
# something safe).
#
# `ClientPreauthKey` is the device pre-auth key in `pipette-plan-types`' wire
# registration body, so a client can send it. It derives `Serialize` for that
# reason, and its hand-written `Debug` redacts, pinned by its own test.
#
# `UserPreauthKey` is its account counterpart in the operator API's bodies:
# sent on a registration, answered once on a mint. The same
# hand-written `Debug` redacts it, pinned by its test.
#
# `ClientPrivateKey` is the device signing key a registration answers when the
# server generates the keypair, so a client reads it. Same shape and test.
#
# This is type-scoped, not field-scoped, on purpose: an `auth_token` retyped to
# `String` still fails rule 1.
WIRE_REDACTING_TYPES = {
    "AuthToken",
    "ClientPreauthKey",
    "ClientPrivateKey",
    "UserPreauthKey",
}

# Reviewed per-field exceptions, as `path:field`. A lock lease id is a
# capability guard rather than a credential: `release` compares it to decide
# whether this process still owns the lease, and it grants nothing to whoever
# reads it. It is also persisted as JSON under that name, so the field name is
# on-disk format rather than a free choice.
EXEMPT_FIELDS = {
    "crates/pipette-store-init/src/storage_lock.rs:token",
}

# Expressions that reach the wrapped value. A rendering impl containing one is
# printing the secret, whatever else it also prints.
RAW_ACCESS = ("as_ref", "self.0", "into_inner", "expose")

REDACTED = "(redacted)"

# Derives that would leak a secret: `Debug` and `Display` render it, `Serialize`
# writes it into whatever record its parent is stored in, `Deref` hands out the
# raw value with no `expose()` to grep for.
FORBIDDEN_DERIVES = ("Debug", "Display", "Serialize", "Deref")

# A field name that reads like it holds a credential. `token` matches only in
# the singular — see the module docstring.
SECRET_NAME = re.compile(
    r"^(?:[a-z0-9_]*_)?"
    r"(?:token|secret|password|passwd|credential|api_key|apikey|preauth_key|private_key)$"
)

# `pub token: Option<ClientPreauthToken>,` → name, type. Struct fields and
# struct-variant fields read the same.
FIELD = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?([a-z][a-z0-9_]*)\s*:\s*(.+?),?\s*$")

# `struct Foo {` / `enum Bar {` — the bodies rule 1 scans.
TYPE_OPEN = re.compile(
    r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum)\s+[A-Za-z_][A-Za-z0-9_]*"
)

# The type a field must resolve to once wrappers are peeled off.
WRAPPER = re.compile(r"^(?:Option|Vec)<(.+)>$")


def tracked_rust_files() -> list[Path]:
    out = subprocess.run(
        ["git", "ls-files", "crates/*/src/*.rs", "crates/*/tests/*.rs"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.split()
    return [Path(f) for f in out]


def unwrap(ty: str) -> str:
    """`Option<Vec<a::b::ClientPreauthToken>>` → `ClientPreauthToken`.

    Wrappers are peeled and the module path is dropped, so a field declared
    `crate::client_preauth::ClientPreauthToken` reads as the type it is. A path
    is not evidence on its own — a second type of the same name elsewhere would
    read as registered — but the alternative, matching the path too, makes the
    check fail on a move that changed nothing about the secret.
    """
    while match := WRAPPER.match(ty.strip()):
        ty = match.group(1)
    return ty.strip().rsplit("::", 1)[-1]


def type_body_fragments(path: Path) -> list[tuple[int, str]]:
    """Field-declaration candidates inside a `struct`/`enum` body.

    Brace counting, not parsing: it only has to answer "could this be a field
    declaration in a type body", which excludes function parameters and consts.
    A body opening and closing on one line (`struct Foo { a: u8 }`) still
    declares fields, so the text after the brace is split and kept.
    """
    fragments: list[tuple[int, str]] = []
    depth = 0
    for number, line in enumerate(path.read_text().splitlines(), start=1):
        if depth == 0:
            if not (TYPE_OPEN.match(line) and "{" in line):
                continue
            inline = line.split("{", 1)[1].rstrip().removesuffix("}")
            fragments.extend((number, field) for field in inline.split(","))
        else:
            fragments.append((number, line))
        depth += line.count("{") - line.count("}")
    return fragments


def check_fields(files: list[Path]) -> list[str]:
    """Rule 1: a secret-named field is declared with a registered secret type."""
    violations = []
    approved = SECRET_TYPES.keys() | NON_SECRET_TYPES | WIRE_REDACTING_TYPES
    for path in files:
        for number, line in type_body_fragments(path):
            if (stripped := line.strip()).startswith(("//", "#[")):
                continue
            if not (field := FIELD.match(line)):
                continue
            name, ty = field.group(1), field.group(2)
            if not SECRET_NAME.match(name) or unwrap(ty) in approved:
                continue
            if f"{path}:{name}" in EXEMPT_FIELDS:
                continue
            violations.append(
                f"{path}:{number}: field `{name}: {ty}` reads as a secret but is "
                f"not one of {', '.join(sorted(SECRET_TYPES))}.\n"
                f"       Declare it with a redacting newtype, or, if it holds no "
                f"credential, add it to NON_SECRET_TYPES or EXEMPT_FIELDS in "
                f"{Path(__file__).name}. A redacting newtype the clients' "
                f"wire fixes goes in WIRE_REDACTING_TYPES instead.\n"
                f"       {stripped}"
            )
    return violations


def rendering_impl(source: str, trait: str, ty: str) -> str | None:
    """The code of `impl fmt::<trait> for <ty>`, or `None` if absent.

    Both the `fmt::` and `std::fmt::` spellings are accepted, since either
    compiles and the choice is a matter of what the file imported. Bounded at
    the impl's closing brace so a `(redacted)` belonging to some later impl
    can't vouch for this one, and stripped of comments so prose about `expose`
    neither implicates nor absolves the impl. Stripping is line-based, which
    would also cut a `//` inside a string literal — no redaction marker
    contains one.
    """
    for prefix in ("std::fmt::", "fmt::"):
        start = source.find(f"impl {prefix}{trait} for {ty} {{")
        if start != -1:
            break
    else:
        return None
    end = source.find("\n}", start)
    code = source[start:] if end == -1 else source[start:end]
    return "\n".join(line.split("//", 1)[0] for line in code.splitlines())


def derive_list(source: str, ty: str) -> str:
    """The `#[nutype(derive(...))]` / `#[derive(...)]` list preceding `struct <ty>`."""
    head = source.split(f"struct {ty}(")[0]
    for opener in ("#[nutype", "#[derive"):
        if opener in head:
            return head.rsplit(opener, 1)[-1]
    return ""


def check_newtype_shape() -> list[str]:
    """Rule 2: the secret newtypes keep hand-written, redacting renderings."""
    violations = []
    for ty, path in SECRET_TYPES.items():
        if not path.exists():
            violations.append(
                f"{path}: expected to define `{ty}`; update SECRET_TYPES in "
                f"{Path(__file__).name} if it moved."
            )
            continue
        source = path.read_text()
        if f"struct {ty}(" not in source:
            violations.append(
                f"{path}: no `struct {ty}(`; update SECRET_TYPES in "
                f"{Path(__file__).name} if it moved or was renamed."
            )
            continue

        # Rendering has to be hand-written, so a derived one is the bug, and a
        # derived `Serialize` or `Deref` is a leak of its own.
        derives = derive_list(source, ty)
        for forbidden in FORBIDDEN_DERIVES:
            if re.search(rf"\b{forbidden}\b", derives):
                violations.append(
                    f"{path}: `{ty}` derives {forbidden}, which exposes the "
                    f"secret.\n"
                    f"       Drop it from the derive list; `Debug` is hand-written "
                    f"below, printing `{REDACTED}`."
                )

        body = rendering_impl(source, "Debug", ty)
        if body is None:
            violations.append(
                f"{path}: `{ty}` has no hand-written Debug printing `{REDACTED}`.\n"
                f"       Without it a dump publishes the secret, and a type with "
                f"no Debug at all is one no surrounding struct can derive Debug for."
            )
        else:
            if REDACTED not in body:
                violations.append(
                    f"{path}: `{ty}`'s Debug does not print `{REDACTED}`.\n"
                    f"       Whatever it renders instead reaches the logs."
                )
            if leak := next((r for r in RAW_ACCESS if r in body), None):
                violations.append(
                    f"{path}: `{ty}`'s Debug reaches the raw value via `{leak}`.\n"
                    f"       A rendering impl must not touch it; `expose()` is for "
                    f"callers that need the real value."
                )

        # `Display` is absent by design here, so `{}` on a secret does not
        # compile. One written later still has to redact.
        display = rendering_impl(source, "Display", ty)
        if display is not None and REDACTED not in display:
            violations.append(
                f"{path}: `{ty}` has a Display that does not print `{REDACTED}`.\n"
                f"       Prefer no Display at all; a `{{}}` on a secret should not "
                f"compile."
            )

        if re.search(rf"impl (?:std::)?ops::Deref for {ty}\b", source):
            violations.append(
                f"{path}: `{ty}` exposes `Deref`, which re-exposes the raw value.\n"
                f"       `AsRef`, behind `expose()`, is the sanctioned door; see "
                f"docs/authentication.md."
            )
    return violations


def main() -> int:
    violations = check_fields(tracked_rust_files()) + check_newtype_shape()
    if not violations:
        print("secret newtypes OK")
        return 0
    print("secret values must live in a redacting newtype:\n")
    for violation in violations:
        print(f"  {violation}")
    print("\nRule: docs/authentication.md.")
    return 1


if __name__ == "__main__":
    sys.exit(main())
