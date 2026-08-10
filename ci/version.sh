#!/usr/bin/env bash
# Naming for published builds. Also runnable locally: `ci/version.sh` answers
# "what would this commit publish as?" without pushing.
#
#   version  release/2026.08.1 three commits in -> 2026.08.1-3-ga1b2c3d4ab
#            anything else -> the bare commit sha
#   image    GHCR path for this repository
#
# The release form is the train from the branch name, the distance from the
# branch point, and the commit. Nothing reads the clock and nothing needs a
# pre-existing tag, so a commit always names itself the same way. It is also
# the shape `git describe` emits, so tagging branch points later changes
# nothing.
#
# The one thing that can renumber a released commit is rewriting main: the
# distance is measured from `git merge-base origin/main HEAD`, so a force-push
# that moves the branch point moves the count with it.
#
# Usage: ci/version.sh [version|image]
set -euo pipefail

ref="${GITHUB_REF:-refs/heads/$(git rev-parse --abbrev-ref HEAD)}"

repo="${GITHUB_REPOSITORY:-}"
if [[ -z "$repo" ]]; then
  url="$(git remote get-url origin)"
  url="${url%.git}"
  # scp-style remotes separate host and org with `:`, which basename ignores.
  url="${url//:/\/}"
  repo="$(basename "$(dirname "$url")")/$(basename "$url")"
fi

version() {
  case "$ref" in
    refs/heads/release/*)
      train="${ref#refs/heads/release/}"
      # `release/**` matches nested names, and a `/` would be illegal in the
      # image tag. Fail loudly on the branch name rather than at push time.
      case "$train" in
        */*) echo "release branch must not nest: ${train}" >&2; exit 1 ;;
      esac
      # Distance from where the branch left main. Merging main back in adds
      # only the merge commit, so this never goes backwards.
      base="$(git merge-base origin/main HEAD)"
      echo "${train}-$(git rev-list --count "${base}..HEAD")-g$(git rev-parse --short=10 HEAD)"
      ;;
    *) git rev-parse --short=10 HEAD ;;
  esac
}

# GHCR rejects uppercase path segments and the org is `Liquid4All`.
image() { echo "ghcr.io/$(echo "$repo" | tr '[:upper:]' '[:lower:]')"; }

case "${1:-version}" in
  version) version ;;
  image) image ;;
  *) echo "usage: $0 [version|image]" >&2; exit 2 ;;
esac
