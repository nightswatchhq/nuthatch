//! Burrmill behind [`crate::engine`]: the second engine of shadow mode (RFC-0044 Amendment 2,
//! phase 2b), or the only one when `NUTHATCH_ENGINE=burrmill` (phase 3a). Compiled only with the
//! `shadow-burrmill` feature; chosen by [`crate::analytics::install_engine`].
//!
//! A session is one `burrmill::Engine` opened empty, with the tables the policy code binds registered
//! as it binds them: the same segment list, the same hot rows, the same declared columns and window
//! DuckDB is given. The parser role (`serialize_sql`), the DuckDB plan walk (`cold_scan_operators`)
//! and the DuckDB catalogue text (`view_definitions`) are not Burrmill's to answer and are refused;
//! the shadow session only ever asks the primary for those.

use crate::engine::{Collected, Died, Engine, FactWindow, Interrupt, Session};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) struct BurrmillEngine;

impl Engine for BurrmillEngine {
    fn open(&self, _dir: &Path) -> Result<Box<dyn Session>> {
        Ok(Box::new(BurrmillSession::new()?))
    }

    fn open_bare(&self) -> Result<Box<dyn Session>> {
        Ok(Box::new(BurrmillSession::new()?))
    }
}

/// Put Burrmill beside DuckDB on every nest this process serves. Once per process.
/// Records go to the log, and also as JSON lines to the file `NUTHATCH_SHADOW_LOG` names, when it
/// names one; so do the running counts of every statement shadowed.
pub fn enable_shadow() -> Result<()> {
    install_pair(false)
}

/// The same pair the other way round: Burrmill is served and DuckDB checks behind it.
pub fn enable_checked() -> Result<()> {
    install_pair(true)
}

fn install_pair(burrmill_serves: bool) -> Result<()> {
    use crate::engine_shadow::{both_sinks, file_sink, log_sink, ShadowEngine};
    let sink = match std::env::var_os("NUTHATCH_SHADOW_LOG") {
        Some(path) => both_sinks(log_sink(), file_sink(Path::new(&path))?),
        None => log_sink(),
    };
    let engine = if burrmill_serves {
        ShadowEngine::new(
            Box::new(BurrmillEngine),
            Box::new(crate::engine_duck::DuckEngine),
            sink,
        )
        .reversed()
    } else {
        ShadowEngine::new(
            Box::new(crate::engine_duck::DuckEngine),
            Box::new(BurrmillEngine),
            sink,
        )
    };
    crate::engine_shadow::install(match std::env::var_os("NUTHATCH_SHADOW_LOG") {
        Some(path) => engine.with_tally_in(Path::new(&path))?,
        None => engine,
    })
}

pub(crate) struct BurrmillSession {
    engine: Mutex<burrmill::Engine>,
    /// Hot rows staged by `load_hot`, bound by the next `bind_facts` for that table.
    hot: Mutex<HashMap<String, Vec<Value>>>,
    /// Held for the session's life, as DuckDB's is; removed on drop and swept by pid after a crash.
    _spill: crate::engine_duck::SpillDir,
    /// Each view's `CREATE VIEW` text as defined, for the integrity sweep's walk through views.
    views: Mutex<std::collections::BTreeMap<String, String>>,
    /// The declared columns of each maintained relation `load_relation` staged, so one with no rows
    /// still binds (#1598). Burrmill types columns by name, so these carry names only.
    relations: Mutex<HashMap<String, Vec<(String, String)>>>,
}

impl BurrmillSession {
    fn new() -> Result<Self> {
        let spill = crate::engine_duck::new_spill_dir()?;
        let budget = budget(&crate::analytics_budget::from_env(), &spill.0);
        #[allow(unused_mut)]
        let mut engine = burrmill::Engine::open_empty_budgeted(budget).map_err(engine_err)?;
        #[cfg(feature = "graph")]
        crate::analytics_scalars::register_burrmill(&mut engine);
        Ok(Self {
            engine: Mutex::new(engine),
            hot: Mutex::new(HashMap::new()),
            _spill: spill,
            views: Mutex::new(std::collections::BTreeMap::new()),
            relations: Mutex::new(HashMap::new()),
        })
    }

