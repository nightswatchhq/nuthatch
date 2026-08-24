# RFC-0041: Authored incremental entities - make predefined computation pay at indexing time

- Status: **Proposed - design only.** No implementation under the 2026 feature freeze.
- Author: Jenny
- Date: 2026-08-24
- Depends on: RFC-0018 §1/§3 (authored SQL and the deferred hot-view promise), RFC-0013 §3
  (DuckDB hot+cold query surface), RFC-0033 (derivation keys and grafting), RFC-0026 (a dead view
  is a nest fault), and the existing DBSP balance/exposure/velocity circuits.
- Origin: GraphOps product feedback recorded on 2026-08-24. The objection was exact: a predefined
  view that is recomputed on every query gives the caller a name, but no query-performance benefit.
- Blocks: claiming that authored nest logic is an indexing-time advantage. Today that is true only
  of the three built-in IVM views, not of arbitrary `views/*.sql`.

## §0 - The correction

Nuthatch currently does two different things under the word *view*:

1. It indexes and persists decoded facts, then recreates authored `views/*.sql` inside a fresh
   in-memory DuckDB connection for each analytical query.
2. It maintains three hard-coded DBSP relations - balances, exposure and velocity - as blocks arrive.
   A reorg feeds the same inputs at weight `-1`; a restart reconstructs them from stored facts.

The first gives a nest reusable query definitions and a governed vocabulary. It does **not** move the
query's computation to indexing time. A `GROUP BY` over ten million transfers is still a ten-million-
row computation each time it is asked. The second is the product property the README and standing
architecture describe, but nest authors cannot use it.

This RFC closes that gap with an explicit, narrower construct: an **authored incremental entity**.
It is a keyed relation compiled to a DBSP circuit, maintained while indexing, retracted on reorg, and
served as a table. Ordinary views remain ordinary views. Nuthatch does not pretend all SQL can be
incrementalised safely merely because all SQL fits in a file.

The product statement after this RFC is honest and useful:

> Raw facts are indexed once. General SQL views are evaluated when queried. Entities explicitly
> admitted to the incremental subset are computed a little at a time as blocks arrive.

## §1 - Why this is a nest capability, not a query optimisation

The present architecture is comparable to loading decoded logs into a conventional analytical
database and saving named SQL views over them. The nest adds substantial value around that database -
ABI-bound decode, deterministic history, content identity, distribution, schema, semantics, checks,
sharing and a bounded serving surface - but predefining the view itself gives no latency advantage.

Incremental maintenance changes the unit of work. Instead of answering a request by rescanning the
history, each incoming window contributes a small weighted delta to an already-maintained relation:

```text
                         indexing time                         query time

decoded window -> +rows -> DBSP circuit -> keyed entity state -> lookup / small scan
reorg          -> -rows -------^                                  no raw-history fold
```

That is the stateful-query property subgraphs provide with imperative mappings, and the property AMP
provides by materialising authored definitions. Nuthatch's version stays declarative: the author says
what the relation is; the host owns state, ordering, replay and rollback.

This is also the first point at which RFC-0033's per-derivation reuse has stored output to reuse.
Before this RFC, changing an authored view could move an NID but could not invalidate materialised view
data, because none existed.

## §2 - Goals and non-goals

### Goals

- Let a nest opt one derived relation into indexing-time incremental maintenance.
- Preserve ordinary `views/*.sql` as the broad, low-risk analytical surface.
- Make backfill, tip ingestion and reorg use one weighted circuit, not three implementations.
- Make the entity queryable by key and as a relation without rescanning its source facts.
- Rebuild locally from already-indexed facts after a restart or definition change. A derived-state
  rebuild must never imply an historical RPC fetch.
- Bound every entity's memory and failure domain inside the existing per-cursor 2 GB budget.
- Give `nuthatch check` a reference implementation: the incrementally maintained result must equal
  DuckDB evaluating the authored SQL over the same finalized facts.
- Compose with RFC-0033 so unchanged entities can eventually graft independently.

### Non-goals

