# The port-queue nest - finding the underserved with nuthatch

- Status: **design, unbuilt. The claim it rested on is now verified** (2026-08-19, §5).
- Author: Pete (cargopete), with Jenny
- Date: 2026-08-19
- Related: [community.md](community.md) §2 (the loop this feeds), [nest-catalogue.md](../nest-catalogue.md)
  Tier 0.1, [RFC-0011](../rfcs/0011-graph-network-nest-lodestar-migration.md) (the nests this builds on)

## 1. What this is

The [port loop](community.md) needs a queue: deployments whose owners have a data problem *now*. This
document designs the smallest thing that produces one, and notes that it is not a subgraph port but a
three-contract join, most of which is already running in production.

It is also the thesis demonstrating itself. Using nuthatch to find the people nuthatch is for is a
better argument than any sentence in the launch copy.

## 2. The filter is the whole design

The obvious query is "deployments with no open allocation." **That query is nearly useless.** Thousands
of deployments have no allocation because they are dead test subgraphs nobody has thought about in
years. Ranked that way the list is mostly corpses, and reading it is a week gone.

The sharp query is one word different:

> **Curation signal, with no open allocation.**

Someone has GRT at stake on a deployment, and nobody is serving it. That is a party with a
demonstrated, financial, on-chain interest in data they are not currently receiving, and the size of
the signal ranks the list without any further judgement. It is the difference between a list of the
dead and a list of the underserved.

Everything else here is plumbing in service of that one predicate.

## 3. Three sources, one join

| Source | Gives | Status |
|---|---|---|
| `L2GNS` `SubgraphPublished` | the deployment universe | **already indexed** - `graph-gns-nest`, live in production |
| `SubgraphService` allocation events | who is serving what, now | **new contract, verified** - `0xb2Bb92d0DE618878E438b55D5846cfecD9301105` |
| `L2Curation` `Signalled` / `Burned` | who paid to have it served | **new contract, verified** - `0x22d78fb4bc72e191C765807f8891B5e1785C8014` |

One SQL join across those three answers the question. No UI, no scoring service, no automation, no
scheduled report. The deliverable is a query and a ranked list.

### Why this is not "port the network subgraph"

[nest-catalogue.md](../nest-catalogue.md) Tier 0.1 calls the network subgraph "the crown jewel" and
ranks it on demand grounds. That framing is correct and describes a **large** job with a great deal in
it that nobody would read. This is a much smaller thing wearing the same hat: three contracts and one
predicate, scoped to a single question. Build this; the full Tier-0 nest remains a separate, later
decision on its own merits.

## 4. The correction about Horizon

An earlier version of this plan assumed allocation events could be added to `graph-staking-nest` as an
allowlist tweak, since that nest already indexes `HorizonStaking` on Arbitrum and vendors its ABI.

**That is wrong.** The vendored ABI at `graph-staking-nest/abis/staking.json` carries 28 events and
**not one of them is an allocation.** It is provisions, delegation and thawing throughout:
`ProvisionCreated`, `TokensDelegated`, `ThawRequestFulfilled`, and so on. The nest's own allowlist
comment says as much, describing "all 28 HorizonStaking tables" as delegation surface.

This is Horizon working as designed. `HorizonStaking` became the generic provision layer, and
allocations moved out to the **SubgraphService** data service. So this is one new contract, not a
two-line config change. Still small, still on a chain already indexed, still on machinery that exists -
but a day, not an afternoon.

## 5. Verified (2026-08-19)

The design rested on an inference: that Horizon moved allocations out of `HorizonStaking` and into
`SubgraphService`. **It holds.** Checked with nuthatch itself rather than by reading documentation.

Addresses cross-checked against two independent sources - `graphprotocol/contracts`
`packages/subgraph-service/addresses.json`, and Lodestar's own `src/lib/wallet.ts`, which already
carries the same `SubgraphService` address for its staking-pool calls.

**`SubgraphService`** - `0xb2Bb92d0DE618878E438b55D5846cfecD9301105` (Arbitrum One). 31 events. The
four that matter: **`AllocationCreated`, `AllocationClosed`, `AllocationResized`,
`LegacyAllocationMigrated`.**

**`L2Curation`** - `0x22d78fb4bc72e191C765807f8891B5e1785C8014` (Arbitrum One). 7 events, including
**`Signalled`, `Burned`, `Collected`**.

Both are proxies, and **nuthatch resolved each to its implementation unassisted** (`proxy →
implementation 0x80d1a234…` and `0xc4ce508c…`, both then resolved via Sourcify). The proxy-ABI trap
that bites a naive Sourcify lookup did not apply.

### The contract carries more than this nest needs

`SubgraphService` also emits `IndexingRewardsCollected`, `QueryFeesCollected`,
`ServicePaymentCollected`, `ServiceProviderRegistered`, `ServiceProviderSlashed`, `ServiceStarted`
and `ServiceStopped`. Those cover Lodestar's `payments`, `rewards-history`, `subgraph-fees-30d` and
parts of the indexer directory - **more of the migration than one contract was estimated to buy.**
Keep this nest's allowlist narrow anyway, per RFC-0011's own precedent, and let the Lodestar work
widen it deliberately.

### The one thing still open: start blocks

`nuthatch init` reported *"deployment block undetected … backfill starts from a tip offset"* for both,
because the public Arbitrum RPC has no archive state that far back. A tip offset is fine for a scaffold
and **wrong for this nest**, which needs full history to know whether an allocation ever closed.

Deployment blocks must be pinned before backfilling. `graph-staking-nest` pins `start_block =
42449585`, which is the Horizon deployment and is the obvious candidate to verify against, since these
contracts went live in the same event (GIP-0066, Arbitrum mainnet, 2 December 2025).

## 6. What this cannot do

An earlier claim in [community.md](community.md) said the network subgraph shows which deployments have
no allocation "**or are falling behind**". The first half is right. The second is not available from
this data: **indexing lag is off-chain**, exposed by indexer status endpoints, not emitted as an event.

**This nest answers *unserved*, never *unhealthy*.** A deployment with a healthy-looking allocation may
be served by an indexer that is weeks behind, and nothing here would show it. Do not let the queue
imply a health check it does not perform.

Query volume is likewise not on-chain in any per-deployment form, so "much-queried and unserved" is not
answerable this way either. Signal is the available proxy for demand, and it is a proxy.

## 7. Build order

1. ~~**Confirm `SubgraphService`**~~ - **done 2026-08-19** (§5). Remaining prerequisite: **pin the
   deployment blocks**, since a tip offset cannot answer "did this allocation ever close".
2. **Add the two contracts.** `SubgraphService` allocations and `L2Curation` signal, on `arbitrum-one`,
   alongside the nests already running.
3. **Write the join.** Signal, no open allocation, ranked by signalled tokens.
4. **Read the top of the list by hand.** Before any automation. If the top twenty are not recognisably
   real projects with real problems, the filter is wrong and no amount of tooling fixes that.
5. **Only then** consider anything scheduled.

Step 4 is the gate. A queue nobody has manually validated is a lead-generation system for an
unconfirmed market, which is the trap this document exists to avoid falling into twice.
