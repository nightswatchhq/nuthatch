//! Discovery for an address-history nest (RFC-0063 §4): which normal transactions and token
//! transfers touch a watched address, fetched window by window and recorded with their coverage.
//!
//! Normal transactions come from `trace_filter`, once with `fromAddress` and once with `toAddress`
//! (both at once means both, not either), keeping root frames. Token transfers come from `eth_getLogs`
//! with no emitter and the address in a topic position. Each hash is hydrated once and cached.
//!
//! A window is recorded as covered only when every call in it succeeded and nothing looked like a
//! silent empty answer. Two checks catch the latter: an empty trace window is sampled against
//! `trace_block`, and for an externally owned account the outgoing transactions found must match the
//! nonce's movement across the window.

use anyhow::{anyhow, bail, Context, Result};
use futures::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::address_history::{Action, AddressHistory, Row};

/// The actions discovery fills; the others stay incomplete until their slices land.
pub const DISCOVERED: [Action; 4] = [
    Action::TxList,
    Action::TokenTx,
    Action::TokenNftTx,
    Action::Token1155Tx,
];

/// Blocks per recorded window. A failure costs at most this much refetching.
pub const OUTER_WINDOW: u64 = 50_000;
/// A sub-call returning this many items may have been truncated by the provider, so it is split.
const SUSPICIOUSLY_FULL: usize = 10_000;
/// Hashes hydrated at once.
const HYDRATE_CONCURRENCY: usize = 8;

const TRANSFER: &str = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const TRANSFER_SINGLE: &str = "0xc3d58168c5ae7397731d063d5bbf3d657854427343f4c083240f7aacaa2d0f62";
const TRANSFER_BATCH: &str = "0x4a39dc06d4c0dbc64b70af90fd698a233a518aa5d07e595d983b8c0526c8f7fb";

/// One JSON-RPC endpoint pool.
pub trait Rpc: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> impl Future<Output = Result<Value>> + Send;
}

impl Rpc for crate::rpc::RpcClient {
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        crate::rpc::RpcClient::call(self, method, params).await
    }
}

/// An endpoint pool that counts what it is asked, by method.
pub struct Counted<R> {
    inner: R,
    calls: Mutex<BTreeMap<String, u64>>,
}

impl<R> Counted<R> {
    pub fn new(inner: R) -> Counted<R> {
        Counted {
            inner,
            calls: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn calls(&self) -> BTreeMap<String, u64> {
        self.calls.lock().expect("calls lock").clone()
    }

    pub fn inner(&self) -> &R {
        &self.inner
    }
}

impl<R: Rpc> Rpc for Counted<R> {
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        *self
            .calls
            .lock()
            .expect("calls lock")
            .entry(method.to_string())
            .or_default() += 1;
        self.inner.call(method, params).await
    }
}

/// A block span a provider accepts per call, learned from its refusals.
pub struct Window(AtomicU64);

impl Window {
    pub fn new(start: u64) -> Window {
        Window(AtomicU64::new(start.max(1)))
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// Shrink after a refusal: to the limit the error names, if it names one below the current
    /// span, else by half.
    fn shrink(&self, error: &str) -> u64 {
        let now = self.get();
        let named = stated_limit(error).filter(|n| *n >= 1 && *n < now);
        let next = named.unwrap_or(now / 2).max(1);
        self.0.store(next, Ordering::Relaxed);
        next
    }
}

/// The block-span limit an error message states, in the forms providers use:
/// `limited to 100 blocks`, `"maxAllowedRange":16384`, `max range of 2000`.
fn stated_limit(error: &str) -> Option<u64> {
    let lower = error.to_ascii_lowercase();
    for marker in [
        "limited to ",
        "maxallowedrange\":",
        "max range of ",
        "maximum range of ",
    ] {
        if let Some(i) = lower.find(marker) {
            let digits: String = lower[i + marker.len()..]
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(n) = digits.parse() {
                return Some(n);
            }
        }
    }
    None
}

/// Run a ranged call over `[from, to]` in spans the provider accepts, concatenating the arrays.
async fn ranged<R: Rpc>(
    rpc: &R,
    window: &Window,
    from: u64,
    to: u64,
    method: &str,
    params: impl Fn(u64, u64) -> Value,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut start = from;
    while start <= to {
        let span = window.get();
        let end = start.saturating_add(span - 1).min(to);
        match rpc.call(method, params(start, end)).await {
            Ok(Value::Array(items)) if items.len() >= SUSPICIOUSLY_FULL && end > start => {
                window.shrink("");
            }
            Ok(Value::Array(items)) => {
                out.extend(items);
                start = end + 1;
            }
            Ok(other) => bail!("{method} [{start}, {end}] answered a non-list: {other}"),
            Err(e) if end > start => {
                let next = window.shrink(&format!("{e:#}"));
                tracing::debug!(
                    "{method} [{start}, {end}] refused, retrying in spans of {next}: {e:#}"
                );
            }
            Err(e) => {
                return Err(e).with_context(|| format!("{method} at block {start}"));
            }
        }
    }
    Ok(out)
}

fn hex_u64(v: &Value) -> Result<u64> {
    match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| anyhow!("not a u64: {n}")),
        Value::String(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16)
            .with_context(|| format!("not a hex quantity: {s}")),
        other => bail!("not a quantity: {other}"),
    }
}

