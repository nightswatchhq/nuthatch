#!/bin/bash
# RFC-0063 §9.6: rotki's history for one wallet over a bounded range, once through a nuthatch nest and
# once with Etherscan alone, and the two compared on every transaction and decoded event.
#
#   ROTKI=<rotki-checkout> scripts/rfc0063-rotki-e2e.sh <nest-url> <work-dir>
#
# MODE=nest-only (the default) makes the nest Ethereum's only indexer and fails if rotki sends a
# single request to Etherscan; MODE=fallback puts Etherscan behind the nest and fails only if the
# range's account lists reached Etherscan. Rotki ships its own Etherscan key, so "only indexer" is
# the evm_indexers_order setting, and the log is the proof.
#
# ROTKI is rotki on pete/nuthatch-indexer with `uv sync` done. <nest-url> is a running
# rotki-mode nest whose [address_history] watches ADDR over at least blocks 17,040,000-17,300,000
# and whose mirror (or RPC) covers the headers there, since rotki turns the time range into blocks
# with getblocknobytime. The Etherscan run uses rotki's packaged key; no key is configured.
# Each run gets a fresh data directory under <work-dir>; nothing else of rotki's is touched.
set -euo pipefail
: "${ROTKI:?set ROTKI to a rotki checkout}"
NEST=${1:?usage: ROTKI=<rotki-checkout> $0 <nest-url> <work-dir>}
WORK=${2:?usage: ROTKI=<rotki-checkout> $0 <nest-url> <work-dir>}
MODE=${MODE:-nest-only}
ADDR=0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045
FROM_TS=1681408055 # block 17,040,000
TO_TS=1684577915   # block 17,299,999
PID=""
trap '[ -n "$PID" ] && kill "$PID" 2>/dev/null || true' EXIT

call() { # method port path [json]: the response body; fails on an HTTP error
  local out code body=()
  [ -n "${4:-}" ] && body=(-H 'content-type: application/json' --data "$4")
  out=$(curl -s -w '\n%{http_code}' -X "$1" "http://127.0.0.1:$2/api/1/$3" ${body[@]+"${body[@]}"})
  code=${out##*$'\n'}
  out=${out%$'\n'*}
  if [ "$code" -ge 300 ]; then
    echo "FAIL: $1 $3 answered $code: $(head -c 400 <<<"$out")" >&2
    return 1
  fi
  printf '%s\n' "$out"
}

run() { # name port order: rotki's decoded history for ADDR over the range, into <work-dir>/<name>
  local name=$1 port=$2 order=$3 data="$WORK/$1" offset=0 page n started
  rm -rf "$data"
  mkdir -p "$data"
  (cd "$ROTKI" && exec uv run python -m rotkehlchen --data-dir "$data" --rest-api-port "$port" \
    --websockets-api-port $((port + 1)) --loglevel debug --logfile "$data/rotki.log") > "$data/stdout.log" 2>&1 &
  PID=$!
  for _ in $(seq 120); do curl -s "http://127.0.0.1:$port/api/1/ping" >/dev/null && break; sleep 1; done
  call PUT "$port" users '{"name":"e2e","password":"e2e-password","initial_settings":{"submit_usage_analytics":false}}' >/dev/null
  call PUT "$port" settings "{\"settings\":{\"active_modules\":[],\"nuthatch_api_endpoint\":\"$NEST\",\"evm_indexers_order\":{\"ethereum\":$order}}}" >/dev/null
  call PUT "$port" blockchains/eth/accounts "{\"async_query\":true,\"accounts\":[{\"address\":\"$ADDR\"}]}" >/dev/null
  for _ in $(seq 300); do
    call GET "$port" blockchains/eth/accounts | jq -e --arg a "$ADDR" '.result | any(.address == $a)' >/dev/null && break
    sleep 2
  done
  started=$(date +%s)
  call POST "$port" blockchains/transactions "{\"async_query\":false,\"from_timestamp\":$FROM_TS,\"to_timestamp\":$TO_TS,\"accounts\":[{\"address\":\"$ADDR\",\"blockchain\":\"eth\"}]}" >/dev/null
  echo "$name: transactions queried in $(( $(date +%s) - started ))s"
  call POST "$port" blockchains/transactions/decode '{"async_query":false,"chain":"eth"}' | jq -c "{$name: .result}"
  : > "$data/events.jsonl"
  while :; do
    page=$(call POST "$port" history/events "{\"location\":\"ethereum\",\"from_timestamp\":$FROM_TS,\"to_timestamp\":$TO_TS,\"limit\":1000,\"offset\":$offset}")
    # A swap's events come grouped in an array; every other entry is one event.
    jq -c '.result.entries[] | if type == "array" then .[] else . end | (.entry // .) | {tx_ref, sequence_index, event_type, event_subtype, asset, amount, counterparty, location_label}' <<<"$page" >> "$data/events.jsonl"
    # Without premium rotki answers only the newest event groups up to its limit, and paging cannot
    # reach past it, so a capped answer would compare two truncated histories.
    jq -e '.result.entries_limit == -1 or .result.entries_found < .result.entries_limit' <<<"$page" >/dev/null \
      || { echo "FAIL: $name: rotki capped the events at $(jq -c '.result | {entries_found, entries_limit}' <<<"$page")" >&2; exit 1; }
    n=$(jq '(.result.entries // .result.events) | length' <<<"$page")
    [ "$n" -eq 1000 ] || break
    offset=$((offset + 1000))
  done
  jq -r .tx_ref "$data/events.jsonl" | sort -u > "$data/txs"
  echo "$name: $(wc -l < "$data/txs" | tr -d ' ') transactions, $(wc -l < "$data/events.jsonl" | tr -d ' ') events"
  kill "$PID"
  wait "$PID" 2>/dev/null || true
  PID=""
}

case $MODE in
  nest-only) run nuthatch 14242 '["nuthatch"]' ;;
  fallback) run nuthatch 14242 '["nuthatch", "etherscan"]' ;;
  *) echo "FAIL: MODE=$MODE; choose nest-only or fallback" >&2; exit 1 ;;
