//! The rotki-mode runtime (RFC-0063): a nest with `[address_history]` and no contracts.
//!
//! It shares nothing with the event path. No decode registry, no DBSP circuit, no Burrmill, no
//! sealing: one redb handle, one cursor that reads the chain head every `poll_interval`, and a small
//! router serving `/api`, `/health`, `/ready` and `/metrics`. After each head read the cursor
//! discovers every watched address up to the finalized head ([`Discovery`]).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::address_discovery::{Counted, Discoverer, Rpc};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::address_history::{AddressHistory, AddressHistoryConfig};
use crate::config::Config;
use crate::rpc::RpcClient;

/// Where the cursor reads the chain head. A trait so tests drive the loop without a network.
pub trait HeadSource: Send + Sync + 'static {
    fn head(&self) -> impl std::future::Future<Output = Result<u64>> + Send;
    /// Every request this source has sent so far, failovers and startup checks included.
    fn requests(&self) -> u64;
}

impl HeadSource for RpcClient {
    async fn head(&self) -> Result<u64> {
        self.block_number().await
    }

    fn requests(&self) -> u64 {
        self.request_count()
    }
}

pub struct ModeState {
    pub history: AddressHistory,
    pub chain_id: u64,
    pub poll_interval: Duration,
    /// Whether the bound socket is a loopback address. Watch and unwatch change what the nest asks its
    /// RPC for, so they are refused on any other bind.
    pub loopback: bool,
    started: Instant,
    /// Milliseconds after `started` of the last successful poll, plus one; 0 for none yet. Monotonic,
    /// so a wall-clock step cannot hide a stall.
    last_poll: AtomicU64,
    polls: AtomicU64,
    poll_errors: AtomicU64,
    rpc_requests: AtomicU64,
    /// Blocks behind the chain head the nest serves through, so nothing it records can be reorged.
    pub depth: u64,
    /// `[address_history]`'s `start_block` and `end_block`: the history each address backfills.
    pub start_block: u64,
    pub end_block: Option<u64>,
    /// Rung by watch, so a new address starts backfilling now rather than at the next poll.
    pub wake: tokio::sync::Notify,
    /// Why the cursor stopped for good, e.g. an RPC on the wrong chain.
    cursor_error: Mutex<Option<String>>,
    /// RPC calls by endpoint and method.
    calls: Mutex<BTreeMap<(String, String), u64>>,
    windows: AtomicU64,
    discovery_errors: AtomicU64,
    /// Traces one transaction into the store, for a `txhash` lookup the store cannot yet answer.
    pub tracer: Option<Tracer>,
    /// Forwards `proxy` and `logs` requests to the nest's RPC.
    pub forward: Option<crate::address_proxy::Forward>,
    /// Verified block partitions, and the blocks `[address_history.mirror]` takes from them.
    pub mirror: Option<(Arc<crate::address_partitions::Source>, (u64, u64))>,
}

/// See [`ModeState::tracer`].
pub type Tracer =
    Arc<dyn Fn(String) -> futures::future::BoxFuture<'static, Result<()>> + Send + Sync>;

impl ModeState {
    pub fn new(
        history: AddressHistory,
        chain_id: u64,
        poll_interval: Duration,
        loopback: bool,
    ) -> ModeState {
        ModeState {
            history,
            chain_id,
            poll_interval,
            loopback,
            started: Instant::now(),
            last_poll: AtomicU64::new(0),
            polls: AtomicU64::new(0),
            poll_errors: AtomicU64::new(0),
            rpc_requests: AtomicU64::new(0),
            depth: 0,
            start_block: 0,
            end_block: None,
            wake: tokio::sync::Notify::new(),
            cursor_error: Mutex::new(None),
            calls: Mutex::new(BTreeMap::new()),
            windows: AtomicU64::new(0),
            discovery_errors: AtomicU64::new(0),
            tracer: None,
            forward: None,
            mirror: None,
        }
    }

    pub fn with_forward(mut self, forward: crate::address_proxy::Forward) -> Self {
        self.forward = Some(forward);
        self
    }

    pub fn with_tracer(mut self, tracer: Tracer) -> Self {
        self.tracer = Some(tracer);
        self
    }

    pub fn with_mirror(
        mut self,
        source: Arc<crate::address_partitions::Source>,
        blocks: (u64, u64),
    ) -> Self {
        self.mirror = Some((source, blocks));
        self
    }

    pub fn with_range(mut self, start_block: u64, end_block: Option<u64>, depth: u64) -> Self {
        self.start_block = start_block;
        self.end_block = end_block;
        self.depth = depth;
        self
    }

    fn stop(&self, why: String) {
        tracing::error!("address history: cursor stopped: {why}");
        *self.cursor_error.lock().expect("cursor_error lock") = Some(why);
    }

    fn set_calls(&self, endpoint: &str, calls: BTreeMap<String, u64>) {
        let mut all = self.calls.lock().expect("calls lock");
        for (method, n) in calls {
            all.insert((endpoint.to_string(), method), n);
        }
    }

    fn stalled(&self) -> bool {
        let since_start = self.started.elapsed();
        let last = self.last_poll.load(Ordering::Relaxed);
        let since_poll = if last == 0 {
            since_start
        } else {
            since_start.saturating_sub(Duration::from_millis(last - 1))
        };
        since_poll > 3 * self.poll_interval
    }
}

impl<R: Rpc> HeadSource for Counted<R> {
    async fn head(&self) -> Result<u64> {
        let n = self.call("eth_blockNumber", json!([])).await?;
        let s = n
            .as_str()
            .context("eth_blockNumber did not answer a string")?;
        u64::from_str_radix(s.trim_start_matches("0x"), 16).context("eth_blockNumber")
    }

    fn requests(&self) -> u64 {
        self.calls().values().sum()
    }
}

/// What the cursor does after each head read. `()` does nothing, for a cursor without discovery.
pub trait Discovery: Send + Sync + 'static {
    fn catch_up(
        &self,
        state: &ModeState,
        safe: u64,
    ) -> impl std::future::Future<Output = ()> + Send;
}

impl Discovery for () {
    async fn catch_up(&self, _: &ModeState, _: u64) {}
}

impl<D: Discovery> Discovery for Arc<D> {
    async fn catch_up(&self, state: &ModeState, safe: u64) {
        (**self).catch_up(state, safe).await
    }
}

