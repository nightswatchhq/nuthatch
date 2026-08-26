//! RFC-0041 slice 2: the runtime lifecycle for an authored incremental entity (#864).
//!
//! This is deliberately the same shape as [`crate::views::BalanceView`], because §5.1 says the
//! lifecycle is the one already proved by the built-in circuits rather than a second one:
//!
//! > Backfill uses larger batches, but not different semantics.
//!
//! A circuit on its own thread, a channel of weighted batches, a health flag that latches false when
//! the thread dies, and a flush barrier. Three things are added that the built-in views do not need.
//!
//! **The circuit comes from the entity's plan** (#870). This used to drive
//! [`authored_entity_spike::Spike`](crate::authored_entity_spike::Spike), whose input relations were
//! fixed Rust structs for one hardcoded query - so the lifecycle was real and the entity was not.
//! It now drives [`EntityCircuit`], built from a [`Plan`], fed through a [`Binding`] that turns a
//! decoded window into that plan's input relations (§5.1).
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

use anyhow::{anyhow, Context, Result};
use std::sync::mpsc::{channel, sync_channel, Sender, SyncSender};
use std::sync::{Arc, RwLock};

use crate::entity_bind::Binding;
use crate::entity_circuit::EntityCircuit;
use crate::entity_plan::{Plan, Relation};
use crate::entity_row::Row;
use crate::registry::{DecodeRegistry, DecodedRow};
use dbsp::ZWeight;

/// One window's facts, already bound to the plan's input relations and weighted.
///
/// Binding happens on the caller's thread rather than the circuit's, so a window that cannot be
/// converted - a `uint256` too wide to be exact, say - is an error the ingest path sees at the
/// point it fed the window, not a circuit that dies later for reasons it can no longer explain.
pub struct Batch {
    pub left: Vec<(Row, ZWeight)>,
    pub right: Vec<(Row, ZWeight)>,
}

impl Batch {
    fn weight(&self) -> i64 {
        self.left
            .iter()
            .chain(self.right.iter())
            .map(|(_, w)| *w)
            .sum()
    }
}

enum Msg {
    /// A weighted batch, and the block it carries the entity through once folded.
    Batch(Box<Batch>, u64),
    Flush(SyncSender<()>),
}

/// What the entity currently answers with, published as one value.
///
/// The relation and the watermark used to be a lock and an atomic, updated in that order with a
/// comment explaining why the order mattered. It is not a comment's job to hold an invariant: a
/// reader between the two stores sees a head whose rows are not published yet, and no test can catch
/// that reliably. One lock, one publication, and the ordering has nowhere to go wrong.
#[derive(Default)]
struct Published {
    relation: Relation,
    /// The dataset head this entity has **actually folded**, which is not the nest's head while it
    /// is catching up. Advanced only after the batch carrying it has been applied.
    through: u64,
    /// When `through` last actually moved, in unix seconds.
    ///
    /// **Progress, not duration** - the lesson #846 paid for one layer down, where `/ready`
    /// suppressed every stall term during a bulk seal and put nothing in its place, so a pass frozen
    /// for ten hours answered `200 {"ready":true}`. An entity catching up is the same shape:
    /// legitimately behind, indefinitely, and indistinguishable from dead unless something watches
    /// it advance. Zero until the first batch lands, which is "no progress yet" rather than 1970.
    progress_at: u64,
}

/// One maintained authored entity: its circuit, its state, and how far it has been applied.
pub struct EntityView {
    name: String,
    /// The output columns' names, in the order the relation's key then aggregates appear.
    ///
    /// Carried here rather than on [`Plan`] because a circuit and a batch evaluator are positional
    /// and have no use for a name; only the serving surface does (#822).
    columns: Vec<String>,
    tx: Sender<Msg>,
    binding: Binding,
    state: Arc<RwLock<Published>>,
    /// Why this entity holds no answer despite the nest being fine - today, only a warm restart.
    ///
    /// Kept apart from `fault` on purpose. A faulted entity had a circuit that died; an unavailable
    /// one never had state to lose. Reporting them as the same thing would make #866's fault
    /// reporting say something untrue about a nest that is working.
    unavailable: Option<String>,
    /// Why the circuit thread stopped, if it has - on start, on a step, or on the declared bound.
    /// §5.2: "Serving frozen derived state as healthy is not graceful degradation; it is a lie with
    /// a pleasant HTTP status."
    ///
    /// The reason and not merely the fact, because #866 asks for an entity that is *visibly* dead,
    /// and "unhealthy" without a cause sends whoever is on call to the logs of a process that may
    /// since have restarted.
    fault: Arc<RwLock<Option<String>>>,
}

