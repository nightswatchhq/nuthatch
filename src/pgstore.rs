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
//! ## Concurrency
//!
//! [`HotStore`] is a synchronous trait, matching redb, and this uses the blocking `postgres` client
//! rather than `tokio-postgres` for the same reason: the serving layer already performs blocking
//! store reads from async handlers, and quietly changing that here would be a second, invisible
//! refactor riding along with this one. A pool serves concurrent reads; writes are still single-owner
//! per the trait's contract, and under RFC-0022 §2 that ownership becomes a cursor lease.

use anyhow::{anyhow, Context, Result};
use r2d2::Pool;
use r2d2_postgres::PostgresConnectionManager;
use std::collections::HashMap;

use crate::store::{HotScanTooLarge, HotStore};

type PgPool = Pool<PostgresConnectionManager<postgres::NoTls>>;

/// Meta key holding the next outbox sequence number. Must match `store::OUTBOX_SEQ` - the two
/// backends read the same logical counter, and a nest migrated between them would otherwise restart
/// its sequence.
const OUTBOX_SEQ: &str = "outbox_next_seq";

pub struct PgStore {
    pool: PgPool,
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
        let manager = PostgresConnectionManager::new(config, postgres::NoTls);
        let pool = Pool::builder()
            .max_size(8)
            .build(manager)
            .with_context(|| format!("cannot connect to Postgres at '{}'", redact(url)))?;