- Materialising every `views/*.sql` file automatically.
- Supporting all of DuckDB SQL in the incremental compiler.
- A second external service, JVM compiler, background database, or runtime download.
- A transparent result cache with expiry and invalidation rules.
- Persisting a second copy of every source event under a more fashionable table name.
- Partial-range reuse of holistic aggregates.
- Starting implementation during the 2026 feature freeze.

## §3 - The authoring contract

### 3.1 Two explicit classes of authored logic

The distinction lives in the filesystem and is therefore visible to a human, an agent, the NID and
the bundle manifest:

| Surface | Meaning | Execution |
|---|---|---|
| `views/*.sql` | General analytical view | Recreated and evaluated in DuckDB per query |
| `entities/*.sql` | Keyed incremental relation | Validated, compiled and maintained by DBSP |

An entity file contains **one `SELECT`**, not `CREATE VIEW` and not several statements. Its filename
is the relation name. Metadata belongs in a separate `entities.toml` because key and resource bounds
are part of the executable contract, not comments wearing a small hat:

```toml
[[entities]]
name = "delegations"
sql = "entities/delegations.sql"
key = ["delegator", "indexer"]
max_rows = 500_000
```

`name`, `sql`, `key` and `max_rows` are required. `entities.toml` is an authored input and therefore
moves the NID, but it is excluded from the decoded-fact identity just like `queries.toml`; putting the
declarations in `nuthatch.toml` would hash an entity-only edit as changed decode input and needlessly
forfeit fact adoption. There is no automatic promotion of an existing view. Moving a definition from
`views/` to `entities/` is a deliberate change in storage, memory, restart and failure semantics, and
the diff should look like one.

The first implementation supports one entity per file. This gives derivation identity an unambiguous
unit and makes a compiler error name the thing that failed.

### 3.2 An entity is keyed state

The declared key must be unique in the query result. A circuit emits inserts, deletes and updates for
that key. Nuthatch refuses a result that produces two live rows for one key; choosing one by arrival
order would import implicit row order into stored state.

The key is what makes the result useful without an analytical scan:

- `GET /derived/{entity}/{key...}` is a direct point read (`/entities` already names decoded event
  rows and does not acquire a second meaning);
- `/sql` exposes the current relation under its entity name;
- MCP schema and semantic surfaces identify it as `incremental`, including its applied-through block.

`max_rows` is an admission and runtime bound, not documentation. Crossing it faults that nest loudly
before the cursor admits more entity state. It must not degrade into an unbounded allocation and ask
the kernel to enforce the 2 GB limit with its customary lack of ceremony.

### 3.3 The v1 SQL subset

The initial subset is chosen for soundness and the Lodestar workload, not language completeness:

- deterministic projection and filtering;
- exact integer and decimal arithmetic;
- `GROUP BY` over declared keys;
- `count`, `sum`, `min`, `max`, and `avg` represented as sum plus count;
- inner equijoins where both inputs are indexed facts or earlier entities and the join keys are named;
- `CASE`, `COALESCE` and casts whose DuckDB and incremental semantics pass the parity gate.

Refused in v1:

- volatile functions and bare volatile keywords from RFC-0033;
- floating-point aggregation;
- `ORDER BY`, `LIMIT` and anything relying on implicit row order;
- window functions, recursive CTEs, correlated subqueries and arbitrary UDFs;
- `DISTINCT`, holistic aggregates (`median`, percentiles, `count(distinct ...)`) and outer joins;
- a dependency cycle;
- any expression for which the incremental evaluator cannot demonstrate DuckDB parity.

Refusal means “leave this as an ordinary view”, not “the nest cannot express this question”. The
general query surface remains the escape valve, which lets the compiler be conservative without
making the product brittle.

## §4 - Compilation without smuggling in a platform

Nuthatch embeds Feldera's Rust `dbsp` runtime. It does **not** currently embed Feldera's SQL compiler.
The fact that the Feldera platform can compile SQL does not establish that its compiler can ship
inside Nuthatch's one static Rust binary. The existing circuits are constructed by Rust code against
DBSP operators.

