#!/usr/bin/env bash
# Puts every production nest under the release gate (#1794), run from a workstation that can
# `ssh root@<helsinki>` and `ssh thinkpad`:
#   deploy/release-gate/install-nests-from-mac.sh [root@89.167.109.4] [thinkpad]
#
# Helsinki: installs gate-export.sh and rewrites /etc/nuthatch/gate-export.env, the export's
# allowlist, with one NEST= line per Helsinki unit, its directory and address read from the running
# process; snapshots each once; and reads each unit's NUTHATCH_* budget from that process's
# environment. ThinkPad: pulls ~/nuthatch-ops and ~/kittiwake, writes ~/release-gate/nests.conf, one
# environment file per nest (the QoS nest's read from qos-reo-nest here), the QoS nest's local
# export config, refreshes every copy and installs the unit.
#
# Needs: the ThinkPad's key already authorised for the export (install-export-from-mac.sh), this
# change on nuthatch main (the ThinkPad runs main) and kittiwake's query sets on its main.
set -euo pipefail
host=${1:-root@89.167.109.4}
tp=${2:-thinkpad}
repo=$(cd "$(dirname "$0")/../.." && pwd)
src=$repo/deploy/release-gate/helsinki/gate-export.sh
[ -f "$src" ] || { echo "missing $src; run from an up-to-date checkout" >&2; exit 1; }

# <gate name>:<unit>. The QoS nest is the ThinkPad's own unit, qos-reo-nest.
helsinki_nests="alloc-nest:graph-allocations-nest-next gns-nest:graph-gns-nest-next dips-nest:nuthatch-dips data-services-nest:data-services-nest staking-archive-nest:graph-staking-legacy-readonly"
# The settings that shape what serving costs. Tokens, RPC URLs and paths into the unit's own
# directory (its spill directory) stay on the box.
keys='NUTHATCH_(SQL_MAX_CONCURRENCY|ANALYTICS_MEMORY_LIMIT|ANALYTICS_THREADS|ANALYTICS_MAX_TEMP_SIZE|ENGINE|BURRMILL_MEMORY_LIMIT|MAX_RSS|SQL_MEMO_BYTES|HOT_STORE_CACHE_BYTES|INGESTION_RESERVATION)'

