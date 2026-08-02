//! RFC-0023 tier 3 - **pinned-block `eth_call` as a host-side ingestion source.**
//!
//! Tiers 1-2 remove most `eth_call`s by *deriving* state from events (`recipes.rs`) and caching
//! immutable metadata (`metadata.rs`). This module handles what is left: the **irreducible residue** -
//! an oracle read, an ungoverned parameter, a view on a contract whose events we do not fully cover.
//!
//! Three properties make a call result safe to store, and all three come from pinning the block:
//!
//! 1. **Deterministic.** `result = f(code, storage, block, calldata)`. Re-executing the same
//!    declaration at the same block on another machine next year returns the same bytes. `latest`
//!    would break this, which is why [`crate::rpc::RpcClient::eth_call`] is documented as
//!    out-of-band-only and the data path uses `eth_call_at`.
//! 2. **Content-addressable** by `(chain, block, contract, calldata)` - see [`CallKey`]. The address
//!    *is* the identity, so two operators who run the same declaration over the same range produce
//!    byte-identical results and can share segments (tier 4) without trusting each other.
//! 3. **Host-run.** The host issues the call and hands components only *data*, so components stay
//!    zero-capability and pure and may still feed entity derivation.
//!
//! **What this is not:** a way to read `latest`. Nothing here ever touches the chain tip - a nest that
//! wants live state wants a different tool, and asking for it here would trade the determinism
//! non-negotiable for convenience.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The identity of one pinned call result: `(chain, block, contract, calldata)`.
///
/// Deliberately *not* including the nest, the declaration's name, or when it ran. Two different nests
/// on two different machines declaring the same read at the same block are asking an identical
/// question of an identical chain state, and must get an identical answer with an identical address -
/// that is what lets tier 4 share segments without a trust relationship. Folding a nest name in would
/// silently make every operator's results incompatible.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallKey {
    pub chain_id: u64,
    pub block: u64,
    /// Lowercase `0x…` - normalised, because address casing is presentation and must not change the
    /// content address.
    pub contract: String,
    /// Lowercase `0x…` calldata, selector included.
    pub calldata: String,
}

impl CallKey {
    pub fn new(chain_id: u64, block: u64, contract: &str, calldata: &str) -> CallKey {
        CallKey {
            chain_id,
            block,
            contract: contract.to_ascii_lowercase(),
            calldata: calldata.to_ascii_lowercase(),
        }
    }

    /// The content address: SHA-256 over the four fields with an unambiguous separator.
    ///
    /// The separator matters. Concatenating the fields raw would let `(contract "0xab", calldata
    /// "cd")` and `(contract "0xabcd", calldata "")` hash identically - a collision between two
    /// genuinely different reads. A byte that cannot occur in lowercase hex removes the ambiguity.
    pub fn address(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.chain_id.to_be_bytes());
        h.update(b"\x1f");
        h.update(self.block.to_be_bytes());
        h.update(b"\x1f");
        h.update(self.contract.as_bytes());
        h.update(b"\x1f");
        h.update(self.calldata.as_bytes());
        hex::encode(h.finalize())
    }
}

/// One resolved call: its identity, and what the chain answered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallResult {
    pub block: u64,
    pub contract: String,
    pub calldata: String,
    /// The raw return data, lowercase `0x…`. `None` means the call **reverted** at that block, which
    /// is a fact about chain state rather than an error - a getter often does not exist before the
    /// contract is initialised, and recording that honestly beats storing an empty string that reads
    /// like a zero.
    pub result: Option<String>,
    /// `CallKey::address()` - carried so a stored row is self-describing and can be re-verified
    /// without recomputing the key from context that may no longer be around.
    pub address: String,
}

/// A nest's declaration of an irreducible read (RFC-0023 §3: "a nest **declares** an irreducible
/// call... the host schedules the batched calls").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallDecl {
    /// Table name for the results, e.g. `oracle__latest_answer`.
    pub name: String,
    /// The contract to call, `0x…`.
    pub contract: String,
    /// Hex calldata including the 4-byte selector. Fixed arguments only: an argument that varied per
    /// block would make the declaration non-reproducible from the config alone, and reproducibility
    /// from the declaration is the whole point.
    pub calldata: String,
    /// Sample every `every` blocks. State reads are not free and most of them change slowly, so a
    /// declaration says how often it actually needs an answer rather than implying "every block".
    #[serde(default = "default_every")]
    pub every: u64,
    /// First block to sample from. Defaults to the nest's own start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
}

fn default_every() -> u64 {
    1000
}