    fn engine(&self) -> std::sync::MutexGuard<'_, burrmill::Engine> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// The walls `analytics_budget` sets for DuckDB, as Burrmill's budget.
fn budget(cfg: &crate::analytics_budget::AnalyticsConfig, spill: &Path) -> burrmill::Budget {
    burrmill::Budget {
        memory_bytes: (cfg.memory_limit_mb as usize) << 20,
        threads: cfg.threads.max(1) as usize,
        spill: Some((
            spill.to_path_buf(),
            crate::analytics_budget::spill_cap_bytes(cfg) as _,
        )),
    }
}

fn engine_err(e: burrmill::BurrmillError) -> anyhow::Error {
    anyhow!("{e}")
}

/// Refused before running, or died running: the split the integrity sweep depends on.
fn died(e: burrmill::BurrmillError) -> Died {
    use burrmill::BurrmillError::*;
    match e {
        NotAllowed(_) | Parse(_) | Plan(_) | NoSegments(_) => Died::Binding(engine_err(e)),
        _ => Died::Executing(engine_err(e)),
    }
}

fn sized(files: &[PathBuf]) -> Result<Vec<(PathBuf, u64)>> {
    files
        .iter()
        .map(|p| {
            let len = std::fs::metadata(p)
                .with_context(|| format!("stat {}", p.display()))?
                .len();
            Ok((p.clone(), len))
        })
        .collect()
}

/// Burrmill's token: the statement stops at its next scan batch, since a DataFusion join yields
/// nothing above the scan until it is done.
struct Cancel(burrmill::CancelToken);

impl Interrupt for Cancel {
    fn interrupt(&self) {
        self.0.cancel();
    }
}

impl Session for BurrmillSession {
    fn execute(&self, sql: &str) -> Result<()> {
        use sqlparser::ast::{ObjectType, Statement};
        if let (Some(name), Some(body)) = (
            crate::analytics::view_name(sql),
            crate::analytics::view_body(sql),
        ) {
            self.engine()
                .register_view(&name, body)
                .map_err(engine_err)?;
            self.views
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(name, sql.to_string());
            return Ok(());
        }
        // The folds path's statements for effect, and nothing else: a statement is still read-only.
        let stmts =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::DuckDbDialect {}, sql)?;
        // In order, as DuckDB runs a batch: each takes effect before the next, none is atomic.
        let mut engine = self.engine();
        for stmt in &stmts {
            match stmt {
                Statement::CreateTable(ct) if ct.query.is_some() => {
                    let name = ct.name.to_string().trim_matches('"').to_string();
                    let query = ct.query.as_ref().expect("checked").to_string();
                    engine.create_table_as(&name, &query).map_err(engine_err)?;
                }
                Statement::Drop {
                    object_type: ObjectType::View | ObjectType::Table,
                    names,
                    ..
                } => {
                    for n in names {
                        let n = n.to_string().trim_matches('"').to_string();
                        engine.drop_relation(&n);
                        self.views
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .remove(&n);
                    }
                }
                Statement::StartTransaction { .. } => engine.begin().map_err(engine_err)?,
                Statement::Commit { .. } => engine.commit().map_err(engine_err)?,
                Statement::Rollback { .. } => engine.rollback().map_err(engine_err)?,
                _ => return Err(anyhow!("Burrmill runs no `{stmt}` for effect")),
            }
        }
        Ok(())
    }

    fn write_parquet(&self, table: &str, path: &Path) -> Result<()> {
        self.engine()
            .write_parquet(&format!("SELECT * FROM \"{table}\" ORDER BY ALL"), path)
            .map_err(engine_err)?;
        Ok(())
    }

    fn load_parquet(&self, table: &str, select: &str, path: &Path) -> Result<()> {
        const LOADING: &str = "__checkpoint_load";
        let mut engine = self.engine();
        engine.load_parquet(LOADING, path).map_err(engine_err)?;
        let made = engine.create_table_as(table, &format!("SELECT {select} FROM \"{LOADING}\""));
        engine.drop_relation(LOADING);
        made.map_err(engine_err)?;
        Ok(())
    }

