//! The writer-worker role (RFC-0022 §2): the process that actually reconciles.
//!
//! Everything this needs already existed and tested — the control plane, the scheduler, the lease,
//! the fence, [`crate::reconcile::tick`] — and **nothing called it**. `reconcile::tick` had six tests
//! against a live Postgres and no caller, so the compose `writer` service ran an ordinary embedded
//! `dev`: no heartbeat, no lease, no desired state. `--scale writer=2` therefore did not behave as the
//! docs described. This is the missing wire.
//!
//! Worth naming as a failure mode rather than glossing: a well-tested library function with no caller
//! looks exactly like a working feature from the outside, and every test passes.
//!
//! ## The loop
//!
//! Tick, sleep, repeat. Each tick heartbeats, re-reads desired state, plans, and acquires or releases
//! leases. Between ticks the cursors it owns index normally.
//!
//! ## Why a tick failure is a warning, not an exit
//!
//! A control-plane outage must stop *rescheduling*, not *ingestion*. If the database is unreachable
//! this logs and tries again next tick, while the cursors it already holds keep working on leases that
//! have not yet expired. Exiting on a failed tick would convert a control-plane blip into a fleet-wide
//! outage — precisely the coupling the independence between lease and heartbeat exists to avoid.
//!
//! The lease TTL is deliberately several ticks long for the same reason: one missed tick must not hand
//! a cursor to somebody else while its owner is perfectly healthy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::controlplane::ControlPlane;
use crate::reconcile::{tick, CursorHosts, TickOutcome};
use crate::store::HotStore;

/// How often a worker reconciles.
pub const TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Lease TTL, in ticks. A lease must outlive several missed ticks: at 5s a ×6 TTL tolerates half a
/// minute of control-plane trouble before a healthy owner risks losing a cursor it is still indexing.
const LEASE_TTL_TICKS: u64 = 6;

/// The stores a worker can host, one per chain.
///
/// Built once at startup from the nests on disk. A worker offers the chains it *can* run; the
/// scheduler decides which it *should*, and the lease decides which it *does*.
pub struct Hosts {
    stores: Vec<(String, Arc<dyn HotStore>)>,
}

impl Hosts {
    /// One store per chain, from a Postgres hot store shared with the rest of the fleet.
    ///
    /// Keyed by chain rather than by nest because the **cursor** is the unit of ownership (RFC-0021):
    /// every nest on a chain shares one, so they share one lease. A per-nest lease would let two
    /// workers each hold "half" a cursor, which is not a thing that can exist.
    pub fn from_chains(hot_store_url: &str, chains: &[String]) -> Result<Hosts> {
        let mut stores = Vec::new();
        for chain in chains {
            // The schema name has to be a valid identifier, and chain names carry hyphens.
            let ns = format!("cursor_{}", chain.replace('-', "_"));
            let store = crate::pgstore::PgStore::connect(hot_store_url, &ns)
                .with_context(|| format!("cannot open the hot store for chain '{chain}'"))?;
            stores.push((chain.clone(), Arc::new(store) as Arc<dyn HotStore>));
        }
        Ok(Hosts { stores })
    }
}

impl CursorHosts for Hosts {
    fn stores(&self) -> Vec<(String, Arc<dyn HotStore>)> {
        self.stores.clone()
    }
}

/// The nests a worker is actually indexing, keyed by the chain whose cursor authorises them.
///
/// Keyed by **chain**, not by nest, because the lease is per cursor: losing one cursor stops exactly
/// the nests that cursor authorised and leaves the others running. That is the isolation RFC-0021
/// promises, expressed as the shape of this map rather than as care taken at each call site.
#[derive(Default)]
struct Running {
    by_chain: std::collections::HashMap<String, Vec<crate::indexer::NestRuntime>>,
}

impl Running {
    /// Stop everything a cursor authorised. Called when its lease is lost or released.
    ///
    /// The store's fence already refuses writes from a stale holder, so nothing here protects
    /// correctness - it stops a task grinding through RPC for a cursor it no longer owns, which is
    /// waste and, in a log, actively misleading.
    fn stop(&mut self, chain: &str) {
        if let Some(rts) = self.by_chain.remove(chain) {
            for rt in &rts {
                rt.ingest.abort();
            }
            if !rts.is_empty() {
                tracing::info!(chain = %chain, nests = rts.len(), "stopped nests - cursor released");
            }
        }
    }
}

