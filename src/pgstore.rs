//! The Postgres hot store (RFC-0022 slice 2) - the scaled-mode implementation of [`HotStore`].
//!
//! Same contract, different substrate. Embedded mode keeps redb and stays a single binary with zero
//! external services (non-negotiable 1); scaled mode points the *same* business logic at Postgres so
//! a cursor's state lives somewhere a second machine can reach. No `#[cfg]` forks of business logic
//! anywhere - the only thing that changes is which `HotStore` the runtime is handed.
//!
//! ## The faithful-mapping rules
//!
//! Every table here is `(key TEXT PRIMARY KEY, value TEXT)`, mirroring redb's key-value shape rather
//! than "improving" it into a relational schema. That is deliberate. The acceptance test for this
//! slice is that served results match redb byte for byte, and the cheapest way to be sure of that is
//! to not restate the data model on the way across. A relational schema is a later optimisation with
//! its own parity proof to earn, not a freebie to take now.
//!
//! Three details that would silently break parity if missed:
//!
//! 1. **`COLLATE "C"` on every key column.** redb orders keys by *bytes*. Postgres orders `TEXT` by
//!    the database's collation, which under `en_US.UTF-8` sorts punctuation and case differently.
//!    Entity keys are `{block:012}-{log_index:06}` and outbox keys are zero-padded integers, so a
//!    locale-aware collation would reorder rows that redb keeps in chain order - and every range
//!    scan, `recent`, `checkpoints_desc` and prune bound depends on that order. `"C"` is byte order.
//! 2. **The outbox sequence stays in `meta`**, not a `SERIAL`. A sequence is not transactional in the
//!    way the rest of this is (it survives rollback by design), so a restart or an aborted
//!    transaction would let redb and Postgres disagree about the next seq. Same counter, same place,
//!    same semantics.
//! 3. **One schema per nest.** Nests sharing a database must not share a namespace - per-nest
//!    isolation is a non-negotiable, and it should not become weaker just because the store moved.
//!
//! ## Concurrency, and the trap that shaped it
//!
//! [`HotStore`] is synchronous, matching redb. The obvious implementation - the blocking `postgres`
//! client behind an `r2d2` pool - **panics**, and it took running the parity suite to find out:
//!
//! ```text
//! Cannot start a runtime from within a runtime.
//! ```
//!
//! `postgres` is not a synchronous client. It is a blocking *wrapper* that owns a private tokio
//! runtime and `block_on`s an async one, and `block_on` panics when it is called from inside another
//! runtime's context. redb is genuinely synchronous - no runtime, no reentrancy - so "use the sync
//! client, like redb" was reasoning from a false equivalence.
//!
//! This is not a test artefact. nuthatch serves from axum handlers, which run *inside* the tokio
//! runtime, so a `/sql` or `/entity` request against a Postgres-backed nest would have hit the same
//! panic in production. `spawn_blocking` does not save it either: those threads still carry a runtime
//! context.
//!
//! So the client lives on **one dedicated thread of its own**, outside any tokio runtime, and the
//! trait methods post closures to it and block on the reply. The reentrancy disappears because the
//! work genuinely happens elsewhere.
//!
//! That single thread also **serialises every store operation**, which is worth stating plainly as a
//! limitation rather than dressing up as a design: it matches the trait's single-writer contract and
//! it is honest for a v1, but concurrent reads now queue behind each other where redb's would not.
//! A pool of worker threads is the fix, and it is deliberately *not* here - it changes throughput,
//! throughput needs a benchmark, and RFC-0013's convergence work is already benchmark-gated. Parity
//! first, then speed, each with its own evidence.

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;

use crate::store::{HotScanTooLarge, HotStore};

/// A unit of work handed to the connection thread. Boxed so the trait methods can post arbitrary
/// closures rather than an enum of every query shape, which would have to grow with every method.
type Job = Box<dyn FnOnce(&mut postgres::Client) + Send>;

/// Owns the `postgres::Client` on a thread with no tokio runtime above it. See the module docs for
/// why that is load-bearing rather than fussy.
struct Conn {
    tx: Mutex<Sender<Job>>,
}

