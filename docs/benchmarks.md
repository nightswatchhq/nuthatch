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


## What a backfill costs against a metered endpoint (2026-08-19)

Throughput is not the only number an operator cares about. "Be your own indexer" is partly an argument
about bills, so this is what a day of real backfills actually consumed - measured against one Alchemy
key, across Arbitrum, BSC, Polygon, Optimism and Gnosis.

**~11.5M compute units for six backfills** - about **$5** at the rate an Alchemy PAYG invoice actually
charges, **$0.00000045/CU** ($0.45 per million; taken from a real July invoice, not from a pricing
page). The largest runs were 454M blocks of Arbitrum history for a two-contract nest and 200,000
blocks of BSC for a single busy ERC-20.

For scale: a whole month of this project's development traffic came to **67.5M CU = $30.39**. Indexing
is cheap; the interesting question is what fraction of it is *necessary*, which is the next section.

| Run | Estimated CU | Shape |
|---|---:|---|
| `graph-allocations`, 454M blocks | ~4.4M | ~5,500 `getLogs` + a header per event-bearing block |
| BSC, 200k blocks, 1.67M events | ~3.0M | dense contract, ~180k event-bearing blocks |
| Uniswap V3, 674k blocks, 196k events | ~3.6M | plus retries |
| Chain sweep + diffs + DOUDOCHAIN | ~0.4M | all bounded |

### The finding: our own default is the cost, not the indexing

**Roughly 80% of that is `eth_getBlockByNumber`, not `eth_getLogs`.** The provider's method breakdown
put the header call *above* the log call. The cause is `block_timestamps = true`, which nuthatch
defaults on: one header fetch for every block that carries an event.

It is worse than the raw count suggests. A busy range provokes partial responses - the Uniswap run
logged two dozen warnings of the form `block_timestamps: 882/1548 block(s) missing from the RPC
response`, and each one is a re-ask of the same range.

So against a metered endpoint, **nuthatch's default costs several times more than its actual indexing
work**. That is a fact about our defaults rather than about any provider, and it is the sort of thing
this project should publish about itself rather than have an operator discover on an invoice.

At today's volumes that is $5 against $1, which nobody would optimise for. At a hundred nests it is
the difference between a rounding error and a line item, and it is a claim nuthatch makes about
itself - so it is worth stating the number rather than the adjective.

`block_timestamps = false` (RFC-0029 §6b) removes it entirely, at the cost of the column. A nest that
never asks "when" should not be paying for it.

### Price a backfill before you start it

A from-deployment backfill on a mature chain is not the same order of expense as a bounded one, and the
difference is three figures rather than a rounding error. The arithmetic is simple enough that there is
no excuse for skipping it:

```
cost ≈ (blocks / measured_blocks) × measured_CU × $0.00000045
```

Worked, from a run that was actually measured: Uniswap V3 on Arbitrum over 674,425 blocks cost ~3.6M CU
(~$1.62). Its factory was deployed at block **175**, so the full history is **736 times** that range:

| Range | Events | CU | Cost |
|---|---:|---:|---:|
| 674k blocks (measured) | 195,515 | 3.6M | **$1.62** |
| Full factory history (extrapolated) | ~144M | ~2,650M | **~$1,192** |

Early chain history is far sparser than the tip, so that is an upper bound - but a tenth of it is still
three figures. **A full-history backfill on a busy factory gets priced and agreed before it is
started, not after.** A day of bounded proof runs across six chains came to **$5.15**; one unpriced
backfill would have been two hundred times that.

Two guards worth having on the provider side rather than in a habit: a **spend limit** (the "Set limit"
control beside each usage chart) turns an accident into a refusal, and a **custom throughput override**
turns a concurrency spike into throttling rather than billing.

### The other lever: concurrency, not volume

The only limit actually hit that day was **throughput, not spend**: 11,717 CU/s against a 10,000 cap,
caused by two backfills running at once rather than by either one being large. A provider-side
throughput cap turns that into throttling instead of billing, and costs nothing to set.

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

