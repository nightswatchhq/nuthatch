#!/usr/bin/env bash
# Rolls a published release onto the Helsinki units, run from a workstation that can
# `ssh root@<host>`. Uses the deploy scripts from the release's own tag, checksums the Linux release,
# installs it, and rolls each unit with its smoke file, the allocations nest first. A smoke failure
# stops here: a regression reverts that unit, a failure the previous release shares keeps it (exit 3).
#   deploy/roll-helsinki-from-mac.sh <version> [unit ...]
#   deploy/roll-helsinki-from-mac.sh 4.3.1                         every unit
#   deploy/roll-helsinki-from-mac.sh 4.3.1 graph-allocations-nest-next
# The smoke files carry kittiwake's statements, so they live in the private kittiwake repo:
# SMOKE_DIR (default ~/Projects/kittiwake/nuthatch-gate/smoke) holds <unit without -next>.sql.
set -euo pipefail
v=${1:?usage: roll-helsinki-from-mac.sh <version> [unit ...]}; shift
host=${ROLL_HOST:-root@89.167.109.4}
smoke_dir=${SMOKE_DIR:-$HOME/Projects/kittiwake/nuthatch-gate/smoke}
units=("$@")
[[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { echo "not a version: $v" >&2; exit 2; }
[ ${#units[@]} -gt 0 ] || units=(graph-allocations-nest-next data-services-nest graph-gns-nest-next nuthatch-dips graph-staking-legacy-readonly)
for u in "${units[@]}"; do [[ $u =~ ^[a-z0-9][a-z0-9-]*$ ]] || { echo "not a unit name: $u" >&2; exit 2; }; done
[ -d "$smoke_dir" ] || { echo "no smoke directory at $smoke_dir: clone nightswatchhq/kittiwake or set SMOKE_DIR" >&2; exit 2; }
smokes=()
for u in "${units[@]}"; do
  if [ -f "$smoke_dir/${u%-next}.sql" ]; then smokes+=("${u%-next}.sql")
  else echo "note: no $smoke_dir/${u%-next}.sql, $u rolls without a smoke" >&2; fi
done

# The smoke files ride over the same connection on stdin, into a directory beside the clone.
COPYFILE_DISABLE=1 tar -C "$smoke_dir" -cf - -T /dev/null ${smokes[@]+"${smokes[@]}"} | ssh "$host" "V=$v; UNITS='${units[*]}'; "'set -e
d=/tmp/nuthatch-roll-$V; rm -rf "$d" "$d-smoke"; mkdir "$d-smoke"; tar -xf - -C "$d-smoke"
git clone -q --depth 1 --branch "v$V" https://github.com/nightswatchhq/nuthatch "$d"; cd "$d"
u=https://github.com/nightswatchhq/nuthatch/releases/download/v$V/nuthatch-x86_64-unknown-linux-gnu.tar.gz
curl -fsSL -o n.tgz "$u"; curl -fsSL -o n.sha "$u.sha256"
echo "$(cut -d" " -f1 n.sha)  n.tgz" | sha256sum -c --quiet
mkdir x; tar xzf n.tgz -C x; bin=$(find x -type f -name nuthatch | head -1)
scripts/deploy-nest.sh install "$bin" "$V"
for unit in $UNITS; do
  smoke=$d-smoke/${unit%-next}.sql
  if [ -f "$smoke" ]; then scripts/deploy-nest.sh roll "$unit" "$V" --smoke "$smoke"
  else scripts/deploy-nest.sh roll "$unit" "$V"; fi
done
scripts/deploy-nest.sh check'
