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
#   --concurrency N        statements in flight at once (default 1)
#
# At --concurrency N the set goes out N statements at a time in its own order: statements 1..N
# together, then N+1..2N once all of the first group have answered, and so on, the same groups on
# every pass. The set lists each kittiwake call site's statements together, so a pair is mostly two
# statements one route sends at once (its tokio::join! sites); the refresh job's run in sequence in
# kittiwake but alongside the warmer, which fires on the same minute. Each statement keeps its own
# time, and $out/schedule-pass-N.tsv records group, id and start and end in ms.
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
# Also FAIL on the serving process's peak RSS over the 2 GiB per-cursor budget (GATE_MAX_RSS_MB).
# On Linux the peak is the kernel's high-water mark, VmHWM in /proc/<pid>/status (GATE_PROC_ROOT),
# read as each server is stopped, the largest kept. RSS is also sampled every half second, which is
# all macOS has and can miss a spike between samples; that figure is reported alongside. At each new
# sampled peak the binary's /metrics is read too, so a binary that exports the analytics pool and jemalloc gauges
# (#1778) reports where the peak sat.
# With --baseline, also FAIL ("answer differs") on a statement whose answer is not the baseline's.
# Each answer is kept canonical in <out>/answers/<id>.rows (keys sorted, floats to 12 significant
# digits, rows sorted unless the statement has a top-level ORDER BY) and compared by its sha256.
# A statement tagged `# volatile: <id> <why>` in the set is compared on its row count only.
# Exit 0 is PASS; exit 2 is a usage or setup fault, which is not a verdict on the binary.
# Two runs against one copy wait for each other: the second `serve` could not open the redb.
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
MAX_RSS_MB=${GATE_MAX_RSS_MB:-2048}
PROC_ROOT=${GATE_PROC_ROOT:-/proc}

die() { echo "release-gate: $*" >&2; exit 2; }
trap 'rc=$?; echo "release-gate: internal error at line $LINENO (exit $rc)" >&2; exit 2' ERR

baseline="" write_baseline="" passes=3 out="" timeout=300 concurrency=1
while [ $# -gt 0 ]; do
  case "$1" in
    --concurrency) [ $# -ge 2 ] || die "--concurrency needs a number"; concurrency=$2; shift 2 ;;
    --baseline) [ $# -ge 2 ] || die "--baseline needs a file"; baseline=$2; shift 2 ;;
    --write-baseline) [ $# -ge 2 ] || die "--write-baseline needs a file"; write_baseline=$2; shift 2 ;;
    --passes) [ $# -ge 2 ] || die "--passes needs a number"; passes=$2; shift 2 ;;
    --out) [ $# -ge 2 ] || die "--out needs a directory"; out=$2; shift 2 ;;
    --timeout) [ $# -ge 2 ] || die "--timeout needs seconds"; timeout=$2; shift 2 ;;
    -h|--help) sed -n '2,39p' "$0"; exit 0 ;;
    --*) die "unknown option $1" ;;
    *) break ;;
  esac
done
[ $# -eq 3 ] || die "usage: release-gate.sh [options] <nuthatch-binary> <nest-copy-dir> <query-set>"
bin=$1 nest=$2 set_file=$3
case "$passes" in ''|*[!0-9]*|0) die "--passes must be a positive integer" ;; esac
case "$timeout" in ''|*[!0-9]*|0) die "--timeout must be a positive integer" ;; esac
case "$concurrency" in ''|*[!0-9]*|0) die "--concurrency must be a positive integer" ;; esac
[ -x "$bin" ] || die "not an executable: $bin"
[ -f "$nest/nuthatch.toml" ] || die "no nuthatch.toml in $nest"
[ -f "$nest/nuthatch.redb" ] || die "no nuthatch.redb in $nest: a copy without its redb serves no sealed history"
[ -f "$set_file" ] || die "no query set at $set_file"
[ -z "$baseline" ] || [ -f "$baseline" ] || die "no baseline at $baseline"
command -v curl >/dev/null || die "curl is not on PATH"
command -v jq >/dev/null || die "jq is not on PATH"
if command -v sha256sum >/dev/null; then
  sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null; then
  sha256_of() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  die "neither sha256sum nor shasum is on PATH"
