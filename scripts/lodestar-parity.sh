#!/usr/bin/env bash
# Continuous Lodestar parity, at a pinned block (#1076).
#
# An absent comparison must not read as agreement. This script exits 1 when it
# cannot reach the nest, when a view returns no rows, when GRAPH_API_KEY is
# unset so the subgraph side cannot be asked, or when comparable sides disagree.
#
# Exit status:
#   0  parity CLEAN - every comparison ran and agreed
#   2  parity NOT CLEAN - every gated comparison agreed, known differences remain (#1113)
#   1  anything else, including a genuine disagreement and any failure to compare
# 0 is the only status that means parity. 2 exists so "agrees" is distinguishable from
# "agrees on the parts we check", which is the distinction the epoch fields cost us.
#
# The first run compared incomparable populations (all-time subgraph totals vs
# Horizon-only nest views) and reported four DIFFs that were not row disagreements.
# This version compares:
#   allocations  nest count vs subgraph allocations where isLegacy: false
#   disputes     nest ids vs subgraph disputes where isLegacy: false
#   epochs       field-by-field over the closed, comparable window (see EPOCH_PARITY_FROM).
#                The reward trio is a hard gate. Three fields measure a different quantity
#                from their subgraph namesakes and are reported KNOWN-DIFF (#1113), never OK.
#                start/end block are L2 vs L1 and are INCOMPARABLE, not DIFF.
#   escrow       counts, with a type breakdown; ids are different schemes
#
# On disagreement it prints the differing rows, not just a count.
set -euo pipefail

