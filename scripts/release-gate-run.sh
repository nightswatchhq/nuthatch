#!/usr/bin/env bash
# release-gate-run.sh - the ThinkPad's side of the release gate (#1749): fetch a release
# candidate's Linux binary, run scripts/release-gate.sh against the allocations nest copy, and
# report the verdict on the candidate's commit as a status. See docs/release-gate.md.
#
#   scripts/release-gate-run.sh <tag>                     gate a published release or candidate
#   scripts/release-gate-run.sh --poll                    gate the newest ungated release that is
#                                                         newer than production; none, exit 0 quietly
#   scripts/release-gate-run.sh --binary PATH --sha SHA   gate a local build of commit SHA
#   options: --production <tag>  the version production runs, measured as the baseline
#                                (default: the latest full release that is not the candidate)
#            --no-status         run and print, post nothing
#
# The candidate is compared with the production version measured on the same box, the same copy
# and the same day, so neither a refreshed copy nor a different machine reads as a regression.
# Production over its own budget, or failing statements itself, is still the baseline: the
# candidate is judged on its own refusals, regressions, differing answers and peak.
# The status never blocks the tag: the tag exists before the gate runs, and the context is not a
# required check anywhere. A red status stops the roll (deploy-nest.sh), not the release.
#
# Environment:
#   GATE_STATE    working directory: binaries and run logs             (default ~/release-gate)
#   GATE_NEST     the allocations nest copy, segments plus redb        (default $GATE_STATE/alloc-nest)
#   GATE_SET      the query set                  (default scripts/gate/alloc-queries.tsv beside this)
#   GATE_REFRESH  a command run before the gate to refresh GATE_NEST, e.g. the rsync from Helsinki;
#                 unset means the copy is used as it stands
#   GATE_REPO     (default nightswatchhq/nuthatch)
#   GATE_TARGET   release asset target         (default x86_64-unknown-linux-gnu)
#   GATE_PASSES   passes per binary            (default 3)
#   GATE_CONCURRENCY  statements in flight at once (default 2, as 8107's SQL_MAX_CONCURRENCY)
set -euo pipefail

CONTEXT=release-gate/alloc-nest
here=$(cd "$(dirname "$0")" && pwd)
state=${GATE_STATE:-$HOME/release-gate}
nest=${GATE_NEST:-$state/alloc-nest}
set_file=${GATE_SET:-$here/gate/alloc-queries.tsv}
repo=${GATE_REPO:-nightswatchhq/nuthatch}
target=${GATE_TARGET:-x86_64-unknown-linux-gnu}
passes=${GATE_PASSES:-3}
concurrency=${GATE_CONCURRENCY:-2}

log() { echo "release-gate-run: $*" >&2; }
die() { log "$*"; exit 2; }
trap 'rc=$?; log "internal error at line $LINENO (exit $rc)"; exit 2' ERR

tag="" poll=0 local_bin="" sha="" production="" post=1
while [ $# -gt 0 ]; do
  case "$1" in
    --poll) poll=1; shift ;;
    --binary) [ $# -ge 2 ] || die "--binary needs a path"; local_bin=$2; shift 2 ;;
    --sha) [ $# -ge 2 ] || die "--sha needs a commit"; sha=$2; shift 2 ;;
    --production) [ $# -ge 2 ] || die "--production needs a tag"; production=$2; shift 2 ;;
    --no-status) post=0; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    --*) die "unknown option $1" ;;
    *) [ -z "$tag" ] || die "one tag at a time"; tag=$1; shift ;;
  esac
