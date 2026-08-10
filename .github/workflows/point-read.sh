#!/usr/bin/env bash
#
# Measure entity point-read p50/p99 and enforce a ceiling (issue #283).
#
# CLAUDE.md names entity point-read p50/p99 as a CI artifact whose regressions fail the build.
# `nuthatch bench query` has measured it for a while; nothing compared the number to anything, so it
# was a print statement rather than a gate and every perf change to the storage path could regress it
# in silence.
#
# The shape is `footprint.sh`'s, for the same two reasons (issue #260): the chain is served locally by
# footprint-rpc.py and the nest is written here rather than fetched, so there is **no secret and no
# third party**. A fork PR can satisfy this check, and every run reads exactly the same 8,004 rows -
# so a change in p99 is a change in nuthatch and not in somebody's rate limiter.
#
# Two orders of magnitude of headroom is deliberate. A point-read is a redb B-tree lookup out of page
# cache and measures single-digit microseconds; the ceiling catches a *structural* regression - a scan
# where there was a seek, a per-read open, a lock in the path - rather than policing the noise of a
# shared CI runner. Tighten it when the numbers across releases say what the spread actually is.
#
# Env: BIN (default target/release/nuthatch), MAX_P50_US (default 200), MAX_P99_US (default 2000),
#      PORT (default 8289), RPC_PORT (default 8546), OUT (default point-read-report.json).
set -euo pipefail

BIN="${BIN:-target/release/nuthatch}"
MAX_P50_US="${MAX_P50_US:-200}"
MAX_P99_US="${MAX_P99_US:-2000}"
PORT="${PORT:-8289}"
RPC_PORT="${RPC_PORT:-8546}"
OUT="${OUT:-point-read-report.json}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

python3 "$HERE/footprint-rpc.py" "$RPC_PORT" &
RPC_PID=$!
trap 'kill "$RPC_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  curl -fsS -m 2 -X POST -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' \
    "127.0.0.1:$RPC_PORT" >/dev/null 2>&1 && break
  sleep 0.25
done

DIR="$(mktemp -d)"
mkdir -p "$DIR/abis"
cat > "$DIR/nuthatch.toml" <<TOML
[nest]
name = "point-read"
chain = "mainnet"
chain_id = 1
rpc_urls = ["http://127.0.0.1:$RPC_PORT"]
schema_version = 1

[[contracts]]
alias = "usdc"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/usdc.json"
TOML
cat > "$DIR/abis/usdc.json" <<'JSON'
[{"type":"event","name":"Transfer","anonymous":false,"inputs":[
  {"name":"from","type":"address","indexed":true},
  {"name":"to","type":"address","indexed":true},
  {"name":"value","type":"uint256","indexed":false}]}]
JSON

# The same fixed workload footprint.sh indexes: 2000 blocks x 4 logs + the tip's 4 = 8004 rows.
BACKFILL_BLOCKS=2000
EXPECT=$(( BACKFILL_BLOCKS * 4 + 4 ))
TIP=20000

# **A point-read is a hot-store read, so its population is the unsealed tip, not the 8004 rows
# indexed.** Found by running this rather than reasoning about it: the first version of this script
# asked for 8004 reads and the gate correctly refused the run - `timed 256 point-read(s)`. Everything
# past finality had sealed to Parquet and been pruned out of redb, exactly as designed.
#
# mainnet is `Finality::Depth(64)` (src/chains.rs), the mock serves 4 logs a block, and its tip is
# fixed at 20000 - so the settled hot tip is blocks 19937..=20000, which is 256 rows.
#
# It is a *floor*, not an equality: a run whose sealing has not caught up holds **more** than this,
# never fewer, so the check is satisfied at every point after the backfill completes rather than only
# in the settled state. Asserting equality would turn the seal loop's timing into a flaky gate.
FINALITY_DEPTH=64
LOGS_PER_BLOCK=4
HOT_EXPECT=$(( FINALITY_DEPTH * LOGS_PER_BLOCK ))

"$BIN" dev --dir "$DIR" --listen "127.0.0.1:$PORT" --backfill "$BACKFILL_BLOCKS" >"$DIR/dev.log" 2>&1 &
DEV_PID=$!
trap 'kill "$DEV_PID" 2>/dev/null || true; kill "$RPC_PID" 2>/dev/null || true' EXIT
for _ in $(seq 1 80); do
  sleep 1
  kill -0 "$DEV_PID" 2>/dev/null || break
  last="$(curl -s "127.0.0.1:$PORT/" 2>/dev/null | grep -o '"last_block":"[0-9]*"' | grep -o '[0-9]*' || true)"
  if [ -n "$last" ] && [ "$last" -ge "$TIP" ]; then break; fi
done
rows="$(curl -s -G "127.0.0.1:$PORT/sql" --data-urlencode \
  'q=SELECT count(*) n FROM usdc__transfer' 2>/dev/null \
  | grep -o '"n":[0-9]*' | grep -o '[0-9]*' || true)"
rows="${rows:-0}"

# `bench query` opens the store directly, so `dev` has to be gone first - not merely asked to go.
kill "$DEV_PID" 2>/dev/null || true
wait "$DEV_PID" 2>/dev/null || true

if [ "$rows" -lt "$EXPECT" ]; then
  tail -30 "$DIR/dev.log" || true
  echo "FAIL: indexed $rows of $EXPECT rows - the run did not complete, so its latencies are not a"
  echo "      measurement. The chain is served locally, so this is a nuthatch or fixture fault"
  echo "      rather than a flaky endpoint; the indexer's log is above."
  exit 1
fi

# `--min-reads` is not belt-and-braces on top of the row check above: the row count is served from
# hot ∪ cold and stays 8004 however much of the store has sealed away, while an empty *hot* store
# samples no keys and reports p50 = 0µs, which passes any ceiling. The floor is the thing that makes
# a green run mean something.
set +e
"$BIN" bench query --dir "$DIR" --reads "$HOT_EXPECT" --iters 5 --out "$OUT" \
  --label "CI point-read gate: $HOT_EXPECT-row hot tip of a $EXPECT-row index, locally-served chain" \
  --min-reads "$HOT_EXPECT" \
  --max-point-read-p50-us "$MAX_P50_US" \
  --max-point-read-p99-us "$MAX_P99_US" | tee "$DIR/bench.log"
status="${PIPESTATUS[0]}"
set -e

p50="$(grep -o '"point_read_p50_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
p99="$(grep -o '"point_read_p99_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
echo "point-read over the ${HOT_EXPECT}-row hot tip of a ${rows}-row index: p50 ${p50}µs (ceiling ${MAX_P50_US}µs), p99 ${p99}µs (ceiling ${MAX_P99_US}µs)"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### point-read latency"
    echo "p50 **${p50}µs** (ceiling ${MAX_P50_US}µs), p99 **${p99}µs** (ceiling ${MAX_P99_US}µs) over the ${HOT_EXPECT}-row hot tip of a ${rows}-row index"
  } >> "$GITHUB_STEP_SUMMARY"
fi

if [ "$status" -ne 0 ]; then
  echo "FAIL: the point-read gate rejected this run - see the message above."
  exit 1
fi
echo "OK: within the point-read ceilings"
