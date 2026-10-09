#!/usr/bin/env bash
# Puts the QoS nest inside its 2 GiB budget (#1899), from a workstation that can `ssh thinkpad` with
# sudo there:
#   deploy/qos-budget-from-mac.sh
# Sets the measured settings on qos-reo-nest through `deploy-nest.sh env`, which restarts it, puts the
# previous settings back if it does not come ready with them or refuses a smoke statement, and then
# rewrites the release gate's copy of the unit's budget so the gate measures what production runs.
# The smoke file carries kittiwake's statements, so it lives in the private kittiwake repo:
# SMOKE_DIR (default ~/Projects/kittiwake/nuthatch-gate/smoke) holds qos-reo-nest.sql.
set -euo pipefail
host=${QOS_HOST:-thinkpad}
unit=qos-reo-nest
smoke_dir=${SMOKE_DIR:-$HOME/Projects/kittiwake/nuthatch-gate/smoke}
repo=$(cd "$(dirname "$0")/.." && pwd)
# Measured on burrmill 1a1e326 (#1899): ingest 372 MiB, up to 931 MiB outside the pool at the gate's
# peak over twelve runs. 704 + 384 + 960 = 2048; four gate runs inside MemoryMax=2G peaked at 1316 to
# 1349 MiB, nine of nine answering. Needs a release with that burrmill and this check: 4.8.0 refuses
# the reservation, and an older burrmill refuses indexer_day at 704 MB.
settings="NUTHATCH_BURRMILL_MEMORY_LIMIT=704MB NUTHATCH_ANALYTICS_THREADS=4 NUTHATCH_MAX_RSS=2048MB NUTHATCH_INGESTION_RESERVATION=384MB NUTHATCH_RUNTIME_HEADROOM=960MB"

[ -f "$repo/scripts/deploy-nest.sh" ] || { echo "no scripts/deploy-nest.sh under $repo" >&2; exit 2; }
[ -f "$smoke_dir/$unit.sql" ] || { echo "no $smoke_dir/$unit.sql: clone nuthatch-org/kittiwake or set SMOKE_DIR" >&2; exit 2; }

COPYFILE_DISABLE=1 tar -cf - -C "$repo/scripts" deploy-nest.sh -C "$smoke_dir" "$unit.sql" |
  ssh "$host" "UNIT=$unit SETTINGS='$settings'; "'set -euo pipefail
d=$(mktemp -d); trap "rm -rf $d" EXIT
tar -xf - -C "$d"
# A ceiling the kernel enforces if the budget check misses; MemoryHigh sits just under it because page
# cache counts toward the unit. Applied by the restart below, and reverted with it on failure.
cap=/etc/systemd/system/$UNIT.service.d/zzz-engine.conf
sudo -n cp "$cap" "$d/zzz-engine.conf.prev"
sudo -n sed -i -E "s/^MemoryHigh=.*/MemoryHigh=1900M/; s/^MemoryMax=.*/MemoryMax=2G/" "$cap"
sudo -n bash "$d/deploy-nest.sh" env "$UNIT" $SETTINGS --smoke "$d/$UNIT.sql" \
  || { sudo -n cp "$d/zzz-engine.conf.prev" "$cap"; sudo -n systemctl daemon-reload; sudo -n systemctl restart "$UNIT"; exit 1; }
env_file=$HOME/release-gate/env/qos-nest.env
pid=$(systemctl show -p MainPID --value "$UNIT")
keys="NUTHATCH_(SQL_MAX_CONCURRENCY|ANALYTICS_MEMORY_LIMIT|ANALYTICS_THREADS|ANALYTICS_MAX_TEMP_SIZE|ENGINE|BURRMILL_MEMORY_LIMIT|MAX_RSS|SQL_MEMO_BYTES|HOT_STORE_CACHE_BYTES|INGESTION_RESERVATION|RUNTIME_HEADROOM)"
body=$(sudo -n cat "/proc/$pid/environ" | tr "\0" "\n" | grep -E "^$keys=")
{ echo "# Production environment of $UNIT on this box, the NUTHATCH_* budget settings read from its running"
  echo "# process on $(date -u +%Y-%m-%dT%H:%M:%SZ) by qos-budget-from-mac.sh. Re-run the installer when the unit changes."
  printf "%s\n" "$body"; } >"$env_file.new"
mv "$env_file.new" "$env_file"
echo "ok   release gate budget for qos-nest now reads:"; sed "s/^/       /" "$env_file"'
