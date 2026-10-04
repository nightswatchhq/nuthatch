#!/usr/bin/env bash
# reference.sh - check a nuthatch binary's answers to a gate query set against DuckDB's, at the nest
# copy's sealed pin (#1796). The gate's own comparison is with the previous release, so a wrong
# answer two releases share passes it; this one is with an engine that is not nuthatch's.
#
#   scripts/gate/reference.sh [--out DIR] [--sealed-through N] [--env FILE] <nuthatch-binary> <nest-copy> <query-set>
#
#   --out DIR             keep the pin, both engines' answers and logs here (default: a temp dir)
#   --sealed-through N    the pin (default: sealed_through= in the copy's PROVENANCE)
#   --env FILE            the nest's production environment, as release-gate.sh --env (default PROD_ENV)
#
# GATE_DUCK is a burrmill-bench binary with `gate-duck` (burrmill crates/burrmill-bench), which runs
# each statement through DuckDB over the nest set up as nuthatch set it up before Burrmill, `_dec`
# columns and authored views included, and writes its answer as nuthatch's /sql body.
#
# The pin is a directory holding the copy's config, views and the sealed segments at or below N,
# with a manifest of only those, and no redb, so neither engine sees a hot row. The binary answers
# it with `nuthatch check --update` under production's budget (PROD_ENV), which runs each statement
# through the same path as /sql, cold-only and without /sql's row cap.
#
# Answers are compared as release-gate.sh compares them (gate/common.sh): keys sorted, floats to 12
# significant digits, rows sorted unless the statement has a top-level ORDER BY, a statement tagged
# volatile by its row count only.
# FAIL, exit 1: an answer that differs from DuckDB's, or a statement the binary does not answer.
# A statement DuckDB will not run is listed with DuckDB's reason and is not compared.
# Exit 0 is PASS; exit 2 is a usage or setup fault, not a verdict on the binary.
set -euo pipefail

die() { echo "reference: $*" >&2; exit 2; }
trap 'rc=$?; echo "reference: internal error at line $LINENO (exit $rc)" >&2; exit 2' ERR
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=common.sh
. "$here/common.sh"

out="" pin_at="" env_file=""
while [ $# -gt 0 ]; do
  case "$1" in
    --out) [ $# -ge 2 ] || die "--out needs a directory"; out=$2; shift 2 ;;
    --sealed-through) [ $# -ge 2 ] || die "--sealed-through needs a block"; pin_at=$2; shift 2 ;;
    --env) [ $# -ge 2 ] || die "--env needs a file"; env_file=$2; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    --*) die "unknown option $1" ;;
    *) break ;;
  esac
done
[ $# -eq 3 ] || die "usage: reference.sh [--out DIR] [--sealed-through N] [--env FILE] <nuthatch-binary> <nest-copy> <query-set>"
bin=$1 nest=$2 set_file=$3
[ -x "$bin" ] || die "not an executable: $bin"
[ -f "$nest/nuthatch.toml" ] || die "no nuthatch.toml in $nest"
[ -f "$nest/segments/manifest.json" ] || die "no segments/manifest.json in $nest"
[ -f "$set_file" ] || die "no query set at $set_file"
[ -z "$env_file" ] || gate_load_env "$env_file"
[ -n "${GATE_DUCK:-}" ] || die "GATE_DUCK is not set: it names the burrmill-bench binary that runs gate-duck"
[ -x "$GATE_DUCK" ] || die "GATE_DUCK is not an executable: $GATE_DUCK"
command -v jq >/dev/null || die "jq is not on PATH"
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
nest=$(cd "$nest" && pwd)
set_file=$(cd "$(dirname "$set_file")" && pwd)/$(basename "$set_file")

if [ -z "$pin_at" ] && [ -f "$nest/PROVENANCE" ]; then
  pin_at=$(sed -n 's/^sealed_through=//p' "$nest/PROVENANCE" | tail -n 1 | tr -d '\r')
fi
[ -n "$pin_at" ] || die "no pin: $nest has no PROVENANCE with sealed_through=, and no --sealed-through was given"
case "$pin_at" in *[!0-9]*) die "the pin must be a block number, not '$pin_at'" ;; esac

gate_load_set "$set_file"
for id in "${ids[@]}"; do
  case "$id" in *[!A-Za-z0-9._-]*) die "query id '$id' cannot name a file" ;; esac
