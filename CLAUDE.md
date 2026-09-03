# Nuthatch - CLAUDE.md

Nuthatch is a self-hosted-first, AI-native blockchain indexer. One Rust binary, one command,
live indexed API in under two minutes. No mandatory third-party data dependency, ever.
Tagline: "be your own indexer."

This file is the standing brief. Read it before any task. When a task conflicts with the
non-negotiables below, stop and flag it instead of proceeding.

## Non-negotiables

1. **Single static binary** is the primary deliverable. Embedded mode must run with zero
   external services: no Postgres, no Docker, no IPFS. `curl | sh` → `nuthatch init 0xAddr
   --chain mainnet` → `nuthatch dev` → live API. Target: <2 minutes to first indexed query.
2. **Footprint budget: ≤2 GB RAM per active-chain cursor** - one chain's tip-following +
   serving in embedded mode, whether that cursor hosts one nest or several. A single-chain
   runtime is one cursor (≤2 GB); a multichain runtime's total is Σ cursors (RFC-0021). The budget
   is per-cursor and shared across the nests on that cursor - density is RAM-bounded, not free.
   Treat this as a CI-enforced budget (per cursor), not an aspiration. If a design decision
   threatens it, surface the tradeoff before implementing.
3. **No phone-home.** No telemetry, no mandatory API tokens, no gated data services. AI
   features use local models (Ollama) or BYO API key, and degrade gracefully offline.
4. **Determinism in the core.** ABI decoding, reorg handling, entity derivation, and anything
   feeding stored state must be deterministic and re-executable. LLMs generate code and tests;
   LLM output never sits in the runtime data path.
5. **`MIT OR Apache-2.0`** for the core - the maximally permissive option, and the Rust-ecosystem
   norm. Anyone may use, modify, embed or resell it, including in closed products. *(Relicensed from
   AGPL-3.0 on 2026-07-28. This was a deliberate trade: copyleft was the only thing preventing a
   hosted competitor from closing and reselling nuthatch, and that protection was given up in
   exchange for maximal adoption and zero friction for embedders.)*
   **The dependency rule is now stricter, not looser:** we can no longer consume GPL/AGPL code at
   all, so do not vendor or port from copyleft projects we don't own (notably SQD's worker-rs) -
   read for ideas only. Safe dependencies: reth (MIT/Apache), Cryo (permissive), Feldera/DBSP
   (MIT OR Apache-2.0), DataFusion/Arrow/DuckDB (Apache-2.0). Do NOT add Materialize (BSL) or any
   Envio/HyperSync dependency. `deny.toml` enforces this in CI.

## Architecture (two modes, one codebase)

**Embedded mode (default):** single process. Ingestion (RPC extraction with aggressive
batching, Cryo-style; optional reth ExEx when colocated with a node) → deterministic decode →
hot tip store (redb) for entity point-reads → sealed content-addressed Parquet segments past
finality → DuckDB attaching segments **read-only** for analytical SQL. DuckDB is single-writer:
only the ingestion thread writes; queries attach read-only. Never design around concurrent
DuckDB writers.

**Scaled mode (docker-compose):** same crates, Postgres replaces redb for the hot store,
DataFusion federates hot + cold behind one SQL surface. Feature-flag the storage backend
behind a trait; no `#[cfg]` forks of business logic.

**Multi-nest tenancy (in the runtime):** one runtime hosts **N nests**, across **one or more
chains**, running **one isolated cursor per distinct chain** (RFC-0021) - each cursor with its
own finality view and reorg boundary. A single nest is simply N=1; there is no separate mode to
opt into and no container to declare.

**A nest's data is keyed by its content address (NID); a mount is keyed by (tenant, NID).** A
*tenant* is an **opaque string** that labels a mount and never touches the data layer - it exists
so one operator can host nests on behalf of several parties. Two tenants mounting the same nest
share one dataset: it is **never indexed twice**, and deleting one mount decrements a reference
rather than destroying data someone else is using. Because the NID is a true content address, any
edit yields a different nest, so divergence forks its own data automatically and cannot
contaminate a shared one.

Strict per-nest **and per-cursor** isolation of storage, reorg, and blast radius: one nest's bad
view or runaway factory, or one chain's stall or reorg, must not harm another. The single-cursor
law holds **per chain**: a cursor is always single-chain, single-writer, one observable failure
boundary - never multiplex two chains behind one cursor. Multichain in one runtime is a
**capability, not a mandate**; one chain per runtime stays valid and is the default. A second
chain means a second cursor - in the same runtime or on another worker (the distributed pool,
RFC-0022) - but never a second chain behind one cursor. See RFC-0012, RFC-0021.

