#!/bin/bash
# Re-pins Etherscan's txlistinternal answers that the RFC-0063 slice 4 gate compares against:
#   ETHERSCAN_KEY=<key> scripts/rfc0063-pin-internal.sh [out-dir]
# For each case in tests/fixtures/rfc0063/cases.tsv: the address form over its one block and the
# txhash form of its transaction. For vitalik.eth: the txhash form of every transaction in
# vitalik-txlistinternal.jsonl. The key stays in the environment and is never written out.
set -euo pipefail
: "${ETHERSCAN_KEY:?set ETHERSCAN_KEY}"
HERE=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$HERE/tests/fixtures/rfc0063}
API="https://api.etherscan.io/v2/api?chainid=1&module=account&action=txlistinternal"

get() { # query-suffix file
  local r
  # The key goes to curl on its standard input, so it never appears in a process listing.
  r=$(printf 'url = "%s&%s&apikey=%s"\n' "$API" "$1" "$ETHERSCAN_KEY" | curl -s --retry 3 -K -)
  sleep 0.25
  if [ "$(jq -r .status <<<"$r")" != 1 ] && [ "$(jq -r .message <<<"$r")" != "No transactions found" ]; then
    echo "FAIL $1: $(jq -c .result <<<"$r" | head -c 200)" >&2
    exit 1
  fi
  jq -c '.result[]?' <<<"$r" > "$2"
}

grep -v '^#' "$HERE/tests/fixtures/rfc0063/cases.tsv" | while IFS=$'\t' read -r name address block hash _; do
  get "address=$address&startblock=$block&endblock=$block&sort=asc" "$OUT/$name-address.jsonl"
  get "txhash=$hash" "$OUT/$name-txhash.jsonl"
  echo "$name: $(wc -l < "$OUT/$name-address.jsonl" | tr -d ' ') by address, $(wc -l < "$OUT/$name-txhash.jsonl" | tr -d ' ') by txhash"
done

: > "$OUT/vitalik-txhash.jsonl"
for h in $(jq -r .hash "$OUT/vitalik-txlistinternal.jsonl" | sort -u); do
  get "txhash=$h" /dev/stdout | jq -c --arg h "$h" '{parent: $h} + .' >> "$OUT/vitalik-txhash.jsonl"
done
echo "vitalik: $(wc -l < "$OUT/vitalik-txhash.jsonl" | tr -d ' ') rows by txhash"