fi

# shellcheck source=gate/lock.sh
. "$(dirname "$0")/gate/lock.sh"
trap gate_unlock EXIT
gate_lock "$nest" || die "could not take the lock on $nest"

if [ -z "$out" ]; then
  out=$(mktemp -d "${TMPDIR:-/tmp}/release-gate.XXXXXX")
else
  mkdir -p "$out"
fi
# Absolute, because a baseline names its answers directory for the run that compares against it.
out=$(cd "$out" && pwd)
rm -rf "$out/answers"
mkdir -p "$out/answers"

# The set, loaded once: parallel arrays indexed by query number.
ids=() consumers=() sqls=() volatile=" "
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    '# volatile: '*)
      v=${line#'# volatile: '}
      v=${v%% *}
      [ -n "$v" ] || die "a volatile tag without an id in $set_file"
      volatile="$volatile$v "
      continue
      ;;
    ''|'#'*) continue ;;
  esac
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

# ordered: a top-level ORDER BY, so row order is part of the answer. sorted: none, so rows are
# compared as a set. sorted:unsure: a comment, a stray quote or unbalanced parentheses, so it could
# not tell, and sorts. Parentheses hide a window's or a subquery's ORDER BY; quotes hide literals.
order_of() {
  printf '%s\n' "$1" | awk '
    { s = s $0 " " }
    END {
      n = length(s); depth = 0; top = ""; unsure = 0; i = 1
      while (i <= n) {
        c = substr(s, i, 1)
        if (c == "\047" || c == "\"") {
          j = i + 1; closed = 0
          while (j <= n) {
            if (substr(s, j, 1) == c) {
              if (substr(s, j + 1, 1) == c) { j += 2; continue }
              closed = 1; break
            }
            j++
          }
          if (!closed) unsure = 1
          i = j + 1; top = top " "; continue
        }
        if (c == "-" && substr(s, i + 1, 1) == "-") unsure = 1
        if (c == "/" && substr(s, i + 1, 1) == "*") unsure = 1
        if (c == "(") depth++
        else if (c == ")") { depth--; if (depth < 0) unsure = 1 }
        else if (depth == 0) top = top toupper(c)
        i++
      }
      if (depth != 0) unsure = 1
      gsub(/[ \t\r\n]+/, " ", top)
      if (unsure) print "sorted:unsure"
      else if (top ~ /(^|[^A-Z0-9_])ORDER BY([^A-Z0-9_]|$)/) print "ordered"
      else print "sorted"
    }'
}

# How each statement's answer is compared: volatile (row count only) or its order_of.
modes=()
i=0
while [ $i -lt "$n" ]; do
  case "$volatile" in
    *" ${ids[$i]} "*) modes+=(volatile) ;;
    *) modes+=("$(order_of "${sqls[$i]}")") ;;
  esac
  i=$((i + 1))
done
for v in $volatile; do
  case " ${ids[*]} " in *" $v "*) ;; *) die "volatile tag for $v, which is not in $set_file" ;; esac
done

# One row per line, keys sorted. A number written as an integer is kept exactly; any other is
# rounded to 12 significant digits, so a float summed in another order still compares equal. So is
# a string in exponent form: a DOUBLE cast to VARCHAR, which no integer or DECIMAL renders as.
CANON_JQ='
def canon_float:
  if . == 0 then 0
  else
    (if . < 0 then -1 else 1 end) as $sign
    | fabs as $a
    | ($a | log10 | floor) as $e
    | (if $e >= 11 then $a / pow(10; $e - 11) else $a * pow(10; 11 - $e) end | round) as $m
    | [$m, $e - 11]
    | until(.[0] % 10 != 0; [.[0] / 10, .[1] + 1])
    | (if .[1] >= 0 then .[0] * pow(10; .[1]) else .[0] / pow(10; -.[1]) end) * $sign
  end;
