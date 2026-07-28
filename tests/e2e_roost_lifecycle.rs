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
    roost_dir: &std::path::Path,
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
        estimates: std::collections::HashMap::from([
            ("usdc".to_string(), 90),
            ("arb".to_string(), 90),
        ]),
        mount_ctx: roost::MountContext {
            dir: roost_dir.to_path_buf(),
            sources: std::collections::HashMap::from([(
                "arbitrum-one".to_string(),
                tape.clone() as Arc<dyn nuthatch::source::Source>,
            )]),
            backfill: None,
            seal_direct: false,
            concurrency: 1,
            window_override: Some(2),
            admin_enabled: false,
            admin_token: None,
            max_rss_mb: 2048,
        },
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
    let (mut handles, _tape) =
        two_nest_roost(usdc_dir.path(), usdc_dir.path(), arb_dir.path()).await;

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
    let (mut handles, _tape) =
        two_nest_roost(usdc_dir.path(), usdc_dir.path(), arb_dir.path()).await;

    handles.unmount("not-mounted").await.expect("no-op");
    assert_eq!(handles.states.len(), 2, "nothing was removed");

    handles.unmount("arb").await.expect("first unmount");
    handles.unmount("arb").await.expect("second is idempotent");
    assert_eq!(handles.states.len(), 1);
}

/// RFC-0027 §3: the three admission refusals, each decided **before** any work is done - no store
/// opened, no block fetched, nothing left behind by a rejected mount.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mount_is_refused_for_a_taken_name_an_undeclared_chain_or_a_breached_budget() {
    let roost_dir = tempfile::tempdir().unwrap();
    let usdc_dir = roost_dir.path().join("nests/usdc");
    let arb_dir = roost_dir.path().join("nests/arb");
    std::fs::create_dir_all(&usdc_dir).unwrap();
    std::fs::create_dir_all(&arb_dir).unwrap();
    let (mut handles, _tape) = two_nest_roost(roost_dir.path(), &usdc_dir, &arb_dir).await;

    // 1. A name already on the roost. This is an upgrade (RFC-0020), not a mount.
    let err = handles.mount("usdc").await.unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<roost::MountRefusal>(),
            Some(roost::MountRefusal::AlreadyMounted(_))
        ),
        "expected AlreadyMounted, got: {err:#}"
    );

    // 2. A nest whose chain the roost declares no cursor for. Scaffold one on a different chain.
    let other = roost_dir.path().join("nests/elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let mut cfg = scaffold_nest(&other, "elsewhere", USDC);
    cfg.nest.chain = "base".to_string();
    cfg.nest.chain_id = 8453;
    cfg.save(&other).unwrap();
    let err = handles.mount("elsewhere").await.unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<roost::MountRefusal>(),
            Some(roost::MountRefusal::UndeclaredChain { .. })
        ),
        "expected UndeclaredChain, got: {err:#}"
    );

    // 3. A mount that would breach the cursor's ceiling. The budget is a refusal, not a warning -
    //    `CLAUDE.md`'s per-cursor limit stops being a budget the moment a mount may quietly exceed it.
    let third = roost_dir.path().join("nests/third");
    std::fs::create_dir_all(&third).unwrap();
    scaffold_nest(&third, "third", ARB);
    handles.mount_ctx.max_rss_mb = 100; // below even the base cost, so any mount breaches it
    let err = handles.mount("third").await.unwrap_err();
    match err.downcast_ref::<roost::MountRefusal>() {
        Some(roost::MountRefusal::OverBudget {
            projected_mb,
            ceiling_mb,
            ..
        }) => assert!(
            projected_mb > ceiling_mb,
            "the refusal must carry the numbers an operator needs to act: {projected_mb} vs {ceiling_mb}"
        ),
        other => panic!("expected OverBudget, got: {other:?} / {err:#}"),
    }

    // Every refusal left the roost exactly as it was.
    assert_eq!(handles.states.len(), 2, "no partial mount was left behind");
}
