#!/usr/bin/env bash
# Dead-man check for the ThinkPad's monitoring, run from Helsinki's cron every minute.
# Prometheus re-sends the always-firing Watchdog to Alertmanager each evaluation, and Alertmanager
# drops it a few minutes after the last one, so "Watchdog active" proves the box, Prometheus's rule
# evaluation and Alertmanager are all alive. Pages Discord once on loss and once on recovery.
set -uo pipefail

AM=${DEADMAN_ALERTMANAGER:-http://100.83.44.63:9493}
HOOK_FILE=${DEADMAN_WEBHOOK_FILE:-/etc/nuthatch/deadman_discord_webhook_url}
STATE=${DEADMAN_STATE:-/var/lib/nuthatch-deadman/state}

alive() {
  local body
  body=$(curl -sS -m 10 -G "$AM/api/v2/alerts" \
    --data-urlencode 'filter=alertname="Watchdog"' --data-urlencode active=true 2>/dev/null) || return 1
  case "$body" in *'"alertname":"Watchdog"'*) return 0 ;; *) return 1 ;; esac
}

page() {
  local hook
  hook=$(cat "$HOOK_FILE") || { echo "deadman: cannot read $HOOK_FILE" >&2; return 1; }
  curl -sS -m 10 -H 'Content-Type: application/json' \
    -d "{\"content\":\"$1\"}" "$hook" >/dev/null
}

mkdir -p "$(dirname "$STATE")"
was=$(cat "$STATE" 2>/dev/null || echo up)
if alive; then now=up; else now=down; fi
[ "$now" = "$was" ] && exit 0

if [ "$now" = down ]; then
  page "FIRING: MonitoringDead - $AM holds no active Watchdog; the ThinkPad, its Prometheus or its Alertmanager has stopped, so no other alert can reach you" || exit 1
else
  page "RESOLVED: MonitoringDead - $AM holds the Watchdog again" || exit 1
fi
echo "$now" >"$STATE"
