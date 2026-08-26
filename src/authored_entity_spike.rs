//! RFC-0041 slice-zero spike: one authored Lodestar-shaped relation lowered into the embedded DBSP
//! runtime. This is deliberately **not** the `entities.toml` product surface. It establishes the
//! compiler boundary before authoring, lifecycle and serving work are allowed to begin.
//!
//! The accepted statement contains the smallest useful v1 shape: a filter, exact integer arithmetic,
//! an inner equijoin and a `GROUP BY` over the declared key. DuckDB parses it and remains the reference
//! evaluator; the lowerer only admits this exact plan family. Anything wider belongs in slice one,
//! after this gate says the deployment model can carry the circuit at all.

use anyhow::{anyhow, bail, Context, Result};
use dbsp::utils::Tup2;
use dbsp::{IndexedZSetReader, OrdZSet, OutputHandle, RootCircuit, Runtime};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The canonical authored query used for the slice-zero Lodestar delegation spike.
pub const DELEGATION_SQL: &str = "SELECT d.indexer, d.delegator, SUM(d.amount + 0) AS delegated \
FROM delegations d JOIN indexers i ON d.indexer = i.indexer \
WHERE d.amount > 0 AND i.active = true GROUP BY d.indexer, d.delegator";

const SEP: char = '\u{1f}';

/// One canonical fact from the fixed corpus. `indexer` is the join key; the declared entity key is
/// `(indexer, delegator)`. Amount stays `i128`: float is not an accidental part of the first gate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DelegationFact {
    pub indexer: String,
    pub delegator: String,
    pub amount: i128,
}

/// One row from the joined indexer dimension. Changing `active` is represented as a retraction of
/// the old row followed by insertion of the new row, precisely the shape a reorg produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexerFact {
    pub indexer: String,
    pub active: bool,
}

/// One fixed-corpus input batch. The caller supplies weights so the same plan sees both `+1` facts
/// and `-1` retractions without a second rollback implementation.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Batch {
    pub delegations: Vec<(DelegationFact, i64)>,
    pub indexers: Vec<(IndexerFact, i64)>,
}

/// The only plan slice zero admits. Keeping this type explicit prevents a passing spike being
/// mistaken for a general DuckDB compiler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationPlan {
    pub key: [&'static str; 2],
    pub source_tables: BTreeSet<String>,
}

/// The finalized, manifest-bound input and DuckDB result for the actual Horizon workload. The
/// delegation positions are normalised from the older sealed event schema before they enter DBSP;
/// this keeps the circuit's input relation stable while making the raw source explicit.
pub struct HorizonFixture {
    pub delegations: Vec<(DelegationFact, i64)>,
    pub indexers: Vec<(IndexerFact, i64)>,
    pub expected: BTreeMap<String, i128>,
}

/// Reproducible measurements for one manifest-bound Horizon run. RSS names this process, which is
/// the conservative number for the embedded cursor: the compiler does not hide in a helper service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HorizonMeasurement {
    /// `fixture` includes DuckDB normalisation from sealed Parquet. `replay` applies the recorded
    /// weighted batches through the same entity ingestion boundary without touching DuckDB or RPC.
    pub source: &'static str,
    pub tape_content_address: Option<String>,
    pub declared_max_rows: usize,
    pub input_batches: usize,
    pub input_rows: usize,
    pub result_rows: usize,
    /// Compiling the SQL and building the circuit. A once-per-nest-load cost, and on a corpus this
    /// size it was **76% of the window the published rate divided by** (#837), so it is reported
    /// rather than folded in.
    pub setup_ms: u128,
    /// Folding the batches, and nothing else. This is what `input_rows_per_second` divides by.
    pub apply_ms: u128,
    /// Setup plus apply. Kept because a cold-start figure is worth having; it is simply not a
    /// throughput figure.
    pub elapsed_ms: u128,
    pub input_rows_per_second: u64,
    /// RSS with the circuit built and no rows applied - §9 criterion 3's "empty-circuit RSS".
    pub empty_circuit_rss_kb: Option<u64>,
    /// The high-water mark across the **apply phase alone**. It used to be the maximum across a
    /// window that also spanned the DuckDB normalise scan, which is why the published per-row cost
    /// was 74x the tape path's for the same relation (#837).
    pub circuit_peak_rss_kb: Option<u64>,
    /// The normalise scan's high-water mark, fixture path only, `None` on the tape path because no
    /// such scan happens there. **This is one-off scan headroom, not entity cost**, and it is a
    /// separate field so it cannot be subtracted across phases by accident again.
    pub normalise_peak_rss_kb: Option<u64>,
    pub rss_per_input_row: PerRowRss,
}

/// The per-row RSS estimate, or why there isn't one.
///
/// Deliberately not an `Option<u64>`. The previous shape produced a **missing JSON field** when
/// `peak.checked_sub(fixed)` underflowed - entirely possible when the sampler polls every 20 ms
/// against a 55 ms run - so a failed measurement and an unmeasured platform rendered identically,
/// and both rendered as nothing at all rather than as a problem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PerRowRss {
    Measured {
        bytes: u64,
    },
    /// The apply phase's peak came in below the empty-circuit sample. On a short run that means the
    /// sampler missed the peak, not that the circuit freed memory - either way it is not a figure.
    PeakBelowEmptyCircuit {
        empty_circuit_kb: u64,
        peak_kb: u64,
    },
    Unavailable {
        why: &'static str,
    },
}

/// A recorded, manifest-bound sequence of weighted entity inputs. This is deliberately distinct
/// from RFC-0039's RPC tape: the Horizon capture starts at sealed Parquet, and inventing RPC calls
/// for it would make the replay look like ordinary indexing while exercising no such source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HorizonTapeManifest {
    pub fixture_manifest_sha256: String,
    pub batch_rows: usize,
    pub content_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HorizonTape {
    pub manifest: HorizonTapeManifest,
    expected: BTreeMap<String, i128>,
    batches: Vec<Batch>,
}

#[derive(Deserialize)]
struct SegmentManifest {
    tables: BTreeMap<String, Vec<Segment>>,
}

#[derive(Deserialize)]
struct Segment {
    file: String,
}

