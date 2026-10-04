//! The sampled audit of sealed history against a second endpoint (RFC-0049 §10 item 5, #1786).
//!
//! Nothing on the ingest path can see an `eth_getLogs` answer with a hole in the middle (#1670). This
//! asks a different endpoint for a sample of sealed ranges, decodes its answer with the nest's own
//! registry, and compares the rows with the sealed segments' rows for the same blocks. A difference
//! is reported, never repaired: sealed segments are immutable, and which endpoint was wrong is for a
//! person to decide.

use crate::registry::{DecodeRegistry, DecodedRow, TableKind, TableSchema};
use crate::source::{LogFilter, Source};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Blocks in one sampled range.
pub const DEFAULT_SPAN: u64 = 1_000;
/// Ranges the background audit samples a day.
pub const DEFAULT_PER_DAY: u64 = 24;
/// Logs held for one sample. A denser range is audited up to the block where the budget ran out.
pub const SAMPLE_LOG_BUDGET: usize = 20_000;
/// Rows of each kind a mismatch report names; the counts are always complete.
const NAMED_ROWS: usize = 10;

/// `(block_number, log_index)`: one log, whichever table it decoded into.
type RowKey = (u64, u64);

/// What the audit compares: the nest's contract event tables, decoded by the registry the indexer
/// uses. Factory children are left out, since which children exist at a block depends on all the
/// history before it rather than on the sampled range.
pub struct Auditor {
    registry: DecodeRegistry,
    schema: Vec<TableSchema>,
    filter: LogFilter,
}

impl Auditor {
    pub fn new(dir: &Path, config: &crate::config::Config) -> Result<Self> {
        let registry = crate::registry::from_nest(dir, config)?;
        let aliases: BTreeSet<&str> = config.contracts.iter().map(|c| c.alias.as_str()).collect();
        let schema: Vec<TableSchema> = registry
            .schema()
            .into_iter()
            .filter(|t| t.kind == TableKind::Event && aliases.contains(t.alias.as_str()))
            .collect();
        let addresses: Vec<String> = registry
            .addresses()
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect();
        let topic0s: Vec<String> = schema
            .iter()
            .map(|t| t.topic0.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let filter = LogFilter::new(&addresses, &topic0s)
            .context("this nest declares no contract events, so there is nothing to audit")?;
        Ok(Self {
            registry,
            schema,
            filter,
        })
    }

    fn audits(&self, table: &str) -> bool {
        self.schema.iter().any(|t| t.table == table)
    }
}

/// One audited range and how the two sides differ.
#[derive(Debug)]
pub struct RangeReport {
    pub from: u64,
    /// The last block compared. Below `requested_to` when the range held more than
    /// [`SAMPLE_LOG_BUDGET`] logs.
    pub to: u64,
    pub requested_to: u64,
    pub endpoint_rows: usize,
    pub sealed_rows: usize,
    /// Rows the endpoint served that no sealed segment holds: what an omitting first endpoint leaves.
    pub endpoint_only: Vec<String>,
    /// Sealed rows the endpoint did not serve.
    pub sealed_only: Vec<String>,
    /// `(sealed, endpoint)` for one log that decoded differently.
    pub differing: Vec<(String, String)>,
}

impl RangeReport {
    pub fn mismatches(&self) -> usize {
        self.endpoint_only.len() + self.sealed_only.len() + self.differing.len()
    }

