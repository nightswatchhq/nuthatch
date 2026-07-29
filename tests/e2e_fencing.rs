//! RFC-0022 slice 4: **single-owner, enforced rather than assumed**.
//!
//! §2 promises a cursor is owned by exactly one worker. A control-plane lease alone does not deliver
//! that - it makes it *likely*. The case a lease misses needs no partition and no bug:
//!
//! 1. worker A holds the lease and stalls - a long GC, a paused container, a host that went away;
//! 2. the lease expires and worker B claims the cursor, entirely legitimately;
//! 3. worker A wakes up. Nothing has told it anything happened. It finishes its window and writes.
//!
//! Two workers, one cursor, healthy network. The remedy is a monotonic fence enforced **by the
//! store**, because a worker that checks its own lease before writing is checking a fact that can
//! expire between the check and the write.
//!
//! These tests are the reason the fence lives in `HotStore` rather than in a scheduler: the guarantee
//! is only worth as much as the layer that refuses the write.
//!
//! ## Why these use one redb handle rather than two processes
//!
//! redb takes an **exclusive file lock**, so a second `Store::open` on the same file fails outright -
//! two redb writers are impossible by construction, which is exactly why embedded mode never needed a
//! fence. The scenario the fence exists for is a *shared* store, and there the rival worker is another
//! process against Postgres.
//!
//! So a rival is simulated the only honest way available here: by advancing the persisted fence, which
//! is precisely what another worker's `claim` does. The Postgres side of this is covered by
//! `pg_parity`, where two real connections exist.

mod common;

use nuthatch::store::{HotStore, LostOwnership, Store};

fn row(block: u64) -> Vec<(String, String)> {
    vec![(
        Store::entity_key(block, 0),
        serde_json::json!({ "table": "t", "block_number": block, "log_index": 0 }).to_string(),
    )]
}

/// A store nobody has claimed enforces nothing. This is embedded mode, and it must stay free of
/// ceremony: one process, one writer, by construction.
#[test]
fn an_unclaimed_store_is_not_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.redb")).unwrap();
    assert_eq!(store.held_fence(), 0);
    assert_eq!(store.current_fence().unwrap(), 0);
    store
        .commit_window(&row(1), None, 1)
        .expect("embedded writes are never fenced");
    assert_eq!(store.count().unwrap(), 1);
}

/// The fence is monotonic across claimants - that is the whole property.
#[test]
fn claiming_advances_the_fence_monotonically() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let a = Store::open(&path).unwrap();

    assert_eq!(a.claim("worker-a").unwrap(), 1);
    assert_eq!(
        a.claim("worker-a").unwrap(),
        2,
        "re-claiming still advances"
    );
    assert_eq!(a.current_fence().unwrap(), 2);
}

/// redb's exclusive lock, asserted rather than assumed - it is the reason embedded mode is safe
/// without a fence, and if it ever stopped being true the fence would become load-bearing here too.
#[test]
fn redb_refuses_a_second_writer_outright() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let _a = Store::open(&path).unwrap();
    assert!(
        Store::open(&path).is_err(),
        "two redb handles on one file would be two writers; the file lock must prevent it"
    );
}

/// Stand in for a rival worker by doing what its `claim` does: advance the persisted fence.
fn rival_claims(store: &Store) -> u64 {
    let next = store.current_fence().unwrap() + 1;
    store
        .set_meta(nuthatch::store::OWNER_FENCE, &next.to_string())
        .expect("the current owner may still write, so this stands in for the rival's claim");
    next
}