.rows[] | walk(
  if type == "number" and (tojson | test("^-?[0-9]+$") | not) then canon_float
  elif type == "string" and test("^-?[0-9]+(\\.[0-9]+)?[eE][-+]?[0-9]+$") then tonumber | canon_float | tostring
  else . end)'

# canon_answer <body> <mode> <dest>: writes the canonical rows to dest and prints their sha256.
canon_answer() {
  jq -c -S "$CANON_JQ" "$1" >"$3.tmp" || return 1
  if [ "$2" = ordered ]; then
    mv "$3.tmp" "$3" || return 1
  else
    LC_ALL=C sort "$3.tmp" >"$3" || return 1
    rm -f "$3.tmp"
  fi
  sha256_of "$3"
}

server_pid=""
sampler_pid=""
# Keeps the highest RSS (KiB) seen for the serving process in $out/rss-peak-kb, across servers.
sample_rss() {
  local pid=$1 file=$2 k p
  while kill -0 "$pid" 2>/dev/null; do
    k=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ' || true)
    p=$(cat "$file" 2>/dev/null || true); p=${p:-0}
    if [ -n "$k" ] && [ "$k" -gt "$p" ]; then
      echo "$k" >"$file.tmp" && mv "$file.tmp" "$file"
      metrics_now | awk '$1 ~ /^nuthatch_(analytics_pool_reserved_bytes|analytics_engines|jemalloc_allocated_bytes|jemalloc_resident_bytes)$/' \
        >"$out/rss-peak-gauges.tmp" || true
      mv "$out/rss-peak-gauges.tmp" "$out/rss-peak-gauges"
    fi
    sleep 0.5
  done
}
metrics_now() { curl -s -m 1 "http://127.0.0.1:$port/metrics" 2>/dev/null || true; }
port=""
# Keeps the largest VmHWM (KiB) in $out/hwm-peak-kb. Where /proc exists but a server's status has no
# VmHWM (it exited before it was stopped), the server is listed in $out/hwm-unread instead.
read_hwm() {
  local hwm p
  [ -d "$PROC_ROOT" ] || return 0
  hwm=$(awk '$1 == "VmHWM:" { print $2 }' "$PROC_ROOT/$1/status" 2>/dev/null || true)
  case "$hwm" in
    ''|*[!0-9]*) echo "$1" >>"$out/hwm-unread"; return 0 ;;
  esac
  p=$(cat "$out/hwm-peak-kb" 2>/dev/null || echo 0)
  if [ "$hwm" -gt "$p" ]; then echo "$hwm" >"$out/hwm-peak-kb"; fi
}
stop_server() {
  if [ -n "$server_pid" ]; then
    read_hwm "$server_pid"
    # Since start, so read before the server goes; the largest over every server is kept.
    local pool p
    pool=$(metrics_now | awk '$1 == "nuthatch_analytics_pool_peak_bytes" { print $2 }')
    p=$(cat "$out/pool-peak-bytes" 2>/dev/null || echo 0)
    if [ -n "$pool" ] && [ "$pool" -gt "$p" ]; then echo "$pool" >"$out/pool-peak-bytes"; fi
    # Waited for, so it cannot be killed half way through replacing the peak file.
    [ -z "$sampler_pid" ] || { kill "$sampler_pid" 2>/dev/null; wait "$sampler_pid" 2>/dev/null; } || true
    sampler_pid=""
    kill "$server_pid" 2>/dev/null || true
    wait "$server_pid" 2>/dev/null || true
    server_pid=""
  fi
}
trap 'stop_server; gate_unlock' EXIT

alive() { [ -n "$server_pid" ] && kill -0 "$server_pid" 2>/dev/null; }

