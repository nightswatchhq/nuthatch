//! The reconcile loop (RFC-0022 §2/§3): turning a plan into acquired leases and drained cursors.
//!
//! This is the component that makes the previous five slices a system. One tick:
//!
//! 1. heartbeat, so the fleet knows this worker exists;
//! 2. read desired state and the live workers from the control plane;
//! 3. [`crate::scheduler::plan`] decides placement;
//! 4. for cursors assigned to *us*, acquire the lease - which is where placement stops being an
//!    opinion and becomes ownership;
//! 5. for cursors planned away from us, release.
//!
//! ## The plan is advice; the lease is the fact
//!
//! Step 4 can fail, and that is **not an error**. Two workers reconciling concurrently can compute
//! plans that disagree - they read the control plane at different instants, and nothing serialises
//! them. When that happens both try to acquire, exactly one wins, and the loser simply does not run
//! that cursor this tick. The lease is the arbiter precisely so the scheduler does not have to be a
//! distributed consensus problem.
//!
//! This is why the control plane can be wrong about what is running and nothing breaks: reconciliation
//! never trusts a stored assignment, it re-derives ownership from the leases each tick.
//!
//! ## What this deliberately does not do
//!
//! **It does not move a cursor away from a worker that still holds a live lease on it.** If the plan
//! says a cursor belongs elsewhere but we hold it, we release *ours* and let the other worker acquire
//! on its own tick. Reaching across to revoke someone else's lease would be the one operation capable
//! of creating two writers, so it does not exist.

use std::collections::BTreeSet;

use anyhow::Result;

use crate::controlplane::{ControlPlane, DEFAULT_WORKER_TTL_SECS};
use crate::scheduler::{plan, Assignment, Plan};
use crate::store::{HotStore, LeaseHeld};

/// What one tick actually did, as opposed to what it intended. Returned rather than logged-and-
/// forgotten so a caller can assert on it, and so an operator endpoint can report it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    /// Cursors whose lease we now hold and did not before.
    pub acquired: Vec<String>,
    /// Cursors we released because the plan moved them elsewhere, or nobody wants them.
    pub released: Vec<String>,
    /// Cursors the plan gave us but another worker already holds. Not a failure - see the module
    /// docs. Recorded because a cursor that is *persistently* contended means two schedulers
    /// disagree about the world, which is worth an operator's attention even though each individual
    /// occurrence is benign.
    pub contended: Vec<String>,
    /// What nobody could place, straight from the plan. Surfaced every tick so an under-served fleet
    /// is loud rather than merely incomplete.
    pub unplaceable: Vec<String>,
}

/// A worker's view of the cursors it can act on: chain → its hot store.
///
/// Passed in rather than opened here because opening a store is the *mount* path, which owns bundle
/// resolution, secret injection and view rebuilds. Reconciliation decides *whether* a cursor should
/// run here; it does not decide how one is built.
pub trait CursorHosts {
    /// Hot stores this worker can lease, keyed by chain.
    fn stores(&self) -> Vec<(String, std::sync::Arc<dyn HotStore>)>;
}

/// Run one reconciliation tick for `worker_id`.
///
/// `lease_ttl_secs` should be comfortably shorter than the interval at which this is called times the
/// number of consecutive failures you are willing to tolerate - a lease that expires between ticks
/// hands the cursor to someone else while its owner is perfectly healthy.
pub fn tick(
    cp: &ControlPlane,
    hosts: &dyn CursorHosts,
    worker_id: &str,
    budget_mb: u64,
    lease_ttl_secs: u64,
) -> Result<TickOutcome> {
    cp.heartbeat(worker_id, budget_mb)?;

    let desired = cp.desired()?;
    let workers = cp.live_workers(DEFAULT_WORKER_TTL_SECS)?;
    let stores = hosts.stores();

    // Ownership is re-derived from the leases, never remembered. A stored assignment can be stale the
    // instant after it is written; a lease cannot, because it is only meaningful in the transaction
    // that took it.
    let mut current: Vec<Assignment> = Vec::new();
    for (chain, store) in &stores {
        if let Some(l) = store.current_lease()? {
            if l.expires_in_secs > 0 {
                current.push(Assignment {
                    chain: chain.clone(),
                    worker: l.owner,
                });
            }
        }
    }

    let p: Plan = plan(&workers, &desired, &current);
    let mine: BTreeSet<&str> = p
        .assign
        .iter()
        .filter(|a| a.worker == worker_id)
        .map(|a| a.chain.as_str())
        .collect();
    let held_here: BTreeSet<&str> = current
        .iter()
        .filter(|a| a.worker == worker_id)
        .map(|a| a.chain.as_str())
        .collect();

    let mut out = TickOutcome {
        unplaceable: p.unplaceable.iter().map(|u| u.chain.clone()).collect(),
        ..Default::default()
    };

    for (chain, store) in &stores {
        let want = mine.contains(chain.as_str()) || held_here.contains(chain.as_str());
        let should_drop = p
            .drain
            .iter()
            .any(|d| &d.chain == chain && d.worker == worker_id);

        if should_drop || (!want && held_here.contains(chain.as_str())) {
            store.release_lease()?;
            out.released.push(chain.clone());
            continue;
        }
        if !want {
            continue;
        }
        if held_here.contains(chain.as_str()) {
            // Ours already - extend rather than re-acquire, so the fence stays put and our in-flight
            // writes stay valid.
            store.renew_lease(lease_ttl_secs)?;
            continue;
        }
        match store.acquire_lease(worker_id, lease_ttl_secs) {
            Ok(_) => out.acquired.push(chain.clone()),
            // Someone got there first. Benign by design - see the module docs.
            Err(e) if e.downcast_ref::<LeaseHeld>().is_some() => out.contended.push(chain.clone()),
            Err(e) => return Err(e),
        }
    }

    out.acquired.sort();
    out.released.sort();
    out.contended.sort();
    out.unplaceable.sort();
    Ok(out)
}