    /// The lines an operator reads: one summary, then up to [`NAMED_ROWS`] rows of each kind.
    pub fn describe(&self, endpoint: &str) -> Vec<String> {
        let truncated = if self.to < self.requested_to {
            format!(
                " (stopped at {} of {}: the range held more than {SAMPLE_LOG_BUDGET} logs)",
                self.to, self.requested_to
            )
        } else {
            String::new()
        };
        if self.mismatches() == 0 {
            return vec![format!(
                "sealed audit clean: blocks {}..={} against {endpoint}, {} row(s) on both sides{truncated}",
                self.from, self.to, self.sealed_rows
            )];
        }
        let mut lines = vec![format!(
            "sealed audit MISMATCH: blocks {}..={} against {endpoint}: {} row(s) the endpoint served \
             that the sealed segments lack, {} sealed row(s) it did not serve, {} decoded \
             differently ({} sealed, {} served){truncated}. Sealed data is unchanged; ask a third \
             endpoint before deciding which side is wrong",
            self.from,
            self.to,
            self.endpoint_only.len(),
            self.sealed_only.len(),
            self.differing.len(),
            self.sealed_rows,
            self.endpoint_rows,
        )];
        for row in self.endpoint_only.iter().take(NAMED_ROWS) {
            lines.push(format!("  endpoint only: {row}"));
        }
        for row in self.sealed_only.iter().take(NAMED_ROWS) {
            lines.push(format!("  sealed only:   {row}"));
        }
        for (sealed, served) in self.differing.iter().take(NAMED_ROWS) {
            lines.push(format!("  sealed:        {sealed}"));
            lines.push(format!("  served:        {served}"));
        }
        lines
    }
}

/// A row as both sides compare it. `block_timestamp` comes from a header rather than the log, and
/// the audit fetches no headers, so it is zeroed on both sides.
fn canonical(row: &DecodedRow) -> String {
    let mut row = row.clone();
    row.block_timestamp = 0;
    row.to_json().to_string()
}

fn digest(row: &str) -> [u8; 32] {
    Sha256::digest(row.as_bytes()).into()
}

/// Re-fetch `from..=to` from `source` and compare it with the sealed rows for those blocks.
pub async fn audit_range(
    dir: &Path,
    auditor: &Auditor,
    source: &dyn Source,
    from: u64,
    to: u64,
) -> Result<RangeReport> {
    let (logs, _, covered) = crate::indexer::fetch_logs_splitting_tracked(
        source,
        &auditor.filter,
        from,
        to,
        SAMPLE_LOG_BUDGET,
    )
    .await?;
    let requested_to = to;
    let to = covered.min(to);

    let mut served: BTreeMap<RowKey, String> = BTreeMap::new();
    for log in &logs {
        if let Ok(Some(row)) = auditor.registry.decode(log) {
            if auditor.audits(&row.table) {
                served.insert((row.block_number, row.log_index), canonical(&row));
            }
        }
    }
    drop(logs);

    let read = {
        let dir = dir.to_path_buf();
        let schema = auditor.schema.clone();
        // A sweep in another process may fold a provisional segment away between reading the
        // catalogue and opening the file; the second read sees the catalogue that replaced it.
        move || {
            sealed_rows(&dir, &schema, from, to).or_else(|_| sealed_rows(&dir, &schema, from, to))
        }
    };
    let (sealed, duplicates) = tokio::task::spawn_blocking(read)
        .await
        .context("the sealed read panicked")??;

    let mut report = RangeReport {
        from,
        to,
        requested_to,
        endpoint_rows: served.len(),
        sealed_rows: sealed.len() + duplicates.len(),
        endpoint_only: Vec::new(),
        sealed_only: duplicates,
        differing: Vec::new(),
    };
    for (key, row) in &sealed {
        match served.get(key) {
            None => report.sealed_only.push(row.clone()),
            Some(other) if digest(other) != digest(row) => {
                report.differing.push((row.clone(), other.clone()))
            }
            Some(_) => {}
        }
    }
    for (key, row) in served {
        if !sealed.contains_key(&key) {
            report.endpoint_only.push(row);
        }
    }
    Ok(report)
}

/// The audited tables' sealed rows in `from..=to`, read one segment at a time. A second sealed row
/// for one log is returned apart, since no endpoint can serve it twice.
fn sealed_rows(
    dir: &Path,
    schema: &[TableSchema],
    from: u64,
    to: u64,
) -> Result<(BTreeMap<RowKey, String>, Vec<String>)> {
    let _lease = crate::seal::read_lease(dir);
    let manifest = crate::seal::load_manifest(dir)?;
    let mut rows = BTreeMap::new();
    let mut duplicates = Vec::new();
    for table in schema {
        let Some(segments) = manifest.tables.get(&table.table) else {
            continue;
        };
        for segment in segments
            .iter()
            .filter(|s| s.from_block <= to && s.to_block >= from)
        {
            for row in crate::seal::read_segment_decoded(dir, segment, table)? {
                if (from..=to).contains(&row.block_number) {
                    let key = (row.block_number, row.log_index);
                    if let Some(earlier) = rows.insert(key, canonical(&row)) {
                        duplicates.push(earlier);
                    }
                }
            }
        }
    }
    Ok((rows, duplicates))
}

/// The sealed block range, and the audited tables' segments with their row counts.
struct Catalogue {
    lo: u64,
    hi: u64,
    weighted: Vec<(u64, u64, u64)>,
    rows: u64,
}

impl Catalogue {
    fn load(dir: &Path, auditor: &Auditor) -> Result<Option<Self>> {
        let manifest = crate::seal::load_manifest(dir)?;
        let all = manifest.tables.values().flatten();
        let (Some(lo), Some(hi)) = (
            all.clone().map(|s| s.from_block).min(),
            all.map(|s| s.to_block).max(),
        ) else {
            return Ok(None);
        };
        let mut weighted: Vec<(u64, u64, u64)> = auditor
            .schema
            .iter()
            .filter_map(|t| manifest.tables.get(&t.table))
            .flatten()
            .filter(|s| s.rows > 0)
            .map(|s| (s.from_block, s.to_block, s.rows as u64))
            .collect();
        weighted.sort_unstable();
        let rows = weighted.iter().map(|w| w.2).sum();
        Ok(Some(Self {
            lo,
            hi,
            weighted,
            rows,
        }))
    }

