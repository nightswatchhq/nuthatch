//! The control plane (RFC-0022 §3): **desired state**, and who is available to run it.
//!
//! Two tables and no cleverness. `desired_nest` is what the operator asked for; `worker` is who is
//! currently alive to serve it. The scheduler ([`crate::scheduler::plan`]) reads both and decides
//! placement; nothing here decides anything.
//!
//! ## What this holds, and what it deliberately does not
//!
//! It holds **intent**. It does *not* hold ownership - which worker currently runs which cursor lives
//! in the hot store, next to the fence, for the reasons argued in RFC-0022's lease-placement note: a
//! lease and a fence in two different databases can disagree, and that disagreement is exactly the
//! split brain the fence exists to prevent.
//!
//! The practical consequence is worth stating, because it looks like a gap: **the control plane can
//! be wrong about what is running and nothing breaks.** It is a statement of what *should* be true.
//! Reconciliation reads ownership from the hot stores, which cannot be stale, because a lease is only
//! meaningful in the transaction that took it.
//!
//! ## Liveness
//!
//! A worker is alive if it has heartbeated within its TTL, measured on the **database's** clock - the
//! same discipline as the lease, and for the same reason. A worker whose own clock ran fast would
//! otherwise declare itself alive after everyone else had written it off, and the fleet would plan
//! around a machine that is not there.
//!
//! Losing the heartbeat does **not** stop a worker writing: that is the lease's job, and it expires
//! on its own schedule. The two are deliberately independent - a control plane outage must not stop
//! ingestion, only stop *rescheduling*.

use anyhow::{anyhow, Context, Result};

use crate::scheduler::{DesiredNest, Worker};

/// Meta-free, schema-per-fleet. One control plane serves one operator's fleet, so the schema name is
/// fixed rather than parameterised - a second fleet is a second database, not a second namespace in
/// the first.
const SCHEMA: &str = "nuthatch_control";

/// A worker's heartbeat TTL. Generous on purpose: rescheduling a cursor is expensive (drain, lease
/// handover, re-warm), so a worker that pauses for a few seconds should not lose its work. The lease
/// TTL is the tighter of the two, and it is the one that actually guards correctness.
pub const DEFAULT_WORKER_TTL_SECS: u64 = 30;

pub struct ControlPlane {
    conn: crate::pgstore::Conn,
}

impl ControlPlane {
    /// Connect and ensure the control-plane schema exists.
    pub fn connect(url: &str) -> Result<ControlPlane> {
        let config: postgres::Config = url
            .parse()
            .with_context(|| format!("cannot parse control-plane URL '{}'", redact(url)))?;
        let conn = crate::pgstore::Conn::spawn(config)
            .with_context(|| format!("cannot reach the control plane at '{}'", redact(url)))?;
        let cp = ControlPlane { conn };
        cp.migrate()?;
        Ok(cp)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.with(move |c| {
            c.batch_execute(&format!(
                r#"
                CREATE SCHEMA IF NOT EXISTS "{SCHEMA}";
                CREATE TABLE IF NOT EXISTS "{SCHEMA}".desired_nest (
                    name              TEXT PRIMARY KEY,
                    chain             TEXT        NOT NULL,
                    estimated_rss_mb  BIGINT      NOT NULL,
                    added_at          TIMESTAMPTZ NOT NULL DEFAULT now()
                );
                CREATE TABLE IF NOT EXISTS "{SCHEMA}".worker (
                    id            TEXT PRIMARY KEY,
                    budget_mb     BIGINT      NOT NULL,
                    last_seen_at  TIMESTAMPTZ NOT NULL DEFAULT now()
                );
                "#
            ))?;
            Ok(())
        })
    }

    // ---- desired state -------------------------------------------------------------------------