impl<H: HeadSource> HeadSource for Arc<H> {
    async fn head(&self) -> Result<u64> {
        (**self).head().await
    }

    fn requests(&self) -> u64 {
        (**self).requests()
    }
}

/// The head as the discoverer's main endpoint reads it, so its calls are counted with the rest.
pub struct MainHead<M, T>(pub Arc<Discoverer<Counted<M>, Counted<T>>>);

impl<M: Rpc, T: Rpc> HeadSource for MainHead<M, T> {
    async fn head(&self) -> Result<u64> {
        self.0.main.head().await
    }

    fn requests(&self) -> u64 {
        self.0.main.requests() + self.0.trace.requests()
    }
}

impl<M: Rpc, T: Rpc> Discovery for Discoverer<Counted<M>, Counted<T>> {
    /// Backfill every watched address window by window up to `safe` (or `end_block`). A failed window
    /// records nothing and ends this pass for that address; the next poll resumes from coverage.
    async fn catch_up(&self, state: &ModeState, safe: u64) {
        let to = state.end_block.map_or(safe, |e| e.min(safe));
        let watched = match state.history.watched() {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("address history: reading the watched set: {e:#}");
                return;
            }
        };
        for address in watched.clone() {
            let mut recorded_through: Option<u64> = None;
            loop {
                let next = match crate::address_discovery::next_uncovered(
                    &state.history,
                    &address,
                    state.start_block,
                ) {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("address history: coverage for {address}: {e:#}");
                        break;
                    }
                };
                if next > to {
                    break;
                }
                // A recorded window must move coverage past it; one that did not would be fetched
                // again forever, at the provider's expense.
                if recorded_through.is_some_and(|r| next <= r) {
                    state.discovery_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        "address history: coverage for {address} did not advance past {next}; stopping this pass"
                    );
                    break;
                }
                let end = next
                    .saturating_add(crate::address_discovery::OUTER_WINDOW - 1)
                    .min(to);
                let outcome = async {
                    let generation = state.history.generation()?;
                    let pending = crate::address_discovery::Pending::default();
                    let found = self
                        .discover(&state.history, &address, next, end, &pending)
                        .await;
                    // What was fetched is kept even when the window fails, so its retry refetches
                    // only what it still lacks.
                    let history = state.history.clone();
                    let who = address.clone();
                    tokio::task::spawn_blocking(move || {
                        pending.persist(&history)?;
                        crate::address_discovery::record(
                            &history,
                            &who,
                            (next, end),
                            found?,
                            generation,
                        )
                    })
                    .await?
                }
                .await;
                state.set_calls("main", self.main.calls());
                state.set_calls("trace", self.trace.calls());
                match outcome {
                    Ok(()) => {
                        recorded_through = Some(end);
                        state.windows.fetch_add(1, Ordering::Relaxed);
                        tracing::info!("address history: {address} covered through {end}");
                    }
                    Err(e) => {
                        state.discovery_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            "address history: [{next}, {end}] for {address} not recorded, retrying next poll: {e:#}"
                        );
                        break;
                    }
                }
            }
        }
        // Withdrawals and produced blocks come from bodies, and the definitions they are matched to
        // are Ethereum mainnet's, so only there is the block scan run.
        if state.chain_id == 1 {
            let skip = state.mirror.as_ref().map(|(_, blocks)| *blocks);
            if let Some((source, blocks)) = &state.mirror {
                mirror_pass(self, state, &watched, to, source, *blocks).await;
            }
            scan_bodies(self, state, &watched, to, skip).await;
        }
    }
}

