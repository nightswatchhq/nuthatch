#!/bin/bash
# RFC-0063 gates for slices 3 and 4: run real address-history nests and compare their /api answers
# with pinned Etherscan answers.
#
#   NUTHATCH_RPC=<url> NUTHATCH_TRACE_RPC=<url> scripts/rfc0063-gate.sh <nuthatch-binary> <fixtures-dir>
#
# NUTHATCH_RPC serves logs, transactions, receipts and nonces; NUTHATCH_TRACE_RPC serves
# trace_filter and trace_transaction (it may be the same endpoint). Both carry keys, so they reach
# the nest through its environment, never its arguments, and every line printed here masks them.
#
# <fixtures-dir> holds Etherscan's answers for vitalik.eth over blocks 18,000,000-20,000,000, one
# JSON row per line (vitalik-txlist.jsonl, vitalik-tokentx.jsonl, vitalik-tokennfttx.jsonl,
# vitalik-token1155tx.jsonl, vitalik-txlistinternal.jsonl); re-pin them with Etherscan's account API
# over the same range. The internal-transaction cases are committed in tests/fixtures/rfc0063 and
# re-pinned by scripts/rfc0063-pin-internal.sh.
#
# Phases: (1) vitalik's history, every hash set and the fields rotki reads; (2) each of vitalik's
# internal transactions by txhash; (3) one single-block nest per case in cases.tsv; (4) the beacon
# withdrawals of 0x7a25bd5f286fb722e7578c62e86a675ff0a00b15 over 17,034,870-17,300,000, a body per
# block, so the slow one (<fixtures-dir>/withdrawals-0x7a25-txsBeaconWithdrawal.jsonl); (5) one
# nest per getminedblocks case in mined.tsv. PHASES picks any of 1, 3, 4 and 5 (2 runs within 1).
# Exits non-zero on any mismatch, after listing every one.
set -euo pipefail

BIN=${1:?usage: $0 <nuthatch-binary> <fixtures-dir>}
FIX=${2:?usage: $0 <nuthatch-binary> <fixtures-dir>}
: "${NUTHATCH_RPC:?set NUTHATCH_RPC}" "${NUTHATCH_TRACE_RPC:?set NUTHATCH_TRACE_RPC}"
export NUTHATCH_RPC NUTHATCH_TRACE_RPC
CASES=$(cd "$(dirname "$0")/.." && pwd)/tests/fixtures/rfc0063
VITALIK=0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045
PORT=${PORT:-18364}
URL="http://127.0.0.1:$PORT"

