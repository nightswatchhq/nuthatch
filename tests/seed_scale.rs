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
//! reading of itself. That is how #894 was found: 309,549 distinct delegators went in and 9,549
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
    // reading. #894 was exactly this disagreement.
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

/// **#822 criterion 3, against a real nest.** *"An analytical query over the maintained relation does
/// not rerun the entity's defining aggregation or join over raw source tables."*
///
/// The plan itself is not capturable through nuthatch's own surfaces - `/explain` is a bind-only
/// `LIMIT 0` probe and the read-only gate refuses `EXPLAIN` outright - so this asserts the property
/// the plan would have shown, by the same method the adoption tests use: **take the raw source away
/// and see who still answers.** A query that still returns the right rows with every source segment
/// renamed out of reach cannot have scanned them.
///
/// Measured against `graph-allocations-nest` on 2026-08-27: with all 2,985 source segments renamed
/// out of reach, the maintained relation returned all 309,549 rows and the recomputing query
/// returned none. Referenced tables were `{staking_legacy__stake_delegated}` before and `{probe}`
/// after.
///
/// It also prints the **fixed cost** of a request - `SELECT 1`, which reads nothing - because on a
/// real nest that term dominates everything else here and would otherwise be read as the entity's
/// cost. 2,465 ms on a 38,428-segment nest against 263 ms on a 2,985-segment one; see #896. The
/// maintained read itself is the difference, 22 ms and 3 ms.
///
/// Run it the same way as `seed_cost_against_a_real_nest`; it skips without `NUTHATCH_SEED_NEST`.
/// It **moves segment files** inside that nest and moves them back, so point it at a corpus you can
/// afford to have interrupted - a hardlink mirror of a real nest costs nothing and is what these
/// figures were taken against.
#[test]
fn a_maintained_relation_answers_without_the_segments_it_was_built_from() {
    let Ok(dir) = std::env::var("NUTHATCH_SEED_NEST") else {
        eprintln!("set NUTHATCH_SEED_NEST to a nest directory to run this");
        return;
    };
    let sql = std::env::var("NUTHATCH_SEED_SQL").expect("set NUTHATCH_SEED_SQL");
    let reference = std::env::var("NUTHATCH_SEED_REFERENCE_SQL")
        .expect("set NUTHATCH_SEED_REFERENCE_SQL to the DuckDB equivalent of the entity's own SQL");
    // Wrapped around **both** queries, so the comparison is like for like. A panel asks for a top-N
    // or a single key, not for every maintained row; `SELECT *` is the pathological end of criterion
    // 6 ("IVM does not repeal I/O") and not what criterion 4 is about.
    let shape = std::env::var("NUTHATCH_SEED_SHAPE").unwrap_or_default();
    let wrap = |inner: &str| {
        if shape.is_empty() {
            inner.to_string()
        } else {
            format!("SELECT * FROM ({inner}) AS _shaped {shape}")
        }
    };
    let dir = std::path::PathBuf::from(shellexpand(&dir));

    let config = nuthatch::config::Config::load(&dir).unwrap();
    let registry = nuthatch::registry::from_nest(&dir, &config).unwrap();
    let schema = registry.schema();
    let (plan, columns) = nuthatch::entity_lower::lower_with_columns(&sql).unwrap();
    let mut view = nuthatch::entity_view::EntityView::start(
        "probe", &plan, &columns, &registry, 5_000_000, true,
    )
    .unwrap();

    // Seed from the sealed corpus, and note what it cost to read - that is the work the *old* view
    // paid on every request and the new one pays once per restart.
    let mut source_bytes = 0u64;
    let mut source_rows = 0usize;
    view.seed_begin();
    for table in view.tables() {
        let ts = schema.iter().find(|t| t.table == table).unwrap();
        nuthatch::seal::read_table_rows_by_segment(&dir, ts, &mut |chunk| {
            source_rows += chunk.len();
            view.seed_chunk(&chunk, u64::MAX)
        })
        .unwrap();
        for f in segment_files(&dir, table) {
            source_bytes += std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        }
    }
    view.seed_end().unwrap();

    let guard = nuthatch::analytics::QueryGuard {
        timeout: std::time::Duration::from_secs(120),
        max_rows: 5_000_000,
    };
    let empty = nuthatch::analytics::HotRows::new();
    let mut maintained = nuthatch::analytics::HotRows::new();
    maintained.insert("probe".to_string(), view.rows_as_json());

    // BEFORE: the authored SQL, recomputed over the raw source tables, as `views/*.sql` did it.
    let t = Instant::now();
    let before = nuthatch::analytics::query_hot_cold(
        &dir,
        &wrap(&reference),
        guard,
        &empty,
        u64::MAX,
        &schema,
    )
    .expect("the reference aggregation must run");
    let before_ms = t.elapsed().as_millis().max(1);

    // AFTER: the same answer, read from maintained state.
    let t = Instant::now();
    let after = nuthatch::analytics::query_hot_cold(
        &dir,
        &wrap("SELECT * FROM probe"),
        guard,
        &maintained,
        u64::MAX,
        &schema,
    )
    .expect("the maintained relation must serve");
    let after_ms = t.elapsed().as_millis().max(1);

    // The fixed cost of a request, before it reads anything at all: `define_views` builds a view
    // for every table in schema ∪ manifest ∪ hot on every query, whether or not the query names it.
    let t = Instant::now();
    let _ = nuthatch::analytics::query_hot_cold(
        &dir,
        "SELECT 1 AS one",
        guard,
        &maintained,
        u64::MAX,
        &schema,
    )
    .expect("SELECT 1 must run");
    let fixed_ms = t.elapsed().as_millis().max(1);
    let segs = std::fs::read_dir(dir.join("segments"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
        .count();

    println!("\n=== #822 criterion 3, against {}", dir.display());
    println!("manifest   : {segs} sealed segment file(s) in this nest");
    println!(
        "fixed cost : {fixed_ms} ms for `SELECT 1` - paid by every request before it reads a row"
    );
    println!(
        "shape      : {}",
        if shape.is_empty() {
            "every maintained row"
        } else {
            &shape
        }
    );
    println!("source     : {source_rows} rows across {source_bytes} bytes of sealed segments");
    println!(
        "before     : {} rows in {before_ms} ms (recomputed over the raw tables)",
        before.rows.len()
    );
    println!(
        "after      : {} rows in {after_ms} ms (read from maintained state)",
        after.rows.len()
    );
    println!(
        "referenced : before={:?} after={:?}",
        before.referenced_tables, after.referenced_tables
    );
    assert_eq!(
        before.rows.len(),
        after.rows.len(),
        "the maintained relation must return the same rows as the SQL it replaces"
    );

    // The assertion the plan would have shown. Rename every source segment out of reach; a query
    // that still answers cannot have read them.
    let moved = hide_segments(&dir, &view.tables());
    assert!(
        !moved.is_empty(),
        "no segments were hidden, so this proves nothing"
    );
    let blind = nuthatch::analytics::query_hot_cold(
        &dir,
        &wrap("SELECT * FROM probe"),
        guard,
        &maintained,
        u64::MAX,
        &schema,
    );
    let control = nuthatch::analytics::query_hot_cold(
        &dir,
        &wrap(&reference),
        guard,
        &empty,
        u64::MAX,
        &schema,
    );
    restore_segments(&moved);

    let blind = blind.expect("the maintained relation must answer with the source segments gone");
    assert_eq!(
        blind.rows.len(),
        after.rows.len(),
        "and answer with the same rows - a short answer means it was reading the segments after all"
    );
    // The control: the recomputing query must NOT survive the same treatment, or the segments were
    // never load-bearing and the assertion above is about nothing.
    let control_rows = control.map(|o| o.rows.len()).unwrap_or(0);
    assert_ne!(
        control_rows,
        before.rows.len(),
        "the recomputing query still returned {control_rows} rows with its segments hidden, so \
         hiding them proves nothing about the maintained one"
    );
    println!("control    : the recomputing query returned {control_rows} rows once blinded");
    println!(
        "verdict    : {} segment file(s) hidden; maintained answered in full, recomputed did not",
        moved.len()
    );
}

fn segment_files(dir: &std::path::Path, table: &str) -> Vec<std::path::PathBuf> {
    let segs = dir.join("segments");
    std::fs::read_dir(&segs)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "parquet")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&format!("{table}-")))
        })
        .collect()
}

/// Rename every segment of `tables` aside, returning the moves so they can be undone.
fn hide_segments(
    dir: &std::path::Path,
    tables: &[&str],
) -> Vec<(std::path::PathBuf, std::path::PathBuf)> {
    let mut moved = Vec::new();
    for table in tables {
        for f in segment_files(dir, table) {
            let to = f.with_extension("parquet.hidden");
            if std::fs::rename(&f, &to).is_ok() {
                moved.push((to, f));
            }
        }
    }
    moved
}

fn restore_segments(moved: &[(std::path::PathBuf, std::path::PathBuf)]) {
    for (from, to) in moved {
        let _ = std::fs::rename(from, to);
    }
}
