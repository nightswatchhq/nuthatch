//! Read-only analytical SQL over the sealed Parquet segments **and the hot tip**, via an embedded
//! DuckDB. DuckDB is single-writer/OLAP: we only ever ATTACH the segments read-only here; the
//! ingestion path never writes DuckDB. The sealed segments cover finalized history; the unsealed tip
//! lives in redb. For `/sql` (RFC-0013) the hot rows are scanned into per-table temp tables and
//! `UNION ALL`'d into each table's view. Hot and cold are kept disjoint *structurally* by the
//! `sealed_through` watermark (COR-1): cold includes only segments finalized at/below it, hot only rows
//! past it - so the union is exact with no dedup, even across the brief seal→prune window. Trusted
//! point-reads pass no hot rows (and `u64::MAX`, i.e. all segments).
//!
//! The binary stays single-file: DuckDB is statically bundled. Memory is capped so an analytical
//! query can't blow the embedded-mode RAM budget.

use anyhow::{bail, Context, Result};
use duckdb::types::{Value as DuckValue, ValueRef};
use duckdb::Connection;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// Cap DuckDB's working memory so `/sql` can't breach the embedded footprint budget.
const MEM_LIMIT: &str = "512MB";
const MAX_THREADS: u32 = 2;

/// A resource guard for the untrusted `/sql` surface: a hard wall-clock deadline (enforced by
/// interrupting the running DuckDB query) and a cap on materialised rows. Trusted internal callers
/// (`net_balances`, `get_row`) run *unguarded* - their SQL is registry-built, never user text, and
/// they must run to completion. Access control (who may query, per-caller quotas) is deliberately
/// *not* here: that needs caller identity a sovereign single-tenant node doesn't have - it's a
/// gateway's job. This guard is only about the node protecting itself from any single query.
#[derive(Clone, Copy)]
pub struct QueryGuard {
    pub timeout: Duration,
    pub max_rows: usize,
}

/// The result of a query: the rows, plus whether a guard's row cap truncated them.
#[derive(Debug)]
pub struct QueryOutput {
    pub rows: Vec<Value>,
    pub truncated: bool,
}

/// Hot (unsealed) rows grouped by logical table - from [`crate::store::Store::hot_rows_by_table`].
/// Passed to the query path so the live tip is `UNION ALL`'d into each table's view (RFC-0013).
pub type HotRows = std::collections::HashMap<String, Vec<Value>>;

/// Run a read-only query to completion. Only SELECT/WITH statements are accepted - this is a query
/// surface, not a mutation surface. Unguarded: for trusted, registry-built SQL that must finish.
pub fn query(dir: &Path, sql: &str) -> Result<Vec<Value>> {
    Ok(run(dir, sql, None, &HotRows::new(), u64::MAX)?.rows)
}

/// Run a trusted read-only query over **only the segments finalized at/below `sealed_through`** (the
/// same watermark filter `define_views` applies). The warm-restart view rebuilds use this instead of
/// [`query`] (which reads *every* segment): their cold seed must stay disjoint from the hot replay, and
/// a crash in the seal->prune window leaves already-sealed rows still in the hot store. Folding all
/// segments here would then count those rows twice - permanently double-counting balances and the
/// compliance exposure/velocity views. Bounding to the persisted watermark keeps cold (<= watermark)
/// and hot (everything still in the store) partitioned regardless of crash timing.
fn query_cold(dir: &Path, sql: &str, sealed_through: u64) -> Result<Vec<Value>> {
    Ok(run(dir, sql, None, &HotRows::new(), sealed_through)?.rows)
}

/// Run a read-only query under a resource guard, over the **sealed segments only** - the cold path used
/// by trusted callers and the `/table` endpoint's cold fill (which merges hot itself). See [`QueryGuard`].
pub fn query_guarded(dir: &Path, sql: &str, guard: QueryGuard) -> Result<QueryOutput> {
    // Cold-only: `u64::MAX` includes every sealed segment (no hot rows to keep disjoint from).
    run(dir, sql, Some(guard), &HotRows::new(), u64::MAX)
}

/// Run a guarded read-only query over the sealed segments **and the hot tip** - the public `/sql`
/// surface (RFC-0013). `hot` is the unsealed rows grouped by table; each is `UNION ALL`'d into its
/// table's view. A query outliving `guard.timeout` is interrupted; a result past `guard.max_rows` is
/// truncated and flagged.
pub fn query_hot_cold(
    dir: &Path,
    sql: &str,
    guard: QueryGuard,
    hot: &HotRows,
    sealed_through: u64,
) -> Result<QueryOutput> {
    run(dir, sql, Some(guard), hot, sealed_through)
}

fn run(
    dir: &Path,
    sql: &str,
    guard: Option<QueryGuard>,
    hot: &HotRows,
    sealed_through: u64,
) -> Result<QueryOutput> {
    // Check the first *statement keyword*, past any leading whitespace and SQL comments - a query
    // that opens with `-- note` or `/* … */` is still a SELECT. DuckDB gets the original text.
    let head = strip_leading_sql_comments(sql).to_ascii_lowercase();
    if !(head.starts_with("select") || head.starts_with("with")) {
        bail!("only SELECT/WITH queries are allowed on the read-only SQL surface");
    }
    // Read-only is enforced three-deep - do NOT loosen any of these without re-reasoning SEC-7:
    //   1. this leading-keyword gate rejects a *statement* that opens with INSERT/UPDATE/DELETE/COPY/
    //      ATTACH/PRAGMA/… (a `WITH cte AS (…) INSERT …` is the only way DML could ride a `with`
    //      prefix, and DuckDB won't parse INSERT/COPY *inside* a CTE/subquery);
    //   2. `reject_statement_stacking` refuses a `;`-stacked second statement. This used to say
    //      "`conn.prepare` is single-statement" - it is NOT (the bundled duckdb-rs prepares AND runs
    //      `SELECT 1; INSERT …`), which made a stacked `COPY … TO` an arbitrary file write. See that
    //      function's docs;
    //   3. the connection is a fresh in-memory instance whose only tables are read-only views over
    //      Parquet plus an ephemeral hot temp table, so even a hypothetical write has no durable target.
    // `COPY … TO` (a file write) must *lead* the statement, which (1) blocks.
    // SEC-2: refuse DuckDB filesystem/network table functions (`read_text`, `glob`, …) - they read
    // files from inside a plain SELECT, past the keyword gate, and would otherwise leak any file the
    // process can read (e.g. `nuthatch.toml`'s secrets). This is the primary control; the
    // `allowed_directories` lockdown below is defense-in-depth (its runtime enforcement is
    // version-dependent in the bundled DuckDB).
    reject_statement_stacking(sql)?;
    reject_file_access(sql)?;
    reject_replacement_scan(sql)?;

    let conn = Connection::open_in_memory().context("failed to open DuckDB")?;
    // **The allowlist, and the control that is meant to outlive the others** (audit finding 5).
    //
    // Everything above enumerates what is *forbidden*, over a vocabulary DuckDB grows every release.
    // That approach has now been wrong twice: about spelling (`"read_csv"(…)` slipped past a check
    // that expected `(` after whitespace) and about coverage (`read_xlsx`, `st_read`, `iceberg_scan`
    // and friends were never listed and are inert only because those extensions are not bundled).
    // Both failures are silent, and the feedback loop is "someone exploits it".
    //
    // So this asks DuckDB's own parser what the query actually references and permits only what we
    // recognise. A new file-reading function added upstream tomorrow is refused by default, because it
    // is not on the list of things we allow - which is the property the denylist can never have.
    //
    // Kept *beside* the denylist rather than replacing it: two independent controls that must both
    // pass, so a gap in either is covered while this one earns trust.
    reject_unknown_table_refs(&conn, sql)?;
    conn.execute_batch(&format!(
        "SET memory_limit='{MEM_LIMIT}'; SET threads={MAX_THREADS};"
    ))
    .context("failed to configure DuckDB")?;
    // Defense-in-depth for SEC-2 (the query denylist above is the primary control): pin DuckDB's file
    // access to the nest's own data dirs (segments + labels, never the nest root that holds the config)
    // and `lock_configuration` so a query can't widen it.
    //
    // MEASURED, not assumed: on the DuckDB we currently bundle, `allowed_directories` does **not**
    // block an out-of-allowlist read - see
    // `tests::the_denylist_not_the_directory_lockdown_is_what_blocks_a_file_read`. So this layer buys
    // nothing today beyond `lock_configuration` (which does hold, preventing a query widening the
    // setting). It is kept because it costs nothing and becomes real if upstream starts enforcing it -
    // but `reject_file_access` is the control that actually stops a file read, and it must never be
    // weakened on the belief that this is behind it.
    let allowed: Vec<String> = [crate::seal::SEGMENTS_DIR, "labels"]
        .iter()
        .map(|sub| dir.join(sub))
        .filter(|p| p.exists())
        .map(|p| format!("'{}'", p.display().to_string().replace('\'', "''")))
        .collect();
    // `enable_external_access` is a startup-only setting, so we scope at runtime with
    // `allowed_directories` (an empty allowlist blocks all file access - the fresh-nest/tip-only case)
    // and freeze it with `lock_configuration` so the untrusted query can't widen it back.
    let lockdown = format!(
        "SET allowed_directories=[{}]; SET lock_configuration=true;",
        allowed.join(", ")
    );
    conn.execute_batch(&lockdown)
        .context("failed to lock down DuckDB filesystem access")?;
    define_views(&conn, dir, hot, sealed_through)?;
    // A nest can ship derived-entity views (`views/*.sql`) that build on the per-event tables; the
    // analytical `/sql` surface sees them. Point-reads (`net_balances`, `get_row`) deliberately skip
    // this - they only touch the raw per-event tables.
    define_nest_views(&conn, dir);
    // The compliance substrate: expose imported label snapshots as a `labels` view so `/sql` (and the
    // internal `cold_exposure` fold) can join against them. Best-effort - no snapshots, no view.
    define_labels_view(&conn, dir);
    // Factory nests (RFC-0009): a `{template}__children` view over the sealed factory events, so
    // "which pools, discovered when, by which parent" is one query. Best-effort - no factories, no-op.
    define_children_views(&conn, dir);

    // Hard wall-clock deadline for the untrusted surface: a watchdog thread interrupts the in-flight
    // query once it outlives the guard's timeout (a cartesian blow-up can't be stopped by the memory
    // cap alone). `interrupt()` makes the running query fail; we translate that into a clear timeout
    // error below. On normal completion we signal the watchdog so it never fires. Unguarded (trusted)
    // queries skip all of this and run to completion.
    let interrupted = Arc::new(AtomicBool::new(false));
    let watchdog = guard.map(|g| {
        let handle = conn.interrupt_handle();
        let flag = interrupted.clone();
        let (tx, rx) = mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            // Only a genuine timeout interrupts; a value (normal completion) or a dropped sender
            // (panic) leaves the query alone.
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(g.timeout) {
                flag.store(true, Ordering::SeqCst);
                handle.interrupt();
            }
        });
        (tx, join)
    });

    let cap = guard.map(|g| g.max_rows);
    let outcome = collect(&conn, sql, cap);

    // Stop the watchdog before interpreting the result: a value arriving before the deadline makes
    // `recv_timeout` return `Ok`, so it won't interrupt; then join so it can't fire late.
    if let Some((tx, join)) = watchdog {
        let _ = tx.send(());
        let _ = join.join();
    }

    let (mut rows, over_cap) = match outcome {
        Ok(v) => v,
        Err(e) => {
            if interrupted.load(Ordering::SeqCst) {
                let secs = guard.map(|g| g.timeout.as_secs()).unwrap_or(0);
                bail!("query exceeded the {secs}s time budget on the read-only SQL surface");
            }
            return Err(e);
        }
    };

    let truncated = match cap {
        Some(max) if over_cap => {
            rows.truncate(max);
            true
        }
        _ => false,
    };
    Ok(QueryOutput { rows, truncated })
}

/// Prepare, execute and materialise the result. With `cap = Some(n)` it stops after `n + 1` rows so
/// the caller can report truncation precisely (the returned bool is true when that extra row existed,
/// i.e. more than `n` rows were available); the caller then truncates back to `n`. `cap = None`
/// materialises every row. Row materialisation is Rust-side and escapes DuckDB's own memory limit,
/// so the cap is what actually bounds a `SELECT *` result buffer.
fn collect(conn: &Connection, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool)> {
    let mut stmt = conn.prepare(sql).context("failed to prepare query")?;
    let mut rows = stmt.query([]).context("query failed")?;
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
    while let Some(row) = rows.next().context("row read failed")? {
        let mut obj = Map::new();
        for (i, name) in column_names.iter().enumerate() {
            let v = value_to_json(row.get_ref(i)?);
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

/// The per-result Rust-side byte ceiling for the guarded `/sql` surface (64 MiB). Comfortably above any
/// legitimate 50k-row result, far below the per-cursor RAM budget - the backstop against a wide-cell
/// `SELECT` inflating the materialised buffer past the budget (see `collect`).
const SQL_MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;

/// A cheap lower-bound byte estimate of a materialised cell - dominated by string payloads, which is
/// exactly the wide-cell attack vector. Numbers/bools/null count a small fixed cost.
fn value_bytes(v: &Value) -> usize {
    match v {
        Value::String(s) => s.len(),
        Value::Array(a) => 8 + a.iter().map(value_bytes).sum::<usize>(),
        Value::Object(o) => {
            8 + o
                .iter()
                .map(|(k, x)| k.len() + value_bytes(x))
                .sum::<usize>()
        }
        _ => 8,
    }
}

/// Skip leading whitespace and SQL comments (`-- line` and `/* block */`) so the read-only guard
/// sees the first real keyword. Returns the remainder starting at that keyword.
fn strip_leading_sql_comments(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = match rest.find('\n') {
                Some(i) => rest[i + 1..].trim_start(),
                None => "",
            };
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = match rest.find("*/") {
                Some(i) => rest[i + 2..].trim_start(),
                None => "",
            };
        } else {
            return s;
        }
    }
}

/// DuckDB table functions that read the filesystem or network - usable inside a plain SELECT, so the
/// read-only keyword gate doesn't stop them (SEC-2). Legit `/sql` hits the per-table views, never these.
const FORBIDDEN_FNS: &[&str] = &[
    "read_text",
    "read_blob",
    "read_csv",
    "read_csv_auto",
    "read_json",
    "read_json_auto",
    "read_json_objects",
    "read_ndjson",
    "read_parquet",
    "parquet_scan",
    "parquet_metadata",
    "parquet_schema",
    "parquet_kv_metadata",
    "csv_scan",
    "glob",
    "sniff_csv",
    // **Audit finding 4**: extension-gated readers. Inert today only because those extensions are not
    // in the bundled build - i.e. safe by build configuration rather than by policy. Bundling one, or
    // DuckDB promoting one to core, would turn each into a live file read with no change on our side.
    // The AST allowlist already refuses them; listing them keeps the two controls agreeing.
    "read_xlsx",
    "st_read",
    "st_readosm",
    "iceberg_scan",
    "delta_scan",
    "postgres_scan",
    "postgres_query",
    "sqlite_scan",
    "mysql_scan",
    "mysql_query",
    // **Audit finding 2**: environment disclosure. Measured on an untrusted `/sql`, these return the
    // absolute `secret_directory` (which embeds the OS username), the temp and extension directories,
    // and the exact state of the sandbox. Not a file read - free reconnaissance for someone looking
    // for one, and there is no legitimate reason a nest query needs them.
    "duckdb_settings",
    "duckdb_extensions",
    "duckdb_secrets",
    "duckdb_databases",
    "duckdb_temporary_files",
    "getenv",
];

/// Strip all SQL comments (line `--…` and block `/* … */`) so a function call can't be split or hidden
/// by a comment before the denylist scan. Deliberately naive about string literals - over-stripping a
/// query with `--`/`/*` inside a string just makes it invalid (rejected), which is the safe direction.
fn strip_all_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
            out.push(' ');
        } else {
            out.push(b[i] as char);
            i += 1;
        }
    }
    out
}

