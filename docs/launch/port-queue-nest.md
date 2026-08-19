# The port-queue nest - finding the underserved with nuthatch

- Status: **design, unbuilt, and one claim in it is unverified.** See §5 before acting on §3.
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
| `SubgraphService` allocation events | who is serving what, now | **new contract** - address and event names unconfirmed, see §5 |
| `L2Curation` `Signalled` / `Burned` | who paid to have it served | **new contract** |

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

## 5. What is unverified, and blocks §3

**That allocations live in `SubgraphService` is inference from the Horizon architecture, not something
read out of a deployed contract's ABI.** It has not been confirmed. Confirm the address and the
allocation event names before building anything in §3, because the entire design rests on it.

Also unconfirmed: whether pre-Horizon allocations in the legacy staking contract matter here. For
*currently* unserved they should not, since only present state is asked about, but that reasoning has
not been checked against the migration's actual shape.

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

1. **Confirm `SubgraphService`** - address, ABI, allocation event names. Blocks everything (§5).
2. **Add the two contracts.** `SubgraphService` allocations and `L2Curation` signal, on `arbitrum-one`,
   alongside the nests already running.
3. **Write the join.** Signal, no open allocation, ranked by signalled tokens.
4. **Read the top of the list by hand.** Before any automation. If the top twenty are not recognisably
   real projects with real problems, the filter is wrong and no amount of tooling fixes that.
5. **Only then** consider anything scheduled.

Step 4 is the gate. A queue nobody has manually validated is a lead-generation system for an
unconfirmed market, which is the trap this document exists to avoid falling into twice.
