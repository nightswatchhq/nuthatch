//! `nuthatch bench backfill` - honest, reproducible backfill throughput measurement (RFC-0004).
//!
//! Runs the real fetch → decode → store path over a *pinned* block range and reports sustained
//! events/sec, wall-clock, peak RSS, and RPC requests, emitting a `bench-report.json`. Measure
//! first, optimise second: nothing here optimises anything - it establishes the baseline that the
//! seal-direct / adaptive-chunker / pipeline work (later slices) must each beat on a before/after.
//!
//! The house rule (RFC-0004): every published number traces to a report artifact produced here, with
//! date, provider, hardware, and commit. No hand-typed numbers on the website.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::chains;
use crate::cli::BackfillBenchArgs;
use crate::config::Config;
use crate::factory::{ChildRegistry, FactorySet};
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::source::Source;
use crate::store::Store;

/// Which backfill path a run takes. Extracted from `one_run`'s `if` chain so the choice is testable
/// without a network: the factory-nest bug this exists to prevent was a *dispatch* bug, not a decode
/// or fetch bug, and it was invisible because the wrong path still returned a plausible number.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum BackfillPath {
    /// Sequential two-pass factory backfill - the only path that discovers children (RFC-0009 §3).
    Factory,
    /// Concurrent seal-direct.
    Pipelined,
    /// Sequential seal-direct.
    Direct,
    /// Decode → redb hot store.
    HotStore,
}

/// A factory nest takes the factory path **whatever else was asked for**: `--concurrency` cannot
/// override it, because the pipelined path has no child registry and would silently measure the
/// factory's own events alone.
fn choose_path(is_factory: bool, seal_direct: bool, concurrency: usize) -> BackfillPath {
    if is_factory {
        BackfillPath::Factory
    } else if seal_direct && concurrency > 1 {
        BackfillPath::Pipelined
    } else if seal_direct {
        BackfillPath::Direct
    } else {
        BackfillPath::HotStore
    }
}

/// One run's raw measurements.
struct Run {
    events: u64,
    wall_clock_s: f64,
    events_per_sec: f64,
    peak_rss_mb: u64,
    rpc_requests: u64,
    /// Children discovered during the run - 0 for a nest with no `[[factories]]`. A factory nest that
    /// reports 0 here has measured nothing but its own factory events, which is the failure this
    /// field exists to make visible.
    children: u64,
}

/// The published artifact: medians across runs plus the pinned inputs and provenance.
#[derive(Debug, Serialize)]
pub struct BenchReport {
    pub bench: &'static str,
    pub label: Option<String>,
    pub nest: String,
    pub chain: String,
    pub from_block: u64,
    pub to_block: u64,
    pub blocks: u64,
    /// The window the backfill *started* from - the chain default (20 on mainnet, deliberately small
    /// for a dense L1 tip). On the seal-direct paths it is only a starting point: the controller sizes
    /// every subsequent window from what came back (RFC-0029 §6f), so this number does not describe
    /// what the run actually did.
    ///
    /// It is named for what it is because the old name (`window`) invited exactly the wrong reading.
    /// This run started at 20 and reached the 100,000 ceiling; a report claiming `"window": 20` reads
    /// as though 22.2M blocks were fetched 20 at a time, which would be 1,110,000 requests rather than
    /// the 454 it took. `rpc_requests` is the honest measure of range control, and the reason there is
    /// no `--window` override here is that there is no longer anything useful to override.
    pub initial_window: u64,
    /// Whether the window adapted during the run (true on the seal-direct paths).
    pub window_adaptive: bool,
    pub seal_direct: bool,
    pub concurrency: usize,
    pub runs: usize,
    /// Medians across runs.
    pub events: u64,
    pub wall_clock_s: f64,
    pub events_per_sec: f64,
    pub peak_rss_mb: u64,
    pub rpc_requests: u64,
    /// Whether the nest declares `[[factories]]`, and so took the sequential two-pass factory
    /// backfill rather than the static-address path. Recorded because it changes what `concurrency`
    /// means (it does not apply) and because a factory nest measured without it is measuring the
    /// wrong workload.
    pub factory: bool,
    /// Children discovered during the run. For a factory workload this is the number that says the
    /// harness actually followed the templates: `"factory": true` with `"children": 0` is a broken
    /// measurement, not a fast one.
    pub children: u64,
    pub commit: Option<String>,
    /// The RPC endpoint's host, never the full URL - an API key in a committed benchmark report is a
    /// credential leak, and these get pasted into issues.
    ///
    /// RFC-0004's house rule is that every published number traces to provider, hardware, date and
    /// commit. Three of those were already recorded and this one was a documentation promise. A number
    /// whose provider is unknown is not comparable to any other number, because provider caps and
    /// latency dominate backfill throughput far more than anything in this codebase does.
    pub provider: Option<String>,
    /// Enough hardware to make two reports comparable: cores and total RAM.
    pub hardware: Option<String>,
}

/// The endpoint's host, with any credentials stripped.
///
/// Providers put API keys in the path (`/v2/<key>`) as often as in a header, so the path goes too -
/// host and scheme identify the provider, which is all a report needs.
fn provider_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split('/').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// Cores and total RAM, best-effort. `None` rather than a guess when it cannot be read - an invented
/// hardware string is worse than an absent one, because it looks authoritative.
fn hardware_summary() -> Option<String> {
    let cores = std::thread::available_parallelism().ok()?.get();
    let ram_gb = total_ram_gb();
    Some(match ram_gb {
        Some(gb) => format!("{cores} cores, {gb} GB RAM"),
        None => format!("{cores} cores"),
    })
}

fn total_ram_gb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        return Some((kb + 512 * 1024) / (1024 * 1024));
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        let bytes: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        return Some(bytes / (1024 * 1024 * 1024));
    }
    #[allow(unreachable_code)]
    None
}

