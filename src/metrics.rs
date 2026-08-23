//! Prometheus `/metrics` - the operator's alerting and billing surface (RFC-0005 §6).
//!
//! Hand-rolled: a handful of process-global atomics plus text formatting, not a metrics framework -
//! the single-binary / minimal-deps rule stands, and the metric set is small and fixed. Gauges
//! (tip, watermark, RSS) are set to the latest value; counters (rows, queries) only ever increase.
//! Nothing here phones home: metrics are exposed on the same local API and scraped by the operator.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// The one process-wide metrics registry. `const`-constructed, so it needs no lazy init.
pub static METRICS: Metrics = Metrics::new();

/// Per-nest counterparts of the nest-scoped signals (SEC-9). In a runtime the process-global gauges and
/// counters blend every mounted nest into one number; these let an operator see each nest's own
/// progress under a `{nest="…"}` label. Updating a per-nest value also bumps the matching global
/// aggregate, so the existing unlabelled series stay correct and backward-compatible.
#[derive(Default)]
pub struct NestMetrics {
    last_block: AtomicU64,
    sealed_through: AtomicU64,
    /// The tip of **this nest's chain**, and when its cursor last polled successfully.
    ///
    /// Duplicated per nest rather than kept only process-globally because a multichain runtime runs one
    /// cursor per chain: with a single global tip, whichever cursor polled last wins, and
    /// `/<nest>/ready` reports another chain's block height. Observed live - a mainnet nest reporting
    /// an Arbitrum tip of 488,677,305 while mainnet was at 25,632,906.
    ///
    /// Every nest on the same cursor holds the same value, which is correct: they share a chain.
    tip: AtomicU64,
    last_poll_ok: AtomicU64,
    /// Set after a failed source poll and cleared by the next success. This distinguishes "the
    /// process has only just begun" from "the process has already proved its only RPC pool dead".
    /// `/ready` must not give the latter a 90-second green grace period.
    poll_failed: AtomicBool,
    /// Unix seconds this nest's ingest was built, i.e. when it first started trying to reach the chain.
    /// `0` until [`NestMetrics::mark_started`] runs. Bounds `/ready`'s "never polled yet" grace period
    /// (#510): without it, a pool that is dead from the very first poll never sets `last_poll_ok` and
    /// looks permanently "just starting up" instead of eventually stalled.
    started_at: AtomicU64,
    /// Unix seconds `last_block` last actually *changed* value. `0` until it has ever moved. Distinct
    /// from `last_poll_ok` (#578): a source poll can succeed on schedule - keeping `last_poll_ok` fresh
    /// and `/ready` looking healthy - while the cursor itself is wedged refusing to seal the window in
    /// front of it (e.g. `process_window` holding position on an unfetchable block timestamp,
    /// `indexer.rs:3437`). `last_poll_ok` alone cannot see that; this can.
    last_progress: AtomicU64,
    rows_decoded: AtomicU64,
    rows_sealed: AtomicU64,
    reorgs: AtomicU64,
}

