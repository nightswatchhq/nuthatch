//! #1165: concurrent `/sql` execution segfaulted a production nest - 25 SEGVs in a minute at four
//! permits on 3.5.0, none at one, with a `general protection fault ... in libc.so.6` on a tokio
//! worker and 4.9 GB free, which is native heap corruption rather than exhaustion.
//!
//! The concurrency the gate permits had no test at all: `sql_concurrency_is_measurable` asserts the
//! permit *count* and never runs two queries at once. This runs the shape the crashes were serving -
//! a filtered `ORDER BY` over a view whose scan reads a sealed segment holding rows on both sides of
//! the filter - from several threads against one nest, which is exactly what the analytical path
//! does when more than one permit is free, because a second concurrent query finds the connection
//! cache empty and opens a DuckDB instance of its own.
//!
//! A pass here is not proof the fault is gone: the production reproduction needs the real corpus and
//! this fixture is small. A *failure* is worth a great deal, because it moves #1165 from a
//! production-only story to something CI can hold.
mod common;

use std::sync::Arc;

use nuthatch::{analytics, indexer};

use common::tape::*;

/// A nest with one sealed segment spanning many blocks, each carrying rows on both sides of any
/// value filter - the shape the curator lists have.
async fn sealed_nest(dir: &std::path::Path) {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=60u64 {
        let transfers: Vec<(&str, &str, u128)> = (0..350)
            .map(|i| (a1.as_str(), a2.as_str(), (100 * b + i) as u128))
            .collect();
        tape.insert_block(
            b,
            transfers_block(b, 0, 1_700_000_000 + b, USDC, &transfers),
        );
    }
    tape.advance_tip_to(60);
    tape.advance_finalized_to(60);

    let registry = nuthatch::registry::from_nest(dir, &cfg).expect("registry");
    let addresses: Vec<String> = cfg.contracts.iter().map(|c| c.address.clone()).collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();
    indexer::backfill_direct(
        tape.as_ref(),
        &registry,
        dir,
        &addresses,
        &topic0s,
        &[],
        None,
        0,
        1,
        60,
        10,
        false,
    )
    .await
    .expect("backfill");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_queries_over_a_sealed_segment_do_not_corrupt_the_process() {
    let dir = tempfile::tempdir().expect("tempdir");
    sealed_nest(dir.path()).await;

    // The shape from the crash reports, and one that must read the segment rather than a summary:
    // a filter that the segment spans, ordered by the filtered column.
    //
    // `value` repeats across blocks (100*b + i collides for neighbouring b), so the order is made
    // total with the row's own coordinates: otherwise two correct answers could differ in the order
    // of tied rows and the comparison below would call a healthy process corrupt.
    let sql = "SELECT block_number, log_index, value FROM usdc__transfer \
               WHERE CAST(value AS HUGEINT) > 3000 \
               ORDER BY CAST(value AS HUGEINT) DESC, block_number, log_index LIMIT 200";
    let rows = analytics::query(dir.path(), sql).expect("the query answers at all");
    assert!(
        !rows.is_empty(),
        "the fixture must return rows, or the concurrency proves nothing"
    );
    // The rows, not their count: `LIMIT 200` makes a count of 200 satisfiable by rows from the wrong
    // blocks, in the wrong order, or with corrupted values (Jules on #1181). Corruption that keeps the
    // count is exactly the kind a spill collision produces.
    let expected = rows;

    // Four threads is where production died; the loop is long enough to have crossed that window
    // several times over.
    let mut handles = Vec::new();
    for _ in 0..4 {
        let d = dir.path().to_path_buf();
        let q = sql.to_string();
        handles.push(std::thread::spawn(move || {
            for _ in 0..25 {
                let got = analytics::query(&d, &q).expect("concurrent query");
                assert_eq!(
                    got, expected,
                    "a concurrent query returned different rows than the same query alone"
                );
            }
        }));
    }
    for h in handles {
        h.join()
            .expect("a query thread died - #1165 reproduces in CI");
    }
}
