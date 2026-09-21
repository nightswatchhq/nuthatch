# RFC-0060: The Network Subgraph endpoint - serving indexer-agent, indexer-service-rs and tap-agent from a nest

**Status:** **Accepted 2026-09-21 (Chief)**, on one condition that binds every slice: everything
this programme adds ships behind cargo features that are **off by default**, and a regular nuthatch
user sees none of the graph-facing surface unless they opt in (§5.6). Tracking #1442.

The work was started on 2026-09-17 and parked the morning of 2026-09-21 (§4). This RFC is the record
of why it was parked and the plan it resumes on. S0's rescue commit exists (§7).

**Date:** 2026-09-21

**Author:** Pete (cargopete)

**Depends on:** [RFC-0059](0059-checkpointed-folds.md) (checkpointed folds, which removes the reason
it stopped), RFC-0053 (the GraphQL compatibility surface), RFC-0038 §6a and §8 (the order-dependence
boundary and the no-AssemblyScript rule), RFC-0047 C4 (the per-cursor query budget), RFC-0021 (one
cursor per chain).

**Unparks, narrowly:** RFC-0053. Its trigger is "someone with a stopped subgraph whose queries fall
inside the surface, asking for it", and a named consumer, not a coverage figure. This RFC names one: the
indexers whose local Network Subgraph froze on 2026-09-16, and the fixed query set of the three
programs they run (§2). It unparks the surface **for those 21 operations only**. It does not resume
RFC-0053's coverage grind.

**Nature:** new binary capability (RFC-0059, plus consistent reads pinned to a hash), a first-party
nest (`nightswatchhq/graph-network-nest`), and an operated public endpoint.