/// Resolve `--keep`, refusing the one path a caller is most likely to type by mistake.
///
/// Every run clears this directory before starting, so pointing it at the nest itself would delete
/// `nuthatch.toml` and the indexed data it was aimed at - the worst outcome available from this
/// flag. The check runs before the first run rather than on the way out, because the wipe is the
/// first thing `one_run` does with the path.
///
/// Split out of `backfill` so it is *reachable*: inline, the only way to reach the guard was a full
/// backfill against a live RPC, which is why it shipped on inspection alone. A guard nobody
/// exercises is the argument this whole change is built on, so it does not get an exemption.
fn resolve_keep(keep: &Option<String>) -> Result<Option<PathBuf>> {
    let Some(p) = keep.as_ref().map(PathBuf::from) else {
        return Ok(None);
    };
    if p.join(crate::config::CONFIG_FILE).exists() {
        bail!(
            "--keep {} is a nest directory (it holds {}), and every bench run clears its work dir \
             before starting. Point --keep at a new or dedicated path, then query it with \
             `nuthatch sql --dir <that path>`.",
            p.display(),
            crate::config::CONFIG_FILE
        );
    }
    Ok(Some(p))
}

pub async fn backfill(args: BackfillBenchArgs) -> Result<()> {
    if args.to < args.from {
        bail!("--to ({}) is before --from ({})", args.to, args.from);
    }
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    let registry = Arc::new(DecodeRegistry::from_nest(&dir, &config)?);

    // **A factory nest must be measured through the factory path** (RFC-0009 §3). The static
    // `addresses` filter below is fixed at start-up, so a factory nest measured through the plain
    // paths sees only its own creation events and never the children they announce - it reports a
    // fast, small, entirely wrong number, and reports success. That is the "harness measuring a
    // strawman" failure of RFC-0029 §4b in a second place, found running OBIB case 6 (which is
    // *entirely* child activity: 232 factory events against 35,039 expected records).
    let factory = {
        let fs = FactorySet::build(&config)?;
        if fs.is_empty() {
            None
        } else {
            Some(fs)
        }
    };

    let rpc_urls = match &args.rpc {
        Some(u) => vec![u.clone()],
        None => config.nest.rpc_urls.clone(),
    };
    let window = chains::lookup(&config.nest.chain)
        .map(|c| c.log_window)
        .unwrap_or(20);
    let addresses: Vec<String> = registry
        .addresses()
        .iter()
        .map(|a| format!("0x{}", hex::encode(a)))
        .collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    println!(
        "bench backfill: nest '{}' on {}, blocks {}..={} ({} blocks), window {}, {} run(s)",
        config.nest.name,
        config.nest.chain,
        args.from,
        args.to,
        args.to - args.from + 1,
        window,
        args.runs,
    );

    println!(
        "storage path: {}{}",
        if args.seal_direct {
            "seal-direct (decode → Parquet, no hot store)"
        } else {
            "hot store (decode → redb)"
        },
        if args.seal_direct && args.concurrency > 1 && factory.is_none() {
            format!(", {}-way concurrent fetch", args.concurrency)
        } else {
            String::new()
        }
    );
    if let Some(fs) = &factory {
        // Say it out loud rather than silently ignoring the flag: the factory backfill is the
        // sequential two-pass path (pass 2 needs pass 1's discoveries), so `--concurrency` does not
        // apply. A number that quietly ignored the flag would be unreproducible by anyone reading the
        // command line that produced it.
        println!(
            "factory nest: {} template(s) - sequential two-pass factory backfill{}",
            fs.templates().len(),
            if args.concurrency > 1 {
                ", --concurrency does not apply"
            } else {
                ""
            }
        );
    }

    let keep = resolve_keep(&args.keep)?;
    if let Some(p) = keep.as_deref() {
        println!(
            "  --keep: writing run data to {} (last run survives)",
            p.display()
        );
    }

    let mut runs = Vec::with_capacity(args.runs);
    for run in 1..=args.runs {
        let r = one_run(
            &rpc_urls,
            &registry,
            factory.as_ref(),
            &addresses,
            &topic0s,
            args.from,
            args.to,
            window,
            args.seal_direct,
            args.concurrency,
            run,
            keep.as_deref(),
        )
        .await?;
        println!(
            "  run {run}/{}: {} events in {:.1}s = {:.0} ev/s, peak {} MB, {} rpc req{}",
            args.runs,
            r.events,
            r.wall_clock_s,
            r.events_per_sec,
            r.peak_rss_mb,
            r.rpc_requests,
            if factory.is_some() {
                format!(", {} children", r.children)
            } else {
                String::new()
            }
        );
        runs.push(r);
    }

    let report = BenchReport {
        bench: "backfill",
        label: args.label.clone(),
        nest: config.nest.name.clone(),
        chain: config.nest.chain.clone(),
        from_block: args.from,
        to_block: args.to,
        blocks: args.to - args.from + 1,
        initial_window: window,
        // The hot-store path still fetches at a fixed width; only the seal-direct paths adapt.
        window_adaptive: args.seal_direct,
        seal_direct: args.seal_direct,
        // The factory path is sequential, so reporting the flag's value would describe a run that did
        // not happen.
        concurrency: if args.seal_direct && factory.is_none() {
            args.concurrency
        } else {
            1
        },
        runs: args.runs,
        events: median_u64(runs.iter().map(|r| r.events)),
        wall_clock_s: round2(median_f64(runs.iter().map(|r| r.wall_clock_s))),
        events_per_sec: round2(median_f64(runs.iter().map(|r| r.events_per_sec))),
        peak_rss_mb: median_u64(runs.iter().map(|r| r.peak_rss_mb)),
        rpc_requests: median_u64(runs.iter().map(|r| r.rpc_requests)),
        factory: factory.is_some(),
        children: median_u64(runs.iter().map(|r| r.children)),
        commit: git_commit(),
        provider: rpc_urls.first().and_then(|u| provider_of(u)),
        hardware: hardware_summary(),
    };

    let json = serde_json::to_string_pretty(&report)?;
    println!("\n{json}");
    if let Some(out) = &args.out {
        std::fs::write(out, &json).with_context(|| format!("failed to write {out}"))?;
        println!("\nwrote {out}");
    }
    Ok(())
}