    /// Sample `index` under `seed`. Even samples land in a segment chosen in proportion to its rows,
    /// where an omission has something to omit; odd samples are uniform over the sealed blocks, so a
    /// range the first endpoint answered as empty is still asked about.
    fn pick(&self, seed: u64, index: u64, span: u64) -> (u64, u64) {
        let mut state = seed.wrapping_add(index.wrapping_mul(GOLDEN));
        let (a, b) = (splitmix64(&mut state), splitmix64(&mut state));
        let centre = if index.is_multiple_of(2) && self.rows > 0 {
            let mut at = a % self.rows;
            let &(from, to, _) = self
                .weighted
                .iter()
                .find(|w| {
                    if at < w.2 {
                        true
                    } else {
                        at -= w.2;
                        false
                    }
                })
                .expect("at is below the sum of the weights");
            from + b % (to - from + 1)
        } else {
            self.lo + a % (self.hi - self.lo + 1)
        };
        let span = span.clamp(1, self.hi - self.lo + 1);
        let from = centre
            .saturating_sub(span / 2)
            .clamp(self.lo, self.hi + 1 - span);
        (from, from + span - 1)
    }
}

const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// SplitMix64, written out so a seed picks the same ranges on every build.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(GOLDEN);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The ranges samples `indices` audit under `seed`, or `None` when nothing has sealed yet.
pub fn sample_ranges(
    dir: &Path,
    auditor: &Auditor,
    seed: u64,
    indices: std::ops::Range<u64>,
    span: u64,
) -> Result<Option<Vec<(u64, u64)>>> {
    Ok(Catalogue::load(dir, auditor)?.map(|c| indices.map(|i| c.pick(seed, i, span)).collect()))
}

/// Refuse an audit endpoint the nest indexes from: an omission it made once it would make again.
pub fn refuse_indexing_endpoint(audit: &str, indexing: &[String]) -> Result<()> {
    let norm = |u: &str| u.trim().trim_end_matches('/').to_string();
    if indexing.iter().any(|u| norm(u) == norm(audit)) {
        bail!(
            "the audit endpoint {} is one this nest indexes from; an audit against it can only agree \
             with itself. Point it at a different provider",
            crate::rpc::redact_url(audit)
        );
    }
    let host = crate::rpc::redact_url(audit);
    if indexing.iter().any(|u| crate::rpc::redact_url(u) == host) {
        tracing::warn!(
            "the audit endpoint shares a host with an indexing endpoint ({host}); one provider may \
             serve both from the same backend, and an omission it makes twice is invisible"
        );
    }
    Ok(())
}

fn record(report: &RangeReport) {
    crate::metrics::METRICS.inc_audit_ranges();
    crate::metrics::METRICS.add_audit_mismatches(report.mismatches() as u64);
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `nuthatch audit sealed`.
pub async fn run_cli(args: crate::cli::AuditSealedArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = crate::config::Config::load(&dir)?;
    refuse_indexing_endpoint(&args.rpc, &config.nest.rpc_urls)?;
    let auditor = Auditor::new(&dir, &config)?;
    let rpc = crate::rpc::RpcClient::new(vec![args.rpc.clone()])?;
    rpc.verify_chain_ids(config.nest.chain_id).await?;
    let endpoint = crate::rpc::redact_url(&args.rpc);

    let ranges = match (args.from, args.to) {
        (Some(from), Some(to)) => {
            let Some(c) = Catalogue::load(&dir, &auditor)? else {
                bail!("nothing in {} has sealed yet", dir.display());
            };
            if from > to || from < c.lo || to > c.hi {
                bail!(
                    "blocks {from}..={to} are not inside the sealed range {}..={}",
                    c.lo,
                    c.hi
                );
            }
            vec![(from, to)]
        }
        _ => {
            let seed = args.seed.unwrap_or_else(unix_now);
            println!("seed {seed}");
            sample_ranges(&dir, &auditor, seed, 0..args.samples, args.span)?
                .with_context(|| format!("nothing in {} has sealed yet", dir.display()))?
        }
    };

    let mut mismatches = 0;
    for (from, to) in ranges {
        let report = audit_range(&dir, &auditor, &rpc, from, to).await?;
        record(&report);
        for line in report.describe(&endpoint) {
            println!("{line}");
        }
        mismatches += report.mismatches();
    }
    println!("{} JSON-RPC request(s) to {endpoint}", rpc.request_count());
    if mismatches > 0 {
        bail!("{mismatches} row(s) differ between the sealed segments and {endpoint}");
    }
    Ok(())
}

/// The background audit's settings, from `dev`'s `--audit-*` flags.
pub struct Settings {
    pub rpc: String,
    pub per_day: u64,
    pub span: u64,
    pub seed: Option<u64>,
}

/// Start the background audit: one sampled range every `86400 / per_day` seconds, the first one
/// interval after start. It never stops the nest; a failed sample is a warning and a counter.
pub fn spawn(
    dir: PathBuf,
    config: &crate::config::Config,
    indexing: &[String],
    settings: Settings,
) -> Result<tokio::task::JoinHandle<()>> {
    refuse_indexing_endpoint(&settings.rpc, indexing)?;
    let auditor = Auditor::new(&dir, config)?;
    let rpc = crate::rpc::RpcClient::new(vec![settings.rpc.clone()])?;
    let endpoint = crate::rpc::redact_url(&settings.rpc);
    let chain_id = config.nest.chain_id;
    let interval = Duration::from_secs((86_400 / settings.per_day.max(1)).max(1));
    let seed = settings.seed.unwrap_or_else(unix_now);
    let span = settings.span;
    tracing::info!(
        "sealed audit on: {} range(s) of {span} blocks a day against {endpoint}, seed {seed}",
        settings.per_day
    );
    Ok(tokio::spawn(async move {
        let mut chain_verified = false;
        for index in 0u64.. {
            tokio::time::sleep(interval).await;
            let sample = async {
                if !chain_verified {
                    rpc.verify_chain_ids(chain_id).await?;
                    chain_verified = true;
                }
                let Some(ranges) = sample_ranges(&dir, &auditor, seed, index..index + 1, span)?
                else {
                    return Ok(None);
                };
                let (from, to) = ranges[0];
                audit_range(&dir, &auditor, &rpc, from, to).await.map(Some)
            };
            match sample.await {
                Ok(Some(report)) => {
                    record(&report);
                    let lines = report.describe(&endpoint);
                    if report.mismatches() == 0 {
                        tracing::info!("{} (sample {index}, seed {seed})", lines[0]);
                    } else {
                        tracing::warn!("{} (sample {index}, seed {seed})", lines[0]);
                        for line in &lines[1..] {
                            tracing::warn!("{line}");
                        }
                    }
                }
                Ok(None) => tracing::info!("sealed audit: nothing has sealed yet"),
                Err(e) => {
                    crate::metrics::METRICS.inc_audit_errors();
                    tracing::warn!(
                        "sealed audit sample {index} (seed {seed}) against {endpoint} failed: {e:#}"
                    );
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalogue() -> Catalogue {
        Catalogue {
            lo: 100,
            hi: 10_099,
            weighted: vec![(100, 199, 5), (5_000, 5_999, 95)],
            rows: 100,
        }
    }

    #[test]
    fn a_seed_picks_the_same_ranges_and_another_seed_does_not() {
        let c = catalogue();
        let picks = |seed| (0..8).map(|i| c.pick(seed, i, 50)).collect::<Vec<_>>();
        assert_eq!(picks(7), picks(7));
        assert_ne!(picks(7), picks(8));
    }

    #[test]
    fn every_pick_is_span_blocks_inside_the_sealed_range() {
        let c = catalogue();
        for i in 0..2_000 {
            let (from, to) = c.pick(42, i, 50);
            assert_eq!(to - from + 1, 50, "sample {i}");
            assert!(from >= c.lo && to <= c.hi, "sample {i}: {from}..={to}");
        }
        assert_eq!(c.pick(1, 0, 1_000_000), (c.lo, c.hi));
    }

    #[test]
    fn even_samples_follow_the_rows_and_odd_samples_do_not() {
        let c = catalogue();
        let dense = |(from, to): (u64, u64)| to >= 5_000 && from <= 5_999;
        let even = (0..400)
            .step_by(2)
            .filter(|&i| dense(c.pick(3, i, 10)))
            .count();
        let odd = (1..400)
            .step_by(2)
            .filter(|&i| dense(c.pick(3, i, 10)))
            .count();
        // 95% of the rows sit in a tenth of the blocks.
        assert!(even > 170, "{even} of 200 row-weighted samples were dense");
        assert!(odd < 40, "{odd} of 200 uniform samples were dense");
    }

    #[test]
    fn the_indexing_endpoint_is_refused_and_another_is_not() {
        let pool = vec!["https://eth.example/v2/KEY/".to_string()];
        let err = refuse_indexing_endpoint("https://eth.example/v2/KEY", &pool).unwrap_err();
        assert!(!format!("{err:#}").contains("KEY"), "{err:#}");
        refuse_indexing_endpoint("https://other.example/v2/KEY", &pool).unwrap();
    }
}
