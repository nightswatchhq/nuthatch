# RFC-0059: Checkpointed folds - order-dependent views read from a sealed checkpoint and the hot tail

**Status:** **Accepted 2026-09-21 (Chief)**, on one condition that binds every slice: it ships
behind the `folds` cargo feature, **off by default**, and a regular nuthatch user never sees it
unless they opt in (§ Packaging). Tracking #1441. S0 is #1439; S1 to S4 are filed when S0 reports.

> **S0 reported 2026-09-21: continue** (#1439, harness and results in
> nightswatchhq/graph-network-nest#1). The Network nest's saved-clock, epoch and pause chain was run in
> carry-and-window form over the sealed replay.
>
> - **Exactness:** it matched the one-shot views exactly at 24 blocks, and the reference matched the
>   gateway at 123.6M and at head.
> - **Cost at head:** 63.2 s and 10.7 GiB for the one-shot views; 0.14 s for the fold's own statements.
>
> Four measured corrections are folded into §5, §8 and §9 below:
>
> - invariance is checked just after each cut, not only at the end;
> - windows are bounded by event volume;
> - head cost is measured in process;
> - large set carries need pruning.
>
> The recursive delegation ledger remains untested.

**Date:** 2026-09-21

**Author:** Pete (cargopete)

**Depends on:** RFC-0018 §1 (authored views, evaluated per request over hot ∪ sealed), RFC-0041
(authored incremental entities, whose §5.3 names this follow-up), RFC-0047 (the sealed directory and
its catalogue as a contract; C4, the per-cursor analytics budget), RFC-0053 §Time travel (the contract
a block-pinned read must meet).

**Answers:** RFC-0041 §5.3 and its open question 4: *"If a real restart measurement says the seed is
too slow, immutable content-addressed entity checkpoints get a follow-up RFC."* The measurement that
triggers it is not a restart. It is a request (§0).

**Does not schedule:** #357 (whole-derivation reuse across an NID change), frozen for 2027. Its
revisit condition names "durable materialised entity checkpoints". This RFC builds a different kind of
checkpoint, keyed to one fold within one dataset, and does not reopen #357. RFC-0042's fourth trigger
fires only if #357 itself is scheduled, so it does not fire here.

**First consumer:** [RFC-0060](0060-the-network-subgraph-endpoint.md), the Network Subgraph endpoint.

**Nature:** new binary capability. Per RFC-0044 §8 and CLAUDE.md, it is decided by Chief and recorded,
or it does not happen.

## Abstract

An authored view that folds history in order is expensive to evaluate on every request. Such views
include latest-value-by-order, carry-forward clocks, running ledgers and recursive state machines.
The cost of each request is the whole history of the chain. RFC-0041 fixed this for aggregates that
DBSP can maintain under retraction. It refuses order-dependent shapes by design, and that refusal is
correct.

This RFC fixes the problem for folds. A fold declares its **carry**: the state it needs to continue.
The runtime writes the carry at seal boundaries as an immutable, content-addressed **checkpoint**.
Every read at block `n` is `step(checkpoint at C ≤ n, facts in (C, n])`. The same SQL answers at head
and at any retained block. The full-history build is the steady-state step repeated, so no
whole-history query ever runs, even during backfill. Reorgs touch only the tail.

Built-in balances, exposure and velocity already work this way. This RFC gives authored SQL the same
mechanism, with a durable seed that advances with sealing.

## 0. The measurement

The Network Subgraph nest (`nightswatchhq/graph-network-nest`, LEARNINGS.md, 2026-09-21) replayed
Arbitrum One from the protocol's first block to head. The replay produced **10,285** sealed segments.
Its 26 views total 1,694 lines of SQL. 19 of them use window functions, and 3 are recursive
(`05-epoch-schedule`, `41-delegation`, `45-indexer`). Every GraphQL request evaluated those views over
the whole corpus. The results:

- A block-pinned `graphNetwork` query first hit the 1,024 file-descriptor limit.
- Raising the limit to 65,536 moved the failure: the query then exceeded the 512 MiB public-query
  budget of RFC-0047 C4.
- Reader batching and disabling insertion-order retention did not bring it under that budget. The
  final bounded-reader run took **36.1 s**.

Raising the budget was deliberately refused. It would have made the endpoint less safe and still not
fresh enough. The work stopped there. The folds were correct: the same views matched the reference
deployment field for field at five historical checkpoints. What failed was evaluating them per request.

Lodestar had the same problem in #1186: 8 s for the indexers list. RFC-0041 cured Lodestar because its
views were aggregates. The Network Subgraph's views are not.

## 1. Why RFC-0041 does not cover it

The v1 lowerer admits `sum`, `min`, `max`, `avg`, `count` and `count_star` (`src/entities.rs:335`). It
refuses `arg_max`, `first`, `list` and the rest by test (`src/entities.rs:816`). #1313 records three
further limits:

- one entity is one relation;
- a singleton entity has no group key;
- duplicate accumulating aliases are unsupported.

`graphNetwork` is a singleton with id `"1"`. Most of the nest's state is latest-by-order, a running
ledger, or a value captured at the moment a particular handler ran.

This is not a gap waiting to be filled. DBSP maintains a relation under inserts and retractions *in
any order*. An ordered fold's value at block `n` depends on the sequence of events, so retracting an
event from the middle means re-running everything after it. But nuthatch never needs that. Reorgs
retract only from the end, and sealed history never changes. A fold only ever has to **extend forward
from a finalized point**, which is a much smaller problem than general incremental maintenance.

## 2. What already exists

`src/indexer.rs:7327` logs `rebuilt balance view: N holders (cold-seeded net(s) + hot transfer(s)
replayed)`. The built-in views seed from the sealed range and replay the hot tail. RFC-0041 §5.3 does
the same for entities, recomputing its seed from the finalized range once per restart.

Two things are missing:

- **a seed that persists and advances as sealing advances**, so it is never recomputed from history;
- **a way for authored SQL to use it.**

## Goals

1. **Bounded read cost.** Reading a fold costs the hot tail plus the result, not the history.
2. **One SQL for every block.** The same fold SQL answers at head and at any block `n` that has a
   retained checkpoint at or before it. The answer is exact, including block-pinned reads.
3. **Verifiable checkpoints.** Checkpoints are deterministic, content-addressed and re-executable from
   sealed facts. Any one can be verified by recomputing it.
4. **One path for backfill and steady state.** The full-history build is the steady-state step
   repeated. It never needs a whole-history query or whole-history memory.
5. **Reorgs stay in the hot store.** Nothing sealed or checkpointed changes on a reorg.
6. **The 2 GiB budget holds.** Memory stays inside the per-cursor budget, and fold memory is measured
   and admitted rather than assumed.

## Non-goals

- **Not a way to express a cross-keyed fixed point.** RFC-0038 §6a stands. A fold whose step reads
  the previously stored output of arbitrary other keys is not made expressible here. Uniswap's
  `findEthPerToken()` reading another token's stored `derivedETH` is the standing example.
  Checkpoints make an expressible fold cheap. They do not make an inexpressible one expressible, and
  nothing here reopens RFC-0038 §8.
- **Not durable DBSP state.** RFC-0041 entities keep §5.3's per-restart seed. RFC-0042's "DBSP
  checkpoint (rkyv)" row is untouched.
- **Not #357.** A checkpoint belongs to one fold in one dataset. Editing a fold rebuilds that fold's
  checkpoints locally from sealed facts. Reuse across NIDs is not attempted.
- **Not mutable cold state.** Checkpoints are written beside the sealed directory, never into a
  segment, and never changed after they are written. The standing reorg rule is untouched.
- **Not a general versioned entity store.** RFC-0053 S3 (#1267) is not built here. §6 describes how
  much of its contract folded state gets for free.
- **Not a wider query surface.** `/sql` and the GraphQL surface keep every limit they have.

## Packaging: outside the default binary

Chief's condition at acceptance (2026-09-21): this stays outside the core, behind a feature flag, and
no regular nuthatch user sees any of it unless they opt in. The pattern is the one `counter` (RFC-0046
S2) and `postgres-store` already use.

- **A cargo feature, `folds`, off by default.** RFC-0060's `graph` feature enables it. A default build
  contains no fold loading, no checkpoint writer, no `fold` subcommand and no `check --folds`.
- **No `#[cfg]` forks of business logic.** CLAUDE.md's rule for storage backends applies. The seal
  loop exposes a post-seal hook and the analytics binder exposes a relation-binding hook. The feature
  registers implementations of both, and a default build registers none. The `cfg` sits at
  registration, not scattered through the seal path.
- **Refuse, never ignore.** A nest that ships `folds/`, loaded by a binary built without the feature,
  is refused at load with an error that names the feature. Silently skipping the directory would serve
  views that depend on folds as if they had no state. That is a wrong answer, and a wrong answer is
  worse than a refusal.
- **Invisible by default.** Nothing reaches `init` scaffolds, `llms.txt`, the shipped skills or
  operator docs that a default-build user reads. Fold documentation lives with the feature.
- **The deletion test gates every slice.** Build with default features. CLI help, the config `init`
  writes, the on-disk layout and runtime behaviour must be exactly what they would be had this RFC
  never been written. Each slice's acceptance includes it.

## Design

### 3. The fold contract

A fold is one authored SQL statement in `folds/<name>.sql`. Files load in name order, as `views/`
does. Inside a fold, the runtime binds three kinds of name:

| Name | Resolves to |
|---|---|
| `<name>__carry` | this fold's state at the window's lower bound `lo`: the previous checkpoint, or the empty relation at genesis |
| any fact table, or any ordinary view over facts | **only the rows in the window `(lo, hi]`** |
| another fold `<other>` | that fold at `hi`, evaluated first. `<other>__carry` is that fold at `lo` |

The output schema must equal the carry schema. This is checked at load time. The output of a fold *is*
its carry: whatever the fold needs in order to continue must be a column of its output. That is the
one discipline an author has to hold. §8's partition-invariance test enforces it semantically.

**History is not in scope inside a fold.** A fold cannot scan the past, because the past is not
bound. The only view it has of history is through carries. That bounds the cost by construction, in
the same spirit as a zero-capability component being pure by construction. An *as-of* lookup (the
clock at the moment an allocation was created) is therefore always `carry ∪ window` and never a
history scan.

Two illustrative shapes, with invented table names:

```sql
-- folds/deployment_denial.sql - latest value by order
SELECT deployment, denied_at, block_number, log_index
FROM (
    SELECT * FROM deployment_denial__carry
    UNION ALL
    SELECT deployment, since_block, block_number, log_index FROM rewards__denylist_updated
)
QUALIFY row_number() OVER (PARTITION BY deployment ORDER BY block_number DESC, log_index DESC) = 1
```

```sql
-- folds/deployment_signal.sql - running ledger
SELECT deployment, sum(delta) AS signal
FROM (
    SELECT deployment, signal AS delta FROM deployment_signal__carry
    UNION ALL SELECT deployment,  tokens FROM curation__signalled
    UNION ALL SELECT deployment, -tokens FROM curation__burned
)
GROUP BY deployment
```

A recursive fold takes its base case from the carry instead of from the first event.

**Keyed folds.** A fold declares its key: an `id` column, or a constant for a singleton. The contract
is that **an event changes only the keys it names**. For a keyed fold, the runtime evaluates the step
over the carry rows *for the keys the window touches*, plus the window. Untouched rows pass through
unchanged. Per-head work is then proportional to the window, not to the state. §4's snapshots can also
share the checkpoint rather than copying it.

The Graph's mappings load entities by ids taken from the event, so the contract is the natural one.
Aggregates over everything (the network totals) are singletons, keyed by a constant. A fold that
breaks the contract must declare itself unkeyed and pays full evaluation per step. §8's differential
test catches a fold that breaks the contract without saying so.

**Determinism.** A fold may not call a volatile function. `version()`, `random()`, `now()` and their
kind are **refused at load**, where views are only warned about today. Row identity is the key.
Checkpoint equality is multiset equality of rows.

### 4. Reading a fold at block `n`

To read at `n`, take `C` = the latest retained checkpoint with block ≤ `n`, and evaluate the fold with
`lo = C` and `hi = n` over hot ∪ sealed facts. The same path serves every kind of read:

- **At head:** the window is the unsealed hot tail, plus any sealed segments not yet checkpointed.
- **Pinned by number:** resolve `n` and read as above.
- **Pinned by hash:** resolve the hash to a canonical block number first. A hash that is not canonical
  is refused. graph-node errors in the same case.
- **Before the oldest retained checkpoint:** refused by name:
  `block N predates retained history for fold F (oldest checkpoint: M)`. graph-node's pruned
  deployments refuse in the same way.

**Once per head.** At each head advance, the runtime evaluates the declared folds once into an
in-memory snapshot keyed by block hash, and every request at that head reads it. Advances are
coalesced: at most one evaluation is in flight, and the next one starts at whatever the head is when
the previous one finishes. The snapshot's block is what `_meta` reports, so `_meta` never claims a
block the state does not reflect. The request count does not multiply the compute.

The last `R` snapshots are **retained for `T` seconds**. A client that pins its later pages to the
hash returned by its first page (indexer-rs does exactly this, RFC-0060 §2) reads the snapshot it
started from. After `T`, a pinned read falls back to the on-demand path above. That path is exact, but
it costs one window's evaluation and is subject to the ordinary query limits.

For a keyed fold, a snapshot holds only the rows its window changed and refers to the shared
checkpoint for the rest, so retaining `R` snapshots does not cost `R` copies of the state.

### 5. Checkpoints

**When.** When the seal loop advances `sealed_through` from `S` to `S'`, the runtime evaluates each
fold with carry = checkpoint(`S`) and window `(S, S']`, and writes the result as checkpoint(`S'`).
This happens after the seal commits and never inside it. A slow or failed checkpoint build must not
delay sealing. If one lags, reads stay correct and simply see a longer window. The lag is a metric,
`fold_checkpoint_lag_blocks`, and past a threshold it degrades `/ready`.

**Where.** `checkpoints/<fold_hash>/<block>.parquet` beside `segments/`, with its own manifest,
written by atomic rename as the catalogue is. Only the ingestion process writes checkpoints. Queries
read them read-only, as they read segments, so the single-writer rule is unchanged.

**Identity.** `id(S') = H(fold_hash, id(S), content hashes of the catalogue segments covering (S, S'])`,
with `id(genesis) = H(fold_hash, ∅)`. The chain makes checkpoint(`S'`) a content address of the fold
definition and every sealed fact up to `S'`. `fold_hash` covers the fold's SQL, every fold and view it
references transitively, and the schemas of the fact tables it binds. Each checkpoint also records an
order-independent digest of its rows for verification. Identity rests on the inputs and the logical
rows, not on Parquet bytes.

**Rebuild.** A new `fold_hash` has no checkpoints. The runtime walks forward from genesis over the
sealed segments one window at a time. This makes no RPC calls and never evaluates more than one window
at once. It is the same code path backfill uses.

**A window is bounded by event volume, not by block span** (S0). A dense 10M-block window peaked at
591 MiB. The same span split at its midpoint peaked at 417 and 359 MiB, with identical carries. How this interacts with `Manifest::data_identity()`,
so that a fold edit does not force re-ingestion, is confirmed in S1.

**Retention.** Every checkpoint in the most recent `R_seals` seals is kept. Older history keeps one
checkpoint every `retain_every_blocks`, and the rest are pruned. A historical read costs at most one
retention interval of window. The default is an open question with a measurement behind it.

It must be chosen rather than defaulted to "keep all". S0 measured the Network chain's carries at head.

- **Eight of the nine carries total under 40 KB.**
- **The ninth, the set of every legacy allocation id, is 591,071 rows and 23.9 MB.** It exists only to
  filter `HorizonRewardsAssigned` against legacy allocations.

Kept at every seal, that one set alone would be hundreds of GB. So a large set carry is either narrowed
to the members that can still matter (here, the legacy allocations that can still receive rewards) or
stored as deltas against the previous checkpoint.

**Verification.** `nuthatch check --folds` recomputes a checkpoint from its predecessor, which costs
one window. `--from-genesis` walks the whole chain offline.

**Restart.** The runtime loads the latest checkpoint and has nothing to seed. For folds, that settles
the restart question RFC-0041 left open.

### 6. What this gives RFC-0053's time-travel contract

RFC-0053 says time travel needs "the equivalent SCD-2 or bitemporal store plus a block-hash index".
For folded state there is a cheaper route to the same exact answer: state at `n` is `step(C ≤ n, (C, n])`.

Event-shaped entities are already immutable rows carrying their block, so time travel over them is a
filter. Between the two, a nest whose mutable entities are all folds meets `block: {number | hash}`
without a versioned store, at any retained block. Whether that makes #1267 unnecessary for such nests
is for #1267 to decide. This RFC does not decide it.

### 7. The non-negotiables

- **Single binary, embedded mode:** no new service and no new dependency. Checkpoints are Parquet,
  written by the existing writer path.
- **2 GiB per cursor:** a step's memory is bounded by the touched carry rows plus one window. Each
  fold's measured peak is admitted at load against a declared `max_rows`, as RFC-0041 admits entities,
  and the per-head snapshot set is counted against the cursor's analytics budget.
- **Determinism:** folds are SQL over sealed facts with fixed bounds, and volatile functions are
  refused.
- **Reorgs:** checkpoints exist only past finality. A reorg rolls back hot facts, and the next
  evaluation reads the rolled-back tail. Snapshots at orphaned hashes are dropped, and a pinned read to
  one is refused as non-canonical.
- **Licence:** nothing new is consumed.

## 8. Testing

- **Differential, the core test.** For random `C < n` over a real corpus, a one-window evaluation from
  genesis to `n` must equal a walk to `C` followed by `step(C, (C, n])`, compared as multisets. When a
  view is ported, the fold is also diffed against the original one-shot view.
- **Partition invariance, checked just after each cut.** Stepping from genesis in windows of random
  sizes must give the same state as a regular partition. **It must be compared at the first event after
  every cut, not only at the end.**

  S0 learned this the hard way. Folds heal themselves: later events in a window overwrite an early
  mistake. Two broken carries both left the end state identical to the correct run. Evaluated at the
  first refresh event after each of 37 random cuts, one of them showed at 26 cuts. The other showed at
  none, consistent with its carried value being redundant for every served field. That is the other
  thing this check finds.

  The runtime can run a sampled version on itself at checkpoint time, by computing one window as two
  halves and comparing the first events after the split. Whether it should is open question 4.
- **Reorg property.** Random reorg depths within the tail must converge to the one-shot fold over
  canonical facts, and must never modify a checkpoint file.
- **Mutation.** Dropping a column from a carry must turn the post-cut invariance check red. If it
  does not, the carry is either redundant or untested, and the RFC says which. Replacing the window
  binding with the full history must be caught by the scope test (S1). An absence test that stays green
  with the mechanism removed proves nothing, and each gate here is shown to fail first.
- **Crash.** Kill the writer mid-checkpoint. On restart it must serve from the previous checkpoint and
  rebuild the missing one.

## 9. Slices

Each slice's acceptance is written so it can fail.

- **S0 - kill-or-continue, on real data, with no runtime changes.** Take the saved-clock, epoch and
  pause chain of RFC-0060 on the existing ThinkPad corpus. Rewrite it by hand into carry-and-window
  form, and build checkpoints with a script. *Accept when:*
  - it equals the one-shot views at ≥20 blocks, including ≥3 epoch boundaries and the Horizon
    transition;
  - partition invariance holds for ≥3 random partitions;
  - dropping a carry column turns the test red;
  - head evaluation is **≤500 ms p99 and ≤256 MiB peak** (targets, to be confirmed with Chief);
  - no step of the whole-history walk exceeds 512 MiB.

  *Stop* if a fold cannot carry its state, or if head evaluation does not fit those targets. Either
  answer is worth having.

  **Reported 2026-09-21: continue** (#1439, nightswatchhq/graph-network-nest#1).

  | Criterion | Result |
  |---|---|
  | ≥20 blocks | 24 of 24 exact |
  | ≥3 random partitions | Invariance holds on all three, at the end state and at 37 post-cut probe points |
  | Dropping a carry turns the test red | Only once the test was moved to the post-cut probe points |
  | Head evaluation targets | The fold's statements met them (0.135 to 0.146 s). A cold process did not (0.85 to 1.12 s and 252 to 313 MiB, of which 0.64 s and about 275 MiB is an empty window) |
  | No step over 512 MiB | Met at 5M-block windows, not at 10M |

  The recursive delegation ledger was not covered. It needs `nuthatch_mul_div`, which plain DuckDB
  cannot reproduce, so it moves into S1.
- **S1 - folds in the runtime.** Load `folds/`, bind carries and window-scoped facts, check the schema
  and volatility at load, declare keys, and read at `n` on demand from a checkpoint built by
  `nuthatch fold build`. *Accept when:*
  - a fold computing `count(*)` over a fact table returns the window's count, not history's;
  - a volatile fold is refused;
  - a schema mismatch is refused;
  - head evaluation, measured **inside the running process**, meets S0's 500 ms and 256 MiB targets;
  - the recursive delegation ledger passes the S0 differential and post-cut invariance, on nuthatch's
    own connection with its real scalars.
- **S2 - checkpoints from the seal loop.** Identity chain, atomic write, retention, restart and
  `check --folds`. *Accept when:*
  - a mid-write kill recovers;
  - a recomputed identity chain matches;
  - seal latency is unchanged within run-to-run noise.
- **S3 - head snapshots.** Coalesced evaluation, `R`/`T` retention for pinned reads, and touched-key
  deltas. *Accept when:*
  - N concurrent requests at one head cause exactly one evaluation (counter);
  - a pinned read to a retained hash is identical to the head response at that hash;
  - an orphaned hash is refused;
  - the memory of `R` snapshots stays within the declared bound.
- **S4 - serving.** `/sql` and the RFC-0053 GraphQL surface read fold projections, and refusals are
  named. *Accept when* snapshot responses equal on-demand evaluation at the same block.

## Risks

- **Carry size.** The largest carries (every allocation ever made) set checkpoint storage and snapshot
  RAM. S0 measures them. Splitting current from closed state may be needed.
- **Author discipline.** Writing a fold in carry-and-window form is a skill. Partition invariance is
  the guard, and it must be shown to fail on a real mistake, not merely to pass.
- **Recursive CTEs inside a window.** DuckDB's recursive performance over a few thousand ordered events
  is unmeasured. S0 includes the recursive delegation ledger for that reason.
- **Two incremental mechanisms.** A fold can do what an entity does, only more slowly on update. The
  rule has to be stated wherever authors will read it: aggregates whose result does not depend on
  order are entities (RFC-0041), and ordered folds are folds (this RFC).
- **The seal path.** Checkpoint builds sit next to the seal loop and must never sit in it. S2's seal
  latency criterion is there to prove it.

## Alternatives considered

1. **Extend DBSP (RFC-0041) to ordered shapes.** `arg_max` under retraction is feasible. Windows and
   recursion over an ordered sequence are not natural to DBSP, and retracting from the middle is a
   generality nuthatch never uses. That is research with no guaranteed end, done to reach something
   §1 shows is simpler.
2. **Versioned entity rows** (graph-node's `block_range`, maintained at ingestion). This needs an
   imperative per-event state machine and pays graph-node's write amplification on every change.
3. **Periodic full recompute into a table.** The cost grows with history. It is 36 s and over the
   budget today, and it only gets worse.
4. **A per-nest Rust or WASM reducer.** It either breaks the stateless-component contract or needs a
   host-side state protocol, and it is a second implementation of the mapping in another language.
5. **Raise the query budget.** Refused in the nest's own record. It is not a fix.

## Open questions

1. Checkpoint spacing and retention defaults. S0 measures carry size for the network nest.
2. Are checkpoints part of the published directory (RFC-0047, RFC-0052)? The default is no: they are
   derivable, and the contract is the facts.
3. Declaration: a `folds/` directory, as proposed, or `kind = "fold"` in `entities.toml`?
4. Should the runtime self-check partition invariance on a sample of windows at checkpoint time?
5. Does §6 make #1267 unnecessary for nests whose mutable state is all folds?