/// Read-path bench report (`nuthatch bench query`): entity point-read latency and the `/sql` hot∪cold
/// scan cost + peak RSS. The regression guard the perf refactors (bound the hot-scan, persistent DuckDB
/// connection, compact row format) must each beat on a before/after - none of these were measurable
/// before, so they could regress silently.
#[derive(Debug, Serialize)]
pub struct QueryBenchReport {
    pub bench: &'static str,
    pub label: Option<String>,
    pub nest: String,
    pub chain: String,
    /// Rows in the unsealed hot tip a `/sql` query materialises into temp tables - the scan-cost driver
    /// and the #1 RAM risk on deep-finality L2s.
    pub hot_rows: u64,
    pub sealed_through: u64,
    pub reads: usize,
    /// Whether the timed point-reads ran against an already-warmed cache. Always true, and recorded
    /// rather than assumed: a point-read latency means nothing without it, and a reader comparing two
    /// releases has no other way to know these are not cold-start numbers.
    pub warm_cache: bool,
    pub point_read_p50_us: f64,
    pub point_read_p99_us: f64,
    pub point_read_p999_us: f64,
    pub sql: String,
    pub sql_iters: usize,
    pub sql_p50_ms: f64,
    pub sql_p99_ms: f64,
    pub sql_peak_rss_mb: u64,
    pub commit: Option<String>,
    /// Cores and total RAM, as [`BenchReport`] records. A point-read latency is *entirely* a property
    /// of the machine's storage and cache, so two of these reports are not comparable at all without
    /// it - more so than for backfill, where the provider dominates. It was already on the write-path
    /// report and missing here.
    pub hardware: Option<String>,
}

/// Enforce the `--min-reads` floor and the `--max-point-read-*` ceilings, listing **every** breach
/// rather than the first.
///
/// This is what makes the bench a gate rather than a print statement: CLAUDE.md requires benchmark
/// regressions to fail the build, and until now nothing compared a point-read number to anything.
/// The comparison lives here, in Rust, rather than in the CI shell script (as `footprint.sh` does its
/// own) precisely so `cargo test --lib` covers it - an untested gate is how a gate silently stops
/// gating.
///
/// **`--min-reads` is the important one, not the ceilings.** A nest with no sampled keys reports
/// `p50 = 0.0`, and zero is under every ceiling anyone would ever set - so the gate would pass
/// loudest exactly when it had measured nothing at all. That is the "a criterion phrased as the
/// absence of an effect passes trivially when the mechanism is missing" failure this project has hit
/// five times, and `footprint.sh` guards its own version of it by asserting the row count before
/// comparing the peak. Requiring the reads is the same guard.
///
/// A limit that is not passed is neither a pass nor a failure: it is simply not asked for, which is
/// how an operator running the bench by hand should experience it.
///
/// **The failure message names where to re-baseline, because the obvious place is the wrong one.**
/// It used to say "re-baselining against a committed report", and the only committed report is
/// `docs/bench/point-read.json` - a 32-core dev-box number (p50 1.24µs) that `docs/benchmarks.md`
/// explicitly tells the reader is *not* what the ceiling was set against (the runner's baseline is
/// 0.59-0.82µs). Following the instruction re-derives a runner ceiling from dev-box hardware, and
/// this gate is the one that already proved baseline and regression do not scale together across
/// machines: the scan mutation ran *faster* on the runner (18.15µs) than on the dev box (24.17µs).
/// This is #395's lesson on the RSS harness in miniature - a FAIL message that points at a
/// different scenario than the one it enforces is a trap with a comment on it - so the message
/// points at the enforcing machine instead. #385 tracks committing a runner-produced artifact,
/// which is what would let this instruction name a file again.
fn check_gate(
    report: &QueryBenchReport,
    max_p50_us: Option<f64>,
    max_p99_us: Option<f64>,
    min_reads: Option<usize>,
) -> Result<()> {
    let mut breaches: Vec<String> = Vec::new();
    if let Some(min) = min_reads {
        if report.reads < min {
            breaches.push(format!(
                "timed {} point-read(s), below the --min-reads floor of {min} - the nest is empty or \
                 barely indexed, so its latencies are not a measurement and passing a ceiling with \
                 them would be a false pass",
                report.reads
            ));
        }
    }
    for (name, measured, max) in [
        ("point-read p50", report.point_read_p50_us, max_p50_us),
        ("point-read p99", report.point_read_p99_us, max_p99_us),
    ] {
        if let Some(max) = max {
            if measured > max {
                breaches.push(format!(
                    "{name} {measured:.1}µs exceeds the {max:.1}µs ceiling"
                ));
            }
        }
    }
    if breaches.is_empty() {
        return Ok(());
    }
    bail!(
        "point-read gate failed - {}.\nThis is a gate, not a warning: either the change made \
         point-reads slower, or the ceiling needs re-baselining - measured on the machine that \
         enforces it, and with a stated reason.\nNot from docs/bench/point-read.json: that is a \
         32-core dev-box reference point, and this ceiling was set on the CI runner, whose \
         baseline and whose regression are both different numbers. See \"Where the ceilings come \
         from\" in docs/benchmarks.md.",
        breaches.join("; ")
    )
}

