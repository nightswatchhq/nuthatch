//! Operator-visible resource governance for the embedded analytical engine (RFC-0047 C4, #1225).
//!
//! Runtime knobs, not nest identity: read from the environment, never from `nuthatch.toml`, so two
//! operators with different RAM still share a nest content address. Same shape as
//! `NUTHATCH_SQL_MAX_CONCURRENCY`.

use anyhow::{bail, Result};
use std::path::PathBuf;

/// Per-connection DuckDB ceiling. Operator key `analytics.memory_limit`.
pub const DEFAULT_MEMORY_LIMIT_MB: u64 = 512;
/// DuckDB worker threads. Operator key `analytics.threads`.
pub const DEFAULT_THREADS: i64 = 2;
/// Hard ceiling on `analytics.threads`. Matches [`crate::serve::SQL_MAX_CONCURRENCY_CEILING`].
pub const THREADS_CEILING: i64 = crate::serve::SQL_MAX_CONCURRENCY_CEILING as i64;

pub const ENV_MEMORY_LIMIT: &str = "NUTHATCH_ANALYTICS_MEMORY_LIMIT";
pub const ENV_THREADS: &str = "NUTHATCH_ANALYTICS_THREADS";
pub const ENV_TEMP_DIRECTORY: &str = "NUTHATCH_ANALYTICS_TEMP_DIRECTORY";
pub const ENV_MAX_TEMP_SIZE: &str = "NUTHATCH_ANALYTICS_MAX_TEMP_SIZE";
pub const ENV_INGESTION_RESERVATION: &str = "NUTHATCH_INGESTION_RESERVATION";

/// Unmeasured. RFC-0047 §6 wants a high-water mark for Rust, DBSP, decode and result
/// materialisation outside DuckDB, on the box that enforces the 2 GB budget. Counted as zero
/// rather than invented; the term still appears in the inequality so an operator can see it.
pub const RUNTIME_HEADROOM_MB: u64 = 0;

/// Live analytics resource settings. Defaults equal the constants the binary already applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyticsConfig {
    pub memory_limit_mb: u64,
    pub threads: i64,
    /// Parent of per-instance spill directories. `None` → `std::env::temp_dir()`. Instance dirs
    /// remain `nuthatch-duckdb-{pid}-{seq}` under it (#1165).
    pub temp_directory: Option<PathBuf>,
    /// DuckDB `max_temp_directory_size`. `None` → unset, today's unbounded spill.
    pub max_temp_size: Option<String>,
    /// Named ingest floor, in MB. `None` → [`derived_ingestion_reservation_mb`].
    pub ingestion_reservation_mb: Option<u64>,
}

impl Default for AnalyticsConfig {
    fn default() -> Self {
        Self {
            memory_limit_mb: DEFAULT_MEMORY_LIMIT_MB,
            threads: DEFAULT_THREADS,
            temp_directory: None,
            max_temp_size: None,
            ingestion_reservation_mb: None,
        }
    }
}

impl AnalyticsConfig {
    /// Remainder of the 2 GiB cursor budget after the shipped DuckDB split (2 × 512 MB).
    ///
    /// Not a measured ingest RSS high-water. RFC-0047 §6 leaves that unresolved; this is the floor
    /// today's walls already left for everything that is not DuckDB.
    pub fn reservation_mb(&self) -> u64 {
        self.ingestion_reservation_mb
            .unwrap_or_else(derived_ingestion_reservation_mb)
    }
}

/// Named ingest floor left after the shipped DuckDB split:
/// `2048 - (SQL_MAX_CONCURRENCY × DEFAULT_MEMORY_LIMIT_MB)`. Not an ingest RSS cap.
///
/// **It is a floor and not a slider**, which is the whole reason the inequality means anything.
/// Nothing in the runtime caps ingest, DBSP, redb or result materialisation at this figure, so a
/// config that lowered it would not shrink ingest by one byte - it would only buy DuckDB headroom
/// against a promise no code keeps, and the cursor could then exceed 2 GiB with the gate green.
/// `runtime_headroom` is inside this number for the same reason: it is unmeasured, so it cannot be
/// a term an operator gets to spend. Raising the reservation is the conservative direction and is
/// allowed; lowering it is refused by [`validate_against`]. The consequence is the property worth
/// stating: **no accepted split hands DuckDB more RAM than the shipped default the footprint CI
/// job actually measures.**
pub fn derived_ingestion_reservation_mb() -> u64 {
    crate::runtime::DEFAULT_MAX_RSS_MB
        .saturating_sub(crate::serve::SQL_MAX_CONCURRENCY as u64 * DEFAULT_MEMORY_LIMIT_MB)
}