    fn collect(&self, sql: &str, cap: Option<usize>) -> Result<Collected, Died> {
        // The same two caps `engine_duck` applies, so the shadow truncates where the primary does.
        let hard = cap.map(|c| c + 1);
        let byte_cap = cap.map(|_| crate::engine::SQL_MAX_RESULT_BYTES);
        let mut bytes = 0usize;
        let mut out = Vec::new();
        let mut over = false;
        let mut columns: Vec<String> = Vec::new();
        let engine = self.engine();
        let r = engine.sql_for_each(sql, |batch| {
            if columns.is_empty() {
                columns = batch
                    .schema()
                    .fields()
                    .iter()
                    .map(|f| f.name().clone())
                    .collect();
            }
            for row in burrmill::df::encode::rows(&batch)? {
                if byte_cap.is_some() {
                    bytes += row
                        .as_object()
                        .map(|o| {
                            o.iter()
                                .map(|(k, v)| k.len() + crate::engine::value_bytes(v))
                                .sum::<usize>()
                        })
                        .unwrap_or(0);
                }
                out.push(row);
                if hard.is_some_and(|h| out.len() >= h) || byte_cap.is_some_and(|max| bytes >= max)
                {
                    over = true;
                    return Err(burrmill::BurrmillError::LimitExceeded("cap".into()));
                }
            }
            Ok(())
        });
        let truncated = match r {
            Ok(()) => false,
            Err(_) if over => true,
            Err(e) => return Err(died(e)),
        };
        Ok(Collected {
            rows: out,
            columns,
            truncated,
        })
    }

    fn for_each_row(&self, sql: &str, f: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
        let engine = self.engine();
        let mut failed: Option<anyhow::Error> = None;
        let r = engine.sql_for_each(sql, |batch| {
            let names: Vec<String> = batch
                .schema()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
            for row in burrmill::df::encode::rows(&batch)? {
                let cells: Vec<Value> = names
                    .iter()
                    .map(|n| row.get(n).cloned().unwrap_or(Value::Null))
                    .collect();
                if let Err(e) = f(&cells) {
                    failed = Some(e);
                    return Err(burrmill::BurrmillError::Cancelled);
                }
            }
            Ok(())
        });
        match (r, failed) {
            (_, Some(e)) => Err(e),
            (Ok(()), None) => Ok(()),
            (Err(e), None) => Err(engine_err(e)),
        }
    }

    fn one_value(&self, sql: &str) -> Result<Value> {
        let out = self.collect(sql, Some(1))?;
        let first = out.columns.first();
        out.rows
            .first()
            .and_then(|r| first.and_then(|c| r.get(c)).cloned())
            .ok_or_else(|| anyhow!("no rows"))
    }

    /// Through Arrow IPC, because Burrmill's arrow is not nuthatch's.
    fn query_arrow(&self, sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        let ipc = self.engine().sql_ipc(sql).map_err(engine_err)?;
        let reader = arrow::ipc::reader::StreamReader::try_new(std::io::Cursor::new(ipc), None)?;
        Ok(reader.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// From the plan, as DuckDB's prepare gives them: a result with no rows has no batch to read.
    fn column_names(&self, sql: &str) -> Result<Vec<String>> {
        Ok(self
            .engine()
            .describe(sql)
            .map_err(engine_err)?
            .into_iter()
            .map(|(name, _)| name)
            .collect())
    }

    fn describe(&self, sql: &str) -> Result<Vec<(String, String)>> {
        self.engine().describe(sql).map_err(engine_err)
    }

    fn has_relation(&self, name: &str) -> bool {
        self.engine().has_table(name)
    }

    fn relations(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .engine()
            .visible_tables()
            .into_iter()
            .map(|t| t.to_ascii_lowercase())
            .collect())
    }

    fn view_definitions(&self) -> Option<Vec<(String, String)>> {
        let views = self.views.lock().unwrap_or_else(|p| p.into_inner());
        Some(views.iter().map(|(n, s)| (n.clone(), s.clone())).collect())
    }

    fn canonical_plan(&self, sql: &str) -> Option<String> {
        burrmill::inspect::canonical(sql)
    }

    fn engine_version(&self) -> String {
        burrmill::ENGINE.to_string()
    }

    fn table_refs(&self, sql: &str) -> Option<(BTreeSet<String>, BTreeSet<String>)> {
        Some((
            burrmill::inspect::base_tables(sql)?,
            burrmill::inspect::refs(sql)?.functions,
        ))
    }

    fn serialize_sql(&self, _sql: &str) -> Result<Value> {
        Err(anyhow!("the parser role is DuckDB's until phase 2 proper"))
    }

    /// Burrmill's own walk, `inspect::reach`: sqlparser's AST under nuthatch's allowlist and
    /// reachability rules, failing closed. A parse it cannot make is `None`, as DuckDB's is.
    fn reach(&self, sql: &str) -> Option<Result<(BTreeSet<String>, bool)>> {
        match burrmill::inspect::reach(sql) {
            Ok(r) => Some(Ok((r.tables, r.surveys))),
            Err(burrmill::BurrmillError::Parse(_)) => None,
            Err(e) => Some(Err(engine_err(e))),
        }
    }

    fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
        Arc::new(Cancel(self.engine().cancel_token()))
    }

