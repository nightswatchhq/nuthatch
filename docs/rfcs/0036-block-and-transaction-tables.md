# RFC-0036: Block and transaction tables

**Status:** Draft (2026-08-07). Scopes OBIB cases 3 and 4 (issue #308), which are the last two cases
that are **not** gated on hardware. Depends on 0001 (decode/nests), 0029 (round-trip economics),
and corrects a scoping error in 0014.

## 1. The correction this RFC exists to make

RFC-0014 files **blocks, transactions, traces and state diffs** together as "firehose-class
extraction", blocked on a colocated node. That is true of traces and state diffs, which need `debug_*`
and are deliberately not sourced from public RPC. **It is not true of blocks and transactions.**

`eth_getBlockByNumber` is ordinary RPC, and we already call it: `rpc.rs::fetch_timestamp_batch_once`
issues a batched `eth_getBlockByNumber(block, false)` for every window when `block_timestamps = true`,
reads `timestamp`, and **throws the rest of the header away**. Case 3 is that call, kept.

Filing them with traces made two node-independent cases look node-blocked for a fortnight, and it is
why nuthatch could answer OBIB cases 1, 2 and 6 but not 3 and 4. The bundling was by *shape* (all
"non-event data") when the thing that matters is the *source*.

## 2. What the two cases actually ask for

Measured from the benchmark's own case READMEs rather than inferred:

| Case | Range | Records | Needs |
|---|---|---|---|
| 3 Ethereum block | 0 - 100,000 | **100,001** (one per block) | header fields |
| 4 On transaction | 22,280,000 - 22,290,000 | **1,696,641** | full block bodies |

Case 4's published counts disagree by 218 between implementations, and the benchmark explains why:
Envio and Ponder treat the end block as **exclusive** (1,696,423), Sentio and Subsquid as **inclusive**
(1,696,641). We will be inclusive, matching Sentio/Subsquid, and say so in the artifact - an
unexplained 218-record gap is exactly the kind of thing that reads as a correctness bug.

Envio also published aggregates that make case 4 checkable rather than merely countable: **493,181
unique senders** and **315,861 unique recipients**. Those are the acceptance criteria, not the row
count alone.

## 3. Why this is not simply "another table"

Every table in nuthatch today is an [`EventDecoder`](../../src/registry.rs): it has an `alias`, a
`topic0`, a `signature`, and an `Event` it decodes a `Log` with. A block has no topic0 and no log.

Measured coupling, because the answer decides the shape of the work:

| Module | `topic0` refs | `EventDecoder` refs |
|---|---|---|
| `registry.rs` | 52 | 17 |
| `indexer.rs` | 104 | 0 |
| `analytics.rs` | 4 | 0 |
| `seal.rs` | **0** | **0** |
| `store.rs` | **0** | **0** |
| `schema.rs` | **0** | **0** |

**The storage half is already table-agnostic.** Sealing, the hot store and schema generation operate on
`DecodedRow { table, params, block_number, block_hash, block_timestamp, timestamps, log_index,
tx_hash }` and never ask what produced it. The coupling is entirely at the *decode* boundary, which is
the correct place for it and the only place this RFC has to touch.

So the work is a **second source of `DecodedRow`s**, not a second storage path. That is the load-bearing
finding here: it makes this a slice rather than a refactor, and it is why this RFC is short.

## 4. Design

### 4.1 Config

Two new fields on `[extract]`, joining the existing `traces`/`state`:

```toml
[extract]
blocks = true          # one row per block: header fields. Bounded by construction.
transactions = true    # one row per transaction. UNBOUNDED by construction - see 4.3.
```

They live on `[extract]` rather than a new section because that is where "non-event data" already is,
and because the volume guard and contract scoping already live there.

**The startup refusal must be split.** `indexer.rs` currently bails when `Extract::enabled()`, on the
grounds that no source exists. Once `blocks`/`transactions` are sourceable that refusal is wrong for
them and right for `traces`/`state`. The condition becomes "refuse if `traces || state`", and the
message keeps naming the node for those two only.

Getting this wrong in the other direction is worse than leaving it: a nest that declares `blocks = true`
and silently produces nothing is issue #262 again. **Nothing ships until the rows do** - no config field
that parses and does nothing.

### 4.2 Case 3: the `blocks` table

Reuse `fetch_timestamp_batch_once` rather than adding a second header path. It already has what took
three attempts to get right (RFC-0029 §6h): a batch **narrowing** path rather than a retry-at-same-width
path, and failover across the pool. A parallel fetcher would have to relearn all of it.

The change is to return the header instead of just its `timestamp`, and to have the caller keep the
fields it wants. Columns: `number`, `hash`, `parent_hash`, `timestamp`, `miner`, `gas_used`,
`gas_limit`, `base_fee_per_gas`, `size`, `transaction_count`.

Row identity: `log_index` is meaningless for a block, and `tx_hash` is empty. Both are load-bearing in
the hot store's key, so a block row uses `log_index = 0` and the block's own hash as `tx_hash` - stated
here because a silent convention in the key encoding is how the reserved-column collision (COR-6) got
written.

**Bounded:** exactly one row per block, so no volume guard is needed. Free when
`block_timestamps = true`, because the call is already being made.

### 4.3 Case 4: the `transactions` table

Needs `eth_getBlockByNumber(block, true)` - full bodies, which are one to two orders of magnitude
larger than headers. Columns: `block_number`, `transaction_index`, `hash`, `from`, `to`, `value`, `gas`,
`gas_price`, `max_fee_per_gas`, `nonce`, `input_size`.

**Gas: what we can compute without receipts.** The transaction object carries `gas` (the limit) and
`gas_price`; **`gasUsed` is only in the receipt**. OBIB describes case 4 as "transaction gas usage" and
Envio reports a total "gas value" in wei, which is consistent with either `gas * gas_price` or
`gasUsed * effectiveGasPrice`, and the spec does not say which. v1 stores what the body provides and
records the ambiguity in the artifact rather than picking silently. `eth_getBlockReceipts` is a second
call per block and doubles the round trips; it is a follow-up, gated on the aggregate not matching.

**Unbounded by construction**, exactly like a traces nest: ~170 transactions per block on mainnet, and
the count is set by the chain rather than by the nest. `Extract::scope_check` already refuses an
unscoped extraction nest without `unbounded = true`; `transactions = true` must be inside that guard,
and the pre-backfill estimate should say "≈N rows" loudly the way the RFC-0009 factory estimate does.

### 4.4 The round-trip economics, which is the interesting part

RFC-0029's finding was that **~85% of case 1's wall clock bought `block_timestamp`** - a column that
workload never stored - because the header fan-out is serial *inside* each window.

Case 4 is that problem with the volume turned up: 10,000 blocks of full bodies, and the payload is much
larger than a header. Case 3 is the same shape at header size. So these two cases are not merely two
more rows in the results table - **they are the honest stress test of the fan-out that RFC-0029
diagnosed and did not fix.** Expect the first measurement to be bad and to be informative. Sentio takes
17 minutes on case 4 and Envio 1m26s from a pre-indexed network; the interesting comparison is against
Sentio, which is also going over RPC.

## 5. Slices

| # | Slice | Acceptance |
|---|---|---|
| 1 | Split the `[extract]` refusal so `traces`/`state` still refuse and `blocks`/`transactions` do not. **No config field lands without rows behind it.** | A nest with `traces = true` still refuses, naming the node; the split is covered by a test that fails if either half regresses |
| 2 | `blocks` table from the existing header batch, with the row-identity convention of §4.2 | OBIB case 3 range yields exactly **100,001** rows; `nuthatch sql` reads them; published as `docs/bench/obib-case3.json` |
| 3 | `transactions` table from full bodies, inside `scope_check`, with a loud pre-backfill estimate | OBIB case 4 range yields exactly **1,696,641** rows (inclusive end block) and **493,181** unique senders / **315,861** unique recipients, matching Envio's published aggregates |
| 4 | Round-trip economics: measure, then decide whether receipts and a concurrent header fan-out are worth it | A number, and a decision recorded either way |

## 6. Non-goals

- **Traces and state diffs.** Still RFC-0014, still node-gated. This RFC deliberately does not widen
  its own scope to the two things that actually need a node.
- **A receipts table.** `eth_getBlockReceipts` may be needed for true `gasUsed` (§4.3); a general
  receipts surface is not in scope.
- **Retrofitting existing nests.** Both tables are opt-in. A nest that does not ask keeps today's
  behaviour and today's round trips.

## 7. Status

Draft. Written after reading the code rather than before: §3's coupling table is measured, and it is
what makes this a slice instead of a refactor. Slice 1 is a precondition, not a feature - it should not
ship on its own, because a config key that parses and produces nothing is the defect issue #262 spent a
release being.