/// Refuse a query that *calls* any [`FORBIDDEN_FNS`] function. Comments are stripped first, then each
/// name is matched only when it's a real call: a word boundary before it and (after optional
/// whitespace) a `(` after it - so a table or column merely *named* like one (e.g. `pool__glob`) is
/// fine, while `read_text/**/('…')` and `READ_TEXT (…)` are both caught. (SEC-2, primary control.)
/// Refuse a `;`-stacked second statement (SEC-7, and a **real** hole found by the audit-tail test work).
///
/// The read-only story used to rest on three layers, and the second one did not exist:
///   1. the leading-keyword gate inspects only the FIRST statement, so `SELECT 1; COPY …` sails past it;
///   2. `conn.prepare` was documented as single-statement. **It is not.** The bundled duckdb-rs prepares
///      `SELECT 1; INSERT INTO t VALUES (99)` happily and *executes the INSERT*;
///   3. the in-memory connection has no durable tables - but `COPY … TO 'path'` and `ATTACH 'path'`
///      write to the filesystem regardless of what the connection holds.
///
/// Composed, that was an arbitrary **file-write** primitive on an unauthenticated GET surface:
/// `SELECT 1; COPY (SELECT 1) TO '/home/user/.zshrc'` wrote the file. Verified end-to-end through
/// `query` before this guard existed.
///
/// So statement stacking is now rejected here, in our own code, rather than delegated to a DuckDB
/// behaviour we do not control. A trailing `;` (with only whitespace after it) is fine - that is how
/// people habitually end a query - but anything following one is refused.
///
/// String-literal aware, because `SELECT ';'` is a perfectly legal query. Single quotes with `''`
/// escaping and double-quoted identifiers are both tracked. Dollar-quoting is NOT parsed: a `;` inside
/// a `$$…$$` block is treated as a statement separator and refused, which fails safe.
fn reject_statement_stacking(sql: &str) -> Result<()> {
    let cleaned = strip_all_sql_comments(sql);
    let b = cleaned.as_bytes();
    let mut i = 0;
    let (mut in_single, mut in_double) = (false, false);
    while i < b.len() {
        match b[i] {
            b'\'' if !in_double => {
                // `''` inside a literal is an escaped quote, not a terminator.
                if in_single && i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            b'"' if !in_single => in_double = !in_double,
            b';' if !in_single && !in_double => {
                if cleaned[i + 1..].trim().is_empty() {
                    return Ok(()); // a trailing semicolon, nothing behind it
                }
                bail!(
                    "the read-only SQL surface accepts a single statement; `;`-stacking is refused \
                     (only the first statement is checked for read-only-ness, so a stacked second one \
                     could write files)"
                );
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Table functions a query may legitimately call. Everything else is refused.
///
/// Deliberately tiny. Nuthatch's data reaches a query through views *we* define, so a user query needs
/// no table function at all to do its job - these exist because ordinary analytical SQL uses them for
/// generating rows, not for reaching data. Adding to this list means asserting a function cannot read a
/// file, open a socket, or leak the environment.
const ALLOWED_TABLE_FNS: &[&str] = &["generate_series", "range", "unnest"];

/// Ask DuckDB's parser what the statement references, and refuse anything unrecognised.
///
/// Two rules, both derived from what the AST actually looks like (measured, not assumed):
///
/// - A **table function** must be in [`ALLOWED_TABLE_FNS`]. Quoting collapses here for free:
///   `read_csv(…)` and `"read_csv"(…)` both parse to `TABLE_FUNCTION` with the same name, so the
///   evasion that defeated the textual denylist is not expressible.
/// - A **base table** must be named like an identifier. A DuckDB *replacement scan* - `FROM
///   '/x.parquet'` - parses as a `BASE_TABLE` whose name is the path, so the AST alone does not
///   distinguish it; requiring `[A-Za-z0-9_]` does, and no legitimate view of ours is named otherwise.
///
/// Fails **open** if the parse is unavailable: `json_serialize_sql` is a DuckDB feature and this is the
/// newer of two controls. A parse failure must not take down `/sql` while the denylist - which has
/// guarded this surface since RFC-0008 - is still in front of it.
fn reject_unknown_table_refs(conn: &Connection, sql: &str) -> Result<()> {
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let Ok(ast) = conn.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
        r.get::<_, String>(0)
    }) else {
        return Ok(());
    };
    let Ok(v) = serde_json::from_str::<Value>(&ast) else {
        return Ok(());
    };
    if v.get("error").and_then(Value::as_bool) == Some(true) {
        // DuckDB could not parse it. Let it say so itself, with its own error message.
        return Ok(());
    }
    let mut bad: Option<String> = None;
    walk_table_refs(&v, &mut |kind, name| {
        if bad.is_some() {
            return;
        }
        match kind {
            "TABLE_FUNCTION" => {
                let f = name.to_ascii_lowercase();
                if !ALLOWED_TABLE_FNS.contains(&f.as_str()) {
                    bad = Some(format!("table function `{name}` is not permitted here"));
                }
            }
            // A DuckDB *replacement scan* (`FROM '/x.parquet'`) parses as a BASE_TABLE whose name is
            // the path, so the AST alone cannot tell it from a real table - the name has to be checked.
            "BASE_TABLE"
                if name.is_empty()
                    || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                bad = Some(format!(
                    "`{name}` is not a table name - a quoted path in table position reads a file"
                ));
            }
            _ => {}
        }
    });
    match bad {
        Some(why) => bail!("{why} - the SQL surface serves this nest's tables and views only"),
        None => Ok(()),
    }
}

/// Walk the serialized AST, calling `f(kind, name)` for every table reference found.
fn walk_table_refs(v: &Value, f: &mut impl FnMut(&str, &str)) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(t)) = map.get("type") {
                if t == "TABLE_FUNCTION" {
                    // The callee's name lives on the nested function expression.
                    if let Some(name) = map
                        .get("function")
                        .and_then(|fun| fun.get("function_name"))
                        .and_then(Value::as_str)
                    {
                        f("TABLE_FUNCTION", name);
                    }
                } else if t == "BASE_TABLE" {
                    if let Some(name) = map.get("table_name").and_then(Value::as_str) {
                        f("BASE_TABLE", name);
                    }
                }
            }
            for child in map.values() {
                walk_table_refs(child, f);
            }
        }
        Value::Array(items) => {
            for child in items {
                walk_table_refs(child, f);
            }
        }
        _ => {}
    }
}

fn reject_file_access(sql: &str) -> Result<()> {
    // **Double quotes are removed before scanning.** DuckDB accepts a quoted function name and calls
    // it exactly as the bare form, so `"read_csv"('/etc/passwd')` executed while sailing past a check
    // that looked for `(` after optional *whitespace* - a quote is not whitespace. Verified against a
    // live DuckDB during the pre-1.0 adversary pass: the quoted form returned the file's contents.
    //
    // Stripping is the robust fix rather than "also skip quotes when seeking `(`", because it
    // normalises every placement at once - `"read_csv"(`, `read"_"csv(`, and anything else quoting can
    // do to break a name into pieces. It can only ever make the denylist match *more*, and a denylist
    // that over-refuses is the safe direction: the cost is a rejected query with a bizarre quoted
    // identifier, and the alternative cost is reading /etc/passwd.
    let cleaned = strip_all_sql_comments(sql)
        .to_ascii_lowercase()
        .replace('"', "");
    let b = cleaned.as_bytes();
    let is_ident = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    for name in FORBIDDEN_FNS {
        let mut from = 0;
        while let Some(pos) = cleaned[from..].find(name) {
            let start = from + pos;
            let end = start + name.len();
            let boundary_before = start == 0 || !is_ident(b[start - 1]);
            let mut j = end;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let is_call = j < b.len() && b[j] == b'(';
            if boundary_before && is_call {
                bail!("query uses forbidden filesystem/network function `{name}` - refused");
            }
            from = end;
        }
    }
    Ok(())
}

/// Refuse a DuckDB **replacement scan**: a bare string literal in table position (`FROM '/x.parquet'`,
/// `JOIN '…'`) makes DuckDB read that file with *no function name* for [`reject_file_access`] to match,
/// bypassing the denylist entirely. A legitimate query names a view or a subquery after FROM/JOIN, never
/// a single-quoted string (a double-quoted identifier is fine and untouched) - so rejecting a
/// single-quote as the first non-space token after a word-bounded FROM/JOIN closes the bypass without
/// affecting real queries. Comments are stripped first, mirroring the denylist scan.
fn reject_replacement_scan(sql: &str) -> Result<()> {
    let cleaned = strip_all_sql_comments(sql).to_ascii_lowercase();
    let b = cleaned.as_bytes();
    let is_ident = |c: u8| c == b'_' || c.is_ascii_alphanumeric();
    for kw in ["from", "join"] {
        let mut from = 0;
        while let Some(pos) = cleaned[from..].find(kw) {
            let start = from + pos;
            let end = start + kw.len();
            let boundary_before = start == 0 || !is_ident(b[start - 1]);
            let mut j = end;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            // A single-quote as the first token after a word-bounded FROM/JOIN is a file replacement
            // scan. (If the keyword is part of a larger identifier, the next char is an ident char, not a
            // quote, so this never false-positives on e.g. `fromage`.)
            if boundary_before && j < b.len() && b[j] == b'\'' {
                bail!("query reads a file via a `{kw} '…'` replacement scan - refused");
            }
            from = end;
        }
    }
    Ok(())
}

/// Net balance per address for one sealed transfer table, summed as i128 (DuckDB HUGEINT). This is
/// how the IVM view is re-seeded on restart: instead of replaying every sealed transfer through the
/// circuit, we let DuckDB fold each immutable segment down to one (address, net) row. Addresses
/// whose net is exactly zero are omitted (matching the view's drop-at-zero behaviour). `table` and
/// the column names come from the registry (`{alias}__transfer`; from/to/value column names vary by
/// token - USDC from/to/value, WETH src/dst/wad), never user text, so there is no injection surface.
pub fn net_balances(
    dir: &Path,
    table: &str,
    from_col: &str,
    to_col: &str,
    value_col: &str,
    sealed_through: u64,
) -> Result<Vec<(String, i128)>> {
    // `to` receives (+value), `from` sends (−value); TRY_CAST yields NULL (skipped) for the rare
    // value that overflows i128, mirroring the caller's i128 parse-or-skip.
    let sql = format!(
        "SELECT addr, SUM(d)::VARCHAR AS net FROM (\
           SELECT \"{to_col}\" AS addr, TRY_CAST(\"{value_col}\" AS HUGEINT) AS d FROM \"{table}\" \
           UNION ALL \
           SELECT \"{from_col}\" AS addr, -TRY_CAST(\"{value_col}\" AS HUGEINT) AS d FROM \"{table}\"\
         ) GROUP BY addr HAVING SUM(d) <> 0"
    );
    let mut out = Vec::new();
    for r in query_cold(dir, &sql, sealed_through)? {
        if let (Some(addr), Some(net)) = (r["addr"].as_str(), r["net"].as_str()) {
            if let Ok(n) = net.parse::<i128>() {
                out.push((addr.to_string(), n));
            }
        }
    }
    Ok(out)
}

/// Cold exposure fold (RFC-0008 C1): direct counterparty exposure to the labeled set for one sealed
/// transfer table, computed in DuckDB by joining the segments against the `labels` view. Mirrors
/// `net_balances` - it lets a restart re-seed the exposure view from immutable segments instead of
/// replaying every sealed transfer. Returns `(encoded_key, amount, count)` where the key is
/// `address\u{1f}label\u{1f}direction`, matching `exposure::seed_item`. `table`/column names are
/// registry-derived (never user text); addresses are lower-cased to match the label snapshots.
pub fn cold_exposure(
    dir: &Path,
    table: &str,
    from_col: &str,
    to_col: &str,
    value_col: &str,
    sealed_through: u64,
) -> Result<Vec<(String, i128, i128)>> {
    // Outbound: the sender has exposure to the labels of a labeled recipient. Inbound: the recipient
    // has exposure from the labels of a labeled sender. COUNT/SUM per (address, label, direction).
    let sql = format!(
        "SELECT addr, label, dir, SUM(d)::VARCHAR AS amount, COUNT(*) AS cnt FROM (\
           SELECT lower(t.\"{from_col}\") AS addr, l.label AS label, 'out' AS dir, \
                  TRY_CAST(t.\"{value_col}\" AS HUGEINT) AS d \
           FROM \"{table}\" t JOIN labels l ON lower(t.\"{to_col}\") = l.address \
           UNION ALL \
           SELECT lower(t.\"{to_col}\") AS addr, l.label, 'in', \
                  TRY_CAST(t.\"{value_col}\" AS HUGEINT) AS d \
           FROM \"{table}\" t JOIN labels l ON lower(t.\"{from_col}\") = l.address\
         ) GROUP BY addr, label, dir"
    );
    let mut out = Vec::new();
    for r in query_cold(dir, &sql, sealed_through)? {
        let (Some(addr), Some(label), Some(dir_s), Some(cnt)) = (
            r["addr"].as_str(),
            r["label"].as_str(),
            r["dir"].as_str(),
            r["cnt"].as_i64(),
        ) else {
            continue;
        };
        let amount = r["amount"]
            .as_str()
            .and_then(|s| s.parse::<i128>().ok())
            .unwrap_or(0);
        let key = format!("{addr}\u{1f}{label}\u{1f}{dir_s}");
        out.push((key, amount, cnt as i128));
    }
    Ok(out)
}