Measured before/after. `--seal-direct` changes two things at once, not one: the storage path, and the
fetch-windowing strategy (`window_adaptive: args.seal_direct` at `src/bench.rs:383-384` - no flag
separates them, so this table cannot isolate the storage path alone). On this range both arms did 34
requests, so the windowing difference never actually diverged the two runs here, and the sequential
comparison below is unaffected. Artifacts:
[`docs/bench/722-hot.json`](bench/722-hot.json), [`docs/bench/722-seal-direct.json`](bench/722-seal-direct.json).

| Path | Range | Events | Wall-clock | events/sec | RPC requests |
|---|---|---|---|---|---|
| hot store (decode → redb) | USDC, blocks 25,809,368–25,809,487 (public RPC) | 11,758 | 4.11 s | **2,860** | 34 |
| seal-direct (decode → Parquet) | same | 11,758 | 4.46 s | **2,634** | 34 |

**No speedup measured today - if anything ~8% slower (0.92×).** That contradicts both this page's own
prior entry (5.2×, measured hours earlier the same day, same commit lineage) and the architectural
reasoning below it: a buffered bulk Parquet write shouldn't lose to a redb B-tree point-insert plus
fsync per row. This is not published as a correction to 5.2× - it's a discrepancy neither number
explains. Reproduced twice, back-to-back (a 5-run and a separate 3-run session, both landing in the
same ~2,600-2,900 ev/s band for *both* paths): not a single noisy sample. Leading suspect is that this
ran on a heavily shared dev box with two other agents' `cargo test`/`cargo build` active during the
measurement window (load 4.8-6.4 on 32 cores) - redb's many small synchronous writes are exactly the
operation most exposed to shared disk contention, more than seal-direct's few large buffered ones.
**Unconfirmed.** Tracked in #744 for a controlled re-run before either figure is trusted as the
storage path's true delta.

**Events also don't match the prior entry: 11,758, not 12,933.** Verified directly against
`eth_getLogs` on three independent providers with no nuthatch binary involved - `eth.drpc.org`,
`eth.api.onfinality.io/public`, and `eth-pokt.nodies.app` (chunked at its own 50-block cap and
summed) - all three agree on 11,758. That range is ~9 months past finality, so the result is
deterministic; 12,933, reported both by this PR's own earlier commit and separately by Iris's
independent replication, appears to have been wrong rather than provider noise. Also tracked in #744.