/// Start every declared nest for `chain` that this worker can find on disk.
///
/// **Where a nest comes from is the open half of this.** RFC-0022 names RFC-0019 - "the registry
/// workers pull nests from" - as the intended source, and `desired_nest` carries only
/// `(name, chain, estimated_rss_mb)`: no bundle address, no path. That gap is *why* ingestion was
/// never wired (issue #250) - you cannot build a nest you cannot locate.
///
/// This slice takes the nests the operator has already placed on the machine, which is exactly what
/// the compose topology implies with its `./nest` mount, and leaves the registry pull as the next
/// step. `<root>/<name>` first, then `<root>` itself when it is a single nest whose name matches -
/// so the one-nest compose case works without a per-nest subdirectory.
async fn start_nests_for(
    cp: &ControlPlane,
    running: &mut Running,
    store: Arc<dyn HotStore>,
    nest_root: &std::path::Path,
    chain: &str,
) -> Result<usize> {
    if running.by_chain.contains_key(chain) {
        return Ok(0); // already indexing this cursor
    }
    let desired: Vec<_> = cp
        .desired()?
        .into_iter()
        .filter(|d| d.chain == chain)
        .collect();

    let mut started = Vec::new();
    for d in &desired {
        let dir = {
            let per_nest = nest_root.join(&d.name);
            if per_nest.join(crate::config::CONFIG_FILE).exists() {
                per_nest
            } else {
                nest_root.to_path_buf()
            }
        };
        let cfg = match crate::config::Config::load(&dir) {
            Ok(c) => c,
            Err(e) => {
                // Not fatal, and **loud**: a declared nest this worker cannot find is exactly the
                // silence that let #250 survive. Another worker may have it; this one says so.
                tracing::warn!(
                    nest = %d.name, chain = %chain, dir = %dir.display(),
                    "declared nest not found on this worker - it will not be indexed here: {e:#}"
                );
                continue;
            }
        };
        if cfg.nest.chain != chain {
            tracing::warn!(
                nest = %d.name, declared = %chain, found = %cfg.nest.chain,
                "nest on disk is for a different chain than the control plane declared - skipping"
            );
            continue;
        }
        let source: Arc<dyn crate::source::Source> =
            Arc::new(crate::rpc::RpcClient::new(cfg.nest.rpc_urls.clone())?);
        match crate::indexer::spawn_nest_on_store(
            source,
            dir,
            cfg,
            store.clone(),
            None,
            false,
            // Sequential seal path: a writer shares its endpoint quota with every other worker in the
            // pool, so a per-nest fan-out here multiplies across the fleet. Conservative until the
            // pool has a shared budget to spend (RFC-0022 has none today).
            1,
        )
        .await
        {
            Ok(rt) => {
                tracing::info!(nest = %d.name, chain = %chain, "indexing (writer pool)");
                started.push(rt);
            }
            Err(e) => tracing::warn!(nest = %d.name, "could not start: {e:#}"),
        }
    }
    let n = started.len();
    if n > 0 {
        running.by_chain.insert(chain.to_string(), started);
    } else if !desired.is_empty() {
        tracing::warn!(
            chain = %chain, declared = desired.len(),
            "holding this cursor but indexing nothing - no declared nest for it is present on this \
             worker (see --nest-root)"
        );
    }
    Ok(n)
}

/// Run the reconcile loop until the process is asked to stop.
///
/// `nest_root` is where this worker looks for the nests it is asked to index - see
/// [`start_nests_for`] for why a path is needed at all, and why pulling bundles from the registry
/// (RFC-0019) is the next step rather than this.
///
/// On shutdown it **releases its leases** rather than letting them expire. A graceful exit that left
/// them held would strand every cursor it owned for a full TTL, which is the difference between a
/// rolling restart being invisible and being a thirty-second outage per worker.
pub async fn run(
    cp: ControlPlane,
    hosts: Hosts,
    worker_id: &str,
    budget_mb: u64,
    secrets_for_assigned: bool,
    nest_root: std::path::PathBuf,
) -> Result<()> {
    let ttl = TICK_INTERVAL.as_secs() * LEASE_TTL_TICKS;
    tracing::info!(
        worker = %worker_id,
        budget_mb,
        tick_secs = TICK_INTERVAL.as_secs(),
        lease_ttl_secs = ttl,
        chains = hosts.stores.len(),
        "writer worker started (RFC-0022 §2)"
    );

    let mut held: Vec<String> = Vec::new();
    let mut running = Running::default();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(TICK_INTERVAL) => {}
            _ = shutdown() => {
                // Best-effort: a failure here costs a TTL of staleness, never correctness, so it is
                // logged rather than propagated out of a shutdown path.
                for (chain, store) in &hosts.stores {
                    if held.contains(chain) {
                        // Stop indexing *before* releasing: the moment the lease is gone another
                        // worker may take the cursor, and two writers briefly overlapping is the
                        // situation the fence exists to survive rather than one to create casually.
                        running.stop(chain);
                        if let Err(e) = store.release_lease() {
                            tracing::warn!(chain = %chain, "could not release lease on shutdown: {e:#}");
                        }
                    }
                }
                if let Err(e) = cp.deregister(worker_id) {
                    tracing::warn!("could not deregister: {e:#}");
                }
                tracing::info!(worker = %worker_id, released = held.len(), "worker stopped cleanly");
                return Ok(());
            }
        }

        match tick(&cp, &hosts, worker_id, budget_mb, ttl) {
            Ok(outcome) => {
                let before = held.clone();
                held = apply(&outcome, held);
                report(&outcome, worker_id);

                // **The half that was missing (issue #250).** Holding a cursor now means indexing the
                // nests it authorises; losing one stops exactly those and leaves the rest running.
                for chain in before.iter().filter(|c| !held.contains(c)) {
                    running.stop(chain);
                }
                for chain in &held {
                    if let Some((_, store)) = hosts.stores.iter().find(|(c, _)| c == chain) {
                        if let Err(e) =
                            start_nests_for(&cp, &mut running, store.clone(), &nest_root, chain)
                                .await
                        {
                            tracing::warn!(chain = %chain, "could not start nests: {e:#}");
                        }
                    }
                }
                if secrets_for_assigned && !outcome.acquired.is_empty() {
                    // Secrets are fetched *after* acquiring, scoped to what we actually hold - never
                    // to what we were merely assigned. A worker that lost the race must not be handed
                    // credentials for a cursor it does not own.
                    if let Err(e) = load_secrets(&cp, &held) {
                        tracing::warn!("could not load secrets for assigned nests: {e:#}");
                    }
                }
            }
            // Deliberately not fatal - see the module docs. Cursors we already hold keep indexing on
            // leases that have not expired.
            Err(e) => tracing::warn!(
                worker = %worker_id,
                "reconcile tick failed, retrying next tick (held cursors keep working): {e:#}"
            ),
        }
    }
}

