//! Sealed segments must not depend on how the range was fetched (RFC-0028 §4).
//!
//! A segment's content address is its bytes, and its bytes include which rows it holds. Until this
//! test existed, a segment ended wherever the *fetch window* happened to stop - so two operators
//! indexing the same contract over the same range with different `--window` produced segments with
//! different boundaries, different bytes and different hashes. They did not dedupe.
//!
//! Nothing was incorrect - the rows were identical either way - but "same inputs, same
//! content-addressed output" was quietly conditional on matching RPC tuning, which is not what
//! `CLAUDE.md` claims and is load-bearing for RFC-0019 bundle dedup and RFC-0020 segment reuse.

mod common;

use std::sync::Arc;

use nuthatch::{indexer, seal};

use common::tape::*;

/// Backfill `[1, 60]` of an identical tape with the given `getLogs` window, and return the sealed
/// segment hashes for the transfer table.
async fn seal_with_window(dir: &std::path::Path, window: u64) -> Vec<String> {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    // Enough rows to cross `SEAL_DIRECT_BATCH` (20,000) at least once, because the whole point is the
    // *cut*: with a small fixture both windows would produce a single segment covering the whole range
    // and the test would pass no matter what the flush logic did. Row counts per block are deliberately
    // uneven, so the threshold lands mid-block rather than tidily on a boundary.
    for b in 1..=60u64 {
        let n = 350 + (b % 7);
        let transfers: Vec<(&str, &str, u128)> = (0..n)
            .map(|i| (a1.as_str(), a2.as_str(), (100 * b + i) as u128))
            .collect();
        tape.insert_block(
            b,
            transfers_block(b, 0, 1_700_000_000 + b, USDC, &transfers),
        );
    }
    tape.advance_tip_to(60);
    tape.advance_finalized_to(60);

    let registry = nuthatch::registry::from_nest(dir, &cfg).expect("registry");
    let addresses: Vec<String> = cfg.contracts.iter().map(|c| c.address.clone()).collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    indexer::backfill_direct(
        tape.as_ref(),
        &registry,
        dir,
        &addresses,
        &topic0s,
        &[],
        None,
        0,
        1,
        60,
        window,
        true,
    )
    .await
    .expect("backfill_direct");

    let manifest = seal::load_manifest(dir).expect("manifest");
    manifest
        .tables
        .get(&transfer_table("usdc"))
        .map(|segs| segs.iter().map(|s| s.hash.clone()).collect())
        .unwrap_or_default()
}

/// The headline acceptance test of RFC-0028 §4. **This failed before the fix.**
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segments_are_identical_however_the_range_was_fetched() {
    let wide = tempfile::tempdir().unwrap();
    let narrow = tempfile::tempdir().unwrap();

    // Two operators, same chain, same range, wildly different RPC tuning.
    let wide_hashes = seal_with_window(wide.path(), 50).await;
    let narrow_hashes = seal_with_window(narrow.path(), 3).await;

    assert!(
        !wide_hashes.is_empty(),
        "the fixture must actually seal something, or this proves nothing"
    );
    assert_eq!(
        wide_hashes, narrow_hashes,
        "segment content addresses must not depend on the fetch window - otherwise two operators \
         indexing identical data produce segments that refuse to dedupe"
    );
}

/// Re-running the same window twice must also agree, which is the weaker property the project already
/// claimed. Kept alongside so a regression tells you *which* invariant broke.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn segments_are_identical_across_two_runs_of_the_same_window() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    assert_eq!(
        seal_with_window(a.path(), 7).await,
        seal_with_window(b.path(), 7).await
    );
}
