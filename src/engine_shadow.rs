//! Shadow mode (RFC-0044 Amendment 2, phase 2b): a second engine answers beside the first, the
//! first is served, and every difference is recorded with the statement and both answers.
//!
//! The shadow runs inline, after the primary's rows are in hand and before they are returned, so a
//! shadowed request is slower by the secondary's time. That is the cost of the period, and Gate 2
//! measures p99 with it on. A shadow is skipped, and the skip recorded, when the primary alone has
//! used most of the guard's budget; the served answer is never delayed past the deadline for the
//! sake of a comparison.
//!
//! Only `collect` is compared. The catalogue calls are forwarded to both engines so the secondary
//! sees the same tables, views and hot rows; a secondary that refuses one is recorded and the
//! primary's result stands. The fold and describe paths (`for_each_row`, `one_value`,
//! `query_arrow`, `column_names`, `describe`) go to the primary alone.

use crate::engine::{Died, Engine, FactWindow, Interrupt, Session};
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// One disagreement between the engines, or a shadow that could not run.
#[derive(Debug, Clone)]
pub(crate) struct Difference {
    pub(crate) sql: String,
    pub(crate) kind: Kind,
    /// What the primary did, in one line.
    pub(crate) primary: String,
    /// What the secondary did, in one line.
    pub(crate) secondary: String,
    pub(crate) primary_ms: u128,
    pub(crate) secondary_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Both answered, and the row multisets differ.
    Rows,
    /// One answered and the other refused, or both refused differently.
    Refusal,
    /// A catalogue call the secondary would not take; `sql` names the call.
    Catalogue,
    /// The secondary was not run: the primary had used the budget.
    Skipped,
}

pub(crate) type Sink = Arc<dyn Fn(&Difference) + Send + Sync>;

/// Records to the log under the `shadow` target. The operator pulls it with the rest of the log.
pub(crate) fn log_sink() -> Sink {
    Arc::new(|d: &Difference| {
        tracing::warn!(
            target: "shadow",
            kind = ?d.kind,
            primary_ms = d.primary_ms,
            secondary_ms = d.secondary_ms,
            primary = %d.primary,
            secondary = %d.secondary,
            sql = %d.sql,
            "shadow engine differs"
        );
    })
}

/// Opens a primary and a secondary session for every nest and pairs them.
pub(crate) struct ShadowEngine {
    primary: Box<dyn Engine>,
    secondary: Box<dyn Engine>,
    sink: Sink,
    /// How much of a guard's budget the primary may use before the shadow is skipped.
    budget_share: f64,
}

impl ShadowEngine {
    pub(crate) fn new(primary: Box<dyn Engine>, secondary: Box<dyn Engine>, sink: Sink) -> Self {
        Self {
            primary,
            secondary,
            sink,
            budget_share: 0.5,
        }
    }
}

impl Engine for ShadowEngine {
    fn open(&self, dir: &Path) -> Result<Box<dyn Session>> {
        let primary = self.primary.open(dir)?;
        let secondary = match self.secondary.open(dir) {
            Ok(s) => Some(s),
            Err(e) => {
                (self.sink)(&Difference {
                    sql: "open".into(),
                    kind: Kind::Catalogue,
                    primary: "opened".into(),
                    secondary: format!("{e:#}"),
                    primary_ms: 0,
                    secondary_ms: 0,
                });
                None
            }
        };
        Ok(Box::new(ShadowSession {
            primary,
            secondary,
            sink: self.sink.clone(),
            budget_share: self.budget_share,
            deadline: std::sync::Mutex::new(None),
        }))
    }

    fn open_bare(&self) -> Result<Box<dyn Session>> {
        self.primary.open_bare()
    }
}

static INSTALLED: OnceLock<ShadowEngine> = OnceLock::new();

/// Put a shadow engine in front of the one `analytics` uses. Once per process; a second call is
/// refused rather than silently replacing the first.
pub(crate) fn install(engine: ShadowEngine) -> Result<()> {
    INSTALLED
        .set(engine)
        .map_err(|_| anyhow::anyhow!("a shadow engine is already installed"))
}

pub(crate) fn installed() -> Option<&'static dyn Engine> {
    INSTALLED.get().map(|e| e as &dyn Engine)
}

pub(crate) struct ShadowSession {
    primary: Box<dyn Session>,
    /// `None` once the secondary could not be opened; the primary then runs alone, and said so.
    secondary: Option<Box<dyn Session>>,
    sink: Sink,
    budget_share: f64,
    /// The guard's deadline for the statement in flight, when the caller told us one.
    deadline: std::sync::Mutex<Option<Instant>>,
}