/// A hex quantity or 32-byte word as the decimal string Etherscan prints.
fn decimal(hex: &str) -> Result<String> {
    let digits = hex.trim_start_matches("0x");
    if digits.is_empty() {
        return Ok("0".into());
    }
    Ok(alloy_primitives::U256::from_str_radix(digits, 16)
        .with_context(|| format!("not a 256-bit hex value: {hex}"))?
        .to_string())
}

fn str_field<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing `{k}` in {v}"))
}

fn topic_address(word: &str) -> String {
    format!("0x{}", &word.trim_start_matches("0x")[24..])
}

fn padded(address: &str) -> String {
    format!("0x{:0>64}", address.trim_start_matches("0x"))
}

/// Cache entries a window fetched, committed once at its end rather than one commit per fetch.
type CachedTx = (Vec<u8>, Map<String, Value>);

#[derive(Default)]
struct Pending {
    txs: Mutex<Vec<CachedTx>>,
    timestamps: Mutex<Vec<(u64, u64)>>,
}

/// What one window found for one address, ready to record.
#[derive(Debug, Default)]
pub struct Found {
    pub txlist: Vec<Row>,
    pub tokentx: Vec<Row>,
    pub tokennfttx: Vec<Row>,
    pub token1155tx: Vec<Row>,
}

/// Discovery over a main endpoint pool (logs, hydration, nonces) and a trace one, which may be the
/// same endpoints.
pub struct Discoverer<M, T> {
    pub main: M,
    pub trace: T,
    pub trace_window: Window,
    pub log_window: Window,
    /// Every this-many empty trace windows, one is checked against `trace_block`; the first always.
    pub empty_sample: u64,
    empties: AtomicU64,
}

impl<M: Rpc, T: Rpc> Discoverer<M, T> {
    pub fn new(main: M, trace: T) -> Discoverer<M, T> {
        Discoverer {
            main,
            trace,
            trace_window: Window::new(100_000),
            log_window: Window::new(100_000),
            empty_sample: 8,
            empties: AtomicU64::new(0),
        }
    }

    /// Discover `[from, to]` for `address` (lowercase `0x…`). An error means nothing may be recorded.
    pub async fn discover(
        &self,
        history: &AddressHistory,
        address: &str,
        from: u64,
        to: u64,
    ) -> Result<Found> {
        let a = address.to_ascii_lowercase();
        let (txs, logs) = futures::try_join!(
            self.normal_transactions(&a, from, to),
            self.token_logs(&a, from, to)
        )?;
        let pending = Pending::default();
        let mut found = Found {
            txlist: self.hydrate(history, &txs, &pending).await?,
            ..Found::default()
        };
        let timestamps = self.timestamps(history, &logs, &pending).await?;
        for log in &logs {
            transfer_rows(log, &a, &timestamps, &mut found)?;
        }
        history.cache_txs(&pending.txs.into_inner().expect("pending lock"))?;
        history.cache_block_timestamps(&pending.timestamps.into_inner().expect("pending lock"))?;
        Ok(found)
    }