    /// Declare a nest should be running. Idempotent - re-declaring updates chain and estimate rather
    /// than failing, so an operator can correct a mistake without a delete-then-add dance that would
    /// briefly drain a healthy cursor.
    pub fn declare_nest(&self, nest: &DesiredNest) -> Result<()> {
        if nest.name.is_empty() || nest.chain.is_empty() {
            return Err(anyhow!("a desired nest needs both a name and a chain"));
        }
        let (name, chain, mb) = (
            nest.name.clone(),
            nest.chain.clone(),
            nest.estimated_rss_mb as i64,
        );
        self.conn.with(move |c| {
            c.execute(
                &format!(
                    "INSERT INTO \"{SCHEMA}\".desired_nest (name, chain, estimated_rss_mb) \
                     VALUES ($1, $2, $3) ON CONFLICT (name) DO UPDATE \
                     SET chain = EXCLUDED.chain, estimated_rss_mb = EXCLUDED.estimated_rss_mb"
                ),
                &[&name, &chain, &mb],
            )?;
            Ok(())
        })
    }

    /// Stop wanting a nest. Returns whether it was there - a scheduler logs a removal differently
    /// from a no-op, and an API returns 404 rather than 200 for the second.
    pub fn undeclare_nest(&self, name: &str) -> Result<bool> {
        let name = name.to_string();
        self.conn.with(move |c| {
            let n = c.execute(
                &format!("DELETE FROM \"{SCHEMA}\".desired_nest WHERE name = $1"),
                &[&name],
            )?;
            Ok(n > 0)
        })
    }

    /// Everything the operator currently wants running, name-ordered so the result is stable.
    pub fn desired(&self) -> Result<Vec<DesiredNest>> {
        self.conn.with(move |c| {
            let rows = c.query(
                &format!(
                    "SELECT name, chain, estimated_rss_mb FROM \"{SCHEMA}\".desired_nest \
                     ORDER BY name"
                ),
                &[],
            )?;
            Ok(rows
                .into_iter()
                .map(|r| DesiredNest {
                    name: r.get(0),
                    chain: r.get(1),
                    estimated_rss_mb: r.get::<_, i64>(2).max(0) as u64,
                })
                .collect())
        })
    }

    // ---- worker registry -----------------------------------------------------------------------

    /// Register or heartbeat a worker. One call for both: a worker that has been away long enough to
    /// be reaped should be able to rejoin by doing exactly what it does every second anyway, rather
    /// than needing to notice it was reaped and take a different path.
    pub fn heartbeat(&self, id: &str, budget_mb: u64) -> Result<()> {
        if id.is_empty() {
            return Err(anyhow!("a worker needs an id"));
        }
        let (id, budget) = (id.to_string(), budget_mb as i64);
        self.conn.with(move |c| {
            c.execute(
                &format!(
                    "INSERT INTO \"{SCHEMA}\".worker (id, budget_mb, last_seen_at) \
                     VALUES ($1, $2, now()) ON CONFLICT (id) DO UPDATE \
                     SET budget_mb = EXCLUDED.budget_mb, last_seen_at = now()"
                ),
                &[&id, &budget],
            )?;
            Ok(())
        })
    }

    /// Workers seen within `ttl_secs`, by the **database's** clock.
    ///
    /// The comparison is done in SQL rather than by fetching timestamps and comparing here, so a
    /// scheduler with a skewed clock cannot conjure or condemn workers.
    pub fn live_workers(&self, ttl_secs: u64) -> Result<Vec<Worker>> {
        let ttl = ttl_secs as f64;
        self.conn.with(move |c| {
            let rows = c.query(
                &format!(
                    "SELECT id, budget_mb FROM \"{SCHEMA}\".worker \
                     WHERE last_seen_at > now() - make_interval(secs => $1) ORDER BY id"
                ),
                &[&ttl],
            )?;
            Ok(rows
                .into_iter()
                .map(|r| Worker {
                    id: r.get(0),
                    budget_mb: r.get::<_, i64>(1).max(0) as u64,
                })
                .collect())
        })
    }

    /// Forget a worker outright - the graceful-shutdown path, so a worker that is going away on
    /// purpose does not have to be waited out.
    pub fn deregister(&self, id: &str) -> Result<bool> {
        let id = id.to_string();
        self.conn.with(move |c| {
            let n = c.execute(
                &format!("DELETE FROM \"{SCHEMA}\".worker WHERE id = $1"),
                &[&id],
            )?;
            Ok(n > 0)
        })
    }
}

fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            format!("{}://***@{}", &url[..scheme], &url[at + 1..])
        }
        _ => url.to_string(),
    }
}
