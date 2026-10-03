//! #1778: the QoS nest's RSS climbed from 1186 to 2168 MiB in three hours while `/metrics`
//! accounted for 64 MiB of it. These are the gauges that say where the rest is: the analytics
//! engines' pools, and on Linux the allocator's own view.
mod common;

use std::sync::Arc;

use nuthatch::analytics;
use nuthatch::indexer;
use nuthatch::metrics::METRICS;

use common::tape::*;

// The binary's allocator, so the allocator gauges read a heap this process actually uses.
#[cfg(all(target_os = "linux", not(target_env = "musl")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn value(text: &str, series: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.split(' ').next() == Some(series))
        .and_then(|l| l.rsplit(' ').next()?.parse().ok())
}

fn sealed_tape() -> Arc<TapeSource> {
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=40u64 {
        let transfers: Vec<(&str, &str, u128)> = (0..300)
            .map(|i| (a1.as_str(), a2.as_str(), (100 * b + i) as u128))
            .collect();
        tape.insert_block(
            b,
            transfers_block(b, 0, 1_700_000_000 + b, USDC, &transfers),
        );
    }
    tape.advance_tip_to(40);
    tape.advance_finalized_to(40);
    tape
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pool_gauges_rise_under_a_query_that_reserves_memory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let name = "poolgauge";
    let cfg = scaffold_nest(dir.path(), name, USDC);
    let tape = sealed_tape();
    let registry = nuthatch::registry::from_nest(dir.path(), &cfg).expect("registry");
    let addresses: Vec<String> = cfg.contracts.iter().map(|c| c.address.clone()).collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();
    indexer::backfill_direct(
        tape.as_ref(),
        &registry,
        dir.path(),
        &addresses,
        &topic0s,
        &[],
        None,
        0,
        1,
        40,
        10,
        nuthatch::chains::DEFAULT_SEAL_SPAN,
        false,
    )
    .await
    .expect("backfill");

    let before = METRICS.render();
    for series in [
        "nuthatch_analytics_pool_reserved_bytes",
        "nuthatch_analytics_pool_peak_bytes",
        "nuthatch_analytics_engines",
    ] {
        assert!(
            value(&before, series).is_some(),
            "{series} must be on /metrics before any query has run:\n{before}"
        );
    }
    assert_eq!(
        value(&before, "nuthatch_analytics_pool_peak_bytes"),
        Some(0),
        "nothing has queried yet, so a rise below is this test's query"
    );

    // A sort reserves against the pool before it emits anything.
    let sql = "SELECT block_number, log_index, value FROM poolgauge__transfer \
               ORDER BY CAST(value AS HUGEINT) DESC, block_number, log_index";
    let rows = analytics::query(dir.path(), sql).expect("the query answers");
    assert!(!rows.is_empty(), "the fixture must return rows");

    let after = METRICS.render();
    let peak = value(&after, "nuthatch_analytics_pool_peak_bytes").unwrap();
    assert!(
        peak > 0,
        "a sort over {} rows ran and the pool's peak still reads 0:\n{after}",
        rows.len()
    );
    assert!(
        value(&after, "nuthatch_analytics_engines").unwrap() >= 1,
        "the query's session is cached, so its engine is alive:\n{after}"
    );

    // Labelled by the nest whose dataset the engine was opened over, from the indexer's own record.
    let rt = indexer::spawn_nest(
        tape.clone(),
        dir.path().to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest");
    let labelled = METRICS.render();
    let peak_series = format!("nuthatch_nest_analytics_pool_peak_bytes{{nest=\"{name}\"}}");
    let nest_peak = value(&labelled, &peak_series)
        .unwrap_or_else(|| panic!("{peak_series} is missing:\n{labelled}"));
    assert!(
        nest_peak > 0,
        "the nest's engine reserved for the sort, so its peak is not 0:\n{labelled}"
    );
    let reserved_series = format!("nuthatch_nest_analytics_pool_reserved_bytes{{nest=\"{name}\"}}");
    assert!(
        value(&labelled, &reserved_series).is_some(),
        "{reserved_series} is missing:\n{labelled}"
    );
    let engines_series = format!("nuthatch_nest_analytics_engines{{nest=\"{name}\"}}");
    assert!(
        value(&labelled, &engines_series).unwrap_or(0) >= 1,
        "{engines_series} must count the cached session:\n{labelled}"
    );

    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;
}

#[cfg(all(target_os = "linux", not(target_env = "musl")))]
#[test]
fn the_allocator_gauges_read_the_jemalloc_heap() {
    // Rendered once first: a scrape that did not advance the epoch would repeat this one.
    let _ = METRICS.render();
    let held: Vec<Vec<u8>> = (0..64).map(|_| vec![7u8; 1 << 20]).collect();
    let text = METRICS.render();
    let read = |s: &str| value(&text, s).unwrap_or_else(|| panic!("{s} is missing:\n{text}"));
    let allocated = read("nuthatch_jemalloc_allocated_bytes");
    let active = read("nuthatch_jemalloc_active_bytes");
    let resident = read("nuthatch_jemalloc_resident_bytes");
    read("nuthatch_jemalloc_retained_bytes");
    assert!(
        allocated >= 64 << 20,
        "64 MiB is held and jemalloc reports {allocated} allocated:\n{text}"
    );
    assert!(
        active >= allocated,
        "active {active} < allocated {allocated}"
    );
    assert!(
        resident >= allocated,
        "resident {resident} < allocated {allocated}"
    );
    drop(held);
}
