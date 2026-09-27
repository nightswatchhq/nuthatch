//! DuckDB behind [`crate::engine`]. Everything in this file is DuckDB-shaped: the configuration and
//! lockdown, the spill directory, `read_parquet` and `UNION ALL BY NAME` in the view DDL, the
//! Appender for hot rows, `json_serialize_sql`, `EXPLAIN (FORMAT JSON)` and its operator names, the
//! `duckdb_views()` catalogue, and the encoding of DuckDB values as nuthatch's JSON. It was moved
//! here from `analytics.rs` unchanged in what it does; `analytics.rs` keeps the policy and no longer
//! names the engine.

use crate::engine::{
    value_bytes, Died, Engine, FactWindow, Interrupt, Session, SQL_MAX_RESULT_BYTES,
};
use anyhow::{bail, Context, Result};
use duckdb::arrow::datatypes::DataType;
use duckdb::types::{Value as DuckValue, ValueRef};
use duckdb::{Config, Connection};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub(crate) struct DuckEngine;

impl Engine for DuckEngine {
    fn open(&self, dir: &Path) -> Result<Box<dyn Session>> {
        let (conn, spill) = open_locked_duckdb(dir)?;
        Ok(Box::new(DuckSession {
            conn,
            _spill: Some(spill),
        }))
    }

    fn open_bare(&self) -> Result<Box<dyn Session>> {
        let conn = Connection::open_in_memory()?;
        register_extensions(&conn)?;
        Ok(Box::new(DuckSession { conn, _spill: None }))
    }
}

pub(crate) struct DuckSession {
    conn: Connection,
    /// This instance's private spill directory, removed when the session is dropped (#1165). Held
    /// here so its lifetime is exactly the connection's. A bare session has none.
    _spill: Option<SpillDir>,
}

impl Interrupt for duckdb::InterruptHandle {
    fn interrupt(&self) {
        duckdb::InterruptHandle::interrupt(self)
    }
}

impl Session for DuckSession {
    fn execute(&self, sql: &str) -> Result<()> {
        Session::execute(&self.conn, sql)
    }
    fn collect(&self, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
        Session::collect(&self.conn, sql, cap)
    }
    fn for_each_row(&self, sql: &str, f: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
        self.conn.for_each_row(sql, f)
    }
    fn one_value(&self, sql: &str) -> Result<Value> {
        self.conn.one_value(sql)
    }
    fn query_arrow(&self, sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        Session::query_arrow(&self.conn, sql)
    }
    fn column_names(&self, sql: &str) -> Result<Vec<String>> {
        self.conn.column_names(sql)
    }
    fn describe(&self, sql: &str) -> Result<Vec<(String, String)>> {
        self.conn.describe(sql)
    }
    fn has_relation(&self, name: &str) -> bool {
        self.conn.has_relation(name)
    }
    fn relations(&self) -> Result<BTreeSet<String>> {
        self.conn.relations()
    }
    fn view_definitions(&self) -> Option<Vec<(String, String)>> {
        self.conn.view_definitions()
    }
    fn serialize_sql(&self, sql: &str) -> Result<Value> {
        self.conn.serialize_sql(sql)
    }
    fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
        Session::interrupt_handle(&self.conn)
    }
    fn cold_scan_operators(&self, sql: &str) -> Result<u64> {
        self.conn.cold_scan_operators(sql)
    }
    fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()> {
        self.conn.load_hot(table, rows)
    }
    fn bind_facts(
        &self,
        table: &str,
        cols: &[(String, String)],
        sealed: &[PathBuf],
        hot: bool,
        window: FactWindow,
    ) -> Result<bool> {
        self.conn.bind_facts(table, cols, sealed, hot, window)
    }
    fn segment_binds(&self, path: &Path) -> Result<()> {
        self.conn.segment_binds(path)
    }
    fn file_schema(&self, path: &Path) -> Option<Vec<(String, String)>> {
        self.conn.file_schema(path)
    }
    fn bind_snapshots(&self, view: &str, files: &[PathBuf]) -> Result<()> {
        self.conn.bind_snapshots(view, files)
    }
    fn bind_labels(&self, labels_dir: &Path) -> Result<()> {
        self.conn.bind_labels(labels_dir)
    }
}

/// The engine proper. On the connection itself so a test that holds one can hand it to any policy
/// function and still poke it directly.
impl Session for Connection {
    fn execute(&self, sql: &str) -> Result<()> {
        Ok(self.execute_batch(sql)?)
    }

    fn collect(&self, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
        collect(self, sql, cap)
    }

    fn for_each_row(&self, sql: &str, f: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
        let mut stmt = self.prepare(sql)?;
        let mut rows = stmt.query([])?;
        let width = rows.as_ref().map_or(0, |s| s.column_count());
        let mut cells = Vec::with_capacity(width);
        while let Some(row) = rows.next()? {
            cells.clear();
            for i in 0..width {
                cells.push(value_to_json(row.get_ref(i)?));
            }
            f(&cells)?;
        }
        Ok(())
    }

