#!/usr/bin/env bash
#
# Runs every executable check in ci/checks/ and reports a summary. Each check is
# run via its own shebang, so a check can be any language (bash, python, …) — no
# per-language runner here. The same entry point runs locally (`./ci/lint.sh`)
# and in CI, so a green local run matches the PR check. See ci/README.md to add
# one.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

checks=()
for f in ci/checks/*; do
  # An `if` rather than `[[ ... ]] && checks+=(...)`: under `set -e` the `&&`
  # list is the loop body's last command, so the first non-executable entry
  # would exit this script instead of being skipped.
  if [[ -f "$f" && -x "$f" ]]; then
    checks+=("$f")
  fi
done
if [[ ${#checks[@]} -eq 0 ]]; then
  echo "no checks found in ci/checks/"
  exit 0
fi

# Preflight the interpreter before running anything, if any selected check is
# Python. Several of them annotate with PEP 604 unions (`X | None`) at module
# level, where the annotation is evaluated at import time, so they need 3.10+.
# On an older python3 every one of them fails with
#   TypeError: unsupported operand type(s) for |: 'type' and 'NoneType'
# which this script would print as `FAIL <name>`, indistinguishable from a real
# convention violation. That is how a stale interpreter can silently disable the
# very gates this script exists to enforce, em-dash-rust among them.
#
# The floor is the language requirement, not the CI version: CI runs 3.12 (see
# the python-check job), and anything from 3.10 up runs these checks correctly.
if printf '%s\n' "${checks[@]}" | grep -q '\.py$'; then
  if ! python3 -c 'import sys; sys.exit(0 if sys.version_info >= (3, 10) else 1)' 2>/dev/null; then
    have=$(python3 -c 'import sys; print("%d.%d.%d" % sys.version_info[:3])' 2>/dev/null) || have="missing"
    echo "ci/lint.sh: python3 is ${have}, but the Python checks in ci/checks/ need 3.10+." >&2
    echo "  Running them on an older interpreter reports each as a failure with a" >&2
    echo "  'TypeError: unsupported operand type(s) for |', which looks like a" >&2
    echo "  convention violation rather than the environment problem it is." >&2
    echo "  Put a newer python3 first on PATH (pyenv, uv, or homebrew). CI runs 3.12." >&2
    exit 1
  fi
fi

failed=0
for check in "${checks[@]}"; do
  name=$(basename "$check")
  name=${name%.*}
  if out=$("$check" 2>&1); then
    printf '  ok   %s\n' "$name"
  else
    printf '  FAIL %s\n' "$name"
    printf '%s\n' "$out" | sed 's/^/       /'
    failed=1
  fi
done

if [[ $failed -ne 0 ]]; then
  echo
  echo "convention checks failed — see output above" >&2
  exit 1
fi
echo "all convention checks passed"