/// The partition pass: each partition in the mirror's blocks that some watched address, or the
/// verified headers, still lack is downloaded, verified against the RPC and read for all of them at
/// once. A partition that is missing or fails verification records nothing, and its blocks stay
/// incomplete: the RPC body scan does not stand in for it.
async fn mirror_pass<M: Rpc, T: Rpc>(
    d: &Discoverer<Counted<M>, Counted<T>>,
    state: &ModeState,
    watched: &[String],
    to: u64,
    source: &crate::address_partitions::Source,
    (first, last): (u64, u64),
) {
    use crate::address_discovery::{record_blocks, BLOCK_SCANNED};
    use crate::address_partitions::{finalized, ingest, span_of, SPAN};
    // Coverage the RPC scan recorded before the mirror was configured does not count here.
    let history = state.history.clone();
    let kept = tokio::task::spawn_blocking(move || {
        history.keep_verified_only(&BLOCK_SCANNED, (first, last))
    })
    .await;
    if let Err(e) = kept.map_err(anyhow::Error::from).and_then(|r| r) {
        tracing::warn!("address history: setting the mirror's blocks aside: {e:#}");
        return;
    }
    let (lo, hi) = (state.start_block.max(first), to.min(last));
    if lo > hi {
        return;
    }
    let mut ceiling: Option<u64> = None;
    let mut start = span_of(lo).0;
    while start <= hi {
        let end = start + SPAN - 1;
        let clip = (start.max(lo), end.min(hi));
        let verified = |a: &String| {
            BLOCK_SCANNED.iter().all(|&action| {
                state
                    .history
                    .verified_coverage(action, a)
                    .is_ok_and(|spans| spans.iter().any(|(f, t)| *f <= clip.0 && clip.1 <= *t))
            })
        };
        let lacking: Vec<String> = watched.iter().filter(|a| !verified(a)).cloned().collect();
        let headers = state
            .history
            .header_coverage()
            .is_ok_and(|spans| spans.iter().any(|(f, t)| *f <= clip.0 && clip.1 <= *t));
        if lacking.is_empty() && headers {
            start += SPAN;
            continue;
        }
        // A partition is anchored at its last block, which the RPC must call finalized: the
        // nest's serving depth is a guess at finality, not finality.
        if ceiling.is_none() {
            match finalized(&d.main).await {
                Ok(f) => ceiling = Some(f),
                Err(e) => {
                    state.discovery_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!("address history: the RPC's finalized block: {e:#}");
                    return;
                }
            }
        }
        if ceiling.is_some_and(|f| end > f) {
            return;
        }
        let outcome = async {
            let generation = state.history.generation()?;
            let mut got = ingest(&d.main, source, start, &lacking, clip).await?;
            let history = state.history.clone();
            tokio::task::spawn_blocking(move || {
                history.record_headers(clip, &got.timestamps)?;
                for a in lacking {
                    let f = got
                        .found
                        .remove(&a.to_ascii_lowercase())
                        .unwrap_or_default();
                    record_blocks(&history, &a, clip, f, generation, true)?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await?
        }
        .await;
        state.set_calls("main", d.main.calls());
        match outcome {
            Ok(()) => {
                state.windows.fetch_add(1, Ordering::Relaxed);
                tracing::info!("address history: partition [{start}, {end}] verified and read");
            }
            Err(e) => {
                state.discovery_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "address history: partition [{start}, {end}] not used, its blocks stay incomplete: {e:#}"
                );
            }
        }
        start += SPAN;
    }
}

/// The block-body pass: each window is read once for every watched address that still lacks it,
/// starting from the least covered, so an address watched later backfills without a second scan
/// of what the others already have.
/// Blocks in `skip` are the mirror's, and are never read here.
async fn scan_bodies<M: Rpc, T: Rpc>(
    d: &Discoverer<Counted<M>, Counted<T>>,
    state: &ModeState,
    watched: &[String],
    to: u64,
    skip: Option<(u64, u64)>,
) {
    use crate::address_discovery::{
        next_uncovered_for, record_blocks, BLOCK_SCANNED, BLOCK_WINDOW,
    };
    let mut last: Option<u64> = None;
    let mut cursor = state.start_block;
    loop {
        let mut next = Vec::new();
        for a in watched {
            match next_uncovered_for(&state.history, a, cursor, &BLOCK_SCANNED) {
                Ok(n) => next.push((a.clone(), n)),
                Err(e) => {
                    tracing::warn!("address history: block coverage for {a}: {e:#}");
                    return;
                }
            }
        }
        let Some(from) = next.iter().map(|(_, n)| *n).min().filter(|n| *n <= to) else {
            return;
        };
        if let Some((_, mirror_end)) = skip.filter(|(f, l)| *f <= from && from <= *l) {
            cursor = mirror_end.saturating_add(1);
            continue;
        }
        if last.is_some_and(|l| from <= l) {
            state.discovery_errors.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                "address history: block coverage did not advance past {from}; stopping this pass"
            );
            return;
        }
        let mut end = from.saturating_add(BLOCK_WINDOW - 1).min(to);
        if let Some((first, _)) = skip.filter(|(f, _)| from < *f) {
            end = end.min(first - 1);
        }
        // The scan reads the whole window for every address that lacks any of it, so each is
        // recorded over the whole window.
        let addresses: Vec<String> = next
            .into_iter()
            .filter(|(_, n)| *n <= end)
            .map(|(a, _)| a)
            .collect();
        let lacking = addresses.clone();
        let outcome = async {
            let generation = state.history.generation()?;
            let pending = crate::address_discovery::Pending::default();
            let found = d.scan_blocks(&addresses, from, end, &pending).await;
            let history = state.history.clone();
            tokio::task::spawn_blocking(move || {
                pending.persist(&history)?;
                let mut found = found?;
                for a in lacking {
                    let f = found.remove(&a.to_ascii_lowercase()).unwrap_or_default();
                    record_blocks(&history, &a, (from, end), f, generation, false)?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await?
        }
        .await;
        state.set_calls("main", d.main.calls());
        state.set_calls("trace", d.trace.calls());
        match outcome {
            Ok(()) => {
                last = Some(end);
                state.windows.fetch_add(1, Ordering::Relaxed);
                tracing::info!("address history: block bodies read through {end}");
            }
            Err(e) => {
                state.discovery_errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "address history: bodies [{from}, {end}] not recorded, retrying next poll: {e:#}"
                );
                return;
            }
        }
    }
}

/// One poll: read the head and record the finalized head under it, which is what the nest serves
/// through. Fails on a failed head read or a failed write.
pub async fn poll_once(state: &ModeState, source: &impl HeadSource) -> Result<u64> {
    state.polls.fetch_add(1, Ordering::Relaxed);
    let read = source.head().await;
    state
        .rpc_requests
        .store(source.requests(), Ordering::Relaxed);
    let recorded = match read {
        Ok(head) => {
            let history = state.history.clone();
            let head = head.saturating_sub(state.depth);
            tokio::task::spawn_blocking(move || history.set_head(head))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
                .map(|()| head)
        }
        Err(e) => Err(e),
    };
    match recorded {
        Ok(head) => {
            let ms = u64::try_from(state.started.elapsed().as_millis()).unwrap_or(u64::MAX - 1);
            state.last_poll.store(ms + 1, Ordering::Relaxed);
            Ok(head)
        }
        Err(e) => {
            state.poll_errors.fetch_add(1, Ordering::Relaxed);
            Err(e)
        }
    }
}

/// Poll forever, the first poll at once, and catch up after each. A failed poll is logged and
/// retried at the next tick: covered history keeps serving while the RPC is down. A watch wakes it
/// early.
pub async fn cursor(state: Arc<ModeState>, source: impl HeadSource, discovery: impl Discovery) {
    let mut tick = tokio::time::interval(state.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = state.wake.notified() => {}
        }
        match poll_once(&state, &source).await {
            Ok(safe) => discovery.catch_up(&state, safe).await,
            Err(e) => tracing::warn!("address history: poll failed, retrying next interval: {e:#}"),
        }
    }
}

/// A panicking cursor must take /ready down with it, not leave it reading ready over stale history.
fn supervise(
    state: Arc<ModeState>,
    work: tokio::task::JoinHandle<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        match work.await {
            Err(e) if e.is_panic() => state.stop(format!("the cursor task panicked: {e}")),
            _ => {}
        }
    })
}

pub fn router(state: Arc<ModeState>) -> Router {
    Router::new()
        .route("/api", get(api))
        .route("/api/watch", get(watched).post(watch))
        .route("/api/unwatch", post(unwatch))
        .route("/health", get(|| async { "ok" }))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .with_state(state)
}

/// Run a store read off the async workers.
async fn blocking<T: Send + 'static>(
    s: &Arc<ModeState>,
    f: impl FnOnce(&AddressHistory) -> Result<T> + Send + 'static,
) -> Result<T> {
    let history = s.history.clone();
    tokio::task::spawn_blocking(move || f(&history)).await?
}

fn failed(status: StatusCode, e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": e.to_string() })))
}

