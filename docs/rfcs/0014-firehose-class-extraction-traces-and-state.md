# RFC-0014: Firehose-class extraction - traces and state diffs via ExEx

- Status: **Slice 0 implemented (2026-07-29); extraction still deferred on RFC-0003**
- Author: Pete (cargopete)
- Date: 2026-07-17
- Depends on: RFC-0003 (the ExEx execution-time source this reads from - the hard
  prerequisite), RFC-0001 (the decode registry, extended here for calldata + state),
  RFC-0004 (own-node re-execution for the backfill path)
- Blocks: full data-coverage parity with Firehose/Substreams-class products (Amp); the
  "nothing they do we can't do" claim (2026-07-17 GraphOps convo).
- Priority: **deferred.** Gated on RFC-0003 actually landing (ExEx wired to a real
  node). Not before the pilot, not before ExEx is live. This RFC exists now for one
  concrete reason - to make RFC-0003 get *built* forward-compatibly (see its §6 add-on)
  so this is additive later, not a rewrite. Building it comes after.
- Nature: capability/design RFC; the plan is a sketch, not a build order.

## Abstract

Index the two data classes a Firehose/Amp-class product exposes that `eth_getLogs`
structurally cannot: **state changes** (storage-slot writes, balance / nonce / code
changes) and **call traces** (the internal call tree with decoded calldata). Both are
outputs of block execution, and both are *deterministic re-execution artifacts* - which
is Nuthatch's founding thesis, not a bolt-on. The extraction surface is new; everything
downstream (hot store, Parquet sealing, SQL, IVM views, reorg-safety) is row-agnostic
and inherited for free.

## Motivation

The competitive edge of Firehose/Amp is the *rich* data, not the events - traces and
state diffs power things (internal transfers, storage-level accounting, MEV analysis,
proxy/impl introspection) that event logs can't express. RFC-0003 gives us the
substrate: in-process execution output from a colocated reth, with no third party. This
RFC closes the coverage gap and turns "we could do that" into a design on record. The
philosophical fit is the flex - Firehose *is* deterministic execution-time extraction,
and that is already what Nuthatch promises for events.

## Goals

1. **State diffs as first-class rows.** A `state_diffs` surface: `(address, slot,
   prev, new, block_number, tx_index, log_index?)`, plus balance/nonce/code changes.
   Cheap - they fall straight out of the ExEx `ExecutionOutcome` / `BundleState` reth
   already computed.
2. **Call traces as first-class rows.** A `traces` surface: `(block_number, tx_hash,
   trace_address, from, to, value, call_type, gas, gas_used, input, output, error)`,
   with **decoded calldata** (function selector → args) reusing the same alloy ABI
   machinery event decode uses - the calldata mirror of topic0-keyed event decode.
3. **Same determinism guarantee.** Content-addressed segments; the ExEx path and an
   own-node re-execution backfill produce byte-identical rows (the RFC-0003 /
   RFC-0004 discipline, extended to traces/state).
4. **Both regimes.** Tip (from the ExEx notification) and backfill (own-node
   re-execution, RFC-0004). RPC mode is explicitly excluded (§Non-goals) - stated
   honestly, not hidden.

## Non-goals

- **RPC-mode firehose data.** Public RPC `debug_trace*` / `debug_storageRangeAt` are
  expensive, rate-limited, and frequently absent. This capability is **own-node / ExEx
  only**; the RPC embedded path stays log-centric. Say so plainly.
- **Full archive state.** We capture *diffs* and *traces* as rows, not full historical
  state at every block. That's an archive node's job.
- **A Firehose wire protocol / gRPC Firehose server.** Our interchange is Arrow +
  content-addressed Parquet. If Firehose *protocol* compatibility is ever wanted, it's a
  separate export shim, not core.
- **Substreams-runtime compatibility.** Different topic; the WASM transform layer is our
  answer to programmable extraction, not a Substreams clone.
- **ABI-aware storage-layout decode** (decoding mappings/structs from slots) in v1 - raw
  slots first; layout-aware decode is a later increment (§Risks).

## Design

### 1. Extraction source - mostly already in hand

