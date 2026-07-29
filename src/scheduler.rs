//! Cursor placement (RFC-0022 §2) - deciding which worker runs which cursor.
//!
//! Everything here is a **pure function of (workers, desired nests, current assignments)**. No
//! database, no clock, no network. That is deliberate: placement is where the interesting mistakes
//! live - double-assignment, silent under-scheduling, churn - and none of them need distribution to
//! reproduce. The parts that genuinely need a fleet are acquiring the lease (slice 4b, done) and
//! serving the desired set from the control plane (slice 6); this is the decision in between, and it
//! is decided the same way on a laptop as on twenty machines.
//!
//! ## The unit of placement is a cursor, not a nest
//!
//! RFC-0021: one cursor per **chain**, and every nest on that chain shares it. So placement groups
//! nests by chain first and places the groups. A scheduler that placed nests individually could put
//! two nests of the same chain on different workers, which would mean two cursors for one chain -
//! the single-cursor law broken by the component whose job is to uphold it.
//!
//! ## What this deliberately does not do
//!
//! **It never moves a cursor that is already placed and still fits.** Rebalancing on every tick would
//! churn cursors between workers, and a cursor move is not free - it drains, hands over the lease,
//! and re-warms. Stability is worth more than perfectly even load, so a move has to be *forced* (the
//! worker is gone, or the cursor no longer fits) rather than merely *preferred*.

use std::collections::{BTreeMap, BTreeSet};

use crate::roost::{DEFAULT_MAX_RSS_MB, ROOST_BASE_RSS_MB};

/// A nest the operator wants running, as recorded in the control plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredNest {
    pub name: String,
    /// The chain decides which cursor it lands on - nests sharing a chain share a cursor.
    pub chain: String,
    /// Projected resident footprint, from the same estimator the roost already uses at mount time.
    pub estimated_rss_mb: u64,
}

/// A worker available to run cursors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worker {
    pub id: String,
    /// RAM ceiling for everything this worker runs. Defaults to the CLAUDE.md per-cursor budget.
    pub budget_mb: u64,
}

impl Worker {
    pub fn new(id: impl Into<String>) -> Worker {
        Worker {
            id: id.into(),
            budget_mb: DEFAULT_MAX_RSS_MB,
        }
    }
}

/// A cursor: one chain, and every desired nest on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub chain: String,
    pub nests: Vec<String>,
    /// Projected footprint of the whole cursor - the roost base plus each nest's estimate, matching
    /// how `roost::estimate` already reasons. Co-tenancy is RAM-bounded, not free.
    pub rss_mb: u64,
}

/// Where a cursor currently runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub chain: String,
    pub worker: String,
}

/// What the scheduler wants to happen next.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Cursors to start on a worker that does not currently run them.
    pub assign: Vec<Assignment>,
    /// Cursors to drain off a worker - either the worker is gone, or the cursor moved.
    pub drain: Vec<Assignment>,
    /// Cursors nobody can run, with the reason. **Never silently dropped**: an operator who asked for
    /// a nest and got nothing must be told why, or the fleet quietly under-serves and looks healthy.
    pub unplaceable: Vec<Unplaceable>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unplaceable {
    pub chain: String,
    pub rss_mb: u64,
    pub reason: UnplaceableReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnplaceableReason {
    /// No worker exists at all.
    NoWorkers,
    /// Every worker is too full. Distinct from `TooLargeForAnyWorker` because this one is fixed by
    /// adding capacity and that one never is.
    NoRoomRightNow,
    /// The cursor alone exceeds the largest worker's entire budget. Adding workers will not help;
    /// the nest set on that chain has to shrink or the budget has to rise.
    TooLargeForAnyWorker,
}

impl std::fmt::Display for UnplaceableReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UnplaceableReason::NoWorkers => write!(f, "no workers are available"),
            UnplaceableReason::NoRoomRightNow => {
                write!(f, "every worker is at its RAM budget - add a worker")
            }
            UnplaceableReason::TooLargeForAnyWorker => write!(
                f,
                "this cursor alone exceeds the largest worker's budget - adding workers will not \
                 help; split the nests across chains or raise the budget"
            ),
        }
    }
}