    fn one_value(&self, sql: &str) -> Result<Value> {
        Ok(self.query_row(sql, [], |r| Ok(value_to_json(r.get_ref(0)?)))?)
    }

    fn query_arrow(&self, sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        let mut stmt = self.prepare(sql)?;
        Ok(stmt.query_arrow([])?.collect())
    }

    fn column_names(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.prepare(sql)?;
        let rows = stmt.query([])?;
        drop(rows);
        Ok(stmt.column_names().iter().map(|s| s.to_string()).collect())
    }

    fn describe(&self, sql: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.prepare(&format!("DESCRIBE {sql}"))?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    fn has_relation(&self, name: &str) -> bool {
        self.query_row(
            "SELECT count(*) FROM (SELECT view_name AS n FROM duckdb_views() WHERE NOT internal \
                 UNION ALL SELECT table_name FROM duckdb_tables()) WHERE lower(n) = ?",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(true)
    }

    fn relations(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .prepare("SELECT lower(view_name) FROM duckdb_views() WHERE NOT internal")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?)
    }

    fn view_definitions(&self) -> Option<Vec<(String, String)>> {
        let mut defs = Vec::new();
        self.prepare("SELECT view_name, sql FROM duckdb_views()")
            .and_then(|mut s| {
                let rows =
                    s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                defs.extend(rows.flatten());
                Ok(())
            })
            .ok()?;
        Some(defs)
    }

    fn serialize_sql(&self, sql: &str) -> Result<Value> {
        let literal = format!("'{}'", sql.replace('\'', "''"));
        let ast: String =
            self.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
                r.get(0)
            })?;
        Ok(serde_json::from_str(&ast)?)
    }

    fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
        self.interrupt_handle()
    }

    fn cold_scan_operators(&self, sql: &str) -> Result<u64> {
        let plan: String = self
            .query_row(&format!("EXPLAIN (FORMAT JSON) {sql}"), [], |row| {
                row.get(1)
            })
            .context("failed to plan query")?;
        let plan: Value = serde_json::from_str(&plan).map_err(|e| {
            crate::analytics::unboundable(format!(
                "DuckDB returned a physical plan that is not JSON: {e}"
            ))
        })?;
        physical_parquet_scans(&plan)
    }

    fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()> {
        load_hot_temp(self, &hot_table(table), rows)
    }

    fn bind_facts(
        &self,
        table: &str,
        cols: &[(String, String)],
        sealed: &[PathBuf],
        hot: bool,
        window: FactWindow,
    ) -> Result<bool> {
        let files: Vec<String> = sealed
            .iter()
            .map(|p| format!("'{}'", p.display()))
            .collect();
        let mut parts: Vec<String> = Vec::new();
        if !files.is_empty() {
            // COR-2: `union_by_name=true` NULL-fills columns that differ across segments - segment
            // schemas legitimately drift over a nest's life as ABIs are versioned (CLAUDE.md), and
            // without this a single drifted column makes `read_parquet` throw and the whole table's
            // view silently vanish.
            parts.push(format!(
                "SELECT *{} FROM {}",
                crate::analytics::derived_bigint_cols(cols),
                with_declared_base_cols(
                    &format!("read_parquet([{}], union_by_name=true)", files.join(", ")),
                    cols
                )
            ));
        }
        if hot {
            parts.push(format!(
                "SELECT *{} FROM {}",
                crate::analytics::derived_bigint_cols(cols),
                with_declared_base_cols(&format!("\"{}\"", hot_table(table)), cols)
            ));
        }
        let ddl = if parts.is_empty() {
            // Nothing sealed and nothing hot: an empty typed view so nest views resolve to zero rows
            // instead of cascade-failing (skip a table with no declared columns).
            if cols.is_empty() {
                return Ok(false);
            }
            empty_view_ddl(table, cols)
        } else {
            // `UNION ALL BY NAME` aligns columns by name and NULL-fills any a side lacks (a column
            // all-null over the sealed range is dropped from its Parquet schema; hot may still
            // carry it).
            let union = parts.join(" UNION ALL BY NAME ");
            let select = if window.is_bounded() {
                format!(
                    "SELECT * FROM ({union}) historical_facts WHERE CASE \
                     WHEN block_number IS NULL THEN error('historical query requires block-stamped facts: unstamped archived row') \
                     ELSE {} END",
                    window.predicate()
                )
            } else {
                union
            };
            format!("CREATE OR REPLACE VIEW \"{table}\" AS {select}")
        };
        self.execute_batch(&ddl)?;
        Ok(true)
    }

    fn segment_binds(&self, path: &Path) -> Result<()> {
        // `read_parquet` validates the footer while binding, so a present but unreadable segment
        // fails here and nowhere cheaper.
        let probe = format!(
            "SELECT 1 FROM read_parquet(['{}'], union_by_name=true) LIMIT 0",
            path.display()
        );
        self.prepare(&probe)?;
        Ok(())
    }

    fn file_schema(&self, path: &Path) -> Option<Vec<(String, String)>> {
        self.prepare(&format!(
            "DESCRIBE SELECT * FROM read_parquet(['{}'])",
            path.display()
        ))
        .and_then(|mut st| {
            st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect()
        })
        .ok()
    }

    fn bind_snapshots(&self, view: &str, files: &[PathBuf]) -> Result<()> {
        let files: Vec<String> = files
            .iter()
            .map(|p| format!("'{}'", p.display().to_string().replace('\'', "''")))
            .collect();
        let ddl = format!(
            "CREATE OR REPLACE VIEW \"{view}\" AS SELECT * FROM read_parquet([{}], union_by_name=true)",
            files.join(", ")
        );
        Ok(self.execute_batch(&ddl)?)
    }

    fn bind_labels(&self, labels_dir: &Path) -> Result<()> {
        let glob = labels_dir.join("*.json");
        let ddl = format!(
            "CREATE OR REPLACE VIEW labels AS SELECT lower(address) AS address, label \
             FROM read_json('{}', format='array', columns={{address: 'VARCHAR', label: 'VARCHAR'}})",
            glob.display()
        );
        Ok(self.execute_batch(&ddl)?)
    }
}