/// Read the sealed Horizon snapshot identified by `segments/manifest.json`.
///
/// The capture predates the current `extension__…` nesting used by the checked-out Horizon views.
/// Its real source tables are `staking__tokens_delegated`,
/// `staking__delegated_tokens_withdrawn`, and the three `service__allocation_*` event streams.
/// Files are selected solely from the manifest, which makes a measurement reproducible even when a
/// neighbouring cursor has added newer Parquet files to the directory.
pub fn load_horizon_fixture(segments_dir: &Path) -> Result<HorizonFixture> {
    let manifest_path = segments_dir.join("manifest.json");
    let manifest: SegmentManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("read Horizon manifest {}", manifest_path.display()))?,
    )
    .context("parse Horizon segment manifest")?;
    let delegated = manifest_paths(&manifest, segments_dir, "staking__tokens_delegated")?;
    let withdrawn = manifest_paths(
        &manifest,
        segments_dir,
        "staking__delegated_tokens_withdrawn",
    )?;
    let mut allocations = manifest_paths(&manifest, segments_dir, "service__allocation_created")?;
    allocations.extend(manifest_paths(
        &manifest,
        segments_dir,
        "service__allocation_resized",
    )?);
    allocations.extend(manifest_paths(
        &manifest,
        segments_dir,
        "service__allocation_closed",
    )?);

    let conn = Connection::open_in_memory().context("open DuckDB for Horizon fixture")?;
    let sql = format!(
        "WITH deltas AS ( \
           SELECT lower(\"serviceProvider\") AS indexer, lower(delegator) AS delegator, \
                  TRY_CAST(tokens AS DECIMAL(38, 0)) AS delta FROM read_parquet([{}], union_by_name=true) \
           UNION ALL \
           SELECT lower(\"serviceProvider\"), lower(delegator), \
                  -TRY_CAST(tokens AS DECIMAL(38, 0)) FROM read_parquet([{}], union_by_name=true) \
         ), delegations AS ( \
           SELECT indexer, delegator, SUM(delta) AS delegated FROM deltas \
           WHERE delta IS NOT NULL GROUP BY indexer, delegator HAVING SUM(delta) <> 0 \
         ), indexers AS ( \
           SELECT DISTINCT lower(indexer) AS indexer FROM read_parquet([{}], union_by_name=true) \
         ) \
         SELECT d.indexer, d.delegator, CAST(d.delegated AS VARCHAR) AS delegated \
         FROM delegations d INNER JOIN indexers i ON d.indexer = i.indexer \
         WHERE d.delegated > 0 ORDER BY d.indexer, d.delegator",
        parquet_list(&delegated),
        parquet_list(&withdrawn),
        parquet_list(&allocations),
    );
    let mut statement = conn
        .prepare(&sql)
        .context("prepare Horizon delegation relation")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut delegations = Vec::new();
    let mut expected = BTreeMap::new();
    let mut indexers = BTreeSet::new();
    for row in rows {
        let (indexer, delegator, amount) = row?;
        let amount = amount
            .parse::<i128>()
            .context("parse exact Horizon delegation amount")?;
        indexers.insert(indexer.clone());
        expected.insert(format!("{indexer}{SEP}{delegator}"), amount);
        delegations.push((
            DelegationFact {
                indexer,
                delegator,
                amount,
            },
            1,
        ));
    }
    let indexers = indexers
        .into_iter()
        .map(|indexer| {
            (
                IndexerFact {
                    indexer,
                    active: true,
                },
                1,
            )
        })
        .collect();
    Ok(HorizonFixture {
        delegations,
        indexers,
        expected,
    })
}

impl HorizonTape {
    fn from_fixture(
        fixture: HorizonFixture,
        fixture_manifest_sha256: String,
        batch_rows: usize,
    ) -> Result<Self> {
        if batch_rows == 0 {
            bail!("Horizon tape batch_rows must be greater than zero")
        }
        let mut batches = vec![Batch {
            delegations: Vec::new(),
            indexers: fixture.indexers,
        }];
        batches.extend(
            fixture
                .delegations
                .chunks(batch_rows)
                .map(|delegations| Batch {
                    delegations: delegations.to_vec(),
                    indexers: Vec::new(),
                }),
        );
        Ok(Self {
            manifest: HorizonTapeManifest {
                fixture_manifest_sha256,
                batch_rows,
                content_address: None,
            },
            expected: fixture.expected,
            batches,
        })
    }

    pub fn content_address(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(&(
            &self.manifest.fixture_manifest_sha256,
            self.manifest.batch_rows,
            &self.expected,
            &self.batches,
        ))?;
        let mut hash = Sha256::new();
        hash.update(bytes);
        Ok(hex::encode(hash.finalize()))
    }

    pub fn write_dir(&mut self, dir: &Path) -> Result<()> {
        if dir.exists() {
            bail!(
                "refusing to overwrite Horizon tape directory {}",
                dir.display()
            )
        }
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create Horizon tape directory {}", dir.display()))?;
        self.manifest.content_address = Some(self.content_address()?);
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&self.manifest)?,
        )?;
        std::fs::write(dir.join("tape.json"), serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn read_dir(dir: &Path) -> Result<Self> {
        let mut tape: Self = serde_json::from_slice(
            &std::fs::read(dir.join("tape.json"))
                .with_context(|| format!("read Horizon tape {}", dir.display()))?,
        )
        .context("parse Horizon tape")?;
        let manifest: HorizonTapeManifest = serde_json::from_slice(
            &std::fs::read(dir.join("manifest.json"))
                .with_context(|| format!("read Horizon tape manifest {}", dir.display()))?,
        )
        .context("parse Horizon tape manifest")?;
        if tape.manifest.fixture_manifest_sha256 != manifest.fixture_manifest_sha256
            || tape.manifest.batch_rows != manifest.batch_rows
            || tape.manifest.content_address != manifest.content_address
        {
            bail!(
                "Horizon tape manifest and payload disagree at {}",
                dir.display()
            )
        }
        let claimed = tape
            .manifest
            .content_address
            .as_deref()
            .ok_or_else(|| anyhow!("Horizon tape {} has no content address", dir.display()))?;
        let actual = tape.content_address()?;
        if actual != claimed {
            bail!(
                "Horizon tape at {} does not match its own content address: manifest says {claimed}, tape hashes to {actual}",
                dir.display()
            )
        }
        // Keep the parsed external manifest as the source of the returned provenance.
        tape.manifest = manifest;
        Ok(tape)
    }
}

