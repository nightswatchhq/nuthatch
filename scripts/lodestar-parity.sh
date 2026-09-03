#!/usr/bin/env bash
# Continuous Lodestar parity, at a pinned block (#1076).
#
# An absent comparison must not read as agreement. This script exits 1 when it
# cannot reach the nest, when a view returns no rows, when GRAPH_API_KEY is
# unset so the subgraph side cannot be asked, or when comparable sides disagree.
#
# Exit status:
#   0  parity CLEAN - every comparison ran and agreed
#   2  parity NOT CLEAN - every gated comparison agreed, known differences remain (#1116, #1114)
#   1  anything else, including a genuine disagreement and any failure to compare
# 0 is the only status that means parity. 2 exists so "agrees" is distinguishable from
# "agrees on the parts we check", which is the distinction the epoch fields cost us.
#
# The first run compared incomparable populations (all-time subgraph totals vs
# Horizon-only nest views) and reported four DIFFs that were not row disagreements.
# This version compares:
#   allocations  nest count vs subgraph allocations where isLegacy: false
#   disputes     nest ids vs subgraph disputes where isLegacy: false
#   epochs       field-by-field, each field from its own measured comparability epoch.
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

# The nest indexes Horizon (`subgraph_service__*`) events only, so early epochs describe a different
# population on each side and comparing them is the category error all-time `allocationCount` vs
# Horizon-only allocations already illustrates. **Each field crosses into comparability at its own
# epoch**, and those are measured constants in the python below rather than one number here - the
# reward trio at 1195, signal at 1105, the two fee fields at 1302. Every boundary asserts both halves
# of its claim: comparable above, and still disagreeing below. A boundary raised until a run goes
# green therefore fails instead.
#
# This is an optional **floor** an operator may set to narrow the window further. It is never a way
# to lower a measured boundary, and it defaults to unset.
EPOCH_PARITY_FROM=${EPOCH_PARITY_FROM:-}
case "${EPOCH_PARITY_FROM:-0}" in
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
# `-ge`, not `-eq`. What the pin needs is that the block is at or below a sealed boundary, so both
# sides describe immutable history. A pin *older* than the current watermark satisfies that more
# strongly, not less - more of it is settled. Demanding equality made every run a race against the
# sealer, which is a poor property for a check billed as continuous.
[ "$NEST_SEALED" -ge "$BLOCK" ] || die "PINNED_BLOCK=$BLOCK is above sealed_through=$NEST_SEALED, so it is not settled history"

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
# **The six fields do not become comparable at the same epoch**, and pretending they did was what
# made #1117 look like a missing-value defect. A single `EPOCH_PARITY_FROM` of 1195 was right for the
# reward trio and wrong for the other three, so the fee fields carried a transition-era tail inside
# the window and it read as a live shortfall of 1.7e23.
#
# Each boundary below is **measured**, not chosen: it is the lowest epoch above which every
# disagreement in that field is an adjacent equal-and-opposite pair, i.e. pure boundary drift with
# nothing lost. #1113's two definitional fixes are deployed, so all three former "known-diff" fields
# now reconcile in exactly that way.
#
# Two classes:
#   gate   directly comparable. **Any** disagreement above the boundary fails the run.
#   drift  comparable, but subject to the observed-boundary problem in #1116: value filed one epoch
#          out. A disagreement above the boundary is tolerated **only if it is half of an adjacent
#          equal-and-opposite pair**. An unpaired one is a new defect and fails. That is the whole
#          difference between a classification and an excuse.
# (nest column, subgraph field, comparable-from epoch, class)
EPOCH_FIELDS = [
    ("total_rewards", "totalRewards", 1195, "gate"),
    ("total_indexer_rewards", "totalIndexerRewards", 1195, "gate"),
    ("total_delegator_rewards", "totalDelegatorRewards", 1195, "gate"),
    ("signalled_tokens", "signalledTokens", 1105, "drift"),
    ("query_fees_collected", "queryFeesCollected", 1302, "drift"),
    ("curator_query_fees", "curatorQueryFees", 1302, "drift"),
]
EPOCH_GATE = [(n, g) for n, g, _, c in EPOCH_FIELDS if c == "gate"]
EPOCH_KNOWN_DIFF = [(n, g) for n, g, _, c in EPOCH_FIELDS if c == "drift"]
# An optional floor an operator can raise, never lower: narrowing the window is a caller's business,
# widening it past a measured boundary is how a green run stops meaning anything.
epoch_floor = int(os.environ.get("EPOCH_PARITY_FROM") or 0)

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
top_closed = max(overlap, key=int)