/// The temp table one logical table's hot rows are staged in.
fn hot_table(table: &str) -> String {
    format!("__hot_{table}")
}

/// Open an in-memory DuckDB whose file access is pinned to the nest's data dirs (#289).
///
/// DuckDB's `allowed_directories` is an *addition* to the allow-list while `enable_external_access`
/// is on, and a restriction only when it is off. The flag is startup-only, so it has to go on the
/// `Config`, not in a later `SET`. `lock_configuration` then freezes both so a query cannot widen
/// them. Measured against `libduckdb-sys` 1.10504.0, the bundled build.
fn allowed_read_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        dir.join(crate::seal::SEGMENTS_DIR),
        dir.join("labels"),
        dir.join(crate::offchain::DIR).join("segments"),
    ];
    #[cfg(feature = "folds")]
    dirs.push(dir.join(crate::folds::CHECKPOINTS_DIR));
    // Runtime layout (RFC-0033): Parquet lives at `<root>/segments/{hash}.parquet`, not under
    // `data/<nid>/segments`. Locking only the per-dataset dir made `/sql` succeed with zero rows
    // on every mounted nest (#289 follow-up, `e2e_early_cutoff`).
    if let Some(shared) = crate::seal::shared_store(dir) {
        dirs.push(shared);
    }
    dirs
}

/// A DuckDB instance's private spill directory, removed when the instance is dropped (#1165).
///
/// DuckDB spills buffers past `max_memory` into `temp_directory`, whose files are named by *index*
/// within that directory - `duckdb_temp_storage_DEFAULT-0.tmp`, `..._S128K-0.tmp`, and so on. The
/// name says nothing about which instance wrote it, because DuckDB assumes one instance owns the
/// directory. This process runs several: the connection cache hands its connection to a query for
/// the query's whole duration, so a second concurrent query finds the cache empty and opens an
/// instance of its own. With the default `temp_directory` - `.tmp` under the working directory,
/// which for a nest service is the nest's own data directory - those instances write the same file
/// names in one place and overwrite each other's spilled blocks.
///
/// Reading such a block back fails DuckDB's own check, `decompressed_size == buffer->AllocSize()` in
/// `temporary_file_manager.cpp`, and the process dies. Reproduced 2026-09-06 against a copy of the
/// production corpus: four threads replaying 51 captured statements, a segmentation fault inside
/// fifteen seconds, every time. On the box it was 25 SEGVs in a minute at four permits and none at
/// one, which is the same fault seen from the outside - one permit never has a second instance.
pub(crate) struct SpillDir(pub(crate) PathBuf);

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Remove spill directories left by processes that are no longer running.
///
/// The cache lives in a `static`, and Rust does not drop statics at exit, so a cached connection's
/// [`SpillDir`] guard never runs on the way out and its directory outlives the process. One empty
/// directory per instance is not much, but a nest restarting every fifteen seconds under a fault -
/// which is exactly what #1165 looked like - would leave one behind each time, with whatever it had
/// spilled inside. Ownership is by pid: a directory whose pid still has a `/proc` entry belongs to a
/// live process and is left alone. Where `/proc` is not there to ask (macOS, dev machines), nothing
/// is swept, because guessing by age could delete a running instance's spill under it.
fn sweep_dead_spill_dirs(parent: &Path) {
    if !Path::new("/proc").is_dir() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(rest) = name
            .to_string_lossy()
            .strip_prefix("nuthatch-duckdb-")
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(pid) = rest.split('-').next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        if !Path::new(&format!("/proc/{pid}")).exists() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// The in-process half of a spill directory's name; the other half is the PID.
pub(crate) static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

fn spill_parent() -> PathBuf {
    crate::analytics_budget::from_env()
        .temp_directory
        .unwrap_or_else(std::env::temp_dir)
}

/// A directory no other DuckDB instance in this process (or any other) will write to.
///
/// Created **exclusively**: `create_dir`, not `create_dir_all`, and a name that already exists is
/// skipped for the next sequence number. The name alone is not enough. A process that dies without
/// dropping its static cache leaves its directories behind, the sweep keeps any whose PID is live,
/// and a later process handed that same PID by the kernel would otherwise start its sequence at zero
/// and write into the dead process's `-0` - the collision this whole change exists to remove, back
/// through PID reuse. Refusing an existing path makes the directory this instance's by construction,
/// whatever is left on disk. The parent is `analytics.temp_directory` when set, else the process
/// temp dir; the instance name is unchanged (#1165).
pub(crate) fn new_spill_dir() -> Result<SpillDir> {
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| {
        sweep_dead_spill_dirs(&std::env::temp_dir());
        if let Some(ref p) = crate::analytics_budget::from_env().temp_directory {
            if *p != std::env::temp_dir() {
                sweep_dead_spill_dirs(p);
            }
        }
    });
    let parent = spill_parent();
    if !parent.exists() {
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating the DuckDB spill parent {}", parent.display()))?;
    }
    loop {
        let path = parent.join(format!(
            "nuthatch-duckdb-{}-{}",
            std::process::id(),
            SPILL_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(SpillDir(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("creating the DuckDB spill directory {}", path.display())
                })
            }
        }
    }
}