impl EntityView {
    /// Spawn the circuit on its own thread. DBSP drives worker threads; keep it off the async pool,
    /// exactly as `BalanceView::start` does.
    ///
    /// The plan is bound against `registry` here, so an entity naming a column this nest's ABI does
    /// not have is refused now rather than at the first block that would have used it.
    /// `warm` says this nest already has indexed history behind it.
    ///
    /// **A warm start makes the entity unavailable, deliberately.** Entity state is derived and not
    /// persisted, and it cannot be rebuilt yet: sealing *prunes* the sealed rows from the hot store,
    /// so replaying what is left would cover only the unsealed tail and produce a relation that is
    /// missing all of history while looking perfectly populated. Rebuilding from sealed Parquet is
    /// §5.3's warm-restart seed, which is not built.
    ///
    /// Feeding it from the cursor onward would be worse than leaving it empty: a relation holding
    /// *some* of the answer is the "plausible partial relation served as current" §5.1 forbids,
    /// where an empty one at least cannot be mistaken for the truth. So it is fed nothing and says
    /// why.
    pub fn start(
        name: &str,
        plan: &Plan,
        columns: &[String],
        registry: &DecodeRegistry,
        max_rows: usize,
        warm: bool,
    ) -> Result<Self> {
        if max_rows == 0 {
            return Err(anyhow!(
                "entity `{name}` declares max_rows = 0, which admits nothing. §7 wants a bound that \
                 bites, not one that forbids the entity outright"
            ));
        }
        let binding = Binding::bind(plan, registry)
            .with_context(|| format!("binding entity `{name}` to this nest's tables"))?;
        let unavailable = warm.then(|| {
            format!(
                "entity `{name}` cannot be rebuilt after a restart: its state is derived and not \
                 persisted, and sealing prunes sealed rows from the hot store, so replaying what \
                 remains would cover only the unsealed tail. RFC-0041 §5.3's warm-restart seed is \
                 not implemented. Re-index this nest from its start block to repopulate it."
            )
        });

        let (tx, rx) = channel::<Msg>();
        let state = Arc::new(RwLock::new(Published::default()));
        let shared = state.clone();
        let fault = Arc::new(RwLock::new(None::<String>));
        let stopped = fault.clone();
        let plan = plan.clone();
        let label = name.to_string();
        let thread_label = label.clone();

        std::thread::Builder::new()
            .name(format!("nuthatch-entity-{name}"))
            .spawn(move || {
                let mut circuit = match EntityCircuit::build(plan) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("entity `{thread_label}` circuit failed to start: {e:#}");
                        *stopped.write().unwrap() = Some(format!("{e:#}"));
                        return;
                    }
                };
                // The live input cardinality, as a running total rather than a copy of the input.
                //
                // The spike kept a map of every fact and its weight in order to count them, which is
                // an entire second copy of the input inside a budget measured in gigabytes (§7).
                // Weights sum, so a signed running total is exact for a multiset and costs eight
                // bytes - and it counts **both** input relations, which is #838: the spike's bound
                // watched the delegations and admitted fifty thousand indexer facts at a declared
                // bound of one.
                let mut live: i64 = 0;
                while let Ok(msg) = rx.recv() {
                    match msg {
                        Msg::Batch(batch, through) => {
                            match step(&mut circuit, &batch, &mut live, max_rows) {
                                Err(e) => {
                                    // Includes crossing the declared `max_rows`, which is a fault
                                    // and not a warning (criterion 10). The watermark deliberately
                                    // does not advance: whatever this batch carried, the entity did
                                    // not fold it.
                                    tracing::error!("entity `{thread_label}` step failed: {e:#}");
                                    *stopped.write().unwrap() = Some(format!("{e:#}"));
                                    break;
                                }
                                Ok(()) => {
                                    // The rows and the head they account for, published together.
                                    let mut published = shared.write().unwrap();
                                    let moved = through > published.through;
                                    *published = Published {
                                        relation: circuit.relation().clone(),
                                        through,
                                        // Stamped only when the watermark actually moves. A window
                                        // that carries the entity no further is not progress, and
                                        // counting it as such is how a wedged entity looks busy.
                                        progress_at: if moved {
                                            crate::metrics::now_unix()
                                        } else {
                                            published.progress_at
                                        },
                                    };
                                }
                            }
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
            columns: columns.to_vec(),
            tx,
            binding,
            state,
            unavailable,
            fault,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The output columns' names - key columns first, then aggregates.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The relation as JSON objects keyed by column name, which is the shape `/sql` needs.
    ///
    /// **This copies every maintained row, per call** - #822 criterion 7's separate term, and it is
    /// what the analytical seam is: `/sql` builds a `HashMap<String, Vec<Value>>` of hot rows and
    /// defines a view over it. Recorded rather than optimised away by instinct; the criterion says to
    /// measure it first and only act if it is material.
    pub fn rows_as_json(&self) -> Vec<serde_json::Value> {
        let state = match self.state.read() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        state
            .relation
            .iter()
            .map(|(k, v)| {
                let mut obj = serde_json::Map::new();
                for (name, cell) in self.columns.iter().zip(k.0.iter().chain(v.0.iter())) {
                    obj.insert(name.clone(), serde_json::Value::String(cell.to_string()));
                }
                serde_json::Value::Object(obj)
            })
            .collect()
    }

    /// Whether the circuit thread is alive and folding. `false` means it died - on start, on a step,
    /// or on the declared bound - and the ingest loop treats that as terminal rather than serving a
    /// frozen relation. A clean shutdown drops the sender and exits without flipping this.
    pub fn is_healthy(&self) -> bool {
        self.fault.read().map(|f| f.is_none()).unwrap_or(false)
    }

    /// Why this entity holds no answer, if it does not. `None` means it is maintaining normally.
    pub fn unavailable(&self) -> Option<&str> {
        self.unavailable.as_deref()
    }

    /// The decoded tables this entity reads - one, or two when it joins.
    pub fn tables(&self) -> Vec<&str> {
        std::iter::once(self.binding.left.table.as_str())
            .chain(self.binding.right.as_ref().map(|r| r.table.as_str()))
            .collect()
    }

    /// Bring a warm-started entity to life with the history it missed (RFC-0041 §5.3).
    ///
    /// Called once, at startup, with **every** fact the nest holds - sealed and hot. It clears the
    /// `unavailable` state on the way in, because until this lands the entity is deliberately fed
    /// nothing at all.
    ///
    /// §5.1 says backfill and tip are one path differing only in batch size, and this is that taken
    /// literally: **the seed is a backfill batch**. There is no separate seed relation to combine,
    /// no per-aggregate combine rule, and no question about a finalized row joining a hot one -
    /// which is where §5.3's base-plus-delta wording comes apart, since such a pair is in neither
    /// half of it. Criterion 5's "matches uninterrupted execution" holds by construction rather than
    /// by an argument about algebra that has to be re-checked for every aggregate anyone adds.
    ///
    /// The cost is the one §5.3 already accepts: *"This pays the historical computation once per
    /// restart rather than once per request."* `max_rows` applies to the seed like any other window,
    /// so an entity whose history does not fit its declared bound cannot be restarted - which is the
    /// correct answer, since it could not have been running in the first place.
    pub fn seed(&mut self, rows: &[DecodedRow], through: u64) -> Result<()> {
        self.unavailable = None;
        self.apply_window(rows, 1, through)
            .with_context(|| format!("seeding entity `{}` from stored history", self.name))?;
        self.flush();
        if let Some(why) = self.fault() {
            return Err(anyhow!("seeding entity `{}` faulted: {why}", self.name));
        }
        Ok(())
    }

    /// Why this entity stopped, if it has. `None` is a live entity.
    pub fn fault(&self) -> Option<String> {
        self.fault.read().ok().and_then(|f| f.clone())
    }

    /// **§5.1 and §5.2 in one call.** Convert a decoded window to this entity's input relations and
    /// enqueue it at `weight`.
    ///
    /// `+1` is a window of canonical facts, from backfill or from tip - §5.1 says the two differ in
    /// batch size and not in semantics. `-1` is the same rows fed back before deletion, which is all
    /// a reorg is here (§5.2). The caller feeds the removed rows at `-1` and the replacements at
    /// `+1`; there is no rollback interface because there is nothing to roll back.
    ///
    /// A window carrying nothing for this entity is still progress and still advances the watermark:
    /// the entity is current through that block, having correctly folded no facts.
    pub fn apply_window(&self, rows: &[DecodedRow], weight: ZWeight, through: u64) -> Result<()> {
        // An unavailable entity is fed nothing at all. Half an answer is the failure mode; none is
        // merely an absence, and `unavailable()` is what says so.
        if self.unavailable.is_some() {
            return Ok(());
        }
        let (left, right) = self
            .binding
            .window(rows)
            .with_context(|| format!("converting a window for entity `{}`", self.name))?;
        self.apply(
            Batch {
                left: left.into_iter().map(|r| (r, weight)).collect(),
                right: right.into_iter().map(|r| (r, weight)).collect(),
            },
            through,
        );
        Ok(())
    }

    /// Enqueue an already-bound batch. Non-blocking, and drops silently if the thread has died -
    /// which `is_healthy` is what reports, not this.
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
    pub fn relation(&self) -> Relation {
        self.state
            .read()
            .map(|s| s.relation.clone())
            .unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.state.read().map(|s| s.relation.len()).unwrap_or(0)
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
        self.state.read().map(|s| s.through).unwrap_or(0)
    }

    /// When the applied-through watermark last moved, in unix seconds; `0` before the first batch.
    pub fn last_progress(&self) -> u64 {
        self.state.read().map(|s| s.progress_at).unwrap_or(0)
    }

    /// Is this entity current for a dataset advertising `head`?
    pub fn is_current(&self, head: u64) -> bool {
        self.applied_through() >= head
    }
}

/// Fold one batch, bound first.
///
/// The bound is checked **before** the batch reaches the circuit, not after. Checking afterwards
/// means the memory has already been allocated, which is the difference between a bound that
/// prevents an over-budget cursor and one that reports it. Worth saying plainly that the tests below
/// cannot tell the two orders apart - they prove the fault, not the allocation - so this ordering is
/// a design choice held by this comment and not by a red test.
fn step(circuit: &mut EntityCircuit, batch: &Batch, live: &mut i64, max_rows: usize) -> Result<()> {
    let next = live
        .checked_add(batch.weight())
        .ok_or_else(|| anyhow!("the entity's live input row count overflowed i64"))?;
    if next < 0 {
        return Err(anyhow!(
            "this batch retracts {} more rows than the entity holds. A reorg feeds back rows that \
             were applied; retracting rows that were not is a bookkeeping fault, not a small one",
            -next
        ));
    }
    if next as usize > max_rows {
        return Err(anyhow!(
            "RFC-0041 max_rows exceeded: {next} live input rows across both relations, declared \
             bound {max_rows}"
        ));
    }
    circuit.apply(&batch.left, &batch.right)?;
    *live = next;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_expr::{Cmp, Expr};
    use crate::entity_plan::{Agg, Join, Source};
    use crate::entity_row::Scalar;
    use crate::registry::ContractSpec;
    use crate::rpc::Log;
    use proptest::prelude::*;

    const ERC20: &str = r#"[
        {"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true},
            {"name":"to","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false},
        {"type":"event","name":"Approval","inputs":[
            {"name":"owner","type":"address","indexed":true},
            {"name":"spender","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false}
    ]"#;
    const TRANSFER_TOPIC0: &str =
        "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    const APPROVAL_TOPIC0: &str =
        "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925";
    const TOKEN: &str = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
    const ALICE: &str = "0x1111111111111111111111111111111111111111";
    const BOB: &str = "0x2222222222222222222222222222222222222222";

    fn registry() -> DecodeRegistry {
        let abi: alloy_json_abi::JsonAbi = serde_json::from_str(ERC20).unwrap();
        DecodeRegistry::build(vec![ContractSpec {
            alias: "usdc".into(),
            address: TOKEN.parse().unwrap(),
            abi,
            events: Vec::new(),
        }])
        .unwrap()
    }

    fn log(topic0: &str, a: &str, b: &str, value: &str, block: u64, li: u64) -> Log {
        Log {
            address: TOKEN.into(),
            topics: vec![
                topic0.into(),
                format!("0x{:0>64}", a.trim_start_matches("0x")),
                format!("0x{:0>64}", b.trim_start_matches("0x")),
            ],
            data: format!("0x{:0>64}", value),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xtx".into(),
            log_index: li,
        }
    }

    fn decode(reg: &DecodeRegistry, logs: &[Log]) -> Vec<DecodedRow> {
        logs.iter()
            .map(|l| reg.decode(l).unwrap().expect("a decoder must match"))
            .collect()
    }

    fn src(t: &str, cols: &[&str]) -> Source {
        Source {
            table: t.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// `SELECT to, SUM(value) FROM usdc__transfer WHERE value > 0 GROUP BY to`
    /// Column names for the fixtures below. The serving surface needs them; a circuit does not.
    fn cols() -> Vec<String> {
        vec!["to".into(), "sum_value".into()]
    }

    fn received() -> Plan {
        Plan {
            left: src("usdc__transfer", &["to", "value"]),
            left_filter: Some(Expr::Compare(
                Cmp::Gt,
                Expr::Column(1).into(),
                Expr::Literal(Scalar::Int(0)).into(),
            )),
            join: None,
            key: vec![Expr::Column(0)],
            aggregates: vec![Agg::Sum(Expr::Column(1))],
        }
    }

    /// The same, joined to the approvals table, so both input relations carry rows.
    fn received_by_approved() -> Plan {
        Plan {
            join: Some(Join {
                right: src("usdc__approval", &["owner"]),
                right_filter: None,
                on: (0, 0),
            }),
            ..received()
        }
    }

    fn bob() -> Row {
        Row(vec![Scalar::Str(BOB.into())])
    }

    #[test]
    fn a_decoded_window_is_folded_and_carries_the_watermark_with_it() {
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1_000, false).unwrap();
        assert_eq!(v.applied_through(), 0, "nothing folded yet");

        let rows = decode(
            &reg,
            &[
                log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0),
                log(TRANSFER_TOPIC0, ALICE, BOB, "5", 100, 1),
            ],
        );
        v.apply_window(&rows, 1, 100).unwrap();
        v.flush();

        assert!(v.is_healthy());
        assert_eq!(
            v.relation().get(&bob()),
            Some(&Row(vec![Scalar::Int(12)])),
            "0x7 + 0x5, summed by a circuit built from the plan"
        );
        assert_eq!(v.applied_through(), 100);
        assert!(v.is_current(100));
        assert!(!v.is_current(101), "it has not folded 101");
    }

    #[test]
    fn a_reorg_retracts_at_minus_one_and_converges_on_the_replacement() {
        // §5.2: removed rows are fed at -1 before deletion, replacements arrive at +1.
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1_000, false).unwrap();

        let orphaned = decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0)]);
        let replacement = decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, BOB, "9", 100, 0)]);

        v.apply_window(&orphaned, 1, 100).unwrap();
        v.apply_window(&orphaned, -1, 100).unwrap();
        v.apply_window(&replacement, 1, 101).unwrap();
        v.flush();

        assert_eq!(v.relation().get(&bob()), Some(&Row(vec![Scalar::Int(9)])));
        assert_eq!(v.applied_through(), 101);
        assert!(v.is_healthy());
    }

    /// **#838.** The spike's bound counted the delegation input alone, so fifty thousand indexer
    /// facts were admitted at a declared bound of one. A bound that watches one of two relations is
    /// not a bound on the entity's footprint.
    #[test]
    fn max_rows_counts_both_input_relations() {
        let reg = registry();
        let v = EntityView::start("received", &received_by_approved(), &cols(), &reg, 3, false)
            .unwrap();

        // One left row, and four right rows. The left side alone never crosses three.
        let window = decode(
            &reg,
            &[
                log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0),
                log(APPROVAL_TOPIC0, BOB, ALICE, "1", 100, 1),
                log(APPROVAL_TOPIC0, BOB, ALICE, "2", 100, 2),
                log(APPROVAL_TOPIC0, BOB, ALICE, "3", 100, 3),
                log(APPROVAL_TOPIC0, BOB, ALICE, "4", 100, 4),
            ],
        );
        v.apply_window(&window, 1, 100).unwrap();
        v.flush();

        assert!(
            !v.is_healthy(),
            "five input rows against a declared bound of three is a fault"
        );
        let why = v.fault().unwrap_or_default();
        assert!(why.contains("max_rows exceeded"), "{why}");
        assert!(why.contains("both relations"), "{why}");
        assert_eq!(v.applied_through(), 0, "and the watermark did not move");
    }

