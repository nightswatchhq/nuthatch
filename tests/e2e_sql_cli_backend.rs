//! Which backend `nuthatch sql` picks, driven through the real binary (issue #413).
//!
//! The decision lives in `SqlBackend::open` in `src/main.rs`, so there is no library seam to unit-test
//! it through - these run the compiled `nuthatch` and watch a stand-in for `nuthatch dev` to see
//! whether the fallback fired. That is the point of the issue: the probe used to open the store with
//! `Database::create`, so in a directory with no nest it *made* one, reported a local nest, answered
//! from the empty file it had just written, and never asked the running instance that held the data.
//!
//! All three routings are asserted together, because "absent → HTTP" alone is satisfied by a backend
//! that always chooses HTTP.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::{extract::Query, routing::get, Json, Router};
use serde_json::json;

/// A stand-in for a running `nuthatch dev`: answers `/sql` with one unmistakable row, and counts the
/// requests it was asked, so "the answer came from HTTP" is observed rather than inferred.
struct Instance {
    url: String,
    hits: Arc<AtomicUsize>,
}

/// The marker only the HTTP instance can produce. It cannot appear in a local answer - there is no
/// such column in any nest - so finding it in stdout is proof of where the row came from.
const MARKER: &str = "answered-by-the-running-instance";

async fn serving_instance() -> Instance {
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let app = Router::new().route(
        "/sql",
        get(move |Query(q): Query<HashMap<String, String>>| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Json(json!({
                    "rows": [{ "who": MARKER, "asked": q.get("q") }],
                    "truncated": false,
                }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Instance { url, hits }
}

/// Run `nuthatch sql <query> --dir <dir> --url <url>` and return its exit status with its
/// stdout+stderr.
///
/// The status is returned rather than dropped because without it every assertion here is a *negative*
/// one - "the instance was not asked" - and a backend that was chosen and then failed outright
/// satisfies those just as well as one that worked. See `a_store_that_is_here_and_unheld_is_queried_locally`.
async fn run_sql(dir: &Path, url: &str, query: &str) -> (std::process::ExitStatus, String) {
    let (dir, url, query) = (dir.to_path_buf(), url.to_string(), query.to_string());
    tokio::task::spawn_blocking(move || {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_nuthatch"))
            .args(["sql", &query, "--dir"])
            .arg(&dir)
            .args(["--url", &url])
            .output()
            .expect("running the nuthatch binary");
        (
            out.status,
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    })
    .await
    .unwrap()
}

/// **Absent → HTTP, and no store left behind.** The reported defect, both halves.
///
/// The directory has no nest in it, which is what `--dir .` means whenever the command is run one
/// level up from the nest - the ordinary way to meet this.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_local_store_asks_the_running_instance_and_creates_nothing() {
    let instance = serving_instance().await;
    let empty = tempfile::tempdir().unwrap();

    let (status, output) = run_sql(empty.path(), &instance.url, "SELECT 1 AS n").await;

    assert!(
        status.success(),
        "the fallback must answer, not merely be chosen, got:\n{output}"
    );
    assert!(
        output.contains(MARKER),
        "with no store here the answer must come from the running instance, got:\n{output}"
    );
    assert_eq!(
        instance.hits.load(Ordering::SeqCst),
        1,
        "and the instance must actually have been asked"
    );
    assert!(
        !empty.path().join("nuthatch.redb").exists(),
        "probing for a store must not create one - it left {:?} behind",
        empty.path().join("nuthatch.redb")
    );
}

/// **Present and free → local.** Without this the fix is indistinguishable from "always use HTTP",
/// which would break the offline case the local backend exists for.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_that_is_here_and_unheld_is_queried_locally() {
    let instance = serving_instance().await;
    let nest = tempfile::tempdir().unwrap();
    let db = nest.path().join("nuthatch.redb");
    // A real store, closed again so nothing holds redb's lock.
    drop(nuthatch::store::Store::open(&db).unwrap());

    let (status, output) = run_sql(nest.path(), &instance.url, "SELECT 1 AS n").await;

    // Positive first. The two assertions below are both negative, and a local backend that was chosen
    // and then died satisfies each of them - so on their own they prove *not-HTTP*, not *local*.
    assert!(
        status.success(),
        "the local backend must answer, not merely be chosen, got:\n{output}"
    );
    assert!(
        output.contains('n') && output.contains('1'),
        "and the answer must be the query's, got:\n{output}"
    );
    assert!(
        !output.contains(MARKER),
        "a store that is present and free is queried locally, got:\n{output}"
    );
    assert_eq!(
        instance.hits.load(Ordering::SeqCst),
        0,
        "and the running instance must not have been asked at all"
    );
}

/// **Present but held by `dev` → HTTP.** redb is single-writer; this is the case the original comment
/// described and the one the fix must not regress.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_store_held_by_another_process_falls_back_to_the_instance() {
    let instance = serving_instance().await;
    let nest = tempfile::tempdir().unwrap();
    let db = nest.path().join("nuthatch.redb");
    // Held open for the duration, exactly as `nuthatch dev` holds it.
    let _held = nuthatch::store::Store::open(&db).unwrap();

    let (status, output) = run_sql(nest.path(), &instance.url, "SELECT 1 AS n").await;

    assert!(
        status.success(),
        "the fallback must answer, not merely be chosen, got:\n{output}"
    );
    assert!(
        output.contains(MARKER),
        "a store held by `dev` falls back to the running instance, got:\n{output}"
    );
    assert_eq!(instance.hits.load(Ordering::SeqCst), 1);
}

/// **Absent store, and no instance to fall back to → both halves named.** (#474)
///
/// `absent_store` exists so the connect failure can say more than "is `nuthatch dev` running?" -
/// that alone is only half the truth when the real mistake is the directory. Nothing in the suite
/// named the message itself; it could be reworded into nonsense, or the `Some(db)` arm dropped for
/// the bare `None` message, with every other test here still green, because they all point `--url`
/// at a real `serving_instance`.
///
/// Port 1 is privileged, so nothing in CI binds it - the connection is refused rather than merely
/// slow, without needing to race a listener's shutdown.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_local_store_and_no_instance_names_both_in_the_error() {
    let empty = tempfile::tempdir().unwrap();

    let (status, output) = run_sql(empty.path(), "http://127.0.0.1:1", "SELECT 1 AS n").await;

    assert!(
        !status.success(),
        "there is nothing to answer this query, got:\n{output}"
    );
    let expected_db = empty.path().join("nuthatch.redb");
    assert!(
        output.contains(&format!("no store at {}", expected_db.display())),
        "the message must name the absent store, got:\n{output}"
    );
    assert!(
        output.contains("Is `nuthatch dev` running, and is --dir the nest directory?"),
        "and must ask about both the process and the directory, got:\n{output}"
    );
    assert!(
        !expected_db.exists(),
        "probing for a store must not create one - it left {expected_db:?} behind"
    );
}
