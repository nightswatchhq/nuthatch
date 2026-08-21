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
}

const MAINNET: Chain = Chain {
    name: "mainnet",
    chain_id: 1,
    rpc_urls: &[
        // **Ordered by measured backfill capability, best first** - see the module note above on why
        // this list has an expiry date. Re-measured 2026-07-31 with a 10-block address-filtered
        // `eth_getLogs` 5,000 blocks behind tip, which is the smallest request a real backfill makes.
        // Measured with `nuthatch doctor --rpc … --address <usdc>` on 2026-08-07, **ordered
        // archive-first** because that is the only limit here with no workaround:
        //   eth-pokt.nodies.app   window 40   batch 10  archive YES
        //   eth.drpc.org          window 160  batch 3   archive YES
        //   onfinality (public)   window 160  batch 3   archive NO
        // A batch cap *degrades* - the timestamp fetcher splits down to it. Missing archive state is
        // fatal to a from-genesis backfill and cannot be split around, so it outranks batch width.
        "https://eth-pokt.nodies.app",
        "https://eth.drpc.org",
        "https://eth.api.onfinality.io/public",
        // Removed 2026-07-31: `ethereum-rpc.publicnode.com` now answers
        // `Archive requests require a personal token` for anything more than ~100 blocks behind tip,
        // so it cannot serve a backfill at all - and it was listed *first*. `eth.llamarpc.com` was
        // returning HTTP 521 (origin down). Both had been "verified" in 2026-07.
    ],
    // ~2 epochs; real finality signals arrive with the ExEx mode. The `finalized` tag exists
    // post-merge but Depth keeps a single conservative policy until ExEx lands.
    finality: Finality::Depth(64),
    log_window: 20,
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
};

/// BNB Smart Chain. **Tip-following works out of the box; a from-deployment backfill does not.**
///
/// Measured 2026-08-19 against the RFC-0030 §4 bar. `bsc-rpc.publicnode.com` is the only keyless
/// endpoint that passes at all - getLogs 5/5, batch-of-5 OK, `finalized` OK - and even it reports
/// **archive depth no**. Six others were probed and every one returned getLogs **0/5** on a 10-block
/// address-filtered request 5,000 behind tip (`bsc-dataseed*.bnbchain.org`, `.defibit.io`,
/// `.ninicoin.io`, `bsc-mainnet.public.blastapi.io`); `1rpc.io/bnb` managed 4/5 with a 40-block
/// ceiling and refused the archive probe on quota; `binance.llamarpc.com`, `bsc.drpc.org`,
/// `bsc-pokt.nodies.app` and `rpc.therpc.io/bsc` were unreachable.
///
/// So this is shipped honestly rather than not shipped: a nest that tip-follows is fine, and one that
/// wants history needs `--rpc <your archive endpoint>`. Saying that here beats a default that dies a
/// thousand blocks into a backfill.
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

    #[test]
    fn base_is_registered_as_op_stack_l2() {
        let c = lookup("base").expect("base in registry");
        assert_eq!(c.chain_id, 8453);
        // OP-stack L2 → same finalized-tag policy as Arbitrum.
        assert!(matches!(c.finality, Finality::FinalizedTag { .. }));
        assert!(!c.rpc_urls.is_empty());
        assert_eq!(lookup("base-mainnet").unwrap().chain_id, 8453);
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
        // The exception announces its own obsolescence. It asserts BSC has **exactly one** endpoint,
        // so the day somebody adds a second this test fails and whoever does it deletes this block
        // rather than leaving a stale carve-out behind.
        const SINGLE_ENDPOINT_EXCEPTION: &str = "bsc";
        for c in all() {
            if c.name == SINGLE_ENDPOINT_EXCEPTION {
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
        for id in [1u64, 42161, 8453, 56, 137, 100, 10] {
            assert!(
                probed.contains(&id),
                "chain {id} resolves via `lookup` but `all()` never probes it, so `init` without \
                 `--chain` cannot find a contract that lives there: probed {probed:?}"
            );
        }
        assert_eq!(
            probed.len(),
            7,
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
