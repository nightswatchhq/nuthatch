//! RFC-0041 slice 2: the runtime lifecycle for an authored incremental entity (#864).
//!
//! This is deliberately the same shape as [`crate::views::BalanceView`], because §5.1 says the
//! lifecycle is the one already proved by the built-in circuits rather than a second one:
//!
//! > Backfill uses larger batches, but not different semantics.
//!
//! A circuit on its own thread, a channel of weighted batches, a health flag that latches false when
//! the thread dies, and a flush barrier. Two things are added that the built-in views do not need.
//!
//! **An applied-through watermark** (criterion 2). A built-in view is fed from the same commit as the
//! facts and has no separate identity, so "how far has this got" is the nest's own cursor. An
//! authored entity is one of several, each of which may be behind, so it carries its own. Serving is
//! gated on it: an entity answers for the head it has actually applied, never for the nest's.
//!
//! **`max_rows` as a fault, not a warning** (criterion 10). Crossing the declared bound stops the
//! circuit and latches unhealthy, which the ingest loop already treats as terminal. §7 wants the
//! bound enforced while running, not merely validated at load, and a bound that logs and continues
//! is the shape this project keeps finding in its own instruments.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
use std::sync::{Arc, RwLock};

use crate::authored_entity_spike::{Batch, DelegationPlan, Spike};

enum Msg {
    /// A weighted batch, and the block it carries the entity through once folded.
    Batch(Box<Batch>, u64),
    Flush(SyncSender<()>),
}

/// One maintained authored entity: its circuit, its state, and how far it has been applied.
pub struct EntityView {
    name: String,
    tx: Sender<Msg>,
    rows: Arc<RwLock<BTreeMap<String, i128>>>,
    /// Latches `false` if the circuit thread dies - on start, on a step, or on the declared bound.
    /// §5.2: "Serving frozen derived state as healthy is not graceful degradation; it is a lie with
    /// a pleasant HTTP status."
    healthy: Arc<AtomicBool>,
    /// The dataset head this entity has **actually folded**, which is not the nest's head while it
    /// is catching up. Advanced only after the batch carrying it has been applied.
    applied_through: Arc<AtomicU64>,
}

