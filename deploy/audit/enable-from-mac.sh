#!/usr/bin/env bash
# Turns on the sealed-history audit (#1786) on Helsinki's four tip-following units, run from a
# workstation that can `ssh root@<host>`, after they run 4.5.0 or later:
#   AUDIT_RPC=<url> [AUDIT_SPAN=<blocks>] deploy/audit/enable-from-mac.sh [root@89.167.109.4]
#
# The audit refuses an endpoint the nest indexes from or sealed its history from: its unit's --rpc
# and --rpc-fallback, and its nuthatch.toml rpc_urls. Every unit is checked before any is edited.
# Each edit goes into the file whose ExecStart systemd runs (a drop-in overrides the unit, #1729),
# keeps a .bak-audit beside it, and is restored by the script if the unit does not come back ready
# and audited.
set -euo pipefail
host=${1:-root@89.167.109.4}
audit=${AUDIT_RPC:?set AUDIT_RPC to an endpoint none of the nests index from}
span=${AUDIT_SPAN:-}
ssh "$host" "AUDIT=$(printf '%q' "$audit") SPAN=$(printf '%q' "$span") bash -s" <<'BOX'
set -euo pipefail
units=(graph-allocations-nest-next graph-gns-nest-next nuthatch-dips data-services-nest)
ah=$(printf '%s' "$AUDIT" | sed -E 's#^[a-z]+://([^/:]+).*#\1#')
flags="--audit-rpc $AUDIT"; [ -z "$SPAN" ] || flags="$flags --audit-span $SPAN"
declare -A file port
todo=()
for u in "${units[@]}"; do
  f=""
  for c in /etc/systemd/system/$u.service /etc/systemd/system/$u.service.d/*.conf; do
    [ -f "$c" ] && grep -qE "^ExecStart=/" "$c" && f=$c
  done
  [ -n "$f" ] || { echo "STOP: $u: no file sets its ExecStart; nothing was changed" >&2; exit 1; }
  if grep -q -- "--audit-rpc" "$f"; then echo "$u: already audited ($f)"; continue; fi
  line=$(grep -E "^ExecStart=/" "$f")
  bin=$(printf '%s' "$line" | grep -oE "^ExecStart=/[^ ]+" | sed "s/^ExecStart=//")
  dir=$(printf '%s' "$line" | grep -oE -- "--dir [^ ]+" | cut -d' ' -f2)
  "$bin" dev --help | grep -q -- --audit-rpc \
    || { echo "STOP: $u runs $bin, which has no --audit-rpc; roll 4.5.0 first. Nothing was changed" >&2; exit 1; }
  if [[ $line == *"$ah"* ]] || grep -qF "$ah" "$dir/nuthatch.toml"; then
    echo "STOP: $u indexes or sealed from $ah (its unit or $dir/nuthatch.toml); nothing was changed" >&2; exit 1
  fi
  file[$u]=$f
  port[$u]=$(printf '%s' "$line" | grep -oE -- "--listen[= ][^ ]+" | sed -E "s/.*://")
  todo+=("$u")
done
for u in ${todo[@]+"${todo[@]}"}; do
  f=${file[$u]} p=${port[$u]}
  cp "$f" "$f.bak-audit"
  sed -i -E "/^ExecStart=\//s#\$# $flags#" "$f"
  systemctl daemon-reload
  systemctl restart "$u"
  ready=""
  for i in $(seq 1 60); do
    if curl -sf -m 3 "http://127.0.0.1:$p/ready" | grep -q '"ready":true'; then ready=1; break; fi
    sleep 5
  done
  if [ -z "$ready" ] || ! curl -sf -m 5 "http://127.0.0.1:$p/metrics" | grep -q "^nuthatch_audit_ranges_total"; then
    cp "$f.bak-audit" "$f"; systemctl daemon-reload; systemctl restart "$u"
    echo "STOP: $u did not come back ready and audited, so its unit was restored and restarted; journalctl -u $u says why. Units audited before it stay audited" >&2
    exit 1
  fi
  echo "$u: audited against $ah (edited $f)"
done
BOX
