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
relay_deliveries() { # block: every relay's deliveries for it, one JSON list; fails if any relay does
  local b=$1 r out
  : > "$OUT/.delivered"
  for r in $RELAYS; do
    out=$(curl -sf --retry 3 -m 30 "${r#*=}/relay/v1/data/bidtraces/proposer_payload_delivered?block_number=$b") \
      || { echo "FAIL: relay ${r%%=*} did not answer for block $b" >&2; exit 1; }
    jq -c --arg relay "${r%%=*}" --arg b "$b" '.[] | select(.block_number == $b) | {relay: $relay,
      recipient: (.proposer_fee_recipient | ascii_downcase), value, hash: (.block_hash | ascii_downcase),
      proposer: .proposer_pubkey}' <<<"$out" >> "$OUT/.delivered"
  done
  jq -sc . "$OUT/.delivered"
}
grep -v '^#' "$HERE/tests/fixtures/rfc0063/mev.tsv" | while IFS=$'\t' read -r name address from to _; do
  a=$(tr 'A-F' 'a-f' <<<"$address")
  fetch "action=getminedblocks&address=$address&blocktype=blocks&page=1&offset=1000" \
    | jq -c --argjson f "$from" --argjson t "$to" 'select((.blockNumber | tonumber) >= $f and (.blockNumber | tonumber) <= $t)' > "$OUT/$name.mined"
  fetch "action=txlist&address=$address&startblock=$from&endblock=$to&page=1&offset=1000&sort=asc" \
    | jq -c --arg a "$a" 'select(.to == $a and .value != "0" and .isError == "0")' > "$OUT/$name.incoming"
  : > "$OUT/$name.jsonl"
  for b in $( (jq -r .blockNumber "$OUT/$name.mined"; jq -r .blockNumber "$OUT/$name.incoming") | sort -un); do
    header=$(proxy "module=proxy&action=eth_getBlockByNumber&tag=$(printf '0x%x' "$b")&boolean=false")
    miner=$(jq -r '.miner | ascii_downcase' <<<"$header")
    hash=$(jq -r '.hash | ascii_downcase' <<<"$header")
    last=$(jq -r '.transactions | length - 1' <<<"$header")
    delivered=$(relay_deliveries "$b" | jq -c --arg h "$hash" 'map(select(.hash == $h))')
    mined=$(jq -c --arg b "$b" 'select(.blockNumber == $b)' "$OUT/$name.mined")
    payment=$(jq -c --arg b "$b" --arg m "$miner" --arg a "$a" --arg last "$last" \
      'select(.blockNumber == $b and ((.from == $m and $m != $a) or .transactionIndex == $last))' "$OUT/$name.incoming" | tail -1)
    [ -n "$mined" ] || [ -n "$payment" ] || continue
    mev=$(jq -r 'if length == 0 then "none" elif (map([.recipient, .value]) | unique | length) > 1 then "conflict" else "relay" end' <<<"$delivered")
    if [ -z "$mined" ] && [ "$mev" != conflict ] && [ "$(jq -r --arg a "$a" 'any(.recipient == $a)' <<<"$delivered")" != true ]; then
      continue # a payment that no relay calls a delivery to this address
    fi
    if [ -n "$mined" ]; then
      ts=$(jq -r .timeStamp <<<"$mined"); fee=$a; reward=$(jq -r .blockReward <<<"$mined")
    else
      ts=$(jq -r .timeStamp <<<"$payment"); fee=$miner
      reward=$(proxy "module=block&action=getblockreward&blockno=$b" | jq -r .blockReward)
    fi
    jq -cn --arg b "$b" --arg h "$hash" --arg ts "$ts" --arg fee "$fee" --arg reward "$reward" \
      --arg mev "$mev" --arg a "$a" --argjson d "$delivered" \
      --arg ptx "$(jq -r '.hash // ""' <<<"${payment:-null}")" --arg pv "$(jq -r '.value // ""' <<<"${payment:-null}")" '
      ([$d[] | select(.recipient == $a)] + $d)[0] as $c | {
        blockNumber: $b, blockHash: $h, timeStamp: $ts, feeRecipient: $fee, blockReward: $reward,
        mev: $mev, mevRecipient: ($c.recipient // ""),
        mevReward: (if $mev == "relay" then $c.value else "" end),
        relays: ($d | map(.relay) | join(",")), proposerPubkey: ($c.proposer // ""),
        paymentTx: $ptx, paymentValue: $pv
      }' >> "$OUT/$name.jsonl"
  done
  rm -f "$OUT/$name.mined" "$OUT/$name.incoming" "$OUT/.delivered"
  echo "$name: $(wc -l < "$OUT/$name.jsonl" | tr -d ' ') produced blocks"
done
