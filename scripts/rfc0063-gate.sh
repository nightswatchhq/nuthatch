#!/bin/bash
# RFC-0063 §9.3 gate: run a real address-history nest over vitalik.eth, Ethereum blocks
# 18,000,000-20,000,000, and compare its /api answers with pinned Etherscan answers.
#
#   MAIN_RPC=<url> TRACE_RPC=<url> scripts/rfc0063-gate.sh <nuthatch-binary> <fixtures-dir>
#
# MAIN_RPC serves logs, transactions, receipts and nonces; TRACE_RPC serves trace_filter (it may be
# the same endpoint). Both stay in the environment: they carry keys, and every line this script
# prints has them masked. Fixtures are Etherscan's answers for the same address and range, one JSON
# row per line (vitalik-txlist.jsonl, vitalik-tokentx.jsonl, vitalik-tokennfttx.jsonl,
# vitalik-token1155tx.jsonl); re-pin them with Etherscan's account API over the same range.
# Exits non-zero on any hash or field mismatch.
set -euo pipefail

BIN=${1:?usage: $0 <nuthatch-binary> <fixtures-dir>}
FIX=${2:?usage: $0 <nuthatch-binary> <fixtures-dir>}
: "${MAIN_RPC:?set MAIN_RPC}" "${TRACE_RPC:?set TRACE_RPC}"
ADDR=0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045
FROM=18000000
TO=20000000
PORT=${PORT:-18364}
URL="http://127.0.0.1:$PORT"

mask() { sed -e "s#${MAIN_RPC}#<main-rpc>#g" -e "s#${TRACE_RPC}#<trace-rpc>#g"; }

WORK=$(mktemp -d)
trap 'kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; rm -rf "$WORK"' EXIT
mkdir "$WORK/nest"
cat > "$WORK/nest/nuthatch.toml" <<EOF
[nest]
name = "rfc0063-gate"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://ethereum-rpc.publicnode.com"]

[address_history]
addresses = ["$ADDR"]
start_block = $FROM
end_block = $TO
EOF

if lsof -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "FAIL: port $PORT is already in use" >&2
  exit 1
fi

START=$(date +%s)
"$BIN" dev --dir "$WORK/nest" --listen "127.0.0.1:$PORT" --rpc "$MAIN_RPC" --trace-rpc "$TRACE_RPC" \
  > "$WORK/nest.log" 2>&1 &
PID=$!

page() { # action page offset
  curl -s "$URL/api?module=account&action=$1&address=$ADDR&startblock=$FROM&endblock=$TO&page=$2&offset=$3"
}

complete() { # every discovered action answers a list for the whole range
  local a
  for a in txlist tokentx tokennfttx token1155tx; do
    [ "$(page "$a" 1 1 | jq -r '.status' 2>/dev/null)" = 1 ] || return 1
  done
}

PEAK_RSS=0
until complete; do
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "FAIL: the nest exited during backfill:" >&2
    tail -20 "$WORK/nest.log" | mask >&2
    exit 1
  fi
  rss=$(ps -o rss= -p "$PID" | tr -d ' ')
  [ "${rss:-0}" -gt "$PEAK_RSS" ] && PEAK_RSS=$rss
  if [ $(( $(date +%s) - START )) -gt 7200 ]; then
    echo "FAIL: not covered after two hours" >&2
    tail -20 "$WORK/nest.log" | mask >&2
    exit 1
  fi
  sleep 5
done
ELAPSED=$(( $(date +%s) - START ))
echo "backfill: ${ELAPSED}s wall, peak RSS $(( PEAK_RSS / 1024 )) MB"
echo "idle RSS after backfill: $(( $(ps -o rss= -p "$PID" | tr -d ' ') / 1024 )) MB"

all_rows() { # action file: every page at 10,000 rows, with the generation pinned after page 1
  local a=$1 out=$2 p=1 gen="" body n
  : > "$out"
  while :; do
    body=$(curl -s "$URL/api?module=account&action=$a&address=$ADDR&startblock=$FROM&endblock=$TO&page=$p&offset=10000${gen:+&generation=$gen}")
    [ "$(jq -r .status <<<"$body")" = 1 ] || { echo "FAIL: $a page $p: $(jq -c .result <<<"$body")" >&2; exit 1; }
    gen=$(jq -r .generation <<<"$body")
    n=$(jq '.result | length' <<<"$body")
    jq -c '.result[]' <<<"$body" >> "$out"
    [ "$n" -eq 10000 ] || break
    p=$((p + 1))
  done
}

FAILED=0
for a in txlist tokentx tokennfttx token1155tx; do
  all_rows "$a" "$WORK/$a.jsonl"
  jq -r .hash "$WORK/$a.jsonl" | sort -u > "$WORK/$a.ours"
  jq -r .hash "$FIX/vitalik-$a.jsonl" | sort -u > "$WORK/$a.theirs"
  missing=$(comm -13 "$WORK/$a.ours" "$WORK/$a.theirs")
  extra=$(comm -23 "$WORK/$a.ours" "$WORK/$a.theirs")
  echo "$a: $(wc -l < "$WORK/$a.ours" | tr -d ' ') hashes ours, $(wc -l < "$WORK/$a.theirs" | tr -d ' ') Etherscan's, $(wc -l < "$WORK/$a.jsonl" | tr -d ' ') rows"
  for h in $missing; do echo "  MISSING $a $h"; FAILED=1; done
  for h in $extra; do echo "  EXTRA   $a $h"; FAILED=1; done
done

# txlist field parity on what rotki reads, per hash.
FIELDS='{hash,blockNumber,timeStamp,from,to,value,input,gas,gasPrice,gasUsed,nonce}'
jq -c "$FIELDS" "$WORK/txlist.jsonl" | sort -u > "$WORK/fields.ours"
jq -c "$FIELDS" "$FIX/vitalik-txlist.jsonl" | sort -u > "$WORK/fields.theirs"
while read -r h; do
  ours=$(grep -F "\"$h\"" "$WORK/fields.ours" || true)
  theirs=$(grep -F "\"$h\"" "$WORK/fields.theirs" || true)
  [ -n "$ours" ] && [ -n "$theirs" ] || continue
  if [ "$ours" != "$theirs" ]; then
    echo "  FIELDS  txlist $h"
    diff='to_entries | map(select(.value != $other[.key])) | from_entries'
    echo "    ours:   $(jq -c --argjson other "$theirs" "$diff" <<<"$ours")"
    echo "    theirs: $(jq -c --argjson other "$ours" "$diff" <<<"$theirs")"
    FAILED=1
  fi
done < <(comm -12 "$WORK/txlist.ours" "$WORK/txlist.theirs")

echo "RPC calls:"
curl -s "$URL/metrics" | grep '^nuthatch_address_history_rpc_calls_total' | sed 's/^nuthatch_address_history_rpc_calls_total/  /' | mask

if [ "$FAILED" -ne 0 ]; then
  echo "FAIL: mismatches above"
  exit 1
fi
echo "PASS"
