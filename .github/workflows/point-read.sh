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
# a full scan.
#
# **p99 and p99.9 are recorded and gated on by nothing**, and that is a deliberate design call rather
# than an omission. Gating p50 loosely and tracking the tail precisely are two different jobs: a p99
# ceiling tight enough to catch a real regression flakes on a shared runner, and one loose enough not
# to flake catches nothing. This is not a judgement call here - it was measured. The linear-scan
# mutation that p50 catches at 18.15µs against an 8µs ceiling only moved p99 to 34.45µs, so the 150µs
# p99 backstop this script used to apply did **not** fire on the one regression the gate exists to
# catch. A ceiling that passes the known break is not a weak gate, it is decoration that reads as
# coverage. The tail is the number we track across releases and read with our eyes, in the artifact.
#
# `MAX_P99_US` therefore has no default. Set it to gate the tail deliberately (an operator on a quiet
# machine may well want to); leave it unset and p99 is reported and not enforced.
#
# **Running this on your own machine:** 8µs is `ubuntu-latest`'s number, and applying a ceiling
# measured on one machine to a different one is the mistake this gate already made once. A slower or
# busier box can fail it without anything being wrong, so raise it - `MAX_P50_US=200 bash
# .github/workflows/point-read.sh` reports the numbers without gating on them in any useful sense.
# There is deliberately no "off" for p50: it always applies, and `MAX_P50_US=0` would fail every run
# rather than disable the check. What CI enforces is set in ci.yml and is not affected by this
# default.
#
# **`BASELINE` checks the committed reference came from this machine** (issue #385).
# `docs/bench/point-read.json` is the number anyone re-baselining the ceiling will reach for, and for
# a year it was a 32-core dev-box artifact while the gate ran on a 4-core runner. Nothing detected
# that, because a committed JSON file cannot go stale loudly - it can only be read and believed. So
# CI now points `BASELINE` at it and this script compares the `hardware` the baseline records against
# the `hardware` the run it just did records. Mismatch is a hard failure.
#
# It compares provenance and **not values**, deliberately. A "measured p50 must be within Nx of the
# baseline" check is a second, much tighter ceiling wearing a different hat: the runner's own p50 has
# been seen from 0.58µs to 0.82µs at fixed commits and its p99.9 from 0.77µs to 23.02µs, so any factor
# loose enough not to flake is looser than the 8µs gate already is, and any factor tight enough to
# add information flakes. The one thing a committed baseline can be *definitively* wrong about is
# which machine produced it, so that is what is enforced.
#
# Unset by default, because the answer is machine-dependent and this script is documented for running
# by hand: on your own box the check would fail correctly and uselessly every time. What CI enforces
# is set in ci.yml.
#
# Env: BIN (default target/release/nuthatch), MAX_P50_US (see ci.yml for the value CI uses and why),
#      MAX_P99_US (unset: recorded, not gated), PORT (default 8289), RPC_PORT (default 8546), OUT
#      (default point-read-report.json), LABEL (default names the fixture),
#      BASELINE (unset: not checked; CI sets docs/bench/point-read.json).
set -euo pipefail

BIN="${BIN:-target/release/nuthatch}"
MAX_P50_US="${MAX_P50_US:-8}"
MAX_P99_US="${MAX_P99_US:-}"
BASELINE="${BASELINE:-}"
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
#
# p99 is passed only when asked for. An unset `--max-point-read-p99-us` is not a ceiling of infinity
# dressed up as a number: `check_gate` treats an absent limit as "not asked for" and reports the
# measurement, which is the whole point of recording the tail without enforcing it.
#
# The expansion below is `${p99_arg[@]+"${p99_arg[@]}"}` rather than the obvious `"${p99_arg[@]}"`,
# because this script is `set -u` and bash treats an *empty* array as unset when it expands one.
# Confirmed on `bash:3.2` (3.2.57, what macOS ships as `/bin/bash`): the bare form aborts with
# `arr[@]: unbound variable` before the bench runs. That is the default path now that `MAX_P99_US`
# has no value, and it is the path the header above tells operators to run by hand - so on a Mac the
# documented invocation would have died rather than measured. Bash 4.4+ made the bare form legal,
# which is why CI (ubuntu, bash 5.x) stays green either way and would never have caught this.
p99_arg=()
if [ -n "$MAX_P99_US" ]; then
  p99_arg=(--max-point-read-p99-us "$MAX_P99_US")