The chosen direction is therefore a bounded runtime plan builder:

1. Parse the entity `SELECT` with DuckDB's own parser, using the same serialized AST already used by
   RFC-0033 canonicalisation.
2. Bind table and column names against the live decode registry and earlier entity schemas.
3. Lower only the §3.3 subset into a small relational plan.
4. Build a DBSP circuit over Nuthatch's dynamic row representation. No generated native code, `cargo`
   invocation, JVM, network fetch or external compiler appears at nest load.
5. Store the canonical plan and compiler version in the derivation reuse key.

Using DuckDB's parser prevents a second parser disagreeing about syntax. It does **not** prove that
our evaluator agrees with DuckDB about expression semantics. The determinism/parity gate in §8 is
the proof, and an expression class remains unsupported until that proof exists.

### Slice-zero gate

Before accepting this direction for implementation, build a throwaway vertical spike for one real
Lodestar relation containing a filter, exact arithmetic, grouping and an equijoin. It must:

- compile from the authored SQL at runtime with no external executable;
- match DuckDB row-for-row over a fixed finalized corpus;
- accept `+1` and `-1` batches and converge after randomized reorgs;
- remain within the per-cursor RSS budget with the declared `max_rows` reached;
- keep release-binary growth measured and recorded;
- sustain at least the existing ingest-rate floor on the same recorded input.

If the dynamic DBSP plan cannot meet that gate, this RFC parks. The fallback is **not** to bundle a
JVM compiler, require a Feldera service, or ask users to install a Rust toolchain. That would solve a
query-performance problem by deleting Nuthatch's deployment model.

## §5 - Runtime lifecycle

### 5.1 Backfill and live indexing are one path

Each decoded window is converted to the circuit's input relations and applied at weight `+1`. The
circuit emits keyed result changes, which update the entity state. Backfill uses larger batches, but
not different semantics.

The entity becomes serveable only after its state reaches the dataset's advertised indexed head. A
catching-up entity is reported as such; it never serves a plausible partial relation as current.

### 5.2 Reorgs are retractions

The existing hot-store rollback already recovers the rows removed by a reorg. Those rows are fed to
every dependent entity at weight `-1` before being deleted, exactly as balances work today. Replacement
canonical rows arrive at `+1`. A random sequence of apply/retract operations must converge to a clean
replay of the surviving chain.

A circuit thread dying is a terminal fault for that nest under RFC-0026. Serving frozen derived state
as healthy is not graceful degradation; it is a lie with a pleasant HTTP status.

### 5.3 Restart and the first persistence boundary

The first slice follows the proven built-in-view model:

- canonical facts remain the durable source of truth in redb plus sealed Parquet;
- entity state is derived and may be rebuilt;
- on restart, DuckDB computes one finalized seed relation from sealed facts, then the circuit replays
  only the unsealed hot tail;
- the seed query is the same authored `SELECT`, restricted to the finalized range, and its output is
  checked against the entity schema before admission.

This pays the historical computation once per restart rather than once per request. It deliberately
does not introduce durable materialised snapshots in v1. If a real restart measurement says the seed
is too slow, immutable content-addressed entity checkpoints get a follow-up RFC. Writing mutable cold
state into sealed history is forbidden by the standing reorg rule.

### 5.4 Query serving

The maintained entity is available through direct keyed reads and as an input relation to `/sql`.
The analytical path may still copy or scan the **entity rows** into its ephemeral DuckDB connection;
it must not evaluate the entity's defining aggregation or join over raw history again.

That distinction matters. A query returning all 500,000 maintained delegations still has to move
500,000 rows. IVM removes the ten-million-event derivation beneath them; it does not repeal I/O.
Existing SQL row, byte, time and concurrency guards continue to apply.

## §6 - Identity, upgrades and grafting

RFC-0033 currently has two identity axes. Stored entities require three:

| Identity | Covers | Used for |
|---|---|---|
| NID | every authored input | package, mounts, versioning |
| fact identity | inputs that determine decoded canonical facts | adopting redb/segments without RPC |
| entity reuse key | canonical entity plan + input keys + compiler/runtime version | reusing one entity's state |