# Start `serve` on a free port under PROD_ENV. A port somebody else holds makes serve exit, so a
# few random ports are tried before giving up.
start_server() {
  local log=$1 tries=0
  while [ $tries -lt 5 ]; do
    tries=$((tries + 1))
    port=$(( 20000 + (RANDOM % 20000) ))
    if curl -s -m 1 -o /dev/null "http://127.0.0.1:$port/health"; then continue; fi
    env "${PROD_ENV[@]}" "$bin" serve --dir "$nest" --listen "127.0.0.1:$port" >"$log" 2>&1 9>&- &
    server_pid=$!
    sample_rss "$server_pid" "$out/rss-peak-kb" &
    sampler_pid=$!
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
now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time() * 1000'; }

# One query against the running server in flight slot <slot>, its answer kept canonical in
# <rows-file>. Prints: status<TAB>rows<TAB>ms<TAB>truncated<TAB>digest<TAB>detail
run_query() {
  local q=$1 mode=$2 rows_file=$3 body=$out/body-$4.json err=$out/curl-$4.err code_time rc=0 code secs ms detail digest
  code_time=$(curl -sS -m "$timeout" -o "$body" -w '%{http_code} %{time_total}' \
    --get "http://127.0.0.1:$port/sql" --data-urlencode "q=$q" 2>"$err") || rc=$?
  if [ $rc -ne 0 ]; then
    if alive; then
      if [ $rc -eq 28 ]; then
        printf 'timeout\t-\t%s\t-\t-\tno answer within %ss\n' "$((timeout * 1000))" "$timeout"
      else
        printf 'transport\t-\t-\t-\t-\tcurl exit %s: %s\n' "$rc" "$(head -c 160 "$err" | tr '\n' ' ')"
      fi
    else
      printf 'died\t-\t-\t-\t-\tthe server exited under this query (out of memory or a crash; see its log)\n'
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
      printf 'degraded\t%s\t%s\t%s\t-\tthe answer is degraded: %s\n' "$rows" "$ms" "$truncated" \
        "$(grep -o '"degraded_tables":\[[^]]*\]' "$body" | head -c 160)"
      return 0
    fi
    if ! digest=$(canon_answer "$body" "$mode" "$rows_file" 2>"$out/canon-$4.err"); then
      printf 'error\t%s\t%s\t%s\t-\tthe answer could not be canonicalised: %s\n' "${rows:-0}" "$ms" "$truncated" \
        "$(head -c 160 "$out/canon-$4.err" | tr '\n' ' ')"
      return 0
    fi
    printf 'ok\t%s\t%s\t%s\t%s\t\n' "${rows:-0}" "$ms" "$truncated" "$digest"
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
  printf '%s\t-\t%s\t-\t-\thttp %s: %s\n' "$status" "$ms" "$code" "$detail"
}

version=$("$bin" --version 2>/dev/null | head -n 1) || version="unknown"
echo "release-gate: $version against $nest"
echo "release-gate: $n queries from $set_file, $passes pass(es), results in $out"
echo "release-gate: budget ${PROD_ENV[*]}"
echo "release-gate: concurrency $concurrency (the set's statements sent $concurrency at a time, in order)"

rm -f "$out/rss-peak-kb" "$out/hwm-peak-kb" "$out/hwm-unread" "$out/died-after" "$out/unstable" "$out/rss-peak-gauges" "$out/pool-peak-bytes"
# Each pass on a fresh server: the nest memoises answers by statement text, so a second ask of the
# same statement on one server would time the memo.
pass=1
while [ "$pass" -le "$passes" ]; do
  : >"$out/pass-$pass.tsv"
  mkdir -p "$out/answers/pass-$pass"
  start_server "$out/serve-pass-$pass.log"
  if [ "$pass" -eq 1 ]; then
    prov=$(curl -s -m 30 --get "http://127.0.0.1:$port/sql" --data-urlencode "q=SELECT 1 AS one" \
      | grep -o '"as_of":[0-9]*,"sealed_through":[0-9]*' || true)
    echo "release-gate: nest provenance ${prov:-unknown}"
  fi
  : >"$out/schedule-pass-$pass.tsv"
  i=0 group=0 sent=""
  while [ $i -lt "$n" ]; do
    if ! alive; then
      # It answered the previous group and then died; that group is charged with it.
      [ -z "$sent" ] || printf '%s\n' $sent >>"$out/died-after"
      stop_server
      start_server "$out/serve-pass-$pass-restart-$i.log"
    fi
    group=$((group + 1)) sent="" slot=0 pids=()
    while [ $slot -lt "$concurrency" ] && [ $((i + slot)) -lt "$n" ]; do
      j=$((i + slot))
      (
        start=$(now_ms)
        r=$(run_query "${sqls[$j]}" "${modes[$j]}" "$out/answers/pass-$pass/${ids[$j]}.rows" "$slot")
        printf '%s\t%s\t%s\t%s\n' "$group" "${ids[$j]}" "$start" "$(now_ms)" >"$out/slot-$slot.schedule"
        printf '%s\t%s\n' "${ids[$j]}" "$r" >"$out/slot-$slot.result"
      ) &
      pids+=("$!")
      sent="$sent ${ids[$j]}"
      slot=$((slot + 1))
    done
    wait "${pids[@]}"
    k=0
    while [ $k -lt $slot ]; do
      cat "$out/slot-$k.result" >>"$out/pass-$pass.tsv"
      cat "$out/slot-$k.schedule" >>"$out/schedule-pass-$pass.tsv"
      k=$((k + 1))
    done
    i=$((i + slot))
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
  status=ok rows=- truncated=- detail="" times="" digest=-
  pass=1
  while [ "$pass" -le "$passes" ]; do
    row=$(awk -F'\t' -v id="$id" '$1 == id' "$out/pass-$pass.tsv")
    s=$(printf '%s' "$row" | cut -f2)
    if [ "$s" = ok ]; then
      [ "$rows" != - ] || rows=$(printf '%s' "$row" | cut -f3)
      truncated=$(printf '%s' "$row" | cut -f5)
      times="$times $(printf '%s' "$row" | cut -f4)"
      d=$(printf '%s' "$row" | cut -f6)
      if [ "$digest" = - ]; then
        digest=$d
        cp "$out/answers/pass-$pass/$id.rows" "$out/answers/$id.rows"
      elif [ "$d" != "$digest" ]; then
        printf '%s\t%s\n' "$id" "$pass" >>"$out/unstable"
      fi
    elif [ "$status" = ok ]; then
      status=$s
      detail=$(printf '%s' "$row" | cut -f7-)
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
  else
    digest=-
  fi
  # No field may be empty but the last: `read` with a tab IFS folds empty fields away.
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$id" "${consumers[$i]:--}" "$status" "${rows:--}" "$med" \
    "${truncated:--}" "${digest:--}" "${modes[$i]}" "$detail" >>"$results"
  i=$((i + 1))
done

p99_of() {
  awk -F'\t' '!/^#/ && $3 == "ok" { print $5 }' "$1" | sort -n \
    | awk '{ a[NR] = $1 } END { if (NR == 0) { print "-" } else { i = int(NR * 0.99); if (i < NR * 0.99) i++; print a[i] } }'
}

base_val() { awk -F'\t' -v id="$1" -v col="$2" '!/^#/ && $1 == id { print $col }' "$baseline"; }

# first_diff <a> <b>: the first row at which two canonical answers differ, as n<TAB>a's<TAB>b's,
# "(no row)" standing in for the shorter side.
first_diff() {
  awk -v A="$1" '
    FILENAME == A { a[FNR] = $0; na = FNR; next }
    { nb = FNR
      if (!done && (FNR > na || a[FNR] != $0)) {
        printf "%d\t%s\t%s\n", FNR, (FNR > na ? "(no row)" : a[FNR]), $0; done = 1
      } }
    END { if (!done) { r = nb + 1; printf "%d\t%s\t%s\n", r, (r > na ? "(no row)" : a[r]), "(no row)" } }' "$1" "$2"
}

b_answers=""
[ -z "$baseline" ] || b_answers=$(sed -n 's/^# answers: //p' "$baseline" | head -n 1)

failures=0 regressions=0 differs=0 matched=0 counted=0 uncompared=0 differ_ids=""
echo
printf '%-9s %-36s %9s %9s  %s\n' STATUS QUERY ROWS MS DETAIL
while IFS=$'\t' read -r id consumer status rows med truncated digest mode detail; do
  note="" verdict=ok shown=""
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
    if [ -f "$out/unstable" ] && cut -f1 "$out/unstable" | grep -qxF "$id"; then
      note="${note:+$note; }its answer differed between passes"
    fi
    if [ -n "$baseline" ]; then
      b_status=$(base_val "$id" 3)
      b_ms=$(base_val "$id" 5)
      b_rows=$(base_val "$id" 4)
      b_truncated=$(base_val "$id" 6)
      b_digest=$(base_val "$id" 7)
      if [ -z "$b_status" ]; then
        note="${note:+$note; }not in the baseline"
        uncompared=$((uncompared + 1))
      elif [ "$b_status" = ok ]; then
        if awk -v m="$med" -v b="$b_ms" -v f="$QUERY_FACTOR" -v s="$QUERY_SLACK_MS" \
          'BEGIN { exit !(m > b * f && m > b + s) }'; then
          verdict=SLOW
          regressions=$((regressions + 1))
          note="${note:+$note; }regressed: ${med} ms against a baseline of ${b_ms} ms (bound: >${QUERY_FACTOR}x and >${QUERY_SLACK_MS} ms slower)"
        else
          note="${note:+$note; }baseline ${b_ms} ms"
        fi
        # A truncated answer without a top-level ORDER BY is an arbitrary subset of the rows, so
        # like a volatile one it is held to its row count only.
        how=""
        if ! printf '%s' "$b_digest" | grep -Eq '^[0-9a-f]{64}$'; then
          uncompared=$((uncompared + 1))
          note="$note; answer not compared: the baseline records no digest"
        elif [ "$mode" = volatile ] || { [ "$mode" != ordered ] && { [ "$truncated" = true ] || [ "$b_truncated" = true ]; }; }; then
          if [ "$mode" = volatile ]; then how=volatile; else how="truncated without a top-level ORDER BY"; fi
          if [ "$rows" = "$b_rows" ]; then
            counted=$((counted + 1))
            note="$note; $how, so compared on its row count"
          else
            verdict=FAIL
            differs=$((differs + 1))
            differ_ids="$differ_ids${differ_ids:+, }$id"
            note="$note; answer differs: ${rows} rows against the baseline's ${b_rows} ($how, so compared on its row count)"
          fi
        elif [ "$digest" = "$b_digest" ]; then
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
          note="$note; answer differs (rows compared $how)"
          if [ -n "$b_answers" ] && [ -f "$b_answers/$id.rows" ]; then
            shown=$(first_diff "$out/answers/$id.rows" "$b_answers/$id.rows" | awk -F'\t' '{
              printf "          first differing row, row %d:\n", $1
              printf "            candidate: %s\n", substr($2, 1, 400)
              printf "            baseline:  %s", substr($3, 1, 400) }')
          else
            shown="          the baseline kept no rows to show; this run's are in $out/answers/$id.rows"
          fi
        fi
        if [ "$b_rows" != "$rows" ]; then
          case "$note" in *"rows against"*) ;; *) note="$note; rows ${rows} against the baseline's ${b_rows}" ;; esac
        fi
      else
        note="${note:+$note; }the baseline failed this query"
        uncompared=$((uncompared + 1))
      fi
    fi
  fi
  printf '%-9s %-36s %9s %9s  %s\n' "$verdict" "$id" "$rows" "$med" "$note"
  [ -z "$shown" ] || printf '%s\n' "$shown"
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
    echo "# answers: $out/answers"
    echo "# id<TAB>consumer<TAB>status<TAB>rows<TAB>median ms<TAB>truncated<TAB>answer sha256<TAB>compared<TAB>detail"
    cat "$results"
  } >"$write_baseline"
  echo
  echo "release-gate: baseline written to $write_baseline"
