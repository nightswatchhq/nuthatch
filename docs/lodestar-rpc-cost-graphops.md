# What the Lodestar nests cost in RPC on GraphOps (tentative)

**What this is:** the RPC bill for the five Lodestar nests on the Helsinki box, backfill and
continuous running, priced in GraphOps credits. Measured and counted on **2026-10-05** against
**nuthatch 4.8.0**, tip near block 511,891,000 on Arbitrum One. It replaces the Alchemy figures in
[the Lodestar lifecycle](lodestar-lifecycle.md) §2, which were taken on 3.5.1 and a different
provider.

> **Status: tentative.** The running figures rest on two polls per nest, taken ten minutes after a
> restart. They are a first reading, not a figure to quote or budget against. Leave the nests running
> for several days, then replace the continuous section with what the per-key Usage page on
> platform.graphops.xyz reports for that period.

**How much of it is measured.** Continuous running is measured: per-method counters on the live
nests across polls. Backfill is **computed, not measured**: these nests were backfilled mostly on
Alchemy, so the backfill figure is a count of what the stored data would have required, priced at
GraphOps rates. The two are labelled throughout.

## The answer so far

| | credits | at Growth rate ($19.90/M) | at Starter rate ($26/M) |
|---|---:|---:|---:|
| Continuous, all nests, per 30 days | **~227,000** | **~$4.50** | **~$5.90** |
| Backfill, all nests, once | **1.16 M to 1.33 M** | **$23 to $26** | **$30 to $35** |

GraphOps sells plans, not credits, so the bill an operator actually sees is the plan:

- **Running:** 227,000 credits is 15% of Starter's 1.5 M. **$39 a month** covers all five nests with
  about 6.6x headroom.
- **The backfill month:** backfill plus the same month's running is 1.39 M to 1.56 M credits, which
  straddles Starter's 1.5 M. Either take one Growth month ($199) or backfill the allocations nest
  across a renewal. Whether Starter allows overage, and at what price, is not known here.
- **Year one:** $468 if the backfill fits Starter, $628 with one Growth month.

The allocations nest is 93% of the backfill and a quarter of the running cost. Everything else is
the price of polling.

## Prices used

Measured against the GraphOps endpoint on 2026-09-29, by calling each method and reading the usage
page: `eth_blockNumber`, `eth_chainId` and `eth_getBlockByNumber` cost **1 credit**; `eth_getLogs`
(any range up to the 25,000-block cap) and `eth_call` cost **2**. Plans: Starter $39 for 1.5 M
credits, Growth $199 for 10 M. These are the same schedules the admin cockpit's RPC tab carries.

## Continuous running (measured)

All four cursors run `--poll-interval 5m` with a 25,000-block window, so one `eth_getLogs` covers a
whole poll (five minutes of Arbitrum is about 1,200 blocks). Counters read at two instants 471
seconds apart, two polls between them, zero retries and zero failures on every nest:

| nest | `eth_blockNumber` | `eth_getLogs` | `eth_getBlockByNumber` | credits per poll |
|---|---:|---:|---:|---:|
| graph-allocations-nest | 1 | 1 | 3 | 6 |
| graph-gns-nest | 1 | 1 | 3 | 6 |
| dips-nest | 1 | 1 | 4 | 7 |
| data-services-nest | 1 | 1 | 3.5 | 6.5 |
| legacy-flows (`serve` only, no cursor) | 0 | 0 | 0 | 0 |

`eth_chainId` is three calls at startup and none afterwards. The three or four headers per poll are
the cursor's own fork check and are paid whether or not anything happened on chain.

On top of that, one header per block that carried a matching event. Counted from the stored data
over the last 10,368,000 blocks (30 days):

| nest | fixed, 8,640 polls | event-bearing blocks, 30 d | credits per 30 d | at $19.90/M |
|---|---:|---:|---:|---:|
| graph-allocations-nest | 51,840 | 6,518 | 58,358 | $1.16 |
| graph-gns-nest | 51,840 | 294 | 52,134 | $1.04 |
| dips-nest | 60,480 | 14 | 60,494 | $1.20 |
| data-services-nest | 56,160 | 0 | 56,160 | $1.12 |
| legacy-flows | 0 | 0 | 0 | $0 |
| **total** | | | **227,146** | **$4.52** |

Event headers are counted as additional to the fixed three, which is the upper reading. Even on the
allocations nest they are 11% of the line.

