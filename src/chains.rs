//! Tiny chain registry. Ships sensible public-RPC defaults with round-robin failover - the
//!
//! ## These endpoint lists have an expiry date
//!
//! They are the **zero-setup path**: `init 0xAddr --chain mainnet && dev` uses them, and the founding
//! non-negotiable is a live indexed query in under two minutes with no configuration. That makes them
//! product surface, not constants - and they rot without anyone touching this file.
//!
//! On 2026-07-31, **two of the four mainnet defaults were dead**: `ethereum-rpc.publicnode.com` had
//! moved archive requests behind a token (so it could not serve a backfill *at all*, while being
//! listed first), and `eth.llamarpc.com` was returning HTTP 521. The comment beside them said
//! "Verified to serve keyless eth_getLogs (2026-07)" - within the same month.
//!
//! **Re-measure before trusting this list**, with the smallest request a real backfill makes - a
//! 10-block address-filtered `eth_getLogs` a few thousand blocks behind tip:
//!
//! ```sh
//! nuthatch doctor --rpc <url> --address <a-busy-contract>
//! ```
//!
//! A tip-only check passes on endpoints that cannot backfill, which is exactly how this went unnoticed.
//! first-run killer is RPC friction, so out of the box you should not need to bring a key.
//! (The "no third-party" upgrade is to colocate with a reth node; that path comes later.)
//!
//! The registry also carries each chain's finality policy and `eth_getLogs` window, so an L2 like
//! Arbitrum - different finality semantics, denser blocks - is a data entry here, not a fork of the
//! indexing loop.

use anyhow::Context;

/// How a chain decides a block is final enough to seal to the immutable cold layer. The sealing
/// invariant is unchanged either way: the columnar layer never sees a reorg, so this only sets *how
/// far behind the tip* we wait before a block is beyond reorg risk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finality {
    /// Seal blocks at least `n` behind the tip. A conservative proxy for finality (Ethereum L1).
    Depth(u64),
    /// Prefer the node's L1-aware `finalized` block tag (correct by construction on an L2 like
    /// Arbitrum); fall back to `Depth(fallback_depth)` when the endpoint doesn't serve the tag.
    FinalizedTag { fallback_depth: u64 },
}

pub struct Chain {
    pub name: &'static str,
    pub chain_id: u64,
    /// Tried in order, then round-robin, so a single flaky endpoint doesn't stall a run.
    pub rpc_urls: &'static [&'static str],
    /// When a block is safe to seal (see `Finality`).
    pub finality: Finality,
    /// Block span per `eth_getLogs` call. Small on dense L1 (dodge result-size caps); large on a
    /// sparse L2 like Arbitrum where events are few but block heights climb fast.
    pub log_window: u64,
    /// Whether a `getLogs` with an empty address list (topic0-only, the factory flip) is accepted.
    /// `false` means the shipped default returns an error such as "Please specify an address"; a
    /// factory nest on that chain must not discover the refusal mid-backfill.
    pub topic0_only_getlogs: bool,
}

const MAINNET: Chain = Chain {
    name: "mainnet",
    chain_id: 1,
    rpc_urls: &[
        // **Ordered by measured backfill capability, best first** - see the module note above on why
        // this list has an expiry date. Re-measured 2026-07-31 with a 10-block address-filtered
        // `eth_getLogs` 5,000 blocks behind tip, which is the smallest request a real backfill makes.
        // Re-measured 2026-08-23 with `nuthatch doctor --rpc … --address <usdc>` (#761), confirmed
        // 2026-08-24 with a 10-block address-filtered getLogs 5,000 behind tip:
        //   eth-pokt.nodies.app   archive YES, topic0-only YES
        //   eth.drpc.org          archive YES, topic0-only YES (batch-of-5 500s; the timestamp
        //                         fetcher already splits down to the cap)
        // `eth.api.onfinality.io/public` dropped: the 23rd's doctor probe did not complete (empty
        // hang). A spare that stalls the run is not failover. It answered the same probes on the
        // 24th; it stays off the list until it survives a doctor run, not a one-shot getLogs.
        // A batch cap *degrades* - the timestamp fetcher splits down to it.
        "https://eth-pokt.nodies.app",
        "https://eth.drpc.org",
        // Removed 2026-08-23 (#761): `eth.api.onfinality.io/public` - doctor probe does not complete,
        // and it was never archive. Removed 2026-07-31: `ethereum-rpc.publicnode.com` (archive token)
        // and `eth.llamarpc.com` (HTTP 521).
    ],
    // ~2 epochs; real finality signals arrive with the ExEx mode. The `finalized` tag exists
    // post-merge but Depth keeps a single conservative policy until ExEx lands.
    finality: Finality::Depth(64),
    log_window: 20,
    topic0_only_getlogs: true,
};

