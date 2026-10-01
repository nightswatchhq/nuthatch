//! RFC-0042 slice 6, experiment A1: is the flat ~40 qps of #986 an engine property or a
//! deployment property?
//!
//! **The controlled variable is the concurrency discipline, and nothing else.** Every arm calls the
//! same `nuthatch::analytics::query_guarded` against the same directory with the same SQL, so the
//! engine, the view definitions, the guards and the `DUCK_CACHE` behaviour are identical across
//! arms. What differs is only what wraps the call:
//!
//! - `mutex`     - one global mutex held across the whole query. This is the model #986's harness
//!                 built (`Arc<Mutex<Connection>>`, lock taken before `prepare` and dropped after
//!                 the last row) and described as "nuthatch's actual serving path".
//! - `gate<N>`   - a semaphore of N permits, fail-fast on contention, which is what `serve.rs`
//!                 actually does (`SQL_MAX_CONCURRENCY = 2`, `try_acquire_owned`, 503 on refusal).
//!                 `gate2` is therefore the shipped `/sql` surface; `gate4/8/16` are the pool sizes
//!                 the slice-6 brief asks for.
//! - `unbounded` - no discipline at all: the ceiling the engine and the box can reach.
//!
//! A refused request under `gate<N>` is counted separately and is *not* a latency sample, because
//! the product returns 503 immediately rather than queueing; folding an instant refusal into the
//! latency distribution would flatter the gated arms.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use nuthatch::analytics::{query_guarded, QueryGuard};

/// A fail-fast counting semaphore: `try_take` mirrors `Semaphore::try_acquire_owned`, refusing
/// rather than queueing, which is the behaviour `serve.rs` chose and the reason a saturated `/sql`
/// answers 503 instead of building a backlog.
struct Gate {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(n: usize) -> Self {
        Gate { permits: Mutex::new(n), cv: Condvar::new() }
    }
    fn try_take(&self) -> bool {
        let mut p = self.permits.lock().unwrap();
        if *p == 0 {
            return false;
        }
        *p -= 1;
        true
    }
    fn give(&self) {
        *self.permits.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

enum Arm {
    Mutex(Mutex<()>),
    Gate(Gate),
    Unbounded,
}

fn main() -> anyhow::Result<()> {
    let dir: PathBuf = std::env::var("NEST").expect("NEST=<nest dir>").into();
    let sql = std::env::var("SQL").expect("SQL=<statement>");
    let label = std::env::var("QLABEL").unwrap_or_else(|_| "q".into());
    let clients: usize = std::env::var("CLIENTS").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let repeats: usize = std::env::var("REPEATS").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    // Whether a queued caller's wait counts as its latency. See the `Arm::Mutex` branch.
    let wait_inclusive = std::env::var("WAIT_INCLUSIVE").map(|v| v != "0").unwrap_or(true);
    let arms: Vec<String> = std::env::var("ARMS")
        .unwrap_or_else(|_| "mutex,gate2,gate4,gate8,gate16,unbounded".into())
        .split(',')
        .map(|s| s.to_string())
        .collect();

    // #986's guard, so a refusal or a timeout means the same thing it means in the product.
    let guard = QueryGuard {
        timeout: std::time::Duration::from_secs(30),
        max_rows: 50_000,
    };

    // **Warm-up, discarded.** The first call pays for the page cache over 10 480 Parquet footers and
    // for the first `open_locked_duckdb`; charging that to whichever arm happens to run first is how
    // an ordering artefact gets published as an engine property.
    let t = Instant::now();
    let warm = query_guarded(&dir, &sql, guard)?;
    println!(
        "WARMUP\tnest={}\tsql={label}\trows={}\tms={:.1}",
        dir.display(),
        warm.rows.len(),
        t.elapsed().as_secs_f64() * 1000.0
    );

    for arm_name in &arms {
        let arm = match arm_name.as_str() {
            "mutex" => Arm::Mutex(Mutex::new(())),
            "unbounded" => Arm::Unbounded,
            g if g.starts_with("gate") => Arm::Gate(Gate::new(g[4..].parse()?)),
            other => anyhow::bail!("unknown arm {other}"),
        };
        let arm = Arc::new(arm);
        let refused = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(AtomicUsize::new(0));

        let start = Instant::now();
        let lat: Vec<u128> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..clients)
                .map(|_| {
                    let arm = arm.clone();
                    let refused = refused.clone();
                    let errors = errors.clone();
                    let dir = dir.clone();
                    let sql = sql.clone();
                    sc.spawn(move || {
                        let mut v = Vec::with_capacity(repeats);
                        for _ in 0..repeats {
                            match &*arm {
                                Arm::Mutex(m) => {
                                    // **The clock starts before the lock**, which is what #986's
                                    // harness does and what a caller experiences: time spent queued
                                    // behind another caller's query is latency, not free. Timing
                                    // from after acquisition measures service time instead, and
                                    // service time is flat under serialisation by construction - it
                                    // is exactly the statistic that cannot see a queue.
                                    // `WAIT_INCLUSIVE=0` restores the service-time clock so the two
                                    // can be reported side by side.
                                    let t0 = Instant::now();
                                    let _g = m.lock().unwrap();
                                    let t = if wait_inclusive { t0 } else { Instant::now() };
                                    if query_guarded(&dir, &sql, guard).is_err() {
                                        errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                    v.push(t.elapsed().as_micros());
                                }
                                Arm::Gate(g) => {
                                    if !g.try_take() {
                                        // The product's 503. Not a latency sample.
                                        refused.fetch_add(1, Ordering::Relaxed);
                                        continue;
                                    }
                                    let t = Instant::now();
                                    if query_guarded(&dir, &sql, guard).is_err() {
                                        errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                    v.push(t.elapsed().as_micros());
                                    g.give();
                                }
                                Arm::Unbounded => {
                                    let t = Instant::now();
                                    if query_guarded(&dir, &sql, guard).is_err() {
                                        errors.fetch_add(1, Ordering::Relaxed);
                                    }
                                    v.push(t.elapsed().as_micros());
                                }
                            }
                        }
                        v
                    })
                })
                .collect();
            hs.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        let wall = start.elapsed().as_secs_f64();

        let mut l = lat.clone();
        l.sort_unstable();
        let pc = |q: f64| if l.is_empty() { 0 } else { l[((l.len() as f64 - 1.0) * q).round() as usize] };
        // Throughput counts *served* queries over wall time. Refusals are reported beside it rather
        // than folded in: a gate that refuses 94% of offered load has not served 94% faster.
        println!(
            "CONC\tarm={arm_name}\twait_inclusive={wait_inclusive}\tsql={label}\tclients={clients}\trepeats={repeats}\tserved={}\trefused={}\terrors={}\tqps={:.2}\tp50_ms={:.1}\tp95_ms={:.1}\tp99_ms={:.1}\twall_s={:.1}",
            l.len(),
            refused.load(Ordering::Relaxed),
            errors.load(Ordering::Relaxed),
            l.len() as f64 / wall,
            pc(0.50) as f64 / 1000.0,
            pc(0.95) as f64 / 1000.0,
            pc(0.99) as f64 / 1000.0,
            wall
        );
    }
    Ok(())
}
