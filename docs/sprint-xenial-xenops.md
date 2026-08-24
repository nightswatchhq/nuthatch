# Sprint: xenial-xenops

Filed while watchful-wren (#816) is still open. Independent of that branch; starts from `main`.
**Three issues.** A sprint is a labelled set. It has no calendar.

## Definition of done

Every issue carrying the **`xenial-xenops`** label is closed, and no open PR is for one of
them. That is three issues: #286, #295, #649. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

## The theme

**A published budget that has never met the nest that would break it, a query surface that
rebuilds itself to answer, and a count that disagrees with the network.**

The high-event-rate half of the 2 GB promise is measured on ten events through twenty nests.
ABI breadth is not: more decoders, more tables, more keyspaces. `/sql` still opens a fresh
DuckDB and rebuilds every view for every query. Lodestar's `curatorCount` / `indexerCount`
disagree with the network subgraph, and the remaining guesses have already been wrong once.

Freeze-legal throughout: a measurement with a floor, a connection we already open, and a
rule cited from mappings rather than invented from logs. Not RFC-0040. Not RFC-0041.

## The three

### 1. #286 - the ≤2 GB budget under a large ABI at tip

The `per-cursor RAM budget (dense multi-nest)` job answers density and event *rate* (ten
events, 200 logs/block, twenty nests). It does not answer ABI *breadth*. Ten events is
Uniswap V4. `graph-staking-nest` is 28; `SubgraphService` is 31. More tables, more
keyspaces, a different pressure on the hot store.

The Uniswap V4 nest is not in this repo. The measurement is hermetic, like the density
job: a local RPC, an inline nest, no secret, a fork can satisfy it.

**Acceptance**

1. A CI scenario runs **one nest**, **≥28 event types**, high logs/block, at tip after a
   backfill, against the 2048 MB budget.
2. It has a floor: every table non-empty, and a row count far above zero. "RSS stays under
   2 GB" must not pass when the workload indexed nothing.
3. The density job is untouched. These are two questions; their ceilings stay different.
4. `prod-readiness.md` §5 no longer calls the breadth half unproven.

### 2. #295 - hold a persistent DuckDB connection

Each `/sql` query opens DuckDB, locks it down, attaches every segment, registers views,
runs, and drops the process. The issue is the rebuild, not the lockdown.

Single-writer still the law: only ingestion writes; this connection is read-only. One
query at a time on the cached connection. An interrupt discards it rather than leaving
DuckDB half-cancelled for the next caller.

**Acceptance**

1. Two queries against the same nest, same `sealed_through`, do not call
   `open_locked_duckdb` twice. A test fails if the second open happens.
2. A change in `sealed_through` (new sealed segments) opens a new connection. A test
   fails if it reuses the stale one.
3. A timed-out query does not leave a poisoned connection in the cache.
4. Concurrent writers are not introduced. Queries still attach read-only.

### 3. #649 - curator and indexer counts, from the mappings

#659 already cited when a `Curator` is created. This issue is the rest, and the bound
from filing still holds: find the subgraph's rule, or write down why we cannot. Do not
index on a guess.

`createOrLoadIndexer` increments `indexerCount` on first miss. Empty entities (zero
stake, zero delegation, zero allocations) are created by any handler that calls it.
Chain probes have been wrong about which events those are. The mappings are the source.

**Acceptance**

1. A document in this repo cites `createOrLoadIndexer` / `createOrLoadCurator` with
   file-and-line against `graphprotocol/graph-network-subgraph`, names every mapping
   that creates one, and records what is still unknown.
2. The three empty indexer entities still unattributed are either attributed from a
   mapping citation, or explicitly left as "not worth closing: empty by construction,
   remaining handlers named, no guess."
3. We do not add events to a nest to chase a number whose rule we have not cited.
4. The subgraph's own `stakedIndexersCount` (88) disagreeing with its entity set (97)
   is recorded as theirs, not ours.

## Explicitly not in this sprint

- **watchful-wren / #816.** Independent. Do not restack.
- **RFC-0040, RFC-0041 implementation**, anything `frozen`.
- **#790**, the tyre-kicking pass. Someone without today's scars.
- **#750, #698, #815, #638.** Credentials or GraphOps attention.
- **#296**, compact binary rows. Storage-format change, RFC-0020 no-resync question.
- **Applying the curator/indexer rules to `graph-allocations-nest` and re-indexing.**
  That is nest work plus an archive RPC. Cited here; run by the board.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; do not `@`-mention Rowan in GitHub markdown; one merge per
CI cycle. `Closes` is one keyword per issue, not a comma list - squash only honours the first.