**The poll interval is the whole bill.** The fixed term scales inversely with it:

| poll interval | credits per nest per 30 d | four cursors | plan |
|---|---:|---:|---|
| 5 m (deployed) | ~52,000 | ~227,000 | Starter |
| 1 m | ~260,000 | ~1.04 M | Starter, just |
| 2 s (the block-time default) | ~7.8 M | ~31 M | beyond Growth |

A nest started without `--poll-interval` on a credit-metered endpoint is a $600-a-month mistake.

## Backfill (computed)

Three terms per nest: one `eth_getLogs` per 25,000-block window from the earliest `start_block` to
tip, one `eth_getBlockByNumber` per distinct block holding a matching event (`block_timestamps =
true` on all four), and the `[[calls]]` samples.

| nest | from block | windows | event-bearing blocks | logs stored | credits |
|---|---:|---:|---:|---:|---:|
| graph-allocations-nest | 42,449,227 | 18,778 | 990,364 | 4,958,153 | 1,084,260 |
| graph-gns-nest | 0 (none declared) | 20,476 | 28,060 | 74,694 | 69,012 |
| dips-nest | 486,895,281 | 1,000 | 32 | 66 | 2,032 |
| data-services-nest | 456,917,519 | 2,199 | 7 | 7 | 4,405 |
| **total** | | **42,453** | **1,018,463** | | **1,159,709** |

The allocations line includes 56,340 credits for `[[calls]]`: six samples at `every = 100000`,
taken as if all six ran from the first block, which overstates it (three begin at Horizon, block
408,847,369).

**Allowance for what was not counted.** At tip each poll pays about four credits of overhead beyond
its `eth_getLogs`. If a backfill window pays the same, that is another 170,000 credits, which gives
the upper figure of **1.33 M**. Retries are not in either figure: Alchemy cost a measured 1.28x on
this workload because it dropped blocks from large batches; GraphOps showed none at tip, and its
backfill behaviour has not been observed.

The shape is the one the 2026-08-22 measurement found: headers, not logs, are the bill. 990,364 of
the allocations nest's 1,084,260 credits are block headers fetched for timestamps.

## A second operator does not have to pay the backfill

A nest's data is keyed by its content address, and a nest directory resumes from its own
`last_block`. An operator who is handed a copy of the directory and runs the same nest pays only for
the blocks between the copy and their own first poll: one `eth_getLogs` per 25,000 blocks, which is
104 minutes of Arbitrum, plus a header per event-bearing block in the gap. A day-old copy of the
allocations nest costs about 250 credits to catch up, half a cent.

What exists for this today, and what does not:

- **Copying the directory works now.** Sealed segments are immutable and safe to copy live; the hot
  store (`nuthatch.redb`) needs the process stopped or a filesystem snapshot
  ([operators](operators.md#data-lifecycle)). This is how the QoS nest moved boxes on 2026-09-26.
  The recipient must run the same nest, since an edited nest has a different address and would start
  empty.
- **The published mirror is not yet a way in.** `nuthatch publish sync` (RFC-0052) mirrors sealed
  segments to a bucket for readers. Seeding a new nest from one, `init --from-mirror`, is RFC-0052
  §8's later work and is not built. The mirror also carries no hot store, so it stops at
  `sealed_through`.
- **A copy is taken on trust.** Segment files are content-addressed, so a recipient can tell a
  corrupted file from an intact one, but nothing lets them check that the history is what the chain
  said short of re-indexing it. Among cooperating operators that is acceptable; it is not
  verification.

For a fleet, the consequence is that the $23 to $35 backfill is paid once per nest, not once per
operator, and a new operator's entry cost is the $39 plan.

## What this does not establish

- **The backfill on GraphOps has not been run.** Every backfill figure is arithmetic over stored
  rows. Rate limits, 504s on long ranges (Kittiwake's block-0 census met one) and retries could move
  it, and only a real run on a metered key would say by how much.
- **The running figure rests on two polls per nest** after a restart at 09:57 UTC. The per-key Usage
  page on platform.graphops.xyz after a full day is the check, and the number to quote.
- **Event density is the last 30 days'.** A busier month raises the allocations line only: 20,000
  event blocks instead of 6,518 adds $0.27.
- **Query traffic costs no RPC at all**, so none of this depends on how much Lodestar reads.
- The VPS is not in these figures.