    fn cold_scan_operators(&self, sql: &str) -> Result<u64> {
        self.engine()
            .parquet_scans(sql)
            .map_err(|e| crate::analytics::unboundable(e.to_string()))
    }

    fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()> {
        self.hot.lock().unwrap_or_else(|p| p.into_inner()).insert(
            table.to_string(),
            rows.iter().map(|r| (*r).clone()).collect(),
        );
        Ok(())
    }

    fn load_relation(
        &self,
        table: &str,
        cols: &[(String, &'static str)],
        rows: &[&Value],
    ) -> Result<()> {
        let declared = cols
            .iter()
            .map(|(c, _)| (c.clone(), "string".into()))
            .collect();
        self.relations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(table.to_string(), declared);
        self.load_hot(table, rows)
    }

    fn bind_facts(
        &self,
        table: &str,
        cols: &[(String, String)],
        sealed: &[PathBuf],
        hot: bool,
        window: FactWindow,
    ) -> Result<bool> {
        let staged;
        let cols = if cols.is_empty() && hot {
            staged = self
                .relations
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(table)
                .cloned()
                .unwrap_or_default();
            staged.as_slice()
        } else {
            cols
        };
        let hot_rows: Vec<Value> = if hot {
            self.hot
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(table)
                .cloned()
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if sealed.is_empty() && hot_rows.is_empty() && cols.is_empty() {
            return Ok(false);
        }
        self.engine()
            .register_facts(
                table,
                cols,
                sized(sealed)?,
                &hot_rows,
                (window.after, window.through),
            )
            .map_err(engine_err)?;
        Ok(true)
    }

    fn segment_binds(&self, path: &Path) -> Result<()> {
        let f = std::fs::File::open(path)?;
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f)
            .with_context(|| format!("{} will not bind", path.display()))?;
        Ok(())
    }

    fn file_schema(&self, path: &Path) -> Option<Vec<(String, String)>> {
        let f = std::fs::File::open(path).ok()?;
        let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).ok()?;
        Some(
            b.schema()
                .fields()
                .iter()
                .map(|f| (f.name().clone(), f.data_type().to_string()))
                .collect(),
        )
    }

    fn bind_snapshots(&self, view: &str, files: &[PathBuf]) -> Result<()> {
        self.engine()
            .register_facts(view, &[], sized(files)?, &[], (None, None))
            .map_err(engine_err)
    }

    fn bind_labels(&self, labels_dir: &Path) -> Result<()> {
        let mut rows = Vec::new();
        for e in std::fs::read_dir(labels_dir)?.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let text = std::fs::read_to_string(&p)?;
            let doc: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not JSON", p.display()))?;
            for item in doc.as_array().into_iter().flatten() {
                let address = item
                    .get("address")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase);
                let label = item.get("label").cloned().unwrap_or(Value::Null);
                rows.push(serde_json::json!({ "address": address, "label": label }));
            }
        }
        if rows.is_empty() {
            return Err(anyhow!("no label snapshots"));
        }
        self.engine()
            .register_rows("labels", &rows)
            .map_err(engine_err)
    }
}