impl NestMetrics {
    /// Also stamps `last_progress` when `v` actually advances the watermark, so `/ready` can tell "the
    /// source answered" apart from "the cursor is getting somewhere" (#578).
    pub fn set_last_block(&self, v: u64) {
        if self.last_block.swap(v, Relaxed) != v {
            self.last_progress.store(now_unix(), Relaxed);
        }
        METRICS.set_last_block(v);
    }
    pub fn last_progress(&self) -> u64 {
        self.last_progress.load(Relaxed)
    }
    /// Test seam, mirroring [`NestMetrics::set_last_poll_ok_for_test`]: pin the progress clock directly,
    /// so a test can simulate "advanced once, then froze" without waiting out real time between calls.
    #[cfg(test)]
    pub fn set_last_progress_for_test(&self, t: u64) {
        self.last_progress.store(t, Relaxed);
    }
    /// This nest's chain tip, and the global aggregate for a solo `dev`.
    pub fn set_tip(&self, v: u64) {
        self.tip.store(v, Relaxed);
        METRICS.set_tip(v);
    }
    pub fn tip(&self) -> u64 {
        self.tip.load(Relaxed)
    }
    pub fn mark_poll_ok(&self) {
        self.last_poll_ok.store(now_unix(), Relaxed);
        self.poll_failed.store(false, Relaxed);
        METRICS.mark_poll_ok();
    }
    pub fn last_poll_ok(&self) -> u64 {
        self.last_poll_ok.load(Relaxed)
    }
    /// Record a failed source poll. A later successful poll clears this, so this is only a
    /// readiness failure while no successful poll has happened yet.
    pub fn mark_poll_failed(&self) {
        self.poll_failed.store(true, Relaxed);
        METRICS.mark_poll_failed();
    }
    pub fn poll_failed(&self) -> bool {
        self.poll_failed.load(Relaxed)
    }
    /// Test seam: force this nest's last successful poll to an absolute unix time, so a test can make
    /// one cursor *look* dark without waiting out `serve`'s 90-second stall threshold. Deliberately
    /// does NOT touch the process-global aggregate, unlike [`NestMetrics::mark_poll_ok`] - the point of
    /// a cross-cursor stall test is that a healthy chain keeps the global fresh while a dead one does
    /// not, and reading the global is the bug being guarded against (RFC-0021, #356).
    #[cfg(test)]
    pub fn set_last_poll_ok_for_test(&self, t: u64) {
        self.last_poll_ok.store(t, Relaxed);
    }
    /// Stamp this nest's readiness clock - called once, when its ingest is built. Also stamps the
    /// process-global clock the first time any nest calls it, so a solo `dev` (which reads only the
    /// global aggregate) gets the same bounded grace period.
    pub fn mark_started(&self) {
        let now = now_unix();
        self.started_at.store(now, Relaxed);
        METRICS.mark_started_once(now);
    }
    pub fn started_at(&self) -> u64 {
        self.started_at.load(Relaxed)
    }
    /// Test seam, mirroring [`NestMetrics::set_last_poll_ok_for_test`].
    #[cfg(test)]
    pub fn set_started_at_for_test(&self, t: u64) {
        self.started_at.store(t, Relaxed);
    }
    /// Nest-only: does not bump the process-global flag, so a runtime `/ready` fixture cannot
    /// leak into a later solo-dev test that reads [`Metrics::poll_failed`].
    #[cfg(test)]
    pub fn set_poll_failed_for_test(&self, v: bool) {
        self.poll_failed.store(v, Relaxed);
    }
    pub fn last_block(&self) -> u64 {
        self.last_block.load(Relaxed)
    }
    pub fn sealed_through(&self) -> u64 {
        self.sealed_through.load(Relaxed)
    }
    pub fn set_sealed_through(&self, v: u64) {
        self.sealed_through.store(v, Relaxed);
        METRICS.set_sealed_through(v);
    }
    pub fn add_rows_decoded(&self, n: u64) {
        self.rows_decoded.fetch_add(n, Relaxed);
        METRICS.add_rows_decoded(n);
    }
    pub fn add_rows_sealed(&self, n: u64) {
        self.rows_sealed.fetch_add(n, Relaxed);
        METRICS.add_rows_sealed(n);
    }
    pub fn inc_reorgs(&self) {
        self.reorgs.fetch_add(1, Relaxed);
        METRICS.inc_reorgs();
    }
}

pub struct Metrics {
    // Ingestion - the "is it keeping up?" signals an operator alerts on.
    tip_height: AtomicU64,
    last_block: AtomicU64,
    sealed_through: AtomicU64,
    /// Unix seconds of the last *successful* source poll (tip fetch). `0` = never polled yet. The
    /// readiness signal: if now − this exceeds the stall threshold, every RPC endpoint is unreachable
    /// and indexing has stalled (as opposed to "caught up and idle", which keeps this fresh).
    last_poll_ok: AtomicU64,
    /// See [`NestMetrics::poll_failed`]. The solo `dev` readiness path reads this global value.
    poll_failed: AtomicBool,
    /// Unix seconds the first nest was built, i.e. process/runtime start. `0` until set. See
    /// [`NestMetrics::started_at`] - a solo `dev` reads this one, since it has no per-nest health.
    started_at: AtomicU64,
    /// Global counterpart of [`NestMetrics::last_progress`], read by a solo `dev` (no per-nest health).
    last_progress: AtomicU64,
    rows_decoded: AtomicU64,
    rows_sealed: AtomicU64,
    reorgs: AtomicU64,
    alert_outbox_depth: AtomicU64,
    // Serving - the surface an operator bills against.
    http_requests: AtomicU64,
    sql_queries: AtomicU64,
    sql_rejections: AtomicU64,
    rpc_requests: AtomicU64,
    rpc_methods: Mutex<BTreeMap<String, u64>>,
    /// Per-nest handles, keyed by nest name (SEC-9). `BTreeMap` so `/metrics` renders in a stable
    /// order. Populated once per nest at build; a solo `dev` has a single entry.
    per_nest: Mutex<BTreeMap<String, Arc<NestMetrics>>>,
}

