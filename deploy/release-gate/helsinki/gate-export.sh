#!/usr/bin/env bash
# nuthatch-gate-export - Helsinki's half of the release gate's copy refresh (#1774). It is the
# forced command of the ThinkPad's key in root's authorized_keys, so it answers two requests and
# refuses everything else:
#
#   snapshot                              stage a consistent copy of the allocations nest, print its
#                                         PROVENANCE
#   rsync --server --sender <flags> . S   the read-only rsync the ThinkPad pulls the stage S with
#
# The nest's redb is held open by its unit, and redb has no online backup. A copy is consistent when
# nothing wrote the store while it was taken, so the copy is checked rather than trusted: the redb
# and the segment manifest are hashed before and after, the copy is hashed, and /ready's last_block is
# read on both sides. Any difference means the ingest cycle wrote during the copy, and it is taken
# again. Default durability fsyncs every commit, so an unwritten window is a byte image of the last
# commit, which is what `serve` opens. Sealed segments are immutable and are hardlinked, not copied.
#
# Configuration: /etc/nuthatch/gate-export.env (GATE_EXPORT_CONF), KEY=VALUE lines:
#   NEST_DIR   the allocations nest's directory (required)
#   NEST_URL   its HTTP address                                    (default http://127.0.0.1:8107)
#   STAGE      where the copy is staged                            (default /var/lib/nuthatch-gate/stage)
# GATE_EXPORT_ATTEMPTS (default 10) and GATE_EXPORT_RETRY_SECS (default 20) bound the retries.
#
# Install, as root on Helsinki:
#   install -m 755 gate-export.sh /usr/local/bin/nuthatch-gate-export
#   printf 'NEST_DIR=/opt/nuthatch/<allocations nest>\n' > /etc/nuthatch/gate-export.env
#   and in /root/.ssh/authorized_keys, with the ThinkPad's ~/.ssh/nuthatch-gate.pub:
#   from="100.83.44.63",command="/usr/local/bin/nuthatch-gate-export",restrict ssh-ed25519 AAAA... nuthatch-gate
set -euo pipefail

say() { echo "nuthatch-gate-export: $*" >&2; }
die() { say "$*"; exit 1; }
trap 'rc=$?; say "internal error at line $LINENO (exit $rc)"; exit 1' ERR

