#!/usr/bin/env bash
#
# Backfill events/sec on a fixed, locally served chain, against a floor (#1723).
#
# `nuthatch bench backfill --seal-direct --concurrency 4` over blocks 1 to 20,000 of the chain
# footprint-rpc.py serves: one contract, four Transfers a block, 80,000 rows. It is RFC-0052 S2's
# scenario (scripts/publish-throughput-gate.sh) without the mirror, so no third party and no secret,
# and a fork PR behaves identically. BATCHES batches of RUNS runs each; the bench reports each batch's
# median, and the figure gated is the median of those.
#
# The fixture is single-threaded Python and serves every log the run decodes, so part of the wall
# clock is the fixture's. A slowdown in nuthatch therefore shows here diluted, never amplified.
#
# Every scenario knob defaults to the enforced scenario (#395). CI sets only the floor, the baseline
# and where the report goes.
#
# Env: BIN (target/release/nuthatch), MIN_EVENTS_PER_SEC (unset: recorded, not gated), BASELINE
#      (unset: not checked; CI sets docs/bench/backfill-throughput.json), OUT
#      (backfill-throughput-report.json), BATCHES (3), RUNS (15), CONCURRENCY (4), RPC_PORT (8547).
set -euo pipefail

BIN="${BIN:-target/release/nuthatch}"
MIN_EVENTS_PER_SEC="${MIN_EVENTS_PER_SEC:-}"
BASELINE="${BASELINE:-}"
OUT="${OUT:-backfill-throughput-report.json}"
BATCHES="${BATCHES:-3}"
RUNS="${RUNS:-15}"
CONCURRENCY="${CONCURRENCY:-4}"
RPC_PORT="${RPC_PORT:-8547}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TO_BLOCK=20000
EXPECT=$(( TO_BLOCK * 4 ))
LABEL="backfill gate: blocks 1-$TO_BLOCK, 4 Transfers a block, seal-direct, concurrency $CONCURRENCY, locally-served chain"
WORK="$(mktemp -d)"

python3 "$HERE/footprint-rpc.py" "$RPC_PORT" &
RPC_PID=$!
trap 'kill "$RPC_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS -m 2 -X POST -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
    "127.0.0.1:$RPC_PORT" >/dev/null 2>&1 && break
  sleep 0.25
done

mkdir -p "$WORK/nest/abis"
cat > "$WORK/nest/nuthatch.toml" <<TOML
[nest]
name = "backfill-gate"
chain = "mainnet"
chain_id = 1
rpc_urls = ["http://127.0.0.1:$RPC_PORT"]
schema_version = 1

[[contracts]]
alias = "usdc"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/usdc.json"
TOML
cat > "$WORK/nest/abis/usdc.json" <<'JSON'
[{"type":"event","name":"Transfer","anonymous":false,"inputs":[
  {"name":"from","type":"address","indexed":true},
  {"name":"to","type":"address","indexed":true},
  {"name":"value","type":"uint256","indexed":false}]}]
JSON

medians=()
for b in $(seq 1 "$BATCHES"); do
  "$BIN" bench backfill --dir "$WORK/nest" --from 1 --to "$TO_BLOCK" --runs "$RUNS" \
    --seal-direct --concurrency "$CONCURRENCY" --label "$LABEL" --out "$WORK/batch-$b.json" \
    > "$WORK/batch-$b.log" 2>&1 || { tail -30 "$WORK/batch-$b.log" >&2; echo "FAIL: batch $b did not finish"; exit 1; }
  events="$(jq -r '.events' "$WORK/batch-$b.json")"
  if [ "$events" != "$EXPECT" ]; then
    tail -30 "$WORK/batch-$b.log" >&2
    echo "FAIL: batch $b decoded $events events, not $EXPECT. A rate over the wrong amount of work measures nothing."
    exit 1
  fi
  m="$(jq -r '.events_per_sec' "$WORK/batch-$b.json")"
  echo "batch $b: median $m ev/s over $RUNS runs, $(jq -r '.peak_rss_mb' "$WORK/batch-$b.json") MB peak RSS"
  medians+=("$m")
done

sorted="$(printf '%s\n' "${medians[@]}" | sort -g)"
median="$(echo "$sorted" | awk '{a[NR]=$1} END {print (NR%2) ? a[(NR+1)/2] : (a[NR/2]+a[NR/2+1])/2}')"
lo="$(echo "$sorted" | head -1)"
hi="$(echo "$sorted" | tail -1)"
spread="$(awk -v lo="$lo" -v hi="$hi" -v m="$median" 'BEGIN {printf "%.1f", (hi-lo)*100/m}')"
jq --argjson median "$median" --argjson batches "$(printf '%s\n' "${medians[@]}" | jq -s .)" \
  '.events_per_sec = $median | .batch_medians = $batches | .runs = (.runs * ($batches | length))' \
  "$WORK/batch-1.json" > "$OUT"

echo "backfill: median ${median} ev/s across $BATCHES batches (batch medians ${lo}-${hi}, spread ${spread}%), floor ${MIN_EVENTS_PER_SEC:-none}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### backfill throughput"
    echo "median **${median} ev/s** across $BATCHES batches of $RUNS (batch medians ${lo}-${hi}, spread ${spread}%), floor ${MIN_EVENTS_PER_SEC:-none}"
  } >> "$GITHUB_STEP_SUMMARY"
fi

status=0
if [ -n "$MIN_EVENTS_PER_SEC" ] && awk -v m="$median" -v f="$MIN_EVENTS_PER_SEC" 'BEGIN {exit !(m < f)}'; then
  echo "FAIL: backfill median ${median} ev/s is under the ${MIN_EVENTS_PER_SEC} ev/s floor."
  echo "      The floor and the runner figures it was set from are in ci.yml beside MIN_EVENTS_PER_SEC."
  status=1
fi

# As point-read.sh (#385, #424): the committed baseline must come from this machine and this scenario.
if [ -n "$BASELINE" ]; then
  if [ ! -f "$BASELINE" ]; then
    echo "FAIL: BASELINE=$BASELINE does not exist; a missing reference is not a pass."
    exit 1
  fi
  for field in hardware label; do
    base="$(jq -r ".$field // \"\"" "$BASELINE")"
    this="$(jq -r ".$field // \"\"" "$OUT")"
    if [ "$base" != "$this" ]; then
      echo "FAIL: $BASELINE records $field '$base', this run measured '$this'."
      echo "      The floor was set from runs on the enforcing runner and scenario. Refresh the baseline"
      echo "      from a green run of this job and re-derive the floor, or say why not. Do not edit the field."
      exit 1
    fi
  done
  echo "OK: baseline $BASELINE matches this machine and this scenario"
fi
exit "$status"
