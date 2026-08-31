//! The ingestion `Source` - the seam between "where blocks come from" and everything downstream.
//!
//! Decode, hot store, sealing, IVM, and serving are all oblivious to whether a block arrived by
//! RPC polling or was handed to us in-process by a colocated reth node. That obliviousness is the
//! whole point of this trait: adding ExEx tip-ingestion is a new `Source` impl, never a fork of the
//! indexing logic (per the standing brief - no `#[cfg]` forks of business logic).
//!
//! - `RpcSource` (always available): polls a JSON-RPC endpoint. Works today, no node required.
//! - `ExExSource` (feature = "exex"): the "no third-party" sovereignty upgrade - native-block-time
//!   tip latency from a colocated reth node. Stubbed here with the design written up; see
//!   `docs/exex-design.md`.

use anyhow::Result;
use std::collections::HashMap;

use crate::rpc::{Log, RpcClient};

/// A `getLogs` filter that is known not to mean *every log on the chain*.
///
/// An `eth_getLogs` with an empty address list **and** an empty topic list does not mean "no logs" to
/// a node - it matches everything. A nest with no contract at all (`[extract] blocks = true` and no
/// `[[contracts]]`, OBIB case 3) produces exactly that pair, so the request succeeds and returns an
/// answer that is wrong rather than absent: every log on the chain, fetched and then discarded, since
/// no log can decode without a matching address or topic. The bench harness builds such a registry
/// directly and does reach it; `build_nest` currently refuses the config outright, so the *nest*
/// paths are guarded ahead of the configuration rather than behind it - see
/// `a_contract_free_nest_cannot_be_built_at_all_today` in `indexer.rs`.
///
/// This type exists because the per-call-site guard was written **three times and missed a path each
/// time** - #421 guarded `backfill_direct`, #429 guarded `fetch_logs_splitting`, and both left the two
/// live tip loops issuing it forever (#432). Enumerating call sites is the losing move: the set grows,
/// and a new one is a silent regression rather than a compile error.
///
/// So the guard moves into the type. [`LogFilter::new`] is the only constructor and returns `None` for
/// the empty-and-empty case, and [`Source::logs`] takes a `&LogFilter` rather than two slices. A caller
/// therefore *cannot* hold a value that means every log on the chain, and a new call site cannot ask
/// for one without first deciding what "nothing to ask for" means there. The check is at the type
/// boundary, not at each of the places that fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFilter {
    addresses: Vec<String>,
    topic0s: Vec<String>,
}

impl LogFilter {
    /// The only way to build one. `None` when both halves are empty, which is the case that would
    /// otherwise ask a node for the entire chain's logs.
    ///
    /// Note that *one* empty half is fine and load-bearing: a factory nest deliberately drops the
    /// address filter and matches on topic0 alone, because its children are not known in advance.
    pub fn new(addresses: &[String], topic0s: &[String]) -> Option<Self> {
        (!addresses.is_empty() || !topic0s.is_empty()).then(|| Self {
            addresses: addresses.to_vec(),
            topic0s: topic0s.to_vec(),
        })
    }

    /// The addresses to match. Possibly empty - a factory nest matches on topic0 alone.
    pub fn addresses(&self) -> &[String] {
        &self.addresses
    }

    /// The topic0s to match. Possibly empty - a nest may filter by address alone.
    pub fn topic0s(&self) -> &[String] {
        &self.topic0s
    }
}