`Manifest::data_identity()` is today's fact identity, although its name predates this distinction.
`entities.toml` and `entities/**` must be added to its non-data-input exclusions: they affect derived
state, never decoded canonical facts. Editing an entity definition must therefore reuse all decoded
facts and rebuild locally. Conversely, entity state must never be adopted merely because the fact
identity matches.

Each entity key is the RFC-0033 derivation key extended with:

```text
entity_key = H(
    entity_cache_format_version
  || canonical_incremental_plan
  || [entity_key(input_entity)]
  || source_identity(input_fact_table)
  || key_columns || output_schema
  || incremental_compiler_id || version
)
```

The first implementation may rebuild all entities locally after an NID change. The acceptance bar
still requires zero historical RPC. A later slice activates RFC-0033's deferred whole-derivation
reuse: unchanged keys graft, the changed node and its descendants rebuild, and unrelated entities do
not. That slice only becomes testable once entity output is persisted; until then there is still
nothing durable to graft.

## §7 - Resource accounting and isolation

An entity consumes circuit state, output state and a worker thread. It is therefore included in mount
admission exactly like the existing IVM views, against the **shared per-chain cursor** budget rather
than a fictional per-nest allowance.

Required controls:

- authored `max_rows`, validated before activation and enforced while running;
- a measured per-row estimate plus circuit fixed cost in admission accounting;
- a runtime ceiling on total incremental entities per cursor;
- no circuit for an entity nobody declared;
- entity health and applied-through block in `/nests`, `/ready` and metrics;
- quarantine of the faulty nest, never the whole cursor, unless the cursor's own invariant failed.

The spike records RSS at the declared bound. “DBSP can spill” is not a memory measurement.

## §8 - Correctness and acceptance

Every entity ships two implementations by construction: DBSP maintains it; DuckDB evaluates its SQL.
That is useful only if CI makes them disagree loudly.

### Static checks

- exactly one `SELECT` per file;
- declared key exists, is non-null and unique in the reference result;
- dependencies form a DAG;
- every operator and expression is in the supported subset;
- no volatile construct from RFC-0033;
- `max_rows` is present and non-zero;
- the declared output schema matches both engines.

### Deterministic simulation

For every entity in a checked nest:

1. Evaluate the SQL in DuckDB over a fixed finalized fixture.
2. Feed the same facts through the DBSP circuit in several batch partitions.
3. Compare keyed results byte-for-byte after canonical ordering.
4. Repeat with randomized apply/retract/replacement sequences.
5. Restart from the finalized seed plus hot replay and compare again.

Different batching, window sizes, concurrency and restart points must produce the same result. Float
aggregation remains refused because a parity test over one ordering does not make addition associative.

### Product acceptance on Lodestar

Choose one Lodestar panel whose current authored view scans historical event tables. Against the same
real dataset:

- results match the old SQL view exactly;
- one new block performs work proportional to that block's affected rows, not historical row count;
- the panel query no longer scans the raw source tables;
- p50/p99 latency and bytes scanned are recorded before and after;
- a restart reconstructs the relation without RPC;
- editing that entity causes zero historical RPC and leaves an unrelated entity available;
- a randomized reorg run converges to clean replay.

The criterion is a measured disappearing scan, not merely a route returning `200`. A fixture with a
dozen transfers can prove semantics and almost nothing about the product objection that created this
RFC.

## §9 - Slices