const ARBITRUM_ONE: Chain = Chain {
    name: "arbitrum-one",
    chain_id: 42161,
    rpc_urls: &[
        // Keyless Arbitrum One endpoints. Both re-measured 2026-08-06 against the RFC-0030 §4 bar
        // with a 10-block address-filtered `eth_getLogs` 5,000 behind tip, five times each: archive
        // OK, getLogs 5/5, batch-of-5 OK, `finalized` OK. The official sequencer RPC first.
        "https://arb1.arbitrum.io/rpc",
        "https://arb-pokt.nodies.app",
        // Removed 2026-08-06 (issue #267): `arbitrum.drpc.org` failed two criteria - `getLogs`
        // **0/5** with `Request timeout on the free plan, please upgrade`, and a 5-request batch
        // rejected with `Batch of more than 3 requests are not allowed`. It could not serve a
        // backfill at all, and it was listed *second*, so round-robin handed it real traffic.
        // `arbitrum-one-rpc.publicnode.com` removed 2026-07-31 - same archive-token policy as its
        // mainnet sibling; it cannot serve a backfill.
    ],
    // True finality is L1 confirmation of the batch (~10-20 min). Prefer the node's `finalized`
    // tag; else ~7.5 min at 250 ms blocks. Horizon is sparse, so the extra hot window is cheap.
    finality: Finality::FinalizedTag {
        fallback_depth: 1800,
    },
    // Arbitrum blocks are frequent but Horizon events are rare, so a wide window keeps up cheaply.
    //
    // Measured 2026-08-07, address-filtered: `arb1.arbitrum.io` sustains ~163,840 blocks, but
    // `arb-pokt.nodies.app` only ~40 - and failover can route any request to the narrower one, so
    // `doctor` recommends 20 across the pair. 2000 is kept deliberately: it is right for the sparse-L2
    // case this window exists for, and RFC-0028's `fetch_logs_splitting` narrows a refused range
    // rather than failing it. The cost of that rescue is a burst of retries at the start of a
    // backfill, which reads as slowness - so a busy contract wants `--window` set from `doctor`.
    log_window: 2000,
    topic0_only_getlogs: true,
};

const BASE: Chain = Chain {
    name: "base",
    chain_id: 8453,
    rpc_urls: &[
        // Keyless Base mainnet endpoints. Both re-measured 2026-08-06 against the RFC-0030 §4 bar:
        // archive OK, getLogs 5/5, batch-of-5 OK, `finalized` OK. The official RPC first.
        "https://mainnet.base.org",
        "https://base-pokt.nodies.app",
        // Removed 2026-08-06 (issue #267): `base.drpc.org` rejects a 5-request batch with `Batch of
        // more than 3 requests are not allowed`, and `getLogs` was 4/5 with the same free-plan
        // timeout its sibling endpoints return. Flaky *and* over the bar's batch floor.
        // `base-rpc.publicnode.com` removed 2026-07-31 - same archive-token policy.
    ],
    // OP-stack L2: true finality is L1 confirmation. Base exposes the L1-aware `finalized` tag, so
    // prefer it (same policy as Arbitrum); the fallback (~30 min at 2 s blocks) only bites if an
    // endpoint doesn't serve the tag.
    finality: Finality::FinalizedTag {
        fallback_depth: 900,
    },
    // ~2 s blocks and busy - a moderate window that the adaptive chunker (RFC-0004 §2) tunes further.
    // Measured 2026-08-07, address-filtered: `mainnet.base.org` ~80, `base-pokt.nodies.app` ~40.
    // Same reasoning as Arbitrum: the optimistic default is recovered by adaptive splitting, at the
    // cost of early retries.
    log_window: 1000,
    topic0_only_getlogs: true,
};

/// BNB Smart Chain. **Tip-following of a static contract works out of the box; a from-deployment
/// backfill does not; a factory nest does not.**
///
/// Re-measured 2026-08-23 (#761), confirmed 2026-08-24. `bsc-rpc.publicnode.com` is still the only
/// keyless endpoint that answers address-filtered getLogs at all, and historical getLogs at block
/// 1,000,000 is HTTP 403 - **archive depth no**. `bsc-dataseed.binance.org` fails a 10-block getLogs
/// (`limit exceeded`) and has no trie state ~1M behind tip; `bsc.drpc.org` 429s the public plan;
/// `1rpc.io/bnb` is over quota; `binance.llamarpc.com` does not connect.
///
/// It also **refuses address-less getLogs** (`-32701 Please specify an address`) - the shape
/// RFC-0009 §4's factory flip issues. A pancakeswap-style nest works until 500 children and then
/// fails every window. `topic0_only_getlogs` is false so `build_nest` names that at load.
///
/// So this is shipped honestly: tip-follow a static contract on the default; history and factories
/// need `--rpc <your archive endpoint>`.
const BSC: Chain = Chain {
    name: "bsc",
    chain_id: 56,
    rpc_urls: &["https://bsc-rpc.publicnode.com"],
    // Fast finality is ~2 blocks, and the endpoint serves the tag; the fallback is deliberately far
    // more conservative than that because a single-endpoint chain has nothing to cross-check against.
    finality: Finality::FinalizedTag {
        fallback_depth: 1000,
    },
    // Measured max 320 address-filtered. Set at the measurement rather than optimistically above it:
    // with one endpoint there is no sibling to absorb a retry storm.
    log_window: 320,
    topic0_only_getlogs: false,
};

/// Polygon PoS. Archive is available but narrow; the wide endpoint is not archive.
///
/// Measured 2026-08-20 (three runs each): `polygon.drpc.org` is archive, getLogs up to 80 blocks,
/// batch 10, doctor recommends 40. `polygon-bor-rpc.publicnode.com` gives a 5,120-block window and
/// batch 200+ but **archive depth no** - tip-following only, backfills fail. `polygon-rpc.com` was
/// unreachable on 2026-08-19.
///
/// Archive depth is the binding constraint for backfill; the archive endpoint goes first so a new
/// nest does not open against a non-archive URL. The wider endpoint is kept as a secondary for
/// tip-following fallover once an initial backfill completes.
const POLYGON: Chain = Chain {
    name: "polygon",
    chain_id: 137,
    rpc_urls: &[
        "https://polygon.drpc.org",
        "https://polygon-bor-rpc.publicnode.com",
    ],
    // Heimdall checkpoints are the real finality signal and the tag reflects them; the fallback is
    // sized for a checkpoint interval rather than a block time.
    finality: Finality::FinalizedTag {
        fallback_depth: 1000,
    },
    // polygon.drpc.org (archive, first endpoint) caps getLogs at 80 blocks; doctor recommends 40.
    // Measured 2026-08-20.
    log_window: 40,
    topic0_only_getlogs: true,
};