/// A source for a role that **never polls one** (#815).
///
/// `serve_role` owns no cursor - its own comment says "No `Source` is ever polled on this role; the
/// parameter exists for the ingest half we discard" - and yet it built an [`RpcClient`] from
/// `config.nest.rpc_urls` to satisfy `build_nest`, whose first parameter is named `_source` and is
/// unused. A vestigial dependency, and not a harmless one: a nest with `rpc_urls = []` could not be
/// served at all, failing with `no RPC URLs configured`.
///
/// That shape is not exotic. It is exactly a **finished, fully-sealed nest** with no chain behind it
/// any more - including the RFC-0016 eval fixture, which is why this was found. The production
/// read-only shadow on the Lodestar box only starts because it happens to carry URLs it never uses.
///
/// Every method errors rather than returning an empty success. If a future change starts polling on
/// a serve-only role, it must fail loudly at the first call rather than silently indexing nothing -
/// an empty result here would be indistinguishable from "the chain had nothing", which is the
/// asymmetry `block_headers` already documents below.
pub struct UnpolledSource;

#[async_trait::async_trait]
impl Source for UnpolledSource {
    async fn tip(&self) -> Result<u64> {
        anyhow::bail!(
            "this role serves a nest without indexing it and has no ingestion source; something \
             asked it for the chain tip, which means a cursor is running where none should be"
        )
    }

    async fn block_hash(&self, _number: u64) -> Result<Option<String>> {
        anyhow::bail!(
            "this role serves a nest without indexing it and has no ingestion source; something \
             asked it for a block hash, which means reorg detection is running where none should be"
        )
    }

    async fn logs(&self, _filter: &LogFilter, _from: u64, _to: u64) -> Result<Vec<Log>> {
        anyhow::bail!(
            "this role serves a nest without indexing it and has no ingestion source; something \
             asked it for logs, which means an ingest loop is running where none should be"
        )
    }
}

/// Everything the indexer needs from an ingestion source. Pull-shaped: the single-cursor loop asks
/// for the tip, verifies canonical hashes (reorg detection), and requests decoded logs for a range.
#[async_trait::async_trait]
pub trait Source: Send + Sync {
    /// Latest block the source can serve.
    async fn tip(&self) -> Result<u64>;

    /// Canonical block hash at `number`, or `None` if the source can't answer (retry later).
    async fn block_hash(&self, number: u64) -> Result<Option<String>>;

    /// The source's `finalized` block number, if it exposes one (L1-aware on an L2). `None` means
    /// "no finalized signal available" - the caller falls back to a depth-based finality policy.
    async fn finalized(&self) -> Result<Option<u64>> {
        Ok(None)
    }

    /// Unix timestamps (seconds) for the given blocks. Best-effort: a source that can't answer omits
    /// them (default: none). Used to populate every row's implicit `block_timestamp` column.
    async fn block_timestamps(&self, _blocks: &[u64]) -> Result<HashMap<u64, u64>> {
        Ok(HashMap::new())
    }

    /// Full block headers for the given blocks (RFC-0036 §4.2), for a nest with `[extract] blocks`.
    ///
    /// Default: none, which means a source that cannot answer produces **no block rows** rather than
    /// empty ones. That asymmetry with `block_timestamps` is deliberate: a missing timestamp leaves a
    /// column unset on a row that still exists, while a missing header would be a missing *row* - and
    /// a gap in a blocks table is indistinguishable from "the chain had no block there".
    async fn block_headers(&self, _blocks: &[u64]) -> Result<HashMap<u64, serde_json::Value>> {
        Ok(HashMap::new())
    }

    /// Full blocks **with transaction bodies**, for a nest with `[extract] top_level_calls`
    /// (RFC-0038 §5).
    ///
    /// Same default and same reasoning as `block_headers`: a source that cannot answer produces no
    /// call rows rather than empty ones, because an empty table is indistinguishable from "this
    /// contract was never called".
    async fn block_bodies(&self, _blocks: &[u64]) -> Result<HashMap<u64, serde_json::Value>> {
        Ok(HashMap::new())
    }

    /// Forget any cached per-block data above `block`, because the chain reorganised there.
    ///
    /// Default: nothing to forget. A source that caches anything keyed by **block number** must
    /// implement this, because a number is not an identity - after a reorg the block at that height is
    /// a different block with a different timestamp, and `block_timestamp` is a sealed column feeding
    /// the segment's content address. Serving a pre-reorg value for a re-indexed block would seal
    /// something a re-execution against the canonical chain could not reproduce.
    ///
    /// On the trait rather than on `RpcClient` so the reorg path, which holds a `dyn Source`, can call
    /// it without downcasting - and so any future caching source is *asked* the question rather than
    /// silently getting it wrong.
    fn forget_cached_above(&self, _block: u64) {}

