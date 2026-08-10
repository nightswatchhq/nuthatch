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
# The ceilings were chosen by breaking the code on purpose and seeing what the numbers did, rather
# than by leaving "plenty of headroom" - see the justification in ci.yml. Short version, all measured
# on the `ubuntu-latest` runner that enforces this: a linear scan in place of the B-tree seek moves
# p50 from 0.59-0.82µs to 18.15µs, so p50 gates at 8µs - 9.8x above the worst baseline and 2.3x below
# a full scan. p99 over 256 samples is preemption-dominated on a shared runner, so it is a loose
# backstop at 150µs and nothing more.
#
# **Running this on your own machine:** 8µs is `ubuntu-latest`'s number, and applying a ceiling
# measured on one machine to a different one is the mistake this gate already made once. A slower or
# busier box can fail it without anything being wrong, so raise it - `MAX_P50_US=200 bash
# .github/workflows/point-read.sh` reports the numbers without gating on them in any useful sense.
# There is deliberately no "off": this script always passes both ceilings, and `MAX_P50_US=0` would
# fail every run rather than disable the check. What CI enforces is set in ci.yml and is not affected
# by this default.
#
# Env: BIN (default target/release/nuthatch), MAX_P50_US, MAX_P99_US (see ci.yml for the values CI
#      uses and why), PORT (default 8289), RPC_PORT (default 8546), OUT (default
#      point-read-report.json), LABEL (default names the fixture).
set -euo pipefail

BIN="${BIN:-target/release/nuthatch}"
MAX_P50_US="${MAX_P50_US:-8}"
MAX_P99_US="${MAX_P99_US:-150}"
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

# **Point-reads see the unsealed tip, not the whole backfill.** Rows past finality are sealed to
# Parquet and pruned out of redb, so of the 8,004 rows indexed only the last finality window is still
# in the hot store - and `get_entity` is a hot-store read. Measured: sealed_through 19,936 against a
# 20,000 tip, so 64 blocks x 4 logs = 256 rows.
#
# This is written down because the first version of this script asserted `--min-reads 8004` and
# therefore failed *every* run: it was reasoned from the backfill size rather than run. 256 is what
# the fixture actually leaves hot. If nuthatch's finality or sealing changes, this goes red and wants
# a human, which is the correct outcome rather than a silently shrinking sample.
#
# Named rather than left as a bare `64 * 4`, because both numbers are quotations from elsewhere and a
# reader who cannot see the source cannot check them: mainnet is `Finality::Depth(64)`
# (`src/chains.rs:77`) and the mock serves `LOGS_PER_BLOCK = 4` (`footprint-rpc.py:25`) up to a fixed
# `TIP = 20_000`, so the settled hot tip is blocks 19937..=20000.
#
# It is a **floor, not an equality**. A run whose seal loop has not caught up holds *more* than this,
# never fewer, so the check is satisfied at any point after the backfill completes rather than only in
# the settled state. Asserting equality would turn the seal loop's timing into a flaky gate.
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

# `--min-reads` is not belt-and-braces on top of the row check above: an empty store samples no keys
# and reports p50 = 0µs, which passes any ceiling. The floor is what makes a green run mean something.
set +e
"$BIN" bench query --dir "$DIR" --reads "$EXPECT" --iters 5 --out "$OUT" \
  --label "${LABEL:-point-read gate: $EXPECT rows indexed, $HOT_EXPECT hot, locally-served chain}" \
  --min-reads "$HOT_EXPECT" \
  --max-point-read-p50-us "$MAX_P50_US" \
  --max-point-read-p99-us "$MAX_P99_US" | tee "$DIR/bench.log"
status="${PIPESTATUS[0]}"
set -e

p50="$(grep -o '"point_read_p50_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
p99="$(grep -o '"point_read_p99_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
echo "point-read over $rows rows: p50 ${p50}µs (ceiling ${MAX_P50_US}µs), p99 ${p99}µs (ceiling ${MAX_P99_US}µs)"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### point-read latency"
    echo "p50 **${p50}µs** (ceiling ${MAX_P50_US}µs), p99 **${p99}µs** (ceiling ${MAX_P99_US}µs) over $rows rows"
  } >> "$GITHUB_STEP_SUMMARY"
fi

if [ "$status" -ne 0 ]; then
  echo "FAIL: the point-read gate rejected this run - see the message above."
  exit 1
fi
echo "OK: within the point-read ceilings"
