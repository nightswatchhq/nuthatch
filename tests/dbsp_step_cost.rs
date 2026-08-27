//! **Is the per-step cost of #897 ours or DBSP's?**
//!
//! `EntityCircuit` folds a one-row window in ~0.2 µs per maintained group - flat to a hundred groups,
//! linear past that, 72 ms at 309,548 groups on the ThinkPad. Everything in `entity_circuit.rs` that
//! could explain it has been ruled out by measurement (see #897), so this asks the question with our
//! code removed: the same operator chain, built directly on dbsp, over plain `u64` keys and then over
//! our own `Row`.
//!
//! Hand-run, like the rest of the measurement suite:
//! `cargo test --release --test dbsp_step_cost -- --nocapture --ignored`
use dbsp::typed_batch::OrdIndexedZSet;
use dbsp::utils::Tup2;
use dbsp::{Runtime, Stream, ZWeight};
use std::time::Instant;

use dbsp::algebra::{AddAssignByRef, AddByRef, HasZero, MulByRef};
use rkyv::{Archive, Deserialize as RkyvDeserialize, Serialize as RkyvSerialize};
use size_of::SizeOf;

/// A stand-in for `entity_circuit::LinAcc`, which is `pub` but whose field is not - same shape, same
/// derives, same arithmetic. The variable under test is "a `Vec`-backed accumulator as DBSP's weight
/// type" against a plain `i64`.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    SizeOf,
    Archive,
    RkyvSerialize,
    RkyvDeserialize,
)]
#[archive_attr(derive(PartialEq, Eq, PartialOrd, Ord))]
pub struct BenchAcc(Vec<i128>);

dbsp::never_none!(BenchAcc);
dbsp::never_roaring_filter!(BenchAcc);

impl BenchAcc {
    fn at(&self, i: usize) -> i128 {
        self.0.get(i).copied().unwrap_or(0)
    }
    fn zip(&self, other: &Self, f: impl Fn(i128, i128) -> i128) -> Self {
        let n = self.0.len().max(other.0.len());
        BenchAcc((0..n).map(|i| f(self.at(i), other.at(i))).collect())
    }
}
impl HasZero for BenchAcc {
    fn zero() -> Self {
        BenchAcc(Vec::new())
    }
    fn is_zero(&self) -> bool {
        self.0.iter().all(|v| *v == 0)
    }
}
impl AddByRef for BenchAcc {
    fn add_by_ref(&self, other: &Self) -> Self {
        self.zip(other, |a, b| a + b)
    }
}
impl AddAssignByRef for BenchAcc {
    fn add_assign_by_ref(&mut self, other: &Self) {
        *self = self.add_by_ref(other);
    }
}
impl MulByRef<ZWeight> for BenchAcc {
    type Output = Self;
    fn mul_by_ref(&self, w: &ZWeight) -> Self {
        let w = i128::from(*w);
        BenchAcc(self.0.iter().map(|v| v * w).collect())
    }
}

