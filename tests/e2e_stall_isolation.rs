//! RFC-0021 §2 - **cross-cursor stall isolation**, the fourth testing criterion and the one that had
//! no coverage.
//!
//! Sibling of `e2e_reorg::reorg_on_one_chain_leaves_the_other_untouched`, and deliberately the same
//! shape - but a reorg is a rare event we go looking for, whereas a dead RPC endpoint is a Tuesday.
//! Here chain A's provider goes dark (every `Source` call fails, which is what `escalate_stall`
//! reports) while chain B keeps serving blocks, and the runtime must confine the damage to A's cursor.
//!
//! **On the absence-test trap.** "Chain B keeps working" passes trivially when nothing is wrong, so
//! it is not the assertion this test rests on. The load-bearing ones are the pair that can only both
//! hold if the cursors are genuinely independent:
//!
//! - chain B advances to a tip it had *not* reached when A went dark - forward progress *during* a
//!   sibling's outage, not mere survival of it;
//! - chain A is simultaneously frozen at its pre-outage block, which is what proves A really did go
//!   dark and that B's progress is not just "nothing happened to anyone";
//! - and A's per-nest poll clock stops while B's keeps ticking, which is the per-cursor stall signal
//!   `/ready` reads (the serving half of this is pinned in `serve.rs`).
//!
//! It then restores A and holds `escalate_stall`'s own promise - "it resumes automatically when an
//! endpoint recovers" - to account, which also distinguishes a stall from a cursor death.
//!
//! **Why this drives `spawn_runtime` and not `spawn_nest`.** `spawn_nest` runs `index_loop`
//! (`indexer.rs:3174`), the *solo* path, which publishes poll freshness to the process-global
//! `METRICS` only. The multichain runtime runs `runtime_index_loop` (`indexer.rs:1004`) via
//! `spawn_runtime`, and only that one marks the **per-nest** clock `/ready` reads. Written the
//! obvious way - copying `e2e_reorg`'s `spawn_nest` scaffolding - every per-nest metric assertion
//! here compares `0` with `0` and passes vacuously, and a mutation that converts the stall into a
//! cursor death goes undetected. The two cursors therefore also share one `RuntimeHealth`, exactly
//! as `runtime::dev` clones one across every cursor, so the genuinely shared state is in the test's
//! path rather than designed out of it.

mod common;

use std::sync::Arc;

use nuthatch::indexer;
use nuthatch::metrics::METRICS;

use common::tape::*;

/// A canonical block `b`: one USDC transfer, value `100*b`.
fn canonical_block(b: u64) -> BlockFixture {
    transfers_block(
        b,
        0,
        1_700_000_000 + b,
        USDC,
        &[(account(1).as_str(), account(2).as_str(), (100 * b) as u128)],
    )
}

/// Bring up one **cursor** over `tape` hosting a single nest named `name`, sharing `health` with
/// every other cursor in this runtime, and wait until it has indexed through `tip`.
async fn spawn_cursor(
    dir: &std::path::Path,
    name: &str,
    tape: Arc<TapeSource>,
    health: Arc<nuthatch::health::RuntimeHealth>,
    tip: u64,
) -> (
    indexer::ChainCursor,
    std::sync::Arc<dyn nuthatch::store::HotStore>,
) {
    let cfg = scaffold_nest(dir, name, USDC);
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
        // Production's default. This is `spawn_runtime`'s `fail_fast`, which the per-nest supervisor
        // acts on at `indexer.rs:898-899`: a quarantined nest aborts the whole cursor instead of being
        // isolated, so it is the one configuration under which this isolation property is not
        // expected to hold. (The sibling-cursor coupling that `--fail-fast` also drives lives
        // separately, in `runtime.rs:1539`'s `supervise_cursors`.)
        false,
    )
    .await
    .expect("spawn_runtime");
    let store = cursor.states[0].1.store.clone();
    assert!(
        at_block(&store, tip).await,
        "nest {name} did not index to block {tip} in time"
    );
    (cursor, store)
}

/// Bounded wait for `store`'s `last_block` to reach exactly `n`.
async fn at_block(store: &std::sync::Arc<dyn nuthatch::store::HotStore>, n: u64) -> bool {
    let want = n.to_string();
    wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some(want.as_str())
    })
    .await
}

fn last_block(store: &std::sync::Arc<dyn nuthatch::store::HotStore>) -> Option<String> {
    store.get_meta("last_block").ok().flatten()
}

async fn shutdown(cursor: indexer::ChainCursor) {
    cursor.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_chain_does_not_stall_its_co_tenant_cursor() {
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

    // Two cursors, one per chain, one shared RuntimeHealth - the multichain runtime's shape.
    let health = Arc::new(nuthatch::health::RuntimeHealth::new());
    let (rt_a, store_a) =
        spawn_cursor(dir_a.path(), "chain-a", tape_a.clone(), health.clone(), 10).await;
    let (rt_b, store_b) =
        spawn_cursor(dir_b.path(), "chain-b", tape_b.clone(), health.clone(), 10).await;

    // Chain A's whole endpoint set goes unreachable.
    tape_a.go_dark();
    let a_poll_when_dark = METRICS.nest("chain-a").last_poll_ok();
    // Guard the guard: the per-nest clock must actually be running, or every assertion below that
    // reads it compares 0 with 0. This is what caught the solo-vs-cursor path confusion.
    assert!(
        a_poll_when_dark > 0,
        "chain-a's per-nest poll clock was never marked - this test would be vacuous"
    );

    // Both chains genuinely progress from here - A simply cannot see that it has. Extending A's tape
    // too is what makes A's freeze attributable to the outage rather than to an idle chain.
    for b in 11..=15u64 {
        tape_a.insert_block(b, canonical_block(b));
        tape_b.insert_block(b, canonical_block(b));
    }
    tape_a.advance_tip_to(15);
    tape_b.advance_tip_to(15);

    // The load-bearing assertion: B reaches a tip it had NOT reached when A died. This is forward
    // progress *during* a sibling cursor's total outage, not survival of a no-op.
    assert!(
        at_block(&store_b, 15).await,
        "chain B must keep indexing to its own tip while chain A's provider is dark; \
         it is stuck at {:?}",
        last_block(&store_b)
    );

    // Non-vacuity: A really is dark. Had A kept indexing, B's progress would prove nothing.
    assert_eq!(
        last_block(&store_a).as_deref(),
        Some("10"),
        "chain A must be frozen at its pre-outage block - if it advanced, the outage did not take"
    );

    // The per-cursor stall signal `/ready` reads: A's poll clock stopped, B's did not.
    let a_poll = METRICS.nest("chain-a").last_poll_ok();
    let b_poll = METRICS.nest("chain-b").last_poll_ok();
    assert_eq!(
        a_poll, a_poll_when_dark,
        "chain A's last successful poll must not advance while its provider is dark"
    );
    assert!(
        b_poll > a_poll,
        "chain B's poll clock ({b_poll}) must keep ticking past chain A's frozen one ({a_poll})"
    );

    // A stall is not a death: `escalate_stall` promises indexing "resumes automatically when an
    // endpoint recovers". Hold it to that, and confirm A's cursor was alive and retrying all along.
    tape_a.restore();
    assert!(
        at_block(&store_a, 15).await,
        "chain A must catch up on its own once its provider returns; it is stuck at {:?}",
        last_block(&store_a)
    );
    assert_eq!(
        last_block(&store_b).as_deref(),
        Some("15"),
        "chain B stays at its own tip throughout"
    );

    shutdown(rt_a).await;
    shutdown(rt_b).await;
}