fi

echo
sampled_kb=$(cat "$out/rss-peak-kb" 2>/dev/null || true)
sampled_kb=${sampled_kb:-0}
hwm_kb=$(cat "$out/hwm-peak-kb" 2>/dev/null || true)
hwm_kb=${hwm_kb:-0}
# A server whose VmHWM went unread still has its samples, so the verdict takes the larger.
peak_kb=$hwm_kb
[ "$sampled_kb" -le "$peak_kb" ] || peak_kb=$sampled_kb
peak_mb=$(( peak_kb / 1024 ))
rss_failed=0
rss_line="peak RSS ${peak_mb} MiB, budget ${MAX_RSS_MB} MiB"
if [ "$hwm_kb" -gt 0 ]; then
  rss_how="the kernel's high-water mark (VmHWM) $(( hwm_kb / 1024 )) MiB; sampled every 0.5 s, $(( sampled_kb / 1024 )) MiB"
else
  rss_how="sampled every 0.5 s, $(( sampled_kb / 1024 )) MiB; no VmHWM read from $PROC_ROOT"
fi
if [ -s "$out/hwm-unread" ]; then
  rss_how="$rss_how; VmHWM unread for $(wc -l <"$out/hwm-unread" | tr -d ' ') server(s) that exited before they were stopped"
