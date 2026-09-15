# RFC-0058: Cross-nest SQL - a declared, read-only query across mounted nests in one runtime

**Status:** **Draft** - awaiting Chief's decision. Design only; build slices are filed from §9 on
acceptance. Tracking #1324.

**Date:** 2026-09-15

**Author:** Pete (cargopete)

**Depends on:** RFC-0021 (one isolated cursor per chain, and its open question on cross-cursor reads,
which this answers), RFC-0032 (the mount table, `(tenant, alias) -> nid`, derived refcounts), RFC-0034
(a mount's SQL surface, which a cross view may not widen), RFC-0018 §1 (authored views evaluated per
request over hot ∪ sealed), RFC-0016 §4 (the provenance stamp), RFC-0026 (per-nest quarantine),
RFC-0047 C4 (the per-cursor analytics budget), RFC-0041 (why a cross view is not an incremental
entity, §7).

**Amends:** RFC-0012 §6's "no cross-nest DuckDB attach", for a declared cross route only. A mount's own
`/sql` stays scoped to that mount, unchanged.

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
indexed. A member cannot tell that a cross view exists, except by the permits the cross view borrows.

The one part of #1324 this RFC does not keep is "reports the lower of the two `as_of` watermarks". A
block height has no meaning on another chain. §5 replaces it with a per-member provenance vector and
one cross-member bound that is well defined.

## §0 - The non-negotiables this touches, and why they hold

**Per-nest and per-cursor isolation.** One nest's bad view, or one chain's stall or reorg, must not harm
another.

- A member's own routes, cursor, store, connection cache entry and `/ready` do not change when a cross
  record exists. S1's criterion is that they are byte-identical.
- A cross query holds a member cursor's analytical permit only while it holds every member's permit.
  It never waits on one cursor while it holds another's (§6). So a saturated or stalled chain degrades
  the cross route, visibly, and never takes capacity from the other chain.
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

**The per-cursor RAM budget.** RFC-0047 C4's inequality is per cursor:
`(sql_permits × analytics.memory_limit) + ingestion_reservation ≤ 2 GiB`. A cross query takes one
permit from **each distinct member cursor** and runs under one `analytics.memory_limit`. So every
cursor it touches has reserved a full slot for it, and no cursor can exceed its own inequality on the
cross query's account. This counts one connection against two budgets, which over-reserves. That is the
safe direction, and it adds no term to the inequality. Hot materialisation is bounded by each member's
own `SQL_MAX_HOT_ROWS` and `SQL_MAX_HOT_BYTES`, and the result by the existing row and byte caps.

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

- An operator declares, once, which mounts may be read together and under which names.
- A caller reads them with ordinary SQL, including joins over each member's own authored views.
- Every answer says, per member, which dataset answered and as of what, and gives one bound that is
  valid across chains.
- A cross view cannot slow, block, stall, quarantine or unmount a member. When a member is in any of
  those states, the cross view says so.
- Members on the same chain work too. Two nests on one cursor take one permit.

### Non-goals

- **A cross-chain incremental entity.** See §7.
- **An undeclared cross-mount read.** There is no runtime-root `/sql` over every mount (§10).
- **Joining two tenants' mounts.** Refused (§3).
- **Cross-runtime or cross-machine joins.** Members are mounts in this runtime. In v1, RFC-0022's
  query-FE tier and the cursorless serve role refuse a cross record by name (§11).
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

**The declaration is the consent boundary.** The operator states which mounts may be read together.
Without a record, nothing crosses.

**Serving.** The record is served under its route key, like a mount, with four routes:

- `/sql` and `/explain`, with today's parameters;
- `/schema`, which lists each member's tables and views under its schema, with the members'
  `semantic.toml` descriptions;
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

A cross view's authored SQL is validated like `views/*.sql` (RFC-0018 §1). A broken file is a loud
warning at startup and a `nuthatch check --dir` failure against the runtime directory, and the other
files still load.

## §4 - Building the connection

A cross `/sql` request runs these steps in order.

