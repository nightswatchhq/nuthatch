#!/bin/bash
# The CONCURRENT small-query floor (RFC-0042 Appendix A, §13 gate 3's second half).
#
# Every latency figure we have is one client at a time. Appendix A names the concurrent small-query
# API workload as "the dimension where public single-query ClickBench numbers are least
# representative", and it is the shape a served nest actually sees. So it is the floor most likely to
# change a slice-2 answer, and the one we had nothing for.
#
# Measures throughput and per-request latency at increasing concurrency against a real nest.
set -uo pipefail
PORT=${PORT:-8105}
DUR=${DUR:-10}
SQL=${SQL:-SELECT 1 AS x}

worker() { # end_epoch outfile
  local end="$1" out="$2"
  while [ "$(date +%s)" -lt "$end" ]; do
    local t0 t1
    t0=$(date +%s%N)
    curl -s --max-time 30 --get "http://127.0.0.1:$PORT/sql" --data-urlencode "q=$SQL" > /dev/null
    t1=$(date +%s%N)
    echo $(( (t1-t0)/1000000 )) >> "$out"
  done
}

echo "=== concurrent floor, port $PORT, ${DUR}s per level ==="
echo "query: $SQL"
printf "%-6s %-10s %-8s %-8s %-8s %-8s\n" "conc" "req/s" "med_ms" "p95_ms" "max_ms" "vs_1"
base=""
for c in 1 2 4 8 16; do
  tmp=$(mktemp -d); end=$(( $(date +%s) + DUR ))
  for i in $(seq 1 "$c"); do worker "$end" "$tmp/$i" & done
  wait
  cat "$tmp"/* 2>/dev/null | sort -n > "$tmp/all"
  n=$(wc -l < "$tmp/all")
  read -r med p95 mx <<< "$(awk '{a[NR]=$1} END {print a[int((NR+1)/2)], a[int(NR*0.95+0.5)], a[NR]}' "$tmp/all")"
  rps=$(awk -v n="$n" -v d="$DUR" 'BEGIN{printf "%.1f", n/d}')
  [ -z "$base" ] && base="$med"
  ratio=$(awk -v m="$med" -v b="$base" 'BEGIN{printf "%.2fx", m/b}')
  printf "%-6s %-10s %-8s %-8s %-8s %-8s\n" "$c" "$rps" "$med" "$p95" "$mx" "$ratio"
  rm -rf "$tmp"
done