done

# shellcheck source=lock.sh
. "$here/lock.sh"
trap gate_unlock EXIT
gate_lock "$nest" || die "could not take the lock on $nest"

if [ -z "$out" ]; then
  out=$(mktemp -d "${TMPDIR:-/tmp}/reference.XXXXXX")
else
  mkdir -p "$out"
fi
out=$(cd "$out" && pwd)
pin=$out/pin
rm -rf "$pin" "$out/duckdb" "$out/answers"
mkdir -p "$pin/segments" "$pin/checks" "$out/duckdb" "$out/answers/burrmill" "$out/answers/duckdb"

# Everything but the store, the segments and the copy's own checks, copied: `check` reads the
# config and ABIs, and the views must be the copy's.
for entry in "$nest"/* "$nest"/.[!.]*; do
  [ -e "$entry" ] || continue
  case "$(basename "$entry")" in
    segments | checks | nuthatch.redb* | PROVENANCE) ;;
    *) cp -R "$entry" "$pin/" ;;
  esac
done
jq --argjson pin "$pin_at" '.tables |= (map_values(map(select(.to_block <= $pin))) | with_entries(select(.value | length > 0)))' \
  "$nest/segments/manifest.json" >"$pin/segments/manifest.json"
kept=0
while IFS= read -r file; do
  [ -f "$nest/segments/$file" ] || die "the manifest names $file, which is not in $nest/segments"
  ln "$nest/segments/$file" "$pin/segments/$file" 2>/dev/null || ln -s "$nest/segments/$file" "$pin/segments/$file"
  kept=$((kept + 1))
done < <(jq -r '.tables[][].file' "$pin/segments/manifest.json")
listed=$(jq '[.tables[][]] | length' "$nest/segments/manifest.json")
[ "$kept" -gt 0 ] || die "no sealed segment at or below block $pin_at in $nest"

i=0
while [ $i -lt "$n" ]; do
  printf '%s\n' "${sqls[$i]}" >"$pin/checks/${ids[$i]}.sql"
  i=$((i + 1))
done

version=$("$bin" --version 2>/dev/null | head -n 1) || version="unknown"
echo "reference: $version against DuckDB, on $nest pinned at sealed_through $pin_at"
echo "reference: $kept of $listed sealed segments at or below the pin, no hot rows; $n statements from $set_file"
echo "reference: budget ${PROD_ENV[*]:-the defaults}"

# The binary first, then DuckDB, so the two never contend for the box.
env ${PROD_ENV[@]+"${PROD_ENV[@]}"} "$bin" check --update --dir "$pin" >"$out/burrmill.log" 2>&1 || true
"$GATE_DUCK" gate-duck "$pin" "$set_file" "$out/duckdb" >"$out/duckdb.log" 2>&1 \
  || { tail -n 20 "$out/duckdb.log" >&2; die "gate-duck failed; log: $out/duckdb.log"; }

# A view DuckDB would not define, for naming why a statement reading it was not compared.
view_fault_for() {
  local sql=$1 name file why
  [ -s "$out/duckdb/views.err" ] || return 0
  while IFS=$'\t' read -r name file why; do
    if printf '%s' "$sql" | grep -qiw -- "$name"; then
      printf 'it reads %s (%s), which DuckDB will not define: %s' "$name" "$file" "$why"
      return 0
    fi
  done <"$out/duckdb/views.err"
}

matched=0 counted=0 differs=0 failed=0 uncompared=0 differ_ids="" failed_ids="" skipped=""
echo
printf '%-9s %-36s %9s %9s  %s\n' VERDICT QUERY BURRMILL DUCKDB DETAIL
i=0
while [ $i -lt "$n" ]; do
  id=${ids[$i]} mode=${modes[$i]} verdict=ok note="" shown="" b_rows=- d_rows=-
  b_err="" d_err=""
  b_file=$pin/checks/expected/$id.json
  if grep -qF "● $id: recorded" "$out/burrmill.log" && [ -f "$b_file" ]; then
    jq '{rows: .}' "$b_file" >"$out/answers/burrmill/$id.json"
    b_rows=$(jq '.rows | length' "$out/answers/burrmill/$id.json")
  else
    b_err=$(grep -F "✗ $id: " "$out/burrmill.log" | head -n 1 | sed "s/^✗ $id: //" | head -c 240 || true)
    b_err=${b_err:-no answer recorded; see $out/burrmill.log}
  fi
  if [ -f "$out/duckdb/$id.json" ]; then
    d_rows=$(jq '.rows | length' "$out/duckdb/$id.json")
  else
    d_err=$(head -c 240 "$out/duckdb/$id.err" 2>/dev/null || true)
    d_err=${d_err:-gate-duck wrote nothing for it}
  fi
  if [ -n "$b_err" ]; then
    verdict=FAIL
    failed=$((failed + 1))
    failed_ids="$failed_ids${failed_ids:+, }$id"
    note="the binary did not answer: $b_err"
    [ -z "$d_err" ] || note="$note; DuckDB did not either"
  elif [ -n "$d_err" ]; then
    verdict=skip
    uncompared=$((uncompared + 1))
    why=$(view_fault_for "${sqls[$i]}")
    note="not compared: DuckDB will not run it: ${why:-$d_err}"
    skipped="$skipped  $id: ${why:-$d_err}"$'\n'
  elif [ "$mode" = volatile ]; then
    if [ "$b_rows" = "$d_rows" ]; then
      counted=$((counted + 1))
      note="volatile, so compared on its row count"
    else
      verdict=FAIL
      differs=$((differs + 1))
      differ_ids="$differ_ids${differ_ids:+, }$id"
      note="differs: $b_rows rows against DuckDB's $d_rows (volatile, so compared on its row count)"
    fi
  else
    b_digest=$(canon_answer "$out/answers/burrmill/$id.json" "$mode" "$out/answers/burrmill/$id.rows") \
      || die "could not canonicalise the binary's answer to $id"
    d_digest=$(canon_answer "$out/duckdb/$id.json" "$mode" "$out/answers/duckdb/$id.rows") \
      || die "could not canonicalise DuckDB's answer to $id"
    if [ "$b_digest" = "$d_digest" ]; then
      matched=$((matched + 1))
    else
      verdict=FAIL
      differs=$((differs + 1))
      differ_ids="$differ_ids${differ_ids:+, }$id"
      case "$mode" in
        ordered) how="in order" ;;
        sorted) how="sorted, having no top-level ORDER BY" ;;
        *) how="sorted, as it could not tell whether an ORDER BY is top-level" ;;
      esac
      note="differs from DuckDB (rows compared $how)"
      label="first differing row"
      # Still wrong under an ORDER BY. A statement whose ties let two engines order rows
      # differently needs a tiebreaker in its SQL or a volatile tag in the set.
      if [ "$mode" = ordered ] \
        && [ "$(LC_ALL=C sort "$out/answers/burrmill/$id.rows" | sha256_of /dev/stdin)" \
          = "$(LC_ALL=C sort "$out/answers/duckdb/$id.rows" | sha256_of /dev/stdin)" ]; then
        note="order differs from DuckDB: the same rows in another order under its top-level ORDER BY"
        label="first row out of place"
      fi
      shown=$(first_diff "$out/answers/burrmill/$id.rows" "$out/answers/duckdb/$id.rows" | awk -F'\t' -v label="$label" '{
        printf "          %s, row %d:\n", label, $1
        printf "            burrmill: %s\n", substr($2, 1, 400)
        printf "            duckdb:   %s", substr($3, 1, 400) }')
    fi
  fi
  printf '%-9s %-36s %9s %9s  %s\n' "$verdict" "$id" "$b_rows" "$d_rows" "$note"
  [ -z "$shown" ] || printf '%s\n' "$shown"
  i=$((i + 1))
done

echo
echo "reference: $n statements: $matched match DuckDB, $counted compared on row count only, $differs differ, $failed not answered by the binary, $uncompared not compared"
if [ -n "$skipped" ]; then
  echo "reference: not compared, as DuckDB will not run them:"
  printf '%s' "$skipped"
fi
if [ "$differs" -gt 0 ] || [ "$failed" -gt 0 ]; then
  named=""
  [ -z "$differ_ids" ] || named="differs from DuckDB: $differ_ids"
  [ -z "$failed_ids" ] || named="${named:+$named; }not answered: $failed_ids"
  echo "RESULT: FAIL - $named"
  exit 1
fi
echo "RESULT: PASS"
