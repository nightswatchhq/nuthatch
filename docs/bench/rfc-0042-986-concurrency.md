# RFC-0042 #986: concurrent throughput, and what the median hides

Measured 2026-08-30 on the Linux dev box (`pepe-thinkpad`, 32 cores, 62 GB, load 0.29 at start), tree
`59a54639`. 2 M rows, `net_balances`, 10 queries per client, clients 1 -> 32. Raw log alongside.

§11's concurrent-throughput row, and the reason `docs/bench/noise-floor.md` insists on **p95 under
concurrency**: the distribution is bimodal, and a median cannot see it.

## The two paths are not concurrent in the same way

`analytics.rs` holds **one cached read-only DuckDB connection per directory, taken under a mutex**
(#295) - "still read-only and still single-user; queries take the mutex". So concurrent `/sql` against
one nest **serialises**, whatever the engine does inside a single query.

The specialised operator (#987) has no shared connection. It is a pure function over files, so N
callers genuinely overlap.

Modelling DuckDB without that mutex would measure an engine nuthatch does not deploy, so the harness
keeps it.

## 100 segments

| clients | DuckDB qps | DuckDB p50 | DuckDB **p95** | DuckDB **p99** | Rust qps | Rust p50 | Rust **p95** | Rust **p99** |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 40.3 | 24.1 | 29.5 | 29.5 | 43.1 | 23.2 | 27.6 | 27.6 |
| 2 | 39.9 | 24.0 | 35.5 | 286.9 | 62.9 | 32.1 | 40.6 | 40.6 |
| 4 | 40.3 | 24.2 | 281.5 | 773.9 | 99.9 | 38.0 | 48.6 | 50.3 |
| 8 | 41.5 | 23.7 | 750.3 | 1465.2 | 102.1 | 76.4 | 88.8 | 90.2 |
| 16 | 40.8 | 23.8 | 1746.0 | 3215.0 | 108.5 | 146.5 | 162.2 | 167.0 |
| 32 | 39.6 | 24.6 | **3705.6** | **7066.0** | 107.1 | 276.6 | 476.3 | 943.6 |

## 10 000 segments - the realistic layout

| clients | DuckDB qps | DuckDB p50 | DuckDB **p95** | DuckDB **p99** | Rust qps | Rust p50 | Rust **p95** | Rust **p99** |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 1 | 2.7 | 366.8 | 503.3 | 503.3 | 4.4 | 225.2 | 279.2 | 279.2 |
| 4 | 2.6 | 376.7 | 3906.1 | 11644.5 | 8.1 | 493.5 | 514.0 | 526.8 |
| 8 | 2.5 | 392.8 | 12117.0 | 24228.4 | 9.2 | 868.1 | 895.1 | 912.5 |
| 16 | 2.6 | 387.2 | 27049.5 | 50614.4 | 9.2 | 1736.3 | 1824.7 | 1845.6 |
| 32 | 2.5 | 392.4 | **59738.8** | **111370.2** | 8.9 | 3525.3 | 4081.0 | 4187.2 |

## Three findings, in order of how much they matter

### 1. The serving path does not scale with concurrent load at all

**DuckDB throughput is flat from 1 to 32 clients** - 40.3 to 39.6 qps at 100 segments, 2.7 to 2.5 at
10 000. Adding 31 callers adds no work done. That is the mutex, not the engine: one connection, one
query at a time, everyone else queues.

### 2. The median reports everything as healthy, at every load point

DuckDB p50 is **24 ms at 1 client and 24.6 ms at 32**. Unchanged. Meanwhile p99 goes from 29.5 ms to
7 066 ms - a **240x** degradation the median cannot see, because the served query really does take
24 ms and the other thirty-one are waiting.

At 10 000 segments the median says 392 ms while p99 says **111 seconds**.

This is the shape that looks like success and is not, and it is why `noise-floor.md` specifies p95 for
this row. A p50-only comparison at 8 clients would report DuckDB beating the Rust operator by 3x
(23.7 ms vs 76.4 ms); p95 reports the opposite by 8x (750 ms vs 89 ms).

### 3. The Rust operator scales and degrades gracefully - for an architectural reason

qps 43 -> 107 before plateauing at CPU saturation, and its distribution stays *flat*: at 16 clients
p50 146.5, p95 162.2, p99 167.0. Every caller sees roughly the same latency because none of them is
queued behind a lock.

**This is not evidence that the Rust operator is a faster engine.** It wins here because it has no
shared connection. Any engine deployed the way `analytics.rs` deploys DuckDB would show the same flat
throughput, and any engine deployed as a pure function over files would show similar scaling.

## What this says about RFC-0042, carefully

It fills §11's concurrent-throughput row, and it does so with a result that is **mostly about our
deployment rather than about DuckDB**. §3 requires concurrent throughput not to regress materially; a
candidate without a single-connection constraint improves it by construction, which is a real product
difference but not an argument about engine quality.

Per §3a the outcome stays binary and per §0 there is no preferred answer.

## Not measured here

Restart-to-ready - the other genuinely absent row from #986 - and RSS under concurrency. Ingest
throughput and peak RSS turned out to be already covered by `nuthatch bench backfill` and CI's
`footprint (RAM budget)` job; see the correction on #986.
