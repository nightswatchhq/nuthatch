//! RFC-0047 C4 / #1225 - resource governance is config over the existing DuckDB walls.
//!
//! Defaults, the startup refusal, and the unconfigured path all drive `spawn_nest`, not a
//! hand-built `AppState`. Env is process-global, so one lock for every read or write.

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::analytics_budget::{
    AnalyticsConfig, ENV_INGESTION_RESERVATION, ENV_MEMORY_LIMIT, ENV_TEMP_DIRECTORY, ENV_THREADS,
};
use nuthatch::indexer;
use nuthatch::serve::SQL_MAX_CONCURRENCY;

fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

struct EnvRestore {
    key: &'static str,
    prev: Option<String>,
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        match self.prev.as_deref() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[must_use]
fn install(key: &'static str, value: Option<&str>) -> EnvRestore {
    let prev = std::env::var(key).ok();
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
    EnvRestore { key, prev }
}

async fn spawn_under(dir: &std::path::Path) -> anyhow::Result<nuthatch::indexer::NestRuntime> {
    let tape = Arc::new(TapeSource::new());
    tape.insert_block(1, empty_block(1, 0, 1_700_000_000));
    tape.advance_tip_to(1);
    let cfg = scaffold_nest(dir, "budget", USDC);
    indexer::spawn_nest(
        tape,
        dir.to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn unconfigured_spawn_nest_behaves_as_today() {
    let _g = env_lock().lock().await;
    let _a = install(ENV_MEMORY_LIMIT, None);
    let _b = install(ENV_THREADS, None);
    let _c = install(ENV_INGESTION_RESERVATION, None);
    let _d = install(ENV_TEMP_DIRECTORY, None);
    let _e = install("NUTHATCH_SQL_MAX_CONCURRENCY", None);

    let dir = tempfile::tempdir().unwrap();
    let rt = spawn_under(dir.path())
        .await
        .expect("unconfigured nest must start");
    assert_eq!(
        rt.state.sql_gate.available_permits(),
        SQL_MAX_CONCURRENCY,
        "unconfigured permit count is still 2"
    );
    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;

    let cfg = nuthatch::analytics_budget::from_env();
    assert_eq!(cfg, AnalyticsConfig::default());
}

#[tokio::test(flavor = "multi_thread")]
async fn over_budget_memory_is_refused_by_spawn_nest() {
    let _g = env_lock().lock().await;
    let _a = install(ENV_MEMORY_LIMIT, Some("2048MB"));
    let _b = install(ENV_INGESTION_RESERVATION, None);
    let _c = install("NUTHATCH_SQL_MAX_CONCURRENCY", None);

    let dir = tempfile::tempdir().unwrap();
    let err = match spawn_under(dir.path()).await {
        Ok(_) => panic!("2 × 2048 MB plus the derived ingest floor must not start"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("analytics.memory_limit"),
        "must name analytics.memory_limit: {err}"
    );
    assert!(
        err.contains("ingestion_reservation"),
        "must name ingestion_reservation: {err}"
    );
    assert!(
        err.contains(ENV_MEMORY_LIMIT),
        "must name the env key: {err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn raising_permits_at_default_memory_is_refused_by_spawn_nest() {
    let _g = env_lock().lock().await;
    let _a = install(ENV_MEMORY_LIMIT, None);
    let _b = install("NUTHATCH_SQL_MAX_CONCURRENCY", Some("4"));

    let dir = tempfile::tempdir().unwrap();
    let err = match spawn_under(dir.path()).await {
        Ok(_) => panic!("4 × 512 MB plus the derived ingest floor must not start"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("NUTHATCH_SQL_MAX_CONCURRENCY"), "{err}");
    assert!(err.contains("analytics.memory_limit"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fitting_override_is_accepted_by_spawn_nest() {
    let _g = env_lock().lock().await;
    let _a = install(ENV_MEMORY_LIMIT, Some("256MB"));
    let _b = install("NUTHATCH_SQL_MAX_CONCURRENCY", Some("4"));

    let dir = tempfile::tempdir().unwrap();
    let rt = spawn_under(dir.path())
        .await
        .expect("4 × 256 + 1024 = 2048 must start");
    assert_eq!(rt.state.sql_gate.available_permits(), 4);
    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;
}

#[test]
fn custom_temp_directory_still_gets_private_instance_dirs() {
    let _g = env_lock().blocking_lock();
    let parent = tempfile::tempdir().unwrap();
    let _a = install(ENV_TEMP_DIRECTORY, Some(parent.path().to_str().unwrap()));
    let _b = install(ENV_MEMORY_LIMIT, None);

    let dir = tempfile::tempdir().unwrap();
    nuthatch::analytics::query(dir.path(), "SELECT 1").unwrap();

    let kids: Vec<_> = std::fs::read_dir(parent.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("nuthatch-duckdb-"))
        .collect();
    assert!(
        !kids.is_empty(),
        "DuckDB spill must land under the operator parent as nuthatch-duckdb-{{pid}}-{{seq}}, got {kids:?}"
    );
    assert!(
        kids.iter()
            .all(|n| n.starts_with(&format!("nuthatch-duckdb-{}-", std::process::id()))),
        "must not regress #1165: {kids:?}"
    );
}