    /// Hashes of root frames from or to `a`, with their block.
    async fn normal_transactions(&self, a: &str, from: u64, to: u64) -> Result<Vec<(String, u64)>> {
        let mut frames = Vec::new();
        for side in ["fromAddress", "toAddress"] {
            frames.extend(
                ranged(&self.trace, &self.trace_window, from, to, "trace_filter", |s, e| {
                    json!([{ "fromBlock": format!("0x{s:x}"), "toBlock": format!("0x{e:x}"), side: [a] }])
                })
                .await?,
            );
        }
        if frames.is_empty() {
            self.check_empty_traces(from, to).await?;
        }
        let mut roots: BTreeMap<String, (u64, bool)> = BTreeMap::new();
        for f in &frames {
            let root = f
                .get("traceAddress")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty);
            let Some(hash) = f.get("transactionHash").and_then(Value::as_str) else {
                continue;
            };
            if !root {
                continue;
            }
            let action = f.get("action").unwrap_or(&Value::Null);
            let sender = action.get("from").and_then(Value::as_str).unwrap_or("");
            let receiver = action.get("to").and_then(Value::as_str).unwrap_or("");
            let outgoing = sender.eq_ignore_ascii_case(a);
            if !outgoing && !receiver.eq_ignore_ascii_case(a) {
                continue;
            }
            let block = hex_u64(f.get("blockNumber").unwrap_or(&Value::Null))?;
            let e = roots
                .entry(hash.to_ascii_lowercase())
                .or_insert((block, false));
            e.1 |= outgoing;
        }
        let outgoing = roots.values().filter(|(_, out)| *out).count() as u64;
        self.check_nonce(a, from, to, outgoing).await?;
        Ok(roots.into_iter().map(|(h, (b, _))| (h, b)).collect())
    }

    /// An empty answer from a filter is only believed if the trace source demonstrably has traces
    /// for the range: some providers answer `[]` for blocks they never traced.
    async fn check_empty_traces(&self, from: u64, to: u64) -> Result<()> {
        let n = self.empties.fetch_add(1, Ordering::Relaxed);
        if !n.is_multiple_of(self.empty_sample.max(1)) {
            return Ok(());
        }
        let probe = from + (to - from) / 2;
        let traced = self
            .trace
            .call("trace_block", json!([format!("0x{probe:x}")]))
            .await
            .context("trace_block, checking an empty trace_filter window")?;
        if traced.as_array().is_some_and(|t| !t.is_empty()) {
            return Ok(());
        }
        let count = self
            .main
            .call(
                "eth_getBlockTransactionCountByNumber",
                json!([format!("0x{probe:x}")]),
            )
            .await?;
        if hex_u64(&count)? > 0 {
            bail!(
                "the trace source returned no traces for block {probe}, which holds transactions; \
                 not recording [{from}, {to}] as covered"
            );
        }
        Ok(())
    }

    /// For an EOA every sent transaction moves the nonce by one, so the outgoing root frames found
    /// must equal the nonce's movement. A contract's nonce counts its creations instead, and a
    /// delegated account (EIP-7702) can move without sending, so neither is checked.
    async fn check_nonce(&self, a: &str, from: u64, to: u64, outgoing: u64) -> Result<()> {
        // Code is read at the window's end, not today: an account delegated under EIP-7702 since
        // was a plain EOA through all of its earlier history.
        let code = self
            .main
            .call("eth_getCode", json!([a, format!("0x{to:x}")]))
            .await?;
        if code.as_str() != Some("0x") {
            return Ok(());
        }
        let nonce_at = |b: u64| async move {
            let n = self
                .main
                .call("eth_getTransactionCount", json!([a, format!("0x{b:x}")]))
                .await?;
            hex_u64(&n)
        };
        let before = if from == 0 {
            0
        } else {
            nonce_at(from - 1).await?
        };
        let after = nonce_at(to).await?;
        let moved = after.saturating_sub(before);
        if moved != outgoing {
            bail!(
                "{a} sent {moved} transactions in [{from}, {to}] by its nonce, and the traces show \
                 {outgoing}; not recording the window as covered"
            );
        }
        Ok(())
    }

    async fn token_logs(&self, a: &str, from: u64, to: u64) -> Result<Vec<Value>> {
        let who = padded(a);
        let filters = [
            json!([TRANSFER, who]),
            json!([TRANSFER, null, who]),
            json!([[TRANSFER_SINGLE, TRANSFER_BATCH], null, who]),
            json!([[TRANSFER_SINGLE, TRANSFER_BATCH], null, null, who]),
        ];
        let mut seen = BTreeSet::new();
        let mut logs = Vec::new();
        for topics in filters {
            let got = ranged(&self.main, &self.log_window, from, to, "eth_getLogs", |s, e| {
                json!([{ "fromBlock": format!("0x{s:x}"), "toBlock": format!("0x{e:x}"), "topics": topics }])
            })
            .await?;
            for log in got {
                if log.get("removed").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let key = (
                    str_field(&log, "transactionHash")?.to_ascii_lowercase(),
                    hex_u64(log.get("logIndex").unwrap_or(&Value::Null))?,
                );
                if seen.insert(key) {
                    logs.push(log);
                }
            }
        }
        Ok(logs)
    }

    /// Block timestamps for the logs' blocks: from the log when the provider includes it, else the
    /// cache, else one header read per block.
    async fn timestamps(
        &self,
        history: &AddressHistory,
        logs: &[Value],
        pending: &Pending,
    ) -> Result<BTreeMap<u64, u64>> {
        let mut ts = BTreeMap::new();
        let mut missing = BTreeSet::new();
        for log in logs {
            let block = hex_u64(log.get("blockNumber").unwrap_or(&Value::Null))?;
            match log.get("blockTimestamp") {
                Some(t) => {
                    ts.insert(block, hex_u64(t)?);
                }
                None => {
                    missing.insert(block);
                }
            }
        }
        for b in missing {
            if let std::collections::btree_map::Entry::Vacant(slot) = ts.entry(b) {
                slot.insert(self.block_timestamp(history, b, pending).await?);
            }
        }
        Ok(ts)
    }

    async fn block_timestamp(
        &self,
        history: &AddressHistory,
        block: u64,
        pending: &Pending,
    ) -> Result<u64> {
        if let Some(t) = history.block_timestamp(block)? {
            return Ok(t);
        }
        let header = self
            .main
            .call(
                "eth_getBlockByNumber",
                json!([format!("0x{block:x}"), false]),
            )
            .await?;
        let t = hex_u64(header.get("timestamp").unwrap_or(&Value::Null))
            .with_context(|| format!("block {block} header"))?;
        pending
            .timestamps
            .lock()
            .expect("pending lock")
            .push((block, t));
        Ok(t)
    }

    async fn hydrate_one(
        &self,
        history: &AddressHistory,
        hash: String,
        block: u64,
        pending: &Pending,
    ) -> Result<Map<String, Value>> {
        let key = alloy_primitives::hex::decode(hash.trim_start_matches("0x"))?;
        if let Some(r) = history.cached_tx(&key)? {
            return Ok(r);
        }
        let (tx, receipt, ts) = futures::try_join!(
            self.main.call("eth_getTransactionByHash", json!([hash])),
            self.main.call("eth_getTransactionReceipt", json!([hash])),
            self.block_timestamp(history, block, pending)
        )?;
        if tx.is_null() || receipt.is_null() {
            bail!("{hash} has no transaction or receipt at the main endpoint");
        }
        let record = txlist_record(&tx, &receipt, ts)?;
        pending
            .txs
            .lock()
            .expect("pending lock")
            .push((key, record.clone()));
        Ok(record)
    }

    /// Each hash's Etherscan `txlist` record: from the cache if it was hydrated before, else from
    /// the transaction, its receipt and its block's timestamp, cached as it is built.
    async fn hydrate(
        &self,
        history: &AddressHistory,
        txs: &[(String, u64)],
        pending: &Pending,
    ) -> Result<Vec<Row>> {
        let records: Vec<Result<Map<String, Value>>> = futures::stream::iter(txs.iter().cloned())
            .map(|(hash, block)| self.hydrate_one(history, hash, block, pending))
            .buffer_unordered(HYDRATE_CONCURRENCY)
            .collect()
            .await;
        records
            .into_iter()
            .map(|r| {
                let record = r?;
                let num = |k: &str| -> Result<u64> {
                    record
                        .get(k)
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow!("record lacks {k}"))?
                        .parse()
                        .with_context(|| format!("record {k}"))
                };
                Ok(Row {
                    block: num("blockNumber")?,
                    tx_index: num("transactionIndex")?,
                    position: 0,
                    record,
                })
            })
            .collect()
    }
}

