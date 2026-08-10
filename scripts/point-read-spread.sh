#!/usr/bin/env bash
#
# Measure the run-to-run spread of the point-read bench, so the CI ceiling is chosen from data
# rather than guessed (issue #283).
#
# Builds the CI gate's fixture nest ONCE, then runs `nuthatch bench query` against it N times and
# prints one line per run. Indexing is deliberately outside the loop: the question this answers is
# "how much does the *measurement* move on one commit and one machine", which is what decides whether
# a p99 ceiling can be tight or has to be structural. The full-script spread (re-indexing every time)
# is a different and larger number - run `point-read.sh` in a loop for that one.
#
# This is a research tool, not a gate. It is committed because the ceiling in
# .github/workflows/ci.yml has to cite something, and "I ran it a few times" is not a citation.
#
# Env: BIN (required), RUNS (default 15), RPC_PORT (default 8547), PORT (default 8290),
#      BACKFILL_BLOCKS (default 2000).
set -euo pipefail

BIN="${BIN:?set BIN to a nuthatch release binary}"
RUNS="${RUNS:-15}"
RPC_PORT="${RPC_PORT:-8547}"
PORT="${PORT:-8290}"
BACKFILL_BLOCKS="${BACKFILL_BLOCKS:-2000}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MOCK="$HERE/../.github/workflows/footprint-rpc.py"

EXPECT=$(( BACKFILL_BLOCKS * 4 + 4 ))
TIP=20000

python3 "$MOCK" "$RPC_PORT" &
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
name = "point-read-spread"
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

echo "indexing $EXPECT rows into $DIR ..." >&2
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
kill "$DEV_PID" 2>/dev/null || true
wait "$DEV_PID" 2>/dev/null || true

if [ "${rows:-0}" -lt "$EXPECT" ]; then
  tail -30 "$DIR/dev.log" >&2 || true
  echo "FAIL: indexed ${rows:-0} of $EXPECT - not a measurement" >&2
  exit 1
fi
echo "indexed $rows rows; running $RUNS measurements" >&2

echo "run,p50_us,p99_us,p999_us,reads"
for i in $(seq 1 "$RUNS"); do
  out="$DIR/r$i.json"
  "$BIN" bench query --dir "$DIR" --reads "$EXPECT" --iters 1 --out "$out" >/dev/null 2>&1
  p50="$(grep -o '"point_read_p50_us": [0-9.]*' "$out" | grep -o '[0-9.]*$')"
  p99="$(grep -o '"point_read_p99_us": [0-9.]*' "$out" | grep -o '[0-9.]*$')"
  p999="$(grep -o '"point_read_p999_us": [0-9.]*' "$out" | grep -o '[0-9.]*$')"
  reads="$(grep -o '"reads": [0-9]*' "$out" | grep -o '[0-9]*$')"
  echo "$i,$p50,$p99,$p999,$reads"
done