/// Normalise the sealed capture once and record the exact weighted batches later fed to the entity.
/// Replay deliberately starts after this one-off scan: it measures maintained-state ingestion, not
/// DuckDB's historical scan, and it cannot dial an RPC endpoint.
pub fn record_horizon_tape(segments_dir: &Path, dir: &Path, batch_rows: usize) -> Result<String> {
    let manifest_bytes = std::fs::read(segments_dir.join("manifest.json"))
        .with_context(|| format!("read Horizon manifest in {}", segments_dir.display()))?;
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(manifest_bytes);
    let mut tape = HorizonTape::from_fixture(
        load_horizon_fixture(segments_dir)?,
        hex::encode(hash.finalize()),
        batch_rows,
    )?;
    tape.write_dir(dir)?;
    tape.manifest
        .content_address
        .clone()
        .ok_or_else(|| anyhow!("Horizon tape write did not produce a content address"))
}

/// Load and apply the captured relation once, sampling RSS through the whole DuckDB-normalise plus
/// DBSP-apply path. `max_rows` is part of the measurement input, not an after-the-fact warning.
pub fn measure_horizon_fixture(segments_dir: &Path, max_rows: usize) -> Result<HorizonMeasurement> {
    // The normalise scan gets its own sampler, stopped before the circuit is built. It used to share
    // one window with the apply phase, and since `fixed` was sampled *after* the scan released, the
    // published "per input row" cost was to a first approximation the scan's transient high-water
    // mark divided by the entity's row count (#837).
    let normalise_sampler = RssSampler::start();
    let fixture = load_horizon_fixture(segments_dir)?;
    let normalise_peak_rss_kb = normalise_sampler.stop();
    measure_horizon_batches(
        "fixture",
        None,
        fixture.expected,
        vec![Batch {
            delegations: fixture.delegations,
            indexers: fixture.indexers,
        }],
        max_rows,
        normalise_peak_rss_kb,
    )
}

/// Replay a content-addressed normalised-input tape through the same weighted entity boundary used
/// by the fixture path. No DuckDB scan, network source, or alternate implementation participates.
pub fn measure_horizon_tape(dir: &Path, max_rows: usize) -> Result<HorizonMeasurement> {
    let tape = HorizonTape::read_dir(dir)?;
    let address = tape.manifest.content_address.clone();
    measure_horizon_batches(
        "replay",
        address,
        tape.expected,
        tape.batches,
        max_rows,
        // No DuckDB scan happens on this path, so there is no normalise peak to report. `None` says
        // that; a zero would read as "measured, and it was nothing".
        None,
    )
}

fn measure_horizon_batches(
    source: &'static str,
    tape_content_address: Option<String>,
    expected: BTreeMap<String, i128>,
    batches: Vec<Batch>,
    max_rows: usize,
    normalise_peak_rss_kb: Option<u64>,
) -> Result<HorizonMeasurement> {
    let started = Instant::now();
    let input_batches = batches.len();
    let input_rows = batches.iter().map(|b| b.delegations.len()).sum();
    // Setup is timed separately rather than being inside the divisor. `compile` opens a fresh
    // in-memory DuckDB to parse one constant string, which is a fixed cost paid once at nest load -
    // expressing it as rows per second turns a cold-start latency into something that looks
    // comparable to the ingest floor, in whichever direction the reader wants (#837).
    let plan = compile(DELEGATION_SQL)?;
    let mut spike = Spike::with_max_rows(&plan, max_rows)?;
    let setup_ms = started.elapsed().as_millis();

    // Sampled with the circuit built and nothing applied, which is what criterion 3 asks for.
    let empty_circuit_rss_kb = current_rss_kb();
    let apply_sampler = RssSampler::start();
    let apply_started = Instant::now();
    for batch in batches {
        spike.apply(batch)?;
    }
    let apply_ms = apply_started.elapsed().as_millis();
    if spike.rows() != expected {
        bail!("Horizon entity replay diverged from DuckDB reference")
    }
    let circuit_peak_rss_kb = apply_sampler.stop();
    let rss_per_input_row = per_row_rss(empty_circuit_rss_kb, circuit_peak_rss_kb, input_rows);
    Ok(HorizonMeasurement {
        source,
        tape_content_address,
        declared_max_rows: max_rows,
        input_batches,
        input_rows,
        result_rows: spike.rows().len(),
        setup_ms,
        apply_ms,
        elapsed_ms: started.elapsed().as_millis(),
        // Divided by the apply window, not the whole run. The two differ by more than 4x on a corpus
        // this size, and the published figure was the slower one.
        input_rows_per_second: rows_per_second(input_rows, apply_ms),
        empty_circuit_rss_kb,
        circuit_peak_rss_kb,
        normalise_peak_rss_kb,
        rss_per_input_row,
    })
}

struct RssSampler {
    stop: Arc<AtomicBool>,
    peak_kb: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak_kb = Arc::new(AtomicU64::new(current_rss_kb().unwrap_or_default()));
        let sampler_stop = Arc::clone(&stop);
        let sampler_peak = Arc::clone(&peak_kb);
        let thread = std::thread::spawn(move || {
            loop {
                if let Some(kb) = current_rss_kb() {
                    sampler_peak.fetch_max(kb, Ordering::Relaxed);
                }
                // Sample last, then check: the previous shape could stop without ever sampling the
                // final state. And 20 ms against a 55 ms run left the peak resting on two or three
                // samples (#837); reading `/proc/self/status` is cheap enough to do it far more
                // often, and on any platform where it is not, `current_rss_kb` refuses outright.
                if sampler_stop.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self {
            stop,
            peak_kb,
            thread: Some(thread),
        }
    }

    fn stop(mut self) -> Option<u64> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take()?.join().ok()?;
        let peak = self.peak_kb.load(Ordering::Relaxed);
        (peak > 0).then_some(peak)
    }
}

/// This process's RSS in KB, or nothing.
///
/// **`/proc` only, deliberately.** The previous fallback forked `ps` once per sample, which at fifty
/// samples a second perturbs the very thing being measured, and the RSS gate this feeds is a Linux
/// gate anyway. Refusing to produce a figure on a platform where it cannot be sampled cheaply is the
/// honest answer; producing a perturbed one and publishing it is how [[#837]] happened.
fn current_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .trim()
            .trim_end_matches("kB")
            .trim()
            .parse()
            .ok()
    })
}