NEST=${NEST_URL:-http://127.0.0.1:8105}
BLOCK=${PINNED_BLOCK:-}
NETWORK_SG=DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp
GATEWAY=${GRAPH_GATEWAY:-https://gateway-arbitrum.network.thegraph.com}

# The nest indexes Horizon (`subgraph_service__*`) events only. Below epoch 1195 the network was
# still paying legacy staking rewards the nest never sees, so the two sides describe different
# populations there and a comparison would be the same category error as all-time
# `allocationCount` vs Horizon-only allocations. 1195 is the first epoch at which the reward trio
# agrees to the digit and keeps agreeing. The script asserts BOTH halves of that claim: agreement
# above the boundary, and continued disagreement below it. A window widened until it goes green
# therefore fails instead.
EPOCH_PARITY_FROM=${EPOCH_PARITY_FROM:-1195}
case "$EPOCH_PARITY_FROM" in
  ''|*[!0-9]*) die_early=1 ;;
esac

die() { printf 'FAIL %s\n' "$*" >&2; exit 1; }
[ -z "${die_early:-}" ] || die "EPOCH_PARITY_FROM must be a decimal epoch, got ${EPOCH_PARITY_FROM}"

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

# Aggregate nest views do not expose historical versions or a reorg generation, so the comparison is
# pinned to the sealed boundary.
#
# Epoch aggregates were previously skipped here as having "no as-of surface". That is not true, and
# #1113 records the measurement. `lodestar_epochs` aggregates with no block filter, but every epoch
# except the newest is immutable at a pin: rewards group by the event's own `currentEpoch`, which is
# monotonic in block, and fees and signal bucket by `block_number` inside a window that closes as
# soon as the successor epoch is first observed. An epoch whose successor is already visible at the
# pin cannot change. Only the newest epoch is open, and it is excluded below.
if [ -z "$BLOCK" ]; then
  BLOCK=$(python3 - "$ready" << 'PY'
import json,sys
d=json.loads(sys.argv[1])
n=d.get("sealed_through") or 0
if not n:
    raise SystemExit(1)
print(int(n))
PY
) || die "sealed_through is missing so there is no immutable pin"
  echo "PINNED_BLOCK defaulted to sealed_through=$BLOCK"
fi

read -r _NEST_BLOCK NEST_SEALED <<EOF
$(python3 - "$ready" << 'PY'
import json,sys
d=json.loads(sys.argv[1])
if d.get("last_block") is None or d.get("sealed_through") is None:
    raise SystemExit(1)
print(int(d["last_block"]), int(d["sealed_through"]))
PY
)
EOF
[ -n "${NEST_SEALED:-}" ] || die "nest readiness lacks a sealed boundary"
[ "$NEST_SEALED" -eq "$BLOCK" ] || die "PINNED_BLOCK=$BLOCK but sealed_through=$NEST_SEALED"

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

EPOCH_SUMMARY=$(mktemp)
trap 'rm -f "$EPOCH_SUMMARY"' EXIT
export EPOCH_SUMMARY

if [ -z "${GRAPH_API_KEY:-}" ]; then
  die "GRAPH_API_KEY is unset: the subgraph side was not asked, so this is not a comparison"
fi

export ALLOC_N EPOCH_N DISPUTE_N ESCROW_N
ALLOC_N=$(nest_count lodestar_allocations created_at_block)
EPOCH_N=$(nest_count lodestar_epochs start_block)
DISPUTE_N=$(nest_count lodestar_disputes created_at_block)
ESCROW_N=$(nest_count lodestar_escrow_transactions block_number)

export ALLOC_N EPOCH_N DISPUTE_N ESCROW_N BLOCK NETWORK_SG GATEWAY GRAPH_API_KEY NEST EPOCH_PARITY_FROM
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
    # `/sql` caps a response (50,000 rows when this was written) and flags it. A capped read is a
    # partial population, and comparing a partial population is how an absent comparison reads as
    # agreement. This is not hypothetical: the escrow join in #1114 was first attempted against a
    # silently capped 50,000 of 70,417 rows and produced a confident, entirely fictional answer.
    if d.get("truncated"):
        raise SystemExit(
            "nest /sql truncated the response (%d rows) for: %s" % (len(d.get("rows") or []), q)
        )
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
    # Fetch the complete pinned classification first. Filtering legacy rows here would make
    # `sg_legacy` empty and leave the nest-side exclusion below as rather fine decoration.
    w = f', where: {{ id_gt: "{last}" }}' if last else ""
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

# --- epochs: field-by-field at the pinned block ---
# Two classes of field, and the difference is the whole point of #1113:
#   gate       the reward trio, which is keyed by the event's own `currentEpoch` and is directly
#              comparable. Any disagreement above EPOCH_PARITY_FROM fails the run.
#   known-diff signalled_tokens, query_fees_collected and curator_query_fees, which measure a
#              different quantity from their subgraph namesakes. They are compared and their
#              disagreement is printed, but the count is not silently absorbed: the run cannot
#              report OK on their behalf, and if one of them starts agreeing that is reported too,
#              because it means #1113 moved and this script's classification is stale.
EPOCH_GATE = [
    ("total_rewards", "totalRewards"),
    ("total_indexer_rewards", "totalIndexerRewards"),
    ("total_delegator_rewards", "totalDelegatorRewards"),
]
EPOCH_KNOWN_DIFF = [
    ("signalled_tokens", "signalledTokens"),
    ("query_fees_collected", "queryFeesCollected"),
    ("curator_query_fees", "curatorQueryFees"),
]
epoch_from = int(os.environ["EPOCH_PARITY_FROM"])

nest_epochs = {
    str(r["id"]): r
    for r in nest_sql(
        "SELECT * FROM lodestar_epochs WHERE start_block <= %s" % block
    )
}
if not nest_epochs:
    raise SystemExit("lodestar_epochs returned no rows at block %s" % block)
# EPOCH_N was counted before the subgraph was asked. If the two reads of the same pinned view
# disagree the snapshot moved underneath us and nothing below is a comparison.
if len(nest_epochs) != int(os.environ["EPOCH_N"]):
    raise SystemExit(
        "lodestar_epochs changed between reads at block %s: counted %s, then read %s"
        % (block, os.environ["EPOCH_N"], len(nest_epochs))
    )

# The newest epoch at the pin is still open: its end_block is its own last observation and both it
# and its totals still move. Excluding it is what makes the rest immutable.
open_epoch = max(nest_epochs, key=int)

sg_epochs = {}
ordered = sorted(nest_epochs, key=int)
for i in range(0, len(ordered), 100):
    chunk = ordered[i : i + 100]
    data = gql(
        "{ epoches(first: 1000, where: { id_in: [%s] }, block: { number: %s }) { id %s } }"
        % (
            ",".join('"%s"' % e for e in chunk),
            block,
            " ".join(g for _, g in EPOCH_GATE + EPOCH_KNOWN_DIFF),
        )
    )
    for r in data.get("epoches") or []:
        sg_epochs[str(r["id"])] = r

closed_nest = [e for e in ordered if e != open_epoch]
# The intersection is where an absent comparison would hide. The Graph Network subgraph holds every
# epoch the network has had, and the nest holds a Horizon-era subset, so every closed nest epoch
# must come back. Silently intersecting instead would let the subgraph drop a disagreeing epoch and
# leave the survivors to report parity.
missing = [e for e in closed_nest if e not in sg_epochs]
if missing:
    failed = True
    print(
        "  MISSING %s of %s closed nest epochs absent from the subgraph response at block %s: %s"
        % (len(missing), len(closed_nest), block, ", ".join(missing[:20]))
    )
overlap = [e for e in closed_nest if e in sg_epochs]
if not overlap:
    raise SystemExit(
        "no closed epoch is held by both sides at block %s, so nothing was compared" % block
    )
gated = [e for e in overlap if int(e) >= epoch_from]
below = [e for e in overlap if int(e) < epoch_from]
if not gated:
    raise SystemExit(
        "EPOCH_PARITY_FROM=%s leaves no epoch to compare (overlap is %s..%s)"
        % (epoch_from, overlap[0], overlap[-1])
    )

print(
    "lodestar_epochs nest=%s overlap=%s open_excluded=%s gated=%s (>=%s) below=%s"
    % (len(nest_epochs), len(overlap) + 1, open_epoch, len(gated), epoch_from, len(below))
)

def disagreements(ids, pairs):
    out = {}
    for nest_col, sg_col in pairs:
        bad = [
            (e, str(nest_epochs[e][nest_col]), str(sg_epochs[e][sg_col]))
            for e in ids
            if str(nest_epochs[e][nest_col]) != str(sg_epochs[e][sg_col])
        ]
        out[nest_col] = bad
    return out

gate_bad = disagreements(gated, EPOCH_GATE)
for nest_col, _ in EPOCH_GATE:
    bad = gate_bad[nest_col]
    status = "OK" if not bad else "DIFF"
    print("  %s %s/%s epochs agree %s" % (nest_col, len(gated) - len(bad), len(gated), status))
    if bad:
        failed = True
        for e, a, b in bad[:20]:
            print("    epoch %s nest=%s subgraph=%s" % (e, a, b))

# The boundary must have teeth. If the reward trio also agrees below EPOCH_PARITY_FROM then the
# window is not measuring what its comment claims, and a green run above it proves nothing.
if below:
    below_bad = disagreements(below, EPOCH_GATE)
    if not any(below_bad[c] for c, _ in EPOCH_GATE):
        failed = True
        print(
            "  BOUNDARY epochs below %s now agree too: EPOCH_PARITY_FROM excludes comparable data "
            "and must be lowered" % epoch_from
        )
    else:
        print(
            "  boundary holds: %s of %s epochs below %s still disagree (legacy staking rewards)"
            % (
                max(len(below_bad[c]) for c, _ in EPOCH_GATE),
                len(below),
                epoch_from,
            )
        )

known_bad = disagreements(gated, EPOCH_KNOWN_DIFF)
epoch_known_diff = []
for nest_col, _ in EPOCH_KNOWN_DIFF:
    bad = known_bad[nest_col]
    if bad:
        epoch_known_diff.append(nest_col)
        print(
            "  %s %s/%s epochs disagree KNOWN-DIFF (#1113)"
            % (nest_col, len(bad), len(gated))
        )
        for e, a, b in bad[:3]:
            print("    epoch %s nest=%s subgraph=%s" % (e, a, b))
    else:
        # Not a failure, but the classification above is now wrong and must not go unnoticed.
        failed = True
        print(
            "  %s now agrees on all %s epochs: #1113 has moved, reclassify it as a gate"
            % (nest_col, len(gated))
        )

# start_block/end_block are L2 observations here and L1 epoch boundaries there.
print("  start_block/end_block INCOMPARABLE (nest L2 observed, subgraph L1 EpochManager)")

epoch_gated_n = len(gated)
epoch_known_names = list(epoch_known_diff)

# --- escrow: row-level, at the pinned block ---
# The ids ARE joinable, contrary to what this script used to claim. A
# `paymentsEscrowTransactions.id` is `txHash(32 bytes) || logIndex(uint32 little-endian)`.
#
# The log index is **not on the same base for both types**, which is why the offset is derived rather
# than written down. Measured at a pinned block: deposits match the nest's `log_index` exactly, and
# collections match it plus one, both with zero rows left over on the subgraph side. Two magic numbers
# in the source would be two things to be silently wrong about later, so `derive_offset` recovers each
# one from the data and refuses to proceed unless a single candidate is decisive.
SG_ESCROW_TYPES = {"deposit", "redeem"}

def decode_escrow_id(eid):
    h = eid[2:] if eid.startswith("0x") else eid
    if len(h) != 72:
        raise SystemExit("unexpected escrow id shape %r: expected 36 bytes" % eid)
    return ("0x" + h[:64].lower(), int.from_bytes(bytes.fromhex(h[64:]), "little"))

sg_escrow, last = {}, ""
while True:
    w = ', where: { id_gt: "%s" }' % last if last else ""
    data = gql(
        "{ paymentsEscrowTransactions(first: 1000, orderBy: id, orderDirection: asc,"
        " block: { number: %s }%s) { id type } }" % (block, w)
    )
    rows = data.get("paymentsEscrowTransactions") or []
    for r in rows:
        sg_escrow[decode_escrow_id(r["id"])] = r["type"]
    if len(rows) < 1000:
        break
    last = rows[-1]["id"]

escrow_nest = int(os.environ["ESCROW_N"])
print(
    "lodestar_escrow_transactions nest=%s subgraph=%s" % (escrow_nest, len(sg_escrow))
)
types = nest_sql(
    "SELECT type, count(*) AS n FROM lodestar_escrow_transactions WHERE block_number <= %s GROUP BY type ORDER BY type"
    % block
)
print("  nest types: %s" % ", ".join("%s=%s" % (t["type"], t["n"]) for t in types))

# Only the two types the subgraph entity actually models are comparable. It has no Thaw or
# CancelThaw, so counting those as nest-only differences would manufacture a permanent DIFF out of
# the nest indexing more than the subgraph does.
# 70,000+ rows is well past the node's 50,000-row result cap, so this pages by block. The count is
# taken first and asserted against the assembled set: a page boundary that dropped or double-counted
# rows would otherwise look exactly like a parity difference, and get reported as one.
self_collected = set()
escrow_known_diff = False

def nest_escrow_ids(table):
    """Paged read of one escrow source table, keyed the way the subgraph keys it.

    70,000+ rows is well past the node's 50,000-row result cap, so this pages by block. The count is
    taken first and asserted against the assembled set: a page boundary that dropped or double-counted
    rows would otherwise look exactly like a parity difference, and get reported as one.
    """
    bounds = nest_sql(
        "SELECT min(block_number) AS lo, max(block_number) AS hi, count(*) AS n"
        " FROM %s WHERE block_number <= %s" % (table, block)
    )
    if not bounds or bounds[0]["n"] is None or int(bounds[0]["n"]) == 0:
        raise SystemExit("%s has no rows at block %s" % (table, block))
    lo, hi, expect = int(bounds[0]["lo"]), int(bounds[0]["hi"]), int(bounds[0]["n"])
    # Half the cap per page, so a dense block range still lands inside it.
    pages = max(1, (expect // 25000) + 1)
    step = ((hi - lo) // pages) + 1
    out, cur = set(), lo
    while cur <= hi:
        for r in nest_sql(
            "SELECT tx_hash, log_index, payer, collector FROM %s"
            " WHERE block_number >= %s AND block_number < %s AND block_number <= %s"
            % (table, cur, cur + step, block)
        ):
            key = (r["tx_hash"].lower(), int(r["log_index"]))
            out.add(key)
            if str(r["payer"]).lower() == str(r["collector"]).lower():
                self_collected.add(key)
        cur += step
    if len(out) != expect:
        raise SystemExit(
            "%s paging assembled %s rows but count(*) says %s: the pages do not cover the range"
            % (table, len(out), expect)
        )
    return out

nest_collected_raw = nest_escrow_ids("escrow__escrow_collected")
def derive_offset(label, nest_keys, sg_keys):
    """Recover the subgraph's log-index base for one escrow type.

    Returns (offset, shifted_nest_keys). Fails rather than guessing: the winning candidate must
    account for at least 99% of the subgraph rows and must beat every other candidate. An encoding
    change therefore stops the run instead of being reported as a large row difference.
    """
    if not sg_keys:
        raise SystemExit("subgraph returned no %s rows at block %s" % (label, block))
    scored = []
    for off in (-2, -1, 0, 1, 2):
        shifted = {(t, l + off) for t, l in nest_keys}
        scored.append((len(shifted & sg_keys), off, shifted))
    scored.sort(key=lambda x: -x[0])
    best, off, shifted = scored[0]
    runner_up = scored[1][0]
    if best < 0.99 * len(sg_keys) or best <= runner_up:
        raise SystemExit(
            "cannot derive the %s log-index base at block %s: %s"
            % (label, block, ", ".join("offset %+d matches %d" % (o, n) for n, o, _ in scored))
        )
    print("  %s log-index base: nest log_index %+d (%d of %d subgraph rows)"
          % (label, off, best, len(sg_keys)))
    return off, shifted

sg_collected = {k for k, t in sg_escrow.items() if t == "redeem"}

# Deposits are a modelled type on both sides and are gated the same way. Fetching them and then
# comparing only collections would let a missing, extra or mistyped deposit pass as parity, which is
# the exact failure this script exists to make impossible.
nest_deposits_raw = nest_escrow_ids("escrow__deposit")
sg_deposits = {k for k, t in sg_escrow.items() if t == "deposit"}
deposit_off, nest_deposits = derive_offset("deposit", nest_deposits_raw, sg_deposits)
dep_only_nest = sorted(nest_deposits - sg_deposits)
dep_only_sg = sorted(sg_deposits - nest_deposits)
dep_status = "OK" if not dep_only_nest and not dep_only_sg else "DIFF"
print(
    "  deposits nest=%s subgraph=%s matched=%s %s"
    % (len(nest_deposits), len(sg_deposits), len(nest_deposits & sg_deposits), dep_status)
)
if dep_only_nest:
    failed = True
    for tx, li in dep_only_nest[:20]:
        print("    nest-only deposit %s log_index=%s" % (tx, li - deposit_off))
if dep_only_sg:
    failed = True
    for tx, li in dep_only_sg[:20]:
        print("    subgraph-only deposit %s log_index=%s" % (tx, li - deposit_off))

# Every subgraph row must be one of the two modelled types. A third would mean the entity changed
# under us and the two type filters above silently stopped covering it.
unknown_types = {t for t in sg_escrow.values() if t not in SG_ESCROW_TYPES}
if unknown_types:
    failed = True
    print("    subgraph has unmodelled escrow types not compared: %s" % ", ".join(sorted(unknown_types)))

collected_off, nest_collected = derive_offset("collected/redeem", nest_collected_raw, sg_collected)
self_collected = {(t, l + collected_off) for t, l in self_collected}
only_nest = sorted(nest_collected - sg_collected)
only_sg = sorted(sg_collected - nest_collected)
status = "OK" if not only_nest and not only_sg else "DIFF"
print(
    "  collected/redeem nest=%s subgraph=%s matched=%s %s"
    % (len(nest_collected), len(sg_collected), len(nest_collected & sg_collected), status)
)
# #1114: the nest-only rows are `EscrowCollected` events where one address collects from itself.
# The subgraph drops them, and the nest is the one that is right. That is a KNOWN-DIFF, but it is
# recorded as a *rule* rather than a list of ids: a nest-only row whose payer differs from its
# collector is not explained by it and is a hard failure. A hardcoded list of nine hashes would
# have absorbed the tenth.
known_self = [k for k in only_nest if k in self_collected]
unexplained = [k for k in only_nest if k not in self_collected]
if known_self:
    escrow_known_diff = True
    print(
        "    %s nest-only rows are self-collections (payer == collector) KNOWN-DIFF (#1114)"
        % len(known_self)
    )
    for tx, li in known_self[:5]:
        print("      %s log_index=%s" % (tx, li - collected_off))
if unexplained:
    failed = True
    print("    %s nest-only rows are NOT self-collections and are unexplained:" % len(unexplained))
    for tx, li in unexplained[:20]:
        print("      %s log_index=%s" % (tx, li - collected_off))
if only_sg:
    failed = True
    for tx, li in only_sg[:20]:
        print("    subgraph-only %s log_index=%s" % (tx, li - collected_off))
unmodelled = sum(int(t["n"]) for t in types if t["type"] not in ("Deposit", "EscrowCollected"))
if unmodelled:
    print(
        "  %s nest rows are types the subgraph does not model (Thaw, CancelThaw) and are not compared"
        % unmodelled
    )

known = list(epoch_known_names)
if escrow_known_diff:
    known.append("escrow_self_collections")
with open(os.environ["EPOCH_SUMMARY"], "w") as fh:
    fh.write("%s %s\n" % (epoch_gated_n, ",".join(known) or "-"))

if failed:
    raise SystemExit("nest and subgraph disagree at block %s" % block)
PY

# `/sql` reads the live aggregate. The pre-read sealed boundary is immutable; this post-read check
# rejects any progress, so the comparison cannot mix that sealed snapshot with later hot state.
ready_after=$(curl -fsS -m10 -A 'nuthatch-lodestar-parity' "$NEST/ready" || true)
[ -n "$ready_after" ] || die "nest at $NEST did not answer /ready after comparison"
read -r _AFTER_BLOCK AFTER_SEALED <<EOF
$(python3 - "$ready_after" << 'PY'
import json,sys
d=json.loads(sys.argv[1])
if not d.get("ready") or d.get("last_block") is None or d.get("sealed_through") is None:
    raise SystemExit(1)
print(int(d["last_block"]), int(d["sealed_through"]))
PY
)
EOF
[ -n "${AFTER_SEALED:-}" ] || die "nest /ready was not ready after comparison"
[ "$AFTER_SEALED" -eq "$BLOCK" ] || die "sealed boundary changed during comparison"
read -r EPOCH_GATED EPOCH_KNOWN < "$EPOCH_SUMMARY"
echo "  proved: allocation counts, dispute id sets, escrow rows joined by id, and the epoch reward trio over ${EPOCH_GATED} closed epochs"
if [ "$EPOCH_KNOWN" = "-" ]; then
  echo "parity CLEAN at block $BLOCK"
  exit 0
fi
# Every gated comparison agreed, and three epoch fields still do not. Reporting that as OK would be
# the same fault this script was rewritten to remove, one level up: a known absence of proof reading
# as proof. Distinct exit status, so an operator can tell "agrees" from "agrees on what we check".
echo "  NOT proved: ${EPOCH_KNOWN} remain KNOWN-DIFF, see #1113 and #1114"
echo "parity NOT CLEAN at block $BLOCK: gated comparisons agree, known differences outstanding"
exit 2
