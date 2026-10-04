#!/usr/bin/env bash
# Turns on the sealed-history audit (#1786) on Helsinki's four tip-following units, run from a
# workstation that can `ssh root@<host>`, after they run 4.5.0 or later:
#   deploy/audit/enable-from-mac.sh [root@89.167.109.4]
#
# The audit endpoint must be one the nest does not index from, so arb1.arbitrum.io moves from each
# unit's fallbacks to --audit-rpc; GraphOps stays primary and the other fallback stays. Edits the file
# whose ExecStart systemd runs (a drop-in overrides the unit, #1729), keeps a .bak-audit beside it,
# restarts one unit at a time and waits for /ready and the audit's metric. Safe to re-run.
set -euo pipefail
host=${1:-root@89.167.109.4}
ssh "$host" 'set -euo pipefail
audit=https://arb1.arbitrum.io/rpc
for u in graph-allocations-nest-next graph-gns-nest-next nuthatch-dips data-services-nest; do
  f=""
  for c in /etc/systemd/system/$u.service /etc/systemd/system/$u.service.d/*.conf; do
    [ -f "$c" ] && grep -qE "^ExecStart=/" "$c" && f=$c
  done
  [ -n "$f" ] || { echo "STOP: $u: no file sets its ExecStart" >&2; exit 1; }
  bin=$(grep -oE "^ExecStart=/[^ ]+" "$f" | sed "s/^ExecStart=//")
  "$bin" dev --help | grep -q -- --audit-rpc || { echo "STOP: $u runs $bin, which has no --audit-rpc; roll 4.5.0 first" >&2; exit 1; }
  if grep -q -- "--audit-rpc" "$f"; then echo "$u: already audited ($f)"; continue; fi
  cp "$f" "$f.bak-audit"
  sed -i -E "/^ExecStart=\//{s# --rpc-fallback $audit##g; s#\$# --audit-rpc $audit#}" "$f"
  grep -q -- "--rpc-fallback $audit" "$f" && { echo "STOP: $u: $audit is still a fallback in $f" >&2; exit 1; }
  systemctl daemon-reload
  systemctl restart "$u"
  port=$(grep -oE -- "--listen[= ][^ ]+" "$f" | head -1 | sed -E "s/.*://")
  for i in $(seq 1 60); do
    curl -sf -m 3 "http://127.0.0.1:$port/ready" | grep -q "\"ready\":true" && break; sleep 5
  done
  curl -sf -m 3 "http://127.0.0.1:$port/ready" | grep -q "\"ready\":true" || { echo "STOP: $u not ready after the change; revert with $f.bak-audit" >&2; exit 1; }
  curl -sf -m 5 "http://127.0.0.1:$port/metrics" | grep -q "^nuthatch_audit_ranges_total" \
    || { echo "STOP: $u is ready but publishes no nuthatch_audit_ranges_total" >&2; exit 1; }
  echo "$u: audited against $audit (edited $f)"
done'
