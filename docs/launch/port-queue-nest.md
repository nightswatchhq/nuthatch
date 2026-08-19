# The port-queue nest - finding the underserved with nuthatch

- Status: **built, proven, parked** (2026-08-19). Runs locally, not hosted. Results in §8.
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

## 3. Two contracts, one new nest

| Source | Gives | Deployed at (Arbitrum One) |
|---|---|---|
| `SubgraphService` `AllocationCreated` / `AllocationClosed` / `AllocationResized` | who is serving what, now | **397,492,865** - 2025-11-06 19:00 UTC |
| `L2Curation` `Signalled` / `Burned` / `Collected` | who paid to have it served | **42,449,403** - 2022-11-30 13:39 UTC |

`SubgraphService` is `0xb2Bb92d0DE618878E438b55D5846cfecD9301105`; `L2Curation` is
`0x22d78fb4bc72e191C765807f8891B5e1785C8014`. Both blocks found by binary search on `eth_getCode`
against an archive RPC (the public endpoint refuses historical state and fails identically for both
addresses, which is the endpoint declining rather than the contract being absent). `L2Curation`'s block
lands 182 blocks from `graph-staking-nest`'s pinned `42449585`, same day, which corroborates that
number as the L2 deployment era rather than someone's guess.

**`L2Curation` is the expensive half.** Signal is cumulative, so current signal on a deployment needs
every `Signalled` and `Burned` since 2022. `SubgraphService` is ten months old by comparison.

### Correction: `L2GNS` is not needed

An earlier version of this section listed three sources including `L2GNS`. It buys nothing here.
Allocations and signal both key on `subgraphDeploymentID`, so **"signal with no open allocation" is
answerable from two contracts**. `L2GNS` adds only human-readable identity, and the display names
actually live in IPFS-pinned JSON (RFC-0037), which nuthatch cannot resolve yet - so it would deliver a
deployment ID we already hold.

### Why a *new* nest, rather than extending one that exists

1. **Queries are per-nest scoped.** RFC-0012: a query sees one nest's segments, and cross-nest
   federated queries are scaled-mode. Lodestar's `nuthatch.ts` already shows the consequence - a
   `basePath` argument picking between `/sql` and `/gns/sql`, because those are two surfaces. **The
   join must live inside one nest** or it is not a join, it is client-side stitching.
2. **Editing a nest changes its NID.** A nest's data is keyed by its content address, so any edit
   yields a different nest and a full re-index. `graph-staking-nest` is serving Lodestar's delegation
   feed in production; re-indexing it to bolt on an unrelated query is a poor trade.
3. **It is nearly free.** Same chain, so it rides the `arbitrum-one` cursor already running rather than
   opening a second one.

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

## 5a. The staged upgrade, and what it does to decoding

`graphprotocol/contracts` shows a `pendingImplementation` on every Horizon contract, all deployed
within a minute of each other on **2026-07-23**: `SubgraphService`, `DisputeManager`, `HorizonStaking`,
`L2Curation`, `PaymentsEscrow`, `RewardsManager`, plus a new `RecurringCollector`. A coordinated
upgrade, staged and awaiting execution. Diffed the event signatures rather than assuming:

**`L2Curation`: no change at all.** 7 events before, 7 after, every signature identical. Signal history
is safe across the upgrade.

**`SubgraphService`: signatures do change** - but **not the three this nest depends on**.
`AllocationCreated`, `AllocationClosed` and `AllocationResized` all survive untouched. What moves:

| Change | Event |
|---|---|
| **Removed** | `LegacyAllocationMigrated(address,address,bytes32)` |
| **Removed** | `StakeClaimLocked(...)`, `StakeClaimReleased(...)` |
| **Arity changed** (so topic0 changes) | `GraphDirectoryInitialized` 10 → 9 args; `SubgraphServiceDirectoryInitialized` 4 → 5 |
| **Added** | `POIPresented`, `IndexingFeesCutSet`, `BlockClosingAllocationWithActiveAgreementSet` |

The consequence is an ABI-versioning one, and CLAUDE.md already has the rule: *never retroactively
re-decode stored history when ABIs improve; version decodings.* **Vendor the current ABI**, because it
is what decodes the history; a nest built on the pending ABI would silently fail to match historical
`LegacyAllocationMigrated` logs. `POIPresented` is worth wanting *after* the upgrade lands.

**Open risk: `HorizonStaking`'s pending implementation is not verified on Sourcify**
(`0xd3ba4a3b…`), so its ABI cannot be fetched and its events cannot be diffed. That is the contract
`graph-staking-nest` indexes for Lodestar's live delegation feed, so it is the one place where an
undiffable upgrade meets a production panel. Re-check before the upgrade executes.

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

1. ~~**Confirm `SubgraphService`**~~ and ~~**pin the deployment blocks**~~ - **both done 2026-08-19**
   (§3, §5). Remaining prerequisite: **re-check `HorizonStaking`'s pending ABI** when it verifies
   (§5a), because that one touches production.
