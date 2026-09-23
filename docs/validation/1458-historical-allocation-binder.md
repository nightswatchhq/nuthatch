# Historical allocation binder failure (#1458)

Reproduced on the graph split at `d58075e2`, macOS, using the bundled DuckDB.

The failing test is
`serve::tests::network_rust_allocation_pages_keep_the_first_page_snapshot_when_the_tip_advances`.
Run it with `cargo test --locked --features graph --lib` and that full test name.
Before the fix it needs `-- --ignored --exact`; after the fix use `-- --exact`.

## Reduction

With the test's three hot event rows (one stake deposit and two allocation creations), these
queries succeed independently:

```graphql
{ allocations { id indexer { id } } }
{ allocations(where: {indexer_: {id: "0x1111111111111111111111111111111111111111"}}) { id } }
{ allocations(where: {closedAt_gte: 0}) { id } }
```

Combining the relation selection and relation filter fails:

```graphql
{ allocations(where: {indexer_: {id: "0x1111111111111111111111111111111111111111"}}) { id indexer { id } } }
```

DuckDB reports `Failed to bind column reference ... inequal types (HUGEINT != VARCHAR)`
during prepare. The fixture has no sealed segments, so a sealed/hot type mismatch is not
an explanation. The raw bigint projection has not been changed.

## Workaround

The compiler previously combined a selected-relation LEFT JOIN with a correlated EXISTS
over the same derived indexer relation. Lower the filter as an uncorrelated membership query:

```sql
coalesce(b.indexer IN (SELECT n0.id FROM indexer n0 WHERE n0.id = '...'), false)
```

Membership does not multiply parent rows. The coalesce preserves EXISTS's false result for
missing/null references, including when a child ID is null. A separate executable compiler
test covers matching, nonmatching, missing, null and duplicate child IDs and negation.

The formerly ignored full HTTP test now passes, checking both the pinned second page after
head advancement and the unpinned latest page. This is a SQL-shape workaround; the specific
DuckDB optimiser defect has not been reduced to a standalone upstream SQL fixture.

## Validation

- Debug: original full HTTP test fails with the binder assertion; changed query passes.
- All 22 existing Graph compiler tests pass with the updated SQL-shape expectation.
- New membership/null/duplicate execution test passes.
- Release: restored original predicate fails with the same binder assertion in 0.29 s;
  the membership predicate passes the full HTTP test in 0.96 s. The release build does not
  silently execute the invalid plan in this reproduction.
- Network contract suite: 19 passed, two failed their 5-second query budget while the release
  build ran, one operator cold-replay test ignored. Both budget failures passed on isolated
  reruns (`fee_splits_curation_and_rewards_preserve_event_order_without_double_counting` and
  `captured_genesis_facts_match_same_block_network_subgraph`). This is not a clean concurrent
  suite pass.
