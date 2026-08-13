//! MountTable lifecycle end-to-end: unmounting a nest releases everything it held (RFC-0027 §6).
//!
//! The acceptance test that matters here is not "does the route disappear" - that is easy and
//! unconvincing - but **does the nest's redb file actually become free**. `Store` is an `Arc<Database>`
//! cloned three ways at nest construction (the cursor, the alert delivery worker, the serving state),
//! and redb only releases the file when the last clone drops. Reopening the store is therefore a
//! single assertion that proves all three were let go; miss any one and it fails.

mod common;

use std::sync::Arc;

use nuthatch::{health::RuntimeHealth, indexer, runtime, serve, store::Store};

use common::tape::*;

/// A two-nest, one-chain mounts over a scripted tape, wrapped in the driver handles a live mounts keeps.
async fn two_nest_roost(
    roost_dir: &std::path::Path,
    usdc_dir: &std::path::Path,
    arb_dir: &std::path::Path,
) -> (runtime::RuntimeHandles, Arc<TapeSource>) {
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
    let health = Arc::new(RuntimeHealth::new());
    health.register("usdc", "arbitrum-one");
    health.register("arb", "arbitrum-one");

    let cursor = indexer::spawn_runtime(
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
    .expect("spawn_runtime");

    let roster = serde_json::json!({
        "mounts": "test",
        "nests": [{"name": "usdc"}, {"name": "arb"}],
    });
    let live = serve::LiveRuntime::new(serve::compose_runtime(
        roster.clone(),
        cursor.states.clone(),
        health.clone(),
    ));
    let handles = runtime::RuntimeHandles {
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
        mount_ctx: runtime::MountContext {
            dir: roost_dir.to_path_buf(),
            // Un-migrated: no mount records, so resolution stays on the pre-2.0 `nests/<name>` path.
            mounts: Vec::new(),
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
async fn status(live: &serve::LiveRuntime, path: &str) -> axum::http::StatusCode {
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
    // rather than restarting the runtime.
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

    // 1. A name already on the runtime. This is an upgrade (RFC-0020), not a mount.
    let err = handles.mount("usdc", None).await.unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<runtime::MountRefusal>(),
            Some(runtime::MountRefusal::AlreadyMounted(_))
        ),
        "expected AlreadyMounted, got: {err:#}"
    );

    // 2. A nest whose chain the runtime declares no cursor for. Scaffold one on a different chain.
    let other = roost_dir.path().join("nests/elsewhere");
    std::fs::create_dir_all(&other).unwrap();
    let mut cfg = scaffold_nest(&other, "elsewhere", USDC);
    cfg.nest.chain = "base".to_string();
    cfg.nest.chain_id = 8453;
    cfg.save(&other).unwrap();
    let err = handles.mount("elsewhere", None).await.unwrap_err();
    assert!(
        matches!(
            err.downcast_ref::<runtime::MountRefusal>(),
            Some(runtime::MountRefusal::UndeclaredChain { .. })
        ),
        "expected UndeclaredChain, got: {err:#}"
    );

    // 3. A mount that would breach the cursor's ceiling. The budget is a refusal, not a warning -
    //    `CLAUDE.md`'s per-cursor limit stops being a budget the moment a mount may quietly exceed it.
    let third = roost_dir.path().join("nests/third");
    std::fs::create_dir_all(&third).unwrap();
    scaffold_nest(&third, "third", ARB);
    handles.mount_ctx.max_rss_mb = 100; // below even the base cost, so any mount breaches it
    let err = handles.mount("third", None).await.unwrap_err();
    match err.downcast_ref::<runtime::MountRefusal>() {
        Some(runtime::MountRefusal::OverBudget {
            projected_mb,
            ceiling_mb,
            ..
        }) => assert!(
            projected_mb > ceiling_mb,
            "the refusal must carry the numbers an operator needs to act: {projected_mb} vs {ceiling_mb}"
        ),
        other => panic!("expected OverBudget, got: {other:?} / {err:#}"),
    }

    // Every refusal left the runtime exactly as it was.
    assert_eq!(handles.states.len(), 2, "no partial mount was left behind");
}

/// RFC-0027 §5: a lifecycle change must survive a restart.
///
/// `mounts.toml` is the embedded stand-in for a control-plane DB - desired state lives in the same file
/// the static boot path reads. Without this, an unmount would silently come back on the next restart,
/// which is the worst kind of bug because it looks like it worked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lifecycle_change_is_persisted_to_the_mount_table() {
    let roost_dir = tempfile::tempdir().unwrap();
    let usdc_dir = roost_dir.path().join("nests/usdc");
    let arb_dir = roost_dir.path().join("nests/arb");
    std::fs::create_dir_all(&usdc_dir).unwrap();
    std::fs::create_dir_all(&arb_dir).unwrap();

    // A mount table matching the running set, as `dev` would have loaded.
    std::fs::write(
        roost_dir.path().join(nuthatch::runtime::MOUNTS_FILE),
        r#"[runtime]
name = "test"
chain = "arbitrum-one"
chain_id = 42161
rpc_urls = ["http://127.0.0.1:1"]
nests = ["usdc", "arb"]
"#,
    )
    .unwrap();

    let (mut handles, _tape) = two_nest_roost(roost_dir.path(), &usdc_dir, &arb_dir).await;
    handles.unmount("arb").await.expect("unmount");

    let reloaded = nuthatch::runtime::MountTable::load(roost_dir.path())
        .expect("the mount table still parses");
    assert_eq!(
        reloaded.runtime.nests,
        vec!["usdc".to_string()],
        "the unmount must be recorded, or it silently returns on the next restart"
    );
    // Everything else about the manifest survives the rewrite untouched.
    assert_eq!(reloaded.runtime.chain.as_deref(), Some("arbitrum-one"));
    assert_eq!(reloaded.runtime.chain_id, Some(42161));
}

/// #517: `POST /_admin/nests` resolved `nests/<name>/` even in a 2.0 `mounts.toml` runtime, because
/// `mount` only ever looked a nest's `nid` up from its own startup snapshot of `[[mounts]]` - and a
/// nest mounted live for the *first* time, the ordinary case, has no such record yet. `DELETE` already
/// resolved and persisted correctly; this is the missing mount half, plus the persistence half that
/// has to go with it - a live mount not written back to `mounts.toml` "works" until the next restart
/// and then silently disappears, the exact failure that file exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mounting_an_unrecorded_nest_resolves_by_nid_and_persists_its_record() {
    let roost_dir = tempfile::tempdir().unwrap();
    let usdc_nid = "bb22".repeat(16);
    let usdc_dir = runtime::MountTable::data_dir(roost_dir.path(), &usdc_nid);
    std::fs::create_dir_all(&usdc_dir).unwrap();

    // A genuinely 2.0 `mounts.toml`: it already has one recorded mount, which is what distinguishes
    // "migrated but this alias is new" from "not migrated at all" (`mount_ctx.mounts` empty falls
    // back to the pre-2.0 layout by design).
    std::fs::write(
        roost_dir.path().join(runtime::MOUNTS_FILE),
        format!(
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[mounts]]\nalias = \"usdc\"\nnid = \"{usdc_nid}\"\n"
        ),
    )
    .unwrap();

    let tape = Arc::new(TapeSource::new());
    let cfg_u = scaffold_nest(&usdc_dir, "usdc", USDC);
    let health = Arc::new(RuntimeHealth::new());
    health.register("usdc", "arbitrum-one");

    let cursor = indexer::spawn_runtime(
        tape.clone(),
        vec![("usdc".to_string(), usdc_dir.to_path_buf(), cfg_u)],
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
    .expect("spawn_runtime");

    let roster = serde_json::json!({"runtime": "test", "nests": [{"name": "usdc"}]});
    let live = serve::LiveRuntime::new(serve::compose_runtime(
        roster.clone(),
        cursor.states.clone(),
        health.clone(),
    ));
    let mut handles = runtime::RuntimeHandles {
        live,
        states: cursor.states,
        alert_workers: cursor.alert_workers,
        lifecycle: std::collections::HashMap::from([(
            "arbitrum-one".to_string(),
            cursor.lifecycle.clone(),
        )]),
        health,
        roster,
        estimates: std::collections::HashMap::from([("usdc".to_string(), 90)]),
        mount_ctx: runtime::MountContext {
            dir: roost_dir.path().to_path_buf(),
            // Only `usdc` is on record - `gamma` below is exactly the "runtime has never seen this
            // alias before" shape a first-time live mount actually has.
            mounts: vec![runtime::Mount {
                tenant: "default".to_string(),
                alias: "usdc".to_string(),
                nid: usdc_nid.clone(),
                sql: Default::default(),
                queries: Vec::new(),
            }],
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
    std::mem::forget(cursor.ingest);

    // `gamma`'s data already lives at `data/<nid>` - delivered out of band, exactly as #517 describes
    // - but the runtime has never recorded it. Mounting it live must resolve `data/<nid>`, not the
    // pre-2.0 `nests/gamma` this runtime doesn't even have.
    let gamma_nid = "cc33".repeat(16);
    let gamma_dir = runtime::MountTable::data_dir(roost_dir.path(), &gamma_nid);
    std::fs::create_dir_all(&gamma_dir).unwrap();
    scaffold_nest(&gamma_dir, "gamma", ARB);

    handles
        .mount("gamma", Some(runtime::Nid::parse(&gamma_nid).unwrap()))
        .await
        .expect("mounting an unrecorded nest by nid must resolve data/<nid>, not nests/<name>");
    assert!(handles.states.iter().any(|(n, _)| n == "gamma"));

    // The record must be durable - `mounts.toml` is the record RFC-0027 §5 promises survives a
    // restart, and the whole point of fixing the mount half is that it now behaves like the unmount
    // half already did.
    let reloaded = runtime::MountTable::load(roost_dir.path()).expect("mounts.toml still parses");
    let gamma_record = reloaded
        .mounts
        .iter()
        .find(|m| m.alias == "gamma")
        .expect("the live mount must be written back to mounts.toml, not vanish on restart");
    assert_eq!(gamma_record.nid, gamma_nid);
    assert!(
        reloaded.mounts.iter().any(|m| m.alias == "usdc"),
        "the pre-existing mount must survive untouched"
    );
}

/// NIG-185 / the #544 review finding: `nid` from the admin body reached `MountTable::data_dir` with
/// nothing validating it in between, and `Path::join` discards the mount root outright when the
/// argument is absolute or `..`-relative. The consequence that matters is not the 400 by itself - it
/// is that an admitted bad `nid` gets to `persist_mounted_nests` (RFC-0027 §5's "converge on
/// restart" write), and the very next `MountTable::load` calls `validate_mounts`, which has always
/// refused a non-64-hex record. `load` is the single entry point for the whole table, so that is not
/// "one nest fails" - it is every nest in the runtime failing to start, from a call that looked like
/// it worked.
///
/// The traversal shape is what makes this reproducible without any coincidence: `../spare/legacy` is
/// not a NID by any definition, but it is a real, valid, already-scaffolded nest directory once
/// `data_dir` joins it onto the mount root and `..` walks back out - so *with the guard removed* this
/// mount would fully succeed, right up through the cursor handshake, and the record would get
/// written. That is what gives the final assertion teeth: delete the validation and this test does
/// not merely fail to see a 400, it watches `MountTable::load` refuse the file the "successful" call
/// just wrote.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_malformed_nid_is_rejected_before_the_runtime_stops_loading() {
    const TOKEN: &str = "s3cret-admin-token";

    let roost_dir = tempfile::tempdir().unwrap();
    let usdc_nid = "bb22".repeat(16);
    let usdc_dir = runtime::MountTable::data_dir(roost_dir.path(), &usdc_nid);
    std::fs::create_dir_all(&usdc_dir).unwrap();

    std::fs::write(
        roost_dir.path().join(runtime::MOUNTS_FILE),
        format!(
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[mounts]]\nalias = \"usdc\"\nnid = \"{usdc_nid}\"\n"
        ),
    )
    .unwrap();

    // A real, valid nest a traversal-style `nid` can reach *without* being mounted itself - the
    // danger here is not "no directory exists there", it is "one does".
    let spare_dir = roost_dir.path().join("spare/legacy");
    std::fs::create_dir_all(&spare_dir).unwrap();
    scaffold_nest(&spare_dir, "legacy", ARB);
    let traversal_nid = "../spare/legacy";

    let tape = Arc::new(TapeSource::new());
    let cfg_u = scaffold_nest(&usdc_dir, "usdc", USDC);
    let health = Arc::new(RuntimeHealth::new());
    health.register("usdc", "arbitrum-one");

    let cursor = indexer::spawn_runtime(
        tape.clone(),
        vec![("usdc".to_string(), usdc_dir.to_path_buf(), cfg_u)],
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
    .expect("spawn_runtime");

    let roster = serde_json::json!({"runtime": "test", "nests": [{"name": "usdc"}]});
    let live = serve::LiveRuntime::new(serve::compose_runtime(
        roster.clone(),
        cursor.states.clone(),
        health.clone(),
    ));
    let handles = runtime::RuntimeHandles {
        live,
        states: cursor.states,
        alert_workers: cursor.alert_workers,
        lifecycle: std::collections::HashMap::from([(
            "arbitrum-one".to_string(),
            cursor.lifecycle.clone(),
        )]),
        health,
        roster,
        estimates: std::collections::HashMap::from([("usdc".to_string(), 90)]),
        mount_ctx: runtime::MountContext {
            dir: roost_dir.path().to_path_buf(),
            mounts: vec![runtime::Mount {
                tenant: "default".to_string(),
                alias: "usdc".to_string(),
                nid: usdc_nid.clone(),
                sql: Default::default(),
                queries: Vec::new(),
            }],
            sources: std::collections::HashMap::from([(
                "arbitrum-one".to_string(),
                tape.clone() as Arc<dyn nuthatch::source::Source>,
            )]),
            backfill: None,
            seal_direct: false,
            concurrency: 1,
            window_override: Some(2),
            admin_enabled: true,
            admin_token: Some(TOKEN.to_string()),
            max_rss_mb: 2048,
        },
    };
    std::mem::forget(cursor.ingest);

    let handles = Arc::new(tokio::sync::Mutex::new(handles));
    let routes = runtime::lifecycle_routes(handles.clone(), true, Some(TOKEN.to_string()));

    let (status, body) = call(
        &routes,
        "POST",
        &format!("/_admin/nests?token={TOKEN}"),
        None,
        Some(&format!(r#"{{"name":"legacy","nid":"{traversal_nid}"}}"#)),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::BAD_REQUEST,
        "a nid that is not 64 hex characters must be refused, not resolved: {body}"
    );

    assert!(
        !handles
            .lock()
            .await
            .states
            .iter()
            .any(|(n, _)| n == "legacy"),
        "the refused mount must not have taken effect"
    );

    // The assertion with teeth: `mounts.toml` was never touched, so the table the whole runtime
    // depends on still parses and still holds exactly what it held before the call.
    let reloaded =
        runtime::MountTable::load(roost_dir.path()).expect("mounts.toml must still load");
    assert_eq!(reloaded.mounts.len(), 1, "no record for the refused mount");
    assert_eq!(reloaded.mounts[0].alias, "usdc");
    assert_eq!(reloaded.mounts[0].nid, usdc_nid);
}

/// RFC-0027 §2: when `admin_enabled` is false (the default for any non-localhost bind that does not
/// supply `--admin-token`), `lifecycle_routes` must return an empty router - 404 on every path, not
/// 401. The distinction matters: a 401 proves the route exists but rejected the caller; a 404 proves
/// the route was never registered and no handler code ran.
///
/// This is Guard 1 of the two guards named in issue #400. Guard 2 (the credential check on an
/// enabled surface) is covered by `the_lifecycle_routes_demand_the_admin_token_before_they_act`.
///
/// The test includes a premise block that confirms the routes DO exist when `admin_enabled: true`.
/// Without that block the test passes trivially against a `lifecycle_routes` that always returns
/// an empty router, which is exactly the false-green condition the sprint theme names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_admin_surface_exposes_no_lifecycle_routes() {
    const TOKEN: &str = "gate-test-token";

    let roost_dir = tempfile::tempdir().unwrap();
    let usdc_dir = roost_dir.path().join("nests/usdc");
    let arb_dir = roost_dir.path().join("nests/arb");
    std::fs::create_dir_all(&usdc_dir).unwrap();
    std::fs::create_dir_all(&arb_dir).unwrap();
    let (handles, _tape) = two_nest_roost(roost_dir.path(), &usdc_dir, &arb_dir).await;
    let handles = Arc::new(tokio::sync::Mutex::new(handles));

    // Premise: with admin_enabled: true the routes exist - an unauthenticated call gets 401, not
    // 404. This rules out a `lifecycle_routes` that always returns Router::new().
    let enabled_routes = runtime::lifecycle_routes(handles.clone(), true, Some(TOKEN.to_string()));
    let (premise_status, _) = call(
        &enabled_routes,
        "POST",
        "/_admin/nests",
        None,
        Some(r#"{"name":"premise"}"#),
    )
    .await;
    assert_ne!(
        premise_status,
        axum::http::StatusCode::NOT_FOUND,
        "premise: POST /_admin/nests must exist (non-404) when admin is enabled - \
         if this fails the gate test is testing nothing"
    );
    drop(enabled_routes);

    // admin_enabled: false - this is the posture for any localhost bind without an explicit token
    // or any remote bind that omits --admin-token.
    let routes = runtime::lifecycle_routes(handles.clone(), false, None);

    let (mount_status, _) = call(
        &routes,
        "POST",
        "/_admin/nests",
        None,
        Some(r#"{"name":"third"}"#),
    )
    .await;
    assert_eq!(
        mount_status,
        axum::http::StatusCode::NOT_FOUND,
        "POST /_admin/nests must be absent (404), not refused (401), when admin is disabled"
    );

    let (unmount_status, _) = call(&routes, "DELETE", "/_admin/nests/usdc", None, None).await;
    assert_eq!(
        unmount_status,
        axum::http::StatusCode::NOT_FOUND,
        "DELETE /_admin/nests/{{name}} must be absent (404), not refused (401), when admin is disabled"
    );

    // The guard is structural, not positional - no code ran, so both nests must still be present.
    let names: Vec<String> = handles
        .lock()
        .await
        .states
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    assert_eq!(
        names,
        vec!["usdc".to_string(), "arb".to_string()],
        "neither nest must have been affected - the empty router must have returned before any handler ran"
    );
}

/// Drive one request through the lifecycle routes as an HTTP caller would, and return
/// (status, body). Built from `lifecycle_routes` itself rather than from the handlers, so route
/// registration and extractor order are part of what is under test.
async fn call(
    routes: &axum::Router,
    method: &str,
    uri: &str,
    bearer: Option<&str>,
    body: Option<&str>,
) -> (axum::http::StatusCode, String) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    if let Some(t) = bearer {
        req = req.header(axum::http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(b.to_string()))
            .unwrap(),
        None => req.body(axum::body::Body::empty()).unwrap(),
    };
    let resp = routes.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// RFC-0027 §5: mount and unmount are the highest-privilege operations the runtime exposes - they add
/// and remove nests on a live process - and off-localhost they are gated by the admin credential.
///
/// **Nothing exercised that gate.** Every lifecycle test above calls `handles.mount`/`unmount`
/// directly, which is the handler-free path, and `lifecycle_routes` was never constructed by any
/// test in the repo. Deleting either `token_ok` line therefore broke nothing, and an unauthenticated
/// caller could mount a nest on a public bind.
///
/// The assertion is the refusal **and its silence**: the call must be turned away *before* the
/// effect, so the running set is unchanged afterwards. A 401 that mounted the nest anyway would pass
/// a status-code test and fail the only property that matters. Both credential forms are exercised
/// (`?token=` on one route, `Authorization: Bearer` on the other) because both are accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_lifecycle_routes_demand_the_admin_token_before_they_act() {
    const TOKEN: &str = "s3cret-admin-token";

    let roost_dir = tempfile::tempdir().unwrap();
    let usdc_dir = roost_dir.path().join("nests/usdc");
    let arb_dir = roost_dir.path().join("nests/arb");
    std::fs::create_dir_all(&usdc_dir).unwrap();
    std::fs::create_dir_all(&arb_dir).unwrap();
    let (handles, _tape) = two_nest_roost(roost_dir.path(), &usdc_dir, &arb_dir).await;

    // A genuinely mountable nest: same chain, within budget. So with the guard removed the
    // unauthenticated call does not merely reach the handler, it *succeeds* - which is the finding.
    let third = roost_dir.path().join("nests/third");
    std::fs::create_dir_all(&third).unwrap();
    scaffold_nest(&third, "third", ARB);

    let live_service = handles.live.service();
    let handles = Arc::new(tokio::sync::Mutex::new(handles));
    // The posture an off-localhost bind derives: surface on, credential required.
    let routes = runtime::lifecycle_routes(handles.clone(), true, Some(TOKEN.to_string()));

    // 1. An unauthenticated mount is refused, and mounts nothing.
    let (status, _) = call(
        &routes,
        "POST",
        "/_admin/nests",
        None,
        Some(r#"{"name":"third"}"#),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNAUTHORIZED,
        "an unauthenticated POST /_admin/nests must be refused"
    );
    let names: Vec<String> = handles
        .lock()
        .await
        .states
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    assert_eq!(
        names,
        vec!["usdc".to_string(), "arb".to_string()],
        "the refusal must land before the effect - an unauthenticated caller mounted a nest"
    );

    // 2. An unauthenticated unmount is refused, and the nest keeps serving.
    let (status, _) = call(&routes, "DELETE", "/_admin/nests/arb", None, None).await;
    assert_eq!(
        status,
        axum::http::StatusCode::UNAUTHORIZED,
        "an unauthenticated DELETE /_admin/nests/{{name}} must be refused"
    );
    let (status, _) = call(&live_service, "GET", "/arb/health", None, None).await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "the refused unmount must have left the nest serving"
    );

    // 3. Positive controls: with the credential the same two routes reach their handlers, so the 401s
    //    above are the guard talking and not a broken route. `usdc` is already mounted, which is
    //    RFC-0027 §3's AlreadyMounted refusal - it proves the mount logic ran.
    let (status, body) = call(
        &routes,
        "POST",
        &format!("/_admin/nests?token={TOKEN}"),
        None,
        Some(r#"{"name":"usdc"}"#),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::CONFLICT,
        "with the token the mount must reach the handler: {body}"
    );
    // The unmount half, via the header form, on a name that is not mounted: idempotent no-op, so the
    // control costs the fixture nothing.
    let (status, body) = call(
        &routes,
        "DELETE",
        "/_admin/nests/not-mounted",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "with the token the unmount must reach the handler: {body}"
    );
}
