//! Warm-restart rebuild, end to end (issue #150).
//!
//! Derived views are not persisted - on restart they are rebuilt from a **cold fold** over sealed
//! segments plus a **hot replay** of everything above the sealed watermark. Get the boundary wrong in
//! either direction and the result is silently incorrect rather than loudly broken:
//!
//! - fold a segment that the hot store still holds → every row in it counts **twice**;
//! - skip a range that was sealed and pruned → those rows vanish from the view.
//!
//! `analytics::cold_fold_respects_the_sealed_through_watermark` covers the first at unit level against
//! a hand-built segment. This is the fuller version the audit asked for: run a real nest, really seal
//! and prune, drop it, respawn it on the same directory, and assert the rebuilt balances are identical
//! to a nest that indexed the same chain from scratch. Only the whole pipeline can catch a boundary
//! that is off by one segment.

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::indexer;

/// A block carrying one transfer of `100 * b` from account 1 to account 2.
fn transfer_at(b: u64) -> BlockFixture {
    transfers_block(
        b,
        0,
        1_700_000_000 + b,
        USDC,
        &[(account(1).as_str(), account(2).as_str(), (100 * b) as u128)],
    )
}

/// A tape carrying blocks `1..=10` plus an empty block 11, tip at 11. Block 11 exists so the loop
/// processes a fresh window *after* finality moves, which is what triggers sealing.
fn tape_with_ten_transfers() -> Arc<TapeSource> {
    let tape = Arc::new(TapeSource::new());
    for b in 1..=10u64 {
        tape.insert_block(b, transfer_at(b));
    }
    tape.insert_block(11, empty_block(11, 0, 1_700_000_100));
    tape.advance_tip_to(11);
    tape
}

async fn spawn(dir: &std::path::Path, tape: Arc<TapeSource>) -> indexer::NestRuntime {
    let cfg = scaffold_nest(dir, "usdc", USDC);
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
    .expect("spawn_nest")
}

/// Stop a nest and, crucially, **wait for it to actually be gone**.
///
/// `abort()` only *requests* cancellation - the task's stack, which owns a `Store` handle, lives on
/// until the runtime finishes unwinding it. redb is single-writer and refuses a second open, so
/// respawning before that completes fails with "Database already open". Awaiting the aborted handle is
/// what makes this a genuine restart rather than a race.
async fn shutdown(rt: indexer::NestRuntime) {
    let indexer::NestRuntime {
        state,
        ingest,
        alert_worker,
    } = rt;
    ingest.abort();
    let _ = ingest.await;
    if let Some(w) = alert_worker {
        w.abort();
        let _ = w.await;
    }
    drop(state);
}

/// Balances as a sorted `(address, balance)` list - the comparable shape.
fn balances_of(rt: &indexer::NestRuntime) -> Vec<(String, i128)> {
    rt.state.balances.flush();
    let mut v = rt.state.balances.top(1_000);
    v.sort();
    v
}

/// Wait for the nest to reach `last_block == 11`.
async fn wait_indexed(store: &dyn nuthatch::store::HotStore) -> bool {
    wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some("11")
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_warm_restart_rebuilds_balances_identically_to_a_clean_replay() {
    // ---- The restarted nest: index, seal past finality, drop, respawn on the same dir. ----
    let dir = tempfile::tempdir().unwrap();
    let tape = tape_with_ten_transfers();
    let rt = spawn(dir.path(), tape.clone()).await;
    let store = rt.state.store.clone();
    assert!(
        wait_indexed(&store).await,
        "first run did not reach the tip"
    );

    // Finalize through block 5 so [1,5] seals to Parquet and is pruned from the hot store. This is
    // the split that makes the test meaningful: the rebuild must fold the cold half and replay the
    // hot half, counting every transfer exactly once across the seam.
    tape.advance_finalized_to(5);
    tape.insert_block(12, empty_block(12, 0, 1_700_000_200));
    tape.advance_tip_to(12);
    let sealed = wait_until(POLL_TIMEOUT, || store.sealed_through() >= 5).await;
    assert!(sealed, "range [1,5] did not seal in time");

    let before = balances_of(&rt);
    assert!(!before.is_empty(), "the view must hold balances to compare");
    shutdown(rt).await;
    drop(store);

    // Respawn on the SAME directory - the warm-restart path, with a real sealed/hot split on disk.
    let restarted = spawn(dir.path(), tape.clone()).await;
    let restarted_store = restarted.state.store.clone();
    let caught_up = wait_until(POLL_TIMEOUT, || {
        restarted_store
            .get_meta("last_block")
            .ok()
            .flatten()
            .as_deref()
            == Some("12")
    })
    .await;
    assert!(caught_up, "restarted nest did not resume to the tip");
    let after = balances_of(&restarted);
    shutdown(restarted).await;

    // ---- The reference: a clean nest indexing the same chain from an empty directory. ----
    let clean_dir = tempfile::tempdir().unwrap();
    let clean_tape = tape_with_ten_transfers();
    clean_tape.advance_finalized_to(5);
    clean_tape.insert_block(12, empty_block(12, 0, 1_700_000_200));
    clean_tape.advance_tip_to(12);
    let clean = spawn(clean_dir.path(), clean_tape).await;
    let clean_store = clean.state.store.clone();
    let clean_caught_up = wait_until(POLL_TIMEOUT, || {
        clean_store.get_meta("last_block").ok().flatten().as_deref() == Some("12")
    })
    .await;
    assert!(clean_caught_up, "clean nest did not reach the tip");
    let clean_balances = balances_of(&clean);
    shutdown(clean).await;

    // The property: a restart is invisible in the derived view.
    assert_eq!(
        after, clean_balances,
        "warm-restart balances must equal a clean replay - a mismatch here is the seal/hot boundary \
         being double-counted or dropped"
    );
    // And the restart did not disturb what was already correct before it.
    assert_eq!(
        before, after,
        "the rebuilt view must match the view the nest held before it was restarted"
    );

    // Belt and braces on the actual arithmetic: transfers of 100*b for b in 1..=10 sum to 5500, so the
    // recipient holds exactly +5500 and the sender -5500. A double-counted sealed range [1,5] would
    // show 7000/-7000 (the 1500 counted twice) - the specific corruption this test exists to catch.
    let recipient = account(2).to_ascii_lowercase();
    let got = after
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(&recipient))
        .map(|(_, b)| *b)
        .expect("recipient must hold a balance");
    assert_eq!(got, 5_500, "sum of 100*b for b in 1..=10, counted once");
}

