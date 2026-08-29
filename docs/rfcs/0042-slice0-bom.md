
---

# Slice 1 addendum: where the boundary already is, and where it leaks (#936)

§6 asks for an analytical boundary across which "DuckDB-specific connection, value or AST types do not
escape". Measuring before building, as slice 0 did, found that most of it already exists.

## `analytics.rs` is already behind a boundary

53 connection operations, and **not one DuckDB type in a public signature**. Its surface takes `&Path`
and `&str` and returns `serde_json::Value`. The engine is entirely internal to the module that owns it.

That is the module §6 is actually about, and it needs no work.

## `graft.rs` is where it leaks, and it leaks doubly

Six public functions take or return a `duckdb::Connection`:

```rust
pub fn canonical_plan(conn: &Connection, sql: &str) -> CanonicalPlan
pub fn engine_version(conn: &Connection) -> String
pub fn parser_connection() -> Result<Connection>
pub fn determinism_gate(conn: &Connection, sql: &str) -> Result<()>
impl Dag { pub fn build(conn: &Connection, files: &[(String, String)]) -> Dag }
```

This matters more than the count suggests, because `graft.rs` is **the same module that writes the
engine string into grafting identity** (`engine: "duckdb-v1.4.0"`, slice 0 §4). So the module with the
migration consequence is also the module with the API leak. A replacement engine has to answer both at
once, and they are not independent: `engine_version(conn)` is *how* the string gets recorded.

`authored_entity_spike.rs` exposes one more (`duckdb_reference`), and it ships.

## What was built instead of a boundary

`tests/duckdb_containment.rs`. Two assertions:

1. **The engine does not spread beyond the six inventoried sites.** A shrink-only list: a hand-kept
   allowlist is normally the `CONFIG_SOURCES` failure mode, but this one may only get *shorter*, since
   removing a site is the whole point. It also fails when a site disappears, because that means slice
   0's inventory has become wrong and both should move together.
2. **`analytics.rs` keeps DuckDB types internal.** It does today; this stops it stopping.

Mutation-verified: a new module importing `duckdb::Connection` turns the first red, and a new public
function in `analytics.rs` taking a `&Connection` turns the second red. The first attempt at the second
mutation changed an existing signature instead, which broke every caller and never compiled - the
compiler caught it, not the gate, so it proved nothing about the gate.

## What slice 1 still owes

**The parity corpus.** Not built here. §6's shape list is written down and the method amendment fixes
how it must be measured - crossing DuckDB's 2,048-row vector, DataFusion's 8,192-row batch and dbsp's
10,000-row step, because #894 is what happens when a corpus sits under an engine's internal boundary.
That is the remaining work, and it is larger than the containment gate.

**Closing the `graft.rs` leak.** Deliberately not attempted. Its public API is shaped around a
`Connection` because the determinism gate and the canonical plan genuinely need a parser; replacing
that surface is engine-replacement work, not boundary work, and doing it now would prejudge slice 2.