/// The `txlist` fields Etherscan prints, in its order, from a transaction and its receipt.
/// `functionName` needs the callee's ABI and `confirmations` changes by the block, so neither is
/// kept. `gasPrice` is what the sender paid: the receipt's effective price.
pub fn txlist_record(tx: &Value, receipt: &Value, timestamp: u64) -> Result<Map<String, Value>> {
    let dec = |v: &Value, k: &str| -> Result<String> { decimal(str_field(v, k)?) };
    let status = receipt.get("status").and_then(Value::as_str);
    let input = str_field(tx, "input")?.to_string();
    let method_id = if input.len() >= 10 {
        input[..10].to_string()
    } else {
        "0x".to_string()
    };
    let to = tx
        .get("to")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let created = receipt
        .get("contractAddress")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let gas_price = match receipt.get("effectiveGasPrice").and_then(Value::as_str) {
        Some(p) => decimal(p)?,
        None => dec(tx, "gasPrice")?,
    };
    let mut r = Map::new();
    let mut put = |k: &str, v: String| {
        r.insert(k.to_string(), Value::String(v));
    };
    put("blockNumber", dec(tx, "blockNumber")?);
    put(
        "blockHash",
        str_field(tx, "blockHash")?.to_ascii_lowercase(),
    );
    put("timeStamp", timestamp.to_string());
    put("hash", str_field(tx, "hash")?.to_ascii_lowercase());
    put("nonce", dec(tx, "nonce")?);
    put("transactionIndex", dec(tx, "transactionIndex")?);
    put("from", str_field(tx, "from")?.to_ascii_lowercase());
    put("to", to.to_ascii_lowercase());
    put("value", dec(tx, "value")?);
    put("gas", dec(tx, "gas")?);
    put("gasPrice", gas_price);
    put("input", input);
    put("methodId", method_id);
    put("contractAddress", created.to_ascii_lowercase());
    put("cumulativeGasUsed", dec(receipt, "cumulativeGasUsed")?);
    put(
        "txreceipt_status",
        match status {
            Some(s) => decimal(s)?,
            None => String::new(),
        },
    );
    put("gasUsed", dec(receipt, "gasUsed")?);
    put(
        "isError",
        if status.is_some_and(|s| decimal(s).is_ok_and(|d| d == "0")) {
            "1".into()
        } else {
            "0".into()
        },
    );
    Ok(r)
}