async fn api(
    State(s): State<Arc<ModeState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    // A txhash lookup needs no coverage: a transaction the store has not traced is traced now, once
    // the request is one `respond` would answer.
    if let Some(forward) = s.forward.clone() {
        if let Some(v) = crate::address_proxy::answer(&q, s.chain_id, &forward).await {
            return Json(v);
        }
    }
    let traceable = q.get("module").map(String::as_str) == Some("account")
        && q.get("action").map(String::as_str) == Some("txlistinternal")
        && q.get("chainid")
            .is_none_or(|c| c.parse::<u64>().ok() == Some(s.chain_id));
    let key = q
        .get("txhash")
        .and_then(|h| h.strip_prefix("0x"))
        .and_then(|h| alloy_primitives::hex::decode(h).ok())
        .filter(|k| k.len() == 32);
    if let (true, Some(key), Some(hash), Some(tracer)) =
        (traceable, key, q.get("txhash").cloned(), s.tracer.clone())
    {
        let known = blocking(&s, move |h| Ok(h.tx_internals(&key)?.is_some())).await;
        if matches!(known, Ok(false)) {
            if let Err(e) = tracer(hash).await {
                if let Some(nf) = e.downcast_ref::<crate::address_discovery::NotFinalized>() {
                    return Json(crate::address_history::incomplete(&nf.to_string()));
                }
                return Json(
                    json!({ "status": "0", "message": "NOTOK", "result": format!("Error! {e:#}") }),
                );
            }
        }
    }
    Json(
        blocking(&s, move |h| {
            Ok(crate::address_history::respond(Some(h), &q))
        })
        .await
        .unwrap_or_else(
            |e| json!({ "status": "0", "message": "NOTOK", "result": format!("Error! {e:#}") }),
        ),
    )
}

#[derive(serde::Deserialize)]
struct AddressBody {
    address: String,
}

async fn watched(State(s): State<Arc<ModeState>>) -> (StatusCode, Json<Value>) {
    match blocking(&s, |h| h.watched()).await {
        Ok(w) => (StatusCode::OK, Json(json!({ "watched": w }))),
        Err(e) => failed(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn watch(
    State(s): State<Arc<ModeState>>,
    Json(b): Json<AddressBody>,
) -> (StatusCode, Json<Value>) {
    change(s, b.address, true).await
}

async fn unwatch(
    State(s): State<Arc<ModeState>>,
    Json(b): Json<AddressBody>,
) -> (StatusCode, Json<Value>) {
    change(s, b.address, false).await
}

async fn change(s: Arc<ModeState>, address: String, add: bool) -> (StatusCode, Json<Value>) {
    if !s.loopback {
        return failed(
            StatusCode::FORBIDDEN,
            "the watched set changes only on a loopback bind",
        );
    }
    if let Err(e) = crate::address_history::parse_address(&address) {
        return failed(StatusCode::BAD_REQUEST, format!("address `{address}`: {e}"));
    }
    let done = blocking(&s, move |h| {
        if add {
            h.watch(&address)?;
        } else {
            h.unwatch(&address)?;
        }
        h.watched()
    })
    .await;
    match done {
        Ok(w) => {
            if add {
                s.wake.notify_one();
            }
            (StatusCode::OK, Json(json!({ "watched": w })))
        }
        Err(e) => failed(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
    }
}

async fn ready(State(s): State<Arc<ModeState>>) -> (StatusCode, Json<Value>) {
    // Offline is not unready: covered history still serves, and a cursor that has not polled for
    // three intervals is reported stalled. A store that cannot be read is unready, and so is a
    // cursor stopped for good: covered history still serves, but nothing will ever extend it.
    let stopped = s.cursor_error.lock().expect("cursor_error lock").clone();
    match blocking(&s, |h| Ok((h.head()?, h.watched()?.len()))).await {
        Ok((head, watched)) if stopped.is_some() => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "address_history",
                "ready": false,
                "chain_id": s.chain_id,
                "head": head,
                "watched": watched,
                "error": stopped,
            })),
        ),
        Ok((head, watched)) => (
            StatusCode::OK,
            Json(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "address_history",
                "ready": true,
                "chain_id": s.chain_id,
                "head": head,
                "poll_interval_secs": s.poll_interval.as_secs(),
                "stalled": s.stalled(),
                "watched": watched,
            })),
        ),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "mode": "address_history",
                "ready": false,
                "error": format!("{e:#}"),
            })),
        ),
    }
}

async fn metrics(State(s): State<Arc<ModeState>>) -> impl IntoResponse {
    let (head, watched) = match blocking(&s, |h| Ok((h.head()?, h.watched()?.len()))).await {
        Ok((head, watched)) => (head.unwrap_or(0), watched),
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::CONTENT_TYPE, "text/plain")],
                format!("store unreadable: {e:#}\n"),
            );
        }
    };
    let mut by_method = String::from(
        "# HELP nuthatch_address_history_rpc_calls_total RPC calls by endpoint role and method.\n\
         # TYPE nuthatch_address_history_rpc_calls_total counter\n",
    );
    for ((endpoint, method), n) in s.calls.lock().expect("calls lock").iter() {
        by_method.push_str(&format!(
            "nuthatch_address_history_rpc_calls_total{{endpoint=\"{endpoint}\",method=\"{method}\"}} {n}\n"
        ));
    }
    let body = format!(
        "{by_method}\
         # TYPE nuthatch_address_history_windows_recorded_total counter\n\
         nuthatch_address_history_windows_recorded_total {}\n\
         # TYPE nuthatch_address_history_discovery_errors_total counter\n\
         nuthatch_address_history_discovery_errors_total {}\n\
         # HELP nuthatch_address_history_rpc_requests_total RPC requests sent, startup checks and failovers included.\n\
         # TYPE nuthatch_address_history_rpc_requests_total counter\n\
         nuthatch_address_history_rpc_requests_total {}\n\
         # TYPE nuthatch_address_history_polls_total counter\n\
         nuthatch_address_history_polls_total {}\n\
         # TYPE nuthatch_address_history_poll_errors_total counter\n\
         nuthatch_address_history_poll_errors_total {}\n\
         # TYPE nuthatch_address_history_head gauge\n\
         nuthatch_address_history_head {head}\n\
         # TYPE nuthatch_address_history_watched gauge\n\
         nuthatch_address_history_watched {watched}\n",
        s.windows.load(Ordering::Relaxed),
        s.discovery_errors.load(Ordering::Relaxed),
        s.rpc_requests.load(Ordering::Relaxed),
        s.polls.load(Ordering::Relaxed),
        s.poll_errors.load(Ordering::Relaxed),
    );
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

