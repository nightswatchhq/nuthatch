# Sprint: watchful-wren

Filed after veracious-vireo (#813) landed.
**Three issues.** A sprint is a labelled set. It has no calendar.

## Definition of done

Every issue carrying the **`watchful-wren`** label is closed, and no open PR is for one of
them. That is three issues: #733, #299, #304. Work discovered in flight is filed
**unlabelled**. Pulling it into scope needs a board reply.

Two splits happened at filing, so those three can close without lying:

- #814 holds COR-6, COR-7, COR-8 from #299. Same deferral reasons. Not this sprint.
- #815 holds the RFC-0016/0017 keyed evals from #304. board-only. Not this sprint.

## The theme

**A control that is tested in the wrong place, or not at all, is not a control.**

Four compiles sharing one `target/` is why a docs-only PR ran the runner out of disk. A
`debug_assert` with no test is a comment that panics in CI if someone happens to trip it. A
keyword gate that trusts DuckDB not to parse `WITH … INSERT` is the stacking hole again, one
keyword over. An MCP surface whose tests never call a tool is a brochure.

Freeze-legal throughout: CI disk, two leftover audit items that are already in the code, and
tests for a server that already exists. Not RFC-0040. Not a subscribe tool. Not a keyed eval.

## The three

### 1. #733 - four artifact sets on one disk

`fmt · clippy · test` compiles clippy and test, each again for `exex`, into one `target/`.
The hostedtoolcache reclaim bought back a few gigabytes. The job is still a 90-percent
occupant of the runner, and the next cache-restore variance is the same ENOSPC.

Required check name stays `fmt · clippy · test`. Splitting the work into two jobs would
change that name unless a third job keeps it.

**Acceptance**

1. The job that GitHub requires, named `fmt · clippy · test`, is green only when both the
   default feature set and the `exex` feature set have been fmt/clippy/test'd (decode too,
   on the default side).
2. Those two feature sets do not share a `target/` on one runner. Two disks, or a drop of
   the first set before the second starts on the same disk - not four compiles left standing.
3. Deleting the split (or the aggregator that AND's the legs) fails the required check or
   puts four artifact sets back on one disk. We do not close this by reclaiming another
   directory.

### 2. #299 - COR-10 and SEC-7 only

COR-10: `_seq` is `block << 20 | log_index`. The 20-bit field is a `debug_assert` today, and
the packing still masks in release. Unreachable under current gas limits; silent if that
changes and nobody trips a debug build. The assert is already there. A test that fails if it
is deleted is not.

SEC-7: the leading-keyword gate accepts `WITH`, then comments that DuckDB will not parse
`INSERT` after a CTE list. That is the same class of claim as "`conn.prepare` is
single-statement", which was false. Refuse `WITH … INSERT/UPDATE/DELETE/COPY` in our own
code, on the public `/sql` path, string-aware.

COR-6, COR-7, COR-8 are #814.

**Acceptance**

1. A `log_index` of `1 << 20` panics `DecodedRow::seq` under debug assertions with the
   existing message. Deleting the `debug_assert` fails that test.
2. A `log_index` of `(1 << 20) - 1` still packs without colliding with the next block's
   index 0.
3. `query("WITH t AS (SELECT 1 AS x) INSERT INTO t SELECT 1")` is refused by our gate, with
   a message that names the WITH-prefixed DML, before DuckDB is asked. The same for UPDATE,
   DELETE, COPY. A legitimate `WITH … SELECT` still runs. Deleting the call from `attempt`
   fails the test.

### 3. #304 - MCP tools/call coverage and the offline path

`initialize` and `tools/list` are tested. `tools/call` is not, against a nest or without
one. CLAUDE.md says AI features degrade offline; that is unproven by test. Streaming
subscribe is not shipped (RFC-0010) and is not being built. The keyed evals are #815.

**Acceptance**

1. `tools/call` against a fake HTTP nest exercises schema discovery, SQL exec, and entity
   lookup (and the other generic tools that already exist). Deleting a `call_tool` arm fails
   the matching assertion.
2. `tools/list` does not advertise a `subscribe` tool that is not there.
3. With no nest listening: `initialize` and `tools/list` still answer (fail-open, already
   tested); `tools/call` returns `isError` and tells the caller to start `nuthatch dev`.
   That is the no-network degrade path. There is no `--offline` flag; the test is the path.

## Explicitly not in this sprint

- **RFC-0040** and anything still `frozen`.
- **#814**, the rest of the 0.4.0 lows.
- **#815**, the keyed evals.
- **#790**, the tyre-kicking pass.
- **#698**, the live site.
- **Streaming subscribe.** RFC-0010 said it is not shipped. Covering a tool that does not
  exist would be inventing it.

## How this sprint runs

Standing from nocturnal-nightjar, unchanged:

1. **Scope is the board's.** Discovered work is filed unlabelled.
2. **`Reviewed-by:` names the party who read the diff.**
3. **Acceptance is above.**

Also standing: never `git add -A`; do not `@`-mention Rowan in GitHub markdown; one merge per
CI cycle. `Closes` is one keyword per issue, not a comma list - squash only honours the first.
