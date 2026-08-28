# Authored incremental entities - a relation the indexer keeps, not one it recomputes (RFC-0041)

A nest can declare a relation in `entities.toml`. Nuthatch compiles the `SELECT` into a DBSP circuit
and **maintains it as blocks arrive**, so a query reads an answer that is already computed instead of
recomputing one. Shipped in 3.0.0-alpha.

Read [views.md](views.md) first if you have not: a view and an entity look almost identical in the
file and are completely different at runtime, and choosing wrongly is the common mistake.

## View or entity

| | authored view | authored entity |
|---|---|---|
| lives in | `views/*.sql` | `entities.toml` |
| when it runs | on every query | as each block arrives |
| costs | query time, every time | memory, continuously |
| SQL it accepts | anything the read-only surface allows | a small admitted subset, below |
| reorg | nothing to do - it reads current data | retraction, handled for you |

**Prefer a view.** It is free until someone asks, it accepts any SQL, and it cannot be wrong about
history. Reach for an entity when a specific query is both *asked often* and *expensive*, which in
practice means an aggregation over a table with a lot of sealed history.

On a real nest, one panel over 733 sealed segments went from **2.15 s to 88 ms** by becoming an
entity. That is the shape worth converting: the same aggregate, asked repeatedly, over history that
keeps growing.

## Declaring one

```toml
[[entities]]
name = "indexer_rewards"
sql = "SELECT indexer, SUM(tokensRewards) FROM service__indexing_rewards_collected GROUP BY indexer"
key = ["indexer"]
max_rows = 100000
```

- **`name`** is the relation's name. It is how you query it, and it **must not collide with a decoded
  table** - the nest refuses to start rather than shadow one.
- **`key`** must name at least one output column, and must be unique in the result. Validation runs
  the query once and checks; a non-unique key is refused at load, not at the first block.
- **`max_rows`** is the declared bound, and it is a **fault** rather than a warning. Crossing it stops
  the entity, which quarantines the nest. Size it for what the relation will hold in a year, not
  today, and see the budget note below.

## The SQL it accepts

An entity's `SELECT` is compiled, not merely run, so the subset is small and checked at load:

- **Aggregates: `sum`, `min`, `max`, `avg`, `count`, `count(*)`.** Nothing else. This is an
  allowlist, not a refusal list, and deliberately so - DuckDB knows 88 aggregate names and grows the
  set, so anything not proven incrementally maintainable is refused rather than silently admitted.
- One `GROUP BY`, projection, filtering, exact arithmetic, and an equijoin between two tables.
- Column names bind against the nest's ABI at load. An entity naming a column the contract does not
  have is refused when you start, not at the first block that would have used it.

If your query does not fit, it is a view. That is not a consolation prize - see the table above.

## What it costs

Charged against the per-cursor RAM budget at **3,200 bytes per declared `max_rows`**, plus about 8 MB
for the circuit and its thread. So `max_rows = 100000` reserves roughly 320 MB of the cursor's budget
whether or not the relation ever fills. A mount that would exceed the budget is refused at load.

Measured at real scale the true cost is 940-1,482 bytes per maintained row depending on how wide the
key and aggregates are, so the charge is deliberately conservative.

## What happens on a restart

**The entity is rebuilt from the sealed corpus and the hot tail.** No RPC, no re-index: the facts are
already on disk. On a nest with 733 sealed segments that took **1.9 seconds**; on one with 2,985, 2.4
seconds. Expect a restart to get slower by about that much, once, per entity.

This is also why editing an entity is cheap. `entities.toml` is excluded from the nest's *data*
identity, so an edit moves the package NID without invalidating the decoded facts - the entity
rebuilds locally and the chain is never re-fetched.

## What happens on a reorg

Nothing you have to write. The removed facts are fed back at weight `-1` and the aggregate retracts;
there is no rollback interface because there is nothing to roll back. Observed live on Ethereum
mainnet: a reorg rolled back 32 rows and the relation still summed to the same total as every decoded
transfer the nest held, to the last digit.

## Watching one

Six series on `/metrics`, labelled by nest and entity:

| series | alert on |
|---|---|
| `nuthatch_entity_current` | 0 for long means it is not keeping up |
| `nuthatch_entity_seconds_since_progress` | the watermark has stopped moving |
| `nuthatch_entity_faulted` | the circuit stopped. Terminal, and the nest is quarantined |
| `nuthatch_entity_unavailable` | it holds no answer and is not being served |
| `nuthatch_entity_applied_through` | how far it has folded |
| `nuthatch_entity_rows` | how big it has become - watch it against `max_rows` |

A fault also pushes an `entity_fault` alert if a `[[alerts]]` sink watches that kind, so you find out
without polling. `faulted` and `unavailable` are separate because the response differs: one is dead,
the other is simply empty.

## Reading one

- `GET /derived/{entity}/{key}` - a keyed point read. A map lookup; it does not touch DuckDB.
- `GET /derived/{entity}` - the first page, with provenance.
- `` `SELECT … FROM {entity}` `` on `/sql` - queryable by name, under the column names you wrote,
  joinable against decoded tables like any other relation.

Every response carries provenance saying the relation is incremental and how far it is applied, so a
caller can tell a current answer from a lagging one.

## Footguns

- **A relation is only cheap to *derive*, never cheap to *return*.** Selecting every maintained row
  still pays for those rows and is still bounded by the row cap. IVM does not repeal I/O.
- **A fault is terminal.** The entity does not restart itself, and the nest is quarantined behind it,
  because serving a frozen relation as though it were current is the failure mode this design exists
  to avoid.
- **Adding an entity to a nest with history costs a seed on the next restart**, not on the next block.
  Measure it before you add one to a nest whose restart time matters.