mask() { # literal replacement: a URL is not a safe regular expression
  local line
  while IFS= read -r line; do
    line=${line//"$NUTHATCH_RPC"/<main-rpc>}
    printf '%s\n' "${line//"$NUTHATCH_TRACE_RPC"/<trace-rpc>}"
  done
}

WORK=$(mktemp -d)
PID=""
stop() { [ -n "$PID" ] && { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; }; PID=""; }
trap 'stop; rm -rf "$WORK"' EXIT
FAILED=0

start() { # name address from to: start a nest watching one address over [from, to]
  mkdir -p "$WORK/$1"
  cat > "$WORK/$1/nuthatch.toml" <<EOF
[nest]
name = "rfc0063-gate"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://ethereum-rpc.publicnode.com"]

[address_history]
addresses = ["$2"]
start_block = $3
end_block = $4
EOF
  if lsof -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    echo "FAIL: port $PORT is already in use" >&2
    exit 1
  fi
  "$BIN" dev --dir "$WORK/$1" --listen "127.0.0.1:$PORT" > "$WORK/$1.log" 2>&1 &
  PID=$!
}

api() { curl -s "$URL/api?module=account&$1"; }

wait_covered() { # name address from to actions...: until every action answers the whole range
  local name=$1 address=$2 from=$3 to=$4 a covered started peak=0 rss
  shift 4
  started=$(date +%s)
  while :; do
    covered=1
    for a in "$@"; do
      [ "$(api "action=$a&address=$address&startblock=$from&endblock=$to&page=1&offset=1" | jq -r .status 2>/dev/null)" = 1 ] || covered=0
    done
    [ "$covered" = 1 ] && break
    if ! kill -0 "$PID" 2>/dev/null; then
      echo "FAIL: $name exited during backfill:" >&2
      tail -20 "$WORK/$name.log" | mask >&2
      exit 1
    fi
    rss=$(ps -o rss= -p "$PID" | tr -d ' ')
    [ "${rss:-0}" -gt "$peak" ] && peak=$rss
    if [ $(( $(date +%s) - started )) -gt 7200 ]; then
      echo "FAIL: $name not covered after two hours" >&2
      tail -20 "$WORK/$name.log" | mask >&2
      exit 1
    fi
    sleep 5
  done
  echo "$name: covered in $(( $(date +%s) - started ))s, peak RSS $(( peak / 1024 )) MB"
}

all_rows() { # action address from to file: every page at 10,000 rows, generation pinned after page 1
  local a=$1 address=$2 from=$3 to=$4 out=$5 p=1 gen="" body n
  : > "$out"
  while :; do
    body=$(api "action=$a&address=$address&startblock=$from&endblock=$to&page=$p&offset=10000${gen:+&generation=$gen}")
    [ "$(jq -r .status <<<"$body")" = 1 ] || { echo "FAIL: $a page $p: $(jq -c .result <<<"$body")" >&2; exit 1; }
    gen=$(jq -r .generation <<<"$body")
    n=$(jq '.result | length' <<<"$body")
    jq -c '.result[]' <<<"$body" >> "$out"
    [ "$n" -eq 10000 ] || break
    p=$((p + 1))
  done
}

same_rows() { # label fields ours theirs: the two files hold the same rows, as multisets, on fields
  local label=$1 fields=$2 d rc
  # Projections go to files under `set -e`, so a jq failure stops the gate instead of reading as
  # two empty sides that agree.
  jq -cS "$fields" "$3" | sort > "$WORK/.ours"
  jq -cS "$fields" "$4" | sort > "$WORK/.theirs"
  if [ ! -s "$WORK/.theirs" ]; then
    echo "FAIL: $label: the reference $4 holds no rows" >&2
    exit 1
  fi
  d=$(diff "$WORK/.ours" "$WORK/.theirs") && rc=0 || rc=$?
  case $rc in
    0) ;;
    1)
      echo "  ROWS    $label"
      sed -e 's/^</    ours:  /' -e 's/^>/    theirs:/' <<<"$d" | grep -E '^    (ours|theirs)' | head -20
      FAILED=1
      ;;
    *)
      echo "FAIL: $label: diff could not compare ($rc)" >&2
      exit 1
      ;;
  esac
}

# The internal-transaction fields compared: every one rotki reads, plus Etherscan's failure fields.
INTERNAL='{hash,blockNumber,timeStamp,from,to,value,traceId,gas,gasUsed,type,isError,errCode,contractAddress}'
BYHASH='{parent,blockNumber,timeStamp,from,to,value,gas,gasUsed,type,isError,errCode,contractAddress,traceId}'

PHASES=${PHASES:-1345}
# Phase 2 runs inside phase 1, on its nest, so it cannot be chosen alone.
if ! [[ $PHASES =~ ^[1345]+$ ]]; then
  echo "FAIL: PHASES=$PHASES; choose from 1 (vitalik, with its txhash phase), 3, 4 and 5" >&2
  exit 1
fi
if [[ $PHASES == *1* ]]; then
# Phase 1: vitalik.eth.
FROM=18000000
TO=20000000
ACTIONS="txlist txlistinternal tokentx tokennfttx token1155tx"
start vitalik "$VITALIK" "$FROM" "$TO"
# shellcheck disable=SC2086
wait_covered vitalik "$VITALIK" "$FROM" "$TO" $ACTIONS
echo "idle RSS after backfill: $(( $(ps -o rss= -p "$PID" | tr -d ' ') / 1024 )) MB"

