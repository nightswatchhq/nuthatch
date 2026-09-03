# RFC-0051: Monad chain support

- Status: **Implemented** (2026-09-03). Recorded as a frozen draft in the morning and carved out
  in the afternoon: **carve-out three**, Chief's decision, recorded in `CLAUDE.md`, for this one
  chain and nothing else. Shipped as a registry entry on the generic EVM path with a **depth of 8**
  rather than the `finalized` tag; the execution-lag guard proposed below was **not built**, and the
  addendum's items 10 to 14 say why, and what would move it to the tag.
- Author: Pete
- Date: September 2026
- Tracking issue: [#1136](https://github.com/nightswatchhq/nuthatch/issues/1136)
- RFC number: the draft carried a provisional 0043, which was already taken; 0050 is the sibling
  Robinhood Chain draft ([#1133](https://github.com/nightswatchhq/nuthatch/issues/1133)), so this
  is 0051. See the maintainer addendum at the end for the other `[VERIFY]` items.

-----

## Summary / TL;DR

This RFC adds **Monad** (mainnet chain id `143` / `0x8f`, native token `MON`) as a built-in chain, as a sibling to the Robinhood Chain work in the companion RFC.

Monad is a full-EVM-bytecode L1 with a pipelined, HotStuff-derived BFT consensus (MonadBFT) that gives **single-slot, deterministic finality** roughly two blocks (~600–800 ms) behind the tip, with blocks every **~300 ms**. For a logs/events indexer this is close to the easy case: there is exactly **one block per height**, and once a block is `Finalized` it is, per Monad's own RPC docs, *"irreversible without a hard fork."* The one genuinely new wrinkle is **asynchronous (deferred) execution** — consensus agrees on ordering first and execution lags by a fixed few blocks — which changes *when* receipts/logs and state roots become queryable, not *whether* they are stable.

**Should we do this, and how (three lines):**

1. **Yes** — Monad is a live, high-activity EVM L1 with real, already-served indexing demand and no non-EVM surprises for our ingestion path.
1. **Reuse the generic EVM path unchanged**, and register Monad with a **tag-based `finalized` finality policy** (not depth-based): on Monad `finalized` means BFT-final, so its closeness to the tip is the *safe* case, not the dangerous one the README warns about.
1. **Ship behind `doctor`-measured window defaults** (public `rpc.monad.xyz` caps `eth_getLogs` at 100 blocks), add a small execution-lag guard so we never seal a finalized-but-not-yet-executed block, and gate GA on a backfill + tip-follow correctness run against a public subgraph.

-----

## Motivation

Three reasons, in order of weight:

1. **Demand is already here.** The Graph lists **Monad Mainnet** as a supported network;  Goldsky (subgraphs + Turbo pipelines, single-CLI migration),  Envio HyperIndex (Monad Mainnet indexing shipped Nov 2025), QuickNode, Alchemy, Ankr, dRPC and Chainstack all advertise Monad indexing. That is exactly the population nuthatch's "no subgraph? no problem" pitch targets. A built-in entry turns `nuthatch init 0xADDR --chain monad` into the two-minute demo.
1. **It exercises a genuinely different finality model in our favour.** Every built-in chain so far is either probabilistic-finality PoS (Ethereum), an L2 with delayed L1 finality (Arbitrum, Base, Optimism), or a fast-finality PoS sidechain (BSC, Polygon, Gnosis). Monad is our first **single-slot-final BFT** chain. Documenting how our tag-based policy maps onto it, once and carefully, pays off: the Robinhood Chain sibling RFC and future BFT EVMs reuse the reasoning.
1. **It is a stress test for RFC-0028 range control.** Monad produces a block every ~300 ms and, per Monad's RPC-limits docs, *"can accommodate up to 5,000 transactions per block with computation up to 200M gas… Blocks are both extremely frequent and significantly larger than Ethereum blocks, which is the main motivation for keeping per-call block range limits low."*  If our splitter and rate-limit classifier are right, Monad should "just work"; if not, it will find the gap — the way the OBIB run surfaced the Alchemy HTTP-400 oversized-range refusal (RFC-0029).

-----

## Background on Monad

Everything here is sourced (see References). Items that could not be pinned to a primary source are marked **[VERIFY]**.

### Identity

|Field               |Mainnet                                                                                                          |Testnet                                             |
|--------------------|-----------------------------------------------------------------------------------------------------------------|----------------------------------------------------|
|Chain id            |`143` (`0x8f`)                                                                                                   |`10143` (`0x279f`)                                  |
|Native token        |MON                                                                                                              |MON                                                 |
|Launch              |**24 November 2025, 14:00 UTC** (MON TGE same day; total supply 100 B MON)                                       |19 February 2025                                    |
|Canonical public RPC|`https://rpc.monad.xyz`                                                                                          |`https://testnet-rpc.monad.xyz`                     |
|Other public RPC    |`rpc1.monad.xyz` (Alchemy), `rpc3.monad.xyz` (Ankr), `rpc-mainnet.monadinfra.com` (Monad Foundation)             |provider-specific                                   |
|Explorers           |MonadVision (`monadvision.com`, by BlockVision),  MonadScan (`monadscan.com`), SocialScan (`monad.socialscan.io`)|`testnet.monadscan.com`, `testnet.monadexplorer.com`|
|Docs                |`docs.monad.xyz`                                                                                                 |same                                                |
|Core repos          |`category-labs/monad`, `category-labs/monad-bft`, `category-labs/monad-revm`, `category-labs/alloy-monad-evm`    |same                                                |

Chain id `143` is corroborated by chainlist/chainid.network, Chainstack, QuickNode and DefiLlama; testnet `10143` likewise.

### Architecture (what matters to an indexer)

Monad keeps **EVM bytecode equivalence** (redeploy without recompilation; Cancun opcodes `TSTORE`/`TLOAD`/`MCOPY` supported)  but re-engineers everything underneath:

- **MonadBFT** — a pipelined, HotStuff-derived BFT consensus. Two voting phases, pipelined so that when block `N` is proposed, `N-1` is being voted and `N-2` is being finalized.  Block `N` is **finalized at the proposal of `N+2`** (a QC-on-QC).   Under normal operation this is deterministic single-slot finality in ~600–800 ms. MonadBFT is explicitly designed to resist **tail-forking** (a proposed block being dropped when the next leader fails).
- **Block time** — **400 ms at launch, reduced to 300 ms** by governance change **MIP-12** (which also cut per-block tx cap 5,000→3,750 and block gas limit 200M→150M).  Current RPC docs state "a block every 300 ms" (~3.3 blocks/sec).
- **Asynchronous / deferred execution** — consensus agrees on transaction *ordering* first; execution runs afterwards in a separate pipeline.   Per Monad's own docs, **execution lags consensus by `k=3` blocks** — the "delayed merkle root" parameter `D`, which the docs state is *"currently set in testnet and mainnet to `3`."*  (The "~10 block" figure that appears in some third-party summaries is **not** supported by Monad's engineering docs; `k=3` is authoritative.) Block proposals carry the state root from `k` blocks ago as a sanity check.
- **Optimistic parallel execution** — independent transactions run in parallel, re-executed on conflict; results are **identical to serial execution**. No indexer impact — we consume the canonical, serialized log/receipt output, byte-identical to a sequential EVM.
- **MonadDb** — purpose-built state store (Merkle-trie diffs); relevant only in that speculative execution is cheap to discard, which is *why* dropping a non-finalized proposed block is clean.
- **Gas model differences** — gas is **charged on the transaction's `gasLimit`, not `gasUsed`** (a direct consequence of deferred execution: leaders build and validators vote before executing),  with a **reserve balance** rule carving out gas budget across the in-flight `k` blocks. Nominal per-opcode gas equals Ethereum's;  MIP-8 added 4 KB storage-page ("warm page") pricing.  **None of this touches an events indexer** — we store what the receipt reports.
- **EIP-4844 blobs are unsupported** — blob transactions are rejected by `eth_sendRawTransaction`/`eth_call`/`eth_estimateGas`.  We do not index blobs, so this is a non-issue beyond being a fact for the identity table.

### Finality / reorg model → RPC tags → indexing meaning

Monad defines four block states and maps them onto the standard Geth tags. This is the load-bearing table for our design:

|Monad state|Geth tag   |Position vs proposed tip `N` |Reversible?                                                                         |What it means for nuthatch                                                 |
|-----------|-----------|-----------------------------|------------------------------------------------------------------------------------|---------------------------------------------------------------------------|
|`Proposed` |`latest`   |`N`                          |Yes (speculative)                                                                   |Do **not** seal. Speculatively executed; may be abandoned (rare).          |
|`Voted`    |`safe`     |`N-1`                        |Extremely unlikely                                                                  |Usable for a low-latency read path; not our seal boundary.                 |
|`Finalized`|`finalized`|`N-2`                        |**No** — Monad docs: *"Irreversible without a hard fork. Use for value settlement."*|**Our seal boundary.** Exactly one block per height.                       |
|`Verified` |(no tag)   |`finalized − execution_delay`|No                                                                                  |Execution outputs (state root)  agreed. Relevant for state reads, not logs.|

**Reorg characteristics.** Unlike Ethereum, **only one block is proposed and voted per height**  — there are no competing forks to reorganize, so retroactive reorgs of finalized blocks do not occur.  The single residual risk is **tail-forking**: a *proposed* (not-yet-finalized) block can be dropped if the next leader fails.  This is the reason we never seal anything before `finalized`.

**The subtle part — deferred execution vs log availability.** Because execution lags consensus, a block can be `Finalized` (consensus done) *before* its transactions are executed and its logs/receipts exist (`Verified`). `eth_getLogs`/`eth_getTransactionReceipt` serve data from *executed* blocks. So there is a brief window where the `finalized` height is ahead of the height whose logs are actually retrievable. Monad's own docs illustrate the user-facing version of this: after seeing a transaction receipt you should *"wait another 1.2 seconds"* before the resulting state is safely readable. Practically this gap is sub-second to a few blocks, but our ingestion must not equate "block is finalized" with "block's logs are fetchable" — see Detailed design.

### RPC availability and quirks

- **`eth_getLogs` block-range caps** (from Monad's RPC-limits table, verbatim): `rpc.monad.xyz` (QuickNode) **100 blocks**; `rpc1.monad.xyz` (Alchemy) **1,000 blocks and 10,000 logs (whichever is more constraining)**; `rpc3.monad.xyz` (Ankr) **1,000 blocks**; Monad Foundation `rpc-mainnet.monadinfra.com` **100 blocks**.  Monad recommends **1–10 block ranges** for latency;  indexing guidance suggests **100-block ranges with ~100 concurrent workers**.
- **Rate limits** are provider-specific; public endpoints are shared and throttled — the README's standing warning applies with extra force here given the block rate.
- **`eth_getBlockReceipts` is supported** — one round trip per block for all receipts, our preferred receipt path.
- **Debug/tracing** — `debug_traceBlockByNumber/Hash`, `debug_traceCall`, `debug_traceTransaction` are supported, but the **trace-options object is required** (omitting it returns `-32602`), the default tracer is `callTracer`, and **opcode-level struct logs are not supported**.  `debug_getRawBlock/Header/Receipts/Transaction` are available. We do not need traces for the events path.
- **State availability** — full nodes **do not serve arbitrary historic state**; `eth_call` at an old block may fail.  This matters for state-pinned `[[calls]]` (RFC-0023), not for logs backfill.
- **Monad-specific methods** — `admin_ethCallStatistics`, `txpool_statusByAddress`, `txpool_statusByHash`, `eth_sendRawTransactionSync`, `eth_fillTransaction`. None needed by nuthatch.
- **WebSocket subscriptions** — standard `eth_subscribe` `newHeads`/`logs` fire on the **`Proposed`** (speculative) block.  Monad adds **`monadNewHeads`** and **`monadLogs`**, which carry `blockId` (distinct per proposal, since multiple can share a height) and `commitState` (`Proposed`/`Voted`/`Finalized`/`Verified`) and re-emit as a block advances.  `syncing` and `newPendingTransactions` subscriptions are unsupported.
- **Execution Events SDK** — Category Labs ships `libmonad_event`, a shared-memory ring-buffer firehose  (C/C++/**Rust**)  that a co-located sidecar reads for the lowest possible latency,  emitting `BlockStart`/`BlockQC`/`BlockFinalized`/`BlockVerified`.  It requires running on the **same host as the node**,  so it is outside nuthatch's RPC-only golden path — noted under Future work.
- **Transaction lifecycle quirks** — deferred nonce/balance validation means `eth_sendRawTransaction` may accept a tx that later reverts; `eth_getTransactionByHash` returns `null` for mempool txs.  Neither affects read-only indexing.

### Security / decentralization posture

Monad is a PoS BFT chain with a validator set in the low hundreds — Messari reports **Testnet-2 (June 2025) running with over 160 globally distributed validators and 100+ live applications**, and a mainnet validator target on the same order. Finality is economic-and-cryptographic (≥2/3 stake QC-on-QC), not probabilistic. For our purposes this is a footnote: what we depend on is that `finalized` is honoured as irreversible, which the protocol guarantees absent a coordinated hard fork.

-----

## Detailed design

### Chain registration entry

Monad is registered as a built-in chain in the chain registry. **[VERIFY — the exact source file, chain-entry struct name and finality-policy enum/variant identifiers could not be read from `src/` at authoring time; the fetcher refused directory and blob listings, and the subagent confirmed the same tool restriction. The README confirms the two finality policies exist in prose (a `finalized`/`safe`-tag policy and a depth-based-confirmations policy, `docs/operators.md#running-an-unlisted-evm-chain`) but does not name the Rust identifiers. The illustrative entry below MUST be reconciled with the real `ChainConfig`/`FinalityPolicy` types before merge.]**

Illustrative entry (adapt field names to the real schema):

```rust
// src/chains.rs  [VERIFY path/struct/enum names]
BuiltinChain {
    name: "monad",
    chain_id: 143,
    native_token: "MON",
    // measured, not assumed — commit the `nuthatch doctor` output alongside
    rpc_urls: &[
        "https://rpc.monad.xyz",   // QuickNode-backed, getLogs cap 100
        "https://rpc1.monad.xyz",  // Alchemy-backed, getLogs cap 1000 / 10000-logs
    ],
    finality: FinalityPolicy::Tag { tag: FinalityTag::Finalized }, // NOT depth-based
    // guard so we never seal a finalized-but-unexecuted block
    execution_lag_guard_blocks: 4,  // >= k(=3); see rationale
    default_log_window: 100,        // conservative; doctor tunes upward per endpoint
    default_concurrency: 20,
    block_time_ms: 300,
}
```

```toml
# equivalent nuthatch.toml surface for an operator overriding defaults
[chain]
name = "monad"
rpc_urls = ["https://your-endpoint.example/monad"]
finality = "finalized"      # tag-based
log_window = 100
concurrency = 50
```

### Which ingestion path to reuse

**The generic EVM path, unchanged.** Monad is Geth-JSON-RPC-compatible at the level nuthatch cares about: `eth_blockNumber`, `eth_getLogs`, `eth_getBlockByNumber`, `eth_getBlockReceipts` and the `finalized` tag all behave as our ingestion expects. Decode, reorg handling and entity derivation are already chain-agnostic (per README); nothing about Monad's parallel execution or MonadDb changes the *bytes* we decode, because canonical output is identical to serial execution. This is a **registry entry plus defaults**, not a new ingestion path.

### What specifically differs from the Arbitrum / Robinhood case

|Concern                |Arbitrum One (L2)                                                    |Monad (this RFC)                                                                  |
|-----------------------|---------------------------------------------------------------------|----------------------------------------------------------------------------------|
|Finality source        |L1 (Ethereum) confirmation; `finalized` lags minutes–hours behind tip|Native BFT; `finalized` lags **~2 blocks (~0.7 s)** behind tip                    |
|Correct finality policy|Tag-based `finalized` (deep)                                         |**Tag-based `finalized` (shallow)** — see below                                   |
|Reorg surface          |Sequencer reorgs before L1 posting                                   |One block per height; only *proposed* blocks can tail-fork                        |
|Block rate             |~4/sec, small blocks                                                 |**~3.3/sec, very large blocks** (up to 5,000 tx / 200M gas) → tighter getLogs caps|
|New failure mode       |none unusual                                                         |**Deferred execution**: finalized ≠ logs-available (briefly)                      |

**Why the README's "finalized close to the tip" warning does *not* apply here — the crux.** The README warns that a chain whose `finalized` tag runs close to the tip needs a depth-based policy, *"or you seal immutable Parquet that could never be corrected."* That warning targets chains where `finalized` is *close to the tip because it is not really final* — e.g. an endpoint that aliases `finalized` to `latest`, or a chain whose "finalized" block can still be reorged. On **Monad**, `finalized` is close to the tip because finality genuinely *is* fast: it is a BFT QC-on-QC, irreversible without a hard fork, over a chain with exactly one block per height. Closeness-to-tip here is a **latency win, not a safety risk**. Using a depth-based confirmations policy on Monad would add pure latency for zero additional safety. **We therefore use the tag-based `finalized` policy** — and say so explicitly in the operator docs, so no one "fixes" it into depth-based by analogy with an L2.

### Block / tx model → storage schema mapping

No schema change. Monad blocks and receipts populate the existing per-event columns exactly as any EVM chain does:

|Column           |Monad source   |Notes                                                             |
|-----------------|---------------|------------------------------------------------------------------|
|`block_number`   |block header   |linear, one per height                                            |
|`block_hash`     |block header   |stable once finalized                                             |
|`block_timestamp`|block header   |costs the usual header round trip; `--no-timestamps` still applies|
|`tx_hash`        |receipt / log  |                                                                  |
|`log_index`      |log            |                                                                  |
|`address`        |log            |                                                                  |
|`_seq`           |derived ordinal|unchanged                                                         |

Monad's header carries the delayed merkle root rather than an immediately-final state root, but nuthatch does not read state roots on the logs path, so this is invisible to the schema.

### Finality / reorg strategy

1. **Seal boundary = `finalized` tag.** Poll `eth_getBlockByNumber("finalized")` for the seal frontier. Blocks at or below it are immutable → eligible for Parquet.
1. **Execution-lag guard.** Because a block can be `Finalized` before it is executed (`Verified = finalized − execution_delay`, with the delay driven by `k=3`), do **not** assume the finalized height's logs are fetchable. Seal up to `min(finalized_height, latest_height_with_retrievable_logs)`: advance the seal frontier only for heights whose `eth_getLogs` has already been fetched successfully; a finalized-but-unexecuted block that returns empty/absent is retried, not sealed. Default `execution_lag_guard_blocks = 4` (≥ `k=3`) prevents sealing a hole.
1. **Tip handling.** The hot store (redb) holds only tip→finality, which on Monad is ~2 blocks — trivially small in block count, though it churns fast. Reorg handling only ever touches this hot region; sealed Parquet is strictly past `finalized` and never rewritten, exactly as today.

### Head tracking: polling vs WebSocket / native streaming

**Ship with HTTP polling** (our existing model), tuned to the block rate:

- Poll `finalized` and `latest` at ~250–300 ms cadence to match block time, with jitter and per-endpoint health tracking (already implemented; `--rpc` round-robin).
- `eth_getLogs` in ≤ `default_log_window` (100) chunks, concurrency-limited.

**Optional low-latency path (not required for GA):** subscribe to **`monadLogs`** over WebSocket and act only when `commitState == Finalized`, using `blockId` to distinguish proposals at the same height. This cuts tip latency but adds a second code path and a WS dependency; proposed as a follow-up, not part of this RFC. The **Execution Events SDK** firehose is explicitly out of scope (co-location requirement).

### RPC client configuration

- **`eth_getLogs` window:** default **100 blocks** (the `rpc.monad.xyz` cap). `nuthatch doctor --rpc <url> --address 0xADDR` measures the true cap per endpoint and prints the largest safe `--window`; on an Alchemy-backed endpoint doctor should report ~1,000 blocks / 10,000 logs.
- **Rate-limit / oversized-range classification:** relies on RFC-0028. Monad returns `-32602 "Invalid block range"` for oversized `eth_getLogs`.  **Action item:** confirm this refusal shape is enumerated by the splitter alongside the Alchemy HTTP-400 case (RFC-0029) so a too-wide window is split, not retried unchanged forever.
- **Receipts:** prefer `eth_getBlockReceipts` (one round trip/block).
- **Batching:** JSON-RPC batch limits are provider-specific; doctor reports the measured batch limit.
- **Pool hygiene:** every endpoint in a `--rpc` pool must return chain id `143`; nuthatch already refuses mixed pools at startup.

### Backfill strategy

- **Genesis:** 24 November 2025. By September 2026 the chain is ~10 months old at ~3.3 blocks/sec, i.e. on the order of **tens of millions of blocks** **[VERIFY exact tip height via `eth_blockNumber`]**.
- **Implication:** a full-history backfill over a busy contract is **not** a public-endpoint job — the 100-block cap on `rpc.monad.xyz`, combined with blocks holding up to 5,000 tx, means very many `eth_getLogs` calls. The README's "assume a paid endpoint or your own node for anything you keep running" is doubly true here. Run `doctor` before any deep backfill and confirm archive depth (Chainstack enabled Monad mainnet archive + trace on Global Nodes in a December 2025 update;  QuickNode and Alchemy offer archive).
- **`--seal-direct`** is the right default for a cold backfill (write straight to Parquet past finality); tip-follow switches to the hot store.
- **Timestamps:** `block_timestamp` is ~85% of backfill wall clock (per README); recommend `--no-timestamps` for Monad nests that never ask time-series questions, given the block count.

### Schema migrations

**None expected.** This is a registry addition. On-disk format is unchanged; the semver / binary-swap promise (README) holds.

### CLI / config surface

- `nuthatch init 0xADDR --chain monad` (bytecode auto-probe also works if `--chain` is omitted and the contract is on a built-in chain — but the probe reaches only built-ins, so Monad-only contracts should name `--chain monad` explicitly).
- `nuthatch init 0xADDR --chain monad --rpc https://your-endpoint/monad` for a private endpoint.
- `nuthatch doctor --rpc https://rpc.monad.xyz --address 0xADDR` before trusting a backfill.
- New config keys: none beyond the standard `--window`, `--concurrency`, `--seal-direct`, `--no-timestamps`.

### Observability

No new metrics needed; the existing Prometheus `/metrics` (tip lag, rows decoded/sealed, reorgs, RPC counts, RSS) covers Monad. Two dashboard notes during rollout:

- **Tip lag in blocks** will look large in *count* at 300 ms/block even when small in wall-clock — set alert thresholds in **seconds, not blocks**.
- **Reorg counter** should stay ~0 for the sealed region (single block per height); any nonzero sealed reorg is a bug, or an endpoint serving non-canonical proposed data under the `finalized` tag.

### Docs

- Add Monad to the built-in chains list in `README.md`.
- Add a Monad subsection to `docs/operators.md` next to "running an unlisted EVM chain," stating explicitly: **use the tag-based `finalized` policy; do not switch to depth-based; here is why (BFT single-slot finality)**, plus the execution-lag guard rationale and the 100-block getLogs cap.

-----

## Testing plan

1. **Unit / registry:** chain id `143` resolves; finality policy is tag-based `finalized`; defaults (window 100, block_time 300) load.
1. **`doctor` snapshot:** run `nuthatch doctor` against `rpc.monad.xyz`, `rpc1.monad.xyz` and one paid endpoint; commit the measured getLogs window / batch limit / archive-depth output as the source of the shipped defaults (measured, not assumed — per README convention).
1. **Tip-follow correctness:** index a high-traffic contract (a major DEX pair or USDC) for a fixed window; verify no sealed-region reorgs and that the seal frontier never overtakes retrievable-logs height (the deferred-execution guard).
1. **Backfill correctness vs an independent source:** pick a contract with a public Goldsky / The Graph subgraph on Monad; reconcile event counts over a fixed block range (the "correctness against a public subgraph" method from `docs/kicking-the-tyres.md`).
1. **Range-control regression:** point at the 100-block-cap public endpoint and confirm RFC-0028 splitting handles Monad's `-32602` oversized-range refusal without stalling.
1. **Determinism:** two operators with different `--window`/`--concurrency` on the same range produce byte-identical sealed segments (RFC-0028 boundary-from-data guarantee).

## Rollout phases (with benchmarks / thresholds that would change the plan)

- **Phase 0 — Testnet (chain id `10143`).** Validate against `testnet-rpc.monad.xyz`. *Threshold to proceed:* clean backfill + 24 h tip-follow with zero sealed reorgs.
- **Phase 1 — Mainnet, experimental flag.** Ship the registry entry; document as experimental. *Threshold to GA:* event-count parity with a public subgraph within tolerance on ≥2 contracts, and doctor-measured defaults confirmed on ≥2 independent endpoints.
- **Phase 2 — GA / built-in.** Promote Monad into the built-in list with bundled measured endpoints. *Threshold that would change the plan:* if the deferred-execution guard proves insufficient (any observed sealed hole), **raise `execution_lag_guard_blocks`** and, if still unreliable, seal at `finalized − N` with an empirically chosen `N` rather than the tag alone.
- **Phase 3 (optional).** Add the `monadLogs`/`commitState` WebSocket head path if tip latency matters to users.

A benchmark that would **reopen the design:** if a paid archive endpoint cannot sustain the getLogs rate needed to keep tip lag under ~2 s on a busy contract, revisit whether the RPC-only path is adequate or whether the Execution Events SDK sidecar becomes a supported (non-golden-path) option.

## Drawbacks / risks / unknowns

- **Deferred execution is a new class of edge case.** The finalized-vs-executed gap is small but real; a wrong guard can seal a hole. Mitigated by the guard + Phase-1 testing; flagged as the top risk.
- **Public endpoints are punishing** at Monad's block rate and per-block size; the "paid endpoint for real work" caveat is sharper than for slower chains.
- **Backfill volume** (tens of millions of blocks) makes full-history nests expensive; `--no-timestamps` and archive endpoints are close to mandatory.
- **RFC-0043 number is provisional [VERIFY].**
- **Chain-registry source identifiers are unverified [VERIFY]** — struct/enum/field names must be reconciled with `src/` before merge.

## Alternatives considered

1. **Leave Monad as an unlisted chain** (`init --chain monad --rpc …`, since v2.4.0). Works today, but loses bundled measured endpoints and tuned finality defaults, and — critically — leaves each operator to pick a finality policy, where the natural-but-wrong choice (depth-based, by analogy with L2s) adds latency. A built-in entry encodes the correct policy once.
1. **Depth-based confirmations policy.** Rejected: adds latency for no safety on a single-slot-final BFT chain (see crux above).
1. **WebSocket `monadLogs` as the primary head path.** Rejected for GA: second code path + WS dependency; polling is sufficient and matches every other chain.
1. **Execution Events SDK firehose.** Rejected for the golden path: requires co-locating with a node; violates the single-binary, RPC-only model. Kept as Future work.

## Open questions

1. Does the mainnet execution delay parameter (`k=3`, per docs) ever change with MIP upgrades? It sets the guard.
1. Do any bundled public endpoints ever serve **proposed/speculative data under the `finalized` tag** during upgrades or under load? If so, the guard must widen.
1. Is `eth_getBlockReceipts` uniformly available across the endpoints we would bundle, or do some restrict it?
1. Confirm the real **chain-registry file/struct/finality-enum identifiers** and the **next free RFC number**.

## Future work

- **`monadLogs` / `commitState` low-latency head tracking** as an opt-in.
- **Execution Events SDK sidecar** as a supported (non-embedded) ingestion mode for operators who co-locate with a Monad node and need sub-block latency or very high volume.
- **Robinhood Chain sibling RFC** shares the BFT-finality reasoning; keep the two operator docs cross-referenced.
- Revisit **state-pinned `[[calls]]` (RFC-0023)** on Monad given that full nodes do not serve arbitrary historic state — likely needs an archive node and explicit documentation.

## References

- Monad docs — Block States: <https://docs.monad.xyz/monad-arch/consensus/block-states>
- Monad docs — RPC differences / JSON-RPC overview: <https://docs.monad.xyz/reference/rpc-differences>
- Monad docs — RPC limits (getLogs caps, block size): <https://docs.monad.xyz/reference/rpc-limits>
- Monad docs — Asynchronous execution (`k=3` / delayed merkle root): <https://docs.monad.xyz/monad-arch/consensus/asynchronous-execution>
- Monad docs — MonadBFT: <https://docs.monad.xyz/monad-arch/consensus/monad-bft>
- Monad docs — Gas pricing (charged on gasLimit): <https://docs.monad.xyz/developer-essentials/gas-pricing>
- Monad docs — Releases changelog (MIP-12 block time 400→300 ms): <https://docs.monad.xyz/developer-essentials/changelog/releases>
- Monad docs — Execution Events SDK: <https://docs.monad.xyz/execution-events>
- Monad blog — How Monad Works: <https://blog.monad.xyz/blog/how-monad-works>
- Monad blog — Execution Events SDK: <https://blog.monad.xyz/blog/execution-events-sdk>
- Messari — Monad project profile (launch, validators, supply): <https://messari.io/project/monad>
- The Graph — Monad Mainnet supported network: <https://thegraph.com/docs/en/supported-networks/monad/>
- Goldsky — Monad indexing: <https://goldsky.com/chains/monad>
- Envio — Monad Mainnet indexing (blog): <https://docs.envio.dev/blog>
- Chainstack — Monad tooling / RPC providers 2026: <https://docs.chainstack.com/docs/monad-tooling>
- QuickNode — Monad docs: <https://www.quicknode.com/docs/monad>
- DefiLlama — Monad chain: <https://defillama.com/chain/monad>
- category-labs/monad: <https://github.com/category-labs/monad>
- category-labs/monad-bft: <https://github.com/category-labs/monad-bft>
- nuthatch README: <https://github.com/nightswatchhq/nuthatch>

-----

*Caveats:* Two classes of fact in this RFC are explicitly unverified and marked **[VERIFY]**: (1) nuthatch's own chain-registry **source identifiers** (exact file path, `ChainConfig`/`FinalityPolicy` struct and enum/variant names, confirmation-depth field) and the **exact next RFC number** — the code fetcher refused all `src/`, `docs/rfcs/`, blob, raw and GitHub-API listings, and a dedicated subagent confirmed the same restriction; the README confirms the two finality policies exist and that `0041` is the highest RFC it references, but not the code names. (2) The **current mainnet tip height** (derive live via `eth_blockNumber`). All Monad technical claims are sourced to Monad's own documentation/blog or corroborated by Messari, Chainstack, QuickNode, The Graph, Goldsky, Envio and DefiLlama as cited. The "10-block execution delay" seen in some third-party summaries is **superseded** by Monad's engineering docs, which state `k=3` for both testnet and mainnet.

-----

## Maintainer addendum (2026-09-03)

The `[VERIFY]` items and action items above are checked against the tree at `f7a4bdd4` and the
live endpoints on 2026-09-03; the body above is left as written. The probes are recorded on
[#1136](https://github.com/nightswatchhq/nuthatch/issues/1136), with the `doctor` output.

1. **RFC number.** 0043 is taken (lessons from Amp). 0050 is the Robinhood Chain draft. This is
   0051, and the header says so.
2. **Registry file and schema (open question 4).** `src/chains.rs`, one `const Chain` per chain:
   `name`, `chain_id`, `rpc_urls`, `finality`, `log_window`, `topic0_only_getlogs`. `Finality` has
   exactly two variants, `Depth(u64)` and `FinalizedTag { fallback_depth }`. The illustrative
   entry's `native_token`, `execution_lag_guard_blocks`, `default_concurrency` and `block_time_ms`
   fields do not exist, and `FinalityPolicy::Tag { tag: Finalized }` is spelled
   `Finality::FinalizedTag { fallback_depth }`. So the registry half of this proposal is one
   `FinalizedTag` row with a fallback depth sized for a 300 ms cadence. The **execution-lag guard
   is not a registry field**: it is a change to the seal loop, and it is the part of this RFC that
   is more than a data entry. Whether it is needed at all is item 6.
3. **Chain ids, live.** `eth_chainId` returns `0x8f` = **143** on `rpc.monad.xyz`,
   `rpc1.monad.xyz` and `rpc3.monad.xyz`, and `0x279f` = **10143** on `testnet-rpc.monad.xyz`.
4. **Block tags and cadence, live.** `latest`, `safe` and `finalized` are all served and sit
   within 0 to 1 block of one another with identical timestamps, on every endpoint probed, so
   `finalized` runs about one block behind tip rather than the two the body assumes. Cadence
   measured over 30 s on rpc1: 100 blocks in 30.2 s, **302 ms/block**. The header carries no
   `l1BlockNumber`, as expected for an L1.
5. **Tip height (the second caveat).** 101,691,142 at probe time, so "tens of millions" is short
   by a factor of a few. Block 1 is timestamped 2025-05-14, some six months before the public
   launch date in the identity table, and the early history is empty blocks. The history is served
   from block 1 (item 8).
6. **Deferred execution at the `finalized` tag.** `eth_getBlockReceipts("finalized")` and
   `eth_getLogs` at that height answered at once, with rows, on every endpoint: 15 receipts and 73
   logs on rpc.monad.xyz, 16 and 127 on rpc1. One sample each. That is an absence of evidence for a
   finalized-but-unexecuted window on public RPC, not proof there is none, and it is the measurement
   the Phase 1 threshold should repeat many times before deciding the guard's size or its existence.
7. **Oversized-range refusal shapes (the RPC action item).** None of them is the
   `-32602 "Invalid block range"` the body asks the splitter to enumerate.
   - `rpc.monad.xyz` (QuickNode): a 101-block span is served; 1,000 returns **HTTP 413**
     `-32614 "eth_getLogs is limited to a 100 range"`. No text marker in `looks_like_cap` matches
     that message, but `classify_status` maps 413 to `Narrowable`, so the splitter handles it by
     status alone.
   - `rpc1.monad.xyz` (Alchemy): 1,001 served; 10,000 returns HTTP 400 `-32602 "Log response size
     exceeded ... up to a 1,000 block range"`, the RFC-0029 shape, matched.
   - `rpc3.monad.xyz` (Ankr): 1,001 returns HTTP 200 `-32062 "Block range is too large"`, matched
     by the `range is too` marker. **1,000 returns HTTP 200 `-32603 "response exceeds size limit"`,
     which matches no marker and no status**, so it would classify `Transient` and be retried at the
     same width. Not proven in a run; a nest at `--window 1000` against rpc3 is the reproduction,
     and at the body's default of 100 it cannot fire.
8. **Public endpoints, `nuthatch doctor` 3.1.0** (open question 3, and the archive question). Range-only
   windows 80 / 163,840 / 640, recommending `--window 40` across the pool; batch 50+ / 200+ and
   **10** on Ankr, which timestamp fetches will split down to; archive depth **429 rate-limited**
   on rpc.monad.xyz and **no historic state about 1M blocks behind tip** on rpc1 and rpc3, which is
   what the body's state-availability bullet predicted. Historic **logs and blocks are served** by
   all three from block 1: probed at blocks 1, 1,000,000 and 50,000,000, the last returning 1,343
   logs in a 51-block span. So a from-genesis logs backfill is possible on public RPC, subject to
   the caps and rate limits above; state-pinned `[[calls]]` (RFC-0023) at old blocks are not.
9. **Not checked here.** The `k=3` execution delay, the `Verified` state, `monadLogs` and the
   MIP history are cited to Monad's own documentation in the body and were not independently
   measured.
10. **Decision, later the same day.** The freeze was lifted for this chain and for nothing else:
    carve-out three, recorded in `CLAUDE.md`. Shipped as `Finality::FinalizedTag { fallback_depth:
    200 }` on the generic path, with `rpc1.monad.xyz`, `rpc.monad.xyz` and `rpc3.monad.xyz` in that
    order and `log_window: 100`. The RFC-0030 §4 bar, re-run address-filtered against the busiest
    contract of the day (7,831 logs in 101 blocks): five of five ten-block `eth_getLogs` 5,000 behind
    tip, batch-of-5 served, `finalized` served, topic0-only served, on all three. Address-filtered
    `doctor` windows 640 / 80 / 320, batch 200+ / 100+ / 10. `rpc-mainnet.monadinfra.com` is the one
    keyless endpoint that keeps historic state, and it is not listed: it refuses JSON-RPC batches
    (HTTP 403 `Restricted JSON RPC method`), which fails the bar's batch floor.
11. **The execution-lag guard is not built, and the reviewer's objection to its design stands.**
    As specified it cannot tell a finalized-but-unexecuted block from an executed block with no
    matching logs, since both answer an empty list. It does not need to. Measured eight times at
    `latest` on rpc1 on 2026-09-03: every block's receipts and logs were complete on the first read,
    receipt count equal to transaction count, identical two seconds later, same hash. The public RPC
    layer serves only executed blocks, so `Finalized` never runs ahead of what `eth_getLogs` can
    answer, and there is nothing to guard. What the same probe did show is that a load-balanced URL
    can answer `finalized` from a backend one or two blocks ahead of the one that answered `latest`;
    the client already treats a request past the serving backend's head as "beyond current head"
    rather than as a cap (#903).
12. **Ankr's refusal shape is classified.** `exceeds size limit` and `is limited to a` are in
    `classify_rpc_error`'s narrowable list and in the text fallback, with a test that fails without
    them. Item 7's reproduction is therefore no longer needed.
13. **`--rpc` on a built-in name is honoured, and the README said otherwise.** The CLI section above
    tells an operator to run `init 0xADDR --chain monad --rpc <url>` for a private endpoint, and the
    README's "`--rpc` is ignored for the built-in names" read as a contradiction in review. The code is
    the RFC's way: `chains::resolve` ignores `--rpc` only for the chain *id* (a built-in name never
    dials to learn it), and `select_rpcs` then makes the given endpoints the entire pool, with nothing
    public appended. The README sentence now says that, and the operator note points a pinned-call or
    deployment-block user at it.
14. **Shipped as `Depth(8)`, not the tag, and item 11 is narrowed.** Review of the implementation
    made the point item 11 did not answer: eight samples at `latest` show finalized and executed were
    coupled in those samples, not that they are coupled under provider load or a pipeline stall, and
    a hole sealed on an empty answer for an unexecuted block cannot be repaired without mutating a
    sealed segment. The remedy the review named is the one taken: a conservative offset until the
    invariant is demonstrated. Eight blocks is 2.4 s at 300 ms, more than twice the `k = 3` deferral,
    and every sealed block is at least six past `finalized`, so the body's crux still holds in the
    sense that matters - nothing past the tag reorgs, so the depth is an execution margin, not a
    finality one - and the cost against the tag is two seconds of seal latency. The way back to
    `FinalizedTag` is a soak that reads `eth_getBlockReceipts` at `finalized` continuously for a day
    on a shipped endpoint and never sees a short answer; a sample is not that.
15. **The invariant the depth rests on, demonstrated rather than assumed.** Review asked for an
    execution-availability check with a conservative failure mode, or a protocol/RPC invariant that
    makes a depth boundary safe. Three facts, two of them measured on all three shipped endpoints on
    2026-09-03:
    - *A node serves a height only once it has executed it.* Monad's RPC reference says `latest` is
      "backed by speculative execution"; the `Proposed` block is executed on receipt, which is why
      every probe at `latest` had complete receipts.
    - *A height the node has not executed is answered with an error, never an empty list.* Alchemy:
      `-32602 "block range extends beyond current head block"`; QuickNode and Ankr: `-32602 "Block
      requested not found"`; `eth_getBlockReceipts` and `eth_getBlockByNumber` answer `null` or
      `Unknown block`. The first is the #903 not-a-cap shape and the others match no marker, so all
      classify `Transient` and the range is retried, which is the conservative failure mode: a short
      answer cannot be sealed as an empty block, because it is not an answer.
    - *The header's `logsBloom` is the block's own, not the delayed block's.* Tested by keccak
      membership of every log-emitting address in a six-block window: 52 of 52 present in header N's
      bloom, 32 of 52 in header N+3's. So the one-sided empty-case oracle RFC-0049 §1 describes is
      available from headers the indexer already fetches when `block_timestamps` is on. It is not
      wired in this change; it is the chain-agnostic follow-up, and it would protect every chain, not
      only this one.
    What remains outside the invariant is a load-balanced URL whose backend answering `eth_getLogs`
    sits a block or two behind the one that answered `latest`, and silently truncates a range that
    straddles its head instead of erroring. Alchemy errors on that; the other two could not be told
    apart from the chain simply advancing during the probe. That risk is chain-agnostic and is
    exactly RFC-0049 §1's open item; the depth of eight does not create it and does not close it.
