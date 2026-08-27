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
use duckdb::{Config, Connection};
use serde_json::{Map, Value};
#[cfg(test)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Cap DuckDB's working memory so `/sql` can't breach the embedded footprint budget.
const MEM_LIMIT: &str = "512MB";
const MAX_THREADS: i64 = 2;

/// Open an in-memory DuckDB whose file access is pinned to the nest's data dirs (#289).
///
/// DuckDB's `allowed_directories` is an *addition* to the allow-list while `enable_external_access`
/// is on, and a restriction only when it is off. The flag is startup-only, so it has to go on the
/// `Config`, not in a later `SET`. `lock_configuration` then freezes both so a query cannot widen
/// them. Measured against `libduckdb-sys` 1.10504.0, the bundled build.
fn allowed_read_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![dir.join(crate::seal::SEGMENTS_DIR), dir.join("labels")];
    // Runtime layout (RFC-0033): Parquet lives at `<root>/segments/{hash}.parquet`, not under
    // `data/<nid>/segments`. Locking only the per-dataset dir made `/sql` succeed with zero rows
    // on every mounted nest (#289 follow-up, `e2e_early_cutoff`).
    if let Some(shared) = crate::seal::shared_store(dir) {
        dirs.push(shared);
    }
    dirs
}

/// One cached read-only DuckDB (#295). A fresh in-memory instance per query was the rebuild the
/// issue named: open, lockdown, attach, teardown. The connection is still read-only and still
/// single-user; queries take the mutex, ingestion never writes here. An interrupt drops the slot
/// rather than leaving DuckDB half-cancelled for the next caller.
struct DuckCache {
    dir: PathBuf,
    sealed_through: u64,
    excluded: std::collections::BTreeSet<String>,
    inputs: std::collections::BTreeMap<PathBuf, DuckInputStamp>,
    last_used: u64,
    conn: Connection,
}

/// A content hash of one cache input, hex sha256 (#840).
///
/// **This was `(len, modified_ns)` and could not see a same-length rewrite.** Measured on the Linux
/// dev box, 500 trials of "write 27 bytes, stat, rewrite 27 different bytes, stat": 497 collisions
/// on btrfs, 499 on tmpfs. The cause is the mtime clock, not the filesystem - 2,000 consecutive
/// writes produced **nine** distinguishable timestamps, a granularity of ~3.3 ms. So on the platform
/// this deploys to, a `>` changed to a `<` in a view did not merely *risk* going unnoticed by the
/// cache, it went unnoticed essentially always, and the cached connection served the previous
/// definition with no error anywhere. On macOS APFS the same probe gives 0/500 at ~37 us resolution,
/// which is why it looked fine in local development.
///
/// The cost is reading these files rather than stat-ing them. They are `nuthatch.toml`, `views/*.sql`
/// and `labels/*.json` - and `attempt()` already stats every one of them on every query, so this is
/// a read where there was a stat, over files that are small by construction.
type DuckInputStamp = String;

fn content_stamp(path: &Path) -> Option<DuckInputStamp> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(hex::encode(Sha256::digest(&bytes)))
}

static DUCK_CACHE: OnceLock<Mutex<std::collections::HashMap<PathBuf, DuckCache>>> = OnceLock::new();
static DUCK_OPENS: OnceLock<Mutex<std::collections::HashMap<PathBuf, u64>>> = OnceLock::new();
static DUCK_USE: AtomicU64 = AtomicU64::new(0);
const DUCK_CACHE_CAPACITY: usize = 16;

fn duck_cache_lock() -> std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, DuckCache>>
{
    DUCK_CACHE
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

fn note_duck_open(dir: &Path) {
    *DUCK_OPENS
        .get_or_init(|| Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(dir.to_path_buf())
        .or_default() += 1;
}

/// Drop a nest's analytical connection when its runtime ownership ends (#824).
pub fn invalidate_duck_cache(dir: &Path) {
    duck_cache_lock().remove(dir);
}

fn duck_inputs(dir: &Path) -> std::collections::BTreeMap<PathBuf, DuckInputStamp> {
    let mut paths = vec![dir.join(crate::config::CONFIG_FILE)];
    if let Ok(entries) = std::fs::read_dir(dir.join("views")) {
        paths.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "sql")),
        );
    }
    if let Ok(entries) = std::fs::read_dir(dir.join(crate::labels::LABELS_DIR)) {
        paths.extend(
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json")),
        );
    }
    paths
        .into_iter()
        .filter_map(|path| {
            let stamp = content_stamp(&path)?;
            Some((path, stamp))
        })
        .collect()
}

fn retain_duck_cache(
    mut cache: std::sync::MutexGuard<'static, std::collections::HashMap<PathBuf, DuckCache>>,
    slot: DuckCache,
) {
    cache.insert(slot.dir.clone(), slot);
    while cache.len() > DUCK_CACHE_CAPACITY {
        let Some(victim) = cache
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(dir, _)| dir.clone())
        else {
            break;
        };
        cache.remove(&victim);
    }
}

#[cfg(test)]
fn duck_opens_for(dir: &Path) -> u64 {
    DUCK_OPENS
        .get()
        .and_then(|m| m.lock().ok().map(|g| g.get(dir).copied().unwrap_or(0)))
        .unwrap_or(0)
}

fn open_locked_duckdb(dir: &Path) -> Result<Connection> {
    note_duck_open(dir);
    let allowed: Vec<String> = allowed_read_dirs(dir)
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| format!("'{}'", p.display().to_string().replace('\'', "''")))
        .collect();
    // DuckDB's docs set the allow-list first, then turn external access off. Doing it the other
    // way round is refused: "Cannot change allowed_directories when enable_external_access is
    // disabled". The flag is *not* startup-only on 1.10504.0; a `SET` after open works, which is
    // why this was inert until now - we set the list and never flipped the flag.
    let config = Config::default()
        .max_memory(MEM_LIMIT)
        .context("duckdb max_memory")?
        .threads(MAX_THREADS)
        .context("duckdb threads")?;
    let conn = Connection::open_in_memory_with_flags(config).context("open DuckDB")?;
    let lockdown = format!(
        "SET allowed_directories=[{}]; SET enable_external_access=false; SET lock_configuration=true;",
        allowed.join(", ")
    );
    conn.execute_batch(&lockdown)
        .context("failed to lock down DuckDB filesystem access")?;
    Ok(conn)
}

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

/// The result of a query: the rows, plus the two ways they can fail to be the whole answer.
///
/// `truncated` is the caller's own row cap biting. `degraded_tables` is the other one and it is not
/// the caller's doing: the tables whose **cold data was incomplete** when the views were built for
/// this query - a sealed segment the manifest lists but that could not be read, so the view was
/// rebuilt from what remained (#430, #433), or a table whose view could not be defined at all.
///
/// Reduction is the right policy - a bad segment must not delete a table, see [`define_views`] - but
/// it makes the query **succeed** with quietly less data, and `SELECT SUM(value)` then returns a
/// number that is wrong rather than absent (#435). Empty on the healthy path, which is every query
/// on a nest whose segments all match their content addresses.
///
/// Scope note: the views cover every table in the nest, not just the ones this SQL touches, so this
/// over-reports - a bad segment on an untouched table still flags the query. Narrowing it would mean
/// parsing the SQL for table references, which is exactly the kind of guess that produces a
/// confidently wrong answer. Over-reporting *with the names attached* lets the caller judge; silence
/// does not.
///
/// **Which constrains how a surface may word it.** Because this is a property of the nest and not of
/// the answer, a caveat must be a statement about the nest - "this nest could not serve complete cold
/// data for X" - and never about these rows. A query over a healthy table on a nest with one bad
/// segment is complete and correct, and `SELECT 1` and `.tables` have no rows drawn from these tables
/// at all. Nor may it name a cause: the undefinable-view arm above lands here with every segment
/// binding fine. Both mistakes shipped in the first rendering of this field and neither test nor
/// mutation could see them, because every fixture had exactly one table.
///
/// `tip_unavailable` is the other kind of incomplete, and deliberately not folded into
/// `degraded_tables` (#472). A hot-scan failure (`begin_read`, `open_table`, `t.iter()`, or a row
/// partway through) is not per-table the way a bad segment is - it drops the *entire* unsealed tip,
/// every table at once - and its cause and remedy differ: a damaged or unreadable hot store, not a
/// corrupt segment. Shoehorning it into `degraded_tables` would either name every table for a failure
/// that named none of them, or name none and repeat #472's silence. `QueryOutput` itself never sets
/// this field - the hot scan happens in the caller, above `query_hot_cold` - so a caller that scans the
/// tip assigns it after the query returns.
#[derive(Debug, Default)]
pub struct QueryOutput {
    pub rows: Vec<Value>,
    pub truncated: bool,
    pub degraded_tables: std::collections::BTreeSet<String>,
    pub tip_unavailable: bool,
    /// The base tables this statement referenced, lowercased, or `None` where the parse was
    /// unavailable and the answer is therefore not known.
    ///
    /// Comes from the same security walk that `reject_unknown_table_refs` already performs - one
    /// parse, one answer about what a query reaches, so the control and the provenance can never
    /// disagree. `/sql` uses it to name which **maintained relations** answered (#822 criterion 9);
    /// `None` leaves that block off the response entirely rather than reporting an empty set, since
    /// "we did not parse it" and "it touched no entity" are different facts.
    pub referenced_tables: Option<std::collections::BTreeSet<String>>,
}

impl QueryOutput {
    /// Whether any table's cold data was incomplete for this query. The one-bit form of
    /// `degraded_tables`, for surfaces with room to say only yes or no.
    pub fn degraded(&self) -> bool {
        !self.degraded_tables.is_empty()
    }
}

/// Hot (unsealed) rows grouped by logical table - from [`crate::store::Store::hot_rows_by_table`].
/// Passed to the query path so the live tip is `UNION ALL`'d into each table's view (RFC-0013).
pub type HotRows = std::collections::HashMap<String, Vec<Value>>;

/// **The nest-wide corruption sweep.** Which of this nest's tables have sealed segments that will
/// not bind, whether or not anybody has asked about them.
///
/// This used to be a side effect of every query: `define_views` bound *every* table in the manifest
/// on every request, so a query about one table happened to discover corruption in another. That was
/// issue #477's contract and it was paid for at ~62 µs per sealed segment per request - 2.5 seconds
/// on a 38,428-segment nest, before the query read a row (#896).
///
/// So the discovery moved here, where it can run on a cadence and be reported by `/ready` without a
/// caller having to stumble into it. A query still degrades correctly on a table *it* reads; what it
/// no longer does is survey the rest of the nest on the caller's time.
///
/// Deliberately opens its own connection rather than borrowing the cached one: this defines every
/// view, which is exactly what the cached connection is now avoiding, and leaving that behind on a
/// pooled connection would hand the next query a catalogue full of definitions it did not ask for.
pub fn degraded_tables(
    dir: &Path,
    declared: &[crate::registry::TableSchema],
) -> Result<std::collections::BTreeSet<String>> {
    let conn = open_locked_duckdb(dir).context("failed to open DuckDB for the segment sweep")?;
    define_views(
        &conn,
        dir,
        &HotRows::new(),
        u64::MAX,
        &Default::default(),
        declared,
        None,
    )
}

/// Run a read-only query to completion. Only SELECT/WITH statements are accepted - this is a query
/// surface, not a mutation surface. Unguarded: for trusted, registry-built SQL that must finish.
pub fn query(dir: &Path, sql: &str) -> Result<Vec<Value>> {
    Ok(run(dir, sql, None, &HotRows::new(), u64::MAX, &[])?.rows)
}

/// Run a trusted read-only query over **only the segments finalized at/below `sealed_through`** (the
/// same watermark filter `define_views` applies). The warm-restart view rebuilds use this instead of
/// [`query`] (which reads *every* segment): their cold seed must stay disjoint from the hot replay, and
/// a crash in the seal->prune window leaves already-sealed rows still in the hot store. Folding all
/// segments here would then count those rows twice - permanently double-counting balances and the
/// compliance exposure/velocity views. Bounding to the persisted watermark keeps cold (<= watermark)
/// and hot (everything still in the store) partitioned regardless of crash timing.
fn query_cold(dir: &Path, sql: &str, sealed_through: u64) -> Result<Vec<Value>> {
    Ok(run(dir, sql, None, &HotRows::new(), sealed_through, &[])?.rows)
}

/// Run a read-only query under a resource guard, over the **sealed segments only** - the cold path used
/// by trusted callers and the `/table` endpoint's cold fill (which merges hot itself). See [`QueryGuard`].
pub fn query_guarded(dir: &Path, sql: &str, guard: QueryGuard) -> Result<QueryOutput> {
    // Cold-only: `u64::MAX` includes every sealed segment (no hot rows to keep disjoint from).
    run(dir, sql, Some(guard), &HotRows::new(), u64::MAX, &[])
}

/// Run a guarded read-only query over the sealed segments **and the hot tip** - the public `/sql`
/// surface (RFC-0013). `hot` is the unsealed rows grouped by table; each is `UNION ALL`'d into its
/// table's view. A query outliving `guard.timeout` is interrupted; a result past `guard.max_rows` is
/// truncated and flagged.
///
/// `declared` is the live, registry-derived schema (`indexer::full_schema`) - see `define_views` (#663)
/// for why a table this lists gets an empty view even when `schema.json` on disk has fallen behind it.
pub fn query_hot_cold(
    dir: &Path,
    sql: &str,
    guard: QueryGuard,
    hot: &HotRows,
    sealed_through: u64,
    declared: &[crate::registry::TableSchema],
) -> Result<QueryOutput> {
    run(dir, sql, Some(guard), hot, sealed_through, declared)
}

/// How one attempt at a query ended.
///
/// The distinction that matters for #433 is `DiedExecuting`: a query that fails to **bind** is a
/// question about names - a typo, a missing column, an unknown table - and no corrupt page can cause
/// one, so it must never trigger the integrity sweep. Only a query that bound and then died while
/// reading rows is worth paying for.
///
/// **That split rules out a typo and nothing else, and on its own it does not bound the sweep.** It
/// was claimed here that it did. `SELECT CAST('x' AS INTEGER)` binds, dies executing, names no table
/// and is 27 bytes; measured, it hashed every segment of a healthy nest, once per request, on a
/// surface with no auth and two concurrency permits. What bounds the sweep is `tables` below: the
/// tables the failed query actually referenced, which for that query is none.
enum Attempt {
    Ok(QueryOutput),
    DiedExecuting {
        error: anyhow::Error,
        /// The base tables the query named, lowercased - the reachability bound on the sweep. `None`
        /// when DuckDB could not serialize the statement, i.e. when we do not know what it reached;
        /// see `run` for why that skips the sweep rather than widening it.
        tables: Option<std::collections::BTreeSet<String>>,
    },
}

/// Test-only knob: an artificial delay standing in for a slow first attempt, so a test can drive the
/// deadline shared across both attempts and the sweep well past expiry by the time the sweep runs -
/// distinct from a **fresh** `guard.timeout` recomputed at the sweep call site, which this delay does
/// not touch.
///
/// Keyed by `dir`, not a bare process-global (#529) - `run` is `query_guarded`'s entry point and dozens
/// of unrelated tests call it, so a global read unconditionally in the retry path would delay *any*
/// concurrently running test that also died on its first attempt, for as long as this knob happened to
/// be armed - the same class of cross-test contamination `seal::test_set_sweep_expire_after_checks`
/// was fixed to avoid, just reached through a different knob. Keying by `dir` means only a call
/// against this test's own tempdir ever sees the delay.
#[cfg(test)]
fn test_first_attempt_delays() -> &'static Mutex<HashMap<PathBuf, u64>> {
    static DELAYS: OnceLock<Mutex<HashMap<PathBuf, u64>>> = OnceLock::new();
    DELAYS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn test_set_first_attempt_delay_ms(dir: &Path, ms: u64) {
    if ms == 0 {
        test_first_attempt_delays().lock().unwrap().remove(dir);
    } else {
        test_first_attempt_delays()
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), ms);
    }
}