impl Metrics {
    const fn new() -> Self {
        Self {
            tip_height: AtomicU64::new(0),
            last_block: AtomicU64::new(0),
            sealed_through: AtomicU64::new(0),
            last_poll_ok: AtomicU64::new(0),
            poll_failed: AtomicBool::new(false),
            started_at: AtomicU64::new(0),
            last_progress: AtomicU64::new(0),
            rows_decoded: AtomicU64::new(0),
            rows_sealed: AtomicU64::new(0),
            reorgs: AtomicU64::new(0),
            alert_outbox_depth: AtomicU64::new(0),
            http_requests: AtomicU64::new(0),
            sql_queries: AtomicU64::new(0),
            sql_rejections: AtomicU64::new(0),
            rpc_requests: AtomicU64::new(0),
            rpc_methods: Mutex::new(BTreeMap::new()),
            per_nest: Mutex::new(BTreeMap::new()),
        }
    }

    /// Get (or create) the per-nest metrics handle for `name`. Called once per nest at build; the
    /// returned handle is cheap to clone and its updates also feed the process-global aggregates.
    pub fn nest(&self, name: &str) -> Arc<NestMetrics> {
        let mut map = self.per_nest.lock().unwrap();
        if let Some(h) = map.get(name) {
            return h.clone();
        }
        let h = Arc::new(NestMetrics::default());
        map.insert(name.to_string(), h.clone());
        h
    }

