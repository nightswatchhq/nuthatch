//! The freshness dial (RFC-0040 §3, knobs 1 and 2): how often a caught-up cursor asks for the tip,
//! and whether it follows the tip at all or stops at the chain's finality boundary.
//!
//! Both are operator settings, not nest identity. A nest polled every five minutes holds exactly the
//! rows a nest polled every two seconds holds; what differs is when they arrive and what the RPC
//! provider bills for the asking. Measured on the Lodestar box (2026-09-06, #1173): one Arbitrum
//! cursor at tip spent ~9,900 Alchemy compute units a minute, of which the rows themselves were a
//! rounding error - 95 of the day's 345,600 blocks carried an event. The rest was the two-second
//! cadence: a tip call and a reorg check per poll, a checkpoint hash and a `finalized` probe per
//! committed window. The dial turns the cadence down; nothing about the data changes.
//!
//! Undialled, a cursor polls once per block time (#1497), floored at two seconds: the registry's
//! figure for a chain it ships, and one measured at startup for a chain it does not.
//!
//! RFC-0040 §4's conditions hold by construction: `/ready` reports the mode and interval and its
//! stall thresholds scale with the interval (no silent staleness); a slower cursor returns the same
//! rows later, never a substitute (no fabricated values); and sealing is untouched, so a segment's
//! content address stays a function of its block range and rows.

use std::time::Duration;

/// The shortest interval a defaulted poll takes, and the default when a chain's block time is unknown.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Blocks averaged over when measuring an unregistered chain's block time at startup.
const BLOCK_TIME_SAMPLE: u64 = 100;

/// How stale a cursor is allowed to be, and how it gets there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// How long a caught-up cursor waits before asking for the tip again (knob 1).
    pub poll_interval: Duration,
    /// Cap the cursor at the chain's finality boundary instead of the tip (knob 2). Nothing indexed
    /// under it can be reorged, so the reorg check is not paid once the store holds no unfinalised
    /// rows, and the hot store only ever carries rows waiting to seal.
    pub finality_only: bool,
    /// `poll_interval` came from `--poll-interval`, so the chain's block time never replaces it.
    pub poll_interval_explicit: bool,
}

impl Default for Freshness {
    fn default() -> Self {
        Freshness {
            poll_interval: DEFAULT_POLL_INTERVAL,
            finality_only: false,
            poll_interval_explicit: false,
        }
    }
}

/// The interval a chain gets when the operator names none (#1497): its block time in whole seconds,
/// never under [`DEFAULT_POLL_INTERVAL`]. Polling faster than blocks arrive finds nothing new and is
/// still billed; a block cannot be seen before it exists.
pub fn default_poll_interval(block_time: Option<Duration>) -> Duration {
    block_time.map_or(DEFAULT_POLL_INTERVAL, |t| {
        Duration::from_secs(t.as_secs()).max(DEFAULT_POLL_INTERVAL)
    })
}

impl Freshness {
    /// The dial as `dev`'s flags set it. `None` leaves the interval to the chain's block time.
    pub fn from_flags(poll_interval: Option<Duration>, finality_only: bool) -> Self {
        Freshness {
            poll_interval: poll_interval.unwrap_or(DEFAULT_POLL_INTERVAL),
            finality_only,
            poll_interval_explicit: poll_interval.is_some(),
        }
    }

    /// Settle a defaulted interval on `block_time`. An explicit `--poll-interval` always wins.
    pub fn for_block_time(self, block_time: Option<Duration>) -> Self {
        if self.poll_interval_explicit {
            return self;
        }
        Freshness {
            poll_interval: default_poll_interval(block_time),
            ..self
        }
    }

    /// [`Self::for_block_time`] for `chain`: the registry's block time, or for a chain it does not
    /// ship, one measured from `source` over the last [`BLOCK_TIME_SAMPLE`] blocks.
    pub async fn for_chain(self, chain: &str, source: &dyn crate::source::Source) -> Self {
        if self.poll_interval_explicit {
            return self;
        }
        let block_time = match crate::chains::lookup(chain) {
            Some(c) => Some(c.block_time()),
            None => measure_block_time(source).await,
        };
        let settled = self.for_block_time(block_time);
        match block_time {
            Some(t) => tracing::info!(
                "{chain} blocks every {}ms; polling every {}s (--poll-interval overrides)",
                t.as_millis(),
                settled.poll_interval.as_secs()
            ),
            None => tracing::warn!(
                "could not measure {chain}'s block time; polling every {}s (--poll-interval overrides)",
                settled.poll_interval.as_secs()
            ),
        }
        settled
    }