/// Traces one transaction into `history` for a txhash lookup, once `verified` says the RPCs are on
/// the nest's chain. Only a finalized transaction's trace is kept; a newer one is refused.
pub fn tracer<M: Rpc, T: Rpc>(
    discovery: Arc<Discoverer<Counted<M>, Counted<T>>>,
    history: AddressHistory,
    verified: Arc<tokio::sync::OnceCell<()>>,
) -> Tracer {
    Arc::new(move |hash: String| {
        let (discovery, history, verified) = (discovery.clone(), history.clone(), verified.clone());
        Box::pin(async move {
            if verified.get().is_none() {
                bail!("the RPC's chain has not been checked yet; try again shortly");
            }
            let pending = crate::address_discovery::Pending::default();
            let finalized = history.head()?.unwrap_or(0);
            discovery
                .internal_rows_of(&history, &hash, &pending, finalized)
                .await?;
            tokio::task::spawn_blocking(move || pending.persist(&history)).await?
        })
    })
}

/// Forwards a `proxy` or `logs` call to the nest's main RPC, once `verified` says it is on the
/// nest's chain. Counted with the rest of the nest's calls.
pub fn forward<M: Rpc, T: Rpc>(
    discovery: Arc<Discoverer<Counted<M>, Counted<T>>>,
    verified: Arc<tokio::sync::OnceCell<()>>,
) -> crate::address_proxy::Forward {
    Arc::new(move |method: String, params: Value| {
        let (discovery, verified) = (discovery.clone(), verified.clone());
        Box::pin(async move {
            if verified.get().is_none() {
                bail!("the RPC's chain has not been checked yet; try again shortly");
            }
            discovery.main.call(&method, params).await
        })
    })
}

/// `--poll-interval` wins over `poll_interval` in `[address_history]`, which wins over the default.
pub fn poll_interval(flag: Option<Duration>, config: &AddressHistoryConfig) -> Result<Duration> {
    match flag {
        Some(d) => Ok(d),
        None => config.poll_interval(),
    }
}

/// `dev` flags that drive the event path. Accepting one here would start a healthy-looking nest that
/// silently ignores it.
fn refuse_event_flags(args: &crate::cli::DevArgs) -> Result<()> {
    let set: Vec<&str> = [
        ("--backfill", args.backfill.is_some()),
        ("--seal-direct", args.seal_direct),
        ("--concurrency", args.concurrency != 1),
        ("--window", args.window.is_some()),
        ("--finality-only", args.finality_only),
        ("--publish-target", args.publish_target.is_some()),
        ("--audit-rpc", args.audit_rpc.is_some()),
        ("--state-rpc", !args.state_rpc.is_empty()),
        ("--ipfs", !args.ipfs.is_empty()),
        ("--registry", args.registry.is_some()),
    ]
    .into_iter()
    .filter_map(|(flag, on)| on.then_some(flag))
    .collect();
    if !set.is_empty() {
        bail!(
            "an address-history nest does not take {}: those drive an event nest's indexing",
            set.join(", ")
        );
    }
    Ok(())
}

