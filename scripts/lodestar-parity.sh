#!/usr/bin/env bash
# Continuous Lodestar parity, at a pinned block (#1076).
#
# An absent comparison must not read as agreement. This script exits 1 when it
# cannot reach the nest, when a view returns no rows, when GRAPH_API_KEY is
# unset so the subgraph side cannot be asked, or when the two sides disagree.
set -euo pipefail

NEST=${NEST_URL:-http://127.0.0.1:8105}
BLOCK=${PINNED_BLOCK:-}
NETWORK_SG=DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp
GATEWAY=${GRAPH_GATEWAY:-https://gateway-arbitrum.network.thegraph.com}

die() { printf 'FAIL %s\n' "$*" >&2; exit 1; }

if [ -n "$BLOCK" ]; then
  case "$BLOCK" in
    ''|*[!0-9]*) die "PINNED_BLOCK must be a decimal block number, got ${BLOCK}" ;;
  esac
fi

ready=$(curl -fsS -m10 -A 'nuthatch-lodestar-parity' "$NEST/ready" || true)
[ -n "$ready" ] || die "nest at $NEST did not answer /ready"

python3 - "$ready" << 'PY' || die "nest /ready is not ready"
import json,sys
d=json.loads(sys.argv[1])
if not d.get("ready"):
    raise SystemExit(1)
print("ready last_block=%s sealed_through=%s" % (d.get("last_block"), d.get("sealed_through")))
PY

# Default the pin to the nest's sealed watermark so both sides answer the same
# history. A live tip vs a live subgraph is lag, not disagreement.
if [ -z "$BLOCK" ]; then
  BLOCK=$(python3 - "$ready" << 'PY'
import json,sys
d=json.loads(sys.argv[1])
n=d.get("sealed_through") or 0
if not n:
    raise SystemExit(1)
print(int(n))
PY
) || die "sealed_through is missing so there is no pin"
  echo "PINNED_BLOCK defaulted to sealed_through=$BLOCK"
fi

nest_count() {
  local view="$1" col="$2"
  local q="SELECT count(*) AS n FROM $view WHERE $col <= $BLOCK"
  local body
  body=$(curl -fsS -m30 -A 'nuthatch-lodestar-parity' --get "$NEST/sql" --data-urlencode "q=$q" || true)
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
print(int(n))
PY
}

echo "nest lodestar_allocations $(nest_count lodestar_allocations created_at_block)"
echo "nest lodestar_epochs $(nest_count lodestar_epochs start_block)"
echo "nest lodestar_disputes $(nest_count lodestar_disputes created_at_block)"
echo "nest lodestar_escrow_transactions $(nest_count lodestar_escrow_transactions block_number)"

if [ -z "${GRAPH_API_KEY:-}" ]; then
  die "GRAPH_API_KEY is unset: the subgraph side was not asked, so this is not a comparison"
fi

ALLOC_N=$(nest_count lodestar_allocations created_at_block)
EPOCH_N=$(nest_count lodestar_epochs start_block)
DISPUTE_N=$(nest_count lodestar_disputes created_at_block)
ESCROW_N=$(nest_count lodestar_escrow_transactions block_number)

export ALLOC_N EPOCH_N DISPUTE_N ESCROW_N BLOCK NETWORK_SG GATEWAY GRAPH_API_KEY
python3 - << 'PY' || die "subgraph comparison failed"
import json, os, sys, urllib.request

key = os.environ["GRAPH_API_KEY"]
block = int(os.environ["BLOCK"])
sg = os.environ["NETWORK_SG"]
gateway = os.environ["GATEWAY"].rstrip("/")
url = f"{gateway}/api/{key}/subgraphs/id/{sg}"

def gql(query):
    req = urllib.request.Request(
        url,
        data=json.dumps({"query": query}).encode(),
        headers={
            "Content-Type": "application/json",
            "User-Agent": "nuthatch-lodestar-parity",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            body = r.read().decode()
    except urllib.error.HTTPError as e:
        raise SystemExit("subgraph HTTP %s: %s" % (e.code, e.read()[:300].decode(errors="replace")))
    d = json.loads(body)
    if d.get("errors"):
        raise SystemExit("subgraph graphql error: %s" % d["errors"])
    if not d.get("data"):
        raise SystemExit("subgraph returned no data: %s" % body[:300])
    return d["data"]

def page_count(entity, extra_where=""):
    n = 0
    last = ""
    where = extra_where.strip()
    while True:
        parts = ["first: 1000", "orderBy: id", "orderDirection: asc", f"block: {{ number: {block} }}"]
        cond = []
        if where:
            cond.append(where)
        if last:
            cond.append(f'id_gt: "{last}"')
        if cond:
            parts.append("where: { %s }" % ", ".join(cond))
        data = gql("{ %s(%s) { id } }" % (entity, " ".join(parts)))
        rows = data.get(entity) or []
        n += len(rows)
        if len(rows) < 1000:
            return n
        last = rows[-1]["id"]

meta = gql("{ _meta { block { number } } }")
sg_block = (meta.get("_meta") or {}).get("block", {}).get("number")
if sg_block is None:
    raise SystemExit("subgraph _meta.block.number missing, so the pin cannot be checked")
sg_block = int(sg_block)
print("subgraph _meta.block.number=%s pin=%s" % (sg_block, block))
if sg_block < block:
    raise SystemExit(
        "subgraph head %s is below pin %s: a match here would not be a comparison at that block"
        % (sg_block, block)
    )

net = gql("{ graphNetwork(id: \"1\", block: { number: %s }) { allocationCount } }" % block)
alloc_sg = (net.get("graphNetwork") or {}).get("allocationCount")
if alloc_sg is None:
    raise SystemExit("graphNetwork.allocationCount missing at pin %s" % block)
alloc_sg = int(alloc_sg)

epoch_sg = page_count("epoches")
dispute_sg = page_count("disputes")
escrow_sg = page_count("paymentsEscrowTransactions")

pairs = [
    ("lodestar_allocations", int(os.environ["ALLOC_N"]), alloc_sg),
    ("lodestar_epochs", int(os.environ["EPOCH_N"]), epoch_sg),
    ("lodestar_disputes", int(os.environ["DISPUTE_N"]), dispute_sg),
    ("lodestar_escrow_transactions", int(os.environ["ESCROW_N"]), escrow_sg),
]
failed = False
for name, nest, sub in pairs:
    status = "OK" if nest == sub else "DIFF"
    print("%s nest=%s subgraph=%s %s" % (name, nest, sub, status))
    if nest != sub:
        failed = True
if failed:
    raise SystemExit("nest and subgraph disagree at block %s" % block)
print("parity OK at block %s" % block)
PY