    #[test]
    fn crossing_max_rows_faults_the_circuit_rather_than_warning() {
        // Criterion 10: neither warns-and-continues nor OOMs the cursor.
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1, false).unwrap();
        let rows = decode(
            &reg,
            &[
                log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0),
                log(TRANSFER_TOPIC0, ALICE, ALICE, "5", 100, 1),
            ],
        );
        v.apply_window(&rows, 1, 100).unwrap();
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

    /// The bound is on the entity's live footprint, not on one window's size. An entity fed one row
    /// at a time still crosses it, and a check that only ever looks at the batch in hand never sees
    /// that happen.
    #[test]
    fn the_bound_is_on_what_the_entity_holds_not_on_one_windows_size() {
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 2, false).unwrap();
        for (i, to) in [BOB, ALICE, TOKEN].iter().enumerate() {
            let block = 100 + i as u64;
            v.apply_window(
                &decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, to, "7", block, 0)]),
                1,
                block,
            )
            .unwrap();
        }
        v.flush();

        assert!(!v.is_healthy(), "three rows against a bound of two");
        assert_eq!(
            v.applied_through(),
            101,
            "it folded the first two windows and refused the third"
        );
    }

    /// Retraction gives the footprint back. An entity that reorgs its way down below the bound is
    /// not over it, and a running total that only ever climbs would fault on a nest that never grew.
    #[test]
    fn a_retraction_returns_the_footprint_it_took() {
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 2, false).unwrap();
        let first = decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0)]);
        let second = decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, ALICE, "5", 101, 0)]);
        let third = decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, TOKEN, "3", 102, 0)]);

        v.apply_window(&first, 1, 100).unwrap();
        v.apply_window(&second, 1, 101).unwrap();
        v.apply_window(&first, -1, 102).unwrap();
        v.apply_window(&third, 1, 103).unwrap();
        v.flush();

        assert!(
            v.is_healthy(),
            "two live rows throughout, against a bound of two"
        );
        assert_eq!(v.applied_through(), 103);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn the_watermark_does_not_advance_past_a_batch_the_circuit_refused() {
        // The stale-serving guard criterion 2 asks for: an entity that stopped folding at 100 must
        // not answer for 200 merely because a later batch was enqueued.
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1, false).unwrap();
        v.apply_window(
            &decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0)]),
            1,
            100,
        )
        .unwrap();
        v.flush();
        assert_eq!(v.applied_through(), 100);

        v.apply_window(
            &decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, ALICE, "5", 200, 0)]),
            1,
            200,
        )
        .unwrap();
        v.flush();
        assert!(!v.is_healthy());
        assert_eq!(v.applied_through(), 100, "it did not fold 200");
        assert!(!v.is_current(200));
    }

    #[test]
    fn an_empty_window_still_advances_the_watermark() {
        // A window with no facts for this entity is progress: the entity is current through it. The
        // built-in views can skip an empty batch because they have no watermark to move.
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1_000, false).unwrap();
        v.apply_window(&[], 1, 500).unwrap();
        v.flush();
        assert!(v.is_healthy());
        assert_eq!(v.applied_through(), 500);
        assert!(v.is_empty());
    }

    /// A window carrying only other tables' rows is the ordinary case - a nest decodes many tables
    /// and an entity reads one or two - and it must be progress rather than a fault.
    #[test]
    fn a_window_of_other_tables_is_progress_not_a_fault() {
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1_000, false).unwrap();
        v.apply_window(
            &decode(&reg, &[log(APPROVAL_TOPIC0, ALICE, BOB, "1", 300, 0)]),
            1,
            300,
        )
        .unwrap();
        v.flush();
        assert!(v.is_healthy());
        assert_eq!(v.applied_through(), 300);
        assert!(v.is_empty(), "the approval is not this entity's table");
    }

    /// Retracting rows that were never applied means the caller and the circuit disagree about what
    /// the entity holds. Continuing from there produces answers neither of them can account for.
    #[test]
    fn retracting_more_rows_than_were_applied_is_a_fault() {
        let reg = registry();
        let v = EntityView::start("received", &received(), &cols(), &reg, 1_000, false).unwrap();
        v.apply_window(
            &decode(&reg, &[log(TRANSFER_TOPIC0, ALICE, BOB, "7", 100, 0)]),
            -1,
            100,
        )
        .unwrap();
        v.flush();
        assert!(!v.is_healthy());
        assert_eq!(v.applied_through(), 0);
        // The *reason*, not merely the fact. Without this the test passes on a build where the
        // negative count wraps into the `max_rows` comparison instead - the right refusal reached by
        // the wrong route, reported to whoever is on call as a bound they have not crossed.
        let why = v.fault().unwrap_or_default();
        assert!(why.contains("retracts"), "{why}");
        assert!(!why.contains("max_rows"), "{why}");
    }

    /// An entity naming a column this nest's ABI does not have is refused when it starts, not at the
    /// first block that would have used it.
    #[test]
    fn an_entity_that_does_not_bind_is_refused_at_start() {
        let reg = registry();
        let plan = Plan {
            left: src("usdc__transfer", &["to", "amount"]),
            ..received()
        };
        let err = format!(
            "{:#}",
            EntityView::start("received", &plan, &cols(), &reg, 1_000, false)
                .err()
                .expect("an unbindable entity must not start")
        );
        assert!(err.contains("no column amount"), "{err}");
    }

    #[test]
    fn a_declared_bound_of_zero_is_refused() {
        let reg = registry();
        let err = format!(
            "{:#}",
            EntityView::start("received", &received(), &cols(), &reg, 0, false)
                .err()
                .expect("a bound of zero must not start")
        );
        assert!(err.contains("admits nothing"), "{err}");
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, ..ProptestConfig::default() })]

        /// **Criterion 4.** A random sequence of applies and retractions must converge to a clean
        /// replay of the surviving facts - which is what a reorg of any depth is, at this layer.
        #[test]
        fn random_apply_and_retract_sequences_converge_on_a_clean_replay(
            facts in prop::collection::vec((0u8..3, 1u64..20, any::<bool>()), 0..10),
        ) {
            let reg = registry();
            let who = [ALICE, BOB, TOKEN];

            let live = EntityView::start("received", &received(), &cols(), &reg, 1_000, false).unwrap();
            let replay = EntityView::start("replay", &received(), &cols(), &reg, 1_000, false).unwrap();

            let mut survivors: Vec<DecodedRow> = Vec::new();
            for (i, (to, value, orphaned)) in facts.iter().enumerate() {
                let block = 100 + i as u64;
                let row = decode(
                    &reg,
                    &[log(TRANSFER_TOPIC0, ALICE, who[*to as usize], &format!("{value:x}"), block, 0)],
                );
                live.apply_window(&row, 1, block).unwrap();
                if *orphaned {
                    // The reorg: fed back at -1 before deletion, exactly as balances work today.
                    live.apply_window(&row, -1, block).unwrap();
                } else {
                    survivors.extend(row);
                }
            }

            // The clean replay: only the facts that survived, applied once each.
            for (i, row) in survivors.iter().enumerate() {
                replay.apply_window(std::slice::from_ref(row), 1, 100 + i as u64).unwrap();
            }

            live.flush();
            replay.flush();
            prop_assert!(live.is_healthy() && replay.is_healthy());
            // Two empty relations compare equal, so say what a pass has to mean: every surviving
            // fact has a value in 1..20 and the filter admits anything above zero, so a non-empty
            // survivor set must produce a non-empty entity. Without this the property holds
            // perfectly on a circuit that folded nothing at all.
            prop_assert_eq!(
                live.relation().is_empty(),
                survivors.is_empty(),
                "survivors: {}, live rows: {}",
                survivors.len(),
                live.len()
            );
            prop_assert_eq!(live.relation(), replay.relation());
        }
    }
}