/// Gnosis. The best-served of the four chains added here: two keyless **archive** endpoints, both
/// with a 163,840-block getLogs ceiling and batch 200+.
///
/// Measured 2026-08-19. `gnosis-rpc.publicnode.com` also passes the bar but reports archive depth no,
/// so it is deliberately not listed - a third endpoint that cannot serve history is a trap during a
/// backfill, not redundancy.
const GNOSIS: Chain = Chain {
    name: "gnosis",
    chain_id: 100,
    rpc_urls: &[
        "https://rpc.gnosischain.com",
        "https://rpc.gnosis.gateway.fm",
    ],
    finality: Finality::FinalizedTag {
        fallback_depth: 200,
    },
    // Both endpoints measured at 163,840; 20,000 leaves headroom for the adaptive chunker to climb
    // without opening on a window neither can serve.
    log_window: 20_000,
    topic0_only_getlogs: true,
};

/// Optimism. OP-stack L2, so the same finality reasoning as Base: the `finalized` tag is L1-aware.
///
/// Measured 2026-08-19: `mainnet.optimism.io` (window 640) and `optimism-rpc.publicnode.com`
/// (window 1,280), both **archive**, both batch-of-5 OK. `optimism.drpc.org` is excluded for failing
/// batch-of-5 - the identical failure that removed `arbitrum.drpc.org` and `base.drpc.org` under
/// issue #267, which is now three chains in a row.
const OPTIMISM: Chain = Chain {
    name: "optimism",
    chain_id: 10,
    rpc_urls: &[
        "https://mainnet.optimism.io",
        "https://optimism-rpc.publicnode.com",
    ],
    finality: Finality::FinalizedTag {
        fallback_depth: 900,
    },
    log_window: 600,
    topic0_only_getlogs: true,
};

/// Monad. A full-EVM-bytecode L1 with MonadBFT single-slot finality, and the first chain here whose
/// `finalized` tag runs *one block* behind tip because finality genuinely is that fast, not because
/// an endpoint aliases it to `latest` (RFC-0051). Blocks every 300 ms - measured 2026-09-03 at 302 ms
/// over 100 blocks - and dense: the busiest contract that day carried 7,831 logs across 101 blocks.
///
/// Measured 2026-09-03 with `nuthatch doctor --address <that contract>` against the RFC-0030 §4 bar:
///
/// | endpoint                    | getLogs (addr) | batch | `finalized` | history              |
/// |-----------------------------|----------------|-------|-------------|----------------------|
/// | `rpc1.monad.xyz` (Alchemy)  | 640            | 200+  | yes         | logs + blocks from 1 |
/// | `rpc.monad.xyz` (QuickNode) | 80 (cap 100)   | 100+  | yes         | logs + blocks from 1 |
/// | `rpc3.monad.xyz` (Ankr)     | 320            | 10    | yes         | logs + blocks from 1 |
///
/// **`doctor` reports archive depth "no" on all three, and that verdict is right about what it
/// measures and wrong about what a backfill needs.** Its probe is a pinned `eth_getBalance` a million
/// blocks behind tip, and none of the three keeps historic *state* - Monad full nodes do not. All
/// three serve historic *logs and blocks* from block 1 (probed at 1, 1,000,000, 40,000,000 and
/// 50,000,000), which is what a from-genesis backfill reads. So backfills work on the shipped
/// defaults; a pinned `[[calls]]` at an old block (RFC-0023) does not, and wants `--rpc` at an archive
/// endpoint. `rpc-mainnet.monadinfra.com` is the one keyless endpoint that *does* keep state, and it
/// is not listed because it refuses JSON-RPC batches outright (HTTP 403 `Restricted JSON RPC method`),
/// which fails the bar's batch floor and would make every timestamp fetch a single-block request.
///
/// The three refuse an over-wide range in three different shapes, none of them the `-32602` the RFC
/// draft expected: QuickNode HTTP 413 `-32614 "eth_getLogs is limited to a 100 range"`, Alchemy the
/// RFC-0029 HTTP 400 `-32602 "Log response size exceeded"`, and Ankr HTTP 200 `-32603 "response
/// exceeds size limit"`. The first and third are in `classify_rpc_error` because of this chain; the
/// third matched nothing before and would have been retried at the same width.
///
/// **No execution-lag guard, and the tag after a day's soak.** RFC-0051 proposed a guard on the
/// grounds that Monad finalises a block before executing it (execution is deferred by `k = 3`
/// blocks). Measured eight times at `latest` on 2026-09-03: every block's receipts and logs were
/// complete on the first read (receipt count equal to transaction count), identical two seconds
/// later, same hash - the RPC layer served only executed blocks. Eight samples are not an invariant,
/// so 3.3.0 shipped a **depth of 8** (2.4 s, more than twice the deferral) and asked for a soak
/// instead: read `finalized` and its receipts continuously for a day on every shipped endpoint and
/// count the short answers (#1145). The soak ran 2026-09-04 07:05 to 2026-09-05 07:05 UTC on all
/// three endpoints, 300 ms apart (1 s on Alchemy's, which throttled 300 ms): **zero short answers
/// and zero disagreements** across 648,532 reads, the only errors being five
/// transport failures the reader counts apart. So the boundary is the `finalized` tag, as the RFC's
/// body argued, with the two seconds of seal latency the depth was costing given back. The fallback
/// depth only runs on an endpoint that stops serving the tag: 200 blocks is one minute at 300 ms, a
/// hundred times the protocol's own finality and sixty times the execution deferral.
///
/// **What stands between a finalised-but-unexecuted block and a sealed segment, now that the depth
/// is gone.** A day of polling is evidence, not an invariant, so the tag does not rest on the soak
/// alone. Three things, in the order they would fire:
///
/// 1. The RPC layer's own behaviour, measured 2026-09-03 on all three endpoints: a height a node has
///    **not** executed is answered with an **error**, never an empty list - Alchemy `-32602 "block
///    range extends beyond current head block"`, QuickNode and Ankr `-32602 "Block requested not
///    found"` - and both classify `Transient` and are retried at the same height. Monad's own docs
///    say `latest` is "backed by speculative execution", i.e. a node serves a height only once it
///    has executed it, and the soak found the same of `finalized` 648,532 times.
/// 2. The tail refetch (#1144, `indexer::FETCH_TAIL_OVERLAP`), in the base since 3.3.1 and chain
///    agnostic: every window is asked for again two blocks back by the next one, rows are keyed by
///    `(block, log_index)` so the second pass adds what the first missed, and **the seal cut is held
///    back until the tail has been asked for again**. Should an endpoint ever answer an unexecuted
///    height with an empty list rather than the error above, that is the shape this guard exists
///    for. Two blocks covers the two-block skew it was sized on; the `k = 3` deferral is one block
///    wider, so the refetch is the backstop for the first two of those three blocks and not, on its
///    own, the proof. The proof is item 1 plus the soak.
/// 3. The header's `logsBloom` is the block's **own** (52 of 52 log-emitting addresses present in
///    header N's bloom, 32 in N+3's), so the RFC-0049 §1 empty-case oracle is available from headers
///    already fetched when timestamps are on, should anyone want to wire it. Not wired.
const MONAD: Chain = Chain {
    name: "monad",
    chain_id: 143,
    rpc_urls: &[
        // Widest window and widest batch first; the canonical endpoint second, because its 100-block
        // cap and 50 req/s limit make it the narrow one; Ankr third with its batch of 10.
        "https://rpc1.monad.xyz",
        "https://rpc.monad.xyz",
        "https://rpc3.monad.xyz",
    ],
    // MonadBFT: a block is final at the proposal of N+2, about 600 ms, and every shipped endpoint
    // serves the `finalized` tag one block behind tip. Nothing past `finalized` reorgs on Monad, and
    // the day-long soak (#1145, the doc comment above) found the tag never served a block before its
    // receipts, so the execution margin `Depth(8)` bought is not needed. RFC-0051 addendum item 16.
    finality: Finality::FinalizedTag {
        fallback_depth: 200,
    },
    // At the narrowest endpoint's documented range cap (QuickNode, 100), not above it: Monad blocks
    // are dense enough that the *result* cap is what bites on a busy contract, and RFC-0028's chunker
    // narrows from here on a cap it can see. `doctor` recommends 40 across the pool.
    log_window: 100,
    topic0_only_getlogs: true,
};