print(
    "lodestar_epochs nest=%s overlap=%s open_excluded=%s floor=%s"
    % (len(nest_epochs), len(overlap) + 1, open_epoch, epoch_floor or "none")
)


def cancels_into_open_epoch(nest_col, sg_col, top_delta):
    """Is the top closed epoch's residue the other half of a pair that ends in the open epoch?

    Exempting `top_closed` unconditionally was wrong (#1123 review): a genuine ingestion loss at the
    newest closed epoch would have been waved through as an expected edge artefact. The claim is
    testable, so it is tested - the open epoch is excluded from the *comparison* because its totals
    still move, but its delta at this pin is a perfectly good witness for whether the residue
    cancels. Measured at pin 501162197: epoch 1369 is high by 10194076412605471504 and 1370 is low by
    exactly that.
    """
    # **Adjacency first.** A pair is two *neighbouring* epochs, and cancellation alone is not that.
    # If the overlap is missing the real next closed epoch on either side, `top_closed` is older than
    # it looks and a residue there could be cancelled by a non-adjacent open epoch and waved through
    # as drift. The relationship has to hold, not just the arithmetic.
    if int(open_epoch) != int(top_closed) + 1:
        print(
            "    epoch %s is not adjacent to the open epoch %s, so its residue cannot be an edge "
            "artefact - the overlap is missing an epoch between them" % (top_closed, open_epoch)
        )
        return False
    if open_epoch not in sg_epochs:
        return False
    a = str(nest_epochs[open_epoch][nest_col])
    b = str(sg_epochs[open_epoch][sg_col])
    return (int(a) - int(b)) == -top_delta


def pairing_holds(ids, nest_col, sg_col):
    """Is every disagreement in `ids` half of an adjacent equal-and-opposite pair?

    This is the property that defines a `drift` field's boundary, so it is also what decides whether
    that boundary could be lowered.
    """
    bad = deltas(ids, nest_col, sg_col)
    seen = set()
    for e in sorted(bad, key=int):
        if e in seen:
            continue
        nxt = str(int(e) + 1)
        if nxt in bad and bad[e] == -bad[nxt]:
            seen.add(e)
            seen.add(nxt)
        elif e == top_closed and cancels_into_open_epoch(nest_col, sg_col, bad[e]):
            seen.add(e)
        else:
            return False
    return True


def deltas(ids, nest_col, sg_col):
    """Signed nest-minus-subgraph per epoch. Signed, because the sign is the diagnosis: a pair that
    cancels is value in the wrong bucket, a residue that does not is value nobody has."""
    out = {}
    for e in ids:
        a, b = str(nest_epochs[e][nest_col]), str(sg_epochs[e][sg_col])
        if a != b:
            out[e] = int(a) - int(b)
    return out


