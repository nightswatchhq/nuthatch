/// A single on-chain log as returned by `eth_getLogs`. Owned by the decode layer
/// (`nuthatch-decode`) so fuzz targets can import it without pulling in the full
/// RPC client (and transitively `dbsp`).
#[derive(Debug, Clone)]
pub struct Log {
    /// Emitting contract. Unused while we filter by a single address in the query, but retained
    /// for multi-contract / ABI-priority decode in later slices.
    #[allow(dead_code)]
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    pub block_number: u64,
    pub block_hash: String,
    pub tx_hash: String,
    pub log_index: u64,
}
