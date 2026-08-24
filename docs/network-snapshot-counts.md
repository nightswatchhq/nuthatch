# Network-snapshot counts, from the mappings

#649. The numbers that disagree with `graphNetwork(id:"1")` on the network subgraph, and the
rules that actually produce them. Cited against `graphprotocol/graph-network-subgraph` on
`master` (`helpers.ts` as fetched 2026-08-24). #659 already did the curator half; this is the
rest, written down so the next person does not re-probe the chain.

**Do not index on a guess.** Every wrong answer so far has been a plausible event that the
mappings do not use that way.

## `curatorCount` / `activeCuratorCount` — rule known (#659)

`createOrLoadCurator()` in `src/mappings/helpers/helpers.ts` creates on first
`Curator.load(id)` miss (`id = curatorAddress.toHexString()`) and increments
`graphNetwork.curatorCount` once. Callers, with different meanings of "curator":

- **Curation** `Signalled` / `Burned` — `event.params.curator` is `msg.sender` to Curation.
  GNS-routed mints fire this too, with `curator = GNS's own address`, so they collapse to
  one entity.
- **GNS** v1 `NSignalMinted` / `NSignalBurned` (`nameCurator`) and v2 `SignalMinted` /
  `SignalBurned` (`curator`) — the real per-person identity. Same address, both ABI
  generations, one data source.
- **L2GNS** `SubgraphReceivedFromL1` / `CuratorBalanceReceived` — a Curator with no mint
  or burn, credited from an L1→L2 migration.

Active is the 0↔1 edge of `activeCombinedSignalCount` (direct vSignal ∪ GNS nSignal), not
every mint. L1GNS migration handlers do not decrement it — a leak in the *reference*
subgraph, reproduced if the target is the gateway's literal number.

`curation__signalled`-only decoding cannot see GNS identity. Distinct addresses on
`SignalMinted` alone (≈6,587) is not the rule either. The union above, keyed as
`createOrLoadCurator` keys it, is.

Applying it is a nest change plus a re-index. That is board work, not this document.

## `indexerCount` — rule known, three empty entities still unattributed

`createOrLoadIndexer()` in the same `helpers.ts` creates on first `Indexer.load(id)` miss
and increments `graphNetwork.indexerCount`. It does **not** require stake. Any handler
that calls it mints an Indexer, including ones that leave `stakedTokens = 0`,
`delegatedTokens = 0`, `allocationCount = 0`.

Known wrappers and callers (code search against the same repo, 2026-08-24):

- `createOrLoadLegacyIndexer` — calls `createOrLoadIndexer` and sets `isLegacy = true`.
- `src/mappings/horizonStaking.ts` — `event.params.serviceProvider`.
- `src/mappings/subgraphService.ts` — `event.params.serviceProvider` and
  `event.params.indexer`.

Chain probes on #649 attributed two of the five empty entities to
`DelegationParametersUpdated` and `DelegationFeeCutSet` (one each). Those are mapping
handlers that go through the helper above; they are not a "add two events" config line.

The remaining three empty entities were not attributed. Remaining mapping candidates,
not chain guesses: RewardsManager (its own address — a probe against the staking proxy
does not count), the **legacy** DisputeManager (the nest's disputes contract starts at
Horizon), GNS. #649 stopped rather than pull full history on a public RPC.

**Whether those three are worth closing at all** is a product call. They hold nothing.
Closing them is at least two handlers plus an unknown, for 2.7% on a count of empty
entities. This sprint does not add events to chase them.

`stakedIndexersCount` (entities with stake > 0) was a missing `StakeSlashed` on the
legacy ABI. That is a nest PR (`nightswatchhq/graph-allocations-nest#1`), set-exact at
97 vs the subgraph's entity set of 97. The subgraph's own `stakedIndexersCount` field
is 88. That nine-entity drift is theirs.

## What this repo will not do

- Add GNS `SignalMinted` because a distinct-address count was closer than 50.
- Reproduce the subgraph's 88-vs-97 `stakedIndexersCount` drift so the field matches.
- Re-index `graph-allocations-nest` from here. Archive RPC, nest content address, board.

A cited limit is a passing answer. A plausible event with no mapping citation is not.
