#!/usr/bin/env bash
# stopgap-orphan-alert - posts one Discord message when a bsc or matic deployment joins the orphan
# list (#1942): over 1 GRT of signal, no active allocation, as Lodestar's /subgraphs/migration shows
# it from kittiwake's /api/subgraph-directory. Started by stopgap-orphan-alert.timer.
#
# The first run, and every run before ORPHAN_ALERT_FROM, records the list and posts nothing, so the
# first page is a deployment that joined after the migration began. A deployment that leaves and
# comes back pages again: an indexer dropping one is the case worth knowing about.
#
#   0  ran; any joins were posted (or printed, with ORPHAN_ALERT_DRY_RUN=1)
#   1  could not read a complete list or could not post; the state file is left as it was
#
# Environment (all optional):
#   ORPHAN_ALERT_API          (default https://api.lodestar-dashboard.com)
#   ORPHAN_ALERT_NETWORKS     (default "bsc matic")
#   ORPHAN_ALERT_FROM         ISO date posting starts (default 2026-10-08)
#   ORPHAN_ALERT_STATE_DIR    (default ${XDG_STATE_HOME:-$HOME/.local/state}/stopgap-orphans)
#   ORPHAN_ALERT_WEBHOOK_FILE (default ~/.config/nightswatch/discord-webhook)
#   ORPHAN_ALERT_DRY_RUN      1 prints the message instead of posting it
set -euo pipefail

api=${ORPHAN_ALERT_API:-https://api.lodestar-dashboard.com}
networks=${ORPHAN_ALERT_NETWORKS:-bsc matic}
from=${ORPHAN_ALERT_FROM:-2026-10-08}
state_dir=${ORPHAN_ALERT_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/stopgap-orphans}
hook_file=${ORPHAN_ALERT_WEBHOOK_FILE:-$HOME/.config/nightswatch/discord-webhook}
dry=${ORPHAN_ALERT_DRY_RUN:-0}
page_size=100

say() { echo "stopgap-orphan-alert: $*" >&2; }
command -v jq >/dev/null || { say "jq is not on PATH"; exit 1; }
command -v curl >/dev/null || { say "curl is not on PATH"; exit 1; }
from_s=$(date -u -d "$from" +%s 2>/dev/null || date -u -j -f %Y-%m-%d "$from" +%s 2>/dev/null) \
  || { say "ORPHAN_ALERT_FROM=$from is not a date"; exit 1; }

mkdir -p "$state_dir"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

post() {
  if [ "$dry" = 1 ]; then
    printf '%s\n' "$1"
    return 0
  fi
  local hook
  hook=$(head -n 1 "$hook_file" 2>/dev/null) || hook=""
  [ -n "$hook" ] || { say "no webhook in $hook_file, so this was not posted: $1"; return 1; }
  curl -fsS -m 15 -H 'Content-Type: application/json' \
    -d "$(jq -nc --arg c "${1:0:1900}" '{content: $c}')" "$hook" >/dev/null \
    || { say "could not post to Discord"; return 1; }
}

# A failure pages once, when the last run succeeded, rather than every quarter hour.
fail() {
  say "$*"
  if [ ! -f "$state_dir/failing" ]; then
    post "STOPGAP ORPHAN LIST NOT READ on $(hostname): $*" && : > "$state_dir/failing"
  fi
  exit 1
}

for net in $networks; do
  : > "$work/$net.jsonl"
  skip=0
  while :; do
    url="$api/api/subgraph-directory?network=$net&indexersMax=0&signalMin=1&sort=signal&first=$page_size&skip=$skip"
    curl -fsS -m 60 -A stopgap-orphan-alert "$url" -o "$work/page.json" || fail "$net: $url did not answer"
    jq -e '.data | type == "array"' "$work/page.json" >/dev/null || fail "$net: no data array from $url"
    [ "$skip" -gt 0 ] || jq -r '.total' "$work/page.json" > "$work/$net.total"
    jq -c '.data[]' "$work/page.json" >> "$work/$net.jsonl"
    [ "$(jq '.data | length' "$work/page.json")" -eq "$page_size" ] || break
    skip=$((skip + page_size))
  done
  # The directory is a cached set paged by skip; a total that moved between pages is a torn read.
  got=$(jq -r '.id' "$work/$net.jsonl" | sort -u | wc -l | tr -d ' ')
  want=$(cat "$work/$net.total")
  [ "$got" = "$want" ] || fail "$net: read $got distinct rows of a total of $want"
  jq -r --arg net "$net" 'select(.network == $net) | .id' "$work/$net.jsonl" | LC_ALL=C sort -u > "$work/$net.ids"
done

joined=()
for net in $networks; do
  known=$state_dir/$net.ids
  # A first run is the baseline, wherever the date stands; otherwise installing late pages the lot.
  [ -f "$known" ] || { say "$net: no earlier list in $state_dir, recording a baseline"; continue; }
  while read -r id; do
    [ -n "$id" ] || continue
    joined+=("$(jq -r --arg id "$id" 'select(.id == $id) |
      "\(.network) \(.ipfsHash) \(.displayName // "unnamed"), \((.signalledTokens | tonumber / 1e18 | floor)) GRT, \(.curatorCount) curators"' \
      "$work/$net.jsonl" | head -n 1)")
  done < <(LC_ALL=C comm -13 "$known" "$work/$net.ids")
done

now=$(date -u +%s)
if [ "${#joined[@]}" -gt 0 ] && [ "$now" -ge "$from_s" ]; then
  msg="STOPGAP ORPHAN: ${#joined[@]} deployment(s) joined the list (signal, no indexer), https://www.lodestar-dashboard.com/subgraphs/migration"
  for line in "${joined[@]}"; do
    next="$msg"$'\n'"$line"
    if [ "${#next}" -gt 1700 ]; then
      msg="$msg"$'\n'"and more"
      break
    fi
    msg=$next
  done
  post "$msg" || exit 1
elif [ "${#joined[@]}" -gt 0 ]; then
  say "${#joined[@]} joined before $from; recorded, not posted"
fi

for net in $networks; do
  mv "$work/$net.ids" "$state_dir/$net.ids"
  printf '%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$net" "$(wc -l < "$state_dir/$net.ids" | tr -d ' ')" >> "$state_dir/sizes.tsv"
done
rm -f "$state_dir/failing"