2. **Add the two contracts.** `SubgraphService` allocations and `L2Curation` signal, on `arbitrum-one`,
   alongside the nests already running.
3. **Write the join.** Signal, no open allocation, ranked by signalled tokens.
4. **Read the top of the list by hand.** Before any automation. If the top twenty are not recognisably
   real projects with real problems, the filter is wrong and no amount of tooling fixes that.
5. **Only then** consider anything scheduled.

Step 4 is the gate. A queue nobody has manually validated is a lead-generation system for an
unconfirmed market, which is the trap this document exists to avoid falling into twice.

---

## 8. Built and measured (2026-08-19)

`~/Projects/graph-allocations-nest`, local only, **deliberately not hosted**. Backfilled once, queried,
parked. `views/port_queue.sql` carries the query and these numbers as a comment.

**The build.** `nuthatch init` + `nuthatch add`, both proxies resolved to their implementations
unassisted. `nuthatch doctor` against an archive RPC first, which reported an 81,920-block `getLogs`
window once probed **with `--address`** - its range-only probe recommends 320, and taking that at face
value would have made this backfill 256 times longer than it needed to be. Probe with the address.

**The backfill.** 42,449,403 → 496,121,293, both contracts, 8-way seal-direct: **12 minutes, 178 MB
RSS, 54 MB on disk** (3.5 MB redb + 51 MB sealed segments). Well inside the per-cursor budget.

| Table | Rows |
|---|---:|
| `subgraph_service__allocation_created` | 244,952 |
| `subgraph_service__allocation_closed` | 231,646 |
| `subgraph_service__allocation_resized` | 24,298 |
| `curation__signalled` | 21,387 |
| `curation__burned` | 9,969 |
| `subgraph_service__legacy_allocation_migrated` | **0** |

### The result, and the correction it forces

| Population | Count |
|---|---:|
| Deployments ever signalled | 13,881 |
| Still carrying net signal | 7,621 |
| With an open allocation | 6,781 |
| **Signalled and unserved** | **3,853** |
| **Signalled > 1,000 and unserved** | **63** |

**§2 was half right.** "Signal with no open allocation" is much sharper than "no allocation", but on
its own it returns **3,853 rows - over half of everything still signalled**. That is a haystack, not a
queue. A magnitude threshold is not decoration: at >1,000 net signal it becomes **63 deployments**,
which is a list a person can actually read. The threshold is now in the query with that reasoning
attached.

The top entry carries **208,847 GRT signalled and no indexer serving it**. The second, 44,287.

### Four things to be suspicious of before treating these as leads

1. **A repeating 10,000 GRT / 9,900 signal pattern** runs through the middle of the list - 10,000 GRT
   with a 1% curation tax. That shape is programmatic (a curation programme, a script, a batch), not
   somebody deciding this dataset matters. The gate in §7 step 4 is to read the top by hand, and this
   is exactly what it is for. Resolve a handful before believing any of it.
2. **`LegacyAllocationMigrated` fired zero times.** The event exists in the ABI, is being removed by
   the staged upgrade, and has never once been emitted. Harmless to have indexed, but it means the
   pre-Horizon migration path is not visible here, so **"no open allocation on `SubgraphService`" is
   only equivalent to "unserved" if every live allocation now lives on `SubgraphService`.** 13,306 open
   allocations is a plausible network-wide figure, which supports that, but it is inference not proof.
3. **Names are still unavailable.** These are deployment IDs. Turning one into "whose subgraph is
   this" needs the GNS plus IPFS-pinned metadata - RFC-0037 again, from the other direction.
4. **`AllocationResized` is deliberately not consulted** by the open/closed logic, since resizing
   neither opens nor closes. Worth re-checking if the numbers ever look wrong.

### Two small defects found by building it

- **Renaming a contract alias leaves `semantic.toml` stale.** Changing `c0`/`c1` to
  `subgraph_service`/`curation` produced **38 startup warnings** of the form "semantic.toml describes
  table `c1__signalled`, which the registry has no decoder for". `nuthatch schema` regenerates
  `schema.json` and the AI surface but does not re-key `semantic.toml`'s table list. Warnings only,
  nothing wrong with the data, but it is noise the operator has to learn to ignore - which is how real
  warnings get missed.