/// `nuthatch bench query` - measure the read path against an already-indexed nest (run offline: it
/// opens the store directly). Establishes the point-read and `/sql` scan baselines the #40 perf
/// refactors are gated on.
pub fn query(args: crate::cli::QueryBenchArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)?;
    let store = Store::open(&dir.join(crate::config::DB_FILE))
        .context("open the nest store (stop `nuthatch dev` first - the bench needs the DB)")?;

    let keys = store.sample_entity_keys(args.reads)?;
    let hot = store.hot_rows_by_table()?;
    let hot_rows: u64 = hot.values().map(|v| v.len() as u64).sum();
    let sealed_through = store.sealed_through();

    // --- Point-read latency: time get_entity over the sampled keys. ---
    let (p50_us, p99_us, p999_us, n_reads) = if keys.is_empty() {
        (0.0, 0.0, 0.0, 0)
    } else {
        // **Warm the cache first, and discard it.** The `/sql` half below has always done this and the
        // point-read half never did, so a timed read could be paying for a redb page fault - which
        // makes it a measurement of the page cache's state rather than of the read path, and a gate
        // needs the same workload every run to mean anything.
        //
        // Warm is therefore what the gate measures, and `warm_cache` records that so nobody reads
        // these as cold-start latencies. Cold-start is a real and different question - it is not the
        // one a regression gate can ask, and it is not asked here.
        //
        // Measured warm on a 32-core Linux box, 256 hot rows, 15 runs at one commit: p50 0.66-1.59µs,
        // p99 0.77-1.88µs. The unwarmed spread was *not* measured, so this is a correctness argument
        // about what is being timed, not a claim of a stability improvement.
        //
        // **Every read is required to hit, and that is a gate condition rather than a sanity check.**
        // The keys come from `sample_entity_keys` over this same store, so a miss is impossible unless
        // the read path is broken - and a broken read path is *faster* than a working one. Timing a
        // `get_entity` that returns `None` for everything would sail under any ceiling, so a gate that
        // discards the result passes most loudly exactly when the thing it guards has been removed.
        // Counting hits is what stops the measurement from being vacuous.
        let mut hits = 0usize;
        for k in &keys {
            hits += usize::from(store.get_entity(k)?.is_some());
        }
        let mut us: Vec<f64> = Vec::with_capacity(keys.len());
        for k in &keys {
            let t = Instant::now();
            let found = store.get_entity(k)?.is_some();
            // `elapsed` is read before the counter moves, so the bookkeeping is outside the window.
            us.push(t.elapsed().as_nanos() as f64 / 1000.0);
            hits += usize::from(found);
        }
        let expected = keys.len() * 2;
        if hits != expected {
            bail!(
                "point-read gate failed - {} of {} reads returned no entity. The keys were sampled \
                 from this store, so every read must hit: these numbers time a read path that is \
                 not returning data, and a broken read is faster than a working one.",
                expected - hits,
                expected
            );
        }
        us.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        (
            percentile(&us, 50.0),
            percentile(&us, 99.0),
            percentile(&us, 99.9),
            us.len(),
        )
    };

    // --- /sql hot∪cold scan cost: run the query `iters` times, time each, sample peak RSS. ---
    let sql = match &args.sql {
        Some(s) => s.clone(),
        None => {
            // Default: count over the largest hot table (forces the full-tip materialisation); a fully
            // sealed nest (no hot rows) falls back to the first registry table.
            let table = match hot
                .iter()
                .max_by_key(|(_, v)| v.len())
                .map(|(t, _)| t.clone())
            {
                Some(t) => t,
                None => DecodeRegistry::from_nest(&dir, &config)?
                    .tables()
                    .first()
                    .map(|d| d.table.clone())
                    .context("nest has no tables to scan - pass --sql")?,
            };
            format!("SELECT count(*) FROM {table}")
        }
    };
    let guard = || crate::analytics::QueryGuard {
        timeout: Duration::from_secs(60),
        max_rows: 5_000_000,
    };
    // Warm-up (attach segments + build temp tables) kept out of the percentiles.
    crate::analytics::query_hot_cold(&dir, &sql, guard(), &hot, sealed_through)
        .with_context(|| format!("running bench query: {sql}"))?;
    let rss = RssSampler::start();
    let mut ms: Vec<f64> = Vec::with_capacity(args.iters.max(1));
    for _ in 0..args.iters.max(1) {
        let t = Instant::now();
        let _ = crate::analytics::query_hot_cold(&dir, &sql, guard(), &hot, sealed_through)?;
        ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let sql_peak_rss_mb = rss.stop();
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    println!(
        "bench query: nest '{}' on {} - {} hot rows, sealed_through {}, {} point-read(s), {} sql iter(s)",
        config.nest.name, config.nest.chain, hot_rows, sealed_through, n_reads, args.iters,
    );
    println!("  point-read: p50 {p50_us:.1}µs  p99 {p99_us:.1}µs  p99.9 {p999_us:.1}µs");
    println!(
        "  /sql `{}`: p50 {:.1}ms  p99 {:.1}ms  peak {} MB",
        sql,
        percentile(&ms, 50.0),
        percentile(&ms, 99.0),
        sql_peak_rss_mb
    );

    let report = QueryBenchReport {
        bench: "query",
        label: args.label.clone(),
        nest: config.nest.name.clone(),
        chain: config.nest.chain.clone(),
        hot_rows,
        sealed_through,
        reads: n_reads,
        // False on the no-keys branch, where no warm pass runs. `--min-reads` refuses that run
        // anyway, but the field exists to be trusted rather than to be usually right.
        warm_cache: n_reads > 0,
        point_read_p50_us: round2(p50_us),
        point_read_p99_us: round2(p99_us),
        point_read_p999_us: round2(p999_us),
        sql,
        sql_iters: args.iters,
        sql_p50_ms: round2(percentile(&ms, 50.0)),
        sql_p99_ms: round2(percentile(&ms, 99.0)),
        sql_peak_rss_mb,
        commit: git_commit(),
        hardware: hardware_summary(),
    };
    let json = serde_json::to_string_pretty(&report)?;
    println!("\n{json}");
    if let Some(out) = &args.out {
        std::fs::write(out, &json).with_context(|| format!("failed to write {out}"))?;
        println!("\nwrote {out}");
    }

    // The gate last, and **after** the artifact is written: a run that fails the gate is the run whose
    // report you most want to read, so failing before writing it would throw away the evidence.
    check_gate(
        &report,
        args.max_point_read_p50_us,
        args.max_point_read_p99_us,
        args.min_reads,
    )
}