impl Conn {
    fn spawn(config: postgres::Config) -> Result<Conn> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("nuthatch-pg".into())
            .spawn(move || {
                let mut client = match config.connect(postgres::NoTls) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                // Ends when the last `Sender` drops, i.e. when the `PgStore` goes away.
                while let Ok(job) = rx.recv() {
                    job(&mut client);
                }
            })
            .context("cannot spawn the Postgres connection thread")?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Conn { tx: Mutex::new(tx) }),
            Ok(Err(e)) => Err(anyhow!("{e}")),
            Err(_) => Err(anyhow!("the Postgres connection thread died on startup")),
        }
    }

    /// Run `f` on the connection thread and wait for its result.
    fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut postgres::Client) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = mpsc::channel();
        let job: Job = Box::new(move |client| {
            let _ = tx.send(f(client));
        });
        self.tx
            .lock()
            .map_err(|_| anyhow!("the Postgres connection lock was poisoned"))?
            .send(job)
            .map_err(|_| anyhow!("the Postgres connection thread has stopped"))?;
        rx.recv()
            .map_err(|_| anyhow!("the Postgres connection thread dropped a request"))?
    }
}

/// Meta key holding the next outbox sequence number. Must match `store::OUTBOX_SEQ` - the two
/// backends read the same logical counter, and a nest migrated between them would otherwise restart
/// its sequence.
const OUTBOX_SEQ: &str = "outbox_next_seq";

pub struct PgStore {
    conn: Conn,
    /// The nest's schema, already validated and quoted-safe.
    schema: String,
}

impl PgStore {
    /// Connect and ensure this nest's schema exists.
    ///
    /// `nest` names a schema, so it is validated rather than escaped: an identifier cannot be a bound
    /// parameter in DDL, and building DDL by string-concatenating unvalidated input is how you get an
    /// injection. The charset is the same one `is_valid_alias` already enforces for nest aliases, so
    /// nothing legitimate is refused.
    pub fn connect(url: &str, nest: &str) -> Result<PgStore> {
        if nest.is_empty()
            || !nest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(anyhow!(
                "nest name '{nest}' cannot be a Postgres schema - expected [a-z0-9_]+"
            ));
        }
        let schema = format!("nest_{nest}");
        let config: postgres::Config = url
            .parse()
            .with_context(|| format!("cannot parse Postgres URL '{}'", redact(url)))?;
        let conn = Conn::spawn(config)
            .with_context(|| format!("cannot connect to Postgres at '{}'", redact(url)))?;

