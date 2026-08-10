//! RFC-0021 §2 - **cross-cursor death isolation**, the criterion left uncovered by #356 (issue #387).
//!
//! `#356` covered a cursor that *stalls*: chain A's provider goes dark, A's loop retries forever, and
//! B carries on. This covers the harder half - a cursor that **dies**. Chain A takes a reorg below its
//! sealed watermark, which is a finality violation it cannot repair, so its only nest is terminally
//! quarantined, `runtime_index_loop` runs out of live nests and bails (`indexer.rs:1092`), and its task
//! ends with an error. `supervise_cursors` must absorb that death: retire A's cursor, leave B's alone,
//! and keep the runtime up.
//!
//! **What this adds over `runtime::tests::a_dead_cursor_does_not_take_its_siblings_down`.** That test
//! hands the supervisor two `tokio::spawn`ed futures - one that returns `Err` immediately, one that
//! ticks a counter. It proves the supervisor's own logic and nothing about the runtime it supervises:
//! that a real fault *escalates* to a cursor exit at all is assumed, and "still indexing" is an
//! `AtomicU64`, not a block. Here both cursors are real - `spawn_runtime`, real stores, a real chain,
//! a real fault - so the whole chain is under test: a nest-level terminal fault becomes a cursor exit,
//! the supervisor sees it, and the sibling indexes *blocks* it had not reached when its neighbour died.
//!
//! **On vacuity.** "B keeps working" passes trivially when nothing has gone wrong, so it is not what
//! this rests on. The load-bearing assertions are the ones that can only hold together:
//!
//! - chain A's nest is quarantined *for a finality violation* - the fault really fired, so there is a
//!   death to isolate;
//! - `nuthatch_cursor_live{chain="base"}` goes to 0 while `arbitrum-one` stays 1. That gauge is written
//!   only by `RuntimeHealth::quarantine_cursor`, which only `supervise_cursors` calls, so it is the
//!   supervisor's own act - and it proves the nest-level fault escalated all the way to a cursor exit;
//! - the supervisor **did not return** while B was alive - returning is what ends the runtime - and B
//!   reached a tip it had not reached at the moment A died.
//!
//! The supervisor is raced against those conditions rather than waited on for a fixed period: if the
//! blast radius widens (the pre-RFC-0026 `select_all`, a blanket abort, `--fail-fast`'s coupling), the
//! supervisor wins the race and the test fails at the moment it happens.
//!
//! **Why the two nests sit on different chains.** A cursor is quarantined *by chain name*, so two nests
//! scaffolded on the default `arbitrum-one` would share one cursor entry in `RuntimeHealth` - and the
//! surviving nest would report itself quarantined because of its neighbour, turning the isolation
//! assertion into a false failure and the gauge assertions into a single value read twice.

mod common;

use std::sync::Arc;

use nuthatch::indexer;

use common::tape::*;

/// The dying cursor's chain and the surviving one's. Both are `FinalizedTag` chains, so `finalized()`
/// drives sealing identically on each.
const DYING: &str = "base";
const SURVIVING: &str = "arbitrum-one";

/// A canonical block `b` (variant 0): one USDC transfer, value `100*b`.
fn canonical_block(b: u64) -> BlockFixture {
    transfers_block(
        b,
        0,
        1_700_000_000 + b,
        USDC,
        &[(account(1).as_str(), account(2).as_str(), (100 * b) as u128)],
    )
}

/// A replacement block `b` (variant 1 → distinct hash), so the reorg is a real divergence.
fn replacement_block(b: u64) -> BlockFixture {
    transfers_block(
        b,
        1,
        1_700_000_500 + b,
        USDC,
        &[(account(3).as_str(), account(4).as_str(), (7_000 + b) as u128)],
    )
}

type Ingests = Vec<(String, tokio::task::JoinHandle<anyhow::Result<()>>)>;