#[cfg(test)]
mod tests {
    /// `unconfigured_duckdb_still_opens_at_todays_walls`, for Burrmill: the same memory, threads and
    /// private spill directory, unless the operator says otherwise.
    #[test]
    fn unconfigured_burrmill_opens_at_todays_walls() {
        let _env = crate::analytics_budget::tests::env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cfg = crate::analytics_budget::from_env();
        assert_eq!((cfg.memory_limit_mb, cfg.threads), (512, 2));
        let spill = crate::engine_duck::new_spill_dir().unwrap();
        let budget = super::budget(&cfg, &spill.0);
        assert_eq!(budget.memory_bytes, 512 << 20);
        assert_eq!(budget.threads, 2);
        assert_eq!(budget.spill, Some((spill.0.clone(), 2 << 30)));
        assert!(super::BurrmillSession::new().is_ok());
    }

    #[test]
    fn burrmill_keys_derivations_by_its_own_parse_and_build() {
        use crate::engine::Session;
        let s = super::BurrmillSession::new().unwrap();
        let a = s.canonical_plan("SELECT a.x FROM t a -- c\nWHERE a.y > 1");
        assert!(a.is_some());
        assert_eq!(a, s.canonical_plan("select b.x from t b where b.y > 1"));
        assert_ne!(a, s.canonical_plan("SELECT a.x FROM u a WHERE a.y > 1"));
        assert!(s.engine_version().starts_with("burrmill "));
    }

    use super::*;
    use crate::engine_shadow::{Difference, ShadowEngine};
    use serde_json::json;

    /// Five sealed blocks and two hot rows, bound on both engines the way `/sql` binds them, then
    /// the kind of statement a dashboard runs. The shadow must record nothing.
    #[test]
    fn duckdb_and_burrmill_agree_on_a_sealed_and_hot_nest() {
        let dir = tempfile::tempdir().unwrap();
        crate::seal::test_set_table_floor(dir.path(), 0);
        for b in 1..=5u64 {
            let row = json!({"table": "t", "block_number": b, "who": if b % 2 == 0 { "x" } else { "y" }, "amount": (b * 1_000_000_000_000_000_000u64).to_string()});
            crate::seal::seal_range(dir.path(), &[row.to_string()], b, b).unwrap();
        }
        let seen = Arc::new(Mutex::new(Vec::<Difference>::new()));
        let s = seen.clone();
        let engine = ShadowEngine::new(
            Box::new(crate::engine_duck::DuckEngine),
            Box::new(BurrmillEngine),
            Arc::new(move |d: &Difference| s.lock().unwrap().push(d.clone())),
        );
        let session = engine.open(dir.path()).unwrap();
        let manifest = crate::seal::load_manifest_with_hash(dir.path()).unwrap().0;
        let sealed: Vec<PathBuf> = manifest.tables["t"]
            .iter()
            .map(|s| crate::seal::segment_path(dir.path(), &s.file, &s.hash))
            .collect();
        let cols = vec![
            ("block_number".to_string(), "u64".to_string()),
            ("who".to_string(), "address".to_string()),
            ("amount".to_string(), "word32".to_string()),
        ];
        let hot = [
            json!({"block_number": 6, "who": "x", "amount": "7"}),
            json!({"block_number": 7, "who": "y", "amount": "8"}),
        ];
        session
            .load_hot("t", &hot.iter().collect::<Vec<_>>())
            .unwrap();
        assert!(session
            .bind_facts("t", &cols, &sealed, true, FactWindow::default())
            .unwrap());
        for sql in [
            "SELECT count(*) AS n FROM t",
            "SELECT who, sum(amount_dec) AS total, max(block_number) AS last FROM t GROUP BY who ORDER BY who",
            "SELECT block_number, amount_overflow FROM t WHERE block_number > 4 ORDER BY 1",
        ] {
            let Collected { rows, truncated, .. } = session.collect(sql, Some(1000)).unwrap();
            assert!(!rows.is_empty(), "{sql}");
            assert!(!truncated);
        }
        let seen = seen.lock().unwrap();
        assert!(seen.is_empty(), "{seen:#?}");
    }

