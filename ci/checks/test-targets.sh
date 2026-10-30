#!/usr/bin/env bash
#
# Guards the test-binary grouping that a crate's `tests/suite.rs` documents,
# which its `Cargo.toml` turns auto-discovery off to express. Three ways to get
# it wrong, none of which any other gate catches:
#
# - A file no target claims is silently never run: cargo compiles nothing and
#   the file reads like a passing test suite.
# - A file claimed twice is compiled into two binaries and run twice.
# - A file that captures logs, in the shared binary, flakes about one run in
#   three: `tracing` caches a callsite's interest process-globally, so a
#   neighbour reaching the same `event!` first with no subscriber installed
#   caches it as never (see `tests/helpers/logs.rs`).
#
# Files under `tests/helpers/` are a module tree, not test targets, which is
# what the `:(glob)` pathspec below leaves out: under that magic `*` stops
# matching `/`, so only the top level of each `tests/` is listed.
#
# A directory target (`path = "tests/<dir>/main.rs"`) is checked the same way:
# each other `tests/<dir>/*.rs` must be a `mod` of that main.rs, and none may
# capture logs, since they all share its binary.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

unwired=()
double=()
captured=()
checked=0

while IFS= read -r f; do
  name=$(basename "$f" .rs)
  [[ "$name" == "suite" ]] && continue

  crate=${f%/tests/*}
  # `[[test]] path` in a manifest is relative to the crate it is written in.
  rel="tests/${name}.rs"
  suite="$crate/tests/suite.rs"
  checked=$((checked + 1))

  own_target=0
  if grep -qxF "path = \"$rel\"" "$crate/Cargo.toml"; then
    own_target=1
  fi
  suite_module=0
  if [[ -f "$suite" ]] && grep -qxF "mod $name;" "$suite"; then
    suite_module=1
  fi

  if ((own_target == 0 && suite_module == 0)); then
    unwired+=("$f")
  elif ((own_target == 1 && suite_module == 1)); then
    double+=("$f")
  elif ((suite_module == 1)) && grep -q 'capture_logs' "$f"; then
    captured+=("$f")
  fi
done < <(git ls-files ':(glob)crates/*/tests/*.rs')

while IFS= read -r manifest; do
  crate=${manifest%/Cargo.toml}
  while IFS= read -r dir; do
    main="$crate/tests/$dir/main.rs"
    while IFS= read -r f; do
      name=$(basename "$f" .rs)
      [[ "$name" == "main" ]] && continue
      checked=$((checked + 1))
      if ! grep -qxF "mod $name;" "$main"; then
        unwired+=("$f")
      elif grep -q 'capture_logs' "$f"; then
        captured+=("$f")
      fi
    done < <(git ls-files ":(glob)$crate/tests/$dir/*.rs")
  done < <(sed -n 's|^path = "tests/\([^/]*\)/main\.rs"$|\1|p' "$manifest")
done < <(git ls-files ':(glob)crates/*/Cargo.toml')

fail=0

if [[ ${#unwired[@]} -gt 0 ]]; then
  echo "error: integration test file(s) that no test target builds." >&2
  echo "The crate sets autotests = false, so a test file runs only if it is" >&2
  echo "listed in its Cargo.toml as a [[test]] path or declared as a mod in" >&2
  echo "its tests/suite.rs, or in the tests/<dir>/main.rs of its directory." >&2
  printf '  %s\n' "${unwired[@]}" >&2
  echo >&2
  fail=1
fi

if [[ ${#double[@]} -gt 0 ]]; then
  echo "error: integration test file(s) wired twice." >&2
  echo "Each is both a mod of its crate's tests/suite.rs and its own [[test]]" >&2
  echo "path in Cargo.toml, so cargo builds it into two binaries and runs its" >&2
  echo "tests twice. Keep one wiring." >&2
  printf '  %s\n' "${double[@]}" >&2
  echo >&2
  fail=1
fi

if [[ ${#captured[@]} -gt 0 ]]; then
  echo "error: log-capturing test file(s) sharing a binary." >&2
  echo "A test that captures logs needs a process to itself, or a neighbour" >&2
  echo "reaching the same callsite first can cache it as never and the capture" >&2
  echo "comes back empty (tests/helpers/logs.rs). Drop the mod line from the" >&2
  echo "crate's tests/suite.rs and give the file its own [[test]] block in" >&2
  echo "Cargo.toml; a module under tests/<dir>/ moves out to tests/<name>.rs first." >&2
  printf '  %s\n' "${captured[@]}" >&2
  echo >&2
  fail=1
fi

if [[ $fail -ne 0 ]]; then
  exit 1
fi

echo "test target wiring OK ($checked test files checked)"
