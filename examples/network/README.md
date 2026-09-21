# Network Subgraph facade workbench

Not a production replacement yet. Do not point an indexer-agent or tap-agent here.
The complete client contract and deployment acceptance gates remain the target;
this directory currently contains the independently tested escrow and allocation
lifecycle folds, not the complete Network Subgraph entity model.

With a complete-history policy enabled, entity queries accept any block number
within the indexed range, including blocks between retained checkpoints. The
entity folds filter source facts before aggregation. Historical `_meta` and
hash selectors still require a retained canonical checkpoint; unavailable hashes
or headers are refused, not fetched at query time or inferred from nearby blocks.
Complete historical metadata/hash coverage remains a publication gate.

The base vendored ABIs and mapping rules come from
`graphprotocol/graph-network-subgraph@3ca039189e35912729e878f54ceb1ae684276ae6`
(MIT, see `LICENSE-upstream`). The September changes were checked against
`bbc312511049bcc152e3c819a936cc53484f05bb` (PR #335, still unmerged when checked):
allocator-aware issuance and bounded YAML manifest parsing. The allocator's
RewardsManager-target update also refreshes the network epoch clock. This source
pin is not yet a reproducible-build match to the grafted deployment's WASM.
Contract addresses were checked against
`@graphprotocol/address-book@1.3.0`, Arbitrum One, on 2026-09-17. Start blocks come
from the existing full-history Lodestar nest. Archive probes on 2026-09-17 confirmed
empty bytecode at start minus one and nonempty bytecode at start for the ten
creation boundaries listed in DEPLOYMENT.md. Controller uses upstream's earlier
conservative boundary, not a claimed creation block. Deployment receipts have not yet been checked;
see DEPLOYMENT.md for the checkpoint evidence and remaining publication gates.

`CurationEvents.json` contains only the two event declarations from the MIT-licensed
subgraph manifest. It is not a vendored contract-package ABI or implementation.
`EpochManagerEvents.json` is likewise a minimal declaration of the two events in
that manifest, and `ControllerEvents.json` declares its five controller events.
Neither is a vendored contract-package ABI.

`views/05-epoch-schedule.sql` reconstructs epoch-length anchors from pinned L1
block reads. Mid-epoch length updates retain the old epoch start. The observation
clock in `26-network-clock.sql` follows mapping refresh events, including token
approvals, and models the cached L1 clock used by curation. Conditional save paths
and their interaction with curation still require validation. Only touched epochs
exist. `55-epochs.sql` assigns financial movements in receipt-log order;
`60-network.sql` projects the protocol controls, pause flags, issuance and clock.
These have fixture coverage and same-block genesis parity for 35 network fields
and 13 epoch fields, recorded in `validation/genesis-parity.json`. That early-chain
comparison is not live full-history parity evidence. Independently ingested cold
history also matches those 35 network fields and 13 fields across 145 epochs at
block 84363706, recorded in `validation/backfill-84m-reference.json`. Token-only
refreshes and POI presentations retain the saved network clock unless their
handler saves the network or creates an epoch. The complete
GraphNetwork field set remains unfinished. The larger checkpoint at block
123615752 matches all 35 network fields and 13 fields across 261 epochs; see
`validation/backfill-123m-check.json`. Existing-indexer delegation parameter
changes also retain the saved clock unless they create an epoch. The saved-clock
projection is in `48-network-clock-saved.sql`. Governor initialisation now uses a
hash-pinned getter at the first Controller event, before NewOwnership exists;
`validation/genesis-controls.json` matches controller, governor and pause guardian
against the reference and tests refusal of a mismatched-fork getter result.

At block 123615752, all 39 indexers also match 18 selected stake, allocation,
delegation, capacity and fee/reward fields. The cold-replay test reads the
independently ingested segments, not a synthetic balance fixture. This is a
historical checkpoint, not current-head indexer or client-process acceptance.

The next retained checkpoint, block 129252459, matches 35 network fields,
13 fields across 279 epochs, and ten fields for all 376 deployments. Its
delegation fold now recurses over reward events only, with additive balances
computed in receipt order; a 600-row differential test checks it against the
original fold. See `validation/backfill-129m-check.json` for the snapshot,
query timings and deployment status. These checks still do not establish
full-history serving capacity.

`views/10-escrow.sql` covers deposits, withdrawals, collections, thaw/cancel,
signer authorisation/thaw/revocation, and Tally redemptions. Amounts use DuckDB
`BIGNUM`, not fixed-width arithmetic. Redemption IDs use the actual log index
encoded as four little-endian bytes; allocation IDs use the final 20 bytes of
`collectionId`. Signer re-authorisation preserves an existing thaw deadline,
matching the upstream handler.
The account views matched all four selected balance fields for one real account
over 13,229 independently indexed logs at block 506000000; the exact query and
source totals are in `validation/escrow-parity.json`. A real redemption also
matches the grafted reference through the HTTP handler and the captured TAP
document. At block 502165911, all nine signers match the fresh rebuild through
HTTP using independently indexed authorization/thaw events and deposits that
establish payer identities. `validation/signers-reference.json` records that
comparison. The captured Rust signer-pagination document also checks the thaw
deadline boundary and `id_gt`. No cancellation, revocation or reauthorization
occurred in that captured history, so those paths still have synthetic coverage
only. This is not current-head signer or account-balance parity.

`views/20-allocations.sql` covers both legacy closure signatures and Horizon
creation, resize, POI and closure. A row-driven pinned `currentEpoch()` call is
required for Horizon closures, including forced closures with no rewards event.
A missing or reverted read is an error, not a guessed epoch. These calls require
an operator-supplied archive RPC when indexing.
Allocation creation and closure block numbers are L1 numbers on Arbitrum, read
from the pinned EpochManager clock; their hashes remain L2 hashes. The reference
capture in `validation/legacy-allocations-reference.json` records that distinction.
Four legacy allocations match its 13 selected fields through both SQL and the
HTTP GraphQL handler using independently indexed logs and hash-pinned reads.
Four Horizon allocations also match 15 selected fields through HTTP at block
506000000, including resizing, POI presentation and closure. Their independent
source fixture supplements Lodestar's missing POIPresented coverage with two raw
RPC logs decoded by the production registry. Current-head and full-indexer
comparisons remain pending.

`43-reward-distribution.sql` tracks indexer/delegator reward splits without
duplicating Horizon pool-addition events. The four Horizon allocations also match
all three reward counters against the reference at block 506000000 through HTTP;
the capture is in `validation/horizon-reward-distribution-reference.json`.
`42-query-fee-distribution.sql` reconstructs Horizon fee cuts and pool balances at
each fee event, plus legacy replacement versus Horizon additive allocation rebate
semantics. Fee splits and provision counters have ordered replay coverage, but
not live full-history parity yet. The final allocation projection is in
`44-allocation-state.sql`.
Indexer `queryFeesCollected`, `queryFeeRebates` and `delegatorQueryFees` accumulate
both legacy and Horizon movements. In particular, a later legacy rebate replaces
the allocation's counters but adds to the indexer's counters. The replay checks
these totals at three historical blocks; current-head parity remains unverified.

Every call declaration here sets `canonical = true`: the RPC must support
EIP-1898 `blockHash` with `requireCanonical`, with no fallback to a block number.
Source log hashes and timestamps must agree with the fetched header before a
call is admitted. Provider errors and missing batch items are not recorded as
EVM reverts. Hash-pinned call content addresses include the fork identity.
The nest requires config schema v3 and a build containing this implementation.
Older binaries must reject it, not silently ignore `canonical` and read another
fork by block number. Do not downgrade its `schema_version` to make it load.

`views/30-rewards.sql` follows RewardsManager parameter reads and the allocator's
`TargetAllocationUpdated`, selecting the RewardsManager target only. The
allocator ABI is from upstream PR #334, commit
`93bff46da62a831eed10d2e077b0fb22d1b97818`; its deployment block is recorded in the
official address book. No issuance rate or activation date is hardcoded in the
view. The deny-list view retains updates and removals at their event blocks.

`views/40-provisions.sql` covers the agent-selected Stage-1 provision balances and
parameters, including staging, thawing, deprovisioning, slashing and allocation
resizing. The upstream slashing handler leaves thawing unchanged; this fold follows
that behaviour rather than silently substituting a different accounting rule.
Provision thaw requests can create a zero-stake provision; their maximum deadline
is retained independently of token movements. Delegation thaw requests do not
create provisions through that handler.
The delegation ledger in `41-delegation.sql` and final projection in
`44-provision-state.sql` preserve the SubgraphService migration balance at the
first provision event, before that event's own delegation movement. Other
verifiers start at zero. Legacy rewards update an existing SubgraphService
provision; deposits, undelegation, partial withdrawal and slashing remain scoped
to their verifier. These balances have ordered replay coverage, not live parity
or full-history performance evidence yet.
Provision URL, geohash and rewards destination follow valid SubgraphService
registrations and subsequent destination changes. Malformed registrations leave
the last valid values unchanged, and metadata is not copied to other verifiers.

`46-capacity.sql` reconstructs the indexer's cached `delegatedCapacity`,
`tokenCapacity` and `availableStake` at the last upstream capacity-refresh event.
It uses the balances and protocol parameters at that event, including the
Horizon formula selected by `maxThawingPeriod`. Parameter changes and Horizon
stake deposits do not independently refresh those cached fields. The replay
checks twelve historical states; live parity and full-history query performance
remain unverified. The indexer projection is in `47-indexer-state.sql`.
The twelve-state debug replay took 32.26 seconds before dependency cleanup and
13.30 seconds afterwards in single local runs, without changing the five-second
per-query guard. `validation/local-capacity-timings.json` records the workload and
limitations. These figures do not establish public-endpoint capacity or memory use.
The ThinkPad release build passed all 17 network fixtures in 21.64 seconds.
A separate single capacity replay took 6.19 seconds with 98,764 KiB peak RSS.
`validation/linux-fixture-validation.json` pins the tested source fingerprints
and records which later changes were not included. These are small-fixture
measurements, not a running full-history cursor's resource or freshness results.

The public Arbitrum RPC rejected historical state probes on 2026-09-17. It is not
a suitable archive backend for the pinned reads; configure an archive-capable RPC
before attempting a full replay.

Tests use the actual ABI-derived table schemas and replay fixture events through
the production analytical path:

```sh
cargo test --test it network_contract
cargo test --lib graph_history
cargo test --lib historical_
cargo test --lib checkpoint_hash_index
```

Still required before publication: remaining protocol contracts and parameter
reads; complete indexer/provision/deployment/epoch/dispute entities; historical
block coverage; full backfill; same-block reference parity; real client smoke
tests; measured freshness and resource use; and the public host/TLS/limits/alerts.
Compilation and fixture replay are not substitutes for these gates.