/// RFC-0060 §5.6: what a `graph` build adds to every connection. A default build adds nothing, so its
/// connections behave exactly as they did before the feature existed.
fn register_extensions(conn: &Connection) -> Result<()> {
    #[cfg(feature = "graph")]
    {
        crate::analytics_scalars::register(conn)?;
        // GraphQL and the SQL surface define observable order explicitly, so retaining insertion
        // order buys no contract and can hold a whole extra ordering buffer for a large historical
        // aggregation.
        conn.execute_batch("SET preserve_insertion_order=false;")
            .context("failed to disable DuckDB insertion-order preservation")?;
    }
    #[cfg(not(feature = "graph"))]
    let _ = conn;
    Ok(())
}

fn open_locked_duckdb(dir: &Path) -> Result<(Connection, SpillDir)> {
    let spill = new_spill_dir()?;
    let allowed: Vec<String> = allowed_read_dirs(dir)
        .into_iter()
        .chain(std::iter::once(spill.0.clone()))
        .filter(|p| p.exists())
        .map(|p| format!("'{}'", p.display().to_string().replace('\'', "''")))
        .collect();
    // DuckDB's docs set the allow-list first, then turn external access off. Doing it the other
    // way round is refused: "Cannot change allowed_directories when enable_external_access is
    // disabled". The flag is *not* startup-only on 1.10504.0; a `SET` after open works, which is
    // why this was inert until now - we set the list and never flipped the flag.
    let resources = crate::analytics_budget::from_env();
    let mem_limit = format!("{}MB", resources.memory_limit_mb);
    let mut config = Config::default()
        .max_memory(&mem_limit)
        .context("duckdb max_memory")?
        .threads(resources.threads)
        .context("duckdb threads")?
        // Set on the config rather than by a later `SET`, so no query can ever run against the
        // shared default - and before `lock_configuration`, which freezes it (#1165).
        .with("temp_directory", spill.0.display().to_string())
        .context("duckdb temp_directory")?;
    if let Some(ref size) = resources.max_temp_size {
        config = config
            .with("max_temp_directory_size", size)
            .context("duckdb max_temp_directory_size")?;
    }
    let conn = Connection::open_in_memory_with_flags(config).context("open DuckDB")?;
    register_extensions(&conn)?;
    // Every build (#1152, then #1165). The bundled DuckDB's `D_ASSERT(min_val <= input)` in compressed
    // materialisation fires on an ordinary shape: a filtered `ORDER BY` whose scan reads one Parquet
    // file holding rows on both sides of the filter. The optimiser compresses the sort key against
    // the minimum the filter promised, and a row the filter has not yet removed reaches it. #1152
    // switched the optimiser off in debug builds, where the assertion aborts the process, and left
    // the release library alone because six measured shapes still answered correctly there.
    //
    // #1165 is why it is off everywhere now. A 3.4.0 nest serving Lodestar's curator lists - every
    // one a filtered `ORDER BY` over a view whose scan reads one segment spanning the filter - died
    // twice at the same instruction, a `free` on the HTTP response path: the signature of a native
    // library corrupting the heap, found where the neighbouring chunk is released. An assertion
    // violated in a release build is code carrying on with a value its author ruled out, and this
    // is the one such assertion known to trip on nuthatch's own shapes. Not proven to be the cause
    // (the fresh-process reproduction did not fault in an hour of the same queries); removed
    // because it is the cheapest suspect to remove and #1152 already measured that the answers do
    // not change without it. Every sealed segment of more than one block has this shape once a
    // cursor sits inside it.
    conn.execute_batch("SET disabled_optimizers='compressed_materialization';")
        .context("failed to disable compressed materialisation")?;
    let lockdown = format!(
        "SET allowed_directories=[{}]; SET enable_external_access=false; SET lock_configuration=true;",
        allowed.join(", ")
    );
    conn.execute_batch(&lockdown)
        .context("failed to lock down DuckDB filesystem access")?;
    Ok((conn, spill))
}

