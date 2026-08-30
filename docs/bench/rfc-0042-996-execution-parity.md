# RFC-0042 #996: the authored views, executed against real Lodestar data

Measured 2026-08-30 on Apple Silicon. Corpus: `graph-allocations-nest` - the Lodestar nest -
**38,428 sealed segments, 595 MB, 34 tables with data**. Read-only; no service touched, no nest
switched. Harness `tools/df-gate/src/bin/view_exec.rs`, medians of 7.

## Result: parity exact, timing competitive

**5 views ran. 5 parity-exact. 0 mismatches.**

| view | rows | DuckDB | DataFusion | ratio | `HUGEINT` rewrite |
| --- | ---: | ---: | ---: | ---: | :---: |
| `open_allocations` | 6 779 | 1014 ms | 1018 ms | **1.00x** | no |
| `lodestar_allocations` | 248 487 | 4773 ms | 4320 ms | **0.91x** | yes |
| `epoch_boundaries` | 256 | 1008 ms | 820 ms | **0.81x** | yes |
| `deployment_signal` | 13 896 | 1217 ms | 1639 ms | 1.35x | yes |
| `lodestar_epochs` | 256 | 5201 ms | 8521 ms | 1.64x | yes |

**Range 0.81x-1.64x, faster on two of five including the largest** - a quarter of a million rows agreed
on exactly, value for value.

## This is the number #964 could not produce

#964 measured DataFusion at **2.53-2.80x** on `net_balances` *at a realistic 10 000-segment layout* and
that read as disqualifying under §7. But §5.3 routes a heavy known fold to a specialised operator
(#987, **0.55-0.85x** across the same sweep), so **in a composed path that query never reaches
DataFusion.** What DataFusion would actually carry is this: authored views.

On that workload it is between 19% faster and 64% slower, on real data, at a real segment count.
`net_balances` was measuring a workload the composed path would not send it.

## Two genuine gaps

1. **`Unsupported SQL type HUGEINT`** - 4 of 5 views needed the substitution to `DECIMAL(38,0)`, the
   equivalent 128-bit width. Mechanical, and every rewritten view still returned identical results.
   Per §3 this is a regression **without a compatibility layer**, not a free pass.
2. **`port_queue`** - "Schema contains qualified field name `s.net_signal` and unqualified field name
   `net_signal` which would be ambiguous". DataFusion's ambiguity rules are stricter than DuckDB's.
   One view, and the only non-`HUGEINT` failure found.

The three `DUCK-ERR` lines are tables declared in `schema.json` with **no sealed segments**
(`disputes__query_dispute_created`, `escrow__withdraw`, `staking_legacy__stake_slashed`) - absent data,
not an engine gap, and they fail identically under DuckDB.

## Two harness bugs were caught before publishing, not after

Both would have produced a confident wrong headline.

**"4 parity failures"** - row counts matched exactly (13 896 vs 13 896, 248 487 vs 248 487) and so did
every value. The comparison rendered DuckDB values with `Debug` (`Text("0x00..")`, `HugeInt(990..)`,
`Null`) against DataFusion's display form (`0x00..`, `990..`, empty). A harness bug dressed as a
finding.

**"Table does not exist"** on every dependent view - the harness ran each authored view but never
registered it, so `port_queue` could not see `deployment_signal`. That reads as a corpus gap and is not
one.

And the first timings were **single runs**, which `noise-floor.md` forbids quoting - "a single
measurement is worthless here". They said 1.15-1.81x; the medians say 0.81-1.64x, and two views that
looked slower are faster.

## What this does not establish

- **Five views, one nest.** The corpus is the most demanding one we have, but it is five queries.
- **Seven repeats, not fifteen.** These are seconds-scale queries over 38 428 segments; `noise-floor.md`
  asks for >=15 and this is short of it. Treat sub-10% differences as unresolved.
- **Single-client.** #986 found concurrent behaviour diverging sharply from single-client behaviour, and
  none of this is under load.
- **Nothing was built.** This is a spike outside nuthatch's dependency graph. Five of DuckDB's six roles
  still have no implementation.

Per §3a the outcome stays binary and per §0 there is no preferred answer.