1. **Admission** (§6), charged against `SQL_TIMEOUT` as today.
2. **Snapshot each member**, in declared order, in one blocking task. Resolve the alias to its current
   `AppState`; if the mount is gone, refuse (§8). Take that member's hot snapshot, entity rows and
   watermarks, `sealed_through`, `last_block`, `indexed_head` and the timestamp of `indexed_head`,
   under that member's own `SQL_MAX_HOT_ROWS` and `SQL_MAX_HOT_BYTES`. These are the reads the member's
   own `/sql` makes, with the same same-task discipline (#932), so each member's rows and provenance
   describe one state of that member. The redb read transaction ends when the snapshot is copied, so a
   long cross query holds no member's store.
3. **Open or reuse a connection.** It is locked down as today. It is keyed by the record's name and by
   every member's `(nid, sealed_through, excluded set, inputs)`. Its `allowed_directories` is the union
   of each member's `allowed_read_dirs`.
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

## §6 - Admission without coupling two cursors

1. Collect the distinct member cursors, ordered by chain id.
2. Try to acquire a permit on each, in order. If every attempt succeeds, run.
3. If one fails, **release every permit already taken**. Then wait on the cursor that refused, for what
   remains of `SQL_ADMISSION_WAIT`, holding nothing else. When a permit frees, release it and retry the
   whole set from step 2. Past the deadline, answer `503` and name the busy member.

This enforces the isolation property, not an efficiency: **a cross query never holds one cursor's
permit while it waits on another's.** Without this rule, a saturated Sepolia cursor would park cross
queries that hold Arc permits, and Arc's own callers would be refused on Sepolia's account. The ordered
pass also removes the deadlock in which one query holds A and waits on B while another holds B and
waits on A. The cost is that a cross query can lose a race to members' own queries repeatedly and be
refused. That is the direction in which a shared budget should fail.

The cross route has its own queue counter, bounded by `SQL_MAX_QUEUED`, so cross callers cannot take a
member's queue slots.

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
wrong values.

**NID change.** When a member's alias is bound to a new NID (an edited nest is a new nest), the cross
view follows the alias at the next request, and the provenance names the new NID. The connection key
includes each member's NID, so a connection built over the old dataset is never reused. Pinning a
member to an NID is §11's first open question.

## §9 - Slices, each with a criterion that can fail

S0 runs first, and it can stop the rest.

| # | Slice | Ends with | Fails if |
|---|---|---|---|
| S0 | Measure §1's two unknowns against the tree and the bundled DuckDB (`duckdb` 1.10504.0). No product code. | A test that defines a view with an unqualified reference inside schema `a`, where both `a.t` and `b.t` exist, and records which one it binds. A test over `json_serialize_sql` that records what the walk sees for `a.t`. A table of whether the store holds a timestamp for `indexed_head` after a tip commit, under finality-only, after the seal-direct hand-off, and with `block_timestamps = false`, on the redb and Postgres stores. | The unqualified reference binds to `b.t` or fails to bind, which invalidates §4 step 4 and returns this RFC for redesign; or the walk cannot recover a base table's schema. |
| S1 | `[[cross]]` parsing, validation and §3's named refusals, and `nuthatch check --dir` over a runtime directory. No route yet. | A runtime directory with one valid and one invalid record starts and serves every mount. | Any member's `/sql`, `/tables` or `/ready` response differs, byte for byte, from the same runtime with no `[[cross]]` record; or the runtime refuses to start; or any row of §3's table is accepted. |
| S2 | The cross `/sql` and `/explain`: §4's pipeline, §5's provenance and §6's admission. | §12's `intents` cross view over fixture copies of both datasets, with hot rows on both sides and ingestion stopped. | Its rows differ from an oracle that runs each member's own `/sql` for the six source relations and merges them in the test; a mutation that drops one member's hot rows, or the schema from the walk, leaves that comparison green; a reorg on one member changes the other member's rows or provenance; with one cursor's gate held saturated, the other member's own `/sql` is refused, or a cross query holds a permit on the free cursor (asserted on the semaphore's available count); two cross views over the same members, declared in opposite order, hang past `SQL_TIMEOUT`. |
| S3 | Health, unmount, remount, `/ready` and `/schema` on the cross route. | Unmount a member: the cross route refuses and names it, and prune reclaims the dataset. Remount under a new NID: the provenance names it. | `nuthatch prune` keeps the dataset on the cross record's account; any request answers from the remaining member; the cross `/ready` answers `200` with a quarantined member. |
| S4 | The budget, on the enforcing surface. | The dense multi-nest RSS gate gains a two-cursor runtime under concurrent member and cross load. | Any cursor's attributed RSS exceeds its 2 GiB budget; or the number of live query connections exceeds the sum of the member cursors' permits. |
| S5 | Documentation: `docs/operators.md`, the builder skill's `views.md`, `llms.txt`, and the MCP nest selector accepting a cross name. | The documentation drift gates pass, and a runnable example uses the Arcaidia pair. | `tests/doc_command_check.rs` or `tests/skill_refs.rs` fails; or the example's documented query does not run against the fixture runtime. |

## §10 - Alternatives

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
across tenants unless a rule is added (and that rule is the declaration by another name), and spends
every cursor's permits on every query. Rejected.

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

Nothing in the two nests changes. The nest repository, both NIDs and both mounts' own URLs stay exactly
as they are, which is S1's criterion applied to a real runtime.
