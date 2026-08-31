# #296: compact binary rows, measured

**This document contains no decision.** #296 is a storage-format change, and the sprint brief adds
the constraint that matters: *"Do not trade away the no-resync promise by implication."* That is a
product commitment, so this measures the cost, prices the options, and stops.

Measured 2026-08-31 against the **live Lodestar deployment**, not a fixture.

## The hot store is already over half the budget

`nuthatch_hot_store_bytes`, read live:

| port | nest | hot store |
| ---: | --- | ---: |
| 8095 | graph-staking-nest | **1,084,432,384 B (1.08 GB)** |
| 8096 | graph-gns-nest | **1,140,813,824 B (1.14 GB)** |
| 8098 | horizon | 334,966,784 B (335 MB) |
| 8104 | dips-nest | 48,607,232 B (49 MB) |

The per-cursor budget is **2 GB**, shared across every nest on that cursor. These four are separate
processes, so each is its own cursor today - but two of them already spend **over half their budget
on the hot store alone**, before DuckDB, before serving, before a second nest is mounted beside them.

That is why this issue is worth more than a size saving. CLAUDE.md's non-negotiable 2 makes density
RAM-bounded by design, so this is the difference between "one nest per cursor is comfortable" and
"two nests per cursor is possible".

## What the format costs, on real rows

Rows are stored as JSON strings: `ENTITIES: TableDefinition<&str, &str>` in `store.rs`, written by
`DecodedRow::to_json().to_string()`.

Sampled from two differently-shaped production tables and modelled against a schema-driven binary
encoding - field names dropped (the schema has them), hashes as 32 raw bytes rather than 66-char hex,
addresses as 20 rather than 42, `uint256` as the 32-byte word rather than a decimal string, block
numbers and timestamps as varints, and `_seq` **not stored at all** because it is derived from
`(block << 20) | log_index`:

| table | rows | JSON | compact | ratio | saving |
| --- | ---: | ---: | ---: | ---: | ---: |
| `staking_legacy__stake_delegated` | 2,000 | 651 B/row | 266 B/row | **2.45x** | 59% |
| `staking__tokens_delegated` | 345 | 713 B/row | 287 B/row | **2.49x** | 60% |

Two independent shapes agreeing to within 0.04x. **Field names alone are 30% of the stored bytes in
both** - the schema is re-transmitted once per row.

Applied to the measured hot stores, 1.08 GB would become roughly **440 MB**.

### One saving that is not the format's, and must not be counted as it

Both tables carry `shares`/`shares_dec` and `tokens`/`tokens_dec` holding **identical strings**.
Dropping the duplicate takes `staking__tokens_delegated` from 713 to 223 B/row - a 3.20x ratio - but
that is schema redundancy rather than encoding. It belongs to its own issue and is excluded from
every figure above.

## What is not measured

- **Decode cost on point-read.** #296 names it; this does not measure it. A varint/fixed-width reader
  should beat `serde_json` comfortably, but "should" is not a number, and the `point-read latency`
  gate exists to settle it.
- **redb's own overhead.** The figures above are row payloads. `hot_store_bytes` is the file, which
  includes redb's B-tree, so the realised saving will be **smaller** than 59%.
- **Any prototype.** No encoder exists. The compact column is a model computed from the schema, not a
  measurement of a format.

## The contracts on the table

RFC-0020's promise is that a version upgrade is a binary swap - proven in production, 0.3.0 to 0.6.0
with no data migration. That promise is what this change spends.

| option | cost | what it forecloses |
| --- | --- | --- |
| **Versioned read path** - write v2, read v1 and v2, convert lazily | most work: two decoders live in the tree indefinitely, and every reader - hot path, reorg rollback, `from_stored`, the entity circuit - handles both | nothing. The no-resync promise holds intact |
| **Rebuild on upgrade** - refuse a v1 store, tell the operator to re-index | least work: one guard, one message | **the no-resync promise, explicitly.** `horizon-nest` backfills from block 95,000,000: hours to days of RPC, and real money at the rates in `docs/bench/750-rpc-cost-2026-08-31.md` |
| **Rebuild the hot store only** - sealed Parquet untouched; drop and re-derive the unsealed tail from `sealed_through` | small: the hot store holds only rows past the sealed watermark, which is finality-bounded rather than history-bounded | little. It is a re-index of the *tail*, not of history |
| **Do nothing** | free | leaves two nests at half their cursor budget in hot storage, and makes multi-nest density worse than it needs to be |

**Recommendation, to take or reject: the third.** It gets the saving without spending the promise.
The no-resync commitment is about *history* - the part that costs hours and money - and the hot store
is by construction the finality-bounded tail. Re-deriving it is a bounded, minutes-scale operation
over data the nest has already sealed, and `sealed_through` marks exactly where to resume.

That is still a **narrower promise than "a binary swap changes nothing"**, so it is a promise being
changed and belongs in release notes rather than being discovered. Not free - only much cheaper than
a full resync.

**Before implementing, whichever is chosen:** measure point-read decode cost against a prototype
encoder. The size argument is settled; the latency one is asserted, and this project has published
enough asserted numbers this month.
