# Sprint: fastidious-fulmar

## Definition of done

Every issue labelled `fastidious-fulmar` closed, and no open PR for one of them.

## The theme

**Answer RFC-0042's one live question with a number, and refuse to answer it with anything else.**

RFC-0042 was unfrozen in full on 2026-08-29, after slices 0 and 1 established what DuckDB costs and
what it is for. The RFC's own decision rule is unchanged and governs this sprint:

> There is no preferred answer. If evidence says DuckDB remains best, it stays.

Slice 0 already found evidence pointing that way. **DuckDB is 10.6% of clean build time on Linux and
8.0% on macOS**, while wasmtime and cranelift are roughly twice that on both - so §1's premise that it
*dominates* build time is measured false. It is 93% of native artefact bytes and the sole reason a C++
runtime links, which is real but narrower than the RFC assumed, and on macOS `libc++` is a system
library that costs a user nothing.

So this sprint is not a migration. It is the measurement that decides whether one is worth discussing,
and **"keep DuckDB, with these regressions quantified" is a successful outcome.**

## The pieces

### 1. #945 - finish the parity corpus. First, because it gates the number

`rfc verification`. The corpus covers 7 of §6's shapes. **hot+cold is among the missing**, which is
where COR-1's disjointness invariant lives and the shape most likely to separate two engines. Also
absent: joins, authored and nested views, timeout and cancellation.

§13's one outstanding readiness condition. A spike measured against a corpus that cannot see a
chunk-seam defect produces a number nobody should act on - and #894 is what that looks like: **857
tests all sat under dbsp's 10,000-row step**, so none could see a relation keeping `groups mod 10,000`.

### 2. #947 - segment layout, because it is a confound before it is a feature

`question performance`. 10,923 files at a **6 KB median** on the oldest nest, no compaction anywhere in
the codebase, and a file costs 0.14-0.18 ms on a `COUNT(*)`. A DuckDB-versus-candidate comparison over
many-small-segments measures file layout at least as much as an engine.

Either normalise it or record it as a covariate of every figure. **Not** a licence to add compaction:
segments are content-addressed, RFC-0009 makes the sealed layer append-only, and RFC-0011 compares
segment hashes across operators as a determinism check. Rewriting a segment changes its identity.

### 3. #956 - slice 2, the DataFusion re-run

`rfc performance`. The one live question. RFC-0013 §4 measured **1.6-2.7x, gap widening with segment
count, 2026-08-02**; §0 forbids inheriting that as a premise.

Measured by the methods established at cost this week: medians of >= 15 runs single-client, **p95 under
concurrency** because the median goes bimodal, 5% as the noise threshold, segment layout recorded
alongside, and every material gap profiled to a cause. §5.1: *a ratio without a cause is not an
architectural conclusion*.

### 4. #944 - close the `graft.rs` leak

`rfc tech-debt`. Six public functions take a `duckdb::Connection`, and it is the same module that
writes the engine string into grafting identity - `engine_version(conn)` is *how* the string is
recorded, so the API leak and the migration consequence are one problem.

Deferred from slice 1 because it would have prejudged slice 2. It no longer does: the RFC is unfrozen,
and closing it is what makes a candidate engine substitutable at all.

**Sequenced after #956**, deliberately. Reshaping that surface before the number exists would be
building for an answer we have not got.

## Explicitly not in this sprint

- **Slices 3 to 6** - Turso, the composed path, the decision, the native tail. §10 allows 3 in parallel
  with 2; this sprint does not take it, because slice 2's causes are what tell you whether a second
  spike is worth running. Taking both is how an investigation becomes a programme.
- **Compaction as a change.** #947 is measurement and a design question. Any change is a separate
  decision with numbers attached.
- **#296** (compact binary row format). Adjacent to what slice 2 might find, and doing it first would
  contaminate the comparison.
- **#750** (`p1`, `board-only`), #698, #814, #638, #815, #790. Real, none urgent.

## How this sprint runs

Standing, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

And one added from `exacting-egret`, which earned it six times over:

4. **A gate is at its most dangerous on the day it is written.** Three gates written that sprint passed
   with the guarded thing deleted; one matched its own source code; one counted historical CI runs as
   current; one merged on an empty check list. None was found by reading. **Mutate on day one.**