/// The last variable: `EntityCircuit` indexes the **whole decoded row** as the value
/// (`left.map_index(|r| (eval_key(&keys, r), r.clone()))`), not a scalar. A decoded Horizon row is
/// ~11 columns of addresses and hashes, so the indexed zset's trace holds all of that per fact.
fn step_cost_wide(groups: u64) -> u128 {
    use nuthatch::entity_row::{Row, Scalar};
    let key = |k: u64| Row(vec![Scalar::Str(format!("0x{k:040x}"))]);
    // A row shaped like a real decoded event: block/tx/log implicits plus the event's own columns.
    let wide = |k: u64| {
        Row(vec![
            Scalar::Int(k as i128),
            Scalar::Str(format!("0x{k:064x}")),
            Scalar::Int(1_700_000_000 + k as i128),
            Scalar::Str(format!("0x{k:064x}")),
            Scalar::Int(0),
            Scalar::Str(format!("0x{k:040x}")),
            Scalar::Str(format!("0x{k:040x}")),
            Scalar::Str(format!("0x{k:040x}")),
            Scalar::Str(format!("0x{k:064x}")),
            Scalar::Int(k as i128 * 1_000_000_000),
            Scalar::Int(k as i128),
        ])
    };

    let (mut handle, (input, out)) = Runtime::init_circuit(1, move |circuit| {
        let (stream, input) = circuit.add_input_zset::<Row>();
        // Key on one column, value is the whole row - exactly what `lower` builds.
        let grouped = stream.map_index(|r: &Row| (Row(vec![r.0[5].clone()]), r.clone()));
        let agg: Stream<_, OrdIndexedZSet<Row, BenchAcc>> =
            grouped.aggregate_linear(|_r: &Row| BenchAcc(vec![1, 0]));
        let out = agg
            .map(|(k, v): (&Row, &BenchAcc)| Tup2(k.clone(), v.clone()))
            .output();
        Ok((input, out))
    })
    .unwrap();

    let mut batch: Vec<Tup2<Row, ZWeight>> = (0..groups).map(|k| Tup2(wide(k), 1)).collect();
    input.append(&mut batch);
    handle.start_transaction().unwrap();
    handle.start_commit_transaction().unwrap();
    while !handle.step().unwrap() {
        let _ = out.consolidate();
    }
    let _ = out.consolidate();
    let _ = key;

    let mut each = Vec::new();
    for i in 0..5u64 {
        let mut one = vec![Tup2(wide(i % groups.max(1)), 1)];
        input.append(&mut one);
        let t = Instant::now();
        handle.start_transaction().unwrap();
        handle.start_commit_transaction().unwrap();
        while !handle.step().unwrap() {
            let _ = out.consolidate();
        }
        let _ = out.consolidate();
        each.push(t.elapsed().as_micros());
    }
    handle.kill().unwrap();
    each[2..].iter().sum::<u128>() / 3
}

/// The same circuit again, with the `Vec`-backed accumulator in place of `i64`.
fn step_cost_acc(groups: u64) -> u128 {
    use nuthatch::entity_row::{Row, Scalar};
    let key = |k: u64| Row(vec![Scalar::Str(format!("0x{k:040x}"))]);

    let (mut handle, (input, out)) = Runtime::init_circuit(1, move |circuit| {
        let (stream, input) = circuit.add_input_zset::<Tup2<Row, i64>>();
        let grouped = stream.map_index(|Tup2(k, v): &Tup2<Row, i64>| (k.clone(), *v));
        let agg: Stream<_, OrdIndexedZSet<Row, BenchAcc>> =
            grouped.aggregate_linear(|v: &i64| BenchAcc(vec![1, *v as i128]));
        let out = agg
            .map(|(k, v): (&Row, &BenchAcc)| Tup2(k.clone(), v.clone()))
            .output();
        Ok((input, out))
    })
    .unwrap();

    let mut batch: Vec<Tup2<Tup2<Row, i64>, ZWeight>> =
        (0..groups).map(|k| Tup2(Tup2(key(k), 1i64), 1)).collect();
    input.append(&mut batch);
    handle.start_transaction().unwrap();
    handle.start_commit_transaction().unwrap();
    while !handle.step().unwrap() {
        let _ = out.consolidate();
    }
    let _ = out.consolidate();

    let mut each = Vec::new();
    for i in 0..5u64 {
        let mut one = vec![Tup2(Tup2(key(i % groups.max(1)), 1i64), 1)];
        input.append(&mut one);
        let t = Instant::now();
        handle.start_transaction().unwrap();
        handle.start_commit_transaction().unwrap();
        while !handle.step().unwrap() {
            let _ = out.consolidate();
        }
        let _ = out.consolidate();
        each.push(t.elapsed().as_micros());
    }
    handle.kill().unwrap();
    each[2..].iter().sum::<u128>() / 3
}

