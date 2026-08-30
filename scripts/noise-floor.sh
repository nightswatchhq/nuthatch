#!/bin/bash
# RFC-0042 §13 gate 3: the benchmark noise floor, with its method.
#
# "No material regression" is meaningless without a number for what is *not* material. This runs each
# corpus-shaped query N times against DuckDB on a real nest and reports the spread, so slice 2 has a
# threshold rather than an argument.
#
# Warm only, and said so: cold-cache figures are a different measurement and mixing them inflates the
# floor until nothing counts as a regression.
#
# #977: A SAMPLE IS ONLY A SAMPLE IF THE REQUEST SUCCEEDED.
#
# Until 2026-08-30 this recorded `t1 - t0` whatever curl did. That fails in the worst possible
# direction: a dead or refusing server returns almost instantly, so **the noise floor got tighter the
# more broken the server was**, and a tight floor is what makes later regressions look material. This
# file produces `docs/bench/noise-floor.md`, which is the threshold every RFC-0042 measurement was
# judged against, so a fast wrong number here propagates into every comparison downstream.
#
# Now: 2xx or the sample is discarded, the run retries until N *successful* samples exist, and a run
# that cannot reach N fails loudly rather than reporting a short, flattering set.
set -uo pipefail
PORT=${PORT:-8105}
N=${N:-15}
# The documented minimum, enforced rather than described. `docs/bench/noise-floor.md` asks for >= 15
# because the distribution is bimodal under load; a smaller n cannot see the second mode, and the
# figure it produces reads as a tighter floor than the system has.
MIN_N=15
if [ "$N" -lt "$MIN_N" ]; then
  echo "N=$N is below the documented minimum of $MIN_N. docs/bench/noise-floor.md asks for >= $MIN_N" >&2
  echo "because the distribution is bimodal; a smaller sample reports a floor the system does not have." >&2
  exit 2
fi
# How many failed attempts to tolerate while chasing N good ones, before giving up.
MAX_ATTEMPTS=$(( N * 4 ))

# Prints the elapsed milliseconds on success; prints nothing and returns 1 on any non-2xx or
# transport failure. `--fail` makes curl exit nonzero for >= 400, and the written status code covers
# the rest (a 3xx, or a body served with an error status curl would otherwise accept).
timed_request() { # timed_request <sql>
  local sql="$1" t0 t1 code
  t0=$(date +%s%N)
  code=$(curl -s --fail --show-error --max-time 120 -o /dev/null -w '%{http_code}' \
    --get "http://127.0.0.1:$PORT/sql" --data-urlencode "q=$sql" 2>/dev/null) || return 1
  t1=$(date +%s%N)
  case "$code" in 2??) ;; *) return 1 ;; esac
  echo $(( (t1-t0)/1000000 ))
}

run() { # label sql
  local label="$1" sql="$2" times=() failed=0 attempts=0 ms
  while [ "${#times[@]}" -lt "$N" ]; do
    if [ "$attempts" -ge "$MAX_ATTEMPTS" ]; then
      echo "FAIL: $label - only ${#times[@]}/$N successful samples after $attempts attempts ($failed failed)." >&2
      echo "      A latency figure from a server that is refusing requests is not a latency figure." >&2
      exit 1
    fi
    attempts=$(( attempts + 1 ))
    if ms=$(timed_request "$sql"); then
      times+=( "$ms" )
    else
      failed=$(( failed + 1 ))
    fi
  done
  [ "$failed" -gt 0 ] && echo "note: $label discarded $failed failed request(s)" >&2
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