esac
run etherscan 14342 '["etherscan"]'

FAILED=0
LOG=$(ls "$WORK"/nuthatch/*rotki.log)
asked=$(grep -c "Querying Nuthatch.*'action': '\(txlist\|txlistinternal\|tokentx\)'" "$LOG" || true)
leaked=$(grep "Querying Etherscan.*'action': '\\(txlist\\|txlistinternal\\|tokentx\\)'" "$LOG" | grep -c "'startblock': '17040000'" || true)
echo "nuthatch run: $asked account-list requests to the nest, $leaked to Etherscan"
[ "$asked" -gt 0 ] || { echo "  NEST    the nest was never asked for an account list"; FAILED=1; }
[ "$leaked" -eq 0 ] || { echo "  LEAK    the range's account lists went to Etherscan"; FAILED=1; }
if [ "$MODE" = nest-only ]; then
  etherscan=$(grep -c "Querying Etherscan" "$LOG" || true)
  echo "nuthatch run: $etherscan requests to Etherscan in all"
  [ "$etherscan" -eq 0 ] || { echo "  LEAK    rotki asked Etherscan in nest-only mode"; FAILED=1; }
fi
if ! diff <(cat "$WORK/nuthatch/txs") <(cat "$WORK/etherscan/txs") > "$WORK/txs.diff"; then
  echo "  TXS     the transaction sets differ ($WORK/txs.diff)"; FAILED=1
fi
if ! diff <(jq -cS . "$WORK/nuthatch/events.jsonl" | sort) <(jq -cS . "$WORK/etherscan/events.jsonl" | sort) > "$WORK/events.diff"; then
  echo "  EVENTS  the decoded events differ ($WORK/events.diff)"; FAILED=1
fi
[ -s "$WORK/etherscan/txs" ] || { echo "FAIL: the Etherscan run found nothing to compare"; exit 1; }
[ "$FAILED" -eq 0 ] && echo "PASS" || { echo "FAIL"; exit 1; }
