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
//! RFC-0040 §4's conditions hold by construction: `/ready` reports the mode and interval and its
//! stall thresholds scale with the interval (no silent staleness); a slower cursor returns the same
//! rows later, never a substitute (no fabricated values); and sealing is untouched, so a segment's
//! content address stays a function of its block range and rows.

use std::time::Duration;

/// The tip path's historical cadence: as close to the chain as the loop can get.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How stale a cursor is allowed to be, and how it gets there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// How long a caught-up cursor waits before asking for the tip again (knob 1).
    pub poll_interval: Duration,
    /// Cap the cursor at the chain's finality boundary instead of the tip (knob 2). Nothing indexed
    /// under it can be reorged, so the reorg check is not paid once the store holds no unfinalised
    /// rows, and the hot store only ever carries rows waiting to seal.
    pub finality_only: bool,
}

impl Default for Freshness {
    fn default() -> Self {
        Freshness {
            poll_interval: DEFAULT_POLL_INTERVAL,
            finality_only: false,
        }
    }
}

impl Freshness {
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

/// Parse an operator's duration: `2s`, `5m`, `1h`, or bare seconds. Zero is refused - a cursor that
/// never waits is a busy loop, and the two-second default already means "as fast as sensible".
pub fn parse_duration(s: &str) -> Result<Duration, String> {
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
    if secs == 0 {
        return Err("the poll interval must be at least one second".into());
    }
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        assert_eq!(slow.stall_threshold_secs(90), 900);
        // A long base threshold (the seal-progress one) is not shortened by a short interval.
        assert_eq!(slow.stall_threshold_secs(1_000), 1_000);
    }
}
