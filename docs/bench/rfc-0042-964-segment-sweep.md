# RFC-0042 #964: the engine gate over a realistic multi-segment layout

Measured 2026-08-30 on the Linux dev box (`pepe-thinkpad`, 32 cores, 62 GB, Debian 13 / GCC 14, load
0.35 at start), tree `dbaf935d`. **DataFusion 55.0.0 / Arrow 59** against **DuckDB 1.10504.0 bundled**,
same query, same fixture generator and same acceptance criterion as slice 2 (#956).

Run on Linux rather than the Mac deliberately: this is a timing comparison and the laptop is also
running everything else. Raw log alongside this file.

## The question

Slice 2 measured **one segment**. A real nest has **10,923** at a 6.3 KB median (#889). Its own write-up
said so and named this as the next measurement:

> Nothing here says which handles many-small-files better, and on our real layout that could dominate
> everything above.

> **Correction, 2026-08-30 (#981).** This document said the two orderings agreed within noise. **The
> `REPEATS` path never read `DF_FIRST`** - it always ran duckdb, then datafusion, then rust - so the
> "df_first" rows were the *same execution order run twice*. What that demonstrated was run-to-run
> stability, not order-independence, and it biased against DuckDB, which always paid the cold-cache
> cost on the first iteration.
>
> **The harness is fixed and the sweep was re-run with orderings that genuinely differ.** The findings
> hold: ratios move by at most 0.04 between orderings, and parity is now verified *before* any timing
> is printed. Corrected figures from the re-run are in `rfc-0042-981-reswept.md`; the tables below are
> the original measurements and their ratios stand.

## Result: it dominates everything above

`SEGMENTS=n` splits the same rows across n files; both orderings run at every point. **Parity identical
at all 24 configurations** - the acceptance criterion before any timing counts.

Median ms over 15 repeats at 2 M, 7 at 8 M and 20 M. Both orderings shown, because a ratio that
survives both is about the engines rather than about who warmed the page cache.

| rows | segments | DuckDB | DataFusion | ratio (duck-first / df-first) |
| ---: | ---: | ---: | ---: | --- |
| 2 M | 1 | 68 / 65 | 37 / 38 | **0.54 / 0.58** |
| 2 M | 100 | 25 / 28 | 47 / 48 | 1.88 / 1.71 |
| 2 M | 1 000 | 70 / 54 | 77 / 72 | 1.10 / 1.33 |
| 2 M | 10 000 | 310 / 310 | 856 / 894 | **2.76 / 2.88** |
| 8 M | 1 | 108 / 102 | 196 / 195 | 1.81 / 1.91 |
| 8 M | 100 | 84 / 88 | 160 / 163 | 1.90 / 1.85 |
| 8 M | 1 000 | 88 / 87 | 154 / 153 | 1.75 / 1.76 |
| 8 M | 10 000 | 352 / 349 | 880 / 881 | **2.50 / 2.52** |
| 20 M | 1 | 191 / 192 | 382 / 379 | 2.00 / 1.97 |
| 20 M | 100 | 195 / 192 | 381 / 382 | 1.95 / 1.99 |
| 20 M | 1 000 | 182 / 182 | 348 / 340 | 1.91 / 1.87 |
| 20 M | 10 000 | 424 / 424 | 1061 / 1049 | **2.50 / 2.47** |

## What this changes

**Slice 2's headline finding does not survive.** Its new result was "at 2 M rows DataFusion now wins,
where it lost by 1.85x" - reproduced here on different hardware and more strongly (0.54x). **At 10 000
segments that inverts to 2.76x slower.** The crossover is a property of the one-segment fixture, not of
the row count.

At a realistic segment count **DataFusion is 2.5-2.9x slower at every size measured**, which is
RFC-0013's original finding restored - and restored at the small end where slice 2 had reported it
overturned.

**Both engines degrade with many small files; they do not degrade alike.** From 1 to 10 000 segments at
2 M rows, DuckDB goes 68 -> 310 ms (**4.6x**) and DataFusion 37 -> 856 ms (**23x**). That gap is the
whole finding.

One detail worth keeping: at 2 M rows DuckDB is *faster* at 100 segments than at 1 (68 -> 25 ms),
presumably parallelising across files. DataFusion does not (37 -> 47 ms). So the harm is not
"small files are slow" but that only one of the two engines gets anything back for them.

## What it does not say

**Not a verdict on RFC-0042.** Per §3a the outcome is binary, and per §0 there is no preferred answer;
this is one role - general SQL - measured on one query. The other five roles are #966, and none of them
turns on latency.

**The layout model is an approximation.** #889 measured `horizon-nest` at a 6.3 KB median, 80% under
20 KB, bimodal because the two seal paths are (#947). The fixture reproduces that shape - 80% of files
holding 5% of the rows between them - not that exact distribution. An even split was deliberately
avoided: it is a different problem and would flatter whichever engine handles uniform work better.

**The layout is not fixed.** #947 concluded the tip path should batch the way the backfill path already
does, which would make segments fewer and larger. So 10 000 segments is today's shape and the
pessimistic bound, and the 1 000-segment row is the closest thing here to what a compacted nest would
look like - where the ratio is 1.75-1.91, its narrowest.

**Segmentation does not change the answer.** Parity held at all 24 points, and the fixture is generated
by offset so the union of any layout is exactly the rows a single file would hold. That is asserted on
the i128 path; the float path is a separate question, noted on #961.