    pub fn set_tip(&self, v: u64) {
        self.tip_height.store(v, Relaxed);
    }
    pub fn set_last_block(&self, v: u64) {
        if self.last_block.swap(v, Relaxed) != v {
            self.last_progress.store(now_unix(), Relaxed);
        }
    }
    pub fn last_progress(&self) -> u64 {
        self.last_progress.load(Relaxed)
    }
    /// Test seam, mirroring the per-nest one.
    #[cfg(test)]
    pub fn set_last_progress_for_test(&self, t: u64) {
        self.last_progress.store(t, Relaxed);
    }
    pub fn set_sealed_through(&self, v: u64) {
        self.sealed_through.store(v, Relaxed);
    }
    /// Record a successful source poll - call it on every tip fetch that returns (the tip loop does),
    /// so readiness reflects "we can still reach the chain", independent of whether we're behind.
    pub fn mark_poll_ok(&self) {
        self.last_poll_ok.store(now_unix(), Relaxed);
        self.poll_failed.store(false, Relaxed);
    }
    /// Unix seconds of the last successful poll (`0` = never). Read by the readiness endpoint.
    pub fn last_poll_ok(&self) -> u64 {
        self.last_poll_ok.load(Relaxed)
    }
    pub fn mark_poll_failed(&self) {
        self.poll_failed.store(true, Relaxed);
    }
    pub fn poll_failed(&self) -> bool {
        self.poll_failed.load(Relaxed)
    }
    /// First-wins stamp of process/runtime start, called from [`NestMetrics::mark_started`]. Only the
    /// first caller sets it - later nests built in the same startup burst must not push it forward.
    fn mark_started_once(&self, t: u64) {
        let _ = self.started_at.compare_exchange(0, t, Relaxed, Relaxed);
    }
    /// Unix seconds of process/runtime start (`0` = not yet stamped). Read by the readiness endpoint.
    pub fn started_at(&self) -> u64 {
        self.started_at.load(Relaxed)
    }
    /// Test seam, mirroring the per-nest one.
    #[cfg(test)]
    pub fn set_started_at_for_test(&self, t: u64) {
        self.started_at.store(t, Relaxed);
    }
    // Getters for the readiness endpoint (the setters already exist for the ingest loop).
    pub fn tip_height(&self) -> u64 {
        self.tip_height.load(Relaxed)
    }
    pub fn last_block(&self) -> u64 {
        self.last_block.load(Relaxed)
    }
    pub fn sealed_through_val(&self) -> u64 {
        self.sealed_through.load(Relaxed)
    }
    pub fn add_rows_decoded(&self, n: u64) {
        self.rows_decoded.fetch_add(n, Relaxed);
    }
    pub fn add_rows_sealed(&self, n: u64) {
        self.rows_sealed.fetch_add(n, Relaxed);
    }
    pub fn inc_reorgs(&self) {
        self.reorgs.fetch_add(1, Relaxed);
    }
    pub fn set_alert_outbox(&self, v: u64) {
        self.alert_outbox_depth.store(v, Relaxed);
    }
    pub fn inc_http(&self) {
        self.http_requests.fetch_add(1, Relaxed);
    }
    pub fn inc_sql(&self) {
        self.sql_queries.fetch_add(1, Relaxed);
    }
    pub fn inc_sql_rejected(&self) {
        self.sql_rejections.fetch_add(1, Relaxed);
    }
    pub fn inc_rpc(&self) {
        self.rpc_requests.fetch_add(1, Relaxed);
    }
    pub fn inc_rpc_method(&self, method: &str) {
        self.inc_rpc_methods(method, 1);
    }
    pub fn inc_rpc_methods(&self, method: &str, count: u64) {
        if count == 0 || method.is_empty() {
            return;
        }
        let mut map = self.rpc_methods.lock().unwrap();
        *map.entry(method.to_string()).or_insert(0) += count;
    }
    /// Test seam for the solo `/ready` path, which reads these process-global fields rather than
    /// a per-nest handle.
    #[cfg(test)]
    pub fn reset_readiness_for_test(&self) {
        self.last_poll_ok.store(0, Relaxed);
        self.poll_failed.store(false, Relaxed);
        self.started_at.store(0, Relaxed);
    }

