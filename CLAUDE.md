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
  every query gives the caller a name but no query-performance benefit. It was built under the 2026
  feature freeze as the first of five carve-outs, and the ordered sequence in §9 is complete (#818,
  #820, #821, #822). The freeze itself ended on 2026-09-08; see the build-order status below.
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

> **Status 2026-09-08: the 2026 feature freeze is over. Chief lifted it, in full, on 2026-09-08.**
> Slices 1-5 are shipped. The freeze ran from 2026-08-20 and did what it was written to do: every
> defect worth finding that quarter came from running the product rather than extending it. It ends
> because there is now a body of design worth building, not because the discipline failed.
>
> **The carve-out mechanism is retired with it.** All five carve-outs were taken and all five are
> spent: RFC-0041 (authored incremental entities, shipped 3.0.0-alpha), RFC-0042 (the no-DuckDB
> investigation, closed KEEP DuckDB at §14), RFC-0051 (Monad), RFC-0050 (Robinhood Chain) and
> RFC-0040 (the freshness dial, shipped 3.5.0). Their reasoning lives in those RFCs and in the
> release notes; it is not repeated here. Nothing needs a carve-out any more, because nothing is
> frozen that a decision has not separately deferred.
>
> **What the unfreeze is for.** One programme, chosen the day the freeze lifted: **RFC-0044 through
> RFC-0048, in full.** The subgraph port skill, offchain data, x402 at the counter, the lakehouse
> commitments, and pricing query access. They are one argument in five documents - the first RFCs
> about what nuthatch guarantees to people who are not running it - and each proposes specifying
> behaviour that already happens rather than inventing a mechanism. Every one of them leads with the
> non-negotiable it appears to break and shows why it does not; **those arguments are load-bearing
> and none of them is now waived.** RFC-0046 §1's test in particular is a build gate, not a
> paragraph: *delete every payment feature from the tree and a self-hoster loses nothing.*
>
> **RFC-0053 is accepted, and this line is the record RFC-0044 §8 asks for.** Chief accepted
> [RFC-0053](docs/rfcs/0053-graph-subgraph-compatibility.md) on 2026-09-09, a Graph-dialect-to-SQL
> compatibility surface so a caller adopts a nest by changing a GraphQL URL. **Rescoped 2026-09-12:
> that last clause did not survive contact with a measurement. What was built is a partial read
> surface - exact for a subgraph's event-shaped fields, a named refusal for the rest - and an
> analytics client cannot adopt it unmodified, because GraphQL refuses a whole query for one
> unanswerable field. Do not describe this as a drop-in replacement anywhere; RFC-0053's Measured
> outcome has the figures and the rescoped deliverables.** It is **new binary
> capability**, which is exactly the case RFC-0044 §8 says must be decided here or not happen, and it
> **overrides RFC-0044 §11's "not a subgraph compatibility layer" non-goal**, annotated in that RFC.
> It sits outside the programme's original five and is now the sixth document in it.
>
> Three things the acceptance does not do, because they were the grounds for §11 in the first place.
> It does not run AssemblyScript, in the indexing path or anywhere else; RFC-0038 §8 still holds.
> It does not promise byte-for-byte parity for the ordered, mapping-derived family - `derivedETH`,
> `volumeUSD`, `totalValueLockedUSD` and their relatives are a **compiler over stored state, not a
> second indexer**, and RFC-0038 §6a's finding is unchanged. And it does not reopen arbitrary SQL or
> RFC-0034's resource limits merely because a request arrived as GraphQL. The compatibility surface
> is a read surface; nothing about it touches non-negotiable 4, because no LLM and no query shape
> feeds stored state.
>
> **The authorisation is scoped to the evidence.** RFC-0053 S0 (#1264, the migration validator) is in
> the `resolute-robin` sprint. S1 to S4 (#1265 to #1268) are filed and wait on what S0 measures,
> because S0 is the only slice that can falsify the others. Accepting the direction is not accepting
> the build-out sight unseen.
>
> **What is still deferred, and stays deferred.** Lifting the freeze is not a blanket reopening.
> `docs/frozen-for-2027.md` stands unchanged, with its own rule: reopen one item at a time, naming
> the new demand or evidence and an acceptance criterion that can fail. Chief separately deferred
> RFC-0003, RFC-0023, RFC-0031, RFC-0033, RFC-0034 and RFC-0036 again on 2026-09-08, and closed
> RFC-0013 and RFC-0021 the same day. Slice 6 below (ExEx, scaled mode) is not started. RFC-0042 is
> parked to 2027-09-01 or a §14 trigger, and its fourth trigger still binds: **if RFC-0033 slice 4
> (#357) is ever scheduled, reopen RFC-0042 before it, not after.**
>
> **The out-of-scope list below is unchanged and still binds.** No hosted service, no token, no
> non-EVM before EVM is airtight, no TEE or zk, no Kubernetes. The freeze ending widens what may be
> built; it does not widen what the product is.
>
> **On the apparent conflict between that list and RFC-0046 / RFC-0048**, raised in review of the PR
> that made this change and settled here rather than in an RFC a reader has to go and find. The list
> forbids **us** running a hosted service and **us** billing and metering customers - the
> become-a-data-service-company path - and per-tenant billing and authz remain the gateway's job. It
> does not forbid an operator charging at their own endpoint. **That is the same line the tenancy
> paragraph above already draws:** what nuthatch does about tenants, not who they are. The Nuthatch
> Data Service has answered `402 TAP-Receipt header required` since long before this programme and
> nobody read it as a violation, because the paywall is a deployment an operator chose rather than a
> property of the binary. x402 is a second payment method for the same choice, and the §8 governance
> question was put to Chief on 2026-08-30 and answered: cross the line deliberately.
>
> **What keeps that distinction honest is a test, not an assurance**, and it gates the whole payment
> half: *delete every payment feature from the tree and a self-hoster loses nothing; enable one and
> the binary still runs, unpriced, for anyone who did not.* A price that cannot be turned off, a
> settlement path the binary requires to start, or a key we hold has crossed from operator choice
> into gated product and violates §3. #1217 exists to fail if it ever does.

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
