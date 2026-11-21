#!/usr/bin/env bash
# For the weekly Postgres rebuild check (postgres-rebuild.yml): compares
# the published image's HIGH and CRITICAL CVEs that have a fix with a
# fresh build's (trivy-scan.sh's published.json and fresh.json). While the
# fresh build has fewer, one issue per major version, labelled security,
# says a patch release would pick up the fixes. Closed once it doesn't.
# Needs GH_TOKEN and GH_REPO.
#
#   rebuild-issue.sh <major> <published ref> <pinned base> <today's base>
set -euo pipefail
export LC_ALL=C

major=$1 published=$2 pinned=$3 today=$4
marker="<!-- pyo-postgres-rebuild: $major -->"
run="${GITHUB_SERVER_URL:-https://github.com}/${GH_REPO}/actions/runs/${GITHUB_RUN_ID:-0}"

ids() { jq -r '[.Results[]?.Vulnerabilities[]?.VulnerabilityID] | unique | .[]' "$1"; }
before=$(ids published.json)
after=$(ids fresh.json)
nb=$(grep -c . <<<"$before" || true)
na=$(grep -c . <<<"$after" || true)
fixed=$(comm -23 <(echo "$before") <(echo "$after") | sed '/^$/d')

echo "Postgres $major: the published image has $nb HIGH or CRITICAL CVEs with a fix, a fresh build has $na." \
  | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"

issue=$(gh issue list --state open --label security --limit 500 --json number,body \
  | jq -r --arg m "$marker" 'map(select(.body | contains($m))) | .[0].number // empty')

if [ "$na" -ge "$nb" ]; then
  if [ -n "$issue" ]; then
    gh issue close "$issue" --comment "A rebuild on $(date -u +%F) no longer has fewer CVEs than \`$published\` ([run]($run))."
  fi
  exit 0
fi

{
  echo "$marker"
  echo "A fresh build of Pyo's Postgres $major image has $na HIGH or CRITICAL CVEs with a fix. The published \`$published\` has $nb. A patch release would pick up these fixes:"
  echo
  jq -r --argjson fixed "$(jq -Rnc '[inputs | select(length > 0)]' <<<"$fixed")" '
    [.Results[]?.Vulnerabilities[]? | select(.VulnerabilityID as $id | $fixed | index($id))]
    | unique_by([.VulnerabilityID, .PkgName])
    | sort_by([(if .Severity == "CRITICAL" then 0 else 1 end), .VulnerabilityID])
    | "| CVE | Severity | Package | Published has | Fixed in |",
      "|---|---|---|---|---|",
      (.[:250][] | "| [\(.VulnerabilityID)](\(.PrimaryURL // "https://osv.dev/vulnerability/\(.VulnerabilityID)")) | \(.Severity) | \(.PkgName) | \(.InstalledVersion) | \(.FixedVersion) |")
  ' published.json
  echo
  echo "The fresh build is the release's \`deploy/images/postgres.Dockerfile\` with this week's extension packages, on the base image of the same tag as it is today."
  echo
  if [ "$pinned" = "$today" ]; then
    echo "The base is the one the release pins, so the fixes are in the packages the build installs. Cutting a patch release rebuilds the image with them."
  else
    echo "- The release pins \`$pinned\`"
    echo "- Today it's \`$today\`"
    echo
    echo "Bump \`BASE_$major\` in \`deploy/images/postgres.Dockerfile\` to today's digest (Renovate's pull request does it), then cut a patch release."
  fi
  echo
  echo "See RELEASING.md, Security patch release. Checked $(date -u +%F) ([run]($run)). This issue is updated each week and closed when a rebuild no longer fixes more."
} > rebuild.issue.md

title="Postgres $major: a rebuild has $((nb - na)) fewer CVEs"
if [ -n "$issue" ]; then
  gh issue edit "$issue" --title "$title" --body-file rebuild.issue.md
else
  gh issue create --title "$title" --label security --body-file rebuild.issue.md
fi
