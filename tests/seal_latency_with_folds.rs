//! RFC-0059 S2's seal-latency criterion, as an operator measurement: does the fold checkpoint writer
//! slow the seal? Seals of one batch of synthetic rows are timed with no writer, twice (the
//! run-to-run noise), then while the writer walks the nest's whole sealed history from no checkpoint,
//! the heaviest load it can put on the box. Ignored in CI: it needs a real corpus it may write to.
//!
//! `NUTHATCH_SEAL_BENCH_DIR` is a throwaway copy of a nest with `folds/` and no `checkpoints/`.

use std::time::Instant;

use nuthatch::{folds, metrics::NestMetrics, seal};

const SEALS: usize = 30;
const ROWS: u64 = 20_000;

fn batch(start: u64) -> (u64, u64, Vec<String>) {
    let rows = (0..ROWS)
        .map(|i| {
            serde_json::json!({
                "table": "bench__seal",
                "block_number": start + i / 200,
                "log_index": i % 200,
                "v": format!("{:064x}", start * ROWS + i),
            })
            .to_string()
        })
        .collect();
    (start, start + ROWS / 200 - 1, rows)
}

/// Milliseconds for each of `SEALS` seals, starting at block `next`, which it advances.
fn seals(dir: &std::path::Path, next: &mut u64, writer: Option<&folds::Writer>) -> Vec<f64> {
    let mut out = Vec::with_capacity(SEALS);
    for _ in 0..SEALS {
        let (from, to, rows) = batch(*next);
        let t = Instant::now();
        seal::seal_range(dir, &rows, from, to).unwrap();
        out.push(t.elapsed().as_secs_f64() * 1e3);
        if let Some(w) = writer {
            w.sealed_through_advanced(to);
        }
        *next = to + 1;
    }
    out
}

fn summary(label: &str, mut ms: Vec<f64>) -> serde_json::Value {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |q: f64| ms[((ms.len() - 1) as f64 * q).round() as usize];
    serde_json::json!({"phase": label, "seals": ms.len(), "p50_ms": at(0.5), "p90_ms": at(0.9), "max_ms": at(1.0)})
}

#[test]
#[ignore = "operator check: needs NUTHATCH_SEAL_BENCH_DIR, a throwaway copy of a nest with folds/"]
fn seal_latency_with_the_fold_writer_busy() {
    let dir = std::path::PathBuf::from(
        std::env::var("NUTHATCH_SEAL_BENCH_DIR").expect("NUTHATCH_SEAL_BENCH_DIR"),
    );
    assert!(
        !dir.join(folds::CHECKPOINTS_DIR).exists(),
        "the writer must start from no checkpoint, so it is busy for the whole run"
    );
    let manifest = seal::load_manifest(&dir).unwrap();
    let sealed_through = manifest
        .tables
        .values()
        .flatten()
        .map(|s| s.to_block)
        .max()
        .unwrap();
    let mut next = sealed_through + 1_000_000;

    let off_a = seals(&dir, &mut next, None);
    let off_b = seals(&dir, &mut next, None);

    let set = folds::FoldSet::load(&dir, &[]).unwrap();
    let w = folds::Writer::start(
        dir.clone(),
        vec![],
        set,
        sealed_through,
        std::sync::Arc::new(NestMetrics::default()),
    )
    .unwrap()
    .expect("a writer");
    // Busy means walking toward the history's last block and not there yet, before the first timed
    // seal and after the last. The walk's target is fixed now, so a later notification cannot pass
    // for work still to do.
    let start = std::time::Instant::now();
    let before = loop {
        let s = w.status();
        if s.target > 0 || s.fault.is_some() || start.elapsed().as_secs() > 120 {
            break s;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert!(
        before.fault.is_none() && before.target >= sealed_through / 2,
        "the writer never began walking history: {before:?}"
    );
    assert!(before.checkpointed_through < before.target, "{before:?}");
    let on = seals(&dir, &mut next, Some(&w));
    let after = w.status();
    assert!(after.fault.is_none(), "{after:?}");
    assert!(
        after.checkpointed_through < before.target,
        "the writer reached the end of history during the timed seals, so part of them ran \
         unloaded: {after:?}"
    );

    println!(
        "{}",
        serde_json::json!({
            "rows_per_seal": ROWS,
            "phases": [summary("no writer", off_a), summary("no writer, again", off_b), summary("writer walking history", on)],
            "writer": {"checkpointed_through_after": after.checkpointed_through, "target": after.target},
        })
    );
    // The writer is mid-walk; ending the process ends it. Dropping it would wait for the walk.
    std::mem::forget(w);
}
