# RFC-0063: Address history - an Etherscan-compatible local source for rotki

- Status: **Accepted by Chief, 2026-10-09.** Locked by Jenny and Codex in two rounds the same day.
- Author: Pete (cargopete)
- Date: 2026-10-09
- Reopens: #272 (watched positional filters), #277 (traces, from a remote RPC), #308 (bounded blocks
  and transactions). #276 (a colocated node) stays closed unless measured coverage or cost demands it.
- Nature: new binary capability. Per RFC-0044 §8 and CLAUDE.md it is decided by Chief and recorded,
  or it does not happen. Chief: "this is our first potential serious user... if we have to extend
  nuthatch we will."

## 1. The user and what he asked for

rotki is a local-first portfolio and accounting app. On 2026-10-09 its founder answered
rotki/rotki#13335: "Absolutely would add a local indexer as a source." What rotki gets from Etherscan
and Blockscout today is the set of transactions where a user's address appears as a normal
transaction, a token transfer or an internal transaction; timestamp to block number; and on mainnet,
staking withdrawals, MEV and block production.

The free versions of those sources closed in 2025-2026: Etherscan's free tier dropped chains on
2025-11-22, Blockscout keyed every call from 2026-07-01, and Routescan serves Ethereum only. A user
without a paid key now gets empty history on Base, Optimism and Gnosis.

## 2. How rotki consumes an indexer (read at rotki 2783d11)

One abstraction, `EtherscanLikeApi` (`rotkehlchen/externalapis/etherscan_like.py`), with indexers
tried in a user-set order (`chain/evm/node_inquirer.py`, `_get_indexers_in_order`). **A failure falls
through to the next indexer; an empty success does not.** So an indexer that cannot answer must fail,
never return `[]`.

| Action | What rotki reads |
|---|---|
| `account/txlist` | `hash, blockNumber, timeStamp, from, to, value, input, gas, gasPrice, gasUsed, nonce`, optional type, authorisations, L1 fee. Saved as full records. |
| `account/txlistinternal` | parent `hash, blockNumber, timeStamp, from, to, value, traceId, gas, gasUsed, callType`; delegatecalls skipped; also queried by `txhash`. `traceId` is parsed as an integer. |
| `account/tokentx` | `hash, timeStamp, blockNumber` only. Hashes are hydrated over RPC. |
| `block/getblocknobytime` | `timestamp, closest=before|after`, a decimal block number. |
| `account/txsBeaconWithdrawal` | `blockNumber, withdrawalIndex, validatorIndex, timestamp, amount` (gwei). |
| `account/getminedblocks` | `blockNumber, timeStamp, blockReward` (wei). |

Account lists page at 1,000 (Etherscan) or 10,000 (Blockscout) rows and restart at the last block,
inclusively. **A single block holding a full page does not progress.** That trap is rotki's
(`etherscan_like.py:562`) and the adapter in §6 overrides it.

## 3. Architecture

**A local rotki-mode nest per user.** Config names the watched addresses, the chains, the history
bounds and the RPCs. Per-user history never leaves the user's machine, except as the address filters
the upstream RPC necessarily sees. One cursor per chain, as always.

**Hosted global nests for what is address-free.** Timestamp to block number reveals nothing about a
user, so one headers nest per chain on our boxes answers it for every rotki user, and appears in the
Hosted section of nuthatch-indexer.com/nests. Per-address history is never hosted: a hosted service
would have to index every transfer and transaction on a chain, and would see every user's wallet.

**Polling.** Rotki reads history, not the tip. A rotki-mode nest polls every **5 minutes** by default
(Chief, 2026-10-09), so an idle nest costs a handful of calls an hour.

## 4. Discovery

- **Token transfers.** `eth_getLogs` with no emitter and a positional address filter: ERC-20 and
  ERC-721 `Transfer` with the address in topic 1, then topic 2; ERC-1155 `TransferSingle` and
  `TransferBatch` with the address in topic 2, then topic 3. Union, deduplicated by (tx, log index).
  Today's `LogFilter` carries emitter and topic0 only, and the union fetch can drop the emitter
  restriction; positional predicates must survive every fetch path.
- **Normal and internal transactions.** `trace_filter` with `fromAddress`, then separately
  `toAddress` (a combined filter can mean intersection), unioned. Root frames are the normal
  transactions; non-root value-carrying frames are the internal ones. Reverted effects are excluded,
  including successful children of a reverted ancestor. Creations and selfdestruct beneficiaries are
  normalised. `traceId` is a stable integer derived from the frame's position in the whole
  transaction's trace, so an address query and a `txhash` query give the same id.
- **Nonce search is a cross-check, not a source.** `eth_getTransactionCount` is monotone, so a binary
  search finds every block where an EOA's nonce moved. It checks outgoing completeness cheaply. It
  is not sufficient alone: EIP-7702 authorisations move the nonce without a sent transaction, and
  ERC-4337 operations are sent by a bundler. Balance search is unsound and is not used.