/// Cold velocity fold (RFC-0008 C3): per-address outbound volume + count per tumbling block-window,
/// summed in DuckDB over one sealed transfer table - the restart re-seed for the velocity view (as
/// `net_balances`/`cold_exposure` are for their views). Returns `(encoded_key, volume, count)` where
/// the key is `address\u{1f}window_start`, matching `velocity::seed_item`. Registry-derived names.
pub fn cold_velocity(
    dir: &Path,
    table: &str,
    from_col: &str,
    value_col: &str,
    window: u64,
    sealed_through: u64,
) -> Result<Vec<(String, i128, i128)>> {
    let w = window.max(1);
    // window_start = (block // W) * W; sum outbound volume + count per (sender, window).
    let sql = format!(
        "SELECT lower(\"{from_col}\") AS addr, (block_number / {w}) * {w} AS ws, \
                SUM(TRY_CAST(\"{value_col}\" AS HUGEINT))::VARCHAR AS vol, COUNT(*) AS cnt \
         FROM \"{table}\" GROUP BY addr, ws"
    );
    let mut out = Vec::new();
    for r in query_cold(dir, &sql, sealed_through)? {
        let (Some(addr), Some(ws), Some(cnt)) =
            (r["addr"].as_str(), r["ws"].as_u64(), r["cnt"].as_i64())
        else {
            continue;
        };
        let vol = r["vol"]
            .as_str()
            .and_then(|s| s.parse::<i128>().ok())
            .unwrap_or(0);
        out.push((format!("{addr}\u{1f}{ws}"), vol, cnt as i128));
    }
    Ok(out)
}

/// Define a read-only `labels` view over the content-addressed snapshots in `dir/labels/*.json`
/// (each a flat JSON array of `{address, label}`). No snapshots → no view, so joins against it are
/// only attempted when labels exist. Addresses are lower-cased for a clean join with decoded hex.
fn define_labels_view(conn: &Connection, dir: &Path) {
    let labels_dir = dir.join(crate::labels::LABELS_DIR);
    let has_snapshot = std::fs::read_dir(&labels_dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|x| x == "json"))
        })
        .unwrap_or(false);
    if !has_snapshot {
        return;
    }
    let glob = labels_dir.join("*.json");
    let ddl = format!(
        "CREATE VIEW labels AS SELECT lower(address) AS address, label \
         FROM read_json('{}', format='array', columns={{address: 'VARCHAR', label: 'VARCHAR'}})",
        glob.display()
    );
    if let Err(e) = conn.execute_batch(&ddl) {
        tracing::debug!("labels view skipped: {e}");
    }
}

/// Define a `{template}__children` view per template for a factory nest (RFC-0009 §Serving): the set
/// of discovered child contracts with their provenance (address, discovered block/log/timestamp,
/// parent), unioned across every factory that produces the template and de-duplicated to the earliest
/// discovery per address. Reads the nest's factory config from `nuthatch.toml`; best-effort, so a
/// factory table with no sealed events yet (only an empty typed view) just yields an empty children
/// view. Non-factory nests are a no-op.
fn define_children_views(conn: &Connection, dir: &Path) {
    let Ok(config) = crate::config::Config::load(dir) else {
        return;
    };
    if config.factories.is_empty() {
        return;
    }
    let Ok(fs) = crate::factory::FactorySet::build(&config) else {
        return;
    };
    // A timestamp-free nest (RFC-0029 §6b) has no `block_timestamp` to project, so the provenance
    // view drops `discovered_timestamp` rather than selecting a column that doesn't exist. Leaving
    // the reference in would make the whole view fail to create - and the failure is swallowed as a
    // `debug!` below, so a factory nest would silently lose `{template}__children` entirely. Omitted
    // rather than `0 AS discovered_timestamp` for the same reason the column itself is omitted: a
    // zero that looks like a timestamp is worse than an error.
    let (ts, cols) = if config.nest.block_timestamps {
        (
            "block_timestamp AS discovered_timestamp, ",
            "discovered_timestamp, ",
        )
    } else {
        ("", "")
    };

    let mut by_template: std::collections::BTreeMap<String, Vec<(String, String)>> =
        std::collections::BTreeMap::new();
    for (template, table, child_param) in fs.view_sources() {
        by_template
            .entry(template)
            .or_default()
            .push((table, child_param));
    }

    for (template, sources) in by_template {
        // `child_param`/`table` are registry-derived (never user text) → no injection surface.
        let selects: Vec<String> = sources
            .iter()
            .map(|(table, cp)| {
                format!(
                    "SELECT lower(\"{cp}\") AS address, block_number AS discovered_block, \
                     log_index AS discovered_log_index, {ts}\
                     lower(address) AS parent_address FROM \"{table}\""
                )
            })
            .collect();
        let union = selects.join(" UNION ALL ");
        let ddl = format!(
            "CREATE VIEW \"{template}__children\" AS \
             SELECT address, discovered_block, discovered_log_index, {cols}parent_address \
             FROM ({union}) \
             QUALIFY row_number() OVER (PARTITION BY address ORDER BY discovered_block, discovered_log_index) = 1"
        );
        if let Err(e) = conn.execute_batch(&ddl) {
            tracing::debug!("children view {template}__children skipped: {e}");
        }
    }
}

/// Point-read fallback: fetch a single sealed transfer by (block, log_index). Used when the hot
/// store has already pruned it. Integers are interpolated (not user text), so no injection surface.
pub fn get_row(dir: &Path, block: u64, log_index: u64) -> Result<Option<Value>> {
    let manifest = crate::seal::load_manifest(dir)?;
    for table in manifest.tables.keys() {
        let sql = format!(
            "SELECT * FROM \"{table}\" WHERE block_number = {block} AND log_index = {log_index} LIMIT 1"
        );
        if let Some(row) = query(dir, &sql)?.into_iter().next() {
            return Ok(Some(row));
        }
    }
    Ok(None)
}

