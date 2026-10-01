# Parked issues closed, 2026-09-28

These ten issues were open with the `parked` label. Parked means deferred by decision, and they were still sitting in `gh issue list` beside live work. They were closed on 2026-09-28 so the open board is the live queue.

Closure means the deferral stands. It does not mean the work shipped, and it does not mean the decision was reversed. Anything already in the tree stays there. Reopening one of these is a new decision: restore the priority named below, and say what changed. CLAUDE.md remains the standing record for RFC-0058, RFC-0059 and RFC-0060.

Four of them (#1441, #1442, #1514, #1516) still wore `p1` beside `parked`. An open issue carries one priority or `parked`, not both. Closing them takes both off the open board.

## RFC-0059 and RFC-0060

Parked 2026-09-26. RFC-0059 S0 to S3 are in the tree behind the off-by-default `folds` and `graph` features. S3 landed in #1517. S4 serving was not started, and neither were the RFC-0060 fold ports. #1514 measured a keyed `allocations` fold at about 400 MB against the 300 MiB head gate: 861,541 allocations ever made, about 14,600 of them active. The next step would have been that measurement or a live/final split. Reopening is a call on whether replacing the gateway for indexer-agent, indexer-service-rs and tap-agent is worth the remaining sprints.

- **#1441** RFC-0059 tracking. Was `p1`.
- **#1442** RFC-0060 tracking. Was `p1`.
- **#1514** allocations, the saved clock and epochs. Was `p1`. Not built.
- **#1516** escrow accounts, signers and escrow transactions. Was `p1`. Not built.

## Cross-nest SQL

**#1324**, parked again 2026-09-26 after S0. S0 reported that the design holds (#1519). S1 to S5 were not started. The one user who wanted a cross view, the Arcaidia builder, runs on the hosted platform at one container per nest, so a cross view inside one runtime would not reach him. Was `p2`.

## Warehouse recipes

**#1263**, RFC-0052 S5, parked 2026-09-16. A Snowflake, BigQuery or Databricks recipe is listed as supported only after it has been run against a real published nest. That run was not done. DuckDB and Trino stay covered in CI. Was `p2`.

## Graph port gaps

Parked in the backlog comb of 2026-09-15, and listed as parked by diligent-dunnock.

- **#1280** no transaction sender is stored, so `event.transaction.from` cannot bind. Six fields on the pinned Uniswap V4 deployment. The column would be a per-nest opt-in with its own RPC cost, not a default. Was `p2`.
- **#1288** the graph endpoint refuses a block string in argument position. graph-node decodes it. The refusal is visible, and the case is rare. Was `p2`.
- **#1313** RFC-0041 v1 cannot render accumulator operands that are expressions. Two mechanisms landed in #1315 and moved zero fields on the pinned targets. Was `p1`.

## seal-direct

**#1446**, parked before the label tidy of 2026-09-23. A document `--seal-direct` gives up on is recorded and then omitted from the sealed segment for good. Bringing it back was left for an RFC. The shapes named on the issue each break something the cursor already guarantees. Was `p2`.
