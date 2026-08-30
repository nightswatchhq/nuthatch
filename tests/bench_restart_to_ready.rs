//! **RFC-0042 §11's `restart-to-ready` row (#992).** Warm restart is thoroughly covered for
//! *correctness* - `e2e_warm_restart.rs` asserts a restart rebuilds balances identically to a clean
//! replay - and never timed. §3 asks for "cold startup, warm restart, registration/reconstruction,
//! `/ready`"; this measures the part that varies with stored data.
//!
//! **`#[ignore]` on purpose.** A timing inside the CI-critical suite is flaky under contention and
//! machine-dependent, and would let a slow box fail a test about determinism. `footprint` and
//! `point-read latency` are separate jobs for that reason. Run it deliberately:
//!
//! ```text
//! cargo test --test bench_restart_to_ready -- --ignored --nocapture
//! ```
//!
//! **What it does and does not measure.** `spawn_nest` on a populated directory does the
//! reconstruction - opening the hot store, folding the stored factory events back into the
//! discovered-child registry, attaching sealed segments. Timing it against `spawn_nest` on an empty
//! directory separates that cost from the constant. It is **in-process**: the tape source is
//! test-only, so process spawn and HTTP `/ready` are excluded rather than quietly folded in.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::tape::*;
use nuthatch::indexer;

fn transfer_at(b: u64) -> BlockFixture {
    transfers_block(
        b,
        0,
        1_700_000_000 + b,
        USDC,
        &[(account(1).as_str(), account(2).as_str(), (100 * b) as u128)],
    )
}

/// A tape of `blocks` transfers plus one empty block past them, tip at the end - the empty block is
/// what makes the loop process a window after finality moves, so sealing actually happens.
fn tape_of(blocks: u64) -> Arc<TapeSource> {
    let tape = Arc::new(TapeSource::new());
    for b in 1..=blocks {
        tape.insert_block(b, transfer_at(b));
    }
    tape.insert_block(blocks + 1, empty_block(blocks + 1, 0, 1_700_000_100));
    tape.advance_tip_to(blocks + 1);
    tape
}

async fn spawn_named(
    dir: &std::path::Path,
    tape: Arc<TapeSource>,
    name: &str,
) -> indexer::NestRuntime {
    let cfg = scaffold_nest(dir, name, USDC);
    indexer::spawn_nest(
        tape,
        dir.to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest")
}

/// Stop a nest and wait for it to actually be gone - redb is single-writer and refuses a second open
/// while the aborted task's stack still holds the handle, so a respawn without this is a race rather
/// than a restart.
async fn shutdown(rt: indexer::NestRuntime) {
    let indexer::NestRuntime {
        state,
        ingest,
        alert_worker,
    } = rt;
    ingest.abort();
    let _ = ingest.await;
    if let Some(w) = alert_worker {
        w.abort();
        let _ = w.await;
    }
    drop(state);
}

async fn wait_indexed(store: &dyn nuthatch::store::HotStore, upto: u64) -> bool {
    let want = upto.to_string();
    wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some(want.as_str())
    })
    .await
}

/// Median of a small sample. `noise-floor.md` asks for medians rather than means or single runs; the
/// sample here is small enough that the figure is indicative, and the test says so rather than
/// dressing three runs up as a benchmark.
fn median(mut v: Vec<Duration>) -> Duration {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Balances as a sorted list - the observable that tells us the nest is analytically current, and
/// the same shape `e2e_warm_restart.rs` compares.
fn balances_of(rt: &indexer::NestRuntime) -> Vec<(String, i128)> {
    rt.state.balances.flush();
    let mut v = rt.state.balances.top(1_000);
    v.sort();
    v
}

/// Poll tightly until the rebuilt view matches what the nest held before it was stopped.
///
/// **Not `wait_until`.** That is the right tool for a correctness test - a bounded poll on observable
/// state - but its interval becomes the measurement's granularity, and a 50 ms poll cannot see a 12 ms
/// rebuild. Here the poll interval *is* the error bar, so it is 1 ms and stated.
async fn time_until_current(rt: &indexer::NestRuntime, want: &[(String, i128)]) -> Duration {
    let t = Instant::now();
    loop {
        if balances_of(rt) == want {
            return t.elapsed();
        }
        if t.elapsed() > Duration::from_secs(30) {
            panic!(
                "the restarted nest never became analytically current; a timing over a nest that \
                    never caught up is not a measurement"
            );
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing benchmark - run explicitly with --ignored"]
async fn restart_to_ready_against_stored_size() {
    const REPEATS: usize = 3;
    println!(
        "\n{:<10} {:>16} {:>16} {:>14}",
        "blocks", "cold to current", "restart-to-ready", "warm/cold"
    );
    println!("{}", "-".repeat(60));

    for blocks in [10u64, 100, 500] {
        let mut cold = Vec::new();
        let mut warm = Vec::new();

        for _ in 0..REPEATS {
            let dir = tempfile::tempdir().unwrap();

            // **Cold**: empty directory, index the whole tape, time to analytically current.
            let rt = spawn_named(dir.path(), tape_of(blocks), "bench").await;
            let t = Instant::now();
            assert!(
                wait_indexed(rt.state.store.as_ref(), blocks + 1).await,
                "the nest must reach block {}; a restart over an empty store measures nothing",
                blocks + 1
            );
            let want = balances_of(&rt);
            assert!(
                !want.is_empty(),
                "the view must hold balances to compare against"
            );
            cold.push(t.elapsed());
            shutdown(rt).await;

            // **Warm**: same directory, now populated. The clock starts *before* `spawn_nest` and
            // stops when the view is rebuilt, because the reconstruction happens inside that call.
            //
            // Two wrong measurements were taken before this one, and both looked plausible:
            //   - timing `spawn_nest` alone and subtracting the cold spawn gave a reconstruction cost
            //     of **zero**, because a warm spawn is *faster* than a cold one - the cold path pays
            //     to create an empty store;
            //   - timing only spawn-return to view-current gave a flat **29 µs** at every size,
            //     because by then the work is already done.
            // The interval that means anything is the whole of it.
            let t = Instant::now();
            let rt = spawn_named(dir.path(), tape_of(blocks), "bench").await;
            let settle = time_until_current(&rt, &want).await;
            warm.push(t.elapsed());
            assert!(
                settle < Duration::from_millis(5),
                "the view took {settle:?} to settle after spawn returned; if that grows, the \
                 reconstruction has moved out of `spawn_nest` and this measurement needs re-reading"
            );
            shutdown(rt).await;
        }

        let c = median(cold);
        let w = median(warm);
        println!(
            "{:<10} {:>14.1?} {:>14.1?} {:>13.2}",
            blocks,
            c,
            w,
            w.as_secs_f64() / c.as_secs_f64()
        );
    }
    println!(
        "\nIn-process, `spawn_nest` to rebuilt view. Process spawn and HTTP /ready are excluded - the \
         tape source is test-only, so measuring them here would report a narrower thing than §11's \
         row names. Poll interval 1 ms, medians of 3.\n"
    );
}
