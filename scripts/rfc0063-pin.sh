#!/bin/bash
# Re-pins the Etherscan answers the RFC-0063 gates compare against that live in the tree:
#   ETHERSCAN_KEY=<key> scripts/rfc0063-pin.sh [out-dir]
# - txlistinternal, for each case in tests/fixtures/rfc0063/cases.tsv: the address form over its one
#   block and the txhash form of its transaction; for vitalik.eth, the txhash form of every
#   transaction in vitalik-txlistinternal.jsonl.
# - getminedblocks (blocktype=blocks), for each case in mined.tsv: every block Etherscan lists for
#   the address, kept to the case's range. Etherscan takes no range for this action, so the whole
#   list is paged and the range applied here.
# The key stays in the environment and reaches curl on its standard input, never its arguments.
set -euo pipefail
: "${ETHERSCAN_KEY:?set ETHERSCAN_KEY}"
HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$HERE/tests/fixtures/rfc0063}
API="https://api.etherscan.io/v2/api?chainid=1&module=account"

fetch() { # query-suffix: one answer's result rows, one JSON object per line
  local r
  r=$(printf 'url = "%s&%s&apikey=%s"\n' "$API" "$1" "$ETHERSCAN_KEY" | curl -s --retry 3 -K -)
  sleep 0.25
  if [ "$(jq -r .status <<<"$r")" != 1 ] && [ "$(jq -r .message <<<"$r")" != "No transactions found" ]; then
    echo "FAIL $1: $(jq -c .result <<<"$r" | head -c 200)" >&2
    exit 1
  fi
  jq -c '.result[]?' <<<"$r"
}

grep -v '^#' "$HERE/tests/fixtures/rfc0063/cases.tsv" | while IFS=$'\t' read -r name address block hash _; do
  fetch "action=txlistinternal&address=$address&startblock=$block&endblock=$block&sort=asc" > "$OUT/$name-address.jsonl"
  fetch "action=txlistinternal&txhash=$hash" > "$OUT/$name-txhash.jsonl"
  echo "$name: $(wc -l < "$OUT/$name-address.jsonl" | tr -d ' ') by address, $(wc -l < "$OUT/$name-txhash.jsonl" | tr -d ' ') by txhash"
done

: > "$OUT/vitalik-txhash.jsonl"
for h in $(jq -r .hash "$OUT/vitalik-txlistinternal.jsonl" | sort -u); do
  fetch "action=txlistinternal&txhash=$h" | jq -c --arg h "$h" '{parent: $h} + .' >> "$OUT/vitalik-txhash.jsonl"
done
echo "vitalik: $(wc -l < "$OUT/vitalik-txhash.jsonl" | tr -d ' ') rows by txhash"

grep -v '^#' "$HERE/tests/fixtures/rfc0063/mined.tsv" | while IFS=$'\t' read -r name address from to _; do
  page=1
  : > "$OUT/$name.all"
  while :; do
    fetch "action=getminedblocks&address=$address&blocktype=blocks&page=$page&offset=1000" > "$OUT/$name.page"
    cat "$OUT/$name.page" >> "$OUT/$name.all"
    [ "$(wc -l < "$OUT/$name.page")" -eq 1000 ] || break
    page=$((page + 1))
  done
  jq -c --argjson f "$from" --argjson t "$to" 'select((.blockNumber | tonumber) >= $f and (.blockNumber | tonumber) <= $t)' "$OUT/$name.all" > "$OUT/$name.jsonl"
  echo "$name: $(wc -l < "$OUT/$name.all" | tr -d ' ') blocks listed, $(wc -l < "$OUT/$name.jsonl" | tr -d ' ') in [$from, $to]"
  rm -f "$OUT/$name.all" "$OUT/$name.page"
done
