//! Burrmill behind [`crate::engine`], the second engine of shadow mode (RFC-0044 Amendment 2,
//! phase 2b). Compiled only with the `shadow-burrmill` feature, and installed by
//! [`enable_shadow`]; a release build never names it.
//!
//! A session is one `burrmill::Engine` opened empty, with the tables the policy code binds registered
//! as it binds them: the same segment list, the same hot rows, the same declared columns and window
//! DuckDB is given. The parser role (`serialize_sql`), the DuckDB plan walk (`cold_scan_operators`)
//! and the DuckDB catalogue text (`view_definitions`) are not Burrmill's to answer and are refused;
//! the shadow session only ever asks the primary for those.

use crate::engine::{Died, Engine, FactWindow, Interrupt, Session};
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
/// names one.
pub fn enable_shadow() -> Result<()> {
    use crate::engine_shadow::{both_sinks, file_sink, log_sink};
    let sink = match std::env::var_os("NUTHATCH_SHADOW_LOG") {
        Some(path) => both_sinks(log_sink(), file_sink(Path::new(&path))?),
        None => log_sink(),
    };
    crate::engine_shadow::install(crate::engine_shadow::ShadowEngine::new(
        Box::new(crate::engine_duck::DuckEngine),
        Box::new(BurrmillEngine),
        sink,
    ))
}

pub(crate) struct BurrmillSession {
    engine: Mutex<burrmill::Engine>,
    /// Hot rows staged by `load_hot`, bound by the next `bind_facts` for that table.
    hot: Mutex<HashMap<String, Vec<Value>>>,
}

impl BurrmillSession {
    fn new() -> Result<Self> {
        Ok(Self {
            engine: Mutex::new(burrmill::Engine::open_empty().map_err(engine_err)?),
            hot: Mutex::new(HashMap::new()),
        })
    }

    fn engine(&self) -> std::sync::MutexGuard<'_, burrmill::Engine> {
        self.engine.lock().unwrap_or_else(|p| p.into_inner())
    }
}

fn engine_err(e: burrmill::BurrmillError) -> anyhow::Error {
    anyhow!("{e}")
}

/// Refused before running, or died running: the split the integrity sweep depends on.
fn died(e: burrmill::BurrmillError) -> Died {
    use burrmill::BurrmillError::*;
    match e {
        NotAllowed(_) | Parse(_) | NoSegments(_) => Died::Binding(engine_err(e)),
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
        // The statements the policy code runs for effect are all view definitions; anything else
        // (a transaction boundary on the folds path) has no Burrmill meaning and says so.
        let (Some(name), Some(body)) = (
            crate::analytics::view_name(sql),
            crate::analytics::view_body(sql),
        ) else {
            return Err(anyhow!(
                "not a view definition, and Burrmill runs nothing else for effect"
            ));
        };
        self.engine().register_view(&name, body).map_err(engine_err)
    }

    fn collect(&self, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
        // The same two caps `engine_duck` applies, so the shadow truncates where the primary does.
        let hard = cap.map(|c| c + 1);
        let byte_cap = cap.map(|_| crate::engine::SQL_MAX_RESULT_BYTES);
        let mut bytes = 0usize;
        let mut out = Vec::new();
        let mut over = false;
        let engine = self.engine();
        let r = engine.sql_for_each(sql, |batch| {
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
        match r {
            Ok(()) => Ok((out, false)),
            Err(_) if over => Ok((out, true)),
            Err(e) => Err(died(e)),
        }
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
        let (rows, _) = self.collect(sql, Some(1))?;
        rows.into_iter()
            .next()
            .and_then(|r| r.as_object().and_then(|o| o.values().next().cloned()))
            .ok_or_else(|| anyhow!("no rows"))
    }

    fn query_arrow(&self, _sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        // Burrmill's arrow is not nuthatch's arrow; the fold snapshot path stays on the primary.
        Err(anyhow!("query_arrow is not available on the shadow engine"))
    }

    fn column_names(&self, sql: &str) -> Result<Vec<String>> {
        let engine = self.engine();
        let mut names = Vec::new();
        engine
            .sql_for_each(sql, |batch| {
                if names.is_empty() {
                    names = batch
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| f.name().clone())
                        .collect();
                }
                Ok(())
            })
            .map_err(engine_err)?;
        Ok(names)
    }

    fn describe(&self, sql: &str) -> Result<Vec<(String, String)>> {
        let engine = self.engine();
        let mut out = Vec::new();
        engine
            .sql_for_each(sql, |batch| {
                if out.is_empty() {
                    out = batch
                        .schema()
                        .fields()
                        .iter()
                        .map(|f| (f.name().clone(), f.data_type().to_string()))
                        .collect();
                }
                Ok(())
            })
            .map_err(engine_err)?;
        Ok(out)
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
        None
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

    fn cold_scan_operators(&self, _sql: &str) -> Result<u64> {
        Err(crate::analytics::unboundable(
            "the admission bound is planned on DuckDB",
        ))
    }

    fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()> {
        self.hot.lock().unwrap_or_else(|p| p.into_inner()).insert(
            table.to_string(),
            rows.iter().map(|r| (*r).clone()).collect(),
        );
        Ok(())
    }

    fn bind_facts(
        &self,
        table: &str,
        cols: &[(String, String)],
        sealed: &[PathBuf],
        hot: bool,
        window: FactWindow,
    ) -> Result<bool> {
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
            let (rows, truncated) = session.collect(sql, Some(1000)).unwrap();
            assert!(!rows.is_empty(), "{sql}");
            assert!(!truncated);
        }
        let seen = seen.lock().unwrap();
        assert!(seen.is_empty(), "{seen:#?}");
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
        crate::engine_shadow::install(ShadowEngine::new(
            Box::new(crate::engine_duck::DuckEngine),
            Box::new(BurrmillEngine),
            sink,
        ))
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
