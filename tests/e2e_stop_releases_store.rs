//! A stopped nest has let go of its store: the next open of the same directory succeeds at once
//! (#1764). Stopped while it is still committing windows, because a commit in flight is the holder
//! an abort does not wait for.

mod common;

use std::sync::Arc;

use nuthatch::{config, indexer, store};

use common::tape::*;

const ITERATIONS: usize = 50;
const BLOCKS: u64 = 400;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_nest_can_be_reopened_at_once() {
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=BLOCKS {
        tape.insert_block(
            b,
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(a1.as_str(), a2.as_str(), 1)],
            ),
        );
    }
    tape.advance_tip_to(BLOCKS);

    let mut refused = Vec::new();
    for i in 0..ITERATIONS {
        let dir = tempfile::tempdir().unwrap();
        let cfg = scaffold_nest(dir.path(), "usdc", USDC);
        let rt = indexer::spawn_nest(
            tape.clone(),
            dir.path().to_path_buf(),
            cfg,
            None,
            false,
            1,
            Some(1),
            false,
            None,
        )
        .await
        .expect("spawn_nest");
        let store = rt.state.store.clone();
        let started = wait_until(POLL_TIMEOUT, || {
            store
                .get_meta("last_block")
                .ok()
                .flatten()
                .and_then(|b| b.parse::<u64>().ok())
                .is_some_and(|b| b >= 3)
        })
        .await;
        assert!(started, "iteration {i}: the nest did not start indexing");
        drop(store);
        rt.shutdown().await.expect("the nest stops");
        if let Err(e) = store::Store::open(&dir.path().join(config::DB_FILE)) {
            refused.push(format!("iteration {i}: {e:#}"));
        }
    }
    assert!(
        refused.is_empty(),
        "{} of {ITERATIONS} reopens refused:\n{}",
        refused.len(),
        refused.join("\n")
    );
}