for a in $ACTIONS; do
  all_rows "$a" "$VITALIK" "$FROM" "$TO" "$WORK/$a.jsonl"
  jq -r .hash "$WORK/$a.jsonl" | sort -u > "$WORK/$a.ours"
  jq -r .hash "$FIX/vitalik-$a.jsonl" | sort -u > "$WORK/$a.theirs"
  echo "$a: $(wc -l < "$WORK/$a.ours" | tr -d ' ') hashes ours, $(wc -l < "$WORK/$a.theirs" | tr -d ' ') Etherscan's, $(wc -l < "$WORK/$a.jsonl" | tr -d ' ') rows"
  for h in $(comm -13 "$WORK/$a.ours" "$WORK/$a.theirs"); do echo "  MISSING $a $h"; FAILED=1; done
  for h in $(comm -23 "$WORK/$a.ours" "$WORK/$a.theirs"); do echo "  EXTRA   $a $h"; FAILED=1; done
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

# txlistinternal by address: every field rotki reads, plus Etherscan's failure fields.
same_rows "txlistinternal vitalik by address" "$INTERNAL" "$WORK/txlistinternal.jsonl" "$FIX/vitalik-txlistinternal.jsonl"

# Phase 2: each internal transaction by txhash, against Etherscan's txhash form, and the same frame
# under the same position whichever way it is asked.
: > "$WORK/vitalik-txhash.jsonl"
for h in $(jq -r .hash "$WORK/txlistinternal.jsonl" | sort -u); do
  body=$(api "action=txlistinternal&txhash=$h")
  [ "$(jq -r .status <<<"$body")" = 1 ] || { echo "  TXHASH  $h: $(jq -c .result <<<"$body")"; FAILED=1; continue; }
  jq -c --arg h "$h" '.result[] | {parent: $h} + .' <<<"$body" >> "$WORK/vitalik-txhash.jsonl"
done
same_rows "txlistinternal vitalik by txhash" "$BYHASH" "$WORK/vitalik-txhash.jsonl" "$CASES/vitalik-txhash.jsonl"
while read -r row; do
  h=$(jq -r .hash <<<"$row")
  index=$(jq -r .nuthatchTraceIndex <<<"$row")
  match=$(jq -c --arg h "$h" --arg i "$index" 'select(.parent == $h and .nuthatchTraceIndex == $i) | {from,to,value}' "$WORK/vitalik-txhash.jsonl")
  if [ "$match" != "$(jq -c '{from,to,value}' <<<"$row")" ]; then
    echo "  IDENTITY $h position $index: by address $(jq -c '{from,to,value}' <<<"$row"), by txhash ${match:-nothing}"
    FAILED=1
  fi
done < "$WORK/txlistinternal.jsonl"
echo "internal identity: $(wc -l < "$WORK/txlistinternal.jsonl" | tr -d ' ') rows checked by address and by txhash"

echo "RPC calls (vitalik):"
curl -s "$URL/metrics" | grep '^nuthatch_address_history_rpc_calls_total' | sed 's/^nuthatch_address_history_rpc_calls_total/  /' | mask
stop
fi

