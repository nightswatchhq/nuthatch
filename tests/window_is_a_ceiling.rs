//! `--window N` is the widest `eth_getLogs` range asked for, not only where the window starts (#1853).
//! `--window 1000` used to issue 4,000-block requests, because an empty range grows the window 4x.

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::{health::RuntimeHealth, indexer};

const TIP: u64 = 20_000;
const WINDOW: u64 = 1_000;

enum Path {
    TipLoop,
    SealDirect,
    RuntimeCursor,
}

/// Index an empty chain of [`TIP`] blocks with `--window` [`WINDOW`], and return the widest range
/// the source was asked for.
async fn widest_request(path: Path) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let tape = Arc::new(TapeSource::new());
    for b in 0..=TIP {
        tape.insert_block(b, empty_block(b, 0, 1_700_000_000 + b));
    }
    tape.advance_tip_to(TIP);
    tape.advance_finalized_to(TIP);

    let (store, ingest) = match path {
        Path::TipLoop | Path::SealDirect => {
            let rt = indexer::spawn_nest(
                tape.clone(),
                dir.path().to_path_buf(),
                cfg,
                Some(TIP),
                matches!(path, Path::SealDirect),
                1,
                Some(WINDOW),
                false,
                None,
            )
            .await
            .expect("spawn_nest");
            (rt.state.store.clone(), rt.ingest)
        }
        Path::RuntimeCursor => {
            let health = Arc::new(RuntimeHealth::new());
            health.register("usdc", "arbitrum-one");
            let cursor = indexer::spawn_runtime(
                tape.clone(),
                vec![("usdc".to_string(), dir.path().to_path_buf(), cfg)],
                Some(TIP),
                false,
                1,
                Some(WINDOW),
                false,
                None,
                health,
                false,
            )
            .await
            .expect("spawn_runtime");
            (cursor.states[0].1.store.clone(), cursor.ingest)
        }
    };
    let done = wait_until(POLL_TIMEOUT, || {
        store
            .get_meta("last_block")
            .ok()
            .flatten()
            .and_then(|b| b.parse::<u64>().ok())
            .is_some_and(|b| b >= TIP)
    })
    .await;
    ingest.abort();
    assert!(done, "the nest did not reach the tip");

    tape.logs_ranges()
        .iter()
        .map(|(from, to)| to - from + 1)
        .max()
        .expect("the source was asked for logs")
}

/// Each window also re-reads the last [`indexer::FETCH_TAIL_OVERLAP`] blocks of the one before.
fn assert_within_window(widest: u64) {
    assert!(
        widest <= WINDOW + indexer::FETCH_TAIL_OVERLAP,
        "asked for {widest} blocks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tip_loop_never_asks_past_the_window() {
    assert_within_window(widest_request(Path::TipLoop).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seal_direct_backfill_never_asks_past_the_window() {
    assert_within_window(widest_request(Path::SealDirect).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_runtime_cursor_never_asks_past_the_window() {
    assert_within_window(widest_request(Path::RuntimeCursor).await);
}