/// One log's Etherscan rows: an ERC-20 `Transfer` (three topics) goes to `tokentx`, an ERC-721
/// `Transfer` (four, the token id indexed) to `tokennfttx`, an ERC-1155 transfer to `token1155tx`
/// with one row per id. Rows sort by log index, then by element within a batch.
fn transfer_rows(
    log: &Value,
    a: &str,
    timestamps: &BTreeMap<u64, u64>,
    found: &mut Found,
) -> Result<()> {
    let topics: Vec<&str> = log
        .get("topics")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("log without topics: {log}"))?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let block = hex_u64(log.get("blockNumber").unwrap_or(&Value::Null))?;
    let tx_index = hex_u64(log.get("transactionIndex").unwrap_or(&Value::Null))?;
    let log_index = hex_u64(log.get("logIndex").unwrap_or(&Value::Null))?;
    let ts = timestamps
        .get(&block)
        .ok_or_else(|| anyhow!("no timestamp for block {block}"))?;
    let data = str_field(log, "data")?.trim_start_matches("0x");
    let word = |i: usize| -> Result<String> {
        data.get(i * 64..(i + 1) * 64)
            .map(|w| format!("0x{w}"))
            .ok_or_else(|| anyhow!("log data too short: {log}"))
    };
    let base = || -> Result<Map<String, Value>> {
        let mut r = Map::new();
        r.insert("blockNumber".into(), Value::String(block.to_string()));
        r.insert("timeStamp".into(), Value::String(ts.to_string()));
        r.insert(
            "hash".into(),
            Value::String(str_field(log, "transactionHash")?.to_ascii_lowercase()),
        );
        r.insert(
            "blockHash".into(),
            Value::String(str_field(log, "blockHash")?.to_ascii_lowercase()),
        );
        r.insert(
            "contractAddress".into(),
            Value::String(str_field(log, "address")?.to_ascii_lowercase()),
        );
        Ok(r)
    };
    let tail = |r: &mut Map<String, Value>| {
        r.insert(
            "transactionIndex".into(),
            Value::String(tx_index.to_string()),
        );
        r.insert("logIndex".into(), Value::String(log_index.to_string()));
    };
    let row = |record, element: u64| Row {
        block,
        tx_index,
        position: (log_index << 16) | element,
        record,
    };
    let party = |t: &str| topic_address(t);
    match (topics.first().copied(), topics.len()) {
        (Some(TRANSFER), 3) => {
            let mut r = base()?;
            r.insert("from".into(), Value::String(party(topics[1])));
            r.insert("to".into(), Value::String(party(topics[2])));
            r.insert("value".into(), Value::String(decimal(&word(0)?)?));
            tail(&mut r);
            found.tokentx.push(row(r, 0));
        }
        (Some(TRANSFER), 4) => {
            let mut r = base()?;
            r.insert("from".into(), Value::String(party(topics[1])));
            r.insert("to".into(), Value::String(party(topics[2])));
            r.insert("tokenID".into(), Value::String(decimal(topics[3])?));
            tail(&mut r);
            found.tokennfttx.push(row(r, 0));
        }
        (Some(TRANSFER_SINGLE), 4) => {
            let mut r = base()?;
            r.insert("from".into(), Value::String(party(topics[2])));
            r.insert("to".into(), Value::String(party(topics[3])));
            r.insert("tokenID".into(), Value::String(decimal(&word(0)?)?));
            r.insert("tokenValue".into(), Value::String(decimal(&word(1)?)?));
            tail(&mut r);
            found.token1155tx.push(row(r, 0));
        }
        (Some(TRANSFER_BATCH), 4) => {
            let (ids, values) = batch_arrays(data)?;
            for (i, (id, value)) in ids.iter().zip(&values).enumerate() {
                let mut r = base()?;
                r.insert("from".into(), Value::String(party(topics[2])));
                r.insert("to".into(), Value::String(party(topics[3])));
                r.insert("tokenID".into(), Value::String(id.clone()));
                r.insert("tokenValue".into(), Value::String(value.clone()));
                tail(&mut r);
                found.token1155tx.push(row(r, i as u64));
            }
        }
        _ => {
            tracing::debug!(
                "address history: skipping an unrecognised transfer log for {a}: {log}"
            );
        }
    }
    Ok(())
}

/// `TransferBatch` data: two dynamic `uint256[]`, ids then values, as decimal strings.
fn batch_arrays(data: &str) -> Result<(Vec<String>, Vec<String>)> {
    let word = |i: usize| -> Result<&str> {
        data.get(i * 64..(i + 1) * 64)
            .ok_or_else(|| anyhow!("TransferBatch data too short"))
    };
    let at = |i: usize| -> Result<usize> {
        usize::try_from(alloy_primitives::U256::from_str_radix(word(i)?, 16)?)
            .map_err(|_| anyhow!("TransferBatch offset out of range"))
    };
    let array = |offset_word: usize| -> Result<Vec<String>> {
        let start = at(offset_word)? / 32;
        let len = at(start)?;
        (0..len)
            .map(|k| decimal(&format!("0x{}", word(start + 1 + k)?)))
            .collect()
    };
    let (ids, values) = (array(0)?, array(1)?);
    if ids.len() != values.len() {
        bail!(
            "TransferBatch with {} ids and {} values",
            ids.len(),
            values.len()
        );
    }
    Ok((ids, values))
}

/// The first block at or after `start` that some discovered action has not covered for `address`.
pub fn next_uncovered(history: &AddressHistory, address: &str, start: u64) -> Result<u64> {
    let mut next = u64::MAX;
    for action in DISCOVERED {
        let mut at = start;
        for (from, to) in history.coverage(action, address)? {
            if from <= at && at <= to {
                at = to.saturating_add(1);
            }
        }
        next = next.min(at);
    }
    Ok(next)
}

