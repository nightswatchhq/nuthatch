# RFC-0058: Cross-nest SQL - a declared, read-only query across mounted nests in one runtime

**Status:** **Accepted 2026-09-26 (Chief)**, when he unparked #1324 and asked for it to be built. S0
reported the same day (§9): continue. Tracking #1324.

**Date:** 2026-09-15

**Author:** Pete (cargopete)

**Depends on:** RFC-0021 (one isolated cursor per chain, and its open question on cross-cursor reads,
which this answers), RFC-0032 (the mount table, `(tenant, alias) -> nid`, derived refcounts), RFC-0034
(a mount's SQL surface, which a cross view may not widen), RFC-0018 §1 (authored views evaluated per
request over hot ∪ sealed), RFC-0016 §4 (the provenance stamp), RFC-0026 (per-nest quarantine),
RFC-0047 C4 (the per-cursor analytics budget), RFC-0041 (why a cross view is not an incremental
entity, §7).

**Amends:** RFC-0012 §6's "no cross-nest DuckDB attach", for a declared cross route only. A mount's own
`/sql` stays scoped to that mount.

**Nature:** new query-surface capability in the binary. Per RFC-0044 §8 and CLAUDE.md, new binary
capability is decided by Chief and recorded, or it does not happen.

## Abstract

A multichain runtime already serves one nest per chain in one process, with one isolated cursor each.
What it cannot do is answer a question that needs two of them. The 2026-09-11 hackathon protocol
creates intents on Ethereum Sepolia and fills and settles them on Arc Testnet. Its `intents` view on
either chain can only say `PENDING`, because the fill is on the other chain. Today the answer is a
client-side merge, or a small service in front of both nests.

This RFC adds a **cross view**: a `[[cross]]` record in `mounts.toml` that names two or more mounts of
one tenant, each under a schema name, plus a directory of authored `*.sql` over those schemas. It is
served at its own route with `/sql`, `/explain`, `/schema` and `/ready`. A request builds each member
exactly as that member's own `/sql` would, from that member's own hot snapshot and sealed watermark,
into one read-only DuckDB connection, and runs the query over them. Nothing is stored and nothing is
indexed. At startup the record reserves a declared share of each member cursor's analytical permits
and RAM, inside the per-cursor budget, and a cross query runs only inside that share. So a cross query
never competes with a member's own `/sql` for admission or memory (§6).

The one part of #1324 this RFC does not keep is "reports the lower of the two `as_of` watermarks". A
block height has no meaning on another chain. §5 replaces it with a per-member provenance vector and
one cross-member bound that is well defined.

## §0 - The non-negotiables this touches, and why they hold

**Per-nest and per-cursor isolation.** One nest's bad view, or one chain's stall or reorg, must not harm
another.

- A cross record costs its members capacity once, at startup, in the open. Each member cursor gives the
  record a declared number of its analytical permits and the RAM behind them, and each nest on that
  cursor reports it in `/ready` and `/schema` (§6).
- At runtime a cross query takes nothing else from a member: no member permit, no member queue slot and
  no member's cached connection. However saturated the cross route is, a member's own `/sql` is
  admitted, queued and refused exactly as it is with the cross route idle. S2 fails if it is not.
- That guarantee covers admission and memory, which nuthatch budgets. It does not cover CPU or disk
  I/O, which nuthatch budgets for no query today. A busy cross route can make a member's own queries
  slower, as two of the member's own queries already can. §11 carries the stronger guarantee as an open
  question.
- Apart from the capacity fields §6 adds, a member's own routes, cursor, store and responses do not
  change when a cross record exists. S1's criterion is that they are otherwise byte-identical.
- A member's faults stay with that member. A damaged segment reduces that member's table and is named
  `member.table`. A failed hot scan marks that member `tip_unavailable`. A quarantined member is named
  in the provenance and makes the cross route not ready. No member is quarantined because of a cross
  view, and an invalid cross record refuses its own route, not the runtime.

**The single-cursor law.** A cross view reads two cursors' data at request time. It never merges two
chains' block streams, never feeds a circuit, and never writes. Each cursor stays single-chain,
single-writer and one failure boundary. §7 records why this also rules out a cross-chain incremental
entity.

**DuckDB single-writer.** The cross connection is the same in-memory, locked-down, read-only connection
that `/sql` uses, with `allowed_directories` widened to the union of its members' data directories and
the shared segment store. It never opens a member's redb file. It reads through the member's
already-open `HotStore` handle, so the exclusive lock that redb takes at `Database::open` is never
contended. Ingestion writes nothing here.

**The per-cursor RAM budget.** RFC-0047 C4's inequality is per cursor, and §6 gives the arithmetic in
full. A cross record's permits come **out of** each member cursor's configured permits and are never
added on top, so every term of the inequality keeps its value. At the shipped defaults a member cursor
runs one member permit and one cross permit at 512 MB each, plus the 1,024 MB reservation: 2,048 MB. One
cross query is charged in full to every member cursor that reserves for it, which over-reserves; that
is the safe direction. A cross query's hot snapshot is bounded to one member query's worth, split across
its members, so the term outside DuckDB does not grow either.

