#!/usr/bin/env bash
#
# Run the dense multi-nest footprint scenario N times and print the spread.
#
# This is where `REGRESSION_MB` in `.github/workflows/multinest-footprint.sh` comes from, and the
# reason it exists as a committed tool rather than a thing someone did once in a terminal: **a
# threshold picked from a single run is folklore.** One run tells you a number; it does not tell you
# whether the next run would have produced the same one, and a ceiling set inside the noise band
# produces a gate that fails at random. A flaky gate gets disabled, and a disabled gate is worse than
# no gate at all, because the green tick still reads as coverage.
#
# Run this on the machine whose numbers you intend to enforce, and re-run it whenever you move the
# ceiling. Sequential by construction: concurrent runs contend for CPU and page cache, which measures
# the harness rather than nuthatch.
#
# Usage: scripts/multinest-rss-spread.sh [runs]        (default 7)
# Env:   BIN, plus every scenario knob multinest-footprint.sh takes.
set -euo pipefail

RUNS="${1:-7}"
BIN="${BIN:-target/release/nuthatch}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="$(mktemp -d)"

echo "$RUNS runs of the dense multi-nest scenario at $(git rev-parse --short HEAD 2>/dev/null || echo '?')"
echo

peaks=()
for r in $(seq 1 "$RUNS"); do
  # Distinct ports per run so a lingering socket from the previous run cannot be mistaken for this
  # one's API - which would report the *previous* run's progress and pass on stale data.
  if BIN="$BIN" PORT=$((8300 + r)) RPC_PORT=$((8560 + r)) REPORT="$OUT/run-$r.json" \
     bash "$HERE/.github/workflows/multinest-footprint.sh" > "$OUT/run-$r.log" 2>&1; then
    peak="$(grep -o '"peak_rss_mb": [0-9]*' "$OUT/run-$r.json" | grep -o '[0-9]*')"
    at_tip="$(grep -o '"at_tip_rss_mb": [0-9]*' "$OUT/run-$r.json" | grep -o '[0-9]*')"
    peaks+=("$peak")
    printf 'run %2d: peak %4s MB   at-tip %4s MB\n' "$r" "$peak" "$at_tip"
  else
    # A failed run is reported, never skipped. Dropping it would bias the band towards the runs that
    # happened to behave, which is the opposite of what a noise band is for.
    printf 'run %2d: FAILED - %s\n' "$r" "$(tail -1 "$OUT/run-$r.log")"
  fi
done

[ "${#peaks[@]}" -gt 0 ] || { echo; echo "every run failed - logs in $OUT"; exit 1; }

printf '%s\n' "${peaks[@]}" | sort -n | awk -v n="${#peaks[@]}" '
  {a[NR]=$1}
  END {
    lo=a[1]; hi=a[NR]; med=a[int((NR+1)/2)]
    printf "\n%d/%d runs completed: min %d MB, median %d MB, max %d MB (band %d MB)\n", NR, n, lo, med, hi, hi-lo
    printf "\nSuggested REGRESSION_MB: %d\n", int(hi*1.25)+1
    printf "  = max observed + 25%%, i.e. clear of the band but far below anything a real\n"
    printf "    regression would leave untouched. Sanity-check it by mutation: make the build\n"
    printf "    genuinely worse and confirm the gate goes red. If it does not, it is not a gate.\n"
  }'

echo
echo "reports + logs: $OUT"
