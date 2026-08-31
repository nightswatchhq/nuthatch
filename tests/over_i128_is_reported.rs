//! COR-8 (#814): a transfer too large for `i128` is dropped from balances, and **says so**.
//!
//! The drop is correct and stays. `TRY_CAST(… AS HUGEINT)` yields NULL on the cold fold and
//! `str::parse::<i128>()` errors on the hot replay, and **both legs go** - dropping only one would
//! invent value, leaving a sender debited with nobody credited. `analytics.rs` already pins that the
//! two paths agree.
//!
//! What was wrong is that it was **silent**: a balance missing a transfer was served exactly like a
//! complete one, so a caller could not tell them apart. The board's decision (2026-08-31) was to say
//! so rather than to change the number.
//!
//! **Not `degraded_tables`**, which was the first choice and the wrong channel: it lives on
//! `QueryOutput` and describes a `/sql` query's cold data, while these drops surface at `/balances`
//! and `/balance/{address}`. `analytics.rs` had already faced that once and given `tip_unavailable`
//! its own field instead, writing down why.

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::indexer;

/// `2^127` - one past `i128::MAX`, the smallest value that cannot be represented, and the same
/// fixture `analytics.rs` uses to pin the cold/hot agreement. It fits `u128` exactly, which is why
/// the tape can carry it at all.
const TOO_BIG: u128 = 1u128 << 127;

#[tokio::test(flavor = "multi_thread")]
async fn a_transfer_beyond_i128_is_dropped_and_counted() {
    assert!(
        i128::try_from(TOO_BIG).is_err(),
        "premise: the fixture value must overflow i128"
    );

    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    // One ordinary transfer, then one that cannot be represented.
    tape.insert_block(
        1,
        transfers_block(
            1,
            0,
            1_700_000_001,
            USDC,
            &[(account(1).as_str(), account(2).as_str(), 500u128)],
        ),
    );
    tape.insert_block(
        2,
        transfers_block(
            2,
            0,
            1_700_000_002,
            USDC,
            &[(account(1).as_str(), account(2).as_str(), TOO_BIG)],
        ),
    );
    tape.insert_block(3, empty_block(3, 0, 1_700_000_003));
    tape.advance_tip_to(3);

    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
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

    let store = rt.state.store.clone();
    assert!(
        wait_until(POLL_TIMEOUT, || {
            store.get_meta("last_block").ok().flatten().as_deref() == Some("3")
        })
        .await,
        "the nest must index both transfers before the count means anything"
    );

    rt.state.balances.flush();

    assert!(
        rt.state.balances.dropped_over_i128() >= 1,
        "the over-i128 transfer was dropped from balances and not counted. A caller then reads a \
         balance that is missing a transfer with nothing saying so - which is the whole of COR-8 \
         (#814), and exactly the state this test exists to prevent returning to"
    );

    // And the ordinary transfer is unaffected: the drop must not take its neighbour with it.
    let b = rt.state.balances.balance(&account(2));
    assert_eq!(
        b,
        Some(500),
        "the representable transfer must still be counted; dropping the whole batch would be a \
         different bug wearing this one's clothes"
    );

    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;
}