**Research input:** two research briefs of 2026-09-21, kept verbatim in `nightswatch-misc` under
[`plans/the-graph/research/`](https://github.com/nightswatchhq/nightswatch-misc/tree/main/plans/the-graph/research).
Their claims were checked against source on the same day. Where a brief and this RFC disagree, this
RFC is the record, and §12 lists each correction.

## Abstract

The Graph's indexer software reads a Network Subgraph to decide which allocations exist, which
receipts to accept and which escrow balances back them. On 2026-09-16, a single CRLF in one manifest
failed every version of that subgraph, and the gateway with it for three hours (graph-support #41).
Indexers whose local copy froze then refused valid receipts for every allocation opened after the
freeze (#43).

Since 2026-09-19, `https://network.thenightswatch.dev/graphql` has been a rate-limited proxy to a
grafted graph-node deployment through the gateway. It is useful, and it is not independent: it runs
the same mapping on the same software as the thing that failed.

This RFC specifies a nest-served replacement **for a fixed, known client set**. The clients send 21
GraphQL operations (§2), which touch 9 entity types and about 65 fields. The nest that reproduces
them already exists, and it matched the reference field for field at every historical checkpoint
where it was compared. It stopped because every request re-derived the protocol from genesis. That
took 36.1 s and more than the 512 MiB budget. RFC-0059 makes each read cost the hot tail instead.

The correctness bar is payment safety, not analytics: **zero differences on the eligible-allocation
and escrow-account sets, measured in shadow before any cutover.**

## 1. Why this exists

**The incident.** Handler `handleSubgraphDeploymentManifest` failed on `26956207 ` (with a trailing
CR) at block 505,750,187. Every version of the network and analytics subgraphs stopped (graph-support
#41). One indexer's copy, `QmbYLC…`, froze at 505,750,130.

Counted from SubgraphService events, that indexer had **303** open allocations. The **49** opened
after the freeze were invisible to its copy, so indexer-service-rs refused their receipts, while the
other **254** were served normally. From outside it looks intermittent (#43).

**Why a second implementation, not a second copy.** Another graph-node running the same mapping
would have failed on the same byte at the same block. The value of a nuthatch endpoint is not speed.
It is **uncorrelated failure**: an implementation that shares no code with the mapping or with
graph-node does not fail on the same input.

That is also the honest answer to the question an indexer asked in the thread
(Marc-André, Ellipfra, 2026-09-19): which *subset* of subgraphs nuthatch could serve so that it helps
the network. This one has the highest operational value and the smallest, fixed query set.

**Why payment safety sets the bar.** indexer-service-rs builds its eligible-allocation map, and reads
escrow accounts and signers, directly from this subgraph. The failure modes are asymmetric and both
cost money:

- a missing allocation refuses valid receipts (#43);
- an extra one accepts receipts for an allocation that is not open;
- an over-read escrow balance accepts queries the payer cannot pay for;
- an under-read one refuses paying customers.

A remote `query_url` gets none of the freshness or deployment validation a local source gets
(indexer-rs #1104, open since 2026-09-16). **Nothing on the client side checks this endpoint's
answers. The checking is ours to do.**

## 2. The client contract

The documents are copied verbatim into the nuthatch test fixtures (`tests/fixtures/network-clients/`
on the facade branch, §7 S0) from pinned sources:

- **indexer-rs**, commit `b845e8fe`, `crates/query/graphql`: 6 operations.
- **graphprotocol/indexer**, commit `d05af4be`: 12 operations from
  `indexer-common/src/indexer-management/monitor.ts` (operation 10 is excluded: it queries the Epoch
  Block Oracle, not this subgraph) and 3 from `indexer-common/src/allocations/monitor.ts`.

| Client | Operations | Roots | Pins a block? |
|---|---|---|---|
| indexer-service-rs, tap-agent | `AllocationsQuery`, `ClosedAllocations`, `NetworkEscrowAccountQueryV2`, `SignersByPayerQuery` | `allocations`, `paymentsEscrowAccounts`, `signers`, `_meta` | **Yes.** The first page is unpinned; later pages pin to the hash its `_meta` returned |
| indexer-service-rs, tap-agent | `HorizonDetectionQuery`, `PaymentsEscrowTransactionsRedeemQuery` | `paymentsEscrowAccounts(first: 1)`, `paymentsEscrowTransactions` | No |
| indexer-agent | 15 operations | `allocation`, `allocations`, `provisions`, `epoches`, `subgraphs`, `subgraphDeployments`, `graphNetwork`, `graphNetworks` | **Never** |

**Field closure** (fields read or filtered on):

- **Allocation:** `id status isLegacy indexer.id allocatedTokens createdAt createdAtEpoch
  createdAtBlockHash closedAt closedAtEpoch closedAtBlockHash closedAtBlockNumber poi
  queryFeesCollected subgraphDeployment`
- **SubgraphDeployment:** `id ipfsHash createdAt deniedAt stakedTokens signalledTokens queryFeesAmount
  indexerAllocations.indexer.id`
- **Subgraph:** `id createdAt versionCount versions.{version createdAt subgraphDeployment.id}`
- **Provision:** `id indexer.id dataService.id tokensProvisioned tokensAllocated tokensThawing
  thawingPeriod maxVerifierCut`
- **Epoch:** `id startBlock endBlock signalledTokens stakeDeposited queryFeeRebates totalRewards
  totalIndexerRewards totalDelegatorRewards`
- **GraphNetwork:** `currentEpoch isPaused`. Only two fields.
- **PaymentsEscrowAccount:** `id balance totalAmountThawing payer.id receiver collector payer.signers`
- **Signer:** `id payer thawEndTimestamp isAuthorized`
- **PaymentsEscrowTransaction:** `id type payer receiver allocationId timestamp`
- **Indexer, DataService:** `id` only.

**Query features in use.**

- Filters: `and`, `or`, `id_gt`, `id_in`, `_in`, nested relation filters (`indexer_`, `receiver_`,
  `payer_`), `_gt`, `_gte`, `_lte`, `_not`, `_not: null`, and plain equality.
- Ordering and pagination: `orderBy` on `id` and on `closedAtBlockNumber`, both directions; `first` of
  1000, 5, or a variable.
- Shapes: a nested collection with its own arguments (`payer { signers(first, orderBy, where) }`), a
  derived field (`indexerAllocations`), and singular roots by id.
- A **default page size**. `PaymentsEscrowTransactionsRedeemQuery` passes no `first`, so graph-node's
  default of 100 applies, and the surface must apply exactly the same default.

**Freshness, as the clients judge it:**

- **indexer-agent:** refuses data more than `subgraph-max-block-distance` behind head. The default is
  1,000 blocks, about four minutes on Arbitrum, and the agent logs a warning on Arbitrum suggesting it
  be raised.
- **indexer-rs:** polls every `syncing_interval_secs = 60`. It judges staleness on
  **`_meta.block.timestamp`** (`max_data_staleness_mins = 30`). A response that is not fresher than
  the best it has seen is rejected, and the previous map is kept, which is why a frozen source keeps
  serving a partial set.
- **indexer-rs, other defaults** (`crates/config/default_values.toml`, indexer-rs `796c882`):
  - `recently_closed_allocation_buffer_secs = 3600`;
  - `escrow_min_balance_grt_wei = 0.1 GRT`;
  - `max_signers_per_payer = 0` (no limit).

**What pinning actually requires.** indexer-agent never pins. indexer-rs pins only to a hash **this
endpoint returned seconds earlier**. The requirement is consistency across a paginated sweep, not
general time travel. That is much smaller than what the first attempt built (§4), and RFC-0059 §4's
retained snapshots meet it directly.

## 3. The correctness bar

The **eligible-allocation set** is, for each indexer, the rows `AllocationsQuery` returns: active,
plus closed within the buffer. It must be exact at every served block. So must the escrow-account set
the escrow query returns for every receiver, and the signer sets under each payer. These are compared
**as sets per indexer and per receiver, across every indexer on the network**, not by sampling.

`_meta.block` must be truthful: the number, hash and timestamp of the state actually served. Under
RFC-0059 §4 that is the snapshot's block by construction.

Every field in §2's closure is **exact**. The drop-in definition allows "converged, declared" and
"refused by name". For this client set neither is usable:

- a refused field fails the whole query (RFC-0053, Measured outcome), so no field in the closure may be
  refused;
- a converged `currentEpoch` or `createdAtEpoch` would drive the agent's allocation decisions from a
  number the reference does not hold.

The draft already met this bar at every historical checkpoint where it was compared. Current-head
parity has never been measured (§9).

## 4. Why the first attempt stopped

What was built, 2026-09-17 to 2026-09-21:

- **An inventory.** 18 contracts, with start blocks proved by `eth_getCode` at start minus one and at
  start.
- **15 `[[calls]]` stanzas**, all `canonical = true` (EIP-1898 `blockHash` with `requireCanonical`,
  no fallback to a number).
- **A durable RPC budget ledger** that fails closed, with a US$150 ceiling of 285,000,000 units.
- **26 views** reproducing the mapping's save paths. The most intricate is the *saved* network clock,
  which is written by indexer creation, by delegation-parameter changes, and by mints but not
  transfers, and by POI presentations only when they open an epoch.
- **Field parity at five checkpoints:**
  - 35 network and 13 epoch fields at 42.46M, 43.44M, 84.36M and 123.6M, across 261 epochs;
  - 18 fields for all 39 indexers at 123.6M;
  - deployments at 129M, allocations at 200M and 506M, and signers at 502M.
- **A full replay to head**, fresh to three seconds when it was stopped.

It stopped for four reasons. Each one is a cause, not a symptom:

1. **Every request re-derived the protocol from genesis.** The views are named queries over hot ∪
   sealed (RFC-0018 §1). A pinned `graphNetwork` read over 10,285 segments hit the descriptor limit,
   then the 512 MiB budget, and took 36.1 s in its best bounded run. Raising the budget was refused,
   rightly.
2. **The obvious cure cannot take this patient.** "Maintain state incrementally" (LEARNINGS step 1)
   names the destination but not a road. RFC-0041 v1 admits `sum/min/max/avg/count`, refuses
   `arg_max`, allows one relation per entity, and cannot key a singleton (#1313). 19 of the 26 views
   use window functions and 3 recurse. RFC-0059 §1 has the detail.
3. **The logic hangs together.** The subgraph is one mutable state machine whose handlers write shared
   entities as side effects. Even `currentEpoch`, one of only two `graphNetwork` fields any client
   reads, depends on indexer, delegation, token and POI events. Scoping down to the 21 operations
   trims the edges, not the core.
4. **The deal-breaker was measured last.** Parity came first, at historical checkpoints, and it held.
   The serving cost only became visible once the replay reached head. The plan in §7 inverts this: the
   serving question is its first slice, on data that already exists.

Two further findings from 2026-09-21, recorded here so nobody builds on them:

- **The nuthatch side of the facade is not versioned anywhere.** `rpc_budget.rs`, `graph_history.rs`,
  the `serve.rs` work and the client fixtures (+3,084/-386 lines across 18 files, against `6e5aef0b`)
  exist only as unversioned rsync trees on the ThinkPad, the newest being
  `~/network-facade-build-20260920`. The nest's DEPLOYMENT.md cites `deploy/run-indexer.sh`,
  `deploy/Caddyfile.candidate` and `deploy/network-facade-backfill.service`, none of which is in the
  nest's repository.
- **LEARNINGS says `arg_max` "was added to the incremental entity engine".** It was not.
  `src/entities.rs` is byte-identical to main in every facade tree, and main refuses `arg_max` by test.

## 5. Design

### 5.1 The nest

`graph-network-nest` stays the source of truth for the semantics: the contract inventory, the
canonical calls, and the save-path rules its README and DEPLOYMENT.md record. Its views are ported to
RFC-0059 folds in client-priority order (§7 S3). Each port is diffed against the one-shot view it
replaces before it is diffed against the reference.

The nest reproduces **write order explicitly, for a small global state plus per-key ledgers**. Each
save rule is written down and checked against the upstream mapping at a pinned commit
(`3ca0391`, with PR #335's September changes). This is authored per subgraph, by hand. It is not a
compiler output and it does not generalise (§6).

### 5.2 Evaluation

This follows RFC-0059:

- carries are checkpointed at each seal;
- a per-head snapshot is evaluated once and shared by every caller;
- the last `R` snapshots are retained for `T` seconds;
- everything else is on-demand from a checkpoint.

`T` must cover the longest paginated sweep a client makes. The target is 120 s, to be confirmed from
S4's traffic.

### 5.3 Pinned reads

| Request | Answer |
|---|---|
| unpinned | the current snapshot. `_meta` names its block |
| `block: {hash}` of a retained snapshot | that snapshot, identical to what page one saw |
| `block: {hash}` or `{number}`, canonical, within retained checkpoints | on-demand, exact |
| a hash not on the canonical chain | error, as graph-node does |
| older than the oldest retained checkpoint | named refusal |

### 5.4 The GraphQL surface

RFC-0053's compiler, S1 and S2, supplies the filter matrix, ordering, pagination and `_meta`. S1 of
this RFC checks that all 21 documents compile on it and lists whatever is missing. Any root or field
outside §2's closure keeps RFC-0053's behaviour: answered if the surface can answer it exactly,
otherwise refused by name.

`_meta.deployment` is open question 1. `hasIndexingErrors` is `false` unless the cursor is quarantined
(RFC-0026).

When the state cannot be computed, the endpoint refuses rather than serving something stale. When it
is merely behind, it serves with a truthful `_meta` and lets each client apply its own staleness rule.

### 5.5 Operations

**Rate limits: what the 429s were.** From the route's opening on 2026-09-19 to 2026-09-21, Caddy
logged **5,472** rate-limit rejections:

- **all from one IP address**;
- all between 13:00 and 15:00 UTC on 2026-09-19;
- **451 of them tripped the global zone** (12,000 per 10 s), which means one address sent more than
  1,200 requests a second;
- none since.

The per-IP limit is already 1,200 per 10 s, about 120 a second sustained. One stack starting cold fans
out into tens of paginated requests, not thousands a second. The shape is a retry loop without backoff
after the first refusal, not a bucket too small for a cold start.

So:

- make sure `429` carries `Retry-After` (to verify on the current Caddy module);
- measure one real stack's cold-start fan-out before resizing anything (open question 4);
- keep the global zone, because it protects the box.

With RFC-0059 snapshots, a request costs a filter over a small relation, so the limits protect the
host, not the computation.

**Hosting.** Chief chose on 2026-09-17: indexing on the ThinkPad, public HTTPS on the Helsinki VPS's
Caddy, over Tailscale. A laptop is a single point of failure for a payment-gating endpoint. Caddy
therefore keeps the gateway route as a **health-checked fallback upstream**, marked by a response
header naming the source. An endpoint that silently changes its source mid-sweep is open question 5.

**HA.** Two front-ends over one store are not available in embedded mode: a query-FE process holds
redb's exclusive lock and cannot run beside the writer or another server. This is out of v1 scope;
RFC-0022's pool is the later route.

**What we tell indexers.** This endpoint is **never the only source**. Keep a local deployment or a
gateway endpoint as the secondary, because the client will not detect a bad remote source itself
(indexer-rs #1104). Publish the rate-limit policy and the expected polling intervals.

**Status.** `GET /status` reports the head, the snapshot block, the lag, the source (native or
gateway) and the oldest retained checkpoint.

### 5.6 Packaging: none of this is in the default binary

Chief's condition at acceptance (2026-09-21): this stays outside the core, behind feature flags, and
no regular nuthatch user sees any graph-facing surface unless they opt in.

- **A cargo feature, `graph`, off by default. It enables RFC-0059's `folds`.** It carries:
  - the hash-pinned history and `_meta` history (`graph_history.rs`);
  - snapshot retention for pinned GraphQL reads;
  - whatever of the rescued `serve.rs` and `graph_query.rs` work is graph-facing.
- **The rescue also carries two tools that are not graph-facing:** the durable RPC budget ledger
  (`rpc_budget.rs`) and canonical call pinning (`canonical = true`, which needs config schema v3).
  They arrived with this programme and they change config surface, so they sit behind `graph` too for
  now. Promoting either into the default build is a separate decision, with its own evidence.
- **No `#[cfg]` forks of business logic.** Gates sit at registration points (routes, hooks,
  config-schema admission), as RFC-0059's packaging section describes.
- **Refuse, never ignore.** A nest that needs the feature (`graph/history.toml`, `canonical = true`,
  schema v3), loaded by a default build, is refused at load with an error naming the feature.
- **Invisible by default.** Nothing reaches `init` scaffolds, `llms.txt`, the shipped skills or the
  operator docs a default-build user reads. The endpoint is an operator deployment built with
  `--features graph`, exactly as the x402 counter is built with `--features counter`.
- **The deletion test gates every slice.** A default build's CLI help, the config `init` writes, its
  on-disk layout and its behaviour are exactly what they were before this programme began.

**Unresolved:** whether the condition reaches back to the RFC-0053 surface already on main, which is
in the default build today. That covers the `/graphql` routes, `port-emit`, `graph-validate` and
`init --from-subgraph`. The feature is named `graph` so that it can take them if Chief says so (#1440).

## 6. What this does and does not claim

- **The drop-in definition can be met here, and only here.** RFC-0053's Measured outcome showed it
  cannot be met for an analytics subgraph, whose clients' queries name fields the surface refuses.
  This client set is fixed, and every field in its closure is built. The claim is limited to these 21
  operations, measured.
- **The wording stands.** CLAUDE.md says of RFC-0053: *"Do not describe this as a drop-in replacement
  anywhere."* Chief accepted this RFC on 2026-09-21 without amending that sentence. The endpoint is
  described as serving the fixed query set of indexer-agent, indexer-service-rs and tap-agent, with the
  evidence attached, and never as a general Network Subgraph replacement.
- **RFC-0038 §6a is untouched.** The network subgraph's order dependence is a small global clock plus
  per-key ledgers, each an explicit fold with its save rules written out. Uniswap's pricing is a
  cross-keyed fixed point, and nothing here makes that expressible. No AssemblyScript runs, per §8.
- **No LLM output and no query shape feeds stored state.** Non-negotiable 4 holds.

## 7. Plan

Every slice's acceptance can fail. S1 is where the whole approach can be stopped cheaply.

- **S0 - put it under version control, then behind the feature.**
  - **Done 2026-09-21: the rescue commit.** Branch `pete/network-facade-rescue`, commit `9681c891`,
    on `6e5aef0b`. It holds the ThinkPad's `network-facade-build-20260920` tree as it ran, unmodified:
    the newest of the three copies for every differing file, and the source of the replay binary. That
    is 22 modified files plus `analytics_scalars.rs`, `graph_history.rs`, `rpc_budget.rs`, the network
    tests and fixtures, and `examples/network/`. The missing `deploy/` files were in
    `examples/network/deploy/` all along. The tree was scanned for credentials before committing, and
    it reads them only from the operator directory and the environment.
  - Rebase onto main and split into reviewable PRs (#1438), each gated per §5.6: the budget guard, canonical
    calls, graph history and `_meta`, and serving.
  - Move `examples/network/deploy/` into `graph-network-nest`, and correct LEARNINGS' `arg_max` claim.

  *Accept when:*
  - the binary that ran the replay can be rebuilt from commits on GitHub;
  - its network fixtures pass from a clean clone with `--features graph`;
  - a default build passes the deletion test.
- **S1 - the kill-or-continue slice.** RFC-0059 S0 on this nest's saved-clock, epoch and pause chain,
  on the ThinkPad corpus. In parallel, compile all 21 documents against the RFC-0053 surface and list
  what is missing. *Stop* if RFC-0059 S0 fails. The gateway proxy remains, and this RFC records why.
- **S2 - the runtime.** RFC-0059 S1 to S3.
- **S3 - port the folds, in the order a failure costs money.** Each lands with its differential test:
  1. allocations, the saved clock and epochs (eligibility, and the agent's lifecycle);
  2. escrow accounts, signers and escrow transactions (receipts and RAV redemption);
  3. deployment financials and denial;
  4. provisions;
  5. epoch totals;
  6. `graphNetwork.isPaused`;
  7. subgraphs and versions.

  Fields outside §2's closure are not built.
- **S4 - acceptance on a private listener.** *Accept when:*
  - The migration validator (#1264) runs all 21 documents against the reference deployment at
    identical blocks: at head and at ≥10 historical blocks, with **zero differences**.
  - Indexer-rs-style pagination pinned to a hash stays consistent across a head change.
  - Real indexer-agent, indexer-service-rs and tap-agent run against the listener without errors.
  - Head lag p99 is **<1,000 blocks** and `_meta.block.timestamp` age p99 is **<60 s** (targets).
  - RSS stays within 2 GiB while the polling of N stacks is replayed.
- **S5 - shadow, then cutover.** Every public request is also evaluated natively and diffed.
  - After **7 consecutive days with zero differences** on the eligible-allocation and escrow sets,
    Caddy's primary upstream flips to the nest, with the gateway as the health-checked fallback.
  - After **30 days**, the gateway route is retired from primary duty.

  The shadow diff keeps running after cutover. It is the monitor, not a gate we pass once.

## 8. Not in scope

- **Full schema parity:** curators, delegators, name signal, disputes, the other 33 `graphNetwork`
  fields and the analytics aggregates. Built only if a named consumer asks.
- **IPFS metadata and ENS names**, which no field in the closure reads.
- **POI, allocation on the network, query attestations.** This endpoint is off-network and free.
- **A Graph Horizon data service.** CLAUDE.md defers it explicitly. NW-RFC-002 discusses it.
- **HA across hosts.**
- **AssemblyScript,** in any path.

## 9. Risks

- **A payment-gating dependency on one operator.** The "never the only source" guidance is
  load-bearing, not boilerplate.
- **Current-head parity has never been measured.** The nest's own record says "other conditional save
  paths still need their own validation". Expect S4 to find some.
- **Upstream moves.** PR #335 was unmerged when checked, and new contracts arrive (the
  IssuanceAllocator did). A mapping change the nest misses is silent divergence. The continuous shadow
  diff is the guard. When the reference itself breaks, as it did on the 16th, the chain decides, not
  the reference.
- **Maintenance.** A hand-authored fold set per upstream release is a standing cost. It should be
  counted before acceptance, not discovered after.
- **The ThinkPad.** It was unreachable for part of 2026-09-17 and 2026-09-18.

## 10. Alternatives considered

- **Keep the gateway proxy.** It is the right fallback. As the only path, its failures are correlated
  with the thing it replaces: the CRLF took the gateway down for three hours. It also depends on an API
  key and on the gateway's availability.
- **Run our own graph-node with the Network Subgraph.** This is the conventional answer and the
  cheapest route to *a* second endpoint. It shares the mapping and the software, so it shares the
  failure. The fresh rebuild `QmatH4…` was still at block 249M on 2026-09-17. Worth doing as well,
  perhaps, but it does not deliver independence.
- **Full schema first** (the brief's M2 before cutover). The client set does not need it.
- **Versioned entity rows** for time travel (the brief's design). RFC-0059 §6 covers folded state
  without a versioned store, and §2 shows the clients need far less than general time travel.

## 11. Open questions

1. **`_meta.deployment`.** The nest's NID (a real content address), a documented sentinel, or a real
   IPFS manifest CID (brief question 2)? indexer-rs with `deployment_id` unset does not check it. The
   leaning is the NID.
2. **The L1 clock without `eth_call`.** Can the Arbitrum header's `l1BlockNumber` replace the 15
   hash-pinned `EpochManager.blockNum()` reads? The draft found the L1-number versus L2-hash
   distinction the hard way. This is a hypothesis, unverified, and would take archive RPC off the tip
   path.
3. **`R` and `T`** for snapshot retention, from S4's measured sweeps.
4. **Rate-limit sizing** from one stack's measured cold-start fan-out, and how many stacks share an
   egress IP (brief question 5).
5. **Fallback mid-sweep.** If Caddy fails over between page one and page two, the hash is pinned on a
   different source. Both are exact at that hash if both are correct. Is that acceptable, or should
   failover be sticky per client?
6. **Upstream placeholder fields** (`NOT IMPLEMENTED`, brief question 4). None is in §2's closure, so
   the question is moot for v1. If the schema widens, the drop-in rule says to reproduce them exactly.
7. **Recognition** (brief question 6). This is NW-RFC-002's question, not this RFC's.

**Answered already:** the genesis start block (brief question 3). DEPLOYMENT.md's table proves each
contract's start by `eth_getCode`, and Controller uses upstream's conservative 42,440,000.

## 12. What the research brief got wrong, checked 2026-09-21

- **"The exact indexer-query (Rust) GraphQL text was not obtainable" (brief question 1).** It is
  obtained, verbatim and pinned (§2). It answers the question: `id_gt` cursoring, a `_meta` on the
  first page, and later pages pinned to its hash.
- **The consumer set was listed as `indexer(id){allocations}` with `signalAmount`, plus
  "graph-network parameters".** That shape is indexer #294's, from an older agent. The pinned source
  uses `allocations(where: {indexer, status, id_gt})` and reads only `currentEpoch` and `isPaused`
  from GraphNetwork.
- **"Epoch numbers can be derived from EpochManager state without live calls."** Unverified. The
  draft needed hash-pinned `blockNum()` reads. The header route is open question 2.
- **"indexer-service defaults its allocation syncing to 120,000 ms"** (second brief). indexer-rs
  says `syncing_interval_secs = 60`.
- **The 429s were attributed to buckets sized below a cold start.** §5.5 has the measurement: one
  address, above 1,200 requests a second.
- **"≥2 stateless front-ends over a shared store."** Not available in embedded mode (§5.5).
- **These held:** indexer-rs's defaults and its keep-the-last-map behaviour; indexer-rs #1104;
  indexer-agent's 1,000-block default; #43's figures; and the payment-safety framing, which this RFC
  adopts as its bar. The 7-day and 30-day shadow windows are adopted from the brief.
