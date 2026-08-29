//! RFC-0042 slice 1 (#936): the parity corpus must cross the engines' internal batch boundaries.
//!
//! **Why this file exists is a measurement, not a principle.** The largest row count any of
//! `analytics.rs`'s 72 tests builds is **8**. Across every integration test and `seal.rs` the largest
//! is **600**. DuckDB's vector is **2,048**, DataFusion's default batch **8,192**, dbsp's transaction
//! step **10,000**.
//!
//! So the entire analytical suite sits below the engine's first internal boundary, and a defect that
//! only appears once a second vector is filled - a chunked aggregate, a spill, an off-by-one at the
//! chunk seam - is invisible to all of it.
//!
//! That is not hypothetical. #894 was exactly this shape one layer down: **857 tests all sat under
//! dbsp's 10,000-row step**, so the suite could not see a relation silently keeping `groups mod
//! 10,000`, with nothing faulted and every surviving group holding the right value.
//!
//! These cases are the first of RFC-0042 §6's corpus. They assert exact results across a boundary, so
//! a candidate engine put behind the same surface has something to be wrong about.

use serde_json::{json, Value};

/// DuckDB's vector size. A dataset at or below this is processed in one chunk and proves nothing about
/// chunking.
const DUCKDB_VECTOR: usize = 2_048;

/// The dataset sizes the cases run at. A `const` rather than literals inline, so the guard at the
/// bottom can check the **values** instead of grepping the source for them.
///
/// The first version of that guard did grep, for `"DUCKDB_VECTOR * 2"` - and passed with every case
/// shrunk to 8 rows, because that literal appears in the guard's own assertion. A gate matching its
/// own source, which is the third instance this sprint and the first that stripping comments would
/// not have caught.
const SIZES: [usize; 4] = [DUCKDB_VECTOR - 1, DUCKDB_VECTOR, DUCKDB_VECTOR + 1, 5_000];

/// Two full vectors and a ragged tail, for the grouped case.
const GROUPED_SIZE: usize = DUCKDB_VECTOR * 2 + 7;

/// One row per `i`, with values chosen so every aggregate below has an exact closed form. Exactness is
/// the point: an approximate expectation cannot tell a chunk-seam bug from a rounding difference.
fn rows(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| {
            json!({
                "table": "tok__transfer",
                "block_number": (i as u64) + 1,
                "log_index": 0u64,
                "tx_hash": format!("0x{i:064x}"),
                "address": "0xabc",
                // Three distinct groups, so grouping crosses the seam too rather than collapsing.
                "to": format!("0x{:040x}", i % 3),
                "value": (i as u64) + 1,
            })
        })
        .collect()
}

fn hot(n: usize) -> nuthatch::analytics::HotRows {
    let mut h = nuthatch::analytics::HotRows::new();
    h.insert("tok__transfer".to_string(), rows(n));
    h
}

fn scalar(dir: &std::path::Path, n: usize, sql: &str) -> i128 {
    let out = nuthatch::analytics::query_hot_cold(
        dir,
        sql,
        nuthatch::analytics::QueryGuard {
            timeout: std::time::Duration::from_secs(60),
            max_rows: 1_000_000,
        },
        &hot(n),
        0,
        &[],
    )
    .unwrap_or_else(|e| panic!("{sql} over {n} rows: {e:#}"));
    let v = &out.rows[0];
    let cell = v.as_object().unwrap().values().next().unwrap();
    cell.as_i64()
        .map(i128::from)
        .or_else(|| cell.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not a number: {cell:?}"))
}

/// The headline case. `n = DUCKDB_VECTOR + 1` is the smallest dataset that forces a second vector, so
/// a failure here is specifically about the seam and not about size in general.
#[test]
fn aggregates_are_exact_across_duckdbs_vector_boundary() {
    let dir = tempfile::tempdir().unwrap();
    for n in SIZES {
        // Sum of 1..=n, in closed form: nothing about this expectation comes from running the query.
        let want = (n as i128) * (n as i128 + 1) / 2;
        let got = scalar(
            dir.path(),
            n,
            "SELECT SUM(CAST(value AS HUGEINT)) AS s FROM tok__transfer",
        );
        assert_eq!(
            got,
            want,
            "SUM over {n} rows: got {got}, want {want}. {n} spans {} vector(s) of {DUCKDB_VECTOR}.",
            n.div_ceil(DUCKDB_VECTOR)
        );
        let count = scalar(dir.path(), n, "SELECT COUNT(*) AS c FROM tok__transfer");
        assert_eq!(count as usize, n, "COUNT over {n} rows");
    }
}

/// Grouping is where a chunk-seam bug is likeliest to survive a `COUNT(*)`: the counts can be right
/// while a group's accumulator is reset or dropped at the boundary. #894 was precisely this.
#[test]
fn grouped_aggregates_are_exact_across_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let n = GROUPED_SIZE;
    let groups = scalar(
        dir.path(),
        n,
        "SELECT COUNT(*) AS g FROM (SELECT \"to\" FROM tok__transfer GROUP BY \"to\")",
    );
    assert_eq!(
        groups, 3,
        "the fixture has three distinct recipients by construction"
    );

    // Every row lands in exactly one group, so the group sums must total the whole sum.
    let total = scalar(
        dir.path(),
        n,
        "SELECT SUM(CAST(value AS HUGEINT)) AS s FROM tok__transfer",
    );
    let regrouped = scalar(
        dir.path(),
        n,
        "SELECT SUM(s) AS t FROM (SELECT SUM(CAST(value AS HUGEINT)) AS s FROM tok__transfer GROUP BY \"to\")",
    );
    assert_eq!(
        regrouped,
        total,
        "grouped sums must total the ungrouped sum across {n} rows ({} vectors)",
        n.div_ceil(DUCKDB_VECTOR)
    );
    assert_eq!(
        total,
        (n as i128) * (n as i128 + 1) / 2,
        "and match the closed form"
    );
}

/// The guard rail: if the corpus ever stops crossing the boundary, it has quietly become the suite it
/// was written to fix.
#[test]
fn the_corpus_actually_crosses_the_boundary() {
    let largest = SIZES.iter().copied().max().unwrap().max(GROUPED_SIZE);
    assert!(
        largest > DUCKDB_VECTOR,
        "the largest case is {largest} rows and DuckDB's vector is {DUCKDB_VECTOR}: every case fits \
         in one chunk, so this file proves nothing it was written to prove"
    );
    assert!(
        GROUPED_SIZE > DUCKDB_VECTOR * 2,
        "the grouped case must span more than two vectors, so a bug at the SECOND seam is reachable \
         and not just the first"
    );
    assert!(
        SIZES.contains(&(DUCKDB_VECTOR + 1)),
        "keep the smallest dataset that forces a second vector: a failure there is about the seam, \
         not about size in general"
    );
}