/// The case the sealed-through watermark actually exists for: a crash in the window between "segment
/// written and catalogued" and "watermark advanced + hot rows pruned".
///
/// The test above deliberately does NOT cover this. On the normal path the pruning makes the watermark
/// redundant - sealed rows are gone from hot, so a cold fold that ignored the watermark still counts
/// everything once, and the test passes even with the guard removed (verified by mutation). Only the
/// crash state makes the watermark load-bearing: segments hold [1,5], the hot store *still* holds
/// [1,10], and the watermark is stale. Fold the segments there and blocks 1..=5 count twice.
///
/// The crash state is reconstructed from rows the real pipeline produced - snapshot the hot rows before
/// sealing, then put them back and rewind the watermark - so this exercises the true on-disk shape a
/// `kill -9` leaves, not an invented one.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_between_sealing_and_pruning_does_not_double_count() {
    let dir = tempfile::tempdir().unwrap();
    let tape = tape_with_ten_transfers();
    let rt = spawn(dir.path(), tape.clone()).await;
    let store = rt.state.store.clone();
    assert!(wait_indexed(&store).await, "did not reach the tip");

    // Snapshot the hot rows for [1,5] BEFORE they are sealed and pruned - these are the rows a crash
    // would leave behind, so re-inserting them later reproduces the state exactly.
    let doomed: Vec<String> = store.entities_in_range(1, 5).unwrap();
    assert_eq!(doomed.len(), 5, "blocks 1..=5 should be hot before sealing");

    // Now let the pipeline seal [1,5] and prune it, for real.
    tape.advance_finalized_to(5);
    tape.insert_block(12, empty_block(12, 0, 1_700_000_200));
    tape.advance_tip_to(12);
    assert!(
        wait_until(POLL_TIMEOUT, || store.sealed_through() >= 5).await,
        "range [1,5] did not seal in time"
    );
    shutdown(rt).await;
    drop(store); // redb is single-writer: release our handle before reopening below

    // Reconstruct the crash: segments still hold [1,5] (untouched on disk), the hot rows come back,
    // and the watermark never advanced.
    {
        let s = nuthatch::store::Store::open(&dir.path().join("nuthatch.redb")).unwrap();
        for raw in &doomed {
            let v: serde_json::Value = serde_json::from_str(raw).unwrap();
            let key = nuthatch::store::Store::entity_key(
                v["block_number"].as_u64().unwrap(),
                v["log_index"].as_u64().unwrap_or(0),
            );
            s.put_entity(&key, raw).unwrap();
        }
        s.set_meta("sealed_through", "0").unwrap();
        assert_eq!(
            s.entities_in_range(1, 10).unwrap().len(),
            10,
            "the crash state must hold every row hot AND sealed"
        );
    }

    // Restart into that state and let the rebuild run.
    let restarted = spawn(dir.path(), tape).await;
    let after = balances_of(&restarted);
    shutdown(restarted).await;

    let recipient = account(2).to_ascii_lowercase();
    let got = after
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(&recipient))
        .map(|(_, b)| *b)
        .expect("recipient must hold a balance");
    assert_eq!(
        got, 5_500,
        "every transfer counted exactly once. 7000 here means the cold fold ignored the stale \
         watermark and re-counted the sealed-but-still-hot range [1,5] (which sums to 1500)"
    );
}
