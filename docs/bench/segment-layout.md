# Sealed-segment layout on a long-running nest (#889)

Measured 2026-08-29 on the Lodestar production box. RFC-0043 §7.1 noted that upstream Amp shipped
compaction and Bloom filters off and small Parquet files degraded planning; our sealing path is not
their tip path, so this checks whether the same shape reached us by a different route.

It has.

## What ships today

Read from the code, not assumed: `seal.rs::write_parquet` sets **compression only**
(`Compression::SNAPPY`). Everything else is the `parquet` crate's default. **There is no compaction
anywhere in the codebase** - no function, no trigger, no scheduled pass.

Read from a real segment's footer with a purpose-built reader, because "the default is probably X" is
how a measurement becomes an assumption:

| property | actual |
| --- | --- |
| column statistics | **written, every column** - so predicate pushdown has something to use |
| Bloom filters | **written on none** |
| row groups per file | **always 1** - each file is one seal |
| writer | `parquet-rs version 58.3.0` |

## The layout, per nest

| nest | files | total | median | p90 | max |
| --- | ---: | ---: | ---: | ---: | ---: |
| `horizon-nest` | **10,923** | 389 MB | **6 KB** | 83 KB | 1,864 KB |
| `graph-staking-nest` | 523 | 5 MB | 5 KB | 6 KB | 17 KB |
| `graph-gns-nest` | 110 | 2 MB | 5 KB | 5 KB | 929 KB |
| `entity-soak` | 264 | 71 MB | 257 KB | 425 KB | 1,590 KB |
| `dips-nest` | 20 | 1 MB | 4 KB | 5 KB | 6 KB |

The median is the finding, not the tail. **A 6 KB Parquet file is mostly footer**, and the smallest
segment on the box holds **one row in 545 bytes** - a complete file, schema and metadata, for a single
row. `entity-soak` shows the contrast: a nest sealing wider windows lands at a 257 KB median.

## Does it cost anything? Yes, and it is separable

Five runs each, `COUNT(*)`, both nests on 3.0.0-alpha.1 so #896 is in and view definition is not the
variable:

| table | segments | rows | mean | min |
| --- | ---: | ---: | ---: | ---: |
| `staking__thawing_period_cleared` | 1 | 1 | 29 ms | 26 ms |
| `service__service_started` | 806 | 254,458 | 202 ms | 184 ms |
| `service__service_payment_collected` | 1,090 | 319,280 | 218 ms | 204 ms |
| `usdc__transfer` (entity-soak) | **264** | **795,903** | **161 ms** | 143 ms |

**The model-free statement, and the one to trust:** `usdc__transfer` reads **3.1x more rows** from
**3.1x fewer segments** and is **20% faster**. Whatever the coefficients, a file costs more than the
rows in it on this deployment.

Fitting `t = a + b·segments + c·rows` across the four points gives a ≈ 29 ms fixed and **b ≈ 0.14 to
0.18 ms per segment** depending on which pair you solve from. The spread is the honest precision of a
four-point fit; the conclusion survives either end. At the **low** estimate, 1,090 segments account
for ~153 ms of that query's 218 ms.

## What #896 already fixed, and what it did not

`SELECT 1` no longer tracks segment count at all: **30 ms on 10,923 segments** against 103 ms on 264.
Before #896 it was 2,465 ms on a 38,428-segment nest, because a view was defined per manifest table
whether the statement named it or not. That correlation is gone.

What remains is the **read** path: a query that names a table still opens every segment of it.

## What this means for RFC-0042

**Segment layout is a covariate of every engine measurement**, which is why sprint `exacting-egret`
sequences this before slice zero. A DuckDB-versus-candidate comparison run against
`service__service_payment_collected` is, on these numbers, measuring 1,090 small files at least as
much as it is measuring an engine. A ratio produced there would have two causes and name one.

## What is not established

- **No causal claim about *why* a file costs ~0.15 ms.** Open, footer parse, statistics decode and
  planning are not separated here. RFC-0042 §5.1's rule applies: a ratio without a cause is not an
  architectural conclusion.
- **Whether compaction would help, and at what cost.** Not measured. Segments are content-addressed
  and RFC-0009 makes the sealed layer append-only, so compaction is a design question with an
  identity consequence, not a flag to flip.
- **Whether Bloom filters would pay.** Statistics are already written; the marginal value of blooms on
  these predicates is unmeasured, and on 6 KB files the filter could plausibly cost more than it saves.
- Cold versus warm page cache is not controlled. All figures are warm, repeated five times.