done
modes=$(( poll + (${#tag} > 0 ? 1 : 0) + (${#local_bin} > 0 ? 1 : 0) ))
[ "$modes" -eq 1 ] || die "give exactly one of <tag>, --poll, --binary PATH --sha SHA"
[ -z "$local_bin" ] || [ -n "$sha" ] || die "--binary needs --sha: the status has to land on a commit"
command -v gh >/dev/null || die "gh is not on PATH"
[ -f "$set_file" ] || die "no query set at $set_file"

mkdir -p "$state/bins" "$state/runs" "$(dirname "$nest")"
# One gate at a time on the copy, held across the refresh and both runs; release-gate.sh takes the
# same lock, so a run by hand waits for this one, and the children here ride on it.
# shellcheck source=gate/lock.sh
. "$here/gate/lock.sh"
status_posted=0
cleanup() {
  rc=$?
  gate_unlock
  if [ "$rc" -ne 0 ] && [ "$rc" -ne 1 ] && [ "$status_posted" -eq 0 ] && [ -n "$sha" ]; then
    post_status error "the gate could not run (exit $rc); see $state/runs on the gate box" || true
  fi
}
trap cleanup EXIT
gate_lock "$nest" || die "could not take the lock on $nest"

post_status() {
  local st=$1 desc=$2
  desc=${desc:0:140}
  if [ "$post" -eq 0 ]; then
    log "status (not posted): $CONTEXT $st - $desc"
    return 0
  fi
  gh api -X POST "repos/$repo/statuses/$sha" -f state="$st" -f context="$CONTEXT" \
    -f description="$desc" >/dev/null
  # A pending status left behind by a dying run would read as "still running" for ever.
  [ "$st" = pending ] || status_posted=1
  log "posted $CONTEXT $st on $sha: $desc"
}

sha256_check() {
  local dir=$1 file=$2
  if command -v sha256sum >/dev/null; then
    (cd "$dir" && sha256sum -c "$file.sha256" >/dev/null)
  else
    (cd "$dir" && shasum -a 256 -c "$file.sha256" >/dev/null)
  fi
}

# Download and verify one release's binary; prints its path.
fetch() {
  local t=$1 dir=$state/bins/$1 asset=nuthatch-$target.tar.gz
  if [ ! -x "$dir/nuthatch" ]; then
    rm -rf "$dir"
    mkdir -p "$dir"
    gh release download "$t" -R "$repo" -p "$asset" -p "$asset.sha256" -D "$dir" >&2 \
      || die "could not download $asset from $t"
    sha256_check "$dir" "$asset" || die "$asset from $t fails its checksum"
    tar -xzf "$dir/$asset" -C "$dir"
    local found
    found=$(find "$dir" -type f -name nuthatch | head -n 1)
    [ -n "$found" ] || die "no nuthatch binary inside $asset from $t"
    [ "$found" = "$dir/nuthatch" ] || mv "$found" "$dir/nuthatch"
    chmod +x "$dir/nuthatch"
  fi
  printf '%s\n' "$dir/nuthatch"
}

commit_of() { gh api "repos/$repo/commits/$1" --jq .sha; }

# The newest status for the context decides. A pending one older than GATE_STALE_SECS is a run that
# died before posting its verdict, so the release is gated again rather than left pending forever.
has_status() {
  gh api "repos/$repo/commits/$1/statuses" --jq "[.[] | select(.context == \"$CONTEXT\")] | first
    | if . == null then \"none\"
      elif .state == \"pending\" and (now - (.updated_at | fromdate)) > ${GATE_STALE_SECS:-7200} then \"stale\"
      else .state end" | grep -qvE '^(none|stale)$'
}

full_releases=""
if [ -z "$production" ]; then
  full_releases=$(gh release list -R "$repo" -L 20 --exclude-pre-releases --json tagName,isDraft \
    --jq '.[] | select(.isDraft | not) | .tagName') || die "could not list the releases of $repo"
fi
# The version production runs when the candidate is $1: --production, or the latest full release
# that is not the candidate.
production_for() {
  if [ -n "$production" ]; then
    printf '%s\n' "$production"
  else
    printf '%s\n' "$full_releases" | grep -vxF "$1" | head -n 1 || true
  fi
}

is_num() { case "$1" in '' | *[!0-9]*) return 1 ;; esac; }

# semver_gt A B: tag A is a newer version than tag B. A leading v and any +build are ignored, a
# prerelease sorts before its release, and prerelease identifiers compare as numbers where both are.
# A tag that is not a version is never newer.
semver_gt() {
  local a=${1#v} b=${2#v} ac bc ap bp a1 a2 a3 b1 b2 b3 rest x y
  a=${a%%+*} b=${b%%+*}
  ac=${a%%-*} bc=${b%%-*}
  ap=${a#"$ac"} bp=${b#"$bc"}
  ap=${ap#-} bp=${bp#-}
  IFS=. read -r a1 a2 a3 rest <<<"$ac"
  IFS=. read -r b1 b2 b3 rest <<<"$bc"
  for x in "$a1" "$a2" "$a3" "$b1" "$b2" "$b3"; do is_num "${x:-0}" || return 1; done
  a1=${a1:-0} a2=${a2:-0} a3=${a3:-0} b1=${b1:-0} b2=${b2:-0} b3=${b3:-0}
  if [ "$a1" -ne "$b1" ]; then [ "$a1" -gt "$b1" ]; return; fi
  if [ "$a2" -ne "$b2" ]; then [ "$a2" -gt "$b2" ]; return; fi
  if [ "$a3" -ne "$b3" ]; then [ "$a3" -gt "$b3" ]; return; fi
  [ "$ap" != "$bp" ] || return 1
  [ -n "$ap" ] || return 0
  [ -n "$bp" ] || return 1
  while :; do
    x=${ap%%.*} y=${bp%%.*}
    if [ "$x" != "$y" ]; then
      if is_num "$x" && is_num "$y"; then [ "$x" -gt "$y" ]; return; fi
      if is_num "$x"; then return 1; fi
      if is_num "$y"; then return 0; fi
      [ "$x" \> "$y" ]
      return
    fi
    # Equal so far: the one with fewer identifiers is the older.
    [ "$ap" != "$x" ] || return 1
    [ "$bp" != "$y" ] || return 0
    ap=${ap#*.} bp=${bp#*.}
  done
}

if [ "$poll" -eq 1 ]; then
  # Newest first. A draft is skipped: its assets are still being built, and it is not yet a release.
  # A release at or below production is never a candidate: it was superseded, not missed.
  listed=$(gh release list -R "$repo" -L 20 --json tagName,isDraft \
    --jq '.[] | select(.isDraft | not) | .tagName') || die "could not list the releases of $repo"
  for t in $listed; do
    base=$(production_for "$t")
    [ -n "$base" ] || die "no full release to measure as production; pass --production"
    semver_gt "$t" "$base" || continue
    c=$(commit_of "$t")
    if ! has_status "$c"; then tag=$t; break; fi
  done
  [ -n "$tag" ] || exit 0
  log "gating $tag, the newest release newer than production without a $CONTEXT status"
fi

if [ -n "$tag" ]; then
  sha=$(commit_of "$tag")
  label=$tag
else
  [ -x "$local_bin" ] || die "not an executable: $local_bin"
  label="local build of ${sha:0:12}"
fi

production=$(production_for "${tag:-}")
[ -n "$production" ] || die "no full release to measure as production; pass --production"
[ "$production" != "$tag" ] || die "the candidate and production are the same release ($tag)"

post_status pending "running $label against the allocations nest copy, baseline $production"

if [ -n "${GATE_REFRESH:-}" ]; then
  log "refreshing the copy: $GATE_REFRESH"
  sh -c "$GATE_REFRESH" >&2 || die "GATE_REFRESH failed"
fi

prod_bin=$(fetch "$production")
if [ -n "$tag" ]; then cand_bin=$(fetch "$tag"); else cand_bin=$local_bin; fi

run=$state/runs/$(date -u +%Y%m%dT%H%M%SZ)-${tag:-${sha:0:12}}
mkdir -p "$run"

log "measuring production $production for the baseline"
rc=0
"$here/release-gate.sh" --passes "$passes" --concurrency "$concurrency" --out "$run/production" \
  --write-baseline "$run/baseline.tsv" "$prod_bin" "$nest" "$set_file" >"$run/production.txt" 2>&1 || rc=$?
# Production failing its own gate is still what production does, so it is still the baseline. Only
# a run that could not measure it (exit 2, no baseline written) leaves nothing to compare with.
if { [ "$rc" -ne 0 ] && [ "$rc" -ne 1 ]; } || [ ! -s "$run/baseline.tsv" ]; then
  cat "$run/production.txt" >&2
  post_status error "production $production could not be measured (exit $rc), so there is no baseline; see $run"
  exit 2
fi
prod_note=""
if [ "$rc" -eq 1 ]; then
  prod_failed=$(sed -n 's/^RESULT: FAIL - failed: //p' "$run/production.txt" | sed -e 's/, peak RSS$//' -e 's/^peak RSS$//')
  prod_peak=$(sed -n 's/^release-gate: peak RSS \([0-9]*\) MiB, budget \([0-9]*\) MiB: OVER$/\1 \2/p' "$run/production.txt")
  if [ -n "$prod_failed" ]; then
    n_failed=$(printf '%s\n' "$prod_failed" | awk -F', ' '{ print NF }')
    if [ "$n_failed" -eq 1 ]; then they="it cannot"; else they="they cannot"; fi
    log "production $production fails $prod_failed itself, so $they regress; the candidate still fails on any it refuses"
    prod_note="$prod_note, which fails $n_failed itself"
  fi
  if [ -n "$prod_peak" ]; then
    log "production $production peaked at ${prod_peak% *} MiB, over the ${prod_peak#* } MiB budget; the candidate is held to the budget on its own peak"
    if [ -n "$prod_failed" ]; then prod_note="$prod_note and"; else prod_note="$prod_note, which"; fi
    prod_note="$prod_note peaked at ${prod_peak% *} MiB, over budget"
  fi
fi

log "gating $label"
rc=0
"$here/release-gate.sh" --passes "$passes" --concurrency "$concurrency" --out "$run/candidate" \
  --baseline "$run/baseline.tsv" "$cand_bin" "$nest" "$set_file" >"$run/candidate.txt" 2>&1 || rc=$?
cat "$run/candidate.txt"

# The counts only: the p99 is in the run's output, and the status has 140 characters.
summary=$(grep -E '^release-gate: [0-9]+ of [0-9]+ answered' "$run/candidate.txt" | sed -e 's/^release-gate: //' -e 's/;.*//' || true)
result=$(grep -E '^RESULT: ' "$run/candidate.txt" | sed 's/^RESULT: //' || true)
case "$rc" in
  0) post_status success "${summary:-passed} (against $production$prod_note)" ;;
  1) post_status failure "${summary:-failed}; ${result#FAIL - } (against $production$prod_note)" ; exit 1 ;;
  *) post_status error "the gate could not run (exit $rc); see $run" ; exit 2 ;;
esac
