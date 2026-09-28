//! The analytical engine as a trait, with DuckDB behind it (`engine_duck`).
//!
//! RFC-0044 Amendment 2, phase 2a. `analytics.rs` keeps every policy decision it had - the four
//! read-only gates, the allowlist walk, the connection cache, the shared deadline, the integrity
//! sweep - and asks the engine only for things any SQL engine over the sealed segments can answer:
//! bind a fact table from its inputs, run a statement to rows, cancel one, count its cold scans.
//! The DDL that does it, and the catalogue functions behind the catalogue questions, live with the
//! implementation. Nothing here changes an answer; the suite is the proof.

use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) use crate::analytics::FactWindow;

/// The per-result Rust-side byte ceiling for the guarded `/sql` surface (64 MiB). Comfortably above
/// any legitimate 50k-row result, far below the per-cursor RAM budget - the backstop against a
/// wide-cell `SELECT` inflating the materialised buffer past the budget. Part of `collect`'s
/// contract: every engine applies it whenever a row cap is given, so a shadow truncates where the
/// primary does.
pub(crate) const SQL_MAX_RESULT_BYTES: usize = 64 * 1024 * 1024;

/// A cheap lower-bound byte estimate of a materialised cell - dominated by string payloads, which is
/// exactly the wide-cell attack vector. Numbers/bools/null count a small fixed cost.
pub(crate) fn value_bytes(v: &Value) -> usize {
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

/// Opens sessions. One implementation today; a shadow engine is phase 2b.
pub(crate) trait Engine: Send + Sync {
    /// A session bounded and locked to `dir`: the nest's memory, thread and spill limits, file
    /// access confined to its data directories.
    fn open(&self, dir: &Path) -> Result<Box<dyn Session>>;
    /// An unbounded session with no nest behind it, for binding authored SQL to see whether it
    /// parses and validates. Nothing untrusted runs on one.
    fn open_bare(&self) -> Result<Box<dyn Session>>;
}

/// Cancels a running statement from another thread.
pub(crate) trait Interrupt: Send + Sync {
    fn interrupt(&self);
}

/// Which phase of a statement failed.
///
/// The distinction that matters for #433 is `Executing`: a statement that fails to **bind** is a
/// question about names - a typo, a missing column, an unknown table - and no corrupt page can
/// cause one, so it must never trigger the integrity sweep. Only a statement that bound and then
/// died while reading rows is worth paying for.
#[derive(Debug)]
pub(crate) enum Died {
    /// The engine refused it before running: a name the catalogue does not have, a type that does
    /// not check.
    Binding(anyhow::Error),
    /// It bound, then died running or materialising rows - the only shape a corrupt page produces.
    Executing(anyhow::Error),
}

impl From<Died> for anyhow::Error {
    fn from(died: Died) -> Self {
        match died {
            Died::Binding(e) | Died::Executing(e) => e,
        }
    }
}

/// One engine instance over one nest: a catalogue being built up and statements run against it.
/// `Send`, because the connection cache hands a session from one request's thread to the next.
// The methods only `folds` calls are part of the contract without it.
#[allow(dead_code)]
pub(crate) trait Session: Send {
    /// Run a statement for its effect: a view definition, a transaction boundary.
    fn execute(&self, sql: &str) -> Result<()>;

    /// Prepare, execute and materialise the result as nuthatch-encoded JSON rows. With
    /// `cap = Some(n)` it stops after `n + 1` rows so the caller can report truncation precisely
    /// (the bool is true when that extra row existed), and also caps cumulative result bytes;
    /// `cap = None` materialises every row.
    fn collect(&self, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died>;

    /// Stream a result row by row, each as its cells in column order, so a large result is never
    /// held whole.
    fn for_each_row(&self, sql: &str, f: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()>;

    /// The first cell of the first row.
    fn one_value(&self, sql: &str) -> Result<Value>;

    /// A result as Arrow.
    fn query_arrow(&self, sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>>;

    /// The column names of a statement's result, learned by running it.
    fn column_names(&self, sql: &str) -> Result<Vec<String>>;

    /// `(column, type)` of a statement's output, in the engine's own spelling of the type.
    fn describe(&self, sql: &str) -> Result<Vec<(String, String)>>;

    /// Whether a table or view of this name exists, compared without case. `true` when the
    /// catalogue cannot be asked, which is the answer that refuses rather than admits.
    fn has_relation(&self, name: &str) -> bool;

    /// Every table and view currently defined, lowercased.
    fn relations(&self) -> Result<BTreeSet<String>>;

    /// Each defined view's name and its stored `CREATE VIEW` text; `None` when the catalogue
    /// cannot be listed.
    fn view_definitions(&self) -> Option<Vec<(String, String)>>;

    /// The statement's AST as the engine's parser serialises it. A statement the parser refuses
    /// still returns `Ok`, with the refusal in-band as the parser reports it; `Err` is the
    /// serialisation itself failing.
    fn serialize_sql(&self, sql: &str) -> Result<Value>;

    /// The security walk: what the statement reaches, from the engine's own parse. `None` when the
    /// parser could not say (it fails open; the denylist is still in front), `Some(Err)` when the
    /// statement is refused with the reason, `Some(Ok((tables, surveys)))` with the base tables and
    /// CTE names lowercased and whether it asks about the catalogue.
    fn reach(&self, _sql: &str) -> Option<Result<(BTreeSet<String>, bool)>> {
        None
    }

    /// The physical tables a statement reads (names a `WITH` binds in scope excluded) and the table
    /// functions it calls, lowercased; `None` when the statement will not parse.
    fn table_refs(&self, sql: &str) -> Option<(BTreeSet<String>, BTreeSet<String>)> {
        crate::analytics::table_refs_from_ast(&self.serialize_sql(sql).ok()?)
    }

    /// A handle another thread can use to cancel whatever this session is running.
    fn interrupt_handle(&self) -> Arc<dyn Interrupt>;

    /// The guard's deadline for the statement about to run, so an engine that does extra work
    /// beside the answer (a shadow) can decline it when the budget is nearly spent. Advisory; the
    /// watchdog still enforces the deadline.
    fn set_deadline(&self, _deadline: Option<std::time::Instant>) {}

    /// How many cold Parquet scans the statement's physical plan performs (RFC-0048 §3). Refuses,
    /// as `AdmissionRefusal::Unboundable`, any plan it cannot count.
    fn cold_scan_operators(&self, sql: &str) -> Result<u64>;

    /// Load one table's hot rows so that `bind_facts(.., hot = true, ..)` can union them in.
    fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()>;

    /// Define `table` over its sealed segments and, when `hot`, the rows `load_hot` staged, with
    /// every declared column present and the derived `*_dec`/`*_overflow` columns projected.
    /// `Ok(false)` when there was nothing to define: no segments, no hot rows and no declared
    /// columns. `Err` when a segment would not bind, which the caller answers by probing each one.
    fn bind_facts(
        &self,
        table: &str,
        cols: &[(String, String)],
        sealed: &[PathBuf],
        hot: bool,
        window: FactWindow,
    ) -> Result<bool>;

    /// Whether one sealed segment binds at all; the error carries the engine's own wording.
    fn segment_binds(&self, path: &Path) -> Result<()>;

    /// `(column, type)` of one sealed segment, or `None` when it cannot be read.
    fn file_schema(&self, path: &Path) -> Option<Vec<(String, String)>>;

    /// Define `view` over immutable offchain snapshots, unioned by name.
    fn bind_snapshots(&self, view: &str, files: &[PathBuf]) -> Result<()>;

    /// Define the `labels` view over the `*.json` snapshots in `labels_dir`.
    fn bind_labels(&self, labels_dir: &Path) -> Result<()>;
}
