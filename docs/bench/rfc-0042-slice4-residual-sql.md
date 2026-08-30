# RFC-0042 slice 4: can DataFusion read the SQL our nests actually declare?

Measured 2026-08-30. Corpus: every `views/*.sql` and `checks/*.sql` in the nest repos on this box -
`dips-nest`, `epoch-block-oracle-nest`, `graph-allocations-nest`, `graph-gns-nest`,
`graph-staking-nest`, `graph-tap-escrow-nest`, `peeranha-nest`, `qos-reo-nest`, `spookyswap-nest`,
`nests-mvp`. Probe: `tools/df-gate/src/bin/view_dialect.rs`.

## Why this question, and why now

#964 measured DataFusion on `net_balances` at 2.5-2.9x DuckDB, which read as disqualifying under §7.
But §5.3's composed path routes a **heavy known fold** to a specialised operator, and #987 showed that
operator beating DuckDB by 18-47%. So in a composed path the fold never reaches DataFusion, and what
DataFusion would actually carry is *general and ad-hoc* SQL - a different workload from the one #964's
number describes.

Before timing that workload, the cheaper question: **can it parse what we already ship?** A dialect gap
is a hard blocker no amount of speed fixes.

## Corpus shape

53 files, of which **26 are comment-only** (documentation stubs) leaving **27 with SQL**. Constructs
present, by file:

| construct | files |
| --- | ---: |
| `GROUP BY` | 12 |
| CTE (`WITH`) | 10 |
| `UNION` | 9 |
| `JOIN` | 7 |
| window (`OVER`) | 5 |
| `DISTINCT` | 4 |
| `FILTER (` | 4 |
| `::` cast | 4 |
| `PARTITION BY` | 3 |
| `EXISTS` | 3 |
| `LATERAL` | 1 |
| `HAVING` | 1 |
| `QUALIFY` | 0 |

Modest SQL. Window functions appear in 5 files, `LATERAL` in 1, `QUALIFY` in none.

## Result: no dialect gap

**DataFusion's parser reads all 27.** Zero `DIALECT-GAP` lines.

DuckDB's reference check parses 21 of 27; the other 6 each contain **two** `CREATE VIEW` statements and
the probe extracts a single select, so that is the probe's limit rather than DuckDB's - every file that
holds one view parses under both.

## Four wrong probes preceded this number

Recorded because each produced a plausible-looking result, and reporting any of them would have been
worse than reporting nothing.

1. **`files=0`.** Paths were relative to another directory and an unreadable file silently `continue`d -
   a confident zero from a probe that never opened anything. It now `exit(2)`s on an unreadable file.
2. **"DuckDB rejects 18 of its own shipped views."** `json_serialize_sql` returns
   `{"error":true,"error_type":"not implemented","error_message":"Only SELECT statements can be
   serialized to json!"}` for a `CREATE VIEW`. That is a serialisation limit, not a parse failure, and
   the claim should have been unbelievable on its face.
3. **Both engines rejecting the same 18.** The extraction used `find(" AS ")`, which needs a trailing
   space - a view header ends `AS\n`, so it skipped past and matched the ` AS ` inside
   `CAST(log_index AS VARCHAR)`, handing both parsers a fragment. Two engines agreeing is not
   corroboration when they were handed the same broken input.
4. **A construct census reporting "98 of 79 files."** `grep -lc` is contradictory, and a count larger
   than the corpus should stop the reader. The corpus was also double-counted by overlapping `find`
   patterns - 53 unique, not 79.

## What this does not establish

**Parse only.** Planning and execution are separate: planning needs a schema, and execution needs data
and the segment layout that #964 and #987 both found dominant. A parser that accepts a query says
nothing about whether the plan is correct or fast.

**Not a verdict.** Per §3a the outcome stays binary and per §0 there is no preferred answer. This
removes one hypothesised blocker - a dialect gap - and leaves the rest of slice 4 open.
