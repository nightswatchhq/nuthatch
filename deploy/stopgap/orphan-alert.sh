#!/usr/bin/env bash
# stopgap-orphan-alert - posts one Discord message when a bsc or matic deployment joins the orphan
# list (#1942): over 1 GRT of signal, no active allocation, as Lodestar's /subgraphs/migration shows
# it from kittiwake's /api/subgraph-directory. Started by stopgap-orphan-alert.timer.
#
# The first run, and every run before ORPHAN_ALERT_FROM, records the list and posts nothing, so the
# first page is a deployment that joined after the migration began. A deployment that leaves and
# comes back pages again: an indexer dropping one is the case worth knowing about.
#
# It keeps a second list the same way: signalled deployments whose only active allocation is the
# Foundation's upgrade indexer, which stops serving them by 2026-10-31. Joining that list is the
# warning weeks ahead; joining the orphan list is the deployment going dark.
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
#   ORPHAN_ALERT_SUBGRAPH     network subgraph GraphQL (default https://network.thenightswatch.dev/graphql)
#   ORPHAN_ALERT_UPGRADE      the Foundation's upgrade indexer (default 0xbdfb5ee5a2abf4fc7bb1bd1221067aef7f9de491)
set -euo pipefail

api=${ORPHAN_ALERT_API:-https://api.lodestar-dashboard.com}
networks=${ORPHAN_ALERT_NETWORKS:-bsc matic}
from=${ORPHAN_ALERT_FROM:-2026-10-08}
state_dir=${ORPHAN_ALERT_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/stopgap-orphans}
hook_file=${ORPHAN_ALERT_WEBHOOK_FILE:-$HOME/.config/nightswatch/discord-webhook}
dry=${ORPHAN_ALERT_DRY_RUN:-0}
subgraph=${ORPHAN_ALERT_SUBGRAPH:-https://network.thenightswatch.dev/graphql}
upgrade=${ORPHAN_ALERT_UPGRADE:-0xbdfb5ee5a2abf4fc7bb1bd1221067aef7f9de491}
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

# Pages the directory for one network into $3; $2 is any extra filter.
read_dir() {
  local net=$1 extra=$2 out=$3 skip=0 url got want
  : > "$out"
  while :; do
    url="$api/api/subgraph-directory?network=$net$extra&signalMin=1&sort=signal&first=$page_size&skip=$skip"
    curl -fsS -m 60 -A stopgap-orphan-alert "$url" -o "$work/page.json" || fail "$net: $url did not answer"
    jq -e '.data | type == "array"' "$work/page.json" >/dev/null || fail "$net: no data array from $url"
    [ "$skip" -gt 0 ] || jq -r '.total' "$work/page.json" > "$out.total"
    jq -c '.data[]' "$work/page.json" >> "$out"
    [ "$(jq '.data | length' "$work/page.json")" -eq "$page_size" ] || break
    skip=$((skip + page_size))
  done
  # The directory is a cached set paged by skip; a total that moved between pages is a torn read.
  got=$(jq -r '.id' "$out" | sort -u | wc -l | tr -d ' ')
  want=$(cat "$out.total")
  [ "$got" = "$want" ] || fail "$net: read $got distinct rows of a total of $want"
}

# Active allocations on the deployments listed in $2, as {i, d} lines into $3.
read_allocs() {
  local net=$1 ids=$2 out=$3 chunk skip n q
  : > "$out"
  rm -f "$work/chunk."*
  split -l 100 "$ids" "$work/chunk."
  for chunk in "$work/chunk."*; do
    [ -s "$chunk" ] || continue
    skip=0
    while :; do
      q=$(jq -nc --argjson ids "$(jq -R . "$chunk" | jq -sc .)" --argjson skip "$skip" \
        '{query: "query($ids:[String!],$skip:Int){allocations(first:1000,skip:$skip,where:{status:Active,subgraphDeployment_in:$ids}){indexer{id} subgraphDeployment{id}}}", variables: {ids: $ids, skip: $skip}}')
      curl -fsS -m 60 -A stopgap-orphan-alert -H 'Content-Type: application/json' -d "$q" "$subgraph" -o "$work/alloc.json" \
        || fail "$net: $subgraph did not answer"
      jq -e '.data.allocations | type == "array"' "$work/alloc.json" >/dev/null || fail "$net: no allocations array from $subgraph"
      jq -c '.data.allocations[] | {i: .indexer.id, d: .subgraphDeployment.id}' "$work/alloc.json" >> "$out"
      n=$(jq '.data.allocations | length' "$work/alloc.json")
      [ "$n" -eq 1000 ] || break
      skip=$((skip + 1000))
    done
  done
}

for net in $networks; do
  read_dir "$net" "&indexersMax=0" "$work/$net.jsonl"
  jq -r --arg net "$net" 'select(.network == $net) | .id' "$work/$net.jsonl" | LC_ALL=C sort -u > "$work/$net.ids"

  read_dir "$net" "" "$work/all-$net.jsonl"
  jq -r --arg net "$net" 'select(.network == $net) | .id' "$work/all-$net.jsonl" | LC_ALL=C sort -u > "$work/all-$net.ids"
  read_allocs "$net" "$work/all-$net.ids" "$work/allocs-$net.jsonl"
  jq -rs --arg u "$upgrade" 'group_by(.d)[] | select(([.[].i | ascii_downcase] | unique) == [$u]) | .[0].d' \
    "$work/allocs-$net.jsonl" | LC_ALL=C sort -u > "$work/fdnonly-$net.ids"
done

# Lines for ids in $2 that are not in $1, described from the directory rows in $3, appended to $4.
joins() {
  local known=$1 current=$2 rows=$3 out=$4 id
  # A first run is the baseline, wherever the date stands; otherwise installing late pages the lot.
  [ -f "$known" ] || { say "no earlier list at $known, recording a baseline"; return 0; }
  while read -r id; do
    [ -n "$id" ] || continue
    jq -r --arg id "$id" 'select(.id == $id) |
      "\(.network) \(.ipfsHash) \(.displayName // "unnamed"), \((.signalledTokens | tonumber / 1e18 | floor)) GRT, \(.curatorCount) curators"' \
      "$rows" | head -n 1 >> "$out"
  done < <(LC_ALL=C comm -13 "$known" "$current")
}

: > "$work/orphan.joined"
: > "$work/fdnonly.joined"
for net in $networks; do
  joins "$state_dir/$net.ids" "$work/$net.ids" "$work/$net.jsonl" "$work/orphan.joined"
  joins "$state_dir/fdnonly-$net.ids" "$work/fdnonly-$net.ids" "$work/all-$net.jsonl" "$work/fdnonly.joined"
done

outbox=$state_dir/outbox
mkdir -p "$outbox"

# Writes $1 followed by the lines of $2, trimmed to fit one Discord message, to the outbox as $3.
queue() {
  local msg=$1 lines=$2 name=$3 line next
  while read -r line; do
    next="$msg"$'\n'"$line"
    if [ "${#next}" -gt 1700 ]; then
      msg="$msg"$'\n'"and more"
      break
    fi
    msg=$next
  done < "$lines"
  printf '%s' "$msg" > "$outbox/$name.tmp"
  mv "$outbox/$name.tmp" "$outbox/$name"
}

# A page is queued before its list's state is committed and removed once Discord accepts it, so a
# committed list never pages twice and a failed post is retried from the outbox on the next run.
# Only a kill between an accepted post and its rm can repeat a page.
send_outbox() {
  local f
  for f in "$outbox"/*; do
    [ -f "$f" ] || continue
    case $f in *.tmp) rm -f "$f"; continue ;; esac
    post "$(cat "$f")" || return 1
    rm -f "$f"
  done
}

commit() {
  local prefix=$1 net
  for net in $networks; do
    mv "$work/$prefix$net.ids" "$state_dir/$prefix$net.ids"
  done
}

now=$(date -u +%s)
stamp=$(date -u +%Y%m%dT%H%M%SZ)
orphans=$(wc -l < "$work/orphan.joined" | tr -d ' ')
fdnonly=$(wc -l < "$work/fdnonly.joined" | tr -d ' ')
if [ "$now" -lt "$from_s" ] && [ $((orphans + fdnonly)) -gt 0 ]; then
  say "$orphans orphan and $fdnonly foundation-only joins before $from; recorded, not posted"
fi
if [ "$now" -ge "$from_s" ] && [ "$orphans" -gt 0 ]; then
  queue "STOPGAP ORPHAN: $orphans deployment(s) joined the list (signal, no indexer), https://www.lodestar-dashboard.com/subgraphs/migration" "$work/orphan.joined" "$stamp-1-orphan"
fi
commit ""
if [ "$now" -ge "$from_s" ] && [ "$fdnonly" -gt 0 ]; then
  queue "STOPGAP FOUNDATION-ONLY: $fdnonly deployment(s) now served only by the Foundation's upgrade indexer, which stops by 2026-10-31" "$work/fdnonly.joined" "$stamp-2-fdnonly"
fi
commit "fdnonly-"

for net in $networks; do
  printf '%s\t%s\t%s\t%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$net" \
    "$(wc -l < "$state_dir/$net.ids" | tr -d ' ')" "$(wc -l < "$state_dir/fdnonly-$net.ids" | tr -d ' ')" >> "$state_dir/sizes.tsv"
done
rm -f "$state_dir/failing"
send_outbox || exit 1