| # | Slice | Ends with |
|---|---|---|
| 0 | **Compiler and budget gate.** One real Lodestar query lowered from DuckDB AST to a dynamic DBSP plan. | Exact DuckDB parity, random-reorg convergence, ingest floor and RSS/binary measurements. Failure parks the RFC. |
| 1 | **Authoring and validation.** `entities/*.sql`, `entities.toml`, schema/dependency/refusal reporting in `check`. | An eligible entity is accepted; every unsupported construct names the reason and remains expressible as a normal view. |
| 2 | **One lifecycle.** Backfill/tip `+1`, reorg `-1`, restart seed plus hot replay, health and resource admission. | A killed/restarted/reorged runtime converges to the reference result without serving stale state as healthy. |
| 3 | **Serving.** Keyed entity reads, `/sql` relation, schema/MCP/semantic exposure and provenance. | The Lodestar panel reads maintained state and its raw-history scan disappears in a captured plan. |
| 4 | **Per-entity grafting.** Persisted entity output and RFC-0033 whole-derivation reuse, designed in a follow-up once restart measurements justify persistence. | Edit one of several entities: unchanged siblings graft, descendants rebuild locally, historical RPC count is zero. |

Slices 1-3 are new capability and remain behind the 2026 freeze. Slice 4 is deliberately not smuggled
into the first implementation: persistence has its own corruption, atomicity, collection and migration
surface, and should be paid for only after the in-memory model proves product value.

## §10 - Risks

**Wrong incremental semantics silently serve wrong state.** This is the largest risk. The answer is a
narrow subset plus continuous DuckDB-reference parity, not confidence in the phrase “relational
algebra”. Outer joins and holistic aggregates stay out until their retraction semantics are proved.

**State cardinality breaks the cursor budget.** A group key controlled by users can grow without
bound. Required limits, admission accounting and runtime enforcement make that an explicit refusal.

**Startup merely moves the expensive query.** V1 recomputes the finalized seed once per restart. That
is still a large improvement over once per request, but it must be measured. Durable checkpoints are a
follow-up if and only if the measured restart cost warrants their lifecycle complexity.

**Two view classes confuse authors.** The distinction is semantic and visible: `views/` are general
questions, `entities/` are bounded maintained state. `nuthatch check` explains why a view is or is not
eligible; it never silently changes execution class.

**The compiler becomes a second SQL engine.** It does, for a small subset. DuckDB remains the parser
and reference evaluator, and unsupported syntax is refused. Trying to keep pace with all DuckDB SQL
would be a category error and should be rejected as such.

## §11 - Alternatives

### Materialise every authored view

Rejected. Some views are volatile, unbounded, order-dependent or simply larger than their sources.
Automatic materialisation turns a harmless query into mandatory indexing state without an author or
operator choosing its cost.

### Persist query results as a cache

Rejected as the foundation. Cache invalidation across new blocks, reorgs, view dependencies, engine
versions and NID changes recreates the derivation problem with weaker semantics. IVM states how output
changes from input changes; a cache merely notices later that it was stale.

### Embed Feldera's complete SQL compiler or require a Feldera service

Rejected unless slice zero disproves the present constraint. An external compiler/service violates the
single-binary and offline contracts; a JVM hidden beside the binary is still a JVM.

### Generate Rust and compile it when a nest mounts

Rejected. A production runtime does not require Cargo, a writable build cache and arbitrary native
code compilation merely to load authored data. It also destroys the under-two-minute path.

### Add more hard-coded recipes

Useful for common protocol state and already shipped, but not a general authored logic layer. It makes
the maintainer the compiler and every author wait for a release.

### Persistent DuckDB catalogue and prepared-plan cache

Orthogonal. It could reduce per-query connection, DDL and binding overhead, but it does not remove the
historical aggregation or join. Profile it separately; do not present workshop tidying as incremental
materialisation.

## §12 - Open questions

1. Which Lodestar relation is the slice-zero dogfood: delegations, allocations, or an epoch aggregate?
   Choose by captured scan cost, not by which SQL is prettiest.
2. Is `max_rows` enough for admission, or must authored metadata also bound bytes per row? The spike
   supplies the measurement.
3. Can `/sql` consume maintained entity state without copying every entity row into each ephemeral
   DuckDB connection? This affects serving cost, not whether the derivation is incremental.
4. Does a real restart justify immutable entity checkpoints? If yes, specify them in a separate RFC
   together with atomic commit, verification, pruning and grafting.
5. Should `Manifest::data_identity()` be renamed to `fact_identity()` in the next breaking release,
   or retain the name and document the now-sharper meaning?