/// Expose each table's sealed segments as a read-only DuckDB view named after the table. Tables with
/// no sealed segments yet simply have no view (they hold only unsealed tip data, served from hot).
///
/// Big-integer columns (uint/int > 64 bits) are stored as exact text (canonical form). For ergonomic
/// SQL (RFC-0001 §2) each such column `c` gets two derived view columns: `c_dec` - the value as
/// `DECIMAL(38,0)` when it fits, else NULL - and `c_overflow` - true when the exact value exceeds
/// 38 digits (so `c_dec` is NULL but `c` isn't). Analytics can `SUM(c_dec)` without hand-casting.
fn define_views(conn: &Connection, dir: &Path, hot: &HotRows, sealed_through: u64) -> Result<()> {
    let manifest = crate::seal::load_manifest(dir)?;
    let schema = schema_columns(dir);
    let cols_of = |table: &str| -> &[(String, String)] {
        schema
            .iter()
            .find(|(t, _)| t == table)
            .map(|(_, c)| c.as_slice())
            .unwrap_or(&[])
    };

    // The full set of tables to define: declared (schema) ∪ sealed (manifest) ∪ hot. Each view is the
    // `UNION ALL` of whichever of {sealed Parquet, hot tip} exist. COR-1: hot and cold are kept disjoint
    // structurally by `sealed_through` - cold includes only segments finalized *up to* the watermark,
    // hot only rows *past* it - so the union is exact even across the brief seal→prune window (a segment
    // written before its watermark advances is excluded from cold; its rows are still served from hot).
    let mut tables: std::collections::BTreeSet<String> =
        schema.iter().map(|(t, _)| t.clone()).collect();
    tables.extend(manifest.tables.keys().cloned());
    tables.extend(hot.keys().cloned());

    for table in &tables {
        let cols = cols_of(table);
        // Only segments finalized at or below the served watermark (COR-1 disjointness).
        let sealed_files: Vec<String> = manifest
            .tables
            .get(table)
            .map(|segs| {
                segs.iter()
                    .filter(|s| s.to_block <= sealed_through)
                    .filter_map(|s| {
                        // Resolve through the shared store when this dataset belongs to a runtime
                        // (RFC-0033 §11a), falling back to the per-dataset path.
                        let p = crate::seal::segment_path(dir, &s.file, &s.hash);
                        // Skip a manifest segment whose file is gone from disk (quarantined as corrupt
                        // by the startup integrity pass, or externally removed). Without this, one
                        // missing file makes `read_parquet` throw and the whole query fail; instead the
                        // table's cold data is reduced, loudly, and queries keep working.
                        if p.exists() {
                            Some(format!("'{}'", p.display()))
                        } else {
                            tracing::warn!(
                                "segment {} for {table} missing on disk - skipping (cold data reduced)",
                                s.file
                            );
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Only tip rows strictly past the watermark (COR-1 disjointness; belt-and-braces with the
        // atomic seal→prune, which already keeps sealed rows out of hot).
        //
        // ...but only where cold actually covers something. `sealed_through` is 0 both when the
        // watermark sits at block 0 AND when nothing has ever been sealed (`Store::sealed_through`
        // documents that fallback), and `> 0` drops the genesis row in the second case: block 0
        // lands in neither half of the union and is unreadable. Invisible on any chain indexed from
        // a later block, fatal on one indexed from 0 - OBIB case 3 wants 100,001 rows for blocks
        // 0-100,000 and we returned 100,000, starting at block 1.
        //
        // Gating on this table's own sealed segments is what makes it exact rather than a special
        // case for zero: a row is withheld from hot only when cold genuinely holds that range, so
        // disjointness is preserved in every other state (and a table that has never sealed - newly
        // added, or lagging its siblings - stops being silently truncated at its first block too).
        let hot_rows: Vec<&Value> = hot
            .get(table)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .filter(|r| {
                sealed_files.is_empty()
                    || r.get("block_number").and_then(Value::as_u64).unwrap_or(0) > sealed_through
            })
            .collect();

        // The hot tip: load this table's unsealed rows into a temp table, then union it in. Columns are
        // derived from the rows themselves (like the sealed Parquet, `seal::rows_to_batch`), so this
        // works with or without a `schema.json`. The `*_dec` derived columns still come from the schema.
        let hot_part: Option<String> = if hot_rows.is_empty() {
            None
        } else {
            let hot_tbl = format!("__hot_{table}");
            match load_hot_temp(conn, &hot_tbl, &hot_rows) {
                Ok(()) => Some(format!(
                    "SELECT *{} FROM {}",
                    derived_bigint_cols(cols),
                    with_bigint_base_cols(&format!("\"{hot_tbl}\""), cols)
                )),
                Err(e) => {
                    tracing::debug!("hot rows for {table} skipped: {e:#}");
                    None
                }
            }
        };

        // The view over a given set of sealed files plus whatever hot rows loaded. Built as a closure
        // because a segment that will not bind is dropped and the view rebuilt from what remains, below.
        let view_ddl = |files: &[String]| -> Option<String> {
            let mut parts: Vec<String> = Vec::new();
            if !files.is_empty() {
                // COR-2: `union_by_name=true` NULL-fills columns that differ across segments - segment
                // schemas legitimately drift over a nest's life as ABIs are versioned (CLAUDE.md), and
                // without this a single drifted column makes `read_parquet` throw and the whole table's
                // view silently vanish.
                parts.push(format!(
                    "SELECT *{} FROM {}",
                    derived_bigint_cols(cols),
                    with_bigint_base_cols(
                        &format!("read_parquet([{}], union_by_name=true)", files.join(", ")),
                        cols
                    )
                ));
            }
            parts.extend(hot_part.clone());
            if parts.is_empty() {
                // Nothing sealed and nothing hot: an empty typed view so nest views resolve to zero rows
                // instead of cascade-failing (skip a table with no declared columns).
                if cols.is_empty() {
                    return None;
                }
                Some(empty_view_ddl(table, cols))
            } else {
                // `UNION ALL BY NAME` aligns columns by name and NULL-fills any a side lacks (a column
                // all-null over the sealed range is dropped from its Parquet schema; hot may still
                // carry it).
                Some(format!(
                    "CREATE VIEW \"{table}\" AS {}",
                    parts.join(" UNION ALL BY NAME ")
                ))
            }
        };

        let Some(ddl) = view_ddl(&sealed_files) else {
            continue;
        };
        let Err(e) = conn.execute_batch(&ddl) else {
            continue;
        };
        // A sealed segment that is present but *unreadable* throws while `read_parquet` binds its
        // footer, which happens at DDL time. Swallowing that used to delete the table from the SQL
        // surface outright, so the caller was told the table does not exist and a corrupt file on disk
        // read as a naming fault (#419). Treat it the way a missing file is treated above: drop the
        // segments that will not bind and rebuild the view from what remains, so one bad file *reduces*
        // the table rather than deleting it. The probe is free on the healthy path - it only runs once
        // the whole-view DDL has already failed.
        let readable: Vec<String> = sealed_files
            .iter()
            .filter(|f| {
                let probe = format!("SELECT 1 FROM read_parquet([{f}], union_by_name=true) LIMIT 0");
                match conn.prepare(&probe) {
                    Ok(_) => true,
                    Err(err) => {
                        tracing::warn!(
                            "segment {f} for {table} will not bind - skipping (cold data reduced): {err}"
                        );
                        false
                    }
                }
            })
            .cloned()
            .collect();
        if readable.len() == sealed_files.len() {
            // Every segment binds, so the failure is something else entirely: report it and leave the
            // table undefined, as before. `warn!` rather than `debug!` - a table vanishing from `/sql`
            // is not a debugging detail.
            tracing::warn!("view {table} skipped: {e}");
            continue;
        }
        if let Some(Err(e)) = view_ddl(&readable).map(|retry| conn.execute_batch(&retry)) {
            tracing::warn!("view {table} skipped after dropping bad segments: {e}");
        }
    }
    Ok(())
}

/// The DuckDB column type for a sealed/hot column, matching `seal::rows_to_batch`: the four counter
/// columns are `UBIGINT`, everything else is stored as canonical text (`VARCHAR`).
fn hot_col_type(name: &str) -> &'static str {
    if matches!(
        name,
        "block_number" | "log_index" | "_seq" | "block_timestamp"
    ) {
        "UBIGINT"
    } else {
        "VARCHAR"
    }
}

/// Create a temp table for one logical table's hot rows and append them, typed to match the sealed
/// Parquet (so `UNION ALL BY NAME` lines up). Columns are the sorted union of the rows' JSON keys -
/// exactly how `seal::rows_to_batch` derives the Parquet schema - so no `schema.json` is required.
/// Value marshalling mirrors seal exactly: counter columns are `u64` (0 if absent), every other column
/// is the JSON string as-is, or the JSON value stringified, or NULL when absent/null.
fn load_hot_temp(conn: &Connection, name: &str, rows: &[&Value]) -> Result<()> {
    let mut columns: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
        .map(|c| format!("\"{c}\" {}", hot_col_type(c)))
        .collect();
    conn.execute_batch(&format!(
        "CREATE TEMP TABLE \"{name}\" ({})",
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
    if hot_col_type(col) == "UBIGINT" {
        DuckValue::UBigInt(v.and_then(Value::as_u64).unwrap_or(0))
    } else {
        match v {
            Some(Value::String(s)) => DuckValue::Text(s.clone()),
            None | Some(Value::Null) => DuckValue::Null,
            Some(other) => DuckValue::Text(other.to_string()),
        }
    }
}

/// Load a nest's derived-entity views from `{dir}/views/*.sql` into the connection, in sorted
/// filename order (so `10-foo.sql` can build on nothing and `20-bar.sql` can build on foo). Run
/// after the per-event table views (§4 of RFC-0002), so views may reference `{alias}__{event}`
/// tables. Best-effort: a view over a table with no sealed segment yet - or a bad statement - is
/// skipped with a debug log rather than failing the whole query. Nest SQL is authored by the nest
/// you chose to consume; it runs read-only in this ephemeral in-memory DuckDB, same trust as `/sql`.
fn define_nest_views(conn: &Connection, dir: &Path) {
    for v in nest_view_files(dir) {
        // **Per statement, not per file** (issue #241 item 4). `execute_batch` runs the whole file as
        // one unit, so a single view referencing a table that has never fired - `TaskCancelled`, a
        // module deployed but not yet used - took down *every* view in that file, including the ones
        // that would have worked. The reported workaround was commenting out correct views and
        // uncommenting them once the event fired, which is a poor trade for a fault-isolation gain
        // that was never needed at this granularity.
        for stmt in split_sql_statements(&v.sql) {
            if let Err(e) = conn.execute_batch(&stmt) {
                tracing::debug!("nest view {} statement skipped: {e}", v.file);
            }
        }
    }
}

/// Split authored SQL into individual statements on top-level `;`.
///
/// Deliberately small rather than a parser: it tracks single-quoted strings, double-quoted
/// identifiers, and `--` line comments, which is everything a `;` can hide behind in the SQL a nest
/// authors. A dollar-quoted body would defeat it - DuckDB has no such syntax, and if that changes this
/// is the function to revisit rather than a mystery to debug.
pub(crate) fn split_sql_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut in_s, mut in_d, mut in_c) = (false, false, false);
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if in_c {
            if c == '\n' {
                in_c = false;
                cur.push(c);
            }
            continue;
        }
        match c {
            '-' if !in_s && !in_d && chars.peek() == Some(&'-') => {
                in_c = true;
                continue;
            }
            '\'' if !in_d => in_s = !in_s,
            '"' if !in_s => in_d = !in_d,
            ';' if !in_s && !in_d => {
                if !cur.trim().is_empty() {
                    out.push(cur.trim().to_string());
                }
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// One authored view file: its basename (`10-recipients.sql`) and SQL, in load order.
pub struct NestViewFile {
    pub file: String,
    pub sql: String,
}

/// Read `{dir}/views/*.sql` in sorted filename order - so `10-foo.sql` builds on nothing and
/// `20-bar.sql` can build on foo. Empty when there is no `views/` dir. The one reader both the live
/// loader and the validation gate use, so they never disagree about what a nest's views are.
pub fn nest_view_files(dir: &Path) -> Vec<NestViewFile> {
    let Ok(entries) = std::fs::read_dir(dir.join("views")) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| {
            let sql = std::fs::read_to_string(&p).ok()?;
            let file = p.file_name()?.to_string_lossy().into_owned();
            Some(NestViewFile { file, sql })
        })
        .collect()
}

/// A view that failed to load - RFC-0018 §1 turns the old silent skip into a first-class, teachable
/// signal.
#[derive(Debug, Clone)]
pub struct ViewIssue {
    pub file: String,
    /// The raw engine error (path-free - it's a bind, no segment paths).
    pub error: String,
    /// A fuzzy-matched fix hint (RFC-0016 errors-as-prompts), when the failure is a known class - a
    /// renamed/absent table or column (drift), a reserved word, or a big-int arithmetic slip.
    pub hint: Option<String>,
}

/// Validate a nest's authored views (RFC-0018 §1, the loud gate). Sets up the base surface - empty
/// typed per-event views + labels + children, from the nest's own `schema.json`; no data needed, we're
/// *binding*, not running - then defines each view in load order and records any that fail. A failure
/// is either a syntax error or a reference to a table/column the registry no longer has (**drift**);
/// both come back with a fuzzy-matched fix hint. Loading for real queries stays fault-isolated in
/// `define_nest_views`; this is the separate, surfaced check for `dev` startup and `nuthatch check`.
pub fn validate_nest_views(dir: &Path, schema: &[crate::registry::TableSchema]) -> Vec<ViewIssue> {
    let files = nest_view_files(dir);
    if files.is_empty() {
        return Vec::new();
    }
    let Ok(conn) = Connection::open_in_memory() else {
        return Vec::new();
    };
    // Base surface the views bind against. `u64::MAX` includes every sealed segment (or, on a fresh
    // nest, yields the empty typed views) so a view referencing `usdc__transfer` resolves.
    let empty_hot = HotRows::new();
    let _ = define_views(&conn, dir, &empty_hot, u64::MAX);
    define_labels_view(&conn, dir);
    define_children_views(&conn, dir);

    let mut issues = Vec::new();
    for v in &files {
        // Per statement, matching the live loader - and **every** failure in the file, not the first.
        // `execute_batch` stops at the first error, so a file referencing three tables that have never
        // fired reported one, sent the author to fix it, and revealed the next on the following run
        // (issue #241 item 4: "fix → restart → next error → repeat"). The whole set is known here in
        // one pass; withholding it is a choice, and a bad one.
        let mut errors: Vec<String> = Vec::new();
        for stmt in split_sql_statements(&v.sql) {
            if let Err(e) = conn.execute_batch(&stmt) {
                errors.push(format!("{e}"));
            }
        }
        if errors.is_empty() {
            continue;
        }
        // Lead with the missing tables, collected across every failing statement and deduplicated -
        // that list is the actual work item, and it is what the author would otherwise assemble by
        // hand over several restarts.
        let mut missing: Vec<String> = errors
            .iter()
            .filter_map(|e| missing_table_of(e))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        missing.dedup();
        let error = if missing.len() > 1 {
            format!(
                "{} statement(s) failed; unresolved tables: {}",
                errors.len(),
                missing.join(", ")
            )
        } else {
            errors.join("; ")
        };
        let hint = crate::sql_errors::enrich(&errors[0], &v.sql, schema);
        issues.push(ViewIssue {
            file: v.file.clone(),
            error,
            hint,
        });
    }
    issues
}

/// The table name out of a DuckDB catalog error, if that is what this is.
///
/// Format-dependent by necessity - DuckDB gives no structured error code for it - so it fails soft:
/// an unrecognised message simply yields `None` and the raw error is reported instead of a
/// half-parsed one.
fn missing_table_of(err: &str) -> Option<String> {
    let after = err.split("Table with name ").nth(1)?;
    let name = after.split_whitespace().next()?;
    (!name.is_empty()).then(|| name.to_string())
}

/// (table, [(column, storage)]) for every declared table, from the nest's `schema.json`. Empty if
/// the file is absent/unparseable. Drives both the derived `*_dec` columns and the empty typed views.
fn schema_columns(dir: &Path) -> Vec<(String, Vec<(String, String)>)> {
    let mut out = Vec::new();
    let Ok(raw) = std::fs::read_to_string(dir.join("schema.json")) else {
        return out;
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return out;
    };
    for t in v
        .get("tables")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = t.get("table").and_then(Value::as_str) else {
            continue;
        };
        let cols: Vec<(String, String)> = t
            .get("columns")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some((
                    c.get("name")?.as_str()?.to_string(),
                    c.get("storage")?.as_str()?.to_string(),
                ))
            })
            .collect();
        out.push((name.to_string(), cols));
    }
    out
}

/// True for a big-integer (uint/int > 64-bit) storage kind - the columns that get `*_dec`/`*_overflow`.
fn is_bigint(storage: &str) -> bool {
    storage == "word16" || storage == "word32"
}

/// The extra `SELECT` items projecting the derived `{c}_dec` / `{c}_overflow` columns for a table's
/// big-integer columns (empty string if none), shared by the sealed and empty view builders.
fn derived_bigint_cols(cols: &[(String, String)]) -> String {
    let mut s = String::new();
    for (c, _) in cols.iter().filter(|(_, s)| is_bigint(s)) {
        s.push_str(&format!(
            ", TRY_CAST(\"{c}\" AS DECIMAL(38,0)) AS \"{c}_dec\", \
               (\"{c}\" IS NOT NULL AND TRY_CAST(\"{c}\" AS DECIMAL(38,0)) IS NULL) AS \"{c}_overflow\""
        ));
    }
    s
}

/// Wrap a row source so every declared big-integer column is present in its schema, NULL-filled where
/// no input carries it.
///
/// COR-2's `union_by_name=true` only unions the schemas of the *listed inputs*, and `derived_bigint_cols`
/// projects its casts one level above them - so a `word16`/`word32` column that **no** input carries is
/// referenced by a cast and bound by nothing, the whole-view DDL fails on `Referenced column not found`,
/// and the table disappears from `/sql` entirely (#434). That is not an exotic state: it is every nest
/// between `schema.json` gaining a big-int column and the first segment carrying it sealing. One input
/// out of N carrying the column already worked, which is what made this easy to believe was covered.
///
/// A zero-row typed branch fixes it where the drift belongs - inside the union, so the column is
/// NULL-filled exactly as a partially-present one is, rather than by weakening the cast. `WHERE false`
/// contributes schema and no rows, and it costs no extra scan or bind of the segments themselves.
/// Types come from `hot_col_type` (COR-4: by column *name*), matching `empty_view_ddl` and the hot temp
/// table, so a column does not change type the instant its first segment seals.
fn with_bigint_base_cols(from_item: &str, cols: &[(String, String)]) -> String {
    let stubs: Vec<String> = cols
        .iter()
        .filter(|(_, s)| is_bigint(s))
        .map(|(c, _)| format!("CAST(NULL AS {}) AS \"{c}\"", hot_col_type(c)))
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
fn empty_view_ddl(table: &str, cols: &[(String, String)]) -> String {
    let mut sel: Vec<String> = Vec::new();
    for (name, storage) in cols {
        // COR-4: type by column NAME (`hot_col_type`), exactly as `seal::rows_to_batch` and the hot temp
        // table do - only the four counter columns are UBIGINT, everything else (incl. a `u64`-storage
        // event field like a `uint24`) is VARCHAR. Typing by *storage* here made a column flip type the
        // instant the first row sealed (`AVG(fee)` valid empty, erroring once populated).
        let ty = hot_col_type(name);
        sel.push(format!("CAST(NULL AS {ty}) AS \"{name}\""));
        if is_bigint(storage) {
            sel.push(format!("CAST(NULL AS DECIMAL(38,0)) AS \"{name}_dec\""));
            sel.push(format!("CAST(NULL AS BOOLEAN) AS \"{name}_overflow\""));
        }
    }
    format!(
        "CREATE VIEW \"{table}\" AS SELECT {} WHERE false",
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

    #[test]
    fn rejects_non_select() {
        let dir = tempfile::tempdir().unwrap();
        assert!(query(dir.path(), "DROP TABLE x").is_err());
    }

    /// The `/sql` row cap bounds the Rust-side result buffer and flags truncation precisely.
    #[test]
    fn guarded_query_caps_rows_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"t__transfer","from":"0xa","to":"0xb","value":"1","block_number":1,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"t__transfer","from":"0xa","to":"0xc","value":"2","block_number":1,"tx_hash":"0xt","log_index":1}"#.to_string(),
            r#"{"table":"t__transfer","from":"0xa","to":"0xd","value":"3","block_number":1,"tx_hash":"0xt","log_index":2}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        // Cap below the row count: truncated to max_rows and flagged.
        let guard = QueryGuard {
            timeout: Duration::from_secs(30),
            max_rows: 2,
        };
        let out = query_guarded(dir.path(), r#"SELECT * FROM "t__transfer""#, guard).unwrap();
        assert_eq!(out.rows.len(), 2, "capped at max_rows");
        assert!(out.truncated, "flagged when more rows existed");

        // Cap at the exact row count: everything returned, not flagged (the +1 sentinel finds no more).
        let guard = QueryGuard {
            timeout: Duration::from_secs(30),
            max_rows: 3,
        };
        let out = query_guarded(dir.path(), r#"SELECT * FROM "t__transfer""#, guard).unwrap();
        assert_eq!(out.rows.len(), 3);
        assert!(!out.truncated);
    }

    /// RFC-0009 step 6: a factory nest gets an auto-generated `{template}__children` view over the
    /// sealed factory events - the discovered children with provenance, de-duplicated to the earliest
    /// discovery per address. Answers "which pools, discovered when, by whom" in one query.
    #[test]
    fn children_view_lists_discovered_contracts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"
[nest]
name="univ3"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1f98431c8ad98523631ae4a59f267346ea31f984"
abi="abis/factory.json"
[[templates]]
name="pool"
abi="abis/pool.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        // Seal two PoolCreated events (pool_a, pool_b) + a duplicate discovery of pool_a (later block,
        // must be de-duplicated to the earliest).
        let rows = vec![
            r#"{"table":"factory__pool_created","pool":"0xAAAA000000000000000000000000000000000001","block_number":10,"log_index":0,"block_timestamp":1700000010,"tx_hash":"0xt","address":"0x1f98431c8ad98523631ae4a59f267346ea31f984"}"#.to_string(),
            r#"{"table":"factory__pool_created","pool":"0xBBBB000000000000000000000000000000000002","block_number":12,"log_index":1,"block_timestamp":1700000012,"tx_hash":"0xt","address":"0x1f98431c8ad98523631ae4a59f267346ea31f984"}"#.to_string(),
            r#"{"table":"factory__pool_created","pool":"0xAAAA000000000000000000000000000000000001","block_number":20,"log_index":0,"block_timestamp":1700000020,"tx_hash":"0xt","address":"0x1f98431c8ad98523631ae4a59f267346ea31f984"}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &rows, 10, 20).unwrap();

        let count = query(dir.path(), r#"SELECT count(*) AS n FROM "pool__children""#).unwrap();
        assert_eq!(
            count[0]["n"],
            Value::from(2u64),
            "two distinct discovered pools"
        );
        let a = query(
            dir.path(),
            r#"SELECT discovered_block, discovered_timestamp, parent_address FROM "pool__children" WHERE address = '0xaaaa000000000000000000000000000000000001'"#,
        )
        .unwrap();
        assert_eq!(
            a[0]["discovered_block"],
            Value::from(10u64),
            "earliest discovery wins"
        );
        assert_eq!(a[0]["discovered_timestamp"], Value::from(1700000010u64));
        assert_eq!(
            a[0]["parent_address"],
            Value::from("0x1f98431c8ad98523631ae4a59f267346ea31f984")
        );
    }

    /// A runaway query is interrupted by the watchdog and surfaced as a timeout, not left to hang.
    #[test]
    fn guarded_query_times_out_on_a_runaway() {
        let dir = tempfile::tempdir().unwrap();
        // A recursive CTE that would iterate ~a billion times: it cannot finish inside the budget, so
        // the watchdog interrupts it. Needs no sealed data - it never touches a table.
        let runaway = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 1000000000) SELECT count(*) FROM t";
        let guard = QueryGuard {
            timeout: Duration::from_millis(250),
            max_rows: 1000,
        };
        let err = query_guarded(dir.path(), runaway, guard).unwrap_err();
        assert!(
            format!("{err:#}").contains("time budget"),
            "expected a timeout error, got: {err:#}"
        );
    }

    #[test]
    fn queries_a_sealed_per_table_segment() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"7","block_number":10,"tx_hash":"0xt","log_index":1}"#.to_string(),
            r#"{"table":"usdc__approval","owner":"0xa","spender":"0xd","value":"9","block_number":10,"tx_hash":"0xt","log_index":2}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 10).unwrap();

        // Each table is its own view.
        let t = query(dir.path(), r#"SELECT count(*) AS n FROM "usdc__transfer""#).unwrap();
        assert_eq!(t[0]["n"], Value::from(2u64));
        let a = query(dir.path(), r#"SELECT count(*) AS n FROM "usdc__approval""#).unwrap();
        assert_eq!(a[0]["n"], Value::from(1u64));

        // Point-read searches all tables by (block, log_index).
        let one = get_row(dir.path(), 10, 1).unwrap().unwrap();
        assert_eq!(one["to"], Value::from("0xc"));
        let appr = get_row(dir.path(), 10, 2).unwrap().unwrap();
        assert_eq!(appr["spender"], Value::from("0xd"));
    }

    #[test]
    fn query_survives_a_missing_segment_file() {
        // A segment listed in the manifest but gone from disk (quarantined as corrupt / removed) must
        // not fail the whole query - its cold data is skipped, the surviving segment still answers.
        let dir = tempfile::tempdir().unwrap();
        let row = |b: u64| {
            format!(
                r#"{{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"1","block_number":{b},"tx_hash":"0xt","log_index":0}}"#
            )
        };
        crate::seal::seal_range(dir.path(), &[row(10)], 10, 10).unwrap();
        crate::seal::seal_range(dir.path(), &[row(11)], 11, 11).unwrap();
        // Both sealed → 2 rows.
        let n = query(dir.path(), r#"SELECT count(*) AS n FROM "usdc__transfer""#).unwrap();
        assert_eq!(n[0]["n"], Value::from(2u64));

        // Delete one segment file (as quarantine would). The query still works, returning the survivor.
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let gone = &manifest.tables["usdc__transfer"][0].file;
        std::fs::remove_file(dir.path().join(crate::seal::SEGMENTS_DIR).join(gone)).unwrap();
        let n = query(dir.path(), r#"SELECT count(*) AS n FROM "usdc__transfer""#).unwrap();
        assert_eq!(
            n[0]["n"],
            Value::from(1u64),
            "surviving segment still queryable"
        );
    }

    #[test]
    fn sql_disjoint_union_never_double_counts_an_overlapping_row() {
        // COR-1: even if a block sits in BOTH a sealed segment and the hot store (the seal→prune crash
        // window), the `sealed_through` filter counts it once - cold ≤ watermark, hot > watermark.
        let dir = tempfile::tempdir().unwrap();
        let cold = vec![r#"{"table":"t__e","block_number":10,"log_index":0,"x":"1"}"#.to_string()];
        crate::seal::seal_range(dir.path(), &cold, 10, 10).unwrap();
        // Hot deliberately still holds block 10 (the overlap) AND a genuinely-unsealed block 20.
        let mut hot = HotRows::new();
        hot.insert(
            "t__e".into(),
            vec![
                serde_json::json!({"table":"t__e","block_number":10,"log_index":0,"x":"1"}),
                serde_json::json!({"table":"t__e","block_number":20,"log_index":0,"x":"2"}),
            ],
        );
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        // Watermark = 10: cold keeps block 10, hot keeps only block 20 → 2 rows, not 3.
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT count(*) AS n FROM "t__e""#,
            guard,
            &hot,
            10,
        )
        .unwrap();
        assert_eq!(out.rows[0]["n"], Value::from(2u64));
    }

    #[test]
    fn sql_serves_the_genesis_row_when_nothing_has_been_sealed() {
        // Regression for OBIB case 3. `sealed_through` reads 0 both when the watermark sits at block
        // 0 and when nothing has ever been sealed, so filtering hot to `block_number > sealed_through`
        // silently dropped block 0 - it belonged to neither half of the union. A backfill of blocks
        // 0-100,000 returned 100,000 rows starting at block 1 against an expected 100,001.
        let dir = tempfile::tempdir().unwrap();
        let mut hot = HotRows::new();
        hot.insert(
            "t__b".into(),
            vec![
                serde_json::json!({"table":"t__b","block_number":0,"log_index":0,"x":"genesis"}),
                serde_json::json!({"table":"t__b","block_number":1,"log_index":0,"x":"one"}),
                serde_json::json!({"table":"t__b","block_number":2,"log_index":0,"x":"two"}),
            ],
        );
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT count(*) AS n, min(block_number) AS lo FROM "t__b""#,
            guard,
            &hot,
            0,
        )
        .unwrap();
        assert_eq!(
            out.rows[0]["n"],
            Value::from(3u64),
            "block 0 must be readable when nothing has been sealed"
        );
        assert_eq!(
            out.rows[0]["lo"],
            Value::from(0u64),
            "the served range must start at genesis, not block 1"
        );
    }

    #[test]
    fn sql_excludes_hot_genesis_once_cold_actually_holds_it() {
        // The other side of the fix: serving block 0 from hot must not reopen the double-count it
        // sits beside. Once genesis IS sealed, the hot copy has to stay excluded - and the watermark
        // is still 0, so only the presence of a real segment can tell the two states apart.
        let dir = tempfile::tempdir().unwrap();
        let cold =
            vec![r#"{"table":"t__b","block_number":0,"log_index":0,"x":"genesis"}"#.to_string()];
        crate::seal::seal_range(dir.path(), &cold, 0, 0).unwrap();
        let mut hot = HotRows::new();
        hot.insert(
            "t__b".into(),
            vec![
                serde_json::json!({"table":"t__b","block_number":0,"log_index":0,"x":"genesis"}),
                serde_json::json!({"table":"t__b","block_number":1,"log_index":0,"x":"one"}),
            ],
        );
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT count(*) AS n FROM "t__b""#,
            guard,
            &hot,
            0,
        )
        .unwrap();
        assert_eq!(
            out.rows[0]["n"],
            Value::from(2u64),
            "a sealed genesis row must not also be served from hot"
        );
    }

    #[test]
    fn empty_view_types_columns_by_name_not_storage() {
        // COR-4: a `u64`-storage event field with a NON-counter name (e.g. a `uint24` fee) must be
        // VARCHAR in the empty view - matching what `seal::rows_to_batch` writes - so the column's SQL
        // type doesn't flip (valid empty, erroring once populated) the instant the first row seals.
        let ddl = empty_view_ddl("pool__swap", &[("fee".to_string(), "u64".to_string())]);
        assert!(
            ddl.contains(r#"CAST(NULL AS VARCHAR) AS "fee""#),
            "u64-storage non-counter column must be VARCHAR, got: {ddl}"
        );
        // The four counter columns stay UBIGINT (by name).
        let ddl2 = empty_view_ddl("t__e", &[("block_number".to_string(), "u64".to_string())]);
        assert!(ddl2.contains(r#"CAST(NULL AS UBIGINT) AS "block_number""#));
    }

    #[test]
    fn sql_survives_schema_drift_across_segments() {
        // COR-2: two segments of one table with different column sets (an ABI gained a `fee` field
        // between them) must UNION via `union_by_name`, not throw and drop the whole view.
        let dir = tempfile::tempdir().unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__e","block_number":10,"log_index":0,"a":"1"}"#.to_string()],
            10,
            10,
        )
        .unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__e","block_number":20,"log_index":0,"a":"2","fee":"9"}"#.to_string()],
            20,
            20,
        )
        .unwrap();
        // Without union_by_name this errors ("table not found" - the view was silently dropped).
        let out = query(dir.path(), r#"SELECT count(*) AS n FROM "t__e""#).unwrap();
        assert_eq!(out[0]["n"], Value::from(2u64));
        // The drifted column is NULL-filled for the earlier segment.
        let fees = query(dir.path(), r#"SELECT count(fee) AS with_fee FROM "t__e""#).unwrap();
        assert_eq!(fees[0]["with_fee"], Value::from(1u64));
    }

    #[test]
    fn sql_cannot_read_files_outside_the_data_dirs() {
        // Hardening SEC-2: DuckDB table functions (read_text/read_csv/glob/…) are file-read primitives
        // usable inside a SELECT. The lockdown must confine them to the nest's segments/labels dirs.
        let dir = tempfile::tempdir().unwrap();
        let cold = vec![r#"{"table":"t__e","block_number":10,"log_index":0,"x":"1"}"#.to_string()];
        crate::seal::seal_range(dir.path(), &cold, 10, 10).unwrap();
        // A secret in the nest root (where nuthatch.toml with webhook secrets + RPC keys actually lives).
        std::fs::write(dir.path().join("secret.txt"), "TOP SECRET").unwrap();
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        // Absolute path outside the allowlist → refused.
        assert!(
            query_guarded(
                dir.path(),
                "SELECT content FROM read_text('/etc/hosts')",
                guard
            )
            .is_err(),
            "read_text('/etc/hosts') must be blocked"
        );
        // The nest ROOT (config lives here) is NOT in the allowlist (only segments/ + labels/ are).
        let q = format!(
            "SELECT content FROM read_text('{}')",
            dir.path().join("secret.txt").display()
        );
        assert!(
            query_guarded(dir.path(), &q, guard).is_err(),
            "read_text of the nest root must be blocked (leaks nuthatch.toml)"
        );
        // Case-insensitive + comment-split can't sneak past the denylist.
        assert!(query_guarded(dir.path(), "SELECT * FROM READ_TEXT('/etc/hosts')", guard).is_err());
        assert!(query_guarded(dir.path(), "SELECT * FROM glob('/*')", guard).is_err());
        assert!(query_guarded(
            dir.path(),
            "SELECT content FROM read_text/**/('/etc/hosts')",
            guard
        )
        .is_err());
        // A legitimate query over the sealed segment still works - even when a *column* is named like a
        // function (no call → not blocked).
        let ok = query_guarded(
            dir.path(),
            r#"SELECT count(*) AS read_text FROM "t__e""#,
            guard,
        )
        .unwrap();
        assert_eq!(ok.rows[0]["read_text"], Value::from(1u64));

        // Replacement scans (SEC-2): a bare string literal in table position reads a file with no
        // function name for the denylist to match - the previously-open bypass. Both a `FROM '<path>'`
        // and a `JOIN '<path>'` must be refused, for an absolute path and the nest root alike.
        assert!(
            query_guarded(dir.path(), "SELECT * FROM '/etc/hosts'", guard).is_err(),
            "a `FROM '<path>'` replacement scan must be refused"
        );
        assert!(query_guarded(dir.path(), "SELECT * FROM '/tmp/x.parquet'", guard).is_err());
        assert!(query_guarded(
            dir.path(),
            r#"SELECT * FROM "t__e" JOIN '/etc/hosts' ON true"#,
            guard
        )
        .is_err());
        // Parquet metadata functions read a file too - now denylisted.
        assert!(query_guarded(
            dir.path(),
            "SELECT * FROM parquet_metadata('/etc/hosts')",
            guard
        )
        .is_err());
        // A double-quoted identifier in table position (the legitimate form) is NOT a replacement scan
        // and stays allowed - the guard keys on the single-quote, not the FROM keyword.
        assert!(query_guarded(dir.path(), r#"SELECT count(*) FROM "t__e""#, guard).is_ok());
    }

    #[test]
    fn hot_tip_is_queryable_without_any_segments() {
        // RFC-0013: a nest with only unsealed tip data (no segments, no schema.json) is still SQL-
        // queryable - the hot rows are loaded into a temp table with data-derived columns.
        let dir = tempfile::tempdir().unwrap();
        let mut hot = HotRows::new();
        hot.insert(
            "usdc__transfer".into(),
            vec![
                serde_json::json!({"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":100,"tx_hash":"0xt","log_index":0}),
                serde_json::json!({"table":"usdc__transfer","from":"0xa","to":"0xc","value":"7","block_number":101,"tx_hash":"0xt","log_index":0}),
            ],
        );
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT count(*) AS n, SUM(CAST(value AS DECIMAL(38,0))) AS total FROM "usdc__transfer""#,
            guard,
            &hot,
            0, // nothing sealed → all hot rows (blocks 100/101 > 0) count
        )
        .unwrap();
        assert_eq!(out.rows[0]["n"], Value::from(2u64));
        // Big-int text summed via DECIMAL; DuckDB returns decimals as strings.
        assert_eq!(out.rows[0]["total"].as_str(), Some("12"));
    }

    #[test]
    fn sql_unions_the_hot_tip_with_sealed_cold() {
        // The federation: sealed history + unsealed tip, one SQL surface (RFC-0013). Hot and cold are
        // disjoint by block, so a plain UNION ALL is exact.
        let dir = tempfile::tempdir().unwrap();
        let cold = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"7","block_number":10,"tx_hash":"0xt","log_index":1}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &cold, 10, 10).unwrap();
        let mut hot = HotRows::new();
        hot.insert(
            "usdc__transfer".into(),
            vec![
                serde_json::json!({"table":"usdc__transfer","from":"0xd","to":"0xe","value":"9","block_number":20,"tx_hash":"0xu","log_index":0}),
            ],
        );
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        // Cold-only sees the 2 sealed rows; hot+cold sees all 3.
        let cold_only = query_guarded(
            dir.path(),
            r#"SELECT count(*) AS n FROM "usdc__transfer""#,
            guard,
        )
        .unwrap();
        assert_eq!(cold_only.rows[0]["n"], Value::from(2u64));
        let both = query_hot_cold(
            dir.path(),
            r#"SELECT count(*) AS n FROM "usdc__transfer""#,
            guard,
            &hot,
            10, // sealed through block 10 → cold ≤ 10, hot > 10
        )
        .unwrap();
        assert_eq!(both.rows[0]["n"], Value::from(3u64));
        // The hot row is visible with its columns, filterable by block.
        let tip = query_hot_cold(
            dir.path(),
            r#"SELECT "to" FROM "usdc__transfer" WHERE block_number = 20"#,
            guard,
            &hot,
            10,
        )
        .unwrap();
        assert_eq!(tip.rows.len(), 1);
        assert_eq!(tip.rows[0]["to"], Value::from("0xe"));
    }

    #[test]
    fn net_balances_sum_per_address_as_i128() {
        let dir = tempfile::tempdir().unwrap();
        // 1e20 base units > i64::MAX (~9.2e18): the value that an i64 accumulator would have dropped.
        let big = "100000000000000000000";
        let entities = vec![
            format!(
                r#"{{"table":"t__transfer","from":"0x0","to":"0xa","value":"{big}","block_number":1,"tx_hash":"0xt","log_index":0}}"#
            ),
            r#"{"table":"t__transfer","from":"0xa","to":"0xb","value":"30","block_number":1,"tx_hash":"0xt","log_index":1}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        let map: std::collections::HashMap<String, i128> =
            net_balances(dir.path(), "t__transfer", "from", "to", "value", u64::MAX)
                .unwrap()
                .into_iter()
                .collect();
        let big: i128 = big.parse().unwrap();
        assert_eq!(map["0x0"], -big); // minted out
        assert_eq!(map["0xa"], big - 30); // received big, sent 30
        assert_eq!(map["0xb"], 30);
        assert!(!map.contains_key("nobody"));
    }

    /// RFC-0008 C1: labels imported as a content-addressed snapshot are visible to `/sql` as a
    /// `labels` view, and `cold_exposure` folds sealed transfers × labels into pre-summed exposure
    /// (the restart re-seed path). Uses an amount > i64::MAX to prove the i128 discipline carries.
    #[test]
    fn labels_view_and_cold_exposure_fold() {
        let dir = tempfile::tempdir().unwrap();
        // Label 0xmixer. Two transfers: 0xa → mixer (big), mixer → 0xb (30). 0xa→0xc is unlabeled.
        let mixer = "0x1111111111111111111111111111111111111111";
        let a = "0x00000000000000000000000000000000000000aa";
        let b = "0x00000000000000000000000000000000000000bb";
        let c = "0x00000000000000000000000000000000000000cc";
        let label_file = dir.path().join("l.csv");
        std::fs::write(&label_file, format!("{mixer},mixer\n")).unwrap();
        crate::labels::import(dir.path(), &label_file).unwrap();

        let big = "100000000000000000000"; // > i64::MAX
        let entities = vec![
            format!(
                r#"{{"table":"t__transfer","from":"{a}","to":"{mixer}","value":"{big}","block_number":1,"tx_hash":"0xt","log_index":0}}"#
            ),
            format!(
                r#"{{"table":"t__transfer","from":"{mixer}","to":"{b}","value":"30","block_number":1,"tx_hash":"0xt","log_index":1}}"#
            ),
            format!(
                r#"{{"table":"t__transfer","from":"{a}","to":"{c}","value":"5","block_number":1,"tx_hash":"0xt","log_index":2}}"#
            ),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        // The labels view is queryable via the normal SQL surface.
        let l = query(dir.path(), "SELECT count(*) AS n FROM labels").unwrap();
        assert_eq!(l[0]["n"], Value::from(1u64));

        let exp: std::collections::HashMap<String, (i128, i128)> =
            cold_exposure(dir.path(), "t__transfer", "from", "to", "value", u64::MAX)
                .unwrap()
                .into_iter()
                .map(|(k, amt, cnt)| (k, (amt, cnt)))
                .collect();
        let big: i128 = big.parse().unwrap();
        // 0xa sent `big` to the labeled mixer → outbound exposure (count 1, amount big).
        assert_eq!(exp[&format!("{a}\u{1f}mixer\u{1f}out")], (big, 1));
        // 0xb received 30 from the labeled mixer → inbound exposure.
        assert_eq!(exp[&format!("{b}\u{1f}mixer\u{1f}in")], (30, 1));
        // 0xc's transfer never touched a labeled address → no exposure entry.
        assert!(!exp.contains_key(&format!("{c}\u{1f}mixer\u{1f}in")));
    }

    /// RFC-0001 §2: a uint256 column gets a derived `_dec` DECIMAL(38) view column (value when it
    /// fits in 38 digits, else NULL) and an `_overflow` flag - so ad-hoc SQL can aggregate big ints
    /// without hand-casting.
    #[test]
    fn bigint_columns_get_decimal_and_overflow_views() {
        let dir = tempfile::tempdir().unwrap();
        // schema.json marks `value` as a word32 (uint256) column, driving the derived columns.
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[{"table":"t__transfer","alias":"t","event":"Transfer","topic0":"0x","columns":[{"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        // One value that fits DECIMAL(38) (37 digits) and one that overflows it (a 39-digit u128).
        let fits = "1000000000000000000000000000000000000"; // 1e36, 37 digits
        let overflows = "340282366920938463463374607431768211455"; // u128::MAX, 39 digits > DECIMAL(38)
        let entities = vec![
            format!(
                r#"{{"table":"t__transfer","from":"0xa","to":"0xb","value":"{fits}","block_number":1,"tx_hash":"0xt","log_index":0}}"#
            ),
            format!(
                r#"{{"table":"t__transfer","from":"0xa","to":"0xb","value":"{overflows}","block_number":1,"tx_hash":"0xt","log_index":1}}"#
            ),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();

        let rows = query(
            dir.path(),
            r#"SELECT value_dec, value_overflow FROM "t__transfer" ORDER BY log_index"#,
        )
        .unwrap();
        // Row 0 fits: value_dec present (HUGEINT/DECIMAL stringified), not overflow.
        assert_eq!(rows[0]["value_dec"], Value::from(fits));
        assert_eq!(rows[0]["value_overflow"], Value::from(false));
        // Row 1 overflows DECIMAL(38): value_dec NULL, overflow flagged.
        assert_eq!(rows[1]["value_dec"], Value::Null);
        assert_eq!(rows[1]["value_overflow"], Value::from(true));

        // And SUM(value_dec) works over the fitting rows without a manual cast.
        let s = query(
            dir.path(),
            r#"SELECT SUM(value_dec)::VARCHAR AS s FROM "t__transfer""#,
        )
        .unwrap();
        assert_eq!(s[0]["s"], Value::from(fits));
    }

    /// #434: a declared big-int column that **no** sealed segment carries must not delete the table.
    /// `union_by_name` NULL-fills a column some segments lack, but `derived_bigint_cols` casts one level
    /// above it, so 0-of-N left the cast bound to nothing and the whole view DDL failed - the table
    /// vanished from `/sql` with `Table with name ... does not exist`, no corrupt file involved. That is
    /// the state of every nest between a `schema.json` big-int column landing and the first segment
    /// carrying it sealing. 1-of-N always worked, which is what made it look covered.
    #[test]
    fn declared_bigint_column_no_segment_carries_keeps_the_table() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[{"table":"t__transfer","alias":"t","event":"Transfer","topic0":"0x","columns":[{"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        // Two sealed segments, neither carrying `value` - the ABI bump that added it has not sealed yet.
        for b in [1u64, 2] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"0xa","to":"0xb","block_number":{b},"tx_hash":"0xt","log_index":0}}"#
                )],
                b,
                b,
            )
            .unwrap();
        }

        // The table is still there and still answers over the segments that do exist.
        let rows = query(dir.path(), r#"SELECT count(*) AS n FROM "t__transfer""#).unwrap();
        assert_eq!(
            rows[0]["n"],
            Value::from(2u64),
            "a declared big-int column carried by no segment must reduce to NULLs, not delete the table"
        );
        // The declared column and its derived siblings keep the shape they have at 1-of-N: present,
        // NULL, not flagged as an overflow. A caller's `value_dec` query must not become a naming error.
        let d = query(
            dir.path(),
            r#"SELECT value, value_dec, value_overflow FROM "t__transfer" ORDER BY block_number"#,
        )
        .unwrap();
        assert_eq!(d[0]["value"], Value::Null);
        assert_eq!(d[0]["value_dec"], Value::Null);
        assert_eq!(d[0]["value_overflow"], Value::from(false));
        // And the non-declared columns the segments do carry are untouched.
        assert_eq!(d.len(), 2);
        let f = query(
            dir.path(),
            r#"SELECT "from" FROM "t__transfer" LIMIT 1"#,
        )
        .unwrap();
        assert_eq!(f[0]["from"], Value::from("0xa"));
    }

    /// The hot half of #434, which the issue does not cover. The hot temp table derives its columns
    /// from the rows themselves, so a tip batch that carries no `value` key leaves the same derived
    /// cast bound to nothing - and the view dies with every sealed segment healthy. Same wrap, and it
    /// wants its own test because the sealed test above passes with the hot side still broken.
    #[test]
    fn declared_bigint_column_no_hot_row_carries_keeps_the_table() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[{"table":"t__transfer","alias":"t","event":"Transfer","topic0":"0x","columns":[{"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        let mut hot = HotRows::new();
        hot.insert(
            "t__transfer".into(),
            vec![serde_json::json!({"table":"t__transfer","from":"0xa","to":"0xb","block_number":100,"tx_hash":"0xt","log_index":0})],
        );
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT "from", value, value_dec, value_overflow FROM "t__transfer""#,
            guard,
            &hot,
            0,
        )
        .unwrap();
        assert_eq!(
            out.rows.len(),
            1,
            "a tip batch missing a declared big-int column must not delete the table"
        );
        assert_eq!(out.rows[0]["from"], Value::from("0xa"));
        assert_eq!(out.rows[0]["value"], Value::Null);
        assert_eq!(out.rows[0]["value_dec"], Value::Null);
        assert_eq!(out.rows[0]["value_overflow"], Value::from(false));
    }

    #[test]
    fn query_guard_sees_past_leading_comments() {
        assert_eq!(
            strip_leading_sql_comments("  \n-- hi\nSELECT 1").trim_start(),
            "SELECT 1"
        );
        assert_eq!(
            strip_leading_sql_comments("/* a */ WITH x AS (SELECT 1) SELECT 1")
                .trim_start()
                .split(' ')
                .next(),
            Some("WITH")
        );
        let dir = tempfile::tempdir().unwrap();
        // A comment-prefixed SELECT must be accepted (not rejected as non-SELECT); a DROP still fails.
        assert!(query(dir.path(), "-- a note\nSELECT 42 AS n").is_ok());
        assert!(query(dir.path(), "/* x */ DROP TABLE t").is_err());
    }

    /// A declared-but-unsealed table still resolves as an empty typed view, so a nest view that
    /// UNIONs it with a table that *does* have data doesn't cascade-fail (RFC-0002 dogfood fix).
    #[test]
    fn unsealed_tables_get_empty_typed_views() {
        let dir = tempfile::tempdir().unwrap();
        // schema declares two transfer-ish tables; only `a__ev` will have sealed data.
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[
                {"table":"a__ev","alias":"a","event":"E","topic0":"0x","columns":[
                    {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                    {"name":"amount","sol_type":"uint256","storage":"word32","indexed":false}]},
                {"table":"b__ev","alias":"b","event":"E","topic0":"0x","columns":[
                    {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                    {"name":"amount","sol_type":"uint256","storage":"word32","indexed":false}]}
            ]}"#,
        )
        .unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"a__ev","amount":"100","block_number":1,"log_index":0}"#.to_string()],
            1,
            1,
        )
        .unwrap();

        // b__ev has no segment, but a UNION of both (incl. the derived `_dec` column) must still work.
        let rows = query(
            dir.path(),
            r#"SELECT SUM(amount_dec)::VARCHAR AS total FROM (
                 SELECT amount_dec FROM "a__ev" UNION ALL SELECT amount_dec FROM "b__ev")"#,
        )
        .unwrap();
        assert_eq!(rows[0]["total"], Value::from("100"));
    }

    /// RFC-0002 §4: a nest's `views/*.sql` derived views are loaded and queryable via `/sql`, and
    /// can build on both the per-event tables and earlier (sorted) view files.
    #[test]
    fn nest_defined_views_are_loaded_and_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"7","block_number":11,"tx_hash":"0xu","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"3","block_number":12,"tx_hash":"0xv","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 12).unwrap();

        // Two view files: the second builds on the first - proves sorted load order.
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-recipients.sql"),
            r#"CREATE VIEW recipients AS SELECT "to" AS addr, count(*) AS n FROM "usdc__transfer" GROUP BY "to";"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("views/20-top_recipient.sql"),
            "CREATE VIEW top_recipient AS SELECT addr, n FROM recipients ORDER BY n DESC LIMIT 1;",
        )
        .unwrap();

        let rows = query(dir.path(), "SELECT addr, n FROM top_recipient").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["addr"], Value::from("0xb")); // 0xb received 2, 0xc received 1
        assert_eq!(rows[0]["n"], Value::from(2u64));

        // A broken view file doesn't blow up the surface - the good views still resolve.
        std::fs::write(
            dir.path().join("views/30-broken.sql"),
            "CREATE VIEW broken AS SELECT * FROM nonexistent_table;",
        )
        .unwrap();
        let again = query(dir.path(), "SELECT n FROM recipients WHERE addr = '0xb'").unwrap();
        assert_eq!(again[0]["n"], Value::from(2u64));
    }

    /// RFC-0018 §1: `validate_nest_views` flags a broken/drifted view (with a fuzzy-matched hint) and
    /// leaves a valid one alone - the loud gate the old silent-skip loader never had.
    #[test]
    fn validate_nest_views_flags_the_broken_one_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 10).unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-good.sql"),
            r#"CREATE VIEW good AS SELECT "to" AS addr FROM "usdc__transfer";"#,
        )
        .unwrap();
        // References `transfers` - the classic drop-the-prefix drift the registry no longer has.
        std::fs::write(
            dir.path().join("views/20-broken.sql"),
            "CREATE VIEW broken AS SELECT * FROM transfers;",
        )
        .unwrap();

        let schema = vec![crate::registry::TableSchema {
            table: "usdc__transfer".into(),
            alias: "usdc".into(),
            kind: crate::registry::TableKind::Event,
            function: String::new(),
            selector: String::new(),
            event: "Transfer".into(),
            topic0: "0xddf2".into(),
            columns: vec![],
        }];
        let issues = validate_nest_views(dir.path(), &schema);
        assert_eq!(issues.len(), 1, "only the broken view is flagged");
        assert_eq!(issues[0].file, "20-broken.sql");
        let hint = issues[0].hint.as_ref().expect("a fix hint");
        assert!(
            hint.contains("usdc__transfer"),
            "fuzzy-suggests the real table: {hint}"
        );
    }

    /// RFC-0001 acceptance: `/sql` can JOIN across two per-event tables.
    #[test]
    fn sql_joins_across_two_tables() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xc","value":"7","block_number":11,"tx_hash":"0xu","log_index":0}"#.to_string(),
            r#"{"table":"usdc__approval","owner":"0xa","spender":"0xd","value":"9","block_number":10,"tx_hash":"0xt","log_index":1}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 11).unwrap();

        // Transfers that occurred in a block where an approval also happened (join on block_number).
        let rows = query(
            dir.path(),
            r#"SELECT t.block_number AS b, t."to" AS recip, a.spender AS appr
               FROM "usdc__transfer" t JOIN "usdc__approval" a USING (block_number)"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 1); // only block 10 has both
        assert_eq!(rows[0]["b"], Value::from(10u64));
        assert_eq!(rows[0]["recip"], Value::from("0xb"));
        assert_eq!(rows[0]["appr"], Value::from("0xd"));
    }

    /// The regression test for a **real vulnerability** found while writing the audit-tail coverage:
    /// `/sql` accepted `;`-stacked statements, which was an arbitrary file-write primitive on an
    /// unauthenticated GET surface.
    ///
    /// The leading-keyword gate only inspects the first statement, `conn.prepare` turned out NOT to be
    /// single-statement (it prepares *and executes* a stacked INSERT), and the "no durable target"
    /// argument does not apply to `COPY … TO` or `ATTACH`, which write to the filesystem whatever the
    /// connection holds. Verified end-to-end before the fix: both payloads below wrote real files.
    #[test]
    fn the_sql_surface_refuses_stacked_statements_and_writes_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let exfil = dir.path().join("exfil.csv");
        let evil_db = dir.path().join("evil.db");

        let payloads = [
            format!("SELECT 1; COPY (SELECT 42 AS x) TO '{}'", exfil.display()),
            format!("SELECT 1; ATTACH '{}' AS evil", evil_db.display()),
            "SELECT 1; CREATE TABLE evil (x INTEGER)".to_string(),
            "SELECT 1; INSERT INTO whatever VALUES (1)".to_string(),
            // Comments must not smuggle the separator past the scan, as elsewhere in this module.
            format!(
                "SELECT 1 /* hi */; COPY (SELECT 1) TO '{}'",
                exfil.display()
            ),
        ];
        for sql in &payloads {
            let err = query(dir.path(), sql)
                .expect_err(&format!("must be refused: {sql}"))
                .to_string();
            assert!(err.contains("single statement"), "{sql} -> {err}");
        }

        // The point of the test: nothing reached the filesystem.
        assert!(!exfil.exists(), "a stacked COPY wrote a file");
        assert!(!evil_db.exists(), "a stacked ATTACH created a database");
    }

    /// The guard must not break legitimate SQL: a semicolon inside a string literal or a quoted
    /// identifier is data, and a trailing semicolon is how most people end a query.
    #[test]
    fn statement_stacking_guard_allows_semicolons_that_are_not_separators() {
        for ok in [
            "SELECT ';'",
            "SELECT 'a;b' AS s",
            "SELECT 'it''s; fine'",
            r#"SELECT 1 AS ";""#,
            "SELECT 1;",
            "SELECT 1;   ",
            "SELECT 1; -- trailing comment",
        ] {
            assert!(
                reject_statement_stacking(ok).is_ok(),
                "legitimate query rejected: {ok}"
            );
        }
        for bad in [
            "SELECT 1; SELECT 2",
            "SELECT ';'; DROP TABLE t",
            "SELECT 1;;SELECT 2",
        ] {
            assert!(
                reject_statement_stacking(bad).is_err(),
                "stacked query accepted: {bad}"
            );
        }
    }

    /// The `allowed_directories` + `lock_configuration` lockdown is documented as defence-in-depth
    /// behind the `reject_file_access` denylist. This pins **which of the two actually stops a read**
    /// on the DuckDB we ship, because the answer turned out not to be "both".
    ///
    /// The denylist blocks it. The lockdown, set exactly as `run` sets it, does **not** - an
    /// out-of-allowlist `read_text` succeeds. That is worth an assertion rather than a hopeful comment:
    /// if a DuckDB bump ever starts enforcing it, this test fails and tells us the layer became real.
    #[test]
    fn the_denylist_not_the_directory_lockdown_is_what_blocks_a_file_read() {
        let allowed = tempfile::tempdir().unwrap();
        let secret = tempfile::tempdir().unwrap();
        let secret_file = secret.path().join("nuthatch.toml");
        std::fs::write(&secret_file, "[nest]\napi_key = \"hunter2\"\n").unwrap();
        let sql = format!("SELECT * FROM read_text('{}')", secret_file.display());

        // The primary control refuses it outright - this is the guarantee that actually holds.
        assert!(
            reject_file_access(&sql).is_err(),
            "the denylist must refuse read_text - it is the control we rely on"
        );

        // The backstop, configured exactly as `run` configures it, does not stop the read on this
        // build. Documented, not relied upon.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!(
            "SET allowed_directories=['{}']; SET lock_configuration=true;",
            allowed.path().display()
        ))
        .unwrap();
        let read_succeeded = match conn.prepare(&sql) {
            Ok(mut stmt) => stmt.query_row([], |r| r.get::<_, String>(0)).is_ok(),
            Err(_) => false,
        };
        assert!(
            read_succeeded,
            "allowed_directories now blocks out-of-allowlist reads - the defence-in-depth layer has \
             become load-bearing. Good news: update this test and the comments in `run`, which \
             currently say it is not enforced."
        );

        // `lock_configuration` does hold, at least: a query cannot widen the setting back.
        assert!(
            conn.execute_batch("SET allowed_directories=['/'];")
                .is_err(),
            "lock_configuration must prevent widening file access"
        );
    }

    /// Issue #150: a value larger than `i128` must be dropped **identically** by the cold fold and the
    /// hot replay, or a warm restart would silently change balances.
    ///
    /// The two paths reject it by different mechanisms - the cold fold via `TRY_CAST(… AS HUGEINT)`
    /// yielding NULL, the hot replay via `str::parse::<i128>()` returning `Err` - so their agreement is
    /// a coincidence of intent, not of code, and worth pinning. Both must drop the *whole transfer*:
    /// dropping only one leg would invent value out of nowhere, leaving the sender debited and the
    /// recipient uncredited (or worse).
    #[test]
    fn an_over_i128_value_is_dropped_identically_by_the_cold_fold_and_the_hot_replay() {
        // 2^127 - one past i128::MAX, the smallest value that must be refused.
        const TOO_BIG: &str = "170141183460469231731687303715884105728";
        assert!(
            TOO_BIG.parse::<i128>().is_err(),
            "fixture must overflow i128"
        );

        let row = |from: &str, to: &str, value: &str, block: u64, li: u64| {
            format!(
                r#"{{"table":"t__transfer","from":"{from}","to":"{to}","value":"{value}","block_number":{block},"tx_hash":"0x1","log_index":{li}}}"#
            )
        };

        // A segment holding one ordinary transfer and one that overflows.
        let mixed = tempfile::tempdir().unwrap();
        crate::seal::seal_range(
            mixed.path(),
            &[
                row("0xsender", "0xrecipient", "100", 1, 0),
                row("0xwhale", "0xrecipient", TOO_BIG, 2, 0),
            ],
            1,
            6,
        )
        .unwrap();

        // The reference: the same segment WITHOUT the overflowing row - i.e. what the hot replay
        // produces, since its parse-or-skip never feeds that transfer to the view at all.
        let reference = tempfile::tempdir().unwrap();
        crate::seal::seal_range(
            reference.path(),
            &[row("0xsender", "0xrecipient", "100", 1, 0)],
            1,
            6,
        )
        .unwrap();

        let fold = |dir: &std::path::Path| {
            let mut v = net_balances(dir, "t__transfer", "from", "to", "value", 6).unwrap();
            v.sort();
            v
        };

        assert_eq!(
            fold(mixed.path()),
            fold(reference.path()),
            "the cold fold must drop an over-i128 transfer exactly as the hot replay's parse-or-skip does"
        );

        // Concretely: only the ordinary transfer survives, and both its legs are present.
        let got = fold(mixed.path());
        assert_eq!(
            got,
            vec![
                ("0xrecipient".to_string(), 100i128),
                ("0xsender".to_string(), -100i128),
            ]
        );
        // The whale never appears - neither leg of the dropped transfer leaked through.
        assert!(
            !got.iter().any(|(a, _)| a == "0xwhale"),
            "the sender of a dropped transfer must not be debited: {got:?}"
        );
    }

    #[test]
    fn cold_fold_respects_the_sealed_through_watermark() {
        // Regression for the warm-restart double-count: the cold fold must be bounded by the persisted
        // `sealed_through`, not read every segment. A crash in the seal->prune window leaves a segment
        // durable while the watermark is still stale AND the same rows still sit in the hot store; if
        // the cold fold ignored the watermark, the rebuild would count those rows twice.
        let dir = tempfile::tempdir().unwrap();
        let seg: Vec<String> = vec![
            r#"{"table":"t__transfer","from":"0x0","to":"0xa","value":"100","block_number":3,"tx_hash":"0x1","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &seg, 1, 6).unwrap();

        // Stale watermark (below the segment's range): the fold contributes nothing. With no segment at
        // or below the watermark the table view has no backing at all, so this returns Err - which
        // `rebuild_balances` treats identically to an empty result ("no cold seed"), leaving the hot
        // replay to own those rows exactly once. Either way, the sealed rows must NOT be folded in.
        let stale =
            net_balances(dir.path(), "t__transfer", "from", "to", "value", 0).unwrap_or_default();
        assert!(
            !stale.contains(&("0xa".to_string(), 100i128)),
            "a stale watermark must exclude not-yet-finalized segments from the cold fold"
        );

        // Watermark at/above the segment: the fold includes it.
        let done = net_balances(dir.path(), "t__transfer", "from", "to", "value", 6).unwrap();
        assert!(done.contains(&("0xa".to_string(), 100i128)));
        assert!(done.contains(&("0x0".to_string(), -100i128)));
    }

    #[test]
    fn sql_caps_result_bytes_not_just_rows() {
        // Regression for the unbounded-result-buffer DoS: a row cap bounds count, not width. 100 rows of
        // ~1 MiB each (~100 MiB) is far under the 50k row cap but past the 64 MiB byte cap, so the
        // guarded surface must stop early and flag truncation rather than materialise it all Rust-side.
        let dir = tempfile::tempdir().unwrap();
        let guard = QueryGuard {
            timeout: Duration::from_secs(30),
            max_rows: 50_000,
        };
        let out = query_guarded(
            dir.path(),
            "SELECT repeat('A', 1000000) AS x FROM range(100)",
            guard,
        )
        .unwrap();
        assert!(
            out.truncated,
            "a wide result must be flagged truncated by the byte cap"
        );
        assert!(
            out.rows.len() < 100,
            "the byte cap must stop before materialising all 100 wide rows (got {})",
            out.rows.len()
        );
        assert!(!out.rows.is_empty());

        // A trusted, unguarded query (cap = None) is never byte-capped - it must return all rows.
        let all = query(
            dir.path(),
            "SELECT repeat('A', 1000000) AS x FROM range(100)",
        )
        .unwrap();
        assert_eq!(
            all.len(),
            100,
            "unguarded trusted queries are not byte-capped"
        );
    }

    /// **Issue #419.** A sealed segment that is present on disk but unreadable must *reduce* the
    /// table, not delete it.
    ///
    /// `read_parquet` binds every listed file's footer while the view is being created, so one
    /// corrupt segment throws at DDL time. That failure used to be swallowed, which meant the view was
    /// never created and `/sql` answered `Table with name ... does not exist` - sending an operator to
    /// hunt for a config or naming fault when the actual fault is a file on disk. It also leaked the
    /// internal `__hot_<table>` temp table through DuckDB's did-you-mean, to an untrusted caller.
    ///
    /// The missing-segment case one screen up in `define_views` has always done the right thing (drop
    /// it, `warn!`, carry on). Present-but-corrupt is the more alarming of the two and was the quieter,
    /// which is the asymmetry this pins.
    #[test]
    fn a_corrupt_sealed_segment_reduces_the_table_rather_than_deleting_it() {
        let dir = tempfile::tempdir().unwrap();
        // A schema, so that dropping *every* sealed file still yields the empty **typed** view rather
        // than no view at all. Without it the rebuild-from-nothing case deletes the table, and the
        // assertions below could not tell "rebuilt from the good segment" from "rebuilt from nothing" -
        // both would fail on the `expect` above them. With it, the two are distinguishable, which is
        // what makes the row assertions load-bearing.
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true},
                {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        // Two segments, one block each, so a query can tell which survived.
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__transfer","from":"0xa","value":"1","block_number":1,"tx_hash":"0xt","log_index":0}"#.to_string()],
            1,
            1,
        )
        .unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__transfer","from":"0xb","value":"2","block_number":2,"tx_hash":"0xt","log_index":0}"#.to_string()],
            2,
            2,
        )
        .unwrap();

        // Both segments read before anything is touched - otherwise the assertions below could pass on
        // a table that was never whole.
        let rows = query(
            dir.path(),
            r#"SELECT "from" FROM "t__transfer" ORDER BY block_number"#,
        )
        .expect("both segments readable");
        assert_eq!(rows.len(), 2, "two segments, two rows");

        // Corrupt the second segment in place, as an operator would find it: still listed in the
        // manifest, still on disk, no longer a Parquet file. No restart, so the startup integrity pass
        // has not quarantined it.
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let segs = &manifest.tables["t__transfer"];
        let victim = segs
            .iter()
            .find(|s| s.from_block == 2)
            .expect("the block-2 segment");
        let path = crate::seal::segment_path(dir.path(), &victim.file, &victim.hash);
        std::fs::write(&path, b"not parquet, not even close").unwrap();

        // The table still answers, from the segment that is still good.
        let rows = query(dir.path(), r#"SELECT "from" FROM "t__transfer""#)
            .expect("a corrupt segment must not delete the table from the SQL surface");
        assert_eq!(rows.len(), 1, "the readable segment's row survives");
        assert_eq!(
            rows[0]["from"],
            Value::from("0xa"),
            "and it is the block-1 row, not the corrupt one"
        );
    }

    /// **Issue #241 items 3 and 4.** On a cold nest an authored view must resolve to zero rows, not
    /// fail with `Table ... does not exist`.
    ///
    /// The reported symptom was every view failing at startup on a fresh nest, then being *absent*
    /// until a restart - so the documented first run ("`nuthatch dev`, then query") could not use
    /// views on the run that forms someone's impression of the tool.
    ///
    /// The mechanism is subtle and worth pinning: `define_views` already builds an empty **typed**
    /// view for a table with no sealed segments and no hot rows - but skips it when `cols` is empty,
    /// and `cols` comes from `schema.json`. A hand-written nest has no schema, so no columns, so **no
    /// view at all**, and one missing table cascade-fails the whole view file.
    ///
    /// That makes cold-start view resolution an *emergent* property of `schema.json` being present and
    /// `empty_view_ddl` being reached. This test exists because emergent properties stop being true
    /// quietly.
    #[test]
    fn an_authored_view_resolves_on_a_cold_nest_with_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"tok__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true},
                {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("views/10-big.sql"),
            "CREATE VIEW big_transfers AS SELECT \"from\", value_dec FROM tok__transfer WHERE value_dec > 1000;",
        )
        .unwrap();

        let conn = Connection::open_in_memory().unwrap();
        let empty = HotRows::new();
        define_views(&conn, dir.path(), &empty, u64::MAX).unwrap();
        define_nest_views(&conn, dir.path());

        // The base table exists as an empty typed view…
        let n: i64 = conn
            .query_row("SELECT count(*) FROM tok__transfer", [], |r| r.get(0))
            .expect("a declared table with no rows must still resolve");
        assert_eq!(n, 0);

        // …and so does the authored view built on it, including the derived `_dec` column.
        let n: i64 = conn
            .query_row("SELECT count(*) FROM big_transfers", [], |r| r.get(0))
            .expect("an authored view on an empty table must resolve to zero rows, not fail");
        assert_eq!(n, 0);
    }

    /// The other half of the same mechanism: **without** a schema there are no columns, so the empty
    /// typed view is skipped and the authored view cannot resolve. Pinned so the dependency between
    /// `schema.json` and cold-start views is explicit rather than folklore - it is exactly why
    /// `refresh_stale_artifacts` regenerates a missing schema before anything reads it.
    #[test]
    fn without_a_schema_the_view_cannot_resolve_which_is_why_we_regenerate_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-big.sql"),
            "CREATE VIEW big_transfers AS SELECT * FROM tok__transfer;",
        )
        .unwrap();

        let conn = Connection::open_in_memory().unwrap();
        let empty = HotRows::new();
        define_views(&conn, dir.path(), &empty, u64::MAX).unwrap();
        define_nest_views(&conn, dir.path());

        assert!(
            conn.query_row("SELECT count(*) FROM big_transfers", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "with no schema.json there is no typed empty view, so the authored view cannot resolve - \
             this is the failure `refresh_stale_artifacts` prevents by regenerating the schema"
        );
    }

    /// **Issue #241 item 4.** One view referencing a table that has never fired must not take down the
    /// *other* views in the same file.
    ///
    /// The reported case: `TaskCancelled` and a deployed-but-unused voting module. Both views were
    /// correct, just premature, and the workaround was commenting them out with a note to uncomment
    /// when the event fires - which is a poor trade for fault isolation nobody needed at file
    /// granularity.
    #[test]
    fn one_premature_view_does_not_kill_the_others_in_its_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"tok__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        // Statement 2 references a table this nest never declares. Statements 1 and 3 are fine.
        std::fs::write(
            dir.path().join("views/10-mixed.sql"),
            "CREATE VIEW ok_one AS SELECT block_number FROM tok__transfer;\n\
             CREATE VIEW premature AS SELECT * FROM task__cancelled;\n\
             CREATE VIEW ok_two AS SELECT value_dec FROM tok__transfer;",
        )
        .unwrap();

        let conn = Connection::open_in_memory().unwrap();
        let empty = HotRows::new();
        define_views(&conn, dir.path(), &empty, u64::MAX).unwrap();
        define_nest_views(&conn, dir.path());

        for v in ["ok_one", "ok_two"] {
            conn.query_row(&format!("SELECT count(*) FROM {v}"), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap_or_else(|e| panic!("{v} must exist despite a sibling statement failing: {e}"));
        }
        assert!(
            conn.query_row("SELECT count(*) FROM premature", [], |r| r.get::<_, i64>(0))
                .is_err(),
            "the genuinely-unresolvable view is still absent, which is correct"
        );
    }

    /// And the gate reports **every** unresolved table at once, rather than sending the author round
    /// a fix-restart-next-error loop.
    #[test]
    fn validation_names_all_the_missing_tables_not_just_the_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-three.sql"),
            "CREATE VIEW a AS SELECT * FROM alpha__one;\n\
             CREATE VIEW b AS SELECT * FROM beta__two;\n\
             CREATE VIEW c AS SELECT * FROM gamma__three;",
        )
        .unwrap();

        let issues = validate_nest_views(dir.path(), &[]);
        assert_eq!(issues.len(), 1, "one file, one issue");
        let e = &issues[0].error;
        for t in ["alpha__one", "beta__two", "gamma__three"] {
            assert!(e.contains(t), "every unresolved table must be named: {e}");
        }
        // …and as a *summary*, not three concatenated catalog errors. The author needs the work item
        // ("these three tables are missing"), not three copies of DuckDB explaining what a catalog is.
        // Asserted explicitly because a plain join of the errors also happens to contain all three
        // names - so without this the summary formatting was untested and a mutation of it survived.
        assert!(
            e.contains("unresolved tables:"),
            "the message must summarise, not concatenate: {e}"
        );
        assert!(
            e.contains("3 statement(s) failed"),
            "it must say how many statements failed: {e}"
        );
    }

    /// The splitter must not break on a `;` inside a string or a quoted identifier - splitting there
    /// would mangle correct SQL into two invalid halves, turning a working view into a failure.
    #[test]
    fn semicolons_inside_literals_do_not_split_a_statement() {
        let one = split_sql_statements("CREATE VIEW v AS SELECT 'a;b' AS x;");
        assert_eq!(one.len(), 1, "a quoted `;` is not a separator: {one:?}");

        let ident = split_sql_statements("CREATE VIEW \"odd;name\" AS SELECT 1;");
        assert_eq!(
            ident.len(),
            1,
            "a quoted identifier is not a separator: {ident:?}"
        );

        let commented = split_sql_statements("SELECT 1; -- trailing ; in a comment\nSELECT 2;");
        assert_eq!(
            commented.len(),
            2,
            "a `;` in a comment is not a separator: {commented:?}"
        );

        let two = split_sql_statements("SELECT 1;\nSELECT 2;");
        assert_eq!(two.len(), 2);
    }

    /// **A quoted function name evaded the `/sql` denylist and read arbitrary files.**
    ///
    /// Found in the pre-1.0 adversary pass. `reject_file_access` matched a forbidden name only when the
    /// next non-space character was `(` - and DuckDB accepts a *quoted* function name, where the next
    /// character is `"`. So `SELECT * FROM "read_csv"('/etc/passwd')` passed both guards and DuckDB
    /// executed it, confirmed against a live connection (it returned the contents of `/etc/hosts`).
    ///
    /// Same class as the stacked-`COPY TO` arbitrary *write* found earlier (#153): the guard was
    /// correct about the shape it imagined and the shape had another spelling.
    ///
    /// The cases below are spellings of one idea - break the name away from its parens, or from
    /// itself - and each must stay refused.
    #[test]
    fn a_quoted_function_name_cannot_evade_the_denylist() {
        for q in [
            "SELECT * FROM read_csv('/etc/passwd')",
            r#"SELECT * FROM "read_csv"('/etc/passwd')"#,
            r#"SELECT * FROM "READ_CSV"('/etc/passwd')"#,
            // Quoting a *fragment* of the name is the same trick with a smaller hammer.
            r#"SELECT * FROM read"_"csv('/etc/passwd')"#,
            "SELECT * FROM main.read_csv('/etc/passwd')",
            "SELECT * FROM read_csv\n('/etc/passwd')",
            "SELECT * FROM READ_CSV('/etc/passwd')",
            // The other file-reaching functions deserve the same treatment.
            r#"SELECT * FROM "read_parquet"('/etc/passwd')"#,
            r#"SELECT * FROM "read_json_auto"('/etc/passwd')"#,
        ] {
            assert!(
                reject_file_access(q).is_err() || reject_replacement_scan(q).is_err(),
                "must be refused: {q}"
            );
        }
    }

    /// The fix must not refuse legitimate queries. Stripping quotes before the scan can only make the
    /// denylist match more, so the risk is false positives - pinned here so a later "tidy-up" that
    /// widens it further has to break a test rather than a user's dashboard.
    #[test]
    fn ordinary_quoted_identifiers_still_work() {
        for q in [
            // Reserved-word columns are quoted constantly in this product - `from`/`to` on transfers.
            r#"SELECT "from", "to" FROM usdc__transfer"#,
            r#"SELECT count(*) FROM "usdc__transfer""#,
            // A column whose name merely contains a forbidden name is not a call.
            r#"SELECT my_read_csv_flag FROM t"#,
        ] {
            assert!(reject_file_access(q).is_ok(), "must be allowed: {q}");
            assert!(reject_replacement_scan(q).is_ok(), "must be allowed: {q}");
        }
    }

    /// **Audit finding 5: the allowlist must refuse what the denylist has never heard of.**
    ///
    /// The denylist enumerates forbidden names over a vocabulary DuckDB grows every release, and has
    /// been wrong twice - about spelling and about coverage. This asks the parser what the query
    /// references and permits only what we recognise, so a file-reading function added upstream
    /// tomorrow is refused *by default*.
    ///
    /// The cases below are deliberately ones the denylist does **not** list: if this test passes, the
    /// allowlist is carrying weight of its own rather than shadowing the older control.
    #[test]
    fn the_allowlist_refuses_functions_the_denylist_never_heard_of() {
        let conn = Connection::open_in_memory().unwrap();
        for q in [
            // Not in FORBIDDEN_FNS - inert today only because the extension is not bundled.
            "SELECT * FROM read_xlsx('/etc/passwd')",
            "SELECT * FROM st_read('/etc/passwd')",
            "SELECT * FROM iceberg_scan('/tmp')",
            "SELECT * FROM postgres_scan('host=x','public','t')",
            // A plausible future name nobody has listed anywhere.
            "SELECT * FROM read_totally_new_format('/etc/passwd')",
            // And the ones it does list, by every spelling.
            "SELECT * FROM read_csv('/etc/passwd')",
            r#"SELECT * FROM "read_csv"('/etc/passwd')"#,
        ] {
            assert!(
                reject_unknown_table_refs(&conn, q).is_err(),
                "the allowlist must refuse: {q}"
            );
        }
    }

    /// A replacement scan parses as a `BASE_TABLE` whose name is the path - the AST alone does not
    /// distinguish it from a real table, so the name has to be checked.
    #[test]
    fn a_path_in_table_position_is_not_a_table_name() {
        let conn = Connection::open_in_memory().unwrap();
        for q in [
            "SELECT * FROM '/etc/passwd'",
            "SELECT * FROM '/x.parquet'",
            "SELECT * FROM 'https://evil.example/x.parquet'",
        ] {
            assert!(
                reject_unknown_table_refs(&conn, q).is_err(),
                "a path in table position must be refused: {q}"
            );
        }
    }

    /// And it must not break ordinary analytical SQL - the risk of an allowlist is false refusals,
    /// which is a broken dashboard rather than a breach, but still a bug.
    #[test]
    fn ordinary_analytical_sql_still_passes_the_allowlist() {
        let conn = Connection::open_in_memory().unwrap();
        for q in [
            "SELECT * FROM usdc__transfer",
            r#"SELECT "from", "to", value_dec FROM usdc__transfer WHERE value_dec > 100"#,
            "WITH t AS (SELECT * FROM usdc__transfer) SELECT count(*) FROM t",
            "SELECT a.block_number FROM usdc__transfer a JOIN weth__transfer b USING (tx_hash)",
            // Row-generating functions analytics legitimately uses.
            "SELECT * FROM generate_series(1, 10)",
            "SELECT * FROM range(10)",
            // Inline VALUES references no table at all.
            "SELECT * FROM (VALUES (1),(2)) t(x)",
            "SELECT count(*) FROM usdc__transfer GROUP BY \"from\" ORDER BY 1 DESC LIMIT 5",
        ] {
            assert!(
                reject_unknown_table_refs(&conn, q).is_ok(),
                "legitimate query must be allowed: {q}"
            );
        }
    }

    /// **The allowlist must be wired into the real query path, not merely exist.**
    ///
    /// Written because a mutation exposed the gap: deleting `reject_unknown_table_refs` from `run()`
    /// broke *no test*, since the three tests above call it directly. A control that is unit-tested and
    /// unreachable is the same failure as `reconcile::tick` having six passing tests and no caller, and
    /// as the writer pool holding leases with no indexing code behind them.
    ///
    /// The probe must be a name `FORBIDDEN_FNS` genuinely does **not** list, or the denylist answers
    /// first and this proves nothing about the allowlist. `read_xlsx` was the original probe and
    /// stopped being valid the moment audit finding 4 added it to the denylist - this test caught its
    /// own obsolescence, which is the behaviour worth having.
    #[test]
    fn the_allowlist_is_reachable_from_the_public_query_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = query(
            dir.path(),
            "SELECT * FROM read_some_future_format('/etc/passwd')",
        )
        .expect_err("a function the denylist does not list must still be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not permitted") || msg.contains("tables and views only"),
            "the refusal must come from the allowlist, not from DuckDB failing later: {msg}"
        );

        // And the guarded surface, which is the one actually exposed over HTTP.
        let err = query_guarded(
            dir.path(),
            "SELECT * FROM read_some_future_format('/etc/passwd')",
            QueryGuard {
                timeout: Duration::from_secs(5),
                max_rows: 100,
            },
        )
        .expect_err("the guarded surface must refuse it too");
        assert!(format!("{err:#}").contains("not permitted"));
    }
}