if [[ $PHASES == *3* ]]; then
# Phase 3: one single-block nest per case.
while IFS=$'\t' read -r name address block hash _; do
  start "$name" "$address" "$block" "$block"
  wait_covered "$name" "$address" "$block" "$block" txlistinternal
  all_rows txlistinternal "$address" "$block" "$block" "$WORK/$name-address.jsonl"
  same_rows "$name by address" "$INTERNAL" "$WORK/$name-address.jsonl" "$CASES/$name-address.jsonl"
  api "action=txlistinternal&txhash=$hash" | jq -c '.result[]?' > "$WORK/$name-txhash.jsonl"
  same_rows "$name by txhash" "${BYHASH/parent,/}" "$WORK/$name-txhash.jsonl" "$CASES/$name-txhash.jsonl"
  echo "$name: $(wc -l < "$WORK/$name-address.jsonl" | tr -d ' ') by address, $(wc -l < "$WORK/$name-txhash.jsonl" | tr -d ' ') by txhash"
  if [ -z "${COLD_DONE:-}" ]; then
    # A transaction this nest never discovered, so the lookup takes the on-demand trace path.
    COLD=0x90efd2d8bd258966e47252dc2180da14ea6160934043b3750a9918b69fed7147
    api "action=txlistinternal&txhash=$COLD" | jq -c --arg h "$COLD" '.result[]? | {parent: $h} + .' > "$WORK/cold.jsonl"
    jq -c --arg h "$COLD" 'select(.parent == $h)' "$CASES/vitalik-txhash.jsonl" > "$WORK/cold.theirs"
    same_rows "cold txhash $COLD" "$BYHASH" "$WORK/cold.jsonl" "$WORK/cold.theirs"
    echo "cold txhash: $(wc -l < "$WORK/cold.jsonl" | tr -d ' ') rows traced on demand"
    COLD_DONE=1
  fi
  stop
done < <(grep -v '^#' "$CASES/cases.tsv")
fi

if [[ $PHASES == *4* ]]; then
# Phase 4: beacon withdrawals, read from every block body in the range.
W=0x7a25bd5f286fb722e7578c62e86a675ff0a00b15
start withdrawals "$W" 17034870 17300000
wait_covered withdrawals "$W" 17034870 17300000 txsBeaconWithdrawal
all_rows txsBeaconWithdrawal "$W" 17034870 17300000 "$WORK/withdrawals.jsonl"
# The fixture repeats rows where its pinning split a range, as slice 3's did: compare unique rows.
jq -cS . "$FIX/withdrawals-0x7a25-txsBeaconWithdrawal.jsonl" | sort -u > "$WORK/withdrawals.theirs"
jq -r .withdrawalIndex "$WORK/withdrawals.jsonl" | sort -u > "$WORK/wi.ours"
jq -r .withdrawalIndex "$WORK/withdrawals.theirs" | sort -u > "$WORK/wi.theirs"
echo "withdrawals: $(wc -l < "$WORK/wi.ours" | tr -d ' ') indices ours, $(wc -l < "$WORK/wi.theirs" | tr -d ' ') Etherscan's ($(wc -l < "$FIX/withdrawals-0x7a25-txsBeaconWithdrawal.jsonl" | tr -d ' ') fixture rows), $(wc -l < "$WORK/withdrawals.jsonl" | tr -d ' ') rows"
for i in $(comm -13 "$WORK/wi.ours" "$WORK/wi.theirs"); do echo "  MISSING withdrawal $i"; FAILED=1; done
for i in $(comm -23 "$WORK/wi.ours" "$WORK/wi.theirs"); do echo "  EXTRA   withdrawal $i"; FAILED=1; done
same_rows "withdrawals, every field" . "$WORK/withdrawals.jsonl" "$WORK/withdrawals.theirs"
echo "RPC calls (withdrawals):"
curl -s "$URL/metrics" | grep '^nuthatch_address_history_rpc_calls_total' | sed 's/^nuthatch_address_history_rpc_calls_total/  /' | mask
stop
fi

if [[ $PHASES == *5* ]]; then
# Phase 5: produced blocks, one nest per case.
while IFS=$'\t' read -r name address from to _; do
  start "$name" "$address" "$from" "$to"
  wait_covered "$name" "$address" "$from" "$to" getminedblocks
  all_rows getminedblocks "$address" "$from" "$to" "$WORK/$name.jsonl"
  same_rows "$name" '{blockNumber,timeStamp,blockReward}' "$WORK/$name.jsonl" "$CASES/$name.jsonl"
  echo "$name: $(wc -l < "$WORK/$name.jsonl" | tr -d ' ') blocks, reward $(jq -r .blockReward "$WORK/$name.jsonl" | head -1)"
  stop
done < <(grep -v '^#' "$CASES/mined.tsv")
fi

if [ "$FAILED" -ne 0 ]; then
  echo "FAIL: mismatches above"
  exit 1
fi
echo "PASS"