    /// The SQL nuthatch writes itself, rather than the SQL a user or an authored view writes: the
    /// restart folds, the recipes, a webhook predicate and GraphQL's lowering, each on both engines.
    #[test]
    fn duckdb_and_burrmill_agree_on_the_sql_nuthatch_generates() {
        let dir = tempfile::tempdir().unwrap();
        crate::seal::test_set_table_floor(dir.path(), 0);
        let zero = crate::recipes::ZERO_ADDRESS;
        let transfers = [
            (zero, "0xaa", "1000"),
            (zero, "0xbb", "250"),
            ("0xaa", "0xbb", "400"),
            ("0xbb", "0xcc", "650"),
            ("0xcc", zero, "50"),
            // Past 38 digits: the folds drop it and `oversized_transfers` counts it.
            (
                "0xaa",
                "0xcc",
                "1606938044258990275541962092341162602522202993782792835301376",
            ),
            // Fits i128 but not 38 digits, where Burrmill's HUGEINT ends: dropped by both.
            ("0xbb", "0xaa", "150000000000000000000000000000000000000"),
        ];
        for (i, (from, to, value)) in transfers.iter().enumerate() {
            let b = i as u64 + 1;
            let rows = [
                json!({"table": "tok__transfer", "block_number": b, "log_index": 0, "from": from, "to": to, "value": value}).to_string(),
                json!({"table": "tok__sync", "block_number": b, "log_index": 1, "address": if b.is_multiple_of(2) { "0xp1" } else { "0xp2" }, "reserve0": (b * 10).to_string(), "reserve1": (b * 7).to_string()}).to_string(),
            ];
            crate::seal::seal_range(dir.path(), &rows, b, b).unwrap();
        }
        let labels = dir.path().join("labels");
        std::fs::create_dir_all(&labels).unwrap();
        std::fs::write(
            labels.join("l.json"),
            r#"[{"address":"0xBB","label":"exchange"},{"address":"0xcc","label":"mixer"}]"#,
        )
        .unwrap();

        let seen = Arc::new(Mutex::new(Vec::<Difference>::new()));
        let s = seen.clone();
        let engine = ShadowEngine::new(
            Box::new(crate::engine_duck::DuckEngine),
            Box::new(BurrmillEngine),
            Arc::new(move |d: &Difference| s.lock().unwrap().push(d.clone())),
        );
        let session = engine.open(dir.path()).unwrap();
        let manifest = crate::seal::load_manifest_with_hash(dir.path()).unwrap().0;
        let sealed = |t: &str| -> Vec<PathBuf> {
            manifest.tables[t]
                .iter()
                .map(|s| crate::seal::segment_path(dir.path(), &s.file, &s.hash))
                .collect()
        };
        let col = |n: &str, t: &str| (n.to_string(), t.to_string());
        let transfer_cols = [
            col("block_number", "u64"),
            col("log_index", "u64"),
            col("from", "address"),
            col("to", "address"),
            col("value", "word32"),
        ];
        let sync_cols = [
            col("block_number", "u64"),
            col("log_index", "u64"),
            col("address", "address"),
            col("reserve0", "word32"),
            col("reserve1", "word32"),
        ];
        for (t, cols) in [("tok__transfer", &transfer_cols), ("tok__sync", &sync_cols)] {
            assert!(session
                .bind_facts(t, cols, &sealed(t), false, FactWindow::default())
                .unwrap());
        }
        session.bind_labels(&labels).unwrap();

        let graph = crate::graph_schema::parse(
            r#"
type Pool @entity { id: ID! liquidity: BigInt! token0: Token! swaps: [Swap!]! @derivedFrom(field: "pool") }
type Token @entity { id: ID! symbol: String! }
type Swap @entity { id: ID! pool: Pool! }
"#,
        )
        .unwrap();
        let view = crate::subgraph_import::to_alias;
        for ddl in [
            format!("CREATE VIEW \"{}\" AS SELECT * FROM (VALUES ('p1', '5', 't1'), ('p2', '170141183460469231731687303715884105728', 't2'), ('p3', '12', 't1')) v(id, liquidity, token0)", view("Pool")),
            format!("CREATE VIEW \"{}\" AS SELECT * FROM (VALUES ('t1', 'AAA'), ('t2', 'BBB')) v(id, symbol)", view("Token")),
            format!("CREATE VIEW \"{}\" AS SELECT * FROM (VALUES ('s1', 'p1'), ('s2', 'p1'), ('s3', 'p3')) v(id, pool)", view("Swap")),
        ] {
            session.execute(&ddl).unwrap();
        }
        let mut statements: Vec<String> = [
            crate::recipes::total_supply_select("tok"),
            crate::recipes::balances_select("tok"),
            crate::recipes::holder_count_select("tok"),
            crate::recipes::reserves_select("tok"),
            "SELECT * FROM \"tok__transfer\" WHERE block_number > 0 AND block_number <= 6 \
             AND (CAST(value AS HUGEINT) >= 100) ORDER BY block_number, log_index"
                .to_string(),
        ]
        .into_iter()
        .chain(crate::analytics::generated_fold_sql(
            "tok__transfer",
            "from",
            "to",
            "value",
            3,
        ))
        .collect();
        for q in [
            "{ pools { id liquidity } }",
            "{ pools(orderBy: liquidity, orderDirection: desc) { id } }",
            "{ pools(where: { liquidity_gt: \"6\" }) { id } }",
            "{ pools { id token0 { symbol } } }",
            "{ pools { id swaps { id } } }",
            "{ swaps { id pool { id liquidity } } }",
        ] {
            let root = crate::graph_query::parse(q).unwrap().remove(0);
            statements.push(crate::graph_query::compile(&graph, &root).unwrap().sql);
        }

        for sql in &statements {
            let before = seen.lock().unwrap().len();
            let served = session.collect(sql, Some(1000));
            let after = seen.lock().unwrap().len();
            eprintln!(
                "{}\t{}\t{}",
                if after > before { "DIFF" } else { "same" },
                match &served {
                    Ok(c) => format!("{} rows", c.rows.len()),
                    Err(e) => format!("primary refused: {e:?}"),
                },
                sql.split_whitespace().collect::<Vec<_>>().join(" ")
            );
        }
        let seen = seen.lock().unwrap();
        for d in seen.iter() {
            eprintln!(
                "{:?}\n  primary={}\n  secondary={}",
                d.kind, d.primary, d.secondary
            );
        }
        assert!(seen.is_empty(), "{} differences", seen.len());
    }