The ExEx `ChainCommitted` notification already carries the full `ExecutionOutcome`
(`BundleState`), so **state diffs require no extra execution** - they're a projection of
data reth already handed us (this is exactly why RFC-0003 must pass the whole
notification through, not a logs-only view - RFC-0003 §6). **Traces** need a
tracing-inspector re-execution pass over the block (a revm inspector), so they cost more
and are opt-in per nest.

### 2. Decode model - two new row producers beside event decode

Extend the registry with producers that sit alongside topic0-keyed event decode:

- **State-diff rows:** no ABI needed for raw slots - `(address, slot, prev, new)` keyed
  by block/tx. Emitted for addresses the nest scopes (all, or a contract set).
- **Trace rows:** calldata decoded via the ABI, keyed by the 4-byte **function
  selector** (the calldata analogue of topic0). Undecoded calls still get a raw row
  (selector + raw input), same contract-ABI-priority-then-generic-fallback rule as
  events.

Config is opt-in and scoped (volume, §3):

```toml
[extract]
traces = true            # per-nest; default false
state  = true            # per-nest; default false
# optional: restrict to a contract set / selector allowlist to bound volume
```

### 3. Volume and footprint - the real risk, named up front

Traces and state diffs are **high volume**: every internal call, every `SSTORE`. This is
where the row-count estimate and the ≤2 GB budget bite hardest. Therefore: **opt-in per
nest, scoping per-contract/selector is first-class, and the pre-backfill estimate must
loudly flag a traces/state nest as unbounded-by-construction** (the RFC-0009 estimate
already exists; extend it). An un-scoped `traces = true` on a busy chain is a foot-gun
and must warn like one.

### 4. Downstream - free

Trace and state rows are ordinary rows: they seal to Parquet past finality, gain
per-table SQL views, feed IVM/derived views, and roll back reorg-safely - no new
plumbing. The entire cost is §1-§3 (extraction, decode, volume management).

### 5. Sequencing within this RFC

State diffs **first** (cheap, straight from the bundle, no inspector). Traces **second**
(re-execution pass, dearer, more decode surface). Ship the cheap correct half before the
expensive half.

## Slice 0 - what shipped without a node (2026-07-29)

The RFC's own §Priority said this exists to keep RFC-0003 forward-compatible. In the course
of checking that, it became clear a definable slice needed no node at all - and that it was
the slice carrying most of the *correctness* risk. It is now built and tested.

**What is in.** Calldata decode keyed by 4-byte function selector (`src/calldata.rs`), the
`[extract]` config block with contract and selector scoping (`src/config.rs`), the
`traces`/`state_diffs`/`calls_raw` table schemas, and the volume guard of §3. Twenty-one
tests, of which the two guards were mutation-checked: breaking each one turns a test red.

**What is not.** Extraction. There is still no source, and a nest declaring `[extract]` is
**refused at startup** rather than started. That is deliberate and worth stating plainly: the
alternative is `traces` and `state_diffs` existing, answering queries, and returning nothing -
and an empty table is indistinguishable from "no matching rows" to whoever is querying it.
Being told the source is missing beats being quietly given zero. The refusal validates the
whole config on the way past, so a typo'd alias or malformed selector surfaces now rather
than on the day a node finally appears.

Three decisions worth recording, because they are not obvious from the design above:

1. **A decode miss produces a row.** §2 said undecoded calls get a raw row; implementing it
   clarified *why* the rule differs from events. An unrecognised topic0 belongs to some other
   contract and is not our business. An unrecognised selector *on a contract we index* is our
   business and is information - it usually means the ABI predates an implementation upgrade.
   Those land in `calls_raw` with selector and raw input, so the gap is visible rather than
   absent. A selector match with undecodable arguments falls back the same way, because a
   4-byte selector is cheap to collide with and one odd transaction must not stall a block.

2. **Call tables are `{alias}__call_{fn}`, not `{alias}__{fn}`.** A contract may have both a
   `Transfer` event and a `transfer` function. `usdc__transfer` has meant the event since
   0.1.0 and must keep meaning it, so the new surface takes the qualified name rather than
   renaming a table every existing query depends on.

3. **The guard refuses, it does not warn.** Unscoped extraction is unbounded by *chain
   traffic* rather than by anything the operator wrote, which is a different thing from a
   large nest. `unbounded = true` is the escape hatch and has to be typed by a human. This
   follows RFC-0012's house rule: a budget stops being a budget the moment something may
   quietly exceed it.