/// `nuthatch dev` on a rotki-mode nest.
pub async fn dev(
    dir: &Path,
    config: Config,
    args: &crate::cli::DevArgs,
    cors: Option<tower_http::cors::CorsLayer>,
) -> Result<()> {
    let Some(ah) = &config.address_history else {
        bail!("not an address-history nest");
    };
    if !config.contracts.is_empty() {
        bail!(
            "{} declares both [address_history] and [[contracts]]; an address-history nest watches \
             accounts and indexes no contract, so give each its own nest",
            crate::config::CONFIG_FILE
        );
    }
    refuse_event_flags(args)?;
    let poll_interval = poll_interval(args.poll_interval, ah)?;
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("cannot bind {}", args.listen))?;
    let loopback = listener.local_addr()?.ip().is_loopback();
    let store = crate::store::Store::open(&dir.join(crate::config::DB_FILE))?;
    let history = AddressHistory::open(store, config.nest.chain_id, &ah.addresses)?;

    let urls = crate::rpc::select_rpcs(&args.rpc, config.nest.rpc_urls.clone());
    let trace_urls = if args.trace_rpc.is_empty() {
        urls.clone()
    } else {
        args.trace_rpc.clone()
    };
    let main = RpcClient::with_fallbacks(urls, args.rpc_fallback.clone())?;
    let trace = RpcClient::with_fallbacks(trace_urls, Vec::new())?;
    let chain_id = config.nest.chain_id;
    let depth = match crate::chains::lookup_by_id(chain_id).map(|c| c.finality) {
        Some(crate::chains::Finality::Depth(n)) => n,
        Some(crate::chains::Finality::FinalizedTag { fallback_depth }) => fallback_depth,
        None => 64,
    };
    let discovery = Arc::new(Discoverer::new(Counted::new(main), Counted::new(trace)));
    // Tracing for a txhash lookup waits on the same chain check the cursor does.
    let verified = Arc::new(tokio::sync::OnceCell::<()>::new());
    let tracer = tracer(discovery.clone(), history.clone(), verified.clone());
    let mut state = ModeState::new(history, chain_id, poll_interval, loopback)
        .with_range(ah.start_block.unwrap_or(0), ah.end_block, depth)
        .with_tracer(tracer)
        .with_forward(forward(discovery.clone(), verified.clone()));
    if let Some(m) = &ah.mirror {
        if m.chain_id != chain_id {
            bail!(
                "[address_history.mirror] names chain {}, and this nest is on chain {chain_id}",
                m.chain_id
            );
        }
        // Before anything is served: RPC-scanned rows in the mirror's blocks must not answer while the
        // cursor, which may never get past its chain check, has yet to replace them.
        state.history.keep_verified_only(
            &crate::address_discovery::BLOCK_SCANNED,
            (m.from_block, m.to_block),
        )?;
        let source = crate::address_partitions::Source::open(
            &m.url,
            chain_id,
            dir.join("partitions"),
            m.cache_mb * 1024 * 1024,
        )?;
        tracing::info!(
            "address history: blocks {} to {} from verified partitions at the configured mirror, which \
             sees this machine's IP and the block ranges asked for, never an address",
            m.from_block,
            m.to_block
        );
        state = state.with_mirror(Arc::new(source), (m.from_block, m.to_block));
    }
    let state = Arc::new(state);
    tracing::info!(
        "address history: {} watched from block {}, polling every {}s",
        state.history.watched()?.len(),
        state.start_block,
        poll_interval.as_secs()
    );
    // The chain check runs in the cursor, not before serving: covered history answers at once, even
    // offline. A wrong chain stops the cursor before it fetches anything.
    let task_state = state.clone();
    let work = tokio::spawn(async move {
        for (role, rpc) in [
            ("main", discovery.main.inner()),
            ("trace", discovery.trace.inner()),
        ] {
            if let Err(e) = rpc.verify_chain_ids(chain_id).await {
                task_state.stop(format!("the {role} RPC: {e:#}"));
                return;
            }
        }
        let _ = verified.set(());
        task_state.rpc_requests.store(
            discovery.main.inner().request_count() + discovery.trace.inner().request_count(),
            Ordering::Relaxed,
        );
        cursor(task_state, MainHead(discovery.clone()), discovery).await;
    });
    let work_abort = work.abort_handle();
    let cursor = supervise(state.clone(), work);
    let served = crate::serve::serve_bound(listener, router(state), cors).await;
    cursor.abort();
    work_abort.abort();
    served
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tower::ServiceExt;

    const ALICE: &str = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
    const BOB: &str = "0x00000000219ab540356cbb839cbe05303d7705fa";

    /// A head source: 0 means down. Each read counts as `per_read` requests, as a failover would.
    struct FixedHead {
        head: AtomicU64,
        reads: AtomicU64,
        per_read: u64,
    }
    fn fixed(head: u64, per_read: u64) -> Arc<FixedHead> {
        Arc::new(FixedHead {
            head: AtomicU64::new(head),
            reads: AtomicU64::new(0),
            per_read,
        })
    }
    impl HeadSource for FixedHead {
        async fn head(&self) -> Result<u64> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            match self.head.load(Ordering::Relaxed) {
                0 => bail!("down"),
                h => Ok(h),
            }
        }
        fn requests(&self) -> u64 {
            self.reads.load(Ordering::Relaxed) * self.per_read
        }
    }

    fn state(dir: &Path, loopback: bool) -> Arc<ModeState> {
        let store = Store::open(&dir.join("t.redb")).unwrap();
        let history = AddressHistory::open(store, 1, &[ALICE.into()]).unwrap();
        Arc::new(ModeState::new(
            history,
            1,
            Duration::from_secs(300),
            loopback,
        ))
    }

    async fn call(
        app: Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = axum::http::Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                axum::body::Body::from(b.to_string())
            }
            None => axum::body::Body::empty(),
        };
        let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()));
        (status, v)
    }

    #[tokio::test]
    async fn a_poll_is_one_head_read_and_lands_on_ready_and_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        let source = fixed(1_234, 1);
        assert_eq!(poll_once(&s, &source).await.unwrap(), 1_234);
        assert_eq!(
            source.reads.load(Ordering::Relaxed),
            1,
            "one head read per poll"
        );

        let (_, r) = call(router(s.clone()), "GET", "/ready", None).await;
        assert_eq!(r["head"], 1_234, "{r}");
        assert_eq!(r["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(r["stalled"], false);
        let (_, m) = call(router(s.clone()), "GET", "/metrics", None).await;
        let m = m.as_str().unwrap();
        assert!(
            m.contains("nuthatch_address_history_rpc_requests_total 1\n"),
            "{m}"
        );
        assert!(m.contains("nuthatch_address_history_head 1234\n"), "{m}");

        source.head.store(0, Ordering::Relaxed);
        assert!(poll_once(&s, &source).await.is_err());
        let (_, r) = call(router(s.clone()), "GET", "/ready", None).await;
        assert_eq!(r["head"], 1_234, "a failed poll keeps the last head");
        assert_eq!(r["ready"], true, "offline still serves");
        let (_, m) = call(router(s.clone()), "GET", "/metrics", None).await;
        assert!(m
            .as_str()
            .unwrap()
            .contains("nuthatch_address_history_poll_errors_total 1\n"));
    }

    /// The counter is the source's own request count, so a read that failed over to a second
    /// endpoint shows as two requests, not one poll.
    #[tokio::test]
    async fn metrics_count_requests_not_polls() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        let source = fixed(9, 2);
        poll_once(&s, &source).await.unwrap();
        let (_, m) = call(router(s.clone()), "GET", "/metrics", None).await;
        let m = m.as_str().unwrap();
        assert!(
            m.contains("nuthatch_address_history_rpc_requests_total 2\n"),
            "{m}"
        );
        assert!(
            m.contains("nuthatch_address_history_polls_total 1\n"),
            "{m}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_cursor_polls_once_per_interval_and_reports_a_stall() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        let source = fixed(7, 1);
        let task = tokio::spawn(cursor(s.clone(), source.clone(), ()));
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        settle().await;
        assert_eq!(source.reads.load(Ordering::Relaxed), 1, "one poll at start");
        tokio::time::advance(Duration::from_secs(299)).await;
        settle().await;
        assert_eq!(
            source.reads.load(Ordering::Relaxed),
            1,
            "none before the interval"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(source.reads.load(Ordering::Relaxed), 2);
        task.abort();
    }

    #[test]
    fn a_cursor_quiet_for_three_intervals_is_stalled() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        assert!(!s.stalled());
        let fresh = ModeState::new(s.history.clone(), 1, Duration::from_millis(1), true);
        std::thread::sleep(Duration::from_millis(10));
        assert!(fresh.stalled(), "no poll in three intervals");
        let ms = u64::try_from(fresh.started.elapsed().as_millis()).unwrap();
        fresh.last_poll.store(ms + 1, Ordering::Relaxed);
        assert!(!fresh.stalled(), "a poll just now");
    }

    #[tokio::test]
    async fn a_panicking_cursor_takes_ready_down() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        let work = tokio::spawn(async { panic!("a provider answer the decoder could not take") });
        supervise(s.clone(), work).await.unwrap();
        let (st, r) = call(router(s), "GET", "/ready", None).await;
        assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "{r}");
        assert!(r["error"].as_str().unwrap().contains("panicked"), "{r}");
    }

    #[tokio::test]
    async fn watch_and_unwatch_take_effect_without_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        let (st, r) = call(
            router(s.clone()),
            "POST",
            "/api/watch",
            Some(json!({"address": BOB})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["watched"].as_array().unwrap().len(), 2);
        let q = format!("/api?module=account&action=txlist&address={BOB}&startblock=0&endblock=1");
        let (_, a) = call(router(s.clone()), "GET", &q, None).await;
        assert!(
            a["result"]
                .as_str()
                .unwrap()
                .starts_with("NUTHATCH_INCOMPLETE:"),
            "{a}"
        );

        let (st, r) = call(
            router(s.clone()),
            "POST",
            "/api/unwatch",
            Some(json!({"address": BOB})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(r["watched"], json!([ALICE.to_lowercase()]));
        let (st, _) = call(
            router(s.clone()),
            "POST",
            "/api/watch",
            Some(json!({"address": "0xnope"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn watch_is_refused_off_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), false);
        let (st, _) = call(
            router(s.clone()),
            "POST",
            "/api/watch",
            Some(json!({"address": BOB})),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(s.history.watched().unwrap().len(), 1);
    }

    /// Five minutes, written out: a test that compared against the constant would move with it.
    #[test]
    fn the_poll_interval_defaults_to_five_minutes_and_the_flag_wins() {
        let mut cfg = AddressHistoryConfig {
            addresses: vec![ALICE.into()],
            start_block: None,
            end_block: None,
            poll_interval: None,
            mirror: None,
        };
        assert_eq!(
            poll_interval(None, &cfg).unwrap(),
            Duration::from_secs(5 * 60)
        );
        cfg.poll_interval = Some("2m".into());
        assert_eq!(poll_interval(None, &cfg).unwrap(), Duration::from_secs(120));
        assert_eq!(
            poll_interval(Some(Duration::from_secs(7)), &cfg).unwrap(),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn event_flags_are_refused_by_name() {
        use clap::Parser;
        let parse = |extra: &[&str]| {
            let mut argv = vec!["nuthatch", "dev", "--dir", "x"];
            argv.extend_from_slice(extra);
            match crate::cli::Cli::parse_from(argv).command {
                crate::cli::Command::Dev(a) => a,
                _ => unreachable!(),
            }
        };
        refuse_event_flags(&parse(&[])).unwrap();
        refuse_event_flags(&parse(&["--poll-interval", "1m", "--rpc", "https://x"])).unwrap();
        for flag in [
            &["--backfill", "10"][..],
            &["--seal-direct"],
            &["--window", "100"],
            &["--finality-only"],
            &["--publish-target", "s3://x"],
            &["--audit-rpc", "https://x"],
        ] {
            let err = refuse_event_flags(&parse(flag)).unwrap_err();
            assert!(err.to_string().contains(flag[0]), "{flag:?}: {err}");
        }
    }

    /// With a mirror, block history comes from verified partitions: no body is read over RPC in the
    /// mirror's blocks, only each partition's anchor and the bodies of blocks a watched address
    /// produced, and a partition the mirror lacks leaves its blocks incomplete.
    #[tokio::test]
    async fn the_mirror_covers_its_blocks_and_a_missing_partition_stays_incomplete() {
        use crate::address_discovery::Discoverer;
        use crate::address_history::{Action, Answer, PageRequest, Sort};
        use crate::address_partitions::tests::{chain_rpc, Fake, MINER, PAID};
        use crate::address_partitions::{build, partition_key, Source, SPAN};
        let mirror = tempfile::tempdir().unwrap();
        let chain = chain_rpc(2 * SPAN + 5);
        build(
            &chain,
            mirror.path().to_str().unwrap(),
            1,
            (0, 2 * SPAN - 1),
            4,
            false,
            |_, _, _| {},
        )
        .await
        .unwrap();
        std::fs::remove_file(mirror.path().join(partition_key(1, SPAN))).unwrap();

        // The RPC's finalized block, which the test moves.
        let finalized = Arc::new(AtomicU64::new(2 * SPAN + 5));
        let fin = finalized.clone();
        let main = Fake::new(move |m, p| match m {
            "eth_getBlockByNumber" if p[0] == "finalized" => {
                Ok(json!({"number": format!("0x{:x}", fin.load(Ordering::Relaxed))}))
            }
            "eth_getLogs" => Ok(json!([])),
            "eth_getCode" => Ok(json!("0x")),
            "eth_getTransactionCount" => Ok(json!("0x0")),
            "eth_getBlockTransactionCountByNumber" => Ok(json!("0x1")),
            _ => (chain.answer)(m, p),
        });
        let trace = Fake::new(|m, _| match m {
            "trace_filter" => Ok(json!([])),
            "trace_block" => Ok(json!([{"x": 1}])),
            other => bail!("unexpected {other}"),
        });
        let d = Discoverer::new(Counted::new(main), Counted::new(trace));
        let (paid, miner) = (format!("{PAID:#x}"), format!("{MINER:#x}"));
        let dir = tempfile::tempdir().unwrap();
        let history = AddressHistory::open(
            Store::open(&dir.path().join("t.redb")).unwrap(),
            1,
            &[paid.clone(), miner.clone()],
        )
        .unwrap();
        let source = Source::open(
            mirror.path().to_str().unwrap(),
            1,
            dir.path().join("partitions"),
            u64::MAX,
        )
        .unwrap();
        let state = ModeState::new(history.clone(), 1, Duration::from_secs(300), true)
            .with_range(5, Some(2 * SPAN - 1), 0)
            .with_mirror(Arc::new(source), (0, 2 * SPAN - 1));
        d.catch_up(&state, 2 * SPAN + 5).await;

        let covered = vec![(5, SPAN - 1)];
        assert_eq!(
            history.coverage(Action::BeaconWithdrawals, &paid).unwrap(),
            covered
        );
        assert_eq!(
            history.coverage(Action::MinedBlocks, &miner).unwrap(),
            covered
        );
        assert_eq!(history.header_coverage().unwrap(), covered);
        assert_eq!(
            d.main.calls().get("eth_getBlockByNumber"),
            Some(&3),
            "the finality check, one anchor per partition and no body"
        );
        assert_eq!(
            d.main.calls().get("eth_getBlockByHash"),
            Some(&99),
            "blocks 100 to 9,900"
        );

        let page = |action, address: &str, start, end| {
            history
                .page(
                    action,
                    &PageRequest {
                        address: address.into(),
                        start_block: start,
                        end_block: end,
                        sort: Sort::Asc,
                        page: 1,
                        offset: 10_000,
                        generation: None,
                    },
                )
                .unwrap()
        };
        let Answer::Rows(w) = page(Action::BeaconWithdrawals, &paid, Some(5), Some(SPAN - 1))
        else {
            panic!("covered")
        };
        assert_eq!(w.len() as u64, SPAN - 5);
        let Answer::Rows(m) = page(Action::MinedBlocks, &miner, Some(5), Some(SPAN - 1)) else {
            panic!("covered")
        };
        assert_eq!(m.len(), 99);
        for (start, end) in [(None, Some(SPAN - 1)), (Some(5), Some(SPAN + 10))] {
            assert!(
                matches!(
                    page(Action::MinedBlocks, &miner, start, end),
                    Answer::Incomplete(_)
                ),
                "{start:?}..{end:?}"
            );
        }
        assert_eq!(
            history
                .block_by_time(1_600_000_000 + 12 * 77, false)
                .unwrap(),
            Some(77)
        );
        assert_eq!(
            history
                .block_by_time(1_600_000_000 + 12 * 77 + 1, true)
                .unwrap(),
            Some(78)
        );
        assert_eq!(
            history
                .block_by_time(1_600_000_000 + 12 * SPAN, true)
                .unwrap(),
            None
        );

        // Published late, the partition is used, but only once its last block is final for the nest.
        build(
            &chain_rpc(2 * SPAN + 5),
            mirror.path().to_str().unwrap(),
            1,
            (SPAN, 2 * SPAN - 1),
            4,
            false,
            |_, _, _| {},
        )
        .await
        .unwrap();
        finalized.store(2 * SPAN - 2, Ordering::Relaxed);
        let reads = d.main.calls()["eth_getBlockByNumber"];
        d.catch_up(&state, 2 * SPAN + 5).await;
        assert_eq!(history.header_coverage().unwrap(), covered);
        assert_eq!(
            d.main.calls()["eth_getBlockByNumber"],
            reads + 1,
            "the finality check, and no anchor past it"
        );
        finalized.store(2 * SPAN - 1, Ordering::Relaxed);
        d.catch_up(&state, 2 * SPAN + 5).await;
        assert_eq!(history.header_coverage().unwrap(), vec![(5, 2 * SPAN - 1)]);
        assert_eq!(
            history.coverage(Action::MinedBlocks, &miner).unwrap(),
            vec![(5, 2 * SPAN - 1)]
        );
    }

    /// The mirror pass runs for headers alone when nothing watched lacks the blocks, a range that
    /// ends mid-partition still takes that partition, and the RPC scan stops where the mirror starts.
    #[tokio::test]
    async fn the_mirror_meets_unaligned_ranges_and_the_rpc_scan_at_its_edges() {
        use crate::address_discovery::Discoverer;
        use crate::address_history::Action;
        use crate::address_partitions::tests::{chain_rpc, Fake, PAID};
        use crate::address_partitions::{build, Source, SPAN};
        let mirror = tempfile::tempdir().unwrap();
        build(
            &chain_rpc(2 * SPAN + 5),
            mirror.path().to_str().unwrap(),
            1,
            (0, 2 * SPAN - 1),
            4,
            false,
            |_, _, _| {},
        )
        .await
        .unwrap();
        let nest = |seed: &[String], range: (u64, u64), blocks: (u64, u64)| {
            let chain = chain_rpc(2 * SPAN + 5);
            let main = Fake::new(move |m, p| match m {
                "eth_getLogs" => Ok(json!([])),
                "eth_getCode" => Ok(json!("0x")),
                "eth_getTransactionCount" => Ok(json!("0x0")),
                "eth_getBlockTransactionCountByNumber" => Ok(json!("0x1")),
                _ => (chain.answer)(m, p),
            });
            let trace = Fake::new(|m, _| match m {
                "trace_filter" => Ok(json!([])),
                "trace_block" => Ok(json!([{"x": 1}])),
                other => bail!("unexpected {other}"),
            });
            let dir = tempfile::tempdir().unwrap();
            let history =
                AddressHistory::open(Store::open(&dir.path().join("t.redb")).unwrap(), 1, seed)
                    .unwrap();
            let source = Source::open(
                mirror.path().to_str().unwrap(),
                1,
                dir.path().join("partitions"),
                u64::MAX,
            )
            .unwrap();
            let state = ModeState::new(history.clone(), 1, Duration::from_secs(300), true)
                .with_range(range.0, Some(range.1), 0)
                .with_mirror(Arc::new(source), blocks);
            (
                dir,
                Discoverer::new(Counted::new(main), Counted::new(trace)),
                history,
                state,
            )
        };

        let (_dir, d, history, state) = nest(&[], (5, SPAN - 10), (0, 2 * SPAN - 1));
        d.catch_up(&state, 2 * SPAN + 5).await;
        assert_eq!(history.header_coverage().unwrap(), vec![(5, SPAN - 10)]);

        let paid = format!("{PAID:#x}");
        let (_dir, d, history, state) = nest(
            std::slice::from_ref(&paid),
            (SPAN - 20, SPAN + 30),
            (SPAN, 2 * SPAN - 1),
        );
        d.catch_up(&state, 2 * SPAN + 5).await;
        assert_eq!(
            history.coverage(Action::BeaconWithdrawals, &paid).unwrap(),
            vec![(SPAN - 20, SPAN + 30)]
        );
        assert_eq!(
            d.main.calls()["eth_getBlockByNumber"],
            20 + 2,
            "twenty bodies before the mirror, its finality check and one anchor"
        );

        // Coverage the RPC scan recorded before a mirror was configured does not answer for the
        // mirror's blocks while no partition has replaced it.
        let (_dir, d, history, state) = nest(
            std::slice::from_ref(&paid),
            (2 * SPAN, 2 * SPAN + 5),
            (2 * SPAN, 3 * SPAN - 1),
        );
        let g = history.generation().unwrap();
        history
            .record(
                Action::BeaconWithdrawals,
                &paid,
                &[],
                (2 * SPAN, 2 * SPAN + 5),
                g,
            )
            .unwrap();
        d.catch_up(&state, 2 * SPAN + 5).await;
        assert_eq!(
            history.coverage(Action::BeaconWithdrawals, &paid).unwrap(),
            vec![]
        );
    }
}