#[allow(clippy::too_many_arguments)]
async fn one_run(
    rpc_urls: &[String],
    registry: &DecodeRegistry,
    factory: Option<&FactorySet>,
    addresses: &[String],
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
    seal_direct: bool,
    concurrency: usize,
    run: usize,
    keep: Option<&std::path::Path>,
) -> Result<Run> {
    let source = RpcClient::new(rpc_urls.to_vec())?;
    // Normally a throwaway work dir per run (redb and/or Parquet segments) - never the nest's own
    // database. `--keep` points it somewhere durable instead, because a case whose criterion is a
    // ROW COUNT cannot be answered by a harness that deletes its rows: OBIB case 3 asks for 100,001
    // block records, and until this existed the only way to produce them threw them away
    // (RFC-0036 §5.1). Every run clears and rewrites the directory, so what survives is the last
    // run's data - stated because a median over 3 runs and a row count from 1 are different things.
    let work = match keep {
        Some(p) => p.to_path_buf(),
        None => std::env::temp_dir().join(format!("nuthatch-bench-{}-{run}", std::process::id())),
    };
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;

    let rss = RssSampler::start();
    let start = Instant::now();
    // A fresh child registry per run: discovery is part of what the run measures, so carrying one
    // across runs would make run 2 cheaper than run 1 and the median a blend of two workloads.
    let mut children = ChildRegistry::new();
    let events = match choose_path(factory.is_some(), seal_direct, concurrency) {
        // Sequential two-pass factory backfill - the same path `dev` takes (RFC-0009 §3). Pass 2
        // fetches the children pass 1 discovered, so it cannot be pipelined against itself.
        BackfillPath::Factory => {
            let fs = factory.expect("choose_path returned Factory without a FactorySet");
            crate::indexer::backfill_direct_factory(
                &source,
                registry,
                fs,
                &mut children,
                &work,
                topic0s,
                from,
                to,
                window,
                fs.force_topic0(),
                |_| Ok(()),
                |_, _| {},
            )
            .await?
        }
        // Pipelined seal-direct: concurrent fetch, in-order deterministic sealing. **This is the
        // production seal-direct path** - `nuthatch dev` on a static nest reaches
        // `backfill_direct_pipelined` regardless of concurrency; the branch above it selects on
        // `factory.is_some()`. Benching `Direct` instead measures a path only the bench takes.
        BackfillPath::Pipelined => {
            crate::indexer::backfill_direct_pipelined(
                &source,
                registry,
                &work,
                addresses,
                topic0s,
                from,
                to,
                window,
                concurrency,
                |_| Ok(()), // bench doesn't persist a resume watermark
                |_, _| {},  // bench doesn't render progress
            )
            .await?
        }
        // Sequential seal-direct: decode → Parquet, bypassing the hot store. Reachable only from the
        // bench (`backfill_direct` has no other non-test caller) - it is the sequential control for
        // `Pipelined`, not what production runs.
        BackfillPath::Direct => {
            crate::indexer::backfill_direct(
                &source, registry, &work, addresses, topic0s, from, to, window,
            )
            .await?
        }
        BackfillPath::HotStore => {
            hot_store_backfill(
                &source, registry, &work, addresses, topic0s, from, to, window,
            )
            .await?
        }
    };
    let wall_clock_s = start.elapsed().as_secs_f64();
    let peak_rss_mb = rss.stop();
    // Keep the data when asked. Without this the flag would create the directory, fill it, and then
    // delete it — which is the whole defect it exists to fix, just relocated.
    if keep.is_none() {
        let _ = std::fs::remove_dir_all(&work);
    }

    Ok(Run {
        events,
        wall_clock_s,
        events_per_sec: if wall_clock_s > 0.0 {
            events as f64 / wall_clock_s
        } else {
            0.0
        },
        peak_rss_mb,
        rpc_requests: source.request_count(),
        children: children.len() as u64,
    })
}

