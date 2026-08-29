#!/bin/bash
# RFC-0042 §13 gate 3: the benchmark noise floor, with its method.
#
# "No material regression" is meaningless without a number for what is *not* material. This runs each
# corpus-shaped query N times against DuckDB on a real nest and reports the spread, so slice 2 has a
# threshold rather than an argument.
#
# Warm only, and said so: cold-cache figures are a different measurement and mixing them inflates the
# floor until nothing counts as a regression.
set -uo pipefail
PORT=${PORT:-8105}
N=${N:-15}
run() { # label sql
  local label="$1" sql="$2" times=()
  for i in $(seq 1 "$N"); do
    local t0 t1
    t0=$(date +%s%N)
    curl -s --max-time 120 --get "http://127.0.0.1:$PORT/sql" --data-urlencode "q=$sql" > /dev/null
    t1=$(date +%s%N)
    times+=( $(( (t1-t0)/1000000 )) )
  done
  printf '%s\n' "${times[@]}" | sort -n | awk -v l="$label" -v n="$N" '
    {a[NR]=$1; s+=$1}
    END {
      mean=s/NR; med=a[int((NR+1)/2)]; min=a[1]; max=a[NR];
      p95=a[int(NR*0.95+0.5)]; if (p95=="") p95=max;
      for (i=1;i<=NR;i++) v+=(a[i]-mean)^2; sd=sqrt(v/NR);
      printf "%-30s n=%-3s min=%-5s med=%-5s mean=%-6.1f p95=%-5s max=%-5s sd=%-5.1f spread=%.0f%%\n",
             l, n, min, med, mean, p95, max, sd, 100*(max-min)/mean
    }'
}
echo "=== noise floor, port $PORT, n=$N, warm ==="
echo "nest: $(curl -s --max-time 10 "http://127.0.0.1:$PORT/metrics" | awk '/^nuthatch_sealed_through /{print "sealed_through="$2}')"
run "SELECT 1 (planning only)"        "SELECT 1 AS x"
run "COUNT(*) raw table"              "SELECT COUNT(*) AS n FROM usdc__transfer"
run "SUM over raw table"              "SELECT SUM(CAST(value AS HUGEINT)) AS s FROM usdc__transfer"
run "GROUP BY, high cardinality"      "SELECT COUNT(*) AS g FROM (SELECT \"to\" FROM usdc__transfer GROUP BY \"to\")"
run "maintained entity, full scan"    "SELECT COUNT(*) AS g, SUM(CAST(sum_value AS HUGEINT)) AS t FROM received"
run "point lookup on the entity"      "SELECT sum_value FROM received WHERE \"to\" = '0x0000000000000000000000000000000000000000'"
