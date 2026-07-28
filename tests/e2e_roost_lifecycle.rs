//! Roost lifecycle end-to-end: unmounting a nest releases everything it held (RFC-0027 §6).
//!
//! The acceptance test that matters here is not "does the route disappear" - that is easy and
//! unconvincing - but **does the nest's redb file actually become free**. `Store` is an `Arc<Database>`
//! cloned three ways at nest construction (the cursor, the alert delivery worker, the serving state),
//! and redb only releases the file when the last clone drops. Reopening the store is therefore a
//! single assertion that proves all three were let go; miss any one and it fails.

mod common;

use std::sync::Arc;

use nuthatch::{health::RoostHealth, indexer, roost, serve, store::Store};

use common::tape::*;

/// A two-nest, one-chain roost over a scripted tape, wrapped in the driver handles a live roost keeps.
async fn two_nest_roost(
    usdc_dir: &std::path::Path,
    arb_dir: &std::path::Path,
) -> (roost::RoostHandles, Arc<TapeSource>) {
    let tape = Arc::new(TapeSource::new());
    let a1 = account(1);
    let a2 = account(2);
    for b in 1..=3u64 {
        tape.insert_block(
            b,
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
            ),
        );
    }
    tape.advance_tip_to(3);

    let cfg_u = scaffold_nest(usdc_dir, "usdc", USDC);
    let cfg_a = scaffold_nest(arb_dir, "arb", ARB);
    let health = Arc::new(RoostHealth::new());
    health.register("usdc", "arbitrum-one");
    health.register("arb", "arbitrum-one");

    let cursor = indexer::spawn_roost(
        tape.clone(),
        vec![
            ("usdc".to_string(), usdc_dir.to_path_buf(), cfg_u),
            ("arb".to_string(), arb_dir.to_path_buf(), cfg_a),
        ],
        None,
        false,
        1,
        Some(2),
        false,
        None,
        health.clone(),
        false,
    )
    .await
    .expect("spawn_roost");

    let roster = serde_json::json!({
        "roost": "test",
        "nests": [{"name": "usdc"}, {"name": "arb"}],
    });
    let live = serve::LiveRoost::new(serve::compose_roost(
        roster.clone(),
        cursor.states.clone(),
        health.clone(),
    ));
    let handles = roost::RoostHandles {
        live,
        states: cursor.states,
        alert_workers: cursor.alert_workers,
        // Keyed by the nest's declared chain - `scaffold_nest` writes `arbitrum-one`. Getting this
        // wrong is not cosmetic: `unmount` refuses to proceed without a channel for the chain, rather
        // than removing routes while the cursor may still be writing.
        lifecycle: std::collections::HashMap::from([(
            "arbitrum-one".to_string(),
            cursor.lifecycle.clone(),
        )]),
        health,
        roster,
    };
    // The ingest task is deliberately leaked into the handles' lifetime here: the cursor must stay
    // running for the unmount handshake to be answered at a window boundary.
    std::mem::forget(cursor.ingest);
    (handles, tape)
}

/// Drive one GET through the served composition and return its status.
async fn status(live: &serve::LiveRoost, path: &str) -> axum::http::StatusCode {
    use tower::ServiceExt;
    let req = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    live.service().oneshot(req).await.unwrap().status()
}

/// RFC-0027 §6: unmount is a **drain**, and the proof is that the store becomes reopenable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmounting_a_nest_releases_its_store_and_removes_its_routes() {
    let usdc_dir = tempfile::tempdir().unwrap();
    let arb_dir = tempfile::tempdir().unwrap();
    let (mut handles, _tape) = two_nest_roost(usdc_dir.path(), arb_dir.path()).await;

    // Both mounted to begin with.
    assert_eq!(
        status(&handles.live, "/arb/health").await,
        axum::http::StatusCode::OK
    );
    assert_eq!(
        status(&handles.live, "/usdc/health").await,
        axum::http::StatusCode::OK
    );

    // While it is mounted, the store is held - reopening must fail. This is the control: without it,
    // the assertion after the unmount would pass even if redb never locked the file in the first
    // place, and the test would prove nothing.
    let arb_db = arb_dir.path().join("nuthatch.redb");
    assert!(
        Store::open(&arb_db).is_err(),
        "a mounted nest's store must be held open - otherwise this test cannot prove a release"
    );

    handles.unmount("arb").await.expect("unmount");

    // The routes are gone, and the co-tenant is untouched - the whole point of unmounting one nest
    // rather than restarting the roost.
    assert_eq!(
        status(&handles.live, "/arb/health").await,
        axum::http::StatusCode::NOT_FOUND,
        "the unmounted nest's routes must be gone"
    );
    assert_eq!(
        status(&handles.live, "/usdc/health").await,
        axum::http::StatusCode::OK,
        "the co-tenant must keep serving across another nest's unmount"
    );

    // The assertion this test exists for: every holder let go.
    Store::open(&arb_db)
        .expect("after unmount the nest's store must be reopenable - some holder did not drop");
}

/// Unmounting something that is not mounted is a no-op, not an error - so a control plane retrying a
/// command it already delivered does not produce a spurious failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unmounting_an_absent_nest_is_a_no_op() {
    let usdc_dir = tempfile::tempdir().unwrap();
    let arb_dir = tempfile::tempdir().unwrap();
    let (mut handles, _tape) = two_nest_roost(usdc_dir.path(), arb_dir.path()).await;

    handles.unmount("not-mounted").await.expect("no-op");
    assert_eq!(handles.states.len(), 2, "nothing was removed");

    handles.unmount("arb").await.expect("first unmount");
    handles.unmount("arb").await.expect("second is idempotent");
    assert_eq!(handles.states.len(), 1);
}