        let store = PgStore { conn, schema };
        store.migrate()?;
        Ok(store)
    }

    /// Create the schema and the four tables. Idempotent, so a restart or a second worker taking over
    /// a cursor is a no-op rather than an error.
    fn migrate(&self) -> Result<()> {
        let s = &self.schema;
        let ddl = format!(
            r#"
            CREATE SCHEMA IF NOT EXISTS "{s}";
            CREATE TABLE IF NOT EXISTS "{s}".entities (
                key   TEXT COLLATE "C" PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "{s}".meta (
                key   TEXT COLLATE "C" PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "{s}".blocks (
                key   TEXT COLLATE "C" PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "{s}".outbox (
                key   TEXT COLLATE "C" PRIMARY KEY,
                value TEXT NOT NULL
            );
            "#
        );
        self.conn
            .with(move |c| Ok(c.batch_execute(&ddl)?))
            .with_context(|| format!("failed to migrate schema {s}"))
    }

    fn get_kv(&self, table: &str, key: &str) -> Result<Option<String>> {
        let sql = format!(
            "SELECT value FROM \"{}\".{table} WHERE key = $1",
            self.schema
        );
        let key = key.to_string();
        self.conn
            .with(move |c| Ok(c.query_opt(&sql, &[&key])?.map(|r| r.get::<_, String>(0))))
    }

    fn put_kv(&self, table: &str, key: &str, value: &str) -> Result<()> {
        let sql = format!(
            "INSERT INTO \"{}\".{table} (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        let (key, value) = (key.to_string(), value.to_string());
        self.conn.with(move |c| {
            c.execute(&sql, &[&key, &value])?;
            Ok(())
        })
    }
}

/// Strip any password before a connection string reaches a log or an error message.
fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}://***@{}", &url[..scheme], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}

#[async_trait::async_trait]
impl HotStore for PgStore {
    // ---- entities ---------------------------------------------------------------------------

    fn put_entity(&self, key: &str, json: &str) -> Result<()> {
        self.put_kv("entities", key, json)
    }

    fn get_entity(&self, key: &str) -> Result<Option<String>> {
        self.get_kv("entities", key)
    }

    fn count(&self) -> Result<u64> {
        let sql = format!("SELECT count(*) FROM \"{}\".entities", self.schema);
        self.conn
            .with(move |c| Ok(c.query_one(&sql, &[])?.get::<_, i64>(0) as u64))
    }

    fn recent(&self, limit: usize) -> Result<Vec<String>> {
        // Newest **first**, matching redb's `.iter().rev().take(limit)`. An earlier version reversed
        // this into oldest-first on the assumption that "recent" meant chronological; the parity
        // suite caught it, which is the entire argument for comparing against a live redb rather
        // than against what I believed redb did.
        let sql = format!(
            "SELECT value FROM \"{}\".entities ORDER BY key DESC LIMIT $1",
            self.schema
        );
        let limit = limit as i64;
        self.conn.with(move |c| {
            Ok(c.query(&sql, &[&limit])?
                .into_iter()
                .map(|r| r.get::<_, String>(0))
                .collect())
        })
    }

    fn recent_by_table(&self, table: &str, limit: usize) -> Result<Vec<String>> {
        // The row's table name lives inside the JSON, exactly as it does in redb - the store is
        // table-agnostic on both backends, and making Postgres clever here would be the first crack
        // in "same data model".
        let sql = format!(
            "SELECT value FROM \"{}\".entities \
             WHERE value::jsonb ->> 'table' = $1 ORDER BY key DESC LIMIT $2",
            self.schema
        );
        let (table, limit) = (table.to_string(), limit as i64);
        self.conn.with(move |c| {
            Ok(c.query(&sql, &[&table, &limit])?
                .into_iter()
                .map(|r| r.get::<_, String>(0))
                .collect())
        })
    }

    fn hot_rows_by_table(&self) -> Result<HashMap<String, Vec<serde_json::Value>>> {
        self.hot_rows_by_table_bounded(usize::MAX)
    }

    fn hot_rows_by_table_bounded(
        &self,
        max_rows: usize,
    ) -> Result<HashMap<String, Vec<serde_json::Value>>> {
        let count_sql = format!("SELECT count(*) FROM \"{}\".entities", self.schema);
        let sql = format!(
            "SELECT value FROM \"{}\".entities ORDER BY key",
            self.schema
        );
        self.conn.with(move |c| {
            let total = c.query_one(&count_sql, &[])?.get::<_, i64>(0) as usize;
            if total > max_rows {
                // The same refusal redb gives, for the same reason: a partial tip silently changes
                // the answer to an aggregate. Postgres would happily stream it, which makes refusing
                // here more important, not less.
                return Err(HotScanTooLarge { cap: max_rows }.into());
            }
            let mut out: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            for row in c.query(&sql, &[])? {
                let raw: String = row.get(0);
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                let Some(t) = v.get("table").and_then(|t| t.as_str()) else {
                    continue;
                };
                out.entry(t.to_string()).or_default().push(v);
            }
            Ok(out)
        })
    }

    fn entities_in_range(&self, from: u64, to: u64) -> Result<Vec<String>> {
        let (lo, hi) = (format!("{from:012}-000000"), format!("{to:012}-999999"));
        let sql = format!(
            "SELECT value FROM \"{}\".entities WHERE key >= $1 AND key <= $2 ORDER BY key",
            self.schema
        );
        self.conn.with(move |c| {
            Ok(c.query(&sql, &[&lo, &hi])?
                .into_iter()
                .map(|r| r.get::<_, String>(0))
                .collect())
        })
    }

    fn sample_entity_keys(&self, limit: usize) -> Result<Vec<String>> {
        let sql = format!(
            "SELECT key FROM \"{}\".entities ORDER BY key LIMIT $1",
            self.schema
        );
        let limit = limit as i64;
        self.conn.with(move |c| {
            Ok(c.query(&sql, &[&limit])?
                .into_iter()
                .map(|r| r.get::<_, String>(0))
                .collect())
        })
    }

    // ---- cursor & meta ----------------------------------------------------------------------

    fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.get_kv("meta", key)
    }

    fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.put_kv("meta", key, value)
    }

    fn indexed_head(&self) -> Result<Option<u64>> {
        let hot = self
            .get_meta("last_block")?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let head = hot.max(self.sealed_through());
        Ok((head > 0).then_some(head))
    }

    fn sealed_through(&self) -> u64 {
        self.get_meta("sealed_through")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    fn set_block_hash(&self, block: u64, hash: &str) -> Result<()> {
        self.put_kv("blocks", &format!("{block:012}"), hash)
    }

    fn get_block_hash(&self, block: u64) -> Result<Option<String>> {
        self.get_kv("blocks", &format!("{block:012}"))
    }

    fn checkpoints_desc(&self) -> Result<Vec<(u64, String)>> {
        let sql = format!(
            "SELECT key, value FROM \"{}\".blocks ORDER BY key DESC",
            self.schema
        );
        self.conn.with(move |c| {
            c.query(&sql, &[])?
                .into_iter()
                .map(|r| {
                    let k: String = r.get(0);
                    let block: u64 = k.parse().context("corrupt block key")?;
                    Ok((block, r.get::<_, String>(1)))
                })
                .collect()
        })
    }

    // ---- mutation windows (the atomic ones) --------------------------------------------------

    fn commit_window(
        &self,
        entities: &[(String, String)],
        checkpoint: Option<(u64, &str)>,
        last_block: u64,
    ) -> Result<()> {
        let schema = self.schema.clone();
        let entities = entities.to_vec();
        let checkpoint = checkpoint.map(|(b, h)| (b, h.to_string()));
        self.conn.with(move |c| {
            // One transaction, exactly as redb does it. This is the atomicity the trait's contract
            // promises and `e2e_crash_safety` pins: rows, checkpoint and watermark land together, so
            // a crash leaves the store at a clean window boundary and never mid-window.
            let mut tx = c.transaction()?;
            let ins = format!(
                "INSERT INTO \"{schema}\".entities (key, value) VALUES ($1, $2) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
            );
            for (k, v) in &entities {
                tx.execute(&ins, &[k, v])?;
            }
            if let Some((block, hash)) = &checkpoint {
                let sql = format!(
                    "INSERT INTO \"{schema}\".blocks (key, value) VALUES ($1, $2) \
                     ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
                );
                tx.execute(&sql, &[&format!("{block:012}"), hash])?;
            }
            let meta = format!(
                "INSERT INTO \"{schema}\".meta (key, value) VALUES ('last_block', $1) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
            );
            tx.execute(&meta, &[&last_block.to_string()])?;
            tx.commit()?;
            Ok(())
        })
    }

    async fn commit_window_blocking(
        &self,
        entities: Vec<(String, String)>,
        checkpoint: Option<(u64, String)>,
        last_block: u64,
    ) -> Result<()> {
        // The work already happens on the connection thread, so there is nothing extra to offload -
        // the async signature exists so callers need not know which backend they hold.
        let cp = checkpoint.as_ref().map(|(b, h)| (*b, h.as_str()));
        self.commit_window(&entities, cp, last_block)
    }

    fn rollback_to(&self, block: u64) -> Result<u64> {
        let schema = self.schema.clone();
        self.conn.with(move |c| {
            let mut tx = c.transaction()?;
            let removed = rollback_in_tx(&mut tx, &schema, block)?;
            tx.commit()?;
            Ok(removed)
        })
    }

    fn rollback_to_and_set_meta(&self, block: u64, meta_key: &str, meta_val: &str) -> Result<u64> {
        let schema = self.schema.clone();
        let (mk, mv) = (meta_key.to_string(), meta_val.to_string());
        self.conn.with(move |c| {
            let mut tx = c.transaction()?;
            let removed = rollback_in_tx(&mut tx, &schema, block)?;
            set_meta_in_tx(&mut tx, &schema, &mk, &mv)?;
            tx.commit()?;
            Ok(removed)
        })
    }

    fn prune_range(&self, from: u64, to: u64) -> Result<u64> {
        let schema = self.schema.clone();
        self.conn.with(move |c| {
            let mut tx = c.transaction()?;
            let removed = prune_in_tx(&mut tx, &schema, from, to)?;
            tx.commit()?;
            Ok(removed)
        })
    }

    fn prune_and_set_meta(
        &self,
        from: u64,
        to: u64,
        meta_key: &str,
        meta_val: &str,
    ) -> Result<u64> {
        let schema = self.schema.clone();
        let (mk, mv) = (meta_key.to_string(), meta_val.to_string());
        self.conn.with(move |c| {
            let mut tx = c.transaction()?;
            let removed = prune_in_tx(&mut tx, &schema, from, to)?;
            set_meta_in_tx(&mut tx, &schema, &mk, &mv)?;
            tx.commit()?;
            Ok(removed)
        })
    }

    // ---- delivery outbox --------------------------------------------------------------------

    fn outbox_push(&self, payload: &str) -> Result<u64> {
        let schema = self.schema.clone();
        let payload = payload.to_string();
        self.conn.with(move |c| {
            let mut tx = c.transaction()?;
            // Read-modify-write of the counter inside the transaction, with `FOR UPDATE` so two
            // writers cannot both read the same seq. The trait says single-writer, but a lost outbox
            // entry is silent and permanent, so this one is belt and braces.
            let read = format!("SELECT value FROM \"{schema}\".meta WHERE key = $1 FOR UPDATE");
            let seq: u64 = tx
                .query_opt(&read, &[&OUTBOX_SEQ])?
                .and_then(|r| r.get::<_, String>(0).parse().ok())
                .unwrap_or(0);
            set_meta_in_tx(&mut tx, &schema, OUTBOX_SEQ, &(seq + 1).to_string())?;
            let push = format!("INSERT INTO \"{schema}\".outbox (key, value) VALUES ($1, $2)");
            tx.execute(&push, &[&format!("{seq:020}"), &payload])?;
            tx.commit()?;
            Ok(seq)
        })
    }

    fn outbox_pending(&self, limit: usize) -> Result<Vec<(u64, String)>> {
        let sql = format!(
            "SELECT key, value FROM \"{}\".outbox ORDER BY key LIMIT $1",
            self.schema
        );
        let limit = limit as i64;
        self.conn.with(move |c| {
            c.query(&sql, &[&limit])?
                .into_iter()
                .map(|r| {
                    let k: String = r.get(0);
                    let seq: u64 = k.parse().context("corrupt outbox key")?;
                    Ok((seq, r.get::<_, String>(1)))
                })
                .collect()
        })
    }

    fn outbox_remove(&self, seq: u64) -> Result<()> {
        let sql = format!("DELETE FROM \"{}\".outbox WHERE key = $1", self.schema);
        self.conn.with(move |c| {
            c.execute(&sql, &[&format!("{seq:020}")])?;
            Ok(())
        })
    }

    async fn outbox_remove_batch_blocking(&self, seqs: Vec<u64>) -> Result<()> {
        let sql = format!("DELETE FROM \"{}\".outbox WHERE key = $1", self.schema);
        self.conn.with(move |c| {
            let mut tx = c.transaction()?;
            for seq in seqs {
                tx.execute(&sql, &[&format!("{seq:020}")])?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    fn outbox_len(&self) -> u64 {
        // Matches redb's `.unwrap_or(0)`: this feeds a `/status` gauge, and a gauge that fails the
        // request because the database hiccuped is worse than a gauge that reads zero.
        let sql = format!("SELECT count(*) FROM \"{}\".outbox", self.schema);
        self.conn
            .with(move |c| Ok(c.query_one(&sql, &[])?.get::<_, i64>(0) as u64))
            .unwrap_or(0)
    }

    fn outbox_trim(&self, max: u64) -> Result<u64> {
        let len = self.outbox_len();
        if len <= max {
            return Ok(0);
        }
        let drop = (len - max) as i64;
        let sql = format!(
            "DELETE FROM \"{s}\".outbox WHERE key IN \
             (SELECT key FROM \"{s}\".outbox ORDER BY key LIMIT $1)",
            s = self.schema
        );
        self.conn.with(move |c| Ok(c.execute(&sql, &[&drop])?))
    }
}

/// Upsert one meta key inside an open transaction - shared by every mutation that must land the row
/// change and the watermark together.
fn set_meta_in_tx(
    tx: &mut postgres::Transaction<'_>,
    schema: &str,
    key: &str,
    value: &str,
) -> Result<()> {
    let sql = format!(
        "INSERT INTO \"{schema}\".meta (key, value) VALUES ($1, $2) \
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
    );
    tx.execute(&sql, &[&key, &value])?;
    Ok(())
}

/// Drop every entity and checkpoint strictly above `block`.
///
/// The entity bound is `> {block:012}-999999` rather than a parsed comparison: keys are zero-padded
/// so a lexicographic bound *is* the numeric one, which keeps this a single indexed range delete
/// instead of a full scan. redb reaches the same set by parsing each key, and the two agree exactly
/// because of the padding - which is why the collation note at the top of this file matters.
fn prune_bound_above(block: u64) -> String {
    format!("{block:012}-999999")
}

fn rollback_in_tx(tx: &mut postgres::Transaction<'_>, schema: &str, block: u64) -> Result<u64> {
    let hi = prune_bound_above(block);
    let del_entities = format!("DELETE FROM \"{schema}\".entities WHERE key > $1");
    let removed = tx.execute(&del_entities, &[&hi])?;
    let del_blocks = format!("DELETE FROM \"{schema}\".blocks WHERE key > $1");
    tx.execute(&del_blocks, &[&format!("{block:012}")])?;
    Ok(removed)
}

fn prune_in_tx(
    tx: &mut postgres::Transaction<'_>,
    schema: &str,
    from: u64,
    to: u64,
) -> Result<u64> {
    let (lo, hi) = (format!("{from:012}-000000"), format!("{to:012}-999999"));
    let sql = format!("DELETE FROM \"{schema}\".entities WHERE key >= $1 AND key <= $2");
    Ok(tx.execute(&sql, &[&lo, &hi])?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Connection strings reach logs and error messages; passwords must not travel with them.
    #[test]
    fn passwords_are_redacted_from_connection_strings() {
        assert_eq!(
            redact("postgres://nuthatch:hunter2@db.internal:5432/nuthatch"),
            "postgres://***@db.internal:5432/nuthatch"
        );
        assert_eq!(
            redact("postgres://localhost:5432/nuthatch"),
            "postgres://localhost:5432/nuthatch"
        );
    }

    /// A schema name is concatenated into DDL, so the validation is the injection guard.
    #[test]
    fn schema_names_are_validated_not_escaped() {
        for bad in [
            "",
            "Nest",
            "nest-1",
            "a\"; DROP SCHEMA public CASCADE; --",
            "nest 1",
        ] {
            let err = PgStore::connect("postgres://127.0.0.1:1/none", bad)
                .err()
                .expect("must be refused before any connection is attempted");
            assert!(
                err.to_string().contains("Postgres schema"),
                "{bad} was refused for the wrong reason: {err}"
            );
        }
    }

    /// The zero-padding is what lets a lexicographic bound stand in for a numeric one.
    #[test]
    fn prune_bounds_are_lexicographically_equivalent_to_numeric_ones() {
        assert!(prune_bound_above(10).as_str() < "000000000011-000000");
        assert!(prune_bound_above(10).as_str() > "000000000010-999998");
        // The property that matters: every key of block 11 sorts above the block-10 bound.
        assert!("000000000011-000000" > prune_bound_above(10).as_str());
    }
}
