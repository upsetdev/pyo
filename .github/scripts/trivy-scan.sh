#!/usr/bin/env bash
# Scans an image, or this checkout's lockfiles, with Trivy for HIGH and
# CRITICAL CVEs that have a fixed version. Writes <out>.json (Trivy's
# report), <out>.sarif (for code scanning) and <out>.md (a table, empty
# when there's nothing), adds the table to the job's summary and sets the
# step outputs `critical` and `high`: how many CVEs of each.
#
# It never fails because of what it finds. The workflow decides.
# .trivyignore lists CVEs checked and found not to apply.
#
#   trivy-scan.sh image <ref> <out>
#   trivy-scan.sh fs <dir> <out>
set -euo pipefail

kind=$1 target=$2 out=$3
case "$kind" in
  image) args=() ;;
  # bun.lock's dev dependencies too: they run in CI and on our machines.
  fs) args=(--include-dev-deps) ;;
  *) echo "trivy-scan.sh: image or fs, not $kind" >&2; exit 2 ;;
esac

trivy "$kind" --scanners vuln --ignore-unfixed --severity HIGH,CRITICAL \
  "${args[@]}" --format json --output "$out.json" "$target"
trivy convert --format sarif --output "$out.sarif" "$out.json"

# One row per CVE and package, CRITICAL first.
jq -r '
  [.Results[]? | .Vulnerabilities[]?]
  | unique_by([.VulnerabilityID, .PkgName, .InstalledVersion])
  | sort_by([(if .Severity == "CRITICAL" then 0 else 1 end), .VulnerabilityID])
  | if length == 0 then empty else
      "| CVE | Severity | Package | Installed | Fixed in |",
      "|---|---|---|---|---|",
      (.[] | "| [\(.VulnerabilityID)](\(.PrimaryURL // "https://osv.dev/vulnerability/\(.VulnerabilityID)")) | \(.Severity) | \(.PkgName) | \(.InstalledVersion) | \(.FixedVersion) |")
    end
' "$out.json" > "$out.md"

count() {
  jq --arg s "$1" '[.Results[]?.Vulnerabilities[]? | select(.Severity == $s) | .VulnerabilityID] | unique | length' "$out.json"
}
critical=$(count CRITICAL)
high=$(count HIGH)

{
  echo "### Trivy: \`$target\`"
  echo
  if [ -s "$out.md" ]; then
    echo "$critical CRITICAL and $high HIGH with a fix."
    echo
    head -n 302 "$out.md"
  else
    echo "No HIGH or CRITICAL CVEs with a fix."
  fi
  echo
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"

if [ -n "${GITHUB_OUTPUT:-}" ]; then
  echo "critical=$critical" >> "$GITHUB_OUTPUT"
  echo "high=$high" >> "$GITHUB_OUTPUT"
fi
echo "$target: $critical CRITICAL, $high HIGH with a fix."