/// Parquet scans in a physical plan. An operator not listed refuses, and so does every operator
/// that can run its input more than once: a nested-loop or delim join rescans per outer row, and a
/// recursive CTE has no static scan count at all (RFC-0048 §3 rules 3b to 5).
pub(crate) fn physical_parquet_scans(plan: &Value) -> Result<u64> {
    use crate::analytics::unboundable;
    let nodes = plan
        .as_array()
        .ok_or_else(|| unboundable("the physical plan is not an operator list"))?;
    let mut scans = 0_u64;
    for node in nodes {
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| unboundable("a physical plan operator has no name"))?;
        match name.trim() {
            "READ_PARQUET"
            | "SEQ_SCAN"
            | "COLUMN_DATA_SCAN"
            | "DUMMY_SCAN"
            | "EMPTY_RESULT"
            | "RANGE"
            | "GENERATE_SERIES"
            | "UNNEST"
            | "PROJECTION"
            | "FILTER"
            | "HASH_JOIN"
            | "CROSS_PRODUCT"
            | "PIECEWISE_MERGE_JOIN"
            | "HASH_GROUP_BY"
            | "PERFECT_HASH_GROUP_BY"
            | "UNGROUPED_AGGREGATE"
            | "SIMPLE_AGGREGATE"
            | "ORDER_BY"
            | "TOP_N"
            | "LIMIT"
            | "STREAMING_LIMIT"
            | "LIMIT_PERCENT"
            | "UNION"
            | "WINDOW"
            | "STREAMING_WINDOW"
            | "CTE"
            | "CTE_SCAN"
            | "RESERVOIR_SAMPLE"
            | "STREAMING_SAMPLE" => {}
            other => {
                return Err(unboundable(format!(
                    "cannot bound physical plan operator {other:?}"
                )))
            }
        }
        let children = node
            .get("children")
            .ok_or_else(|| unboundable("a physical plan operator has no children"))?;
        scans = scans
            .checked_add(physical_parquet_scans(children)?)
            .and_then(|n| n.checked_add(u64::from(name.trim() == "READ_PARQUET")))
            .ok_or_else(|| unboundable("the physical scan count overflows"))?;
    }
    Ok(scans)
}

/// Prepare, execute and materialise the result. With `cap = Some(n)` it stops after `n + 1` rows so
/// the caller can report truncation precisely (the returned bool is true when that extra row existed,
/// i.e. more than `n` rows were available); the caller then truncates back to `n`. `cap = None`
/// materialises every row. Row materialisation is Rust-side and escapes DuckDB's own memory limit,
/// so the cap is what actually bounds a `SELECT *` result buffer.
fn collect(conn: &Connection, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
    let mut stmt = conn
        .prepare(sql)
        .context("failed to prepare query")
        .map_err(Died::Binding)?;
    let rows = stmt
        .query([])
        .context("query failed")
        .map_err(Died::Executing)?;
    // Column types are only known once the statement has executed, and a scaled decimal is the one
    // result this cannot materialise (#1433). It is rare, so the answer is read off this run and
    // only that case pays a second, wrapped one. Probing with a query of its own executed every
    // statement twice, which cost the SQL surface 1.7x from 3.8.5 to 3.11.0.
    match decimal_safe_projection(rows.as_ref(), sql) {
        None => drain(rows, cap),
        Some(wrapped) => {
            drop(rows);
            let mut stmt = conn
                .prepare(&wrapped)
                .context("failed to prepare query")
                .map_err(Died::Binding)?;
            let rows = stmt
                .query([])
                .context("query failed")
                .map_err(Died::Executing)?;
            drain(rows, cap)
        }
    }
}