/// Rows per second over the **apply** window.
///
/// Its own function so the divisor is a thing a test can pin. The published figure divided by setup
/// plus apply, and setup was 76% of it on the measured corpus (#837) - a cold-start latency wearing
/// a throughput figure's units, which then invites comparison against the >=10K events/sec ingest
/// floor in whichever direction the reader prefers.
fn rows_per_second(input_rows: usize, apply_ms: u128) -> u64 {
    (input_rows as u128 * 1_000 / apply_ms.max(1)) as u64
}

/// The per-row estimate, or a named reason there isn't one.
fn per_row_rss(empty: Option<u64>, peak: Option<u64>, input_rows: usize) -> PerRowRss {
    let (Some(empty_circuit_kb), Some(peak_kb)) = (empty, peak) else {
        return PerRowRss::Unavailable {
            why: "RSS sampling needs /proc/self/status; this platform cannot be sampled without \
                  perturbing the measurement",
        };
    };
    if input_rows == 0 {
        return PerRowRss::Unavailable {
            why: "no input rows to divide by",
        };
    }
    match peak_kb.checked_sub(empty_circuit_kb) {
        None => PerRowRss::PeakBelowEmptyCircuit {
            empty_circuit_kb,
            peak_kb,
        },
        Some(kb) => PerRowRss::Measured {
            bytes: kb * 1024 / input_rows as u64,
        },
    }
}

fn manifest_paths(
    manifest: &SegmentManifest,
    segments_dir: &Path,
    table: &str,
) -> Result<Vec<PathBuf>> {
    let segments = manifest
        .tables
        .get(table)
        .ok_or_else(|| anyhow!("Horizon fixture manifest has no {table} segments"))?;
    segments
        .iter()
        .map(|segment| {
            let file = Path::new(&segment.file);
            if file.components().count() != 1
                || file.extension().and_then(|e| e.to_str()) != Some("parquet")
            {
                bail!("unsafe Horizon manifest segment name {}", segment.file)
            }
            let path = segments_dir.join(file);
            if !path.is_file() {
                bail!(
                    "Horizon fixture is missing manifest segment {}",
                    path.display()
                )
            }
            Ok(path)
        })
        .collect()
}

fn parquet_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| format!("'{}'", path.display().to_string().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse with DuckDB's own serialised AST, then admit exactly the relation this spike can lower.
/// The textual equality is a *plan-family* guard after parsing, not a second parser: accepting a
/// syntactically similar query with different semantics would be worse than refusing it.
pub fn compile(sql: &str) -> Result<DelegationPlan> {
    let conn = Connection::open_in_memory().context("open DuckDB parser for RFC-0041 spike")?;
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let ast: String = conn
        .query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get(0)
        })
        .context("DuckDB could not parse authored entity SQL")?;
    let ast: Value = serde_json::from_str(&ast).context("DuckDB returned malformed SQL AST")?;
    if ast.get("error").and_then(Value::as_bool) == Some(true) {
        bail!("DuckDB refused authored entity SQL")
    }
    let mut tables = BTreeSet::new();
    ast_tables(&ast, &mut tables);
    let expected = BTreeSet::from(["delegations".to_string(), "indexers".to_string()]);
    if tables != expected || !is_delegation_aggregate(&ast) {
        bail!(
            "RFC-0041 slice-zero only lowers the Lodestar delegation aggregate; keep other SQL as a view until slice one"
        );
    }
    Ok(DelegationPlan {
        key: ["indexer", "delegator"],
        source_tables: tables,
    })
}

/// The slice-zero lowerer is intentionally a recogniser, not a second SQL parser. DuckDB supplies
/// syntax and this verifies the one relational shape for which the DBSP builder below has semantics.
/// Every predicate is structural so whitespace and formatting are immaterial, while a changed join
/// key, expression, filter, aggregate, or grouping cannot wander into the circuit by accident.
fn is_delegation_aggregate(ast: &Value) -> bool {
    let Some(node) = ast.pointer("/statements/0/node") else {
        return false;
    };
    if node.get("type").and_then(Value::as_str) != Some("SELECT_NODE")
        || node.get("modifiers").and_then(Value::as_array) != Some(&vec![])
        || node
            .get("group_sets")
            .and_then(Value::as_array)
            .is_none_or(|sets| sets != &[Value::Array(vec![Value::from(0), Value::from(1)])])
    {
        return false;
    }
    let Some(select) = node.get("select_list").and_then(Value::as_array) else {
        return false;
    };
    if select.len() != 3
        || !column(&select[0], "d", "indexer")
        || !column(&select[1], "d", "delegator")
        || !sum_amount_plus_zero(&select[2])
    {
        return false;
    }
    let Some(join) = node.get("from_table") else {
        return false;
    };
    if join.get("type").and_then(Value::as_str) != Some("JOIN")
        || join.get("join_type").and_then(Value::as_str) != Some("INNER")
        || join.pointer("/left/table_name").and_then(Value::as_str) != Some("delegations")
        || join.pointer("/left/alias").and_then(Value::as_str) != Some("d")
        || join.pointer("/right/table_name").and_then(Value::as_str) != Some("indexers")
        || join.pointer("/right/alias").and_then(Value::as_str) != Some("i")
        || !comparison(
            join.get("condition").unwrap_or(&Value::Null),
            "COMPARE_EQUAL",
            ("d", "indexer"),
            ("i", "indexer"),
        )
    {
        return false;
    }
    let Some(filters) = node.get("where_clause").and_then(|v| v.get("children")) else {
        return false;
    };
    let Some(filters) = filters.as_array() else {
        return false;
    };
    filters.len() == 2
        && positive_amount(&filters[0])
        && active_indexer(&filters[1])
        && node
            .get("group_expressions")
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups.len() == 2
                    && column(&groups[0], "d", "indexer")
                    && column(&groups[1], "d", "delegator")
            })
}

fn column(value: &Value, relation: &str, name: &str) -> bool {
    value.get("type").and_then(Value::as_str) == Some("COLUMN_REF")
        && value
            .get("column_names")
            .and_then(Value::as_array)
            .is_some_and(|columns| columns == &[Value::from(relation), Value::from(name)])
}

fn comparison(value: &Value, kind: &str, left: (&str, &str), right: (&str, &str)) -> bool {
    value.get("type").and_then(Value::as_str) == Some(kind)
        && column(value.get("left").unwrap_or(&Value::Null), left.0, left.1)
        && column(value.get("right").unwrap_or(&Value::Null), right.0, right.1)
}

