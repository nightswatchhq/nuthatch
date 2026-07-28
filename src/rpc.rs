//! The thinnest JSON-RPC client that works: `eth_blockNumber` + `eth_getLogs`, with round-robin
//! failover across the configured endpoints. No ExEx yet - that's the sovereignty upgrade later.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// How many times a whole `block_timestamps` batch is retried before it's returned as an error rather
/// than silently yielding an all-zeros timestamp map into the sealed path.
const TIMESTAMP_ATTEMPTS: usize = 4;

/// Max block numbers per `eth_getBlockByNumber` JSON-RPC batch. Many providers cap batch size and
/// **silently drop** an oversized batch (returning nothing), which the strict no-partial-map guard
/// then correctly rejects - so a dense window that needs 1000+ distinct timestamps would fail on such
/// a node. Splitting into bounded sub-batches keeps each request within common limits.
const MAX_TIMESTAMP_BATCH: usize = 200;

/// Merge `preferred` RPC endpoints ahead of a `fallback` list, preserving order and dropping
/// duplicates. Used by `init --rpc` and `dev --rpc` to prefer a user's own node while keeping the
/// built-in / configured endpoints as fallback. An empty `preferred` leaves `fallback` untouched.
pub fn merge_rpcs(preferred: &[String], fallback: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for url in preferred.iter().cloned().chain(fallback) {
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// After an endpoint fails, skip it for this long (unless every endpoint is unhealthy) - so one dead
/// provider doesn't cost a full request-timeout on every call that round-robins onto it. A partial
/// outage fails over fast instead of stalling the tip loop.
const ENDPOINT_COOLDOWN_MS: u64 = 30_000;

pub struct RpcClient {
    http: reqwest::Client,
    urls: Vec<String>,
    cursor: AtomicUsize,
    /// Per-endpoint health: the millis-since-epoch until which the endpoint is considered unhealthy
    /// (`0` = healthy). Set on a failed call, cleared on a successful one. Endpoints past their cooldown
    /// are tried first; still-unhealthy ones are the fallback of last resort (soonest-to-recover first).
    health: Vec<AtomicU64>,
    /// Total HTTP requests attempted (incl. failover retries) - a benchmark/observability metric.
    requests: AtomicU64,
}

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

impl RpcClient {
    pub fn new(urls: Vec<String>) -> Result<Self> {
        if urls.is_empty() {
            bail!("no RPC URLs configured");
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("failed to build HTTP client")?;
        let health = urls.iter().map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            http,
            urls,
            cursor: AtomicUsize::new(0),
            health,
            requests: AtomicU64::new(0),
        })
    }

    /// Total HTTP requests attempted so far (including failover retries).
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// The order to try endpoints for this call: healthy ones first (round-robin from the cursor for
    /// fairness), then any still in cooldown as a last resort (soonest-to-recover first). Advances the
    /// round-robin cursor once per call.
    fn endpoint_order(&self) -> Vec<usize> {
        let n = self.urls.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        let now = now_millis();
        let mut healthy = Vec::with_capacity(n);
        let mut cooling = Vec::with_capacity(n);
        for i in 0..n {
            let j = (start + i) % n;
            let until = self.health[j].load(Ordering::Relaxed);
            if until <= now {
                healthy.push(j);
            } else {
                cooling.push((until, j));
            }
        }
        cooling.sort_by_key(|(until, _)| *until);
        healthy
            .into_iter()
            .chain(cooling.into_iter().map(|(_, j)| j))
            .collect()
    }

    fn mark_healthy(&self, j: usize) {
        self.health[j].store(0, Ordering::Relaxed);
    }

    fn mark_unhealthy(&self, j: usize) {
        self.health[j].store(now_millis() + ENDPOINT_COOLDOWN_MS, Ordering::Relaxed);
    }

    /// Try endpoints in health order until one answers; a failed endpoint is put into cooldown, a
    /// successful one is cleared.
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut last_err = anyhow!("all RPC endpoints failed");
        for j in self.endpoint_order() {
            let url = &self.urls[j];
            self.requests.fetch_add(1, Ordering::Relaxed);
            crate::metrics::METRICS.inc_rpc();
            match self.call_one(url, method, &params).await {
                Ok(v) => {
                    self.mark_healthy(j);
                    return Ok(v);
                }
                Err(e) => {
                    self.mark_unhealthy(j);
                    tracing::debug!("rpc {} failed for {method}: {e:#}", redact_url(url));
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// POST a raw JSON-RPC body (single object or a batch array) with the same health-ordered failover
    /// as `call`, returning the parsed response. Used for batch requests `call` can't express.
    async fn post_with_failover(&self, body: &Value) -> Result<Value> {
        let mut last_err = anyhow!("all RPC endpoints failed");
        for j in self.endpoint_order() {
            let url = &self.urls[j];
            self.requests.fetch_add(1, Ordering::Relaxed);
            crate::metrics::METRICS.inc_rpc();
            match self.post_one(url, body).await {
                Ok(v) => {
                    self.mark_healthy(j);
                    return Ok(v);
                }
                Err(e) => {
                    self.mark_unhealthy(j);
                    tracing::debug!("rpc {} failed for batch: {e:#}", redact_url(url));
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn post_one(&self, url: &str, body: &Value) -> Result<Value> {
        let resp: Value = self
            .http
            .post(url)
            .json(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        // A whole-batch rejection - e.g. a keyless endpoint answering HTTP 200 with
        // `{"error":{"message":"authenticate with an API key"}}` instead of the expected array - comes
        // back as a single object with a top-level `error`. Treat it as an endpoint failure so
        // `post_with_failover` cools it down and tries the next, exactly as `call_one` does for single
        // calls; otherwise the bad endpoint silently poisons the pool and the batch aborts with a
        // confusing non-error. (Per-item errors inside a normal array response stay the caller's to
        // handle.)
        if let Some(err) = resp.get("error") {
            bail!("rpc error (endpoint rejected the batch): {err}");
        }
        Ok(resp)
    }

    async fn call_one(&self, url: &str, method: &str, params: &Value) -> Result<Value> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: Value = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if let Some(err) = resp.get("error") {
            bail!("rpc error: {err}");
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("rpc response had no result"))
    }

    pub async fn block_number(&self) -> Result<u64> {
        let result = self.call("eth_blockNumber", json!([])).await?;
        parse_hex_u64(result.as_str().unwrap_or_default())
    }

    /// Check **every** endpoint reports `expected` from `eth_chainId`, once, at startup (issue #150).
    ///
    /// A wrong-network endpoint in the pool is uniquely nasty: failover makes it *look* like a
    /// redundancy win while it quietly answers `eth_getBlockByNumber` for a chain we are not indexing.
    /// Every block hash it returns then mismatches our checkpoints, so `detect_reorg` walks the entire
    /// checkpoint history looking for a common ancestor it can never find. For an established nest the
    /// sealed-watermark bail contains the damage; a *fresh* nest, with nothing sealed, would happily
    /// roll itself back towards genesis.
    ///
    /// The per-endpoint loop is the point - `call` would failover past the bad one and report success.
    ///
    /// A **mismatch is fatal**: it is a configuration error that silently corrupts, and the operator
    /// must fix it. An endpoint that is merely *unreachable* is not - it is warned about and left in
    /// the pool, because being offline at startup is a normal condition this indexer tolerates and the
    /// existing health/cooldown machinery already handles it.
    pub async fn verify_chain_ids(&self, expected: u64) -> Result<()> {
        // Checked CONCURRENTLY, with a short deadline of its own. This runs before the first block is
        // fetched, so it sits directly on time-to-first-index: done sequentially at the client's 20 s
        // timeout, a default pool with a couple of dead endpoints (mainnet ships four) delayed the start
        // of indexing by over a minute - a regression against the "<2 minutes to first indexed query"
        // promise, and one that only shows up when a public endpoint is having a bad day. Concurrent +
        // 5 s bounds the whole check at ~5 s no matter how many endpoints are configured or dead.
        const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let checks = self.urls.iter().enumerate().map(|(j, url)| async move {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let r = tokio::time::timeout(
                VERIFY_TIMEOUT,
                self.call_one(url, "eth_chainId", &json!([])),
            )
            .await;
            (j, url, r)
        });
        for (j, url, outcome) in futures::future::join_all(checks).await {
            match outcome {
                Ok(Ok(v)) => {
                    let got = parse_hex_u64(v.as_str().unwrap_or_default()).with_context(|| {
                        format!("unparseable eth_chainId from {}", redact_url(url))
                    })?;
                    if got != expected {
                        bail!(
                            "RPC endpoint {} is on chain {got}, but this nest indexes chain {expected} \
                             - indexing against a mixed-chain endpoint pool silently corrupts state \
                             (every block hash mismatches, and a fresh nest would roll back towards \
                             genesis). Fix `rpc_urls`.",
                            redact_url(url)
                        );
                    }
                    self.mark_healthy(j);
                }
                // Unreachable or slow now ≠ wrong chain. Leave it in the pool; failover copes, and a
                // wrong-chain endpoint that was merely late still gets caught the moment it answers a
                // real call with a mismatching block hash.
                Ok(Err(e)) => tracing::warn!(
                    "could not verify chain id of {} at startup ({e:#}) - leaving it in the pool",
                    redact_url(url)
                ),
                Err(_) => tracing::warn!(
                    "chain id check for {} timed out after {}s - leaving it in the pool",
                    redact_url(url),
                    VERIFY_TIMEOUT.as_secs()
                ),
            }
        }
        Ok(())
    }

    /// A storage slot's value at `address` (latest block) - used to read the EIP-1967 proxy slot.
    pub async fn get_storage_at(&self, address: &str, slot: &str) -> Result<String> {
        let result = self
            .call("eth_getStorageAt", json!([address, slot, "latest"]))
            .await?;
        Ok(result.as_str().unwrap_or("0x0").to_string())
    }

    /// A read-only `eth_call` at latest block: send `data` (a selector + args) to `to`, returning the
    /// raw hex result. Used at init to ask a beacon proxy's beacon for its `implementation()`; never on
    /// the ingest path.
    pub async fn eth_call(&self, to: &str, data: &str) -> Result<String> {
        let result = self
            .call("eth_call", json!([{ "to": to, "data": data }, "latest"]))
            .await?;
        Ok(result.as_str().unwrap_or("0x").to_string())
    }

    /// Contract bytecode at `address` as of `block`. `"0x"` (empty) means not yet deployed.
    pub async fn get_code(&self, address: &str, block: u64) -> Result<String> {
        let result = self
            .call("eth_getCode", json!([address, format!("0x{block:x}")]))
            .await?;
        Ok(result.as_str().unwrap_or("0x").to_string())
    }

    /// Unix timestamps (seconds) for the given block numbers, fetched in a single JSON-RPC batch so
    /// even a dense window costs one round-trip.
    ///
    /// Two different "missing" cases, deliberately kept distinct because timestamps feed the sealed
    /// (immutable) path: a block the endpoint *answered but omitted* is simply absent from the returned
    /// map (best-effort; the caller stores 0 for it), but a *whole-batch request failure* is retried a
    /// few times and then returned as `Err` - never silently collapsed into an all-zeros map, which
    /// would bake `block_timestamp = 0` into a permanent segment from a transient blip.
    pub async fn block_timestamps(&self, blocks: &[u64]) -> Result<HashMap<u64, u64>> {
        if blocks.is_empty() {
            return Ok(HashMap::new());
        }
        // Fetch in bounded sub-batches (see `MAX_TIMESTAMP_BATCH`) and merge, so a dense window whose
        // distinct-block count exceeds a provider's batch cap doesn't fail wholesale.
        let mut out = HashMap::new();
        for chunk in blocks.chunks(MAX_TIMESTAMP_BATCH) {
            out.extend(self.fetch_timestamp_batch(chunk).await?);
        }
        // COR-3: a *partial* response (endpoint answered but a load-balanced/archive-vs-full split
        // returned `null` for some block) must be an error, not a partial map - else the caller defaults
        // the missing block's `block_timestamp` to 0 and *seals it permanently*, breaking determinism
        // (a re-run against a healthy endpoint yields a different timestamp → different content hash).
        // Erroring makes the seal path retry the whole window, exactly like a total failure.
        if out.len() != blocks.len() {
            let missing = blocks.iter().filter(|b| !out.contains_key(b)).count();
            bail!(
                "block_timestamps: {missing}/{} block(s) missing from the RPC response - refusing a \
                 partial map (would seal block_timestamp=0)",
                blocks.len()
            );
        }
        Ok(out)
    }

    /// One bounded `eth_getBlockByNumber` batch → `{block: timestamp}` (may be partial if the endpoint
    /// omitted blocks; the caller's total-count check turns that into an error). A whole-batch request
    /// failure is retried a few times before erroring.
    async fn fetch_timestamp_batch(&self, blocks: &[u64]) -> Result<HashMap<u64, u64>> {
        let batch: Vec<Value> = blocks
            .iter()
            .enumerate()
            .map(|(i, b)| {
                json!({ "jsonrpc": "2.0", "id": i, "method": "eth_getBlockByNumber",
                        "params": [format!("0x{b:x}"), false] })
            })
            .collect();
        let body = Value::Array(batch);
        let mut resp = None;
        let mut last_err = None;
        for attempt in 0..TIMESTAMP_ATTEMPTS {
            match self.post_with_failover(&body).await {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(e) => {
                    tracing::debug!("block_timestamps attempt {} failed: {e:#}", attempt + 1);
                    last_err = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(
                        200 * (attempt as u64 + 1),
                    ))
                    .await;
                }
            }
        }
        let resp = match resp {
            Some(r) => r,
            None => {
                return Err(last_err
                    .unwrap()
                    .context("block_timestamps batch failed after retries"))
            }
        };
        let mut out = HashMap::new();
        for item in resp.as_array().into_iter().flatten() {
            let Some(idx) = item.get("id").and_then(Value::as_u64) else {
                continue;
            };
            let Some(&block) = blocks.get(idx as usize) else {
                continue;
            };
            if let Some(ts) = item
                .pointer("/result/timestamp")
                .and_then(Value::as_str)
                .and_then(|s| parse_hex_u64(s).ok())
            {
                out.insert(block, ts);
            }
        }
        Ok(out)
    }

    /// The node's `finalized` block number (L1-aware on an L2 like Arbitrum), or None if the
    /// endpoint doesn't serve the `finalized` tag. Used by the `FinalizedTag` finality policy.
    pub async fn finalized_block(&self) -> Result<Option<u64>> {
        let result = self
            .call("eth_getBlockByNumber", json!(["finalized", false]))
            .await?;
        Ok(result
            .get("number")
            .and_then(Value::as_str)
            .and_then(|s| parse_hex_u64(s).ok()))
    }

    /// Canonical block hash for a height, or None if the node doesn't have that block.
    pub async fn block_hash(&self, number: u64) -> Result<Option<String>> {
        let result = self
            .call(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        Ok(result.get("hash").and_then(Value::as_str).map(String::from))
    }

    /// One combined `eth_getLogs` across all `addresses`, matching any of `topic0s`.
    pub async fn get_logs(
        &self,
        addresses: &[String],
        topic0s: &[String],
        from: u64,
        to: u64,
    ) -> Result<Vec<Log>> {
        let mut filter = serde_json::Map::new();
        // An empty address list means "no address filter" (topic0-only) - the factory tip regime
        // (RFC-0009 §3) fetches this way so a child created and active in the same block is already in
        // hand. Sending `"address": []` would instead match nothing, so omit the field when empty.
        if !addresses.is_empty() {
            filter.insert("address".into(), json!(addresses));
        }
        if !topic0s.is_empty() {
            filter.insert("topics".into(), json!([topic0s]));
        }
        filter.insert("fromBlock".into(), json!(format!("0x{from:x}")));
        filter.insert("toBlock".into(), json!(format!("0x{to:x}")));
        let result = self
            .call("eth_getLogs", json!([Value::Object(filter)]))
            .await?;
        let arr = result
            .as_array()
            .ok_or_else(|| anyhow!("eth_getLogs did not return an array"))?;
        arr.iter().map(parse_log).collect()
    }
}

fn parse_log(v: &Value) -> Result<Log> {
    let topics = v
        .get("topics")
        .and_then(Value::as_array)
        .map(|t| {
            t.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(Log {
        address: field_str(v, "address")?,
        topics,
        data: field_str(v, "data").unwrap_or_default(),
        block_number: parse_hex_u64(&field_str(v, "blockNumber")?)?,
        block_hash: field_str(v, "blockHash").unwrap_or_default(),
        tx_hash: field_str(v, "transactionHash")?,
        log_index: parse_hex_u64(&field_str(v, "logIndex")?)?,
    })
}

fn field_str(v: &Value, key: &str) -> Result<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("log missing field '{key}'"))
}

fn parse_hex_u64(s: &str) -> Result<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).with_context(|| format!("bad hex number '{s}'"))
}

/// Wall-clock millis since the epoch - used only for endpoint-health cooldowns (a coarse "try again
/// after" timer), never for anything in the deterministic data path.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Reduce an RPC URL to `scheme://host[:port]` for logging - provider endpoints routinely carry the API
/// key in the path (`.../v3/<KEY>`) or query string, and the failure log fires on exactly the outages an
/// operator debugs with `RUST_LOG=debug`. Log *where* it failed, never the key. Returns a slice of the
/// original (the `scheme://host` prefix), so it is zero-alloc.
fn redact_url(url: &str) -> &str {
    match url.split_once("://") {
        // Truncate at the first '/' or '?' after the scheme, i.e. keep scheme://host[:port] only.
        Some((scheme, rest)) => {
            let host_len = rest.find(['/', '?']).unwrap_or(rest.len());
            &url[..scheme.len() + 3 + host_len]
        }
        None => url.split(['/', '?']).next().unwrap_or(url),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// A one-endpoint fake JSON-RPC server on a loopback port. Returns `(url, handle)`; the caller
    /// aborts the handle when done. Real HTTP, so `RpcClient`'s actual request path is exercised -
    /// there is no way to fake a per-endpoint bug like a mixed-chain pool without it.
    async fn fake_rpc(chain_id: u64) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handler(State(chain_id): State<u64>, Json(req): Json<Value>) -> Json<Value> {
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let result = match method {
                "eth_chainId" => json!(format!("0x{chain_id:x}")),
                "eth_blockNumber" => json!(HEALTHY_TIP_HEX),
                _ => json!(null),
            };
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": result}))
        }

        // Answer on ANY path, not just `/` - provider URLs carry the API key in the path
        // (`.../v3/<KEY>`), and a mock that 404s those would read as "endpoint down" and quietly
        // skip the very check under test.
        let app = Router::new()
            .route("/", post(handler))
            .route("/{*rest}", post(handler))
            .with_state(chain_id);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle)
    }

    /// The block height the healthy mock reports, so a failover test can prove *which* endpoint
    /// answered rather than merely that something did.
    const HEALTHY_TIP_HEX: &str = "0x1234";
    const HEALTHY_TIP: u64 = 0x1234;

    /// An endpoint that is up but broken: HTTP 500 on everything. Distinct from an unbound port, so
    /// the test covers a *responding* bad endpoint rather than a refused connection.
    async fn broken_rpc() -> (String, tokio::task::JoinHandle<()>, Arc<AtomicU64>) {
        use axum::{extract::State, http::StatusCode, routing::post, Router};
        let hits = Arc::new(AtomicU64::new(0));
        async fn handler(State(hits): State<Arc<AtomicU64>>) -> StatusCode {
            hits.fetch_add(1, Ordering::Relaxed);
            StatusCode::INTERNAL_SERVER_ERROR
        }
        let app = Router::new()
            .route("/", post(handler))
            .route("/{*rest}", post(handler))
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle, hits)
    }

    /// Issue #150: the failover path itself, not just the ordering maths. The first endpoint is broken,
    /// so the call must still succeed via the second, and the dead one must be marked unhealthy so
    /// subsequent calls stop paying its timeout. Previously only `endpoint_order`'s sorting was tested -
    /// which would happily pass even if `call` never retried at all.
    #[tokio::test]
    async fn a_failed_call_recovers_via_the_next_endpoint_and_cools_the_dead_one() {
        let (broken, hb, broken_hits) = broken_rpc().await;
        let (good, hg) = fake_rpc(1).await;
        let c = RpcClient::new(vec![broken, good]).unwrap();

        // The cursor starts at 0, so the broken endpoint is tried FIRST - the case under test.
        let got = c
            .block_number()
            .await
            .expect("the call must survive one dead endpoint");
        assert_eq!(
            got, HEALTHY_TIP,
            "the answer must come from the healthy endpoint"
        );
        assert!(
            broken_hits.load(Ordering::Relaxed) >= 1,
            "the broken endpoint should actually have been tried"
        );
        assert_eq!(c.request_count(), 2, "one failed attempt, then one success");

        // The dead endpoint is in cooldown, so it now sorts last…
        assert_eq!(
            *c.endpoint_order().last().unwrap(),
            0,
            "the failed endpoint must sink to the back"
        );
        // …and the healthy one is not penalised.
        assert_eq!(c.health[1].load(Ordering::Relaxed), 0);

        // A second call still succeeds, and skips straight to the good endpoint.
        let before = c.request_count();
        assert_eq!(c.block_number().await.unwrap(), HEALTHY_TIP);
        assert_eq!(
            c.request_count() - before,
            1,
            "a cooled-down endpoint must not be retried on every call"
        );

        hb.abort();
        hg.abort();
    }

    /// With every endpoint broken there is nothing to fail over TO, so the call must surface an error
    /// rather than hang or quietly return a default - the tip loop's stall detection depends on it.
    #[tokio::test]
    async fn a_call_fails_when_no_endpoint_can_answer() {
        let (b1, h1, _) = broken_rpc().await;
        let (b2, h2, _) = broken_rpc().await;
        let c = RpcClient::new(vec![b1, b2]).unwrap();
        assert!(c.block_number().await.is_err());
        assert_eq!(
            c.request_count(),
            2,
            "every endpoint tried before giving up"
        );
        h1.abort();
        h2.abort();
    }

    /// Issue #150: every endpoint is checked individually. `call`-based verification would be useless
    /// here - it fails over past the bad endpoint and reports success, which is exactly how a
    /// mixed-chain pool hides.
    #[tokio::test]
    async fn a_wrong_chain_endpoint_is_rejected_even_when_its_neighbours_are_right() {
        let (good1, h1) = fake_rpc(42161).await;
        let (good2, h2) = fake_rpc(42161).await;
        let (wrong, h3) = fake_rpc(8453).await;

        // All correct → starts.
        let ok = RpcClient::new(vec![good1.clone(), good2.clone()]).unwrap();
        assert!(ok.verify_chain_ids(42161).await.is_ok());

        // One wrong endpoint among healthy ones → refuse, naming the chain it is actually on.
        let mixed = RpcClient::new(vec![good1, good2.clone(), wrong.clone()]).unwrap();
        let err = mixed.verify_chain_ids(42161).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("8453"), "should name the wrong chain: {msg}");
        assert!(msg.contains("42161"), "and the expected one: {msg}");

        // Order must not matter - the bad endpoint first is just as fatal.
        let mixed2 = RpcClient::new(vec![wrong, good2]).unwrap();
        assert!(mixed2.verify_chain_ids(42161).await.is_err());

        for h in [h1, h2, h3] {
            h.abort();
        }
    }

    /// Startup must not be held hostage by a dead endpoint. `verify_chain_ids` runs before the first
    /// block is fetched, so its cost lands on time-to-first-index; done sequentially at the client's
    /// 20 s timeout, a default pool with several unreachable endpoints delayed indexing by over a
    /// minute (measured - it is what made the CI footprint job start failing).
    ///
    /// Four black-holed endpoints alongside one good one must still complete in a few seconds.
    #[tokio::test]
    async fn verification_is_bounded_even_when_most_endpoints_hang() {
        let (good, h) = fake_rpc(1).await;
        // 203.0.113.0/24 is TEST-NET-3 (RFC 5737): reserved for documentation, routed nowhere, so
        // connections hang rather than being refused - the case a per-endpoint timeout exists for.
        let mut urls: Vec<String> = (1..=4)
            .map(|i| format!("http://203.0.113.{i}:8545/"))
            .collect();
        urls.push(good);
        let c = RpcClient::new(urls).unwrap();

        let started = std::time::Instant::now();
        let r =
            tokio::time::timeout(std::time::Duration::from_secs(20), c.verify_chain_ids(1)).await;
        let elapsed = started.elapsed();

        assert!(
            r.is_ok(),
            "verification must not hang past its own deadline"
        );
        assert!(
            r.unwrap().is_ok(),
            "unreachable endpoints must not fail startup"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(12),
            "four hanging endpoints took {elapsed:?} - the checks are not concurrent/bounded, and \
             that time is paid before a single block is indexed"
        );
        h.abort();
    }

    /// Offline is not the same as wrong. Nuthatch tolerates an endpoint being down at startup (the
    /// health/cooldown machinery handles it), so an unreachable URL must warn, not block the boot.
    #[tokio::test]
    async fn an_unreachable_endpoint_does_not_block_startup() {
        let (good, h) = fake_rpc(1).await;
        // Port 1 on loopback: nothing listens, connection refused immediately.
        let c = RpcClient::new(vec![good, "http://127.0.0.1:1/".to_string()]).unwrap();
        assert!(
            c.verify_chain_ids(1).await.is_ok(),
            "an endpoint that is merely down must not prevent indexing"
        );
        h.abort();
    }

    /// The error text reaches operator logs, and provider URLs routinely carry the API key in the
    /// path. It must name the host and nothing more.
    #[tokio::test]
    async fn the_mismatch_error_redacts_the_api_key() {
        let (wrong, h) = fake_rpc(8453).await;
        let with_key = format!(
            "{}v3/SUPERSECRETKEY",
            wrong.trim_end_matches('/').to_string() + "/"
        );
        let c = RpcClient::new(vec![with_key]).unwrap();
        let msg = format!("{:#}", c.verify_chain_ids(1).await.unwrap_err());
        assert!(!msg.contains("SUPERSECRETKEY"), "leaked the API key: {msg}");
        h.abort();
    }

    use super::{merge_rpcs, redact_url, RpcClient};

    fn v<const N: usize>(xs: [&str; N]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_failed_endpoint_is_tried_last_until_it_cools_down() {
        let c = RpcClient::new(v(["http://a", "http://b", "http://c"])).unwrap();
        // Endpoint 1 (b) just failed → it must sink to the back of the try order.
        c.mark_unhealthy(1);
        for _ in 0..5 {
            let order = c.endpoint_order();
            assert_eq!(order.len(), 3);
            assert_eq!(
                *order.last().unwrap(),
                1,
                "unhealthy endpoint is tried last"
            );
            // The two healthy endpoints lead, in some round-robin order.
            assert!(order[..2].contains(&0) && order[..2].contains(&2));
        }
        // A success clears it - back into normal rotation, no longer forced last.
        c.mark_healthy(1);
        let mut seen_first = false;
        for _ in 0..3 {
            if c.endpoint_order()[0] == 1 {
                seen_first = true;
            }
        }
        assert!(seen_first, "a recovered endpoint rejoins the round-robin");
    }

    #[test]
    fn empty_preferred_leaves_fallback_untouched() {
        assert_eq!(merge_rpcs(&[], v(["a", "b"])), v(["a", "b"]));
    }

    #[test]
    fn preferred_go_first_then_fallback() {
        assert_eq!(
            merge_rpcs(&v(["mine"]), v(["a", "b"])),
            v(["mine", "a", "b"])
        );
    }

    #[test]
    fn duplicates_are_dropped_keeping_first_position() {
        // A preferred URL already present in the fallback should surface once, at the front.
        assert_eq!(merge_rpcs(&v(["a"]), v(["a", "b"])), v(["a", "b"]));
        // Repeated preferred entries collapse too.
        assert_eq!(merge_rpcs(&v(["m", "m", "n"]), v(["n"])), v(["m", "n"]));
    }

    #[test]
    fn redact_url_keeps_only_scheme_and_host() {
        // The API key in the path or query must never survive into a log line.
        assert_eq!(
            redact_url("https://mainnet.infura.io/v3/SECRETKEY"),
            "https://mainnet.infura.io"
        );
        assert_eq!(
            redact_url("https://eth.g.alchemy.com/v2/KEY?token=x"),
            "https://eth.g.alchemy.com"
        );
        assert_eq!(redact_url("http://localhost:8545"), "http://localhost:8545");
        assert_eq!(redact_url("https://host:8545/"), "https://host:8545");
    }
}