/// Materialise an executed statement's rows as JSON, under the caps `collect` documents.
fn drain(mut rows: duckdb::Rows<'_>, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
    // Column metadata is only materialised once the statement has executed - read it off the
    // executed result, not the prepared statement.
    let column_names: Vec<String> = rows
        .as_ref()
        .map(|s| s.column_names().iter().map(|c| c.to_string()).collect())
        .unwrap_or_default();

    let hard = cap.map(|c| c + 1);
    // A row cap alone bounds row *count*, not row *width*: the materialised `Vec<Value>` lives Rust-side,
    // outside DuckDB's `memory_limit`, so `SELECT repeat('A', 20000000) FROM range(50000)` would accrue
    // ~1 TB before the wall-clock guard fires - breaching the <=2 GB per-cursor budget and, in a runtime,
    // OOM-killing co-tenants. The guarded (untrusted `/sql`) path therefore also caps cumulative result
    // bytes. Trusted unguarded queries (`cap = None`: registry-built folds, cold seeds) are never
    // byte-capped, so a large token's balance rebuild is never silently truncated.
    let byte_cap = cap.map(|_| SQL_MAX_RESULT_BYTES);
    let mut out = Vec::new();
    let mut bytes = 0usize;
    while let Some(row) = rows
        .next()
        .context("row read failed")
        .map_err(Died::Executing)?
    {
        let mut obj = Map::new();
        for (i, name) in column_names.iter().enumerate() {
            let v = value_to_json(row.get_ref(i).map_err(|e| Died::Executing(e.into()))?);
            if byte_cap.is_some() {
                bytes += name.len() + value_bytes(&v);
            }
            obj.insert(name.clone(), v);
        }
        out.push(Value::Object(obj));
        if hard.is_some_and(|h| out.len() >= h) {
            return Ok((out, true));
        }
        if byte_cap.is_some_and(|max| bytes >= max) {
            return Ok((out, true));
        }
    }
    Ok((out, false))
}

