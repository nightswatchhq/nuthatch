#!/usr/bin/env bash
# Install a nuthatch binary and roll units onto it, failing loudly at every step (#1060).
#
# Written after the 3.1.0 deploy, where two things went wrong quietly:
#
#   * `cp` over a running binary fails with "Text file busy". The ad-hoc script ignored it, the
#     service restarted on the OLD binary, and its readiness flipped false->true across the restart -
#     which looked exactly like the fix we were deploying and was the staleness clock resetting on a
#     2.7.1 process. A step whose failure is survivable will eventually lie to somebody.
#   * `graph-staking-legacy-readonly` had been on **2.7.1** for weeks beside units on 3.0.1, because
#     its unit names a path rather than a version and nothing ever compares the two.
#
# So: `mv`, never `cp`; assert the installed version before touching a unit; and a `check` mode that
# answers "what is actually running" without asking a human to remember.
set -euo pipefail

BIN_DIR=${NUTHATCH_BIN_DIR:-/usr/local/bin}
UNIT_DIR=${NUTHATCH_UNIT_DIR:-/etc/systemd/system}
die() { printf '\033[31mFAIL\033[0m %s\n' "$*" >&2; exit 1; }
ok()  { printf '\033[32mok\033[0m   %s\n' "$*"; }

unit_binary() { systemctl show -p ExecStart --value "$1" 2>/dev/null | grep -oE '/[^ ]*nuthatch[^ ]*' | head -1; }
# The address the unit serves on: its `--listen`, any host (the QoS nest listens on a tailnet address),
# with 0.0.0.0 read as loopback and nuthatch's default when there is none. `|| true` because a grep
# that matches nothing would otherwise end the script under `pipefail` before any message.
unit_port() {
  local l
  l=$(systemctl show -p ExecStart --value "$1" 2>/dev/null | grep -oE -- '--listen[= ][^ ;]+' | head -1 | sed -E 's/^--listen[= ]//' || true)
  l=${l/0.0.0.0:/127.0.0.1:}
  echo "${l:-127.0.0.1:8288}"
}

# --- install ------------------------------------------------------------------------------------
# `mv` rather than `cp`: rename replaces the inode atomically and works against a running binary,
# which is exactly what `cp` cannot do.
cmd_install() {
  local src=$1 want=$2
  [ -f "$src" ] || die "no binary at $src"
  local dst="$BIN_DIR/nuthatch-$want"
  cp "$src" "$dst.partial"
  chmod +x "$dst.partial"
  local got
  got=$("$dst.partial" --version 2>/dev/null | awk '{print $2}') || die "$src will not run"
  [ "$got" = "$want" ] || die "asked to install $want, binary reports $got - refusing"
  mv -f "$dst.partial" "$dst"
  ok "installed $dst ($($dst --version))"
}

# --- roll one unit ------------------------------------------------------------------------------
# The file holding the ExecStart systemd will run: the last of the unit file and its drop-ins, in the
# order systemd reads them, that sets a non-empty one. A drop-in that clears and resets ExecStart
# overrides the unit file, and editing the unit file then rolls nothing (#1729).
execstart_file() {
  local u=$1 last="" f
  for f in "$UNIT_DIR/$u.service" "$UNIT_DIR/$u.service.d"/*.conf; do
    [ -f "$f" ] && grep -qE '^ExecStart=/' "$f" && last=$f
  done
  [ -n "$last" ] && echo "$last"
}

# A field /ready may not carry (a `serve`-only unit has no last_block) reads as empty, not as a failure.
ready_field() {
  echo "$1" | grep -oE "\"$2\"[[:space:]]*:[[:space:]]*(\"[^\"]*\"|[a-z0-9]+)" | head -1 |
    sed -E 's/^[^:]*:[[:space:]]*//; s/"//g' || true
}

