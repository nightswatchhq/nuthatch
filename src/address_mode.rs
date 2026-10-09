//! The rotki-mode runtime (RFC-0063): a nest with `[address_history]` and no contracts.
//!
//! It shares nothing with the event path. No decode registry, no DBSP circuit, no Burrmill, no
//! sealing: one redb handle, one cursor that reads the chain head every `poll_interval`, and a small
//! router serving `/api`, `/health`, `/ready` and `/metrics`. Discovery (slices 3 to 5) runs inside
//! [`poll_once`]; today a poll is exactly one RPC call.

use anyhow::{bail, Context, Result};
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

use crate::address_history::AddressHistory;
use crate::config::Config;
use crate::rpc::RpcClient;

/// Where the cursor reads the chain head. A trait so tests drive the loop without a network.
pub trait HeadSource: Send + Sync + 'static {
    fn head(&self) -> impl std::future::Future<Output = Result<u64>> + Send;
}

impl HeadSource for RpcClient {
    async fn head(&self) -> Result<u64> {
        self.block_number().await
    }
}

pub struct ModeState {
    pub history: AddressHistory,
    pub chain_id: u64,
    pub poll_interval: Duration,
    /// Whether the server is bound to loopback. Watch and unwatch change what the nest asks its RPC
    /// for, so they are refused on any other bind.
    pub loopback: bool,
    started: Instant,
    /// Unix seconds of the last successful head read; 0 for none yet.
    last_poll: AtomicU64,
    polls: AtomicU64,
    poll_errors: AtomicU64,
    rpc_calls: AtomicU64,
}

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
            rpc_calls: AtomicU64::new(0),
        }
    }
}

/// One poll: read the head and record it. The only RPC call a poll makes until discovery lands.
pub async fn poll_once(state: &ModeState, source: &impl HeadSource) -> Result<u64> {
    state.polls.fetch_add(1, Ordering::Relaxed);
    state.rpc_calls.fetch_add(1, Ordering::Relaxed);
    let head = match source.head().await {
        Ok(h) => h,
        Err(e) => {
            state.poll_errors.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
    };
    let history = state.history.clone();
    tokio::task::spawn_blocking(move || history.set_head(head)).await??;
    state.last_poll.store(unix_now(), Ordering::Relaxed);
    Ok(head)
}

/// Poll forever. A failed poll is logged and retried at the next tick: covered history keeps
/// serving while the RPC is down.
pub async fn cursor(state: Arc<ModeState>, source: impl HeadSource) {
    let mut tick = tokio::time::interval(state.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if let Err(e) = poll_once(&state, &source).await {
            tracing::warn!("address history: head read failed, retrying next poll: {e:#}");
        }
    }
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

async fn api(
    State(s): State<Arc<ModeState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Json<Value> {
    Json(
        tokio::task::spawn_blocking(move || crate::address_history::respond(Some(&s.history), &q))
            .await
            .unwrap_or_else(
                |e| json!({ "status": "0", "message": "NOTOK", "result": format!("Error! {e}") }),
            ),
    )
}

#[derive(serde::Deserialize)]
struct AddressBody {
    address: String,
}

async fn watched(State(s): State<Arc<ModeState>>) -> impl IntoResponse {
    match s.history.watched() {
        Ok(w) => (StatusCode::OK, Json(json!({ "watched": w }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{e:#}") })),
        ),
    }
}

async fn watch(State(s): State<Arc<ModeState>>, Json(b): Json<AddressBody>) -> impl IntoResponse {
    change(s, b.address, true).await
}

async fn unwatch(State(s): State<Arc<ModeState>>, Json(b): Json<AddressBody>) -> impl IntoResponse {
    change(s, b.address, false).await
}

async fn change(s: Arc<ModeState>, address: String, add: bool) -> (StatusCode, Json<Value>) {
    if !s.loopback {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": "the watched set changes only on a loopback bind" })),
        );
    }
    let history = s.history.clone();
    let done = tokio::task::spawn_blocking(move || {
        if add {
            history.watch(&address)?;
        } else {
            history.unwatch(&address)?;
        }
        history.watched()
    })
    .await;
    match done {
        Ok(Ok(w)) => (StatusCode::OK, Json(json!({ "watched": w }))),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("{e:#}") })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        ),
    }
}

async fn ready(State(s): State<Arc<ModeState>>) -> Json<Value> {
    let last = s.last_poll.load(Ordering::Relaxed);
    let head = s.history.head().ok().flatten();
    // Offline is not unready: covered history still serves. A cursor that has not read the head for
    // three intervals is reported stalled so an operator can see it.
    let stalled = if last == 0 {
        s.started.elapsed() > 3 * s.poll_interval
    } else {
        unix_now().saturating_sub(last) > 3 * s.poll_interval.as_secs()
    };
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "address_history",
        "ready": true,
        "chain_id": s.chain_id,
        "head": head,
        "last_poll": (last != 0).then_some(last),
        "poll_interval_secs": s.poll_interval.as_secs(),
        "stalled": stalled,
        "watched": s.history.watched().map(|w| w.len()).unwrap_or(0),
    }))
}