/// Read the live operator settings. Unparseable values fall back to the shipped default and warn.
/// A DuckDB split that dips under the named ingest floor, and a thread count above
/// [`THREADS_CEILING`], are the validator's job - including an explicit zero reservation, which is
/// carried through as `Some(0)` so it is refused rather than read as "unset".
pub fn from_env() -> AnalyticsConfig {
    AnalyticsConfig {
        memory_limit_mb: env_u64_memory(ENV_MEMORY_LIMIT, DEFAULT_MEMORY_LIMIT_MB),
        threads: env_threads(),
        temp_directory: env_path(ENV_TEMP_DIRECTORY),
        max_temp_size: env_size(ENV_MAX_TEMP_SIZE),
        ingestion_reservation_mb: env_optional_memory(ENV_INGESTION_RESERVATION),
    }
}

/// Refuses a DuckDB split that breaches
/// `(sql_permits × analytics.memory_limit) + ingestion_reservation + runtime_headroom ≤ 2 GiB`,
/// an `ingestion_reservation` below [`derived_ingestion_reservation_mb`], and a thread count above
/// [`THREADS_CEILING`]. Does not cap ingest, DBSP, redb, or result materialisation - which is
/// exactly why the reservation may only be raised. 2 GiB is the footprint CI job / process RSS
/// wall; this is the arithmetic that keeps a config from being allowed to breach it quietly.
pub fn validate_cursor_budget() -> Result<()> {
    validate_against(&from_env(), crate::serve::sql_max_concurrency())
}

/// Same check against an explicit config, so a test can drive the inequality without the process
/// environment. `permits` is clamped to the gate's ceiling, the same bound the live gate uses.
pub fn validate_against(cfg: &AnalyticsConfig, permits: usize) -> Result<()> {
    let permits = permits.clamp(1, crate::serve::SQL_MAX_CONCURRENCY_CEILING) as u64;
    if cfg.memory_limit_mb == 0 {
        bail!(
            "analytics.memory_limit must be greater than zero (set {ENV_MEMORY_LIMIT}, default \
             {DEFAULT_MEMORY_LIMIT_MB}MB)"
        );
    }
    if cfg.threads < 1 {
        bail!(
            "analytics.threads must be at least 1 (set {ENV_THREADS}, default {DEFAULT_THREADS})"
        );
    }
    if cfg.threads > THREADS_CEILING {
        bail!(
            "analytics.threads is {}, above the ceiling of {THREADS_CEILING} (set {ENV_THREADS}, \
             default {DEFAULT_THREADS}). DuckDB worker threads are not an unconstrained config key.",
            cfg.threads
        );
    }
    let duck = permits.saturating_mul(cfg.memory_limit_mb);
    let reservation = cfg.reservation_mb();
    let floor = derived_ingestion_reservation_mb();
    if reservation < floor {
        bail!(
            "ingestion_reservation is {reservation} MB, below the floor of {floor} MB (set \
             {ENV_INGESTION_RESERVATION}). It is a floor, not a slider: nothing caps ingest, DBSP, \
             redb or result materialisation at this figure, so writing a smaller number does not \
             shrink ingest - it only hands DuckDB headroom against a reservation no code enforces, \
             and the cursor can then pass this gate and still exceed the 2 GiB budget. Raise it to \
             give DuckDB less; lower analytics.memory_limit ({ENV_MEMORY_LIMIT}) or \
             NUTHATCH_SQL_MAX_CONCURRENCY if you need room elsewhere."
        );
    }
    let headroom = RUNTIME_HEADROOM_MB;
    let total = duck.saturating_add(reservation).saturating_add(headroom);
    let ceiling = crate::runtime::DEFAULT_MAX_RSS_MB;
    if total > ceiling {
        bail!(
            "DuckDB split does not leave the named ingest floor: (sql_permits × \
             analytics.memory_limit) + ingestion_reservation + runtime_headroom = ({permits} × \
             {} MB) + {reservation} MB + {headroom} MB = {total} MB, which is above {ceiling} MB. \
             This gate refuses that split; it does not cap ingest, DBSP, redb, or result \
             materialisation. The 2 GiB cursor budget is the footprint CI job / process RSS wall, \
             not this arithmetic. Lower analytics.memory_limit ({ENV_MEMORY_LIMIT}) or \
             NUTHATCH_SQL_MAX_CONCURRENCY, or ingestion_reservation ({ENV_INGESTION_RESERVATION}). \
             analytics.max_temp_size is disk and does not buy RAM. A query that cannot run in its \
             budget fails; it never degrades block processing.",
            cfg.memory_limit_mb
        );
    }
    Ok(())
}