fn run(
    dir: &Path,
    sql: &str,
    guard: Option<QueryGuard>,
    hot: &HotRows,
    sealed_through: u64,
    declared: &[crate::registry::TableSchema],
) -> Result<QueryOutput> {
    // One deadline for the whole call, computed once - not a fresh `guard.timeout` handed to each
    // `attempt` (#476). Before this, the watchdog only ever bounded a single `attempt`: the first
    // execution could run a full `timeout`, the sweep between the two attempts ran with nothing
    // watching it at all, and the retry got its own fresh `timeout` on top - up to 2x the advertised
    // budget in query execution alone, plus whatever the sweep cost. Sharing one deadline across both
    // attempts and the sweep makes `guard.timeout` the actual wall-clock ceiling on the whole call.
    let deadline = guard.map(|g| Instant::now() + g.timeout);
    let nothing_excluded = std::collections::BTreeSet::new();
    let (e, tables) = match attempt(
        dir,
        sql,
        guard,
        hot,
        sealed_through,
        &nothing_excluded,
        deadline,
        declared,
    )? {
        Attempt::Ok(out) => return Ok(out),
        Attempt::DiedExecuting { error, tables } => (error, tables),
    };
    #[cfg(test)]
    {
        let ms = test_first_attempt_delays()
            .lock()
            .unwrap()
            .get(dir)
            .copied()
            .unwrap_or(0);
        if ms > 0 {
            std::thread::sleep(Duration::from_millis(ms));
        }
    }
    // **A segment that binds but will not read takes the whole query down** (#433). `read_parquet`
    // validates the footer while the view is being created, which is where #430's reduction hooks in;
    // corruption that leaves the footer intact and destroys the data region passes that probe and
    // fails at execution instead, with `Invalid Error: don't know what type: ` and nothing named.
    //
    // The principle #430 established is that a bad segment *reduces* its table rather than deleting
    // it, and it should not stop holding just because the corruption is deeper in the file. So: ask
    // which segments no longer match their content address, and if any do not, rebuild the views
    // without them and answer from what remains. Only once - a second execution failure is not about
    // segment integrity, because we just verified every segment we kept.
    //
    // Ask only about the segments backing the tables this query **named** - the sweep reads and
    // hashes files, and a segment the query never read cannot be what killed it. Bounding it by
    // reachability rather than by a cache is what keeps this affordable without anything that can go
    // stale (a memo keyed on mtime was tried here and was wrong; see `segments_failing_verification`).
    let Some(tables) = tables else {
        // DuckDB would not serialize the statement, so we do not know what it reached. Sweeping
        // everything on the strength of not knowing is how the unbounded version comes back in
        // through the fallback; the query fails with its own error instead, which is what it did
        // before any of this existed. Loud and bounded beats quiet and expensive.
        tracing::warn!(
            "query died executing but its statement could not be parsed for table references - \
             skipping the segment integrity sweep (cold data, if corrupt, is not reduced here)"
        );
        return Err(e);
    };
    // The budget may already be spent by the first attempt alone; say so plainly rather than silently
    // skipping the sweep and returning `e`, which (for a guard-bound caller) could otherwise read as
    // an ordinary query error rather than the timeout it actually is.
    if let Some(secs) = timed_out(guard, deadline) {
        bail!("query exceeded the {secs}s time budget on the read-only SQL surface");
    }
    let corrupt = crate::seal::segments_failing_verification(dir, &tables, deadline);
    if corrupt.is_empty() {
        if let Some(secs) = timed_out(guard, deadline) {
            bail!("query exceeded the {secs}s time budget on the read-only SQL surface");
        }
        return Err(e);
    }
    match attempt(
        dir,
        sql,
        guard,
        hot,
        sealed_through,
        &corrupt,
        deadline,
        declared,
    )? {
        Attempt::Ok(out) => Ok(out),
        Attempt::DiedExecuting { error, .. } => Err(error),
    }
}

/// `Some(guard.timeout.as_secs())` when `deadline` has already passed, else `None`. Shared by the two
/// places in [`run`] that must turn "we ran out of the shared deadline" into the same wording
/// `attempt`'s own watchdog uses, rather than leaking `e`'s unrelated error text as the reason.
fn timed_out(guard: Option<QueryGuard>, deadline: Option<Instant>) -> Option<u64> {
    if deadline.is_some_and(|d| Instant::now() >= d) {
        Some(guard.map(|g| g.timeout.as_secs()).unwrap_or(0))
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt(
    dir: &Path,
    sql: &str,
    guard: Option<QueryGuard>,
    hot: &HotRows,
    sealed_through: u64,
    excluded: &std::collections::BTreeSet<String>,
    deadline: Option<Instant>,
    declared: &[crate::registry::TableSchema],
) -> Result<Attempt> {
    // Check the first *statement keyword*, past any leading whitespace and SQL comments - a query
    // that opens with `-- note` or `/* … */` is still a SELECT. DuckDB gets the original text.
    let head = strip_leading_sql_comments(sql).to_ascii_lowercase();
    if !(head.starts_with("select") || head.starts_with("with")) {
        bail!("only SELECT/WITH queries are allowed on the read-only SQL surface");
    }
    // Read-only is enforced four-deep - do NOT loosen any of these without re-reasoning SEC-7:
    //   1. this leading-keyword gate rejects a *statement* that opens with INSERT/UPDATE/DELETE/COPY/
    //      ATTACH/PRAGMA/…;
    //   2. `reject_with_prefixed_dml` refuses `WITH cte AS (…) INSERT/UPDATE/DELETE/COPY …`. The
    //      leading gate accepts any `WITH`. The previous comment claimed DuckDB would not parse
    //      DML after a CTE list; that is DuckDB's choice, not ours, and it is the same class of
    //      claim as "`conn.prepare` is single-statement", which was false;
    //   3. `reject_statement_stacking` refuses a `;`-stacked second statement. This used to say
    //      "`conn.prepare` is single-statement" - it is NOT (the bundled duckdb-rs prepares AND runs
    //      `SELECT 1; INSERT …`), which made a stacked `COPY … TO` an arbitrary file write. See that
    //      function's docs;
    //   4. the connection is a fresh in-memory instance whose only tables are read-only views over
    //      Parquet plus an ephemeral hot temp table, so even a hypothetical write has no durable target.
    // `COPY … TO` (a file write) must *lead* the statement or follow a CTE list, which (1) and (2) block.
    // SEC-2: refuse DuckDB filesystem/network table functions (`read_text`, `glob`, …) - they read
    // files from inside a plain SELECT, past the keyword gate, and would otherwise leak any file the
    // process can read (e.g. `nuthatch.toml`'s secrets). This is the primary control; the
    // `allowed_directories` lockdown below is defense-in-depth and, as of #289, actually enforced.
    reject_with_prefixed_dml(sql)?;
    reject_statement_stacking(sql)?;
    reject_file_access(sql)?;
    reject_replacement_scan(sql)?;

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
    // Open with `enable_external_access=false` first (#289): `allowed_directories` is an *addition*
    // to the allow-list when external access is on, and a restriction only when it is off. That is
    // DuckDB's own docs, and it is why the lockdown was inert until this flag went in at startup.
    //
    // #295: reuse the connection when the nest, watermark and exclusion set match. Hot rows still
    // reload below (`define_views`); new sealed segments change `sealed_through` and miss the cache.
    // Taken out of the slot for the query so an interrupt can drop it without fighting the mutex
    // borrow; put back only if DuckDB was not cancelled underneath us.
    let inputs = duck_inputs(dir);
    let mut slot = duck_cache_lock().remove(dir);
    let reusable = slot.as_ref().is_some_and(|c| {
        c.sealed_through == sealed_through && c.excluded == *excluded && c.inputs == inputs
    });
    if !reusable {
        slot = Some(DuckCache {
            dir: dir.to_path_buf(),
            sealed_through,
            excluded: excluded.clone(),
            inputs,
            last_used: DUCK_USE.fetch_add(1, Ordering::Relaxed),
            conn: open_locked_duckdb(dir).context("failed to open DuckDB")?,
        });
    }
    let mut slot = slot.expect("just inserted");
    slot.last_used = DUCK_USE.fetch_add(1, Ordering::Relaxed);
    let (referenced, degraded_tables, interrupted, outcome, cap) = {
        let conn = &slot.conn;
        let walked = reject_unknown_table_refs(conn, sql)?;
        // No parse means no idea what the statement reaches, and the safe answer to that is "all of
        // it" on both counts.
        let surveys = walked.as_ref().map(|(_, sv)| *sv).unwrap_or(true);
        let referenced = walked.map(|(r, _)| r);
        // Define views only for what this statement can reach (#896). `None` - an unparsed statement
        // or a shape `reachable_tables` will not vouch for - defines everything, as before.
        // A statement that reaches into a catalogue schema, or calls one of DuckDB's own
        // enumerating table functions, is asking *what tables exist* - so every view has to exist
        // for it to answer. Those keep the old whole-nest definition; everything else is narrowed.
        let wanted = if surveys {
            None
        } else {
            referenced
                .as_ref()
                .and_then(|r| reachable_tables(conn, dir, r))
        };
        let degraded_tables = define_views(
            conn,
            dir,
            hot,
            sealed_through,
            excluded,
            declared,
            wanted.as_ref(),
        )?;
        // A nest can ship derived-entity views (`views/*.sql`) that build on the per-event tables; the
        // analytical `/sql` surface sees them. Point-reads (`net_balances`, `get_row`) deliberately skip
        // this - they only touch the raw per-event tables.
        define_nest_views(conn, dir);
        // The compliance substrate: expose imported label snapshots as a `labels` view so `/sql` (and the
        // internal `cold_exposure` fold) can join against them. Best-effort - no snapshots, no view.
        define_labels_view(conn, dir);
        // Factory nests (RFC-0009): a `{template}__children` view over the sealed factory events, so
        // "which pools, discovered when, by which parent" is one query. Best-effort - no factories, no-op.
        define_children_views(conn, dir);

        // Every view this query could be reading now exists, so the names it used can be widened to the
        // tables behind them. This is the sweep's reachability bound (see `Attempt`), and it has to
        // happen here rather than beside the security walk: at that point the catalogue was empty.
        let referenced = referenced.map(|names| expand_through_views(conn, &names));

        // Hard wall-clock deadline for the untrusted surface: a watchdog thread interrupts the in-flight
        // query once it outlives `deadline` (a cartesian blow-up can't be stopped by the memory cap
        // alone). `interrupt()` makes the running query fail; we translate that into a clear timeout error
        // below. On normal completion we signal the watchdog so it never fires. Unguarded (trusted)
        // queries skip all of this and run to completion.
        //
        // Waits on `deadline`, not a fresh `guard.timeout`, so a second `attempt` (the #433 reduced retry)
        // only gets whatever's left of the *first* attempt's budget rather than a brand-new full timeout
        // (#476) - `run` computes `deadline` once and threads it through both calls. A deadline already in
        // the past (the sweep between attempts ran long) makes `recv_timeout` fire immediately.
        let interrupted = Arc::new(AtomicBool::new(false));
        let watchdog = guard.zip(deadline).map(|(_, d)| {
            let handle = conn.interrupt_handle();
            let flag = interrupted.clone();
            let (tx, rx) = mpsc::channel::<()>();
            let join = std::thread::spawn(move || {
                let remaining = d.saturating_duration_since(Instant::now());
                // Only a genuine timeout interrupts; a value (normal completion) or a dropped sender
                // (panic) leaves the query alone.
                if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(remaining) {
                    flag.store(true, Ordering::SeqCst);
                    handle.interrupt();
                }
            });
            (tx, join)
        });

        let cap = guard.map(|g| g.max_rows);
        let outcome = collect(conn, sql, cap);

        // Stop the watchdog before interpreting the result: a value arriving before the deadline makes
        // `recv_timeout` return `Ok`, so it won't interrupt; then join so it can't fire late.
        if let Some((tx, join)) = watchdog {
            let _ = tx.send(());
            let _ = join.join();
        }
        (referenced, degraded_tables, interrupted, outcome, cap)
    };
    if interrupted.load(Ordering::SeqCst) {
        drop(slot);
    } else {
        retain_duck_cache(duck_cache_lock(), slot);
    }

    let (mut rows, over_cap) = match outcome {
        Ok(v) => v,
        // #529: the watchdog's `interrupt()` cancels whatever DuckDB phase is currently running, not
        // just an in-flight execute - a query that gets no further than `conn.prepare` before the
        // deadline fires still dies to it, and did so leaking DuckDB's raw "Interrupted!" text here
        // (`Died::Binding` never checked `interrupted`, only `Died::Executing` did). Invisible under
        // light load, where `prepare()` finishes in microseconds long before any real deadline; a
        // heavily contended box can stall `prepare()` itself past the budget, at which point the
        // untrusted `/sql` surface was supposed to say "query exceeded budget" and instead surfaced an
        // internal DuckDB error string - the same class of bug this guard exists to prevent.
        Err(Died::Binding(e)) => {
            if interrupted.load(Ordering::SeqCst) {
                let secs = guard.map(|g| g.timeout.as_secs()).unwrap_or(0);
                bail!("query exceeded the {secs}s time budget on the read-only SQL surface");
            }
            return Err(e);
        }
        Err(Died::Executing(e)) => {
            if interrupted.load(Ordering::SeqCst) {
                let secs = guard.map(|g| g.timeout.as_secs()).unwrap_or(0);
                bail!("query exceeded the {secs}s time budget on the read-only SQL surface");
            }
            // Handed back rather than returned: the caller decides whether a corrupt segment explains
            // it and is worth one reduced retry (#433). The tables ride along because they come from
            // the security walk that already ran above - one parse, one answer about what this query
            // reaches, used by both controls.
            return Ok(Attempt::DiedExecuting {
                error: e,
                tables: referenced,
            });
        }
    };

    let truncated = match cap {
        Some(max) if over_cap => {
            rows.truncate(max);
            true
        }
        _ => false,
    };
    Ok(Attempt::Ok(QueryOutput {
        rows,
        truncated,
        degraded_tables,
        tip_unavailable: false,
        referenced_tables: referenced,
    }))
}

/// Which phase of [`collect`] a failure came from. See [`Attempt`] for why the split is load-bearing.
enum Died {
    /// `conn.prepare` refused it: a name the catalogue does not have, a type that does not check.
    Binding(anyhow::Error),
    /// It bound, then died running or materialising rows - the only shape a corrupt page produces.
    Executing(anyhow::Error),
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
    let mut rows = stmt
        .query([])
        .context("query failed")
        .map_err(Died::Executing)?;
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

/// Refuse `WITH cte AS (…) INSERT/UPDATE/DELETE/COPY …` (SEC-7).
///
/// The leading-keyword gate accepts any statement that opens with `WITH`. A CTE list is only
/// prefix; the actual statement follows the last `AS (subquery)`. That statement must be SELECT
/// (or VALUES / TABLE, which DuckDB treats as a query). Anything else is DML or DDL riding a
/// prefix the keyword gate already blessed.
///
/// String-literal and identifier aware, same as [`reject_statement_stacking`]: `WITH t AS
/// (SELECT 'INSERT') SELECT 1` is a query, `WITH t AS (SELECT 1) INSERT INTO t SELECT 1` is not.
/// Comments are stripped first. A CTE list we cannot parse is refused rather than handed to
/// DuckDB - fail closed, the same direction as a `;` inside an unparsed `$$` block.
fn reject_with_prefixed_dml(sql: &str) -> Result<()> {
    let cleaned = strip_all_sql_comments(sql);
    let head = cleaned.trim_start();
    if !sql_keyword_at(head, "with") {
        return Ok(());
    }
    let Some(rest) = skip_ctes(head) else {
        bail!("only SELECT/WITH queries are allowed on the read-only SQL surface");
    };
    let rest = rest.trim_start();
    if sql_keyword_at(rest, "select")
        || sql_keyword_at(rest, "values")
        || sql_keyword_at(rest, "table")
    {
        return Ok(());
    }
    bail!(
        "WITH-prefixed DML is not allowed on the read-only SQL surface \
         (WITH … INSERT/UPDATE/DELETE/COPY)"
    )
}

/// True when `s` opens with `kw` as a whole SQL word (case-insensitive, not `without` for `with`).
fn sql_keyword_at(s: &str, kw: &str) -> bool {
    let s = s.trim_start();
    if s.len() < kw.len() {
        return false;
    }
    if !s[..kw.len()].eq_ignore_ascii_case(kw) {
        return false;
    }
    match s[kw.len()..].chars().next() {
        None => true,
        Some(c) => !sql_ident_cont(c),
    }
}

fn sql_ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Consume `WITH [RECURSIVE] name AS [(…)] [, name AS (…)]*` and return the remainder.
fn skip_ctes(sql: &str) -> Option<&str> {
    let s = strip_sql_keyword(sql, "with")?;
    let s = strip_sql_keyword(s, "recursive").unwrap_or(s);
    let mut s = s;
    loop {
        let (_, rest) = next_sql_ident(s)?;
        s = rest.trim_start();
        if s.starts_with('(') {
            s = skip_balanced_parens(s)?.trim_start();
        }
        s = strip_sql_keyword(s, "as")?.trim_start();
        if let Some(rest) = strip_sql_keyword(s, "not") {
            s = strip_sql_keyword(rest.trim_start(), "materialized")?.trim_start();
        } else if let Some(rest) = strip_sql_keyword(s, "materialized") {
            s = rest.trim_start();
        }
        if !s.starts_with('(') {
            return None;
        }
        s = skip_balanced_parens(s)?.trim_start();
        if s.starts_with(',') {
            s = s[1..].trim_start();
            continue;
        }
        return Some(s);
    }
}

fn strip_sql_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if !sql_keyword_at(s, kw) {
        return None;
    }
    Some(&s[kw.len()..])
}

/// Next SQL identifier: a quoted `"name"` (with `""` escapes) or a bare `[A-Za-z0-9_]+`.
fn next_sql_ident(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let b = s.as_bytes();
    if b.first() == Some(&b'"') {
        let mut i = 1;
        while i < b.len() {
            if b[i] == b'"' {
                if i + 1 < b.len() && b[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                return Some((&s[..=i], &s[i + 1..]));
            }
            i += 1;
        }
        return None;
    }
    let end = s.find(|c: char| !sql_ident_cont(c)).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// `s` starts with `(`. Return the suffix after the matching `)`, string-aware.
fn skip_balanced_parens(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut i = 0;
    let mut depth = 0;
    let (mut in_single, mut in_double) = (false, false);
    while i < b.len() {
        match b[i] {
            b'\'' if !in_double => {
                if in_single && i + 1 < b.len() && b[i + 1] == b'\'' {
                    i += 1;
                } else {
                    in_single = !in_single;
                }
            }
            b'"' if !in_single => in_double = !in_double,
            b'(' if !in_single && !in_double => depth += 1,
            b')' if !in_single && !in_double => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[i + 1..]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

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
///
/// Returns the **base tables the statement referenced**, lowercased (DuckDB matches identifiers
/// case-insensitively), or `None` where the parse was unavailable and the answer is therefore not
/// known. That set is the integrity sweep's reachability bound (see [`Attempt`]), and it comes from
/// this walk rather than a second one so the two can never disagree about what a query reaches. CTE
/// names parse as `BASE_TABLE` too and are included; a CTE named after a real table can only widen
/// the set to a table that exists, which is the safe direction.
fn reject_unknown_table_refs(
    conn: &Connection,
    sql: &str,
) -> Result<Option<(std::collections::BTreeSet<String>, bool)>> {
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let Ok(ast) = conn.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
        r.get::<_, String>(0)
    }) else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&ast) else {
        return Ok(None);
    };
    if v.get("error").and_then(Value::as_bool) == Some(true) {
        // DuckDB could not parse it. Let it say so itself, with its own error message.
        return Ok(None);
    }
    let mut referenced = std::collections::BTreeSet::new();
    // Whether this statement reaches outside the nest's own tables - a catalogue schema, or a
    // catalogue-listing table function. Such a statement needs **every** view defined, because what
    // it is asking for is the list of them (#896).
    let mut surveys = false;
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
                // `duckdb_views()`, `duckdb_tables()` and friends enumerate the catalogue.
                if f.starts_with("duckdb_") {
                    surveys = true;
                }
            }
            "QUALIFIED_SCHEMA" => surveys = true,
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
            "BASE_TABLE" => {
                referenced.insert(name.to_ascii_lowercase());
            }
            _ => {}
        }
    });
    match bad {
        Some(why) => bail!("{why} - the SQL surface serves this nest's tables and views only"),
        None => Ok(Some((referenced, surveys))),
    }
}

