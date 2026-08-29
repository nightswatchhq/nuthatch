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