### The trap left for the extraction slice

**The hot store has one key namespace.** `store::entity_key(block, log_index)` keys every row
by `(block, index)` across all tables, so call ordinal 5 and log index 5 in the same block
would collide - silently, and only for blocks that have both. Wiring extraction therefore
requires giving calls their own key namespace *first*.

It is recorded here rather than half-solved in slice 0 because the right answer depends on
the ordering the ExEx notification actually supplies, which nobody can see yet. Guessing now
would mean either a scheme that does not match the source, or a `entity_key` variant that no
test can reach. `CallContext::call_index` carries the same warning at the call site.

## Implementation plan (when unblocked by RFC-0003)

1. State-diff extraction from the ExEx `BundleState`; `state_diffs` table; determinism
   test vs `debug_storageRangeAt` at a pinned block.
2. Own-node backfill parity: re-execution produces byte-identical state-diff segments to
   the ExEx tip path (content hashes match).
3. Trace extraction via a revm tracing inspector; `traces` table; calldata decode reuse.
4. Volume controls: per-nest opt-in, contract/selector scoping, estimate integration +
   the loud warning.
5. Trace determinism/parity vs `debug_traceBlock` at a pinned block; published volume +
   RSS numbers (the honesty rule).

## Testing and acceptance

- Determinism: ExEx vs own-node re-execution → byte-identical trace and state segments.
- Parity: spot-check state diffs vs `debug_storageRangeAt`, traces vs `debug_traceBlock`
  at pinned blocks.
- Reorg: trace/state rows roll back with their block range (they're block-keyed rows -
  the existing rollback covers them; assert it).
- Volume: published row-count + RSS for a scoped traces nest; the estimate's warning
  fires for an un-scoped one.

## Risks

- **Volume / footprint (the big one).** Traces + state can dwarf event data. Mitigation:
  opt-in, scoping, loud estimate. Without discipline this blows the budget - the RFC
  treats that as a first-class constraint, not a footnote.
- **Trace re-execution cost** at the tip (inspector pass per block). Mitigation: traces
  opt-in; state-diffs (the cheap half) usable alone.
- **reth inspector / ExEx API churn.** Same containment as RFC-0003 - confined to
  `nuthatch-node`, pinned, CI-gated.
- **Storage-layout decode complexity.** Raw slots are easy; decoding mappings/structs
  needs layout metadata. Deferred - raw slots ship first, layout-aware decode later.

## Alternatives considered

- **Consume an external Firehose** (StreamingFast/Amp feed). Rejected: reintroduces a
  third party and a serialization boundary - against the sovereignty thesis and RFC-0003
  §Alternatives.
- **RPC `debug_` traces/storage.** Rejected: expensive, rate-limited, often unavailable,
  and not sovereign. Own-node/ExEx only.
- **Fold into RFC-0003.** Rejected: this is a distinct capability (new data model, new
  decode, applies to backfill re-execution too, not just tip). RFC-0003 gets only the §6
  forward-compat constraint; the feature lives here.

## Open questions

1. `state_diffs` as one wide table vs per-contract storage tables? Start with one wide
   table; per-contract views over it if demanded.
2. Do traces need a per-nest selector allowlist (not just contract scoping) to bound
   volume on very busy contracts? Likely yes for anything DEX-router-adjacent.
3. Raw-slot vs ABI-aware storage decode - v1 raw; when does layout-aware decode earn its
   complexity? Defer until a nest actually needs mapping/struct decode.
4. **Row keyspace for calls** - see the trap above. Answer it when the ExEx ordering is
   visible, not before.
5. **Top-level calldata over plain RPC?** `eth_getBlockByNumber(full=true)` returns every
   transaction's `input` without any `debug_*` method, so the calldata decoder built in slice
   0 could produce real rows today for *top-level* calls - no node, no archive, no
   rate-limited tracing endpoint. That is strictly less than this RFC promises (it misses
   every internal call, which is most of the interesting ones) and it would mean two
   differently-sourced paths feeding one table, which is exactly the sort of thing that
   produces "why does this row exist for block X but not block Y". Deliberately **not** taken
   in slice 0. If it is ever wanted it needs its own table and its own honest name, not a
   quiet second source for `traces`.
