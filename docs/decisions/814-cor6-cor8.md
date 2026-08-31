# #814: COR-6 and COR-8, measured

**This document contains no decision.** #814 asks for two: a reserved-column rule, and the honest
treatment of values beyond `i128`. Both are schema/product commitments rather than refactors, so this
establishes what the code does today and what each option forecloses, and stops there.

The issue warned that *"entries here have previously outlived their fix"*. Both were re-checked
against `main` on 2026-08-31 and both are live. The COR-6 behaviour is demonstrated by a test
committed alongside this document, so the decision is taken against behaviour rather than a
description of it.

## COR-6: an event parameter may be named like an implicit column

`implicit_columns()` (`decode/src/registry.rs`) gives every table seven columns before the event's
own parameters: `block_number`, `block_hash`, `block_timestamp`, `tx_hash`, `log_index`, `address`,
`_seq`. An ABI is free to name a parameter any of those, and **nothing refuses it**.

### What happens today, measured

`cor6_an_event_param_named_block_number_shadows_the_real_one` pins three facts:

1. **The parameter wins in the data.** `DecodedRow::to_json` inserts the implicit columns first and
   then loops over params; `serde_json::Map::insert` *replaces*. A row whose true block is 4,000,000
   serialises with `"block_number": "7"` if the event carries a `block_number` parameter of 7. The
   chain's block number is not in the row at all.
2. **`_seq` still encodes the true block**, because it is computed before the overwrite. So a single
   row carries both numbers, in different columns, with nothing saying which is which.
3. **The schema advertises the name twice.** `implicit_columns()` is extended with the parameters and
   no check runs, so `/tables`, `schema.json`, the MCP schema tool and `llms.txt` all publish a table
   with two `block_number` columns.

### Why it is not merely cosmetic

`DecodedRow::from_stored` reads `block_number` back out of the stored JSON, and its own doc comment
says *"The reorg path is the one that cannot afford a second opinion"* - a retraction built from a
different value than the insertion does not cancel it in DBSP, it lands beside it and stays forever.
A shadowed block number is therefore a candidate for exactly that.

**Not verified:** whether a real reorg on such a table actually mis-cancels. That needs a rollback
test over a colliding schema and is the first thing to do if the decision is anything other than
"refuse it".

### The options

| option | cost | what it forecloses |
| --- | --- | --- |
| **Refuse at build time** - `registry::build` errors when a parameter name collides | one check, one error message; a nest with such an ABI cannot be indexed at all | indexing any contract whose ABI happens to use these names. That is a real contract someone cannot index, and the error must say how to proceed |
| **Namespace the implicit columns** (`_block_number`, or a `nuthatch_` prefix) | a breaking schema change: every table's columns change name, every authored view and `entities.toml` referencing them breaks, every sealed segment's bytes change | nothing technically, but it is an RFC-0029 §6b-class migration - the same class as `block_timestamps`, which needs a full re-index |
| **Namespace the colliding *parameter*** (`block_number` → `block_number_1`) | localised to the rare table; no change to any existing nest | quiet renaming of a user-visible column; the name in the ABI is not the name in the table, which has to be documented and discoverable |
| **Do nothing, document it** | free | leaves a silent wrong answer in the data path, and the `_seq` disagreement above |

**Recommendation, for the board to take or reject:** *refuse at build time*, because it is the only
option that cannot produce a wrong number, and because the collision is rare enough that the cost
falls on almost nobody. The error should name the colliding parameter and point at the third option
as the escape hatch if someone genuinely needs that contract. What should not happen is the fourth:
the current behaviour is not "rare and harmless", it is "rare and silently wrong".

## COR-8: a value beyond `i128` is dropped, and the drop is invisible

### What happens today, measured

Already well-covered. `an_over_i128_value_is_dropped_identically_by_the_cold_fold_and_the_hot_replay`
pins that both paths drop the **whole transfer** - the cold fold via `TRY_CAST(… AS HUGEINT)` yielding
NULL, the hot replay via `str::parse::<i128>()` erroring - and that they agree, which the test itself
notes is "a coincidence of intent, not of code".

Dropping both legs is right: dropping one would invent value, leaving a sender debited and no
recipient credited.

### The gap

**The drop sets no signal.** `analytics.rs` already has the channel - `degraded_tables`, with a
`degraded()` summary "for surfaces with room to say only yes or no", threaded through `/sql`
provenance - and the i128 skip does not touch it. A balance that silently excludes a transfer is
served with `degraded: false` beside it.

So the arithmetic is honest and the *reporting* is not. A consumer cannot tell a complete balance
from one missing an unrepresentable transfer.

### The options

| option | cost | what it forecloses |
| --- | --- | --- |
| **Count and surface the drops** - a counter per query, folded into `degraded_tables`/provenance, and a `nuthatch_rows_dropped_total{reason="over_i128"}` metric | small; the channel exists | nothing. The balance stays what it is; the caller learns it is incomplete |
| **Refuse the query** when any row overflows | a correct-looking balance becomes an error | serving *any* answer for a table containing one exotic transfer, possibly forever |
| **Widen to `i256`** | large: DuckDB's HUGEINT *is* 128-bit, so the cold fold has no wider native type; it would mean string arithmetic or a decimal type, on the hot path too | the DuckDB-native fold, which is the fast path RFC-0042 §14 just committed to |
| **Do nothing** | free | leaves a number that is quietly wrong with a flag that says it is fine |

**Recommendation:** *count and surface*. It is the cheap option, it uses a channel that already
exists, and it converts a silent wrong answer into a stated incomplete one - which is the difference
the issue's word "honest" is asking for. Widening is disproportionate: values above 2^127 are not
token amounts anyone holds, and the fold's speed was measured and committed to only yesterday.

**One thing to check before implementing:** whether `degraded_tables` is the right channel or whether
this wants its own field. `degraded` currently means "a table could not be fully read"; "a row was
unrepresentable" is a different claim, and conflating them would make an operator chase a storage
fault that is not there.

## What is not decided here

Both of the above. And in COR-6's case, the reorg question is unmeasured - if the board picks
anything other than refusal, a rollback test over a colliding schema comes first.