    /// Logs matching `filter` over the inclusive range `[from, to]`.
    ///
    /// Takes a [`LogFilter`] rather than two slices so that "match everything" is not expressible
    /// here: a caller with nothing to ask for cannot construct one, and so cannot ask (#432).
    async fn logs(&self, filter: &LogFilter, from: u64, to: u64) -> Result<Vec<Log>>;
}

#[async_trait::async_trait]
impl Source for RpcClient {
    fn forget_cached_above(&self, block: u64) {
        self.forget_timestamps_above(block);
    }

    async fn tip(&self) -> Result<u64> {
        self.block_number().await
    }

    async fn block_hash(&self, number: u64) -> Result<Option<String>> {
        // Disambiguate from this trait method - call the inherent one.
        RpcClient::block_hash(self, number).await
    }

    async fn finalized(&self) -> Result<Option<u64>> {
        self.finalized_block().await
    }

    async fn block_timestamps(&self, blocks: &[u64]) -> Result<HashMap<u64, u64>> {
        RpcClient::block_timestamps(self, blocks).await
    }

    async fn block_headers(&self, blocks: &[u64]) -> Result<HashMap<u64, serde_json::Value>> {
        RpcClient::block_headers(self, blocks).await
    }

    async fn block_bodies(&self, blocks: &[u64]) -> Result<HashMap<u64, serde_json::Value>> {
        RpcClient::block_bodies(self, blocks).await
    }

    async fn logs(&self, filter: &LogFilter, from: u64, to: u64) -> Result<Vec<Log>> {
        self.get_logs(filter.addresses(), filter.topic0s(), from, to)
            .await
    }
}

// ---------------------------------------------------------------------------
// ExEx source (feature-gated stub).
//
// A reth Execution Extension is compiled INTO the reth binary and runs in-process, consuming a
// `CanonStateNotification` stream over shared memory - no RPC, no serialization boundary - with
// `ChainCommitted` / `ChainReverted` variants and an `ExExEvent::FinishedHeight` back-channel so
// reth can prune. That gives native-block-time tip latency AND a first-class reorg signal.
//
// The design tension this stub captures: ExEx is PUSH (reth notifies us as blocks commit), while
// the indexer's `Source` is PULL (it asks for `[from,to]`). The bridge is a bounded in-memory
// buffer that a reth ExEx handler fills; `Source::logs` drains the requested range from it. The
// reth wiring itself (a `reth` dependency + node build) is deliberately NOT pulled in here - it's an
// enormous compile that needs a synced node to exercise, so it lands in a dedicated environment.
// This stub establishes the trait boundary and the push→pull contract; see docs/exex-design.md.
// ---------------------------------------------------------------------------
#[cfg(feature = "exex")]
pub mod exex {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// What a reth ExEx handler would push per committed block: its hash and decoded logs.
    #[derive(Default)]
    struct Committed {
        hash: String,
        logs: Vec<Log>,
    }

    /// Bridges reth's push notifications to the indexer's pull `Source`. A real ExEx handler calls
    /// `commit`/`revert` from `CanonStateNotification`; the indexer calls the `Source` methods.
    pub struct ExExSource {
        blocks: Mutex<BTreeMap<u64, Committed>>,
    }

    impl ExExSource {
        pub fn new() -> Self {
            Self {
                blocks: Mutex::new(BTreeMap::new()),
            }
        }

        /// Called by the reth ExEx handler on `ChainCommitted` (per block, pre-decoded).
        pub fn commit(&self, block: u64, hash: String, logs: Vec<Log>) {
            self.blocks
                .lock()
                .unwrap()
                .insert(block, Committed { hash, logs });
        }

