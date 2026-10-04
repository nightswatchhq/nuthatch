//! `--window N` is the widest `eth_getLogs` range asked for, not only where the window starts (#1853).
//! `--window 1000` used to issue 4,000-block requests, because an empty range grows the window 4x.

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::indexer;

const TIP: u64 = 20_000;
const WINDOW: u64 = 1_000;

/// Index an empty chain of [`TIP`] blocks with `--window` [`WINDOW`], and return the widest range
/// the source was asked for, less the blocks each window re-reads from the one before.
async fn widest_request(seal_direct: bool) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let tape = Arc::new(TapeSource::new());
    for b in 0..=TIP {
        tape.insert_block(b, empty_block(b, 0, 1_700_000_000 + b));
    }
    tape.advance_tip_to(TIP);
    tape.advance_finalized_to(TIP);

    let rt = indexer::spawn_nest(
        tape.clone(),
        dir.path().to_path_buf(),
        cfg,
        Some(TIP),
        seal_direct,
        1,
        Some(WINDOW),
        false,
        None,
    )
    .await
    .expect("spawn_nest");
    let store = rt.state.store.clone();
    let done = wait_until(POLL_TIMEOUT, || {
        store
            .get_meta("last_block")
            .ok()
            .flatten()
            .and_then(|b| b.parse::<u64>().ok())
            .is_some_and(|b| b >= TIP)
    })
    .await;
    rt.ingest.abort();
    assert!(done, "the nest did not reach the tip");

    tape.logs_ranges()
        .iter()
        .map(|(from, to)| to - from + 1)
        .max()
        .expect("the source was asked for logs")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tip_loop_never_asks_past_the_window() {
    let widest = widest_request(false).await;
    assert!(
        widest <= WINDOW + indexer::FETCH_TAIL_OVERLAP,
        "asked for {widest} blocks"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seal_direct_backfill_never_asks_past_the_window() {
    let widest = widest_request(true).await;
    assert!(
        widest <= WINDOW + indexer::FETCH_TAIL_OVERLAP,
        "asked for {widest} blocks"
    );
}