impl CallDecl {
    /// Validate a declaration at load time rather than at the first RPC round trip.
    ///
    /// Every one of these is a config error that would otherwise surface thousands of blocks into a
    /// backfill, as a wall of identical failures.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("a [[calls]] declaration needs a `name` - it becomes the result table");
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!(
                "call `{}`: name must be [A-Za-z0-9_] - it is used as a table identifier",
                self.name
            );
        }
        let c = self.contract.strip_prefix("0x").unwrap_or(&self.contract);
        if c.len() != 40 || !c.chars().all(|ch| ch.is_ascii_hexdigit()) {
            bail!(
                "call `{}`: `contract` must be a 20-byte 0x address, got {:?}",
                self.name,
                self.contract
            );
        }
        let d = self.calldata.strip_prefix("0x").unwrap_or(&self.calldata);
        if d.len() < 8 || !d.chars().all(|ch| ch.is_ascii_hexdigit()) {
            bail!(
                "call `{}`: `calldata` must be hex with at least a 4-byte selector, got {:?}",
                self.name,
                self.calldata
            );
        }
        if !d.len().is_multiple_of(2) {
            bail!(
                "call `{}`: `calldata` has an odd number of hex digits ({}) - it is not whole bytes",
                self.name,
                d.len()
            );
        }
        // `every = 0` would divide by zero when scheduling; it is also meaningless.
        if self.every == 0 {
            bail!("call `{}`: `every` must be at least 1", self.name);
        }
        Ok(())
    }

    /// The blocks this declaration wants sampled within `[from, to]`.
    ///
    /// Anchored on absolute block numbers (`block % every == 0`), **not** counted from `from`. That is
    /// what makes a resumed or re-ranged backfill sample the *same* blocks as a fresh one - and
    /// therefore produce the same content addresses. Anchoring on `from` would give two operators
    /// different sample sets for the same declaration, which defeats tier 4 sharing entirely.
    pub fn blocks_in(&self, from: u64, to: u64) -> Vec<u64> {
        let start = self.start.unwrap_or(0).max(from);
        if start > to {
            return Vec::new();
        }
        let first = start.div_ceil(self.every) * self.every;
        (first..=to).step_by(self.every as usize).collect()
    }
}