    /// Render the registry as Prometheus text (`text/plain; version=0.0.4`).
    pub fn render(&self) -> String {
        let tip = self.tip_height.load(Relaxed);
        let last = self.last_block.load(Relaxed);
        // Blocks behind the tip. 0 (not negative) once caught up.
        let lag = tip.saturating_sub(last);
        let rss = rss_bytes();

        let gauge = |name: &str, help: &str, v: u64| {
            format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {v}\n")
        };
        let counter = |name: &str, help: &str, v: u64| {
            format!("# HELP {name} {help}\n# TYPE {name} counter\n{name} {v}\n")
        };

        let mut s = String::with_capacity(2048);
        s.push_str(&gauge(
            "nuthatch_tip_height",
            "Latest block height seen from the source.",
            tip,
        ));
        s.push_str(&gauge(
            "nuthatch_last_block",
            "Highest block the indexer has committed.",
            last,
        ));
        s.push_str(&gauge(
            "nuthatch_tip_lag_blocks",
            "Blocks the indexer is behind the source tip.",
            lag,
        ));
        s.push_str(&gauge(
            "nuthatch_sealed_through",
            "Highest block sealed to the immutable cold layer.",
            self.sealed_through.load(Relaxed),
        ));
        s.push_str(&gauge(
            "nuthatch_rss_bytes",
            "Resident set size of this process, in bytes.",
            rss,
        ));
        s.push_str(&gauge(
            "nuthatch_last_poll_unixtime",
            "Unix time of the last successful source poll (0 = never). Staleness ⇒ RPC stalled.",
            self.last_poll_ok.load(Relaxed),
        ));
        s.push_str(&gauge(
            "nuthatch_alert_outbox_depth",
            "Pending alert-webhook deliveries in the durable outbox.",
            self.alert_outbox_depth.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_rows_decoded_total",
            "Rows decoded since start.",
            self.rows_decoded.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_rows_sealed_total",
            "Rows sealed to Parquet since start.",
            self.rows_sealed.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_reorgs_total",
            "Reorgs detected and rolled back since start.",
            self.reorgs.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_http_requests_total",
            "HTTP API requests served since start.",
            self.http_requests.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_sql_queries_total",
            "Analytical /sql queries accepted since start.",
            self.sql_queries.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_sql_rejections_total",
            "Analytical /sql queries rejected (guard: timeout, too-large, over-capacity).",
            self.sql_rejections.load(Relaxed),
        ));
        s.push_str(&counter(
            "nuthatch_rpc_requests_total",
            "Outbound HTTP POSTs to JSON-RPC endpoints (one per request or batch envelope, including failover retries). Not a method count; see nuthatch_rpc_methods_total.",
            self.rpc_requests.load(Relaxed),
        ));

        // Always advertised, even at zero observations: an operator pricing a just-started nest
        // against a provider CU schedule needs the series to exist before the first batch lands.
        s.push_str("# HELP nuthatch_rpc_methods_total Individual JSON-RPC method invocations. A batch of 200 eth_getBlockByNumber counts 200 here and 1 on nuthatch_rpc_requests_total. Multiply by a provider's per-method CU schedule to estimate a bill.\n# TYPE nuthatch_rpc_methods_total counter\n");
        let methods = self.rpc_methods.lock().unwrap();
        for (method, count) in methods.iter() {
            s.push_str(&format!(
                "nuthatch_rpc_methods_total{{method=\"{method}\"}} {count}\n"
            ));
        }

        // Per-nest series (SEC-9): in a runtime, one `{nest="…"}`-labelled line per mounted nest, so the
        // blended aggregates above can be broken down by nest. Distinct `_nest_` metric names keep the
        // exposition unambiguous (a name is never both labelled and unlabelled).
        let per = self.per_nest.lock().unwrap();
        if !per.is_empty() {
            let mut labelled =
                |name: &str, help: &str, typ: &str, get: &dyn Fn(&NestMetrics) -> u64| {
                    s.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {typ}\n"));
                    for (nest, m) in per.iter() {
                        s.push_str(&format!("{name}{{nest=\"{nest}\"}} {}\n", get(m)));
                    }
                };
            labelled(
                "nuthatch_nest_last_block",
                "Highest block committed, per nest.",
                "gauge",
                &|m| m.last_block.load(Relaxed),
            );
            labelled(
                "nuthatch_nest_sealed_through",
                "Highest block sealed to the cold layer, per nest.",
                "gauge",
                &|m| m.sealed_through.load(Relaxed),
            );
            labelled(
                "nuthatch_nest_rows_decoded_total",
                "Rows decoded since start, per nest.",
                "counter",
                &|m| m.rows_decoded.load(Relaxed),
            );
            labelled(
                "nuthatch_nest_rows_sealed_total",
                "Rows sealed to Parquet since start, per nest.",
                "counter",
                &|m| m.rows_sealed.load(Relaxed),
            );
            labelled(
                "nuthatch_nest_reorgs_total",
                "Reorgs detected and rolled back since start, per nest.",
                "counter",
                &|m| m.reorgs.load(Relaxed),
            );
        }
        s
    }
}