async fn metrics(State(s): State<Arc<ModeState>>) -> impl IntoResponse {
    let head = s.history.head().ok().flatten().unwrap_or(0);
    let body = format!(
        "# HELP nuthatch_address_history_rpc_calls_total RPC calls made by the address-history cursor.\n\
         # TYPE nuthatch_address_history_rpc_calls_total counter\n\
         nuthatch_address_history_rpc_calls_total {}\n\
         # TYPE nuthatch_address_history_polls_total counter\n\
         nuthatch_address_history_polls_total {}\n\
         # TYPE nuthatch_address_history_poll_errors_total counter\n\
         nuthatch_address_history_poll_errors_total {}\n\
         # TYPE nuthatch_address_history_head gauge\n\
         nuthatch_address_history_head {head}\n\
         # TYPE nuthatch_address_history_watched gauge\n\
         nuthatch_address_history_watched {}\n",
        s.rpc_calls.load(Ordering::Relaxed),
        s.polls.load(Ordering::Relaxed),
        s.poll_errors.load(Ordering::Relaxed),
        s.history.watched().map(|w| w.len()).unwrap_or(0),
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `--poll-interval` wins over `poll_interval` in `[address_history]`, which wins over the default.
pub fn poll_interval(
    flag: Option<Duration>,
    config: &crate::address_history::AddressHistoryConfig,
) -> Result<Duration> {
    match flag {
        Some(d) => Ok(d),
        None => config.poll_interval(),
    }
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
    let poll_interval = poll_interval(args.poll_interval, ah)?;
    let store = crate::store::Store::open(&dir.join(crate::config::DB_FILE))?;
    let history = AddressHistory::open(store, config.nest.chain_id, &ah.addresses)?;

    let urls = crate::rpc::select_rpcs(&args.rpc, config.nest.rpc_urls.clone());
    let rpc = RpcClient::with_fallbacks(urls, args.rpc_fallback.clone())?;
    let state = Arc::new(ModeState::new(
        history,
        config.nest.chain_id,
        poll_interval,
        crate::serve::is_localhost(&args.listen),
    ));
    rpc.verify_chain_ids(config.nest.chain_id)
        .await
        .context("checking the RPC is on this nest's chain")?;
    tracing::info!(
        "address history: {} watched, polling every {}s",
        state.history.watched()?.len(),
        poll_interval.as_secs()
    );
    let cursor = tokio::spawn(cursor(state.clone(), rpc));
    let served = crate::serve::bind_and_serve(&args.listen, router(state), cors).await;
    cursor.abort();
    served
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tower::ServiceExt;

    const ALICE: &str = "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045";
    const BOB: &str = "0x00000000219ab540356cbb839cbe05303d7705fa";

    struct FixedHead(AtomicU64, AtomicU64);
    impl HeadSource for Arc<FixedHead> {
        async fn head(&self) -> Result<u64> {
            self.1.fetch_add(1, Ordering::Relaxed);
            match self.0.load(Ordering::Relaxed) {
                0 => bail!("down"),
                h => Ok(h),
            }
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
        let source = Arc::new(FixedHead(AtomicU64::new(1_234), AtomicU64::new(0)));
        assert_eq!(poll_once(&s, &source).await.unwrap(), 1_234);
        assert_eq!(source.1.load(Ordering::Relaxed), 1, "one RPC call per poll");

        let (_, r) = call(router(s.clone()), "GET", "/ready", None).await;
        assert_eq!(r["head"], 1_234, "{r}");
        assert_eq!(r["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(r["stalled"], false);
        let (_, m) = call(router(s.clone()), "GET", "/metrics", None).await;
        let m = m.as_str().unwrap();
        assert!(
            m.contains("nuthatch_address_history_rpc_calls_total 1\n"),
            "{m}"
        );
        assert!(m.contains("nuthatch_address_history_head 1234\n"), "{m}");

        source.0.store(0, Ordering::Relaxed);
        assert!(poll_once(&s, &source).await.is_err());
        let (_, r) = call(router(s.clone()), "GET", "/ready", None).await;
        assert_eq!(r["head"], 1_234, "a failed poll keeps the last head");
        assert_eq!(r["ready"], true, "offline still serves");
    }

    #[tokio::test(start_paused = true)]
    async fn the_cursor_polls_once_per_interval() {
        let dir = tempfile::tempdir().unwrap();
        let s = state(dir.path(), true);
        let source = Arc::new(FixedHead(AtomicU64::new(7), AtomicU64::new(0)));
        let task = tokio::spawn(cursor(s.clone(), source.clone()));
        let settle = || async {
            for _ in 0..20 {
                tokio::task::yield_now().await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        settle().await;
        assert_eq!(source.1.load(Ordering::Relaxed), 1, "one poll at start");
        tokio::time::advance(Duration::from_secs(299)).await;
        settle().await;
        assert_eq!(
            source.1.load(Ordering::Relaxed),
            1,
            "none before the interval"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        settle().await;
        assert_eq!(source.1.load(Ordering::Relaxed), 2);
        task.abort();
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

    /// Five minutes, written out: a test that compared against the constant would move with it.
    #[test]
    fn the_poll_interval_defaults_to_five_minutes_and_the_flag_wins() {
        let mut cfg = crate::address_history::AddressHistoryConfig {
            addresses: vec![ALICE.into()],
            start_block: None,
            end_block: None,
            poll_interval: None,
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
}
