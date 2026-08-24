#!/usr/bin/env bash
# Compare `.github/required-checks.txt` to the live protection API (#715).
# Without a token this still checks the file contains `reviewed-by signature`.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
file="$root/.github/required-checks.txt"
expected=$(grep -v '^#' "$file" | grep -v '^$' | sort)

if ! echo "$expected" | grep -qx 'reviewed-by signature'; then
  echo "required-checks.txt is missing 'reviewed-by signature'" >&2
  exit 1
fi

if [ -z "${GH_TOKEN:-}${GITHUB_TOKEN:-}" ]; then
  echo "no GH_TOKEN; file has $(echo "$expected" | wc -l | tr -d ' ') contexts including reviewed-by signature"
  exit 0
fi

token="${GH_TOKEN:-$GITHUB_TOKEN}"
live=$(curl -fsS -H "Authorization: Bearer $token" -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/${GITHUB_REPOSITORY:-nightswatchhq/nuthatch}/branches/main/protection" \
  | python3 -c "import json,sys; print('\\n'.join(sorted(json.load(sys.stdin)['required_status_checks']['contexts'])))")

if [ "$expected" != "$live" ]; then
  echo "required-checks.txt disagrees with live protection on main:" >&2
  echo "--- file ---" >&2
  echo "$expected" >&2
  echo "--- live ---" >&2
  echo "$live" >&2
  exit 1
fi
echo "required checks match live protection on main"
