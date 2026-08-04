//! **No DuckDB extension may be downloaded at runtime.**
//!
//! CLAUDE.md non-negotiable 1 (a single static binary, zero external services) and 3 (no
//! phone-home) both depend on every DuckDB feature we use being *compiled in*, not fetched.
//!
//! This is not hypothetical. Until 2026-08-04 the crate was built with `features = ["bundled"]`
//! alone, which does **not** statically link the `parquet` or `json` extensions. DuckDB's default
//! `autoinstall_known_extensions` then silently downloaded `parquet.duckdb_extension` from its
//! public repository on the **first `/sql` query touching a sealed segment**:
//!
//!   - a phone-home on a fresh machine, on the read path, without the operator asking
//!   - a hard failure on an air-gapped box, where sealed history simply cannot be read
//!   - invisible in CI, because CI has a network and the download just works
//!
//! Measured at the time: 1120 ms for the first read (the fetch) versus 4 ms once statically
//! linked. The bug was found by accident while probing an unrelated question, which is the whole
//! reason this guard exists.
//!
//! The test points DuckDB at an empty extension directory but leaves autoload/autoinstall at
//! their **production defaults**, then asserts the directory is *still empty* afterwards. A
//! download would leave a file behind. Do not "fix" a failure here by disabling autoload - that
//! hides the fetch rather than removing the need for it. Add the crate feature instead.

use duckdb::Connection;

/// Every file under `dir`, relative and sorted. Empty means nothing was fetched.
fn files_under(dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(dir).unwrap().display().to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// A connection configured like production, except its extension cache is empty - i.e. a fresh
/// machine. Autoload and autoinstall keep their defaults deliberately.
fn fresh_machine(dir: &std::path::Path) -> Connection {
    let conn = Connection::open_in_memory().expect("open duckdb");
    conn.execute_batch(&format!(
        "SET extension_directory='{}';",
        dir.to_str().expect("utf-8 temp path")
    ))
    .expect("set extension_directory");
    conn
}

#[test]
fn reading_a_sealed_segment_downloads_nothing() {
    let dir = std::env::temp_dir().join("nuthatch-extguard-parquet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Write a Parquet file the way a sealed segment is shaped, then read it back the way `/sql`
    // does. Both go through the same connection, whose extension cache is empty.
    let pq = dir.join("segment.parquet");
    let conn = fresh_machine(&dir);
    conn.execute_batch(&format!(
        "COPY (SELECT i AS block_number, 'e' || i AS id FROM range(8) t(i)) TO '{}' (FORMAT PARQUET);",
        pq.to_str().unwrap()
    ))
    .expect("write parquet: if this fails, the `parquet` crate feature is missing");

    let n: i64 = conn
        .prepare(&format!(
            "SELECT count(*) FROM read_parquet('{}')",
            pq.to_str().unwrap()
        ))
        .and_then(|mut s| s.query_row([], |r| r.get(0)))
        .expect("read_parquet: if this fails, the `parquet` crate feature is missing");
    assert_eq!(n, 8, "sealed-segment read returned the wrong row count");

    // The parquet file itself lives in this dir, so exclude it: we are asserting that no
    // *extension* was fetched.
    let fetched: Vec<_> = files_under(&dir)
        .into_iter()
        .filter(|f| f.contains("duckdb_extension"))
        .collect();
    assert!(
        fetched.is_empty(),
        "DuckDB downloaded an extension to read a sealed segment: {fetched:?}. \
         That is a phone-home on the read path and a hard failure when air-gapped. \
         Add the crate feature so it is statically linked; do not disable autoload."
    );
}

#[test]
fn reading_json_downloads_nothing() {
    // `analytics.rs` imports label sets with `read_json(...)`, so the json extension is on a real
    // production path (RFC-0008 labels), not a convenience.
    let dir = std::env::temp_dir().join("nuthatch-extguard-json");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let jf = dir.join("labels.json");
    std::fs::write(&jf, r#"[{"address":"0xabc","label":"exchange"}]"#).unwrap();

    let conn = fresh_machine(&dir);
    let n: i64 = conn
        .prepare(&format!(
            "SELECT count(*) FROM read_json('{}', format='array', \
             columns={{address: 'VARCHAR', label: 'VARCHAR'}})",
            jf.to_str().unwrap()
        ))
        .and_then(|mut s| s.query_row([], |r| r.get(0)))
        .expect("read_json: if this fails, the `json` crate feature is missing");
    assert_eq!(n, 1);

    let fetched: Vec<_> = files_under(&dir)
        .into_iter()
        .filter(|f| f.contains("duckdb_extension"))
        .collect();
    assert!(
        fetched.is_empty(),
        "DuckDB downloaded an extension to read JSON: {fetched:?}"
    );
}
