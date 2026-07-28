#!/usr/bin/env bash
# Measure peak resident memory of a single-chain index and enforce a ceiling.
#
# Runs the documented scenario - `init` a token, then `dev --backfill 200` - samples peak RSS while it
# indexes, and fails if it exceeds MAX_RSS_MB. The ceiling is set generously (256 MB) above the measured
# ~37 MB so RPC flakiness never causes a false pass, and the job retries once if the first attempt
# indexes nothing.
#
# RPC selection. By default this indexes mainnet USDC over the *free public* endpoints nuthatch ships
# with, which are rate-limited and regularly return nothing - measured at roughly a 50% per-attempt
# failure rate, which is why this job was flapping. Set FOOTPRINT_RPC (a repo secret in CI) to use a
# dedicated endpoint instead, with CHAIN/CONTRACT naming a token on that chain.
#
# The endpoint is NEVER echoed: it is passed straight to the binary, and the indexer's own output
# (which logs endpoint hosts on failure) goes to a log file this script does not print.
#
# Env: BIN (default target/release/nuthatch), MAX_RSS_MB (default 256), PORT (default 8288),
#      FOOTPRINT_RPC (default empty -> public endpoints), CHAIN/CONTRACT (default mainnet USDC).
set -euo pipefail

BIN="${BIN:-target/release/nuthatch}"
MAX_RSS_MB="${MAX_RSS_MB:-256}"
PORT="${PORT:-8288}"
FOOTPRINT_RPC="${FOOTPRINT_RPC:-}"
# Defaults index mainnet USDC over the shipped public endpoints. CI overrides all three together so a
# dedicated endpoint is paired with a token that actually exists on its chain.
CHAIN="${CHAIN:-mainnet}"
CONTRACT="${CONTRACT:-0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48}"

# `--rpc` only when an endpoint was supplied; otherwise the chain's public defaults apply.
RPC_ARGS=()
if [ -n "$FOOTPRINT_RPC" ]; then
  RPC_ARGS=(--rpc "$FOOTPRINT_RPC")
  echo "using the configured footprint RPC (value not logged)" >&2
else
  echo "no FOOTPRINT_RPC set - using free public endpoints, which are rate-limited and flaky" >&2
fi

# Prints "<peak_rss_kb> <entities>" to stdout; all logs go to stderr.
measure() {
  local dir peak=0 rss entities=0 pid
  dir="$(mktemp -d)"
  if ! "$BIN" init "$CONTRACT" --chain "$CHAIN" --dir "$dir" "${RPC_ARGS[@]}" >/dev/null 2>&1; then
    echo "0 0"; return 0
  fi
  # stdout+stderr to a file, deliberately never printed: the indexer logs endpoint hosts on failure.
  "$BIN" dev --dir "$dir" --listen "127.0.0.1:$PORT" --backfill 200 "${RPC_ARGS[@]}" >"$dir/dev.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 40); do
    sleep 1.5
    kill -0 "$pid" 2>/dev/null || break
    rss="$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')"
    if [ -n "$rss" ] && [ "$rss" -gt "$peak" ]; then peak="$rss"; fi
    entities="$(curl -s "127.0.0.1:$PORT/" 2>/dev/null | grep -o '"entities":[0-9]*' | grep -o '[0-9]*' || true)"
    entities="${entities:-0}"
    if [ "$entities" -gt 100 ]; then break; fi
  done
  kill "$pid" 2>/dev/null || true
  echo "$peak $entities"
}

out="$(measure || echo "0 0")"
peak="${out%% *}"; entities="${out##* }"
if [ "${entities:-0}" -lt 1 ]; then
  echo "no transfers indexed (public RPC flaky?); retrying once..." >&2
  out="$(measure || echo "0 0")"
  peak="${out%% *}"; entities="${out##* }"
fi

peak_mb=$(( (${peak:-0} + 1023) / 1024 ))
echo "peak RSS: ${peak_mb} MB over ${entities:-0} transfers (ceiling ${MAX_RSS_MB} MB)"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "### footprint"
    echo "peak RSS **${peak_mb} MB** over ${entities:-0} transfers (ceiling ${MAX_RSS_MB} MB)"
  } >> "$GITHUB_STEP_SUMMARY"
fi

if [ "${entities:-0}" -lt 1 ]; then
  echo "FAIL: indexed 0 transfers after retry - cannot measure; failing rather than false-passing"
  if [ -z "$FOOTPRINT_RPC" ]; then
    echo "hint: no FOOTPRINT_RPC configured, so this ran against free public endpoints - they are"
    echo "      rate-limited and often return nothing. Set the secret to make this deterministic."
  fi
  exit 1
fi
if [ "$peak_mb" -gt "$MAX_RSS_MB" ]; then
  echo "FAIL: peak RSS ${peak_mb} MB exceeds ceiling ${MAX_RSS_MB} MB"
  exit 1
fi
echo "OK: within budget"
