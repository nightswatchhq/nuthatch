#!/usr/bin/env bash
# release-gate-run.sh - the ThinkPad's side of the release gate (#1749): fetch a release
# candidate's Linux binary, run scripts/release-gate.sh against the allocations nest copy, and
# report the verdict on the candidate's commit as a status. See docs/release-gate.md.
#
#   scripts/release-gate-run.sh <tag>                     gate a published release or candidate
#   scripts/release-gate-run.sh --poll                    gate the newest release not yet gated
#   scripts/release-gate-run.sh --binary PATH --sha SHA   gate a local build of commit SHA
#   options: --production <tag>  the version production runs, measured as the baseline
#                                (default: the latest full release that is not the candidate)
#            --no-status         run and print, post nothing
#
# The candidate is compared with the production version measured on the same box, the same copy
# and the same day, so neither a refreshed copy nor a different machine reads as a regression.
# The status never blocks the tag: the tag exists before the gate runs, and the context is not a
# required check anywhere. A red status stops the roll (deploy-nest.sh), not the release.
#
# Environment:
#   GATE_STATE    working directory: binaries, run logs, the lock      (default ~/release-gate)
#   GATE_NEST     the allocations nest copy, segments plus redb        (default $GATE_STATE/alloc-nest)
#   GATE_SET      the query set                  (default scripts/gate/alloc-queries.tsv beside this)
#   GATE_REFRESH  a command run before the gate to refresh GATE_NEST, e.g. the rsync from Helsinki;
#                 unset means the copy is used as it stands
#   GATE_REPO     (default nightswatchhq/nuthatch)
#   GATE_TARGET   release asset target         (default x86_64-unknown-linux-gnu)
#   GATE_PASSES   passes per binary            (default 3)
set -euo pipefail

CONTEXT=release-gate/alloc-nest
here=$(cd "$(dirname "$0")" && pwd)
state=${GATE_STATE:-$HOME/release-gate}
nest=${GATE_NEST:-$state/alloc-nest}
set_file=${GATE_SET:-$here/gate/alloc-queries.tsv}
repo=${GATE_REPO:-nightswatchhq/nuthatch}
target=${GATE_TARGET:-x86_64-unknown-linux-gnu}
passes=${GATE_PASSES:-3}

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
    -h|--help) sed -n '2,29p' "$0"; exit 0 ;;
    --*) die "unknown option $1" ;;
    *) [ -z "$tag" ] || die "one tag at a time"; tag=$1; shift ;;
  esac
done
modes=$(( poll + (${#tag} > 0 ? 1 : 0) + (${#local_bin} > 0 ? 1 : 0) ))
[ "$modes" -eq 1 ] || die "give exactly one of <tag>, --poll, --binary PATH --sha SHA"
[ -z "$local_bin" ] || [ -n "$sha" ] || die "--binary needs --sha: the status has to land on a commit"
command -v gh >/dev/null || die "gh is not on PATH"
[ -f "$set_file" ] || die "no query set at $set_file"

mkdir -p "$state/bins" "$state/runs"
# One gate at a time: two would contend for the copy's redb lock and for the box.
lock=$state/lock
mkdir "$lock" 2>/dev/null || die "another gate holds $lock (remove it if no gate is running)"
status_posted=0
cleanup() {
  rc=$?
  rmdir "$lock" 2>/dev/null || true
  if [ "$rc" -ne 0 ] && [ "$rc" -ne 1 ] && [ "$status_posted" -eq 0 ] && [ -n "$sha" ]; then
    post_status error "the gate could not run (exit $rc); see $state/runs on the gate box" || true
  fi
}
trap cleanup EXIT

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

if [ "$poll" -eq 1 ]; then
  # Newest first. A draft is skipped: its assets are still being built, and it is not yet a release.
  for t in $(gh release list -R "$repo" -L 5 --json tagName,isDraft \
    --jq '.[] | select(.isDraft | not) | .tagName'); do
    c=$(commit_of "$t")
    if ! has_status "$c"; then tag=$t; break; fi
  done
  if [ -z "$tag" ]; then
    log "every recent release already carries $CONTEXT; nothing to do"
    exit 0
  fi
  log "gating $tag, the newest release without a $CONTEXT status"
fi

if [ -n "$tag" ]; then
  sha=$(commit_of "$tag")
  label=$tag
else
  [ -x "$local_bin" ] || die "not an executable: $local_bin"
  label="local build of ${sha:0:12}"
fi

if [ -z "$production" ]; then
  production=$(gh release list -R "$repo" -L 20 --exclude-pre-releases --json tagName,isDraft \
    --jq '.[] | select(.isDraft | not) | .tagName' | grep -vxF "${tag:-}" | head -n 1 || true)
  [ -n "$production" ] || die "no full release to measure as production; pass --production"
fi
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
"$here/release-gate.sh" --passes "$passes" --out "$run/production" \
  --write-baseline "$run/baseline.tsv" "$prod_bin" "$nest" "$set_file" >"$run/production.txt" 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
  cat "$run/production.txt" >&2
  post_status error "production $production fails its own gate (exit $rc), so there is no baseline; see $run"
  exit 2
fi

log "gating $label"
rc=0
"$here/release-gate.sh" --passes "$passes" --out "$run/candidate" \
  --baseline "$run/baseline.tsv" "$cand_bin" "$nest" "$set_file" >"$run/candidate.txt" 2>&1 || rc=$?
cat "$run/candidate.txt"

summary=$(grep -E '^release-gate: [0-9]+ of [0-9]+ answered' "$run/candidate.txt" | sed 's/^release-gate: //' || true)
result=$(grep -E '^RESULT: ' "$run/candidate.txt" | sed 's/^RESULT: //' || true)
case "$rc" in
  0) post_status success "${summary:-passed} (against $production)" ;;
  1) post_status failure "${summary:-failed}; ${result#FAIL - }" ; exit 1 ;;
  *) post_status error "the gate could not run (exit $rc); see $run" ; exit 2 ;;
esac