fn positive_amount(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("COMPARE_GREATERTHAN")
        && column(value.get("left").unwrap_or(&Value::Null), "d", "amount")
        && integer(value.get("right").unwrap_or(&Value::Null)) == Some(0)
}

fn active_indexer(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("COMPARE_EQUAL")
        && column(value.get("left").unwrap_or(&Value::Null), "i", "active")
        && value.pointer("/right/cast_type/id").and_then(Value::as_str) == Some("BOOLEAN")
        && value
            .pointer("/right/child/value/value")
            .and_then(Value::as_str)
            == Some("t")
}

fn sum_amount_plus_zero(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("FUNCTION")
        && value
            .get("function_name")
            .and_then(Value::as_str)
            .is_some_and(|name| name.eq_ignore_ascii_case("sum"))
        && value
            .get("children")
            .and_then(Value::as_array)
            .is_some_and(|children| {
                children.len() == 1
                    && children[0].get("function_name").and_then(Value::as_str) == Some("+")
                    && children[0]
                        .get("children")
                        .and_then(Value::as_array)
                        .is_some_and(|addends| {
                            addends.len() == 2
                                && column(&addends[0], "d", "amount")
                                && integer(&addends[1]) == Some(0)
                        })
            })
}

fn integer(value: &Value) -> Option<i64> {
    value.pointer("/value/value").and_then(Value::as_i64)
}

fn ast_tables(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("BASE_TABLE") {
                if let Some(name) = map.get("table_name").and_then(Value::as_str) {
                    out.insert(name.to_ascii_lowercase());
                }
            }
            for child in map.values() {
                ast_tables(child, out);
            }
        }
        Value::Array(values) => values.iter().for_each(|v| ast_tables(v, out)),
        _ => {}
    }
}

type CircuitHandles = (
    (
        dbsp::IndexedZSetHandle<String, Tup2<String, i128>>,
        dbsp::IndexedZSetHandle<String, bool>,
    ),
    OutputHandle<OrdZSet<Tup2<String, i128>>>,
);

/// Embedded dynamic-plan runtime for the one admitted plan. The schema comes from the parsed plan;
/// the Rust representation is fixed for this vertical spike so no generated Rust, Cargo subprocess,
/// JVM or external compiler enters a nest load.
pub struct Spike {
    circuit: dbsp::DBSPHandle,
    delegations: dbsp::IndexedZSetHandle<String, Tup2<String, i128>>,
    indexers: dbsp::IndexedZSetHandle<String, bool>,
    output: OutputHandle<OrdZSet<Tup2<String, i128>>>,
    rows: BTreeMap<String, i128>,
    max_rows: usize,
    delegation_weights: BTreeMap<DelegationFact, i64>,
}

impl Spike {
    /// Construct the spike with a hard bound on live input facts. The maintained circuit retains
    /// its indexed inputs as well as its result, so bounding output rows alone would leave the
    /// expensive part free to grow. A compensated `-1` releases capacity before its replacement
    /// `+1` is admitted.
    pub fn with_max_rows(plan: &DelegationPlan, max_rows: usize) -> Result<Self> {
        if plan.key != ["indexer", "delegator"] {
            bail!("slice-zero plan has an unsupported key")
        }
        if max_rows == 0 {
            bail!("slice-zero entity max_rows must be greater than zero")
        }
        let (circuit, ((delegations, indexers), output)) = Runtime::init_circuit(1, build_circuit)
            .map_err(|e| anyhow!("build RFC-0041 DBSP spike: {e}"))?;
        Ok(Self {
            circuit,
            delegations,
            indexers,
            output,
            rows: BTreeMap::new(),
            max_rows,
            delegation_weights: BTreeMap::new(),
        })
    }

    pub fn apply(&mut self, batch: Batch) -> Result<()> {
        let next_weights = self.admit(&batch.delegations)?;
        let mut delegations = batch
            .delegations
            .into_iter()
            .map(|(d, weight)| Tup2(d.indexer, Tup2(Tup2(d.delegator, d.amount), weight)))
            .collect();
        let mut indexers = batch
            .indexers
            .into_iter()
            .map(|(i, weight)| Tup2(i.indexer, Tup2(i.active, weight)))
            .collect();
        self.delegations.append(&mut delegations);
        self.indexers.append(&mut indexers);
        self.circuit
            .transaction()
            .map_err(|e| anyhow!("RFC-0041 spike transaction: {e}"))?;
        self.delegation_weights = next_weights;

        let mut set = BTreeMap::new();
        let mut cleared = Vec::new();
        self.output.consolidate().iter().for_each(
            |(row, (), weight): (Tup2<String, i128>, (), i64)| {
                if weight > 0 {
                    set.insert(row.0.clone(), row.1);
                } else if weight < 0 {
                    cleared.push(row.0);
                }
            },
        );
        for (key, value) in &set {
            self.rows.insert(key.clone(), *value);
        }
        for key in cleared {
            if !set.contains_key(&key) {
                self.rows.remove(&key);
            }
        }
        Ok(())
    }

    pub fn rows(&self) -> BTreeMap<String, i128> {
        self.rows.clone()
    }

    fn admit(&self, changes: &[(DelegationFact, i64)]) -> Result<BTreeMap<DelegationFact, i64>> {
        let mut next = self.delegation_weights.clone();
        for (fact, weight) in changes {
            let total = next.get(fact).copied().unwrap_or_default() + weight;
            if total < 0 {
                bail!("RFC-0041 spike retracted a delegation not present in the admitted relation")
            }
            if total == 0 {
                next.remove(fact);
            } else {
                next.insert(fact.clone(), total);
            }
        }
        let live = next.values().try_fold(0usize, |rows, weight| {
            rows.checked_add(
                (*weight)
                    .try_into()
                    .map_err(|_| anyhow!("RFC-0041 spike received a negative delegation weight"))?,
            )
            .ok_or_else(|| anyhow!("RFC-0041 spike delegation row count overflowed usize"))
        })?;
        if live > self.max_rows {
            bail!(
                "RFC-0041 spike max_rows exceeded: {live} live input facts, declared bound {}",
                self.max_rows
            )
        }
        Ok(next)
    }
}