/// Record a window's finds, every action's rows with its coverage, under the generation the
/// window's fetch began in.
pub fn record(
    history: &AddressHistory,
    address: &str,
    (from, to): (u64, u64),
    found: Found,
    generation: u64,
) -> Result<()> {
    for (action, rows) in [
        (Action::TxList, found.txlist),
        (Action::TokenTx, found.tokentx),
        (Action::TokenNftTx, found.tokennfttx),
        (Action::Token1155Tx, found.token1155tx),
    ] {
        history.record(action, address, &rows, (from, to), generation)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn a_stated_limit_is_read_from_each_providers_wording() {
        assert_eq!(
            stated_limit("-32602: Block range too large; currently limited to 100 blocks"),
            Some(100)
        );
        assert_eq!(
            stated_limit(r#"{"details":{"requestRange":100001,"maxAllowedRange":16384}}"#),
            Some(16_384)
        );
        assert_eq!(
            stated_limit("query exceeds max range of 2000 blocks"),
            Some(2_000)
        );
        assert_eq!(stated_limit("timeout"), None);
        let w = Window::new(100_000);
        assert_eq!(w.shrink("currently limited to 100 blocks"), 100);
        assert_eq!(w.shrink("timeout"), 50);
        assert_eq!(
            w.shrink("limited to 500 blocks"),
            25,
            "a stated limit above now is ignored"
        );
    }

    /// A provider that silently truncates at 10,000 results: a span answering that many is split
    /// until each answer is believably whole.
    #[tokio::test]
    async fn a_suspiciously_full_answer_is_split() {
        let rpc = script(|_, p| {
            let s = hex_u64(&p[0]["fromBlock"])?;
            let e = hex_u64(&p[0]["toBlock"])?;
            let n = if e - s + 1 > 2 {
                SUSPICIOUSLY_FULL
            } else {
                (e - s + 1) as usize
            };
            Ok(Value::Array(vec![json!(1); n]))
        });
        let w = Window::new(100_000);
        let got = ranged(
            &rpc,
            &w,
            0,
            3,
            "eth_getLogs",
            |s, e| json!([{ "fromBlock": format!("0x{s:x}"), "toBlock": format!("0x{e:x}") }]),
        )
        .await
        .unwrap();
        assert_eq!(got.len(), 4, "one item per block, not a truncated page");
    }

    #[test]
    fn a_batch_transfer_decodes_every_id() {
        // ids [1, 2], values [10, 20]
        let words = ["40", "a0", "2", "1", "2", "2", "a", "14"].map(|w| format!("{w:0>64}"));
        let (ids, values) = batch_arrays(&words.concat()).unwrap();
        assert_eq!(ids, ["1", "2"]);
        assert_eq!(values, ["10", "20"]);
    }

    #[test]
    fn a_txlist_record_matches_etherscans_fields() {
        let tx = json!({
            "blockNumber": "0x112a89e", "blockHash": "0xAB", "hash": "0xCD", "nonce": "0x47",
            "transactionIndex": "0x79", "from": "0x31148E9423e6abdcdcf4cd6b13cbebdeca29a9fe",
            "to": null, "value": "0x0", "gas": "0xa0a4", "gasPrice": "0x1", "input": "0x60806040aa",
        });
        let receipt = json!({
            "status": "0x0", "gasUsed": "0x5208", "cumulativeGasUsed": "0x10",
            "effectiveGasPrice": "0x47a9e8d5c", "contractAddress": "0xBEEF",
        });
        let r = txlist_record(&tx, &receipt, 1_693_067_255).unwrap();
        assert_eq!(r["blockNumber"], "18000030");
        assert_eq!(r["nonce"], "71");
        assert_eq!(r["to"], "", "a creation has an empty to");
        assert_eq!(r["contractAddress"], "0xbeef");
        assert_eq!(r["gasPrice"], "19237080412", "the price paid, not the cap");
        assert_eq!(r["isError"], "1");
        assert_eq!(r["txreceipt_status"], "0");
        assert_eq!(r["methodId"], "0x60806040");
        assert_eq!(r["timeStamp"], "1693067255");
    }

    type Answer = dyn Fn(&str, &Value) -> Result<Value> + Send + Sync;

    /// A scripted endpoint: answers by method, recording what it was asked.
    struct Script {
        answer: Box<Answer>,
        asked: Mutex<Vec<(String, Value)>>,
    }
    impl Rpc for Script {
        async fn call(&self, method: &str, params: Value) -> Result<Value> {
            self.asked
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            (self.answer)(method, &params)
        }
    }
    fn script(f: impl Fn(&str, &Value) -> Result<Value> + Send + Sync + 'static) -> Script {
        Script {
            answer: Box::new(f),
            asked: Mutex::new(Vec::new()),
        }
    }

    const A: &str = "0xd8da6bf26964af9d7eed9e03e53415d37aa96045";

    fn history(dir: &std::path::Path) -> AddressHistory {
        AddressHistory::open(Store::open(&dir.join("t.redb")).unwrap(), 1, &[A.into()]).unwrap()
    }

    fn tx_json(hash: &str, block: u64, nonce: u64) -> Value {
        json!({
            "blockNumber": format!("0x{block:x}"), "blockHash": "0xbb", "hash": hash,
            "nonce": format!("0x{nonce:x}"), "transactionIndex": "0x0", "from": A, "to": "0x01",
            "value": "0x0", "gas": "0x5208", "gasPrice": "0x1", "input": "0x",
        })
    }

    fn receipt_json() -> Value {
        json!({"status": "0x1", "gasUsed": "0x5208", "cumulativeGasUsed": "0x5208",
               "effectiveGasPrice": "0x1", "contractAddress": null})
    }

    /// A chain where `A` sends one transaction at block 150 (nonce 0 -> 1), with a trace source
    /// that refuses spans over 100 blocks and a log source that refuses spans over 64.
    fn chain(empty_traces_have_no_data: bool) -> (Script, Script) {
        let main = script(move |m, p| {
            Ok(match m {
                "eth_getCode" => json!("0x"),
                "eth_getTransactionCount" => {
                    let b =
                        u64::from_str_radix(p[1].as_str().unwrap().trim_start_matches("0x"), 16)?;
                    json!(if b >= 150 { "0x1" } else { "0x0" })
                }
                "eth_getLogs" => {
                    let f = &p[0];
                    let s = hex_u64(&f["fromBlock"])?;
                    let e = hex_u64(&f["toBlock"])?;
                    if e - s + 1 > 64 {
                        bail!(
                            "getLogs request exceeded max allowed range {{\"maxAllowedRange\":64}}"
                        );
                    }
                    json!([])
                }
                "eth_getTransactionByHash" => tx_json(p[0].as_str().unwrap(), 150, 0),
                "eth_getTransactionReceipt" => receipt_json(),
                "eth_getBlockByNumber" => json!({"timestamp": "0x10"}),
                "eth_getBlockTransactionCountByNumber" => json!("0x5"),
                other => bail!("unexpected {other}"),
            })
        });
        let trace = script(move |m, p| {
            Ok(match m {
                "trace_filter" => {
                    let f = &p[0];
                    let s = hex_u64(&f["fromBlock"])?;
                    let e = hex_u64(&f["toBlock"])?;
                    if e - s + 1 > 100 {
                        bail!("Block range too large; currently limited to 100 blocks");
                    }
                    if f.get("fromAddress").is_some() && s <= 150 && 150 <= e {
                        json!([{"transactionHash": "0xaa", "blockNumber": 150, "traceAddress": [],
                                "type": "call", "action": {"from": A, "to": "0x01"}}])
                    } else if f.get("toAddress").is_some() && s <= 300 && 300 <= e {
                        // An internal call to A: a txlistinternal row, never a txlist one.
                        json!([{"transactionHash": "0xbb", "blockNumber": 300, "traceAddress": [0],
                                "type": "call", "action": {"from": "0x02", "to": A}}])
                    } else {
                        json!([])
                    }
                }
                "trace_block" => {
                    if empty_traces_have_no_data {
                        json!([])
                    } else {
                        json!([{"x": 1}])
                    }
                }
                other => bail!("unexpected {other}"),
            })
        });
        (main, trace)
    }

    #[tokio::test]
    async fn a_window_finds_a_sent_transaction_and_learns_the_limits() {
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let (main, trace) = chain(false);
        let d = Discoverer::new(main, trace);
        let found = d.discover(&h, A, 0, 499).await.unwrap();
        assert_eq!(
            found.txlist.len(),
            1,
            "the internal call at 300 is not a normal transaction"
        );
        assert_eq!(found.txlist[0].record["hash"], "0xaa");
        assert_eq!(
            d.trace_window.get(),
            100,
            "learned from the trace source's refusal"
        );
        assert_eq!(
            d.log_window.get(),
            64,
            "learned from the log source's refusal"
        );

        // Hydrated once: a second pass over the window asks for no transaction again.
        let before = d.main.asked.lock().unwrap().len();
        d.discover(&h, A, 0, 499).await.unwrap();
        let again: Vec<String> = d.main.asked.lock().unwrap()[before..]
            .iter()
            .map(|(m, _)| m.clone())
            .collect();
        assert!(
            !again
                .iter()
                .any(|m| m == "eth_getTransactionByHash" || m == "eth_getTransactionReceipt"),
            "{again:?}"
        );
    }

    #[tokio::test]
    async fn a_trace_source_with_no_traces_is_not_believed() {
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let (main, trace) = chain(true);
        let d = Discoverer::new(main, trace);
        let err = d.discover(&h, A, 1_000, 1_099).await.unwrap_err();
        assert!(
            format!("{err:#}").contains("no traces for block"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn a_nonce_that_moved_without_a_trace_fails_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let (main, _) = chain(false);
        // A trace source that sees nothing at all, though the nonce moved at 150.
        let blind = script(|m, _| match m {
            "trace_filter" => Ok(json!([])),
            "trace_block" => Ok(json!([{"x": 1}])),
            other => bail!("unexpected {other}"),
        });
        let d = Discoverer::new(main, blind);
        let err = d.discover(&h, A, 100, 199).await.unwrap_err();
        assert!(format!("{err:#}").contains("by its nonce"), "{err:#}");
    }

    /// Code at the window's end means a contract or a delegated account, whose nonce does not count
    /// sent transactions; code only later (a delegation made since) still leaves the window checked.
    #[tokio::test]
    async fn the_nonce_check_reads_code_at_the_windows_end() {
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let blind = || {
            script(|m, _| match m {
                "trace_filter" => Ok(json!([])),
                "trace_block" => Ok(json!([{"x": 1}])),
                other => bail!("unexpected {other}"),
            })
        };
        let code_from = |delegated_at: u64| {
            let (main, _) = chain(false);
            script(move |m, p| match m {
                "eth_getCode" => {
                    let b = hex_u64(&p[1])?;
                    Ok(json!(if b >= delegated_at {
                        "0xef0100aa"
                    } else {
                        "0x"
                    }))
                }
                _ => (main.answer)(m, p),
            })
        };
        let d = Discoverer::new(code_from(150), blind());
        d.discover(&h, A, 100, 199)
            .await
            .expect("delegated by the window's end, so the nonce is not held to the traces");
        let d = Discoverer::new(code_from(10_000), blind());
        let err = d.discover(&h, A, 100, 199).await.unwrap_err();
        assert!(format!("{err:#}").contains("by its nonce"), "{err:#}");
    }

    #[tokio::test]
    async fn token_logs_become_rows_by_standard_and_dedupe() {
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let who = padded(A);
        let other = padded("0x0000000000000000000000000000000000000001");
        let log = |topics: Value, data: &str, index: u64| {
            json!({"address": "0xT0KEN", "topics": topics, "data": data, "blockNumber": "0x10",
                   "blockTimestamp": "0x20", "transactionHash": "0xHH", "transactionIndex": "0x1",
                   "logIndex": format!("0x{index:x}"), "blockHash": "0xBB", "removed": false})
        };
        let erc20 = log(json!([TRANSFER, who, who]), &format!("0x{:0>64}", "64"), 1);
        let nft = log(
            json!([TRANSFER, other, who, format!("0x{:0>64}", "7")]),
            "0x",
            2,
        );
        let single = log(
            json!([TRANSFER_SINGLE, other, other, who]),
            &format!("0x{:0>64}{:0>64}", "5", "3"),
            3,
        );
        let (main, trace) = chain(false);
        let logs = vec![erc20.clone(), erc20, nft, single];
        let main = script(move |m, p| match m {
            "eth_getLogs" => Ok(json!(logs)),
            _ => (main.answer)(m, p),
        });
        let d = Discoverer::new(main, trace);
        let found = d.discover(&h, A, 0, 63).await.unwrap();
        assert_eq!(
            found.tokentx.len(),
            1,
            "the same log through two filters is one row"
        );
        assert_eq!(found.tokentx[0].record["value"], "100");
        assert_eq!(
            found.tokentx[0].record["timeStamp"], "32",
            "from the log's blockTimestamp"
        );
        assert_eq!(found.tokennfttx.len(), 1);
        assert_eq!(found.tokennfttx[0].record["tokenID"], "7");
        assert_eq!(found.token1155tx.len(), 1);
        assert_eq!(found.token1155tx[0].record["tokenValue"], "3");
    }

    #[test]
    fn next_uncovered_is_the_least_covered_action() {
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let g = h.generation().unwrap();
        assert_eq!(next_uncovered(&h, A, 10).unwrap(), 10);
        record(&h, A, (10, 99), Found::default(), g).unwrap();
        assert_eq!(next_uncovered(&h, A, 10).unwrap(), 100);
        h.record(Action::TxList, A, &[], (100, 199), g).unwrap();
        assert_eq!(
            next_uncovered(&h, A, 10).unwrap(),
            100,
            "token coverage still ends at 99"
        );
    }

    fn mode(h: &AddressHistory, end: u64) -> crate::address_mode::ModeState {
        crate::address_mode::ModeState::new(h.clone(), 1, std::time::Duration::from_secs(300), true)
            .with_range(0, Some(end), 0)
    }

    fn filtered_from(asked: &Mutex<Vec<(String, Value)>>) -> Vec<u64> {
        asked
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == "trace_filter")
            .map(|(_, p)| hex_u64(&p[0]["fromBlock"]).unwrap())
            .collect()
    }

    /// The cursor's catch-up covers every window to `end_block`, and a later pass starts where
    /// coverage ends rather than refetching.
    #[tokio::test]
    async fn catch_up_covers_to_the_end_and_resumes_from_coverage() {
        use crate::address_mode::Discovery;
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let (main, trace) = chain(false);
        let d = Discoverer::new(Counted::new(main), Counted::new(trace));

        d.catch_up(&mode(&h, 99_999), 1_000_000).await;
        for action in DISCOVERED {
            assert_eq!(
                h.coverage(action, A).unwrap(),
                vec![(0, 99_999)],
                "{action:?}"
            );
        }
        let req = crate::address_history::PageRequest {
            address: A.into(),
            start_block: Some(0),
            end_block: Some(99_999),
            sort: crate::address_history::Sort::Asc,
            page: 1,
            offset: 10,
            generation: None,
        };
        let crate::address_history::Answer::Rows(rows) = h.page(Action::TxList, &req).unwrap()
        else {
            panic!("covered")
        };
        assert_eq!(rows.len(), 1, "the transaction at block 150");

        let first_pass = filtered_from(&d.trace.inner().asked).len();
        d.catch_up(&mode(&h, 149_999), 1_000_000).await;
        let second: Vec<u64> = filtered_from(&d.trace.inner().asked)[first_pass..].to_vec();
        assert!(!second.is_empty());
        assert!(
            second.iter().all(|b| *b >= 100_000),
            "the second pass refetched below its coverage: {:?}",
            second.iter().min()
        );
        assert_eq!(h.coverage(Action::TxList, A).unwrap(), vec![(0, 149_999)]);
    }

    #[tokio::test]
    async fn a_failed_window_records_nothing() {
        use crate::address_mode::Discovery;
        let dir = tempfile::tempdir().unwrap();
        let h = history(dir.path());
        let (main, _) = chain(false);
        let down = script(|_, _| bail!("connection refused"));
        let d = Discoverer::new(Counted::new(main), Counted::new(down));
        d.catch_up(&mode(&h, 99_999), 1_000_000).await;
        for action in DISCOVERED {
            assert!(h.coverage(action, A).unwrap().is_empty(), "{action:?}");
        }
    }
}
