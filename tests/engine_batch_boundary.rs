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

/// Enforced by the **compiler**, not a test. Clippy pointed out the runtime form was a constant
/// assertion and was right: if the grouped case ever stops spanning more than two vectors, this file
/// should fail to build rather than pass a test that no longer means anything. A bug at the *second*
/// seam is a different bug from one at the first.
///
/// Verified: shrinking `GROUPED_SIZE` gives
/// `error[E0080]: evaluation panicked: the grouped case must span more than two vectors`.
const _: () = assert!(
    GROUPED_SIZE > DUCKDB_VECTOR * 2,
    "the grouped case must span more than two vectors"
);

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
        SIZES.contains(&(DUCKDB_VECTOR + 1)),
        "keep the smallest dataset that forces a second vector: a failure there is about the seam, \
         not about size in general"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// RFC-0042 §6's shape list, every case above the vector boundary.
//
// §6 enumerates: point lookups; narrow and wide scans; groups; exact signed large integers;
// multi-column groups; joins; authored and nested views; bounded ordering; row caps; and refused SQL.
// Each below is that shape at a size that crosses the seam, with an expectation derived arithmetically
// rather than from a run - the corpus is the reference a candidate engine is measured against, so an
// expectation copied from today's output would only prove the candidate reproduces today's bugs.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

fn rows_out(dir: &std::path::Path, n: usize, sql: &str) -> Vec<Value> {
    nuthatch::analytics::query_hot_cold(
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
    .unwrap_or_else(|e| panic!("{sql} over {n} rows: {e:#}"))
    .rows
}

/// **Point lookup** past the boundary. A single row from beyond the first vector must be findable;
/// a chunk-seam bug that drops the tail makes this return nothing rather than a wrong number, which
/// is the failure an aggregate can mask.
#[test]
fn a_point_lookup_past_the_boundary_finds_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let n = GROUPED_SIZE;
    let target = (DUCKDB_VECTOR + 500) as u64; // comfortably inside the second vector
    let got = rows_out(
        dir.path(),
        n,
        &format!("SELECT value FROM tok__transfer WHERE block_number = {target}"),
    );
    assert_eq!(got.len(), 1, "exactly one row has block_number {target}");
    let v = got[0].as_object().unwrap().values().next().unwrap();
    let v: i128 = v
        .as_i64()
        .map(i128::from)
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .expect("numeric");
    // rows() sets value = i + 1 and block_number = i + 1, so they are equal by construction.
    assert_eq!(
        v, target as i128,
        "the row past the seam must carry its own value"
    );
}

/// **Multi-column grouping** past the boundary. Grouping on two columns exercises a different
/// accumulator path from the single-column case, and #894's defect lived in exactly that machinery.
#[test]
fn multi_column_grouping_is_exact_across_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let n = GROUPED_SIZE;
    let got = rows_out(
        dir.path(),
        n,
        "SELECT COUNT(*) AS g FROM (SELECT \"to\", address FROM tok__transfer GROUP BY \"to\", address)",
    );
    let g = got[0]["g"].as_i64().unwrap();
    // Three recipients, one address: three pairs, by construction.
    assert_eq!(g, 3, "three (to, address) pairs over {n} rows");
}

/// **Exact large-integer arithmetic** past the boundary. The sum of 1..=n at these sizes exceeds
/// nothing dramatic, so this multiplies into i128 territory deliberately: a silent narrowing to i64
/// would survive the plain SUM case and die here.
#[test]
fn large_integer_sums_do_not_narrow_across_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let n = DUCKDB_VECTOR + 1;
    // 1e18 per row: n * 1e18 overflows i64 (max ~9.2e18) at n >= 10, so any narrowing shows.
    let got = rows_out(
        dir.path(),
        n,
        "SELECT SUM(CAST(value AS HUGEINT) * 1000000000000000000) AS s FROM tok__transfer",
    );
    let cell = got[0]["s"].clone();
    let s: i128 = cell
        .as_i64()
        .map(i128::from)
        .or_else(|| cell.as_str().and_then(|x| x.parse().ok()))
        .unwrap_or_else(|| panic!("not numeric: {cell:?}"));
    let want = (n as i128) * (n as i128 + 1) / 2 * 1_000_000_000_000_000_000i128;
    assert_eq!(
        s, want,
        "SUM(value * 1e18) over {n} rows narrowed or lost precision: got {s}, want {want}"
    );
}

/// **Bounded ordering and a row cap** past the boundary. `ORDER BY ... LIMIT` is where an engine may
/// take a top-k shortcut per chunk and merge wrongly at the seam.
#[test]
fn bounded_ordering_returns_the_true_top_across_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let n = GROUPED_SIZE;
    let got = rows_out(
        dir.path(),
        n,
        "SELECT value FROM tok__transfer ORDER BY CAST(value AS HUGEINT) DESC LIMIT 3",
    );
    assert_eq!(got.len(), 3, "LIMIT 3 returns three rows");
    let vals: Vec<i128> = got
        .iter()
        .map(|r| {
            let c = r.as_object().unwrap().values().next().unwrap();
            c.as_i64()
                .map(i128::from)
                .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
                .expect("numeric")
        })
        .collect();
    // values are 1..=n, so the true top three are n, n-1, n-2 - all in the LAST chunk.
    assert_eq!(
        vals,
        vec![n as i128, n as i128 - 1, n as i128 - 2],
        "the top three must come from the final chunk, not from the first one an engine happened to \
         finish"
    );
}

