# RFC-0042: Can Nuthatch remove DuckDB and become Rust-native without sacrificing anything?

- Status: **Draft.** Unfrozen by board decision 2026-08-25 - the second and last carve-out from the
  2026 feature freeze, after RFC-0041. **Sequenced behind RFC-0041**: no slice of this RFC starts
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

## §7 - No-sacrifice decision gate

Replacement requires parity; no lost capability; matching hot+cold semantics and guards; no material regression to material query, concurrent throughput, indexing, peak memory, startup/restart or real-workload result; release builds for each supported target; one-binary/no-service intact; and no DuckDB in the release tree. Expected wins include several of clean/incremental build, disk, binary size, cross-compilation, containers, CI and native toolchain requirements. If these do not materialise, removal needs stronger justification.

A close but slower Rust engine remains experimental. “Only 15% slower” fails if outside measured noise on a load-bearing workload.

## §8 - Removing the native tail

After a DuckDB-free candidate, rerun the BOM. If no C++ remains, Tier 2 is proved. Then apply the same gate to mimalloc, zstd, ring and Wasmtime support code: trace their exact activators; test allocator/compression/crypto/runtime alternatives only for no-regression results; preserve security, interoperability and component behaviour; and report assembly separately. A mature crypto primitive does not become weaker for an attractive README sentence.

## §9 - Interaction with RFC-0041

RFC-0041 assigns DuckDB parsing/canonicalisation, incremental-reference evaluation, finalised restart seeds and analytical entity serving roles. Keep these touchpoints behind boundaries. A winning Rust path uses its parser, frozen corpora or independent reference, production-engine seed and direct entity exposure. RFC-0041 semantics survive; the mechanism is not sacred.

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
