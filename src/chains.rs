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
        "https://eth-pokt.nodies.app",
        "https://eth.api.onfinality.io/public",
        // Removed 2026-08-06 (issue #267): `eth.drpc.org` answers a 5-request JSON-RPC batch with
        // `Batch of more than 3 requests are not allowed` (HTTP 500), failing RFC-0030 §4 criterion 3.
        // `getLogs` was 5/5 - the batch cap is the disqualifier, and it is not one a narrower block
        // range can fix: it is a *request-count* limit, and the block-timestamp fetcher batches.
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
    // Arbitrum blocks are frequent but Horizon events are rare; a wide window keeps up cheaply.
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
    log_window: 1000,
};

pub fn lookup(name: &str) -> Option<&'static Chain> {
    match name {
        "mainnet" | "ethereum" | "eth" => Some(&MAINNET),
        "arbitrum-one" | "arbitrum" | "arb" | "arb1" => Some(&ARBITRUM_ONE),
        "base" | "base-mainnet" | "base-one" => Some(&BASE),
        _ => None,
    }
}

/// Every registered chain, in auto-detect probe order (L1 first, then the busiest L2s). `init`
/// walks this when `--chain` is omitted: the chain a contract lives on is discoverable, not a
/// thing the user should have to know and type.
pub fn all() -> &'static [&'static Chain] {
    &[&MAINNET, &ARBITRUM_ONE, &BASE]
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
        for c in all() {
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
}