> **Status (2026-08-13): shipped, verified against v2.2.0.** The roost is retired: there is no
> `roost` subcommand, and `nuthatch dev --dir <dir>` runs one nest or many depending on what the
> directory holds - a `nuthatch.toml` or a `mounts.toml` (RFC-0032). Data lives at `data/<nid>/`,
> a mount record carries `tenant`, `alias` and `nid`, two mounts may share one nid, and `nuthatch
> prune` is what reclaims a dataset nothing mounts any more - unmounting one of two mounts leaves
> the data alone. `nuthatch migrate` moves a pre-2.0 directory across. Tenants may
> now be described as shipped, with the caveat the paragraph above already states: an opaque label
> nuthatch refcounts and knows nothing else about. `docs/rfcs/0032` is the 2.0 shape; `0012` +
> `0021` + `0027` are how it got here.

**Reorg strategy:** reorgs only ever touch the mutable hot store - and only that of the
affected chain's cursor, isolated from other cursors in the same runtime. Segments are sealed to
Parquet strictly past finality, so the columnar layer is append-only and immutable. If a
change requires mutating sealed segments, the design is wrong - go back.

**Entity derivation.**
- Built-in IVM (shipped): `balances`, `exposure`, and `velocity`, maintained by DBSP. Reorgs
  are retractions; backfills are batch runs of the same circuit.
- Authored SQL (shipped, RFC-0018 §1): `views/*.sql` are named queries evaluated at request
  time over hot ∪ sealed. Not incremental.