/// `duckdb-rs` currently materialises a scaled `DECIMAL(38, s)` through
/// `rust_decimal::Decimal`. That type only holds 96 bits, while DuckDB's decimal holds 128, and its
/// `from_i128_with_scale` constructor panics before we can render the cell. Given an executed
/// statement, name the outer projection that asks DuckDB to format those columns as text, or
/// `None` when no column needs it. This preserves the established JSON contract for exact wide
/// numbers and turns an ordinary SQL result into an ordinary SQL result rather than a request-thread
/// panic (#1433).
fn decimal_safe_projection(statement: Option<&duckdb::Statement<'_>>, sql: &str) -> Option<String> {
    let statement = statement?;
    let columns: Vec<(String, bool)> = statement
        .column_names()
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let scaled_decimal = matches!(
                statement.column_type(i),
                DataType::Decimal128(_, scale) if scale != 0
            );
            (name.to_string(), scaled_decimal)
        })
        .collect();

    if !columns.iter().any(|(_, scaled_decimal)| *scaled_decimal) {
        return None;
    }

    let projection = columns
        .iter()
        .map(|(name, scaled_decimal)| {
            let ident = quote_identifier(name);
            if *scaled_decimal {
                format!("CAST({ident} AS VARCHAR) AS {ident}")
            } else {
                ident
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let inner = crate::analytics::without_trailing_statement_terminator(sql);
    Some(format!(
        "SELECT {projection} FROM ({inner}) AS \"__nuthatch_decimal_source\""
    ))
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Create a temp table for one logical table's hot rows and append them, typed to match the sealed
/// Parquet (so `UNION ALL BY NAME` lines up). Columns are the sorted union of the rows' JSON keys -
/// exactly how `seal::rows_to_batch` derives the Parquet schema - so no `schema.json` is required.
/// Value marshalling mirrors seal exactly: counter columns are `u64` (0 if absent), every other column
/// is the JSON string as-is, or the JSON value stringified, or NULL when absent/null.
fn load_hot_temp(conn: &Connection, name: &str, rows: &[&Value]) -> Result<()> {
    let mut columns: BTreeSet<String> = BTreeSet::new();
    for r in rows {
        if let Some(obj) = r.as_object() {
            columns.extend(obj.keys().cloned());
        }
    }
    let columns: Vec<String> = columns.into_iter().collect();
    if columns.is_empty() {
        bail!("hot rows have no columns");
    }
    let coldefs: Vec<String> = columns
        .iter()
        .map(|c| format!("\"{c}\" {}", crate::analytics::hot_col_type(c)))
        .collect();
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS \"{name}\"; CREATE TEMP TABLE \"{name}\" ({})",
        coldefs.join(", ")
    ))?;
    let mut app = conn.appender(name)?;
    for row in rows {
        let vals: Vec<DuckValue> = columns
            .iter()
            .map(|c| json_to_duck(row.get(c), c))
            .collect();
        let refs: Vec<&dyn duckdb::ToSql> = vals.iter().map(|v| v as &dyn duckdb::ToSql).collect();
        app.append_row(refs.as_slice())?;
    }
    app.flush()?;
    Ok(())
}

/// One JSON cell → a DuckDB value, mirroring `seal::rows_to_batch`'s marshalling for a matching schema.
fn json_to_duck(v: Option<&Value>, col: &str) -> DuckValue {
    if crate::analytics::hot_col_type(col) == "UBIGINT" {
        DuckValue::UBigInt(v.and_then(Value::as_u64).unwrap_or(0))
    } else {
        match v {
            Some(Value::String(s)) => DuckValue::Text(s.clone()),
            None | Some(Value::Null) => DuckValue::Null,
            Some(other) => DuckValue::Text(other.to_string()),
        }
    }
}

/// Wrap a row source so every declared column is present in its schema, NULL-filled where no input
/// carries it.
///
/// COR-2's `union_by_name=true` only unions the schemas of the *listed inputs*, and `derived_bigint_cols`
/// projects its casts one level above them - so a `word16`/`word32` column that **no** input carries is
/// referenced by a cast and bound by nothing, the whole-view DDL fails on `Referenced column not found`,
/// and the table disappears from `/sql` entirely (#434). That is not an exotic state: it is every nest
/// between `schema.json` gaining a big-int column and the first segment carrying it sealing. One input
/// out of N carrying the column already worked, which is what made this easy to believe was covered.
///
/// #729 broadens this from big-integer columns to every declared column, for the same reason with a
/// quieter failure mode: a plain column no listed input carries doesn't fail the DDL (nothing above it
/// casts it) - `SELECT *` simply omits it, so the view builds "successfully" and is silently missing the
/// column, exactly the state #729 reported for a `schema.json` reconciled with a re-fetched ABI whose
/// new field predates every currently-sealed segment. Confirmed against DuckDB directly (not assumed):
/// `read_parquet([...], union_by_name=true)` NULL-fills a column across the *listed* files that carry it
/// unevenly, but a column *no listed file* carries is absent from the result schema outright, and an
/// explicit `SELECT that_col` against it is a binder error, not a NULL row - so the fix is this same
/// stub, one level up, for every declared column rather than only the bigint-derived ones.
///
/// A zero-row typed branch fixes it where the drift belongs - inside the union, so the column is
/// NULL-filled exactly as a partially-present one is, rather than by weakening the cast. `WHERE false`
/// contributes schema and no rows, and it costs no extra scan or bind of the segments themselves.
/// Types come from `hot_col_type` (COR-4: by column *name*), matching `empty_view_ddl` and the hot temp
/// table, so a column does not change type the instant its first segment seals. Stubbing a column the
/// input already carries is harmless - `UNION ALL BY NAME` merges same-named columns from both sides
/// rather than duplicating them - so this does not need to special-case which columns are actually
/// missing from `from_item`.
pub(crate) fn with_declared_base_cols(from_item: &str, cols: &[(String, String)]) -> String {
    let stubs: Vec<String> = cols
        .iter()
        .map(|(c, _)| {
            format!(
                "CAST(NULL AS {}) AS \"{c}\"",
                crate::analytics::hot_col_type(c)
            )
        })
        .collect();
    if stubs.is_empty() {
        return from_item.to_string();
    }
    format!(
        "(SELECT * FROM {from_item} UNION ALL BY NAME SELECT {} WHERE false)",
        stubs.join(", ")
    )
}

/// An empty but correctly-typed view for a declared table that has no sealed segment yet, so a nest
/// view that references it (or UNIONs it with a table that *does* have data) resolves instead of
/// silently vanishing. Columns and their `*_dec`/`*_overflow` siblings match the sealed view's shape;
/// `WHERE false` yields zero rows.
pub(crate) fn empty_view_ddl(table: &str, cols: &[(String, String)]) -> String {
    let mut sel: Vec<String> = Vec::new();
    for (name, storage) in cols {
        // COR-4: type by column NAME (`hot_col_type`), exactly as `seal::rows_to_batch` and the hot temp
        // table do - only the four counter columns are UBIGINT, everything else (incl. a `u64`-storage
        // event field like a `uint24`) is VARCHAR. Typing by *storage* here made a column flip type the
        // instant the first row sealed (`AVG(fee)` valid empty, erroring once populated).
        let ty = crate::analytics::hot_col_type(name);
        sel.push(format!("CAST(NULL AS {ty}) AS \"{name}\""));
        if crate::analytics::is_bigint(storage) {
            sel.push(format!("CAST(NULL AS DECIMAL(38,0)) AS \"{name}_dec\""));
            sel.push(format!("CAST(NULL AS BOOLEAN) AS \"{name}_overflow\""));
        }
    }
    format!(
        "CREATE OR REPLACE VIEW \"{table}\" AS SELECT {} WHERE false",
        sel.join(", ")
    )
}

fn value_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => Value::from(i),
        ValueRef::SmallInt(i) => Value::from(i),
        ValueRef::Int(i) => Value::from(i),
        ValueRef::BigInt(i) => Value::from(i),
        ValueRef::UTinyInt(i) => Value::from(i),
        ValueRef::USmallInt(i) => Value::from(i),
        ValueRef::UInt(i) => Value::from(i),
        ValueRef::UBigInt(i) => Value::from(i),
        ValueRef::Float(f) => Value::from(f),
        ValueRef::Double(f) => Value::from(f),
        ValueRef::HugeInt(i) => Value::String(i.to_string()),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        // Timestamps, decimals, nested types etc. - stringify for the skeleton surface.
        other => Value::String(format!("{other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unconfigured process still opens DuckDB at today's 512 MB / 2 threads. Raising the permit
    /// count no longer silently shrinks the per-connection ceiling: that product is the startup
    /// validator's job (RFC-0047 C4).
    #[test]
    fn unconfigured_duckdb_still_opens_at_todays_walls() {
        let resources = crate::analytics_budget::from_env();
        assert_eq!(resources.memory_limit_mb, 512);
        assert_eq!(resources.threads, 2);
        assert!(resources.temp_directory.is_none());
        assert!(resources.max_temp_size.is_none());
        let dir = tempfile::tempdir().unwrap();
        let (conn, spill) = open_locked_duckdb(dir.path()).unwrap();
        let mem: String = conn
            .query_row("SELECT current_setting('memory_limit')", [], |r| r.get(0))
            .unwrap();
        let threads: i64 = conn
            .query_row("SELECT current_setting('threads')", [], |r| r.get(0))
            .unwrap();
        let temp: String = conn
            .query_row("SELECT current_setting('temp_directory')", [], |r| r.get(0))
            .unwrap();
        // DuckDB prints `512MB` as `488.2 MiB` (SI mega, not mebi). Either spelling is the wall.
        assert!(
            mem.to_ascii_uppercase().contains("512") || mem.contains("488"),
            "unconfigured max_memory must still be 512MB, got {mem}"
        );
        assert_eq!(
            threads, 2,
            "unconfigured threads must still be 2, got {threads}"
        );
        assert_eq!(
            PathBuf::from(&temp),
            spill.0,
            "spill must stay a private per-instance directory (#1165), got {temp}"
        );
        assert!(
            spill
                .0
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("nuthatch-duckdb-{}-", std::process::id())),
            "spill name must stay nuthatch-duckdb-{{pid}}-{{seq}}: {}",
            spill.0.display()
        );
    }

    /// #289: the directory lockdown, configured the way `run` configures it, must refuse an
    /// out-of-allowlist read *on its own*. The denylist is still the primary control; this is the
    /// second layer. Deleting `enable_external_access(false)` from `open_locked_duckdb` fails this.
    #[test]
    fn the_directory_lockdown_blocks_an_out_of_allowlist_file_read() {
        let nest = tempfile::tempdir().unwrap();
        let segments = nest.path().join(crate::seal::SEGMENTS_DIR);
        std::fs::create_dir_all(&segments).unwrap();
        let secret = tempfile::tempdir().unwrap();
        let secret_file = secret.path().join("nuthatch.toml");
        std::fs::write(&secret_file, "[nest]\napi_key = \"hunter2\"\n").unwrap();
        let sql = format!("SELECT * FROM read_text('{}')", secret_file.display());

        assert!(
            crate::analytics::reject_file_access(&sql).is_err(),
            "the denylist must still refuse read_text - it is the control in front"
        );

        let (conn, _spill) = open_locked_duckdb(nest.path()).unwrap();
        let read_succeeded = match conn.prepare(&sql) {
            Ok(mut stmt) => stmt.query_row([], |r| r.get::<_, String>(0)).is_ok(),
            Err(_) => false,
        };
        assert!(
            !read_succeeded,
            "allowed_directories + enable_external_access=false must refuse an out-of-allowlist read"
        );

        assert!(
            conn.execute_batch("SET allowed_directories=['/'];")
                .is_err(),
            "lock_configuration must prevent widening file access"
        );

        // A file inside the allow-list still reads: the lockdown is a restriction, not a total ban.
        let allowed_file = segments.join("ok.txt");
        std::fs::write(&allowed_file, "ok\n").unwrap();
        let ok_sql = format!("SELECT * FROM read_text('{}')", allowed_file.display());
        let mut stmt = conn
            .prepare(&ok_sql)
            .expect("in-allowlist read_text prepares");
        let got: String = stmt
            .query_row([], |r| r.get(0))
            .expect("in-allowlist read_text runs");
        assert!(got.contains("ok"), "got {got:?}");
    }

    /// Runtime layout: Parquet is at `<root>/segments/`, the nest dir is `<root>/data/<nid>/`.
    /// Locking only the nest dir made every mounted `/sql` return empty (#289, e2e_early_cutoff).
    #[test]
    fn the_lockdown_allows_the_shared_segment_store() {
        let root = tempfile::tempdir().unwrap();
        let nid_dir = root.path().join("data").join("nid");
        std::fs::create_dir_all(&nid_dir).unwrap();
        let shared = root.path().join(crate::seal::SEGMENTS_DIR);
        std::fs::create_dir_all(&shared).unwrap();
        let file = shared.join("ok.txt");
        std::fs::write(&file, "shared\n").unwrap();
        let (conn, _spill) = open_locked_duckdb(&nid_dir).unwrap();
        let sql = format!("SELECT content FROM read_text('{}')", file.display());
        let mut stmt = conn.prepare(&sql).expect("shared-store read_text prepares");
        let got: String = stmt
            .query_row([], |r| r.get(0))
            .expect("shared-store read_text runs");
        assert_eq!(got.trim(), "shared");
    }
}