    /// What `/ready` says set the interval.
    pub fn poll_interval_source(&self) -> &'static str {
        if self.poll_interval_explicit {
            "flag"
        } else {
            "block_time"
        }
    }

    /// The highest block this cursor may index right now. `finalized_through` is the chain policy's
    /// finality boundary for this `tip` (see `seal_ceiling`); it only matters under `finality_only`.
    pub fn ceiling(&self, tip: u64, finalized_through: u64) -> u64 {
        if self.finality_only {
            finalized_through.min(tip)
        } else {
            tip
        }
    }

    /// What `/ready` calls the mode.
    pub fn mode(&self) -> &'static str {
        if self.finality_only {
            "finality"
        } else {
            "tip"
        }
    }

    /// A readiness threshold that knows the cursor is *meant* to be quiet. `base` is the tip path's
    /// threshold; a cursor polling every five minutes has not stalled at ninety seconds of silence,
    /// so the threshold is at least three intervals - long enough that one missed poll is not a
    /// stall and short enough that a dead pool is still reported inside a quarter of an hour at the
    /// intervals this dial is for.
    pub fn stall_threshold_secs(&self, base: u64) -> u64 {
        base.max(self.poll_interval.as_secs().saturating_mul(3))
    }
}

/// The mean block time over the last [`BLOCK_TIME_SAMPLE`] blocks: one tip call and one batch of two
/// headers, once at startup. `None` if the source cannot say.
async fn measure_block_time(source: &dyn crate::source::Source) -> Option<Duration> {
    let tip = source.tip().await.ok()?;
    let from = tip.checked_sub(BLOCK_TIME_SAMPLE)?;
    let ts = source.block_timestamps(&[from, tip]).await.ok()?;
    let span = ts.get(&tip)?.checked_sub(*ts.get(&from)?)?;
    Some(Duration::from_millis(span * 1000 / BLOCK_TIME_SAMPLE))
}

/// Parse an operator's duration: `2s`, `5m`, `1h`, or bare seconds. Zero is refused - a cursor that
/// never waits is a busy loop.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let interval = parse_span(s)?;
    if interval.is_zero() {
        return Err("the poll interval must be at least one second".into());
    }
    Ok(interval)
}

