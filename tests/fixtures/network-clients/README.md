# Network client query contract

The non-`ts-` operations (excluding `schema.graphql`) are copied verbatim from `graphprotocol/indexer-rs`,
commit `b845e8fe715cb19e7154f1de65b78be1d53e3857`, directory
`crates/query/graphql`. Their Apache-2.0 licence is `LICENSE-indexer-rs`.

`schema.graphql` is copied verbatim from `graphprotocol/graph-network-subgraph`,
commit `3ca039189e35912729e878f54ceb1ae684276ae6`. Its MIT licence is
`LICENSE-network-subgraph`.

These are source contracts, not reconstructed field lists. In particular,
allocation epochs are `Int`, Horizon escrow uses `paymentsEscrowAccounts`, and
signers are selected through `payer`. The Rust monitors pin subsequent pages to
the hash returned by `meta: _meta` on the first page. Passing the first-page
compiler tests alone does not establish client compatibility: historical reads,
reorg handling, correct data and freshness must also pass before cutover.

`ts-monitor-*.graphql` contains the Network Subgraph operations, in source order,
from `graphprotocol/indexer` commit `d05af4be23950dfdfdf5bf22a8e0cafd9bbdea78`,
`packages/indexer-common/src/indexer-management/monitor.ts`. Operation 10 is
deliberately excluded: it queries the separate Epoch Block Oracle, not the
Network Subgraph. The upstream MIT licence is `LICENSE-indexer-ts`.

`ts-eligible-*.graphql` contains the three operations from the same commit's
`packages/indexer-common/src/allocations/monitor.ts`, with whitespace normalised.
This second monitor also queries `graphNetwork.currentEpoch`; pause detection is
not the only GraphNetwork dependency. The corpus currently has 21 operations.

Reference checked on 2026-09-17:

- Grafted hotfix: `QmR8WQECdNR6TSUf4FfLSFW7D5RDGGkd7F4A6me56pLqJb`.
- Fresh rebuild: `QmatH4cd25ymsfCFpgKPRQQ7uCYNgarxanVQ9WCfg4Y7TZ`.
- Query URL: `https://gateway.thegraph.com/api/deployments/id/<deployment>`.
- Authentication: `Authorization: Bearer <GRAPH_API_KEY>`. Do not persist keys
  in fixture files or URLs.

The graft was at block 506137990 with no indexing errors during the probe. The
fresh rebuild was at block 249681677, so it was not a current-state reference.
These are observations, not a continuing freshness guarantee. Probe `_meta`
again and pin comparisons to a block both endpoints actually serve.