epoch_known_diff = []
gate_windows = []
for nest_col, sg_col, from_epoch, klass in EPOCH_FIELDS:
    cut = max(from_epoch, epoch_floor)
    window = [e for e in overlap if int(e) >= cut]
    below = [e for e in overlap if int(e) < cut]
    if not window:
        raise SystemExit(
            "%s: boundary %s leaves no epoch to compare (overlap is %s..%s)"
            % (nest_col, cut, overlap[0], overlap[-1])
        )
    bad = deltas(window, nest_col, sg_col)

    if klass == "gate":
        gate_windows.append(len(window))
        status = "OK" if not bad else "DIFF"
        print(
            "  %s %s/%s epochs agree from %s %s"
            % (nest_col, len(window) - len(bad), len(window), cut, status)
        )
        if bad:
            failed = True
            for e in sorted(bad, key=int)[:20]:
                print(
                    "    epoch %s nest=%s subgraph=%s"
                    % (e, nest_epochs[e][nest_col], sg_epochs[e][sg_col])
                )
    else:
        # Pair each disagreement with its neighbour. The topmost closed epoch is exempt: its partner
        # is the open epoch, which is excluded from the comparison, so its residue is an artefact of
        # where the window ends rather than a defect.
        paired, unpaired, edge = set(), [], []
        for e in sorted(bad, key=int):
            if e in paired:
                continue
            nxt = str(int(e) + 1)
            if nxt in bad and bad[e] == -bad[nxt]:
                paired.add(e)
                paired.add(nxt)
            elif e == top_closed and cancels_into_open_epoch(nest_col, sg_col, bad[e]):
                edge.append(e)
            else:
                unpaired.append(e)
        if unpaired:
            failed = True
            print(
                "  %s %s of %s disagreements above %s are UNPAIRED, net %s - not boundary drift"
                % (nest_col, len(unpaired), len(bad), cut, sum(bad[e] for e in unpaired))
            )
            for e in unpaired[:20]:
                print("    epoch %s delta=%s" % (e, bad[e]))
        elif bad:
            epoch_known_diff.append(nest_col)
            # Report the paired sum on its own. It must be exactly zero - that is the property that
            # makes this drift rather than loss - and folding the exempt top epoch into the same
            # number would hide the one figure worth checking behind a residue that is expected.
            paired_sum = sum(bad[e] for e in paired)
            print(
                "  %s %s/%s epochs disagree from %s, %s in adjacent pairs summing to %s KNOWN-DIFF (#1116)"
                % (nest_col, len(bad), len(window), cut, len(paired), paired_sum)
            )
            if paired_sum:
                failed = True
                print(
                    "    PAIRS DO NOT CANCEL: sum is %s, so this is not boundary drift" % paired_sum
                )
            if edge:
                print(
                    "    epoch %s carries %s, cancelled by the open epoch %s - checked, not assumed"
                    % (edge[0], bad[edge[0]], open_epoch)
                )
        elif deltas([e for e in overlap if int(e) >= from_epoch], nest_col, sg_col):
            # Nothing in the *narrowed* window, but the measured window still disagrees. That is a
            # caller narrowing the comparison, which is supported, and says nothing about the
            # classification. Treating it as evidence would turn a legitimate `EPOCH_PARITY_FROM`
            # into a spurious hard failure.
            print(
                "  %s agrees on all %s epochs from %s, though the measured window from %s still "
                "drifts - narrowed by the floor, not reclassified"
                % (nest_col, len(window), cut, from_epoch)
            )
        else:
            # Agrees across the **measured** window too, so the classification really has moved.
            # Not a failure of parity, but the boundary is now stale and must not go unnoticed.
            failed = True
            print(
                "  %s agrees on all %s epochs from %s: #1116 has moved, reclassify it as a gate"
                % (nest_col, len(window), from_epoch)
            )

    # **Every boundary must have teeth, and the claim it makes is precise: this is the *lowest* epoch
    # at which the field's property holds.** So the test is whether the boundary could be lowered by
    # one - if the property still holds with `from_epoch - 1` included, the boundary is excluding
    # comparable data and a clean run above it proves less than it appears to.
    #
    # Not "does anything below agree", which was the first thing tried and is wrong in both
    # directions: an epoch below with no fee activity at all agrees trivially and says nothing, while
    # requiring the whole lower region to disagree would let a boundary sit several epochs too high.
    # Lowering by exactly one tests the definition and nothing else.
    #
    # Tested against `from_epoch`, the **measured** boundary, never against `cut`. An operator raising
    # the floor to narrow the window is doing something this script supports, and testing the raised
    # cut would reject it.
    # The predecessor may be absent - an epoch in which nothing happened leaves no row at all, which
    # the view's header warns about. Skipping the test then would leave a hardcoded constant
    # unproven, and this script's own rule is that a failure to compare is not parity. So step down
    # to the nearest epoch that *is* present, and if there is none the boundary sits at the bottom of
    # the overlap and cannot be shown minimal at all.
    lower = [e for e in overlap if int(e) < from_epoch]
    if not lower:
        failed = True
        print(
            "    BOUNDARY %s starts at %s with no epoch below it in the overlap, so it cannot be "
            "shown to be the lowest comparable one" % (nest_col, from_epoch)
        )
    else:
        step_to = max(lower, key=int)
        extended = [e for e in overlap if int(e) >= int(step_to)]
        still_holds = (
            not deltas(extended, nest_col, sg_col)
            if klass == "gate"
            else pairing_holds(extended, nest_col, sg_col)
        )
        if still_holds:
            failed = True
            print(
                "    BOUNDARY %s still holds with epoch %s included, so %s is not the lowest "
                "comparable epoch and the boundary must come down"
                % (nest_col, step_to, from_epoch)
            )