/// Widen the names a query used to the names those names *read*, following the views this connection
/// has just defined.
///
/// Without this, bounding the sweep by reachability would quietly cost authored views their reduction
/// (RFC-0001 `views/*.sql`, plus the generated `labels` and `{template}__children` views): a query
/// over `big_transfers` names `big_transfers`, which is no table in the manifest, so nothing would be
/// verified and a page-corrupt segment under that view would fail the query instead of reducing it -
/// exactly the behaviour #433 is fixing, reintroduced one layer up. Measured, not assumed: the test
/// `a_page_corrupt_segment_under_an_authored_view_still_reduces` fails with this function removed.
///
/// Widening is safe in the direction it goes: it can only add tables the query genuinely reads. The
/// attacker's case is a query that names *nothing*, and nothing expands to nothing.
///
/// Best-effort by design. A view whose definition cannot be re-parsed is skipped rather than treated
/// as "could be anything" - the cost of a miss is a lost reduction on that one query (loud: the query
/// fails with DuckDB's own error), and the cost of the other choice is the unbounded sweep coming
/// back in through a fallback, which is the defect this whole change exists to remove.
fn expand_through_views(
    conn: &Connection,
    named: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeSet<String> {
    let mut defs: std::collections::BTreeMap<String, String> = Default::default();
    let listed = conn
        .prepare("SELECT view_name, sql FROM duckdb_views()")
        .and_then(|mut s| {
            let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for (name, sql) in rows.flatten() {
                defs.insert(name.to_ascii_lowercase(), sql);
            }
            Ok(())
        });
    if listed.is_err() {
        return named.clone();
    }

    let mut out = named.clone();
    let mut frontier: Vec<String> = named.iter().cloned().collect();
    // A view built on a view built on a view: follow the chain, but never in circles. DuckDB refuses
    // to create a cyclic view, so this bound is a backstop rather than the mechanism.
    for _ in 0..8 {
        let mut next = Vec::new();
        for name in frontier.drain(..) {
            let Some(sql) = defs.get(&name) else { continue };
            let Some(body) = view_body(sql) else { continue };
            let Some(inner) = base_tables_in(conn, body) else {
                continue;
            };
            for t in inner {
                if out.insert(t.clone()) {
                    next.push(t);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    out
}

/// The `SELECT` inside a stored `CREATE VIEW … AS …`, which is all `json_serialize_sql` will accept
/// (it answers `Only SELECT statements can be serialized to json!` for the whole statement - measured
/// in the DuckDB CLI, not assumed). `None` when the text is not that shape, which skips the view.
fn view_body(create_view_sql: &str) -> Option<&str> {
    let lower = create_view_sql.to_ascii_lowercase();
    let (view, view_end) = find_keyword(&lower, "view", 0)?;
    let _ = view;
    let (_, as_end) = find_keyword(&lower, "as", view_end)?;
    Some(create_view_sql[as_end..].trim())
}

/// Where `word` appears in `haystack` as a whole token at or after `from`, as `(start, end)`.
///
/// **Bounded by any whitespace, not by spaces.** This was `find(" as ")`, which needs a literal
/// space on both sides - and a real authored view puts a newline after the keyword:
///
/// ```sql
/// CREATE VIEW indexer_rewards AS
/// SELECT "indexer", SUM("tokensRewards_dec")::VARCHAR AS rewards
/// FROM "service__indexing_rewards_collected" GROUP BY "indexer";
/// ```
///
/// That is Lodestar's `views/40-indexers.sql`, and the space-bounded search simply did not find the
/// keyword. Harmless while the only caller was `expand_through_views`, which merely *widens* the
/// integrity sweep's bound and is allowed to be imprecise. It stopped being harmless when #896 made
/// the same parse decide which views get defined at all: the view's source table was never defined,
/// and a view that plainly exists came back as `Catalog Error: Table with name indexer_rewards does
/// not exist`.
fn find_keyword(haystack: &str, word: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = haystack.as_bytes();
    let mut at = from;
    while let Some(i) = haystack[at..].find(word) {
        let start = at + i;
        let end = start + word.len();
        let before_ok = start > 0 && bytes[start - 1].is_ascii_whitespace();
        let after_ok = end < bytes.len() && bytes[end].is_ascii_whitespace();
        if before_ok && after_ok {
            return Some((start, end));
        }
        at = end;
    }
    None
}

/// The name a `CREATE [OR REPLACE] VIEW <name> AS …` declares, lowercased. The mirror of
/// [`view_body`], and parsed the same coarse way: the text between ` view ` and the first ` as `
/// past it.
fn view_name(create_view_sql: &str) -> Option<String> {
    let lower = create_view_sql.to_ascii_lowercase();
    let (_, view_end) = find_keyword(&lower, "view", 0)?;
    let (as_at, _) = find_keyword(&lower, "as", view_end)?;
    let name = lower[view_end..as_at].trim().trim_matches('"').trim();
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| name.to_string())
}

/// Which tables a statement can actually reach - the names it uses in table position, widened
/// through any **authored view** among them to the base tables that view reads.
///
/// This is what lets `define_views` skip the rest. Defining a view costs DuckDB a parse of SQL text
/// carrying every one of that table's sealed segment paths, and it was being paid for all 34 tables
/// of a nest on every request: `SELECT 1` cost 2,465 ms on a 38,428-segment nest against 263 ms on a
/// 2,985-segment one, about 62 µs per segment, for tables the query never named (#896).
///
/// Read from `views/*.sql` **on disk** rather than from `duckdb_views()`, unlike
/// [`expand_through_views`], for two reasons: `define_nest_views` has not run yet at this point in
/// `run`, and on a pooled connection the catalogue may still hold a *previous* request's
/// definitions. The files are the authored truth; the catalogue is a cache of it.
///
/// `None` means "could not work it out", and every caller must then define everything. Returned for
/// a view body that will not parse, and for any `…__children` name: those views are built by
/// `define_children_views` after this point, out of factory tables enumerated from the config, and
/// working out which ones here would be a second copy of that logic to keep in step.
fn reachable_tables(
    conn: &Connection,
    dir: &Path,
    referenced: &std::collections::BTreeSet<String>,
) -> Option<std::collections::BTreeSet<String>> {
    if referenced.iter().any(|n| n.ends_with("__children")) {
        return None;
    }
    let mut bodies: std::collections::BTreeMap<String, String> = Default::default();
    for f in nest_view_files(dir) {
        for stmt in split_sql_statements(&f.sql) {
            if let (Some(name), Some(body)) = (view_name(&stmt), view_body(&stmt)) {
                bodies.insert(name, body.to_string());
            }
        }
    }

    let mut out = referenced.clone();
    let mut frontier: Vec<String> = referenced.iter().cloned().collect();
    // A view on a view on a view: follow the chain, bounded, exactly as `expand_through_views` is.
    for _ in 0..8 {
        let mut next = Vec::new();
        for name in frontier.drain(..) {
            let Some(body) = bodies.get(&name) else {
                continue;
            };
            for t in base_tables_in(conn, body)? {
                if out.insert(t.clone()) {
                    next.push(t);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Some(out)
}

/// The base tables a statement reads, lowercased. The security walk collects the same set for the
/// caller's own query; this is for SQL we hand ourselves, like a view's stored definition.
fn base_tables_in(conn: &Connection, sql: &str) -> Option<std::collections::BTreeSet<String>> {
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let ast = conn
        .query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get::<_, String>(0)
        })
        .ok()?;
    let v = serde_json::from_str::<Value>(&ast).ok()?;
    if v.get("error").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let mut out = std::collections::BTreeSet::new();
    walk_table_refs(&v, &mut |kind, name| {
        if kind == "BASE_TABLE" {
            out.insert(name.to_ascii_lowercase());
        }
    });
    Some(out)
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
                        // The schema qualifier, reported separately so the security walk keeps
                        // seeing bare names - it rejects anything that is not `[A-Za-z0-9_]`, which
                        // is what stops a quoted path in table position reading a file, and a name
                        // carrying a `.` must not slip through that. #896 needs the qualifier for a
                        // different reason: `information_schema.tables` cannot be told apart from a
                        // nest table called `tables` without it, and a catalogue query needs every
                        // view defined in order to list them.
                        if let Some(schema) = map.get("schema_name").and_then(Value::as_str) {
                            if !schema.is_empty() && schema != "main" {
                                f("QUALIFIED_SCHEMA", schema);
                            }
                        }
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

/// Refuse a query that *calls* any [`FORBIDDEN_FNS`] function. Comments are stripped first, then each
/// name is matched only when it's a real call: a word boundary before it and (after optional
/// whitespace) a `(` after it - so a table or column merely *named* like one (e.g. `pool__glob`) is
/// fine, while `read_text/**/('…')` and `READ_TEXT (…)` are both caught. (SEC-2, primary control.)
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
        "CREATE OR REPLACE VIEW labels AS SELECT lower(address) AS address, label \
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
            "CREATE OR REPLACE VIEW \"{template}__children\" AS \
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
///
/// Returns the tables whose cold data ended up **incomplete** - a manifest segment dropped from the
/// view, or a view that could not be defined at all. Every reduction below is already logged, but a
/// log is not reachable by the caller who is about to sum the reduced column, so the same decision is
/// handed back as data and rides out on [`QueryOutput::degraded_tables`] (#435).
fn define_views(
    conn: &Connection,
    dir: &Path,
    hot: &HotRows,
    sealed_through: u64,
    // Content addresses of segments to leave out of every view. Empty on the first attempt; on the
    // retry after an execution-phase failure it holds whatever `seal::segments_failing_verification`
    // found, so a page-corrupt segment *reduces* its table instead of failing the whole query (#433).
    excluded: &std::collections::BTreeSet<String>,
    // The live, registry-derived schema (`indexer::full_schema`/`served`) - every table the config
    // declares, independent of whether it has ever populated. #663: `schema_columns(dir)` alone reads
    // `schema.json` off disk, and that file is only as fresh as the last `init`/`add`/`schema`/`dev`
    // startup that wrote it - a hand-edited `nuthatch.toml`, an out-of-band checkout, or a schema.json
    // committed before the config gained an event can all leave it behind. A table missing from disk
    // but present here still gets its empty typed view, so a genuinely-declared-but-never-fired event
    // degrades to zero rows instead of the whole file failing to bind. Empty when the caller has no
    // live registry handy (most tests, and the handful of internal callers this fix deliberately
    // leaves on the disk-only path) - identical to today's behaviour in that case.
    declared: &[crate::registry::TableSchema],
    // Only define views for these tables, lowercased. `None` defines every table the nest has, which
    // is what every caller outside `run` wants and what `run` falls back to when it cannot work out
    // what a statement reaches. See `reachable_tables` for why this matters (#896).
    wanted: Option<&std::collections::BTreeSet<String>>,
) -> Result<std::collections::BTreeSet<String>> {
    let mut degraded: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let manifest = crate::seal::load_manifest(dir)?;
    let mut schema = schema_columns(dir);
    // #729: the table-name check above (#663) stops here at the table's *existence* - a table already
    // on disk kept exactly the columns `schema.json` had, even when the live registry (a re-fetched ABI,
    // same event, an added field) now declares more. Diff column *names* too, per table, and append what
    // the disk copy is missing; an on-disk column never loses its declared type, and this only ever adds.
    for t in declared {
        let declared_cols: Vec<(String, String)> = t
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.storage.clone()))
            .collect();
        match schema.iter_mut().find(|(name, _)| name == &t.table) {
            Some((_, cols)) => {
                let added: Vec<&str> = declared_cols
                    .iter()
                    .filter(|(name, _)| !cols.iter().any(|(n, _)| n == name))
                    .map(|(name, _)| name.as_str())
                    .collect();
                if !added.is_empty() {
                    // Loud on purpose (#729 acceptance bar): this used to be silent, and silent
                    // correctness in `define_views` is what made #663 and this issue both findable
                    // only by reading the view's output rather than the log.
                    tracing::warn!(
                        "table {t} in schema.json is missing column(s) the live registry declares: {} \
                         - merging them in; segments sealed before this column existed read back NULL \
                         for it, never an error. Re-run `nuthatch schema` (or restart `dev`) to refresh \
                         schema.json and stop seeing this.",
                        added.join(", "),
                        t = t.table,
                    );
                    for (name, storage) in &declared_cols {
                        if !cols.iter().any(|(n, _)| n == name) {
                            cols.push((name.clone(), storage.clone()));
                        }
                    }
                }
            }
            None => schema.push((t.table.clone(), declared_cols)),
        }
    }
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
    // #896: a view's DDL carries every one of that table's sealed segment paths, so defining the
    // ones a statement cannot reach is the dominant per-request cost on a mature nest.
    if let Some(wanted) = wanted {
        tables.retain(|t| wanted.contains(&t.to_ascii_lowercase()));
    }

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
                        // Verified-bad on a previous attempt: its bytes no longer match its content
                        // address, so it cannot contribute rows anyone should trust (#433). Dropped
                        // from the view rather than moved on disk - see
                        // `seal::segments_failing_verification` on why reduction, not quarantine.
                        if excluded.contains(&s.hash) {
                            // Present on disk and corrupt in its pages - never reported before this
                            // query, unlike the missing-file case below, so `error!` to match
                            // `verify_and_quarantine`'s level for the identical decision (#435).
                            tracing::error!(
                                "segment {} for {table} fails verification - skipping (cold data reduced)",
                                s.file
                            );
                            degraded.insert(table.clone());
                            return None;
                        }
                        let p = crate::seal::segment_path(dir, &s.file, &s.hash);
                        // Skip a manifest segment whose file is gone from disk (quarantined as corrupt
                        // by the startup integrity pass, or externally removed). Without this, one
                        // missing file makes `read_parquet` throw and the whole query fail; instead the
                        // table's cold data is reduced, loudly, and queries keep working.
                        if p.exists() {
                            Some(format!("'{}'", p.display()))
                        } else {
                            // Stays at `warn!`: the usual cause is `verify_and_quarantine` having
                            // already moved this file aside and logged it at `error!` at startup, and
                            // re-raising the consequence on every query would double-count it.
                            tracing::warn!(
                                "segment {} for {table} missing on disk - skipping (cold data reduced)",
                                s.file
                            );
                            degraded.insert(table.clone());
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
                    with_declared_base_cols(&format!("\"{hot_tbl}\""), cols)
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
                    with_declared_base_cols(
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
                    "CREATE OR REPLACE VIEW \"{table}\" AS {}",
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
                        // Present and unreadable, same class as the excluded case above: `error!`,
                        // and recorded so the caller learns the table came back short (#435).
                        tracing::error!(
                            "segment {f} for {table} will not bind - skipping (cold data reduced): {err}"
                        );
                        degraded.insert(table.clone());
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
            degraded.insert(table.clone());
            continue;
        }
        if let Some(Err(e)) = view_ddl(&readable).map(|retry| conn.execute_batch(&retry)) {
            tracing::warn!("view {table} skipped after dropping bad segments: {e}");
            degraded.insert(table.clone());
        }
    }
    Ok(degraded)
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
            if let Err(e) = conn.execute_batch(&with_or_replace_view(&stmt)) {
                tracing::debug!("nest view {} statement skipped: {e}", v.file);
            }
        }
    }
}

/// Make a nest-authored `CREATE VIEW` re-runnable on a cached connection (#295).
fn with_or_replace_view(stmt: &str) -> String {
    let s = stmt.trim_start();
    if s.len() < 11 || !s[..6].eq_ignore_ascii_case("create") {
        return stmt.to_string();
    }
    let rest = s[6..].trim_start();
    if rest.len() >= 4
        && rest[..4].eq_ignore_ascii_case("view")
        && rest
            .get(4..)
            .and_then(|r| r.chars().next())
            .is_some_and(|c| c.is_whitespace() || c == '"')
    {
        format!("CREATE OR REPLACE VIEW{}", &rest[4..])
    } else {
        stmt.to_string()
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

/// If a query fails against a name DuckDB says doesn't exist, and that name is a nest-authored view
/// that failed to build, replace the generic "does not exist" + fuzzy-match-on-an-unrelated-table
/// message with the view's real build error (#539). A view that fails to build is reported as though
/// it doesn't exist at all - `define_nest_views` loads views per-statement and swallows failures to
/// `tracing::debug!` for fault isolation, so by the time a query dies at `/sql` there is no record of
/// *why* the name is missing, and `sql_errors::enrich`'s fuzzy match then points at an unrelated real
/// table. This is the one place that record is reconstructed: on the query's error path only (never
/// on a successful query), rebuild the same base surface `validate_nest_views` uses and replay the
/// view files in order, and report whichever `CREATE VIEW` statement targets `missing`.
pub fn enrich_query_error(
    dir: &Path,
    raw: &str,
    query: &str,
    schema: &[crate::registry::TableSchema],
) -> Option<String> {
    if let Some(name) = missing_table_of(raw) {
        if let Some(issue) = view_build_failure(dir, schema, &name) {
            // The caller wraps whatever this returns as its own "hint: …" line, so this must read as
            // that line's content, not carry a second nested "hint:" of its own.
            let extra = issue.hint.map(|h| format!("\n{h}")).unwrap_or_default();
            return Some(format!(
                "view `{name}` failed to build (in `{}`): {}{extra}",
                issue.file, issue.error
            ));
        }
    }
    crate::sql_errors::enrich(raw, query, schema)
}

/// If `missing` is the name of a nest-authored view (`views/*.sql`) that failed to build, the error
/// from that specific `CREATE VIEW` statement - the real fault a query against it hit, rather than
/// the "does not exist" DuckDB reports for a name that was simply never created. `None` if `missing`
/// isn't an authored view name at all (an ordinary unknown-table typo), or names one that in fact
/// built fine (so whatever failed, it wasn't this).
fn view_build_failure(
    dir: &Path,
    schema: &[crate::registry::TableSchema],
    missing: &str,
) -> Option<ViewIssue> {
    // DuckDB validates `CREATE VIEW` eagerly (measured, not assumed - see the analytics.rs test
    // suite), so "two later views joined pool_effective_fee" (#539) means those two views' *own*
    // `CREATE VIEW` statements failed at load, each with the same "pool_effective_fee does not
    // exist". Chase that chain to the view whose failure is not itself just a missing upstream view -
    // the one line that actually explains anything - rather than reporting a hop that only repeats
    // the same "does not exist" one level removed. Bounded to 8 hops, matching `expand_through_views`'
    // cycle guard (DuckDB itself refuses a cyclic view, so this is a backstop, not the mechanism).
    view_build_failure_at(dir, schema, missing, 8)
}

fn view_build_failure_at(
    dir: &Path,
    schema: &[crate::registry::TableSchema],
    missing: &str,
    hops_left: u8,
) -> Option<ViewIssue> {
    let files = nest_view_files(dir);
    if files.is_empty() {
        return None;
    }
    let conn = Connection::open_in_memory().ok()?;
    let empty_hot = HotRows::new();
    let _ = define_views(
        &conn,
        dir,
        &empty_hot,
        u64::MAX,
        &Default::default(),
        schema,
        None,
    );
    define_labels_view(&conn, dir);
    define_children_views(&conn, dir);

    let target = missing.trim_matches('"').to_ascii_lowercase();
    for v in &files {
        for stmt in split_sql_statements(&v.sql) {
            let result = conn.execute_batch(&stmt);
            let Some(name) = view_target_name(&stmt) else {
                continue;
            };
            if name.to_ascii_lowercase() != target {
                continue;
            }
            return match result {
                Ok(()) => None,
                Err(e) => {
                    let error = e.to_string();
                    // If this statement's own failure is "some other name does not exist", and that
                    // name is itself an authored view that also failed, that view's failure is the
                    // actual cause - chase it rather than reporting a repeat of the same "does not
                    // exist" the caller already has.
                    if hops_left > 0 {
                        if let Some(dep) = missing_table_of(&error) {
                            if dep.to_ascii_lowercase() != target {
                                if let Some(root) =
                                    view_build_failure_at(dir, schema, &dep, hops_left - 1)
                                {
                                    return Some(ViewIssue {
                                        file: v.file.clone(),
                                        error: format!(
                                            "depends on view `{dep}` (in `{}`), which failed to \
                                             build: {}",
                                            root.file, root.error
                                        ),
                                        hint: root.hint,
                                    });
                                }
                            }
                        }
                    }
                    let hint = crate::sql_errors::enrich(&error, &stmt, schema);
                    Some(ViewIssue {
                        file: v.file.clone(),
                        error,
                        hint,
                    })
                }
            };
        }
    }
    None
}

/// The view name a `CREATE [OR REPLACE] VIEW <name> AS …` statement targets, unquoted. `None` for a
/// statement that isn't that shape. Same lowercase-scan-for-offsets trick as `view_body`.
fn view_target_name(stmt: &str) -> Option<String> {
    let lower = stmt.to_ascii_lowercase();
    let view_at = lower.find(" view ")?;
    let as_at = lower[view_at..].find(" as ")? + view_at;
    Some(
        stmt[view_at + 6..as_at]
            .trim()
            .trim_matches('"')
            .to_string(),
    )
}

/// Declared tables (`declared`, typically `indexer::full_schema` - live, not `schema.json`) that have
/// never sealed a single segment - the honest, on-disk-permanent signal for "this event's decoder
/// exists but the chain has never actually emitted it" (#663). A table that has fired but not yet
/// sealed (still hot-only, e.g. seconds after its first log on a nest that was just restarted) is
/// misclassified as empty here until its next seal; that window is narrow and self-corrects, and
/// erring toward "say something, occasionally early" beats the silence this issue is about.
///
/// This is what turns "the day that event first fires, the view starts working, and nothing in the
/// logs explains either state" into a startup line an operator can read once and stop wondering about.
pub fn declared_but_never_sealed(
    dir: &Path,
    declared: &[crate::registry::TableSchema],
) -> Vec<String> {
    let manifest = crate::seal::load_manifest(dir).unwrap_or_default();
    declared
        .iter()
        .map(|t| &t.table)
        .filter(|t| !manifest.tables.contains_key(*t))
        .cloned()
        .collect()
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
    let _ = define_views(
        &conn,
        dir,
        &empty_hot,
        u64::MAX,
        &Default::default(),
        schema,
        None,
    );
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

/// Bind one incremental-entity SELECT against the same empty typed fact surface used by view
/// validation. DuckDB's Rust binding only materialises result metadata after `query`, even for a
/// prepared statement. The entity has already passed the single-SELECT gate, and no rows are read.
pub fn entity_output_columns(
    dir: &Path,
    schema: &[crate::registry::TableSchema],
    sql: &str,
) -> Result<Vec<String>> {
    let conn = Connection::open_in_memory()?;
    let empty_hot = HotRows::new();
    let _ = define_views(
        &conn,
        dir,
        &empty_hot,
        u64::MAX,
        &Default::default(),
        schema,
        None,
    );
    define_labels_view(&conn, dir);
    define_children_views(&conn, dir);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query([])?;
    drop(rows);
    Ok(stmt.column_names().iter().map(|s| s.to_string()).collect())
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
fn with_declared_base_cols(from_item: &str, cols: &[(String, String)]) -> String {
    let stubs: Vec<String> = cols
        .iter()
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
            &[],
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
            &[],
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
            &[],
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
            0, // nothing sealed → all hot rows (blocks 100/101 > 0) count,
            &[],
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
            10, // sealed through block 10 → cold ≤ 10, hot > 10,
            &[],
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
            &[],
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
        let f = query(dir.path(), r#"SELECT "from" FROM "t__transfer" LIMIT 1"#).unwrap();
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
            &[],
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
    fn bigint_stub_types_columns_by_name_not_storage() {
        // COR-4, third site. The two tests above assert the stubbed column is NULL, and a wrongly-typed
        // stub is NULL too - so they pass with `hot_col_type` replaced by a constant. Type by column
        // NAME, matching `empty_view_ddl` and `seal::rows_to_batch`: a `word32` column with a
        // non-counter name stubs VARCHAR, so `WHERE value LIKE '7%'` does not become a binder error on
        // the 0-of-N state this stub exists to make survivable (#467).
        let s = with_declared_base_cols("src", &[("fee".to_string(), "word32".to_string())]);
        assert!(
            s.contains(r#"CAST(NULL AS VARCHAR) AS "fee""#),
            "word32-storage non-counter column must stub VARCHAR, got: {s}"
        );
        // The four counter columns stay UBIGINT (by name).
        let s2 =
            with_declared_base_cols("src", &[("block_number".to_string(), "word32".to_string())]);
        assert!(
            s2.contains(r#"CAST(NULL AS UBIGINT) AS "block_number""#),
            "got: {s2}"
        );
    }

    /// #729: `define_views` merged a declared table into `schema.json`'s copy only when no entry of
    /// that *name* existed yet (#663's fix) - a table already on disk kept its on-disk *columns*
    /// forever, even once the live registry (a re-fetched ABI, same event, one more field) knows more.
    /// The view built "successfully" and was silently missing the column: no error, no log, unlike
    /// #663's total-failure case. Confirmed directly against DuckDB (see `with_declared_base_cols`'s
    /// doc) that a column no listed segment carries is a binder error on explicit reference, not a NULL
    /// row - so the fix is a name-keyed column merge plus generalizing #434's null-stub from
    /// big-integer columns to every declared column, not a replacement of the disk column set. CLAUDE.md
    /// rules out ever re-decoding the sealed segment itself.
    #[test]
    fn define_views_merges_a_stale_schema_json_columns_by_name() {
        let dir = tempfile::tempdir().unwrap();
        // schema.json as it stood at the last `dev`/`schema` run: `t__transfer` known, but only `value`.
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"registry_hash":"0x0","tables":[{"table":"t__transfer","alias":"t","event":"Transfer","topic0":"0x","columns":[{"name":"value","sol_type":"uint256","storage":"varchar","indexed":false}]}]}"#,
        )
        .unwrap();
        // One sealed segment, written under the old ABI - it genuinely has no `memo` column.
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__transfer","from":"0xa","to":"0xb","value":"9","block_number":1,"tx_hash":"0xt","log_index":0}"#.to_string()],
            1,
            1,
        )
        .unwrap();

        // The registry has been re-fetched since: the ABI gained `memo` on the same `Transfer` event.
        // `schema.json` was never regenerated, so it still only knows `value` - a stale *column set* on
        // an already-declared table, not a missing table (#663's case). A degenerate fixture (A == A)
        // would pass with the merge never running, so the two sets deliberately differ.
        let declared = vec![crate::registry::TableSchema {
            table: "t__transfer".into(),
            alias: "t".into(),
            kind: crate::registry::TableKind::Event,
            function: String::new(),
            selector: String::new(),
            event: "Transfer".into(),
            topic0: "0x".into(),
            columns: vec![
                crate::registry::ColumnSchema {
                    name: "value".into(),
                    sol_type: "uint256".into(),
                    storage: "varchar".into(),
                    indexed: false,
                },
                crate::registry::ColumnSchema {
                    name: "memo".into(),
                    sol_type: "string".into(),
                    storage: "varchar".into(),
                    indexed: false,
                },
            ],
        }];

        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT value, memo FROM "t__transfer" ORDER BY block_number"#,
            guard,
            &HotRows::new(),
            u64::MAX,
            &declared,
        )
        .expect("the live registry's new column must resolve to NULL, not a binder error");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0]["value"], Value::from("9"));
        assert_eq!(
            out.rows[0]["memo"],
            Value::Null,
            "no sealed segment carries `memo` yet - it must read back NULL, matching #434's precedent \
             for a declared column no input carries, not disappear from the view or error"
        );
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

    /// #295: two queries on the same nest, same watermark, share one DuckDB. Deleting the cache
    /// (or opening every time) fails this. Other tests use other dirs and do not evict this slot.
    #[test]
    fn a_second_query_reuses_the_duckdb_connection() {
        let dir = tempfile::tempdir().unwrap();
        query(dir.path(), "SELECT 42 AS n").unwrap();
        assert_eq!(
            duck_opens_for(dir.path()),
            1,
            "the first query opens DuckDB"
        );
        query(dir.path(), "SELECT 42 AS n").unwrap();
        assert_eq!(
            duck_opens_for(dir.path()),
            1,
            "the second query must not rebuild the world"
        );
    }

    /// #295: new sealed segments change the watermark; reusing the old connection would serve
    /// a view that never saw them.
    #[test]
    fn a_new_watermark_opens_a_fresh_connection() {
        let dir = tempfile::tempdir().unwrap();
        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        let hot = HotRows::new();
        query_hot_cold(dir.path(), "SELECT 1 AS n", guard, &hot, 0, &[]).unwrap();
        assert_eq!(duck_opens_for(dir.path()), 1);
        query_hot_cold(dir.path(), "SELECT 1 AS n", guard, &hot, 10, &[]).unwrap();
        assert_eq!(
            duck_opens_for(dir.path()),
            2,
            "a new sealed_through must not reuse the stale connection"
        );
    }

    /// #825: a cached connection is valid only for the authored inputs it was built from. Both an
    /// added view and its deletion must force a fresh catalogue; otherwise the old view stays
    /// queryable until process restart.
    /// #840 - a view rewritten to the same length inside one mtime tick must still invalidate.
    ///
    /// The cache keyed on `(len, modified_ns)`. On the Linux dev box that stamp misses a same-length
    /// rewrite **497 times in 500** (btrfs) because the mtime clock resolves to ~3.3 ms - 2,000
    /// writes produced nine distinguishable timestamps.
    ///
    /// **What that does and does not cost, measured rather than assumed.** The answer stays correct:
    /// `attempt()` re-runs `define_views`, `define_nest_views`, `define_labels_view` and
    /// `define_children_views` on every query, cached connection or fresh, and all of them are
    /// `CREATE OR REPLACE` - so the catalogue is rebuilt from the current files each time and the
    /// rows are right whatever the stamp says. Run this test against the old `(len, modified_ns)`
    /// implementation and the value assertion below still passes; it is the *invalidation* assertion
    /// that goes red. So the defect is a cache that fails to notice it is stale, not a query that
    /// lies - and the reason to fix it is that a stamp which cannot see a change is wrong
    /// independently of which code path happens to compensate for it today.
    ///
    /// **The collision here is forced rather than raced.** A test that just wrote the file twice
    /// quickly would pass on this laptop whatever the implementation does - APFS resolves to ~37 us
    /// and gives 0/500 collisions - and would only ever be red on Linux. Restoring the mtime
    /// explicitly makes the two stamps provably identical on every platform, so the test is red
    /// against the old implementation everywhere, which is the only way it is worth having.
    #[test]
    fn a_same_length_view_rewrite_in_one_mtime_tick_still_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let views = dir.path().join("views");
        std::fs::create_dir_all(&views).unwrap();
        let view = views.join("one.sql");

        // Two definitions of identical length that disagree about every row they produce.
        let before = "CREATE VIEW one AS SELECT 1 AS n";
        let after = "CREATE VIEW one AS SELECT 2 AS n";
        assert_eq!(
            before.len(),
            after.len(),
            "the rewrite must not change length"
        );

        std::fs::write(&view, before).unwrap();
        let rows = query(dir.path(), "SELECT n FROM one").unwrap();
        assert_eq!(rows[0]["n"], serde_json::json!(1));
        let opens = duck_opens_for(dir.path());
        let stamped = std::fs::metadata(&view).unwrap();
        let (len, mtime) = (stamped.len(), stamped.modified().unwrap());

        std::fs::write(&view, after).unwrap();
        // Put the clock back, so the old `(len, modified_ns)` stamp is *provably* unchanged rather
        // than merely likely to be.
        std::fs::File::options()
            .write(true)
            .open(&view)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        let restamped = std::fs::metadata(&view).unwrap();
        assert_eq!(restamped.len(), len, "the rewrite changed length");
        assert_eq!(
            restamped.modified().unwrap(),
            mtime,
            "the mtime was not restored - this test would prove nothing"
        );

        let rows = query(dir.path(), "SELECT n FROM one").unwrap();
        // Correct either way, because the views are redefined per query - asserted so that a future
        // change to that arrangement is caught here rather than in production.
        assert_eq!(
            rows[0]["n"],
            serde_json::json!(2),
            "the rows must be current"
        );
        // This is the load-bearing one, and the one that is red against `(len, modified_ns)`.
        assert!(
            duck_opens_for(dir.path()) > opens,
            "the connection was reused across a changed view - the stamp did not see the rewrite"
        );
    }

    #[test]
    fn changing_or_removing_an_authored_view_invalidates_the_duckdb_cache() {
        let dir = tempfile::tempdir().unwrap();
        query(dir.path(), "SELECT 42 AS n").unwrap();
        assert_eq!(duck_opens_for(dir.path()), 1);

        let views = dir.path().join("views");
        std::fs::create_dir_all(&views).unwrap();
        let view = views.join("one.sql");
        std::fs::write(&view, "CREATE VIEW one AS SELECT 1 AS n").unwrap();
        query(dir.path(), "SELECT 42 AS n").unwrap();
        assert_eq!(
            duck_opens_for(dir.path()),
            2,
            "a new view changes the inputs"
        );

        std::fs::remove_file(view).unwrap();
        query(dir.path(), "SELECT 42 AS n").unwrap();
        assert_eq!(
            duck_opens_for(dir.path()),
            3,
            "removing a view must not leave the old catalogue cached"
        );
    }

    #[test]
    fn removing_label_snapshots_drops_the_cached_labels_view() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("labels.csv");
        std::fs::write(&input, "0x1111111111111111111111111111111111111111,mixer\n").unwrap();
        crate::labels::import(dir.path(), &input).unwrap();
        query(dir.path(), "SELECT count(*) AS n FROM labels").unwrap();

        std::fs::remove_dir_all(dir.path().join(crate::labels::LABELS_DIR)).unwrap();
        assert!(
            query(dir.path(), "SELECT count(*) AS n FROM labels").is_err(),
            "a removed label snapshot must not remain readable through the cached connection"
        );
    }

    #[test]
    fn explicit_invalidation_releases_a_mounted_nests_connection() {
        let dir = tempfile::tempdir().unwrap();
        query(dir.path(), "SELECT 42 AS n").unwrap();
        invalidate_duck_cache(dir.path());
        query(dir.path(), "SELECT 42 AS n").unwrap();
        assert_eq!(duck_opens_for(dir.path()), 2);
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

    /// #539's own repro: a Solidity `bool` column forces a `COALESCE` type mismatch, which fails the
    /// view's `CREATE VIEW`. Querying it must name the build failure and the real DuckDB error, not
    /// report it as though the view were never defined - and the old fuzzy match onto an unrelated
    /// real table must be gone.
    #[test]
    fn a_view_broken_by_the_bool_footgun_is_named_as_a_build_failure() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"pool_manager__toggle_custom_fee","pool":"0xp","enabled":true,"block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 10).unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/20-custom-fees.sql"),
            "CREATE VIEW pool_effective_fee AS \
             SELECT pool, COALESCE(enabled, false) AS override_enabled \
             FROM pool_manager__toggle_custom_fee;",
        )
        .unwrap();
        // A real, unrelated table - present so the *old* fuzzy match had something to (wrongly) find.
        std::fs::write(
            dir.path().join("views/05-unrelated.sql"),
            "CREATE VIEW pool_manager__set_default_fee_alias AS \
             SELECT pool FROM pool_manager__toggle_custom_fee;",
        )
        .unwrap();

        let schema = vec![crate::registry::TableSchema {
            table: "pool_manager__toggle_custom_fee".into(),
            alias: "pool_manager".into(),
            kind: crate::registry::TableKind::Event,
            function: String::new(),
            selector: String::new(),
            event: "ToggleCustomFee".into(),
            topic0: "0x".into(),
            columns: vec![
                crate::registry::ColumnSchema {
                    name: "pool".into(),
                    sol_type: "address".into(),
                    storage: "address".into(),
                    indexed: false,
                },
                crate::registry::ColumnSchema {
                    name: "enabled".into(),
                    sol_type: "bool".into(),
                    storage: "bool".into(),
                    indexed: false,
                },
            ],
        }];

        // The exact DuckDB message a query against the broken view produces.
        let raw = "Catalog Error: Table with name pool_effective_fee does not exist!\nDid you \
                    mean \"pool_manager__set_default_fee_alias\"?";
        let msg = enrich_query_error(dir.path(), raw, "SELECT * FROM pool_effective_fee", &schema)
            .unwrap();
        assert!(
            msg.contains("pool_effective_fee") && msg.contains("failed to build"),
            "names the view and says it failed to build: {msg}"
        );
        assert!(
            msg.contains("20-custom-fees.sql"),
            "names the file the broken view lives in: {msg}"
        );
        assert!(
            msg.contains("COALESCE") && msg.contains("explicit cast"),
            "carries DuckDB's real error: {msg}"
        );
        assert!(
            !msg.to_ascii_lowercase()
                .contains("pool_manager__set_default_fee_alias"),
            "must not still suggest the unrelated real table now the real cause is known: {msg}"
        );
    }

    /// The chained case: a view built *on top of* the broken one also fails to build (its own
    /// `CREATE VIEW` cannot resolve `pool_effective_fee` either), and DuckDB's error for it names
    /// `pool_effective_fee`, not the queried view. The message must still land on the root cause
    /// rather than repeating "pool_effective_fee does not exist" one hop removed.
    #[test]
    fn a_view_built_on_a_broken_view_reports_the_root_cause() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"pool_manager__toggle_custom_fee","pool":"0xp","enabled":true,"block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 10).unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/20-custom-fees.sql"),
            "CREATE VIEW pool_effective_fee AS \
             SELECT pool, COALESCE(enabled, false) AS override_enabled \
             FROM pool_manager__toggle_custom_fee;",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("views/30-summary.sql"),
            "CREATE VIEW pool_effective_fee_summary AS SELECT pool FROM pool_effective_fee;",
        )
        .unwrap();

        let schema = vec![crate::registry::TableSchema {
            table: "pool_manager__toggle_custom_fee".into(),
            alias: "pool_manager".into(),
            kind: crate::registry::TableKind::Event,
            function: String::new(),
            selector: String::new(),
            event: "ToggleCustomFee".into(),
            topic0: "0x".into(),
            columns: vec![
                crate::registry::ColumnSchema {
                    name: "pool".into(),
                    sol_type: "address".into(),
                    storage: "address".into(),
                    indexed: false,
                },
                crate::registry::ColumnSchema {
                    name: "enabled".into(),
                    sol_type: "bool".into(),
                    storage: "bool".into(),
                    indexed: false,
                },
            ],
        }];

        // DuckDB validates `CREATE VIEW` eagerly, so `pool_effective_fee_summary` was itself never
        // created - a query against it names *itself* as missing, not `pool_effective_fee`.
        let raw = "Catalog Error: Table with name pool_effective_fee_summary does not exist!";
        let msg = enrich_query_error(
            dir.path(),
            raw,
            "SELECT * FROM pool_effective_fee_summary",
            &schema,
        )
        .unwrap();
        assert!(
            msg.contains("pool_effective_fee_summary") && msg.contains("failed to build"),
            "names the queried view: {msg}"
        );
        assert!(
            msg.contains("pool_effective_fee") && msg.contains("30-summary.sql"),
            "names the dependency and where the dependent view lives: {msg}"
        );
        assert!(
            msg.contains("COALESCE") && msg.contains("explicit cast"),
            "surfaces the *root* cause, not a repeat of \"does not exist\": {msg}"
        );
    }

    /// An ordinary unknown-table typo - no authored view anywhere named after it - must fall through
    /// to the normal fuzzy-match hint unchanged.
    #[test]
    fn an_unrelated_missing_table_is_unaffected_by_view_build_failure_lookup() {
        let dir = tempfile::tempdir().unwrap();
        let entities = vec![
            r#"{"table":"usdc__transfer","from":"0xa","to":"0xb","value":"5","block_number":10,"tx_hash":"0xt","log_index":0}"#.to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 10, 10).unwrap();

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
        let raw = "Catalog Error: Table with name transfers does not exist!";
        let msg = enrich_query_error(dir.path(), raw, "SELECT * FROM transfers", &schema).unwrap();
        assert!(msg.contains("no table `transfers`"), "{msg}");
        assert!(
            msg.contains("usdc__transfer"),
            "still suggests the real table: {msg}"
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

    /// SEC-7: a CTE list is only a prefix. The statement after it must still be a query.
    ///
    /// The leading-keyword gate accepts `WITH`, and the previous comment claimed DuckDB would not
    /// parse INSERT after a CTE. That is the same class of claim as "`conn.prepare` is
    /// single-statement". This guard is ours, on the public `query` path, so deleting the call
    /// from `attempt` fails the last assertion rather than leaving a unit-tested function with
    /// no caller.
    #[test]
    fn with_prefixed_dml_is_refused_on_the_public_query_path() {
        for ok in [
            "WITH t AS (SELECT 1 AS x) SELECT x FROM t",
            "with t as (select 1 as x) select x from t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
            "WITH a AS (SELECT 1 AS x), b AS (SELECT 2 AS x) SELECT * FROM a UNION ALL SELECT * FROM b",
            r#"WITH "t" AS (SELECT 1 AS x) SELECT x FROM "t""#,
            "WITH t AS MATERIALIZED (SELECT 1 AS x) SELECT x FROM t",
            "WITH t AS NOT MATERIALIZED (SELECT 1 AS x) SELECT x FROM t",
            // INSERT is data, not a statement, when it lives in a string inside the CTE.
            "WITH t AS (SELECT 'INSERT' AS s) SELECT s FROM t",
            "SELECT 1",
        ] {
            assert!(
                reject_with_prefixed_dml(ok).is_ok(),
                "legitimate query refused: {ok}"
            );
        }
        for bad in [
            "WITH t AS (SELECT 1 AS x) INSERT INTO t SELECT 1",
            "with t as (select 1 as x) insert into t select 1",
            "WITH t AS (SELECT 1 AS x) UPDATE t SET x = 2",
            "WITH t AS (SELECT 1 AS x) DELETE FROM t",
            "WITH t AS (SELECT 1 AS x) COPY t TO '/tmp/x.csv'",
            "WITH t AS (SELECT 1 AS x) MERGE INTO t USING t ON true",
            "WITH t AS (SELECT 1 AS x) CREATE TABLE x AS SELECT 1",
            // Comments must not smuggle DML past the CTE list.
            "WITH t AS (SELECT 1 AS x) /* hi */ INSERT INTO t SELECT 1",
        ] {
            let err = reject_with_prefixed_dml(bad)
                .expect_err(&format!("must be refused: {bad}"))
                .to_string();
            assert!(err.contains("WITH-prefixed DML"), "{bad} -> {err}");
        }

        let dir = tempfile::tempdir().unwrap();
        let err = query(
            dir.path(),
            "WITH t AS (SELECT 1 AS x) INSERT INTO t SELECT 1",
        )
        .expect_err("WITH-prefixed INSERT must be refused on the public path")
        .to_string();
        assert!(
            err.contains("WITH-prefixed DML"),
            "the refusal must come from our gate, not DuckDB later: {err}"
        );

        let exfil = dir.path().join("exfil.csv");
        let err = query(
            dir.path(),
            &format!("WITH t AS (SELECT 42 AS x) COPY t TO '{}'", exfil.display()),
        )
        .expect_err("WITH-prefixed COPY must be refused")
        .to_string();
        assert!(err.contains("WITH-prefixed DML"), "{err}");
        assert!(!exfil.exists(), "a WITH-prefixed COPY wrote a file");
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
            reject_file_access(&sql).is_err(),
            "the denylist must still refuse read_text - it is the control in front"
        );

        let conn = open_locked_duckdb(nest.path()).unwrap();
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
        let conn = open_locked_duckdb(&nid_dir).unwrap();
        let sql = format!("SELECT content FROM read_text('{}')", file.display());
        let mut stmt = conn.prepare(&sql).expect("shared-store read_text prepares");
        let got: String = stmt
            .query_row([], |r| r.get(0))
            .expect("shared-store read_text runs");
        assert_eq!(got.trim(), "shared");
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

    /// Overwrite a Parquet file's **data region** and leave its footer and magic bytes intact.
    ///
    /// The layout is `PAR1 | data | thrift footer | u32 footer_len | PAR1`, so the last eight bytes
    /// give the footer length and everything from byte 4 up to it is pages. Corrupting exactly that
    /// range is the condition #433 names: `read_parquet` binds the file happily and dies reading it.
    ///
    /// Returns the number of bytes it destroyed, so a caller can assert it actually did something -
    /// a helper that silently corrupted nothing would make every test built on it vacuous.
    fn corrupt_pages_leaving_the_footer_intact(path: &std::path::Path) -> usize {
        let mut bytes = std::fs::read(path).unwrap();
        let len = bytes.len();
        assert!(len > 12 && &bytes[..4] == b"PAR1" && &bytes[len - 4..] == b"PAR1");
        let footer_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
        let end = len - 8 - footer_len;
        assert!(end > 4, "the fixture must have a data region to corrupt");
        bytes[4..end].fill(0xFF);
        std::fs::write(path, &bytes).unwrap();
        end - 4
    }

    /// **Issue #433.** A sealed segment whose **pages** are corrupt but whose **footer reads fine**
    /// must reduce its table, exactly as a footer-corrupt one does since #430 - not fail the whole
    /// query with `Invalid Error: don't know what type: `.
    ///
    /// This is the other half of the class #430 opened, and it does not go through #430's machinery at
    /// all. #430 discriminates with `conn.prepare` over `read_parquet`, which validates the **footer**
    /// at DDL time. Corruption that leaves the footer intact passes that probe untouched, `CREATE
    /// VIEW` succeeds, and the failure lands at execution - taking every table in the query down, not
    /// just this one, with a message that names nothing.
    ///
    /// The fixture's own claim is asserted rather than assumed: the corrupt file must still **bind**.
    /// If it did not, this test would be a second copy of the #430 test wearing a different name, and
    /// would pass with the #433 mechanism deleted.
    #[test]
    fn a_page_corrupt_segment_with_an_intact_footer_reduces_the_table_rather_than_failing_the_query(
    ) {
        let dir = tempfile::tempdir().unwrap();
        // A schema, for the reason the #419 test above gives: it makes "rebuilt from the good segment"
        // distinguishable from "rebuilt from nothing".
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true},
                {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        for (block, from) in [(1u64, "0xa"), (2u64, "0xb")] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"{from}","value":"{block}","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }

        // Whole before anything is touched, or the reduction assertions below prove nothing.
        let rows = query(dir.path(), r#"SELECT "from" FROM "t__transfer""#)
            .expect("both segments readable");
        assert_eq!(rows.len(), 2, "two segments, two rows");

        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let victim = manifest.tables["t__transfer"]
            .iter()
            .find(|s| s.from_block == 2)
            .expect("the block-2 segment");
        let path = crate::seal::segment_path(dir.path(), &victim.file, &victim.hash);
        let destroyed = corrupt_pages_leaving_the_footer_intact(&path);
        assert!(destroyed > 0, "the fixture must have corrupted something");

        // **The fixture is the condition it claims to be.** #430's probe still says this file is fine,
        // so nothing in the footer-corrupt path can be what makes the assertions below pass.
        let conn = Connection::open_in_memory().unwrap();
        let probe = format!(
            "SELECT 1 FROM read_parquet(['{}'], union_by_name=true) LIMIT 0",
            path.display()
        );
        assert!(
            conn.prepare(&probe).is_ok(),
            "this test is about a segment that BINDS and then will not read - if it no longer binds \
             it is #430's case and this test has stopped testing #433"
        );

        // The table still answers, from the segment that is still good.
        let rows = query(dir.path(), r#"SELECT "from" FROM "t__transfer""#).expect(
            "a page-corrupt segment must reduce the table, not fail the query with `don't know what \
             type:`",
        );
        assert_eq!(rows.len(), 1, "the readable segment's row survives");
        assert_eq!(
            rows[0]["from"],
            Value::from("0xa"),
            "and it is the block-1 row, not the corrupt one"
        );
    }

    /// A nest with a schema and two one-block segments for `t__transfer`, blocks 1 and 2, rows `0xa`
    /// and `0xb`. The shape both reduction tests above build by hand; shared by the #435 tests below
    /// so the healthy control and the degraded cases are provably the *same* fixture apart from the
    /// corruption - a control built separately could differ in some other way and stop controlling.
    fn two_segment_nest() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true},
                {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]}]}"#,
        )
        .unwrap();
        for (block, from) in [(1u64, "0xa"), (2u64, "0xb")] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"{from}","value":"{block}","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }
        dir
    }

    /// The block-2 segment's file, for a test that wants to damage it.
    fn block_two_segment(dir: &std::path::Path) -> std::path::PathBuf {
        let manifest = crate::seal::load_manifest(dir).unwrap();
        let victim = manifest.tables["t__transfer"]
            .iter()
            .find(|s| s.from_block == 2)
            .expect("the block-2 segment");
        crate::seal::segment_path(dir, &victim.file, &victim.hash)
    }

    fn cold(dir: &std::path::Path, sql: &str) -> QueryOutput {
        query_guarded(
            dir,
            sql,
            QueryGuard {
                timeout: Duration::from_secs(30),
                max_rows: 10_000,
            },
        )
        .expect("a reduced table must still answer")
    }

    /// **Issue #435, the control.** A nest whose segments are all intact must report **no**
    /// degradation.
    ///
    /// This is the load-bearing half of the pair. Every other #435 test asserts that a flag is *set*,
    /// and all of them would pass just as well if the flag were hard-wired to "always degraded" -
    /// which would be worse than no flag at all, because a warning that fires on every healthy query
    /// is a warning an operator learns to scroll past. Nothing downstream (the `/sql` field, the CLI
    /// line, the MCP notice) is worth anything unless silence on the healthy path is pinned here.
    #[test]
    fn an_intact_nest_reports_no_degradation() {
        let dir = two_segment_nest();
        let out = cold(dir.path(), r#"SELECT "from" FROM "t__transfer""#);
        assert_eq!(out.rows.len(), 2, "two intact segments, two rows");
        assert!(
            out.degraded_tables.is_empty(),
            "an intact nest must report nothing degraded, got {:?}",
            out.degraded_tables
        );
        assert!(!out.degraded(), "and the one-bit form must agree");
    }

    /// **Issue #435, the footer-corrupt half (#419/#430's path).** A segment dropped at DDL time
    /// reduces the table *and says so in the result*.
    ///
    /// #430 chose reduction over deletion and was right to, but it left the decision visible only in
    /// a `warn!` the caller cannot read: the query returns `200` with fewer rows and nothing to
    /// distinguish that from a table which genuinely holds one row. `SELECT SUM(value)` over this
    /// nest answers `1` where the truth is `3`, and both a human and an agent will take it.
    #[test]
    fn a_footer_corrupt_segment_names_its_table_in_the_result() {
        let dir = two_segment_nest();
        // Present, listed in the manifest, no longer a Parquet file - and not quarantined, because
        // nothing has restarted.
        std::fs::write(
            block_two_segment(dir.path()),
            b"not parquet, not even close",
        )
        .unwrap();

        let out = cold(dir.path(), r#"SELECT "from" FROM "t__transfer""#);
        assert_eq!(out.rows.len(), 1, "reduced to the readable segment (#430)");
        assert!(
            out.degraded(),
            "and the caller is told the answer is short, not left to infer it from one row"
        );
        assert_eq!(
            out.degraded_tables,
            ["t__transfer".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the reduced table is named, so a caller can tell which of its numbers to distrust"
        );
    }

    /// **Issue #435, the page-corrupt half (#433's path).** The reduction that happens on the *retry*
    /// must be reported too.
    ///
    /// Worth its own test rather than folding into the one above, because it enters `define_views`
    /// through an entirely different door: the footer-corrupt segment is dropped by the `conn.prepare`
    /// probe on the first attempt, whereas this one binds cleanly, kills the query at execution, and
    /// is only excluded on the second attempt via `segments_failing_verification`. A `degraded_tables`
    /// wired into the first path alone would pass the test above and leave the #433 case - the one
    /// that motivated #435 in the first place - silent.
    #[test]
    fn a_page_corrupt_segment_names_its_table_in_the_result() {
        let dir = two_segment_nest();
        let path = block_two_segment(dir.path());
        assert!(
            corrupt_pages_leaving_the_footer_intact(&path) > 0,
            "the fixture must have corrupted something"
        );
        // The fixture is the condition it claims to be: #430's probe still passes this file, so the
        // footer-corrupt path cannot be what sets the flag below.
        let conn = Connection::open_in_memory().unwrap();
        assert!(
            conn.prepare(&format!(
                "SELECT 1 FROM read_parquet(['{}'], union_by_name=true) LIMIT 0",
                path.display()
            ))
            .is_ok(),
            "if it no longer binds this is #430's case and the test has stopped testing #433's"
        );

        let out = cold(dir.path(), r#"SELECT "from" FROM "t__transfer""#);
        assert_eq!(out.rows.len(), 1, "reduced on the retry (#433)");
        assert_eq!(
            out.degraded_tables,
            ["t__transfer".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the retry's exclusions are degradation too - the query succeeded with less data"
        );
    }

    /// A nest with **two** independently-populated tables, `t__transfer` (blocks 1-2) and
    /// `t__approval` (blocks 1-2), both intact. Every fixture up to #477 - `two_segment_nest` above
    /// included - has exactly one table, so "the nest is degraded" and "this query's table is
    /// degraded" were the same set in every assertion in the tree: a `define_views` bug that flagged
    /// the wrong table, or every table, would have passed all of them, because there was never a
    /// second, untouched table to catch it naming the wrong one. Corruption is the caller's job, on
    /// whichever table it wants degraded - this fixture ships both intact.
    fn two_table_nest() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[
                {"table":"t__transfer","columns":[
                    {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                    {"name":"from","sol_type":"address","storage":"address","indexed":true},
                    {"name":"value","sol_type":"uint256","storage":"word32","indexed":false}]},
                {"table":"t__approval","columns":[
                    {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                    {"name":"owner","sol_type":"address","storage":"address","indexed":true}]}]}"#,
        )
        .unwrap();
        for (block, from) in [(1u64, "0xa"), (2u64, "0xb")] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"{from}","value":"{block}","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }
        for (block, owner) in [(1u64, "0xc"), (2u64, "0xd")] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__approval","owner":"{owner}","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }
        dir
    }

    /// `table`'s block-2 segment file, in a [`two_table_nest`] - for a test that wants to damage one
    /// table's data without touching the other's. Mirrors `block_two_segment` above, scoped to a
    /// named table since this fixture has two.
    fn block_two_segment_of(dir: &std::path::Path, table: &str) -> std::path::PathBuf {
        let manifest = crate::seal::load_manifest(dir).unwrap();
        let victim = manifest.tables[table]
            .iter()
            .find(|s| s.from_block == 2)
            .expect("the block-2 segment");
        crate::seal::segment_path(dir, &victim.file, &victim.hash)
    }

    /// **Issue #477, case 1.** A two-table nest with one table degraded: a query against the
    /// *healthy* one must come back complete and correct, and the flag it carries must name the
    /// *other* table - never itself. `degraded_tables` comes from `define_views`'s
    /// schema ∪ manifest ∪ hot walk, never from the query's own `FROM` clause, so main.rs's and
    /// mcp.rs's caveat renderers say nothing about *this result* - only about whichever tables the
    /// set names. Excluding the healthy table here is what stops that caveat becoming a false
    /// statement about a complete answer.
    #[test]
    fn a_healthy_table_names_the_other_ones_degradation() {
        let dir = two_table_nest();
        std::fs::write(
            block_two_segment_of(dir.path(), "t__transfer"),
            b"not parquet, not even close",
        )
        .unwrap();

        let out = cold(
            dir.path(),
            r#"SELECT "owner" FROM "t__approval" ORDER BY "owner""#,
        );
        assert_eq!(
            out.rows,
            vec![
                serde_json::json!({"owner": "0xc"}),
                serde_json::json!({"owner": "0xd"}),
            ],
            "the healthy table's own segments are both intact - nothing about its answer is short"
        );
        // **The contract changed with #896, deliberately.** A query used to survey the whole nest,
        // because `define_views` bound every table in the manifest on every request - which is where
        // 2.5 seconds of a 38,428-segment nest's request time went. A query now reports what *it*
        // reached, and this one reached nothing damaged.
        assert!(
            out.degraded_tables.is_empty(),
            "a query that read only healthy segments has nothing to caveat: {:?}",
            out.degraded_tables
        );

        // The nest-wide fact has not been lost, only moved somewhere it can be reported without a
        // caller stumbling into it. `/ready` surfaces this; the sweep is what finds it.
        let swept = degraded_tables(dir.path(), &[]).unwrap();
        assert_eq!(
            swept,
            ["t__transfer".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the sweep must name the table that is actually short"
        );
        assert!(!swept.contains("t__approval"), "and never the healthy one");
    }

    /// **Issue #477, case 2.** `SELECT 1` and `.tables` (`information_schema.tables`, the query the
    /// REPL's `.tables` dot-command runs) draw no row from any table at all, healthy or degraded - no
    /// total to understate, nothing to call short. `define_views` runs before the caller's SQL and
    /// without looking at it, so the flag comes back identical to a query that actually reads the
    /// degraded table. That is the property that lets a caller trust the flag even on a query it
    /// cannot line up against any particular row.
    #[test]
    fn select_one_and_dot_tables_carry_the_flag_with_no_rows_to_understate() {
        let dir = two_table_nest();
        std::fs::write(
            block_two_segment_of(dir.path(), "t__transfer"),
            b"not parquet, not even close",
        )
        .unwrap();
        let degraded: std::collections::BTreeSet<String> =
            ["t__transfer".to_string()].into_iter().collect();

        // **`SELECT 1` used to survey the nest, and that was the cost** - it bound every table in
        // the manifest to find out, which is 2.5 seconds on a real one (#896). It draws from no
        // table, so it now reports on no table.
        let one = cold(dir.path(), "SELECT 1 AS one");
        assert_eq!(one.rows, vec![serde_json::json!({"one": 1})]);
        assert!(
            one.degraded_tables.is_empty(),
            "a query that reads nothing caveats nothing: {:?}",
            one.degraded_tables
        );

        // `.tables` is the one shape that still surveys, and has to: listing the catalogue means
        // every view must exist to be listed. `reachable_tables` refuses to narrow a statement it
        // cannot vouch for, and this is one - so the flag it carries is the old one.
        let tables = cold(
            dir.path(),
            "SELECT table_name FROM information_schema.tables \
             WHERE NOT starts_with(table_name, '__hot_') ORDER BY table_name",
        );
        assert_eq!(
            tables.degraded_tables, degraded,
            ".tables lists the catalogue, so it defines the catalogue, so it still learns"
        );

        // And the sweep knows regardless of what anybody queried.
        assert_eq!(degraded_tables(dir.path(), &[]).unwrap(), degraded);
    }

    /// **Issue #477, case 3 (#434's shape).** A view that fails for a reason no segment probe would
    /// ever catch still lands in `degraded_tables` - here, the view name is already taken in the
    /// catalogue, which `define_views` cannot tell apart from any other whole-DDL failure once every
    /// individual segment has already bound (`readable.len() == sealed_files.len()`, the branch #434
    /// occupied before its fix). No file on either table is touched, so this proves the flag does not
    /// depend on - and the caveat therefore must not name - a segment-level cause.
    #[test]
    fn an_undefinable_view_degrades_with_every_segment_intact() {
        let dir = two_table_nest();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(r#"CREATE TABLE "t__transfer" (x INTEGER)"#)
            .unwrap();

        let degraded = define_views(
            &conn,
            dir.path(),
            &HotRows::new(),
            u64::MAX,
            &std::collections::BTreeSet::new(),
            &[],
            None,
        )
        .unwrap();
        assert_eq!(
            degraded,
            ["t__transfer".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "the pre-existing catalogue name, not any segment, is why the view failed"
        );
        assert!(
            !degraded.contains("t__approval"),
            "the other table's view was never touched by the collision"
        );

        // Prove the premise: every segment behind the table still binds on its own, so nothing here
        // went through the corrupt/missing-file paths above - only the undefinable-view arm could
        // have set the flag.
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        for seg in &manifest.tables["t__transfer"] {
            let path = crate::seal::segment_path(dir.path(), &seg.file, &seg.hash);
            let probe = Connection::open_in_memory().unwrap();
            assert!(
                probe
                    .prepare(&format!(
                        "SELECT 1 FROM read_parquet(['{}'], union_by_name=true) LIMIT 0",
                        path.display()
                    ))
                    .is_ok(),
                "every segment must still bind on its own for this to be the undefinable-view arm"
            );
        }
    }

    /// **Issue #433, the half that bounds the cost.** `collect` must report *which phase* a query
    /// died in, because that is what decides whether the integrity sweep runs at all.
    ///
    /// `/sql` is untrusted and the caller writes the query, so "any failure hashes every segment in
    /// the nest" would be a denial-of-service amplifier built out of an integrity check - and a bind
    /// failure cannot have been caused by a corrupt page, so it must never provoke one.
    ///
    /// This asserts the discriminator **directly** rather than through a query's error message. I
    /// wrote the message version first and then killed it: with the phase split deleted, a binder
    /// error still comes back with the same text (the sweep finds nothing to change and the error is
    /// returned either way), so that test passed with the mechanism gone. Which is the exact failure
    /// this sprint is about, in my own new fixture.
    #[test]
    fn collect_separates_a_bind_failure_from_a_read_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true}]}]}"#,
        )
        .unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__transfer","from":"0xa","block_number":1,"tx_hash":"0xt","log_index":0}"#.to_string()],
            1,
            1,
        )
        .unwrap();
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let seg = &manifest.tables["t__transfer"][0];
        let path = crate::seal::segment_path(dir.path(), &seg.file, &seg.hash);
        corrupt_pages_leaving_the_footer_intact(&path);

        let conn = Connection::open_in_memory().unwrap();
        let empty = HotRows::new();
        define_views(
            &conn,
            dir.path(),
            &empty,
            u64::MAX,
            &Default::default(),
            &[],
            None,
        )
        .unwrap();

        // A name the catalogue does not have: refused by the binder, before a page is touched.
        assert!(
            matches!(
                collect(&conn, r#"SELECT no_such_column FROM "t__transfer""#, None),
                Err(Died::Binding(_))
            ),
            "a missing column is a bind failure - it cannot have been caused by a corrupt page"
        );

        // The same connection, a query that binds and then reads the corrupt pages.
        assert!(
            matches!(
                collect(&conn, r#"SELECT * FROM "t__transfer""#, None),
                Err(Died::Executing(_))
            ),
            "reading a page-corrupt segment is an execution failure - the only shape worth sweeping for"
        );
    }

    /// Collect every event message a closure logs. Used below to observe whether the integrity sweep
    /// touched a segment at all - the sweep has no return value a caller can see and no semantic
    /// effect when it looks at a table the query never named, so cost is the only thing that changes
    /// and the log line is the only place it surfaces.
    ///
    /// `with_default` being thread-local does **not** make this safe under parallel tests: `tracing`
    /// caches each callsite's `Interest` globally and process-wide the first time it is evaluated, so
    /// a callsite reached by another test on another thread with no subscriber installed can get
    /// cached `Interest::never()` for the rest of the process - and a `with_default` scope here would
    /// then never see that event regardless of what this layer wants. Confirmed on `segments_failing_
    /// verification`'s `tracing::error!` call site, see #482. Prefer a return-value assertion over
    /// `CapturedLogs` wherever the code under test exposes one; use this only as the last resort, and
    /// only for a negative assertion (an event that fails to arrive because it was never worth logging
    /// looks identical to one dropped by this race, so a positive assertion built on `CapturedLogs`
    /// cannot tell those apart and is unreliable by construction).
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedLogs {
        fn mentioning(&self, needle: &str) -> usize {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter(|l| l.contains(needle))
                .count()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedLogs {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Msg<'a>(&'a mut String);
            impl tracing::field::Visit for Msg<'_> {
                fn record_debug(
                    &mut self,
                    _f: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "{value:?}");
                }
            }
            let mut line = String::new();
            event.record(&mut Msg(&mut line));
            self.0.lock().unwrap().push(line);
        }
    }

    /// **Issue #433, the cost bound as reviewed.** The phase split was claimed to make "the cheap way
    /// to provoke a sweep" nonexistent. It did not: `SELECT CAST('x' AS INTEGER)` is 27 bytes, names
    /// no table, passes every gate, binds, and dies executing - and it hashed every segment in a
    /// healthy nest, once per request, on a surface with no auth and two concurrency permits.
    ///
    /// So the sweep is bounded by **reachability**: only segments backing tables the failed query
    /// named. A query that names nothing sweeps nothing.
    ///
    /// The measurement is the log line `segments_failing_verification` emits when it rejects a
    /// segment, because the sweep has no other observable: looking at a table the query never
    /// referenced changes no answer, only cost. **The positive control is in this test on purpose** -
    /// an absence assertion whose mechanism is missing passes for the wrong reason, which is the
    /// failure this whole sprint is about.
    #[test]
    fn a_query_that_names_no_table_reaches_no_segment() {
        use tracing_subscriber::layer::SubscriberExt as _;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true}]}]}"#,
        )
        .unwrap();
        for (block, from) in [(1u64, "0xa"), (2u64, "0xb")] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"{from}","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let victim = manifest.tables["t__transfer"]
            .iter()
            .find(|s| s.from_block == 2)
            .expect("the block-2 segment");
        let path = crate::seal::segment_path(dir.path(), &victim.file, &victim.hash);
        assert!(
            corrupt_pages_leaving_the_footer_intact(&path) > 0,
            "the fixture must have corrupted something"
        );

        const REJECTED: &str = "does not match its content address";

        // The attack: no table named, so nothing is reachable, so nothing may be hashed - even though
        // this nest does hold a corrupt segment and the query does die in the execution phase.
        let quiet = CapturedLogs::default();
        let err = tracing::subscriber::with_default(
            tracing_subscriber::registry().with(quiet.clone()),
            || query(dir.path(), "SELECT CAST('x' AS INTEGER)").unwrap_err(),
        );
        // `{:#}` for the whole chain: the outermost context is just "query failed", and asserting on
        // that would pass for any execution failure at all, including one this test did not cause.
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Conversion Error"),
            "expected the cast itself to be what failed, got: {chain}"
        );
        // Negative assertion via `CapturedLogs`, not a return value: this is the one place in the
        // binary #482's grep sweep left on the log-capture path, because nothing else observes
        // whether the sweep was reachability-bounded. Safe as a negative check only - see the
        // `CapturedLogs` doc comment for why a positive assertion here would be unreliable.
        assert_eq!(
            quiet.mentioning(REJECTED),
            0,
            "a query naming no table must not read or hash a single segment - this is the 27-byte \
             amplifier the review found"
        );

        // The positive control: a query that *does* name the table sweeps it and finds the corrupt
        // segment. Without this, the assertion above would pass just as happily with the sweep
        // deleted entirely.
        //
        // Asserted on `segments_failing_verification`'s own return value, computed via the same
        // `reject_unknown_table_refs` walk `run()` uses - not by scraping its `tracing::error!` log
        // line. `tracing`'s per-callsite interest is a *global, process-wide* cache: whichever test in
        // this binary hits that callsite first while no subscriber is installed gets it permanently
        // marked uninteresting, so a `with_default` scope elsewhere in the same test binary can miss
        // the event nondeterministically depending on run order (reproduced on `main`, independent of
        // this fix - see #482). The return value has no such race.
        let rows = query(dir.path(), r#"SELECT "from" FROM "t__transfer""#).expect("reduces");
        assert_eq!(rows.len(), 1, "the readable segment's row survives");
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let referenced = reject_unknown_table_refs(&conn, r#"SELECT "from" FROM "t__transfer""#)
            .unwrap()
            .map(|(names, _)| expand_through_views(&conn, &names))
            .expect("the query names a table");
        assert_eq!(
            crate::seal::segments_failing_verification(dir.path(), &referenced, None),
            std::collections::BTreeSet::from([victim.hash.clone()]),
            "the same corrupt segment must be found when the query does name its table"
        );
    }

    /// **Issue #476, end to end - tightened for #500.** Before #476's fix, `run`'s watchdog covered
    /// only the query execution either side of the sweep: the sweep itself ran with nothing watching
    /// it, and the #433 retry got a brand-new `guard.timeout` rather than whatever was left of the
    /// first one. A genuinely degraded nest could cost up to `2 x timeout` in execution alone, plus an
    /// unbounded sweep in between.
    ///
    /// #500: the first version of this test could only tell a bounded sweep apart from an *unbounded*
    /// one (`deadline: None`), not from one hand a **fresh** `guard.timeout` at the sweep call site,
    /// which is the defect its name actually calls out - and that is the mutation that matters, since
    /// it is what `deadline` accidentally recomputed at the wrong point would look like. The first
    /// attempt (dying on a page-corrupt segment) is near-instant, so a fresh deadline taken a few
    /// microseconds later was indistinguishable from the shared one, and the elapsed-time window this
    /// test asserted was wide enough to swallow the gap.
    ///
    /// This version makes the first attempt itself consume most of the budget
    /// (`test_set_first_attempt_delay_ms`), so a shared vs. a fresh deadline stop differing by degree
    /// and start differing in kind. Bound to the shared deadline, only a sliver is left when the sweep
    /// starts: it hashes segments one and two (unrelated, intact) but runs out before segment three,
    /// the corrupt one, so `corrupt` comes back empty and `run` fails on its own plain "time budget"
    /// check - a clean, cooperative bail, never touching a second `attempt`.
    ///
    /// Handed a fresh `guard.timeout` instead, the sweep gets the full budget again from that same
    /// later point and reaches the corrupt segment - but the retry it then triggers is still bound by
    /// the *original* shared `deadline` (only the sweep's own deadline was mutated), which by then has
    /// already passed, so the retry's watchdog interrupts it mid-query instead. Both outcomes are
    /// `Err`, so Ok vs Err cannot see this; the *error itself* differs in kind, though - a plain
    /// "exceeded budget" message the bounded sweep produces cooperatively, versus DuckDB's own
    /// "Interrupted!" from a retry that got cut off while running. That is the assertion below, not
    /// elapsed wall-clock time.
    ///
    /// #529: even that was still timing-coupled. The 200ms budget minus a 120ms first-attempt delay
    /// left ~80ms for a 2x50ms sweep to land in - a margin of one segment's worth, and
    /// `thread::sleep` only guarantees *at least* the requested duration. At load average 34 a
    /// descheduled thread can wake arbitrarily later than 50ms, so which of the two branches the
    /// (unmutated, correct) code actually took stopped being reliable: `cargo test --lib` saw the
    /// *other* outcome's message on a busy box. Fixed by making the margin lopsided rather than
    /// tight enough to race: the first-attempt delay (3s) is set to comfortably outlive the whole
    /// 200ms budget by itself, so the shared deadline is unambiguously, already expired - by seconds,
    /// not by a contended few milliseconds - before the sweep is ever called, on every run regardless
    /// of scheduling. The sweep's own per-segment cost is no longer artificially delayed at all: the
    /// fixture's three tiny segments cost microseconds to hash for real, so a *fresh* deadline
    /// (mutation A) or no deadline (mutation B) still has essentially the whole 200ms of real
    /// headroom to reach the corrupt segment in, which is orders of magnitude more slack than
    /// scheduling jitter needs even under heavy contention. The two outcomes no longer share a
    /// finish line to race across; one is already over before the sweep starts, the other has ample
    /// room regardless of load.
    #[test]
    fn the_sweep_is_bound_by_the_query_s_own_deadline_not_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true}]}]}"#,
        )
        .unwrap();
        for block in [1u64, 2, 3] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"0xa","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let victim = manifest.tables["t__transfer"]
            .iter()
            .max_by_key(|s| s.from_block)
            .expect("the block-3 segment");
        let path = crate::seal::segment_path(dir.path(), &victim.file, &victim.hash);
        assert!(
            corrupt_pages_leaving_the_footer_intact(&path) > 0,
            "the fixture must have corrupted something"
        );

        // A 200ms budget with a 3s first-attempt delay: by the time the sweep is ever called, the
        // shared deadline is already several seconds in the past, unambiguously - not a close race
        // against a comparably-sized per-segment cost (see the doc comment above for why that
        // changed). The three fixture segments are real work but microseconds of it, so a fresh or
        // absent deadline still has essentially the whole 200ms of headroom to reach the corrupt one.
        test_set_first_attempt_delay_ms(dir.path(), 3_000);
        let guard = QueryGuard {
            timeout: Duration::from_millis(200),
            max_rows: 10,
        };
        let result = query_guarded(dir.path(), r#"SELECT "from" FROM "t__transfer""#, guard);
        test_set_first_attempt_delay_ms(dir.path(), 0);

        let message = result.as_ref().err().map(ToString::to_string);
        assert_eq!(
            message.as_deref(),
            Some("query exceeded the 0s time budget on the read-only SQL surface"),
            "bound to the shared deadline, the sweep has only a sliver of the 200ms budget left when \
             it starts (120ms already spent on the first attempt) and cannot reach the corrupt third \
             segment before that budget is spent, so `run` must bail on its own cooperative \"time \
             budget\" check without ever starting a second attempt. A sweep handed a fresh 200ms \
             instead reaches the corrupt segment too late for the *original* shared deadline that \
             still bounds the retry, so it dies mid-query on DuckDB's own interrupt instead - a \
             different error, not just a slower one: got {result:?}"
        );
    }

    /// The other edge of the same bound, and the regression it would otherwise have caused. A query
    /// over an **authored view** (RFC-0001 `views/*.sql`) names the view, which is no table in the
    /// manifest - so a sweep bounded on the named set alone would verify nothing, and a page-corrupt
    /// segment under that view would fail the query instead of reducing it. That is #433's own defect,
    /// reintroduced one layer up by its own cost bound.
    ///
    /// I did not find this from the review; I found it reading my own fix back, which is the only
    /// reason it is not shipping. `expand_through_views` is what this fails without.
    #[test]
    fn a_page_corrupt_segment_under_an_authored_view_still_reduces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true}]}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-senders.sql"),
            "CREATE VIEW senders AS SELECT \"from\", block_number FROM t__transfer;",
        )
        .unwrap();
        for (block, from) in [(1u64, "0xa"), (2u64, "0xb")] {
            crate::seal::seal_range(
                dir.path(),
                &[format!(
                    r#"{{"table":"t__transfer","from":"{from}","block_number":{block},"tx_hash":"0xt","log_index":0}}"#
                )],
                block,
                block,
            )
            .unwrap();
        }

        // Whole first, through the view, or the reduction assertion below proves nothing.
        let rows = query(dir.path(), "SELECT \"from\" FROM senders").expect("the view resolves");
        assert_eq!(rows.len(), 2, "two segments, two rows, through the view");

        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let victim = manifest.tables["t__transfer"]
            .iter()
            .find(|s| s.from_block == 2)
            .expect("the block-2 segment");
        let path = crate::seal::segment_path(dir.path(), &victim.file, &victim.hash);
        assert!(corrupt_pages_leaving_the_footer_intact(&path) > 0);

        let rows = query(dir.path(), "SELECT \"from\" FROM senders").expect(
            "a query that reaches the corrupt segment through a view must still reduce - if this \
             errors, the reachability bound cannot see through views",
        );
        assert_eq!(rows.len(), 1, "the readable segment's row survives");
        assert_eq!(rows[0]["from"], Value::from("0xa"));
    }

    /// The input to that bound: what the security walk reports a statement reaches. One parse feeds
    /// both controls, so this pins the half `reject_unknown_table_refs` did not used to have.
    /// **#896, found against the real Lodestar nest.** A real authored view breaks the line after
    /// `AS`; the keyword search wanted a literal space on both sides and did not find it. The view
    /// then never entered the map `reachable_tables` builds, its source table was never defined, and
    /// a view that plainly exists came back as `Catalog Error: Table with name … does not exist`.
    ///
    /// Every fixture in this file wrote its view on one line, which is why none of them saw it -
    /// and my first attempt at this test did too, and passed against the broken code.
    #[test]
    fn a_view_that_breaks_the_line_after_as_still_yields_its_source_table() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"t__transfer","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"from","sol_type":"address","storage":"address","indexed":true}]}]}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        // The shape of `40-indexers.sql`: prose, then a view whose body starts on the next line.
        std::fs::write(
            dir.path().join("views/10-senders.sql"),
            "-- Per-sender rollup. The count comes from the folded `senders` view (\u{a7}10).\n\
             CREATE VIEW senders AS\n\
             SELECT \"from\", block_number\n\
             FROM t__transfer;",
        )
        .unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"t__transfer","from":"0xa","block_number":1,"tx_hash":"0xt","log_index":0}"#
                .to_string()],
            1,
            1,
        )
        .unwrap();

        let rows = query(dir.path(), r#"SELECT "from" FROM senders"#)
            .expect("a view whose body starts on the next line still resolves");
        assert_eq!(rows.len(), 1, "{rows:?}");
    }

    #[test]
    fn the_table_refs_walk_reports_what_the_statement_reached() {
        let conn = Connection::open_in_memory().unwrap();
        let refs = |sql: &str| reject_unknown_table_refs(&conn, sql).unwrap().unwrap().0;
        let surveys = |sql: &str| reject_unknown_table_refs(&conn, sql).unwrap().unwrap().1;

        assert!(
            refs("SELECT CAST('x' AS INTEGER)").is_empty(),
            "a constant expression reaches no table"
        );
        assert_eq!(
            refs(r#"SELECT * FROM "t__transfer""#),
            ["t__transfer".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
        );
        // DuckDB matches identifiers case-insensitively, so the sweep's lookup must too - otherwise a
        // shouted table name silently loses its reduction.
        assert_eq!(
            refs("SELECT * FROM T__TRANSFER"),
            ["t__transfer".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
        );
        assert_eq!(
            refs("SELECT (SELECT max(a) FROM u) FROM t"),
            ["t".to_string(), "u".to_string()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
            "a table reached from a subquery is still reached"
        );

        // #896: a statement that reaches a catalogue schema or calls one of DuckDB's enumerating
        // table functions is asking *what tables exist*, so every view has to be defined for it to
        // answer. The bare-name set cannot express that - `information_schema.tables` arrives as
        // `tables` with the qualifier dropped, indistinguishable from a nest table of that name.
        assert!(!surveys(r#"SELECT * FROM "t__transfer""#));
        assert!(!surveys("SELECT (SELECT max(a) FROM u) FROM t"));
        assert!(
            surveys("SELECT table_name FROM information_schema.tables"),
            "a catalogue schema must be recognised through the dropped qualifier"
        );
        // DuckDB's own enumerating table functions are refused outright by `ALLOWED_TABLE_FNS`, so
        // the `duckdb_` branch in the walk is unreachable today. It stays because the failure it
        // guards is silent: admit `duckdb_tables` to that allowlist without thinking about #896 and
        // the catalogue listing comes back empty rather than erroring.
        assert!(
            reject_unknown_table_refs(&conn, "SELECT * FROM duckdb_views()").is_err(),
            "an enumerating table function is refused before the survey question arises"
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
        define_views(
            &conn,
            dir.path(),
            &empty,
            u64::MAX,
            &Default::default(),
            &[],
            None,
        )
        .unwrap();
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
        define_views(
            &conn,
            dir.path(),
            &empty,
            u64::MAX,
            &Default::default(),
            &[],
            None,
        )
        .unwrap();
        define_nest_views(&conn, dir.path());

        assert!(
            conn.query_row("SELECT count(*) FROM big_transfers", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "with no schema.json there is no typed empty view, so the authored view cannot resolve - \
             this is the failure `refresh_stale_artifacts` prevents by regenerating the schema"
        );
    }

    /// #663. The reported shape, not a degenerate stand-in for it: one `CREATE VIEW` spans two
    /// declared tables, one populated and one that has genuinely never fired, and the view supplies
    /// fields from both. Collapsing this to one table/one field would make "loses every field" and
    /// "resolves correctly" look the same, which is exactly the fixture the issue warns against.
    ///
    /// `schema.json` on disk only knows the table that has always existed - `gns__grt_withdrawn` was
    /// declared later and the file was never regenerated against it. That is `define_views`'s only
    /// source of "what tables exist" before this fix; `declared` (the live registry schema `dev`
    /// already computes as `served`/`full_schema`) is the fix - a second, always-current source that
    /// doesn't depend on `schema.json` being fresh.
    #[test]
    fn a_view_joining_a_populated_and_a_never_fired_table_resolves_once_the_live_schema_is_supplied(
    ) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("schema.json"),
            r#"{"tables":[{"table":"gns__signal_minted","columns":[
                {"name":"block_number","sol_type":"implicit","storage":"u64","indexed":false},
                {"name":"value","sol_type":"uint256","storage":"word32","indexed":false},
                {"name":"pool","sol_type":"bytes32","storage":"bytes32","indexed":true}]}]}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("views/80-gns-network.sql"),
            "CREATE VIEW gns_network AS SELECT m.value AS minted_value, m.pool AS minted_pool, \
             w.value AS withdrawn_value, w.recipient AS withdrawn_recipient \
             FROM gns__signal_minted m LEFT JOIN gns__grt_withdrawn w ON true;",
        )
        .unwrap();

        let col = |name: &str, storage: &str, indexed: bool| crate::registry::ColumnSchema {
            name: name.into(),
            sol_type: String::new(),
            storage: storage.into(),
            indexed,
        };
        let declared = vec![
            crate::registry::TableSchema {
                table: "gns__signal_minted".into(),
                alias: "gns".into(),
                kind: crate::registry::TableKind::Event,
                function: String::new(),
                selector: String::new(),
                event: "SignalMinted".into(),
                topic0: "0xaaaa".into(),
                columns: vec![
                    col("block_number", "u64", false),
                    col("value", "word32", false),
                    col("pool", "bytes32", true),
                ],
            },
            crate::registry::TableSchema {
                table: "gns__grt_withdrawn".into(),
                alias: "gns".into(),
                kind: crate::registry::TableKind::Event,
                function: String::new(),
                selector: String::new(),
                // The L1-migration event: really on the ABI, never emitted on this chain (#663's repro).
                event: "GRTWithdrawn".into(),
                topic0: "0xbbbb".into(),
                columns: vec![
                    col("block_number", "u64", false),
                    col("value", "word32", false),
                    col("recipient", "address", true),
                ],
            },
        ];

        let mut hot = HotRows::new();
        hot.insert(
            "gns__signal_minted".to_string(),
            vec![
                serde_json::json!({"block_number": 100, "log_index": 0, "value": "500", "pool": "0xpool"}),
            ],
        );

        // Before: `define_views` only knows `schema.json`, which doesn't have `gns__grt_withdrawn`.
        // It gets no view at all, and the single `CREATE VIEW gns_network` statement - which touches
        // both tables - fails to bind. Pinning the bug this issue reports, not just the fix.
        {
            let conn = Connection::open_in_memory().unwrap();
            define_views(
                &conn,
                dir.path(),
                &hot,
                u64::MAX,
                &Default::default(),
                &[],
                None,
            )
            .unwrap();
            define_nest_views(&conn, dir.path());
            assert!(
                conn.query_row("SELECT count(*) FROM gns_network", [], |r| r
                    .get::<_, i64>(0))
                    .is_err(),
                "pin the bug: one never-fired table takes the whole view down, all four fields"
            );
        }

        // After: `declared` (the live registry schema) knows `gns__grt_withdrawn` even though
        // `schema.json` doesn't, so it gets an empty typed view and the join resolves - the fired
        // table's real data intact, the never-fired table's side NULL rather than absent.
        {
            let conn = Connection::open_in_memory().unwrap();
            define_views(
                &conn,
                dir.path(),
                &hot,
                u64::MAX,
                &Default::default(),
                &declared,
                None,
            )
            .unwrap();
            define_nest_views(&conn, dir.path());
            let row = conn
                .query_row(
                    "SELECT minted_value, minted_pool, withdrawn_value, withdrawn_recipient \
                     FROM gns_network",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .expect("the populated half of the join must resolve, not merely avoid erroring");
            assert_eq!(row.0, "500", "the fired table's real data survives the fix");
            assert_eq!(row.1, "0xpool");
            assert_eq!(
                row.2, None,
                "the never-fired table degrades to NULL on its side, not an error"
            );
            assert_eq!(row.3, None);
        }
    }

    /// Reviewer question on #723: is the reported failure reachable through the *real* constructor
    /// chain, or only through a hand-authored `TableSchema`/`schema.json` fixture like the test
    /// above? Built here with nothing hand-authored: a real `nuthatch.toml` through `Config::load`,
    /// a real `schema.json` through `project::refresh_stale_artifacts` (the same call `dev` makes on
    /// startup), a real `declared` through `registry::from_nest` + `indexer::full_schema` (the same
    /// two calls `dev` makes). The only manual step is the one `refresh_stale_artifacts` cannot take
    /// for an identity-keyed nest (see the skip and its comment at the `refresh_stale_artifacts` call
    /// site in `indexer.rs`): editing `nuthatch.toml` to add an event without re-running it, which is
    /// how a real nest's `schema.json` falls behind - a hand-edit, an out-of-band checkout, or a
    /// commit that added the event without regenerating.
    #[test]
    fn the_real_constructor_chain_reproduces_663_and_the_fix_resolves_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("abis/tok.json"),
            r#"[{"type":"event","name":"Minted","anonymous":false,"inputs":[
                {"name":"pool","type":"bytes32","indexed":true},
                {"name":"value","type":"uint256","indexed":false}]},
               {"type":"event","name":"Withdrawn","anonymous":false,"inputs":[
                {"name":"recipient","type":"address","indexed":true},
                {"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();

        // Step 1: declare only `Minted` and generate `schema.json` for real, exactly as `init` does.
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"
[nest]
name = "tok"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "tok"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/tok.json"
events = ["Minted"]
"#,
        )
        .unwrap();
        let cfg = crate::config::Config::load(dir.path()).unwrap();
        crate::project::refresh_stale_artifacts(dir.path(), &cfg).unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("schema.json")).unwrap();
        assert!(
            !on_disk.contains("tok__withdrawn"),
            "schema.json must only know Minted at this point"
        );

        // Step 2: hand-edit `nuthatch.toml` to declare `Withdrawn` too - a real event this contract
        // really emits, just not yet on this chain - and do NOT regenerate. This is the identity-keyed
        // nest's shape: `refresh_stale_artifacts` deliberately never runs for one (indexer.rs), so a
        // config edit like this is the concrete way `schema.json` falls behind in production.
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"
[nest]
name = "tok"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "tok"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/tok.json"
events = ["Minted", "Withdrawn"]
"#,
        )
        .unwrap();
        let cfg = crate::config::Config::load(dir.path()).unwrap();
        let still_on_disk = std::fs::read_to_string(dir.path().join("schema.json")).unwrap();
        assert!(
            !still_on_disk.contains("tok__withdrawn"),
            "schema.json was not touched by the edit - it is genuinely stale, not simulated"
        );

        // Step 3: the two calls `dev` makes at startup to get `declared` - real registry, real
        // full_schema, no hand-built `TableSchema`.
        let registry = crate::registry::from_nest(dir.path(), &cfg).unwrap();
        let declared = crate::indexer::full_schema(&registry, &cfg);
        assert!(
            declared.iter().any(|t| t.table == "tok__withdrawn"),
            "the live registry must know about Withdrawn even though schema.json does not"
        );

        std::fs::write(
            dir.path().join("views/80-tok-network.sql"),
            "CREATE VIEW tok_network AS SELECT m.pool AS minted_pool, m.value AS minted_value, \
             w.recipient AS withdrawn_recipient, w.value AS withdrawn_value \
             FROM tok__minted m LEFT JOIN tok__withdrawn w ON true;",
        )
        .unwrap();

        let mut hot = HotRows::new();
        hot.insert(
            "tok__minted".to_string(),
            vec![serde_json::json!({"block_number": 1, "log_index": 0, "pool": "0xpool", "value": "42"})],
        );

        // Before the fix this view fails to bind at all (pinned with a hand-built fixture above);
        // here, with everything built through the real chain, it must resolve.
        let conn = Connection::open_in_memory().unwrap();
        define_views(
            &conn,
            dir.path(),
            &hot,
            u64::MAX,
            &Default::default(),
            &declared,
            None,
        )
        .unwrap();
        define_nest_views(&conn, dir.path());
        let row = conn
            .query_row(
                "SELECT minted_pool, minted_value, withdrawn_recipient, withdrawn_value FROM tok_network",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .expect("real constructor chain: the view must resolve, not just the hand-built one");
        assert_eq!(row.0, "0xpool");
        assert_eq!(row.1, "42");
        assert_eq!(row.2, None);
        assert_eq!(row.3, None);
    }

    /// #729's counterpart to the #663 test above: the table itself is never missing, only its *column
    /// set* falls behind. Built the same way, with nothing hand-authored - a real `nuthatch.toml`
    /// through `Config::load`, a real `schema.json` through `project::refresh_stale_artifacts`, a real
    /// sealed segment through `seal::seal_range`, and a real `declared` through `registry::from_nest` +
    /// `indexer::full_schema` reading the ABI file *after* it changes. No config edit is needed at all
    /// here (unlike #663's added-event case): `events = ["Transfer"]` never changes, only the ABI's
    /// field list for that same event - which is exactly how a re-fetched ABI drifts from an
    /// already-generated `schema.json` in production.
    #[test]
    fn the_real_constructor_chain_reproduces_729_and_the_fix_resolves_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[
                {"name":"from","type":"address","indexed":true},
                {"name":"to","type":"address","indexed":true},
                {"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"
[nest]
name = "tok"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "tok"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/tok.json"
events = ["Transfer"]
"#,
        )
        .unwrap();

        // Step 1: generate `schema.json` for real, exactly as `init` does, against the pre-refetch ABI.
        let cfg = crate::config::Config::load(dir.path()).unwrap();
        crate::project::refresh_stale_artifacts(dir.path(), &cfg).unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("schema.json")).unwrap();
        assert!(
            !on_disk.contains("memo"),
            "schema.json must only know the pre-refetch ABI at this point"
        );

        // One sealed segment, written under the ABI as it stood when schema.json was generated - it
        // genuinely has no `memo` column, the same way a real segment sealed before an ABI bump can't.
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"tok__transfer","from":"0xa","to":"0xb","value":"9","block_number":1,"tx_hash":"0xt","log_index":0}"#.to_string()],
            1,
            1,
        )
        .unwrap();

        // Step 2: the ABI is re-fetched (Sourcify/Etherscan-class, per CLAUDE.md) and now carries `memo`
        // on the same `Transfer` event - a real ABI change, not a hand-built fixture. `schema.json` is
        // not regenerated, which is the concrete way it falls behind in production: nobody re-ran
        // `nuthatch schema` (or restarted `dev`) the moment the ABI changed.
        std::fs::write(
            dir.path().join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[
                {"name":"from","type":"address","indexed":true},
                {"name":"to","type":"address","indexed":true},
                {"name":"value","type":"uint256","indexed":false},
                {"name":"memo","type":"string","indexed":false}]}]"#,
        )
        .unwrap();
        let still_on_disk = std::fs::read_to_string(dir.path().join("schema.json")).unwrap();
        assert!(
            !still_on_disk.contains("memo"),
            "schema.json was not touched by the ABI re-fetch - it is genuinely stale, not simulated"
        );

        // Step 3: the two calls `dev` makes at startup to get `declared` - real registry, real
        // full_schema, no hand-built `TableSchema`.
        let registry = crate::registry::from_nest(dir.path(), &cfg).unwrap();
        let declared = crate::indexer::full_schema(&registry, &cfg);
        assert!(
            declared
                .iter()
                .find(|t| t.table == "tok__transfer")
                .expect("tok__transfer must still be declared")
                .columns
                .iter()
                .any(|c| c.name == "memo"),
            "the live registry must know about `memo` even though schema.json does not"
        );

        let guard = QueryGuard {
            timeout: Duration::from_secs(5),
            max_rows: 1000,
        };
        // Before the fix this is a binder error ("Referenced column memo not found"); with everything
        // built through the real chain, it must resolve, and `memo` must read back NULL for the segment
        // that predates it.
        let out = query_hot_cold(
            dir.path(),
            r#"SELECT value, memo FROM "tok__transfer""#,
            guard,
            &HotRows::new(),
            u64::MAX,
            &declared,
        )
        .expect("real constructor chain: the new column must resolve, not error");
        assert_eq!(out.rows.len(), 1);
        assert_eq!(out.rows[0]["value"], Value::from("9"));
        assert_eq!(out.rows[0]["memo"], Value::Null);
    }

    /// `declared_but_never_sealed` is what turns the empty-view fix above into a log line an operator
    /// can read - #663's other half ("the logs must explain it"). Sealed, not just declared, is the
    /// bar: a table only in `hot` (fired moments ago, not yet sealed) still counts as "never sealed"
    /// here, which is a documented, self-correcting approximation (see the function's own doc comment).
    #[test]
    fn declared_but_never_sealed_names_only_the_table_with_no_sealed_segment() {
        let dir = tempfile::tempdir().unwrap();
        let declared = vec![
            crate::registry::TableSchema {
                table: "gns__signal_minted".into(),
                alias: "gns".into(),
                kind: crate::registry::TableKind::Event,
                function: String::new(),
                selector: String::new(),
                event: "SignalMinted".into(),
                topic0: "0xaaaa".into(),
                columns: vec![],
            },
            crate::registry::TableSchema {
                table: "gns__grt_withdrawn".into(),
                alias: "gns".into(),
                kind: crate::registry::TableKind::Event,
                function: String::new(),
                selector: String::new(),
                event: "GRTWithdrawn".into(),
                topic0: "0xbbbb".into(),
                columns: vec![],
            },
        ];
        // No manifest on disk at all yet: both tables read as never-sealed.
        assert_eq!(
            declared_but_never_sealed(dir.path(), &declared),
            vec![
                "gns__signal_minted".to_string(),
                "gns__grt_withdrawn".to_string()
            ]
        );

        // A real sealed segment for `gns__signal_minted` only - `gns__grt_withdrawn` still has none.
        let entities = vec![
            r#"{"table":"gns__signal_minted","block_number":1,"log_index":0,"value":"500"}"#
                .to_string(),
        ];
        crate::seal::seal_range(dir.path(), &entities, 1, 1).unwrap();
        assert_eq!(
            declared_but_never_sealed(dir.path(), &declared),
            vec!["gns__grt_withdrawn".to_string()],
            "the table with a sealed segment drops off the list; the genuinely never-fired one remains"
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
        define_views(
            &conn,
            dir.path(),
            &empty,
            u64::MAX,
            &Default::default(),
            &[],
            None,
        )
        .unwrap();
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
