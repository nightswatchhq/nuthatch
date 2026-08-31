//! An unset tip renders as **perfect health**, so nothing could ever alert on it.
//!
//! Found by measuring the Lodestar box for #750. Three nests following tip correctly - their logs
//! saying `following, behind tip blocks_behind=2` - reported this:
//!
//! | surface | tip | lag | last poll |
//! | --- | ---: | ---: | ---: |
//! | `/ready` | 500198892 | 3 | 1788156483 |
//! | `/metrics` | **0** | **0** | **0** |
//!
//! `NestMetrics::{set_tip, mark_poll_ok}` fan out to the process-global gauges as well, but
//! `METRICS::*` does not fan *in*. `index_loop` called the global for tip and poll while calling the
//! per-nest struct for poll *failures* - inconsistent inside one function - so the nest's own `tip`
//! and `last_poll_ok` stayed at zero. `/ready` never noticed, because a solo runtime falls back to
//! the global; `/metrics` prefers the per-nest struct whenever one exists.
//!
//! **Why lag is the one that matters.** It renders as `tip.saturating_sub(last)`. With `tip` at zero
//! and `last` at half a billion, that saturates to **0** - not "unknown" but "exactly at tip". A
//! nest could stall for a week and `nuthatch_tip_lag_blocks` would still read 0.
//!
//! `last_block` was right throughout, because its setter is the fanning-out one. That is what let
//! the fault survive: the gauge beside it looked plausible.
//!
//! Third instance of this shape - #918 was `sealed_through` reading 0 on `/metrics` while `/sql`
//! provenance had it right, and its comment already said *"two surfaces disagreeing about one fact,
//! and the wrong one is where Prometheus looks"*.
//!
//! # This test drives the real ingest loop, and the first version of it did not
//!
//! The first attempt asserted against `NestMetrics` directly - `m.set_tip(...)`, then read it back.
//! Every case passed **with the fix reverted**, because setting the tip by hand is exactly the step
//! `index_loop` was failing to do. A test that supplies the missing call cannot see it missing.
//! So this spawns a nest against a tape and reads the rendered exporter, which is the only
//! arrangement in which the defect is reachable.

mod common;

use std::sync::Arc;

use common::tape::*;
use nuthatch::indexer;
use nuthatch::metrics::METRICS;

/// A short chain with the tip a few blocks past the last event, so the nest is genuinely *behind*
/// and a correct `tip_lag_blocks` is non-zero rather than coincidentally 0.
fn tape_with_headroom(events_to: u64, tip: u64) -> Arc<TapeSource> {
    let t = Arc::new(TapeSource::new());
    for b in 1..=events_to {
        t.insert_block(
            b,
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(account(1).as_str(), account(2).as_str(), (100 * b) as u128)],
            ),
        );
    }
    for b in (events_to + 1)..=tip {
        t.insert_block(b, empty_block(b, 0, 1_700_000_000 + b));
    }
    t.advance_tip_to(tip);
    t
}

fn labelled_values(text: &str, name: &str) -> Vec<(String, u64)> {
    text.lines()
        .filter(|l| l.starts_with(name) && l[name.len()..].starts_with('{'))
        .filter_map(|l| {
            let label = l[name.len()..]
                .split('}')
                .next()?
                .trim_start_matches('{')
                .to_string();
            let v = l.rsplit(' ').next()?.parse().ok()?;
            Some((label, v))
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_nest_publishes_its_tip_to_metrics_not_only_to_ready() {
    let dir = tempfile::tempdir().unwrap();
    let name = "tipgauge";
    let tape = tape_with_headroom(20, 20);
    let cfg = scaffold_nest(dir.path(), name, USDC);

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

    let store = rt.state.store.clone();
    assert!(
        wait_until(POLL_TIMEOUT, || {
            store.get_meta("last_block").ok().flatten().as_deref() == Some("20")
        })
        .await,
        "the nest must index the tape before its gauges mean anything"
    );

    // The nest has polled the source and reached the tip. Whatever it learned about the tip must be
    // on the surface Prometheus scrapes, not only on /ready.
    let m = METRICS.nest(name);
    assert!(
        m.tip() > 0,
        "the nest indexed to block 20 but its own `tip` gauge is still 0. `/ready` would look fine \
         here because a solo runtime falls back to the process-global gauge - which is exactly how \
         this survived in production (#750)"
    );

    let text = METRICS.render();
    let tips = labelled_values(&text, "nuthatch_nest_tip_height");
    let mine = tips
        .iter()
        .find(|(l, _)| l.contains(name))
        .unwrap_or_else(|| panic!("no nuthatch_nest_tip_height for `{name}`:\n{text}"));
    assert!(
        mine.1 > 0,
        "the exporter published `nuthatch_nest_tip_height{{{}}} 0` for a nest that has indexed to \
         the tip. An unset tip then saturates `tip_lag_blocks` to 0 - the healthiest possible \
         reading - so no alert on tip lag could ever fire.\n{text}",
        mine.0
    );

    let polls = labelled_values(&text, "nuthatch_nest_last_poll_unixtime");
    if let Some((label, v)) = polls.iter().find(|(l, _)| l.contains(name)) {
        assert!(
            *v > 0,
            "`nuthatch_nest_last_poll_unixtime{{{label}}}` is 0 after a successful poll. Same cause: \
             `mark_poll_ok` was being called on the process-global struct rather than the nest's."
        );
    }

    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;
}

/// The gauge must be able to read non-zero lag, or it cannot be told from the broken state.
///
/// In production every nest read `tip_lag_blocks 0` while genuinely 2-4 blocks behind. A test that
/// only checked "lag is 0 when caught up" would have passed throughout.
#[tokio::test(flavor = "multi_thread")]
async fn tip_lag_can_be_non_zero_and_is_not_merely_saturating_to_health() {
    let dir = tempfile::tempdir().unwrap();
    let name = "tiplag";
    // Events to block 5, but the source tip is 40: the nest starts well behind.
    let tape = tape_with_headroom(5, 40);
    let cfg = scaffold_nest(dir.path(), name, USDC);

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

    let m = METRICS.nest(name);
    // The tip is what must arrive; `last_block` catching up is not the subject.
    assert!(
        wait_until(POLL_TIMEOUT, || m.tip() > 0).await,
        "the nest polled a source whose tip is block 40 and never recorded a tip at all. This is \
         the #750 defect: `index_loop` published the tip to the process-global gauge only, so the \
         nest's own gauge - the one `/metrics` renders - stayed at 0 forever"
    );
    assert_eq!(
        m.tip(),
        40,
        "the recorded tip must be the source's tip, not some other nest's or a stale value"
    );

    let ingest = rt.ingest;
    ingest.abort();
    let _ = ingest.await;
}