fn build_circuit(circuit: &mut RootCircuit) -> Result<CircuitHandles> {
    let (delegations, delegation_handle) =
        circuit.add_input_indexed_zset::<String, Tup2<String, i128>>();
    let (indexers, indexer_handle) = circuit.add_input_indexed_zset::<String, bool>();
    // Filter and exact arithmetic live inside the maintained plan, before the inner equijoin.
    let eligible = delegations.filter(|(_, Tup2(_, amount))| *amount > 0);
    let joined = eligible.join_index(&indexers, |indexer, Tup2(delegator, amount), active| {
        (*active).then(|| (Tup2(indexer.clone(), delegator.clone()), *amount))
    });
    let totals = joined.aggregate_linear(|amount: &i128| *amount);
    let output = totals
        .map(
            |(Tup2(indexer, delegator), amount): (&Tup2<String, String>, &i128)| {
                Tup2(format!("{indexer}{SEP}{delegator}"), *amount)
            },
        )
        .output();
    Ok(((delegation_handle, indexer_handle), output))
}

/// DuckDB reference result over a finalized canonical corpus. This is intentionally a separate
/// engine and execution path from the DBSP circuit; a shared fold would prove only that it shares
/// a mistake. Incremental weights belong to [`Spike::apply`]; callers must first reduce them to
/// their current `+1` facts before using this reference.
pub fn duckdb_reference(
    delegations: &[(DelegationFact, i64)],
    indexers: &[(IndexerFact, i64)],
) -> Result<BTreeMap<String, i128>> {
    let conn = Connection::open_in_memory()?;
    conn.execute_batch(
        "CREATE TABLE delegations(indexer VARCHAR, delegator VARCHAR, amount HUGEINT, weight BIGINT);\
         CREATE TABLE indexers(indexer VARCHAR, active BOOLEAN, weight BIGINT);",
    )?;
    for (d, weight) in delegations {
        if *weight != 1 {
            bail!("DuckDB reference accepts finalized +1 delegation facts only")
        }
        conn.execute(
            "INSERT INTO delegations VALUES (?, ?, ?, ?)",
            duckdb::params![d.indexer, d.delegator, d.amount.to_string(), *weight],
        )?;
    }
    for (i, weight) in indexers {
        if *weight != 1 {
            bail!("DuckDB reference accepts finalized +1 indexer facts only")
        }
        conn.execute(
            "INSERT INTO indexers VALUES (?, ?, ?)",
            duckdb::params![i.indexer, i.active, *weight],
        )?;
    }
    let sql = "WITH d AS (SELECT indexer, delegator, amount FROM delegations WHERE weight = 1), \
               i AS (SELECT indexer, active FROM indexers WHERE weight = 1) \
               SELECT d.indexer, d.delegator, SUM(d.amount + 0)::VARCHAR AS delegated \
               FROM d JOIN i ON d.indexer = i.indexer \
               WHERE d.amount > 0 AND i.active = true GROUP BY d.indexer, d.delegator";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut out = BTreeMap::new();
    for row in rows {
        let (indexer, delegator, amount) = row?;
        out.insert(format!("{indexer}{SEP}{delegator}"), amount.parse()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    type WeightedDelegations = Vec<(DelegationFact, i64)>;
    type WeightedIndexers = Vec<(IndexerFact, i64)>;
    type Corpus = (WeightedDelegations, WeightedIndexers);

    fn corpus() -> Corpus {
        (
            vec![
                (
                    DelegationFact {
                        indexer: "i1".into(),
                        delegator: "a".into(),
                        amount: 7,
                    },
                    1,
                ),
                (
                    DelegationFact {
                        indexer: "i1".into(),
                        delegator: "a".into(),
                        amount: 5,
                    },
                    1,
                ),
                (
                    DelegationFact {
                        indexer: "i1".into(),
                        delegator: "b".into(),
                        amount: -3,
                    },
                    1,
                ),
                (
                    DelegationFact {
                        indexer: "i2".into(),
                        delegator: "c".into(),
                        amount: 11,
                    },
                    1,
                ),
            ],
            vec![
                (
                    IndexerFact {
                        indexer: "i1".into(),
                        active: true,
                    },
                    1,
                ),
                (
                    IndexerFact {
                        indexer: "i2".into(),
                        active: false,
                    },
                    1,
                ),
            ],
        )
    }

    fn fixture() -> HorizonFixture {
        let (delegations, indexers) = corpus();
        HorizonFixture {
            expected: duckdb_reference(&delegations, &indexers).unwrap(),
            delegations,
            indexers,
        }
    }

    #[test]
    fn recorded_entity_input_tape_replays_the_same_weighted_ingest_boundary() {
        let parent = tempfile::tempdir().unwrap();
        let tape_dir = parent.path().join("delegations");
        let mut tape = HorizonTape::from_fixture(fixture(), "fixture-sha".into(), 2).unwrap();
        let address = tape.content_address().unwrap();
        tape.write_dir(&tape_dir).unwrap();

        let replayed = HorizonTape::read_dir(&tape_dir).unwrap();
        assert_eq!(
            replayed.manifest.content_address.as_deref(),
            Some(address.as_str())
        );
        assert_eq!(
            replayed.batches.len(),
            3,
            "dimension batch plus two input batches"
        );

        let report = measure_horizon_tape(&tape_dir, 1_000).unwrap();
        assert_eq!(report.source, "replay");
        assert_eq!(
            report.tape_content_address.as_deref(),
            Some(address.as_str())
        );
        assert_eq!(report.input_rows, 4);
        assert_eq!(report.result_rows, 1);
    }

    #[test]
    fn tampered_entity_input_tape_is_refused_before_replay() {
        let parent = tempfile::tempdir().unwrap();
        let tape_dir = parent.path().join("delegations");
        let mut tape = HorizonTape::from_fixture(fixture(), "fixture-sha".into(), 2).unwrap();
        tape.write_dir(&tape_dir).unwrap();

        tape.batches[1].delegations[0].0.amount = 8;
        std::fs::write(
            tape_dir.join("tape.json"),
            serde_json::to_vec_pretty(&tape).unwrap(),
        )
        .unwrap();
        let err = HorizonTape::read_dir(&tape_dir).unwrap_err().to_string();
        assert!(
            err.contains("does not match its own content address"),
            "{err}"
        );
    }

    #[test]
    fn parsed_lodestar_shape_matches_duckdb_byte_for_byte() {
        let plan = compile(DELEGATION_SQL).unwrap();
        let (delegations, indexers) = corpus();
        let mut spike = Spike::with_max_rows(&plan, 1_000).unwrap();
        spike
            .apply(Batch {
                delegations: delegations.clone(),
                indexers: indexers.clone(),
            })
            .unwrap();
        assert_eq!(
            spike.rows(),
            duckdb_reference(&delegations, &indexers).unwrap()
        );
        assert_eq!(spike.rows().get("i1\u{1f}a"), Some(&12));
        assert!(!spike.rows().contains_key("i2\u{1f}c"));
    }

    #[test]
    fn retraction_converges_to_a_clean_replay() {
        let plan = compile(DELEGATION_SQL).unwrap();
        let (delegations, indexers) = corpus();
        let removed = delegations[1].0.clone();
        let replacement = DelegationFact {
            indexer: "i1".into(),
            delegator: "a".into(),
            amount: 9,
        };
        let mut reorged = Spike::with_max_rows(&plan, 1_000).unwrap();
        reorged
            .apply(Batch {
                delegations: delegations.clone(),
                indexers: indexers.clone(),
            })
            .unwrap();
        reorged
            .apply(Batch {
                delegations: vec![(removed, -1), (replacement.clone(), 1)],
                indexers: vec![],
            })
            .unwrap();

        let mut clean = Spike::with_max_rows(&plan, 1_000).unwrap();
        let mut canonical = delegations;
        canonical[1] = (replacement, 1);
        clean
            .apply(Batch {
                delegations: canonical,
                indexers,
            })
            .unwrap();
        assert_eq!(reorged.rows(), clean.rows());
    }

    #[test]
    fn parser_refuses_a_nearby_but_unlowered_statement() {
        let err = compile("SELECT indexer, count(*) FROM delegations GROUP BY indexer")
            .unwrap_err()
            .to_string();
        assert!(err.contains("slice-zero"));
    }

    #[test]
    fn ast_lowerer_accepts_formatting_but_refuses_semantic_mutations() {
        let formatted = "SELECT d.indexer, d.delegator, SUM(d.amount + 0) AS delegated\n\
                         FROM delegations d JOIN indexers i ON d.indexer = i.indexer\n\
                         WHERE d.amount > 0 AND i.active = true\n\
                         GROUP BY d.indexer, d.delegator";
        assert!(compile(formatted).is_ok());
        for mutated in [
            DELEGATION_SQL.replace("d.amount + 0", "d.amount + 1"),
            DELEGATION_SQL.replace("d.indexer = i.indexer", "d.delegator = i.indexer"),
            DELEGATION_SQL.replace("i.active = true", "i.active = false"),
        ] {
            assert!(
                compile(&mutated).is_err(),
                "mutated SQL was admitted: {mutated}"
            );
        }
    }

    /// **The failure that used to render as nothing.** `peak.checked_sub(fixed)` returned `None` on
    /// underflow and the field simply vanished from the JSON, so "the sampler missed the peak" and
    /// "this platform cannot be sampled" and "here is your answer" were three states with two
    /// renderings (#837).
    #[test]
    fn every_per_row_outcome_names_itself_in_the_json() {
        let cases = [
            (
                per_row_rss(Some(1_000), Some(2_000), 500),
                "measured",
                PerRowRss::Measured { bytes: 2048 },
            ),
            (
                // Peak below the empty-circuit sample: on a short run the sampler missed it.
                per_row_rss(Some(2_000), Some(1_000), 500),
                "peak_below_empty_circuit",
                PerRowRss::PeakBelowEmptyCircuit {
                    empty_circuit_kb: 2_000,
                    peak_kb: 1_000,
                },
            ),
            (
                per_row_rss(None, Some(1_000), 500),
                "unavailable",
                PerRowRss::Unavailable {
                    why: "RSS sampling needs /proc/self/status; this platform cannot be sampled \
                          without perturbing the measurement",
                },
            ),
            (
                per_row_rss(Some(1_000), Some(2_000), 0),
                "unavailable",
                PerRowRss::Unavailable {
                    why: "no input rows to divide by",
                },
            ),
        ];

        for (got, status, want) in cases {
            assert_eq!(got, want);
            let json: serde_json::Value = serde_json::to_value(&got).unwrap();
            assert_eq!(
                json.get("status").and_then(|v| v.as_str()),
                Some(status),
                "every outcome carries a status, so none of them can be an absent field: {json}"
            );
        }
    }

    /// **The defect #837 is actually about.** The normalise scan's high-water mark must not reach the
    /// per-row figure. It used to, because one sampler spanned the scan and the apply while the
    /// baseline was taken after the scan had released - which is how the same relation got a
    /// published per-row cost 74x the tape path's, and how a 2 GB budget that holds ~800,000 rows
    /// read as one that exhausts at ~11,400.
    ///
    /// **The property is which fields the figure derives from, not that two runs agree.** The first
    /// version of this test compared a run carrying a scan peak against one without, and failed on
    /// Linux CI for a reason that had nothing to do with the scan: RSS is process-global, so the
    /// second measurement starts from the first's raised baseline and reports a smaller delta. Two
    /// sequential in-process measurements can never be equal, and a test that needs them to be is
    /// testing the allocator.
    ///
    /// **Linux only, and absent rather than skipped elsewhere.** `current_rss_kb` reads
    /// `/proc/self/status` and refuses on anything else, so on macOS the figure is `Unavailable`
    /// however the arithmetic is written. A test that passes vacuously is worse than one that is not
    /// there: it was still green with the scan peak folded straight back in. This is the gate's
    /// platform anyway.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_per_row_figure_derives_from_the_circuit_fields_alone() {
        let (delegations, indexers) = corpus();
        let plan = compile(DELEGATION_SQL).unwrap();
        let mut reference = Spike::with_max_rows(&plan, 1_000).unwrap();
        reference
            .apply(Batch {
                delegations: delegations.clone(),
                indexers: indexers.clone(),
            })
            .unwrap();
        let expected = reference.rows();

        // A scan peak of 4 GB - far beyond anything the circuit could account for.
        let scan_peak = 4 * 1024 * 1024;
        let m = measure_horizon_batches(
            "fixture",
            None,
            expected,
            vec![Batch {
                delegations,
                indexers,
            }],
            1_000,
            Some(scan_peak),
        )
        .unwrap();

        assert_eq!(
            m.normalise_peak_rss_kb,
            Some(scan_peak),
            "it is still reported, as its own field"
        );
        assert!(
            matches!(m.rss_per_input_row, PerRowRss::Measured { .. }),
            "nothing is being asserted without a measurement: {:?}",
            m.rss_per_input_row
        );
        assert_eq!(
            m.rss_per_input_row,
            per_row_rss(m.empty_circuit_rss_kb, m.circuit_peak_rss_kb, m.input_rows),
            "the published figure must be exactly what the two circuit fields give, with the 4 GB \
             scan peak playing no part in it"
        );
        assert!(
            m.circuit_peak_rss_kb.is_some_and(|kb| kb < scan_peak),
            "the circuit peak must be the circuit's, not the scan's: {:?}",
            m.circuit_peak_rss_kb
        );
    }

    /// The rate divides by the apply window. Dividing by setup-plus-apply is what turned a
    /// 152,000 rows/sec fold into a published 37,000 (#837).
    #[test]
    fn the_rate_divides_by_the_apply_window_alone() {
        // 876 rows folded in 6 ms, after 18 ms of compiling and circuit-building.
        assert_eq!(rows_per_second(876, 6), 146_000);
        assert_eq!(
            rows_per_second(876, 24),
            36_500,
            "this is what including setup used to report for the same fold"
        );
        // A fold too fast to time is not an infinite rate, and must not divide by zero.
        assert_eq!(rows_per_second(876, 0), 876_000);
    }

    #[test]
    #[ignore = "requires the sealed Horizon capture outside Git"]
    fn captured_horizon_relation_matches_embedded_dbsp() {
        let segments = std::env::var("NUTHATCH_HORIZON_FIXTURE")
            .expect("set NUTHATCH_HORIZON_FIXTURE to the captured Horizon segments directory");
        let measurement = measure_horizon_fixture(Path::new(&segments), 1_000).unwrap();
        assert!(measurement.input_rows > 0);
        eprintln!(
            "Horizon fixture: {} input rows, {} results, setup {} ms + apply {} ms, \
             empty-circuit RSS {:?} KiB, circuit peak {:?} KiB, normalise peak {:?} KiB, \
             per input row {:?}",
            measurement.input_rows,
            measurement.result_rows,
            measurement.setup_ms,
            measurement.apply_ms,
            measurement.empty_circuit_rss_kb,
            measurement.circuit_peak_rss_kb,
            measurement.normalise_peak_rss_kb,
            measurement.rss_per_input_row,
        );
        assert_eq!(measurement.result_rows, measurement.input_rows);
    }

    #[test]
    fn declared_max_rows_is_an_admission_bound_not_a_post_hoc_warning() {
        let plan = compile(DELEGATION_SQL).unwrap();
        let mut spike = Spike::with_max_rows(&plan, 1).unwrap();
        let (delegations, indexers) = corpus();
        let err = spike
            .apply(Batch {
                delegations: delegations[..2].to_vec(),
                indexers,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_rows exceeded"));
        assert!(spike.rows().is_empty());

        // A retraction is admitted before its replacement, so a reorg at the bound does not need
        // one extra slot merely because the cursor presents both changes in one batch.
        let fact = DelegationFact {
            indexer: "i1".into(),
            delegator: "a".into(),
            amount: 7,
        };
        spike
            .apply(Batch {
                delegations: vec![(fact.clone(), 1)],
                indexers: vec![(
                    IndexerFact {
                        indexer: "i1".into(),
                        active: true,
                    },
                    1,
                )],
            })
            .unwrap();
        let replacement = DelegationFact { amount: 9, ..fact };
        spike
            .apply(Batch {
                delegations: vec![
                    (
                        DelegationFact {
                            indexer: "i1".into(),
                            delegator: "a".into(),
                            amount: 7,
                        },
                        -1,
                    ),
                    (replacement, 1),
                ],
                indexers: vec![],
            })
            .unwrap();
        assert_eq!(spike.rows().get("i1\u{1f}a"), Some(&9));
    }

    #[test]
    fn duckdb_oracle_refuses_deltas_instead_of_ignoring_them() {
        let (mut delegations, indexers) = corpus();
        delegations[0].1 = -1;
        let err = duckdb_reference(&delegations, &indexers)
            .unwrap_err()
            .to_string();
        assert!(err.contains("finalized +1"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(4))]

        /// Each replacement is a compensated `-1`/`+1` pair. Compare after every step with a
        /// fresh DuckDB execution over the current canonical facts, including indexer activity.
        #[test]
        fn randomized_replacements_match_fresh_duckdb(
            steps in prop::collection::vec((0usize..8, -20i128..21, any::<bool>()), 1..48)
        ) {
            let plan = compile(DELEGATION_SQL).unwrap();
            let mut spike = Spike::with_max_rows(&plan, 1_000).unwrap();
            let mut current = BTreeMap::<usize, i128>::new();
            let mut active = true;
            spike.apply(Batch {
                delegations: vec![],
                indexers: vec![(IndexerFact { indexer: "i1".into(), active }, 1)],
            }).unwrap();

            for (slot, amount, next_active) in steps {
                let delegator = format!("d{slot}");
                let mut delegations = Vec::new();
                if let Some(old) = current.insert(slot, amount) {
                    delegations.push((DelegationFact {
                        indexer: "i1".into(),
                        delegator: delegator.clone(),
                        amount: old,
                    }, -1));
                }
                delegations.push((DelegationFact {
                    indexer: "i1".into(),
                    delegator,
                    amount,
                }, 1));

                let indexers = if active == next_active {
                    vec![]
                } else {
                    let changes = vec![
                        (IndexerFact { indexer: "i1".into(), active }, -1),
                        (IndexerFact { indexer: "i1".into(), active: next_active }, 1),
                    ];
                    active = next_active;
                    changes
                };
                spike.apply(Batch { delegations, indexers }).unwrap();

                let canonical = current.iter().map(|(slot, amount)| {
                    (DelegationFact {
                        indexer: "i1".into(),
                        delegator: format!("d{slot}"),
                        amount: *amount,
                    }, 1)
                }).collect::<Vec<_>>();
                let reference = duckdb_reference(&canonical, &[(IndexerFact {
                    indexer: "i1".into(),
                    active,
                }, 1)]).unwrap();
                prop_assert_eq!(spike.rows(), reference);
            }
        }
    }
}
