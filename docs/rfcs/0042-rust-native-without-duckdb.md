# RFC-0042: Can Nuthatch remove DuckDB and become Rust-native without sacrificing anything?

- Status: **Decided and parked, 2026-08-30. KEEP DuckDB.** The decision, the regressions it rests on
  and the conditions that reopen it are in **§14**, which is the amendment §11 requires. The carve-out
  from the 2026 feature freeze is spent and the freeze applies again in full; no further slice of this
  RFC is to be started before a §14 reopen condition is recorded.
- Status history: **Draft.** Unfrozen by board decision 2026-08-25 - the second and last carve-out from
  the 2026 feature freeze, after RFC-0041. **Sequenced behind RFC-0041**: no slice of this RFC starts
  until the authored-incremental-entity work is done, because §9 gives DuckDB four roles inside
  RFC-0041 (parser, incremental reference, restart seed, entity serving) and moving the engine under
  a slice programme that is still assigning those roles would make both unattributable. Tracked as
  #849.
- Author: Pete
- Date: 2026-08-24
- Depends on: RFC-0004 (measurement discipline and benchmark harness), RFC-0013 (historical storage/query-engine investigation), RFC-0034 (bounded query surface), RFC-0041 (authored incremental entities and the current DuckDB reference/parser role).
- Nature: **Investigation and decision RFC.** This RFC does not begin with a chosen replacement.
- Question: **Can Nuthatch remove DuckDB - and potentially all C/C++ implementation dependencies from the shipped binary - without sacrificing correctness, capability, performance, resource bounds, reliability or the one-binary deployment model?**
- Blocks: any claim that DuckDB is permanent; any claim that Nuthatch can become a pure-Rust binary; any large query-engine migration justified primarily by language purity.

## §0 - Reset the premise

This RFC deliberately does **not** inherit RFC-0013's conclusions as premises. RFC-0013 measured a real DataFusion implementation against DuckDB and, on the workload then measured, DataFusion lost materially. That result was useful and the decision reasonable. It is not a law of nature.

Versions, optimisers, Arrow versions, Nuthatch's workload, authored logic, incremental computation and serving paths move. An earlier benchmark is evidence to reproduce or falsify, not an architectural commandment. The same applies to every inherited belief: DuckDB required for OLAP; DataFusion too slow; Turso should replace redb; Turso cannot replace DuckDB; DuckDB dominating C++ build cost; Wasmtime/ring/zstd/mimalloc preventing pure Rust; and general SQL having to execute every important query. They are hypotheses or dependency-tree observations, not premises.

> **Measure the current system, build credible Rust-native alternatives, and accept a replacement only if the product loses nothing that matters.**

There is no preferred answer. If evidence says DuckDB remains best, it stays. If it can disappear with no meaningful regression, it goes.

## §1 - Why ask now?

Nuthatch presents itself as Rust-native and self-contained while its graph includes a substantial bundled DuckDB build. That affects clean-build time, disk use, cross-compilation and contributor experience. Current multi-gigabyte debug and smaller but significant release output are anecdotal until slice zero reproduces them, but a dependency which dominates time, disk and portability complexity must justify itself continuously.

A Rust-only closure could simplify cross-compilation, toolchain installation, reproducible builds, compiler caching, static linking, source auditing, platform bring-up, CI images and onboarding. None is worth a slower or less capable Nuthatch alone. Together they make removing the native tail worth investigating.

Removing the `duckdb` line is not the task. DuckDB currently participates in analytical SQL over sealed Parquet, hot+cold federation, authored views, product query semantics, safety/cancellation, parsing/canonicalisation, and reference evaluation for incremental entities. Slice zero finds the complete list. Every role needs a replacement or deliberate redesign; retaining it as parser, oracle or restart seed engine is not removal.

## §2 - Define “pure Rust” before celebrating it

| Tier | Meaning |
| --- | --- |
| 0 | Current system: DuckDB and the native tail ship. |
| 1 | **DuckDB-free:** production neither links, embeds, loads nor invokes DuckDB. It may remain as a development differential oracle during migration. |
| 2 | **C++-free:** no C++ implementation code compiles into or links with the production binary. Proved from the build, not crate names. |
| 3 | **Rust-native production closure:** no third-party C or C++ implementation library ships. Platform ABI crates do not violate this. Assembly is recorded separately. |
| 4 | **Strict Rust implementation closure:** production implementation is Rust source and Rust-generated code, excluding OS interfaces/toolchain requirements. No bundled C, C++ or hand-written assembly remains. |

Tier 4 is a stretch goal, not a reason to sabotage Tier 2 or 3. The RFC may conclude differently for every tier.

## §3 - “Without sacrificing anything” is a gate

Language purity is not acceptance. For supported Nuthatch SQL, preserve logical result sets; exposed null, exact integer/decimal, overflow/refusal and cast behaviour; hot-only/cold-only/hot+cold results; authored views; reorg-visible state; and determinism. Canonical ordering replaces accidental engine order.

Do not silently remove admitted joins/aggregates, Parquet, hot+cold queries, authored views, entity verification, allowlist, explain/debug support, row or byte limits, timeouts, cancellation or concurrency bounds. Internal spelling differences are acceptable. Requiring users to rewrite valid Nuthatch SQL is a regression without a compatibility layer.

Candidates match or beat material workloads within measured baseline noise, not an arbitrary Rust tax. Measure p50/p95/p99, concurrent throughput, cold/warm cache, bytes read, CPU and wall time; then backfill, tip, sealing, DBSP/entity and reorg throughput. Measure idle/query/concurrent/backfill/entity RSS, FDs and temporary disk. Measure cold startup, warm restart, registration/reconstruction, `/ready` and analytical-current readiness. Preserve one binary, no mandatory service/JVM/runtime Cargo/download/helper daemon, and unattended restart.

