//! What a restart seed costs against a real nest's sealed history (#892 item 1).
//!
//! Not a gate, and not run in CI. A measurement driven by hand against a corpus that lives outside
//! the repo, so it skips when `NUTHATCH_SEED_NEST` is unset:
//!
//! ```text
//! NUTHATCH_SEED_NEST=~/Projects/graph-allocations-nest \
//! NUTHATCH_SEED_COUNT_COL=delegator \
//! NUTHATCH_SEED_SQL="SELECT delegator, SUM(tokens) AS total, COUNT(*) AS n
//!                    FROM staking_legacy__stake_delegated GROUP BY delegator" \
//!   cargo test --release --test seed_scale -- --nocapture
//! ```
//!
//! `NUTHATCH_SEED_COUNT_COL` counts the distinct values of the group column **off the reader**,
//! never through the circuit, which is what makes the group count an assertion rather than a
//! reading of itself. That is how #895 was found: 309,549 distinct delegators went in and 9,549
//! groups came out. `NUTHATCH_SEED_CHUNK` sets how many rows go in per window (0, the default,
//! feeds one sealed segment at a time, as `indexer::seed_entities` does).
use std::time::Instant;

fn rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()
}

#[test]
fn seed_cost_against_a_real_nest() {
    let Ok(dir) = std::env::var("NUTHATCH_SEED_NEST") else {
        eprintln!("set NUTHATCH_SEED_NEST to a nest directory to run this");
        return;
    };
    let sql = std::env::var("NUTHATCH_SEED_SQL").expect("set NUTHATCH_SEED_SQL");
    let dir = std::path::PathBuf::from(shellexpand(&dir));
    let chunk_at: usize = std::env::var("NUTHATCH_SEED_CHUNK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let count_col = std::env::var("NUTHATCH_SEED_COUNT_COL").ok();

    let config = nuthatch::config::Config::load(&dir).unwrap();
    let registry = nuthatch::registry::from_nest(&dir, &config).unwrap();
    let schema = registry.schema();

    let (plan, columns) = nuthatch::entity_lower::lower_with_columns(&sql).unwrap();
    let mut view = nuthatch::entity_view::EntityView::start(
        "probe", &plan, &columns, &registry, 5_000_000, true,
    )
    .unwrap();

    let base = rss_kb();
    let t0 = Instant::now();
    let mut rows = 0usize;
    let mut distinct_in: std::collections::BTreeSet<String> = Default::default();
    let mut buffered = Vec::new();
    view.seed_begin();
    for table in view.tables() {
        let ts = schema
            .iter()
            .find(|t| t.table == table)
            .expect("a table the registry describes");
        let before = rows;
        nuthatch::seal::read_table_rows_by_segment(&dir, ts, &mut |chunk| {
            rows += chunk.len();
            if let Some(col) = &count_col {
                for r in &chunk {
                    if let Some((_, v)) = r.params.iter().find(|(n, _)| n == col) {
                        distinct_in.insert(format!("{v:?}"));
                    }
                }
            }
            if chunk_at == 0 {
                return view.seed_chunk(&chunk, u64::MAX);
            }
            buffered.extend(chunk);
            if buffered.len() >= chunk_at {
                let batch = std::mem::take(&mut buffered);
                view.seed_chunk(&batch, u64::MAX)?;
            }
            Ok(())
        })
        .unwrap();
        println!("  read {:44} {:>9} rows", table, rows - before);
    }
    view.seed_chunk(&buffered, u64::MAX).unwrap();
    view.seed_end().unwrap();
    let ms = t0.elapsed().as_millis().max(1);
    let after = rss_kb();

    println!("\n=== seed against {}", dir.display());
    println!("sql   : {sql}");
    println!(
        "window: {}",
        if chunk_at == 0 {
            "one sealed segment".to_string()
        } else {
            format!("{chunk_at} rows")
        }
    );
    println!(
        "seed  : {rows:>9} rows in {ms:>7} ms  ({:.0} rows/sec)",
        rows as f64 * 1000.0 / ms as f64
    );
    let out = view.rows_as_json();
    println!("groups: {}", out.len());
    match (base, after) {
        (Some(b), Some(a)) => println!(
            "rss   : base {b} kB -> after seed {a} kB (+{} kB)",
            a.saturating_sub(b)
        ),
        _ => println!("rss   : no /proc here; peak comes from the harness (`/usr/bin/time -l`)"),
    }
    assert!(view.fault().is_none(), "faulted: {:?}", view.fault());

    // The ground truth, and the whole point of the probe: the reader counted the distinct group
    // values without the circuit's help, so the two numbers disagreeing is a defect and not a
    // reading. #895 was exactly this disagreement.
    if count_col.is_some() {
        assert_eq!(
            out.len(),
            distinct_in.len(),
            "the circuit folded {} groups from {} distinct values of the group column",
            out.len(),
            distinct_in.len()
        );
    }
}

/// `~` only. The env var is typed by a human at a shell that may not have expanded it.
fn shellexpand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    }
}
