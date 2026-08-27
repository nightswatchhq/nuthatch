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
        for f in segment_files(&dir, &table) {
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
        guard.clone(),
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
        guard.clone(),
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
        guard.clone(),
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
        guard.clone(),
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

/// **#822 criterion 5, against a real nest.** *"After one new block, update work is proportional to
/// affected rows in that block rather than historical source cardinality. Record it."*
///
/// **It is not, and this is how far off it is** (`graph-allocations-nest`, 2026-08-27). The same
/// one-row window, fed to circuits that differ only in how much history they folded first:
///
/// | groups maintained | ThinkPad (Linux, idle) | MacBook |
/// |---|---|---|
/// | 1 | 206 µs | 73 µs |
/// | 61 | 86 µs | 60 µs |
/// | 75,733 | 14,654 µs | 5,212 µs |
/// | 309,548 | **72,070 µs** | 26,468 µs |
///
/// Flat to a hundred groups or so, then linear: 4.09x the groups costs 4.92x the update. Steady, not
/// a one-off settling after the seed - five consecutive windows all cost the same. See #897.
///
/// It is not the publish clone (a fold with publication deferred costs the same), not the step loop
/// (one window is one `step()`), and not the circuit failing to be incremental - the output delta
/// for a one-row window against 309,548 groups is **2 rows**. The cost is inside DBSP's transaction
/// commit over a large trace.
///
/// Two circuits, the same plan, the same window fed to both. The window is **real rows off the
/// corpus** (the newest sealed segment), not synthetic ones, so the key distribution is the nest's
/// own: a block whose keys are all new is a different amount of work from one that updates existing
/// groups, and inventing the rows would let me pick the easy case.
#[test]
fn update_cost_tracks_the_block_not_the_history() {
    let Ok(dir) = std::env::var("NUTHATCH_SEED_NEST") else {
        eprintln!("set NUTHATCH_SEED_NEST to a nest directory to run this");
        return;
    };
    let sql = std::env::var("NUTHATCH_SEED_SQL").expect("set NUTHATCH_SEED_SQL");
    let dir = std::path::PathBuf::from(shellexpand(&dir));

    let config = nuthatch::config::Config::load(&dir).unwrap();
    let registry = nuthatch::registry::from_nest(&dir, &config).unwrap();
    let schema = registry.schema();
    let (plan, columns) = nuthatch::entity_lower::lower_with_columns(&sql).unwrap();

    // Every segment's rows, kept in reading order so "the last segment" is the newest.
    let mut chunks: Vec<Vec<nuthatch::registry::DecodedRow>> = Vec::new();
    for table in {
        let v = nuthatch::entity_view::EntityView::start(
            "shape", &plan, &columns, &registry, 5_000_000, true,
        )
        .unwrap();
        v.tables().iter().map(|t| t.to_string()).collect::<Vec<_>>()
    } {
        let ts = schema.iter().find(|t| t.table == table).unwrap();
        nuthatch::seal::read_table_rows_by_segment(&dir, ts, &mut |chunk| {
            chunks.push(chunk);
            Ok(())
        })
        .unwrap();
    }
    assert!(
        chunks.len() > 2,
        "need several segments; got {}",
        chunks.len()
    );
    let window = chunks.pop().expect("the newest segment, as the new block");
    let total: usize = chunks.iter().map(Vec::len).sum();

    let seed_and_time = |history: &[Vec<nuthatch::registry::DecodedRow>], label: &str| -> u128 {
        let mut v = nuthatch::entity_view::EntityView::start(
            "probe", &plan, &columns, &registry, 5_000_000, true,
        )
        .unwrap();
        v.seed_begin();
        for c in history {
            v.seed_chunk(c, u64::MAX).unwrap();
        }
        v.seed_end().unwrap();
        let groups = v.relation().len();
        let rows: usize = history.iter().map(Vec::len).sum();

        // Five consecutive windows. A cost that appears only on the first is a one-off settling of
        // whatever the seed left behind; a cost on every one is what criterion 5 forbids.
        let mut each = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            v.apply_window(&window, 1, u64::MAX).unwrap();
            v.flush();
            each.push(t.elapsed().as_micros());
        }
        let steady = each[2..].iter().sum::<u128>() / 3;
        println!(
            "  {label:<8} history {rows:>8} rows / {groups:>7} groups -> one {}-row window: {each:?} µs (steady {steady})",
            window.len()
        );
        let ms = steady;
        assert!(v.fault().is_none(), "faulted: {:?}", v.fault());
        ms
    };

    println!("\n=== #822 criterion 5, against {}", dir.display());
    println!("sql: {sql}");
    println!(
        "window: {} row(s) from the newest sealed segment",
        window.len()
    );
    // A curve, not two points: the question is not "is it slower" but "what shape".
    let mut curve = Vec::new();
    let mut depth = 1usize;
    while depth < chunks.len() {
        curve.push(seed_and_time(&chunks[..depth], &format!("{depth} seg")));
        depth *= 8;
    }
    curve.push(seed_and_time(&chunks, "all"));

    // The claim, asserted rather than merely printed. `shallow` is the flat region; `deep` is the
    // whole history. An entity exists so that this ratio stays near one.
    let shallow = *curve.iter().take(3).min().unwrap();
    let deep = *curve.last().unwrap();
    let ratio = deep as f64 / shallow.max(1) as f64;
    println!(
        "\nhistory {} rows / {} groups: update cost {ratio:.0}x the flat-region cost",
        total,
        chunks.len()
    );
    assert!(
        ratio < 10.0,
        "one block's update cost scaled {ratio:.0}x with history ({shallow} µs -> {deep} µs), \
         which is the thing an entity exists not to do - #897"
    );
}
