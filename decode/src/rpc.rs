/// A single log entry as returned by `eth_getLogs`.
///
/// `Serialize`/`Deserialize` are here for RFC-0039's recorded tape, which persists whatever a
/// `Source` returned so a benchmark can be replayed from disk. serde is already a dependency of this
/// crate, so this costs nothing new and does not widen the dbsp-free dependency graph #581 extracted
/// this crate to protect.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