/// Wall-clock unix seconds - used only for the poll-freshness/readiness signal, never in the
/// deterministic data path.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// This process's resident set size in bytes. Linux via `/proc/self/status`; else `ps` (macOS/BSD);
/// 0 if neither answers (the gauge just reads 0 rather than failing the scrape).
pub fn rss_bytes() -> u64 {
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Ok(kb) = rest.trim().trim_end_matches("kB").trim().parse::<u64>() {
                    return kb * 1024;
                }
            }
        }
    }
    let pid = std::process::id().to_string();
    if let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
    {
        if let Ok(kb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
            return kb * 1024;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_prometheus_text_with_lag() {
        let m = Metrics::new();
        m.set_tip(1_000);
        m.set_last_block(940);
        m.add_rows_decoded(5);
        m.inc_sql();
        m.inc_sql_rejected();
        let out = m.render();
        assert!(out.contains("# TYPE nuthatch_tip_height gauge"));
        assert!(out.contains("nuthatch_tip_height 1000"));
        assert!(out.contains("nuthatch_tip_lag_blocks 60")); // 1000 - 940
        assert!(out.contains("# TYPE nuthatch_rows_decoded_total counter"));
        assert!(out.contains("nuthatch_rows_decoded_total 5"));
        assert!(out.contains("nuthatch_sql_queries_total 1"));
        assert!(out.contains("nuthatch_sql_rejections_total 1"));
    }

    #[test]
    fn per_nest_series_are_labelled() {
        let m = Metrics::new();
        let horizon = m.nest("horizon");
        let graph = m.nest("graph-network");
        horizon.set_last_block(500);
        horizon.add_rows_decoded(10);
        graph.set_last_block(400);
        graph.inc_reorgs();
        let out = m.render();
        // One labelled series per nest, distinct from the blended aggregates.
        assert!(out.contains("# TYPE nuthatch_nest_last_block gauge"));
        assert!(out.contains("nuthatch_nest_last_block{nest=\"horizon\"} 500"));
        assert!(out.contains("nuthatch_nest_last_block{nest=\"graph-network\"} 400"));
        assert!(out.contains("nuthatch_nest_rows_decoded_total{nest=\"horizon\"} 10"));
        assert!(out.contains("nuthatch_nest_reorgs_total{nest=\"graph-network\"} 1"));
        // A per-nest update also feeds the process-global aggregate (backward-compatible).
        assert_eq!(horizon.last_block.load(Relaxed), 500);
    }

    #[test]
    fn no_nests_means_no_per_nest_block() {
        // A fresh registry with no nests registered renders only the aggregates - no labelled series.
        assert!(!Metrics::new().render().contains("nuthatch_nest_last_block"));
    }

    #[test]
    fn poll_ok_is_recorded_and_rendered() {
        let m = Metrics::new();
        assert_eq!(m.last_poll_ok(), 0, "never polled yet");
        m.mark_poll_ok();
        assert!(m.last_poll_ok() >= now_unix() - 2, "records ~now");
        assert!(m
            .render()
            .contains("# TYPE nuthatch_last_poll_unixtime gauge"));
    }

    #[test]
    fn lag_never_goes_negative() {
        let m = Metrics::new();
        m.set_tip(100);
        m.set_last_block(120); // last ahead of a stale tip read
        assert!(m.render().contains("nuthatch_tip_lag_blocks 0"));
    }

    /// #578: `last_progress` stamps only when `last_block` actually moves, not on every call - a cursor
    /// re-reporting the same watermark (holding position while it waits out an unfetchable block
    /// timestamp) must not look like it's making progress.
    #[test]
    fn last_progress_stamps_only_on_an_actual_advance() {
        let m = Metrics::new();
        assert_eq!(m.last_progress(), 0, "never advanced yet");
        m.set_last_block(100);
        let first = m.last_progress();
        assert!(first > 0, "first advance stamps progress");
        m.set_last_block(100); // same value again - held, not advanced
        assert_eq!(
            m.last_progress(),
            first,
            "re-reporting the same block must not restamp progress"
        );
        m.set_last_block(101); // a genuine advance restamps
        assert!(m.last_progress() >= first);
    }

    #[test]
    fn rpc_methods_series_are_rendered() {
        let m = Metrics::new();
        assert!(
            m.render()
                .contains("# TYPE nuthatch_rpc_methods_total counter"),
            "the series must be advertised before any RPC has been issued"
        );
        m.inc_rpc_method("eth_getBlockByNumber");
        m.inc_rpc_methods("eth_getLogs", 3);
        m.inc_rpc_methods("", 9);
        let out = m.render();
        assert!(out.contains("nuthatch_rpc_methods_total{method=\"eth_getBlockByNumber\"} 1"));
        assert!(out.contains("nuthatch_rpc_methods_total{method=\"eth_getLogs\"} 3"));
        assert!(
            !out.contains("method=\"\""),
            "empty method names are not a billable CU line"
        );
        assert!(
            out.contains("Outbound HTTP POSTs"),
            "help text must say this is envelopes, not methods: {out}"
        );
    }
}