cmd_roll() {
  local u=$1 want=$2
  local target="$BIN_DIR/nuthatch-$want"
  [ -x "$target" ] || die "$target is not installed; run 'install' first"
  local got
  got=$("$target" --version | awk '{print $2}')
  [ "$got" = "$want" ] || die "$target reports $got, not $want"

  local port f before
  port=$(unit_port "$u")
  f=$(execstart_file "$u") || die "$u: no file under $UNIT_DIR sets its ExecStart"
  before=$(ready_field "$(curl -sS -m5 "http://$port/ready" 2>/dev/null || true)" last_block)

  # Point the unit at the VERSIONED path, whatever it named before, so `systemctl show` can answer
  # "which version is this" (#1060).
  # `-i.bak` then remove it: the one spelling GNU and BSD sed both read as in-place.
  sed -i.bak -E "s#^ExecStart=([^ ]*/)?nuthatch(-[0-9][0-9A-Za-z.+-]*)?#ExecStart=$target#" "$f"
  rm -f "$f.bak"
  systemctl daemon-reload
  # What systemd will run, not what this script wrote: the two differ when another file wins.
  [ "$(unit_binary "$u")" = "$target" ] \
    || die "$u: systemd would still run $(unit_binary "$u") after editing $f"
  systemctl restart "$u"

  # Readiness flips false to true across any restart, a no-op roll included. The version /ready
  # reports is the one proof that the new binary is the one serving.
  local s="" v="" r=""
  for _ in $(seq 1 60); do
    sleep "${ROLL_POLL_SECS:-2}"
    s=$(curl -sS -m5 "http://$port/ready" 2>/dev/null || true)
    v=$(ready_field "$s" version)
    r=$(ready_field "$s" ready)
    [ "$v" = "$want" ] && [ "$r" = true ] && break
  done
  [ "$(systemctl is-active "$u")" = active ] || die "$u is not active after restart"
  [ -n "$s" ] || die "$u never answered /ready"
  [ "$v" = "$want" ] || die "$u reports version ${v:-none} on /ready, not $want"
  [ "$r" = true ] || die "$u is on $want but not ready: $s"
  ok "$u -> $want via $(basename "$f")   last_block ${before:-n/a} -> $(ready_field "$s" last_block)"
}

# --- check --------------------------------------------------------------------------------------
# The question nobody could answer on 2026-09-01: what is every unit actually running?
cmd_check() {
  local bad=0 seen=0
  printf '%-38s %-9s %-28s %s\n' UNIT ACTIVE BINARY VERSION
  # **Every** unit directory systemd reads, not just the one operators usually edit. Globbing
  # `/etc/systemd/system` alone means a unit shipped in `/lib` or generated into `/run` is invisible
  # to a check whose entire purpose is answering "what is running" - and it would report all clear
  # while never having looked at it. Review of #1060.
  local dirs=(/etc/systemd/system /run/systemd/system /lib/systemd/system /usr/lib/systemd/system)
  local files=()
  for d in "${dirs[@]}"; do
    [ -d "$d" ] || continue
    for f in "$d"/*.service; do [ -e "$f" ] && files+=("$f"); done
  done
  # A unit in /etc shadows the same name elsewhere, exactly as systemd resolves it, so the same
  # service is not counted twice under two paths.
  local seen_names=" "
  for f in "${files[@]}"; do
    local u b v act
    u=$(basename "$f" .service)
    case "$seen_names" in *" $u "*) continue ;; esac
    seen_names="$seen_names$u "
    grep -q nuthatch "$f" 2>/dev/null || continue
    # `|| true` on every one of these: `systemctl is-active` exits non-zero for an inactive unit,
    # and under `set -e` that assignment kills the loop at the first disabled service - which is
    # how the first version of this printed a header and no rows. A check that silently examines
    # nothing is worse than no check, and it is the same fault this script exists to prevent.
    b=$(unit_binary "$u" || true)
    act=$(systemctl is-active "$u" 2>/dev/null || true)
    local ident
    ident=$([ -x "$b" ] && { "$b" --version 2>/dev/null; } || echo '')
    v=$(echo "$ident" | awk '$1=="nuthatch"{print $2}')
    [ -n "$v" ] || v="-"
    if [ "$act" = active ] && { [ -z "$v" ] || [ "$v" = "-" ]; }; then
      printf '%-38s %-9s %-28s %s   (not a nuthatch binary - not enforced)\n' "$u" "$act" "$b" "-"
      continue
    fi
    seen=$((seen + 1))
    printf '%-38s %-9s %-28s %s\n' "$u" "$act" "$b" "$v"
    # Only active units are enforced: a disabled unit pointing at an unversioned path harms nobody
    # until someone enables it, and failing on those makes the check noise people learn to ignore.
    # Enforced only where the binary **identifies itself as nuthatch**. Two wrong rules preceded
    # this one: `*/nuthatch-*` accepted `nuthatch-gateway` (a different product) as versioned, and
    # `*/nuthatch-[0-9]*` then flagged that same gateway for not following a scheme it has no reason
    # to follow. The honest rule is about identity, not spelling: if it says it is nuthatch, its
    # path must say which nuthatch.
    if [ "$act" = active ] && [ -n "$v" ] && [ "$v" != "-" ]; then
      if [[ "$b" != */nuthatch-[0-9]* ]]; then
        printf '  ^ active unit names an unversioned binary: systemctl cannot say what it runs\n'
        bad=1
      else
        # And the path must not *lie*. A versioned name is only useful if it matches what the binary
        # reports - `nuthatch-3.1.0` containing a 2.7.1 build reads as informative and is worse than
        # the unversioned path it replaced, because now nobody thinks to check. Review of #1060.
        local named=${b##*/nuthatch-}
        if [ "$named" != "$v" ]; then
          printf '  ^ path says %s, binary reports %s - the version in the name is wrong\n' "$named" "$v"
          bad=1
        fi
      fi
    fi
  done
  # A floor, so "examined nothing" can never read as "all clear".
  [ "$seen" -gt 0 ] || die "found no nuthatch units at all - this check examined nothing"
  [ "$bad" = 0 ] || die "at least one active unit names an unversioned binary"
  ok "every active unit names a versioned binary ($seen unit(s) examined)"
}

case "${1:-}" in
  install) shift; cmd_install "$@" ;;
  roll)    shift; cmd_roll "$@" ;;
  check)   shift; cmd_check "$@" ;;
  *) cat >&2 <<USAGE
usage:
  deploy-nest.sh install <path-to-binary> <version>   install and verify it reports that version
  deploy-nest.sh roll    <unit> <version>             point the unit at the versioned path, restart, verify
  deploy-nest.sh check                                what is every unit actually running
USAGE
     exit 2 ;;
esac