/// Group desired nests into cursors, one per chain (RFC-0021).
///
/// Deterministic in every respect: chains sort, nests within a chain sort. Two schedulers given the
/// same desired set must produce the same cursors, or a handover between them would look like a
/// change.
pub fn cursors_of(desired: &[DesiredNest]) -> Vec<Cursor> {
    let mut by_chain: BTreeMap<&str, Vec<&DesiredNest>> = BTreeMap::new();
    for n in desired {
        by_chain.entry(n.chain.as_str()).or_default().push(n);
    }
    by_chain
        .into_iter()
        .map(|(chain, mut nests)| {
            nests.sort_by(|a, b| a.name.cmp(&b.name));
            Cursor {
                chain: chain.to_string(),
                nests: nests.iter().map(|n| n.name.clone()).collect(),
                // The roost base is paid once per cursor, not once per nest - which is exactly why
                // co-locating nests of one chain is cheaper than spreading them.
                rss_mb: ROOST_BASE_RSS_MB + nests.iter().map(|n| n.estimated_rss_mb).sum::<u64>(),
            }
        })
        .collect()
}

/// Decide the next placement step.
///
/// `current` is what is actually running, which in a real fleet is read from the cursors' leases
/// rather than from anyone's memory - the lease is the truth about ownership, and a scheduler that
/// trusted its own record could plan against a world that has moved on.
pub fn plan(workers: &[Worker], desired: &[DesiredNest], current: &[Assignment]) -> Plan {
    let cursors = cursors_of(desired);
    let wanted: BTreeSet<&str> = cursors.iter().map(|c| c.chain.as_str()).collect();
    let live: BTreeSet<&str> = workers.iter().map(|w| w.id.as_str()).collect();
    let mut plan = Plan::default();

    // 1. Drain anything that should not be running: a cursor nobody asked for any more, or one whose
    //    worker has left the pool. A cursor on a departed worker is *also* an assign below, once its
    //    lease expires - the drain here is bookkeeping, since there is nothing left to drain to.
    for a in current {
        if !wanted.contains(a.chain.as_str()) || !live.contains(a.worker.as_str()) {
            plan.drain.push(a.clone());
        }
    }

    // 2. Load already committed to each live worker by placements we are keeping.
    let kept: BTreeMap<&str, &Assignment> = current
        .iter()
        .filter(|a| wanted.contains(a.chain.as_str()) && live.contains(a.worker.as_str()))
        .map(|a| (a.chain.as_str(), a))
        .collect();
    let mut load: BTreeMap<&str, u64> = workers.iter().map(|w| (w.id.as_str(), 0)).collect();
    for c in &cursors {
        if let Some(a) = kept.get(c.chain.as_str()) {
            *load.entry(a.worker.as_str()).or_insert(0) += c.rss_mb;
        }
    }

    let largest_budget = workers.iter().map(|w| w.budget_mb).max().unwrap_or(0);

    // 3. Place what is not already placed. Largest first: a big cursor placed last tends to find
    //    every worker part-full and become unplaceable, when placing it first would have fitted.
    let mut to_place: Vec<&Cursor> = cursors
        .iter()
        .filter(|c| !kept.contains_key(c.chain.as_str()))
        .collect();
    to_place.sort_by(|a, b| b.rss_mb.cmp(&a.rss_mb).then(a.chain.cmp(&b.chain)));

    for c in to_place {
        if workers.is_empty() {
            plan.unplaceable.push(Unplaceable {
                chain: c.chain.clone(),
                rss_mb: c.rss_mb,
                reason: UnplaceableReason::NoWorkers,
            });
            continue;
        }
        if c.rss_mb > largest_budget {
            plan.unplaceable.push(Unplaceable {
                chain: c.chain.clone(),
                rss_mb: c.rss_mb,
                reason: UnplaceableReason::TooLargeForAnyWorker,
            });
            continue;
        }
        // Least-loaded that fits, ties broken by worker id so the plan is deterministic.
        let pick = workers
            .iter()
            .filter(|w| load.get(w.id.as_str()).copied().unwrap_or(0) + c.rss_mb <= w.budget_mb)
            .min_by(|a, b| {
                let (la, lb) = (
                    load.get(a.id.as_str()).copied().unwrap_or(0),
                    load.get(b.id.as_str()).copied().unwrap_or(0),
                );
                la.cmp(&lb).then(a.id.cmp(&b.id))
            });
        match pick {
            Some(w) => {
                *load.entry(w.id.as_str()).or_insert(0) += c.rss_mb;
                plan.assign.push(Assignment {
                    chain: c.chain.clone(),
                    worker: w.id.clone(),
                });
            }
            None => plan.unplaceable.push(Unplaceable {
                chain: c.chain.clone(),
                rss_mb: c.rss_mb,
                reason: UnplaceableReason::NoRoomRightNow,
            }),
        }
    }

    plan.assign.sort_by(|a, b| a.chain.cmp(&b.chain));
    plan.drain.sort_by(|a, b| a.chain.cmp(&b.chain));
    plan.unplaceable.sort_by(|a, b| a.chain.cmp(&b.chain));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(name: &str, chain: &str, mb: u64) -> DesiredNest {
        DesiredNest {
            name: name.into(),
            chain: chain.into(),
            estimated_rss_mb: mb,
        }
    }

    fn at(chain: &str, worker: &str) -> Assignment {
        Assignment {
            chain: chain.into(),
            worker: worker.into(),
        }
    }

    /// The single-cursor law, upheld by the component whose job it is: nests sharing a chain share a
    /// cursor and therefore cannot land on different workers.
    #[test]
    fn nests_on_one_chain_become_one_cursor() {
        let desired = [
            nest("usdc", "mainnet", 90),
            nest("weth", "mainnet", 90),
            nest("arb", "arbitrum-one", 90),
        ];
        let cursors = cursors_of(&desired);
        assert_eq!(cursors.len(), 2, "two chains, two cursors");
        assert_eq!(cursors[0].chain, "arbitrum-one");
        assert_eq!(cursors[1].nests, vec!["usdc", "weth"]);
        // The roost base is paid once per cursor, not per nest.
        assert_eq!(cursors[1].rss_mb, ROOST_BASE_RSS_MB + 180);

        let p = plan(&[Worker::new("w1"), Worker::new("w2")], &desired, &[]);
        let mainnet: Vec<&Assignment> = p.assign.iter().filter(|a| a.chain == "mainnet").collect();
        assert_eq!(mainnet.len(), 1, "one chain is placed exactly once");
    }

    /// A cursor already running where it still fits must not be moved. Churn costs a drain, a lease
    /// handover and a re-warm; even load is not worth paying that on every tick.
    #[test]
    fn placement_is_stable() {
        let desired = [nest("a", "mainnet", 90), nest("b", "arbitrum-one", 90)];
        let current = [at("mainnet", "w1"), at("arbitrum-one", "w1")];
        let p = plan(&[Worker::new("w1"), Worker::new("w2")], &desired, &current);
        assert!(
            p.assign.is_empty() && p.drain.is_empty(),
            "both fit on w1, so an idle w2 is not a reason to move anything: {p:?}"
        );
    }

    /// A worker leaving means its cursors need somewhere else to go.
    #[test]
    fn a_departed_worker_has_its_cursors_replaced() {
        let desired = [nest("a", "mainnet", 90)];
        let current = [at("mainnet", "gone")];
        let p = plan(&[Worker::new("w1")], &desired, &current);
        assert_eq!(p.drain, vec![at("mainnet", "gone")]);
        assert_eq!(p.assign, vec![at("mainnet", "w1")]);
    }

    /// A nest the operator removed should stop running.
    #[test]
    fn an_undesired_cursor_is_drained() {
        let p = plan(&[Worker::new("w1")], &[], &[at("mainnet", "w1")]);
        assert_eq!(p.drain, vec![at("mainnet", "w1")]);
        assert!(p.assign.is_empty());
    }

    /// The budget is a budget. Over-committing a worker is the failure the per-cursor ceiling exists
    /// to prevent, and a scheduler that ignores it is a more elaborate way to breach it.
    #[test]
    fn a_worker_is_never_committed_past_its_budget() {
        let big = ROOST_BASE_RSS_MB + 900;
        let desired = [
            nest("a", "c1", 900),
            nest("b", "c2", 900),
            nest("c", "c3", 900),
        ];
        let workers = [
            Worker {
                id: "w1".into(),
                budget_mb: big * 2,
            },
            Worker {
                id: "w2".into(),
                budget_mb: big,
            },
        ];
        let p = plan(&workers, &desired, &[]);
        let mut per_worker: BTreeMap<&str, u64> = BTreeMap::new();
        let cursors = cursors_of(&desired);
        for a in &p.assign {
            let c = cursors.iter().find(|c| c.chain == a.chain).unwrap();
            *per_worker.entry(a.worker.as_str()).or_insert(0) += c.rss_mb;
        }
        for w in &workers {
            let used = per_worker.get(w.id.as_str()).copied().unwrap_or(0);
            assert!(
                used <= w.budget_mb,
                "{} committed {used}MB against a {}MB budget",
                w.id,
                w.budget_mb
            );
        }
        assert_eq!(p.assign.len(), 3, "all three fit across the two workers");
    }

    /// Under-scheduling must be *reported*. A fleet that quietly runs less than it was asked to looks
    /// healthy while serving stale data, which is the worst combination available.
    #[test]
    fn what_cannot_be_placed_is_reported_not_dropped() {
        let desired = [nest("a", "c1", 900), nest("b", "c2", 900)];
        let workers = [Worker {
            id: "w1".into(),
            budget_mb: ROOST_BASE_RSS_MB + 900,
        }];
        let p = plan(&workers, &desired, &[]);
        assert_eq!(p.assign.len(), 1);
        assert_eq!(p.unplaceable.len(), 1);
        assert_eq!(
            p.unplaceable[0].reason,
            UnplaceableReason::NoRoomRightNow,
            "adding a worker fixes this one, and the message must say so"
        );
    }

    /// "Too big for anyone" is a different problem from "no room right now" - one is fixed by adding
    /// capacity and the other never is. Collapsing them would send an operator shopping for hardware
    /// that cannot help.
    #[test]
    fn an_oversized_cursor_is_distinguished_from_a_full_fleet() {
        let desired = [nest("huge", "c1", 10_000)];
        let p = plan(&[Worker::new("w1")], &desired, &[]);
        assert!(p.assign.is_empty());
        assert_eq!(
            p.unplaceable[0].reason,
            UnplaceableReason::TooLargeForAnyWorker
        );
        assert!(p.unplaceable[0]
            .reason
            .to_string()
            .contains("adding workers will not help"));
    }

    #[test]
    fn no_workers_is_its_own_reason() {
        let p = plan(&[], &[nest("a", "c1", 10)], &[]);
        assert_eq!(p.unplaceable[0].reason, UnplaceableReason::NoWorkers);
    }

    /// Largest-first placement, with a case that genuinely discriminates.
    ///
    /// Cursors of 700/600/400/300 MB across two 1000 MB workers pack perfectly largest-first
    /// (700+300, 600+400) and **fail** smallest-first: 300 and 600 land together, 400 goes to the
    /// other worker, and the 700 then fits nowhere. An earlier version of this test used sizes that
    /// packed either way, so it asserted the ordering without testing it - the mutation that reversed
    /// the sort passed it cleanly.
    #[test]
    fn large_cursors_are_placed_before_small_ones() {
        // `rss_mb` is ROOST_BASE + the nest estimate, so subtract the base to hit round cursor sizes.
        let desired = [
            nest("a", "c700", 700 - ROOST_BASE_RSS_MB),
            nest("b", "c600", 600 - ROOST_BASE_RSS_MB),
            nest("c", "c400", 400 - ROOST_BASE_RSS_MB),
            nest("d", "c300", 300 - ROOST_BASE_RSS_MB),
        ];
        let workers = [
            Worker {
                id: "w1".into(),
                budget_mb: 1000,
            },
            Worker {
                id: "w2".into(),
                budget_mb: 1000,
            },
        ];
        let p = plan(&workers, &desired, &[]);
        assert!(
            p.unplaceable.is_empty(),
            "largest-first packs all four; smallest-first strands the 700MB cursor: {:?}",
            p.unplaceable
        );
        assert_eq!(p.assign.len(), 4);
    }

    /// Two schedulers seeing the same world must produce the same plan, or a handover between them
    /// looks like a change and churns cursors.
    #[test]
    fn the_plan_is_deterministic_regardless_of_input_order() {
        let a = [
            nest("x", "c2", 100),
            nest("y", "c1", 100),
            nest("z", "c3", 100),
        ];
        let b = [
            nest("z", "c3", 100),
            nest("x", "c2", 100),
            nest("y", "c1", 100),
        ];
        let workers = [Worker::new("w2"), Worker::new("w1")];
        let workers_rev = [Worker::new("w1"), Worker::new("w2")];
        assert_eq!(plan(&workers, &a, &[]), plan(&workers, &b, &[]));
        assert_eq!(
            plan(&workers, &a, &[]).assign,
            plan(&workers_rev, &b, &[]).assign,
            "worker declaration order must not change the outcome"
        );
    }

    /// A cursor is never assigned to two workers - the single-owner guarantee, at the planning layer
    /// rather than only at the lease.
    #[test]
    fn a_cursor_is_never_assigned_twice() {
        let desired: Vec<DesiredNest> = (0..12)
            .map(|i| nest(&format!("n{i}"), &format!("chain-{i}"), 100))
            .collect();
        let workers: Vec<Worker> = (0..4).map(|i| Worker::new(format!("w{i}"))).collect();
        let p = plan(&workers, &desired, &[]);
        let mut seen = BTreeSet::new();
        for a in &p.assign {
            assert!(seen.insert(a.chain.clone()), "{} placed twice", a.chain);
        }
    }
}