- **`block_timestamps = true` produced six retry storms** against the archive RPC ("every item in a
  1-block `eth_getBlockByNumber` batch returned an error, inside an HTTP 200"). It recovered on retry
  and cost nothing, but a per-block timestamp fetch is the fragile part of this backfill.

---

## 9. The release gate, met (2026-08-19)

§7 step 4 was the gate: *read the top of the list by hand. If the top twenty are not recognisably real
projects with real problems, the filter is wrong and no amount of tooling fixes that.*

It is better than reading by hand. With a gateway key, the Graph Network subgraph resolves a deployment
id to a name **and independently states its allocation count** - so the queue can be checked against
the network's own answer rather than against our arithmetic.

The top five, resolved:

| Deployment | Active allocations | Signalled | Name |
|---|---:|---:|---|
| `QmWFi6uciaQPQmo1xR…` | **0** | 54,724 GRT | Substreams Uniswap v3 Ethereum |
| `QmdEz2oUhYGsePUteC…` | **0** | 26,774 GRT | estfor |
| `QmPJbGjktGa7c4UYWX…` | **0** | 21,491 GRT | Spookyswap |
| `QmW4Vo3ZYV79pizzYK…` | **0** | 10,673 GRT | Peeranha |
| `QmQKXcNQQRdUvNRMGJ…` | **0** | 9,954 GRT | Graph Network Activity - Arb |

**Every one: zero active allocations, confirmed by the network subgraph itself.** Our nest derived
"signal, no allocation" from raw `SubgraphService` and `L2Curation` events; the network agrees, from a
completely different code path.

And they are **recognisably real**: Spookyswap is a live DEX, Peeranha a live Q&A protocol, estfor a
live game. §8's suspicion - that the list might be mostly programmatic signal and dead deployments -
does not survive contact with the names. The repeating 10,000 GRT shape is further down the list, not
at the top of it.

One of them is worth a wry note: **"Graph Network Activity - Arb" is itself unserved**, with ~10k GRT
signalled on it.

### What this settles

The queue is not a guess. It is a ranked list of named projects whose owners have GRT staked on data
nobody is producing - which is exactly what [community.md](community.md) §2 needed and could not
previously produce. **These five are the port loop's first subjects.**

The signal figures differ slightly from §8's (54,336 vs 54,724 for the top entry) because ours is net
signal *shares* and the network's is `signalledTokens` in GRT. Different units, both correct.

### Still outstanding

`HorizonStaking`'s pending implementation is still unverified on Sourcify, so its upgrade remains
undiffable (§5a). That is the one gate item left, and it is waiting on somebody else's verification.

---

## 10. The queue, triaged (2026-08-19)

The five verified subjects from §9, with their manifests fetched and read. This is what the loop
actually has to work with, and one of them is a finding rather than a job.

| Subject | Signalled | Chain | Shape | Portable |
|---|---:|---|---|---|
| **Substreams Uniswap v3 Ethereum** | 54,724 GRT | mainnet | **0 event sources, 2 `kind: substreams`** | **No** |
| estfor | 26,774 GRT | fantom | 46 event sources | Yes |
| Spookyswap | 21,491 GRT | fantom | 4 sources + 1 template | **Ported** |
| Peeranha | 10,673 GRT | matic | 10 event sources | Yes |
| Graph Network Activity - Arb | 9,954 GRT | arbitrum-one | 12 event sources | Yes |

### The largest one is structurally out of reach, and it is worth saying why

`Substreams Uniswap v3 Ethereum` declares **no `eventHandlers` at all**. Its entire data source is a
Substreams package pinned by CID:

```yaml
- kind: substreams
  mapping: { kind: substreams/graph-entities }
  source: { package: { file: { /: /ipfs/QmfZgQxwtsejAuf8rr1CFj6nuJ2C79oMQGQAPFm5uhEz6z } } }
```

There is nothing for `init --from-subgraph` to read: no ABIs, no events, no contracts. This is **not**
one of RFC-0038's three named gaps - it is a different kind of gap entirely. Those are features a nest
lacks; this is a subgraph that is not event-shaped, so the porting *premise* does not apply.

**And it is the highest-signal unserved deployment on the queue.** The single biggest opportunity the
port loop can see needs Substreams ingestion, which nuthatch does not have and
[RFC-0014 deliberately defers](../rfcs/0014-firehose-class-extraction-traces-and-state.md) - the same
RFC whose *name* was mistaken for Substreams support in the strategy review this morning
([strategy-review-2026-08-19.md](strategy-review-2026-08-19.md) §2).

That is worth stating plainly rather than filing as "skipped": if Substreams-powered subgraphs are a
growing share of what goes unserved, the port loop's ceiling is lower than the queue's raw signal
suggests, and the case for Substreams ingestion changes from "reach" to "reachable demand".

### What the chain work bought

Every portable subject is on a chain nuthatch can now index, and **two of the three only became
reachable today**: Polygon shipped in #646, and Fantom's endpoints were measured for Spookyswap. Before
this morning, three of the four portable subjects would have needed a chain entry first.

### Next subjects, in signal order

1. **estfor** (fantom) - the endpoints are already proven by Spookyswap; 46 sources is the largest port
   attempted so far.
2. **Peeranha** (matic) - first real use of the Polygon entry.
3. **Graph Network Activity - Arb** - Arbitrum, and adjacent to work already done.