/// The baseline path: decode → redb hot store, with the same batched `block_timestamp` fetch the
/// live `dev` loop does - so the only thing that differs from seal-direct is the storage write.
#[allow(clippy::too_many_arguments)]
async fn hot_store_backfill(
    source: &RpcClient,
    registry: &DecodeRegistry,
    dir: &std::path::Path,
    addresses: &[String],
    topic0s: &[String],
    from: u64,
    to: u64,
    window: u64,
) -> Result<u64> {
    // `DB_FILE`, not a bench-specific name: with `--keep` this directory is meant to be opened by
    // `nuthatch sql --dir <path>`, which looks for the standard store. A private filename would
    // persist the data and still leave it unqueryable, which is the failure this flag exists to fix.
    let store = Store::open(&dir.join(crate::config::DB_FILE))?;
    let mut events = 0u64;
    let mut next = from;
    while next <= to {
        let chunk_to = (next + window - 1).min(to);
        // Same rule as `indexer.rs`: an empty address AND topic filter means *every log on the
        // chain*, not none. A blocks-only nest has neither, and no log could decode without them.
        // The rule is now carried by `LogFilter` rather than restated here (#432).
        let logs = match crate::source::LogFilter::new(addresses, topic0s) {
            None => Vec::new(),
            Some(filter) => source
                .logs(&filter, next, chunk_to)
                .await
                .with_context(|| format!("getLogs {next}..={chunk_to}"))?,
        };
        let mut rows: Vec<_> = logs
            .iter()
            .filter_map(|log| registry.decode(log).ok().flatten())
            .collect();
        // RFC-0036 §4.2, mirroring `indexer.rs`: a `blocks` nest must cover blocks that emitted
        // nothing, so the rows are enumerated from the WINDOW, not from `logs`. Deriving them from
        // logs is what produced "zero rows and a green run" - the exact failure the indexer's own
        // comment warns about, which this harness reimplemented rather than shared.
        if registry.blocks() {
            let want: Vec<u64> = (next..=chunk_to).collect();
            let headers = source.block_headers(&want).await?;
            let mut block_rows: Vec<_> = want
                .iter()
                .filter_map(|b| {
                    headers
                        .get(b)
                        .and_then(|h| crate::registry::block_row(*b, h, registry.timestamps()))
                })
                .collect();
            block_rows.sort_by_key(|r| r.block_number);
            rows.append(&mut block_rows);
        }
        let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
        blocks.sort_unstable();
        blocks.dedup();
        let ts = source.block_timestamps(&blocks).await.unwrap_or_default();
        // **One commit per window, as the real path does** (RFC-0029 §4b).
        //
        // This used to call `put_entity` per row, which is one redb write transaction - and one fsync -
        // per row. `store.rs` PERF-2 records that exact shape as the thing that capped tip-follow
        // throughput far below the decode rate, and why `commit_window` exists. So the benchmark was
        // measuring a path the indexer deliberately does not use, and every events/sec figure it has
        // ever produced was a floor set by fsync rather than by nuthatch.
        //
        // **Any improvement from this change is a harness correction, never a product gain.** Recording
        // it as a speedup would be claiming credit for fixing our own measurement.
        let batch: Vec<(String, String)> = rows
            .iter_mut()
            .map(|r| {
                r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
                (
                    Store::entity_key(r.block_number, r.log_index),
                    r.to_json().to_string(),
                )
            })
            .collect();
        events += batch.len() as u64;
        store.commit_window(&batch, None, chunk_to)?;
        next = chunk_to + 1;
    }
    Ok(events)
}

/// Samples this process's resident set size on a background thread, tracking the peak.
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak_kb: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_kb = Arc::new(AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak_kb.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if let Some(kb) = current_rss_kb() {
                    p.fetch_max(kb, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(120));
            }
        });
        Self {
            stop,
            peak_kb,
            handle: Some(handle),
        }
    }

    /// Stop sampling and return the peak RSS in MB.
    fn stop(mut self) -> u64 {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak_kb.load(Ordering::Relaxed) / 1024
    }
}

/// This process's RSS in KB. Linux via `/proc/self/status`; else `ps` (macOS/BSD). None if neither.
fn current_rss_kb() -> Option<u64> {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                return rest.trim().trim_end_matches("kB").trim().parse().ok();
            }
        }
    }
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

fn median_f64(vals: impl Iterator<Item = f64>) -> f64 {
    let mut v: Vec<f64> = vals.collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    median_of_sorted(&v).unwrap_or(0.0)
}

fn median_u64(vals: impl Iterator<Item = u64>) -> u64 {
    let mut v: Vec<f64> = vals.map(|x| x as f64).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    median_of_sorted(&v).unwrap_or(0.0).round() as u64
}