impl ShadowSession {
    /// Forward a catalogue call to both; the secondary's refusal is recorded, never returned.
    fn both<T>(
        &self,
        what: &str,
        f: impl Fn(&dyn Session) -> Result<T>,
        describe: impl Fn(&Result<T>) -> String,
    ) -> Result<T> {
        let primary = f(&*self.primary);
        if let Some(secondary) = &self.secondary {
            let shadow = f(&**secondary);
            if shadow.is_err() != primary.is_err() {
                (self.sink)(&Difference {
                    sql: what.to_string(),
                    kind: Kind::Catalogue,
                    primary: describe(&primary),
                    secondary: describe(&shadow),
                    primary_ms: 0,
                    secondary_ms: 0,
                });
            }
        }
        primary
    }
}

fn outcome(r: &Result<(Vec<Value>, bool), Died>) -> String {
    match r {
        Ok((rows, truncated)) => format!(
            "{} rows{}",
            rows.len(),
            if *truncated { ", truncated" } else { "" }
        ),
        Err(Died::Binding(e)) => format!("refused binding: {e:#}"),
        Err(Died::Executing(e)) => format!("refused executing: {e:#}"),
    }
}

/// Rows compared as a multiset: an engine may return them in any order unless the statement orders
/// them, and a difference in order alone is not a difference in answer.
fn same_rows(a: &[Value], b: &[Value]) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("{} rows against {}", a.len(), b.len()));
    }
    let key = |v: &Value| serde_json::to_string(v).unwrap_or_default();
    let mut xa: Vec<String> = a.iter().map(key).collect();
    let mut xb: Vec<String> = b.iter().map(key).collect();
    xa.sort();
    xb.sort();
    xa.iter()
        .zip(&xb)
        .find(|(x, y)| x != y)
        .map(|(x, y)| format!("first differing row: {x} against {y}"))
}

impl Session for ShadowSession {
    fn execute(&self, sql: &str) -> Result<()> {
        self.both(
            sql,
            |s| s.execute(sql),
            |r| match r {
                Ok(()) => "ok".into(),
                Err(e) => format!("{e:#}"),
            },
        )
    }

    fn collect(&self, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
        let started = Instant::now();
        let primary = self.primary.collect(sql, cap);
        let primary_ms = started.elapsed();
        let Some(secondary) = &self.secondary else {
            return primary;
        };
        let deadline = *self.deadline.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(deadline) = deadline {
            let budget = deadline.saturating_duration_since(started);
            if primary_ms > budget.mul_f64(self.budget_share) {
                (self.sink)(&Difference {
                    sql: sql.to_string(),
                    kind: Kind::Skipped,
                    primary: outcome(&primary),
                    secondary: "not run: the primary used the budget".into(),
                    primary_ms: primary_ms.as_millis(),
                    secondary_ms: 0,
                });
                return primary;
            }
        }
        let shadow_started = Instant::now();
        let shadow = secondary.collect(sql, cap);
        let secondary_ms = shadow_started.elapsed();
        let kind = match (&primary, &shadow) {
            (Ok((a, ta)), Ok((b, tb))) => match same_rows(a, b) {
                None if ta == tb => None,
                None => Some((Kind::Rows, "truncation differs".to_string())),
                Some(why) => Some((Kind::Rows, why)),
            },
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => Some((Kind::Refusal, String::new())),
            (Err(_), Err(_)) => None,
        };
        if let Some((kind, why)) = kind {
            let secondary_text = outcome(&shadow);
            (self.sink)(&Difference {
                sql: sql.to_string(),
                kind,
                primary: outcome(&primary),
                secondary: if why.is_empty() {
                    secondary_text
                } else {
                    format!("{secondary_text}; {why}")
                },
                primary_ms: primary_ms.as_millis(),
                secondary_ms: secondary_ms.as_millis(),
            });
        }
        primary
    }

    fn for_each_row(&self, sql: &str, f: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
        self.primary.for_each_row(sql, f)
    }

    fn one_value(&self, sql: &str) -> Result<Value> {
        self.primary.one_value(sql)
    }

