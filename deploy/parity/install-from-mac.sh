#!/usr/bin/env bash
# Installs the daily parity timer (#1713, #1718) on Helsinki, run from a workstation that can
# `ssh root@<host>`. Prompts once for the Graph gateway key (never echoed), proves a failure pages
# Discord with a deliberately broken comparison, then enables the timer.
#   deploy/parity/install-from-mac.sh [root@89.167.109.4]
set -euo pipefail
host=${1:-root@89.167.109.4}
repo=$(cd "$(dirname "$0")/../.." && pwd)
files=(scripts/lodestar-parity.sh deploy/parity/nuthatch-parity.sh deploy/parity/nuthatch-parity.service deploy/parity/nuthatch-parity.timer)

for f in "${files[@]}"; do
  [ -f "$repo/$f" ] || { echo "missing $f; run from an up-to-date checkout" >&2; exit 1; }
  ssh "$host" "cat > /tmp/${f##*/}" < "$repo/$f"
done

ssh "$host" 'set -e
command -v python3 >/dev/null || { echo "python3 missing on the host" >&2; exit 1; }
command -v curl >/dev/null || { echo "curl missing on the host" >&2; exit 1; }
test -s /etc/nuthatch/deadman_discord_webhook_url || { echo "no /etc/nuthatch/deadman_discord_webhook_url; install the dead-man check first" >&2; exit 1; }
install -D -m 755 /tmp/lodestar-parity.sh /usr/local/lib/nuthatch-parity/lodestar-parity.sh
install -m 755 /tmp/nuthatch-parity.sh /usr/local/bin/nuthatch-parity
install -m 644 /tmp/nuthatch-parity.service /tmp/nuthatch-parity.timer /etc/systemd/system/
install -m 600 /etc/nuthatch/deadman_discord_webhook_url /etc/nuthatch/parity_discord_webhook_url
systemctl daemon-reload
echo "installed"'

if ssh "$host" 'test -s /etc/nuthatch/parity.env'; then
  echo "keeping the existing /etc/nuthatch/parity.env"
else
  read -rsp "GRAPH_API_KEY: " key; echo
  [ -n "$key" ] || { echo "empty key" >&2; exit 1; }
  printf 'GRAPH_API_KEY=%s\n' "$key" | ssh "$host" 'umask 077; cat > /etc/nuthatch/parity.env'
  unset key
fi

echo "deliberate failure: expect exit 1 and one PARITY line in Discord"
ssh "$host" 'sed "s/if alloc_nest == alloc_sg else/if False else/" /usr/local/lib/nuthatch-parity/lodestar-parity.sh > /tmp/parity-broken.sh
grep -q "if False else" /tmp/parity-broken.sh || { echo "the break did not apply; the script changed" >&2; exit 1; }
chmod 755 /tmp/parity-broken.sh
rc=0; PARITY_SCRIPT=/tmp/parity-broken.sh PARITY_MODES=sealed nuthatch-parity || rc=$?; echo "exit $rc"
rm -f /tmp/parity-broken.sh
[ "$rc" -ne 4 ] || { echo "the deliberate failure exited 4: the subgraph side did not answer, so check GRAPH_API_KEY in /etc/nuthatch/parity.env" >&2; exit 1; }
[ "$rc" -eq 1 ] || { echo "the deliberate failure exited $rc, not 1: the timer would not page on a real one" >&2; exit 1; }'

ssh "$host" 'systemctl start --no-block nuthatch-parity.service && systemctl enable --now nuthatch-parity.timer && systemctl list-timers nuthatch-parity.timer --no-pager | head -2'
echo "done; results later with: ssh $host 'column -t -s \"\$(printf \"\\t\")\" /var/log/nuthatch/parity/runs.tsv | tail -n 8'"