fi
if [ "$peak_mb" -gt "$MAX_RSS_MB" ]; then rss_failed=1; rss_line="$rss_line: OVER"; fi
differ_line=""
if [ -n "$baseline" ]; then
  if [ "$differs" -eq 1 ]; then differ_line=", 1 answer differs"; else differ_line=", $differs answers differ"; fi
fi
echo "release-gate: $((n - failures)) of $n answered, $failures failed, $regressions regressed$differ_line; $p99_line"
if [ -n "$baseline" ]; then
  echo "release-gate: answers against the baseline: $matched match, $differs differ, $counted compared on row count only, $uncompared not compared"
fi
echo "release-gate: $rss_line"
echo "release-gate: peak RSS from $rss_how"
if [ -s "$out/rss-peak-gauges" ]; then
  mib() { awk -v k="$1" '$1 == k { printf "%d", $2 / 1048576; f = 1 } END { if (!f) printf "-" }' "$out/rss-peak-gauges"; }
  engines=$(awk '$1 == "nuthatch_analytics_engines" { print $2 }' "$out/rss-peak-gauges")
  echo "release-gate: at the peak, analytics pools held $(mib nuthatch_analytics_pool_reserved_bytes) MiB over ${engines:--} engines; jemalloc allocated $(mib nuthatch_jemalloc_allocated_bytes) MiB, resident $(mib nuthatch_jemalloc_resident_bytes) MiB"
fi
if [ -s "$out/pool-peak-bytes" ]; then
  echo "release-gate: largest single analytics pool reservation $(( $(cat "$out/pool-peak-bytes") / 1048576 )) MiB"
fi
if [ "$failures" -gt 0 ] || [ "$regressions" -gt 0 ] || [ "$differs" -gt 0 ] || [ "$p99_failed" -gt 0 ] || [ "$rss_failed" -gt 0 ]; then
  failed_ids=$(awk -F'\t' '$3 != "ok" { printf "%s%s", sep, $1; sep = ", " }' "$results")
  [ "$rss_failed" -eq 0 ] || failed_ids="${failed_ids:+$failed_ids, }peak RSS"
  named=${failed_ids:+failed: $failed_ids}
  [ -z "$differ_ids" ] || named="${named:+$named; }answer differs: $differ_ids"
  echo "RESULT: FAIL${named:+ - $named}"
  exit 1
fi
echo "RESULT: PASS"