/// **Refused SQL** stays refused at scale. A guard that only holds on small inputs is not a guard, and
/// the query surface is bounded by RFC-0034 rather than by the engine's own opinion.
#[test]
fn refused_sql_is_still_refused_past_the_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let n = DUCKDB_VECTOR + 1;
    let err = nuthatch::analytics::query_hot_cold(
        dir.path(),
        "DROP TABLE tok__transfer",
        nuthatch::analytics::QueryGuard {
            timeout: std::time::Duration::from_secs(60),
            max_rows: 1_000_000,
        },
        &hot(n),
        0,
        &[],
    );
    assert!(
        err.is_err(),
        "a mutating statement must be refused whatever the dataset size"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// hot + cold (#945). The shape §6 lists that the corpus most needed, because it is where COR-1's
// disjointness invariant lives: rows at or below `sealed_through` are cold, rows above are hot, and
// the union must contain every row exactly once. Get it wrong in one direction and rows are counted
// twice; wrong in the other and they vanish.
//
// Every case below puts the seam **and** an engine chunk boundary in the same fixture, because a
// defect at a seam that never coincides with a chunk boundary is a different defect from one that
// does, and only the second is cheap to miss.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

/// One sealed-row JSON blob per block, matching the shape `rows()` produces for the hot side, so a
/// row is identical whichever layer it ends up in. If the two shapes differed, a union bug and a
/// schema bug would look the same.
fn sealed_json(i: usize) -> String {
    serde_json::json!({
        "table": "tok__transfer",
        "block_number": (i as u64) + 1,
        "log_index": 0u64,
        "tx_hash": format!("0x{i:064x}"),
        "address": "0xabc",
        "to": format!("0x{:040x}", i % 3),
        "value": (i as u64) + 1,
    })
    .to_string()
}

/// Seal blocks `1..=cold` into real Parquet on disk, and return the hot rows for `cold+1..=total`.
fn hot_and_cold(dir: &std::path::Path, cold: usize, total: usize) -> nuthatch::analytics::HotRows {
    let sealed: Vec<String> = (0..cold).map(sealed_json).collect();
    nuthatch::seal::seal_range(dir, &sealed, 1, cold as u64).expect("seal the cold half");
    // `rows(i + 1)[i]` would rebuild the whole prefix per row - O(n squared), and it cost 18 seconds
    // before anyone noticed. Build the full set once and take the tail.
    let all = rows(total);
    let mut h = nuthatch::analytics::HotRows::new();
    h.insert("tok__transfer".to_string(), all[cold..].to_vec());
    h
}

fn union_scalar(
    dir: &std::path::Path,
    hot: &nuthatch::analytics::HotRows,
    cold: u64,
    sql: &str,
) -> i128 {
    let out = nuthatch::analytics::query_hot_cold(
        dir,
        sql,
        nuthatch::analytics::QueryGuard {
            timeout: std::time::Duration::from_secs(60),
            max_rows: 1_000_000,
        },
        hot,
        cold,
        &[],
    )
    .unwrap_or_else(|e| panic!("{sql}: {e:#}"));
    let c = out.rows[0].as_object().unwrap().values().next().unwrap();
    c.as_i64()
        .map(i128::from)
        .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or_else(|| panic!("not numeric: {c:?}"))
}

/// **The disjointness invariant, with the seam inside the second vector.**
///
/// `cold` is deliberately larger than one DuckDB vector, so the cold side alone spans a chunk boundary
/// *and* the hot tail begins mid-way through the second. A union that double-counts the seam or drops
/// it fails on the closed form; nothing here is copied from a run.
#[test]
fn the_hot_cold_union_counts_every_row_exactly_once() {
    for (cold, total) in [
        (DUCKDB_VECTOR - 1, DUCKDB_VECTOR + 500), // seam just before the boundary
        (DUCKDB_VECTOR, DUCKDB_VECTOR + 500),     // seam exactly on it
        (DUCKDB_VECTOR + 1, GROUPED_SIZE),        // seam just past it, hot tail into a third vector
    ] {
        let dir = tempfile::tempdir().unwrap();
        let hot = hot_and_cold(dir.path(), cold, total);
        let n = union_scalar(
            dir.path(),
            &hot,
            cold as u64,
            "SELECT COUNT(*) AS n FROM tok__transfer",
        );
        assert_eq!(
            n as usize, total,
            "COUNT over hot+cold with the seam at {cold} of {total}: a union that double-counts or \
             drops the seam is COR-1's failure, and it is silent"
        );
        let s = union_scalar(
            dir.path(),
            &hot,
            cold as u64,
            "SELECT SUM(CAST(value AS HUGEINT)) AS s FROM tok__transfer",
        );
        assert_eq!(
            s,
            (total as i128) * (total as i128 + 1) / 2,
            "SUM over hot+cold with the seam at {cold} of {total} must equal the closed form"
        );
    }
}

/// Grouping across the seam. A count can be right while a group's accumulator is reset at the layer
/// change - the same shape as #894, one layer up.
#[test]
fn grouping_across_the_hot_cold_seam_is_exact() {
    let dir = tempfile::tempdir().unwrap();
    let (cold, total) = (DUCKDB_VECTOR + 1, GROUPED_SIZE);
    let hot = hot_and_cold(dir.path(), cold, total);
    let groups = union_scalar(
        dir.path(),
        &hot,
        cold as u64,
        "SELECT COUNT(*) AS g FROM (SELECT \"to\" FROM tok__transfer GROUP BY \"to\")",
    );
    assert_eq!(
        groups, 3,
        "three recipients by construction, on both sides of the seam"
    );
    let regrouped = union_scalar(
        dir.path(),
        &hot,
        cold as u64,
        "SELECT SUM(s) AS t FROM (SELECT SUM(CAST(value AS HUGEINT)) AS s FROM tok__transfer GROUP BY \"to\")",
    );
    assert_eq!(
        regrouped,
        (total as i128) * (total as i128 + 1) / 2,
        "grouped sums across the seam must total the closed form"
    );
}
