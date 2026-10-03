#!/usr/bin/env bash
# release-gate.sh - run a nuthatch binary against a copy of a production nest and its consumers'
# recorded queries, under production's budget, and say whether it may be rolled (#1749).
#
#   scripts/release-gate.sh [options] <nuthatch-binary> <nest-copy-dir> <query-set>
#
#   --baseline FILE        compare times against FILE (written earlier with --write-baseline)
#   --write-baseline FILE  record this run's results as a baseline
#   --passes N             measure every query N times, each on a freshly started server (default 3)
#   --out DIR              keep the server logs and per-pass results here (default: a temp dir)
#   --timeout SECS         per-query timeout (default 300)
#
# The copy needs its sealed segments *and* its nuthatch.redb: without the redb it serves no sealed
# history. `serve` never writes either. The query set is a TSV, one statement per line:
# id<TAB>consumer<TAB>call site<TAB>sql, `#` for comments (scripts/gate/alloc-queries.tsv).
#
# FAIL, exit 1, on any of: an error or refusal from /sql, an out-of-memory, a degraded answer, the
# server dying, and with --baseline a time regression past these bounds (medians over the passes):
#   per query: slower than 2x its baseline AND more than 1000 ms slower   (GATE_QUERY_FACTOR/_SLACK_MS)
#   p99 of the set: slower than 1.5x the baseline's AND more than 1000 ms (GATE_P99_FACTOR/_SLACK_MS)
# Both conditions must hold, so a 40 ms query taking 90 ms is noise, not a regression.
# Exit 0 is PASS; exit 2 is a usage or setup fault, which is not a verdict on the binary.
set -euo pipefail

# Production's budget: the environment the allocations nest runs under on the Lodestar box (unit
# nuthatch-alloc, port 8107), copied from its systemd unit on 2026-10-03. Change it here when the
# unit changes, or the gate tests a budget nobody runs.
PROD_ENV=(
  NUTHATCH_SQL_MAX_CONCURRENCY=2
  NUTHATCH_ANALYTICS_MEMORY_LIMIT=256MB
  NUTHATCH_ENGINE=burrmill
  NUTHATCH_BURRMILL_MEMORY_LIMIT=2GB
  NUTHATCH_ANALYTICS_THREADS=8
  NUTHATCH_MAX_RSS=6GB
)

QUERY_FACTOR=${GATE_QUERY_FACTOR:-2}
QUERY_SLACK_MS=${GATE_QUERY_SLACK_MS:-1000}
P99_FACTOR=${GATE_P99_FACTOR:-1.5}
P99_SLACK_MS=${GATE_P99_SLACK_MS:-1000}

die() { echo "release-gate: $*" >&2; exit 2; }
trap 'rc=$?; echo "release-gate: internal error at line $LINENO (exit $rc)" >&2; exit 2' ERR

baseline="" write_baseline="" passes=3 out="" timeout=300
while [ $# -gt 0 ]; do
  case "$1" in
    --baseline) [ $# -ge 2 ] || die "--baseline needs a file"; baseline=$2; shift 2 ;;
    --write-baseline) [ $# -ge 2 ] || die "--write-baseline needs a file"; write_baseline=$2; shift 2 ;;
    --passes) [ $# -ge 2 ] || die "--passes needs a number"; passes=$2; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out needs a directory"; out=$2; shift 2 ;;
    --timeout) [ $# -ge 2 ] || die "--timeout needs seconds"; timeout=$2; shift 2 ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    --*) die "unknown option $1" ;;
    *) break ;;
  esac
done
[ $# -eq 3 ] || die "usage: release-gate.sh [options] <nuthatch-binary> <nest-copy-dir> <query-set>"
bin=$1 nest=$2 set_file=$3
case "$passes" in ''|*[!0-9]*|0) die "--passes must be a positive integer" ;; esac
case "$timeout" in ''|*[!0-9]*|0) die "--timeout must be a positive integer" ;; esac
[ -x "$bin" ] || die "not an executable: $bin"
[ -f "$nest/nuthatch.toml" ] || die "no nuthatch.toml in $nest"
[ -f "$nest/nuthatch.redb" ] || die "no nuthatch.redb in $nest: a copy without its redb serves no sealed history"
[ -f "$set_file" ] || die "no query set at $set_file"
[ -z "$baseline" ] || [ -f "$baseline" ] || die "no baseline at $baseline"
command -v curl >/dev/null || die "curl is not on PATH"