/// Robinhood Chain. Robinhood's Arbitrum Orbit L2 on the Nitro stack - the same execution stack and
/// the same settlement shape as Arbitrum One, so it rides the generic EVM path with no special-casing
/// (RFC-0050; the addendum confirms every `42161` outside this file is a fixture). Chain id 4663,
/// mainnet since 2026-07-01, 100 ms blocks (measured 9.9 blocks/s on 2026-09-04), and busy: 148 logs
/// per block over 1,000 sampled blocks from 3,059 emitters, the top of the list being USDG, WETH and
/// the Stock Tokens (MSTR, AMC, SPY, ...) - ERC-20s, which is exactly the shape `init` was built for.
///
/// Measured 2026-09-04 with `nuthatch doctor --address <USDG>` against the RFC-0030 §4 bar. Three
/// keyless hosts answer `eth_chainId` with 4663; one of them can back a nest:
///
/// | endpoint                          | getLogs (addr) | batch | archive        | verdict |
/// |-----------------------------------|----------------|-------|----------------|---------|
/// | `rpc.mainnet.chain.robinhood.com` | 640            | 5     | refused (429)  | shipped |
/// | `robinhood.drpc.org`              | -              | -     | -              | no `eth_blockNumber` (`-32601`) |
/// | `robinhood-rpc.publicnode.com`    | 0 - 403        | 200+  | token required | the archive-token policy its siblings have |
///
/// The shipped endpoint is rate-limited, and the limit is per second rather than per day: the same
/// `doctor` run that read 640 above read **40** and `batching unusable at 2` when it ran straight
/// after a thousand-block sampling pass, and every failure was HTTP 429. A backfill on it works at
/// the shipped window; anything in anger wants `--rpc` at a keyed provider (Alchemy, QuickNode and
/// Chainstack all list the chain). The over-wide refusal is HTTP 200 `-32000 "logs matched by query
/// exceeds limit of 10000"`, which `classify_rpc_error` already narrows on.
///
/// **`FinalizedTag`, not the `safe` tag the RFC's body recommends.** Both tags are served: at
/// 15:08 UTC `safe` was 7,765 blocks (784 s) behind `latest` and `finalized` 11,921 blocks (1,207 s,
/// about twenty minutes). `Finality` has no `safe` arm, and adding one is a seal-loop change rather
/// than a registry entry, so the carve-out takes the tag the code has and pays about seven minutes
/// of seal latency for it - the same policy Arbitrum One runs. The fallback depth is sized for the
/// cadence, not copied from Arbitrum One's 1,800: 15,000 blocks is twenty-five minutes at 10 blocks/s,
/// a margin over the measured `finalized` lag, and it only runs on an endpoint that stops serving
/// the tag.
const ROBINHOOD: Chain = Chain {
    name: "robinhood",
    chain_id: 4663,
    rpc_urls: &[
        // The only keyless endpoint that passed the bar on 2026-09-04; see the table above for the
        // two that did not.
        "https://rpc.mainnet.chain.robinhood.com",
    ],
    finality: Finality::FinalizedTag {
        fallback_depth: 15_000,
    },
    // Half of what the public endpoint sustained address-filtered on the busiest token, which is
    // what `doctor` recommends. The result cap is 10,000 logs, and a busy token here carries about
    // 20 a block, so 320 sits under it with room; RFC-0028's chunker narrows from here on the cap
    // it can see.
    log_window: 320,
    // Address-less windows are served: the 2026-09-04 emitter sample was 20 unfiltered 50-block
    // windows, every one answered.
    topic0_only_getlogs: true,
};