fn median_of_sorted(v: &[f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let mid = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Nearest-rank percentile of a pre-sorted slice. `p` in `[0, 100]`. Empty slice → 0.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

/// The short commit the bench ran at (provenance for the report), or None outside a git checkout.
fn git_commit() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {

    /// A benchmark report gets pasted into issues and PRs. An API key in one is a credential leak, and
    /// providers put keys in the path as often as in a header.
    #[test]
    fn the_provider_field_carries_a_host_and_never_a_key() {
        assert_eq!(
            provider_of("https://arb-mainnet.g.alchemy.com/v2/SECRETKEY123"),
            Some("arb-mainnet.g.alchemy.com".into())
        );
        assert_eq!(
            provider_of("https://arb1.arbitrum.io/rpc"),
            Some("arb1.arbitrum.io".into())
        );
        // Whatever it returns, it must never contain the key.
        assert!(!provider_of("https://x.example/v2/SECRETKEY123")
            .unwrap()
            .contains("SECRETKEY123"));
        assert_eq!(provider_of("not-a-url"), None);
    }

    /// An invented hardware string is worse than an absent one: it looks authoritative and is not.
    #[test]
    fn hardware_is_reported_or_omitted_never_guessed() {
        let h = hardware_summary().expect("this platform reports core count");
        assert!(h.contains("cores"), "got {h}");
    }
    use super::*;

    #[test]
    fn medians() {
        assert_eq!(median_u64([10u64, 30, 20].into_iter()), 20);
        assert_eq!(median_u64([10u64, 20, 30, 40].into_iter()), 25); // even → mean of middle two
        assert_eq!(median_u64(std::iter::empty()), 0);
        assert!((median_f64([1.0, 3.0, 2.0].into_iter()) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn round_two_dp() {
        assert_eq!(round2(1234.5678), 1234.57);
    }

    #[test]
    fn percentiles_nearest_rank() {
        let v = [10.0, 20.0, 30.0, 40.0, 50.0]; // sorted, 5 elements
        assert_eq!(percentile(&v, 0.0), 10.0); // rank round(0)   = 0 → v[0]
        assert_eq!(percentile(&v, 50.0), 30.0); // rank round(2.0) = 2 → v[2]
        assert_eq!(percentile(&v, 99.0), 50.0); // rank round(3.96)= 4 → v[4]
        assert_eq!(percentile(&v, 100.0), 50.0); // rank round(4)  = 4 → v[4]
        assert_eq!(percentile(&[], 99.0), 0.0); // empty guard
        assert_eq!(percentile(&[42.0], 99.0), 42.0); // single element
    }

    /// **The regression this file exists to prevent.** A factory nest measured through any other path
    /// fetches a fixed address list that never grows, so it counts the factory's own creation events
    /// and none of the child activity the workload is *made of* - and it reports success. Found on
    /// OBIB case 6, where the plain path returned 232 events in 2.6 s against an expected 35,039.
    ///
    /// `--concurrency` must not be able to steer a factory nest off the factory path: the pipelined
    /// backfill takes no `ChildRegistry` and cannot discover anything.
    #[test]
    fn a_factory_nest_always_takes_the_factory_path() {
        for seal_direct in [true, false] {
            for concurrency in [1, 8, 16] {
                assert_eq!(
                    choose_path(true, seal_direct, concurrency),
                    BackfillPath::Factory,
                    "factory nest with seal_direct={seal_direct} concurrency={concurrency}"
                );
            }
        }
    }

    /// A report with the two numbers the gate reads and nothing else that matters.
    fn report(reads: usize, p50: f64, p99: f64) -> QueryBenchReport {
        QueryBenchReport {
            bench: "query",
            label: None,
            nest: "t".into(),
            chain: "mainnet".into(),
            hot_rows: 0,
            sealed_through: 0,
            reads,
            warm_cache: true,
            point_read_p50_us: p50,
            point_read_p99_us: p99,
            point_read_p999_us: p99,
            sql: "SELECT 1".into(),
            sql_iters: 1,
            sql_p50_ms: 0.0,
            sql_p99_ms: 0.0,
            sql_peak_rss_mb: 0,
            commit: None,
            hardware: None,
        }
    }

    /// The gate has to bite on each percentile independently, and only when asked. A ceiling nobody
    /// passed is not a ceiling of zero.
    #[test]
    fn a_point_read_ceiling_fails_the_run_and_only_when_asked() {
        let r = report(1000, 12.0, 90.0);
        // Nothing asked for → nothing enforced, however slow the run was.
        assert!(check_gate(&r, None, None, None).is_ok());
        // Under, and exactly on, the ceiling both pass; over fails.
        assert!(check_gate(&r, Some(12.0), Some(90.0), None).is_ok());
        assert!(check_gate(&r, Some(20.0), Some(100.0), None).is_ok());
        assert!(check_gate(&r, Some(11.9), None, None).is_err());
        assert!(check_gate(&r, None, Some(89.9), None).is_err());

        // Both breached → both named, so one CI run tells you everything rather than the first thing.
        let err = check_gate(&r, Some(1.0), Some(2.0), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("p50"), "{err}");
        assert!(err.contains("p99"), "{err}");
    }

    /// **The false pass this gate would otherwise have.** An unindexed nest samples no keys, so
    /// `query` reports p50 = p99 = 0µs, and zero sits under every ceiling anyone would ever write.
    /// Without `--min-reads` the gate is green precisely when it measured nothing, which is how the
    /// footprint check used to false-pass before it asserted its row count.
    #[test]
    fn a_nest_that_measured_nothing_cannot_pass_the_gate() {
        let measured_nothing = report(0, 0.0, 0.0);
        // The trap: ceilings alone are satisfied by an empty run.
        assert!(check_gate(&measured_nothing, Some(50.0), Some(500.0), None).is_ok());
        // The floor is what refuses it.
        let err = check_gate(&measured_nothing, Some(50.0), Some(500.0), Some(1000))
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed 0 point-read"), "{err}");
        // A partially-indexed nest is refused too - 12 reads is not a p99.
        assert!(check_gate(&report(12, 1.0, 1.0), None, None, Some(1000)).is_err());
        // And a full run clears it.
        assert!(check_gate(&report(1000, 1.0, 1.0), None, None, Some(1000)).is_ok());
    }

    /// An indexed nest on disk: config, ABI, and `rows` entities in the hot store.
    fn indexed_nest(dir: &std::path::Path, rows: u64) {
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            r#"[nest]
name = "t"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://x"]
schema_version = 1

[[contracts]]
alias = "c"
address = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
abi = "abis/c.json"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join("abis/c.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        let store = Store::open(&dir.join(crate::config::DB_FILE)).unwrap();
        let batch: Vec<(String, String)> = (0..rows)
            .map(|i| {
                (
                    Store::entity_key(i + 1, 0),
                    format!(
                        r#"{{"table":"c__transfer","block_number":{},"log_index":0,"value":"1"}}"#,
                        i + 1
                    ),
                )
            })
            .collect();
        store.commit_window(&batch, None, rows).unwrap();
    }

    fn gated_args(
        dir: &std::path::Path,
        max_p50: Option<f64>,
        min_reads: Option<usize>,
    ) -> crate::cli::QueryBenchArgs {
        crate::cli::QueryBenchArgs {
            dir: dir.to_string_lossy().into_owned(),
            sql: None,
            reads: 1000,
            iters: 1,
            out: None,
            label: None,
            max_point_read_p50_us: max_p50,
            max_point_read_p99_us: None,
            min_reads,
        }
    }

    /// **The wiring, not the mechanism.** `check_gate` can be correct and complete while `query`
    /// never calls it, which is the exact shape the board caught in PR #369: a parser with tests and
    /// a call site with none, deletable at 488 green. So this drives the real `bench query` against a
    /// real indexed nest on disk and asserts the process-level outcome - a breached ceiling must come
    /// back as `Err`, because `Ok` is what CI reads as a pass.
    #[test]
    fn bench_query_fails_when_a_ceiling_is_breached() {
        let dir = tempfile::tempdir().unwrap();
        indexed_nest(dir.path(), 64);

        // No ceiling: the same run must succeed, so the failure below is the gate and not the fixture.
        query(gated_args(dir.path(), None, None)).unwrap();

        // A ceiling no real point-read can meet. redb serves these from page cache in single-digit
        // microseconds; 0µs is unreachable by construction, so this asserts the gate, not the machine.
        let err = query(gated_args(dir.path(), Some(0.0), None))
            .expect_err("a breached ceiling must fail the run, not just print")
            .to_string();
        assert!(err.contains("point-read p50"), "{err}");

        // The re-baseline instruction has to name the machine that enforces the gate. The previous
        // wording said "against a committed report", and the only committed report is a 32-core
        // dev-box one that docs/benchmarks.md says the ceiling was *not* set against - so whoever
        // followed the message re-derived a runner ceiling from the wrong hardware. Pinned here
        // because a FAIL message nobody reads until CI is red is exactly the text that rots.
        assert!(
            err.contains("machine that enforces it"),
            "the failure must say where to re-baseline, not just that one is needed: {err}"
        );
        // ...and it has to name the file, not just the principle. The assertion above stays green on
        // a message that says "measure it yourself" and nothing more, which leaves the one sentence
        // that actually prevents the mistake - naming docs/bench/point-read.json as the wrong
        // artifact - deletable without this test noticing. That was the defect itself: the reader
        // knew a re-baseline was wanted and reached for the only committed report there is.
        assert!(
            err.contains("Not from docs/bench/point-read.json"),
            "the failure must name the report that is *not* the baseline, since that is the one a \
             reader reaches for otherwise: {err}"
        );

        // The p99 flag still reaches the gate. CI stopped passing --max-point-read-p99-us when the
        // 150µs default was dropped, so nothing else drives this argument end to end any more, and a
        // deliberately-retained escape hatch with no coverage is the shape that rots unnoticed.
        // `check_gate`'s own p99 branch is unit-tested above; what this pins is the CLI wiring into
        // it, which is the half that went quiet.
        let mut p99_args = gated_args(dir.path(), None, None);
        p99_args.max_point_read_p99_us = Some(0.0);
        let err = query(p99_args)
            .expect_err("--max-point-read-p99-us must still gate when an operator asks for it")
            .to_string();
        assert!(err.contains("point-read p99"), "{err}");

        // And the floor is wired too: 64 rows cannot satisfy --min-reads 1000.
        let err = query(gated_args(dir.path(), None, Some(1000)))
            .expect_err("--min-reads must fail a nest that measured too little")
            .to_string();
        assert!(err.contains("timed 64 point-read"), "{err}");
    }

    /// The report is the artifact, so it carries its own provenance. `hardware` was on the write-path
    /// report and missing here, and a point-read latency without the machine it was measured on is
    /// not comparable to the next release's number - which is the whole point of #283.
    #[test]
    fn the_query_report_carries_its_provenance() {
        let dir = tempfile::tempdir().unwrap();
        indexed_nest(dir.path(), 8);
        let out = dir.path().join("report.json");
        let mut args = gated_args(dir.path(), None, None);
        args.out = Some(out.to_string_lossy().into_owned());
        query(args).unwrap();

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert!(
            json["hardware"]
                .as_str()
                .is_some_and(|h| h.contains("cores")),
            "report must name the hardware: {json}"
        );
        assert_eq!(json["reads"], 8);
        assert!(json["point_read_p50_us"].as_f64().unwrap() > 0.0);
    }

    /// **`--keep` wipes what it is given, so the guard is the only thing between a typo and a
    /// deleted nest.** The refusal has to fire on the *presence of `nuthatch.toml`*, not on
    /// anything about the run, because by the time a run has started the directory is already
    /// cleared. Absent this test the guard was reachable only through a live-RPC backfill, so
    /// nothing would have noticed it being weakened or dropped.
    #[test]
    fn keep_refuses_a_nest_directory_and_accepts_anything_else() {
        let dir = tempfile::tempdir().unwrap();

        // Not asked for: no path, and no error.
        assert!(resolve_keep(&None).unwrap().is_none());

        // A bare directory is fine - this is the intended use, a dedicated scratch path.
        let scratch = dir.path().join("scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        assert_eq!(
            resolve_keep(&Some(scratch.to_string_lossy().into_owned())).unwrap(),
            Some(scratch.clone())
        );

        // So is a path that does not exist yet: the run creates it.
        let fresh = dir.path().join("not-yet");
        assert_eq!(
            resolve_keep(&Some(fresh.to_string_lossy().into_owned())).unwrap(),
            Some(fresh)
        );

        // A directory holding a nuthatch.toml is refused, and the message says where to point
        // instead - the one thing someone who has just hit this needs to know.
        std::fs::write(scratch.join(crate::config::CONFIG_FILE), "[nest]\n").unwrap();
        let err = resolve_keep(&Some(scratch.to_string_lossy().into_owned()))
            .expect_err("--keep at a nest directory must be refused, not cleared")
            .to_string();
        assert!(err.contains("is a nest directory"), "{err}");
        assert!(err.contains(crate::config::CONFIG_FILE), "{err}");
    }

    #[test]
    fn a_non_factory_nest_dispatches_on_seal_direct_and_concurrency() {
        assert_eq!(choose_path(false, true, 8), BackfillPath::Pipelined);
        assert_eq!(choose_path(false, true, 1), BackfillPath::Direct);
        assert_eq!(choose_path(false, false, 8), BackfillPath::HotStore);
        assert_eq!(choose_path(false, false, 1), BackfillPath::HotStore);
    }
}