# start_block/end_block are L2 observations here and L1 epoch boundaries there.
print("  start_block/end_block INCOMPARABLE (nest L2 observed, subgraph L1 EpochManager)")

# The gate's window, reported in the summary. All three gate fields share a boundary.
epoch_gated_n = min(gate_windows) if gate_windows else 0
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
# The nest's type vocabulary is closed here on purpose. `Deposit` and `EscrowCollected` are compared
# above; `Thaw` and `CancelThaw` are the two the subgraph's entity genuinely does not model. Anything
# else is a type nobody has decided about, and excluding it because it is unrecognised is precisely
# how an unchecked population reads as agreement. A new escrow event, or a misclassified row, fails.
NEST_COMPARED_TYPES = {"Deposit", "EscrowCollected"}
NEST_UNMODELLED_TYPES = {"Thaw", "CancelThaw"}
unexpected = [t for t in types if t["type"] not in NEST_COMPARED_TYPES | NEST_UNMODELLED_TYPES]
if unexpected:
    failed = True
    print(
        "    nest escrow types nobody has classified, so they were never compared: %s"
        % ", ".join("%s=%s" % (t["type"], t["n"]) for t in unexpected)
    )
unmodelled = sum(int(t["n"]) for t in types if t["type"] in NEST_UNMODELLED_TYPES)
if unmodelled:
    print(
        "  %s nest rows are Thaw/CancelThaw, which the subgraph does not model, and are not compared"
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
# Ordinary forward progress during the run is not a problem: the pin is still sealed, so every read
# above saw settled history. The boundary going *backwards* is the thing worth catching - a reorg
# past the pin, or a nest that lost sealed state - and `-ge` still catches exactly that. Under `-eq`
# this failed a run in which every comparison had already passed, purely because the nest sealed a
# batch while the escrow join was paging.
[ "$AFTER_SEALED" -ge "$BLOCK" ] || die "sealed boundary went backwards during comparison: $BLOCK -> $AFTER_SEALED"
read -r EPOCH_GATED EPOCH_KNOWN < "$EPOCH_SUMMARY"
echo "  proved: allocation counts, dispute id sets, escrow rows joined by id, and the epoch reward trio over ${EPOCH_GATED} closed epochs"
if [ "$EPOCH_KNOWN" = "-" ]; then
  echo "parity CLEAN at block $BLOCK"
  exit 0
fi
# Every gated comparison agreed, and three epoch fields still do not. Reporting that as OK would be
# the same fault this script was rewritten to remove, one level up: a known absence of proof reading
# as proof. Distinct exit status, so an operator can tell "agrees" from "agrees on what we check".
echo "  NOT proved: ${EPOCH_KNOWN} remain KNOWN-DIFF, see #1116 and #1114"
echo "parity NOT CLEAN at block $BLOCK: gated comparisons agree, known differences outstanding"
exit 2
