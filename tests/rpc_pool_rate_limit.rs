//! An endpoint in the pool answering 429 is backed off and routed around (#1853).
//!
//! Found on a Base backfill: the healthy endpoint refused an over-wide range, a second endpoint
//! answered 429, and the 429 was what the pool reported. A rate limit is not narrowable, so the same
//! range was retried at the same width for as long as anyone watched.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};

use common::tape::*;
use nuthatch::{indexer, rpc::RpcClient};

/// The widest `eth_getLogs` range the healthy endpoint serves.
const CAP: u64 = 1_000;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    url
}

fn hex_u64(v: &Value) -> u64 {
    u64::from_str_radix(v.as_str().unwrap().trim_start_matches("0x"), 16).unwrap()
}

/// Serves an empty chain, refusing any `eth_getLogs` range wider than [`CAP`] as a cap.
async fn healthy() -> String {
    async fn handler(Json(req): Json<Value>) -> Json<Value> {
        let id = req["id"].clone();
        if req["method"] == "eth_getLogs" {
            let f = &req["params"][0];
            let (from, to) = (hex_u64(&f["fromBlock"]), hex_u64(&f["toBlock"]));
            if to - from + 1 > CAP {
                return Json(json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32602, "message": "query returned more than 10000 results"}}));
            }
            return Json(json!({"jsonrpc": "2.0", "id": id, "result": []}));
        }
        Json(json!({"jsonrpc": "2.0", "id": id, "result": null}))
    }
    serve(Router::new().route("/", post(handler))).await
}

/// Answers every request with HTTP 429, with a `Retry-After` when given one, counting them.
async fn throttled(hits: Arc<AtomicUsize>, retry_after: Option<&'static str>) -> String {
    let app = Router::new()
        .route(
            "/",
            post(move |State(hits): State<Arc<AtomicUsize>>| async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let mut resp = (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
                if let Some(secs) = retry_after {
                    resp.headers_mut()
                        .insert("retry-after", secs.parse().unwrap());
                }
                resp
            }),
        )
        .with_state(hits);
    serve(app).await
}

/// Backfill blocks 1 to 20,000 of an empty chain from a 4,000-block window, through a pool of the
/// healthy endpoint and one that always answers 429. Returns how often the 429ing one was asked.
async fn backfill_beside_a_429(retry_after: Option<&'static str>) -> usize {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let registry = nuthatch::registry::from_nest(dir.path(), &cfg).expect("registry");
    let addresses: Vec<String> = cfg.contracts.iter().map(|c| c.address.clone()).collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    let hits = Arc::new(AtomicUsize::new(0));
    let pool = RpcClient::new(vec![
        healthy().await,
        throttled(hits.clone(), retry_after).await,
    ])
    .unwrap();

    let run = indexer::backfill_direct_pipelined(
        &pool,
        &registry,
        dir.path(),
        &addresses,
        &topic0s,
        &[],
        None,
        0,
        1,
        20_000,
        4 * CAP,
        nuthatch::chains::DEFAULT_SEAL_SPAN,
        1,
        |_| Ok(()),
        |_, _, _| {},
    );
    let done = tokio::time::timeout(Duration::from_secs(30), run).await;
    assert!(
        matches!(done, Ok(Ok(_))),
        "the backfill must finish on the healthy endpoint, got {done:?} after {} requests to the \
         429ing one",
        hits.load(Ordering::SeqCst)
    );
    hits.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backfill_routes_around_an_endpoint_that_answers_429() {
    // Its Retry-After outlasts the whole run, so it is asked once.
    let hits = backfill_beside_a_429(Some("60")).await;
    assert_eq!(hits, 1, "the 429ing endpoint was asked {hits} times");
}

/// Without a hint it cools like any failed endpoint, but is not the fallback each time the healthy
/// endpoint refuses a range.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backfill_finishes_beside_a_429_that_names_no_retry_after() {
    let hits = backfill_beside_a_429(None).await;
    assert_eq!(hits, 1, "the 429ing endpoint was asked {hits} times");
}

/// With every endpoint answering 429 and naming a second, the pool waits that second between rounds
/// rather than asking again at once, however often it is called.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pool_that_is_all_429_waits_as_long_as_it_was_asked() {
    let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let pool = RpcClient::new(vec![
        throttled(a.clone(), Some("1")).await,
        throttled(b.clone(), Some("1")).await,
    ])
    .unwrap();

    let start = Instant::now();
    let mut calls = 0;
    while start.elapsed() < Duration::from_millis(2_500) {
        assert!(pool.block_number().await.is_err());
        calls += 1;
    }
    let (a, b) = (a.load(Ordering::SeqCst), b.load(Ordering::SeqCst));
    assert!(calls >= 2, "only {calls} calls in 2.5s");
    assert!(
        a <= 4 && b <= 4,
        "{calls} calls in 2.5s asked the endpoints {a} and {b} times"
    );
}

/// Without a `Retry-After` the rest is the ordinary cooldown, so a pool that is all 429 is asked once
/// and then not again for the window measured here, however often it is called.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pool_that_is_all_429_without_a_hint_is_not_asked_again_at_once() {
    let (a, b) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let pool = RpcClient::new(vec![
        throttled(a.clone(), None).await,
        throttled(b.clone(), None).await,
    ])
    .unwrap();

    let calls = AtomicUsize::new(0);
    let _ = tokio::time::timeout(Duration::from_millis(2_500), async {
        loop {
            assert!(pool.block_number().await.is_err());
            calls.fetch_add(1, Ordering::SeqCst);
        }
    })
    .await;
    let (a, b) = (a.load(Ordering::SeqCst), b.load(Ordering::SeqCst));
    assert!(
        a <= 2 && b <= 2,
        "{} calls in 2.5s asked the endpoints {a} and {b} times",
        calls.load(Ordering::SeqCst)
    );
}
