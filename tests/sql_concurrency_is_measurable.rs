//! #1006 - the permit count is a memory bound nobody chose as one, and it could not be measured.
//!
//! `SQL_MAX_CONCURRENCY` is a `const usize = 2`. Measuring the throughput/RSS curve at 1/2/4/8/16
//! permits - which #1006 asks for, on the box that enforces the per-cursor RAM budget - therefore
//! needed **five separate builds**. That is how a ceiling ends up being set from whichever box was
//! convenient, which this project has done before.
//!
//! `NUTHATCH_SQL_MAX_CONCURRENCY` makes it one binary and five settings. It is deliberately capped:
//! concurrent queries do **not** serialise (RFC-0042 §14 corrected that), but each one that misses
//! the connection cache opens its own DuckDB, and unbounded at 32 clients reached **1,313 MB - 64%
//! of one cursor's entire 2 GB**, shared across every nest on that cursor.

use nuthatch::serve::{sql_max_concurrency, SQL_MAX_CONCURRENCY, SQL_MAX_CONCURRENCY_CEILING};

/// The env var is process-global, so these run under one lock rather than in parallel.
fn with_env<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let key = "NUTHATCH_SQL_MAX_CONCURRENCY";
    let prev = std::env::var(key).ok();
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
    let out = f();
    match prev {
        Some(p) => std::env::set_var(key, p),
        None => std::env::remove_var(key),
    }
    out
}

#[test]
fn unset_keeps_the_shipped_default() {
    assert_eq!(
        with_env(None, sql_max_concurrency),
        SQL_MAX_CONCURRENCY,
        "an operator who sets nothing must get exactly the shipped value - this knob exists to make \
         the default measurable, not to change it"
    );
    assert_eq!(
        SQL_MAX_CONCURRENCY, 2,
        "the shipped default is 2 until a measured curve says otherwise (#1006)"
    );
}

#[test]
fn a_requested_value_is_honoured_across_the_measured_range() {
    for n in [1usize, 2, 4, 8, 16] {
        assert_eq!(
            with_env(Some(&n.to_string()), sql_max_concurrency),
            n,
            "the curve #1006 asks for is 1/2/4/8/16 permits; if any of those is not honoured the \
             measurement silently flattens and reports a knee that is an artefact of the harness"
        );
    }
}

#[test]
fn above_the_ceiling_is_clamped_not_obeyed() {
    // The RAM argument, enforced rather than documented. 64 permits could open 64 DuckDBs.
    for n in [17usize, 64, 10_000] {
        assert_eq!(
            with_env(Some(&n.to_string()), sql_max_concurrency),
            SQL_MAX_CONCURRENCY_CEILING,
            "a request above the ceiling must clamp. Each concurrent query can open its own DuckDB \
             and the budget is 2 GB per cursor, shared across every nest on it - an unbounded knob \
             would let an operator spend a budget they cannot see"
        );
    }
}

#[test]
fn nonsense_falls_back_rather_than_disabling_the_gate() {
    // `0` is the dangerous one: a zero-permit semaphore refuses every query forever, and an
    // operator typing 0 meaning "unlimited" would take the SQL surface down rather than open it.
    for bad in ["0", "", "  ", "-1", "eight", "4.5"] {
        let got = with_env(Some(bad), sql_max_concurrency);
        assert_eq!(
            got, SQL_MAX_CONCURRENCY,
            "`{bad}` must fall back to the default. 0 in particular must never reach the semaphore: \
             zero permits refuses every query forever, so `=0` meaning 'unlimited' would silently \
             close the analytical surface"
        );
        assert!(got > 0, "the permit count must never be zero");
    }
}

// ---------------------------------------------------------------------------------------------
// The wiring, not the function.
//
// Everything above passes with `build_nest` reading the bare `const` again - I checked, by making
// that exact edit. A test of `sql_max_concurrency()` proves the parser, and the defect #1006 is
// about lives one line further on: whether the semaphore the handlers actually acquire was built
// from it. Same shape as the mounts tests that proved a handler and never the wiring.
// ---------------------------------------------------------------------------------------------

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::indexer;