        let store = PgStore { pool, schema };
        store.migrate()?;
        Ok(store)
    }

    fn conn(&self) -> Result<r2d2::PooledConnection<PostgresConnectionManager<postgres::NoTls>>> {
        self.pool.get().context("no Postgres connection available")
    }

    /// Create the schema and the four tables. Idempotent, so a restart or a second worker taking over
    /// a cursor is a no-op rather than an error.
    fn migrate(&self) -> Result<()> {
        let mut c = self.conn()?;
        let s = &self.schema;
        c.batch_execute(&format!(
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
        ))
        .with_context(|| format!("failed to migrate schema {s}"))?;
        Ok(())
    }

    fn get_kv(&self, table: &str, key: &str) -> Result<Option<String>> {
        let mut c = self.conn()?;
        let sql = format!(
            "SELECT value FROM \"{}\".{table} WHERE key = $1",
            self.schema
        );
        Ok(c.query_opt(&sql, &[&key])?.map(|r| r.get::<_, String>(0)))
    }

    fn put_kv(&self, table: &str, key: &str, value: &str) -> Result<()> {
        let mut c = self.conn()?;
        let sql = format!(
            "INSERT INTO \"{}\".{table} (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        c.execute(&sql, &[&key, &value])?;
        Ok(())
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

fn entity_block(key: &str) -> Option<u64> {
    key.split('-').next()?.parse().ok()
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
        let mut c = self.conn()?;
        let sql = format!("SELECT count(*) FROM \"{}\".entities", self.schema);
        Ok(c.query_one(&sql, &[])?.get::<_, i64>(0) as u64)
    }

    fn recent(&self, limit: usize) -> Result<Vec<String>> {
        let mut c = self.conn()?;
        // `ORDER BY key DESC` then reversed, mirroring redb's `.iter().rev().take(limit)`: the newest
        // `limit` rows, returned oldest-first.
        let sql = format!(
            "SELECT value FROM \"{}\".entities ORDER BY key DESC LIMIT $1",
            self.schema
        );
        let mut out: Vec<String> = c
            .query(&sql, &[&(limit as i64)])?
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        out.reverse();
        Ok(out)
    }

    fn recent_by_table(&self, table: &str, limit: usize) -> Result<Vec<String>> {
        let mut c = self.conn()?;
        // The row's table name lives inside the JSON, exactly as it does in redb - the store is
        // table-agnostic on both backends, and making Postgres clever here would be the first crack
        // in "same data model".
        let sql = format!(
            "SELECT value FROM \"{}\".entities \
             WHERE value::jsonb ->> 'table' = $1 ORDER BY key DESC LIMIT $2",
            self.schema
        );
        let mut out: Vec<String> = c
            .query(&sql, &[&table, &(limit as i64)])?
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        out.reverse();
        Ok(out)
    }

    fn hot_rows_by_table(&self) -> Result<HashMap<String, Vec<serde_json::Value>>> {
        self.hot_rows_by_table_bounded(usize::MAX)
    }

    fn hot_rows_by_table_bounded(
        &self,
        max_rows: usize,
    ) -> Result<HashMap<String, Vec<serde_json::Value>>> {
        let mut c = self.conn()?;
        let count_sql = format!("SELECT count(*) FROM \"{}\".entities", self.schema);
        let total = c.query_one(&count_sql, &[])?.get::<_, i64>(0) as usize;
        if total > max_rows {
            // The same refusal redb gives, and for the same reason: a partial tip silently changes
            // the answer to an aggregate. Postgres would happily stream it, which makes it *more*
            // important to refuse here, not less.
            return Err(HotScanTooLarge {
                rows: total,
                max: max_rows,
            }
            .into());
        }
        let sql = format!(
            "SELECT value FROM \"{}\".entities ORDER BY key",
            self.schema
        );
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
    }

    fn entities_in_range(&self, from: u64, to: u64) -> Result<Vec<String>> {
        let mut c = self.conn()?;
        let (lo, hi) = (format!("{from:012}-000000"), format!("{to:012}-999999"));
        let sql = format!(
            "SELECT value FROM \"{}\".entities WHERE key >= $1 AND key <= $2 ORDER BY key",
            self.schema
        );
        Ok(c.query(&sql, &[&lo, &hi])?
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect())
    }

    fn sample_entity_keys(&self, limit: usize) -> Result<Vec<String>> {
        let mut c = self.conn()?;
        let sql = format!(
            "SELECT key FROM \"{}\".entities ORDER BY key LIMIT $1",
            self.schema
        );
        Ok(c.query(&sql, &[&(limit as i64)])?
            .into_iter()
            .map(|r| r.get::<_, String>(0))
            .collect())
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
        let mut c = self.conn()?;
        let sql = format!(
            "SELECT key, value FROM \"{}\".blocks ORDER BY key DESC",
            self.schema
        );
        c.query(&sql, &[])?
            .into_iter()
            .map(|r| {
                let k: String = r.get(0);
                let block: u64 = k.parse().context("corrupt block key")?;
                Ok((block, r.get::<_, String>(1)))
            })
            .collect()
    }

    // ---- mutation windows (the atomic ones) --------------------------------------------------

    fn commit_window(
        &self,
        entities: &[(String, String)],
        checkpoint: Option<(u64, &str)>,
        last_block: u64,
    ) -> Result<()> {
        let mut c = self.conn()?;
        // One transaction, exactly as redb does it. This is the atomicity the trait's contract
        // promises and `e2e_crash_safety` pins: rows, checkpoint and watermark land together, so a
        // crash leaves the store at a clean window boundary and never mid-window.
        let mut tx = c.transaction()?;
        let ins = format!(
            "INSERT INTO \"{}\".entities (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        for (k, v) in entities {
            tx.execute(&ins, &[k, v])?;
        }
        if let Some((block, hash)) = checkpoint {
            let sql = format!(
                "INSERT INTO \"{}\".blocks (key, value) VALUES ($1, $2) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                self.schema
            );
            tx.execute(&sql, &[&format!("{block:012}"), &hash])?;
        }
        let meta = format!(
            "INSERT INTO \"{}\".meta (key, value) VALUES ('last_block', $1) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        tx.execute(&meta, &[&last_block.to_string()])?;
        tx.commit()?;
        Ok(())
    }

    async fn commit_window_blocking(
        &self,
        entities: Vec<(String, String)>,
        checkpoint: Option<(u64, String)>,
        last_block: u64,
    ) -> Result<()> {
        // The synchronous path is already the blocking one; the async wrapper exists so the caller
        // does not need to know which backend it holds.
        let cp = checkpoint.as_ref().map(|(b, h)| (*b, h.as_str()));
        self.commit_window(&entities, cp, last_block)
    }

    fn rollback_to(&self, block: u64) -> Result<u64> {
        let mut c = self.conn()?;
        let mut tx = c.transaction()?;
        let removed = rollback_in_tx(&mut tx, &self.schema, block)?;
        tx.commit()?;
        Ok(removed)
    }

    fn rollback_to_and_set_meta(&self, block: u64, meta_key: &str, meta_val: &str) -> Result<u64> {
        let mut c = self.conn()?;
        let mut tx = c.transaction()?;
        let removed = rollback_in_tx(&mut tx, &self.schema, block)?;
        let sql = format!(
            "INSERT INTO \"{}\".meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        tx.execute(&sql, &[&meta_key, &meta_val])?;
        tx.commit()?;
        Ok(removed)
    }

    fn prune_range(&self, from: u64, to: u64) -> Result<u64> {
        let mut c = self.conn()?;
        let mut tx = c.transaction()?;
        let removed = prune_in_tx(&mut tx, &self.schema, from, to)?;
        tx.commit()?;
        Ok(removed)
    }

    fn prune_and_set_meta(
        &self,
        from: u64,
        to: u64,
        meta_key: &str,
        meta_val: &str,
    ) -> Result<u64> {
        let mut c = self.conn()?;
        let mut tx = c.transaction()?;
        let removed = prune_in_tx(&mut tx, &self.schema, from, to)?;
        let sql = format!(
            "INSERT INTO \"{}\".meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        tx.execute(&sql, &[&meta_key, &meta_val])?;
        tx.commit()?;
        Ok(removed)
    }

    // ---- delivery outbox --------------------------------------------------------------------

    fn outbox_push(&self, payload: &str) -> Result<u64> {
        let mut c = self.conn()?;
        let mut tx = c.transaction()?;
        // Read-modify-write of the counter inside the transaction, and `FOR UPDATE` so two writers
        // cannot both read the same seq. The trait says single-writer, but a lost outbox entry is
        // silent and permanent, so this one is belt and braces.
        let read = format!(
            "SELECT value FROM \"{}\".meta WHERE key = $1 FOR UPDATE",
            self.schema
        );
        let seq: u64 = tx
            .query_opt(&read, &[&OUTBOX_SEQ])?
            .and_then(|r| r.get::<_, String>(0).parse().ok())
            .unwrap_or(0);
        let bump = format!(
            "INSERT INTO \"{}\".meta (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
            self.schema
        );
        tx.execute(&bump, &[&OUTBOX_SEQ, &(seq + 1).to_string()])?;
        let push = format!(
            "INSERT INTO \"{}\".outbox (key, value) VALUES ($1, $2)",
            self.schema
        );
        tx.execute(&push, &[&format!("{seq:020}"), &payload])?;
        tx.commit()?;
        Ok(seq)
    }

    fn outbox_pending(&self, limit: usize) -> Result<Vec<(u64, String)>> {
        let mut c = self.conn()?;
        let sql = format!(
            "SELECT key, value FROM \"{}\".outbox ORDER BY key LIMIT $1",
            self.schema
        );
        c.query(&sql, &[&(limit as i64)])?
            .into_iter()
            .map(|r| {
                let k: String = r.get(0);
                let seq: u64 = k.parse().context("corrupt outbox key")?;
                Ok((seq, r.get::<_, String>(1)))
            })
            .collect()
    }

    fn outbox_remove(&self, seq: u64) -> Result<()> {
        let mut c = self.conn()?;
        let sql = format!("DELETE FROM \"{}\".outbox WHERE key = $1", self.schema);
        c.execute(&sql, &[&format!("{seq:020}")])?;
        Ok(())
    }

    async fn outbox_remove_batch_blocking(&self, seqs: Vec<u64>) -> Result<()> {
        let mut c = self.conn()?;
        let mut tx = c.transaction()?;
        let sql = format!("DELETE FROM \"{}\".outbox WHERE key = $1", self.schema);
        for seq in seqs {
            tx.execute(&sql, &[&format!("{seq:020}")])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn outbox_len(&self) -> u64 {
        // Matches redb's `.unwrap_or(0)`: this feeds a `/status` gauge, and a gauge that fails the
        // request because the database hiccuped is worse than a gauge that reads zero.
        let count = || -> Result<u64> {
            let mut c = self.conn()?;
            let sql = format!("SELECT count(*) FROM \"{}\".outbox", self.schema);
            Ok(c.query_one(&sql, &[])?.get::<_, i64>(0) as u64)
        };
        count().unwrap_or(0)
    }

    fn outbox_trim(&self, max: u64) -> Result<u64> {
        let len = self.outbox_len();
        if len <= max {
            return Ok(0);
        }
        let drop = len - max;
        let mut c = self.conn()?;
        let sql = format!(
            "DELETE FROM \"{s}\".outbox WHERE key IN \
             (SELECT key FROM \"{s}\".outbox ORDER BY key LIMIT $1)",
            s = self.schema
        );
        Ok(c.execute(&sql, &[&(drop as i64)])?)
    }
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

fn rollback_in_tx(
    tx: &mut postgres::Transaction<'_>,
    schema: &str,
    block: u64,
) -> Result<u64> {
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

    #[test]
    fn entity_block_parses_the_padded_prefix() {
        assert_eq!(entity_block("000000000042-000007"), Some(42));
        assert_eq!(entity_block("nonsense"), None);
    }
}