- **Where `trace_filter` is unavailable**, completeness needs every block traced (`trace_block`),
  shared across watched addresses. That is the fallback, priced per chain in §7.
- **Hydration.** Every discovered hash is fetched with its receipt and block timestamp.

Coverage is tracked per (chain, address, action, block interval). Adding an address backfills it.
A reorg invalidates the affected tail. Unsupported, incomplete and genuinely empty are three
different outcomes.

## 5. The HTTP surface

`GET /api` on the nest, Etherscan-shaped: `chainid, module, action`, and for account lists
`address, startblock, endblock, sort, page, offset`, inclusive bounds, ascending by default, 1,000
rows a page. `txlistinternal` also takes `txhash`. Actions: the six in §2, plus `account/balance`,
the `proxy` RPC passthroughs rotki uses (`eth_blockNumber`, `eth_getBlockByNumber`,
`eth_getTransactionByHash`, `eth_getTransactionReceipt`, `eth_getCode`, `eth_call`) and
`logs/getLogs`. `contract/getabi`, `contract/getcontractcreation` and `block/getblockreward` answer
unsupported until built.

Success is `{"status":"1","message":"OK","result":...}`; a covered empty list is `[]`. Anything the
nest cannot answer completely is `{"status":"0","message":"NOTOK","result":"NUTHATCH_UNSUPPORTED: ..."}`
or `NUTHATCH_INCOMPLETE: ...`, which the rotki adapter turns into an error, so rotki falls back.
Quantities are decimal strings, as Etherscan's are; `to` is empty for a creation; failed and
zero-value transactions are kept.

## 6. The rotki side

A `Nuthatch` indexer in rotki, configurable by URL, registered in the explorer priority list and in
the separate withdrawals path. It pages with fixed bounds and an incrementing `page` over a pinned
snapshot, and fails on a repeated page rather than looping. We write it and offer it as a PR to
rotki once every slice below passes.

## 7. RPC and cost

| Chain | Source | Traces |
|---|---|---|
| Ethereum | GraphOps | `trace_filter` and `trace_block` answer (probed 2026-10-09) |
| Base | GraphOps | `trace_block` answers; `trace_filter` to be confirmed on a non-empty range |
| Arbitrum | Alchemy | GraphOps serves no trace methods (probed 2026-10-09) |
| Optimism, Gnosis, Polygon, BSC, Scroll | Alchemy | to be probed per chain before its slice |

Filtered traces are cheap: two directional queries over a million blocks at 10,000-block windows are
about 200 requests plus pagination and hydration. Block-by-block tracing is the expensive fallback and
is used only where a chain has no `trace_filter`. Every figure here is to be replaced by a measured
one before the chain ships.

## 8. MEV and block production

Withdrawals come from block bodies (EIP-4895). Fee-recipient blocks come from headers; rewards from
receipts' priority fees. **MEV stays on rotki's existing beaconcha.in path at first**: relay-reported
MEV amounts and proposer attribution are not in execution RPC, and the Beacon API alone does not
carry relay payments. Missing relay evidence is unknown, never zero.

## 9. Slices and their gates

Every gate is parity against Etherscan for a pinned wallet and range, exact hash sets and the fields
rotki reads, run on both boxes.

1. **Coverage, `/api` and pagination.** 1,001 records in one block page through with exact
   identities; an uncovered range answers `NUTHATCH_INCOMPLETE`; a reorg invalidates the tail.
2. **Headers and `getblocknobytime`.** Sampled before/after answers match Etherscan and satisfy the
   adjacent-header inequalities; the hosted headers nest stays under 2 GB RSS while ingesting and
   answering concurrently.
3. **Token transfers and normal transactions.** vitalik.eth (`0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045`),
   Ethereum blocks 18,000,000 to 20,000,000: `tokentx` and `txlist` hash sets equal Etherscan's, and
   `txlist` field parity. A non-empty wallet and range is pinned per further chain before its turn.
4. **Internal transactions.** The same wallet and range, plus pinned fixtures for an internal-only
   receipt, a failed ancestor, a creation, a selfdestruct and an EIP-7702 authorisation. Field parity
   and identical `traceId`s by address and by `txhash`.
5. **Withdrawals and block production.** Lido Withdrawal Vault
   (`0xB9D7934878B5FB9610B3Fe8A5e441e8faF7E293f`), Ethereum 17,034,870 to 17,044,870: exact
   withdrawal indices and amounts. Pinned proposer fixtures for `getminedblocks` and reward parity.
6. **The rotki adapter.** rotki's own test suite passes with nuthatch first in the order, and a
   fresh rotki profile for a pinned wallet produces the same history events as with Etherscan.

Chains go Ethereum first (every slice), then Base, then the rest in the order rotki's users need.

## 10. What this does not do

No hosted per-address history. No relay-reported MEV in v1. No promise of completeness on a chain
until its gates pass; until then that chain answers `NUTHATCH_UNSUPPORTED` and rotki falls back.
