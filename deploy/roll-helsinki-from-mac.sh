#!/usr/bin/env bash
# Rolls a published release onto the Helsinki units, run from a workstation that can
# `ssh root@<host>`. Downloads and checksums the Linux release, installs it, rolls the allocations
# nest first with its smoke file (a failed smoke reverts that unit and stops here), then the rest.
#   deploy/roll-helsinki-from-mac.sh <version> [unit ...]
#   deploy/roll-helsinki-from-mac.sh 4.3.1                         every unit
#   deploy/roll-helsinki-from-mac.sh 4.3.1 graph-allocations-nest-next
set -euo pipefail
v=${1:?usage: roll-helsinki-from-mac.sh <version> [unit ...]}; shift
host=${ROLL_HOST:-root@89.167.109.4}
units=("$@")
[ ${#units[@]} -gt 0 ] || units=(graph-allocations-nest-next data-services-nest graph-gns-nest-next nuthatch-dips graph-staking-legacy-readonly)

ssh "$host" "V=$v; UNITS='${units[*]}'; "'set -e
d=/tmp/nuthatch-roll-$V; rm -rf "$d"; git clone -q --depth 1 https://github.com/nightswatchhq/nuthatch "$d"; cd "$d"
u=https://github.com/nightswatchhq/nuthatch/releases/download/v$V/nuthatch-x86_64-unknown-linux-gnu.tar.gz
curl -fsSL -o n.tgz "$u"; curl -fsSL -o n.sha "$u.sha256"
echo "$(cut -d" " -f1 n.sha)  n.tgz" | sha256sum -c --quiet
mkdir x; tar xzf n.tgz -C x; bin=$(find x -type f -name nuthatch | head -1)
scripts/deploy-nest.sh install "$bin" "$V"
for unit in $UNITS; do
  smoke=scripts/smoke/${unit%-next}.sql
  if [ -f "$smoke" ]; then scripts/deploy-nest.sh roll "$unit" "$V" --smoke "$smoke"
  else scripts/deploy-nest.sh roll "$unit" "$V"; fi
done
scripts/deploy-nest.sh check'