/// Resolve a batch of declarations at one pinned block.
///
/// Returns one [`CallResult`] per declaration, positionally. Declarations are batched into a single
/// JSON-RPC request because they share a block - the same batched-boundary discipline log extraction
/// uses.
pub async fn resolve_at(
    rpc: &crate::rpc::RpcClient,
    chain_id: u64,
    decls: &[CallDecl],
    block: u64,
) -> Result<Vec<CallResult>> {
    let calls: Vec<(String, String)> = decls
        .iter()
        .map(|d| {
            (
                d.contract.to_ascii_lowercase(),
                d.calldata.to_ascii_lowercase(),
            )
        })
        .collect();
    let raw = rpc
        .eth_call_batch_at(&calls, block)
        .await
        .with_context(|| format!("pinned eth_call batch at block {block}"))?;
    Ok(decls
        .iter()
        .zip(raw)
        .map(|(d, r)| {
            let key = CallKey::new(chain_id, block, &d.contract, &d.calldata);
            CallResult {
                block,
                contract: key.contract.clone(),
                calldata: key.calldata.clone(),
                result: r.map(|s| s.to_ascii_lowercase()),
                address: key.address(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(name: &str, every: u64) -> CallDecl {
        CallDecl {
            name: name.into(),
            contract: "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".into(),
            calldata: "0x18160ddd".into(),
            every,
            start: None,
        }
    }

    /// **The RFC-0023 acceptance test**: the same declared call at the same block re-executes to a
    /// byte-identical *address*, across runs and machines.
    ///
    /// Address casing is the case worth pinning down, because it is presentation rather than identity:
    /// two operators who wrote the same address differently must still produce the same segment, or
    /// tier 4 sharing silently degrades into everyone re-running everything.
    #[test]
    fn the_content_address_is_stable_across_casing_and_runs() {
        let a = CallKey::new(
            1,
            19_000_000,
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48",
            "0x18160DDD",
        );
        let b = CallKey::new(
            1,
            19_000_000,
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48",
            "0x18160ddd",
        );
        assert_eq!(
            a.address(),
            b.address(),
            "casing is presentation, not identity"
        );
        assert_eq!(a.address(), a.address(), "and it is stable within a run");
        assert_eq!(a.address().len(), 64);
    }

    /// Every field is part of the identity. If any of them could be changed without changing the
    /// address, two different reads would collide into one stored answer.
    #[test]
    fn every_field_changes_the_address() {
        let base = CallKey::new(1, 100, "0xaa", "0xbb");
        let variants = [
            CallKey::new(2, 100, "0xaa", "0xbb"),
            CallKey::new(1, 101, "0xaa", "0xbb"),
            CallKey::new(1, 100, "0xac", "0xbb"),
            CallKey::new(1, 100, "0xaa", "0xbc"),
        ];
        for v in &variants {
            assert_ne!(
                base.address(),
                v.address(),
                "{v:?} must not collide with the base"
            );
        }
    }

    /// The separator earns its keep: without it, `("0xab","cd")` and `("0xabcd","")` would hash the
    /// same, silently merging two genuinely different reads.
    #[test]
    fn field_boundaries_cannot_be_smeared() {
        let a = CallKey::new(1, 1, "0xab", "cd");
        let b = CallKey::new(1, 1, "0xabcd", "");
        assert_ne!(a.address(), b.address());
    }

    /// Sampling is anchored on absolute block numbers, so a resumed backfill hits the same blocks as a
    /// fresh one. Anchoring on the range start would give two operators different sample sets for the
    /// same declaration - and therefore different content addresses for the same question.
    #[test]
    fn sampling_is_anchored_on_absolute_blocks_not_on_the_range() {
        let d = decl("x", 1000);
        let fresh = d.blocks_in(0, 5000);
        let resumed = d.blocks_in(2500, 5000);
        assert_eq!(fresh, vec![0, 1000, 2000, 3000, 4000, 5000]);
        assert_eq!(resumed, vec![3000, 4000, 5000]);
        assert!(
            resumed.iter().all(|b| fresh.contains(b)),
            "a resumed run must sample a subset of what a fresh run would - got {resumed:?}"
        );
    }

    #[test]
    fn a_declaration_is_validated_before_it_costs_a_round_trip() {
        assert!(decl("ok", 1000).validate().is_ok());

        let mut d = decl("bad", 1000);
        d.contract = "0x1234".into();
        assert!(d.validate().unwrap_err().to_string().contains("20-byte"));

        let mut d = decl("bad", 1000);
        d.calldata = "0x181".into(); // odd digits, and shorter than a selector
        assert!(d.validate().is_err());

        let mut d = decl("bad", 1000);
        d.calldata = "0x18160ddd0".into(); // odd number of hex digits
        assert!(d
            .validate()
            .unwrap_err()
            .to_string()
            .contains("whole bytes"));

        // `every = 0` would divide by zero in scheduling.
        assert!(decl("bad", 0).validate().is_err());

        let mut d = decl("bad", 1000);
        d.name = "not a table".into();
        assert!(d.validate().is_err());
    }

    /// A revert is recorded as `None`, not as an empty string. An empty string reads like a zero-length
    /// return value; `None` says "this call did not answer at this block", which is what a getter on a
    /// not-yet-initialised contract genuinely does.
    #[test]
    fn a_revert_is_distinguishable_from_an_empty_return() {
        let reverted = CallResult {
            block: 1,
            contract: "0xaa".into(),
            calldata: "0xbb".into(),
            result: None,
            address: "x".into(),
        };
        let empty = CallResult {
            result: Some("0x".into()),
            ..reverted.clone()
        };
        assert_ne!(reverted, empty);
        let j = serde_json::to_string(&reverted).unwrap();
        assert!(
            j.contains("\"result\":null"),
            "a revert must serialise as null: {j}"
        );
    }

    /// The determinism claim, **against the real chain** rather than a mock.
    ///
    /// A mock proving my own encoder agrees with itself would not be evidence for "re-executes
    /// byte-for-byte across runs and machines" - the claim is about *the chain*, so the test has to
    /// ask the chain. Skipped without `NUTHATCH_ARCHIVE_RPC` so CI stays hermetic, and **loudly**
    /// skipped, because a silent skip is how "verified" quietly becomes untrue.
    #[tokio::test]
    async fn a_pinned_call_re_executes_identically_against_a_real_archive() {
        let Ok(url) = std::env::var("NUTHATCH_ARCHIVE_RPC") else {
            eprintln!(
                "SKIP a_pinned_call_re_executes_identically_against_a_real_archive: \
                 set NUTHATCH_ARCHIVE_RPC to an archive endpoint to run it"
            );
            return;
        };
        let rpc = crate::rpc::RpcClient::new(vec![url]).unwrap();
        // USDC `totalSupply()` at two fixed historical blocks. Both are long final, so the answers are
        // frozen for good - if these ever change, the endpoint is lying about history.
        let usdc = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
        let decls = vec![CallDecl {
            name: "usdc_total_supply".into(),
            contract: usdc.into(),
            calldata: "0x18160ddd".into(),
            every: 1_000_000,
            start: None,
        }];

        let first = resolve_at(&rpc, 1, &decls, 15_000_000).await.unwrap();
        let again = resolve_at(&rpc, 1, &decls, 15_000_000).await.unwrap();
        assert_eq!(
            first, again,
            "the same pinned call must return the same bytes"
        );
        assert!(
            first[0].result.is_some(),
            "USDC totalSupply() must answer at block 15,000,000 - if it reverts, the endpoint is not \
             serving archive state and this test is measuring nothing"
        );

        // A different block must give a different address, and - for a token that was still being
        // minted - a different answer. That is what proves the pin is doing something.
        let earlier = resolve_at(&rpc, 1, &decls, 12_000_000).await.unwrap();
        assert_ne!(
            first[0].address, earlier[0].address,
            "pinning to a different block must change the content address"
        );
        assert_ne!(
            first[0].result, earlier[0].result,
            "USDC supply moved between blocks 12M and 15M; identical answers would mean the block \
             pin is being ignored and every result is really `latest`"
        );
    }
}
