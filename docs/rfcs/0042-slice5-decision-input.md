# RFC-0042 slice 5: the decision input

> **The verdict was taken on 2026-08-30: KEEP DuckDB. It is RFC-0042 §14, not this document.** This
> file remains the evidence input, amended in place where it was later found wrong - see the struck
> known cost 4 and the withdrawn concurrent-throughput row.

**This document contains no verdict.** §10 makes slice 5 "the decision" and §11 requires it written as
an RFC amendment saying either **Remove DuckDB** or **Keep DuckDB, because these measured regressions
remain**. That was the board's to write. This assembles what has been measured, what has not, and what
each figure does and does not cover, so the decision is made from evidence rather than from a summary
of a summary.

Every figure below names its source. Where a number was corrected, the correction is shown rather than
the number quietly replaced.

## §11's table, as filled

| property | DuckDB baseline | Best Rust candidate | source |
| --- | --- | --- | --- |
| correctness - heavy fold | reference | **exact parity, 24/24 configurations** | #987, #981 |
| correctness - authored views | reference | **exact parity, 5/5 views, 248 487 rows** | #996 |
| representative query - heavy fold | baseline | **0.55-0.85x** (faster) | #987, re-run #981 |
| representative query - authored views | baseline | **0.81-1.64x** | #996 |
| general SQL via DataFusion, realistic layout | baseline | 2.53-2.80x (slower) | #964, re-run #981 |
| concurrent throughput | ~~40.3 -> 39.6 qps~~ **WITHDRAWN - harness mutex, not the product's.** 14.8 -> 81.5 qps, 1 -> 32 clients | not comparable: the Rust figure was measured against that harness | #986, **struck by §14** |
| peak RSS | ~60 MB, CI-gated at 256 MB | not measured for the candidate | `footprint` job |
| ingest throughput | `bench backfill` events/sec | n/a - the candidate is not an ingest path | RFC-0004 |
| restart-to-ready | **68.2 ms at 10 blocks, 74.0 ms at 500**; warm restart is **0.44-0.61x** a cold start | n/a | #992, corrected #999 |
| high-cardinality aggregate | not measured for this comparison | - | - |
| clean debug/release build | **DuckDB 10.6% of clean build (Linux), 8.0% (macOS)** | - | #935 |
| final binary | **DuckDB is 93% of native artefact bytes** | - | #935 |
| C++ compilation | yes - **the sole reason a C++ runtime links** | no | #935 |
| external services | none | none | parity |

## The finding that reframed everything

#964 measured DataFusion at **2.53-2.80x** on `net_balances` at a realistic 10 000-segment layout, and
under §7 - *"'only 15% slower' fails if outside measured noise on a load-bearing workload"* - that read
as disqualifying by roughly a factor of ten.

**§5.3 never routed a heavy known fold to general SQL.** It routes it to a specialised operator. #987
built one: **0.55-0.85x**, faster than DuckDB at every size and layout, parity exact. So in a composed
path `net_balances` never reaches DataFusion, and #964 was measuring a workload that path would not
send it.

What DataFusion would actually carry is authored views. #996 ran those on **38 428 real Lodestar
segments**: parity exact, **0.81-1.64x**, faster on two of five including the largest.

## Known costs, stated as costs

1. **`HUGEINT` requires a compatibility layer.** 4 of 5 real views needed substituting to
   `DECIMAL(38,0)`. Mechanical, and every rewritten view returned identical results - but §3 is
   explicit: *"Requiring users to rewrite valid Nuthatch SQL is a regression without a compatibility
   layer."*
2. **One view hits stricter ambiguity rules.** `port_queue` - qualified and unqualified `net_signal` in
   one schema. DuckDB accepts it; DataFusion does not.
3. **Five of DuckDB's six roles have no implementation.** #966 established none blocks removal on its
   own. That is not the same as any being replaced. **Nothing has been built**: every measurement here
   comes from `tools/df-gate`, deliberately outside nuthatch's dependency graph.
4. ~~**The concurrency win is architectural, not an engine property.** The Rust path scales because it
   has no shared connection. Any engine deployed as `analytics.rs` deploys DuckDB - one cached
   connection under a mutex - would show the same flat 40 qps. See #991: this is a live product
   characteristic independent of which engine wins.~~
   **STRUCK 2026-08-30 under RFC-0042 §14.** `analytics.rs` does not hold its mutex across the query;
   both guards are statement temporaries taken for a map operation. Measured on the product's own path,
   the same engine gives 14.7 qps with a held mutex and up to 81.5 qps without one. This was not a cost
   of anything, and the concurrent-throughput row it supported cannot carry a removal argument.

## What is not measured, and should not be read as measured

- **High-cardinality aggregate** has no DuckDB-versus-candidate figure at all.
- **Peak RSS for the candidate** is unmeasured; only nuthatch's own footprint is gated.
- **Restart-to-ready does not extrapolate.** 500 blocks against `horizon-nest`'s 10 923 segments, and
  segment count dominated every other measurement here. See #997.
- **#996 is five views, one nest, seven repeats.** `noise-floor.md` asks for >=15; treat sub-10%
  differences there as unresolved.
- **Everything except #986 is single-client.**

## Corrections made to this evidence, listed rather than absorbed

Four claims were published and then corrected. They are listed because the decision should know how
much of its input was revised, and by whom.

| claim | reality | found by |
| --- | --- | --- |
| slice 0: graft engine string "written into artefacts on disk", 2 roles product-visible | nothing is persisted; the role is latent; 1 role product-visible | me (#970) |
| #986: four §11 rows never measured | two already had harnesses | me |
| #964/#987: "both orderings agree within noise" | the `REPEATS` path ignored `DF_FIRST`; one ordering ran twice | **review (#981)** |
| #945: join corpus crosses the vector boundary | both sides grouped to three rows first | **review (#982)** |
| **this document's own known cost 4, plus #986 and #991: `analytics.rs` serialises concurrent queries** | **it does not.** Both mutex guards are statement temporaries for a map operation; the query runs unlocked. 14.7 qps held vs 81.5 qps unheld, same engine, same path | **slice 6** |

The common shape is a property asserted in prose that the code did not deliver. **Five of five now**,
and the fifth is the worst of them: it originated in a source doc comment, was repeated into a
benchmark document, then into an issue, then into this decision input, without anyone reading the
function body. Three of five were caught by review rather than by the author.

## The two admissible outcomes

Per §3a, added 2026-08-30, the outcome is binary. There is no routing hybrid: either DuckDB stays in
every role slice 0 inventoried, or it goes entirely and a §5.3 composition takes all six. Per §0 there
is no preferred answer, and *"keep DuckDB, with these measured regressions"* is an explicitly successful
outcome.