    /// Shadow mode over a real nest, through the production path: the shadow is installed, every
    /// authored view is read whole with `query_guarded`, and each difference is printed and
    /// counted. Ignored unless `NUTHATCH_SHADOW_NEST` names the nest directory.
    ///
    ///     NUTHATCH_SHADOW_NEST=/path/to/nest cargo test --release --features shadow-burrmill \
    ///         --lib shadow_replay -- --ignored --nocapture
    #[test]
    #[ignore]
    fn shadow_replay_over_a_nest() {
        let Ok(nest) = std::env::var("NUTHATCH_SHADOW_NEST") else {
            return;
        };
        let dir = Path::new(&nest);
        let seen = Arc::new(Mutex::new(Vec::<Difference>::new()));
        let s = seen.clone();
        let recording: crate::engine_shadow::Sink = Arc::new(move |d: &Difference| {
            eprintln!(
                "DIFF\t{:?}\tprimary={}\tsecondary={}\tprimary_ms={}\tsecondary_ms={}\n\t{}",
                d.kind, d.primary, d.secondary, d.primary_ms, d.secondary_ms, d.sql
            );
            s.lock().unwrap().push(d.clone());
        });
        // The same file `enable_shadow` would write, so the replay exercises the operator's sink.
        let sink = match std::env::var_os("NUTHATCH_SHADOW_LOG") {
            Some(path) => crate::engine_shadow::both_sinks(
                recording,
                crate::engine_shadow::file_sink(Path::new(&path)).unwrap(),
            ),
            None => recording,
        };
        // `NUTHATCH_ENGINE=checked` replays with Burrmill served, as that switch runs a nest.
        let checked = std::env::var(crate::analytics::ENV_ENGINE).as_deref() == Ok("checked");
        let pair = if checked {
            ShadowEngine::new(
                Box::new(BurrmillEngine),
                Box::new(crate::engine_duck::DuckEngine),
                sink,
            )
            .reversed()
        } else {
            ShadowEngine::new(
                Box::new(crate::engine_duck::DuckEngine),
                Box::new(BurrmillEngine),
                sink,
            )
        };
        crate::engine_shadow::install(match std::env::var_os("NUTHATCH_SHADOW_LOG") {
            Some(path) => pair.with_tally_in(Path::new(&path)).unwrap(),
            None => pair,
        })
        .unwrap();
        let mut views: Vec<String> = crate::analytics::nest_view_files(dir)
            .iter()
            .flat_map(|f| crate::analytics::split_sql_statements(&f.sql))
            .filter_map(|stmt| crate::analytics::view_name(&stmt))
            .collect();
        views.sort();
        let guard = crate::analytics::QueryGuard {
            timeout: std::time::Duration::from_secs(600),
            max_rows: 10_000_000,
        };
        for view in &views {
            let before = seen.lock().unwrap().len();
            let started = std::time::Instant::now();
            let out =
                crate::analytics::query_guarded(dir, &format!("SELECT * FROM \"{view}\""), guard);
            let ms = started.elapsed().as_millis();
            let after = seen.lock().unwrap().len();
            match out {
                Ok(o) => eprintln!(
                    "VIEW\t{view}\trows={}\tms={ms}\tdifferences={}",
                    o.rows.len(),
                    after - before
                ),
                Err(e) => eprintln!("VIEW\t{view}\tERROR\tms={ms}\t{e:#}"),
            }
        }
        // The dashboard's own statements, from kittiwake's `dump_nest_sql` example, `;;` between
        // them, with the marker ids it prints replaced by ids this nest actually has.
        if let Ok(file) = std::env::var("NUTHATCH_SHADOW_SQL") {
            let text = std::fs::read_to_string(&file).unwrap();
            let first_cell = |sql: &str| -> Option<String> {
                crate::analytics::query_guarded(dir, sql, guard)
                    .ok()?
                    .rows
                    .into_iter()
                    .next()?
                    .as_object()?
                    .values()
                    .next()?
                    .as_str()
                    .map(str::to_string)
            };
            let markers = [
                (
                    "0x00000000000000000000000000000000000000a1",
                    "SELECT id FROM lodestar_indexers ORDER BY staked_tokens DESC LIMIT 1",
                ),
                (
                    "0x00000000000000000000000000000000000000a2",
                    "SELECT id FROM lodestar_delegators LIMIT 1",
                ),
                (
                    "0x00000000000000000000000000000000000000a3",
                    "SELECT id FROM lodestar_curators LIMIT 1",
                ),
                (
                    "0x00000000000000000000000000000000000000a4",
                    "SELECT id FROM lodestar_indexers ORDER BY staked_tokens DESC LIMIT 1",
                ),
                (
                    "0x00000000000000000000000000000000000000000000000000000000000000d1",
                    "SELECT id FROM lodestar_deployments LIMIT 1",
                ),
            ];
            let subs: Vec<(&str, String)> = markers
                .iter()
                .filter_map(|(m, sql)| first_cell(sql).map(|v| (*m, v)))
                .collect();
            eprintln!("MARKERS\tresolved={}/{}", subs.len(), markers.len());
            for stmt in text.split("\n;;\n") {
                let mut sql = stmt.trim().to_string();
                if sql.is_empty() {
                    continue;
                }
                for (m, v) in &subs {
                    sql = sql.replace(m, v);
                }
                let before = seen.lock().unwrap().len();
                let started = std::time::Instant::now();
                let out = crate::analytics::query_guarded(dir, &sql, guard);
                let ms = started.elapsed().as_millis();
                let after = seen.lock().unwrap().len();
                let head: String = sql.chars().take(90).collect();
                match out {
                    Ok(o) => eprintln!(
                        "STMT\trows={}\tms={ms}\tdifferences={}\t{head}",
                        o.rows.len(),
                        after - before
                    ),
                    Err(e) => eprintln!("STMT\tERROR\tms={ms}\t{head}\t{e:#}"),
                }
            }
        }
        let seen = seen.lock().unwrap();
        let unexplained = seen.iter().filter(|d| d.kind.unexplained()).count();
        eprintln!(
            "SHADOW\tviews={}\trecords={}\tunexplained={unexplained}",
            views.len(),
            seen.len()
        );
    }
}
