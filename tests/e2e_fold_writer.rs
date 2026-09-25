//! RFC-0059 S2, end to end: the seal loop's checkpoint writer in a running nest. Blocks seal through
//! the real tip path, the writer checkpoints what sealed, and a restart after a writer killed mid-way
//! rewrites exactly the checkpoint it lost.

mod common;

use std::sync::Arc;

use nuthatch::{folds, indexer, seal};

use common::tape::*;

const FOLD: &str = "SELECT CAST(coalesce((SELECT max(n) FROM transfers__carry), 0) + count(*) AS UBIGINT) AS n FROM ";
const DECL: &str =
    "[[fold]]\nname = \"transfers\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n";

fn declare_fold(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("folds")).unwrap();
    std::fs::write(
        dir.join("folds/transfers.sql"),
        format!("{FOLD}{}", transfer_table("usdc")),
    )
    .unwrap();
    std::fs::write(dir.join("folds/folds.toml"), DECL).unwrap();
}

async fn spawn(dir: &std::path::Path, tape: Arc<TapeSource>) -> indexer::NestRuntime {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    declare_fold(dir);
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
    .expect("spawn_nest with folds/")
}

/// Stop the nest and wait for it, so the store's lock and the writer's thread are both released.
async fn stop(rt: indexer::NestRuntime) {
    rt.ingest.abort();
    let _ = rt.ingest.await;
    if let Some(w) = rt.alert_worker {
        w.abort();
        let _ = w.await;
    }
    drop(rt.state);
}

fn writer(rt: &indexer::NestRuntime) -> Arc<folds::Writer> {
    rt.state
        .folds
        .clone()
        .expect("a nest with folds/ runs a writer")
}

/// The fold's value at `n`, read from its checkpoint without the store the runtime holds.
fn counted_at(dir: &std::path::Path, n: u64) -> u64 {
    let set = folds::FoldSet::load(dir, &[]).unwrap();
    let s = set
        .read_at(dir, &[], &nuthatch::analytics::HotRows::new(), n, n)
        .unwrap();
    s.rows("transfers").unwrap()[0]["n"].as_u64().unwrap()
}

fn sealed_rows(dir: &std::path::Path) -> u64 {
    seal::load_manifest(dir).unwrap().tables[&transfer_table("usdc")]
        .iter()
        .map(|s| s.rows as u64)
        .sum()
}

fn log_path(dir: &std::path::Path) -> std::path::PathBuf {
    let hashes: Vec<_> = std::fs::read_dir(dir.join(folds::CHECKPOINTS_DIR))
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(hashes.len(), 1, "one fold, one checkpoint directory");
    hashes[0].path().join("manifest.json")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_seal_loop_checkpoints_what_it_seals_and_a_restart_recovers_a_lost_write() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=10u64 {
        let mut fx = transfers_block(
            b,
            0,
            1_700_000_000 + b,
            USDC,
            &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
        );
        if b == 5 {
            pad_address_to_seal(&mut fx, USDC, 4);
        }
        tape.insert_block(b, fx);
    }
    tape.advance_tip_to(10);

    let rt = spawn(dir.path(), tape.clone()).await;
    let store = rt.state.store.clone();
    let w = writer(&rt);
    assert!(
        wait_until(POLL_TIMEOUT, || {
            store.get_meta("last_block").ok().flatten().as_deref() == Some("10")
        })
        .await
    );
    assert_eq!(w.status().checkpointed_through, 0, "nothing has sealed");

    tape.advance_finalized_to(5);
    tape.insert_block(11, empty_block(11, 0, 1_700_000_100));
    tape.advance_tip_to(11);
    assert!(wait_until(SEAL_POLL_TIMEOUT, || store.sealed_through() >= 5).await);
    assert!(
        wait_until(SEAL_POLL_TIMEOUT, || w.status().checkpointed_through >= 5).await,
        "the writer did not checkpoint the sealed range: {:?}",
        w.status()
    );
    let status = w.status();
    assert_eq!(
        (status.lag_blocks, status.fault.clone()),
        (0, None),
        "{status:?}"
    );
    let through = status.checkpointed_through;
    drop((w, store));
    stop(rt).await;

    let expected = sealed_rows(dir.path());
    assert_eq!(
        expected,
        indexer::SEAL_DIRECT_BATCH as u64,
        "blocks 1-5 are padded to one batch"
    );
    assert_eq!(counted_at(dir.path(), through), expected);

    // Kill mid-write: the Parquet file landed, the log never named it.
    let path = log_path(dir.path());
    let intact = std::fs::read(&path).unwrap();
    let mut log: serde_json::Value = serde_json::from_slice(&intact).unwrap();
    let lost = log["checkpoints"].as_array_mut().unwrap().pop().unwrap();
    assert_eq!(lost["block"], through);
    std::fs::write(&path, serde_json::to_vec(&log).unwrap()).unwrap();

    let rt = spawn(dir.path(), tape.clone()).await;
    let w = writer(&rt);
    assert!(
        wait_until(SEAL_POLL_TIMEOUT, || w.status().checkpointed_through
            >= through)
        .await,
        "the restarted writer did not rebuild the lost checkpoint: {:?}",
        w.status()
    );
    drop(w);
    stop(rt).await;
    let rebuilt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        rebuilt["checkpoints"],
        serde_json::from_slice::<serde_json::Value>(&intact).unwrap()["checkpoints"],
        "the rebuilt checkpoint is the one that was lost"
    );
    assert_eq!(counted_at(dir.path(), through), expected);
}
