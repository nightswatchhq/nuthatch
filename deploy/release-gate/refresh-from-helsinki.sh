#!/usr/bin/env bash
# refresh-from-helsinki.sh - the release gate's GATE_REFRESH on the ThinkPad (#1774): bring the
# allocations nest copy up to date from Helsinki over the tailnet.
#
# Helsinki's forced command (deploy/release-gate/helsinki/gate-export.sh) stages a copy whose redb
# was checked not to have moved while it was taken, and prints its PROVENANCE. This pulls the stage
# with rsync beside the current copy, hardlinking unchanged segments from it, checks that what
# arrived is what was staged (the PROVENANCE, and the redb by its sha256) and is not older than the
# copy it replaces, and only then swaps it in. The previous copy is kept as <copy>.prev. Any failure
# leaves the current copy as it was and exits 1. A redb `serve` will still not open is caught by the
# gate's own startup, which exits 2.
#
# Environment:
#   GATE_NEST          the copy                     (default ${GATE_STATE:-~/release-gate}/alloc-nest)
#   GATE_HELSINKI      ssh destination              (default root@100.82.188.91)
#   GATE_SSH_KEY       the key Helsinki forces to gate-export (default ~/.ssh/nuthatch-gate)
#   GATE_REMOTE_STAGE  Helsinki's stage directory    (default /var/lib/nuthatch-gate/stage)
set -euo pipefail

nest=${GATE_NEST:-${GATE_STATE:-$HOME/release-gate}/alloc-nest}
nest=${nest%/}
host=${GATE_HELSINKI:-root@100.82.188.91}
key=${GATE_SSH_KEY:-$HOME/.ssh/nuthatch-gate}
remote=${GATE_REMOTE_STAGE:-/var/lib/nuthatch-gate/stage}
remote=${remote%/}

say() { echo "refresh-from-helsinki: $*" >&2; }
die() { say "$*"; exit 1; }
trap 'rc=$?; say "internal error at line $LINENO (exit $rc)"; exit 1' ERR

command -v ssh >/dev/null || die "ssh is not on PATH"
command -v rsync >/dev/null || die "rsync is not on PATH"
[ -f "$key" ] || die "no ssh key at $key; Helsinki authorises it for the gate-export command only"
case "$key" in *[[:space:]]*) die "the key path may not contain whitespace: $key" ;; esac

sha() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}
prov_key() { sed -n "s/^$1=//p" "$2" | tail -n 1; }

ssh_cmd="ssh -i $key -o BatchMode=yes -o IdentitiesOnly=yes -o ConnectTimeout=20"
incoming=$nest.incoming
snap=$(mktemp "${TMPDIR:-/tmp}/gate-provenance.XXXXXX")
# The gate's lock on the copy: a refresh run by hand waits for a gate serving it, and one run as
# GATE_REFRESH rides on release-gate-run.sh's.
# shellcheck source=../../scripts/gate/lock.sh
. "$(cd "$(dirname "$0")" && pwd)/../../scripts/gate/lock.sh"
trap 'rm -rf "$snap" "$incoming"; gate_unlock' EXIT
mkdir -p "$(dirname "$nest")"
gate_lock "$nest" || die "could not take the lock on $nest"

say "asking $host for a snapshot"
$ssh_cmd "$host" snapshot >"$snap" || die "Helsinki could not take a snapshot; nothing was changed"
want_sha=$(prov_key redb_sha256 "$snap")
new_sealed=$(prov_key sealed_through "$snap")
case "$want_sha" in '' | *[!0-9a-f]*) die "the snapshot's PROVENANCE has no redb_sha256: $(head -c 300 "$snap")" ;; esac
case "$new_sealed" in '' | *[!0-9]*) die "the snapshot's PROVENANCE has no sealed_through" ;; esac

rm -rf "$incoming"
link=()
[ -d "$nest/segments" ] && link=(--link-dest="$nest")
say "pulling $host:$remote/ into $incoming"
rsync -a --delete ${link[@]+"${link[@]}"} -e "$ssh_cmd" "$host:$remote/" "$incoming/" \
  || die "rsync from $host failed; nothing was changed"

[ -f "$incoming/PROVENANCE" ] || die "the pulled copy has no PROVENANCE"
cmp -s "$snap" "$incoming/PROVENANCE" \
  || die "the pulled copy is not the snapshot just taken (another snapshot replaced it?); nothing was changed"
[ -f "$incoming/nuthatch.toml" ] || die "the pulled copy has no nuthatch.toml"
[ -f "$incoming/segments/manifest.json" ] || die "the pulled copy has no segments/manifest.json"
[ -f "$incoming/nuthatch.redb" ] || die "the pulled copy has no nuthatch.redb"
got_sha=$(sha "$incoming/nuthatch.redb")
[ "$got_sha" = "$want_sha" ] \
  || die "the pulled nuthatch.redb does not match the one staged ($got_sha, want $want_sha); nothing was changed"

old_sealed=""
[ -f "$nest/PROVENANCE" ] && old_sealed=$(prov_key sealed_through "$nest/PROVENANCE")
if [ -n "$old_sealed" ] && [ "$new_sealed" -lt "$old_sealed" ]; then
  die "the snapshot's sealed_through $new_sealed is older than the copy's $old_sealed; nothing was changed"
fi

rm -rf "$nest.prev"
if [ -d "$nest" ]; then mv "$nest" "$nest.prev"; fi
mv "$incoming" "$nest"
say "refreshed $nest: sealed_through ${old_sealed:-unknown} -> $new_sealed, last_block $(prov_key last_block "$nest/PROVENANCE"), version $(prov_key version "$nest/PROVENANCE"), taken $(prov_key taken_at "$nest/PROVENANCE")"