**Determinism.** A cross view is a query-time read. It is never materialised, never written back, never
seen by ingest, decode, seal or reorg, and it contains no model output. Non-negotiable 4 is untouched,
for the same reason RFC-0018 §1 gives for `views/*.sql`.

**NID and tenant keying.** A cross record names members by `(tenant, alias)`. It never names an NID and
never crosses tenants. At request time each alias resolves through the mount table to the NID that it
serves now, and the provenance names that NID. The record is mount config, not identity: it has no NID,
editing it re-indexes nothing, and it holds no reference on any dataset (§8).

**The rest.** Nothing here makes a network call, adds a dependency, adds a service or changes the
single-binary shape. It is not hosted multi-tenancy: joining two tenants' mounts is refused (§3).

## §1 - What is already true

**One nest's `/sql`**, in `serve.rs` and `analytics.rs`:

1. Admission takes a permit from the cursor's analytical gate. The gate is created once per cursor and
   shared by every nest on it (#1024). A request parks up to `SQL_ADMISSION_WAIT` in a per-nest queue
   bounded by `SQL_MAX_QUEUED` (#1319).
2. In one blocking task: scan the hot store with `hot_rows_by_table_bounded_with_bytes`, copy the
   maintained entity rows with their watermarks (#932), read `sealed_through`, run
   `analytics::query_hot_cold`, then read `last_block` as `as_of`.
3. `attempt()` refuses non-`SELECT` shapes, statement stacking, file access and replacement scans. It
   then asks DuckDB's parser what the statement references (`reject_unknown_table_refs`). It defines one
   view per reachable table: `read_parquet` over segments at or below `sealed_through`, `UNION ALL` a
   temp table of the hot rows above it, so hot and cold are disjoint by construction (COR-1). Then it
   defines the nest's `views/*.sql`, the offchain views, `labels` and `{template}__children`.
4. The response carries `degraded`, `degraded_tables`, `tip_unavailable` and
   `provenance { as_of, sealed_through, source: "hot+sealed", registry_hash, nid, entities }`.

**The per-cursor analytics budget** is checked by `validate_against` in `analytics_budget.rs`:
`(permits × analytics.memory_limit) + ingestion_reservation + runtime_headroom ≤ 2048 MB`, with the
reservation never below `2048 - 2 × 512 = 1024 MB`. So at most 1,024 MB of any cursor's budget is ever
DuckDB's, whatever the settings. A configuration that breaches it refuses to start, naming the sum.

**The runtime** composes one router: `/health`, `/nests`, `/ready`, and each mount's full router nested
under its route key (`alias`, or `tenant/alias` in a multi-tenant runtime). Each mount's `AppState`
carries its own `dir`, `store`, `sql_gate`, `sql_queued`, `surface` and `nid`.

**The runtime already refuses one cross-chain aggregate.** In a runtime of more than one nest,
`metrics.rs` exposes no process-wide height or watermark, because "heights and watermarks have no
meaningful cross-chain aggregate" (#828). §5 applies the same rule to provenance.

**Two facts the design depends on, which the tree does not settle.** S0 measures both.

- The table-reference walk reports a schema qualifier only as `QUALIFIED_SCHEMA`. That sets
  `surveys = true`, which makes the connection define every view, and it passes `BASE_TABLE` names
  without their schema. So today `arc.fills` and `sepolia.fills` are the same name to the walk, and any
  qualified reference is treated as a catalogue query.
- Since #1337, a block's timestamp can be held in its checkpoint record. On the tip path, the window
  boundary's record packs the timestamp when the boundary header answers, best effort. The seal-direct
  hand-off sets `last_block` directly. So it is not known that every member's coverage head has a
  timestamp.

**Nothing in `docs/frozen-for-2027.md` covers this**, so no reopening rule applies. RFC-0021 lists
cross-chain joins as a non-goal *of that RFC* and leaves an open question: "Cross-cursor read queries in
the shared serving layer - expose now (read-only, no derivation) or defer?" This RFC answers that
question, and it keeps "no derivation".

## §2 - Goals and non-goals

### Goals

- An operator declares, once, which mounts may be read together, under which names, and how much of
  each member cursor's analytical capacity the pairing may use.
- A caller reads them with ordinary SQL, including joins over each member's own authored views.
- Every answer says, per member, which dataset answered and as of what, and gives one bound that is
  valid across chains.
- At runtime a cross view never takes a member's admission or memory. What it costs a member is fixed
  at startup and reported. It cannot stall, quarantine or unmount a member, and when a member is in any
  of those states the cross view says so.
- Members on the same chain work too. A record whose members share a cursor reserves on that cursor
  once.

### Non-goals

- **A cross-chain incremental entity.** See §7.
- **An undeclared cross-mount read.** There is no runtime-root `/sql` over every mount (§10).
- **Joining two tenants' mounts.** Refused (§3).
- **Cross-runtime or cross-machine joins.** Members are mounts in this runtime. In v1, RFC-0022's
  query-FE tier and the cursorless serve role refuse a cross record by name (§11).
- **CPU or disk I/O isolation.** Nuthatch budgets neither for any query today (§6, §11).
- **Memoisation in v1.** A cross answer is computed per request. The single-nest memo keys one
  directory's state; a cross key is every member's state, and it is a later slice if a measurement asks
  for it.
- **The Graph-compatible surface (RFC-0053) over a cross view.** That surface is per nest.
- **Comparing or ordering block numbers across chains.** It cannot be refused, because the engine sees
  integers. It is authoring guidance in the builder skill's `views.md`, with §5's reason.

## §3 - Declaring a cross view

In `mounts.toml`, beside the mount records it names:

```toml
[[cross]]
name = "arcaidia"                  # route key; must not equal a mount's route key
tenant = "default"                 # optional; the runtime's default tenant
views = "cross/arcaidia"           # optional; *.sql, relative to the runtime directory
sql = "open"                       # optional; "open" | "deny" | "allowlist", as RFC-0034
permits = 1                        # optional; permits reserved on each member cursor (§6)
memory_limit_mb = 512              # optional; per cross query; defaults to analytics.memory_limit

[cross.members]
sepolia = "arcaidia-sepolia"       # schema name = mount alias, within `tenant`
arc = "arcaidia-arc"
```

**Why `mounts.toml`, and not a nest.** A cross view spans mounts, and only the mount table knows the
mounts. It cannot live in a member's `views/`, because then one nest's content address would depend on
another nest and on an alias, and an alias is deliberately not identity (RFC-0032 §3). A cross record is
the same kind of thing as RFC-0034's allowlist and RFC-0052's `[mounts.publish]`: operator config about
how data is served, which moves no NID and re-indexes nothing. §10 weighs and rejects a composite nest
with its own NID.

**The declaration is the consent boundary.** The operator states which mounts may be read together,
and what share of their capacity the pairing may take. Without a record, nothing crosses and nothing
is reserved.

**Serving.** The record is served under its route key, like a mount, with four routes:

- `/sql` and `/explain`, with today's parameters;
- `/schema`, which lists each member's tables and views under its schema, with the members'
  `semantic.toml` descriptions and the record's own capacity (§6);
- `/ready` (§8).

It has no `/tables`, `/entity`, `/derived`, `/balances`, GraphQL or admin routes. Those routes are
about one dataset.

**Addressing.** Member relations are schema-qualified: `sepolia.intents`,
`arc.liquidity_vault__fast_filled`. A member's maintained entities appear under its schema, as they
appear in that member's own `/sql`. The record's own `views/*.sql` define relations in `main`, so a
caller queries `intents` directly.

**Validation** runs at load and on every mount change. It refuses the **record**, with a named reason,
while the runtime and every mount start normally. A configuration change must not have a larger blast
radius than a fault (RFC-0027).

| Refused | Why |
|---|---|
| fewer than two members | a one-member cross view is that mount |
| a member alias that is not mounted for `tenant` | a partial member set would answer `PENDING` for everything |
| a member of another tenant | the tenant label is the one separation nuthatch keeps; crossing it serves one tenant's mount under another's path |
| two members that resolve to one NID | a self-join fits in one schema, and two schemas over one dataset make provenance ambiguous |
| a member whose surface is `deny` | the operator said no SQL for that mount |
| a member whose surface is `allowlist`, while the record is `open` | a cross view may not widen a member's surface; the record must be `allowlist` or `deny` |
| a `name` equal to a mount route key, or to `nests`, `ready`, `health` or `_admin` | route collision |
| a schema name that is not `[a-z][a-z0-9_]*`, or that is `main`, `temp`, `information_schema`, `pg_catalog` or `system` | DuckDB's own catalogue names |
| a reservation that leaves a member cursor fewer than one permit of its own | the member's own `/sql` would have nothing to run on (§6) |
| a reservation that breaks a member cursor's budget inequality | the cursor would pass 2 GiB; the refusal prints the sum, as `validate_against` does (§6) |

A refused record reserves nothing: its members start with every configured permit.

A cross view's authored SQL is validated like `views/*.sql` (RFC-0018 §1). A broken file is a loud
warning at startup and a `nuthatch check --dir` failure against the runtime directory, and the other
files still load.

## §4 - Building the connection

A cross `/sql` request runs these steps in order.

1. **Admission** on the record's own gate (§6), charged against `SQL_TIMEOUT` as today. No member gate
   is touched.
2. **Snapshot each member**, in declared order, in one blocking task. Resolve the alias to its current
   `AppState`; if the mount is gone, refuse (§8). Take that member's hot snapshot, entity rows and
   watermarks, `sealed_through`, `last_block`, `indexed_head` and the timestamp of `indexed_head`,
   under `SQL_MAX_HOT_ROWS` and `SQL_MAX_HOT_BYTES` each divided by the number of members (§6). These
   are the reads the member's own `/sql` makes, with the same same-task discipline (#932), so each
   member's rows and provenance describe one state of that member. The redb read transaction ends when
   the snapshot is copied, so a long cross query holds no member's store.
3. **Open or reuse a connection.** It is locked down as today. It is keyed by the record's name and by
   every member's `(nid, sealed_through, excluded set, inputs)`, in a cache of its own, so that it never
   evicts a member's cached connection. Its `allowed_directories` is the union of each member's
   `allowed_read_dirs`, and its memory limit is the record's `memory_limit_mb`.
4. **Define each member into its own schema.** `CREATE SCHEMA <m>`, then that member's table views,
   authored views, offchain views, `labels` and children views, from that member's directory, snapshot
   and watermark. An unqualified name inside a member's view must resolve in that member's schema. Hot ∪
   sealed is COR-1 per member, disjoint by that member's own watermark. No dedup across members is
   needed: they are different datasets, and §3 refuses two schemas over one.
5. **Define the record's own views** in `main`.
6. **Run the security walk over qualified names.** A reference to `<m>.<table>` is permitted when `<m>`
   is a member schema and `<table>` is reachable in it. An unqualified reference resolves in `main`.
   `information_schema` and `duckdb_*` stay catalogue surveys and see only these schemas. The walk's
   reachability output becomes `(schema, table)` pairs, so the integrity sweep reduces a damaged table
   in the member that owns it, and never hashes another member's segments.
7. **Run** under the guard, then answer (§5).

`/explain` is the same pipeline, stopping at the plan.

## §5 - Provenance, and the bound that crosses chains

**Why not the lower `as_of`.** `as_of` is a block height on one chain. Sepolia at block 11,694,285 and
Arc at block 62,000,000 have no order that a caller can use. "The lower of the two" would present
Sepolia's height as if it bounded Arc. It is the cross-chain aggregate that `metrics.rs` already
declines to expose (#828).

**What a caller sees instead:**

```json
{
  "count": 3, "truncated": false, "cached": false,
  "degraded": false, "degraded_tables": [], "tip_unavailable": false,
  "rows": ["..."],
  "provenance": {
    "source": "cross",
    "cross": "arcaidia",
    "complete_before_timestamp": 1789061002,
    "members": {
      "sepolia": {
        "alias": "arcaidia-sepolia", "chain": "sepolia", "nid": "...", "registry_hash": "...",
        "as_of": 11694285, "sealed_through": 11688304,
        "coverage_head": 11694285, "coverage_head_timestamp": 1789061004,
        "source": "hot+sealed", "health": "indexing", "tip_unavailable": false, "entities": null
      },
      "arc": {
        "alias": "arcaidia-arc", "chain": "arc-testnet", "nid": "...", "registry_hash": "...",
        "as_of": 62000000, "sealed_through": 61999936,
        "coverage_head": 62000000, "coverage_head_timestamp": 1789061002,
        "source": "hot+sealed", "health": "indexing", "tip_unavailable": false, "entities": null
      }
    }
  }
}
```

Sepolia's two heights are the tip and sealed watermark #1341 recorded; every other figure is
illustrative.

Each member object is that member's own single-nest provenance, with the same meaning, plus three
fields:

- `chain`;
- `coverage_head`, the member's `indexed_head`: the larger of `last_block` and `sealed_through`. During
  a seal-direct backfill, `as_of` alone understates what the member covers;
- `coverage_head_timestamp`, the block timestamp of `coverage_head`, or `null` when the store does not
  hold one.

`degraded_tables` names `member.table`. The top-level `tip_unavailable` is true when any member's is,
and the member object says which.

**`complete_before_timestamp`** is the minimum of the members' `coverage_head_timestamp`, and `null`
when any member's is `null`. Its guarantee:

> For every member, every block of that member's chain whose timestamp is strictly earlier than
> `complete_before_timestamp` is at or below that member's `coverage_head`. So every row that such a
> block contributes to the member's indexed contracts is in the answer.

This follows from one property of EVM chains: block timestamps do not decrease with height. A block
above `coverage_head` has a timestamp at least equal to the head's, which is at least the minimum. The
bound is strict because two blocks can have equal timestamps.

What the bound does **not** say:

- **It is not finality.** Rows above a member's `sealed_through` can still be reorged away. A caller who
  needs a stable answer reads each member's `sealed_through`.
- **It is not a cross-chain causal cut.** Two chains have no common instant. The bound says what has
  been indexed, measured on each chain's own clock.
- **It says nothing before a member's start block.** A nest indexes from its declared start.
- **It is never estimated.** A missing timestamp gives `null`. RFC-0040 §4's rule applies: a cheaper
  path may return less, never something that looks like data and is not.

In the Arcaidia case, this bound makes `PENDING` meaningful. An intent created before
`complete_before_timestamp`, whose fill also happened before it, cannot read `PENDING`.

## §6 - Capacity: a reservation, never a loan

**Why a cross query does not borrow member permits.** The first draft of this RFC had a cross query
take a permit from each member cursor's shared gate at request time. Review on #1403 found the
contradiction with §0: once a cross query holds a cursor's last permit, that member's own `/sql` queues
or answers `503`. So a record's capacity is reserved at startup, out of each member cursor's budget,
and a cross query never touches a member gate.

**The arithmetic.** Per cursor:

| Symbol | Meaning | Shipped default |
|---|---|---|
| `P` | configured analytical permits per cursor, `NUTHATCH_SQL_MAX_CONCURRENCY` | 2 |
| `M` | `analytics.memory_limit`, per member query | 512 MB |
| `R` | `ingestion_reservation`, never below `2048 - 2 × 512` | 1,024 MB |
| `H` | `runtime_headroom`, unmeasured and counted as zero | 0 |
| `c_x` | `permits` of record `x`, reserved on each of its member cursors | 1 |
| `m_x` | `memory_limit_mb` of record `x`, per cross query | `M` |

A cursor that is a member cursor of the records `X` runs its own gate with `P - Σ c_x` permits. For
that cursor, `validate_against` checks:

```text
(P - Σ c_x) × M  +  Σ (c_x × m_x)  +  R  +  H  ≤  2048
```

At the shipped defaults, with one record:

```text
(2 - 1) × 512  +  1 × 512  +  1024  +  0  =  2048
```

It holds with no setting changed. It costs every nest on that cursor half its analytical concurrency:
one permit where there were two.

A record is refused, naming the cursor and printing the sum, when either condition fails:

- **`P - Σ c_x < 1`.** The member's own `/sql` would have nothing to run on. At the defaults a cursor
  can carry one reserved permit in total. More needs `P` raised and `M` lowered together. For example,
  `P = 4` and `M = 256` fit two records: `(4 - 2) × 256 + 2 × 256 + 1024 = 2048`.
- **The inequality.** When every `m_x ≤ M`, today's check already implies it, because each reserved
  permit replaces a member permit that was charged `M`. It fails only when a record asks for more
  memory than the permit it replaces. At the defaults, `m_x = 1024` gives `512 + 1024 + 1024 = 2560`.

**The shape this rules out** is a cross permit added on top of the member permits instead of taken out
of them: `2 × 512 + 1 × 512 + 1024 = 2560 > 2048`. It is the same shape as a runtime configured with
four permits at the default memory limit, `4 × 512 + 1024 = 3072 > 2048`, which `validate_against`
already refuses at startup. S1 fails if a mutation that adds `Σ c_x` to `P`, instead of subtracting it,
still passes the check at the defaults.

**A refused record reserves nothing.** Its members start with all `P` permits, and the runtime starts.
An environment setting that breaks the inequality for every cursor still refuses startup, as today,
because no cursor could run safely. A cross record is additive and removable, so refusing only the
record is the smaller blast radius.

**The term outside DuckDB.** `R` is a floor that nothing enforces, and it already covers each member
query's hot snapshot and result materialisation. A cross query's snapshot bounds are `SQL_MAX_HOT_ROWS`
and `SQL_MAX_HOT_BYTES` divided by the number of members, and its result caps are unchanged. So one
cross query holds no more outside DuckDB than the member query whose permit it replaced. The cost: a
member with a large hot tip can be answerable through its own `/sql` and refused through the cross
route, with `503` naming the member. §11 asks whether measured headroom should replace the split.

**Admission.** Each record has its own semaphore of `c_x` permits, its own queue counter bounded by
`SQL_MAX_QUEUED`, and the same `SQL_ADMISSION_WAIT`, charged against `SQL_TIMEOUT`. A cross query
acquires that one semaphore and nothing else, so there is no lock ordering to get wrong and no deadlock
between records. Past the wait, it answers `503` naming the record.

**What `/ready` and `/schema` report.** Reduced capacity is a fact about a nest, not a verdict, so it
does not change `ready`.

- Each nest on a member cursor adds an `analytics` object to its `/ready` body, for example
  `{"permits": 1, "configured_permits": 2, "memory_limit_mb": 512, "reserved": [{"cross": "arcaidia", "permits": 1, "memory_limit_mb": 512}]}`.
  `permits` is what that nest's own `/sql` can use.
- The same nest's `/schema` says it in one sentence, because a caller that fans out needs to know it:
  "This nest's cursor runs 1 SQL query at a time; 1 more permit is reserved for cross view `arcaidia`."
- The cross route's `/ready` and `/schema` report the record's `permits` and `memory_limit_mb`, and the
  member cursors that carry them.
- The SQL rejection counter in `/metrics` carries the gate as a label, so a `503` from a cross gate is
  never counted as one from a member gate.

**What this does not isolate: CPU and disk I/O.** DuckDB runs `analytics.threads` per connection, and
nuthatch budgets neither CPU nor I/O per cursor for any query today. A busy cross route can make a
member's own queries slower. It cannot make them wait for admission, and it cannot make them refused.

## §7 - Why a cross view is request-time only

RFC-0041 made one nest's derived relation incremental: a DBSP circuit is fed one cursor's windows in
block order, and a reorg is a retraction from that cursor. A cross-chain relation maintained the same
way would need one circuit to merge two block streams that share no order, and to take retractions from
two independent reorg clocks. That puts two chains' state behind one state machine, which the
single-cursor law forbids, whatever the circuit is called. So a cross view stays a query, evaluated per
request. Each member's own incremental entities are available to it as relations. A cross-chain
incremental entity is a non-goal of this RFC, not a later slice.

The performance consequence is RFC-0041 §1's original one: a cross view computes its join per request.
Where that is too slow, make each member's side small with that member's own incremental entities, and
join those.

## §8 - Lifecycle: health, unmount, NID change

**Health.** The cross route's `/ready` answers `200` only when every member's own `/ready` would. Its
body lists each member's readiness, verbatim, under the member's schema name. A quarantined or stalled
member makes it `503` and is named. `/sql` keeps answering while a member is stalled or quarantined, as
that member's own `/sql` does (RFC-0026 keeps serving and says so), and the member's `health` in the
provenance says which.

**Unmount.** A cross record holds no reference on any dataset. RFC-0032's refcount is a count of mount
records, and a cross record is not a mount record. So unmounting a member decrements the count exactly
as today, and `nuthatch prune` may reclaim the data. Until the member is mounted again, every request to
the cross route is refused with `503` and `member_unmounted`, naming the alias. The route never answers
from the remaining members, because a view that left-joins an absent member answers with confident,
wrong values. The reservation on the remaining members' cursors stays in place until the record is
removed, so their capacity does not move while an operator remounts.

**NID change.** When a member's alias is bound to a new NID (an edited nest is a new nest), the cross
view follows the alias at the next request, and the provenance names the new NID. The connection key
includes each member's NID, so a connection built over the old dataset is never reused. Pinning a
member to an NID is §11's first open question.

## §9 - Slices, each with a criterion that can fail

S0 runs first, and it can stop the rest.

| # | Slice | Ends with | Fails if |
|---|---|---|---|
| S0 | Measure §1's two unknowns against the tree and the bundled DuckDB (`duckdb` 1.10504.0). No product code. | A test that defines a view with an unqualified reference inside schema `a`, where both `a.t` and `b.t` exist, and records which one it binds. A test over `json_serialize_sql` that records what the walk sees for `a.t`. A table of whether the store holds a timestamp for `indexed_head` after a tip commit, under finality-only, after the seal-direct hand-off, and with `block_timestamps = false`, on the redb and Postgres stores. | The unqualified reference binds to `b.t` or fails to bind, which invalidates §4 step 4 and returns this RFC for redesign; or the walk cannot recover a base table's schema. |
| S1 | `[[cross]]` parsing, validation and §3's named refusals; §6's reservation applied to member gates, with its budget check and the `analytics` fields in `/ready` and `/schema`; `nuthatch check --dir` over a runtime directory. No cross route yet. | A runtime directory with one valid and one invalid record starts and serves every mount, and each member cursor of the valid record runs `P - c_x` permits. | Any member's `/sql`, `/tables` or `/ready` response differs, byte for byte, from the same runtime with no `[[cross]]` record, other than §6's capacity fields; or the runtime refuses to start; or any row of §3's table is accepted; or a mutation that adds `Σ c_x` to `P` instead of subtracting it still passes the budget check at the defaults; or a refused record leaves a member with fewer than `P` permits. |
| S2 | The cross `/sql` and `/explain`: §4's pipeline, §5's provenance and §6's admission. | §12's `intents` cross view over fixture copies of both datasets, with hot rows on both sides and ingestion stopped. | Its rows differ from an oracle that runs each member's own `/sql` for the six source relations and merges them in the test; a mutation that drops one member's hot rows, or the schema from the walk, leaves that comparison green; a reorg on one member changes the other member's rows or provenance; with the cross route saturated, every cross permit held and its queue full, a member's own `/sql` waits for admission or answers `503` where the same request with the cross route idle does not (asserted on the member gate's available permits and the member's rejection count); a mutation that makes a cross query acquire a member gate leaves that assertion green; or, with a member's own gate saturated, a cross query is refused. |
| S3 | Health, unmount, remount, `/ready` and `/schema` on the cross route. | Unmount a member: the cross route refuses and names it, and prune reclaims the dataset. Remount under a new NID: the provenance names it. | `nuthatch prune` keeps the dataset on the cross record's account; any request answers from the remaining member; the cross `/ready` answers `200` with a quarantined member. |
| S4 | The budget, on the enforcing surface. | The dense multi-nest RSS gate gains a two-cursor runtime under concurrent member and cross load, with the cross connection's peak charged to every member cursor. | Any cursor's attributed RSS exceeds its 2 GiB budget; or, on any cursor, live query connections exceed its own permits plus the permits reserved on it. |
| S5 | Documentation: `docs/operators.md`, the builder skill's `views.md`, `llms.txt`, and the MCP nest selector accepting a cross name. | The documentation drift gates pass, and a runnable example uses the Arcaidia pair. | `tests/doc_command_check.rs` or `tests/skill_refs.rs` fails; or the example's documented query does not run against the fixture runtime. |

**S0 reported 2026-09-26: continue** (`analytics.rs`, `cross_nest_s0`).

| Question | Answer |
|---|---|
| An unqualified `t` inside a view in schema `a`, with `main.t`, `a.t` and `b.t` all present | Binds `a.t`. Also true for a view created under `SET schema`, and for a view over a view in the same schema. |
| What `json_serialize_sql` gives for `a.t` | Every `BASE_TABLE` node carries `schema_name` beside `table_name`: `a.t`, `b.t` and a bare `t` are three distinct pairs. The walk discards the pairing today, in `walk_table_refs`; it does not lose it. |
| A timestamp for `indexed_head` after a tip commit | Present, on redb and Postgres, whether or not `block_timestamps` is set: the window boundary's record comes from one header, hash and timestamp together (#1494). Read from the code, not yet asserted by a test. |
| After the seal-direct hand-off | Can be absent until the next tip commit, because the hand-off sets `last_block` without a header. §5 already answers `null` then. |

Neither stop condition fired.

## §10 - Alternatives

**Borrowing member permits at request time.** No capacity is reserved, and a cross query takes a permit
from each member cursor's gate when it runs. It costs members nothing while the cross route is idle. It
also lets a saturated cross route make a member's own `/sql` queue or answer `503`, which §0 forbids.
Rejected in §6.

**The client-side merge, which is today's answer.** Two requests, merged on `intentId` in the caller,
as the Arcaidia nest's README tells its builder to keep doing. It works. It costs every two-chain
protocol a small service, and the two answers come from two moments with nothing that says what the
merge covers.

**DuckDB, or a warehouse, over two published mirrors (RFC-0052).** This needs no change to nuthatch,
and it is the right answer for analytics. It reads sealed history only. For a sparse nest on an
unregistered chain, sealed history trailed tip by up to a 36-hour seal span on Sepolia (#1341). An
intents dashboard cannot use that freshness.

**An undeclared runtime-root `/sql` over every mount.** It needs no configuration. It also makes every
mount joinable with every other by default, bypasses RFC-0034 wherever a member is allowlisted, reads
across tenants unless a rule is added (and that rule is the declaration by another name), and has no
place to reserve capacity from. Rejected.

**A composite nest with its own NID over its members' NIDs.** This gives a cross view an identity and a
place in the registry. But it stores nothing, so the identity keys nothing. Every member edit would move
it, and RFC-0032's refcount would count something that is not a mount. Rejected for v1. Revisit only if
a cross view must be published as a package.

**A cross view in a member's `views/`.** Rejected in §3: a nest's identity would depend on another nest
and on an alias.

**Attaching member DuckDB instances into a third.** A member has no persistent DuckDB to attach. Its
`/sql` connection is in-memory views over Parquet plus temp tables, built per request. Defining both
members into one connection is the same work with one engine.

## §11 - Open questions

1. **NID pinning.** May a member name the NID it expects, so that the record refuses when the alias
   moves? This helps reproducibility, and it costs the operator an edit on every nest upgrade.
2. **Scaled mode.** A query-FE node reads a shared Postgres hot store and shared segments, and may be
   able to build members as §4 does. v1 refuses a cross record outside the embedded runtime. Whether the
   FE tier accepts one is a separate decision.
3. **A sealed-only mode.** A `sealed` request parameter would answer from every member's sealed segments
   only. The answer would then be a pure function of the member catalogues, and citable byte for byte.
   It is cheap to add, and the motivating case does not need it.
4. **Named queries across a member's ceiling.** §3 lets a record carry RFC-0034's `allowlist` with named
   queries. Whether a cross record's named query must also sit inside each member's manifest ceiling
   (RFC-0034 phase 2) is not settled.
5. **Isolation beyond admission and memory.** §6 guarantees that a cross route never delays a member's
   admission or takes its memory. It does not guarantee a member's latency, because CPU and disk I/O are
   shared and unbudgeted. Should a record carry its own `analytics.threads`, or does that question need
   a per-cursor CPU budget first? Decide on a measurement of a member's `/sql` latency under a saturated
   cross route, not before.
6. **Measured headroom instead of a split snapshot bound.** Once RFC-0047 §6 measures the high-water
   outside DuckDB, may a cross query spend measured headroom and see each member's full hot tip? Until
   then, §6's split stands.

## §12 - Worked example: Arcaidia on Sepolia and Arc

The two nests run today on the Helsinki box as `arcaidia-sepolia` (chain `sepolia`, id 11155111) and
`arcaidia-arc` (chain `arc-testnet`, id 5042002). They are built from one nest repository, with the same
contracts at CREATE2-identical addresses. Each carries the authored views `intents`, `fills` and
`settlements`. Each `intents` view joins only its own chain's fills and settlements, so an intent created
on Sepolia and filled on Arc reads `PENDING` on both nests.

Neither chain is in `chains.rs`, so both seal at `Finality::Depth(64)`. On Sepolia's 12-second blocks
that is about 13 minutes behind tip; on Arc's sub-second blocks it is under a minute. The members'
`sealed_through` values therefore trail by very different amounts of time, which is why §5's bound is
built from coverage heads and not from sealed watermarks.

`mounts.toml`:

```toml
[runtime]
name = "hackathon"

[[chains]]
chain = "sepolia"
chain_id = 11155111
rpc_urls = ["https://ethereum-sepolia-rpc.publicnode.com"]

[[chains]]
chain = "arc-testnet"
chain_id = 5042002
rpc_urls = ["https://rpc.testnet.arc.network"]

[[mounts]]
alias = "arcaidia-sepolia"
nid = "..."

[[mounts]]
alias = "arcaidia-arc"
nid = "..."

[[cross]]
name = "arcaidia"
views = "cross/arcaidia"

[cross.members]
sepolia = "arcaidia-sepolia"
arc = "arcaidia-arc"
```

`cross/arcaidia/10-intents.sql`:

```sql
-- An intent is created on one chain and filled and settled on the other, so fills and settlements
-- are read from both. arg_min over block_number is sound only because every fill of one intent is
-- on its destination chain. Never order rows from two chains by height.
CREATE VIEW intents AS
  WITH created AS (
    SELECT * FROM sepolia.intents
    UNION ALL BY NAME
    SELECT * FROM arc.intents
  ),
  fill AS (
    SELECT intent_id, arg_min(id, block_number) AS fill, arg_min(vault, block_number) AS fill_vault
    FROM (SELECT intent_id, id, vault, block_number FROM sepolia.fills
          UNION ALL
          SELECT intent_id, id, vault, block_number FROM arc.fills)
    GROUP BY intent_id
  ),
  settled AS (
    SELECT intent_id, arg_min(id, block_number) AS settlement,
           arg_min(outcome, block_number) AS settlement_outcome
    FROM (SELECT intent_id, id, outcome, block_number FROM sepolia.settlements
          UNION ALL
          SELECT intent_id, id, outcome, block_number FROM arc.settlements)
    GROUP BY intent_id
  )
  SELECT c.id, c.sender, c.recipient, c.amount, c.source_chain_id, c.destination_chain_id,
         c.created_at_timestamp,
         CASE WHEN f.fill IS NULL THEN 'PENDING' ELSE 'FAST_FILLED' END AS fast_status,
         CASE WHEN s.settlement IS NULL THEN 'PENDING' ELSE 'SETTLED' END AS canonical_status,
         f.fill, f.fill_vault, s.settlement, s.settlement_outcome
  FROM created c
  LEFT JOIN fill f ON f.intent_id = c.id
  LEFT JOIN settled s ON s.intent_id = c.id;
```

A caller asks for the intents it can trust, using the bound from a previous answer's provenance:

```sh
curl -G http://127.0.0.1:8288/arcaidia/sql \
  --data-urlencode "q=SELECT id, fast_status, canonical_status FROM intents WHERE created_at_timestamp < 1789061002"
```

For an intent created before `complete_before_timestamp`, `FAST_FILLED` and `SETTLED` are exact, and
`PENDING` means that no fill or settlement happened before the bound. For a newer intent, `PENDING` can
also mean that its destination chain has not been indexed that far yet. The builder's own merge
answers the same question with no bound at all.

**What the record costs the two nests.** At the shipped defaults each cursor now runs one query for its
own nest where it ran two, and reserves one for `arcaidia`. Both nests' `/ready` and `/schema` say so.
#1319's builder fanned out six queries at once and was helped by more permits. A runtime that wants
four member permits per cursor, on 3.8.1's budget check, sets `P = 5` and `M = 204`, with the record's
`memory_limit_mb` left at `M`: `(5 - 1) × 204 + 1 × 204 + 1024 = 2044 ≤ 2048`.

Nothing else in the two nests changes. The nest repository, both NIDs, both mounts' own URLs and every
row they serve stay exactly as they are, which is S1's criterion applied to a real runtime.