**Provenance:** measured 2026-08-22 at commit `6145386`, 5 runs (median reported), provider
`eth-pokt.nodies.app` (`BenchReport.provider` - the first of the nest's RPC pool; public endpoints are
noisy and interchangeable within a run), 32 cores / 62 GB RAM. **Caveats:** public-endpoint throughput
varies several-fold run to run - Iris's own hot-store check on 2026-08-22 saw a 3.8× spread inside one
arm (172 ev/s on run 1 against 658-649 ev/s on runs 2-3) - so absolute ev/s is "what this provider gave
today," not a target. Both arms here declare no `[[calls]]`: every seal-direct path hardcodes an empty
calls slice (`src/bench.rs:695/719/741`, #725, open), so this is a bare-event workload and a nest with
`[[calls]]` will not reach these figures. The earlier 8.7× figure (2026-07-16, `f1a57de`) was measured
against a harness that called `put_entity` per row - one redb write transaction and one fsync per row -
rather than the `commit_window` path the indexer uses. #224 (`0cd291e`, 2026-07-30) fixed the harness;
8.7× was an upper bound against that strawman, not a measurement of the current code. Run it yourself:

```sh
nuthatch bench backfill --dir <nest> --from A --to B                 # hot store (baseline)
nuthatch bench backfill --dir <nest> --from A --to B --seal-direct   # seal-direct
```

## Pipeline (`--concurrency K`, seal-direct only)

Once the storage path is cheap, wall-clock is dominated by sequential `getLogs` round-trip latency.
The pipeline fetches `K` windows concurrently (`futures::stream::buffered`) but consumes results
**in block order**, so the sealed segments are byte-identical to the sequential path (asserted by
`indexer::pipelined_backfill_matches_sequential`). Bounded in-flight windows cap RSS.

Stacked measured result (USDC, same 120 blocks, public RPC). Provenance: 2026-08-22, commit
`6145386`, 5 runs (median), provider `eth-pokt.nodies.app`, 32 cores / 62 GB RAM. Artifact:
[`docs/bench/722-pipeline-8.json`](bench/722-pipeline-8.json).

| Path | events/sec | vs hot store | RPC requests |
|---|---|---|---|
| hot store (decode → redb) | 2,860 | 1× | 34 |
| seal-direct | 2,634 | 0.92× | 34 |
| seal-direct + 8-way pipeline | **8,315** | **~2.9×** | 24 |

```sh
nuthatch bench backfill --dir <nest> --from A --to B --seal-direct --concurrency 8
```

The pipeline itself still shows a real speedup - concurrent fetch genuinely overlaps public-endpoint
round-trip latency, ~3.2× over single-threaded seal-direct here, bounded by this provider - though part
of the ~2.9× vs. hot store is fewer, wider requests (24 against the hot arm's 34) rather than the
overlapped latency alone; against your own node (`--rpc`, `--concurrency 16`) it goes further. What
collapsed is the *storage-path* half
of the old ~22.5× (hot store vs. pipeline): see the seal-direct-vs-hot-store discrepancy above - see
also #744 - today's hot-store baseline came in far faster than the 607 ev/s this page previously
quoted, and that drags every "vs hot store" ratio in this table down with it. RSS rose to ~68 MB with
8 windows in flight - bounded by `K` and well within the 256 MB budget.

**Caveats:** public-endpoint throughput varies several-fold run to run (see the note above) - the
*ratios* here are the claim, not the absolute ev/s. All three arms declare no `[[calls]]` (#725, open)
- a bare-event workload; a nest with `[[calls]]` will not reach these multipliers.

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
  --min-reads 256 --max-point-read-p50-us 8
```

Pass none of them and the bench only reports, which is what an operator poking at their own nest
wants. Pass any of them and it is a gate.

That is the command CI runs, and it deliberately sets no p99 ceiling - see below for why the tail is
tracked rather than gated. `--max-point-read-p99-us` still exists for an operator who wants it.

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
- **p99 and p99.9 are tracked and gated on by nothing.** This is the design call, not an oversight:
  gating loosely and tracking precisely are two different jobs, and one ceiling cannot do both. A p99
  tight enough to catch a regression flakes on a shared VM; one loose enough to survive catches
  nothing. This gate briefly carried a 150µs p99 backstop on the second horn of that, and the table
  above is what retired it - the scan mutation only moved p99 to 34.45µs, so a 150µs ceiling did not
  fire on the one regression the gate exists to catch. A ceiling that passes the known break is not a
  weak gate, it is decoration that reads as coverage, and on a *required* check that is worse than no
  check because it stops anyone looking.

  The tail is still the number worth having - it is simply read with our eyes across releases rather
  than compared to a threshold. p99 over 256 samples is the 4th-worst read and preemption-dominated
  here; p99.9 swung 0.77-3.96µs at fixed commits. Neither is a property of the read path on this
  hardware, which is precisely why neither can fail a build. Both are in the uploaded report and the
  step summary.

  `MAX_P99_US` is still there for an operator on a quiet machine who wants the tail enforced
  deliberately; it just has no default and CI does not set it.

What this gate does **not** catch: a regression landing under 8µs - a partial scan, or a scan when the
hot store is much smaller than 256 rows - cold-start latency (it measures warm, see `warm_cache` in the
report), point-reads against the Postgres backend, and anything about the sealed/DuckDB path. It is a
floor on gross regressions, not a microbenchmark.

### The baseline is the runner's own artifact (issue #385)

`docs/bench/point-read.json` is the `point-read latency` job's uploaded report, taken verbatim from a
green run on `main` (commit `a53565a`, run
[31511769517](https://github.com/nightswatchhq/nuthatch/actions/runs/31511769517), p50 0.78µs, p99
0.93µs) and committed with nothing edited but a trailing newline. Its `hardware` field reads
`4 cores, 16 GB RAM`, which is the machine the table above was measured on and the machine the 8µs
ceiling is enforced on. **The committed baseline and the enforcing surface are now the same box.**

They were not, for as long as this gate has existed. The file recorded `32 cores, 62 GB RAM` and
p50 1.24µs from the dev box, which is 1.5-2.1x the runner's own baseline - so anyone re-deriving the
ceiling from the only committed report there was would have derived it from the wrong machine, on a
gate that had already been bitten by exactly that: 15µs came from the dev box and left 1.21x of real
margin below a regression rather than the 1.61x it claimed. The FAIL message in `bench query` had to
spend four lines telling readers *not* to use the one file named "baseline", which is a workaround
with a comment on it rather than a fix.

- **The dev-box record is kept**, as `docs/bench/point-read-devbox.json`. The 32-core numbers are the
  reference point for the footprint work and worth not losing; they are simply not what this gate is
  measured against, and the filename now says so.
- **`BASELINE` in `ci.yml` stops it drifting back.** The job compares the `hardware` its fresh report
  records against the `hardware` the committed baseline records, and fails on a mismatch. A committed
  number cannot go stale loudly - it can only be read and believed - so the check is the only thing
  that would notice.
- **Provenance is checked, values are not.** A "measured p50 within Nx of the baseline" rule is a
  second and much tighter ceiling in disguise: the runner's p50 has been seen from 0.58µs to 0.82µs
  at fixed commits, and its p99.9 from 0.77µs to 23.02µs, so any factor loose enough not to flake is
  looser than the 8µs gate already is. Which machine produced a file is the one thing a committed
  baseline can be definitively wrong about, so that is what is enforced.
- **Refreshing it:** download `point-read-report` from a green `point-read latency` run on `main` and
  commit it as `docs/bench/point-read.json`. `bench query` writes a trailing newline (#385), so the
  committed file is byte-identical to the artifact and a hand-edit is never needed. Do not touch the
  `hardware` field; if the runner spec has genuinely changed, the ceiling wants re-measuring, not the
  baseline re-labelling.


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

Both commands are bare on purpose: the harness's defaults **are** the scenario CI enforces (20 nests,
1000 backfill blocks, tip-follow to 20200), and the spread tool inherits them, so what you measure
here is what the gate measures. Export a knob and you are measuring something else - which matters
most in the one case you would want to, re-baselining a ceiling, because a smaller scenario yields a
smaller band and a ceiling below the healthy enforced figure (#395).

### Two ceilings, and why they are not one

| | ceiling | what a breach means |
|---|---|---|
| `MAX_RSS_MB` | 2048 MB | the **budget** was broken. A product promise, not a tuning parameter. |
| `REGRESSION_MB` | 180 MB | this scenario got materially more expensive. |

Both halves of that 180 are measured **on the runner that enforces it**, which is the only place the
margin is real:

| | |
|---|---|
| runner baseline | 131 MB (ceiling sits 1.37x above) |
| runner, carrying a deliberate leak | 323 MB (ceiling sits 1.79x below) |

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

The 143 above is the **top of the measured band** - the worst of the 15-run spread the regression
ceiling was derived from (111, 132, 133 x7, 134, 140, 141, 141, 143), because a ceiling is set from
the top of a band and not from its middle. The committed artifact
`docs/bench/multinest-footprint.json` is a **single run** of the same scenario, so it reads lower:
139 peak, 134 at-tip. Both are correct and they are not meant to match; if you have compared one
against the other and found them disagreeing, that is the reason.

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
