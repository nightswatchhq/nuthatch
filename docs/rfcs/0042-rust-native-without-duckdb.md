# RFC-0042: Can Nuthatch remove DuckDB and become Rust-native without sacrificing anything?

- Status: **Draft**
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