/// One live cursor hosting a single nest `name` on `chain`, sharing `health` with its siblings exactly
/// as `runtime::dev` does, indexed through `tip`.
async fn spawn_cursor(
    dir: &std::path::Path,
    name: &str,
    chain: &str,
    tape: Arc<TapeSource>,
    health: Arc<nuthatch::health::RuntimeHealth>,
    tip: u64,
) -> indexer::ChainCursor {
    let cfg = scaffold_nest_on_chain(dir, name, USDC, chain);
    health.register(name, &cfg.nest.chain);
    let cursor = indexer::spawn_runtime(
        tape,
        vec![(name.to_string(), dir.to_path_buf(), cfg)],
        None,
        false,
        1,
        Some(2),
        false,
        None,
        health,
        // Production's default. `--fail-fast` deliberately couples cursor fate (`runtime.rs:1128`), so
        // it is the one configuration under which this isolation property is not expected to hold.
        false,
    )
    .await
    .expect("spawn_runtime");
    assert!(
        at_block(&cursor.states[0].1.store, tip).await,
        "nest {name} did not index to block {tip} in time"
    );
    cursor
}

async fn at_block(store: &Arc<dyn nuthatch::store::HotStore>, n: u64) -> bool {
    let want = n.to_string();
    wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some(want.as_str())
    })
    .await
}

fn last_block(store: &Arc<dyn nuthatch::store::HotStore>) -> Option<String> {
    store.get_meta("last_block").ok().flatten()
}