fi

set +e
"$BIN" bench query --dir "$DIR" --reads "$EXPECT" --iters 5 --out "$OUT" \
  --label "${LABEL:-point-read gate: $EXPECT rows indexed, $HOT_EXPECT hot, locally-served chain}" \
  --min-reads "$HOT_EXPECT" \
  --max-point-read-p50-us "$MAX_P50_US" \
  ${p99_arg[@]+"${p99_arg[@]}"} | tee "$DIR/bench.log"
status="${PIPESTATUS[0]}"
set -e

p50="$(grep -o '"point_read_p50_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
p99="$(grep -o '"point_read_p99_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
p999="$(grep -o '"point_read_p999_us": [0-9.]*' "$OUT" 2>/dev/null | grep -o '[0-9.]*$' || echo '?')"
# The tail is labelled "tracked" rather than given a fake ceiling, so a reader can see at a glance
# which number failed a build and which one is here to be read across releases.
if [ -n "$MAX_P99_US" ]; then
  p99_note="ceiling ${MAX_P99_US}µs"
else
  p99_note="tracked, not gated"
fi
echo "point-read over $rows rows: p50 ${p50}µs (ceiling ${MAX_P50_US}µs), p99 ${p99}µs ($p99_note), p99.9 ${p999}µs (tracked, not gated)"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### point-read latency"
    echo "p50 **${p50}µs** (ceiling ${MAX_P50_US}µs) over $rows rows"
    echo ""
    echo "Tail, recorded and not gated: p99 **${p99}µs** ($p99_note), p99.9 **${p999}µs**."
  } >> "$GITHUB_STEP_SUMMARY"
fi

# The provenance check runs whatever the ceiling did, and reports separately: a run that is both
# slow *and* baselined against the wrong machine wants to be told both things, and the second is the
# reason to distrust the first.
# `-f "$OUT"` because a run that died before writing its report has no hardware string to compare,
# and `status` is already non-zero on that path - reporting a bogus mismatch would bury the real
# failure under a second one.
if [ -n "$BASELINE" ] && [ -f "$OUT" ]; then
  if [ ! -f "$BASELINE" ]; then
    echo "FAIL: BASELINE=$BASELINE does not exist. The committed reference is what a re-baseline is"
    echo "      derived from; a missing one is not a pass."
    exit 1
  fi
  hw_of() { python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("hardware") or "")' "$1"; }
  base_hw="$(hw_of "$BASELINE")"
  this_hw="$(hw_of "$OUT")"
  if [ "$base_hw" != "$this_hw" ]; then
    echo "FAIL: $BASELINE records hardware '$base_hw', this run measured on '$this_hw'."
    echo "      The committed baseline has to come from the machine that enforces the gate. It did"
    echo "      not once before (#385): a 32-core dev-box artifact sat in docs/bench/point-read.json"
    echo "      while this job ran on a 4-core runner, and baseline and regression do not scale"
    echo "      together across hardware - the linear-scan mutation this gate exists to catch runs"
    echo "      *faster* on the runner (18.15µs) than on the dev box (24.17µs), so a ceiling derived"
    echo "      from the dev box left 1.21x of real margin rather than the 1.61x it claimed."
    echo "      Fix: take point-read-report.json from a green 'point-read latency' run on main and"
    echo "      commit it, or - if the runner spec genuinely changed - re-measure the ceiling here"
    echo "      and say so in docs/benchmarks.md. Do not edit the hardware field."
    exit 1
  fi
  echo "OK: baseline $BASELINE was measured on this machine ('$base_hw')"
fi

if [ "$status" -ne 0 ]; then
  echo "FAIL: the point-read gate rejected this run - see the message above."
  exit 1
fi
echo "OK: within the point-read ceilings"
