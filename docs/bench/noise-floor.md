# The benchmark noise floor (RFC-0042 §13 gate 3)

Measured 2026-08-29 on `entity-soak`, a real Arbitrum nest on the Lodestar box running the released
3.0.0, `sealed_through=499563389`. Two independent batches of 15, warm cache.

**"No material regression" is meaningless without a number for what is not material.** RFC-0042 §7 says
*"'Only 15% slower' fails if outside measured noise on a load-bearing workload"* - which is only
enforceable once the noise is a figure.

## What one batch of 15 looks like

| query | min | median | mean | p95 | max | sd | spread |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `SELECT 1` (planning only) | 58 | 59 | 63.0 | 64 | 116 | 14.2 | **92%** |
| `COUNT(*)` raw table | 119 | 123 | 126.4 | 133 | 149 | 7.5 | 24% |
| `SUM` over raw table | 143 | 151 | 155.5 | 173 | 212 | 17.1 | 44% |
| `GROUP BY`, high cardinality | 131 | 147 | 145.3 | 155 | 157 | 7.9 | 18% |
| maintained entity, full scan | 94 | 104 | 103.1 | 118 | 119 | 7.5 | 24% |
| point lookup on the entity | 93 | 97 | 99.6 | 105 | 108 | 4.5 | 15% |

Spread is 15% to 44% of the mean, and 92% on the cheapest query - one run at 116 ms against a 59 ms
median. **A single measurement is worthless here**, and a 15% difference taken from one run is
indistinguishable from the machine having a moment.

## The median is the statistic, and here is why

Two independent batches, same nest, same minute:

| query | batch 1 median | batch 2 median | difference |
| --- | ---: | ---: | ---: |
| `SELECT 1` | 59 | 63 | 6.8% |
| `COUNT(*)` | 123 | 126 | 2.4% |
| `SUM` | 151 | 153 | **1.3%** |
| `GROUP BY` | 147 | 143 | 2.7% |
| entity full scan | 104 | 105 | **1.0%** |
| point lookup | 97 | 97 | **0.0%** |

**The median of 15 runs reproduces to within 3%** on every query but the cheapest. The maximum does
not: `SUM` gave 212 ms then 168 ms, a 21% swing on the same query, same nest, minutes apart.

## The rule this establishes

- **Compare medians of at least 15 runs.** Never a single run, never the mean, never the max.
- **Treat a median difference below 5% as noise.** Three percent is the observed reproducibility; five
  gives headroom without swallowing a real regression.
- **A 15% median difference is real**, comfortably outside this floor - so §7's rule is enforceable as
  written, provided the median discipline is followed. On a single run it would not have been.
- **Warm cache only, and say so.** Cold figures are a different measurement; mixing them inflates the
  floor until nothing counts as a regression.
- **Record segment layout alongside** (`docs/bench/segment-layout.md`): a file costs 0.14-0.18 ms on a
  `COUNT(*)`, so two nests with different layouts are not comparable however many runs are taken.

## What this does not cover

Cold cache. Concurrent load - every figure here is one client at a time, and RFC-0042 Appendix A names
the **concurrent small-query** workload as the dimension public benchmarks least represent, so that
floor still has to be measured separately. Ingest throughput. Peak RSS, which needs the sawtooth
treatment recorded in RFC-0042 §6's amendment rather than a latency method.

`scripts/noise-floor.sh` reproduces this against any running nest.

---

# The concurrent floor (RFC-0042 Appendix A)

Measured 2026-08-29, same nest, same release. Two runs, 6 to 8 seconds per level, `SELECT 1` so what is
measured is the serving path rather than the data.

Appendix A calls the concurrent small-query API workload *"the dimension where public single-query
ClickBench numbers are least representative"*. Every figure above it is one client at a time, so this
is the half that was missing.

| concurrency | req/s | median | p95 | max |
| ---: | ---: | ---: | ---: | ---: |
| 1 | 11.2 | 78 ms | 109 ms | 128 ms |
| 2 | 21.3 | 79 ms | 115 ms | 153 ms |
| 4 | 94.2 | **12 ms** | 92 ms | 141 ms |
| 8 | 113.5 | 29 ms | 136 ms | 244 ms |
| 16 | 127.5 | 63 ms | 170 ms | 326 ms |

Reproducible: two independent runs agree to within a few percent at every level, including the odd one.

## Two findings, and the second contradicts the section above

**Throughput saturates at roughly 115 to 128 req/s** from concurrency 8. Beyond that, added clients buy
latency rather than work: p95 goes 109 ms to 170 ms and max 128 ms to 326 ms while req/s moves 11%.

**The median is the wrong statistic under concurrency, and it is the right one for a single client.**
At concurrency 4 the median *falls* to 12 ms while p95 stays at 92 ms. That is not an improvement, it
is a **bimodal distribution**: most requests served fast, a tail an order of magnitude slower, and the
median tracking the fast mode while hiding the tail.

So the rule is split, and stating it once here is cheaper than arguing about it during slice 2:

- **Single client: compare medians of >= 15 runs.** Reproducible to 3%.
- **Concurrent: compare p95, and report req/s alongside.** A median that improves under load is a
  distribution changing shape, not a system getting faster.

## Cause not established

**A ratio without a cause is not an architectural conclusion** (§5.1), so: the bimodality is
*consistent with* a small pool of DuckDB connections - requests landing on a warm one return in ~12 ms
while others pay setup - but that is a hypothesis, untested here. Separating it needs the serving path
instrumented, not more sampling. Recorded as an observation with a shape, not a diagnosis.

`scripts/concurrent-floor.sh` reproduces it against any running nest.
