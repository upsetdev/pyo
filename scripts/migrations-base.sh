#!/bin/sh
# The commit CI checks migration changes against (check-migrations.ts --base).
# Prints nothing for a repository's first commit: there's nothing before it
# for the migrations to keep working.
#
#   PR_BASE         a pull request's base commit
#   BEFORE          what a push replaced (all zeros for a new branch)
#   DEFAULT_BRANCH  the repository's default branch
set -eu

is_commit() { [ -n "$1" ] && git cat-file -e "$1^{commit}" 2>/dev/null; }

if is_commit "${PR_BASE:-}"; then
  echo "$PR_BASE"
  exit
fi
# A plain push: what it replaced.
if is_commit "${BEFORE:-}" && git merge-base --is-ancestor "$BEFORE" HEAD; then
  echo "$BEFORE"
  exit
fi
# A new branch or a force-push: where it left the default branch...
base=""
if [ -n "${DEFAULT_BRANCH:-}" ] && is_commit "origin/$DEFAULT_BRANCH"; then
  base=$(git merge-base "origin/$DEFAULT_BRANCH" HEAD || true)
fi
# ...or, on the default branch itself, the commit before; the first has none.
if [ -z "$base" ] || [ "$base" = "$(git rev-parse HEAD)" ]; then
  base=$(git rev-parse -q --verify 'HEAD^' || true)
fi
echo "$base"
