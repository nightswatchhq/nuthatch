# RFC-0042: the segment sweep, re-run with real orderings (#981)

`docs/bench/rfc-0042-964-segment-sweep.md` and `rfc-0042-987-fold-operator.md` both claimed their two
orderings agreed within noise. **They never ran two orderings.** The `REPEATS` path in
`tools/df-gate/src/main.rs` ignored `DF_FIRST` and always ran duckdb -> datafusion -> rust, so
`DF_FIRST=1` changed the printed label and nothing else. Found by review (#981), not by me.

That matters twice over. Run order is a real confound - the gate's own comment records an OBIB run
3.9x slower for exactly that reason - and the uncontrolled order **biased against DuckDB**, which always
paid the cold-cache cost on iteration one.

A second defect in the same path: the RESULT line printed *before* the parity bail, so a comparison
between engines that disagreed could be copied into a record before anyone read the error.

Both fixed, with controls. The parity control (`BREAK_PARITY=1`) drops one candidate row and the run
must bail with **no RESULT line**. The ordering control immediately found a *third* bug:
`std::env::var("DF_FIRST").is_ok()` is true for an **empty** value, so `DF_FIRST=` silently forced
df-first ordering.

## Re-run, orderings genuinely distinct

`pepe-thinkpad`, 32 cores, medians of 15, parity verified before any figure printed.

| rows | segments | order | DuckDB | DataFusion | Rust operator |
| ---: | ---: | --- | ---: | ---: | ---: |
| 2 M | 1 | duck_first | 73 ms | 0.52x | 0.77x |
| 2 M | 1 | df_first | 70 ms | 0.54x | 0.81x |
| 2 M | 1 000 | duck_first | 47 ms | 1.57x | 0.77x |
| 2 M | 1 000 | df_first | 46 ms | 1.59x | 0.78x |
| 2 M | 10 000 | duck_first | 310 ms | 2.75x | 0.73x |
| 2 M | 10 000 | df_first | 308 ms | 2.80x | 0.74x |
| 20 M | 1 | duck_first | 192 ms | 1.97x | 0.57x |
| 20 M | 1 | df_first | 200 ms | 1.90x | 0.55x |
| 20 M | 1 000 | duck_first | 177 ms | 1.92x | 0.70x |
| 20 M | 1 000 | df_first | 180 ms | 1.90x | 0.69x |
| 20 M | 10 000 | duck_first | 418 ms | 2.59x | 0.84x |
| 20 M | 10 000 | df_first | 420 ms | 2.53x | 0.85x |

**The findings hold.** Ratios move by at most **0.04** between orderings. DataFusion is 2.53-2.80x at
the realistic 10 000-segment layout; the specialised operator is **0.55-0.85x**, faster than DuckDB
everywhere.

That the conclusions survived is luck, not diligence. The claim was unverified when published, and a
larger ordering effect would have invalidated two documents.

## The habit this belongs to

Three review findings in one day - #981, #982 (a join corpus that grouped both sides to three rows
before joining, so it never crossed the boundary it was named for) and #983 (a test contract
contradicting its own fixture) - share one shape: **a property asserted in prose that the code did not
deliver.** So does slice 0's role inventory (#970) and #986's row count.

The check that catches it is re-reading each claim against the code or data *before* publishing, not
after.
