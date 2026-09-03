# RFC-0050: Robinhood Chain support

- Status: **Draft. Under the 2026 feature freeze this is a proposal, not work to start.** A new
  built-in chain is new capability, both carve-outs are spent, and implementing this needs a
  carve-out recorded in `CLAUDE.md` first. Nothing in this document starts, reorders or unblocks
  a slice.
- Author: Pete
- Date: September 2026
- Tracking issue: [#1133](https://github.com/nightswatchhq/nuthatch/issues/1133)
- Target release: written as "2.x (post-2.4.0 unlisted-chain support)"; the tree is at 3.x, see
  the maintainer addendum at the end.

## Summary / TL;DR

Add Robinhood Chain (chain ID **4663**, mainnet) — Robinhood's Arbitrum Orbit / Nitro Ethereum L2 for tokenized equities and RWAs — as a **built-in chain** in Nuthatch, joining the existing seven (Ethereum, Arbitrum One, Base, BSC, Polygon, Gnosis, Optimism). Because Robinhood Chain runs the same Arbitrum Nitro execution stack as the already-supported Arbitrum One, this is a **configuration + finality-policy change, not a new adapter**: Nuthatch's ingestion path is chain-agnostic and parameterised by config. The main real work is (1) picking a correct finality policy given a centralized sequencer and blob-DA rollup, (2) confirming the generic EVM decode path tolerates Arbitrum receipt/tx quirks (`l1BlockNumber`, `gasUsedForL1`, ArbOS system tx types 100–106, `block.number` = L1 estimate), and (3) shipping a measured public endpoint plus tuned defaults so `init` → `dev` works with zero setup. Robinhood Chain is already live (mainnet 2026-07-01), and The Graph lists it as a supported network while Goldsky and Ormi both ship Robinhood Chain subgraphs — so there is clear, demonstrated Graph-ecosystem demand.

**Three-line answer to "should we do this, and how":**
- **Yes, and cheaply** — it is a registry entry riding Nuthatch's existing chain-agnostic Arbitrum-Nitro-capable ingestion path, not a new adapter.
- **The one decision that matters** is the finality/seal boundary: **seal at the `safe` block tag** (batch posted to L1), never at the soft-confirmed `latest` tip, and size the hot store in **blocks** for the ~100ms (~10 blocks/sec) cadence.
- **Everything else is measurement and docs** — measure the public endpoint with `nuthatch doctor`, steer real workloads to a paid/archive RPC, and confirm two open facts (registry file/schema; testnet chain ID) before merging.

## Motivation

- **Tokenized-equity / RWA data is a first-class new dataset.** Robinhood Chain's flagship product is Stock Tokens — ERC-20 tokens tracking listed equities (NVDA, AAPL, GOOG, etc.) that trade 24/7. These are standard ERC-20s (`Transfer`, `balanceOf`, `totalSupply` behave normally), which is exactly Nuthatch's sweet spot: `init 0xADDR` → per-event tables → SQL. The addressable niche is real but still early: per CoinMarketCap Academy, Robinhood "commands roughly 44% of all tokenized-stock holders," though "Robinhood's tokenized equity value still sits at just $28 million" — i.e. lots of holders/accounts (holder-distribution and 24/7-flow analytics, Nuthatch's strengths) against a still-small notional. That is a dataset that will grow, and getting in early is cheap.
- **Graph-ecosystem demand is already proven.** The Graph lists Robinhood Chain Mainnet as a supported network, and both Goldsky and Ormi ship Robinhood Chain subgraph support today. Goldsky states plainly: "Goldsky Subgraphs run on Robinhood Chain and are fully compatible with The Graph protocol, so you can migrate existing subgraphs with a single CLI command. Queries are served via a standard GraphQL API with sub-second indexing latency." Nuthatch's positioning ("no subgraph? no problem") maps directly onto teams that want a self-hosted SQL alternative to a Robinhood Chain subgraph.
- **Low marginal cost.** Robinhood Chain is Arbitrum Nitro, and Nuthatch already ships Arbitrum One. If the generic path already handles Arbitrum One correctly, Robinhood Chain is mostly a new row in the built-in chain registry with a finality policy chosen for its sequencer/DA characteristics.
- **It already works today as an unlisted chain.** Since v2.4.0, `nuthatch init 0xADDR --chain robinhood --rpc https://…` already scaffolds and indexes any EVM chain. This RFC promotes Robinhood Chain from "works if you bring your own RPC and tune finality" to "built-in, measured endpoint, tuned defaults, zero setup."

## Background on Robinhood Chain (verified facts + sources)

### Identity and status

| Property | Mainnet | Testnet |
|---|---|---|
| Chain ID | **4663** (`0x1237`) | **46630** ("4663 with a zero appended"; one aggregator erroneously lists 46646 — see risks) |
| Native gas token | ETH | ETH (test) |
| Public RPC | `https://rpc.mainnet.chain.robinhood.com` | `https://rpc.testnet.chain.robinhood.com` |
| Block explorer | `robinhoodchain.blockscout.com` (Blockscout) | `explorer.testnet.chain.robinhood.com` |
| Sequencer feed | `wss://feed.mainnet.chain.robinhood.com` | `wss://feed.testnet.chain.robinhood.com` |
| Sequencer | `https://sequencer.mainnet.chain.robinhood.com` | `https://sequencer.testnet.chain.robinhood.com` |

- **Status:** Public testnet launched **2026-02-10** (announced at Consensus Hong Kong; ~4M transactions in its first week per Robinhood CEO Vlad Tenev). Public mainnet launched **2026-07-01**, announced at Robinhood's "The World is Flat" keynote at the Old Royal Naval College in London, shipping with Uniswap and Chainlink integrated from day one; Offchain Labs CEO Steven Goldfeder said "Robinhood Chain is well-positioned to help the industry deliver the next chapter of tokenization" (per the Robinhood newsroom).
- **Scale (Sept 2026):** L2BEAT lists "Total Value Secured TVS · $1.76 B." Activity is substantial — L2BEAT reports "Total Ops 544.89 M since 2026 Apr 30" and a max of 303.41 UOPS (2026-08-12). (An earlier draft's "~12M ops/day" figure did not reconcile with L2BEAT's live per-second UOPS reading and has been dropped as unverified; pull the current daily ops figure from L2BEAT at merge time.)
- Sources: Robinhood docs `docs.robinhood.com/chain/connecting`; L2BEAT `l2beat.com/layer2s/projects/robinhood`.

### Architecture

- **Arbitrum Orbit L2 on the Nitro stack**, settling to **Ethereum L1** with **blob (EIP-4844) data availability** — this is a full rollup with all data posted on-chain, **not** an AnyTrust/DAC chain. L2BEAT confirms "All data required for proofs is published on chain" and categorises DA as "Onchain." Gas token is ETH (no custom gas token). This is the same configuration Arbitrum One uses, which is why day-one Ethereum tooling worked unmodified.
- **ArbOS version 61** (`wasmModuleRoot` v61, shared with Arbitrum One and Nova per L2BEAT's program-hash table).
- **Block time ~100ms** (soft confirmations). Note: Arbitrum's *default* block time is 250ms with 100ms as an opt-in lower bound (per Arbitrum docs); Robinhood Chain runs the 100ms configuration. ≈10 blocks/sec.
- **Centralized sequencer**, first-come-first-served ordering by arrival time (no priority-gas auction). Single sequencer / single batch-poster EOA at launch.

### Finality and reorg model (the load-bearing part for an indexer)

Three stages, all observable over RPC:

| Stage | Latency | Meaning for indexing |
|---|---|---|
| Soft confirmation (sequencer) | Sub-second | Ordering promised by sequencer; reversible if sequencer posts a different order. `latest` tag. |
| Posted to Ethereum (batch in L1 Inbox) | Minutes | Ordering fixed; can only reorg if Ethereum reorgs. `safe` tag ≈ newest block whose batch is posted to L1 (~5–15 min behind `latest`, community-measured). |
| Ethereum finality | ~13 min after posting | Irreversible, inherits Ethereum security. `finalized` tag. |

- **`safe` and `finalized` block tags are both supported** and are the correct seal boundary for Nuthatch. Robinhood's docs commit only to "minutes" for `safe`; the "~5–15 min behind `latest`" figure is community-measured (QuickNode) and should be re-checked against the live tags.
- Reorg semantics, verbatim from Robinhood's finality doc: "Once a transaction is posted to Ethereum, it cannot be reorganized unless Ethereum itself reorganizes." The only genuinely mutable window is the soft-confirmed tip.
- Withdrawals to L1 carry a ~7-day challenge period (Arbitrum fraud-proof requirement); this is separate from transaction finality and does not affect indexing.
- Sources: `docs.robinhood.com/chain/transaction-finality`; QuickNode Robinhood guide; Arbitrum docs.

### Arbitrum/Nitro EVM differences that touch an indexer

- **Receipt fields:** Arbitrum adds `l1BlockNumber` and `gasUsedForL1` to every receipt. Generic EVM receipt parsers must tolerate these extra fields (they are optional/allow-null in ethers.js, etc.).
- **Transaction types:** ArbOS adds tx types **100 (ArbitrumDepositTx), 101 (ArbitrumUnsignedTx), 102 (ArbitrumContractTx), 104 (ArbitrumRetryTx), 105 (ArbitrumSubmitRetryableTx), 106 (ArbitrumInternalTx)** — the last is ArbOS-generated for L1 base-fee/block-number state updates. A decoder that hardcodes tx types 0/1/2 must not choke on these; `eth_getTransactionByHash` reflects the custom `type` codes.
- **`block.number` returns an L1 estimate**, not the L2 block number; the true L2 block number comes from the `ArbSys` precompile (`0x…0064`, `arbBlockNumber()`). Nuthatch indexes by the RPC block number (which over `eth_*` RPC is the L2 block number), so log/block ordering is unaffected — but any user-authored view that treats a `block.number`-derived on-chain value as an L2 height must know this.
- **Block header quirks:** `difficulty` fixed at `0x1`; `mixHash` encodes `sendCount` (first 8 bytes) and `l1BlockNumber` (second 8 bytes); `block.prevrandao`/`difficulty` are constant (not randomness); `blockhash(n)` reliable only for recent blocks; `block.coinbase` is the network fee account.
- **Precompiles:** ArbSys (`0x64`), ArbGasInfo, ArbAddressTable, ArbRetryableTx, and (ArbOS 61) ArbFilteredTransactionsManager (`0x74`).
- **Contract size limit** 96 KB (vs Ethereum's 24 KB); init code 192 KB — irrelevant to log indexing but relevant to `[[calls]]` targets.
- Sources: `docs.robinhood.com/chain/differences-from-ethereum`; Alchemy Arbitrum-vs-Ethereum differences; Arbitrum ArbOS/Geth docs.

### Robinhood-specific customization: sequencer-level transaction screening

- Robinhood Chain runs **ArbOS 61 compliance filtering** via the `ArbFilteredTransactionsManager` precompile (`0x00…0074`). An authorized filterer registers tx hashes; the state-transition function then forcibly fails those transactions (including force-included ones), without delay. L2BEAT flags this as a censorship vector and records >6,000 filtered transactions as of mid-2026 (the jump from 278 → 6,086 in July 2026 was one blocked honeypot wallet behind a fake "Robinhood founder seed-phrase leak").
- **Indexer impact is benign.** Robinhood's docs state read operations (`eth_call`, `eth_getLogs`, balance queries) are unaffected, and a blocked transfer "simply appears as though the event never occurred, ensuring indexers remain synchronized with the actual state." Nuthatch indexes emitted logs, so a filtered (never-executed) tx emits no logs and needs no special handling.

### Security posture (state for users, not an indexing blocker)

Per L2BEAT, Robinhood Chain is **below Stage 0**: centralized sequencer, instantly-upgradeable system contracts (no delay), fraud proofs deployed but permissioned. Per Crypto Times citing L2BEAT's July 2026 assessment, "only two whitelisted actors could challenge incorrect state updates," and the chain uses Arbitrum's BoLD dispute system. This is chain-security context, not an indexing-correctness concern.

### RPC availability and quirks

- **Public RPC is rate-limited with no archive.** Per Chainstack, the ~100ms block time means an indexer polling every block issues roughly ten times the requests the same code would on Ethereum, so a rate limit that feels generous on L1 is exhausted quickly. Not suitable for deep backfill.
- **Archive + trace/debug + `eth_getLogs` at scale** are available via dedicated providers: Alchemy (Robinhood-recommended; `https://robinhood-mainnet.g.alchemy.com/v2/{KEY}`), QuickNode, Blockdaemon, dRPC, Validation Cloud, Chainstack, Dwellir, Goldsky. `eth_getLogs`, `trace_*`, and `debug_*` are all offered; exact per-provider block-range caps must be **measured** with `nuthatch doctor`.
- Blockscout (`robinhoodchain.blockscout.com`) is the official explorer from block one; Etherscan does not index chain ID 4663.

## Detailed design

### 1. Chain registration

Add Robinhood Chain to Nuthatch's built-in chain registry alongside the existing seven. Per the README, built-in chains ship "with measured public endpoints and tuned finality settings," and omitting `--chain` makes Nuthatch "probe each for your contract's bytecode and pick the one it lives on."

> **[VERIFY — file path/schema]** The README references built-in chains but does **not** name the file that stores them, and the internal `src/` tree could not be fetched during research. The maintainer must confirm the actual location and schema (a `chains.json`-style asset regenerated into `src/generated/`, or a Rust table in `src/`) before writing the diff. Do **not** assume a path. The fields below are the *logical* registry entry; map them onto whatever the real struct/JSON uses.

Logical registry entry (illustrative — adapt field names to the real schema):

```jsonc
{
  "name": "robinhood",              // canonical --chain name
  "aliases": ["robinhood-chain", "hood"],
  "chain_id": 4663,
  "display_name": "Robinhood Chain",
  "native_token": "ETH",
  "explorer": "https://robinhoodchain.blockscout.com",
  "rpc_urls": [
    "https://rpc.mainnet.chain.robinhood.com"   // measured public endpoint; rate-limited, no archive
  ],
  "finality": { "policy": "tag", "tag": "safe" },   // see §4 for the tag-vs-depth decision
  "getlogs_window": 10000,          // [VERIFY] measure with `nuthatch doctor`
  "family": "arbitrum-nitro"        // if such a discriminator exists; else omit
}
```

Also consider a **`robinhood-testnet`** entry (chain ID 46630) — useful for CI fixtures and for users validating before touching mainnet. Mark testnet as non-default so bytecode probing never accidentally selects it.

### 2. Which adapter to reuse

**Reuse the existing generic EVM ingestion path — do not write a Nitro adapter.** Evidence from the README: the ingestion/decode/serve stack is explicitly "chain-agnostic," Arbitrum One is already built-in, and multi-chain runtimes already run "a Base nest and an Arbitrum nest in one runtime." Robinhood Chain should ride the same path Arbitrum One rides.

The engineering task is therefore **confirmation, not construction**: verify the generic receipt/tx/block parsers already tolerate Arbitrum's `l1BlockNumber`/`gasUsedForL1` receipt fields and tx types 100–106 (they must, for Arbitrum One to work today). If Arbitrum One indexes cleanly, Robinhood Chain inherits that for free. If there is any Arbitrum-One-specific special-casing keyed on chain ID `42161`, generalise it to the whole `arbitrum-nitro` family.

> **[VERIFY]** Whether Nuthatch has any per-chain-family discriminator (e.g. an `arbitrum-nitro` marker) or is purely config-driven could not be confirmed from source. The design language in the README points to "purely config-driven, no per-chain adapters," but `src/` was not fetchable during research.

### 3. Block and transaction model → Nuthatch storage schema

Nuthatch's per-event tables carry `block_number`, `block_hash`, `block_timestamp`, `tx_hash`, `log_index`, `address`, and a `_seq` ordinal. All of these map cleanly from Robinhood Chain over standard `eth_*` RPC:

- `block_number` = the **L2** block number (RPC returns L2 height as the standard `number` field; `l1BlockNumber` is a separate field). Correct for ordering and for `getLogs` windowing.
- `block_hash` = L2 block hash — stable and usable as a reorg-detection key at the soft-confirmed tip.
- `block_timestamp` = L2 block timestamp. The ~100ms cadence means timestamps between adjacent blocks may be equal or near-equal; any time-bucketed view must not assume strictly-monotone per-block timestamps. The `--no-timestamps` init option is attractive here because the ~100ms cadence multiplies the per-block header round-trip cost (the README notes `block_timestamp` is ~85% of backfill wall clock on other chains).
- Arbitrum's extra receipt fields (`l1BlockNumber`, `gasUsedForL1`) are **not** part of Nuthatch's event schema and can be ignored by the decoder; no schema change is required.
- ArbOS system transactions (type 106) emit no user logs of interest and need no table.

**No schema migration is required.** This is purely additive — a new chain, same table shapes.

### 4. Finality and reorg strategy (the decision that matters)

Nuthatch supports two finality policies per the README's unlisted-chain note: a **`finalized`/`safe`-tag policy** and a **depth-based-confirmations policy**, and warns that "a chain whose `finalized` tag runs close to the tip needs a depth-based policy instead, or you seal immutable Parquet that could never be corrected."

For Robinhood Chain the tags are well-defined and *not* dangerously close to the tip:

- **Recommendation: seal at the `safe` tag** (default), with `finalized` available for the paranoid. Rationale: on an Arbitrum Nitro rollup, a block cannot reorg once its batch is posted to L1 (the `safe` tag), except under an Ethereum reorg. `safe` runs a few-to-~15 minutes behind `latest`; sealing there gives a large margin over the soft-confirmed tip while keeping seal latency to minutes rather than the ~13+ min of full `finalized`.
- **Do not seal at the soft-confirmed `latest` tip.** The centralized sequencer can in principle re-order before posting a batch; sealing there risks immutable Parquet that contradicts the eventual posted order.
- **Reorg handling at the tip is unchanged:** reorgs only ever touch the redb hot store, and Nuthatch tracks `block_hash` to detect a replaced block. The one Robinhood-specific consideration is that soft-confirmed blocks are more mutable than an Ethereum block of the same age, so **the hot store must span at least the soft→`safe` gap** — minutes of ~10 blocks/sec means thousands of blocks. Ensure the hot-store depth / window default is sized in **blocks** for the 100ms cadence, not copied from a 12s-block chain.

> **[VERIFY]** Exact Nuthatch config keys/enum values for "tag vs depth" finality and hot-store depth were not confirmable from source. Map the above onto the real `finality` config surface.

### 5. Head tracking and sequencer feed

- **Default:** poll `eth_blockNumber` / `eth_getLogs` like every other Nuthatch chain. At ~10 blocks/sec, tune the poll interval and window so tip-following does not hammer the endpoint (see §7).
- **The Nitro sequencer feed (`wss://feed.mainnet.chain.robinhood.com`) is out of scope** for this RFC. It offers a lower-latency stream of soft-confirmed blocks, but Nuthatch's model deliberately seals only past finality and its ingestion is JSON-RPC-based. A feed-based head tracker is a possible future optimisation, not a requirement — see Future Work.

### 6. RPC client configuration

- Ship the **measured public endpoint** (`https://rpc.mainnet.chain.robinhood.com`) as the zero-setup default, explicitly documented as testing-only (rate-limited, no archive), consistent with how Nuthatch frames its other bundled public endpoints ("the on-ramp, not the road").
- **Strongly steer real workloads to a paid/own endpoint** via `--rpc` (repeatable, round-robined with per-endpoint health tracking) or `rpc_urls` in `nuthatch.toml`. Recommend Alchemy / QuickNode / dRPC / Chainstack / Dwellir / Goldsky for archive + `eth_getLogs` at scale. All endpoints in a pool must be the same chain — Nuthatch verifies this at startup.
- **`eth_getLogs` window:** measure with `nuthatch doctor --rpc <url> --address 0xADDR` and set the built-in default to the measured widest range for the public endpoint; do **not** copy Arbitrum One's number blindly. A given block range covers far less wall-clock time on a 100ms chain but potentially more events per unit time, so window tuning is genuinely different from a 12s-block chain.
- **Batch limits / rate limits:** the RFC-0028 ingestion hardening (split oversized `getLogs`, take provider-suggested ranges, classify rate-limit vs transport vs credential failures, split-once on unclassifiable failures) already covers the failure modes a busy 100ms chain will hit — including Alchemy's HTTP-400 oversized-range refusal that RFC-0029/v0.9.0 addressed. No new ingestion logic is needed; only defaults tuning.

### 7. Historical backfill strategy

- Robinhood Chain mainnet genesis is **2026-07-01**, so full history is short in wall-clock terms but potentially large in block count (~10 blocks/sec). For a contract deployed near genesis, backfill is bounded and feasible on an archive endpoint.
- The public endpoint has **no archive guarantee**; deep backfill must use an archive provider. Document this prominently (mirrors Nuthatch's existing framing).
- `--window`, `--concurrency`, and `--seal-direct` are the tuning knobs; `--seal-direct` is attractive for a one-shot genesis-to-tip backfill.

### 8. CLI / config surface changes

- New built-in `--chain robinhood` (+ aliases), and optionally `--chain robinhood-testnet`.
- Bytecode auto-probe (omit `--chain`) should include Robinhood Chain mainnet in the probe set; **exclude testnet from auto-probe** to avoid mis-selection.
- `--rpc` remains ignored for the built-in name (per README: "a built-in chain never dials").
- No new flags required; this is a registry addition.

### 9. Observability / metrics

- Existing Prometheus `/metrics` (tip lag, rows decoded/sealed, reorgs, query counts, RSS) apply unchanged.
- **Doc note recommended:** on a 100ms chain, "tip lag in blocks" reads ~10× higher than on Ethereum for the same wall-clock lag; alert thresholds should be set in wall-clock or scaled for the cadence.

### 10. Documentation changes

- Add Robinhood Chain to the built-in chains list in `README.md` and to `docs/operators.md`.
- Add a note under `docs/operators.md#running-an-unlisted-evm-chain` cross-referencing that Robinhood Chain is now built-in.
- Document the Arbitrum-family caveats (100ms cadence, `safe`-tag seal boundary, public-endpoint archive limitation, compliance filtering) once, shared with Arbitrum One where possible.

## Testing plan

- **Unit tests:** registry entry resolves (`--chain robinhood` → chain ID 4663; alias resolution; testnet 46630). Finality policy parses to `safe`-tag. Bytecode-probe set includes mainnet, excludes testnet.
- **Fixture-based decode tests (the required gate):** capture real Robinhood Chain blocks/receipts — including at least one ArbOS type-106 internal tx and one receipt carrying `l1BlockNumber`/`gasUsedForL1` — as committed fixtures; assert the generic decoder ignores the extra fields and produces correct event rows. A Stock Token `Transfer` fixture is the canonical case.
- **Integration test against public RPC:** a lightweight `dev` run against `rpc.mainnet.chain.robinhood.com` over a small block range for a known Stock Token, asserting row counts match Blockscout. Gate behind a **network-optional, non-blocking** CI job (the public endpoint is rate-limited/flaky).
- **`nuthatch doctor` snapshot:** record the measured `getLogs` window, batch limit, and archive depth for the public endpoint, and use it to set/verify the built-in default. Re-run periodically since public endpoints drift ("a measurement is a snapshot, not a property").
- **CI considerations:** deterministic decode tests are the required gate; keep the network-dependent Robinhood job non-blocking. Follow the existing OBIB pattern (nests run against a real provider via `--rpc` with a key; keyless committed nests receive the endpoint via `--rpc`).

## Rollout plan / phases

1. **Phase 0 — validate as unlisted (now):** run `nuthatch init 0xADDR --chain robinhood --rpc <archive-endpoint>` against a live Stock Token; confirm the generic path indexes Robinhood Chain end-to-end and matches Blockscout / a public subgraph. De-risks everything before touching the registry.
2. **Phase 1 — testnet entry + fixtures:** add `robinhood-testnet` (46630), capture fixtures, land deterministic decode tests.
3. **Phase 2 — mainnet built-in:** add `robinhood` (4663) with measured public endpoint and `safe`-tag finality default; wire into bytecode auto-probe; docs.
4. **Phase 3 — hardening:** measure and tune `getLogs` window / hot-store depth for the 100ms cadence; add the network-optional integration job; observability doc note.

**Benchmarks / thresholds that change the plan:**
- If Phase 0 shows the generic path *cannot* decode a Robinhood Chain block that Arbitrum One handles, stop and fix the shared Arbitrum-Nitro path first (this would also be an Arbitrum One bug).
- If `nuthatch doctor` shows the public endpoint serves a `getLogs` window too small for a usable zero-setup demo, ship with a keyless provider path as the default instead of Robinhood's public RPC (see Open Question 5).
- If the `safe` tag proves unreliable on a given provider, fall back to depth-based finality for that deployment.

## Drawbacks, risks, and unknowns

- **Chain immaturity / centralization.** L2BEAT rates Robinhood Chain below Stage 0: centralized sequencer, instantly-upgradeable contracts (no delay), and (per L2BEAT's July 2026 assessment via Crypto Times) only two whitelisted challengers under Arbitrum BoLD. This is a chain-security concern, not an indexing-correctness one, but worth stating for users.
- **Public RPC is weak** (rate-limited, no archive, amplified by the 100ms cadence). Zero-setup demo works for light use; anything real needs a paid endpoint. Risk: users blame Nuthatch for endpoint throttling. Mitigation: the existing `doctor` + "on-ramp not the road" framing.
- **Sequencer re-ordering before batch post.** The soft-confirmed tip is more mutable than an equivalent-age Ethereum block; sealing must be at `safe`, not `latest`. Getting the hot-store depth wrong (in blocks) on a 100ms chain is the main correctness risk.
- **Compliance filtering.** Benign for log indexing (filtered txs emit no logs), but a user reconciling against `eth_getTransactionByHash` for a filtered hash could see surprising results. Document.
- **Testnet chain-ID discrepancy.** Official docs and multiple corroborating sources say **46630** ("4663 with a zero appended"); one aggregator (rpcnodelist) lists **46646**. **[VERIFY]** against `eth_chainId` on the live testnet endpoint before shipping the testnet entry — 46630 is the strongly-supported value.
- **Maintenance surface.** Another built-in endpoint to keep measured/current as the provider ecosystem churns.

## Alternatives considered

- **Leave it as an unlisted chain (status quo).** Works today via `--chain robinhood --rpc …`. Rejected as the *default* because it forfeits the zero-setup demo and tuned finality defaults, and there is clear Graph-ecosystem demand for first-class support. This RFC's Phase 0 is exactly this path, used as validation.
- **Write a dedicated Nitro/Arbitrum adapter.** Rejected: the ingestion path is chain-agnostic and Arbitrum One already works; a bespoke adapter would duplicate logic and add maintenance burden for no correctness gain.
- **Sequencer-feed head tracking.** Rejected for now: lower-latency but off-model (Nuthatch seals past finality over JSON-RPC). Future work.
- **Depth-based finality instead of `safe` tag.** Rejected as default: the `safe`/`finalized` tags are well-defined on this rollup and give a cleaner, correct seal boundary than a hand-tuned block depth. Depth remains the fallback if a provider serves tags unreliably.

## Open questions

1. What is the real file/struct for the built-in chain registry, and its exact finality-policy schema? (Blocks the concrete diff.)
2. Does the generic decode path already ingest Arbitrum One without any chain-ID special-casing? If yes, Robinhood Chain is nearly free; if there is `42161`-keyed code, it must be generalised.
3. Measured `getLogs` window / batch limit / archive depth for the public endpoint (via `doctor`).
4. Confirm the testnet chain ID (46630 vs 46646) against the live endpoint.
5. Should the built-in default endpoint be Robinhood's public RPC or a keyless provider path? (Match how the other seven are configured.)

## Unresolved / future work

- Sequencer-feed (`wss://feed…`) low-latency head tracker as an optional ingestion mode for Nitro-family chains.
- Shared `arbitrum-nitro` chain-family configuration so Arbitrum One, Arbitrum Nova, and Robinhood Chain share finality/window defaults and any receipt-field tolerance in one place.
- Optional Stock Token metadata recipe (symbol / underlying-equity mapping) as a convenience for RWA analytics.
- Revisit the finality default if Robinhood decentralises the sequencer or changes its DA model.

## References

- Robinhood Chain — Connecting: https://docs.robinhood.com/chain/connecting
- Robinhood Chain — Transaction Finality: https://docs.robinhood.com/chain/transaction-finality/
- Robinhood Chain — Differences from Ethereum: https://docs.robinhood.com/chain/differences-from-ethereum/
- Robinhood Chain — About: https://docs.robinhood.com/chain/
- L2BEAT — Robinhood Chain: https://l2beat.com/layer2s/projects/robinhood
- The Graph — Robinhood Chain Mainnet (supported networks): https://thegraph.com/docs/en/supported-networks/robinhood/
- Goldsky — Robinhood Chain indexing & RPC: https://goldsky.com/chains/robinhood
- Ormi — Subgraphs now support Robinhood Chain: https://blog.ormilabs.com/ormi-subgraphs-robinhood-chain/
- QuickNode — What is Robinhood Chain: https://www.quicknode.com/guides/robinhood/what-is-robinhood-chain
- Chainstack — Robinhood Chain RPC for RWA: https://chainstack.com/learn/how-to/how-to-get-robinhood-chain-rpc-endpoint-for-rwa/
- Alchemy — Arbitrum vs Ethereum API differences: https://docs.alchemy.com/reference/arbitrumethereum-differences
- Arbitrum — Inside Arbitrum Nitro: https://docs.arbitrum.io/how-arbitrum-works/inside-arbitrum-nitro
- Arbitrum — Block numbers and time: https://docs.arbitrum.io/arbitrum-essentials/arbitrum-vs-ethereum/block-numbers-and-time
- Arbitrum — Sequencer / batch posting: https://docs.arbitrum.io/how-arbitrum-works/deep-dives/sequencer
- Crypto Times — What Is Robinhood Chain (L2BEAT July 2026 assessment): https://www.cryptotimes.io/learn/what-is-robinhood-chain/
- Nuthatch repo: https://github.com/nightswatchhq/nuthatch

---

*Verification legend:* facts drawn from Robinhood's official docs, L2BEAT, Arbitrum docs, and named provider/registry pages are treated as verified and cited above. Items marked **[VERIFY]** are Nuthatch-internal specifics (registry file/schema, finality config keys, any Arbitrum special-casing) that could not be confirmed from the repository source during research, plus the testnet chain-ID discrepancy — the maintainer must confirm these against the codebase and a live `eth_chainId` call before merging. No chain IDs, RPC URLs, or Nuthatch file paths have been invented; where a path is unknown it is explicitly flagged rather than guessed.

---

## Maintainer addendum (2026-09-03)

The `[VERIFY]` items above are Nuthatch-internal facts the draft could not read. Checked against
the tree at `af875f9a` and the live endpoints on 2026-09-03; the body above is left as written.

1. **Registry file and schema (open question 1).** `src/chains.rs`, one `const Chain` per chain:
   `name`, `chain_id`, `rpc_urls`, `finality`, `log_window`, `topic0_only_getlogs`. `Finality` has
   exactly two variants, `Depth(u64)` and `FinalizedTag { fallback_depth }`. **There is no
   `safe`-tag policy.** §4's recommendation therefore maps onto `FinalizedTag` with a fallback depth
   sized for the 100 ms cadence, or a new `SafeTag` variant - which is a code change, not a data
   entry, and changes the "registry entry, not an adapter" framing by one enum arm.
2. **Arbitrum special-casing (open question 2).** None in the ingestion path. Every `42161` in
   `src/` outside `chains.rs` is a test fixture or a config string. Arbitrum One is
   `FinalizedTag { fallback_depth: 1800 }` with `log_window: 2000`.
3. **Chain ids (open question 4).** `eth_chainId` live: mainnet `0x1237` = **4663**, testnet
   `0xb626` = **46630**. The aggregator's 46646 is wrong.
4. **Block tags, live, mainnet at 18:23 UTC.** `latest` 53,622,003; `safe` 53,612,513 (9,490
   blocks and 990 s behind: 9.6 blocks/s, i.e. the 100 ms cadence); `finalized` 53,609,116 (12,887
   blocks and 1,343 s behind, about 22 minutes). Both tags are served by the public endpoint. A
   `FinalizedTag` fallback depth for this chain is of the order of **15,000 blocks**, not 1,800.
5. **Public endpoint (open question 3).** `nuthatch doctor`, unfiltered: `getLogs` window up to
   163,840 blocks range-only (recommend `--window 320` until measured with `--address`); batch
   100+; **archive depth refused with HTTP 429** rather than answered. The RFC's own warning about
   the public endpoint, in one probe. Re-probe with a Stock Token address before setting
   `log_window`.
6. **Open question 5** stays open: the other seven ship keyless public endpoints measured against
   the RFC-0030 §4 bar, and this one rate-limited a single doctor run.
