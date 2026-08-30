# RFC-0042 #987: the specialised Rust operator for the heavy fold

Measured 2026-08-30 on the Linux dev box (`pepe-thinkpad`, 32 cores, 62 GB, Debian 13 / GCC 14, load
0.19 at start), tree `bef5836b`. Same query, fixture generator, segment sweep and acceptance criterion
as #964, with a third implementation added. Raw log alongside.

## The question §5.3 asked and nobody had measured

#964 found DataFusion **2.5-2.9x** slower than DuckDB on `net_balances` at a realistic layout - about
ten times the margin §7 already calls disqualifying. But §5.3 never routed a heavy known fold to general
SQL:

```text
general SQL          -> DataFusion
heavy known fold     -> specialised Rust physical operator
```

#964 measured this query going down the *general SQL* arm. This is the arm §5.3 points it at: `parquet`
+ `arrow` read the segments, an `i128` accumulator does the fold, no SQL engine in the path.

## Result: the operator beats DuckDB at every configuration

**Parity identical at all 24** - the acceptance criterion before any timing counts. Ratios are
candidate/DuckDB, so **below 1.00 is faster than DuckDB**. Both orderings shown; they agree within
noise everywhere.

| rows | segments | DuckDB ms | DataFusion | **Rust operator** |
| ---: | ---: | ---: | ---: | ---: |
| 2 M | 1 | 70 / 68 | 0.53x / 0.54x | **0.80x / 0.82x** |
| 2 M | 100 | 26 / 27 | 1.73x / 1.74x | **0.58x / 0.59x** |
| 2 M | 1 000 | 47 / 48 | 1.53x / 1.52x | **0.77x / 0.77x** |
| 2 M | 10 000 | 307 / 310 | 2.74x / 2.69x | **0.75x / 0.73x** |
| 8 M | 1 | 107 / 102 | 1.79x / 1.87x | **0.75x / 0.75x** |
| 8 M | 100 | 83 / 83 | 1.88x / 1.93x | **0.55x / 0.54x** |
| 8 M | 1 000 | 86 / 85 | 1.76x / 1.82x | **0.77x / 0.78x** |
| 8 M | 10 000 | 344 / 343 | 2.56x / 2.56x | **0.76x / 0.78x** |
| 20 M | 1 | 196 / 180 | 1.83x / 2.09x | **0.56x / 0.61x** |
| 20 M | 100 | 193 / 198 | 1.93x / 1.92x | **0.56x / 0.55x** |
| 20 M | 1 000 | 173 / 174 | 1.97x / 1.93x | **0.69x / 0.67x** |
| 20 M | 10 000 | 425 / 429 | 2.54x / 2.50x | **0.80x / 0.82x** |

**Range 0.53x to 0.82x - between 18% and 47% faster than DuckDB, at every size and every layout.**
Against `noise-floor.md`'s 5% floor that is not a close call in either direction.

At the realistic 10 000-segment layout the two arms separate completely: DataFusion 2.5-2.74x slower,
the specialised operator 0.73-0.82x. **§5.3's routing was right, and #964 was measuring the wrong arm
for this query.**

## What it took, recorded because the first number was wrong

The first working operator ran at **8.04x DuckDB** - worse than DataFusion. That number was never
published, because publishing it would have been benchmark gaming in the direction that flatters the
incumbent, which §11 lists as a risk. Profiling (`src/bin/fold_profile.rs`) on a 200k-row/20-file
fixture:

| stage | ms |
| --- | ---: |
| Parquet decode only | 98 |
| + column access | 99 |
| + `i128` parse | 100 |
| full fold | **224** |

The accumulator was 124 ms and decode 98 ms, while DuckDB did the whole query in 28 ms. What the
operator lacked was what DuckDB has as a matter of course: a fast non-cryptographic hash, and every
core. `rustc-hash` plus rayon took it to 1.81x on that fixture, and to the table above at real sizes.

Two implementation notes that are part of the result:

- **Split by row group, not by file.** File-splitting gives a one-segment fixture no parallelism, and
  the sweep runs 1 to 10 000 segments over the same rows - a file-parallel operator would look strong
  only where the layout suited it, the exact confound the sweep exists to remove.
- **Exact `i128` addition is what makes any split safe.** Partials merge without reassociation error.
  A float sum could not claim that - see #961, where forcing the order took `99998` to `0.0`.

All three implementations use the machine's cores; nothing pins threads, so DuckDB and DataFusion run
with their defaults as they did in #964.

## What this does NOT say

**It is not a licence to remove DuckDB, and #987 said so before the measurement was taken.**

- **One query.** `net_balances` is the material workload and was chosen for that reason, but the
  operator hardcodes its semantics. It is not a query engine and cannot be pointed at authored views or
  ad-hoc `/sql`.
- **A specialised operator beating a general engine at the specialised thing is expected.** The finding
  is not that it wins but that it wins *decisively* - 18-47%, far outside noise - which is what §7
  requires and what an "only 15% slower" candidate fails.
- **The other roles are untouched.** #966 settled that none blocks removal on its own, but the
  admissible function vocabulary, the lowering AST and authored views still need an engine. Nothing here
  replaces one.
- **Four of §11's eleven rows remain unmeasured** (#986): concurrent throughput, ingest, restart-to-ready
  and a real peak-RSS figure. This gate is single-client by construction.

## Where it leaves the RFC

Before this, the evidence pointed at *"Keep DuckDB, because these measured regressions remain"* - §0's
explicitly successful outcome - because the only measured candidate failed §7 by a factor of ten.

**That reading is no longer available.** §5.3's composed path is real for the role that matters most,
so the question is no longer whether a Rust path can match DuckDB on the heavy fold. It is whether the
*rest* of the surface - general SQL, authored views, the function vocabulary, the lowering AST - can be
composed without losing what §3 lists.

Per §3a the outcome is still binary and per §0 there is still no preferred answer. This is evidence for
slice 4 to continue on, which is exactly what #987 was scoped to produce.
