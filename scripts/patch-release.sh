#!/usr/bin/env bash
# Prepares a patch release (RELEASING.md, Security patch release): a
# branch from the last release tag, the given commits cherry-picked onto
# it, and the version bumped as a release commit does it
# (chore(release): X.Y.Z). A dry run by default, which only says what it
# would do. --apply does it, in this repo only. Pushing, the pull request
# and the tag stay with you, and it prints them.
#
#   scripts/patch-release.sh [--apply] [--from vX.Y.Z] [<commit>...]
#
# Without commits, the patch release is a rebuild of the tag's code: its
# images pick up the packages their Dockerfiles install as they are today.
set -euo pipefail

apply=false
from=""
commits=()
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) apply=true ;;
    --from) from=${2:?--from takes a tag, like v0.5.0}; shift ;;
    -h | --help) sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "Unknown option: $1" >&2; exit 2 ;;
    *) commits+=("$1") ;;
  esac
  shift
done

cd "$(git rev-parse --show-toplevel)"
git fetch --quiet --tags origin 2>/dev/null || echo "Couldn't fetch tags from origin. Using the local ones." >&2

if [ -z "$from" ]; then
  from=$(git tag --list 'v[0-9]*.[0-9]*.[0-9]*' --sort=-v:refname | head -n 1)
fi
if [ -z "$from" ] || ! git rev-parse -q --verify "refs/tags/$from" >/dev/null; then
  echo "No release tag ${from:-found}." >&2
  exit 1
fi
old=${from#v}
IFS=. read -r major minor patch <<<"$old"
new="$major.$minor.$((patch + 1))"
branch="release/v$new"
if git rev-parse -q --verify "refs/tags/v$new" >/dev/null; then
  echo "v$new is tagged already." >&2
  exit 1
fi

echo "Patch release $new, from $from, on the branch $branch:"
if [ ${#commits[@]} -eq 0 ]; then
  echo "  no commits to cherry-pick: a rebuild of $from's code"
fi
for c in ${commits[@]+"${commits[@]}"}; do
  if ! git rev-parse -q --verify "$c^{commit}" >/dev/null; then
    echo "Not a commit: $c" >&2
    exit 1
  fi
  echo "  cherry-pick $(git log -1 --format='%h %s' "$c")"
done
echo "  version $old to $new: Cargo.toml, each crate's pyo-proto dependency, Cargo.lock, deploy/release-notices.json"
echo "  commit: chore(release): $new"

if ! $apply; then
  echo
  echo "A dry run: nothing changed. Add --apply to do it."
  exit 0
fi

if [ -n "$(git status --porcelain)" ]; then
  echo "The working tree has changes. Commit or stash them first." >&2
  exit 1
fi

git switch --quiet -c "$branch" "$from"
for c in ${commits[@]+"${commits[@]}"}; do
  if ! git cherry-pick -x "$c"; then
    echo "The cherry-pick of $c stopped. Resolve it and run git cherry-pick --continue, then the rest of this script's steps by hand (see RELEASING.md)." >&2
    exit 1
  fi
done

# Like the release commits: the workspace version, each crate's pyo-proto
# dependency and the notices' version.
o=${old//./\\.}
edit() { sed -i.bak "$1" "$2" && rm -f "$2.bak"; }
edit "s/^version = \"$o\"$/version = \"$new\"/" Cargo.toml
for f in crates/*/Cargo.toml; do
  edit "s/\(pyo-proto = { version = \"\)$o\"/\1$new\"/" "$f"
done
edit "s/\"version\": \"$o\"/\"version\": \"$new\"/" deploy/release-notices.json
if ! grep -q "^version = \"$new\"$" Cargo.toml; then
  echo "Cargo.toml's version isn't $old, so it wasn't changed. Check the tag." >&2
  exit 1
fi

if command -v cargo >/dev/null; then
  cargo update --workspace --offline --quiet 2>/dev/null || cargo update --workspace --quiet
else
  echo "cargo isn't on PATH: run cargo update --workspace, then amend the commit with Cargo.lock." >&2
fi

git commit --quiet -am "chore(release): $new"
echo
git log --oneline "$from..HEAD"
cat <<EOF

Done, locally. Check deploy/release-notices.json: keep minFrom, and clear
beforeYouUpdate and manualOnly unless this patch needs them. Then:

  git push -u origin $branch
  gh pr create --base main --head $branch --title "chore(release): $new"

Once CI passes, merge it into main and tag the merge:

  git switch main && git pull
  git tag v$new && git push origin v$new
EOF
