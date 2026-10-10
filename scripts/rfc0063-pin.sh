#!/bin/bash
# Re-pins the Etherscan answers the RFC-0063 gates compare against that live in the tree:
#   ETHERSCAN_KEY=<key> scripts/rfc0063-pin.sh [out-dir]
# - txlistinternal, for each case in tests/fixtures/rfc0063/cases.tsv: the address form over its one
#   block and the txhash form of its transaction; for vitalik.eth, the txhash form of every
#   transaction in vitalik-txlistinternal.jsonl.
# - getminedblocks (blocktype=blocks), for each case in mined.tsv: every block Etherscan lists for
#   the address, kept to the case's range. Etherscan takes no range for this action, so the whole
#   list is paged and the range applied here.
# - txlist, for each case in created.tsv: the contract's rows over the case's range, its creation
#   among them.
# - getblocknobytime, for each case in blocktimes.tsv: see that section.
# - nuthatchProducedBlocks, for each case in mev.tsv: see that section.
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

grep -v '^#' "$HERE/tests/fixtures/rfc0063/created.tsv" | while IFS=$'\t' read -r name address from to _; do
  fetch "action=txlist&address=$address&startblock=$from&endblock=$to&page=1&offset=1000&sort=asc" > "$OUT/$name.jsonl"
  echo "$name: $(wc -l < "$OUT/$name.jsonl" | tr -d ' ') rows"
done

# getblocknobytime, for each case in blocktimes.tsv: at the block's own timestamp and one second
# after it, closest before and after. The timestamp comes from Etherscan's proxy, so every number
# here is Etherscan's.
block_time() { # block: its timestamp, in decimal
  local r
  r=$(printf 'url = "https://api.etherscan.io/v2/api?chainid=1&module=proxy&action=eth_getBlockByNumber&tag=0x%x&boolean=false&apikey=%s"\n' "$1" "$ETHERSCAN_KEY" | curl -s --retry 3 -K -)
  sleep 0.25
  printf '%d\n' "$(jq -r .result.timestamp <<<"$r")"
}
: > "$OUT/blocktimes.jsonl"
grep -v '^#' "$HERE/tests/fixtures/rfc0063/blocktimes.tsv" | while IFS=$'\t' read -r name block; do
  t=$(block_time "$block")
  for at in "$t" "$((t + 1))"; do
    for closest in before after; do
      r=$(printf 'url = "https://api.etherscan.io/v2/api?chainid=1&module=block&action=getblocknobytime&timestamp=%s&closest=%s&apikey=%s"\n' "$at" "$closest" "$ETHERSCAN_KEY" | curl -s --retry 3 -K -)
      sleep 0.25
      [ "$(jq -r .status <<<"$r")" = 1 ] || { echo "FAIL getblocknobytime $at $closest: $(jq -c .result <<<"$r")" >&2; exit 1; }
      jq -cn --arg c "$name" --arg t "$at" --arg cl "$closest" --arg b "$(jq -r .result <<<"$r")" \
        '{case: $c, timestamp: $t, closest: $cl, block: $b}' >> "$OUT/blocktimes.jsonl"
    done
  done
done
echo "blocktimes: $(wc -l < "$OUT/blocktimes.jsonl" | tr -d ' ') answers"

# nuthatchProducedBlocks, for each case in mev.tsv, from sources other than the nest: Etherscan's
# produced blocks and incoming transfers, and every relay's deliveries for each block either names.
# Every incoming transfer is asked about, a wider net than the nest casts, so a block the nest
# misses shows up here.
RELAYS="flashbots=https://boost-relay.flashbots.net ultrasound=https://relay.ultrasound.money bloxroute-regulated=https://bloxroute.regulated.blxrbdn.com agnostic=https://agnostic-relay.net aestus=https://mainnet.aestus.live titan=https://titanrelay.xyz"
proxy() { # query-suffix: the result of an Etherscan proxy call
  local r
  r=$(printf 'url = "https://api.etherscan.io/v2/api?chainid=1&%s&apikey=%s"\n' "$1" "$ETHERSCAN_KEY" | curl -s --retry 3 -K -)
  sleep 0.25
  jq -c .result <<<"$r"
}
grep -v '^#' "$HERE/tests/fixtures/rfc0063/mev.tsv" | while IFS=$'\t' read -r name address from to _; do
  a=$(tr 'A-F' 'a-f' <<<"$address")
  fetch "action=getminedblocks&address=$address&blocktype=blocks&page=1&offset=1000" \
    | jq -c --argjson f "$from" --argjson t "$to" 'select((.blockNumber | tonumber) >= $f and (.blockNumber | tonumber) <= $t)' > "$OUT/$name.mined"
  fetch "action=txlist&address=$address&startblock=$from&endblock=$to&page=1&offset=1000&sort=asc" \
    | jq -c --arg a "$a" 'select(.to == $a and .value != "0" and .isError == "0")' > "$OUT/$name.incoming"
  : > "$OUT/$name.jsonl"
  for b in $( (jq -r .blockNumber "$OUT/$name.mined"; jq -r .blockNumber "$OUT/$name.incoming") | sort -un); do
    delivered=$(for r in $RELAYS; do
      curl -s --retry 3 -m 30 "${r#*=}/relay/v1/data/bidtraces/proposer_payload_delivered?block_number=$b" \
        | jq -c --arg relay "${r%%=*}" --arg b "$b" '.[] | select(.block_number == $b) | {relay: $relay, recipient: (.proposer_fee_recipient | ascii_downcase), value}'
    done | jq -sc .)
    header=$(proxy "module=proxy&action=eth_getBlockByNumber&tag=$(printf '0x%x' "$b")&boolean=false")
    miner=$(jq -r .miner <<<"$header")
    reward=$(proxy "module=block&action=getblockreward&blockno=$b" | jq -r .blockReward)
    mev=$(jq -r 'if length == 0 then "none" elif (map([.recipient, .value]) | unique | length) > 1 then "conflict" else "relay" end' <<<"$delivered")
    if [ "$miner" != "$a" ] && [ "$(jq -r --arg a "$a" 'any(.recipient == $a)' <<<"$delivered")" != true ]; then
      continue # a transfer in that no relay calls a delivery to this address
    fi
    payment=$(jq -c --arg b "$b" 'select(.blockNumber == $b)' "$OUT/$name.incoming" | tail -1)
    jq -cn --arg b "$b" --arg miner "$miner" --arg reward "$reward" --arg mev "$mev" \
      --argjson d "$delivered" --arg pv "$( [ "$miner" = "$a" ] && echo "" || jq -r .value <<<"$payment")" '{
        blockNumber: $b, feeRecipient: $miner, blockReward: $reward, mev: $mev,
        mevRecipient: ($d[0].recipient // ""), mevReward: (if $mev == "relay" then $d[0].value else "" end),
        relays: ($d | map(.relay) | join(",")), paymentValue: $pv
      }' >> "$OUT/$name.jsonl"
  done
  rm -f "$OUT/$name.mined" "$OUT/$name.incoming"
  echo "$name: $(wc -l < "$OUT/$name.jsonl" | tr -d ' ') produced blocks"
done
