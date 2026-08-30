#!/bin/bash
# The CONCURRENT small-query floor (RFC-0042 Appendix A, §13 gate 3's second half).
#
# Every latency figure we have is one client at a time. Appendix A names the concurrent small-query
# API workload as "the dimension where public single-query ClickBench numbers are least
# representative", and it is the shape a served nest actually sees. So it is the floor most likely to
# change a slice-2 answer, and the one we had nothing for.
#
# Measures throughput and per-request latency at increasing concurrency against a real nest.
#
# #977: FAILED REQUESTS ARE NOT SAMPLES, AND HERE THEY ALSO INFLATE THROUGHPUT.
#
# Until 2026-08-30 every attempt was timed and counted whatever curl did. That corrupts both columns
# at once, and in the flattering direction: a refused request returns in microseconds, so latency
# falls *and* req/s rises. `serve.rs` makes this concrete rather than theoretical - it acquires its
# 2-permit semaphore with `try_acquire_owned` and returns **503** on failure, so at concurrency 4, 8
# and 16 most requests are refused instantly. Those refusals were being reported as fast successes,
# which is precisely backwards: the levels where the server is most saturated looked the best.
#
# Successes and failures are now counted separately. Throughput is successful requests per second,
# latency percentiles come from successful requests only, and the refusal rate is reported as its own
# column because at these concurrencies it is the finding rather than noise.
set -uo pipefail
PORT=${PORT:-8105}
DUR=${DUR:-10}
SQL=${SQL:-SELECT 1 AS x}

worker() { # end_epoch outfile failfile
  local end="$1" out="$2" fail="$3"
  while [ "$(date +%s)" -lt "$end" ]; do
    local t0 t1 code
    t0=$(date +%s%N)
    code=$(curl -s --max-time 30 -o /dev/null -w '%{http_code}' \
      --get "http://127.0.0.1:$PORT/sql" --data-urlencode "q=$SQL" 2>/dev/null) || code=000
    t1=$(date +%s%N)
    case "$code" in
      2??) echo $(( (t1-t0)/1000000 )) >> "$out" ;;
      *)   echo "$code" >> "$fail" ;;
    esac
  done
}

echo "=== concurrent floor, port $PORT, ${DUR}s per level ==="
echo "query: $SQL"
printf "%-6s %-10s %-8s %-8s %-8s %-8s %-8s\n" "conc" "ok/s" "med_ms" "p95_ms" "max_ms" "vs_1" "failed"
base=""
for c in 1 2 4 8 16; do
  tmp=$(mktemp -d); end=$(( $(date +%s) + DUR ))
  mkdir -p "$tmp/ok" "$tmp/bad"
  for i in $(seq 1 "$c"); do worker "$end" "$tmp/ok/$i" "$tmp/bad/$i" & done
  wait
  cat "$tmp"/ok/* 2>/dev/null | sort -n > "$tmp/all"
  nfail=$(cat "$tmp"/bad/* 2>/dev/null | wc -l | tr -d ' ')
  n=$(wc -l < "$tmp/all" | tr -d ' ')
  if [ "$n" -eq 0 ]; then
    # Every request failed. Printing a latency row here would be printing the speed of being refused.
    printf "%-6s %-10s %-8s %-8s %-8s %-8s %-8s\n" "$c" "0.0" "-" "-" "-" "-" "$nfail"
    rm -rf "$tmp"; continue
  fi
  read -r med p95 mx <<< "$(awk '{a[NR]=$1} END {print a[int((NR+1)/2)], a[int(NR*0.95+0.5)], a[NR]}' "$tmp/all")"
  rps=$(awk -v n="$n" -v d="$DUR" 'BEGIN{printf "%.1f", n/d}')
  [ -z "$base" ] && base="$med"
  ratio=$(awk -v m="$med" -v b="$base" 'BEGIN{printf "%.2fx", m/b}')
  printf "%-6s %-10s %-8s %-8s %-8s %-8s %-8s\n" "$c" "$rps" "$med" "$p95" "$mx" "$ratio" "$nfail"
  rm -rf "$tmp"
done