pub fn lookup(name: &str) -> Option<&'static Chain> {
    match name {
        "mainnet" | "ethereum" | "eth" => Some(&MAINNET),
        "arbitrum-one" | "arbitrum" | "arb" | "arb1" => Some(&ARBITRUM_ONE),
        "base" | "base-mainnet" | "base-one" => Some(&BASE),
        "bsc" | "bnb" | "binance" | "bnb-smart-chain" | "bsc-mainnet" => Some(&BSC),
        "polygon" | "matic" | "polygon-pos" | "polygon-mainnet" => Some(&POLYGON),
        "gnosis" | "xdai" | "gnosis-chain" => Some(&GNOSIS),
        "optimism" | "op" | "op-mainnet" | "optimism-mainnet" => Some(&OPTIMISM),
        "monad" | "monad-mainnet" | "mon" => Some(&MONAD),
        "robinhood" | "robinhood-chain" | "robinhood-mainnet" | "rh" => Some(&ROBINHOOD),
        _ => None,
    }
}

/// Policy for a chain with no registry entry: the same "assume L1, wait for real depth, and a
/// narrow `eth_getLogs` window" default the indexer already falls back to for a nest whose
/// `chain` field names nothing in this file. `init`/`add` hand a custom chain the identical
/// policy, so a nest built here behaves exactly as `dev` already promised for one.
pub const UNREGISTERED_FINALITY: Finality = Finality::Depth(64);
pub const UNREGISTERED_WINDOW: u64 = 20;

/// A resolved chain, ready for `init`/`add` to act on. `Chain` stays a purely `&'static` built-in
/// registry entry; this is the owned shape a custom (non-built-in) chain needs, since its name and
/// endpoints come from the command line rather than a `const`.
///
/// `Debug` is load-bearing rather than decorative: a test under the `scaled` feature unwraps a
/// `Result` carrying this, which needs it, and the default build never compiles that test - so the
/// omission was invisible until the scaled-mode CI job ran.
#[derive(Debug)]
pub struct ResolvedChain {
    pub name: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    pub finality: Finality,
    pub log_window: u64,
}

impl From<&'static Chain> for ResolvedChain {
    fn from(c: &'static Chain) -> Self {
        ResolvedChain {
            name: c.name.to_string(),
            chain_id: c.chain_id,
            rpc_urls: c.rpc_urls.iter().map(|s| s.to_string()).collect(),
            finality: c.finality,
            log_window: c.log_window,
        }
    }
}

/// Resolve `--chain` for `init`: a name in the built-in registry always wins, on its own vetted
/// endpoints and policy. A name outside it is accepted too, but only when paired with `--rpc` -
/// nothing else could tell nuthatch where to send traffic for a chain it doesn't ship. The custom
/// chain's `chain_id` comes from the RPC itself (`eth_chainId`, one call) rather than a flag, since
/// the endpoint already knows and a wrong hand-typed id would silently corrupt the nest.
///
/// This is the fix for issue #535: the nest format (`chain` + `chain_id` + `rpc_urls` in
/// `nuthatch.toml`) and the runtime (`indexer::index` falls back to [`UNREGISTERED_FINALITY`] /
/// [`UNREGISTERED_WINDOW`] for exactly this case) already support any chain - `init`'s allow-list
/// was the only thing narrower than what it scaffolds.
pub async fn resolve(name: &str, rpc_urls: &[String]) -> anyhow::Result<ResolvedChain> {
    if let Some(c) = lookup(name) {
        return Ok(c.into());
    }
    if rpc_urls.is_empty() {
        anyhow::bail!(
            "unknown chain '{name}' (try: mainnet, arbitrum-one, base, optimism, polygon, gnosis, \
             bsc) - or pass --rpc <url> to point nuthatch at a chain it doesn't ship built-in"
        );
    }
    let chain_id = crate::rpc::RpcClient::new(rpc_urls.to_vec())?
        .chain_id()
        .await
        .with_context(|| format!("could not read the chain id for '{name}' from --rpc"))?;
    Ok(ResolvedChain {
        name: name.to_string(),
        chain_id,
        rpc_urls: rpc_urls.to_vec(),
        finality: UNREGISTERED_FINALITY,
        log_window: UNREGISTERED_WINDOW,
    })
}

/// The chain of a nest already on disk: a known chain uses its own registry policy, and a nest
/// scaffolded against a custom chain (see [`resolve`]) has its `chain_id` saved in `nuthatch.toml`
/// already, so there is nothing left to ask an RPC for. Used by `add`, which grows an existing nest
/// and must never re-detect or re-resolve the chain it already committed to at `init`.
pub fn from_config(name: &str, chain_id: u64) -> ResolvedChain {
    match lookup(name) {
        Some(c) => c.into(),
        None => ResolvedChain {
            name: name.to_string(),
            chain_id,
            rpc_urls: Vec::new(),
            finality: UNREGISTERED_FINALITY,
            log_window: UNREGISTERED_WINDOW,
        },
    }
}

