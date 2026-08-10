# Benchmarks (RFC-0004)

Two harnesses: **backfill** (the write path - this page's main subject) and **query** (the read path,
[below](#query-benchmarks-the-read-path)).

**House rule:** every performance number nuthatch publishes traces to a `bench-report.json`
produced by `nuthatch bench backfill` - with date, provider, hardware, and commit. No hand-typed
numbers, including flattering ones. This page documents the harness and the pinned workloads; the
baseline matrix is filled in from real runs (an archive node is needed for the historical ranges).

## The harness

```sh
nuthatch bench backfill --dir <nest> --from <block> --to <block> [--runs 3] [--rpc <url>] [--out report.json]
```

Runs the real **fetch → decode → store** path over a pinned block range and reports the **median**
across runs of:

- **events/sec** - total decoded events ÷ wall-clock (excluding init). The headline.
- **wall-clock (s)**, **peak RSS (MB)**, **RPC requests** (including failover retries).

It writes to a throwaway store per run (never the nest's own DB), so runs are independent and
repeatable. `--rpc` overrides the nest's endpoints - point it at your own node for a T2 run.

Nothing here optimises anything. It exists so the seal-direct / adaptive-chunker / pipeline work
(later RFC-0004 slices) is each gated on a measured before/after, not a wish.

## Workloads (pinned, public, reproducible)

| ID | Nest | Range | Character |
|----|------|-------|-----------|
| W1 | USDC (mainnet) | 100,000 blocks ending 21,400,000 | dense single-contract (~1.3M events) |
| W2 | [horizon-nest](https://github.com/cargopete/horizon-nest) (Arbitrum) | full history from deployment | sparse multi-contract, L2 cadence |
| W3 | USDC + WETH + Uniswap V3 factory (mainnet) | 50,000 blocks | mixed density, multi-table fan-out |

## Sourcing tiers

- **T1** - public RPC defaults (round-robin), as a new user experiences it.
- **T2** - your own node (localhost `eth_getLogs`), via `--rpc`.
- **T3** - a caching proxy (e.g. erpc) in front of T1.

W1/W3 historical ranges and W2's full history require an **archive** node (public sequencer/
non-archive endpoints don't serve old `eth_getLogs`). Public-RPC (T1) numbers are noisy - take the
median of three runs, date them, and read them as "what a new user should expect," not as the
product's capability.

## Seal-direct (`--seal-direct`)

For ranges already past finality - nearly all of a backfill - rows can go straight to sealed Parquet,
bypassing the hot store: decode → buffered rows → content-addressed segments, no redb write, no
read-back, no prune. The bounded buffer caps RSS by construction. `seal_range` is the one shared
writer, so a given range yields **byte-identical** segments whether sealed directly or via the hot
store (asserted by `seal::seal_direct_matches_seal_via_hot_store`).

Measured before/after (same range, same RPC cost, only the storage path differs):

| Path | Range | Events | Wall-clock | events/sec |
|---|---|---|---|---|
| hot store (decode → redb) | USDC, 120 recent blocks (public RPC) | 12,127 | 42.0 s | **289** |
| seal-direct (decode → Parquet) | same | 12,127 | 4.8 s | **~2,520** |

**~8.7× faster.** The RPC portion is identical between the two (24 requests each); the difference is
that the hot path commits a redb transaction per row (~12k fsyncs), while seal-direct buffers and
writes a handful of segments. Single-run public-RPC smoke figures - noisy in absolute terms, but the
storage-path delta is the point and is not noise. Run it yourself:

```sh
nuthatch bench backfill --dir <nest> --from A --to B                 # hot store (baseline)
nuthatch bench backfill --dir <nest> --from A --to B --seal-direct   # seal-direct
```

## Pipeline (`--concurrency K`, seal-direct only)

Once the storage path is cheap, wall-clock is dominated by sequential `getLogs` round-trip latency.
The pipeline fetches `K` windows concurrently (`futures::stream::buffered`) but consumes results
**in block order**, so the sealed segments are byte-identical to the sequential path (asserted by
`indexer::pipelined_backfill_matches_sequential`). Bounded in-flight windows cap RSS.

Stacked measured result (USDC, same 120 blocks, public RPC, same ~24 requests):

| Path | events/sec | vs hot store |
|---|---|---|
| hot store (decode → redb) | 289 | 1× |
| seal-direct | 2,420 | 8.4× |
| seal-direct + 8-way pipeline | **5,837** | **~20×** |

```sh
nuthatch bench backfill --dir <nest> --from A --to B --seal-direct --concurrency 8
```

The pipeline's 2.4× here is bounded by the four public endpoints the requests spread across; against
your own node (`--rpc`, `--concurrency 16`) it goes further. RSS rose to ~62 MB with 8 windows in
flight - bounded by `K` and well within the 256 MB budget.

## Baseline matrix (pre-optimization)

_Pending - the full W1-W3 × T1-T3 matrix is populated from archive-node runs (needed for the
historical ranges) and committed as `bench-report.json` artifacts._

## Query benchmarks (the read path)

The backfill harness measures the write path only, which left entity point-read latency and the `/sql`
scan cost free to regress silently. `nuthatch bench query` is the guard for both. It runs **offline
against an already-indexed nest** - stop `dev` first, since the bench opens the store directly.

```sh
nuthatch bench query --dir <nest> [--sql "<query>"] [--reads N] [--iters N] [--out report.json]
```

Reports:

- **entity point-read p50/p99** - keys sampled evenly across the hot store.
- **`/sql` query latency p50/p99 and peak RSS** over the hot ∪ cold union. The default query is a
  `SELECT count(*)` on the largest hot table: deliberately the full-tip-materialising scan, because
  that is the **#1 RAM risk on a deep-finality L2** and the number most worth watching.

Same house rule as the backfill matrix: numbers come from a committed `bench-report.json` with date,
provider, hardware and commit, or they are not quoted. Run it before and after any change to the
serving or storage path - a persistent DuckDB connection, a bounded hot scan, or a compact row format
would all show up here first.

## The per-cursor RAM budget (≤2 GB)

CLAUDE.md non-negotiable 2: **≤2 GB RAM per active-chain cursor**, shared across whatever nests sit on
that cursor. RFC-0021 makes it per-cursor rather than per-runtime. Density is RAM-bounded, not free,
and this is the measurement that says whether we hold it.

It is measured on the adversarial case, because a pass on a quiet contract tells us nothing: a large
ABI, a high event rate, many nests on **one** cursor, and **at tip** rather than mid-backfill.

```sh
bash .github/workflows/multinest-footprint.sh          # measure + enforce
scripts/multinest-rss-spread.sh 7                      # the noise band, before moving a ceiling
```

### Two ceilings, and why they are not one

| | ceiling | what a breach means |
|---|---|---|
| `MAX_RSS_MB` | 2048 MB | the **budget** was broken. A product promise, not a tuning parameter. |
| `REGRESSION_MB` | just outside the noise band | this scenario got materially more expensive. |

The budget alone cannot be a regression gate. The scenario measures ~145 MB against 2048, so a change
could cost ten times the memory and still pass. A gate that cannot fail is not coverage, so the
regression ceiling is set from the observed run-to-run spread and is the one that actually catches
drift. They are nested and must not be reconciled into a single number.

The same logic separates this from the `footprint (RAM budget)` CI job, which is **not** redundant
with it. That one is a tight tripwire on a deliberately small scenario (one nest, one event type,
8004 rows, 256 MB); its sensitivity comes from being small. This one is the product budget on the
scenario most likely to break it. Neither subsumes the other.

### What the measurement found

| | |
|---|---|
| nests on one cursor | 20 |
| ABI | Uniswap V4 `PoolManager` - 10 events, `Initialize`/`Swap` 8 inputs each |
| event rate | 200 logs/block across the cursor |
| blocks | 1000 backfill, then 200 of live tip-following |
| rows | 240,200 |
| **peak RSS** | **143 MB**, against the 2048 MB budget |
| at-tip RSS | 143 MB |

**History is close to free in RAM, and that is the sealing design working rather than luck.** At a 4x
event rate (608,800 rows) peak went only 143 → 198 MB, and the at-tip figure (173 MB) came in *below*
the peak - so the peak is a backfill burst, not a steady-state cost. RSS tracks the near-tip window
and the per-window fan-out, not the size of history, because past finality the rows are sealed to
Parquet and leave the heap.

**The runtime's own projection is ~13x too pessimistic.** `estimate_nest_rss_mb` projected 1920 MB for
this exact cursor (`120 + 20 × 90`) against a measured 143 MB, and it is a flat per-nest constant - it
cannot see ABI size, table count, or event rate, so a ten-event nest is projected identically to a
one-event one. Since a cursor whose *projected* RSS exceeds `max_rss_mb` is refused before it starts,
admission control currently caps a cursor at ~22 nests on a model an order of magnitude out. Worth
revisiting; not changed here, because loosening a refusal path wants more than one scenario behind it.

### The fixture

`multinest-rpc.py` serves the chain locally, the nests are written inline, and topic0s are derived by
hashing the signatures in the same ABI file the harness writes. No secret, no third party, and a fork
PR can satisfy the check - the lesson of issue #260, which this inherits rather than relearns.

The tip **moves**: `eth_blockNumber` advances a fixed step per call once the backfill has drained,
so the cursor genuinely spends the run's second half on the live path. It is deterministic in its
*endpoint* rather than its schedule - a faster machine arrives sooner, but every run serves exactly
the same logs, so peak RSS stays comparable between runs.

**The check refuses to pass unless it can show it did the work**: every nest reached the final tip,
every nest cleared a row floor, and **all ten tables of every nest hold rows**. That last one also
proves each topic0 landed - a wrong topic0 does not error, it yields an empty table, which is exactly
how a footprint check passes having measured nothing.