/// `NUTHATCH_SQL_MAX_CONCURRENCY` is process-global, so tests that vary it take this first.
fn gate_lock() -> &'static tokio::sync::Mutex<()> {
    static L: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn permits_with(value: Option<&str>) -> usize {
    let key = "NUTHATCH_SQL_MAX_CONCURRENCY";
    match value {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    tape.insert_block(1, empty_block(1, 0, 1_700_000_000));
    tape.advance_tip_to(1);
    let cfg = scaffold_nest(dir.path(), "gate", USDC);
    let rt = indexer::spawn_nest(
        tape,
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
    let permits = rt.state.sql_gate.available_permits();
    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;
    std::env::remove_var(key);
    permits
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gate_the_handlers_acquire_is_built_from_the_live_value() {
    let _guard = gate_lock().lock().await;
    // Serialised deliberately: the env var is process-global.
    assert_eq!(
        permits_with(None).await,
        SQL_MAX_CONCURRENCY,
        "an unset override must produce the shipped gate"
    );
    assert_eq!(
        permits_with(Some("8")).await,
        8,
        "the semaphore `/sql` acquires must be built from the live value, not from the `const`. \
         With the const wired in, every arm of #1006's curve would run at 2 permits and report a \
         flat line that looks like a finding"
    );
    assert_eq!(
        permits_with(Some("64")).await,
        SQL_MAX_CONCURRENCY_CEILING,
        "the ceiling must hold at the gate, not only in the parser"
    );
}

/// #1024 - the gate must be owned by the **cursor**, shared by every nest on it.
///
/// Caught in review, and the same shape as everything else this sprint found: the doc comment on
/// `SQL_MAX_CONCURRENCY` has always claimed to bound "the whole analytical surface", while
/// `build_nest` gave every nest its own semaphore. At a hardcoded 2 that was survivable. Made
/// settable, `NUTHATCH_SQL_MAX_CONCURRENCY=16` across six nests would admit **96** concurrent
/// queries, each able to open its own DuckDB - the exact per-cursor overrun the override's warning
/// claims to prevent.
///
/// **The cursor, not the process.** A first attempt made the gate process-global and was too blunt:
/// unrelated cursors in a multichain runtime would contend for a budget they do not share, and
/// parallel integration tests started returning `503 server busy`. `group_by_chain` gives one
/// `spawn_runtime` per chain, so the gate is built there and handed to each nest on that cursor.
///
/// **This drives `spawn_runtime`, not `Arc::clone`.** The first version of this test asserted that
/// two clones of one `Arc` were the same allocation, which is true of `Arc` and says nothing about
/// the wiring - it passed with `spawn_runtime` mutated back to a gate per nest. Third time in this
/// sprint that a test proved a mechanism instead of a seam, so the real call is exercised here.
#[tokio::test(flavor = "multi_thread")]
async fn nests_on_one_cursor_share_one_gate() {
    let _guard = gate_lock().lock().await;
    std::env::set_var("NUTHATCH_SQL_MAX_CONCURRENCY", "4");

    let health = Arc::new(nuthatch::health::RuntimeHealth::default());
    let tape = Arc::new(TapeSource::new());
    tape.insert_block(1, empty_block(1, 0, 1_700_000_000));
    tape.advance_tip_to(1);

    let dirs: Vec<_> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut nests = Vec::new();
    for (i, d) in dirs.iter().enumerate() {
        let name = format!("cursor-share-{i}");
        let cfg = scaffold_nest(d.path(), &name, USDC);
        health.register(&name, &cfg.nest.chain);
        nests.push((name, d.path().to_path_buf(), cfg));
    }

    let cursor = indexer::spawn_runtime(
        tape,
        nests,
        None,
        false,
        1,
        Some(2),
        false,
        None,
        health,
        false,
    )
    .await
    .expect("spawn_runtime");

    assert_eq!(cursor.states.len(), 3, "three nests on one cursor");
    let gates: Vec<_> = cursor
        .states
        .iter()
        .map(|(_, st)| st.sql_gate.clone())
        .collect();

    for (i, g) in gates.iter().enumerate().skip(1) {
        assert!(
            Arc::ptr_eq(&gates[0], g),
            "nest {i} on this cursor has its own semaphore. Three nests would then admit 3 x 4 = 12 \
             concurrent analytical queries against a budget that is per cursor and shared across \
             the nests on it (#1024)"
        );
    }

    // The consequence, which pointer equality alone does not demonstrate: exhausting the gate
    // through one nest must starve its co-tenants.
    let held: Vec<_> = (0..4)
        .map(|_| gates[0].clone().try_acquire_owned().expect("4 permits"))
        .collect();
    for (i, g) in gates.iter().enumerate() {
        assert!(
            g.clone().try_acquire_owned().is_err(),
            "nest {i} could still admit a query while a co-tenant held every permit on the cursor"
        );
    }
    drop(held);
    assert_eq!(
        gates[1].available_permits(),
        4,
        "permits return to the shared gate"
    );

    cursor.ingest.abort();
    let _ = cursor.ingest.await;
    for (_, w) in cursor.alert_workers {
        w.abort();
        let _ = w.await;
    }
    std::env::remove_var("NUTHATCH_SQL_MAX_CONCURRENCY");
}