        /// Called on `ChainReverted` - drop every buffered block above the revert point. (The hot
        /// store's own rollback still runs; this just keeps the buffer canonical.)
        pub fn revert(&self, from_block: u64) {
            self.blocks.lock().unwrap().retain(|&b, _| b < from_block);
        }
    }

    impl Default for ExExSource {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait::async_trait]
    impl Source for ExExSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .keys()
                .next_back()
                .copied()
                .unwrap_or(0))
        }

        async fn block_hash(&self, number: u64) -> Result<Option<String>> {
            Ok(self
                .blocks
                .lock()
                .unwrap()
                .get(&number)
                .map(|c| c.hash.clone()))
        }

        async fn logs(&self, filter: &LogFilter, from: u64, to: u64) -> Result<Vec<Log>> {
            let blocks = self.blocks.lock().unwrap();
            let mut out = Vec::new();
            for (_, c) in blocks.range(from..=to) {
                for log in &c.logs {
                    let matches_addr = filter
                        .addresses()
                        .iter()
                        .any(|a| log.address.eq_ignore_ascii_case(a));
                    let matches_topic = log
                        .topics
                        .first()
                        .map(|t| filter.topic0s().iter().any(|s| s.eq_ignore_ascii_case(t)))
                        .unwrap_or(false);
                    if matches_addr && matches_topic {
                        out.push(log.clone());
                    }
                }
            }
            Ok(out)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn buffers_commits_and_serves_range() {
            let s = ExExSource::new();
            // ERC-20 Transfer topic0.
            let topic =
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".to_string();
            let log = Log {
                address: "0xabc".into(),
                topics: vec![topic.clone()],
                data: "0x".into(),
                block_number: 10,
                block_hash: "0xhash10".into(),
                tx_hash: "0xt".into(),
                log_index: 0,
            };
            s.commit(10, "0xhash10".into(), vec![log]);
            assert_eq!(s.tip().await.unwrap(), 10);
            assert_eq!(s.block_hash(10).await.unwrap().as_deref(), Some("0xhash10"));
            assert_eq!(
                s.logs(
                    &LogFilter::new(&["0xABC".to_string()], std::slice::from_ref(&topic)).unwrap(),
                    0,
                    20,
                )
                .await
                .unwrap()
                .len(),
                1
            );

            s.revert(10); // reorg drops block 10
            assert_eq!(s.tip().await.unwrap(), 0);
        }
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::LogFilter;

    /// The whole point of the type (#432): the one filter that means *every log on the chain* cannot
    /// be built, so no call site can issue it - present or future.
    ///
    /// The three cases below are not symmetric and the asymmetry is the design. One empty half is
    /// legitimate and load-bearing: a factory nest drops the address filter deliberately, because its
    /// children are not known in advance, and a nest can equally filter by address alone. Only both
    /// halves empty is the unbounded request, so only that case is refused.
    #[test]
    fn the_empty_filter_is_unrepresentable() {
        let addr = ["0xabc".to_string()];
        let topic = ["0xddf2".to_string()];

        assert!(
            LogFilter::new(&[], &[]).is_none(),
            "an empty address AND topic filter is every log on the chain, and must not be buildable"
        );
        assert!(
            LogFilter::new(&addr, &topic).is_some(),
            "an ordinary nest's filter is fine"
        );
        assert!(
            LogFilter::new(&[], &topic).is_some(),
            "topic0-only is a factory nest's filter, not the unbounded one"
        );
        assert!(
            LogFilter::new(&addr, &[]).is_some(),
            "address-only is bounded by the address list"
        );

        // What went in comes back out: the type narrows what is representable without editing the
        // filter, so a refusal is the only behaviour it adds.
        let f = LogFilter::new(&addr, &topic).unwrap();
        assert_eq!(f.addresses(), addr);
        assert_eq!(f.topic0s(), topic);
    }
}
