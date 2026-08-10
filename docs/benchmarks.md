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

- **entity point-read p50/p99** - `--reads N` keys in chain order from the oldest, read once to warm
  the cache and then timed. It is a **prefix, not an even spread**: pass `--reads` at or above the hot
  row count (as the CI gate does) and it covers every key, but a smaller `--reads` over a large hot
  store times a contiguous run of the B-tree and will flatter itself.
- **`/sql` query latency p50/p99 and peak RSS** over the hot ∪ cold union. The default query is a
  `SELECT count(*)` on the largest hot table: deliberately the full-tip-materialising scan, because
  that is the **#1 RAM risk on a deep-finality L2** and the number most worth watching.

Same house rule as the backfill matrix: numbers come from a committed `bench-report.json` with date,
provider, hardware and commit, or they are not quoted. Run it before and after any change to the
serving or storage path - a persistent DuckDB connection, a bounded hot scan, or a compact row format
would all show up here first. A `query` report carries `hardware` and `commit` but no `provider`,
because it runs entirely offline against a store on disk: no endpoint is involved, and the machine is
what makes two of these numbers comparable at all.

### The point-read gate (issue #283)

Reporting a number is not tracking it. CLAUDE.md says benchmark regressions fail the build, so
`bench query` also takes limits, and exits non-zero when one is breached:

```sh
nuthatch bench query --dir <nest> --reads 8004 \
  --min-reads 256 --max-point-read-p50-us 8 --max-point-read-p99-us 150
```

Pass none of them and the bench only reports, which is what an operator poking at their own nest
wants. Pass any of them and it is a gate.

**Point-reads see the unsealed tip, not the backfill.** `get_entity` is a hot-store read, and rows
past finality are sealed to Parquet and pruned out of redb - so of the fixture's 8,004 indexed rows
only 256 (64 blocks x 4 logs) are still readable this way, which is what `--min-reads` is set against.
The first version of this gate asserted `--min-reads 8004`, reasoned from the backfill size, and
therefore failed every run.

**Always pass `--min-reads` alongside a ceiling.** A nest with nothing indexed samples no keys and
reports `p50 = 0µs`, and zero is under every ceiling anyone would ever write - so a gate without a
floor is greenest exactly when it has measured nothing. The `footprint` check learned the same lesson
and asserts its row count before it compares a peak.

CI runs this as the **`point-read latency`** job (`.github/workflows/point-read.sh`), on the same
hermetic fixture the `footprint` job uses: `footprint-rpc.py` serves the chain, the nest is written
inline, and every run indexes exactly 8,004 rows. No secret and no third party, so a fork's pull
request can satisfy it, and a change in p99 is a change in nuthatch rather than in somebody's rate
limiter. The report uploads as an artifact on every run, including a failing one.

### Where the ceilings come from

They were chosen by **breaking the read path on purpose and measuring what happened**, not by leaving
generous headroom - and both halves were measured on `ubuntu-latest`, the runner that enforces the
gate, rather than on a dev box. Replacing the B-tree seek in `Store::get_entity` with a linear scan -
the exact regression the gate claims to catch - gives, over the 256-row hot store:

| | p50 | p99 | p99.9 |
|---|---|---|---|
| baseline, 3 CI runs | 0.59 - 0.82µs | 0.70 - 1.00µs | 0.77 - 3.96µs |
| linear scan in place of the seek | 18.15µs | 34.45µs | 49.78µs |

The first version of this gate used 200µs/2,000µs. The scan sits comfortably under both, so that gate
reported `OK: within the point-read ceilings` **with a full scan in the read path** - a number, not a
gate.

A second version used 15µs, measured the same way but on a 32-core/62 GB dev box (baseline 0.66-1.59µs,
scan 24.2µs), on the assumption that a slower CI runner would scale baseline and regression together.
Measuring both on the runner showed that it does not: the runner's baseline is *at or below* the dev
box's, while its mutation is *faster* (18.15µs against 24.2µs), leaving 15µs only a 1.21x margin below
a real regression rather than the 1.61x claimed.

- **p50, ceiling 8µs, is the gate that discriminates.** 9.8x above the worst observed baseline and 2.3x
  below a full scan - better on both sides at once than 15µs was on either. p50 is the robust
  statistic here: its observed spread is 1.4x where p99.9's is 5.1x.
- **p99, ceiling 150µs, is a catastrophe backstop and nothing more** - ~150x baseline, and it does
  *not* catch the scan mutation (34.45µs < 150µs). p99 over 256 samples is the 4th-worst read, so on a
  shared runner it is preemption-dominated. A tight p99 here buys a flaky check, and a flaky gate gets
  waved through until it means nothing.
- **p99.9 is reported and gated on by nothing.** It swung 0.77-3.96µs at fixed commits.

What this gate does **not** catch: a regression landing under 8µs - a partial scan, or a scan when the
hot store is much smaller than 256 rows - cold-start latency (it measures warm, see `warm_cache` in the
report), point-reads against the Postgres backend, and anything about the sealed/DuckDB path. It is a
floor on gross regressions, not a microbenchmark.

Baseline: `docs/bench/point-read.json` - **measured on the 32-core/62 GB dev box, which is not the
machine that enforces this gate.** Read it as the dev-box reference point, not as the number the
ceiling was set against: that is the `ubuntu-latest` table above (p50 0.59-0.82µs), and it is the one
8µs is 9.8x above. Committing a runner-produced artifact instead would remove the need for this
caveat, and #385 tracks it. Saying so here rather than leaving a reader to notice the `hardware`
field disagrees with the argument.