/// **The headline case.** Worker A stalls, worker B takes the cursor, worker A wakes up and writes.
/// The write must be refused - and refused by the store, not by A's own good manners.
#[test]
fn a_stalled_worker_that_wakes_up_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let a = Store::open(&dir.path().join("t.redb")).unwrap();

    let fence_a = a.claim("worker-a").unwrap();
    a.commit_window(&row(1), None, 1)
        .expect("A owns the cursor and writes normally");

    // ... A stalls. Its lease expires. B claims.
    let fence_b = rival_claims(&a);
    assert!(
        fence_b > fence_a,
        "the new owner's fence must exceed the old"
    );

    // ... A wakes up, still holding its old fence, and finishes the window it was in.
    let err = a
        .commit_window(&row(3), None, 3)
        .expect_err("a fenced-out worker must not be able to write");
    let lost = err
        .downcast_ref::<LostOwnership>()
        .expect("the refusal must be typed, so a caller can tell it from an I/O error and stop");
    assert_eq!(lost.held, fence_a);
    assert_eq!(lost.current, fence_b);

    // And the refusal is a refusal: nothing of A's landed.
    assert_eq!(
        a.count().unwrap(),
        1,
        "only A's pre-handover row is present"
    );
    assert!(
        a.get_entity(&Store::entity_key(3, 0)).unwrap().is_none(),
        "block 3 was A's stale write and must be absent"
    );
}

/// Every mutating path is fenced, not just the obvious one. A gap here is a silent corruption route:
/// a stale worker that cannot `commit_window` but *can* rewind `last_block` is still dangerous.
#[test]
fn every_mutating_path_is_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let a = Store::open(&path).unwrap();
    a.claim("worker-a").unwrap();
    a.commit_window(&row(1), Some((1, "0xaa")), 1).unwrap();
    rival_claims(&a);

    fn is_fenced<T: std::fmt::Debug>(r: anyhow::Result<T>, what: &str) {
        let e = r.expect_err(&format!("{what} must be fenced"));
        assert!(
            e.downcast_ref::<LostOwnership>().is_some(),
            "{what} failed for the wrong reason: {e}"
        );
    }

    is_fenced(a.commit_window(&row(9), None, 9), "commit_window");
    is_fenced(a.put_entity("k", "{}").map(|_| ()), "put_entity");
    is_fenced(a.set_meta("last_block", "0").map(|_| ()), "set_meta");
    is_fenced(a.set_block_hash(1, "0xbb").map(|_| ()), "set_block_hash");
    is_fenced(a.rollback_to(0), "rollback_to");
    is_fenced(
        a.rollback_to_and_set_meta(0, "k", "v"),
        "rollback_to_and_set_meta",
    );
    is_fenced(a.prune_range(0, 10), "prune_range");
    is_fenced(a.prune_and_set_meta(0, 10, "k", "v"), "prune_and_set_meta");
    is_fenced(a.outbox_push("{}"), "outbox_push");
    is_fenced(a.outbox_remove(0).map(|_| ()), "outbox_remove");

    // Reads are never fenced: a fenced-out worker may still be serving, and refusing its reads would
    // turn a lease handover into an outage.
    assert!(a.count().is_ok(), "reads must survive losing ownership");
    assert!(a.recent(10).is_ok());
    assert!(a.get_meta("last_block").is_ok());
}

/// Claiming must stay unfenced, or ownership could never transfer - the loser of a race would be
/// unable to ever take over.
#[test]
fn a_fenced_out_worker_can_still_reclaim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.redb");
    let a = Store::open(&path).unwrap();
    a.claim("worker-a").unwrap();
    rival_claims(&a);
    assert!(a.commit_window(&row(1), None, 1).is_err());

    let regained = a.claim("worker-a").unwrap();
    assert_eq!(regained, 3, "the fence keeps advancing across handovers");
    a.commit_window(&row(1), None, 1)
        .expect("having reclaimed, A may write again");
}

/// Fences are never reused across a run of handovers. A reused fence would let a stale worker's
/// number match again and silently readmit it.
#[test]
fn fences_are_never_reused() {
    let dir = tempfile::tempdir().unwrap();
    let a = Store::open(&dir.path().join("t.redb")).unwrap();

    let mut seen = Vec::new();
    for i in 0..8 {
        seen.push(if i % 2 == 0 {
            a.claim(&format!("worker-{i}")).unwrap()
        } else {
            rival_claims(&a)
        });
    }
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), seen.len(), "every fence must be distinct");
    assert!(
        seen.windows(2).all(|w| w[1] > w[0]),
        "and strictly increasing: {seen:?}"
    );
}
