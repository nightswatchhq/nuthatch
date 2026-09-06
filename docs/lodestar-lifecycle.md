# The Lodestar lifecycle: what hosting a nest actually looks like

**What this is:** one page describing the whole loop - RPC in, storage on disk, SQL out - for the
five nests the [Lodestar dashboard](https://www.lodestar-dashboard.com) reads in production, so an
operator considering hosting nuthatch nests knows what to expect before committing a box to it.

**Provenance:** every figure was read off the live Hetzner VPS on **2026-09-06**, in two passes: one
between 12:47 and 13:20 UTC against **3.5.0**, and one between 16:00 and 17:05 UTC against **3.5.1**,
which went on the box at 15:06. The 3.5.1 pass is the current state and is what the tables below
carry. Nothing here is projected. Where a number comes from a short window, the window is stated,
because a rate without one is not a number.

**The box:** `ubuntu-8gb-hel1-1`, 4 cores, 7.7 GB RAM, 150 GB disk. It carries six nuthatch
processes, a TAP gateway and Caddy, at a load average of 0.63, with 91 GB of disk and 5.3 GB of
memory free. Nothing here needs a large machine.

---

## 1. The shape

One host, one basic-auth credential, five nests selected by base path. Lodestar holds
`NUTHATCH_URL`, `NUTHATCH_USER` and `NUTHATCH_PASSWORD`, and nothing else. There is no Graph API
key in the dashboard, no gateway client, and no fallback: an unreachable nest is reported as
unavailable rather than silently answered from somewhere else.

| base path | port | nest | mode | what it serves |
|---|---:|---|---|---|
| `/alloc` | 8107 | `graph-allocations-nest` | `dev`, tip, 5 min poll | 33 of the 34 files in Lodestar's `src/` that name a base path |
| `/gns` | 8113 | `graph-gns-nest` | `dev`, tip, 5 min poll | subgraph names, Developer Activity |
| `/dips` | 8104 | `dips-nest` | `dev`, tip, 5 min poll | indexing-agreement lifecycle |
| `/dips-sepolia` | 8106 | `dips-nest-sepolia` | `dev`, tip, 5 min poll | the same on testnet |
| `/legacy-flows` | 8103 | `graph-staking-legacy-history` | `serve`, frozen | Delegation Flows |

**One nest carries almost everything.** `/alloc` fronts what used to be three separate cursors, and
that concentration is the single most important operational fact on this page: when `/alloc`
saturates, every panel it feeds reports unavailable at the same moment while the other four answer
perfectly well.

**`/legacy-flows` is the control.** It is `nuthatch serve` over sealed segments with no cursor. Over 49 hours of uptime it
answered 91 HTTP requests for **zero** RPC calls, in 39 MB of RSS, and it reports `stalled` for ever
because its head genuinely never moves. Every RPC call elsewhere on this page buys tip-following,
not serving.

---

## 2. The write side: what a cursor costs

`graph-allocations-nest` declares **12 contracts** and **12 ABIs**, and exposes **81 tables** plus
12 authored view files.

The RPC bill is the cursor's polling cadence, not the data. Of a recent day's 345,600 Arbitrum
blocks, 95 carried a Graph event. The headers a cursor buys are overwhelmingly its own reorg check
and finality probe, and that is what `--poll-interval` addresses (RFC-0040, carve-out 5).

**Measured before and after the dial, on the same nest:**

| | poll every 2 s | poll every 5 min |
|---|---:|---:|
| window | 45 s sample | **54 min, 15:06-16:00 UTC** |
| RPC requests/min | 413 | **4.6** |
| Alchemy CU/min | ~9,900 | **~110** |
| CU/month | ~430 M | **~4.8 M** |
| pay-as-you-go | ~$185/month | **~$2.10/month** |

That is a **~90x** reduction for a cursor that indexes exactly the same rows: the sealing path cuts
segments where the rows say to, not where the clock says to, so the sealed output is byte-identical
either side of the change. What it costs is freshness, and the ceiling is stated below.

The right-hand column is a clean 54-minute window taken after a rolling restart, with no traffic of
mine in it. `docs/sprint-frugal-finch.md` step 2 asks for this repeated across a full day before the
number is written down, and that has not been done.

Every nest, over the same 54 minutes:

| port | nest | RPC/min | `eth_getBlockByNumber` | `eth_getLogs` | `eth_blockNumber` | CU/min |
|---:|---|---:|---:|---:|---:|---:|
| | | | *calls / 54 min* | *calls / 54 min* | *calls / 54 min* | |
| 8107 | allocations | 4.6 | 165 | 37 | 48 | **~110** |
| 8104 | dips | 4.1 | 133 | 38 | 49 | ~100 |
| 8106 | dips-sepolia | 10.4 | 332 | 109 | 118 | ~265 |
| 8113 | gns (**backfilling**) | 150 | 5,442 | 1,359 | 1,367 | ~3,754 |
| 8103 | legacy (archive) | **0** | 0 | 0 | 0 | **0** |

The two Arbitrum One cursors Lodestar actually reads come to **210 CU a minute between them**,
against the sprint's target of under 1,000.

`eth_getBlockByNumber` is 60-65% of every bill, and it is the block-timestamp fetch. Turning
`block_timestamps` off would remove it, and it is not available here: three of the allocations
nest's views use `block_timestamp`, and flipping the flag is a breaking schema change that rewrites
every sealed segment. Structural, not a config edit.

**8113 is what a backfill costs.** A nest catching up spends roughly 35x a nest at tip. Budget for
it as a one-off, and note it is running on keyless public endpoints rather than a paid key.

---

## 3. Footprint

| port | nest | RSS | on disk | segments |
|---:|---|---:|---:|---:|
| 8107 | allocations | 245 MB | 661 MB | 1,924 Parquet files, 643 MB; redb 16 MB |
| 8104 | dips | 220 MB | 104 MB | |
| 8113 | gns | 67 MB | 53 MB | |
| 8106 | dips-sepolia | 27 MB | 83 MB | |
| 8103 | legacy archive | 39 MB | 100 MB | |

Well inside the ≤2 GB per-cursor budget, with five cursors on one 7.7 GB box. **Do not read RSS
straight after a restart** - the in-memory views have not rebuilt, and the figure is an order of
magnitude low for the first few minutes.

---

## 4. The read side: how a consumer asks

Lodestar's read pattern is the thing an operator is actually sizing for, and it is unremarkable:
**17 cron schedules between 2 minutes and 6 hours**, plus page traffic behind a CDN.

| cadence | crons |
|---|---|
| 2 min | horizon activity |
| 5 min | refresh, network snapshot, TAP provision |
| 10 min | epochs, bounties, notifications, DIPS check, IPFS warm |
| 15 min | delegations, provider liveness, nest health |
| 30 min | chain health |
| hourly | allocations, RAV, DIPS chain |
| 6 hourly | disputes |

Every serving route sits behind a CDN cache: `s-maxage` runs from 30 s (votes) through 300 s (most
panels) to 86,400 s (ENS). So the nest sees roughly **one read per route per TTL**, not one per
visitor. Measured on the allocations nest over a clean 70-minute window: **380 queries, about 326 an
hour, none refused**, for a dashboard serving the public.

Two client-side disciplines make this work, and an operator hosting for someone else should insist
on both:

- **A one-slot gate.** `src/lib/nuthatch.ts` admits one query at a time per nest, well under the
  node's cap, so the consumer's own composition can never be the cause of a refusal. It bounds one Node process;
  serverless runs many, which is what the retry ladder is for.
- **A readiness gate.** Serving routes go through `nuthatchSqlReady`, which probes `/ready` first and
  returns 503 rather than serving three-week-old rows from a stalled nest. Alerting crons may skip
  it; the page the user sees may not.

---

## 5. Query performance, measured

Three repetitions each, sequential, against the live production nest on **3.5.1** while it was also
serving Lodestar. This is what a real consumer experiences, not a quiet-box benchmark. The 3.5.0
column is the same script two hours earlier, on a build that was crashing and a busier box, so read
it as the surrounding conditions rather than as a clean version comparison.

| query | rows | 3.5.1 | 3.5.0 |
|---|---:|---|---|
| `count(*) lodestar_curators` | 1 | 0.08 / 0.08 / 0.08 s | 0.13 / 1.20 / 1.26 s |
| `lodestar_network_params` | 1 | 0.14 / 0.15 / 0.15 s | 1.26 / 1.31 / 1.31 s |
| `count(*) lodestar_allocations` | 1 | 0.71 / 0.74 / 0.76 s | 1.71 / 1.76 / 1.84 s |
| `lodestar_epochs` top 100 | 100 | 1.95 / 2.02 / 2.05 s | 2.74 / 2.76 / 2.81 s |
| `lodestar_indexers` top 100 | 98 | 4.37 / 4.74 / 4.77 s | 6.39 / 8.03 / 10.32 s |
| `lodestar_network` | 1 | 6.05 / 6.10 / 6.13 s | 7.39 / 8.87 / 8.94 s |
| `count(*) lodestar_delegator_stakes` | - | **out of memory, 1.5-1.8 s, HTTP 400** | out of memory, then a segfault |

**Set expectations at seconds, not milliseconds, for anything that aggregates.** These are authored
SQL views (RFC-0018 §1), evaluated at request time over hot ∪ sealed on every call. They are named
queries, not materialised ones. The consumer's request timeout is 15 s and the node's own wall clock
is 30 s, so the network view at 6.1 s has under 2.5x of headroom. RFC-0041's authored incremental
entities are the answer to this and are shipped; these views have not been migrated onto them.

**A `count(*)` can exhaust the memory budget, and that is the guard working.** `max_memory` per
DuckDB connection is `min(512 MB, 1024 MB / permits)`, so on this box it is **256 MB**, observed as
`244.1 MiB` in the error text. The same query on 3.5.0 with two permits saw `488.2 MiB`. Raising
`NUTHATCH_SQL_MAX_CONCURRENCY` therefore buys throughput by taking memory away from every individual
query, which is the trade an operator is actually making.

**Concurrency: the default is two, and this deployment sets four.** The unit carries
`NUTHATCH_SQL_MAX_CONCURRENCY=4` (and `NUTHATCH_HOT_STORE_CACHE_BYTES=268435456`). Ten simultaneous
requests over three rounds were admitted nine times and refused twenty-one, consistent with four
permits partly occupied by the dashboard. A refusal is `503 server busy: too many concurrent SQL
queries`, returned in **1.7-3.2 ms**: `try_acquire_owned`, so a caller past the limit is refused
immediately rather than queued. That is deliberate self-protection.

**Under ordinary dashboard load alone, refusals are zero.** The allocations nest admitted **380
queries in 70 minutes with no rejections at all**. Refusals appear the moment a second independent
caller arrives, and an earlier figure on this page of 159 refused against 381 admitted was measured
with me as that second caller. An operator should read a rejection count as "someone else is also
asking", not as a fault, and a consumer should read it as backpressure to retry.

---

## 6. Freshness, and how to judge it

Every `/sql` answer carries a provenance block: `as_of`, `sealed_through`, `registry_hash`, `nid`.
Live on 8107 at the time of writing: `tip` 502,346,920, `sealed_through` 501,993,721, `lag_blocks` 0,
`seconds_since_poll` 28. The hot window is ~353,000 unsealed blocks, roughly a day of Arbitrum.

- **The honest freshness signal is `as_of` climbing between two reads.** Not "time since the last
  event" - delegation is low-frequency, and a two-hour gap is the event rate, not a stall.
- **At a 5-minute poll, worst-case staleness is 5 minutes plus block time.** Every Lodestar panel
  refreshes on a 2-to-15-minute cron, so this is invisible to all of them. `/ready` states the mode
  and interval, and its stall threshold scales with the interval.
- **A frozen archive reports `stalled` for ever and is correct to.** Exclude it from alerting
  deliberately rather than by accident.

---

## 7. What goes wrong

Ranked by how likely an operator is to meet it.

1. **Concurrency refusals as soon as there is a second caller.** Four permits here, two by default.
   Expected, and self-inflicted only if the consumer fires a `Promise.all`. Fix it client-side.
2. **The DuckDB memory budget bites on innocuous SQL.** `SELECT count(*)` on one view exhausted the
   256 MB per-connection limit in 1.5 s. Four scalar subqueries in one statement did the same. The
   guard is doing its job; the surprise is how cheap the query looked. On 3.5.1 this is a clean
   `400` with the limit quoted in the error text.
3. **A slow view against a tight client timeout.** 6.1 s against 15 s. A view that grows, or a box
   under contention, closes that gap without warning.
4. **Segfaults on the serving path, now fixed.** `graph-allocations-nest` took **31 SEGVs on
   2026-09-06** on 3.5.0, `tokio-rt-worker` killed with `status=11/SEGV`. The cause was several
   DuckDB instances in one process defaulting `temp_directory` to the nest's own data directory and
   overwriting each other's spilled blocks
   ([#1165](https://github.com/nightswatchhq/nuthatch/issues/1165),
   [#1182](https://github.com/nightswatchhq/nuthatch/pull/1182)). 3.5.1 gives each instance a private
   spill directory. The box has recorded **no kernel segfault since 12:50 UTC** and `NRestarts=0`
   across all four units since the 15:06 roll. The query that reliably killed 3.5.0 now returns a
   clean out-of-memory instead.
5. **Rate limiting on keyless endpoints.** Recovered from, and RFC-0040's knob 4 now widens the
   window again once refusals stop, so a throttled hour costs an hour rather than the rest of the
   backfill.

**Two operational rules earned the hard way.** `Restart=always`, not `on-failure` - a clean SIGTERM
exits 0, systemd calls that success, and nothing restarts. And never `pkill`: `-x nuthatch` matches
every production process on the box and once took it down for 80 minutes. Kill by port.

---

## 8. The summary an operator wants

- **A tip-following cursor costs about $2 a month in RPC** at a 5-minute poll, and about $185 at a
  2-second one. The cadence is the bill.
- **A nest nobody reads costs the same as one under load.** Tip-following cost is independent of
  demand. Park a nest and it keeps billing; a `serve`-only archive bills nothing.
- **Five cursors, 7.7 GB, 4 cores, 1 GB of nest data, load 0.63.** The hardware is not the
  constraint.
- **Reads are sub-second to seconds, and concurrency is a handful.** Size the consumer's caching,
  not the box, and know that raising the permit count takes memory from every individual query.
- **Backfill is the expensive part.** Roughly 35x tip, once.