/// Track what we hold, from what the tick actually did rather than from what it intended.
fn apply(outcome: &TickOutcome, mut held: Vec<String>) -> Vec<String> {
    for c in &outcome.acquired {
        if !held.contains(c) {
            held.push(c.clone());
        }
    }
    held.retain(|c| !outcome.released.contains(c));
    held.sort();
    held
}

/// Log only what changed or what an operator should act on. A loop that logged every tick would bury
/// the one tick that mattered.
fn report(o: &TickOutcome, worker_id: &str) {
    if !o.acquired.is_empty() {
        tracing::info!(worker = %worker_id, chains = ?o.acquired, "acquired cursors");
    }
    if !o.released.is_empty() {
        tracing::info!(worker = %worker_id, chains = ?o.released, "released cursors");
    }
    if !o.contended.is_empty() {
        // One occurrence is benign - another worker got there first. Persistent contention means two
        // schedulers disagree about the world, which is worth someone looking at.
        tracing::debug!(worker = %worker_id, chains = ?o.contended, "cursors held elsewhere");
    }
    if !o.unplaceable.is_empty() {
        // Warn, every tick, on purpose: an under-served fleet must not look healthy. `GET /plan` says
        // why, and the two reasons need different actions.
        tracing::warn!(
            chains = ?o.unplaceable,
            "cursors nobody can place - see GET /plan on the control plane for the reason"
        );
    }
}

fn load_secrets(
    cp: &ControlPlane,
    held: &[String],
) -> Result<HashMap<String, HashMap<String, String>>> {
    let got = cp.secrets_for(held)?;
    // Count only - never the keys, let alone the values. A log line is the easiest place for a
    // credential to escape to.
    tracing::info!(
        nests = got.len(),
        "runtime secrets injected for held cursors"
    );
    Ok(got)
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(acq: &[&str], rel: &[&str]) -> TickOutcome {
        TickOutcome {
            acquired: acq.iter().map(|s| s.to_string()).collect(),
            released: rel.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Held state tracks what a tick *did*, so a contended cursor is never counted as held - which is
    /// what would make a worker release a lease it never had, or report ownership it lost.
    #[test]
    fn held_state_follows_what_actually_happened() {
        let held = apply(&outcome(&["mainnet"], &[]), vec![]);
        assert_eq!(held, vec!["mainnet"]);

        // Acquiring the same cursor twice must not duplicate it.
        let held = apply(&outcome(&["mainnet"], &[]), held);
        assert_eq!(held, vec!["mainnet"]);

        let held = apply(&outcome(&["arbitrum-one"], &[]), held);
        assert_eq!(held, vec!["arbitrum-one", "mainnet"], "and stays sorted");

        let held = apply(&outcome(&[], &["mainnet"]), held);
        assert_eq!(held, vec!["arbitrum-one"], "a release drops it");
    }

    /// A cursor that was contended is not held, so shutdown will not try to release it.
    #[test]
    fn contention_does_not_count_as_ownership() {
        let mut o = outcome(&[], &[]);
        o.contended = vec!["mainnet".into()];
        assert!(apply(&o, vec![]).is_empty());
    }

    /// The lease must outlive several missed ticks, or one control-plane blip hands a cursor away from
    /// a worker that is still healthily indexing it.
    #[test]
    fn the_lease_ttl_is_several_ticks_long() {
        let ttl = TICK_INTERVAL.as_secs() * LEASE_TTL_TICKS;
        assert!(
            ttl >= TICK_INTERVAL.as_secs() * 3,
            "a {ttl}s lease on a {}s tick is too tight to survive a hiccup",
            TICK_INTERVAL.as_secs()
        );
    }
}