conf=${GATE_EXPORT_CONF:-/etc/nuthatch/gate-export.env}
[ -f "$conf" ] || die "no configuration at $conf (NEST_DIR=...)"
conf_key() { sed -n "s/^$1=//p" "$conf" | tail -n 1; }
nest=$(conf_key NEST_DIR)
url=$(conf_key NEST_URL)
url=${url:-http://127.0.0.1:8107}
stage=$(conf_key STAGE)
stage=${stage:-/var/lib/nuthatch-gate/stage}
stage=${stage%/}
attempts=${GATE_EXPORT_ATTEMPTS:-10}
retry=${GATE_EXPORT_RETRY_SECS:-20}

refuse() { die "refused: ${1:0:200}"; }

sha() {
  if command -v sha256sum >/dev/null; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

# /ready's field $1, tolerating whitespace around the colon; empty when absent.
field() { printf '%s' "$2" | sed -n "s/.*\"$1\" *: *\"\{0,1\}\([0-9A-Za-z.+-]*\).*/\1/p" | head -n 1; }

ready() { curl -fsS -m 10 -A nuthatch-gate-export "$url/ready" || true; }

# Hardlink every file under $1 into $2. A file that vanishes mid-walk was unnamed by the manifest
# (a manifest change would fail the copy anyway); any other failure to link is fatal.
link_tree() {
  local src=$1 dst=$2 f
  (cd "$src" && find . -type d) | while IFS= read -r f; do mkdir -p "$dst/$f"; done
  (cd "$src" && find . -type f) | while IFS= read -r f; do
    ln "$src/$f" "$dst/$f" 2>/dev/null || [ ! -e "$src/$f" ] || exit 1
  done
}

snapshot() {
  [ -n "$nest" ] || die "NEST_DIR is not set in $conf"
  [ -f "$nest/nuthatch.redb" ] || die "no nuthatch.redb in $nest"
  [ -f "$nest/nuthatch.toml" ] || die "no nuthatch.toml in $nest"
  [ -f "$nest/segments/manifest.json" ] || die "no segments/manifest.json in $nest"
  command -v curl >/dev/null || die "curl is not on PATH"
  case "$attempts" in '' | *[!0-9]* | 0) die "GATE_EXPORT_ATTEMPTS must be a positive integer" ;; esac

  mkdir -p "$(dirname "$stage")"
  # Globals, not locals: the EXIT trap runs after this function has returned.
  lock=$stage.lock new=$stage.new
  mkdir "$lock" 2>/dev/null || die "another snapshot holds $lock; remove it if none is running"
  trap 'rm -rf "$lock" "$new"' EXIT

  local i=0 ok=0 r0 r1 b0 b1 h0 h1 hc m0 m1 mc e
  while [ "$i" -lt "$attempts" ]; do
    i=$((i + 1))
    rm -rf "$new"
    mkdir -p "$new/segments"
    r0=$(ready)
    b0=$(field last_block "$r0")
    [ -n "$b0" ] || die "the nest at $url did not answer /ready with a last_block"
    h0=$(sha "$nest/nuthatch.redb")
    m0=$(sha "$nest/segments/manifest.json")
    link_tree "$nest/segments" "$new/segments" || die "could not hardlink $nest/segments into $new"
    for e in "$nest"/* "$nest"/.[!.]*; do
      [ -e "$e" ] || continue
      case "${e##*/}" in segments | nuthatch.redb* | .git) continue ;; esac
      cp -Rp "$e" "$new/"
    done
    cp -p "$nest/nuthatch.redb" "$new/nuthatch.redb"
    hc=$(sha "$new/nuthatch.redb")
    mc=$(sha "$new/segments/manifest.json")
    h1=$(sha "$nest/nuthatch.redb")
    m1=$(sha "$nest/segments/manifest.json")
    r1=$(ready)
    b1=$(field last_block "$r1")
    if [ "$h0" = "$hc" ] && [ "$hc" = "$h1" ] && [ "$m0" = "$mc" ] && [ "$mc" = "$m1" ] \
      && [ "$b0" = "$b1" ]; then
      ok=1
      break
    fi
    say "attempt $i: the store moved during the copy (last_block $b0 -> ${b1:-none}); again in ${retry}s"
    [ "$i" -ge "$attempts" ] || sleep "$retry"
  done
  [ "$ok" -eq 1 ] || die "the store did not hold still for a copy in $attempts attempts"

  {
    echo "taken_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "nest_dir=$nest"
    echo "version=$(field version "$r1")"
    echo "last_block=$b1"
    echo "sealed_through=$(field sealed_through "$r1")"
    echo "redb_sha256=$hc"
    echo "manifest_sha256=$mc"
    echo "attempt=$i"
  } >"$new/PROVENANCE"
  rm -rf "$stage.old"
  if [ -d "$stage" ]; then mv "$stage" "$stage.old"; fi
  mv "$new" "$stage"
  rm -rf "$stage.old"
  cat "$stage/PROVENANCE"
}

pull() {
  local rest flags path f
  rest=${1#rsync --server --sender }
  path=${rest##* . }
  flags=${rest% . *}
  [ "$path" = "$stage/" ] || [ "$path" = "$stage" ] || refuse "$1"
  [ -n "$flags" ] && [ "$flags" != "$rest" ] || refuse "$1"
  for f in $flags; do
    case "$f" in
      -*[!A-Za-z0-9.]*) refuse "$1" ;;
      -?*) ;;
      *) refuse "$1" ;;
    esac
  done
  [ -f "$stage/PROVENANCE" ] || die "nothing is staged at $stage; ask for a snapshot first"
  # shellcheck disable=SC2086
  exec rsync --server --sender $flags . "$stage/"
}

cmd=${SSH_ORIGINAL_COMMAND:-${1:-}}
case "$cmd" in
  snapshot) snapshot ;;
  "rsync --server --sender "*) pull "$cmd" ;;
  *) refuse "$cmd" ;;
esac