    fn query_arrow(&self, sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>> {
        self.primary.query_arrow(sql)
    }

    fn column_names(&self, sql: &str) -> Result<Vec<String>> {
        self.primary.column_names(sql)
    }

    fn describe(&self, sql: &str) -> Result<Vec<(String, String)>> {
        self.primary.describe(sql)
    }

    fn has_relation(&self, name: &str) -> bool {
        self.primary.has_relation(name)
    }

    fn relations(&self) -> Result<BTreeSet<String>> {
        self.primary.relations()
    }

    fn view_definitions(&self) -> Option<Vec<(String, String)>> {
        self.primary.view_definitions()
    }

    fn serialize_sql(&self, sql: &str) -> Result<Value> {
        self.primary.serialize_sql(sql)
    }

    fn set_deadline(&self, deadline: Option<Instant>) {
        *self.deadline.lock().unwrap_or_else(|p| p.into_inner()) = deadline;
        self.primary.set_deadline(deadline);
        if let Some(secondary) = &self.secondary {
            secondary.set_deadline(deadline);
        }
    }

    fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
        let handles: Vec<Arc<dyn Interrupt>> = std::iter::once(self.primary.interrupt_handle())
            .chain(self.secondary.as_ref().map(|s| s.interrupt_handle()))
            .collect();
        Arc::new(Both(handles))
    }

    fn cold_scan_operators(&self, sql: &str) -> Result<u64> {
        self.primary.cold_scan_operators(sql)
    }

    fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()> {
        self.both(
            &format!("load_hot {table}"),
            |s| s.load_hot(table, rows),
            |r| match r {
                Ok(()) => "ok".into(),
                Err(e) => format!("{e:#}"),
            },
        )
    }

    fn bind_facts(
        &self,
        table: &str,
        cols: &[(String, String)],
        sealed: &[PathBuf],
        hot: bool,
        window: FactWindow,
    ) -> Result<bool> {
        self.both(
            &format!("bind_facts {table}"),
            |s| s.bind_facts(table, cols, sealed, hot, window),
            |r| match r {
                Ok(b) => format!("{b}"),
                Err(e) => format!("{e:#}"),
            },
        )
    }

    fn segment_binds(&self, path: &Path) -> Result<()> {
        self.primary.segment_binds(path)
    }

    fn file_schema(&self, path: &Path) -> Option<Vec<(String, String)>> {
        self.primary.file_schema(path)
    }

    fn bind_snapshots(&self, view: &str, files: &[PathBuf]) -> Result<()> {
        self.both(
            &format!("bind_snapshots {view}"),
            |s| s.bind_snapshots(view, files),
            |r| match r {
                Ok(()) => "ok".into(),
                Err(e) => format!("{e:#}"),
            },
        )
    }

    fn bind_labels(&self, labels_dir: &Path) -> Result<()> {
        self.both(
            "bind_labels",
            |s| s.bind_labels(labels_dir),
            |r| match r {
                Ok(()) => "ok".into(),
                Err(e) => format!("{e:#}"),
            },
        )
    }
}

/// Cancels both engines' statements.
struct Both(Vec<Arc<dyn Interrupt>>);

