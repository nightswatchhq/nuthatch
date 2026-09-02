#!/usr/bin/env bash
# Continuous Lodestar parity, at a pinned block (#1076).
#
# An absent comparison must not read as agreement. This script exits 1 when it
# cannot reach the nest, when a view returns no rows, or when GRAPH_API_KEY is
# unset so the subgraph side cannot be asked. A skip is a failure.
set -euo pipefail

NEST=${NEST_URL:-http://127.0.0.1:8105}
BLOCK=${PINNED_BLOCK:-}

die() { printf 'FAIL %s\n' "$*" >&2; exit 1; }

if [ -n "$BLOCK" ]; then
  case "$BLOCK" in
    ''|*[!0-9]*) die "PINNED_BLOCK must be a decimal block number, got ${BLOCK}" ;;
  esac
fi

ready=$(curl -fsS -m10 "$NEST/ready" || true)
[ -n "$ready" ] || die "nest at $NEST did not answer /ready"

python3 - "$ready" << 'PY' || die "nest /ready is not ready"
import json,sys
d=json.loads(sys.argv[1])
if not d.get("ready"):
    raise SystemExit(1)
print("ready last_block=%s seal_direct_active=%s" % (d.get("last_block"), d.get("seal_direct_active")))
PY

# Views Lodestar actually queries. Zero rows is a failed comparison, not an empty success.
for view in lodestar_allocations lodestar_epochs lodestar_disputes lodestar_escrow_transactions; do
  q="SELECT count(*) AS n FROM $view"
  if [ -n "$BLOCK" ]; then
    q="SELECT count(*) AS n FROM $view WHERE block_number <= $BLOCK"
  fi
  body=$(curl -fsS -m30 --get "$NEST/sql" --data-urlencode "q=$q" || true)
  [ -n "$body" ] || die "view $view: nest /sql did not answer"
  python3 - "$view" "$body" << 'PY' || die "view ${view} returned no rows"
import json,sys
view, body = sys.argv[1], sys.argv[2]
d=json.loads(body)
if d.get("error"):
    raise SystemExit("sql error: %s" % d["error"])
rows=d.get("rows") or []
if not rows:
    raise SystemExit("no rows")
n=rows[0].get("n") or rows[0].get("count") or 0
if int(n) <= 0:
    raise SystemExit("count is %s" % n)
print("%s count=%s" % (view, n))
PY
done

if [ -z "${GRAPH_API_KEY:-}" ]; then
  die "GRAPH_API_KEY is unset: the subgraph side was not asked, so this is not a comparison"
fi

echo "nest views are populated; subgraph comparison wants a pinned block and is not yet wired"
# The subgraph half is the next increment: same SELECT shape against the gateway at PINNED_BLOCK.
# Shipping a green "compared" without that half would be the absence-as-agreement defect.
exit 1