# Prints "<dir> <host:port>" for the running unit $1; run on the box that runs it, as root.
# shellcheck disable=SC2016
unit_of='unit_of() {
  pid=$(systemctl show -p MainPID --value "$1")
  [ -n "$pid" ] && [ "$pid" -gt 0 ] || { echo "$1 is not running" >&2; return 1; }
  dir=$(tr "\0" "\n" < /proc/$pid/cmdline | sed -n "/^--dir$/{n;p;}")
  case "$dir" in /*) ;; "") dir=$(readlink /proc/$pid/cwd) ;; *) dir=$(readlink /proc/$pid/cwd)/$dir ;; esac
  listen=$(tr "\0" "\n" < /proc/$pid/cmdline | sed -n "/^--listen$/{n;p;}")
  [ -n "$listen" ] || listen=$(ss -ltnpH | grep "pid=$pid," | awk "{print \$4}" | head -n 1)
  listen=$(printf "%s" "$listen" | sed "s/^0\.0\.0\.0:/127.0.0.1:/")
  test -f "$dir/nuthatch.redb" && test -f "$dir/segments/manifest.json" \
    || { echo "$dir ($1) is not a nest with sealed segments" >&2; return 1; }
  [ -n "$listen" ] || { echo "no listening address for $1" >&2; return 1; }
  echo "$dir $listen"
}'

echo "== Helsinki: the export and its allowlist"
ssh "$host" 'cat > /tmp/gate-export.sh' < "$src"
# shellcheck disable=SC2029
helsinki_out=$(ssh "$host" "NESTS='$helsinki_nests'; KEYS='$keys'; $unit_of"'
set -e
command -v rsync >/dev/null || { echo "rsync missing on the host" >&2; exit 1; }
install -m 755 /tmp/gate-export.sh /usr/local/bin/nuthatch-gate-export
mkdir -p /etc/nuthatch
new=/etc/nuthatch/gate-export.env.new
{ echo "# The release gate export allowlist, written by install-nests-from-mac.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)."
  echo "# NEST=<name> <dir> <url>: the nests the ThinkPad key may snapshot; NEST_DIR/NEST_URL: a bare snapshot."; } > "$new"
for pair in $NESTS; do
  name=${pair%%:*} unit=${pair#*:}
  where=$(unit_of "$unit"); set -- $where
  echo "NEST=$name $1 http://$2" >> "$new"
  [ "$name" != alloc-nest ] || printf "NEST_DIR=%s\nNEST_URL=http://%s\n" "$1" "$2" >> "$new"
  pid=$(systemctl show -p MainPID --value "$unit")
  echo "UNIT $name $unit"
  tr "\0" "\n" < /proc/$pid/environ | grep -E "^($KEYS)=" | sed "s/^/ENV $name /" || true
done
mv "$new" /etc/nuthatch/gate-export.env
sed "s/^/CONF /" /etc/nuthatch/gate-export.env
for pair in $NESTS; do
  name=${pair%%:*}
  SSH_ORIGINAL_COMMAND="snapshot $name" nuthatch-gate-export | sed "s/^/PROV $name /"
done
du -sh /var/lib/nuthatch-gate/nests/* | sed "s/^/SIZE /"')
printf '%s\n' "$helsinki_out" | grep -E '^(CONF|PROV|SIZE|UNIT) '

echo "== the ThinkPad key reaches the allowlist and nothing else"
out=$(ssh "$tp" 'for c in hello "snapshot not-a-nest"; do ssh -i ~/.ssh/nuthatch-gate -o BatchMode=yes -o IdentitiesOnly=yes root@100.82.188.91 "$c" 2>&1; echo "exit $?"; done')
echo "$out"
case "$out" in
  *"refused: hello"*"exit 1"*"refused: snapshot not-a-nest"*"exit 1"*) echo "ok: both refused" ;;
  *) echo "STOP: expected the export to refuse both. Is the key authorised (install-export-from-mac.sh)?" >&2; exit 1 ;;
esac

echo "== ThinkPad: config, environments, copies, unit"
# shellcheck disable=SC2029
printf '%s\n' "$helsinki_out" | grep -E '^(UNIT|ENV) ' | ssh "$tp" "KEYS='$keys'; HELSINKI='$host'; $unit_of"'
set -e
git -C ~/nuthatch-ops pull -q --ff-only
git -C ~/kittiwake pull -q --ff-only
ops=~/nuthatch-ops
grep -q -- "--local" $ops/deploy/release-gate/refresh-from-helsinki.sh \
  || { echo "STOP: ~/nuthatch-ops has no multi-nest gate yet; merge it to main first" >&2; exit 1; }
for s in alloc qos gns dips data-services staking-archive; do
  test -f ~/kittiwake/nuthatch-gate/$s-queries.tsv \
    || { echo "STOP: no ~/kittiwake/nuthatch-gate/$s-queries.tsv; merge kittiwake'"'"'s query sets first" >&2; exit 1; }
done
mkdir -p ~/release-gate/env
now=$(date -u +%Y-%m-%dT%H:%M:%SZ)
header() { printf "# Production environment of %s on %s, the NUTHATCH_* budget settings read from its running\n# process on %s by install-nests-from-mac.sh. Re-run the installer when the unit changes.\n" "$2" "$3" "$now" > ~/release-gate/env/$1.env; }
while read -r kind name rest; do
  case "$kind" in
    UNIT) header "$name" "$rest" "$HELSINKI" ;;
    ENV) echo "$rest" >> ~/release-gate/env/$name.env ;;
  esac
done
qos=$(sudo -n bash -c "$(declare -f unit_of); unit_of qos-reo-nest"); set -- $qos
qos_dir=$1 qos_listen=$2
header qos-nest qos-reo-nest "this box"
pid=$(systemctl show -p MainPID --value qos-reo-nest)
sudo -n cat /proc/$pid/environ | tr "\0" "\n" | grep -E "^($KEYS)=" >> ~/release-gate/env/qos-nest.env || true
printf "NEST=qos-nest %s http://%s\n" "$qos_dir" "$qos_listen" > ~/release-gate/export-local.env
cp $ops/deploy/release-gate/nests.conf.example ~/release-gate/nests.conf
for f in ~/release-gate/env/*.env; do echo "--- $f"; grep -v "^#" "$f"; done

cd $ops
for n in alloc-nest gns-nest dips-nest data-services-nest staking-archive-nest; do
  GATE_NEST=~/release-gate/$n deploy/release-gate/refresh-from-helsinki.sh "$n"
done
GATE_NEST=~/release-gate/qos-nest deploy/release-gate/refresh-from-helsinki.sh --local qos-nest
du -sh ~/release-gate/*-nest

cp deploy/release-gate/release-gate.service deploy/release-gate/release-gate.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now release-gate.timer
systemctl --user cat release-gate.service | grep GATE_NESTS'
echo "done: the next poll gates every nest; a hand run is: ssh $tp 'cd ~/nuthatch-ops && GATE_NESTS=~/release-gate/nests.conf scripts/release-gate-run.sh v<version>'"