/// The operator-facing liveness gauge for one chain's cursor: `1` while it runs, `0` once the
/// supervisor has retired it.
fn cursor_live(health: &nuthatch::health::RuntimeHealth, chain: &str) -> Option<u8> {
    let needle = format!("nuthatch_cursor_live{{chain=\"{chain}\"}} ");
    health
        .render_metrics()
        .lines()
        .find_map(|l| l.strip_prefix(needle.as_str())?.trim().parse().ok())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_cursor_leaves_its_sibling_indexing_and_the_runtime_up() {
    const FORK: u64 = 5;

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let tape_a = Arc::new(TapeSource::new());
    let tape_b = Arc::new(TapeSource::new());
    for b in 1..=10u64 {
        tape_a.insert_block(b, canonical_block(b));
        tape_b.insert_block(b, canonical_block(b));
    }
    tape_a.advance_tip_to(10);
    tape_b.advance_tip_to(10);

    // Two cursors, one per chain, one shared `RuntimeHealth` - the multichain runtime's shape, and the
    // genuinely shared state, kept in the test's path rather than designed out of it.
    let health = Arc::new(nuthatch::health::RuntimeHealth::new());
    let cursor_a = spawn_cursor(
        dir_a.path(),
        "nest-a",
        DYING,
        tape_a.clone(),
        health.clone(),
        10,
    )
    .await;
    let cursor_b = spawn_cursor(
        dir_b.path(),
        "nest-b",
        SURVIVING,
        tape_b.clone(),
        health.clone(),
        10,
    )
    .await;
    let store_a = cursor_a.states[0].1.store.clone();
    let store_b = cursor_b.states[0].1.store.clone();
    assert_eq!(
        (cursor_live(&health, DYING), cursor_live(&health, SURVIVING)),
        (Some(1), Some(1)),
        "both cursors must start live, or the gauge assertions below prove nothing"
    );

    // Seal chain A through 8 - past the fork that is coming - so the reorg lands below the sealed
    // watermark and is unrepairable rather than routine. Two more blocks give the loop a window to
    // process and seal in.
    tape_a.advance_finalized_to(8);
    tape_a.insert_block(11, empty_block(11, 0, 1_700_000_111));
    tape_a.insert_block(12, empty_block(12, 0, 1_700_000_112));
    tape_a.advance_tip_to(12);
    assert!(
        wait_until(POLL_TIMEOUT, || store_a.sealed_through() >= 8).await,
        "chain A did not seal [1,8] in time - without a sealed watermark above the fork the reorg \
         below it is routine and no cursor dies"
    );

    // Chain B is given blocks it has NOT yet indexed, so its progress after A's death is forward
    // motion rather than the survival of a chain that had nothing left to do.
    let b_when_a_dies = last_block(&store_b);
    for b in 11..=20u64 {
        tape_b.insert_block(b, canonical_block(b));
    }
    tape_b.advance_tip_to(20);

    // Kill cursor A: a reorg at block 5, below its sealed watermark of 8.
    let replacement: Vec<BlockFixture> = ((FORK + 1)..=12).map(replacement_block).collect();
    tape_a.reorg(FORK, replacement);

    // The live set, exactly as `runtime::dev` assembles it before handing it to the supervisor: keyed
    // by chain, holding each cursor's real ingest handle.
    let mut ingests: Ingests = vec![
        (DYING.to_string(), cursor_a.ingest),
        (SURVIVING.to_string(), cursor_b.ingest),
    ];
    // Boxed rather than `tokio::pin!`ed so the borrow of `ingests` can be released before the set is
    // inspected below.
    let mut supervisor = Box::pin(nuthatch::runtime::supervise_cursors(
        &mut ingests,
        &health,
        false,
    ));

    // The race. The supervisor returning at all - with any value - ends the runtime, and it must not
    // do that while chain B is indexing. Racing rather than sleeping means a widened blast radius
    // fails here, at the moment it happens, rather than being inferred from a state read afterwards.
    let converged = tokio::select! {
        r = &mut supervisor => panic!(
            "the supervisor returned ({r:?}) while chain B was still indexing - one cursor's death \
             ended the runtime, which is the blast radius RFC-0021 forbids"
        ),
        ok = wait_until(POLL_TIMEOUT, || {
            cursor_live(&health, DYING) == Some(0)
                && last_block(&store_b).as_deref() == Some("20")
        }) => ok,
    };
    drop(supervisor);

    assert!(
        converged,
        "chain A's cursor must be retired (gauge {:?}) while chain B indexes on to its own tip 20 \
         (it is at {:?}, and was at {b_when_a_dies:?} when A died)",
        cursor_live(&health, DYING),
        last_block(&store_b)
    );
    assert_ne!(
        last_block(&store_b),
        b_when_a_dies,
        "chain B must have advanced past where it stood when A died, or its survival proves nothing"
    );

    // The surviving cursor is still reported live: the gauge distinguishes the two, so the assertion
    // above is not just "some cursor died".
    assert_eq!(
        cursor_live(&health, SURVIVING),
        Some(1),
        "chain B's cursor must still be reported live"
    );

    // Non-vacuity: the fault really fired, and it is the one this test set up.
    let (a_health, a_detail) = health.json_for("nest-a");
    assert_eq!(
        a_health, "quarantined",
        "chain A's nest must be quarantined - if it is still indexing, no cursor died and there is \
         nothing to isolate"
    );
    let a_reason = a_detail
        .as_ref()
        .and_then(|d| d["reason"].as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        a_reason.contains("finality") || a_reason.contains("sealed"),
        "chain A must be down for the finality violation this test caused, not something else: \
         {a_reason}"
    );

    // The dead cursor is retired from the live set and the healthy one is untouched - the supervisor
    // must not abort a sibling.
    assert_eq!(
        ingests.len(),
        1,
        "exactly one cursor must be retired; the live set holds {:?}",
        ingests.iter().map(|(c, _)| c).collect::<Vec<_>>()
    );
    assert_eq!(
        ingests[0].0, SURVIVING,
        "the surviving cursor must be chain B's"
    );
    assert!(
        !ingests[0].1.is_finished(),
        "chain B's cursor must still be running after its sibling died"
    );

    // And B is not merely alive but healthy, while the runtime as a whole correctly reports that it
    // is not fully indexing.
    assert_eq!(
        health.json_for("nest-b").0,
        "indexing",
        "chain B's nest must stay healthy when a sibling cursor dies"
    );
    assert!(
        !health.all_indexing(),
        "a runtime with a dead cursor is not ready, however healthy its survivors"
    );

    // Chain A's nest keeps serving the state it had rather than losing it (RFC-0026 §6).
    assert!(
        last_block(&store_a).is_some(),
        "chain A's nest must still serve its last indexed state"
    );

    for (_, h) in &ingests {
        h.abort();
    }
    for (_, w) in cursor_a
        .alert_workers
        .into_iter()
        .chain(cursor_b.alert_workers)
    {
        w.abort();
    }
}