if [ -z "$out" ]; then
  out=$(mktemp -d "${TMPDIR:-/tmp}/release-gate.XXXXXX")
else
  mkdir -p "$out"
fi

# The set, loaded once: parallel arrays indexed by query number.
ids=() consumers=() sqls=()
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in ''|'#'*) continue ;; esac
  id=$(printf '%s' "$line" | cut -f1)
  consumer=$(printf '%s' "$line" | cut -f2)
  q=$(printf '%s' "$line" | cut -f4-)
  [ -n "$id" ] && [ -n "$q" ] || die "malformed line in $set_file (want id<TAB>consumer<TAB>site<TAB>sql): ${line:0:80}"
  for seen in ${ids[@]+"${ids[@]}"}; do
    [ "$seen" != "$id" ] || die "duplicate query id $id in $set_file"
  done
  ids+=("$id"); consumers+=("$consumer"); sqls+=("$q")
done < "$set_file"
n=${#ids[@]}
[ "$n" -gt 0 ] || die "no queries in $set_file"

server_pid=""
port=""
stop_server() {
  if [ -n "$server_pid" ]; then
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
  fi
}
trap stop_server EXIT

alive() { [ -n "$server_pid" ] && kill -0 "$server_pid" 2>/dev/null; }

# Start `serve` on a free port under PROD_ENV. A port somebody else holds makes serve exit, so a
# few random ports are tried before giving up.
start_server() {
  local log=$1 tries=0
  while [ $tries -lt 5 ]; do
    tries=$((tries + 1))
    port=$(( 20000 + (RANDOM % 20000) ))
    if curl -s -m 1 -o /dev/null "http://127.0.0.1:$port/health"; then continue; fi
    env "${PROD_ENV[@]}" "$bin" serve --dir "$nest" --listen "127.0.0.1:$port" >"$log" 2>&1 &
    server_pid=$!
    local waited=0
    while [ $waited -lt 240 ]; do
      if ! alive; then
        wait "$server_pid" 2>/dev/null || true
        server_pid=""
        break
      fi
      if curl -fs -m 2 -o /dev/null "http://127.0.0.1:$port/health"; then return 0; fi
      sleep 0.5
      waited=$((waited + 1))
    done
    if alive; then
      stop_server
      die "serve did not answer /health within 120s; log: $log"
    fi
    grep -qi 'address already in use' "$log" || {
      echo "release-gate: serve exited at startup; last lines of $log:" >&2
      tail -n 20 "$log" >&2
      exit 2
    }
  done
  die "could not find a free port after $tries tries"
}

ms_of() { awk -v s="$1" 'BEGIN { printf "%d", s * 1000 + 0.5 }'; }

# One query against the running server. Prints: status<TAB>rows<TAB>ms<TAB>truncated<TAB>detail
run_query() {
  local q=$1 body=$out/body.json code_time rc=0 code secs ms detail
  code_time=$(curl -sS -m "$timeout" -o "$body" -w '%{http_code} %{time_total}' \
    --get "http://127.0.0.1:$port/sql" --data-urlencode "q=$q" 2>"$out/curl.err") || rc=$?
  if [ $rc -ne 0 ]; then
    if alive; then
      if [ $rc -eq 28 ]; then
        printf 'timeout\t-\t%s\t-\tno answer within %ss\n' "$((timeout * 1000))" "$timeout"
      else
        printf 'transport\t-\t-\t-\tcurl exit %s: %s\n' "$rc" "$(head -c 160 "$out/curl.err" | tr '\n' ' ')"
      fi
    else
      printf 'died\t-\t-\t-\tthe server exited under this query (out of memory or a crash; see its log)\n'
    fi
    return 0
  fi
  code=${code_time%% *}
  secs=${code_time#* }
  ms=$(ms_of "$secs")
  local head
  head=$(head -c 64 "$body")
  if [ "$code" = 200 ] && [ "${head#\{\"count\":}" != "$head" ]; then
    local rows truncated=false
    rows=$(head -c 64 "$body" | sed -n 's/^{"count":\([0-9]*\).*/\1/p')
    grep -q '"truncated":true' "$body" && truncated=true
    if grep -q '"degraded":true' "$body"; then
      printf 'degraded\t%s\t%s\t%s\tthe answer is degraded: %s\n' "$rows" "$ms" "$truncated" \
        "$(grep -o '"degraded_tables":\[[^]]*\]' "$body" | head -c 160)"
      return 0
    fi
    printf 'ok\t%s\t%s\t%s\t\n' "${rows:-0}" "$ms" "$truncated"
    return 0
  fi
  detail=$(sed -n 's/^{"error":"\(.*\)"}$/\1/p' "$body" | head -c 400)
  [ -n "$detail" ] || detail=$(head -c 200 "$body")
  detail=$(printf '%s' "$detail" | sed 's/\\n/ | /g' | head -c 240)
  local status=error
  if printf '%s' "$detail" | grep -qiE 'out of memory|memory limit|oom'; then
    status=oom
  elif [ "$code" = 503 ] || [ "$code" = 429 ] || printf '%s' "$detail" | grep -qiE 'refus|busy|budget'; then
    status=refused
  fi
  printf '%s\t-\t%s\t-\thttp %s: %s\n' "$status" "$ms" "$code" "$detail"
}

version=$("$bin" --version 2>/dev/null | head -n 1) || version="unknown"
echo "release-gate: $version against $nest"
echo "release-gate: $n queries from $set_file, $passes pass(es), results in $out"
echo "release-gate: budget ${PROD_ENV[*]}"

# Each pass on a fresh server: the nest memoises answers by statement text, so a second ask of the
# same statement on one server would time the memo.
pass=1
while [ "$pass" -le "$passes" ]; do
  : >"$out/pass-$pass.tsv"
  start_server "$out/serve-pass-$pass.log"
  if [ "$pass" -eq 1 ]; then
    prov=$(curl -s -m 30 --get "http://127.0.0.1:$port/sql" --data-urlencode "q=SELECT 1 AS one" \
      | grep -o '"as_of":[0-9]*,"sealed_through":[0-9]*' || true)
    echo "release-gate: nest provenance ${prov:-unknown}"
  fi
  i=0
  while [ $i -lt "$n" ]; do
    if ! alive; then
      # It answered the previous query and then died; that query is charged with it.
      [ "$i" -eq 0 ] || printf '%s\n' "${ids[$((i - 1))]}" >>"$out/died-after"
      stop_server
      start_server "$out/serve-pass-$pass-restart-$i.log"
    fi
    r=$(run_query "${sqls[$i]}")
    printf '%s\t%s\n' "${ids[$i]}" "$r" >>"$out/pass-$pass.tsv"
    i=$((i + 1))
  done
  stop_server
  pass=$((pass + 1))
done

# Fold the passes: a query's status is its worst pass, its time the median of its passes.
results=$out/results.tsv
: >"$results"
i=0
while [ $i -lt "$n" ]; do
  id=${ids[$i]}
  status=ok rows=- truncated=- detail="" times=""
  pass=1
  while [ "$pass" -le "$passes" ]; do
    row=$(awk -F'\t' -v id="$id" '$1 == id' "$out/pass-$pass.tsv")
    s=$(printf '%s' "$row" | cut -f2)
    if [ "$s" = ok ]; then
      [ "$rows" != - ] || rows=$(printf '%s' "$row" | cut -f3)
      truncated=$(printf '%s' "$row" | cut -f5)
      times="$times $(printf '%s' "$row" | cut -f4)"
    elif [ "$status" = ok ]; then
      status=$s
      detail=$(printf '%s' "$row" | cut -f6-)
      [ "$(printf '%s' "$row" | cut -f3)" = - ] || rows=$(printf '%s' "$row" | cut -f3)
    fi
    pass=$((pass + 1))
  done
  if [ "$status" = ok ] && [ -f "$out/died-after" ] && grep -qxF "$id" "$out/died-after"; then
    status=died
    detail="answered, then the server exited before the next query (out of memory or a crash)"
  fi
  med=-
  if [ "$status" = ok ]; then
    med=$(printf '%s\n' $times | sort -n | awk '{ a[NR] = $1 } END { print a[int((NR + 1) / 2)] }')
  fi
  # No field may be empty but the last: `read` with a tab IFS folds empty fields away.
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "${consumers[$i]:--}" "$status" "${rows:--}" "$med" \
    "${truncated:--}" "$detail" >>"$results"
  i=$((i + 1))
done

p99_of() {
  awk -F'\t' '!/^#/ && $3 == "ok" { print $5 }' "$1" | sort -n \
    | awk '{ a[NR] = $1 } END { if (NR == 0) { print "-" } else { i = int(NR * 0.99); if (i < NR * 0.99) i++; print a[i] } }'
}

base_val() { awk -F'\t' -v id="$1" -v col="$2" '!/^#/ && $1 == id { print $col }' "$baseline"; }

failures=0 regressions=0
echo
printf '%-9s %-36s %9s %9s  %s\n' STATUS QUERY ROWS MS DETAIL
while IFS=$'\t' read -r id consumer status rows med truncated detail; do
  note="" verdict=ok
  if [ "$status" != ok ]; then
    verdict=FAIL
    failures=$((failures + 1))
    sql=""
    i=0
    while [ $i -lt "$n" ]; do [ "${ids[$i]}" = "$id" ] && sql=${sqls[$i]}; i=$((i + 1)); done
    # Name what it reads: "indexer.daily failed" is less use to a reader than the view that broke.
    reads=$(printf '%s' "$sql" | grep -oE '(lodestar_[a-z_]+|[a-z0-9_]+__[a-z0-9_]+)' | sort -u | paste -sd, - || true)
    note="$status (${consumer}; reads ${reads:-?}): ${detail:0:170}"
  else
    [ "$truncated" = true ] && note="truncated at the row cap"
    if [ -n "$baseline" ]; then
      b_status=$(base_val "$id" 3)
      b_ms=$(base_val "$id" 5)
      b_rows=$(base_val "$id" 4)
      if [ -z "$b_status" ]; then
        note="${note:+$note; }not in the baseline"
      elif [ "$b_status" = ok ]; then
        if awk -v m="$med" -v b="$b_ms" -v f="$QUERY_FACTOR" -v s="$QUERY_SLACK_MS" \
          'BEGIN { exit !(m > b * f && m > b + s) }'; then
          verdict=SLOW
          regressions=$((regressions + 1))
          note="${note:+$note; }regressed: ${med} ms against a baseline of ${b_ms} ms (bound: >${QUERY_FACTOR}x and >${QUERY_SLACK_MS} ms slower)"
        else
          note="${note:+$note; }baseline ${b_ms} ms"
        fi
        [ "$b_rows" = "$rows" ] || note="$note; rows ${rows} against the baseline's ${b_rows}"
      else
        note="${note:+$note; }the baseline failed this query"
      fi
    fi
  fi
  printf '%-9s %-36s %9s %9s  %s\n' "$verdict" "$id" "$rows" "$med" "$note"
done <"$results"

p99=$(p99_of "$results")
p99_line="p99 ${p99} ms over the queries that answered"
p99_failed=0
if [ -n "$baseline" ]; then
  b_p99=$(p99_of "$baseline")
  p99_line="$p99_line, baseline ${b_p99} ms"
  if [ "$p99" != - ] && [ "$b_p99" != - ] && awk -v m="$p99" -v b="$b_p99" -v f="$P99_FACTOR" -v s="$P99_SLACK_MS" \
    'BEGIN { exit !(m > b * f && m > b + s) }'; then
    p99_failed=1
    p99_line="$p99_line: REGRESSED (bound: >${P99_FACTOR}x and >${P99_SLACK_MS} ms slower)"
  fi
fi

if [ -n "$write_baseline" ]; then
  {
    echo "# release-gate baseline: $version, $(date -u +%Y-%m-%dT%H:%M:%SZ), $passes pass(es)"
    echo "# nest: $nest ${prov:-}"
    echo "# set: $set_file"
    echo "# id<TAB>consumer<TAB>status<TAB>rows<TAB>median ms<TAB>truncated<TAB>detail"
    cat "$results"
  } >"$write_baseline"
  echo
  echo "release-gate: baseline written to $write_baseline"
fi

echo
echo "release-gate: $((n - failures)) of $n answered, $failures failed, $regressions regressed; $p99_line"
if [ "$failures" -gt 0 ] || [ "$regressions" -gt 0 ] || [ "$p99_failed" -gt 0 ]; then
  failed_ids=$(awk -F'\t' '$3 != "ok" { printf "%s%s", sep, $1; sep = ", " }' "$results")
  echo "RESULT: FAIL${failed_ids:+ - failed: $failed_ids}"
  exit 1
fi
echo "RESULT: PASS"
