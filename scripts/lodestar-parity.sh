#!/usr/bin/env bash
# Continuous Lodestar parity, at a pinned block (#1076).
#
# An absent comparison must not read as agreement. This script exits 1 when it
# cannot reach the nest, when a view returns no rows, when GRAPH_API_KEY is
# unset so the subgraph side cannot be asked, or when comparable sides disagree.
#
# The first run compared incomparable populations (all-time subgraph totals vs
# Horizon-only nest views) and reported four DIFFs that were not row disagreements.
# This version compares:
#   allocations  nest count vs subgraph allocations where isLegacy: false
#   disputes     nest ids vs subgraph disputes where isLegacy: false
#   epochs       last 30 overlapping ids, field-by-field; start/end block are
#                L2 vs L1 and are reported as INCOMPARABLE, not DIFF
#   escrow       counts, with a type breakdown; ids are different schemes
#
# On disagreement it prints the differing rows, not just a count.
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

# Aggregate nest views do not expose historical versions. The comparison is therefore valid only
# at the nest's current block, not at an arbitrary old pin: filtering a current aggregate by its
# start block does not turn its values into an as-of snapshot.
if [ -z "$BLOCK" ]; then
  BLOCK=$(python3 - "$ready" << 'PY'
import json,sys
d=json.loads(sys.argv[1])
n=d.get("last_block") or 0
if not n:
    raise SystemExit(1)
print(int(n))
PY
) || die "last_block is missing so there is no pin"
  echo "PINNED_BLOCK defaulted to nest last_block=$BLOCK"
fi

NEST_BLOCK=$(python3 - "$ready" << 'PY'
import json,sys
n=json.loads(sys.argv[1]).get("last_block")
if n is None:
    raise SystemExit(1)
print(int(n))
PY
) || die "nest last_block is missing"
[ "$NEST_BLOCK" -eq "$BLOCK" ] || die \
  "PINNED_BLOCK=$BLOCK but nest is at $NEST_BLOCK; current aggregate views have no as-of query"

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

export ALLOC_N EPOCH_N DISPUTE_N ESCROW_N
ALLOC_N=$(nest_count lodestar_allocations created_at_block)
EPOCH_N=$(nest_count lodestar_epochs start_block)
DISPUTE_N=$(nest_count lodestar_disputes created_at_block)
ESCROW_N=$(nest_count lodestar_escrow_transactions block_number)

export ALLOC_N EPOCH_N DISPUTE_N ESCROW_N BLOCK NETWORK_SG GATEWAY GRAPH_API_KEY NEST
python3 - << 'PY' || die "subgraph comparison failed"
import json, os, sys, urllib.parse, urllib.request

key = os.environ["GRAPH_API_KEY"]
block = int(os.environ["BLOCK"])
sg = os.environ["NETWORK_SG"]
gateway = os.environ["GATEWAY"].rstrip("/")
nest = os.environ["NEST"].rstrip("/")
url = f"{gateway}/api/{key}/subgraphs/id/{sg}"
failed = False

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