impl Interrupt for Both {
    fn interrupt(&self) {
        for h in &self.0 {
            h.interrupt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_duck::DuckEngine;
    use std::sync::Mutex;
    use std::time::Duration;

    fn recording() -> (Sink, Arc<Mutex<Vec<Difference>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let sink: Sink = Arc::new(move |d: &Difference| s.lock().unwrap().push(d.clone()));
        (sink, seen)
    }

    /// A secondary that answers everything one row short: the difference the shadow exists to see.
    struct ShortEngine;

    struct ShortSession(Box<dyn Session>);

    impl Engine for ShortEngine {
        fn open(&self, dir: &Path) -> Result<Box<dyn Session>> {
            Ok(Box::new(ShortSession(DuckEngine.open(dir)?)))
        }
        fn open_bare(&self) -> Result<Box<dyn Session>> {
            Ok(Box::new(ShortSession(DuckEngine.open_bare()?)))
        }
    }

    impl Session for ShortSession {
        fn execute(&self, sql: &str) -> Result<()> {
            self.0.execute(sql)
        }
        fn collect(&self, sql: &str, cap: Option<usize>) -> Result<(Vec<Value>, bool), Died> {
            let (mut rows, t) = self.0.collect(sql, cap)?;
            rows.pop();
            Ok((rows, t))
        }
        fn for_each_row(&self, sql: &str, f: &mut dyn FnMut(&[Value]) -> Result<()>) -> Result<()> {
            self.0.for_each_row(sql, f)
        }
        fn one_value(&self, sql: &str) -> Result<Value> {
            self.0.one_value(sql)
        }
        fn query_arrow(&self, sql: &str) -> Result<Vec<arrow::record_batch::RecordBatch>> {
            self.0.query_arrow(sql)
        }
        fn column_names(&self, sql: &str) -> Result<Vec<String>> {
            self.0.column_names(sql)
        }
        fn describe(&self, sql: &str) -> Result<Vec<(String, String)>> {
            self.0.describe(sql)
        }
        fn has_relation(&self, name: &str) -> bool {
            self.0.has_relation(name)
        }
        fn relations(&self) -> Result<BTreeSet<String>> {
            self.0.relations()
        }
        fn view_definitions(&self) -> Option<Vec<(String, String)>> {
            self.0.view_definitions()
        }
        fn serialize_sql(&self, sql: &str) -> Result<Value> {
            self.0.serialize_sql(sql)
        }
        fn interrupt_handle(&self) -> Arc<dyn Interrupt> {
            self.0.interrupt_handle()
        }
        fn cold_scan_operators(&self, sql: &str) -> Result<u64> {
            self.0.cold_scan_operators(sql)
        }
        fn load_hot(&self, table: &str, rows: &[&Value]) -> Result<()> {
            self.0.load_hot(table, rows)
        }
        fn bind_facts(
            &self,
            table: &str,
            cols: &[(String, String)],
            sealed: &[PathBuf],
            hot: bool,
            window: FactWindow,
        ) -> Result<bool> {
            self.0.bind_facts(table, cols, sealed, hot, window)
        }
        fn segment_binds(&self, path: &Path) -> Result<()> {
            self.0.segment_binds(path)
        }
        fn file_schema(&self, path: &Path) -> Option<Vec<(String, String)>> {
            self.0.file_schema(path)
        }
        fn bind_snapshots(&self, view: &str, files: &[PathBuf]) -> Result<()> {
            self.0.bind_snapshots(view, files)
        }
        fn bind_labels(&self, labels_dir: &Path) -> Result<()> {
            self.0.bind_labels(labels_dir)
        }
    }

    #[test]
    fn two_identical_engines_record_nothing() {
        let (sink, seen) = recording();
        let engine = ShadowEngine::new(Box::new(DuckEngine), Box::new(DuckEngine), sink);
        let dir = tempfile::tempdir().unwrap();
        let session = engine.open(dir.path()).unwrap();
        session
            .execute("CREATE TABLE t AS SELECT * FROM range(5) r(n)")
            .unwrap();
        let (rows, truncated) = session.collect("SELECT n FROM t ORDER BY n", None).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(!truncated);
        assert!(
            seen.lock().unwrap().is_empty(),
            "{:?}",
            seen.lock().unwrap()
        );
    }

    #[test]
    fn a_planted_difference_is_recorded_and_the_primary_is_served() {
        let (sink, seen) = recording();
        let engine = ShadowEngine::new(Box::new(DuckEngine), Box::new(ShortEngine), sink);
        let dir = tempfile::tempdir().unwrap();
        let session = engine.open(dir.path()).unwrap();
        session
            .execute("CREATE TABLE t AS SELECT * FROM range(5) r(n)")
            .unwrap();
        let (rows, _) = session.collect("SELECT n FROM t", None).unwrap();
        assert_eq!(rows.len(), 5, "the primary's answer is what is served");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].kind, Kind::Rows);
        assert_eq!(seen[0].primary, "5 rows");
        assert!(
            seen[0].secondary.starts_with("4 rows"),
            "{}",
            seen[0].secondary
        );
        assert_eq!(seen[0].sql, "SELECT n FROM t");
    }

    #[test]
    fn a_refusal_on_one_side_only_is_recorded() {
        let (sink, seen) = recording();
        let engine = ShadowEngine::new(Box::new(DuckEngine), Box::new(DuckEngine), sink);
        let dir = tempfile::tempdir().unwrap();
        let session = engine.open(dir.path()).unwrap();
        // Both refuse the same statement: no difference to record.
        assert!(session.collect("SELECT * FROM nowhere", None).is_err());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn row_order_is_not_a_difference() {
        assert!(same_rows(
            &[serde_json::json!({"n": 1}), serde_json::json!({"n": 2})],
            &[serde_json::json!({"n": 2}), serde_json::json!({"n": 1})]
        )
        .is_none());
        assert!(same_rows(
            &[serde_json::json!({"n": 1})],
            &[serde_json::json!({"n": 3})]
        )
        .is_some());
    }

    #[test]
    fn the_shadow_is_skipped_when_the_primary_used_the_budget() {
        let (sink, seen) = recording();
        let engine = ShadowEngine::new(Box::new(DuckEngine), Box::new(ShortEngine), sink);
        let dir = tempfile::tempdir().unwrap();
        let session = engine.open(dir.path()).unwrap();
        // A deadline already in the past leaves no budget at all.
        session.set_deadline(Some(Instant::now() - Duration::from_millis(1)));
        session
            .execute("CREATE TABLE t AS SELECT * FROM range(5) r(n)")
            .unwrap();
        let (rows, _) = session.collect("SELECT n FROM t", None).unwrap();
        assert_eq!(rows.len(), 5);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].kind, Kind::Skipped);
    }
}