/// [`parse_duration`]'s units without its zero refusal, for a caller whose zero means something else
/// and deserves its own message.
pub fn parse_span(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (digits, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("`{s}` is not a duration; try `2s`, `5m` or `1h`"))?;
    let secs = match unit.trim() {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n.checked_mul(60).ok_or("that interval does not fit")?,
        "h" | "hr" | "hrs" => n.checked_mul(3600).ok_or("that interval does not fit")?,
        other => return Err(format!("unknown unit `{other}` in `{s}`; use s, m or h")),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A chain at `tip` producing one block every `block_ms`, counting the calls it answers.
    struct Clocked {
        tip: u64,
        block_ms: u64,
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::source::Source for Clocked {
        async fn tip(&self) -> anyhow::Result<u64> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.tip)
        }
        async fn block_hash(&self, _: u64) -> anyhow::Result<Option<String>> {
            unreachable!()
        }
        async fn block_timestamps(&self, blocks: &[u64]) -> anyhow::Result<HashMap<u64, u64>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(blocks
                .iter()
                .map(|&b| (b, 1_700_000_000 + b * self.block_ms / 1000))
                .collect())
        }
        async fn logs(
            &self,
            _: &crate::source::LogFilter,
            _: u64,
            _: u64,
        ) -> anyhow::Result<Vec<crate::rpc::Log>> {
            unreachable!()
        }
    }

    fn clocked(block_ms: u64) -> Clocked {
        Clocked {
            tip: 50_000,
            block_ms,
            calls: AtomicUsize::new(0),
        }
    }

    #[test]
    fn the_default_is_the_block_time_in_whole_seconds_floored_at_two() {
        let ms = Duration::from_millis;
        assert_eq!(default_poll_interval(None), Duration::from_secs(2));
        assert_eq!(
            default_poll_interval(Some(ms(12_050))),
            Duration::from_secs(12)
        );
        assert_eq!(
            default_poll_interval(Some(ms(5_088))),
            Duration::from_secs(5)
        );
        assert_eq!(
            default_poll_interval(Some(ms(2_000))),
            Duration::from_secs(2)
        );
        assert_eq!(default_poll_interval(Some(ms(500))), Duration::from_secs(2));
        assert_eq!(default_poll_interval(Some(ms(0))), Duration::from_secs(2));
    }

    #[test]
    fn an_explicit_interval_wins_over_any_block_time() {
        let flagged = Freshness::from_flags(Some(Duration::from_secs(1)), false);
        let settled = flagged.for_block_time(Some(Duration::from_secs(12)));
        assert_eq!(settled.poll_interval, Duration::from_secs(1));
        assert_eq!(settled.poll_interval_source(), "flag");

        let defaulted =
            Freshness::from_flags(None, true).for_block_time(Some(Duration::from_secs(12)));
        assert_eq!(defaulted.poll_interval, Duration::from_secs(12));
        assert!(
            defaulted.finality_only,
            "settling the interval keeps the other knob"
        );
        assert_eq!(defaulted.poll_interval_source(), "block_time");
    }

    #[test]
    fn dev_leaves_the_interval_unset_unless_the_flag_is_given() {
        let parse = |argv: &[&str]| match crate::cli::Cli::try_parse_from(argv).unwrap().command {
            crate::cli::Command::Dev(a) => a.poll_interval,
            _ => unreachable!(),
        };
        use clap::Parser;
        assert_eq!(parse(&["nuthatch", "dev"]), None);
        assert_eq!(
            parse(&["nuthatch", "dev", "--poll-interval", "2s"]),
            Some(Duration::from_secs(2))
        );
    }

    #[tokio::test]
    async fn a_shipped_chain_uses_the_registry_and_asks_the_source_nothing() {
        let source = clocked(500);
        let f = Freshness::default().for_chain("mainnet", &source).await;
        assert_eq!(f.poll_interval, Duration::from_secs(12));
        assert_eq!(source.calls.load(Ordering::Relaxed), 0);
        let f = Freshness::default()
            .for_chain("arbitrum-one", &source)
            .await;
        assert_eq!(f.poll_interval, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn an_unshipped_chain_is_measured_once_and_a_fast_one_is_floored() {
        // Sepolia's cadence and Arc testnet's, the two chains #1497 was measured on.
        let sepolia = clocked(12_000);
        let f = Freshness::default().for_chain("sepolia", &sepolia).await;
        assert_eq!(f.poll_interval, Duration::from_secs(12));
        assert_eq!(
            sepolia.calls.load(Ordering::Relaxed),
            2,
            "one tip, one header batch"
        );

        let arc = clocked(500);
        let f = Freshness::default().for_chain("arc-testnet", &arc).await;
        assert_eq!(f.poll_interval, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn an_explicit_interval_is_not_measured() {
        let source = clocked(12_000);
        let f = Freshness::from_flags(Some(Duration::from_secs(1)), false)
            .for_chain("sepolia", &source)
            .await;
        assert_eq!(f.poll_interval, Duration::from_secs(1));
        assert_eq!(source.calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn every_shipped_block_time_agrees_with_its_six_hour_seal_span() {
        for c in crate::chains::all() {
            let hours = c.seal_span as f64 * c.block_time_ms as f64 / 3_600_000.0;
            assert!(
                (5.9..=6.1).contains(&hours),
                "{}: seal_span {} at {} ms is {hours:.2} h, not the 6 h both were measured for",
                c.name,
                c.seal_span,
                c.block_time_ms
            );
        }
    }

    #[test]
    fn durations_parse_in_seconds_minutes_and_hours() {
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration("300").unwrap(), Duration::from_secs(300));
        assert_eq!(
            parse_duration(" 10 min ").unwrap(),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn a_span_accepts_zero_and_leaves_the_refusal_to_its_caller() {
        assert_eq!(parse_span("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_span("5m").unwrap(), Duration::from_secs(300));
        assert!(parse_span("5d").is_err());
    }

    #[test]
    fn nonsense_and_zero_are_refused() {
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("0m").is_err());
        assert!(parse_duration("fast").is_err());
        assert!(parse_duration("5d").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn the_ceiling_is_the_tip_unless_finality_only() {
        let tip = Freshness::default();
        assert_eq!(tip.ceiling(1_000, 900), 1_000);
        let fin = Freshness {
            finality_only: true,
            ..Freshness::default()
        };
        assert_eq!(fin.ceiling(1_000, 900), 900);
        // A finality signal ahead of the tip (a lagging `latest` on a load-balanced pool) never
        // lifts the ceiling above what the source can serve.
        assert_eq!(fin.ceiling(1_000, 1_200), 1_000);
    }

    #[test]
    fn the_stall_threshold_scales_with_the_interval() {
        assert_eq!(Freshness::default().stall_threshold_secs(90), 90);
        let slow = Freshness {
            poll_interval: Duration::from_secs(300),
            finality_only: false,
            poll_interval_explicit: true,
        };
        assert_eq!(slow.stall_threshold_secs(90), 900);
        // A long base threshold (the seal-progress one) is not shortened by a short interval.
        assert_eq!(slow.stall_threshold_secs(1_000), 1_000);
    }
}