/// Every registered chain, in auto-detect probe order (L1 first, then the busiest L2s). `init`
/// walks this when `--chain` is omitted: the chain a contract lives on is discoverable, not a
/// thing the user should have to know and type.
///
/// **This must list every chain [`lookup`] knows** (#696). It returned three while `lookup` knew
/// seven, so a contract on Optimism, Polygon, BSC or Gnosis could be indexed by name and not found
/// by probe - and the README stated in bold that omitting `--chain` probes all seven. Two lists of
/// the same set, one of them silent when it falls behind. `every_registered_chain_is_probed` below
/// derives the expectation from `lookup` rather than restating the literal, so the next chain added
/// fails the test until it is added here too.
pub fn all() -> &'static [&'static Chain] {
    &[
        &MAINNET,
        &ARBITRUM_ONE,
        &BASE,
        &OPTIMISM,
        &POLYGON,
        &BSC,
        &GNOSIS,
        &MONAD,
        &ROBINHOOD,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrum_is_registered_with_l2_finality() {
        let c = lookup("arbitrum-one").expect("arbitrum-one in registry");
        assert_eq!(c.chain_id, 42161);
        assert_eq!(
            c.finality,
            Finality::FinalizedTag {
                fallback_depth: 1800
            }
        );
        assert!(
            c.log_window >= 1000,
            "sparse L2 wants a wide getLogs window"
        );
        assert!(!c.rpc_urls.is_empty());
        // Aliases resolve to the same chain.
        assert_eq!(lookup("arb").unwrap().chain_id, 42161);
        assert_eq!(lookup("arbitrum").unwrap().chain_id, 42161);
    }

    #[test]
    fn mainnet_uses_depth_finality() {
        let c = lookup("mainnet").unwrap();
        assert_eq!(c.finality, Finality::Depth(64));
        assert_eq!(c.log_window, 20);
    }

    /// #761: onfinality's public URL was a third mainnet default that could not serve a backfill
    /// (no archive, and on 2026-08-23 the doctor probe did not complete). Failover across a corpse
    /// is not failover.
    #[test]
    fn mainnet_does_not_ship_onfinality_or_a_single_live_host() {
        let urls = lookup("mainnet").unwrap().rpc_urls;
        assert!(
            !urls.iter().any(|u| u.contains("onfinality")),
            "onfinality public is not archive and does not complete a doctor probe: {urls:?}"
        );
        assert!(
            urls.len() >= 2,
            "mainnet must keep a spare after pruning: {urls:?}"
        );
        assert!(
            lookup("mainnet").unwrap().topic0_only_getlogs,
            "mainnet factory flip is the ordinary case"
        );
    }

    /// #761: BSC's only keyless default refuses address-less getLogs, which is the factory flip.
    #[test]
    fn bsc_shipped_default_does_not_claim_topic0_only_getlogs() {
        let c = lookup("bsc").unwrap();
        assert!(
            !c.topic0_only_getlogs,
            "bsc-rpc.publicnode.com refuses an empty address list"
        );
        assert_eq!(c.rpc_urls, &["https://bsc-rpc.publicnode.com"]);
    }

    #[test]
    fn base_is_registered_as_op_stack_l2() {
        let c = lookup("base").expect("base in registry");
        assert_eq!(c.chain_id, 8453);
        // OP-stack L2 → same finalized-tag policy as Arbitrum.
        assert!(matches!(c.finality, Finality::FinalizedTag { .. }));
        assert!(!c.rpc_urls.is_empty());
        assert_eq!(lookup("base-mainnet").unwrap().chain_id, 8453);
    }

    /// RFC-0051: the first BFT single-slot-final chain in the registry. The seal boundary is the
    /// `finalized` tag, which 3.3.0 withheld behind `Depth(8)` until a day-long soak on every shipped
    /// endpoint showed the tag never serves a block ahead of its receipts (#1145, addendum item 16).
    /// The fallback is sized for a 300 ms cadence, not copied from an L2. The window sits at the
    /// narrowest shipped endpoint's documented cap rather than above it, because Monad's blocks are
    /// dense enough that the result cap, not the range cap, is what a busy contract hits.
    #[test]
    fn monad_is_registered_on_the_finalized_tag_at_the_narrowest_cap() {
        let c = lookup("monad").expect("monad in registry");
        assert_eq!(c.chain_id, 143);
        assert_eq!(
            c.finality,
            Finality::FinalizedTag {
                fallback_depth: 200
            },
            "the tag, on #1145's soak: zero short answers in a day on three endpoints; 200 blocks is \
             a minute at 300 ms and only runs on an endpoint that stops serving the tag"
        );
        assert_eq!(
            c.log_window, 100,
            "QuickNode's documented cap - see the entry's measurements before raising it"
        );
        assert!(
            c.topic0_only_getlogs,
            "measured 2026-09-03: an address-less topic0 getLogs is served by all three"
        );
        assert_eq!(lookup("monad-mainnet").unwrap().chain_id, 143);
    }

    /// RFC-0050: an Arbitrum Nitro L2, so it takes Arbitrum One's tag policy, with the fallback
    /// depth sized for 100 ms blocks rather than copied. The RFC's body asks for the `safe` tag;
    /// `Finality` has no such arm, and the carve-out took the tag the code has rather than a
    /// seal-loop change. The window is what `doctor` recommended on the busiest token, under a
    /// 10,000-log result cap the endpoint names in its refusal.
    #[test]
    fn robinhood_is_registered_on_the_nitro_tag_policy_with_a_cadence_sized_fallback() {
        let c = lookup("robinhood").expect("robinhood in registry");
        assert_eq!(c.chain_id, 4663);
        assert_eq!(
            c.finality,
            Finality::FinalizedTag {
                fallback_depth: 15_000
            },
            "finalized runs about 12,000 blocks behind tip at 10 blocks/s; 1,800 is Arbitrum One's \
             number and would fall short of the tag by ten minutes"
        );
        assert_eq!(
            c.log_window, 320,
            "doctor's recommendation on 2026-09-04 against USDG; the result cap is 10,000 logs"
        );
        assert!(
            c.topic0_only_getlogs,
            "measured 2026-09-04: address-less getLogs windows are served"
        );
        assert_eq!(
            c.rpc_urls.len(),
            1,
            "one keyless endpoint passed the RFC-0030 §4 bar; the two others are recorded in the \
             entry's doc comment with why not"
        );
        assert_eq!(lookup("robinhood-chain").unwrap().chain_id, 4663);
    }

    #[test]
    fn unknown_chain_is_none() {
        assert!(lookup("dogechain").is_none());
    }

    #[test]
    fn all_chains_are_probeable_and_lead_with_l1() {
        let all = all();
        // Every registered chain is reachable via `lookup` and carries endpoints to probe.
        assert!(!all.is_empty());
        for c in all {
            assert!(
                lookup(c.name).is_some(),
                "{} must resolve via lookup",
                c.name
            );
            assert!(
                !c.rpc_urls.is_empty(),
                "{} needs endpoints to probe",
                c.name
            );
        }
        // L1 first: mainnet is the most likely home and the least surprising default hit.
        assert_eq!(all[0].name, "mainnet");
    }

    /// RFC-0030 §4: a chain may ship only with **at least two** endpoints that independently clear
    /// the bar, because the round-robin failover in `rpc_urls` is the mitigation for a flaky host -
    /// and a list of one has nothing to fail over to.
    ///
    /// Enforced here rather than remembered, because the failure is silent: pruning a bad endpoint
    /// is exactly when this drops to one, and the person doing it is looking at what they removed
    /// rather than at what is left. Issue #267 pruned three lists in one pass.
    #[test]
    fn every_chain_ships_at_least_two_endpoints() {
        // BSC is the one exception, and it is recorded rather than waved through. No keyless public
        // archive endpoint for it has been found: probed 2026-08-21 with `nuthatch doctor`,
        // `bsc.drpc.org` and `binance.llamarpc.com` both refused the getLogs probe outright and could
        // not be asked about archive depth, and `bsc-dataseed.bnbchain.org` answered but is
        // tip-following only, so a from-deployment backfill cannot use it.
        //
        // This surfaced only when #696 put BSC into `all()`: the rule had been passing because the
        // chain that breaks it was not in the list the rule iterates. A short list hid a real RFC-0030
        // §4 violation.
        //
        // Robinhood Chain is the second, recorded the same way (RFC-0050 addendum, item 8). Seven
        // keyless hosts were tried on 2026-09-04 and one cleared the bar: `robinhood.drpc.org`
        // answers the chain id but has no `eth_blockNumber` (`-32601`), `robinhood-rpc.publicnode.com`
        // wants an archive token even for a 10-block getLogs, `rpc.ankr.com/robinhood` is 403,
        // `1rpc.io/robinhood` is `unknown network`, thirdweb's two hosts say `Invalid chain`,
        // Tenderly's gateway is 404, and the explorer's `eth-rpc` sits behind a Cloudflare challenge.
        // The chain is two months old; re-probe before the entry's next measurement.
        //
        // Each exception announces its own obsolescence. It asserts the chain has **exactly one**
        // endpoint, so the day somebody adds a second this test fails and whoever does it deletes
        // that name rather than leaving a stale carve-out behind.
        const SINGLE_ENDPOINT_EXCEPTIONS: &[&str] = &["bsc", "robinhood"];
        for c in all() {
            if SINGLE_ENDPOINT_EXCEPTIONS.contains(&c.name) {
                assert_eq!(
                    c.rpc_urls.len(),
                    1,
                    "{} now ships {} endpoints - delete the exception in this test, it has served \
                     its purpose",
                    c.name,
                    c.rpc_urls.len()
                );
                continue;
            }
            assert!(
                c.rpc_urls.len() >= 2,
                "{} ships {} endpoint(s); RFC-0030 §4 requires at least two so round-robin has \
                 somewhere to fail over. Measure a replacement before removing the last spare.",
                c.name,
                c.rpc_urls.len()
            );
        }
    }

    /// No endpoint appears twice in one chain's list - a duplicate looks like failover and is not.
    #[test]
    fn a_chains_endpoints_are_distinct() {
        for c in all() {
            let mut seen = std::collections::BTreeSet::new();
            for u in c.rpc_urls {
                assert!(seen.insert(*u), "{} lists {u} twice", c.name);
            }
        }
    }

    // ---- #535: `resolve`/`from_config` - a chain outside the built-in three ----------------------

    /// A one-endpoint fake JSON-RPC server answering only `eth_chainId`. Mirrors `rpc::tests::fake_rpc`
    /// (private to that module) - real HTTP, so `resolve`'s actual RPC round trip is exercised.
    async fn fake_chain_id_rpc(chain_id: u64) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handler(State(chain_id): State<u64>, Json(_req): Json<Value>) -> Json<Value> {
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": format!("0x{chain_id:x}")}))
        }

        let app = Router::new().route("/", post(handler)).with_state(chain_id);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn resolve_of_a_known_name_ignores_rpc_and_never_dials_it() {
        // A known chain never needs the network: `resolve` must not even try to dial `--rpc` for it -
        // `127.0.0.1:1` is a port nothing listens on, so a dial here would hang or error.
        let resolved = resolve("mainnet", &["http://127.0.0.1:1".into()])
            .await
            .unwrap();
        assert_eq!(resolved.chain_id, 1);
        assert_eq!(resolved.finality, Finality::Depth(64));
        assert_eq!(resolved.log_window, 20);
    }

    /// Every shipped chain resolves under each of its aliases, with the right id.
    ///
    /// Added with the four chains in the top-25 list (BNB, Polygon, Gnosis, Optimism). A typo in an
    /// alias arm is invisible until somebody types that alias, and then it reads as "nuthatch does
    /// not support this chain" rather than "we spelled it wrong".
    #[test]
    fn every_shipped_chain_resolves_under_all_of_its_aliases() {
        // Also the source of truth for `every_registered_chain_is_probed` below, so a new chain is
        // added here once and both properties are checked from it.
        for (aliases, id) in [
            (&["mainnet", "ethereum", "eth"][..], 1u64),
            (&["arbitrum-one", "arbitrum", "arb", "arb1"][..], 42161),
            (&["base", "base-mainnet", "base-one"][..], 8453),
            (
                &["bsc", "bnb", "binance", "bnb-smart-chain", "bsc-mainnet"][..],
                56,
            ),
            (
                &["polygon", "matic", "polygon-pos", "polygon-mainnet"][..],
                137,
            ),
            (&["gnosis", "xdai", "gnosis-chain"][..], 100),
            (
                &["optimism", "op", "op-mainnet", "optimism-mainnet"][..],
                10,
            ),
            (&["monad", "monad-mainnet", "mon"][..], 143),
            (
                &["robinhood", "robinhood-chain", "robinhood-mainnet", "rh"][..],
                4663,
            ),
        ] {
            for a in aliases {
                let c = lookup(a).unwrap_or_else(|| panic!("alias {a:?} resolves to nothing"));
                assert_eq!(c.chain_id, id, "alias {a:?} points at the wrong chain");
            }
        }
    }

    /// Every chain `lookup` knows must also be **probed** by `init` when `--chain` is omitted (#696).
    ///
    /// `all()` returned three while `lookup` knew seven, so a contract on Optimism, Polygon, BSC or
    /// Gnosis could be indexed by name and never found by auto-detect - and the README said in bold
    /// that omitting `--chain` probes all seven.
    ///
    /// The existing `all_chains_are_probeable_and_lead_with_l1` could not see it: it asserts every
    /// chain in `all()` resolves via `lookup`, which is trivially true of any *subset*. One direction
    /// of a two-list invariant, and the missing direction is the one that rots. Same shape as #353.
    ///
    /// The ids come from the alias table above rather than a fresh literal, so there are two lists in
    /// this file and not three: add a chain, add it there, and this fails until `all()` has it too.
    #[test]
    fn every_registered_chain_is_probed_by_auto_detect() {
        let probed: Vec<u64> = all().iter().map(|c| c.chain_id).collect();
        for id in [1u64, 42161, 8453, 56, 137, 100, 10, 143, 4663] {
            assert!(
                probed.contains(&id),
                "chain {id} resolves via `lookup` but `all()` never probes it, so `init` without \
                 `--chain` cannot find a contract that lives there: probed {probed:?}"
            );
        }
        assert_eq!(
            probed.len(),
            9,
            "a chain was added to `all()` without being added to the alias table above: {probed:?}"
        );
    }

    /// A shipped chain with no endpoint would be worse than an unshipped one: `init` would look
    /// supported and then have nowhere to ask.
    #[test]
    fn no_shipped_chain_has_an_empty_endpoint_list() {
        for name in [
            "mainnet",
            "arbitrum-one",
            "base",
            "bsc",
            "polygon",
            "gnosis",
            "optimism",
        ] {
            let c = lookup(name).unwrap();
            assert!(!c.rpc_urls.is_empty(), "{name} ships with no endpoints");
            assert!(
                c.log_window > 0,
                "{name} has a zero log window, which schedules nothing"
            );
        }
    }

    #[tokio::test]
    async fn resolve_of_an_unknown_name_without_rpc_names_the_remedy() {
        let err = resolve("avalanche", &[]).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown chain 'avalanche'"), "{msg}");
        assert!(msg.contains("--rpc"), "{msg}");
    }

    /// The fix for #535: an unregistered chain name is accepted once `--rpc` names where it lives,
    /// and its `chain_id` comes from the endpoint itself rather than a hand-typed flag.
    #[tokio::test]
    async fn resolve_of_an_unknown_name_with_rpc_reads_the_chain_id_live() {
        let (url, _rpc) = fake_chain_id_rpc(43114).await;
        let resolved = resolve("avalanche", std::slice::from_ref(&url))
            .await
            .unwrap();
        assert_eq!(resolved.name, "avalanche");
        assert_eq!(resolved.chain_id, 43114);
        assert_eq!(resolved.rpc_urls, vec![url]);
        // Same fallback policy the indexer already applies to a nest on an unregistered chain
        // (`indexer::DEFAULT_FINALITY`/`DEFAULT_WINDOW`) - a custom chain must not silently get a
        // laxer or stricter default than the runtime it will actually run under.
        assert_eq!(resolved.finality, UNREGISTERED_FINALITY);
        assert_eq!(resolved.log_window, UNREGISTERED_WINDOW);
    }

    #[test]
    fn from_config_of_a_known_chain_uses_the_registry_not_the_saved_id() {
        // A hand-edited `chain_id` that disagrees with the registry is not this function's problem to
        // catch (RPC startup's `verify_chain_ids` is) - it returns the registry's own policy either way.
        let resolved = from_config("base", 999);
        assert_eq!(resolved.chain_id, 8453);
        assert!(matches!(resolved.finality, Finality::FinalizedTag { .. }));
    }

    #[test]
    fn from_config_of_a_custom_chain_carries_the_saved_id_with_no_rpc_round_trip() {
        let resolved = from_config("avalanche", 43114);
        assert_eq!(resolved.name, "avalanche");
        assert_eq!(resolved.chain_id, 43114);
        assert_eq!(resolved.finality, UNREGISTERED_FINALITY);
        assert_eq!(resolved.log_window, UNREGISTERED_WINDOW);
    }
}
