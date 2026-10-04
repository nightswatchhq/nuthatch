#!/usr/bin/env bash
# Turns on the sealed-history audit (#1786) on Helsinki's four tip-following units, run from a
# workstation that can `ssh root@<host>`, after they run 4.5.0 or later:
#   AUDIT_RPC=<url> [AUDIT_SPAN=<blocks>] deploy/audit/enable-from-mac.sh [root@89.167.109.4]
#
# The audit refuses an endpoint the nest indexes from or sealed its history from: its unit's --rpc
# and --rpc-fallback, and its nuthatch.toml rpc_urls. This checks all three before editing anything,
# edits the file whose ExecStart systemd runs (a drop-in overrides the unit, #1729), keeps a
# .bak-audit beside it, and restores that backup by itself if the unit does not come back ready.
set -euo pipefail
host=${1:-root@89.167.109.4}
audit=${AUDIT_RPC:?set AUDIT_RPC to an endpoint none of the nests index from}
span=${AUDIT_SPAN:-}
ssh "$host" "AUDIT='$audit' SPAN='$span'"' bash -s' <<'BOX'
set -euo pipefail
host_of() { printf '%s' "$1" | sed -E 's#^[a-z]+://([^/:]+).*#\1#'; }
ah=$(host_of "$AUDIT")
flags="--audit-rpc $AUDIT"; [ -z "$SPAN" ] || flags="$flags --audit-span $SPAN"
for u in graph-allocations-nest-next graph-gns-nest-next nuthatch-dips data-services-nest; do
  f=""
  for c in /etc/systemd/system/$u.service /etc/systemd/system/$u.service.d/*.conf; do
    [ -f "$c" ] && grep -qE "^ExecStart=/" "$c" && f=$c
  done
  [ -n "$f" ] || { echo "STOP: $u: no file sets its ExecStart" >&2; exit 1; }
  if grep -q -- "--audit-rpc" "$f"; then echo "$u: already audited ($f)"; continue; fi
  line=$(grep -E "^ExecStart=/" "$f")
  bin=$(printf '%s' "$line" | grep -oE "^ExecStart=/[^ ]+" | sed "s/^ExecStart=//")
  dir=$(printf '%s' "$line" | grep -oE -- "--dir [^ ]+" | cut -d' ' -f2)
  port=$(printf '%s' "$line" | grep -oE -- "--listen[= ][^ ]+" | sed -E "s/.*://")
  "$bin" dev --help | grep -q -- --audit-rpc || { echo "STOP: $u runs $bin, which has no --audit-rpc; roll 4.5.0 first" >&2; exit 1; }
  if printf '%s' "$line" | grep -q "$ah" || grep -q "$ah" "$dir/nuthatch.toml"; then
    echo "STOP: $u indexes or sealed from $ah (its unit or $dir/nuthatch.toml); the audit would refuse it" >&2; exit 1
  fi
  cp "$f" "$f.bak-audit"
  sed -i -E "/^ExecStart=\//s#\$# $flags#" "$f"
  systemctl daemon-reload
  systemctl restart "$u"
  for i in $(seq 1 60); do
    curl -sf -m 3 "http://127.0.0.1:$port/ready" | grep -q '"ready":true' && break; sleep 5
  done
  if ! curl -sf -m 5 "http://127.0.0.1:$port/metrics" | grep -q "^nuthatch_audit_ranges_total"; then
    cp "$f.bak-audit" "$f"; systemctl daemon-reload; systemctl restart "$u"
    echo "STOP: $u did not come back ready and audited, so its unit was restored and restarted; journalctl -u $u says why" >&2
    exit 1
  fi
  echo "$u: audited against $ah (edited $f)"
done
BOX