def nest_sql(q):
    qs = urllib.parse.urlencode({"q": q})
    req = urllib.request.Request(
        f"{nest}/sql?{qs}",
        headers={"User-Agent": "nuthatch-lodestar-parity"},
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            d = json.loads(r.read().decode())
    except urllib.error.URLError as e:
        raise SystemExit("nest /sql failed: %s" % e)
    if d.get("error"):
        raise SystemExit("nest sql error: %s" % d["error"])
    return d.get("rows") or []

def page_ids(entity, extra_where=""):
    out = []
    last = ""
    while True:
        parts = [
            "first: 1000",
            "orderBy: id",
            "orderDirection: asc",
            f"block: {{ number: {block} }}",
        ]
        cond = []
        if extra_where.strip():
            cond.append(extra_where.strip())
        if last:
            cond.append(f'id_gt: "{last}"')
        if cond:
            parts.append("where: { %s }" % ", ".join(cond))
        data = gql("{ %s(%s) { id } }" % (entity, " ".join(parts)))
        rows = data.get(entity) or []
        out.extend(r["id"] for r in rows)
        if len(rows) < 1000:
            return out
        last = rows[-1]["id"]

def page_count(entity, extra_where=""):
    return len(page_ids(entity, extra_where))

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

# --- allocations: Horizon vs Horizon, not all-time allocationCount ---
alloc_sg = page_count("allocations", "isLegacy: false")
alloc_nest = int(os.environ["ALLOC_N"])
status = "OK" if alloc_nest == alloc_sg else "DIFF"
print("lodestar_allocations nest=%s subgraph_isLegacy_false=%s %s" % (alloc_nest, alloc_sg, status))
if status == "DIFF":
    failed = True
    print("  (all-time graphNetwork.allocationCount is a different population and is not compared)")

# --- disputes: id sets at pinned block, legacy excluded ---
# Note: lodestar_disputes indexes Arbitrum One (Horizon) dispute manager events directly.
nest_disputes = {
    r["id"].lower()
    for r in nest_sql("SELECT id FROM lodestar_disputes WHERE created_at_block <= %s" % block)
}
sg_disputes = []
last = ""
while True:
    w = f', where: {{ id_gt: "{last}", isLegacy: false }}' if last else ", where: { isLegacy: false }"
    data = gql(
        "{ disputes(first: 1000, orderBy: id, orderDirection: asc, block: { number: %s }%s) { id type isLegacy } }"
        % (block, w)
    )
    rows = data.get("disputes") or []
    sg_disputes.extend(rows)
    if len(rows) < 1000:
        break
    last = rows[-1]["id"]
sg_live = {d["id"].lower() for d in sg_disputes if not d.get("isLegacy")}
sg_legacy = {d["id"].lower() for d in sg_disputes if d.get("isLegacy")}
# Apply the subgraph's pinned legacy classification to the nest set too. The nest view is event
# derived and carries no `is_legacy` column of its own, so comparing every nest id to a
# legacy-filtered subgraph set would manufacture nest-only differences.
nest_live = nest_disputes - sg_legacy
nest_legacy = nest_disputes & sg_legacy
only_nest = sorted(nest_live - sg_live)
only_sg = sorted(sg_live - nest_disputes)
status = "OK" if not only_nest and not only_sg else "DIFF"
print(
    "lodestar_disputes nest_live=%s nest_legacy_excluded=%s subgraph_live=%s subgraph_legacy_excluded=%s %s"
    % (len(nest_live), len(nest_legacy), len(sg_live), len(sg_legacy), status)
)
if only_nest:
    failed = True
    print("  nest-only: %s" % ", ".join(only_nest[:20]))
if only_sg:
    failed = True
    print("  subgraph-only: %s" % ", ".join(only_sg[:20]))

# --- epochs: overlapping recent ids at pinned block, skip L1-vs-L2 start/end ---
epoch_rows = nest_sql(
    "SELECT id, total_rewards, total_indexer_rewards, total_delegator_rewards, "
    "query_fees_collected, curator_query_fees, signalled_tokens "
    "FROM lodestar_epochs WHERE start_block <= %s ORDER BY id DESC LIMIT 30"
    % block
)
if not epoch_rows:
    raise SystemExit("nest lodestar_epochs returned no rows for field comparison at block %s" % block)
ids = [str(int(r["id"])) for r in epoch_rows]
id_list = ", ".join('"%s"' % i for i in ids)
sg_epochs = {
    str(e["id"]): e
    for e in (
        gql(
            "{ epoches(where: { id_in: [%s] }, block: { number: %s }) { "
            "id totalRewards totalIndexerRewards totalDelegatorRewards "
            "queryFeesCollected curatorQueryFees signalledTokens } }"
            % (id_list, block)
        ).get("epoches")
        or []
    )
}
FIELDS = [
    ("total_rewards", "totalRewards"),
    ("total_indexer_rewards", "totalIndexerRewards"),
    ("total_delegator_rewards", "totalDelegatorRewards"),
    ("query_fees_collected", "queryFeesCollected"),
    ("curator_query_fees", "curatorQueryFees"),
    ("signalled_tokens", "signalledTokens"),
]
epoch_diffs = 0
missing_sg = [i for i in ids if i not in sg_epochs]
if missing_sg:
    failed = True
    epoch_diffs += len(missing_sg)
    print("lodestar_epochs subgraph missing ids: %s" % ", ".join(missing_sg[:20]))
for r in epoch_rows:
    eid = str(int(r["id"]))
    sg = sg_epochs.get(eid)
    if not sg:
        continue
    for nest_k, sg_k in FIELDS:
        nv = str(r.get(nest_k) if r.get(nest_k) is not None else "0")
        sv = str(sg.get(sg_k) if sg.get(sg_k) is not None else "0")
        if nv != sv:
            epoch_diffs += 1
            failed = True
            print("lodestar_epochs id=%s %s nest=%s subgraph=%s DIFF" % (eid, nest_k, nv, sv))
print(
    "lodestar_epochs compared %s overlapping ids, %s field diffs; start/end block L1-vs-L2 INCOMPARABLE"
    % (len(ids) - len(missing_sg), epoch_diffs)
)

# --- escrow: counts only at pinned block; ids are different schemes ---
escrow_sg = page_count("paymentsEscrowTransactions")
escrow_nest = int(os.environ["ESCROW_N"])
status = "OK" if escrow_nest == escrow_sg else "DIFF"
print("lodestar_escrow_transactions nest=%s subgraph=%s %s" % (escrow_nest, escrow_sg, status))
types = nest_sql(
    "SELECT type, count(*) AS n FROM lodestar_escrow_transactions WHERE block_number <= %s GROUP BY type ORDER BY type"
    % block
)
print("  nest types: %s" % ", ".join("%s=%s" % (t["type"], t["n"]) for t in types))
if status == "DIFF":
    failed = True
    print("  ids are not joinable (nest tx_hash-log_index vs subgraph bytes id)")

if failed:
    raise SystemExit("nest and subgraph disagree at block %s" % block)
print("parity OK at block %s" % block)
PY