impl EntityView {
    /// Spawn the circuit on its own thread. DBSP drives worker threads; keep it off the async pool,
    /// exactly as `BalanceView::start` does.
    pub fn start(name: &str, plan: &DelegationPlan, max_rows: usize) -> Result<Self> {
        let (tx, rx) = channel::<Msg>();
        let rows = Arc::new(RwLock::new(BTreeMap::new()));
        let shared = rows.clone();
        let healthy = Arc::new(AtomicBool::new(true));
        let health = healthy.clone();
        let applied_through = Arc::new(AtomicU64::new(0));
        let watermark = applied_through.clone();
        let plan = plan.clone();
        let label = name.to_string();
        let thread_label = label.clone();

        std::thread::Builder::new()
            .name(format!("nuthatch-entity-{name}"))
            .spawn(move || {
                let mut spike = match Spike::with_max_rows(&plan, max_rows) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!("entity `{thread_label}` circuit failed to start: {e:#}");
                        health.store(false, Ordering::SeqCst);
                        return;
                    }
                };
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Batch(batch, through) => {
                            if let Err(e) = spike.apply(*batch) {
                                // Includes crossing the declared `max_rows`, which is a fault and
                                // not a warning (criterion 10). The watermark deliberately does not
                                // advance: whatever this batch carried, the entity did not fold it.
                                tracing::error!("entity `{thread_label}` step failed: {e:#}");
                                health.store(false, Ordering::SeqCst);
                                break;
                            }
                            // State first, then the watermark. A reader that sees the new head must
                            // be able to see the rows that head accounts for; the other order
                            // publishes a head the state has not caught up to.
                            *shared.write().unwrap() = spike.rows();
                            watermark.store(through, Ordering::SeqCst);
                        }
                        // Messages are processed in order, so by the time the barrier is seen every
                        // prior batch is folded - the ack unblocks the waiter.
                        Msg::Flush(ack) => {
                            let _ = ack.send(());
                        }
                    }
                }
            })
            .with_context(|| format!("failed to spawn the circuit thread for entity `{name}`"))?;

        Ok(Self {
            name: label,
            tx,
            rows,
            healthy,
            applied_through,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the circuit thread is alive and folding. `false` means it died - on start, on a step,
    /// or on the declared bound - and the ingest loop treats that as terminal rather than serving a
    /// frozen relation. A clean shutdown drops the sender and exits without flipping this.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    /// Enqueue a weighted batch and the block it carries the entity through. Non-blocking, and drops
    /// silently if the thread has died - which `is_healthy` is what reports, not this.
    pub fn apply(&self, batch: Batch, through: u64) {
        let _ = self.tx.send(Msg::Batch(Box::new(batch), through));
    }

    /// Block until every batch enqueued so far has been folded. Used after a restart rebuild so the
    /// first request sees a complete relation, and as the consistency boundary at a commit.
    pub fn flush(&self) {
        let (ack, wait) = sync_channel(0);
        if self.tx.send(Msg::Flush(ack)).is_ok() {
            let _ = wait.recv();
        }
    }

    /// The maintained relation as of the last folded batch.
    pub fn rows(&self) -> BTreeMap<String, i128> {
        self.rows.read().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.rows.read().map(|r| r.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The block this entity has folded through (criterion 2).
    ///
    /// **Serving reads this, never the nest's head.** An entity behind the dataset is a normal state
    /// during backfill and after a definition change; answering for the nest's head while holding
    /// this one's rows is how a partial relation gets stamped current.
    pub fn applied_through(&self) -> u64 {
        self.applied_through.load(Ordering::SeqCst)
    }

    /// Is this entity current for a dataset advertising `head`?
    pub fn is_current(&self, head: u64) -> bool {
        self.applied_through() >= head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authored_entity_spike::{compile, DelegationFact, IndexerFact, DELEGATION_SQL};

    fn plan() -> DelegationPlan {
        compile(DELEGATION_SQL).unwrap()
    }

    fn d(indexer: &str, delegator: &str, amount: i128, w: i64) -> (DelegationFact, i64) {
        (
            DelegationFact {
                indexer: indexer.into(),
                delegator: delegator.into(),
                amount,
            },
            w,
        )
    }

    fn active(indexer: &str) -> (IndexerFact, i64) {
        (
            IndexerFact {
                indexer: indexer.into(),
                active: true,
            },
            1,
        )
    }

    #[test]
    fn a_batch_is_folded_and_carries_the_watermark_with_it() {
        let v = EntityView::start("delegations", &plan(), 1_000).unwrap();
        assert_eq!(v.applied_through(), 0, "nothing folded yet");

        v.apply(
            Batch {
                delegations: vec![d("i1", "a", 7, 1), d("i1", "a", 5, 1)],
                indexers: vec![active("i1")],
            },
            100,
        );
        v.flush();

        assert!(v.is_healthy());
        assert_eq!(
            v.rows().get("i1\u{1f}a"),
            Some(&12),
            "7 + 5, summed by the circuit"
        );
        assert_eq!(v.applied_through(), 100);
        assert!(v.is_current(100));
        assert!(!v.is_current(101), "it has not folded 101");
    }

    #[test]
    fn a_reorg_retracts_at_minus_one_and_converges_on_the_replacement() {
        // §5.2: removed rows are fed at -1 before deletion, replacements arrive at +1.
        let v = EntityView::start("delegations", &plan(), 1_000).unwrap();
        v.apply(
            Batch {
                delegations: vec![d("i1", "a", 7, 1)],
                indexers: vec![active("i1")],
            },
            100,
        );
        v.apply(
            Batch {
                delegations: vec![d("i1", "a", 7, -1), d("i1", "a", 9, 1)],
                indexers: vec![],
            },
            101,
        );
        v.flush();

        assert_eq!(v.rows().get("i1\u{1f}a"), Some(&9));
        assert_eq!(v.applied_through(), 101);
    }

    #[test]
    fn crossing_max_rows_faults_the_circuit_rather_than_warning() {
        // Criterion 10: neither warns-and-continues nor OOMs the cursor.
        let v = EntityView::start("delegations", &plan(), 1).unwrap();
        v.apply(
            Batch {
                delegations: vec![d("i1", "a", 7, 1), d("i1", "b", 5, 1)],
                indexers: vec![active("i1")],
            },
            100,
        );
        v.flush();

        assert!(
            !v.is_healthy(),
            "the declared bound is a fault, not a log line"
        );
        assert_eq!(
            v.applied_through(),
            0,
            "a batch that was not folded must not advance the watermark"
        );
    }

    #[test]
    fn the_watermark_does_not_advance_past_a_batch_the_circuit_refused() {
        // The stale-serving guard criterion 2 asks for: an entity that stopped folding at 100 must
        // not answer for 200 merely because a later batch was enqueued.
        let v = EntityView::start("delegations", &plan(), 1).unwrap();
        v.apply(
            Batch {
                delegations: vec![d("i1", "a", 7, 1)],
                indexers: vec![active("i1")],
            },
            100,
        );
        v.flush();
        assert_eq!(v.applied_through(), 100);

        v.apply(
            Batch {
                delegations: vec![d("i1", "b", 5, 1)],
                indexers: vec![],
            },
            200,
        );
        v.flush();
        assert!(!v.is_healthy());
        assert_eq!(v.applied_through(), 100, "it did not fold 200");
        assert!(!v.is_current(200));
    }

    #[test]
    fn an_empty_batch_still_advances_the_watermark() {
        // A window with no facts for this entity is progress: the entity is current through it. The
        // built-in views can skip an empty batch because they have no watermark to move.
        let v = EntityView::start("delegations", &plan(), 1_000).unwrap();
        v.apply(
            Batch {
                delegations: vec![],
                indexers: vec![],
            },
            500,
        );
        v.flush();
        assert!(v.is_healthy());
        assert_eq!(v.applied_through(), 500);
        assert!(v.is_empty());
    }
}