- Authored incremental entities: [RFC-0041](docs/rfcs/0041-authored-incremental-entities.md),
  **shipped 2026-08-28** in 3.0.0-alpha, off the back of GraphOps feedback that a view recomputed on
  every query gives the caller a name but no query-performance benefit. The 2026 feature freeze was
  lifted for this work specifically and for nothing else, and that carve-out is now spent: the
  ordered sequence in §9 is complete (#818, #820, #821, #822) and the freeze applies again in full.
  An entity is declared in `entities.toml`, maintained by DBSP as blocks arrive, served from
  `/derived` and by name from `/sql`. Slice 3's criteria were measured against a copy of the real
  Lodestar nest: the panel it replaces went p50 2.15 s to 87.7 ms, and one block's update is flat at
  ~285 µs against 309,548 groups. RFC-0033's durable grafting (#357) is **not** in v1 - per-entity
  reuse across an NID change is still a whole-nest local rebuild.
- Imperative (escape hatch): WASM component handlers, per the transform layer below.

## The transform layer: lessons from liminal (nightswatchhq/liminal)

Liminal is the prototype for Nuthatch's transform runtime. Study `liminal-host/`, `wit/`, and
`liminal-sdk/` before writing any transform-layer code. Port the design, not just the idea.

**Adopt directly:**
- WIT-first workflow: define/modify WIT interfaces before touching host or component code.
  Typed channels between stages; the WIT files are the API contract and get reviewed first.
- Per-component capability injection at composition time. The host grants `wasi:http`,
  key-value, filesystem per component, never per pipeline.
- **Purity by construction:** a component granted zero capabilities is deterministic by
  definition. Enforce the rule in the host: only zero-capability components may feed entity
  derivation / stored state. Effectful components (HTTP enrichers etc.) produce annotations
  only, never canonical entities. Purity must be checkable from the composition manifest -
  no code inspection required.
- Single cursor, single process, one observable failure boundary. Never introduce a second
  cursor or a reconciliation layer.
- Host owns orchestration, retries, and state; components are stateless pure stages.
- Optional sinks warn-and-skip when unconfigured (liminal's `--database-url` pattern) - apply
  this graceful-degradation pattern to every optional integration.
- Examples-as-documentation: every capability ships with a runnable example pipeline, in the
  style of liminal's `examples/uni-v3-swaps`.
- Wasmtime pinned, WASIp2 (`wasm32-wasip2`) now; track WASIp3 but do not adopt until stable
  in Wasmtime. Keep WIT interfaces p3-migratable (avoid patterns that only make sense in p2).

**Change from liminal (its known gaps for this workload):**
- **Batch the boundary.** Liminal's per-event component calls won't survive backfill targets
  (≥10K events/sec floor, aim 30K). WIT interfaces take batches - lists of events or
  serialized Arrow IPC buffers - never one event per call. Arrow is the interchange format
  everywhere; don't invent bespoke serialization.
- **Stateless components as a hard contract:** components are pure functions
  `batch of blocks → batch of facts`. All state lives host-side. Components never see reorgs
  and have no rollback interface; the host handles reorg via hot-store rollback and IVM
  retractions.
- Components are the escape hatch, not the front door: the `init` flow must produce a working
  indexer with zero user-written components (generated decode + declarative views).

## Correctness rules

- Decode: deterministic Rust, topic0-keyed, contract-ABI priority with generic fallback.
  ABI acquisition: Sourcify first, then Etherscan-class APIs. Cache ABIs locally.
- Never retroactively re-decode stored history when ABIs improve; version decodings.
- Golden/deterministic-simulation tests for every handler and view (Matchstick lineage):
  fixed block fixtures in, exact entity state out. AI-generated tests are welcome, but they
  must be deterministic and reviewed like any code.
- Property tests for reorg handling: random reorg depths against the hot store must always
  converge to the canonical chain state.
- Benchmarks are CI artifacts: backfill events/sec, tip lag ms, entity point-read p50/p99,
  RSS. Regressions fail the build.

## AI-native surface (built-in, sovereignty-respecting)

- MCP server compiled into the binary: schema discovery, SQL execution, entity lookup,
  streaming subscribe. Works fully offline against the local instance.
- `nuthatch init 0xAddr` scaffolds schema + views + handlers + tests from the ABI.
- Ship `llms.txt`, docs-as-MCP, and a `.claude/skills/` directory in scaffolded projects so
  coding agents get real syntax instead of hallucinating.
- Local-first AI: Ollama support and BYO-key. Any AI feature must have a documented
  no-network fallback or be clearly marked unavailable offline.

## Build order (vertical slices; each ends runnable)

> **Status 2026-08-20: slices 1-5 are shipped, and a feature freeze runs to the end of 2026.**
> `docs/roadmap-2027.md` is the standing direction: no new capability this year, only bug fixes,
> security, performance, maintenance, marketing, and making the delightful core (RFC-0015) best in
> class. Slice 6 (ExEx, scaled mode) and the parked RFC work are **not cancelled** and not to be
> started. Treat a proposal for new capability the way the out-of-scope list below is treated: say
> so, rather than quietly building it. The list survives as the record of how the product was built.
>
> **Three carve-outs, and only these three.** A carve-out is a decision Chief makes explicitly and
> records here. It is not a precedent for the next proposal, and an approved RFC is not a carve-out
> until it appears in this list.
>
> 1. **RFC-0041, authored incremental entities (2026-08-24).** After GraphOps identified query-time
>    view recomputation as a product gap. Conditions in the entity-derivation section above.
> 2. **RFC-0042, the Rust-native/no-DuckDB investigation (2026-08-25).** Was **sequenced behind
>    RFC-0041** - no slice until the entity work was done, because RFC-0042 §9 hands DuckDB four
>    roles inside RFC-0041 (parser, incremental reference, restart seed, entity serving) and moving
>    the engine while those roles were still being assigned would have made both unattributable.
>
>    **That condition is met: RFC-0041 shipped 2026-08-28.** The carve-out is **taken as of
>    2026-08-29, for slices 0 and 1 only** - the native bill of materials and role inventory (#935),
>    and the engine boundary plus parity corpus with DuckDB unchanged (#936). Sprint
>    `docs/sprint-exacting-egret.md`. **The product is byte-identical when those two close; nothing
>    is replaced.**
>
>    **Amended 2026-08-29: RFC-0042 is unfrozen in full**, board decision, taken with slices 0 and 1
>    complete and their findings on the table. It is the work that follows sprint `exacting-egret`.
>    Slices 2 to 6 - the DataFusion spike, the Turso spike, the composed path, the decision and the
>    native tail - no longer need a further carve-out.
>
>    **What does not change is the decision rule.** §0: *"There is no preferred answer. If evidence
>    says DuckDB remains best, it stays."* Unfreezing the work is not a decision to remove DuckDB, and
>    §7's no-sacrifice gate stands unaltered. **"Keep DuckDB, with these measured regressions" remains
>    a successful outcome**, and slice 0 already found evidence pointing that way: DuckDB is 10.6% of
>    clean build time while wasmtime and cranelift are 21.3%, so §1's premise that it *dominates* build
>    time is measured false, even though it is 93% of native artefact bytes.
>
>    §13's five conditions stop being a permission gate and become a **readiness checklist**. Four are
>    met; the outstanding one is the parity corpus (#945), which covers 7 of §6's shapes. A spike run
>    against a corpus that cannot see a chunk-seam defect would produce a number nobody should act on.
>
>    **CLOSED 2026-08-30. The decision is KEEP DuckDB, and this carve-out is spent.** Written as
>    **RFC-0042 §14**, on the six measured regressions listed there, at 78% confidence. Per §0 that is
>    one of the two admissible answers, not a failure to reach the other. The RFC is **parked, not
>    withdrawn**: §14 carries a reopen date of **2027-09-01** and four triggers, any one of which
>    reopens it earlier. Note the fourth especially - **if RFC-0033 slice 4 (#357) is ever scheduled,
>    reopen RFC-0042 before it, not after**, because swapping the engine before durable grafting wires
>    in costs nothing and after it costs a full recompute per derivation.
>
>    **Both carve-outs are now spent, and the 2026 feature freeze applies again in full.** A third was
>    taken on 2026-09-03, below, for one chain. A proposal to resume RFC-0042 work is a proposal for
>    a new carve-out and needs a §14 reopen condition recorded first. Two items escaped the park
>    because they are corrections and performance rather than capability, and §14 names them: the
>    false-serialisation correction, and revisiting `SQL_MAX_CONCURRENCY`.
>
> 3. **RFC-0051, Monad as a built-in chain (2026-09-03).** Chief's decision, recorded the day it was
>    made, tracking #1136. The freeze was lifted for **this one chain and nothing else**: a registry
>    entry in `src/chains.rs` on the generic EVM path, its measured endpoints, the operator note, the
>    live-endpoints probe, and the two over-wide-range refusal shapes its public endpoints answer with.
>    The execution-lag guard the RFC draft proposed was **not built**, on evidence in the RFC's
>    addendum: the RPC layer only serves executed blocks. RFC-0050 (Robinhood Chain) is **not** carved
>    out by this and stays a frozen draft. Spent when #1136 closes; there is no fourth.

1. Skeleton: single binary, config, `init` (ABI fetch → generated project), RPC ingestion,
   decode, redb hot store, HTTP serving of entity point-reads. One chain (Ethereum). This
   slice alone must hit the <2-minute demo.
2. Parquet sealing past finality + DuckDB read-only analytical SQL + reorg property tests.
3. DBSP declarative views (the IVM core) replacing hand-rolled entity updates.
4. Transform runtime ported from liminal with batched Arrow WIT interfaces.
5. MCP server + scaffolded skills + llms.txt.
6. ExEx ingestion mode (colocated reth), then scaled mode (Postgres/DataFusion).

Do not start slice N+1 while slice N has failing tests or an unmet budget.

## Out of scope - do not build, do not suggest

- Hosted service, billing, metering, **hosted-SaaS multi-tenancy** (per-tenant authz/quotas,
  isolation between mutually-untrusting paying customers - that's the become-a-data-service-
  company path, and the gateway's job regardless). Note: **multi-nest tenancy in the runtime**
  (a tenant is an opaque ownership label plus refcounting - no identity, no authn, no quotas, no
  metering) and *distributed **self-hosted** scaled mode* (one operator's writer pool + query-FE
  tier + control-plane, RFC-0022) are both **in scope** - see Architecture. **The line is what
  nuthatch does about tenants, not who they are** (amended 2026-08-04): it sees a string, refcounts
  it, and knows nothing else, so an operator's tenants may well be paying customers and nuthatch
  has no concept of it. Per-tenant billing and authz stay out and are the gateway's job. The
  earlier wording drew the line at "cooperating tenants an operator picked - not paying strangers",
  which asked a question nobody could answer from the code.
- Token, staking, decentralized network features (a possible future Graph Horizon data
  service is explicitly deferred).
- Non-EVM chains before EVM is airtight.
- TEE attestation, zk proofs (verifiability = deterministic re-execution of pure components
  + content-addressed segments; nothing heavier).
- Kubernetes manifests, Helm charts, or any deployment story beyond binary + compose.