fn env_u64_memory(key: &str, default: u64) -> u64 {
    match std::env::var(key) {
        Err(_) => default,
        Ok(raw) => match parse_memory_mb(&raw) {
            Some(n) if n > 0 => n,
            _ => {
                tracing::warn!(
                    value = %raw,
                    default,
                    "{key} is not a positive size in MB or GB; using the default"
                );
                default
            }
        },
    }
}

/// An explicit `0` is a *value*, and an invalid one - it must reach the validator and be refused
/// there, naming the key. Folding it into `None` made it mean "unset", so the derived default
/// applied and the process started, which is the opposite of what the operator wrote.
fn env_optional_memory(key: &str) -> Option<u64> {
    match std::env::var(key) {
        Err(_) => None,
        Ok(raw) if raw.trim().is_empty() => None,
        Ok(raw) => match parse_memory_mb(&raw) {
            Some(n) => Some(n),
            None => {
                tracing::warn!(
                    value = %raw,
                    "{key} is not a size in MB or GB; leaving ingestion_reservation unset"
                );
                None
            }
        },
    }
}

/// `n >= 1` is kept as requested so [`validate_against`] can refuse n above [`THREADS_CEILING`].
fn env_threads() -> i64 {
    match std::env::var(ENV_THREADS) {
        Err(_) => DEFAULT_THREADS,
        Ok(raw) => match raw.trim().parse::<i64>() {
            Ok(n) if n >= 1 => n,
            _ => {
                tracing::warn!(
                    value = %raw,
                    default = DEFAULT_THREADS,
                    "{ENV_THREADS} is not a positive integer; using the default"
                );
                DEFAULT_THREADS
            }
        },
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    match std::env::var(key) {
        Ok(raw) if !raw.trim().is_empty() => Some(PathBuf::from(raw.trim())),
        _ => None,
    }
}

fn env_size(key: &str) -> Option<String> {
    match std::env::var(key) {
        Err(_) => None,
        Ok(raw) if raw.trim().is_empty() => None,
        Ok(raw) => match parse_size_for_duckdb(&raw) {
            Some(s) => Some(s),
            None => {
                tracing::warn!(
                    value = %raw,
                    "{key} is not a size DuckDB will accept (e.g. 10GB, 512MB); leaving unset"
                );
                None
            }
        },
    }
}

/// `512`, `512MB`, `1G`, `2GiB` → megabytes. No fractional values.
pub fn parse_memory_mb(raw: &str) -> Option<u64> {
    let (n, unit) = split_size(raw)?;
    match unit.as_str() {
        "" | "m" | "mb" | "mib" => Some(n),
        "g" | "gb" | "gib" => n.checked_mul(1024),
        _ => None,
    }
}

/// Normalise an operator size into a string DuckDB's `max_temp_directory_size` accepts.
pub fn parse_size_for_duckdb(raw: &str) -> Option<String> {
    let (n, unit) = split_size(raw)?;
    match unit.as_str() {
        "" | "m" | "mb" | "mib" => Some(format!("{n}MB")),
        "g" | "gb" | "gib" => Some(format!("{n}GB")),
        _ => None,
    }
}

fn split_size(raw: &str) -> Option<(u64, String)> {
    let s = raw.trim();
    let digits = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if digits == 0 {
        return None;
    }
    let n: u64 = s[..digits].parse().ok()?;
    let unit = s[digits..].trim().to_ascii_lowercase();
    Some((n, unit))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::DEFAULT_MAX_RSS_MB;
    use crate::serve::{SQL_MAX_CONCURRENCY, SQL_MAX_CONCURRENCY_CEILING};

    fn env_lock() -> &'static std::sync::Mutex<()> {
        static L: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        L.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct EnvRestore {
        prev: Option<String>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.prev.as_deref() {
                Some(v) => std::env::set_var(ENV_THREADS, v),
                None => std::env::remove_var(ENV_THREADS),
            }
        }
    }

    #[test]
    fn defaults_equal_todays_constants() {
        let cfg = AnalyticsConfig::default();
        assert_eq!(cfg.memory_limit_mb, 512);
        assert_eq!(cfg.threads, 2);
        assert!(cfg.temp_directory.is_none());
        assert!(cfg.max_temp_size.is_none());
        assert!(cfg.ingestion_reservation_mb.is_none());
        assert_eq!(SQL_MAX_CONCURRENCY, 2);
        assert_eq!(DEFAULT_MEMORY_LIMIT_MB, 512);
        assert_eq!(DEFAULT_THREADS, 2);
        assert_eq!(THREADS_CEILING, SQL_MAX_CONCURRENCY_CEILING as i64);
        assert_eq!(THREADS_CEILING, 16);
        assert_eq!(derived_ingestion_reservation_mb(), 1024);
        assert_eq!(DEFAULT_MAX_RSS_MB, 2048);
        assert_eq!(RUNTIME_HEADROOM_MB, 0);
        assert_eq!(
            SQL_MAX_CONCURRENCY as u64 * DEFAULT_MEMORY_LIMIT_MB
                + derived_ingestion_reservation_mb()
                + RUNTIME_HEADROOM_MB,
            DEFAULT_MAX_RSS_MB,
            "the shipped split must fill the cursor budget exactly, or a default change has moved \
             the wall without a test going red"
        );
    }

    #[test]
    fn unconfigured_config_validates_at_the_shipped_permits() {
        validate_against(&AnalyticsConfig::default(), SQL_MAX_CONCURRENCY)
            .expect("today's walls must still start");
    }

    #[test]
    fn over_budget_config_is_refused_naming_the_keys() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 2048,
            ..AnalyticsConfig::default()
        };
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("analytics.memory_limit"),
            "must name analytics.memory_limit: {err}"
        );
        assert!(
            err.contains("ingestion_reservation"),
            "must name ingestion_reservation: {err}"
        );
        assert!(
            err.contains("runtime_headroom"),
            "must name runtime_headroom: {err}"
        );
        assert!(err.contains("sql_permits"), "must name sql_permits: {err}");
        assert!(
            err.contains("max_temp_size") && err.contains("disk"),
            "must say max_temp_size is not RAM: {err}"
        );
        assert!(
            err.contains(ENV_MEMORY_LIMIT) && err.contains(ENV_INGESTION_RESERVATION),
            "must name the env keys an operator can actually set: {err}"
        );
    }

    #[test]
    fn raising_permits_without_lowering_memory_is_refused() {
        let err = validate_against(&AnalyticsConfig::default(), 4)
            .unwrap_err()
            .to_string();
        assert!(err.contains("analytics.memory_limit"), "{err}");
        assert!(err.contains("NUTHATCH_SQL_MAX_CONCURRENCY"), "{err}");
    }

    #[test]
    fn fitting_memory_at_four_permits_is_accepted() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 256,
            ..AnalyticsConfig::default()
        };
        validate_against(&cfg, 4).expect("4 × 256 + 1024 = 2048");
    }

    #[test]
    fn explicit_reservation_that_overfills_is_refused() {
        let cfg = AnalyticsConfig {
            ingestion_reservation_mb: Some(1536),
            ..AnalyticsConfig::default()
        };
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ingestion_reservation"), "{err}");
    }

    /// The finding on #1241: `2 × 768 + 512 = 2048` balances, and is still a cursor that can go
    /// over 2 GiB, because the 512 reserves accounting space and caps nothing. A split may not buy
    /// DuckDB room by writing down a smaller number for ingest.
    #[test]
    fn lowering_the_reservation_to_buy_duckdb_memory_is_refused() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 768,
            ingestion_reservation_mb: Some(512),
            ..AnalyticsConfig::default()
        };
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ingestion_reservation") && err.contains("floor"),
            "must say the reservation is a floor: {err}"
        );
        assert!(err.contains(ENV_INGESTION_RESERVATION), "{err}");
    }

    /// Raising it is the conservative direction and stays allowed: DuckDB gets less, not more.
    #[test]
    fn raising_the_reservation_is_allowed_and_shrinks_duckdb() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 256,
            ingestion_reservation_mb: Some(1536),
            ..AnalyticsConfig::default()
        };
        validate_against(&cfg, SQL_MAX_CONCURRENCY).expect("2 × 256 + 1536 = 2048");
    }

    /// The property the floor buys, stated as a test rather than as a paragraph: whatever an
    /// operator writes, an accepted split never hands DuckDB more than the shipped default that
    /// the footprint CI job measures.
    #[test]
    fn no_accepted_split_gives_duckdb_more_than_the_measured_default() {
        let shipped = SQL_MAX_CONCURRENCY as u64 * DEFAULT_MEMORY_LIMIT_MB;
        let mut accepted = 0;
        for permits in 1..=SQL_MAX_CONCURRENCY_CEILING {
            for memory_limit_mb in [64, 128, 256, 512, 768, 1024, 1536, 2048] {
                for reservation in [0, 256, 512, 1023, 1024, 1536, 2048] {
                    let cfg = AnalyticsConfig {
                        memory_limit_mb,
                        ingestion_reservation_mb: Some(reservation),
                        ..AnalyticsConfig::default()
                    };
                    if validate_against(&cfg, permits).is_err() {
                        continue;
                    }
                    accepted += 1;
                    let duck = permits as u64 * memory_limit_mb;
                    assert!(
                        duck <= shipped,
                        "accepted {permits} × {memory_limit_mb} MB = {duck} MB of DuckDB with a \
                         {reservation} MB reservation, above the measured default of {shipped} MB"
                    );
                }
            }
        }
        assert!(
            accepted > 10,
            "only {accepted} splits were accepted; the gate refuses everything and the assertion \
             above is vacuous"
        );
    }

    #[test]
    fn max_temp_size_does_not_enter_the_ram_equation() {
        let cfg = AnalyticsConfig {
            max_temp_size: Some("100GB".into()),
            ..AnalyticsConfig::default()
        };
        validate_against(&cfg, SQL_MAX_CONCURRENCY).expect("disk cap is not RAM");
    }

    #[test]
    fn permits_above_the_gate_ceiling_are_costed_at_the_ceiling() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 64,
            ingestion_reservation_mb: Some(1024),
            ..AnalyticsConfig::default()
        };
        validate_against(&cfg, 64).expect("clamped to 16 × 64 + 1024 = 2048");
        let too_big = AnalyticsConfig {
            memory_limit_mb: 128,
            ingestion_reservation_mb: Some(1024),
            ..AnalyticsConfig::default()
        };
        assert!(validate_against(&too_big, 64).is_err());
        assert_eq!(SQL_MAX_CONCURRENCY_CEILING, 16);
    }

    #[test]
    fn zero_memory_limit_is_refused() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 0,
            ..AnalyticsConfig::default()
        };
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(err.contains("analytics.memory_limit"), "{err}");
    }

    #[test]
    fn zero_ingest_reservation_is_refused() {
        let cfg = AnalyticsConfig {
            memory_limit_mb: 512,
            ingestion_reservation_mb: Some(0),
            ..AnalyticsConfig::default()
        };
        let err = validate_against(&cfg, 1).unwrap_err().to_string();
        assert!(
            err.contains("ingestion_reservation"),
            "must name ingestion_reservation: {err}"
        );
        assert!(
            err.contains(ENV_INGESTION_RESERVATION),
            "must name the env key an operator can actually set: {err}"
        );
        validate_against(&AnalyticsConfig::default(), SQL_MAX_CONCURRENCY)
            .expect("today's walls must still start");
    }

    #[test]
    fn oversize_thread_count_is_refused() {
        let cfg = AnalyticsConfig {
            threads: i64::MAX,
            ..AnalyticsConfig::default()
        };
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("analytics.threads"),
            "must name analytics.threads: {err}"
        );
        assert!(
            err.contains(&THREADS_CEILING.to_string()),
            "must name the ceiling: {err}"
        );
        assert!(
            err.contains(ENV_THREADS),
            "must name the env key an operator can actually set: {err}"
        );
        validate_against(
            &AnalyticsConfig {
                threads: THREADS_CEILING,
                ..AnalyticsConfig::default()
            },
            SQL_MAX_CONCURRENCY,
        )
        .expect("the ceiling itself must still start");
    }

    #[test]
    fn from_env_preserves_oversize_threads_for_the_validator() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var(ENV_THREADS).ok();
        std::env::set_var(ENV_THREADS, "17");
        let _restore = EnvRestore { prev };
        let cfg = from_env();
        assert_eq!(
            cfg.threads, 17,
            "parsing must not clamp 17 to {THREADS_CEILING}"
        );
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("analytics.threads"),
            "must name analytics.threads: {err}"
        );
        assert!(
            err.contains(&THREADS_CEILING.to_string()),
            "must name the ceiling: {err}"
        );
        assert!(
            err.contains(ENV_THREADS),
            "must name the env key an operator can actually set: {err}"
        );
    }

    /// The second finding on #1241. `NUTHATCH_INGESTION_RESERVATION=0` used to fall through to
    /// `None`, so the derived 1024 applied and the process started - the documented knob could not
    /// express the one value it is documented to reject.
    #[test]
    fn an_explicit_zero_reservation_reaches_the_validator() {
        let _g = env_lock().lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var(ENV_INGESTION_RESERVATION).ok();
        struct Restore(Option<String>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.as_deref() {
                    Some(v) => std::env::set_var(ENV_INGESTION_RESERVATION, v),
                    None => std::env::remove_var(ENV_INGESTION_RESERVATION),
                }
            }
        }
        std::env::set_var(ENV_INGESTION_RESERVATION, "0");
        let _restore = Restore(prev);
        let cfg = from_env();
        assert_eq!(
            cfg.ingestion_reservation_mb,
            Some(0),
            "an explicit 0 must not read as unset"
        );
        assert_ne!(
            cfg.reservation_mb(),
            derived_ingestion_reservation_mb(),
            "a written 0 must not silently become the derived default"
        );
        let err = validate_against(&cfg, SQL_MAX_CONCURRENCY)
            .unwrap_err()
            .to_string();
        assert!(err.contains(ENV_INGESTION_RESERVATION), "{err}");
    }

    #[test]
    fn parse_memory_accepts_the_rfc_spellings() {
        assert_eq!(parse_memory_mb("512"), Some(512));
        assert_eq!(parse_memory_mb("512MB"), Some(512));
        assert_eq!(parse_memory_mb(" 512mb "), Some(512));
        assert_eq!(parse_memory_mb("1GB"), Some(1024));
        assert_eq!(parse_memory_mb("2GiB"), Some(2048));
        assert_eq!(parse_memory_mb("1G"), Some(1024));
        assert_eq!(parse_memory_mb(""), None);
        assert_eq!(parse_memory_mb("eight"), None);
        assert_eq!(parse_memory_mb("1.5GB"), None);
        assert_eq!(parse_memory_mb("512TB"), None);
    }

    #[test]
    fn parse_size_for_duckdb_normalises() {
        assert_eq!(parse_size_for_duckdb("10GB").as_deref(), Some("10GB"));
        assert_eq!(parse_size_for_duckdb("512").as_deref(), Some("512MB"));
        assert_eq!(parse_size_for_duckdb("512MB").as_deref(), Some("512MB"));
        assert_eq!(parse_size_for_duckdb("nope"), None);
    }
}
