#!/usr/bin/env bash
# Compare `.github/required-checks.txt` to the live protection API on `main` (#715).
#
# ## Why this refuses to run blind (#845)
#
# It used to answer a missing token by printing a count and **exiting 0**. That is the same shape as
# the mutation checker in #841: the comparison is the entire point of the script, and its degraded
# mode was indistinguishable from success except by reading stdout. Wired into a workflow without a
# token deliberately granted, it would have produced a permanently green step that compared nothing.
#
# So: no token is a **failure**, and `--offline` is how a caller says it knows and wants the
# file-only check anyway. "I could not compare" must never render as "they match".
#
# The token needs read access to branch protection, which on a GitHub Actions `GITHUB_TOKEN` means
# `permissions: administration: read` - the default token does not carry it. That is why the failure
# text below names the permission rather than just saying "no token".
set -euo pipefail

offline=0
for arg in "$@"; do
  case "$arg" in
    --offline) offline=1 ;;
    *) echo "usage: $0 [--offline]" >&2; exit 2 ;;
  esac
done

root="$(cd "$(dirname "$0")/.." && pwd)"
file="$root/.github/required-checks.txt"
expected=$(grep -v '^#' "$file" | grep -v '^$' | sort)

if ! echo "$expected" | grep -qx 'reviewed-by signature'; then
  echo "required-checks.txt is missing 'reviewed-by signature'" >&2
  exit 1
fi

token="${GH_TOKEN:-${GITHUB_TOKEN:-}}"

if [ "$offline" -eq 1 ]; then
  echo "offline: file lists $(echo "$expected" | wc -l | tr -d ' ') contexts including reviewed-by signature."
  echo "offline: NOT compared against live protection - this is not a drift check."
  exit 0
fi

if [ -z "$token" ]; then
  cat >&2 <<'MSG'
FAIL: no GH_TOKEN/GITHUB_TOKEN, so live branch protection was not read.

This is a failure and not a pass. The whole job of this script is to compare the committed list
against what GitHub actually enforces; without a token it has compared nothing, and reporting
success would mean "the check that watches the checks" is itself scenery.

  - Locally:     GH_TOKEN=$(gh auth token) scripts/check-required-contexts.sh
  - In Actions:  the job needs `permissions: { administration: read }` - reading
                 branches/main/protection is an admin-scoped read and the default GITHUB_TOKEN
                 does not carry it.
  - Deliberately without a token: pass --offline, which checks the file alone and says so.
MSG
  exit 1
fi

live=$(curl -fsS -H "Authorization: Bearer $token" -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/${GITHUB_REPOSITORY:-nightswatchhq/nuthatch}/branches/main/protection" \
  | python3 -c "import json,sys; print('\n'.join(sorted(json.load(sys.stdin)['required_status_checks']['contexts'])))")

if [ -z "$live" ]; then
  echo "FAIL: the protection API returned no contexts. Either protection is off on main, or the" >&2
  echo "      token cannot read it. Both are worth knowing; neither is a pass." >&2
  exit 1
fi

if [ "$expected" != "$live" ]; then
  echo "required-checks.txt disagrees with live protection on main:" >&2
  echo "--- file ---" >&2
  echo "$expected" >&2
  echo "--- live ---" >&2
  echo "$live" >&2
  exit 1
fi
echo "required checks match live protection on main ($(echo "$live" | wc -l | tr -d ' ') contexts)"