/// One step over a circuit maintaining `groups` groups, with plain `u64` keys and an `i64` sum.
fn step_cost_u64(groups: u64) -> u128 {
    let (mut handle, (input, out)) = Runtime::init_circuit(1, move |circuit| {
        let (stream, input) = circuit.add_input_zset::<Tup2<u64, i64>>();
        let grouped = stream.map_index(|Tup2(k, v): &Tup2<u64, i64>| (*k, *v));
        let agg: Stream<_, OrdIndexedZSet<u64, i64>> = grouped.aggregate_linear(|v: &i64| *v);
        let out = agg.map(|(k, v): (&u64, &i64)| Tup2(*k, *v)).output();
        Ok((input, out))
    })
    .unwrap();

    // Seed: one row per group, one transaction.
    let mut batch: Vec<Tup2<Tup2<u64, i64>, ZWeight>> =
        (0..groups).map(|k| Tup2(Tup2(k, 1i64), 1)).collect();
    input.append(&mut batch);
    handle.start_transaction().unwrap();
    handle.start_commit_transaction().unwrap();
    while !handle.step().unwrap() {
        let _ = out.consolidate();
    }
    let _ = out.consolidate();

    // Five one-row windows; report the steady state.
    let mut each = Vec::new();
    for i in 0..5u64 {
        let mut one = vec![Tup2(Tup2(i % groups.max(1), 1i64), 1)];
        input.append(&mut one);
        let t = Instant::now();
        handle.start_transaction().unwrap();
        handle.start_commit_transaction().unwrap();
        while !handle.step().unwrap() {
            let _ = out.consolidate();
        }
        let _ = out.consolidate();
        each.push(t.elapsed().as_micros());
    }
    handle.kill().unwrap();
    each[2..].iter().sum::<u128>() / 3
}

/// The same circuit with **our** key type: `Row(Vec<Scalar>)`, one `Scalar::Str` per key, which is
/// what `eval_key` produces for a `GROUP BY` over an address column.
fn step_cost_row(groups: u64) -> u128 {
    use nuthatch::entity_row::{Row, Scalar};
    let key = |k: u64| Row(vec![Scalar::Str(format!("0x{k:040x}"))]);

    let (mut handle, (input, out)) = Runtime::init_circuit(1, move |circuit| {
        let (stream, input) = circuit.add_input_zset::<Tup2<Row, i64>>();
        let grouped = stream.map_index(|Tup2(k, v): &Tup2<Row, i64>| (k.clone(), *v));
        let agg: Stream<_, OrdIndexedZSet<Row, i64>> = grouped.aggregate_linear(|v: &i64| *v);
        let out = agg.map(|(k, v): (&Row, &i64)| Tup2(k.clone(), *v)).output();
        Ok((input, out))
    })
    .unwrap();

    let mut batch: Vec<Tup2<Tup2<Row, i64>, ZWeight>> =
        (0..groups).map(|k| Tup2(Tup2(key(k), 1i64), 1)).collect();
    input.append(&mut batch);
    handle.start_transaction().unwrap();
    handle.start_commit_transaction().unwrap();
    while !handle.step().unwrap() {
        let _ = out.consolidate();
    }
    let _ = out.consolidate();

    let mut each = Vec::new();
    for i in 0..5u64 {
        let mut one = vec![Tup2(Tup2(key(i % groups.max(1)), 1i64), 1)];
        input.append(&mut one);
        let t = Instant::now();
        handle.start_transaction().unwrap();
        handle.start_commit_transaction().unwrap();
        while !handle.step().unwrap() {
            let _ = out.consolidate();
        }
        let _ = out.consolidate();
        each.push(t.elapsed().as_micros());
    }
    handle.kill().unwrap();
    each[2..].iter().sum::<u128>() / 3
}

#[test]
#[ignore = "a measurement, run by hand"]
fn dbsp_step_cost_by_group_count() {
    println!("\n=== one 1-row window, the same chain, two key types");
    println!(
        "  {:>8}  {:>12}  {:>12}  {:>12}  {:>12}",
        "groups", "u64 key/i64", "Row key/i64", "Row key/Vec", "wide value"
    );
    for groups in [1u64, 100, 10_000, 100_000, 300_000] {
        let plain = step_cost_u64(groups);
        let row = step_cost_row(groups);
        let acc = step_cost_acc(groups);
        let wide = step_cost_wide(groups);
        println!("  {groups:>8}  {plain:>9} µs  {row:>9} µs  {acc:>9} µs  {wide:>9} µs");
    }
}