Clean debug/release time, incremental rebuild, `target/` growth, final binary and required host packages must not materially regress, and should improve if this succeeds.

### 3a - The outcome is binary: no query-routing hybrid (amended 2026-08-30)

§11 lists "permanent dual production engines" as a risk. Slice 2 turned it into a live temptation, so
it is promoted here from a risk to be managed into an **acceptance rule**.

Slice 2 measured DataFusion at 0.84x DuckDB at 2 M rows and 2.4-2.6x slower at 8 M and 20 M. A
crossover is exactly the evidence shape that invites "route small queries to one engine and large ones
to the other". **That is not an admissible conclusion of this RFC.**

Only two end states are acceptable:

1. **DuckDB stays**, in every role slice 0 inventoried, with the measured regressions of the best Rust
   candidate recorded against it (§0's explicitly successful outcome).
2. **DuckDB goes entirely** - Tier 1 - and a Rust-native composition (§5.3) takes all six roles.

DuckDB may survive Tier 1 **only** as a development-time differential oracle, per §2. It does not
execute a user's query in a shipped binary under any condition, including a fast path for one size
band.

The reason is not aesthetic. A routing layer means two optimisers, two sets of null/decimal/overflow
semantics and two cancellation paths behind one `/sql` surface, which puts §3's preservation list -
exposed null, exact integer/decimal, overflow/refusal, cast behaviour, timeouts, row and byte limits -
on both engines *and* on the router's choice between them. It also doubles the C++ tail rather than
removing it, so it fails §1's premise while claiming §2's prize. A per-size split additionally makes
the engine a function of data volume, so a nest's results and refusals change shape as it grows.

If a candidate wins only in one size band, the finding is "the candidate does not meet the gate", and
the honest write-up says so with the band named. It is not a reason to ship both.

## §4 - Slice zero: establish current truth

Build a native bill of materials from build logs, objects and link maps for every release target. Record every build-script crate; C/C++ compiler or assembler invocation; static and dynamic library; native language; entry reason/feature; production/dev/target-only classification; attributable clean-build time; and output size. DuckDB, zstd, ring, mimalloc and Wasmtime are leads, not a limiting list.

```text
production/linux-x86_64
  DuckDB       C++       184 MB build output   via duckdb/libduckdb-sys
  zstd         C         ...
  ring         C/asm     ...
  mimalloc     C         ...
```

Separately enumerate each DuckDB use as parser, binder, optimiser, execution engine, Parquet reader, hot/cold federation, view catalogue, reference oracle, test-only, migration utility or other. This is the deletion checklist. Until every item has an owner, replacement is too vague.

> **Amended 2026-08-29, after RFC-0041 shipped.** Two corrections from walking the actual call sites,
> because §9's list was written before the entity work finished and turns out to be an undercount.
>
> **Start the inventory from the call sites, not from §9.** DuckDB is reached from six files -
> `analytics.rs`, `entities.rs`, `entity_lower.rs`, `graft.rs`, `seal.rs`, `authored_entity_spike.rs`.
> Two of those roles are **product-visible** and appear nowhere in §9:
>
> - **`graft.rs` writes the engine string into grafting identity**, e.g. `engine: "duckdb-v1.4.0"`.
>   Removing DuckDB therefore raises a compatibility question about grafts already recorded under that
>   engine, which is a migration concern rather than an implementation one (RFC-0033).
> - **`entities.rs` derives the admissible function vocabulary from `duckdb_functions()`** - its own
>   comment says "the same catalogue the binder uses". **The SQL a nest is allowed to declare is
>   DuckDB's function list.** A replacement changes what a user may write in `entities.toml`, so this
>   is a public contract and not a detail. §9 files it vaguely under parsing.
> - `seal.rs` uses a connection as a segment-binding oracle.
>
> **Baseline on post-#896 code, and record why.** Before #896, a `/sql` request defined a view for
> every table in the manifest whether the statement named it or not, at roughly 62 µs per sealed
> segment: `SELECT 1` cost **2,465 ms** on a 38,428-segment nest before reading a row. That was
> Nuthatch's view management, not DuckDB. A baseline taken before that fix charges the engine 2.4
> seconds and makes any replacement look brilliant for a reason that has nothing to do with engines.
> Slice zero must state the commit it baselined at and confirm it is at or after #896.

## §5 - Candidate set: no preselected winner

### 5.1 DataFusion

Re-run DataFusion against current Nuthatch and current DataFusion, not merely old source unchanged. Investigate optimiser behaviour, Arrow compatibility, pruning, projection pushdown, statistics, partitioning, batch size, parallel aggregation, decimal/i128/string representations, hot relation providers, justified custom physical operators and memory. Historical RFC-0013 numbers remain historical. Profile each material gap: scan, decode, grouping, hashing, decimal arithmetic, threading, Nuthatch conversion, bad plan or engine itself. A ratio without a cause is not an architectural conclusion.

### 5.2 Turso

Turso is an in-process Rust candidate with overlapping concerns, not a presumed DuckDB replacement. Test whether it is a better hot store than redb, simplifies SQL-over-tip, participates efficiently in cold Parquet, removes enough machinery to justify change, meets Tier 3/4 with exact shipped features, and what maturity/compatibility risk it brings. Importing immutable history into a second mutable database duplicates storage and changes the design; it must win the same gate. It may replace redb, DuckDB, neither or a narrow role.

### 5.3 Composed Rust path and independent roles

Product performance matters, not one engine doing everything. Measured bottlenecks may move to DBSP entities, seal-time metadata, segment indexes, custom Arrow operators, precomputed statistics or specialised exact-integer folds:

```text
general SQL          -> DataFusion
maintained entities  -> DBSP state
heavy known fold     -> specialised Rust physical operator
sealed facts         -> Parquet
hot facts            -> HotStore TableProvider
```

This retains coherent public SQL and must not become a home-grown database. DuckDB roles may split across a Rust parser, compatibility layer, general engine, frozen corpus/independent reference, hot store and Parquet. Other credible Rust-native engines may enter the gate on evidence.

## §6 - Boundary, benchmark and parity corpus

Introduce an internal analytical boundary able to execute, register hot/cold tables and views, explain and cancel. Its actual API follows Nuthatch capabilities. DuckDB-specific connection/value/AST types do not escape. The point is a fair experiment, not a permanent plugin system:

```text
same dataset / query / guards / caller
           |
           +--> DuckDB baseline
           `--> Rust candidate
```

DuckDB may remain dev/test-only as an oracle during parity work. Tier 1 requires its exclusion from the release graph; later Tier 4 may replace it with frozen results and/or independent pure-Rust references.

Keep `net_balances`, but do not let it decide alone. The corpus includes point lookups; narrow/wide scans; groups; exact signed large integers; multi-column groups; inner/multi-table joins; authored and nested views; hot/cold/hot+cold; entities and raw joins; bounded ordering; row caps; timeout/cancellation; and malformed/refused SQL. Use small/medium/large datasets with multiple Parquet segments, current schemas, realistic widths/cardinality/skew/hot tail, plus sustained real workload data.

Record cold/warm status, engine order and reversed order; control or expose page-cache effects; retain commit/compiler/engine/dataset hash/CPU/RAM/OS/features/parameters and plans for material differences. Continuously compare nulls, empty inputs, absent hot tails, i128 boundaries, signed values, overflow, decimal conversion, casts, duplicates, large values, reorgs, restarts, view ordering, refusal and cancellation. Nuthatch's public contract decides legitimate engine differences.

> **Amended 2026-08-29. Three method requirements, each from something that has already cost us.**
>
> **The corpus must cross every engine's internal batch boundary.** DuckDB's vector is 2,048 rows and
> DataFusion's default batch is 8,192; dbsp splits a transaction into 10,000-row steps. #894 is the
> warning: **all 857 tests sat under dbsp's step size**, so the entire suite was blind to a defect that
> silently kept `groups mod 10,000` of a relation, with nothing faulted and every surviving group
> holding the right value. "Small, medium, large" does not express this. At least one dataset must
> exceed the largest engine-internal boundary in play, and the corpus must say which boundary each
> size was chosen to cross.
>
> **Peak RSS needs a stated window, not a slope.** A nest's RSS sawtooths - measured on 2026-08-28, a
> ~120 MB swing unaided, with a process observed at 396 MB and 266 MB four minutes apart. A slope
> across that window measures the allocator: the same data gave 40,861 bytes per row and 13,943 bytes
> per row depending on where the window started, and a trough-to-trough reading made an entity's cost
> come out *negative*. Report **peak over a stated window**, with the window named, and never a
> per-unit figure derived from a window shorter than the sawtooth.
>
> **Segment layout is a covariate, not a footnote.** Small Parquet files degrade planning (RFC-0043
> §7.1), so a DuckDB-versus-candidate comparison over many-small-segments is partly measuring file
> layout. Either land #889 before slice zero or record file count, size distribution and row-group
> size alongside every measurement. Without it a ratio has two causes and names one.

## §7 - No-sacrifice decision gate

Replacement requires parity; no lost capability; matching hot+cold semantics and guards; no material regression to material query, concurrent throughput, indexing, peak memory, startup/restart or real-workload result; release builds for each supported target; one-binary/no-service intact; and no DuckDB in the release tree. Expected wins include several of clean/incremental build, disk, binary size, cross-compilation, containers, CI and native toolchain requirements. If these do not materialise, removal needs stronger justification.

A close but slower Rust engine remains experimental. “Only 15% slower” fails if outside measured noise on a load-bearing workload.

## §8 - Removing the native tail

After a DuckDB-free candidate, rerun the BOM. If no C++ remains, Tier 2 is proved. Then apply the same gate to mimalloc, zstd, ring and Wasmtime support code: trace their exact activators; test allocator/compression/crypto/runtime alternatives only for no-regression results; preserve security, interoperability and component behaviour; and report assembly separately. A mature crypto primitive does not become weaker for an attractive README sentence.

## §9 - Interaction with RFC-0041

RFC-0041 assigns DuckDB parsing/canonicalisation, incremental-reference evaluation, finalised restart seeds and analytical entity serving roles. Keep these touchpoints behind boundaries. A winning Rust path uses its parser, frozen corpora or independent reference, production-engine seed and direct entity exposure. RFC-0041 semantics survive; the mechanism is not sacred.

> **Amended 2026-08-29: RFC-0041 shipped, and its result is an input to this RFC rather than something
> to rediscover.**
>
> §11 asks "whether RFC-0041 removes expensive general queries". It does, measured on a copy of the
> real Lodestar nest: the `indexer_rewards` panel went **p50 2.15 s to 87.7 ms** and stopped scanning
> the raw historical tables to derive that relation, and `/derived` keyed reads are a `BTreeMap` lookup
> that does not open a DuckDB connection at all.
>
> That **shrinks the general-SQL surface a replacement has to be fast at**, and it shrinks it in the
> direction that matters: the queries a maintained entity replaces were exactly the expensive scanning
> ones. A candidate engine's weakness on wide historical aggregation is worth less than it was in
> RFC-0013's day, and its behaviour on **many small concurrent reads** is worth more. Appendix A
> already names that as the dimension public single-query benchmarks least represent; this is a second
> reason to weight it.

## §10 - Proposed slices

| # | Slice | Ends with |
| --- | --- | --- |
| 0 | Native BOM, role inventory and baseline variance. | What ships, why, what it costs and benchmark noise. |
| 1 | Engine boundary and parity corpus, DuckDB unchanged. | Byte-identical tests/real workload, no change beyond noise. |
| 2 | Fresh DataFusion spike, profiles and plans. | Current yes/no with causes of gaps. |
| 3 | Turso and alternative-role spike. | Measured role-by-role result. |
| 4 | Composed Rust path driven by profiles. | Best candidate measured on full gate. |
| 5 | Decision. | DuckDB removed or precise blockers recorded. |
| 6 | Native tail. | Published BOM and strongest honest tier. |

Slices 2 and 3 may run in parallel after slice 1. Slice 4 follows evidence, not an open-ended plan to write a database. Slice 6 does not block DuckDB removal.

## §11 - Risks, questions and decision rule

Risks are purity becoming the objective, benchmark gaming, DuckDB quirks leaking into contracts, specialisation becoming a worse database, Turso causing an unrelated rewrite, permanent dual production engines, and weaker crypto/compression. The full gate, real/adversarial corpus, explicit contract classification, narrow specialisation, role-by-role spikes, transitional boundary and security measurements address them.

The RFC must answer: build-time share and C++ inventory; genuine engine-required roles; current DataFusion gaps and their operators; whether RFC-0041 removes expensive general queries; zero-copy hot exposure; Turso's real contribution; whether a capable DuckDB-free binary builds today; what blocks equality; and the remaining native tail and strongest tier.

Useful answers include DuckDB stays with quantified blockers, a feature-complete but slower optional Rust path, DuckDB disappears while C remains, or strict Rust succeeds. “No” is useful evidence.

The final amendment contains measurements like this, then says either **Remove DuckDB** or **Keep DuckDB, because these measured regressions remain: ...**:

| Property | DuckDB baseline | Best Rust candidate | Result |
| --- | ---: | ---: | --- |
| correctness | reference | exact | pass/fail |
| representative query p50/p99 | ... | ... | ... |
| high-cardinality aggregate | ... | ... | ... |
| concurrent throughput | ... | ... | ... |
| peak RSS | ... | ... | ... |
| ingest throughput | ... | ... | ... |
| restart-to-ready | ... | ... | ... |
| clean debug/release build | ... | ... | ... |
| final binary | ... | ... | ... |
| C++ compilation | yes | no | win/parity |
| external services | none | none | parity |

No appeal to a previous RFC, Rust purity, reputation, roadmap or “probably”. The question is empirical: can Nuthatch remove DuckDB, then the native tail, while remaining the same or better Nuthatch?

## §14 - The decision, 2026-08-30

**KEEP DuckDB.** Board decision, taken on the evidence assembled in
`0042-slice5-decision-input.md` and the two independent slice 6 runs recorded in
`0042-slice6-report-20260830T182428Z-ec26929f.md` and `0042-slice6-report-20260830T194831Z-994c939b.md`.
This is the amendment §11 requires, and per §0 it is one of the two admissible answers rather than a
failure to reach the other.

The RFC is **parked**, not withdrawn. Its question was worth asking, the investigation answered it, and
§1's premise did not survive contact with measurement: DuckDB is **10.6%** of clean build time on Linux
while wasmtime and cranelift are **21.3%**, so the dependency that was said to dominate build cost does
not.

### Keep DuckDB, because these measured regressions remain

1. **Five of DuckDB's six inventoried roles have no implementation at all** (slice 0 §4; slice 5 known
   cost 3). Parser/canonicalisation, the aggregate-classification vocabulary, the incremental reference
   oracle, the restart seed and entity serving are unbuilt. Two of them - the `duckdb_functions()`
   classifier and the parser's alias canonicalisation - are **public contracts**: they decide what a
   user may write in `entities.toml` and what the allowlist refuses.
2. **`HUGEINT` requires a compatibility layer that does not exist.** 4 of 5 real authored views needed
   rewriting to `DECIMAL(38,0)` (#996). §3 is explicit: *"Requiring users to rewrite valid Nuthatch SQL
   is a regression without a compatibility layer."*
3. **A valid authored view stops binding.** `port_queue` - DataFusion's ambiguity rules are stricter
   than DuckDB's (#996). One view, one lost capability, unresolved.
4. **Exact-arithmetic semantics are the number-one ranked gate risk and remain unmitigated.**
   DataFusion's default arithmetic wraps where DuckDB errors (arrow-datafusion #17539, open). The first
   specialised Rust operator built for this RFC realised that risk in its own code: **#998** needed
   checked arithmetic at three sites, and a 24/24 "parity exact" corpus could not see it.
5. **General SQL over a realistic layout is 2.53-2.80x slower** (#964, re-run #981). §5.3 routes around
   it with a specialised operator, but that operator is one of the dozen not yet written.
6. **§7's expected wins do not materialise.** Two of seven - disk and binary size. Build time is 10.6%
   with a larger consumer left in place, and cross-compilation has **no named beneficiary** across 347
   tracker issues, one discussion, eight grant milestones and the partnership record (A2, both runs,
   independently: zero names).

### What is struck from the ledger, because it was never a regression

**The §11 concurrent-throughput row does not belong on this list.** It measured a mutex the benchmark
harness built, not one nuthatch holds. `analytics.rs` takes its cache mutex twice for a map operation
and releases it before the query runs; the same engine on the same code path measured 14.7 qps flat
with a held mutex and up to 81.5 qps without one. Slice 5's known cost 4 is struck, and
`docs/bench/rfc-0042-986-concurrency.md` is corrected. See §14's *what must happen anyway* below.

### What the evidence does not support either way

- **A3 (static libstdc++) is inconclusive, not negative.** The build produced **102,449,024 bytes**,
  byte-identical to the ordinary release binary, while `ldd` still lists `libstdc++.so.6`. Statically
  absorbing libstdc++ would change the size. The build log records no flag. So the run cannot
  distinguish *"static linking does not remove the tail"* from *"the flags never took effect"*, and it
  moves confidence in neither direction.
- **High-cardinality aggregate** has no DuckDB-versus-candidate figure at all.
- **Peak RSS for the candidate** was never measured.
- **Restart-to-ready is a curve, and it was published as a scalar.** #992's **74 ms** is what a
  500-block fixture does. Re-measured against **segment count** - the variable #964 and #987 both
  found dominant - it is **49.6 ms at 0 segments, 196 ms at 1,000, 801 ms at 5,000 and 1.7 s at
  11,000**, where `horizon-nest` holds 10,923 (#997). Roughly linear, no knee. This does not
  change the decision: it means the DuckDB baseline for this row is a curve, and any future
  candidate must be compared against the curve rather than against 74 ms.
  `docs/bench/rfc-0042-997-restart-by-segment-count.md`.

**Confidence: 78%**, as recorded by the slice 6 run that reached a decision, and unmoved by A3.

### Reopen conditions - a date, or any one trigger

Reopen RFC-0042 **on or after 2027-09-01**, or earlier if **any one** of these is recorded:

- **a named user or funder deliverable** requires a musl, static or unlisted-platform build that the
  C++ tail blocks. Both slice 6 runs searched independently and found zero; **one is enough**;
- **all five unbuilt roles pass their parity corpus inside a two-day box each**, at which point
  *"nothing has been built"* stops being an argument - it is doing most of the work in regression 1;
- **DataFusion ships checked, erroring integer/decimal overflow by default** (arrow-datafusion #17539
  closed), removing the number-one gate risk without a custom UDAF per aggregate;
- **RFC-0033 slice 4 (#357) is scheduled.** The ordering is asymmetric: swapping the engine *before*
  durable grafting wires in costs nothing, and *after* it costs one full recompute per derivation. **If
  #357 is going to land, reopen this question before it, not after.**

### What must happen anyway, whatever the engine

Neither item is blocked by this decision, and neither is a slice of this RFC.

- **Strike the false serialisation claim** wherever it was published, and fix the `DuckCache` doc
  comment in `src/analytics.rs` that caused it. *"queries take the mutex"* is true only of two map
  operations; two subsequent pieces of work read it as "queries serialise" and published that as a
  measured engine property.
- **Revisit `SQL_MAX_CONCURRENCY = 2`.** The knee measured on a 32-core box is nearer 8, worth roughly
  4.8x throughput, and the constraint that binds is the **per-cursor RAM budget** rather than the
  engine: unbounded at 32 clients reached 1,313 MB, 64% of one cursor's entire 2 GB. That is
  non-negotiable 2 territory and needs measuring on the surface that enforces it, not on a dev box.

## §13 - Readiness before slice 2 (was: what must be true before it starts)

**Amended 2026-08-29. The board unfroze RFC-0042 in full**, so these five stop being a permission gate
and become a readiness checklist. Condition 5 is discharged by that decision. The other four still
matter, because they are what stops slice 2 producing a number nobody should act on - and the point
below about momentum is now *more* live rather than less, since nothing external is holding the brake
any more.

**Status, closed 2026-08-30 by §14.** 1, 2 and 3 were met. **4 was never discharged** - the parity
corpus covers 7 of §6's shapes (#945), and hot+cold is among the missing, which is where COR-1's
disjointness invariant lives. A spike measured against a corpus that cannot see a chunk-seam defect is
a spike measuring the wrong thing. That gap is now part of the record rather than a task: §14 keeps
DuckDB, so no spike is pending, and the outstanding corpus work is stated in §14 as one of the reasons
the removal case rests on unbuilt things. **This checklist is closed.** It reopens only with the RFC,
on a §14 condition.

~~Added 2026-08-29, when the carve-out was taken. Slices 0 and 1 are covered by it; **slices 2 and
beyond are not**, and this section exists so that continuing is a decision somebody makes rather than
something that happens because the previous slice finished.~~ **Struck 2026-08-30 (#979).** This
paragraph predates the same day's full-unfreeze amendment at the head of this section and contradicted
it for a day: one sentence said slices 2 and beyond needed their own decision, the other said the board
had already taken it. It is struck rather than deleted because the thing it guarded against - momentum,
where *nobody ever decides to migrate a query engine, they simply find that they have* - is real, and
§14 is the decision it was asking somebody to make.

The failure this guards against is named in §11 - *purity becoming the objective* - and the shape it
takes in practice is momentum. Slice 1 ends with a working boundary and a parity corpus; the next
thing to do is obviously to plug something into it; and nobody ever decides to migrate a query engine,
they simply find that they have.

**Slice 2 is ready when the first four are true.** Starting before then is permitted now and still
unwise:

1. **The role inventory is complete and every role has a named owner.** Not §9's four - the six found
   in the call sites, including `graft.rs` writing the engine string into grafting identity and
   `entities.rs` deriving the admissible function vocabulary from `duckdb_functions()`. A role with no
   owner is a role nobody has costed.
2. **The BOM says what DuckDB actually costs**, per target: clean-build time, disk, output size,
   cross-compilation consequences. If it turns out to be a small share, §1's premise weakens and the
   board should hear that before anything else is spent.
3. **The noise floor is measured and stated**, with the method: peak RSS over a named window, segment
   layout recorded, and the corpus crossing every engine's internal batch boundary. A comparison
   against an unmeasured floor cannot support the word "regression".
4. **The parity corpus reproduces today's results byte-identically** through the new boundary. If the
   apparatus changes an answer before any engine is swapped, it is measuring itself.
5. ~~**The board records the decision**~~ - **done 2026-08-29**: RFC-0042 unfrozen in full, recorded
   in the CLAUDE.md carve-out list. Kept struck through rather than deleted, because the rule it
   states ("an approved RFC is not a carve-out until it appears in that list") is what made the
   decision necessary, and deleting it would erase why.

**What a "no" looks like, and why it is a result.** If the BOM shows DuckDB is a modest share of build
cost, or the role inventory shows the parser and function-vocabulary roles are expensive to replace
and product-visible, or RFC-0041 has shrunk the general-SQL surface far enough that the remaining
queries are cheap on any engine - then the honest outcome is **keep DuckDB, with the blockers
quantified**, and §11 already lists that among the useful answers. Recording it closes the question
for a year rather than leaving it to be reopened by the next person who reads a competitor's launch
blog.

## §12 - Appendix A: Tier 1 research, 2026-08-25

**Status: research input, not an RFC decision.** Commissioned to answer §5's candidate question for
Tier 1 (DuckDB-free) before slice 0 runs. It surveys the field and names the gates; it does not
discharge them. Every figure below that comes from a vendor or a project's own benchmark is a ceiling
rather than an expectation, and the caveat list at the end says which those are. §7's gate stands
unchanged: the decision is made on Nuthatch's own measurements, on Nuthatch's own corpus.

Recorded verbatim, because a research outcome edited into agreement with the RFC that commissioned it
is worth nothing.

### A.1 Summary

- **Tier 1 is achievable and the precedent is strong.** DataFusion (54.0.0, 29 Jun 2026) plus a custom
  hot-store `TableProvider` covers the analytical-SQL and hot+cold federation roles. Bauplan's
  Nov-2025 DuckDB→DataFusion production cutover - which they report **doubled query performance** on
  Iceberg lakehouses - is the migration precedent for exactly this "build-a-query-engine" case. The
  bounded RFC-0034 query surface makes DataFusion's remaining optimizer gaps (join ordering, subquery
  decorrelation) largely irrelevant, because worst-case plans can be excluded from the allowlist
  rather than optimised.
- **The two genuinely hard risks are exact-arithmetic semantics and the DBSP reference-oracle role.**
  DataFusion by default does *not* error on integer/decimal overflow (issue #17539, open as of DF 50);
  Nuthatch needs refuse-on-overflow for i128/Decimal token sums, which requires custom UDAFs and
  explicit config, validated on Nuthatch's own corpus. The `dbsp` crate is a fast-moving pre-1.0
  library (v0.338.0, 25 Aug 2026) with checkpointing and Z-set retraction but no stability guarantee.
- **Recommended path:** keep DuckDB as a *development-only* differential oracle through the migration
  (as §8 already allows), drive parity with the sqllogictest corpus plus DataFusion-SQLancer, and gate
  the cutover on a measured no-regression benchmark on the **concurrent-small-query API workload** -
  the dimension where public single-query ClickBench numbers are least representative.

### A.2 DataFusion as the general-SQL replacement

**Release state.** 54.0.0 shipped 29 Jun 2026 on a ~6-8 week cadence (50.0 Sep 2025, 51.0 Jan 2026,
52.0 Feb 2026, 53.0 Apr 2026). The 55.0.0 tracking issue (#22393) shows the late-2026 roadmap: a Sort
Pushdown EPIC (#23036), `GroupValuesColumn` nested-type coverage (#22715), Parquet virtual columns
(#22026, #22604), `MERGE INTO` (#22988). A planning-perf regression was caught between 52 and 53
(#21186) - the project self-benchmarks planning latency, which matters for an always-on server.

Performance work directly relevant here: dynamic filter pushdown (Pydantic/Logfire, ">10x" on their
`ORDER BY … DESC LIMIT 1000` pattern); LIMIT-aware Parquet row-group pruning and sort pushdown (DF
53/55: ~2× on full-scan ORDER BY, **27×-49× on LIMIT queries** that become streaming reads with early
stopping); filter pushdown through `UnionExec` and nested joins (DF 53); Parquet metadata caching (DF
50: `metadata_load_time` 229 ms → 229 µs, **82× on that specific query**); nested-field pushdown into
scans (DF 53); cheaper plan cloning (DF 53, ~4-5 ms → ~100 µs for parameterised queries).

**Hot+cold federation has battle-tested precedent** in InfluxDB 3/IOx and GreptimeDB: a custom
`RecordBatchesExec`-style provider for recent unpersisted data UNION-ed with a Parquet source, ordered
via a synthetic `__chunk_order` column and merged with sort-merge deduplication - all stock
`ExecutionPlan` composition. Predicate pushdown and pruning survive the UNION. GreptimeDB wired
DataFusion's runtime TopK dynamic filter down into its own scan layer ("From 29s to 0.21s", PRs
#7545/#7912) - a concrete template for pushing bounded-query optimisations into a custom store.

**Exact arithmetic is the sharpest semantic risk.** DataFusion supports Decimal128/256 and i128, but
default arithmetic **wraps**: `SELECT 10000000000 * 10000000000` returns a wrapped Int64 in DF 50
where Postgres and Trino error (#17539, open). That is a correctness problem for token-balance
aggregation. Related: #6828 (decimal division coercion overflows), #7497 (string→decimal cast
unchecked), #3498 (decimal divide-by-zero via float path), #7661 (conversion inconsistency vs
PG/Spark), #11832 (partial-aggregate skip type mismatch). Mitigation is custom UDAFs with checked
i128/Decimal semantics that **error** on overflow, with the allowlist constrained to those aggregates
- and measured against DuckDB on Nuthatch's corpus.

**Deterministic ordering.** DataFusion parallelises aggregation and scans across Tokio partitions, so
output order is not deterministic unless forced: an explicit final `SortExec` and, for ties, a
single-partition final stage. Cost is the loss of final-stage parallelism. For a view catalogue with a
defined canonical order, inject the sort into the authored view definition rather than relying on
incidental order.

**Cancellation, timeouts and resource bounds** are mature. DF 49 integrated Tokio task-budget
cooperative cancellation in all built-in sources; `CooperativeExec` plus an `EnsureCooperative`
optimizer rule make custom operators cancellable. Watch #19756 (rule not idempotent) and #16994
(`maintains_input_order` bug). Memory: `GreedyMemoryPool` / `FairSpillPool` with TrackConsumers,
per-query limits via `RuntimeEnv`. Spilling exists for sort and hash aggregation (#7400) and
multi-level merge sort (#15700), with sharp edges (#17334, #7858).

**SQL parsing.** `datafusion-sqlparser-rs` is syntax-only and applies no semantics - which suits the
allowlist approach exactly: parse to `Statement`, walk with the `Visitor` trait, refuse anything off
the list before planning. `DFParserBuilder` exposes `SqlParserOptions` and dialect (default
`GenericDialect`, recursion limit 50).

**Views.** `ViewTable` wraps a logical plan; nested views compose; `CatalogProvider`/`SchemaProvider`
back a catalogue; `information_schema` is supported. Maps cleanly onto the authored view catalogue
with canonical ordering baked into each plan.

### A.3 Migration engineering and parity

**Bauplan "Duck Hunt" (Nov 2025) is the key precedent.** They migrated an ephemeral SQL engine from a
**forked DuckDB - their only C++ dependency** - to DataFusion, defaulting `enable-df-query` to true.
Reported outcome: doubled query performance on Iceberg lakehouses "while enabling greater
hackability", with a date-filter optimisation worth "up to 20x". Motivations: DataFusion is designed
for *builders* (optimizer control, parser extension, deep Arrow integration) against DuckDB's
product-driven roadmap, and eliminating the sole C++ dependency. Method: feature-flagged new path,
**differential replay against tracked production SQL**, then cutover. The bulk of the effort was
reimplementing `EXPLAIN SCANS`; "the new code was relatively simple in comparison". They hit
case-sensitivity issues and Iceberg friction. They now run hundreds of thousands of pipelines across
four AWS regions on DataFusion.

**Divergences to encode as a differential checklist:**

| divergence | note |
| --- | --- |
| **Overflow** | DuckDB/PG error; DataFusion wraps (#17539). **Highest priority.** |
| Integer vs float division | verify truncation semantics; #6828 |
| NULL ordering | NULLS FIRST/LAST defaults differ - pin explicitly |
| Empty-result aggregates | COUNT→0 but SUM/AVG/MIN/MAX→NULL over zero rows |
| CAST/coercion | #7497, #6041, #7661; the `-128::tinyint` precedence bug |
| LIMIT without ORDER BY | nondeterministic in both - forbid, or always require ORDER BY |
| GROUP BY ordinal, collation | confirm case sensitivity (Bauplan hit this) |

**Parity infrastructure.** DataFusion ships `datafusion-sqllogictest`, has integrated SQLite's
sqllogictest suite into CI, and supports `pg_compat_*` files running the same script against
DataFusion and Postgres. DuckDB's own corpus is reusable (#4248). `datafusion-contrib/datafusion-
sqlancer` exists and has found DataFusion bugs via NoREC and other oracles; it can be pointed at the
allowlisted surface. (SQLancer's published bug totals are cross-DBMS aggregates, not DataFusion
counts.)

### A.4 Performance gap, honestly

DataFusion has held the top ClickBench spot for querying **raw Parquet** since Nov 2024. For sealed-
segment analytical queries that is a strength.

DuckDB still leads on join ordering, subquery decorrelation, TopN and window functions over
*arbitrary* SQL - advantages realised on unbounded shapes. RFC-0034's allowlist turns "DuckDB's
optimizer is smarter" from a performance risk into a query-admission policy decision.

**The under-measured dimension is concurrent throughput of many small queries** - an indexer serving
API traffic - not single-query latency. Public benchmarks are single-query-at-a-time. DataFusion's
Tokio execution spreads plan fragments across a worker pool, good for one big query, but per-query
planning cost and memory-pool contention dominate at N concurrent small ones. DF 53's plan-clone
speedups and the metadata cache help directly, but **this is the metric to measure first-party**: least
represented in public evidence, most representative of the serving load.

### A.5 DBSP for incremental entities and the oracle role

**Maturity.** v0.338.0 (25 Aug 2026), first release Aug 2023. **Pre-1.0** on an automated 1-3 day
release train - every 0.x bump is potentially breaking under Cargo semver - and docs.rs has failed to
build since 0.325.0 (last good 0.324.0, 23 Jul 2026). API stability is **not guaranteed**.

**Checkpointing.** `DBSPHandle::commit` writes a uuid-named checkpoint to a storage directory; restore
is via `CircuitConfig`/`StorageConfig` at circuit init. Operator state serialises via `rkyv`. Public
items include `circuit::checkpointer::Checkpointer`, `CheckpointCommitter`, `CircuitStorageConfig`,
`StorageOptions`. Fault tolerance is checkpoint plus input replay. Much of this lives in Feldera
*design docs* rather than hardened API guarantees.

**Retraction.** Z-sets carry integer weights (+1 insert, −1 retract). Reorg retraction is feeding the
inverse deltas, or restoring a prior checkpoint. There is **no user-facing "rollback N steps"
primitive**. This matches how Envio HyperIndex, The Graph and Materialize each handle reorgs.

**Numeric exactness.** Feldera forked `rust_decimal` as `feldera_rust_decimal` to reach 38-digit
precision (upstream caps near 28-29). Casts truncate toward zero; divide-by-zero and invalid-precision
casts are runtime errors. Native 256-bit is **not** built in - beyond i128/38-digit decimal, Feldera's
own docs show wrapping the external `i256` crate as a custom accumulator. For i128 blockchain sums
this is adequate and arguably better than DataFusion's default, being exact and erroring.

**Alternatives.** differential-dataflow/timely is the mature engine behind Materialize - more
battle-tested as a library, but a heavier and more idiosyncratic embedding. For a single binary `dbsp`
is the more natural embed, being library-first, and the less stable.

**Replacing the oracle role** without shipping DuckDB: frozen result corpora checked into the repo as
golden tests; DataFusion-batch versus DBSP-incremental as mutual cross-checks, encoding "incremental
result == batch recomputation over full history"; and property tests generating random insert/retract
sequences including reorg-shaped rollbacks.

### A.6 Serving and restart

`SessionContext` creation is cheap; the cost is registering many Parquet segments and reading footers.
DF 50's metadata cache is **in-memory only** (LRU, 50 MB default, invalidated on file change) and is
**not persisted across restarts**. Restart-to-ready for many sealed segments therefore needs an
application-level strategy - precomputed statistics/index files (the external-index pattern), catalog
snapshots, lazy footer reads. This is a real difference from DuckDB's attach-and-go and is a
first-party measurement target. Watch #19051 (Statistics Cache EPIC) and #17214 (ListingTable EPIC).

Precomputed DBSP entity state can be served directly from redb/Arrow without going through DataFusion
at all - a key lookup or range scan against materialised state, skipping planning. Precedent in
GreptimeDB (memtable/write-cache bypass) and IOx (ingester serves un-persisted RecordBatches).

### A.7 Role-by-role replacement map

| DuckDB role | Replacement | Confidence |
| --- | --- | --- |
| Analytical SQL over sealed Parquet | `ListingTable`/`DataSourceExec` + ParquetExec | **High** - ClickBench-leading, direct precedent |
| Exact i128/Decimal sums | custom checked UDAFs that error on overflow | **Medium** - DF wraps by default (#17539) |
| Hot+cold federation | custom redb `TableProvider` UNION Parquet | **High** - IOx/GreptimeDB precedent |
| Authored/nested views, canonical order | `ViewTable` + catalog + injected `SortExec` | **High** |
| Bounded SQL parse/canonicalise | `datafusion-sqlparser-rs` AST + `Visitor` allowlist | **High** |
| Incremental authored entities | `dbsp` (or differential-dataflow) | **Medium** - capable but pre-1.0 |
| Reference oracle | frozen corpora + DF-vs-DBSP differential + property tests | **High** - DuckDB dev-only |
| Finalised restart seeds | DBSP checkpoint (rkyv) or recompute from Parquet | **Medium** |

### A.8 Gate risks, ranked

1. **Exact-arithmetic divergence.** *High likelihood without action.* Custom checked UDAFs, allowlist
   constrained to them, every arithmetic path differential-tested against DuckDB on Nuthatch's corpus.
   **The number one gate.**
2. **Other SQL semantic divergences.** *Medium.* The checklist in A.3 as sqllogictest + pg_compat
   tests, plus Bauplan's tracked-query replay method.
3. **DBSP incremental correctness under reorgs.** *Medium.* Z-set retraction is sound but `dbsp` is
   pre-1.0 and thinly used as a library. Dual batch-vs-incremental invariant testing, property tests
   with reorg-shaped rollbacks, pin and vendor if needed.
4. **Concurrent small-query throughput.** *Medium, poorly measured publicly.* First-party benchmark
   before cutover.
5. **Restart-to-ready with many segments.** *Medium.* The metadata cache does not persist.
6. **One-binary intact.** *Low.* All candidates are pure Rust; confirm no accidental DuckDB link path
   survives behind the dev-oracle feature flag.

### A.9 Caveats - thin, contested, or needing first-party measurement

- **No public blockchain indexer embeds DataFusion as its serving engine.** Nuthatch would be somewhat
  novel; the deepest precedents (IOx, GreptimeDB) are time-series, not chain-reorg workloads.
  **Reorg plus incremental correctness is the least-precedented combination.**
- **Concurrent small-query throughput and memory under concurrency** - unmeasured publicly. Highest-
  value first-party measurement.
- **Restart-to-ready at Nuthatch's segment count** - the external-index strategy is documented, its
  effect here is unmeasured.
- **`dbsp` as an embedded library** - pre-1.0, 1-3 day release train, docs.rs failing, no known
  independent production embedders. Durability is a real bet, and it is a bet **RFC-0041 has already
  placed**, not one this RFC introduces.
- **Exact-arithmetic UDAF performance** - replacing native SIMD SUM with checked i128 may cost
  throughput. Unmeasured.
- **Bauplan's figures** - "doubled performance", "up to 20x" - are their own blog, directional and not
  an independent benchmark for this workload. The "82×" and "27×-49×" figures are DataFusion's own
  micro-benchmarks on specific queries. Treat every vendor and project self-benchmark as a ceiling.
