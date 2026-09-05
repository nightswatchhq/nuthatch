//! `nuthatch dev` - the loop that makes it alive. Poll logs → decode → store, and serve the API
//! concurrently. One process, one cursor, one failure boundary (per the standing brief).

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

use crate::alerts::{self, AlertRouter};
use crate::chains::{
    self, Finality, UNREGISTERED_FINALITY as DEFAULT_FINALITY,
    UNREGISTERED_WINDOW as DEFAULT_WINDOW,
};
use crate::chunker::{self, AdaptiveWindow};
use crate::cli::DevArgs;
use crate::config::{Config, DB_FILE};
use crate::entity_view::EntityView;
use crate::exposure::{self, ExposureView};
use crate::factory::{ChildRegistry, FactorySet};
use crate::labels::{self, LabelSet};
use crate::metrics::METRICS;
use crate::registry::DecodeRegistry;
use crate::rpc::RpcClient;
use crate::screen::{self, LiveScreener, TransferRow};
use crate::seal;
use crate::serve;
use crate::source::{LogFilter, Source};
use crate::store::Store;
use crate::velocity::{self, VelocityView};
use crate::views::{self, BalanceView};

const LAST_BLOCK_KEY: &str = "last_block";
/// What this nest's stored data was indexed *with*: `"1"` if rows carry `block_timestamp`, `"0"` if
/// not (RFC-0029 §6b). Written on first index and compared on every start.
///
/// Without this, flipping `[nest] block_timestamps` on a nest that has already indexed would produce
/// a store and a segment set in two different schemas, and nothing would notice until a query hit the
/// half without the column. The declaration is an `init`-time one precisely because it cannot be
/// changed in place; this key is what enforces that rather than merely documenting it.
const TIMESTAMPS_KEY: &str = "block_timestamps";
/// The decode registry that produced this store's rows (#653). Not the same question as
/// [`TIMESTAMPS_KEY`]: that one guards a column, this one guards the *identity* of the whole decode
/// configuration, which is what a nest's content address is a statement about.
const REGISTRY_KEY: &str = "registry_hash";
const SEALED_THROUGH_KEY: &str = "sealed_through";
const START_BLOCK_KEY: &str = "start_block";
/// Cold-start origin when a nest declares neither `start_block`s nor an explicit `--backfill`.
const DEFAULT_BACKFILL: u64 = 5_000;

/// `nuthatch dev` - the RPC front-end. Builds an RPC `Source` from the nest's `rpc_urls` and runs
/// the shared pipeline. The colocated-reth front-end (`nuthatch-node`, RFC-0003) builds an ExEx
/// `Source` instead and calls [`run`] directly - same core, different tip source.
pub async fn dev(args: DevArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let mut config = Config::load(&dir)?;
    // RFC-0023 tier 3's archive endpoints. Carried on `Config` because every layer below already
    // takes `&Config`, and `#[serde(skip)]` keeps them out of `nuthatch.toml` and so out of the NID.
    config.state_rpc_urls = args.state_rpc.clone();
    config.ipfs_gateways = args.ipfs.clone();
    // Today: RPC polling. The indexer only sees `dyn Source`, so an ExEx tip source slots in here
    // with no change to anything downstream. An explicit `--rpc` replaces the runtime pool without
    // touching the nest's config on disk.
    let rpc_urls = crate::rpc::select_rpcs(&args.rpc, config.nest.rpc_urls.clone());
    let endpoint_count = rpc_urls.len();
    let rpc = RpcClient::new(rpc_urls)?;
    // Every endpoint must be on this nest's chain before a single block is indexed (issue #150): a
    // wrong-network endpoint in the pool corrupts silently, because failover hides it.
    rpc.verify_chain_ids(config.nest.chain_id).await?;
    let source: Arc<dyn Source> = Arc::new(rpc);
    // Guard the single-endpoint backfill deadlock (see `safe_backfill_concurrency`).
    let concurrency = safe_backfill_concurrency(endpoint_count, args.concurrency);
    if concurrency < args.concurrency {
        tracing::warn!(
            "single RPC endpoint: capping seal-direct backfill concurrency {} → {} (high concurrency \
             to one host can stall the runtime); configure multiple rpc_urls for a parallel backfill",
            args.concurrency,
            concurrency
        );
    }
    run(
        source,
        dir,
        config,
        args.listen,
        args.backfill,
        args.seal_direct,
        concurrency,
        args.window,
        args.no_admin,
    )
    .await
}

/// Interpret a finished background task's join result: a clean `Ok(())`, an indexing/serving error, or
/// a panic/cancellation - labelled for the operator (deadlock-review C1: a dead task must surface, never
/// be served over).
fn join_task(what: &str, joined: Result<Result<()>, tokio::task::JoinError>) -> Result<()> {
    match joined {
        Ok(inner) => inner,
        Err(e) if e.is_panic() => Err(anyhow::anyhow!("{what} panicked")),
        Err(e) => Err(anyhow::anyhow!("{what} task failed: {e}")),
    }
}

/// Poll interval for the hot-upgrade catch-up check.
const UPGRADE_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// `nuthatch nest upgrade` (RFC-0020 slice 2b): hot-upgrade a running nest to a **compatible** new
/// version with zero downtime. Classify old→new; a *breaking* change is refused (it needs a new
/// endpoint - slice 3 - not a hot swap). For a compatible change: serve the OLD version immediately,
/// index the NEW version concurrently against the same chain, and atomically flip the endpoint to it
/// once caught up, then retire the old indexer. Two hot stores run during the overlap (the density cost
/// accepted for decode-changed generality). The consumer's endpoint never changes.
#[allow(clippy::too_many_arguments)]
pub async fn upgrade(
    old_dir: PathBuf,
    new_dir: PathBuf,
    listen: String,
    new_endpoint: String,
    rpc_override: Vec<String>,
    seal_direct: bool,
    concurrency: usize,
    window: Option<u64>,
    no_admin: bool,
) -> Result<()> {
    let old_config = Config::load(&old_dir)?;
    let new_config = Config::load(&new_dir)?;

    let verdict = crate::lifecycle::classify_paths(&old_dir, &new_dir)?;
    let breaking = verdict.verdict == crate::lifecycle::Verdict::Breaking;
    if breaking {
        tracing::info!(
            breaking = verdict.breaking_changes().count(),
            "breaking update - serving the new version on a new endpoint alongside the deprecated old"
        );
    } else {
        tracing::info!(
            additive = verdict.additive_changes().count(),
            "compatible update - hot-upgrading with zero downtime"
        );
    }
    // The new version is served under `/<new_endpoint>` in the breaking case; normalized to one leading
    // slash, no trailing.
    let new_prefix = format!(
        "/{}",
        new_endpoint.trim_start_matches('/').trim_end_matches('/')
    );

    // Slice 4 - for a compatible update whose decode is unchanged, mount the old version's sealed
    // segments into the new nest so it resumes *past* that range instead of re-indexing history (the
    // true no-re-index optimization). A changed decode falls back to a normal index. Done before either
    // indexer opens the stores (redb is single-writer).
    if !breaking {
        match crate::lifecycle::reuse_segments(&old_dir, &new_dir)? {
            crate::lifecycle::ReuseOutcome::Reused {
                sealed_through,
                segments,
            } => tracing::info!(
                sealed_through,
                segments,
                "reusing the old version's sealed segments - the new version resumes past block \
                 {sealed_through} instead of re-indexing history"
            ),
            crate::lifecycle::ReuseOutcome::NotReusable(why) => {
                tracing::info!("segment reuse skipped: {why} - the new version will index history")
            }
        }
    }

    // Neither a compatible nor a breaking update changes chains, so one source feeds both indexers.
    let rpc_urls = crate::rpc::select_rpcs(&rpc_override, old_config.nest.rpc_urls.clone());
    let endpoint_count = rpc_urls.len();
    let rpc = RpcClient::new(rpc_urls)?;
    rpc.verify_chain_ids(old_config.nest.chain_id).await?;
    let source: Arc<dyn Source> = Arc::new(rpc);
    let concurrency = safe_backfill_concurrency(endpoint_count, concurrency);
    let admin_enabled = admin_enabled(no_admin, &listen);
    let admin_token = admin_required_token(admin_enabled, &listen);

    // Both versions index concurrently.
    let old_rt = spawn_nest(
        source.clone(),
        old_dir,
        old_config,
        None,
        seal_direct,
        concurrency,
        window,
        admin_enabled,
        admin_token.clone(),
    )
    .await?;
    let new_rt = spawn_nest(
        source,
        new_dir,
        new_config,
        None,
        seal_direct,
        concurrency,
        window,
        admin_enabled,
        admin_token,
    )
    .await?;

    let old_store = old_rt.state.store.clone();
    let new_store = new_rt.state.store.clone();
    let new_state = new_rt.state; // consumed by whichever path runs
    let old_shared = serve::SharedNest::new(old_rt.state);
    let mut ingest_old = old_rt.ingest;
    let mut ingest_new = new_rt.ingest;
    let old_alert = old_rt.alert_worker;
    let new_alert = new_rt.alert_worker;

    let result = if breaking {
        // Slice 3 - serve BOTH on distinct endpoints, no flip: old stays at root (deprecated), new
        // under `new_prefix`. Both persist; the operator sunsets the old when downstream have migrated.
        let new_shared = serve::SharedNest::new(new_state);
        let mut serve_task = {
            let old_shared = old_shared.clone();
            tokio::spawn(async move {
                serve::run_two_versions(&listen, old_shared, &new_prefix, new_shared).await
            })
        };
        let r = tokio::select! {
            r = &mut serve_task => join_task("serving", r),
            j = &mut ingest_old => join_task("old indexing", j),
            j = &mut ingest_new => join_task("new indexing", j),
        };
        serve_task.abort();
        r
    } else {
        // Slice 2b - serve old, index new, atomically flip once caught up, retire old.
        let mut serve_task = {
            let shared = old_shared.clone();
            tokio::spawn(async move { serve::run_shared(&listen, shared).await })
        };
        let mut flip_task = {
            let shared = old_shared.clone();
            let old_store = old_store.clone();
            let new_store = new_store.clone();
            tokio::spawn(async move {
                await_catchup_and_flip(&shared, &old_store, &new_store, new_state, UPGRADE_POLL)
                    .await
            })
        };
        // Phase 1 - old + new both live. Any task dying fails loudly (C1). The flip completing → phase 2.
        let r = tokio::select! {
            r = &mut serve_task => join_task("serving", r),
            j = &mut ingest_old => join_task("old indexing", j),
            j = &mut ingest_new => join_task("new indexing", j),
            f = &mut flip_task => match f {
                Ok(Ok(())) => {
                    tracing::info!("hot-upgrade flip complete - retiring the old version's indexer");
                    // The old indexer is now intentionally retired; its cancellation is NOT a failure.
                    ingest_old.abort();
                    // Phase 2 - only the new version + serving remain.
                    tokio::select! {
                        r = &mut serve_task => join_task("serving", r),
                        j = &mut ingest_new => join_task("new indexing", j),
                    }
                }
                Ok(Err(e)) => Err(e.context("hot-upgrade flip")),
                Err(e) => Err(anyhow::anyhow!("flip task failed: {e}")),
            },
        };
        serve_task.abort();
        flip_task.abort();
        r
    };

    ingest_old.abort();
    ingest_new.abort();
    if let Some(w) = old_alert {
        w.abort();
    }
    if let Some(w) = new_alert {
        w.abort();
    }
    result
}

/// A single nest's contribution to a running process: its serve state plus the background tasks that
/// keep it fed (the ingestion loop, and an optional alert/webhook delivery worker). Built by
/// [`spawn_nest`]; consumed either by [`run`] (one nest, served at the root) or by the runtime
/// (RFC-0012 - many nests, each served under a `/<name>/…` prefix behind one listener).
pub struct NestRuntime {
    pub state: serve::AppState,
    /// The ingestion loop task. Its `Result` is `Ok` only on a clean shutdown; an error or panic here
    /// must surface as a process failure, never be served-over silently (deadlock-review C1).
    pub ingest: tokio::task::JoinHandle<Result<()>>,
    /// The shared alert/webhook delivery worker, if any sink or webhook is configured. Only ever
    /// aborted (it drains a durable outbox), so its output type doesn't matter.
    pub alert_worker: Option<tokio::task::JoinHandle<()>>,
}

/// Poll until the new version's indexed head reaches the old version's, then **atomically flip** the
/// served backing to the new version (RFC-0020 slice 2b, the compatible hot-swap). Old and new indexers
/// run concurrently until this returns; the caller aborts the old ingest afterwards. `poll` bounds how
/// often the two heads are compared.
pub async fn await_catchup_and_flip(
    shared: &serve::SharedNest,
    old_store: &dyn crate::store::HotStore,
    new_store: &dyn crate::store::HotStore,
    new_state: serve::AppState,
    poll: std::time::Duration,
) -> Result<()> {
    loop {
        let old_head = old_store.indexed_head()?;
        let new_head = new_store.indexed_head()?;
        if crate::lifecycle::caught_up(new_head, old_head) {
            tracing::info!(
                ?old_head,
                ?new_head,
                "new version caught up - hot-swapping the served backing (RFC-0020)"
            );
            shared.swap(new_state);
            return Ok(());
        }
        tokio::time::sleep(poll).await;
    }
}

/// Run the indexing pipeline against any `Source` and serve the API - the source-agnostic entry both
/// front-ends share. Decode → hot store → seal → IVM → serve is identical regardless of whether tips
/// arrive by RPC polling or in-process from a reth ExEx.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    source: Arc<dyn Source>,
    dir: PathBuf,
    config: Config,
    listen: String,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    no_admin: bool,
) -> Result<()> {
    // Admin UI (RFC-0010 Part A): on by default on localhost. Off-localhost it needs an explicit token
    // (auth is the operator's gateway's job, but the local UI should never appear unguarded on a public
    // bind); `--no-admin` removes it entirely. Computed here since it depends on the process's `listen`.
    let admin_enabled = admin_enabled(no_admin, &listen);
    let admin_token = admin_required_token(admin_enabled, &listen);
    let NestRuntime {
        state,
        mut ingest,
        alert_worker,
    } = spawn_nest(
        source,
        dir,
        config,
        backfill,
        seal_direct,
        concurrency,
        window_override,
        admin_enabled,
        admin_token,
    )
    .await?;

    // The indexer and the API share a fate. If indexing dies (an error or a panic) the process must
    // not keep serving stale data as if healthy - a silent failure (deadlock-review finding C1). Select
    // over both: whichever ends first decides the exit, and an indexing error/panic propagates out.
    let result = tokio::select! {
        r = serve::run(&listen, state) => r,
        joined = &mut ingest => match joined {
            Ok(inner) => inner,
            Err(e) if e.is_panic() => Err(anyhow::anyhow!("indexing loop panicked")),
            Err(e) => Err(anyhow::anyhow!("indexing loop task failed: {e}")),
        },
    };
    ingest.abort();
    if let Some(w) = alert_worker {
        w.abort();
    }
    result
}

/// The admin token from the environment, treating unset OR empty/whitespace as "no token": an empty
/// `NUTHATCH_ADMIN_TOKEN=` must neither enable the admin route off-localhost nor become a null
/// credential that a bare `?token=` satisfies (SEC).
fn admin_token_env() -> Option<String> {
    std::env::var("NUTHATCH_ADMIN_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
}

/// Whether the built-in admin UI should be served, given `--no-admin` and the bind address. Extracted
/// so the runtime computes it once for the whole process (RFC-0010 Part A semantics unchanged).
///
/// **Every** role that serves the admin surface must route its decision through here rather than
/// reading the env var directly: this is the only place that turns "off-localhost with no token" into
/// *off* instead of *open*, and a role that skips it publishes an unauthenticated admin UI (#292).
pub fn admin_enabled(no_admin: bool, listen: &str) -> bool {
    let enabled = !no_admin && (serve::is_localhost(listen) || admin_token_env().is_some());
    if !no_admin && !enabled {
        tracing::warn!(
            "admin UI disabled: bound off-localhost without NUTHATCH_ADMIN_TOKEN set (RFC-0010 Part A)"
        );
    }
    enabled
}

/// The token an admin-UI request must present, given the bind (SEC-5). `None` on a localhost bind (the
/// UI is open there); `Some(token)` off-localhost (the request must carry `?token=…`) - actually
/// checking it per request, rather than the env var merely *enabling* the route.
pub fn admin_required_token(admin_enabled: bool, listen: &str) -> Option<String> {
    if admin_enabled && !serve::is_localhost(listen) {
        admin_token_env()
    } else {
        None
    }
}

/// Build one nest's runtime: open its store, build its decode registry + IVM views, spawn its
/// ingestion loop and delivery worker, and assemble its serve state - everything *except* binding a
/// listener. The serving decision (root vs a `/<name>/…` prefix, one nest vs many) belongs to the
/// caller. Per-nest isolation (own store, own segments, own views) is the CLAUDE.md non-negotiable a
/// mounts preserves by calling this once per nest.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_nest(
    source: Arc<dyn Source>,
    dir: PathBuf,
    config: Config,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
) -> Result<NestRuntime> {
    let (nest, state, alert_worker, window) = build_nest(
        &source,
        dir,
        &config,
        window_override,
        admin_enabled,
        admin_token,
        None,
        // A solo `dev` nest is its own cursor, so it owns the gate outright (#1024).
        serve::new_sql_gate(),
    )
    .await?;
    refuse_seal_direct_with_entities(seal_direct, &nest)?;
    // Kick off the indexing loop in the background; serve the API on this task.
    let ingest = tokio::spawn(index_loop(
        source,
        nest,
        backfill,
        seal_direct,
        concurrency,
        window,
    ));
    Ok(NestRuntime {
        state,
        ingest,
        alert_worker,
    })
}

/// Seed every warm-started entity from the nest's own stored history (RFC-0041 §5.3, #865).
///
/// **Zero historical RPC calls, by construction** - criterion #865's claim, and it is a claim about
/// the *arguments*: this function is handed a directory, a store and a registry, and has no source
/// to call. Sealed Parquet plus the unsealed hot tail is the whole of a nest's canonical history,
/// which is the point of §5.3's "canonical facts remain the durable source of truth".
///
/// The two halves are read separately because they are stored separately - sealing prunes sealed
/// rows out of hot (`prune_and_set_meta`), so neither layer holds the answer alone. They are then
/// fed as **one batch**: §5.1's "backfill uses larger batches, but not different semantics", and the
/// reason a finalized row joining a hot row is not a problem here the way it is for a base-plus-delta
/// seed, where such a pair is in neither half.
///
/// A failure leaves the entity `unavailable` rather than partly filled, and says so: `seed` clears
/// that state only on the way in, so an entity that faulted mid-seed is not left looking maintained.
fn seed_entities(
    dir: &std::path::Path,
    store: &dyn crate::store::HotStore,
    registry: &DecodeRegistry,
    entities: &mut [EntityView],
    through: u64,
) -> Result<()> {
    if entities.is_empty() {
        return Ok(());
    }
    let schema = registry.schema();

    // The hot tail, read once and shared. Every entity on this nest sees the same window at `+1`
    // during normal ingest, and a seed is a window like any other.
    let hot = decode_stored_rows(&schema, &store.entities_in_range(0, through)?)?;

    for entity in entities.iter_mut() {
        entity.seed_begin();
        // One sealed segment at a time, never the whole history at once - see
        // `seal::read_table_rows_by_segment` for what that is worth in peak RSS. The hot tail is
        // fed by reference for the same reason: it used to be cloned once per entity on the nest.
        let mut sealed = 0usize;
        for table in entity.tables() {
            let Some(table_schema) = schema.iter().find(|t| t.table == table) else {
                anyhow::bail!(TerminalFault(format!(
                    "entity `{}` reads `{table}`, which this nest's registry does not describe",
                    entity.name()
                )))
            };
            crate::seal::read_table_rows_by_segment(dir, table_schema, &mut |rows| {
                sealed += rows.len();
                entity.seed_chunk(&rows, through)
            })?;
        }
        entity.seed_chunk(&hot, through)?;
        tracing::info!(
            "entity `{}` seeding from {sealed} sealed and {} hot row(s) through block {through}",
            entity.name(),
            hot.len(),
        );
        entity.seed_end()?;
    }
    Ok(())
}

/// Stored JSON rows to decoded rows, against a nest's table schemas.
///
/// One conversion, and every caller uses it - the reorg path, the restart seed, and whatever needs it
/// next. A second one written separately is how a retraction stops cancelling its insertion (#864).
fn decode_stored_rows(
    schema: &[crate::registry::TableSchema],
    stored: &[String],
) -> Result<Vec<crate::registry::DecodedRow>> {
    let mut rows = Vec::with_capacity(stored.len());
    for raw in stored {
        let value: serde_json::Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            // Annotations (sanction hits, threshold flags) share the rows' block keys and are not
            // decoded rows. They were never fed to an entity, so they are not read back as one.
            Err(_) => continue,
        };
        let Some(table) = value.get("table").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(table_schema) = schema.iter().find(|t| t.table == table) else {
            anyhow::bail!(TerminalFault(format!(
                "the hot store holds a row of `{table}`, which this nest's registry does not \
                 describe. Reading it against a guessed shape would produce rows that cancel nothing"
            )));
        };
        rows.push(crate::registry::DecodedRow::from_stored(
            &value,
            table_schema,
        )?);
    }
    Ok(rows)
}

/// #866 criterion 13: `--seal-direct` either rebuilds entities from the finished sealed corpus before
/// serving, or refuses the combination clearly. It refuses, **before anything is spawned**.
///
/// Seal-direct writes finalized history straight to Parquet and never puts those windows on the
/// ingest path, so an authored entity would see none of them. Left to run, the nest would complete,
/// serve, and answer with an empty relation - *"a completed run with a silently empty entity is the
/// failure this criterion exists for."*
///
/// **Synchronous on purpose.** The first version of this guard sat inside `prepare`, which runs in
/// the spawned ingest task, so `spawn_nest` returned `Ok` and the nest served for as long as it took
/// the loop to reach the check. A refusal that arrives after the thing it refuses has started is a
/// fault report, not a refusal.
///
/// Rebuilding from the sealed corpus instead is RFC-0041 §5.3's warm-restart seed, which is not
/// built, and is unsound as written for any entity with a join: a finalized row joining a hot row is
/// in neither half. Refusing is the honest half of the criterion until that is settled.
fn refuse_seal_direct_with_entities(seal_direct: bool, nest: &NestIngest) -> Result<()> {
    if !seal_direct || nest.entities.is_empty() {
        return Ok(());
    }
    let named = nest
        .entities
        .iter()
        .map(|e| format!("`{}`", e.name()))
        .collect::<Vec<_>>()
        .join(", ");
    anyhow::bail!(TerminalFault(format!(
        "--seal-direct cannot be combined with authored incremental entities ({named}). \
         Seal-direct writes finalized history straight to Parquet without passing it through the \
         ingest path, so the entit{} would be served empty. Start this nest without --seal-direct; \
         rebuilding an entity from sealed history is RFC-0041 §5.3 and is not implemented.",
        if nest.entities.len() == 1 { "y" } else { "ies" },
    )))
}

/// Spawn a nest against an **externally owned hot store** - the writer-pool path (RFC-0022, issue
/// #250).
///
/// Identical to [`spawn_nest`] except the store is supplied rather than opened locally, which is what
/// makes a worker on one machine index into a Postgres on another. RFC-0022 slice 3b is what allows
/// it: `build_nest` resolves the store once, so nothing downstream knows or cares which backend it got.
///
/// **This is the half of the writer pool that was missing.** `worker::run` acquired cursors and never
/// called anything like this, so a worker held a lease and indexed nothing - the control plane worked
/// and the writer pool did not write.
///
/// The returned handle is the caller's to abort. A worker **must** abort it when its lease is lost:
/// the store's fence already refuses writes from a stale holder, so nothing can corrupt, but a task
/// grinding through RPC for a cursor it no longer owns is pure waste and confusing in logs.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_nest_on_store(
    source: Arc<dyn Source>,
    dir: PathBuf,
    config: Config,
    store: Arc<dyn crate::store::HotStore>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
) -> Result<NestRuntime> {
    let (nest, state, alert_worker, window) = build_nest(
        &source,
        dir,
        &config,
        None,
        // A writer owns no admin surface: it serves nothing, and the FE tier is what operators reach.
        false,
        None,
        Some(store),
        // One writer, one cursor (#1024).
        serve::new_sql_gate(),
    )
    .await?;
    refuse_seal_direct_with_entities(seal_direct, &nest)?;
    let ingest = tokio::spawn(index_loop(
        source,
        nest,
        backfill,
        seal_direct,
        concurrency,
        window,
    ));
    Ok(NestRuntime {
        state,
        ingest,
        alert_worker,
    })
}

/// Case-insensitive membership: is `addr` in `addresses`? The demux + dedup primitive - a provider may
/// return checksummed addresses while our filter list is lowercase hex, so never compare raw.
fn addr_in(addresses: &[String], addr: &str) -> bool {
    addresses.iter().any(|a| a.eq_ignore_ascii_case(addr))
}

/// The mounts demux decision (RFC-0012 §2). A **static** nest (non-empty `addresses`) owns a log by
/// emitting address; a **factory** nest (empty `addresses` - topic0-only) owns it by topic0, so it
/// catches its factory-creation events and its runtime-discovered children regardless of their address.
/// Pure so it's testable without a `NestIngest`.
fn log_owned(addresses: &[String], topic0s: &[String], log: &crate::rpc::Log) -> bool {
    if addresses.is_empty() {
        log.topics.first().is_some_and(|t0| addr_in(topic0s, t0))
    } else {
        addr_in(addresses, &log.address)
    }
}

/// The union `getLogs` filter across all mounted nests: the case-insensitively-deduped concatenation of
/// every nest's address list and topic0 list. One fetch feeds them all (RFC-0012 §2 - the density win:
/// N nests cost one nest's worth of RPC chatter, not N). Takes the raw `(addresses, topic0s)` of each
/// nest so it's testable without constructing a `NestIngest`.
///
/// **Factory nests force topic0-only (RFC-0012 slice 2b).** A factory nest has an empty address filter
/// (children are discovered at runtime, so it must see all addresses matching its topics). An empty
/// address list in `getLogs` means "any address", so if *any* mounted nest is a factory the whole union
/// fetch drops its address filter and goes topic0-only - the factory nest then sees every candidate,
/// and static co-tenants over-fetch but demux back to exactly their own logs (`NestIngest::owns`),
/// keeping per-nest output byte-identical to solo.
fn union_filter<'a>(
    nests: impl Iterator<Item = (&'a [String], &'a [String])>,
) -> (Vec<String>, Vec<String>) {
    let mut addrs: Vec<String> = Vec::new();
    let mut topics: Vec<String> = Vec::new();
    let mut any_factory = false;
    for (nest_addrs, nest_topics) in nests {
        // A nest with neither addresses nor topics can issue no `getLogs` at all (#432) - and wants
        // none. Since #445 that is a real supported shape rather than an unbuildable one: a
        // contract-free `[extract] blocks = true` nest, whose rows come from block headers. It
        // contributes nothing to the union, and it must not reach the factory signal below.
        //
        // If it did, it would clear a co-tenant's address filter on behalf of a nest that wanted no
        // logs in the first place - and then `topic0_only_culprits`, which asks `factory.is_some()`,
        // would find nobody to blame for the wide fetch it caused, so COR-5 would end the cursor and
        // take every sibling with it. The two functions encode the same rule from opposite ends; this
        // is what keeps them encoding the *same* rule now that empty-addresses no longer implies
        // factory.
        if nest_addrs.is_empty() && nest_topics.is_empty() {
            continue;
        }
        // An empty address list is the factory / topic0-only signal (see `build_nest`).
        if nest_addrs.is_empty() {
            any_factory = true;
        }
        for a in nest_addrs {
            if !addr_in(&addrs, a) {
                addrs.push(a.clone());
            }
        }
        for t in nest_topics {
            if !addr_in(&topics, t) {
                topics.push(t.clone());
            }
        }
    }
    // Any factory nest → topic0-only fetch (empty address filter = "any address").
    if any_factory {
        addrs.clear();
    }
    (addrs, topics)
}

/// Which nests are answerable for a union fetch that exceeded the provider's result cap on a single
/// block (COR-5).
///
/// A union fetch has no owner, which is why the cap failure used to end the whole cursor: there was
/// nobody to quarantine, so RFC-0026's "one nest cannot kill its siblings" did not apply. But the
/// blame is one-directional. [`union_filter`] drops the address filter *only* when a live nest is a
/// factory nest, and an unfiltered fetch returns strictly more than a filtered one - so a static
/// nest's addresses cannot have caused a cap breach that a topic0-only union hit. The factory nests
/// forced the wide filter, and they are the ones to fault.
///
/// Returns the empty set when no factory nest is live, which means the union carried real addresses
/// and no nest can be singled out - the caller must then fail loudly, as before.
///
/// Kept beside [`union_filter`] deliberately: the two encode the same rule from opposite ends, and a
/// change to one that is not mirrored in the other is caught by
/// `a_topic0_only_union_always_has_someone_to_blame`.
fn topic0_only_culprits<'a>(nests: impl Iterator<Item = (usize, &'a NestIngest)>) -> Vec<usize> {
    nests
        .filter(|(_, n)| n.factory.is_some())
        .map(|(i, _)| i)
        .collect()
}

/// The address filter to retry a cap-breached topic0-only union fetch with (COR-5).
///
/// [`union_filter`] drops the address filter entirely for a factory nest, because a factory nest does
/// not know its children up front. But it is not true that *nothing* is known: a factory nest knows its
/// declared base contracts (the emitters of the creation events) and every child discovered so far. So
/// the narrowed filter is, per live nest, the static nest's own addresses or the factory nest's
/// `base ∪ discovered children` - which is exactly the filter
/// [`backfill_direct_factory`]'s pass 1 uses, brought to the tip loop.
///
/// This is strictly narrower than "any address", so it is worth trying against a provider cap the wide
/// fetch just broke. It is not *equivalent* on its own: it cannot see a child created in this very
/// block, which is why the caller pairs it with the same in-block discovery fixpoint the backfill path
/// runs ([`refetch_address_filtered`]).
///
/// Returns empty when there is nothing to narrow *to* - a factory nest with no base contracts and no
/// discovered children. An empty list means "any address" to `getLogs`, i.e. the identical fetch, so
/// the caller must treat empty as "no fallback available" rather than issuing it.
fn narrowed_union_addresses<'a>(nests: impl Iterator<Item = &'a NestIngest>) -> Vec<String> {
    let mut addrs: Vec<String> = Vec::new();
    for n in nests {
        // An empty address list is the factory / topic0-only signal (see `build_nest`).
        let nest_addrs: Vec<String> = if n.addresses.is_empty() {
            n.registry
                .addresses()
                .iter()
                .map(|a| format!("0x{}", hex::encode(a)))
                .chain(n.children.addresses().iter().map(|c| c.to_string()))
                .collect()
        } else {
            n.addresses.clone()
        };
        for a in nest_addrs {
            if !addr_in(&addrs, &a) {
                addrs.push(a);
            }
        }
    }
    addrs
}

/// Refetch one over-cap block with an address filter instead of topic0-only, running the in-block
/// child-discovery fixpoint so the result is what the wide fetch would have yielded (COR-5).
///
/// Per round: fetch, decode the batch into every factory nest's child registry (discovery only - the
/// rows are discarded, [`NestIngest::process_window`] does the authoritative decode), then refetch the
/// same block for children discovered in this round but not yet fetched. Nested factories converge here
/// exactly as they do in [`backfill_direct_factory`], and the loop terminates because every round adds
/// strictly-new addresses to a set that only grows from decoded events (and `FactorySet::build` caps the
/// chain at depth 3).
///
/// **Discovery is preserved, not traded away.** A child created in this block announces itself from its
/// factory's address, which round 1 fetched, so round 2 picks up its own logs in the same block - the
/// case `process_window`'s inline discovery exists for. That is the difference between this and simply
/// narrowing the filter, which would keep the nest ingesting while quietly going blind to new children.
///
/// **Determinism** (non-negotiable 4) does not depend on how many rounds it took: the merged logs go
/// through the same [`fan_out_window`] → `process_window` → [`decode_window`] path, and `decode_window`
/// sorts by `(block, log_index)` before decoding. Discovery having run early only means a child is
/// already in the registry when the authoritative decode reaches its creation event, and re-inserting a
/// known child is a no-op (`ChildRegistry::insert` keeps the earliest discovery).
async fn refetch_address_filtered(
    source: &dyn Source,
    nests: &mut [Option<NestIngest>],
    live: &[usize],
    addresses: &[String],
    topics: &[String],
    block: u64,
) -> Result<Vec<crate::rpc::Log>> {
    use std::collections::HashSet;
    // Discovery decodes without timestamps, as the backfill's pass-1 decode does: these rows are
    // thrown away, and the stamped decode happens later in `process_window`.
    let empty_ts = std::collections::HashMap::new();
    let mut fetched: HashSet<String> = addresses.iter().map(|a| a.to_ascii_lowercase()).collect();
    let mut all: Vec<crate::rpc::Log> = Vec::new();
    // COR-5 recovery narrows a wide fetch to `base u discovered children`, so it always has an
    // address half; the `?` on a filter that cannot be built would be a bug in that reasoning rather
    // than a runtime condition, and the caller only reaches here with addresses to narrow to.
    let Some(filter) = LogFilter::new(addresses, topics) else {
        return Ok(Vec::new());
    };
    let mut batch = source.logs(&filter, block, block).await?;
    loop {
        let mut new: Vec<String> = Vec::new();
        for &i in live {
            let n = live_nest(nests, i);
            let Some(fs) = n.factory.clone() else {
                continue;
            };
            let registry = n.registry.clone();
            let _ = decode_window(&registry, Some(&fs), &mut n.children, &batch, &empty_ts);
            for c in n.children.addresses() {
                if !fetched.contains(&c.to_ascii_lowercase()) && !addr_in(&new, c) {
                    new.push(c.to_string());
                }
            }
        }
        all.append(&mut batch);
        if new.is_empty() {
            return Ok(all);
        }
        for c in &new {
            fetched.insert(c.to_ascii_lowercase());
        }
        // `new` is non-empty here (the loop returns above when it is not), so the narrowed child
        // fetch always carries an address filter.
        let Some(child_filter) = LogFilter::new(&new, topics) else {
            return Ok(all);
        };
        batch = source.logs(&child_filter, block, block).await?;
    }
}

/// COR-5: a single block whose topic0-only union fetch broke the provider's `getLogs` cap. Recover it
/// address-filtered if we can, and quarantine the factory nests that forced the wide filter if we
/// cannot. Exactly one of those happens - the cursor never both recovers and faults, and never neither.
///
/// The wide fetch cannot be reissued (the range is one block, so there is nothing left to shrink) and
/// the quarantine it used to take instead is **terminal by design**: re-admission would re-issue the
/// identical fetch against the identical block and fail identically. That reasoning is sound only while
/// the fetch stays identical, and it does not have to. `getLogs` was asked for *any address* because one
/// nest is a factory; asking again for `base ∪ discovered children` is a strictly narrower question the
/// same provider may well answer, and it is the question the backfill path has always asked
/// ([`backfill_direct_factory`] §3). A busy chain with a common template topic0 is the ordinary case for
/// a factory nest, so "dead until a human intervenes" was the common outcome, not the exotic one.
///
/// **Why one narrowed union rather than pulling the factory nest onto its own fetch.** Either would
/// work. A per-nest fetch multiplies request volume per block by the number of nests, which the cursor's
/// pacing budget has to absorb on precisely the blocks that are already the heaviest; and it would give
/// one nest a second cursor in all but name. The narrowed union keeps one fetch for the whole cursor -
/// the RFC-0012 density win - and static co-tenants keep being served by the very same call, so the
/// property that they carry on indexing is preserved structurally rather than re-argued.
///
/// The cost is honest and bounded: on a block that breaches, the cursor pays a failed wide fetch plus
/// the narrowed one (plus a round per nesting depth, capped at 3). It does **not** latch - the next
/// window goes wide again, because the wide fetch is the discovery-complete one and a cap breach on one
/// block is not evidence about the next. If that ever shows up in the pacing budget, latching with
/// hysteresis is the fix, not a per-nest cursor.
#[allow(clippy::too_many_arguments)]
async fn recover_over_cap_block(
    source: &dyn Source,
    nests: &mut [Option<NestIngest>],
    nexts: &mut [u64],
    sup: &mut Supervisor,
    live: &[usize],
    factories: &[usize],
    topics: &[String],
    block: u64,
    tip: u64,
    cause: &anyhow::Error,
) -> Result<()> {
    let narrowed = narrowed_union_addresses(live.iter().map(|&i| live_ref(nests, i)));
    // Empty means "any address" to `getLogs` - the identical fetch, so there is nothing to try.
    if !narrowed.is_empty() {
        match refetch_address_filtered(source, nests, live, &narrowed, topics, block).await {
            Ok(logs) => {
                tracing::warn!(
                    "block {block} broke the provider's getLogs cap topic0-only; refetched it \
                     address-filtered ({} addresses, {} logs) and carried on",
                    narrowed.len(),
                    logs.len()
                );
                return fan_out_window(source, nests, nexts, sup, live, &logs, block, tip).await;
            }
            // The narrowed fetch is over the cap too (a factory with more children than one block's
            // cap allows), or the provider rejected the filter itself (some cap the number of
            // addresses). Either way the fallback is spent; fall through and fault the factories.
            Err(e) => {
                tracing::warn!("block {block}: the address-filtered fallback failed as well: {e:#}")
            }
        }
    }
    for &i in factories {
        // Terminal: everything this cursor can ask the provider has now been asked, and every retry
        // re-asks it. A retryable fault would spin at the backoff ceiling forever and bury the one
        // thing that does move: an operator. Name what they must do.
        let fault = anyhow::Error::new(TerminalFault(format!(
            "{}, and an address-filtered refetch of it did not fit either - raise the provider's \
             getLogs cap, or narrow this nest's topic0 set with an `events` allowlist: {cause:#}",
            single_block_over_cap(block)
        )));
        sup.quarantine(i, &fault)?;
    }
    Ok(())
}

/// A fault that will re-fail identically on retry (RFC-0026 §3), so the unit it kills is quarantined
/// **until restart** rather than backed off and re-admitted: a reorg below the sealed watermark bails
/// again on the next attempt by construction, and a dead IVM circuit thread cannot be revived
/// in-process. Retrying either is a busy-loop that spams the log and hides the operator's real job.
/// Carried through `anyhow` and recognised by downcast, so the bail sites stay one-liners.
#[derive(Debug)]
pub struct TerminalFault(pub String);

impl std::fmt::Display for TerminalFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TerminalFault {}

/// Whether any link in the error chain is a [`TerminalFault`] - i.e. a retry is pointless. Everything
/// else is assumed retryable: a transient store/RPC/webhook error that a later window may well survive.
fn is_terminal(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<TerminalFault>())
}

/// Tags a [`NestIngest::prepare`] failure as "the chain wasn't reachable yet", not a real fault (#510).
/// `prepare`'s cold-start tip lookups used to be a bare `?`: a fully dead RPC pool killed the whole
/// solo `dev` process moments after `serve` had already logged "API live", and with the process gone
/// `/ready` never got a chance to report `stalled` either - the exact tolerance the steady-state tip
/// loop already gives a dead pool never applied to the one-time lookup that runs before it. Recognised
/// by downcast (same idiom as [`TerminalFault`]) so [`prepare_retrying`] can retry *only* this failure
/// and still let any other `prepare` error (corrupt state, a dead IVM thread, …) fail loudly as before.
#[derive(Debug)]
struct ColdStartUnreachable(String);

impl std::fmt::Display for ColdStartUnreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ColdStartUnreachable {}

fn is_cold_start_unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<ColdStartUnreachable>())
}

/// First backoff before a quarantined nest is re-admitted, doubling per attempt (RFC-0026 §4).
const QUARANTINE_BACKOFF_START_SECS: u64 = 5;
/// Ceiling on that backoff: an operator who restarts a wedged endpoint sees recovery within minutes.
const QUARANTINE_BACKOFF_MAX_SECS: u64 = 300;

/// Seconds to wait before re-admitting a nest quarantined `attempts` times (0 = the first quarantine).
/// Doubles from [`QUARANTINE_BACKOFF_START_SECS`], capped at [`QUARANTINE_BACKOFF_MAX_SECS`]. The cap
/// matters more than the curve: re-admission makes the whole cursor re-fetch the range the quarantined
/// nest missed (§4), so it is correct but not free.
fn quarantine_backoff_secs(attempts: u32) -> u64 {
    QUARANTINE_BACKOFF_START_SECS
        .saturating_mul(1u64 << attempts.min(6))
        .min(QUARANTINE_BACKOFF_MAX_SECS)
}

/// One nest's standing in the shared cursor's working set (RFC-0026 §3).
#[derive(Debug)]
enum NestState {
    /// Driving the cursor: counted in the min/max/union below.
    Live,
    /// Removed from the working set. `retry_at` is `None` for a terminal fault (quarantined until
    /// restart); `Some(t)` for a retryable one, re-admitted once the monotonic clock passes `t`.
    Quarantined {
        reason: String,
        retry_at: Option<std::time::Instant>,
    },
    /// Removed by the **operator**, not by a fault (RFC-0027 §6). Never re-admitted, never counted as
    /// a failure, and never a reason to report the runtime unready.
    ///
    /// Kept distinct from `Quarantined` rather than modelled as a terminal fault, because the two mean
    /// opposite things to anyone watching: a terminal quarantine says "something broke, come and
    /// look", while a retirement says "you asked for this". Conflating them would page an operator for
    /// doing exactly what they intended, and - worse - would make a runtime whose last nest was unmounted
    /// exit non-zero as though every nest had died.
    Retired,
}

/// The shared cursor's supervision state: who is live, who is quarantined, and what the outside world
/// is told about it (RFC-0026). Bundled into one place because the pieces must move together - a
/// quarantine has to update the working set, the backoff bookkeeping, and the health surface in one
/// step, and three loose parallel vectors made it far too easy to update two of the three.
struct Supervisor {
    names: Vec<String>,
    states: Vec<NestState>,
    /// Consecutive quarantines per nest, driving the backoff curve. Deliberately **not** a field of
    /// [`NestState::Quarantined`]: re-admission moves a nest back to `Live`, which would discard the
    /// count and leave a flapping nest forever backing off the opening 5 s. RFC-0026 §4 resets it on a
    /// *committed window* - real progress - not on the mere act of being let back in.
    attempts: Vec<u32>,
    /// Whether a nest has a *valid* cursor. A nest whose `prepare` failed has `nexts[i] = 0`, which is
    /// not "start at genesis" but "unknown" - it must re-`prepare` before it may rejoin the working set.
    prepared: Vec<bool>,
    /// The live health surface the API reads (RFC-0026 §5).
    health: Arc<crate::health::RuntimeHealth>,
    /// Fail-stop instead of quarantine (§6): exit on the first fault of any kind. Off by default;
    /// restores the pre-RFC-0026 behaviour for CI, deterministic tests, and operators who want it.
    fail_fast: bool,
}

impl Supervisor {
    fn new(names: Vec<String>, health: Arc<crate::health::RuntimeHealth>, fail_fast: bool) -> Self {
        let n = names.len();
        Self {
            names,
            states: (0..n).map(|_| NestState::Live).collect(),
            attempts: vec![0; n],
            prepared: vec![false; n],
            health,
            fail_fast,
        }
    }

    /// The nests still driving the shared cursor.
    ///
    /// Quarantine means **removal from the working set**, not a skip-flag on an iteration, and this is
    /// the subtlety the whole design turns on (RFC-0026 §3.1). The cursor derives `global_next` from the
    /// *min* of the live nests' cursors, its reorg reference from the *max*, and its `getLogs` filter
    /// from the *union* of their addresses/topics. A quarantined nest left in the min pins the shared
    /// cursor at its dead position - every healthy sibling stalls while the runtime still reports itself
    /// alive, which is strictly worse than the crash this replaces.
    fn live(&self) -> Vec<usize> {
        self.states
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, NestState::Live))
            .map(|(i, _)| i)
            .collect()
    }

    /// Quarantine one nest, logging why and publishing it to the health surface. The attempt count
    /// carries across repeated faults so the backoff keeps doubling for a nest that keeps failing;
    /// real progress resets it ([`Supervisor::mark_progress`]).
    ///
    /// Returns `Err` under `--fail-fast`, which the caller propagates to end the cursor.
    fn quarantine(&mut self, i: usize, e: &anyhow::Error) -> Result<()> {
        let name = self.names[i].clone();
        let reason = format!("{e:#}");
        if self.fail_fast {
            anyhow::bail!("--fail-fast: nest '{name}' faulted: {reason}");
        }
        if is_terminal(e) {
            tracing::warn!(
                "nest '{name}' quarantined (terminal - needs an operator, no retry): {reason}"
            );
            self.health
                .quarantine_nest(&name, reason.clone(), self.attempts[i], None);
            self.states[i] = NestState::Quarantined {
                reason,
                retry_at: None,
            };
        } else {
            let wait = quarantine_backoff_secs(self.attempts[i]);
            tracing::warn!("nest '{name}' quarantined (retrying in {wait}s): {reason}");
            self.health
                .quarantine_nest(&name, reason.clone(), self.attempts[i], Some(wait));
            self.attempts[i] = self.attempts[i].saturating_add(1);
            self.states[i] = NestState::Quarantined {
                reason,
                retry_at: Some(std::time::Instant::now() + std::time::Duration::from_secs(wait)),
            };
        }
        Ok(())
    }

    /// Record real progress for a nest: it is indexing, and its backoff record is cleared (§4).
    fn mark_progress(&mut self, i: usize) {
        self.attempts[i] = 0;
        self.health.mark_indexing(&self.names[i]);
    }

    /// Re-admit every quarantined nest whose backoff has elapsed. Re-admission is safe by construction
    /// (RFC-0026 §4): the nest rejoins with its cursor *unchanged* - i.e. behind - which pulls
    /// `global_next` back down to it, and the siblings that ran ahead skip the re-fetched windows via
    /// the loop's `nexts[i] > to` guard. No nest re-processes a committed window; no nest skips one.
    fn readmit_due(&mut self, now: std::time::Instant) {
        for i in 0..self.states.len() {
            if let NestState::Quarantined {
                retry_at: Some(t), ..
            } = &self.states[i]
            {
                if now >= *t {
                    tracing::warn!("nest '{}' re-admitted to the shared cursor", self.names[i]);
                    self.states[i] = NestState::Live;
                    // Still not *indexing* until it commits a window, but it is no longer waiting on a
                    // backoff - so drop the stale `next_retry_unixtime` an operator would be watching.
                    self.health.mark_indexing(&self.names[i]);
                }
            }
        }
    }

    /// Whether every nest is quarantined with no retry pending - the cursor is dead (§6).
    /// Whether every *remaining* nest is terminally quarantined - i.e. the cursor is dead rather than
    /// merely idle.
    ///
    /// Retired nests are excluded from the question entirely (RFC-0027 §6). A cursor whose nests were
    /// all unmounted has nothing to do, but nothing has *failed*; treating that as "every nest is
    /// terminally quarantined" would make the runtime exit non-zero the moment an operator removed the
    /// last nest - killing the process they were about to mount the replacement into.
    fn all_terminal(&self) -> bool {
        let mut saw_fault = false;
        for s in &self.states {
            match s {
                NestState::Retired => continue,
                NestState::Quarantined { retry_at: None, .. } => saw_fault = true,
                _ => return false,
            }
        }
        saw_fault
    }

    /// Whether every nest has been retired by an operator - the cursor has no work and no fault.
    fn all_retired(&self) -> bool {
        !self.states.is_empty() && self.states.iter().all(|s| matches!(s, NestState::Retired))
    }

    /// Retire a nest at the operator's request (RFC-0027 §6). Idempotent, and deliberately refuses to
    /// resurrect: a retired nest is never re-admitted by [`Supervisor::readmit_due`].
    fn retire(&mut self, i: usize) {
        if matches!(self.states[i], NestState::Retired) {
            return;
        }
        self.states[i] = NestState::Retired;
        self.health.retire_nest(&self.names[i]);
        tracing::info!(
            "nest '{}' retired from this cursor at the operator's request",
            self.names[i]
        );
    }

    /// Admit a newly-mounted nest to the working set (RFC-0027 §3), returning its index.
    ///
    /// `prepared` is set: the nest was `prepare`d by the driver before being sent, so it already has a
    /// valid cursor. Marking it unprepared would make the loop re-`prepare` it and, worse, treat its
    /// `nexts` entry as "unknown" - which the cursor reads as genesis and would drag every co-tenant
    /// back with it (the trap RFC-0026 §3.1 documents).
    fn admit(&mut self, name: &str) -> usize {
        self.names.push(name.to_string());
        self.states.push(NestState::Live);
        self.attempts.push(0);
        self.prepared.push(true);
        self.health.mark_indexing(name);
        self.names.len() - 1
    }

    /// The index of a nest by name, for a lifecycle command naming one.
    fn index_of(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|n| n == name)
    }

    /// Every quarantine reason, for the cursor's own death notice.
    fn reasons(&self) -> Vec<String> {
        self.states
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s {
                NestState::Quarantined { reason, .. } => {
                    Some(format!("{}: {reason}", self.names[i]))
                }
                // A retirement is not a reason the cursor died - it is a reason it has less to do.
                NestState::Live | NestState::Retired => None,
            })
            .collect()
    }
}

/// Fan one detected reorg out to every live nest, quarantining any that cannot handle it.
///
/// A rollback failure is that nest's fault alone - overwhelmingly the finality-violation bail, which
/// is terminal for the nest whose *own* sealed watermark the fork went under. Co-tenants seal on their
/// own watermarks (a nest that sealed less far is perfectly repairable), so they roll back and carry
/// on. This is the headline case from issue #147: before RFC-0026 the first nest to bail here killed
/// the cursor mid-fan-out, leaving its siblings rolled back but never advanced again.
/// A live nest's ingest state, by index.
///
/// A slot is `None` only after the nest was retired (RFC-0027 §6), and retirement removes it from
/// `Supervisor::live()` in the same breath - so every index derived from the live set is present.
/// Panicking here would mean those two fell out of step, which is a logic error rather than a
/// condition to handle: silently skipping would make a nest stop indexing with no diagnosis.
/// Start one circuit per authored entity the nest declares (RFC-0041 §5.1).
///
/// Every failure here is fatal, and that is the point of doing it at startup. An entity whose SQL
/// will not lower, or which names a column this nest's ABI does not have, is a nest that would
/// otherwise start cleanly and then serve an empty relation for as long as nobody looked - §5.1's
/// "it never serves a plausible partial relation as current", applied to the case where the relation
/// was never going to exist at all.
fn start_entities(
    dir: &std::path::Path,
    registry: &Arc<DecodeRegistry>,
    warm: bool,
) -> Result<Vec<EntityView>> {
    let declared = crate::entities::load(dir)?;
    if declared.is_empty() {
        return Ok(Vec::new());
    }
    let mut views = Vec::with_capacity(declared.len());
    for decl in declared {
        let (plan, columns) = crate::entity_lower::lower_with_columns(&decl.sql)
            .with_context(|| format!("lowering entity `{}`", decl.name))?;
        // An entity that shadows a decoded table would silently take that table's name on the
        // analytical surface, so `SELECT * FROM usdc__transfer` would answer from a maintained
        // relation instead of the facts. Refused at load, where it is a typo, rather than at the
        // first query, where it is a mystery.
        if let Some(t) = registry.schema().iter().find(|t| t.table == decl.name) {
            anyhow::bail!(
                "entity `{}` has the same name as the decoded table `{}`. Rename the entity: on the \
                 SQL surface one would shadow the other",
                decl.name,
                t.table
            )
        }
        views.push(EntityView::start(
            &decl.name,
            &plan,
            &columns,
            registry,
            decl.max_rows,
            warm,
        )?);
        tracing::info!(
            "entity `{}` maintained incrementally, bound to max_rows {}",
            decl.name,
            decl.max_rows
        );
    }
    Ok(views)
}

fn live_nest(nests: &mut [Option<NestIngest>], i: usize) -> &mut NestIngest {
    nests[i]
        .as_mut()
        .expect("a live index must have an ingest state; retirement clears both together")
}

/// Shared-reference twin of [`live_nest`].
fn live_ref(nests: &[Option<NestIngest>], i: usize) -> &NestIngest {
    nests[i]
        .as_ref()
        .expect("a live index must have an ingest state; retirement clears both together")
}

fn fan_out_rollback(
    nests: &mut [Option<NestIngest>],
    nexts: &mut [u64],
    sup: &mut Supervisor,
    live: &[usize],
    ancestor: u64,
) -> Result<()> {
    for &i in live {
        match live_nest(nests, i).rollback_reorg(ancestor) {
            Ok(()) => nexts[i] = nexts[i].min(ancestor + 1),
            Err(e) => sup.quarantine(i, &e)?,
        }
    }
    Ok(())
}

/// Hand each live nest exactly the logs it owns within its own un-processed range, through the same
/// per-window path a solo nest runs. A nest already past this window is skipped; a nest with zero owned
/// logs still advances + checkpoints + seals (identical to solo - a window with no matching logs still
/// moves the cursor).
///
/// The `logs` may come from the union fetch or, when that fetch broke the provider's cap, from the
/// address-filtered refetch ([`refetch_address_filtered`]) - the fan-out is the same either way, which
/// is the point of it being one function: the recovery path must not become a second, divergent way of
/// committing a window.
#[allow(clippy::too_many_arguments)]
async fn fan_out_window(
    source: &dyn Source,
    nests: &mut [Option<NestIngest>],
    nexts: &mut [u64],
    sup: &mut Supervisor,
    live: &[usize],
    logs: &[crate::rpc::Log],
    to: u64,
    tip: u64,
) -> Result<()> {
    for &i in live {
        if nexts[i] > to {
            continue;
        }
        let nest_logs: Vec<crate::rpc::Log> = logs
            .iter()
            .filter(|l| l.block_number >= nexts[i] && live_ref(nests, i).owns(l))
            .cloned()
            .collect();
        // `Some(_)` → committed, advance this nest past the window. `None` → timestamps were
        // unavailable, so leave its cursor put: `global_next` (the min) stays here, the next
        // iteration re-fetches, and this nest retries while nests that did advance simply
        // process the forward remainder - never re-processing.
        //
        // An `Err` is this nest's fault alone (decode, store, seal, a dead IVM circuit, a
        // webhook sink): quarantine it and let its co-tenants finish the window. Before
        // RFC-0026 this `?` killed the shared cursor, taking every healthy sibling with it.
        match live_nest(nests, i)
            .process_window(source, &nest_logs, nexts[i], to, tip)
            .await
        {
            Ok(Some(_)) => {
                nexts[i] = to + 1;
                // Real progress clears the nest's record, so a nest that fails every third window
                // is degraded, not escalating towards permanent quarantine (RFC-0026 §4).
                // Re-admission alone must NOT reset this.
                sup.mark_progress(i);
            }
            Ok(None) => {}
            Err(e) => sup.quarantine(i, &e)?,
        }
    }
    Ok(())
}

/// The shared cursor (RFC-0012 slice 2a): one poll drives every mounted nest. One `source.tip()`, one
/// union `getLogs` per window, then each returned log is demuxed to the nest(s) that own it and run
/// through the SAME [`NestIngest::process_window`] a solo `dev` uses - so per-nest tables are
/// byte-identical to running that nest alone. Backfill stays per-nest (each `prepare`s its own history
/// first); the cursor only couples nests at the tip. Reorg is detected ONCE at the shared boundary and
/// fanned out to every nest (slice 3). Factory nests are supported (slice 2b): if any is mounted the
/// union fetch goes topic0-only and each nest demuxes by `owns` - address for static, topic0 for factory.
#[allow(clippy::too_many_arguments)]
/// Apply every lifecycle command waiting on the channel, without blocking.
///
/// Non-blocking on purpose: the cursor's job is to index, and it checks for work rather than waiting
/// for it. `try_recv` drains whatever arrived since the last window and returns immediately when the
/// queue is empty, so an idle lifecycle channel costs one atomic load per window.
///
/// A command naming a nest this cursor does not host is logged and dropped rather than treated as an
/// error - with one cursor per chain, a runtime-level command may legitimately reach the wrong cursor.
fn drain_lifecycle(
    lifecycle: &mut Option<CursorCommands>,
    sup: &mut Supervisor,
    nests: &mut Vec<Option<NestIngest>>,
    nexts: &mut Vec<u64>,
) {
    let Some(rx) = lifecycle.as_mut() else {
        return;
    };
    while let Ok(cmd) = rx.try_recv() {
        match cmd {
            CursorCommand::Unmount { name, ack } => {
                match sup.index_of(&name) {
                    // Retire *and release*: dropping the `NestIngest` drops this cursor's `Store`
                    // clone, its view handles and its screener. redb only lets go of the file when
                    // every clone has, so this is one of three that must (RFC-0027 §6).
                    Some(i) => {
                        sup.retire(i);
                        nests[i] = None;
                    }
                    None => {
                        tracing::debug!("unmount for '{name}' is not for this cursor; ignoring")
                    }
                }
                // Acknowledge either way. A command for another cursor is still "done" as far as this
                // one is concerned, and leaving the driver waiting on a nest we do not host would
                // hang the unmount forever.
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
            CursorCommand::Mount { nest, next, ack } => {
                let name = nest.name.clone();
                if sup.index_of(&name).is_some() {
                    // Mounting over a live name is an upgrade, and that is RFC-0020's job. Refusing
                    // here keeps the two from silently overlapping.
                    tracing::warn!("nest '{name}' is already on this cursor; ignoring the mount");
                } else {
                    // The three arrays stay index-aligned - that invariant is what `live_nest`'s
                    // `expect` relies on, so they are grown together and never separately.
                    nests.push(Some(*nest));
                    nexts.push(next);
                    sup.admit(&name);
                    tracing::info!("nest '{name}' mounted onto this cursor at block {next}");
                }
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
            }
        }
    }
}

/// A lifecycle command for a *running* cursor (RFC-0027 §2).
///
/// The cursor owns its nest set; the outside world sends it commands. Every mutation arrives this way
/// and is applied at a **window boundary**, never from an HTTP handler mid-window - because the cursor
/// computes `global_next` as a min over its nests, detects reorgs once, and fans rollback out to every
/// one of them. Mutating the set underneath that would produce a rollback applied to a nest that was
/// not present for the roll forward.
// Not `Clone`/`Eq`: the acknowledgement is a `oneshot::Sender`, which is neither - and should not be.
// A lifecycle command is consumed exactly once, by exactly one cursor.
pub enum CursorCommand {
    /// Retire a nest from this cursor's working set at the operator's request, then **release
    /// everything the cursor holds for it** - its store handle above all.
    ///
    /// `ack` fires once that is done, and the ordering it enforces is the point of the handshake:
    /// the driver must not remove the nest's routes until the cursor has finished with it (RFC-0027
    /// §6, drain-then-remove). It also tells the driver *when* the cursor's `Store` clone is gone,
    /// which matters because redb only releases the file once every clone drops - the cursor's, the
    /// serving state's, and the alert worker's.
    ///
    /// A dropped `ack` sender is not an error: the driver may have stopped caring, and the cursor's
    /// job is done either way.
    Unmount {
        name: String,
        ack: Option<tokio::sync::oneshot::Sender<()>>,
    },
    /// Admit a nest to this cursor's working set, already prepared and caught up.
    ///
    /// The catch-up happens **before** the command is sent (RFC-0027 §4, phase 1): the driver builds
    /// and `prepare`s the nest off to one side, so what arrives here is a nest whose cursor is already
    /// near the shared one. That matters because the cursor advances from the *min* of its live nests -
    /// splicing in a nest that is a million blocks behind would drag every co-tenant back through
    /// history with it. Correct, but unusable, which is why phase 1 exists.
    ///
    /// `next` is the block the nest resumes at, from its own `prepare`.
    Mount {
        nest: Box<NestIngest>,
        next: u64,
        ack: Option<tokio::sync::oneshot::Sender<()>>,
    },
}

impl CursorCommand {
    /// An unmount with no acknowledgement - for callers that do not need to know when the cursor has
    /// let go (tests, and any fire-and-forget path).
    pub fn unmount(name: impl Into<String>) -> Self {
        CursorCommand::Unmount {
            name: name.into(),
            ack: None,
        }
    }
}

// Hand-written rather than derived: a mount carries a whole `NestIngest` (stores, decode registry,
// view handles), and printing that in a log line would be both enormous and useless. The name is the
// part anyone debugging a lifecycle command actually wants.
impl std::fmt::Debug for CursorCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorCommand::Unmount { name, .. } => write!(f, "Unmount({name})"),
            CursorCommand::Mount { nest, next, .. } => {
                write!(f, "Mount({} at {next})", nest.name)
            }
        }
    }
}

pub type CursorCommands = tokio::sync::mpsc::UnboundedReceiver<CursorCommand>;

#[allow(clippy::too_many_arguments)]
async fn runtime_index_loop(
    source: Arc<dyn Source>,
    nests: Vec<NestIngest>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window: u64,
    health: Arc<crate::health::RuntimeHealth>,
    fail_fast: bool,
    mut lifecycle: Option<CursorCommands>,
) -> Result<()> {
    if nests.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = nests.iter().map(|n| n.name.clone()).collect();
    // Optioned so a retirement can *drop* the nest's ingest state - and with it this cursor's `Store`
    // clone, which redb needs released before the file is free (RFC-0027 §6).
    let mut nests: Vec<Option<NestIngest>> = nests.into_iter().map(Some).collect();
    let mut sup = Supervisor::new(names, health, fail_fast);

    // Phase 0, per nest: each nest backfills its own history to near-tip independently (tip-only
    // coupling - the shared cursor never entangles backfill windows). Each returns its own start cursor.
    // A failure here quarantines that nest (RFC-0026 §3) rather than stillbirthing the cursor: before,
    // one nest's backfill error killed the shared task before a single sibling indexed a block.
    let mut nexts: Vec<u64> = vec![0; nests.len()];
    for (i, slot) in nests.iter_mut().enumerate() {
        // Every slot is `Some` here - nothing has been retired before the loop starts.
        let nest = slot
            .as_mut()
            .expect("no nest can be retired before the cursor begins");
        match nest
            .prepare(source.as_ref(), backfill, seal_direct, concurrency, window)
            .await
        {
            Ok(next) => {
                nexts[i] = next;
                sup.prepared[i] = true;
            }
            Err(e) => sup.quarantine(i, &e)?,
        }
    }

    let mut chunker = AdaptiveWindow::for_window(window);
    let mut poll_failures = 0u32;
    // Same periodic "at tip / N behind" restatement as the solo loop (issue #302), against the
    // cursor's shared `global_next` - the position every co-tenant on this chain has cleared.
    let mut heartbeat = crate::progress::TipHeartbeat::new();
    loop {
        // Apply any lifecycle commands *here* - the top of an iteration, between windows, which is the
        // only point at which the nest set is quiescent and "every live nest has committed the same
        // windows" holds (RFC-0027 §2).
        drain_lifecycle(&mut lifecycle, &mut sup, &mut nests, &mut nexts);
        if sup.all_retired() {
            // Every nest was unmounted by the operator. Nothing left to advance, and nothing wrong -
            // so this returns cleanly rather than bailing, and the runtime stays up (RFC-0027 §6).
            tracing::info!("every nest on this cursor has been unmounted; retiring the cursor");
            return Ok(());
        }
        // Re-admit anything whose backoff elapsed, then take the live set for this iteration. Every
        // min/max/union below is derived from it - never from all nests (RFC-0026 §3.1).
        sup.readmit_due(std::time::Instant::now());
        // A nest quarantined *during* `prepare` never established a cursor, so it re-`prepare`s before
        // rejoining. Without this it would rejoin at `nexts[i] == 0` and, being the new minimum, drag
        // the whole shared cursor back to genesis - re-indexing every co-tenant from block 0.
        // Indexes four parallel arrays (`nests`, `nexts`, `sup.states`, `sup.prepared`) that must stay
        // in step, so an index loop says what it means; enumerating one of them would obscure that.
        #[allow(clippy::needless_range_loop)]
        for i in 0..nests.len() {
            if matches!(sup.states[i], NestState::Live) && !sup.prepared[i] {
                match live_nest(&mut nests, i)
                    .prepare(source.as_ref(), backfill, seal_direct, concurrency, window)
                    .await
                {
                    Ok(next) => {
                        nexts[i] = next;
                        sup.prepared[i] = true;
                        sup.mark_progress(i);
                    }
                    Err(e) => sup.quarantine(i, &e)?,
                }
            }
        }
        let live = sup.live();
        if live.is_empty() {
            // Nothing left to advance. If every quarantine is terminal the cursor is dead and says so
            // (slice 2 turns this into a per-cursor quarantine at the runtime driver); if some are
            // retryable, wait for the backoff rather than spin.
            if sup.all_terminal() {
                anyhow::bail!(TerminalFault(format!(
                    "every nest on this cursor is terminally quarantined - {}",
                    sup.reasons().join("; ")
                )));
            }
            sleep_secs(3).await;
            continue;
        }

        let tip = match source.tip().await {
            Ok(t) => {
                poll_failures = 0;
                // Publish to **each nest on this cursor**, not only to the process-global gauge. With
                // one cursor per chain, a single global tip means whichever cursor polled last wins -
                // and `/<nest>/ready` then answers with another chain's block height. Observed live in
                // a two-chain mounts: the mainnet nest reported an Arbitrum tip.
                for &i in &live {
                    live_ref(&nests, i).metrics.mark_poll_ok();
                }
                t
            }
            Err(e) => {
                for &i in &live {
                    live_ref(&nests, i).metrics.mark_poll_failed();
                }
                poll_failures = escalate_stall(poll_failures, &e);
                sleep_secs(3).await;
                continue;
            }
        };
        for &i in &live {
            live_ref(&nests, i).metrics.set_tip(tip);
        }

        // Shared reorg detection + fan-out (RFC-0012 slice 3). A reorg is a chain event every nest at
        // the tip is exposed to identically, and all caught-up nests checkpoint the same boundaries with
        // the same hashes - so detect ONCE, at the most-caught-up nest's boundary, then fan the rollback
        // out to every nest. This is one detection (a handful of block-hash calls) instead of N, and one
        // observable reorg boundary. `rollback_reorg` is a no-op for any nest already at/below the fork
        // (a still-backfilling nest below finality can't be affected), so fanning to all is safe.
        // Only live nests take part: a quarantined nest is not a valid reorg reference (its store
        // stopped advancing) and must not be rolled back (RFC-0026 §3.1).
        let max_next = live.iter().map(|&i| nexts[i]).max().unwrap();
        if max_next > 0 {
            // Any caught-up nest is a valid checkpoint reference; use one at the max height.
            let reference = *live.iter().find(|&&i| nexts[i] == max_next).unwrap();
            match detect_reorg(
                source.as_ref(),
                &live_ref(&nests, reference).store,
                max_next - 1,
            )
            .await
            {
                Ok(Some(ancestor)) => {
                    tracing::warn!(
                        "mounts reorg to block {ancestor}: rolling back every live nest"
                    );
                    fan_out_rollback(&mut nests, &mut nexts, &mut sup, &live, ancestor)?;
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::debug!("mounts reorg check skipped: {e:#}"),
            }
        }

        // The shared cursor advances from the *least* caught-up live nest, so no nest ever skips a block.
        let global_next = live.iter().map(|&i| nexts[i]).min().unwrap();
        heartbeat.maybe_log(global_next, tip);
        if global_next > tip {
            sleep_secs(2).await;
            continue;
        }
        // A blocks nest pays one header request per block, so a window that is cheap in logs can still
        // be ruinous in headers (RFC-0036). The backfill paths pick their controller once, from a
        // single registry; a cursor's nest set changes under it as mounts arrive and retire, so the
        // ceiling is re-derived every iteration instead - a blocks nest mounted an hour in must not
        // inherit a window that grew to `MAX_WINDOW` while only log-shaped nests were live.
        //
        // `set_max` bounds the controller itself, not only the span this iteration is allowed to
        // issue (#458). Capping only the use left `chunker` fed `observed` results from an
        // `HEADER_WINDOW_CAP`-wide fetch while its own `window` kept climbing toward `MAX_WINDOW`
        // unseen - so retiring the blocks nest handed back the whole drift in `window_cap`'s single
        // next-iteration jump from 800 to `MAX_WINDOW`, instead of the controller's own gradual 4x
        // growth. Calling `set_max` every iteration keeps `window` clamped down live while the blocks
        // nest is live, so the mount-late direction is unaffected and the retire direction now climbs
        // back up through `observed`'s own damping rather than in one step.
        // `MAX_WINDOW` here, not `u64::MAX`: `set_max` overwrites the controller's own ceiling
        // rather than layering an extra cap on top of it, so the "no blocks nest live" arm has to
        // name the controller's ordinary ceiling explicitly - `u64::MAX` would erase it and let a
        // plain nest's window balloon past `MAX_WINDOW` for as long as no blocks nest is mounted.
        let window_cap = if live.iter().any(|&i| live_ref(&nests, i).registry.blocks()) {
            crate::chunker::HEADER_WINDOW_CAP
        } else {
            crate::chunker::MAX_WINDOW
        };
        chunker.set_max(window_cap);
        let to = (global_next + chunker.window() - 1).min(tip);

        // Union over live nests only - a quarantined nest consumes nothing, so paying `getLogs`
        // bandwidth for its addresses is waste (and a quarantined factory nest would keep forcing the
        // whole cursor topic0-only).
        let (u_addrs, u_topics) = union_filter(live.iter().map(|&i| {
            let n = live_ref(&nests, i);
            (n.addresses.as_slice(), n.topic0s.as_slice())
        }));
        // The union of a set of nests that are all contract-free is empty on both halves, which is not
        // "no logs" but *every log on the chain* - and unlike the backfill instance of this defect, a
        // tip loop asks forever, every couple of seconds, for as long as the cursor runs (#432).
        // `LogFilter::new` is what makes that unaskable; the `None` arm is this site deciding what
        // "nothing to ask for" means, which here is an empty window that still gets fanned out to the
        // live nests rather than skipped, so the shared cursor advances in step for all of them.
        let filter = LogFilter::new(&u_addrs, &u_topics);
        let fetched = match &filter {
            Some(f) => source.logs(f, global_next, to).await,
            None => Ok(Vec::new()),
        };
        match fetched {
            Ok(logs) => {
                chunker.observed(logs.len() as u64);
                fan_out_window(
                    source.as_ref(),
                    &mut nests,
                    &mut nexts,
                    &mut sup,
                    &live,
                    &logs,
                    to,
                    tip,
                )
                .await?;
            }
            Err(e) if narrowing_can_help(&e, global_next, to) => {
                if global_next >= to {
                    // COR-5. One block is over the provider's cap and there is no narrower range to
                    // ask for, so this fetch cannot succeed as issued.
                    //
                    // It used to `return Err`, which ends `runtime_index_loop` - and that task drives
                    // *every* nest on the cursor, so one nest's topic0 stopped its co-tenants dead.
                    // RFC-0026 exists to make exactly that impossible, and this path went around it
                    // because a union fetch has no owner: the error is not attributable to a nest a
                    // priori, so there was nobody to quarantine.
                    //
                    // It is attributable in one direction, though. `union_filter` drops the address
                    // filter only when a live nest is a factory nest, so a topic0-only union is the
                    // factory nests' doing and a static nest's addresses cannot have caused it. They
                    // are the nests `recover_over_cap_block` narrows for, and the ones it faults if
                    // the narrowed fetch cannot clear the cap either.
                    let factories =
                        topic0_only_culprits(live.iter().map(|&i| (i, live_ref(&nests, i))));
                    if factories.is_empty() {
                        // No factory nest, so the union carried real addresses and narrowing the
                        // filter is not available either. Unchanged behaviour: fail loudly.
                        return Err(e).with_context(|| single_block_over_cap(global_next));
                    }
                    recover_over_cap_block(
                        source.as_ref(),
                        &mut nests,
                        &mut nexts,
                        &mut sup,
                        &live,
                        &factories,
                        &u_topics,
                        global_next,
                        tip,
                        &e,
                    )
                    .await?;
                    continue;
                }
                chunker.too_large();
                tracing::debug!("range {global_next}..={to} refused; shrinking and retrying");
            }
            Err(e) => {
                // A refusal carrying no width information - a 429 or a 403. Retrying at the same
                // width is right: endpoint failover happens beneath this, and the growth that used
                // to walk into an unserveable width is bounded by evidence in the chunker now
                // (#672), so the width being retried is one the provider has already served.
                tracing::warn!("get_logs {global_next}..={to} failed: {e:#}; retrying");
                sleep_secs(3).await;
            }
        }
    }
}

/// Build every mounted nest and spawn ONE shared-cursor ingestion task driving them all (RFC-0012
/// slice 2). Returns the per-nest serve states (for `/<name>/…` routing), the single shared ingest
/// handle, and the nests' alert-delivery workers. Static and factory nests may be co-mounted (slice 2b):
/// a factory nest forces the union fetch topic0-only and demuxes by topic0, static nests by address.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_runtime(
    source: Arc<dyn Source>,
    nests: Vec<(String, PathBuf, Config)>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
    health: Arc<crate::health::RuntimeHealth>,
    fail_fast: bool,
) -> Result<ChainCursor> {
    let mut ingests = Vec::new();
    let mut states = Vec::new();
    let mut alert_workers: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();
    let mut window = None;
    // **One gate for this cursor** (#1024). `group_by_chain` gives one `spawn_runtime` per chain, so
    // this call *is* the cursor, and every nest built below shares the budget rather than each
    // getting its own copy of it.
    let sql_gate = serve::new_sql_gate();
    for (name, dir, config) in nests {
        let (nest, state, worker, w) = build_nest(
            &source,
            dir,
            &config,
            window_override,
            admin_enabled,
            admin_token.clone(),
            None,
            sql_gate.clone(),
        )
        .await?;
        window.get_or_insert(w);
        ingests.push(nest);
        // So `/<name>/ready` answers for THIS nest rather than the process-global poll freshness.
        let mut state = state;
        state.runtime_health = Some((name.clone(), health.clone()));
        if let Some(worker) = worker {
            alert_workers.push((name.clone(), worker));
        }
        states.push((name, state));
    }
    let window = window.unwrap_or(DEFAULT_WINDOW);
    // The cursor's command channel. The control surface that *sends* on it is slice 4; the driver
    // holds the sender meanwhile so unmount can be driven programmatically and tested.
    // Every nest on this cursor, not merely the first: a runtime hosts N nests and one of them
    // declaring an entity is enough to make `--seal-direct` wrong for the whole cursor, which is the
    // only granularity the flag has.
    for nest in &ingests {
        refuse_seal_direct_with_entities(seal_direct, nest)?;
    }
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let ingest = tokio::spawn(runtime_index_loop(
        source,
        ingests,
        backfill,
        seal_direct,
        concurrency,
        window,
        health,
        fail_fast,
        Some(rx),
    ));
    Ok(ChainCursor {
        states,
        ingest,
        alert_workers,
        lifecycle: tx,
    })
}

/// One chain cursor, plus the handles a driver needs to manage the nests on it (RFC-0027).
pub struct ChainCursor {
    /// Per-nest serving state. The driver **retains** these so it can re-compose the router without a
    /// nest when one is unmounted; previously they were moved straight into the router and lost.
    pub states: Vec<(String, serve::AppState)>,
    pub ingest: tokio::task::JoinHandle<Result<()>>,
    /// Alert delivery workers **keyed by nest**. Each holds its nest's `Store` clone, so unmounting
    /// one requires aborting exactly that worker - impossible while this was a bare `Vec` of handles
    /// with nothing tying a handle to a name.
    pub alert_workers: Vec<(String, tokio::task::JoinHandle<()>)>,
    /// Commands to this cursor, applied at window boundaries.
    pub lifecycle: tokio::sync::mpsc::UnboundedSender<CursorCommand>,
}

impl ChainCursor {
    /// Stop this cursor and release everything it holds.
    ///
    /// Takes `self` on purpose. `abort()` only *requests* cancellation: until the
    /// runtime polls the task again it still holds its `Store` clone, and `Store`
    /// is an `Arc<Database>`, so redb's file lock outlives the call. A caller that
    /// aborts and moves on has not stopped the cursor, it has asked politely.
    ///
    /// Seven e2e fixtures hand-rolled `ingest.abort()` plus a loop over the alert
    /// workers. Six survived only because they never reopen a store in the same
    /// process; `e2e_early_cutoff` does — twice, by design — and failed in CI with
    /// "Database already open. Cannot acquire lock." Awaiting each aborted handle
    /// fixed it, mutation-checked at 15/15 failures without the wait and 0/15 with
    /// it (#407).
    ///
    /// Consuming `self` makes "stopped" a state the caller cannot forget to reach,
    /// and drops the `AppState` clones as part of the contract rather than as
    /// something each caller has to remember.
    pub async fn shutdown(self) {
        self.ingest.abort();
        for (_, w) in &self.alert_workers {
            w.abort();
        }
        // Await every handle, not just ingest. A cancelled task resolves to
        // `Err(JoinError::Cancelled)`, which is the expected outcome here and not
        // a failure — what matters is that the await proves the task is no longer
        // running and its `Store` clone is dropped.
        let _ = self.ingest.await;
        for (_, w) in self.alert_workers {
            let _ = w.await;
        }
        // Explicit rather than incidental: these are the remaining `Store` clones,
        // and dropping them is the reason the file lock is actually free when this
        // returns.
        drop(self.states);
    }
}

/// Build a nest and bring it up to date **off to one side of a running cursor** - phase 1 of the
/// two-phase mount (RFC-0027 §4).
///
/// The catch-up deliberately happens here rather than after the nest joins. A cursor advances from the
/// *min* of its live nests' positions, so splicing in a nest that is far behind drags every co-tenant
/// back through history with it - correct, but unusable. By the time the caller sends
/// [`CursorCommand::Mount`], the returned `next` is already near the shared cursor.
///
/// Returns the ingest state to hand the cursor, the serving state to add to the router, its alert
/// worker (if the nest configures any sinks), and the block it resumes at.
#[allow(clippy::too_many_arguments)]
pub async fn build_and_prepare_nest(
    source: &Arc<dyn Source>,
    // A [`crate::runtime::PreparedDataset`] rather than a `PathBuf`, so the RFC-0033 §5 early cutoff
    // cannot be skipped by a call site that simply did not think of it - which is precisely how this
    // path came to re-index datasets it already had (#414). The only ways to hold one are
    // `runtime::prepare_dataset` and the explicitly-named `PreparedDataset::without_nid`.
    dataset: crate::runtime::PreparedDataset,
    config: &Config,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
    // Hot store to use instead of opening this nest's local redb (RFC-0022 slice 3). A query-FE node
    // is handed the shared store the writer is filling; `None` keeps the embedded behaviour, which is
    // every existing caller.
    store_override: Option<Arc<dyn crate::store::HotStore>>,
) -> Result<(
    NestIngest,
    serve::AppState,
    Option<tokio::task::JoinHandle<()>>,
    u64,
)> {
    let (mut nest, state, worker, window) = build_nest(
        source,
        dataset.into_dir(),
        config,
        window_override,
        admin_enabled,
        admin_token,
        store_override,
        // One nest built on its own is one cursor (#1024).
        serve::new_sql_gate(),
    )
    .await?;
    let next = nest
        .prepare(source.as_ref(), backfill, seal_direct, concurrency, window)
        .await?;
    Ok((nest, state, worker, next))
}

/// Every table this nest actually serves, not merely the ones a decoder produces.
///
/// `registry.schema()` describes the event tables and nothing else, and for a long while that was
/// the whole truth. It stopped being so when tier-3 `[[calls]]` and `[[ipfs]]` documents started
/// landing rows in tables of their own: those rows were queryable through `/sql` and completely
/// invisible everywhere the registry's list is treated as the catalogue, which is most places.
/// Measured on two real nests before this existed - `grt_total_supply` held 3,509 rows and appeared
/// in neither `/tables` nor `/schema`, and `/table/grt_total_supply` answered 404.
///
/// Three consequences, all from the one omission, which is why this is a single list rather than
/// three patches: the point-read endpoint refused a table that exists, the AI-native schema surface
/// told an agent it did not exist at all, and the `semantic.toml` drift check warned that a
/// correctly-described table "has no decoder" - a warning that fires on a correct config, which
/// teaches operators to ignore warnings.
pub fn full_schema(
    registry: &DecodeRegistry,
    config: &Config,
) -> Vec<crate::registry::TableSchema> {
    let ts = registry.timestamps();
    let mut tables = registry.schema();
    tables.extend(crate::calls::schema(&config.calls, ts));
    tables.extend(crate::ipfs::schema(&config.ipfs, ts));
    tables
}

/// Build one nest's runtime state *without* starting the tip loop: open its store, build its decode
/// registry + IVM views, run the warm-restart rebuilds, and assemble both the [`NestIngest`] the
/// ingestion loop drives and the [`serve::AppState`] the API serves - the two sharing the same view
/// handles (the API must see the same views the loop feeds). Also spawns the optional alert/webhook
/// delivery worker, and returns the effective `eth_getLogs` window. Spawning the ingestion loop is
/// the caller's job ([`spawn_nest`] today; a runtime driver tomorrow, RFC-0012). Per-nest isolation
/// (own store, own segments, own views) is the CLAUDE.md non-negotiable a runtime preserves by calling
/// this once per nest.
#[allow(clippy::too_many_arguments)]
async fn build_nest(
    // Unused by the single-nest build (which leaves spawning the tip loop to the caller); kept in the
    // signature per the RFC-0012 contract so a runtime driver can `build_nest` then `index_loop(source, …)`.
    _source: &Arc<dyn Source>,
    dir: PathBuf,
    config: &Config,
    window_override: Option<u64>,
    admin_enabled: bool,
    admin_token: Option<String>,
    store_override: Option<Arc<dyn crate::store::HotStore>>,
    // The cursor's analytical gate, shared by every nest on this cursor (#1024). Created by the
    // caller because only the caller knows the cursor boundary: `spawn_runtime` is one cursor.
    sql_gate: Arc<tokio::sync::Semaphore>,
) -> Result<(
    NestIngest,
    serve::AppState,
    Option<tokio::task::JoinHandle<()>>,
    u64,
)> {
    // RFC-0014 extraction is configured but not yet sourceable. Refuse rather than start, because the
    // failure mode of starting is the worse one: `traces`/`state_diffs` would exist, answer queries,
    // and return nothing - and an empty table is indistinguishable from "no matching rows" to whoever
    // is querying it. Better to be told the source is missing than to be quietly given zero.
    if config.extract.enabled() {
        // The volume guard runs first so a nest that is *both* unscoped and unsourced hears about the
        // scoping, which it will still need once a node exists.
        config.extract.scope_check()?;
        // Validate the decode surface anyway: a typo'd alias or malformed selector should surface now
        // rather than lying dormant until the day extraction is switched on.
        let _ = crate::calldata::CallRegistry::from_nest(&dir, config)?;
        anyhow::bail!(
            "[extract] needs an extraction source, and none is wired yet. Call traces and storage \
             diffs can only come from a colocated node (RFC-0003 ExEx); they are deliberately not \
             sourced from `debug_*` RPC. The decode, schema and scoping for them exist and your \
             config validates - what is missing is the node. Remove [extract] from nuthatch.toml to \
             start this nest on event decode alone."
        );
    }

    // **RFC-0022 slice 3b.** The hot store is resolved once, here, and nothing below this line knows
    // whether it is a local redb or a shared Postgres.
    //
    // It used to `Store::open` unconditionally *and* pass the concrete handle to the view rebuilds -
    // so a query-FE, which is handed a `store_override` and owns no cursor, still created and opened a
    // redb file in the nest directory it had no business writing to. That is why the compose FE mount
    // could not be `:ro`: it failed with `Read-only file system (os error 30)` before serving a single
    // request. The rebuild helpers already took `&dyn HotStore`; only this call site was concrete.
    //
    // One `Arc` per nest, shared by the ingest side and the serving side. They must be the *same*
    // handle - a second `Store::open` on the same file would be a second writer, which the trait's
    // contract forbids.
    let store: Arc<dyn crate::store::HotStore> = match store_override {
        Some(s) => s,
        None => Arc::new(Store::open(&dir.join(DB_FILE))?),
    };
    // The decode registry drives all contracts; the indexer decodes every declared event of every
    // contract in the nest into per-table rows.
    // Before anything reads `schema.json`: regenerate it if it is missing or stale. A hand-written
    // `nuthatch.toml` produces no schema, and the failure is silent and expensive - the schema tool
    // keeps recommending `{col}_dec` companions that the analytics layer never created (issue #241
    // item 2). Cheap and preserves authored views.
    //
    // **Never inside an identity-keyed dataset**, which is the same mistake as the query-FE redb note
    // above: writing to a directory this process does not own. `refresh_stale_artifacts` rewrites
    // `schema.json`, `llms.txt` and `.claude/skills/**`, and all three are hashed into the NID - the
    // blob's `EXCLUDE` covers only `nuthatch.redb`, `segments`, `.git` and `.DS_Store`. Its staleness
    // test is an *mtime* comparison against `nuthatch.toml`, which a checkout, an rsync or a `touch`
    // is enough to trip, so a dataset could be rewritten - and the identity its mount record claims
    // invalidated - without anyone editing a byte of it.
    //
    // Measured 2026-08-07 before this guard: a two-nest runtime started twice, with no operator action
    // between the starts, reported identity drift on the second start. Skipping here is safe because a
    // dataset reached `data/<nid>` through `migrate` or a verified bundle, so its artifacts were
    // consistent when its identity was computed; if they later are not, that is drift, and
    // `MountTable::identity_drift` is what reports it.
    //
    // `info!`, not `debug!` (#727): this fires on every start of every identity-keyed nest, and it is
    // the *only* explanation an operator ever gets for why a hand-edited `nuthatch.toml` did not move
    // `schema.json` - at `debug!` it was invisible at the default `nuthatch=info`, which is very likely
    // how the Lodestar GNS nest's `schema.json` fell behind in the first place. Not `warn!` either:
    // this is the correct, expected state on every such start, and a `warn!` on the normal path trains
    // operators to ignore warnings. `nuthatch schema --dir <dir>` is the verified recovery - `regen`
    // (`project::regen`) writes the derived artifacts unconditionally, with no identity check of its
    // own, so it applies here exactly as it would to an authored nest.
    if crate::runtime::MountTable::is_identity_keyed(&dir) {
        tracing::info!(
            "identity-keyed dataset: leaving derived artifacts alone (rewriting them would move the \
             NID its mount record claims). Hand-edited nuthatch.toml? Run `nuthatch schema --dir {}` \
             to regenerate schema.json/llms.txt/.claude/skills explicitly.",
            dir.display()
        );
    } else {
        match crate::project::refresh_stale_artifacts(&dir, config) {
            Ok(Some(what)) => tracing::info!("{what}"),
            Ok(None) => {}
            // Never fatal: a nest that cannot regenerate its artifacts can still index, and refusing
            // to start over a derived file would be a worse failure than the one being fixed.
            Err(e) => tracing::warn!("could not refresh derived artifacts (continuing): {e:#}"),
        }
    }
    let registry = Arc::new(crate::registry::from_nest(&dir, config)?);
    guard_timestamp_policy(store.as_ref(), config.nest.block_timestamps)?;
    guard_registry_identity(store.as_ref(), &hex::encode(registry.hash()))?;

    // Startup integrity pass (0.5.x hardening): quarantine any sealed segment whose bytes no longer
    // hash to their content address (disk corruption / tampering) before the view rebuild below scans
    // them. A corrupt segment reduces a table's cold data, loudly - it never crash-loops the node.
    if let Err(e) = seal::verify_and_quarantine(&dir) {
        tracing::warn!("segment integrity check failed (continuing): {e:#}");
    }

    let balances = BalanceView::start()?;
    // Labels (RFC-0008 C1) are the annotation substrate the exposure view joins against. Loaded before
    // the exposure view so it only spins up when there's actually something to track.
    let labels = Arc::new(labels::load(&dir));
    if !labels.is_empty() {
        tracing::info!(
            "loaded {} labeled address(es) for exposure tracking",
            labels.len()
        );
    }
    // The exposure view joins transfers against the labeled set - with no labels it can only ever be
    // empty, so don't spend a DBSP circuit + dedicated thread on it (deadlock-review finding L10).
    let exposure = ExposureView::start(!labels.is_empty())?;
    // Optional live sanctions screening (RFC-0008 C2). Absent unless the nest configures
    // `[screening].lists`; when present, every window's transfers are screened against the pure
    // component and `sanction_hit` annotations are stored + sealed alongside the transfers.
    let screener = Arc::new(screen::LiveScreener::from_config(
        &dir,
        &config.screening.lists,
    )?);

    // Optional threshold & velocity flags (RFC-0008 C3). Threshold flags are per-transfer stored
    // annotations (block-keyed → roll back with their transfer); velocity is a DBSP windowed view
    // (rebuilt on restart like balances/exposure).
    let threshold = config.flags.threshold_amount();
    let velocity_cfg = config.flags.velocity();
    // Only fed when a velocity flag is configured - skip its circuit + thread otherwise (L10).
    let velocity = VelocityView::start(velocity_cfg.is_some())?;
    // Authored incremental entities (RFC-0041). A declaration that will not lower or will not bind
    // stops the nest here rather than at the first block that would have used it - `?`, deliberately.
    // A nest with indexed history behind it starts its entities *unavailable* rather than empty and
    // filling: see `EntityView::start`. Read before the rebuild below, which uses the same key.
    let warm = store.get_meta(LAST_BLOCK_KEY)?.is_some();
    let mut started = start_entities(&dir, &registry, warm)?;
    // §5.3: seed a warm-started entity from this nest's own stored history - sealed segments plus the
    // unsealed hot tail - before it serves or ingests anything. No source is in scope here, which is
    // #865's "zero historical RPC calls" as a property of the signature rather than of the code.
    //
    // A failed seed leaves the entity `unavailable` and the nest unready, which is what it already
    // was: this can improve on that state or leave it alone, never make it worse.
    if warm && !started.is_empty() {
        let through = store
            .get_meta(LAST_BLOCK_KEY)?
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if let Err(e) = seed_entities(&dir, store.as_ref(), &registry, &mut started, through) {
            tracing::error!("entity seeding failed, entities stay unavailable: {e:#}");
        }
    }
    let entities = Arc::new(started);
    if threshold.is_some() || velocity_cfg.is_some() {
        tracing::info!("flags enabled: threshold={threshold:?}, velocity={velocity_cfg:?}");
    }

    // Warm restart: the derived views (balances, exposure, velocity) aren't persisted, so rebuild
    // them from stored facts before serving or ingesting. Cold start → nothing stored → no-op.
    if store.get_meta(LAST_BLOCK_KEY)?.is_some() {
        if let Err(e) = rebuild_views(
            &dir,
            store.as_ref(),
            &registry,
            &DerivedViews {
                labels: &labels,
                balances: &balances,
                exposure: &exposure,
                velocity: &velocity,
                velocity_window: velocity_cfg.map(|(_, w)| w),
            },
        ) {
            tracing::warn!("view rebuild failed (will re-derive as it indexes): {e:#}");
        }
    }

    // Factory rules (RFC-0009): validated at load. A factory nest discovers child contracts at
    // runtime, so the tip loop fetches topic0-only (empty address filter) - a child created and
    // traded in the same block is then already in hand, no extra RPC.
    let factory = {
        let fs = FactorySet::build(config)?;
        if fs.is_empty() {
            None
        } else {
            tracing::info!(
                "factory nest: {} template(s), {} rule(s) - topic0-only tip fetch, children discovered at runtime",
                config.templates.len(),
                config.factories.len()
            );
            if let Some(chain) = crate::chains::lookup(&config.nest.chain) {
                if !chain.topic0_only_getlogs {
                    tracing::warn!(
                        "chain '{}' shipped RPC refuses address-less eth_getLogs (the factory \
                         flip after {FACTORY_FLIP_THRESHOLD} children, and the tip fetch from \
                         the first window). A factory nest on the default endpoint will fail \
                         with an address-required error. Pass --rpc at an archive endpoint that \
                         allows topic0-only getLogs.",
                        chain.name
                    );
                }
            }
            Some(Arc::new(fs))
        }
    };

    // The combined `eth_getLogs` filter: contract addresses (empty for a factory nest → topic0-only),
    // matching any registered topic0 (contract + template events).
    let addresses: Vec<String> = if factory.is_some() {
        Vec::new()
    } else {
        registry
            .addresses()
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect()
    };
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();

    // Per-chain policy from the registry; a custom (unregistered) chain falls back to defaults.
    let (finality, chain_window) = match chains::lookup(&config.nest.chain) {
        Some(c) => (c.finality, c.log_window),
        None => (DEFAULT_FINALITY, DEFAULT_WINDOW),
    };
    // A `--window` override wins over the chain default (for sparse-contract long backfills).
    let window = effective_window(window_override, chain_window);

    tracing::info!(
        "indexing nest '{}' on {}: {} contract(s), {} table(s), {} anonymous skipped, finality {:?}, window {}, registry {}…",
        config.nest.name,
        config.nest.chain,
        config.contracts.len(),
        registry.tables().len(),
        registry.skipped_anonymous(),
        finality,
        window,
        &hex::encode(registry.hash())[..12],
    );

    // Governed semantic layer (RFC-0016): if `semantic.toml` describes a table/column the registry
    // doesn't have, the semantics are stale - worse than none. Warn loudly at startup.
    let served = full_schema(&registry, config);
    if let Ok(Some(sem)) = crate::semantic::load(&dir) {
        for w in crate::semantic::drift(&served, &sem) {
            tracing::warn!("semantic.toml drift: {w}");
        }
    }
    // Authored views (RFC-0018 §1): a broken/drifted view no longer vanishes silently - it's a loud
    // startup warning (with a fuzzy-matched fix hint), and a `nuthatch check` failure. The view still
    // loads fault-isolated (a bad one never disables its siblings or the query surface).
    for issue in crate::analytics::validate_nest_views(&dir, &served) {
        match &issue.hint {
            Some(h) => tracing::warn!("view {} failed to load: {} - {h}", issue.file, issue.error),
            None => tracing::warn!("view {} failed to load: {}", issue.file, issue.error),
        }
    }
    // #663: a declared event that has never fired on this chain is an ordinary state, not a fault -
    // but it used to be an invisible one, order-dependent and explained nowhere. Say it once, loudly,
    // at the moment an operator is most likely to be reading the log: the day it stops being true,
    // this line simply stops naming that table, which is as much of an answer as the day it started.
    let never_fired = crate::analytics::declared_but_never_sealed(&dir, &served);
    if !never_fired.is_empty() {
        tracing::info!(
            "{} declared table(s) have no data yet - the event has likely never fired on this chain: \
             {}. Each reads as an empty table, not an error, and starts populating the day it does.",
            never_fired.len(),
            never_fired.join(", "),
        );
    }

    // Grafting (RFC-0033): tell the author up front what will and will not be reusable, rather than
    // leaving them to wonder why edits stay slow. A **cycle is a refusal** - derivations read decoded
    // events and other derivations, never themselves, so a cycle means the nest is malformed (§6).
    // Everything else is advisory: a volatile view is legal to author, it simply cannot be cached, and
    // refusing to start over one would break nests that work today.
    let graft = crate::graft::report(&dir);
    if let Some(cycle) = &graft.cycle {
        anyhow::bail!(
            "this nest's derivations form a cycle: {cycle}. A derivation may read decoded events and \
             other derivations, never itself - break the loop and reload."
        );
    }
    for (view, why) in &graft.never_graftable {
        tracing::warn!("view {view} can never be reused across an edit: it {why}");
    }
    if !graft.uncanonical.is_empty() {
        tracing::debug!(
            "views whose plan could not be canonicalised (only a byte-identical edit will match): {}",
            graft.uncanonical.join(", ")
        );
    }

    // A nest that vendors deployment blocks backfills from the earliest one (full history from
    // deployment); otherwise a cold start falls back to the `--backfill` tip offset.
    let start_block = config.contracts.iter().filter_map(|c| c.start_block).min();

    // Optional alert sinks (RFC-0008 C5) + user webhooks (RFC-0010 Part B) - two producers, one
    // shared delivery engine. The worker drains the durable outbox on its own task, decoupled from
    // indexing, so a slow/dead endpoint never blocks the loop.
    let router = Arc::new(alerts::AlertRouter::new(config.alerts.clone()));
    let webhooks = Arc::new(config.webhooks.clone());
    let alert_worker = if router.is_empty() && webhooks.is_empty() {
        None
    } else {
        tracing::info!(
            "{} alert sink(s), {} webhook(s) configured",
            config.alerts.len(),
            config.webhooks.len()
        );
        Some(tokio::spawn(alerts::run_delivery_worker(
            std::sync::Arc::new(store.clone()) as std::sync::Arc<dyn crate::store::HotStore>,
            crate::webhooks::secrets(&config.webhooks),
        )))
    };

    // Group the per-nest state the loop owns and mutates into one struct, so a runtime can drive many
    // nests from one cursor (RFC-0012). `source` stays shared and borrowed, not owned; `children`
    // starts empty (it is rebuilt/grown by `prepare`). The view handles are cloned here and shared
    // with the `AppState` below - the API must see the same views the loop feeds.
    // RFC-0023 tier 3: the archive endpoint a declared call is resolved against. Built here so a
    // nest declaring `[[calls]]` without one is refused at startup rather than discovered thousands
    // of blocks into a backfill as a wall of identical failures.
    // RFC-0038 §5: built whenever the nest decodes calldata at all. Distinct from `enabled()`, which
    // is the node-gated set - a top-level-calls nest needs this surface and needs no node.
    let call_registry = if config.extract.decodes_calls() {
        Some(Arc::new(crate::calldata::CallRegistry::from_nest(
            &dir, config,
        )?))
    } else {
        None
    };

    let state_rpc = if config.state_rpc_urls.is_empty() {
        if !config.calls.is_empty() {
            anyhow::bail!(
                "this nest declares {} `[[calls]]` entr{}, which need historical `eth_call` and \
                 therefore an archive endpoint.\n\n\
                 Pass `--state-rpc <url>`. It is deliberately not a `nuthatch.toml` field: an \
                 archive endpoint usually carries an API key, and the config is pinned into the \
                 nest's content address.\n\n\
                 Most contract state does not need this at all - try `nuthatch recipe add \
                 total_supply` (also `balances`, `holder_count`, `reserves`), or `nuthatch metadata \
                 fetch` for immutable `decimals`/`symbol`/`name`.",
                config.calls.len(),
                if config.calls.len() == 1 { "y" } else { "ies" },
            );
        }
        None
    } else {
        Some(Arc::new(crate::rpc::RpcClient::new(
            config.state_rpc_urls.clone(),
        )?))
    };

    let shared_store = store.clone();
    let nest = NestIngest {
        name: config.nest.name.clone(),
        dir: dir.clone(),
        store: shared_store.clone(),
        registry: registry.clone(),
        balances: balances.clone(),
        exposure: exposure.clone(),
        velocity: velocity.clone(),
        entities: entities.clone(),
        labels: labels.clone(),
        screener: screener.clone(),
        threshold,
        velocity_cfg,
        router: router.clone(),
        webhooks: webhooks.clone(),
        factory: factory.clone(),
        children: ChildRegistry::new(),
        finality,
        metrics: {
            let m = METRICS.nest(&config.nest.name);
            m.set_storage_paths(
                dir.join(crate::config::DB_FILE),
                crate::seal::shared_store(&dir)
                    .unwrap_or_else(|| dir.join(crate::seal::SEGMENTS_DIR)),
            );
            // **#918: seed the sealed watermark from the store, not from the next seal.**
            //
            // The watermark is durable - `SEALED_THROUGH_KEY` in the store's meta - and the query path
            // has always read it correctly, which is why `/sql` provenance reported the true value
            // while `/metrics` reported 0. The gauge was only ever written by `seal_finalized`, so a
            // freshly restarted nest advertised `nuthatch_sealed_through 0` until its first seal.
            //
            // Measured on the Lodestar box: two units restarted 28 minutes apart, one on 2.7.1 and one
            // on 3.0.0-alpha.1, both reporting 0 on `/metrics` and 499300218 in query provenance,
            // while two units untouched since Aug 24 reported it correctly. Not a regression - it has
            // always done this - but two surfaces disagreeing about one fact, and the wrong one is
            // where Prometheus looks.
            //
            // It matters more now that 3.0.0 ships six `nuthatch_entity_*` series inviting alerts on
            // this surface: "sealed_through has not advanced" or "went backwards" fires after every
            // restart of a perfectly healthy nest, and an alert that cries wolf gets muted.
            m.set_sealed_through(shared_store.sealed_through());
            m
        },
        addresses,
        topic0s,
        start_block,
        ipfs: config.ipfs.clone(),
        ipfs_gateways: if config.ipfs_gateways.is_empty() {
            crate::subgraph_import::DEFAULT_IPFS_GATEWAYS
                .iter()
                .map(|g| g.to_string())
                .collect()
        } else {
            config.ipfs_gateways.clone()
        },
        top_level_calls: config.extract.top_level_calls,
        call_registry: call_registry.clone(),
        chain_id: config.nest.chain_id,
        calls: config.calls.clone(),
        state_rpc,
    };
    // Stamps this nest's readiness clock (#510): `/ready`'s never-polled grace period is bounded from
    // here, not permanent - see `serve::poll_stalled`.
    nest.metrics.mark_started();

    let nest_info = serde_json::json!({
        "name": config.nest.name,
        "chain": config.nest.chain,
        "chain_id": config.nest.chain_id,
        "registry_hash": format!("0x{}", hex::encode(registry.hash())),
        "table_count": registry.tables().len(),
        "contracts": config.contracts.iter()
            .map(|c| serde_json::json!({ "alias": c.alias, "address": c.address })).collect::<Vec<_>>(),
        "templates": config.templates,
        "factories": config.factories,
        // Deliberately NOT the full `url`: webhook URLs routinely embed a secret in the path (Slack/
        // Discord/bearer-in-path), and `/nest` is unauthenticated. Expose only scheme+host so the admin
        // UI can show *where* a webhook points without leaking the credential to any reader (incl. a
        // co-tenant on a runtime).
        "webhooks": config.webhooks.iter()
            .map(|w| serde_json::json!({ "name": w.name, "table": w.table, "target": webhook_host(&w.url),
                "finality": w.finality.clone().unwrap_or_else(|| "sealed".into()) })).collect::<Vec<_>>(),
    });

    let app_state = serve::AppState {
        store: shared_store.clone(),
        // Not `primary()?`: that errors "nest has no contracts", which turned a field the summary
        // renders into a hard refusal to build a contract-free blocks nest (#445).
        address: config.contracts.first().map(|c| c.address.clone()),
        chain: config.nest.chain.clone(),
        dir: dir.clone(),
        balances,
        exposure,
        velocity,
        entities,
        threshold,
        velocity_threshold: velocity_cfg.map(|(amt, _)| amt),
        tables: Arc::new(full_schema(&registry, config)),
        sql_gate,
        sql_max_hot_rows: serve::SQL_MAX_HOT_ROWS,
        // Every cursor-owning role builds through here; `serve_role` flips it after (#1025).
        cursorless: false,
        // Open by default; `runtime::dev` overlays the mount's surface after the nest is built
        // (RFC-0034). A solo `nuthatch dev` has no mount record and therefore no surface to apply.
        surface: Arc::new(crate::allowlist::Surface::default()),
        // Set by the runtime for a mounted nest; a solo `dev` nest has no mount record.
        nid: None,
        // Set by `spawn_runtime` for a co-tenanted nest; a solo `dev` nest has no mounts health surface.
        runtime_health: None,
        admin_enabled,
        admin_token,
        nest_info: Arc::new(nest_info),
    };

    Ok((nest, app_state, alert_worker, window))
}

/// The **query-FE role** (RFC-0022 §1): serve a nest from a shared hot store without indexing it.
///
/// A writer somewhere owns the cursor and fills the store; this process answers reads from it. There
/// is no ingest loop, no tip poller and no cursor - so an operator scales serving capacity by adding
/// these, and ingestion throughput by adding writers, without the two being the same dial.
///
/// **Why it reuses `build_nest` rather than assembling its own state.** The serving surface depends
/// on a pile of derived things - the decode registry, the balance/exposure/velocity views, the table
/// schemas, the nest metadata blob - and a second construction path for them is a second place for
/// them to drift. `build_nest` already builds all of it *without* starting ingestion (spawning the
/// loop is the caller's job), so the FE takes the state it returns and simply never spawns anything.
/// The ingest half is dropped on the floor, which is exactly the semantics wanted.
///
/// The one honest seam today: the view rebuilds still read the nest's local redb even when serving a
/// Postgres store, so an FE currently wants the nest directory on disk. Slice 3b moves those helpers
/// onto the trait; until then this is a real limitation rather than a rough edge, and it is why the
/// compose file mounts the nest directory into the FE.
pub async fn serve_role(args: crate::cli::ServeArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)
        .with_context(|| format!("no nest at '{}' (run `nuthatch init` first)", dir.display()))?;

    // Resolved here, not left to `build_nest`'s `None` branch (issue #520): that branch is
    // `Store::open`, which *creates* the store and commits a write txn - correct for the writer
    // roles that share it, wrong for a role that owns no cursor and has nothing to write. Omitting
    // `--hot-store` still means "serve the local redb", but that store must already exist; this
    // process opens it non-creating (`open_existing`) and refuses rather than bringing an empty one
    // into being (#413's mistake at a site #413 did not cover) or silently taking the write path.
    let store_override: Option<Arc<dyn crate::store::HotStore>> = match &args.hot_store {
        Some(url) => Some(open_shared_hot_store(url, &config.nest.name)?),
        None => Some(Arc::new(Store::open_existing(&dir.join(DB_FILE)).with_context(|| {
            format!(
                "no hot store to serve from at '{}' - `serve` never creates or writes to the local \
                 redb, only reads it. Index the nest first with `nuthatch dev`, or point at a shared \
                 store with --hot-store",
                dir.display()
            )
        })?)),
    };

    // No `Source` is ever polled on this role; the parameter exists for the ingest half we discard.
    // That fact is handed to `/ready` below as `cursorless` - it used to be known only here, which is
    // why a serve-only nest reported `stalled:true` forever (#1025).
    //
    // **And it is no longer an `RpcClient`** (#815). Building one here required `rpc_urls` to be
    // non-empty for a client that is passed to `build_nest`'s `_source` parameter and never touched,
    // so a fully-sealed nest with no chain behind it - `rpc_urls = []` - could not be served at all.
    let source: Arc<dyn Source> = Arc::new(crate::source::UnpolledSource);

    // The FE role gets the *same* admin derivation as `dev` and the roost runtime, not a bare read of
    // the env var. Reading it directly left `--admin` on an off-localhost bind serving `/_admin/` with
    // `admin_token: None`, which `admin_authorized` treats as "localhost, open" - so the query FE, the
    // one role an operator actually puts on a network, was the one role with no credential (#292).
    // `--admin` is opt-in here and opt-out on `dev`, which is the only asymmetry that survives.
    let admin_enabled = admin_enabled(!args.admin, &args.listen);
    let admin_token = admin_required_token(admin_enabled, &args.listen);
    let (_ingest, state, alert_worker, _window) = build_nest(
        &source,
        dir.clone(),
        &config,
        None,
        admin_enabled,
        admin_token,
        store_override,
        serve::new_sql_gate(),
    )
    .await?;
    // **This role owns no cursor** (#1025). `/ready`'s liveness terms all ask about one - has it
    // polled, has `last_block` advanced - and a role that never polls fails them permanently rather
    // than transiently. Told once, here, rather than inferred at the endpoint.
    let state = serve::AppState {
        cursorless: true,
        ..state
    };

    // The delivery worker belongs to whoever owns the cursor. Two processes draining one outbox would
    // deliver the same webhook twice, and at-least-once is a promise about failure, not a licence to
    // duplicate on the happy path.
    if let Some(w) = alert_worker {
        w.abort();
        let _ = w.await;
    }

    // Only the `--hot-store` path is actually shared - the local-redb path holds an exclusive flock
    // on the file for the process's lifetime (#520), so it cannot run beside the `dev`/writer that
    // fills it or beside a second `serve`, whatever "read-only" suggested.
    if args.hot_store.is_some() {
        tracing::info!(
            nest = %config.nest.name,
            chain = %config.nest.chain,
            listen = %args.listen,
            store = %args.hot_store.as_deref().map(shorten_store).unwrap_or("local redb"),
            "serving read-only (RFC-0022 query-FE role) - this process owns no cursor"
        );
    } else {
        tracing::info!(
            nest = %config.nest.name,
            chain = %config.nest.chain,
            listen = %args.listen,
            "serving the local redb (RFC-0022 query-FE role) - this process owns no cursor, but \
             holds an exclusive lock on the store: it cannot run beside the writer or a second serve"
        );
    }

    serve::run(&args.listen, state).await
}

/// Open the shared hot store an FE serves from.
#[cfg(feature = "postgres-store")]
fn open_shared_hot_store(url: &str, nest: &str) -> Result<Arc<dyn crate::store::HotStore>> {
    Ok(Arc::new(crate::pgstore::PgStore::connect(url, nest)?))
}

#[cfg(not(feature = "postgres-store"))]
fn open_shared_hot_store(_url: &str, _nest: &str) -> Result<Arc<dyn crate::store::HotStore>> {
    anyhow::bail!(
        "--hot-store needs a build with `--features postgres-store`. The default binary is the \
         embedded one and deliberately carries no database driver (CLAUDE.md non-negotiable 1)."
    )
}

/// A connection string minus any password, for logging.
fn shorten_store(url: &str) -> &str {
    match url.find("://") {
        Some(i) => &url[..i],
        None => "shared",
    }
}

/// Batch size (rows) at which both seal paths cut a segment: `backfill_direct` via
/// [`take_sealable`], the tip path via [`tip_seal_cut`]. Public so integration tests can pad a
/// finalized range to the same threshold without duplicating the number.
pub const SEAL_DIRECT_BATCH: usize = 20_000;

/// How many blocks at the end of every fetched window are asked for **again** with the next one
/// (#1144, RFC-0049 §1).
///
/// Every way a provider answers a log range short lands at the *end* of the range: a load-balanced
/// backend a block or two behind the one that answered `latest`, a height the node has finalised but
/// not yet executed (Monad, RFC-0051), an answer truncated under load. All of them answer correctly a
/// moment later. A list is a valid answer, so nothing at fetch time can tell a short one from an
/// empty block - measured 2026-09-04, the header's `logsBloom` cannot either: on mainnet a sparse
/// address tests positive in a quarter of the blocks it has nothing to do with. So the next window
/// starts this many blocks *before* the cursor rather than at it, and rows are keyed by
/// `(block, log_index)`, so the second pass adds what the first missed and duplicates nothing.
///
/// At tip the window is a block or two wide, so this widens the one call the poll already makes and
/// adds none; in backfill it adds two blocks to a window of hundreds. Two covers the two-block skew
/// measured on Alchemy's Monad pool; Monad's three-block execution deferral sits behind a seal depth
/// of eight and never reaches the fetch.
pub const FETCH_TAIL_OVERLAP: u64 = 2;

/// Where a window's fetch starts: `FETCH_TAIL_OVERLAP` blocks before the cursor, never before the
/// range began. The first window of a range has nothing behind it to refetch.
fn overlap_from(next: u64, range_from: u64) -> u64 {
    next.saturating_sub(FETCH_TAIL_OVERLAP).max(range_from)
}

/// The first block a later window will ask for again, given this window ended at `w_to`; rows at
/// or past it are not final and must not be sealed yet. Unbounded only once the final pass - the
/// refetch of the range's own last tail - has run, because until then even the range's last block
/// is still owed another answer. Lifting it at the range end instead let a buffer that happened to
/// hold exactly `SEAL_DIRECT_BATCH` rows seal its tail, and the final pass then found nothing to
/// deduplicate against and sealed two rows twice.
fn tail_hold(w_to: u64, final_pass_done: bool) -> u64 {
    if final_pass_done {
        u64::MAX
    } else {
        w_to.saturating_sub(FETCH_TAIL_OVERLAP - 1)
    }
}

/// A row waiting to be sealed: `(block, log_index, json)`. Keyed, not bare JSON, because the tail of
/// the buffer is refetched and merged (#1144): a merge has to drop what it already holds and re-sort
/// the block it touched, and the segment's content address depends on that order.
pub(crate) type SealRow = (u64, u64, String);

/// Merge one fetched window into the seal buffer (#1144).
///
/// Rows whose `(block, log_index)` the buffer already holds are dropped - they are the refetched
/// tail answering the same as before. The rest are appended, and the region from `merge_from` on is
/// re-sorted, so a row the first pass missed slots into its block in canonical order rather than
/// after it. Returns how many rows were new.
///
/// `(block, log_index)` is the row's identity **everywhere**, not only here: it is `Store::entity_key`
/// in the hot store and the sort key of every sealed segment. Rows that descend from no log - block
/// rows, pinned call results, decoded top-level calls, IPFS documents - live in the reserved band
/// `500_000..=999_999` of `log_index` (`registry::BLOCK_ROW_LOG_INDEX` and friends), which a real
/// log cannot reach, precisely because #642 was a block row at index `0` overwriting the first event
/// of its block in the store. So a block row and an event row of the same block never share a key
/// here either; `blocks_and_event_rows_keep_distinct_identities_through_the_merge` holds that.
///
/// A held key whose refetched content **differs** is not a duplicate and not a second row: the
/// store could hold only one of them, so it is the provider answering the same block two ways
/// between one ask and the next. That is refused, by name, rather than resolved by whichever came
/// first - the range is retried, and if it keeps happening the backfill fails loudly instead of
/// sealing a segment nobody can reproduce.
fn merge_window_rows(
    buf: &mut Vec<SealRow>,
    merge_from: u64,
    rows: impl IntoIterator<Item = SealRow>,
) -> Result<u64> {
    let start = buf.partition_point(|r| r.0 < merge_from);
    let held: std::collections::HashMap<(u64, u64), &str> = buf[start..]
        .iter()
        .map(|r| ((r.0, r.1), r.2.as_str()))
        .collect();
    let mut fresh = Vec::new();
    for r in rows {
        match held.get(&(r.0, r.1)) {
            None => fresh.push(r),
            Some(json) if *json == r.2 => {}
            Some(_) => anyhow::bail!(
                "block {} log {} came back with different content on refetch - the provider \
                 answered the same block two ways, refusing to seal either",
                r.0,
                r.1
            ),
        }
    }
    let added = fresh.len() as u64;
    buf.extend(fresh);
    buf[start..].sort_by_key(|r| (r.0, r.1));
    Ok(added)
}

/// Split a full seal buffer at a block boundary chosen from the **data**, not from wherever a fetch
/// window happened to stop (RFC-0028 §4).
///
/// Rows arrive in `(block, log_index)` order, so the cut is "everything up to and including the block
/// that carried the buffer past the threshold". That point is a function of cumulative rows per block -
/// a property of the chain - so two operators running different `--window`/`--concurrency` produce
/// **identical** segments. Before this, the segment ended at the fetch window's last block, which made
/// content-addressing quietly conditional on the operator's RPC tuning and broke the dedup that
/// RFC-0019 bundles and RFC-0020 segment reuse both rest on.
///
/// A block is never split across segments: if one block alone carries the buffer past the threshold,
/// the whole block goes into this segment and the segment is simply larger.
///
/// Returns `(rows, last_block)` and leaves the remainder in `buf`; `None` while the buffer is short.
fn take_sealable(buf: &mut Vec<SealRow>, hold_from: u64) -> Option<(Vec<String>, u64)> {
    // Only rows below `hold_from` are final: the blocks at or past it are the tail the next window
    // asks for again (#1144), and a cut inside them would seal a block a later answer may still
    // add to. The cut itself is unchanged - the block carrying the buffer past the threshold - it is
    // simply not taken until that block is out of the tail, so segment identity is what a complete
    // first answer would have produced.
    let eligible = buf.partition_point(|r| r.0 < hold_from);
    if eligible < SEAL_DIRECT_BATCH {
        return None;
    }
    let cut_block = buf[SEAL_DIRECT_BATCH - 1].0;
    let n = buf.partition_point(|r| r.0 <= cut_block);
    let rows = buf.drain(..n).map(|(_, _, j)| j).collect();
    Some((rows, cut_block))
}

/// Every buffered row, in order - the final flush when a range ends.
fn drain_sealable(buf: &mut Vec<SealRow>) -> Vec<String> {
    buf.drain(..).map(|(_, _, j)| j).collect()
}

/// Drain every currently-sealable prefix from `buf` one segment at a time via `f`.
///
/// `while`, not `if` (#980, #1015). A fetched chunk can carry many multiples of
/// `SEAL_DIRECT_BATCH`; taking only one of them per chunk made the *number* of
/// segments a function of `--window`. The three production loops and the
/// window-independence tests all go through here, so turning this back into
/// `if let` is visible.
///
/// Dispatches each segment to `f` immediately as it is detached from `buf`, so
/// segments are sealed and dropped one by one without accumulating multiple
/// segments in memory concurrently.
fn drain_all_sealable(
    buf: &mut Vec<SealRow>,
    hold_from: u64,
    mut f: impl FnMut(Vec<String>, u64) -> Result<(), anyhow::Error>,
) -> Result<(), anyhow::Error> {
    while let Some((rows, seal_to)) = take_sealable(buf, hold_from) {
        f(rows, seal_to)?;
    }
    Ok(())
}

/// Above this many discovered children, the factory backfill flips from an address-list filter to a
/// topic0-only fetch with local registry-lookup filtering (RFC-0009 §4) - providers cap address-list
/// size, and a huge list is slower than fetching by topic0 and discarding non-children locally.
const FACTORY_FLIP_THRESHOLD: usize = 500;

/// Refuse to start when `[nest] block_timestamps` disagrees with what this nest already indexed.
///
/// Flipping the declaration is not a config edit, it is a **breaking schema change** (RFC-0029 §6b-i):
/// it adds or removes a column on every table, which RFC-0020's classifier calls `ColumnRemoved`, and
/// it changes the bytes of every sealed segment, so the existing content-addressed segments cannot be
/// reused even over an identical range. Starting anyway would leave one nest holding two schemas -
/// rows and segments written before the flip carrying the column and everything after not - and
/// nothing would notice until a query hit the wrong half and got an error, or worse, a silent gap.
///
/// The first index writes the key; a nest predating this (no key) adopts whatever it is declaring,
/// which is correct because before this existed every nest indexed timestamps and the default is
/// `true`. A pre-existing nest that *declares* `false` is the one case worth catching, and the
/// `has_indexed` check below is what catches it: an empty store has nothing to contradict.
/// Refuse to serve rows under a decode registry that did not produce them (#653).
///
/// A nest's `registry_hash` is the claim "this data was produced by this decode configuration", and
/// `blob.rs` already takes it seriously - a packed nest records its expected hash and `mount`
/// regenerates and verifies it. The ordinary `dev --dir <dir>` path did not, so adding an event to an
/// existing nest was **silent**: the nest started, found itself already at tip, indexed nothing, and
/// then stamped the *new* hash onto every query's provenance. Measured: a hash went from
/// `0xa265740366…` to `0xe3de2aa16d…` with zero events indexed and no warning, the only symptom being
/// an unrelated view failing to load because it happened to reference one of the new tables. A change
/// touching tables no view references would have said nothing at all.
///
/// Same reasoning as the `--seal-direct` refusal for declared calls (RFC-0038 §6e): a run that
/// quietly produces a table with no rows is worse than a run that refuses.
fn guard_registry_identity(store: &dyn crate::store::HotStore, registry_hash: &str) -> Result<()> {
    match store.get_meta(REGISTRY_KEY)? {
        Some(found) if found != registry_hash => anyhow::bail!(
            "this nest's stored data was indexed by a different decode registry.\n\n               stored:  0x{found}\n  config:  0x{registry_hash}\n\n             The registry hash covers every contract, event and column this nest decodes, so a              difference means `nuthatch.toml` or an ABI changed after the data was written.              Continuing would serve old rows under a new content address, and any table added by the              change would read as empty rather than as absent.\n\n             Re-index from scratch to adopt the new configuration (remove `nuthatch.redb` and              `segments/`), or restore the previous configuration to keep serving this data. A nest              whose identity changed is a different nest - that is what content addressing means."
        ),
        Some(_) => Ok(()),
        None => {
            // Absent means one of two things and they must not be conflated: a fresh store, or a
            // store written by a build from before this guard existed. Refusing the second would
            // break every deployment on upgrade for a fault none of them necessarily have, so it
            // adopts - but says so when there are already rows, because an adopted hash is a claim
            // nobody verified and it should not read like one that was.
            if store.get_meta(LAST_BLOCK_KEY)?.is_some() {
                tracing::warn!(
                    "adopting registry hash 0x{registry_hash} for a store that predates the identity                      check (#653) - it was recorded, not verified. If this nest's configuration has                      changed since it was indexed, re-index it."
                );
            }
            store.set_meta(REGISTRY_KEY, registry_hash)?;
            Ok(())
        }
    }
}

fn guard_timestamp_policy(store: &dyn crate::store::HotStore, declared: bool) -> Result<()> {
    let want = if declared { "1" } else { "0" };
    match store.get_meta(TIMESTAMPS_KEY)? {
        Some(found) if found != want => {
            let (was, now) = if declared {
                ("without", "with")
            } else {
                ("with", "without")
            };
            anyhow::bail!(
                "this nest indexed its stored data {was} `block_timestamp`, but nuthatch.toml now \
                 declares it {now} it (`[nest] block_timestamps = {declared}`).\n\n\
                 That is a breaking schema change, not a setting: it {} the column on every table, \
                 and it changes the bytes of every sealed segment - so this nest's existing segments \
                 cannot be reused and the data would have to be re-indexed from scratch.\n\n\
                 Restore `block_timestamps = {}` to start, or `nuthatch init` a new nest with the \
                 declaration you want and serve it alongside this one until its consumers move \
                 (RFC-0020 slice 3).",
                if declared { "adds" } else { "removes" },
                !declared
            );
        }
        Some(_) => Ok(()),
        None => {
            // An untouched nest adopts the declaration; one with rows already in it does not get to
            // change its mind silently, so record what it actually has rather than what it now says.
            let has_indexed = store.get_meta(LAST_BLOCK_KEY)?.is_some();
            let actual = if has_indexed && !declared { "1" } else { want };
            store.set_meta(TIMESTAMPS_KEY, actual)?;
            if actual != want {
                anyhow::bail!(
                    "this nest has already indexed with `block_timestamp`, so it cannot switch to \
                     `block_timestamps = false` in place - that removes a column from every table \
                     and invalidates every sealed segment. `nuthatch init --no-timestamps` a new \
                     nest instead, and serve it alongside this one (RFC-0020 slice 3)."
                );
            }
            Ok(())
        }
    }
}

/// Block timestamps for `blocks` - or an empty map, without touching the network, when the nest
/// declared it doesn't index them (RFC-0029 §6b).
///
/// **This is where the win actually is.** Timestamps arrive one `eth_getBlockByNumber` per block over
/// a round trip `eth_getLogs` does not carry, measured at ~85% of backfill wall clock (RFC-0029 §4).
/// Every path that stamps rows goes through here so there is exactly one place the decision is made,
/// and no path can quietly keep paying for a column its nest doesn't have.
///
/// Returning an empty map is safe rather than lossy: `DecodedRow::block_timestamp` stays 0 and
/// `to_json` omits the key entirely, so the zero never reaches the store or a segment.
async fn fetch_timestamps(
    source: &dyn Source,
    registry: &DecodeRegistry,
    blocks: &[u64],
) -> Result<std::collections::HashMap<u64, u64>> {
    if !registry.timestamps() {
        return Ok(std::collections::HashMap::new());
    }
    source.block_timestamps(blocks).await
}

/// Blocks that still need a header after local filtering (#765).
///
/// A topic0-only fetch (factory flip, RFC-0009 §4) returns every log on the chain with that
/// topic0, including other protocols that share the event shape. Stamping timestamps from the
/// raw log list paid `eth_getBlockByNumber` for blocks whose rows we then discarded. The kept
/// rows plus any `[[calls]]` sample blocks are the only headers worth buying.
fn blocks_needing_timestamps(
    rows: &[crate::registry::DecodedRow],
    extra: impl IntoIterator<Item = u64>,
) -> Vec<u64> {
    let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
    blocks.extend(extra);
    blocks.sort_unstable();
    blocks.dedup();
    blocks
}

fn apply_row_timestamps(
    rows: &mut [crate::registry::DecodedRow],
    ts: &std::collections::HashMap<u64, u64>,
) {
    for r in rows {
        r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
    }
}

/// Resolve declared `[[calls]]` for one window of blocks and return them as `DecodedRow` objects in
/// `(block_number, log_index)` order - ready to be merged with event rows before sealing.
///
/// Mirrors the tier-3 resolution in `process_window` exactly so the sealed rows are identical
/// regardless of which path produced them. Returns an empty vec when `calls` is empty.
///
/// `pub(crate)` for `bench.rs`, whose hot-store arm is a private reimplementation of this loop and
/// resolved nothing at all until #743 - the same "the harness measures a workload `dev` does not
/// run" failure as #224 and #725, on the arm `bench backfill` takes when given no path flag.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn resolve_calls_for_window(
    source: &dyn Source,
    calls: &[crate::calls::CallDecl],
    state_rpc: &crate::rpc::RpcClient,
    chain_id: u64,
    event_rows: &[crate::registry::DecodedRow],
    from: u64,
    to: u64,
    timestamps: &std::collections::HashMap<u64, u64>,
    with_timestamps: bool,
) -> Result<Vec<crate::registry::DecodedRow>> {
    use std::collections::BTreeMap;

    let mut wanted: BTreeMap<u64, Vec<(usize, String, String)>> = BTreeMap::new();
    for (i, d) in calls.iter().enumerate() {
        if d.is_row_driven() {
            let table = d.on.as_deref().unwrap_or_default();
            let mut src: Vec<&crate::registry::DecodedRow> =
                event_rows.iter().filter(|r| r.table == table).collect();
            src.sort_by_key(|r| (r.block_number, r.log_index));
            for r in src {
                let (contract, calldata) = d.resolve_for_row(r)?;
                wanted
                    .entry(r.block_number)
                    .or_default()
                    .push((i, contract, calldata));
            }
        } else {
            for b in d.blocks_in(from, to) {
                wanted.entry(b).or_default().push((
                    i,
                    d.contract.to_ascii_lowercase(),
                    d.calldata.to_ascii_lowercase(),
                ));
            }
        }
    }

    let capacity = crate::registry::BLOCK_ROW_LOG_INDEX - crate::registry::CALL_ROW_LOG_INDEX_BASE;
    // One batched header fetch for every block this window's calls touch (#720), rather than a
    // sequential single-block `block_hash` per block below. The blocks' timestamps already came
    // from a batched fetch (`fetch_timestamps`, above the caller); the hash was the one field still
    // paying an unbatched round trip per sampled block.
    let wanted_blocks: Vec<u64> = wanted.keys().copied().collect();
    let headers = retry_transient(
        &format!(
            "seal-direct block headers for {} block(s)",
            wanted_blocks.len()
        ),
        BACKFILL_RETRY_BASE,
        || async { source.block_headers(&wanted_blocks).await },
    )
    .await?;
    let mut out: Vec<crate::registry::DecodedRow> = Vec::new();
    for (block, mut items) in wanted {
        let mut seen = std::collections::HashSet::new();
        items.retain(|(i, c, d)| seen.insert((*i, c.clone(), d.clone())));
        if items.len() as u64 >= capacity {
            anyhow::bail!(
                "block {block} wants {} distinct pinned reads, and only {capacity} fit in the \
                 reserved row-index band.\n\n\
                 A row-driven `[[calls]]` declaration fires once per source row, so a dense \
                 table can ask for more reads than a block can hold. Narrow the source table \
                 (index fewer events), or make the declaration sampled instead.",
                items.len()
            );
        }
        let pairs: Vec<(String, String)> = items
            .iter()
            .map(|(_, c, d)| (c.clone(), d.clone()))
            .collect();
        let results = retry_transient(
            &format!("seal-direct pinned eth_call batch at block {block}"),
            BACKFILL_RETRY_BASE,
            || async { crate::calls::resolve_pairs_at(state_rpc, chain_id, &pairs, block).await },
        )
        .await?;
        let hash = headers
            .get(&block)
            .and_then(|h| h.get("hash"))
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        let ts = timestamps.get(&block).copied().unwrap_or(0);
        for (slot, ((i, _, _), r)) in items.iter().zip(results).enumerate() {
            out.push(r.to_row(&calls[*i].name, slot, &hash, ts, with_timestamps));
        }
    }
    out.sort_by_key(|r| (r.block_number, r.log_index));
    Ok(out)
}

/// Stream a *finalized* block range straight to sealed Parquet, bypassing the hot store entirely
/// (RFC-0004 §1): decode → buffered rows → content-addressed segments. No redb write, no read-back,
/// no prune - the churn a from-history backfill otherwise pays for every historical row. Rows carry
/// the same implicit columns (incl. `block_timestamp`) as the hot path and are sealed via the *same*
/// [`seal::seal_range`], so a given range yields byte-identical segments regardless of path (the
/// determinism guarantee, asserted in seal's path-equivalence test). The bounded buffer caps RSS by
/// construction. Only valid for ranges already past finality - there is no reorg risk to roll back.
///
/// `adaptive` selects the fetch-windowing strategy independently of the fact that this is the
/// seal-direct path (#744): `true` is what every other caller of this function wants (and what
/// `backfill_direct_pipelined`/`backfill_direct_factory` always do), `false` holds `window` fixed
/// for the whole run, with no shrink-and-retry on a provider cap - the "seal-fixed" bench control
/// that isolates the storage-path delta from the adaptive chunker's own contribution. This function
/// has no caller outside `bench` and its own tests, so `false` never reaches `nuthatch dev`.
/// Returns the number of rows sealed.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_direct(
    source: &dyn Source,
    registry: &DecodeRegistry,
    dir: &std::path::Path,
    addresses: &[String],
    topic0s: &[String],
    calls: &[crate::calls::CallDecl],
    state_rpc: Option<&crate::rpc::RpcClient>,
    chain_id: u64,
    from: u64,
    to: u64,
    window: u64,
    adaptive: bool,
) -> Result<u64> {
    // `(block, json)` so a segment can end on a data-determined block boundary (RFC-0028 §4).
    let mut buf: Vec<SealRow> = Vec::new();
    let mut batch_from = from;
    let mut next = from;
    let mut total = 0u64;
    // The last window has no window after it to refetch its tail (#1144), so the range's last
    // `FETCH_TAIL_OVERLAP` blocks are asked for once more, as a window of their own, before the
    // final flush. Set when that pass is the one in flight.
    let mut final_pass = false;
    // Adaptively size the getLogs range around the target response budget (RFC-0004 §2), starting
    // from the chain's default window - so dense and sparse ranges self-tune and provider result
    // caps are handled by shrink-and-retry rather than a hard failure.
    // A blocks nest pays per *block*, not per log, so its window ceiling is different (RFC-0036).
    // `None` when `adaptive` is false: the width then never moves off `window`, and a provider
    // result cap surfaces as a hard error instead of a shrink (see the `Err` arm below).
    let mut chunker = adaptive.then(|| {
        if registry.blocks() {
            AdaptiveWindow::for_window_with_headers(window)
        } else {
            AdaptiveWindow::for_window(window)
        }
    });
    while next <= to {
        let chunk_to = (next + chunker.as_ref().map_or(window, AdaptiveWindow::window) - 1).min(to);
        // **A window of zero blocks is not a window** (#853).
        //
        // `next` advances to `chunk_to + 1`, so a zero-width window leaves it exactly where it was
        // and this loop spins forever, issuing an inverted range to the source on every pass. It
        // cannot be interrupted from outside either: the fetch resolves immediately against a warm
        // source, so the future never yields and no `tokio::time::timeout` around the caller ever
        // gets polled - which is how three mutants of the window controller were recorded by the
        // nightly sweep as Timeout rather than Caught, each burning 688 seconds of it.
        //
        // A pure invariant rather than a heuristic: there is no legitimate zero-width request, so
        // this can never fire in correct operation and needs no tuning. The pipelined path already
        // refuses the same state one line differently - `next.saturating_add(w - 1)` underflows on
        // `u64` - so this brings the sequential path up to the behaviour its sibling already had.
        if chunk_to < next {
            anyhow::bail!(
                "the window controller produced a zero-width range at block {next} - refusing to \
                 loop without advancing. This is a bug in the controller, not a provider fault."
            )
        }
        // Nothing to decode means nothing to ask for. An empty address AND topic filter is not
        // "no logs" to a node - it is *every log on the chain*, which a blocks-only nest (OBIB case
        // 3: no contract at all) would otherwise request for every window and then discard, since
        // no log can decode without a matching address or topic. Public endpoints answer that with
        // a timeout rather than data.
        //
        // The condition used to be spelled out here, and separately at each other fetch. It is now
        // `LogFilter::new` returning `None` (#432), so the sites that forgot it cannot.
        // The tail of the previous window is asked for again (#1144); see `FETCH_TAIL_OVERLAP`.
        let fetch_from = if final_pass {
            next
        } else {
            overlap_from(next, from)
        };
        let logs = match LogFilter::new(addresses, topic0s) {
            None => Vec::new(),
            Some(filter) => match source.logs(&filter, fetch_from, chunk_to).await {
                Ok(logs) => {
                    if let Some(c) = &mut chunker {
                        c.observed(logs.len() as u64);
                    }
                    logs
                }
                // Only reachable in adaptive mode: fixed mode has no chunker to shrink, so a cap
                // falls through to the plain `Err(e)` arm below and the run fails loudly rather than
                // silently changing the width `--window-adaptive` was asked not to change.
                Err(e) if chunker.is_some() && narrowing_can_help(&e, next, chunk_to) => {
                    if next >= chunk_to {
                        return Err(e).with_context(|| single_block_over_cap(next));
                        // H3: can't shrink a block
                    }
                    chunker
                        .as_mut()
                        .expect("checked chunker.is_some() above")
                        .too_large();
                    tracing::debug!("range {next}..={chunk_to} refused; shrinking and retrying");
                    continue; // retry the same `next` with a smaller window
                }
                Err(e) => return Err(e).with_context(|| format!("getLogs {next}..={chunk_to}")),
            },
        };
        let mut rows: Vec<_> = logs
            .iter()
            .filter_map(|log| match registry.decode(log) {
                Ok(Some(r)) => Some(r),
                Ok(None) => None,
                Err(e) => {
                    tracing::debug!("decode skipped: {e:#}");
                    None
                }
            })
            .collect();
        // Seal in canonical (block, log_index) order regardless of how the RPC provider returned the
        // logs. The segment bytes - and therefore its content address - depend on row order; everywhere
        // else in the pipeline sorts defensively (the hot path re-reads redb in key order; the factory
        // path's `decode_window` sorts), so without this two providers returning the same logs in a
        // different order would produce different content hashes for identical data.
        rows.sort_by_key(|r| (r.block_number, r.log_index));
        // Stamp block_timestamp (batched), identical to the hot path, so segments match byte-for-byte.
        // Include the blocks sampled by any [[calls]] declarations so their timestamps are available
        // for the call rows resolved below.
        let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
        if state_rpc.is_some() {
            for d in calls {
                blocks.extend(d.blocks_in(next, chunk_to));
            }
        }
        blocks.sort_unstable();
        blocks.dedup();
        let ts = fetch_timestamps(source, registry, &blocks).await?;
        for r in &mut rows {
            r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
        }
        // RFC-0023 tier-3: resolve declared [[calls]] for this window and merge into rows so the
        // sealed segment is identical to what the hot path would have produced via process_window.
        if let Some(rpc) = state_rpc {
            let call_rows = resolve_calls_for_window(
                source,
                calls,
                rpc,
                chain_id,
                &rows,
                next,
                chunk_to,
                &ts,
                registry.timestamps(),
            )
            .await?;
            rows.extend(call_rows);
            rows.sort_by_key(|r| (r.block_number, r.log_index));
        }
        total += merge_window_rows(
            &mut buf,
            fetch_from,
            rows.iter()
                .map(|r| (r.block_number, r.log_index, r.to_json().to_string())),
        )?;

        // RFC-0036 §4.2: one row per block in the window, for a nest that declares `[extract] blocks`.
        //
        // Enumerated from the **window** rather than from `rows`, which is the whole point: `rows` only
        // covers blocks that emitted a matching log, and a blocks table must cover blocks that emitted
        // nothing. OBIB case 3 is 100,001 blocks of early mainnet with no contract in the nest at all -
        // derive it from logs and you get zero rows and a green run.
        if registry.blocks() {
            let want: Vec<u64> = (next..=chunk_to).collect();
            let headers = source.block_headers(&want).await?;
            let mut block_rows: Vec<_> = want
                .iter()
                .filter_map(|b| {
                    headers
                        .get(b)
                        .and_then(|h| crate::registry::block_row(*b, h, registry.timestamps()))
                })
                .collect();
            // Same canonical ordering rule as above: segment bytes, and therefore the content
            // address, depend on row order.
            block_rows.sort_by_key(|r| r.block_number);
            total += merge_window_rows(
                &mut buf,
                fetch_from,
                block_rows
                    .iter()
                    .map(|r| (r.block_number, r.log_index, r.to_json().to_string())),
            )?;
        }

        next = chunk_to + 1;
        if next > to && !final_pass {
            final_pass = true;
            next = overlap_from(to + 1, from);
            continue;
        }

        // Flush on a data-determined boundary (RFC-0028 §4), not on the fetch window's end.
        //
        // `drain_all_sealable` is `while`, not `if` (#980, #1015). A single fetched chunk can carry
        // many multiples of `SEAL_DIRECT_BATCH`, and sealing only one of them per chunk made the
        // *number* of segments a function of `--window`: measured on a 30,000-block corpus, window
        // 320 produced 6 segments with a largest of 20,003 rows, and window 163,840 produced 2 with
        // a largest of 99,993. Same chain, same rows, different content addresses - which is exactly
        // the operator-dependent segmentation RFC-0028 §4 exists to prevent and what RFC-0019
        // bundles and RFC-0020 segment reuse both rest on. Draining fully restores it: 6 segments at
        // every window.
        drain_all_sealable(
            &mut buf,
            tail_hold(chunk_to, final_pass),
            |rows, seal_to| {
                seal::seal_range(dir, &rows, batch_from, seal_to)?;
                batch_from = seal_to + 1;
                Ok(())
            },
        )?;
        if next > to && !buf.is_empty() {
            seal::seal_range(dir, &drain_sealable(&mut buf), batch_from, to)?;
            batch_from = next;
        }
    }
    Ok(total)
}

/// Whether narrowing is the right response to `err` **for the range still on the table**.
///
/// [`chunker::is_result_too_large`] answers "is this narrowable at all". This answers the question the
/// call sites actually have, which includes `from..=to`, and the two differ in exactly one case.
///
/// A **pool-wide 429** is escalated to `Narrowable` by RFC-0028 §3d, on the argument that narrowing is
/// also less load and so the escalation is benign even when the cause was really pacing. That argument
/// is sound while there is range left to narrow, and **spent the moment there is not**: a single block
/// cannot be split, the provider never said the result was too large, and no width we can ask for will
/// change the answer.
///
/// Returning `false` there is the whole fix (#916). It does not need new retry machinery, because each
/// of these call sites already has the correct handler sitting in its next arm - warn, back off, retry
/// the same width - written for exactly this case and commented "a 429 or a 403". The escalation was
/// routing a throttle *past* its own handler and into
/// `block N alone exceeds the provider's getLogs result cap`, which was never true, and which sends the
/// reader off to buy a bigger provider when the remedy is to slow down.
///
/// Measured: `nuthatch-dips` on the Lodestar box, two free endpoints, `NRestarts` climbing about twice
/// an hour under `Restart=always`.
fn narrowing_can_help(err: &anyhow::Error, from: u64, to: u64) -> bool {
    chunker::is_result_too_large(err) && !(from >= to && crate::rpc::escalated_from_rate_limit(err))
}

/// The error context when a single block's logs exceed a provider's `getLogs` result cap - it can't be
/// split or shrunk further, so the backfill/tip loop stops loudly instead of retrying forever (H3).
fn single_block_over_cap(block: u64) -> String {
    format!(
        "block {block} alone exceeds the provider's getLogs result cap - use a provider with a \
         higher/no cap"
    )
}

/// Fetch logs for `[from, to]`, transparently splitting the range in half and retrying each half when
/// a provider rejects it as "too many results" (RFC-0004 §2). The pipelined backfill uses a *fixed*
/// window and otherwise has no shrink-retry (deadlock-review finding H2), so an oversized `--window`
/// against a capped provider would abort the whole run; this makes it self-correct. A single block that
/// alone exceeds the cap can't be split further, so it fails with a clear message rather than looping
/// forever (finding H3).
/// Where to split `[from, to]` when the provider named a range that *would* have worked (RFC-0028 §3c).
///
/// Returns the last block of the provider's suggested range, so the first half is exactly what it told
/// us it can serve and the remainder is retried separately. `None` whenever the hint is unusable, in
/// which case the caller halves as before.
///
/// Validated rather than trusted - this is provider prose, not a contract. A hint is only used when it
/// starts at our `from` (otherwise it is describing a different request), ends before our `to` (a
/// "suggestion" no narrower than what we asked for would not make progress, and an equal one would
/// loop forever), and is not the whole range.
fn suggested_split_point(err: &anyhow::Error, from: u64, to: u64) -> Option<u64> {
    let crate::rpc::FailureClass::Narrowable {
        suggested: Some((s_from, s_to)),
        ..
    } = crate::rpc::class_of(err)?
    else {
        return None;
    };
    (s_from == from && s_to >= from && s_to < to).then_some(s_to)
}

/// [`fetch_logs_splitting`], reporting **the widest range that was actually served** alongside the
/// logs (#672).
///
/// The plain version hides its own recovery: it halves a refused range until the pieces succeed and
/// returns the merged result, so a caller cannot tell a window that worked from one that had to be
/// cut into eight. The controller then grows on the strength of a success that never happened at that
/// width. This hands back the evidence.
///
/// Implemented by threading a cell through the recursion rather than changing the return type,
/// because the plain form has four call sites in tests that are about splitting behaviour and should
/// stay as they are.
async fn fetch_logs_splitting_tracked(
    source: &dyn Source,
    filter: &LogFilter,
    from: u64,
    to: u64,
) -> Result<(Vec<crate::rpc::Log>, u64)> {
    let widest = std::sync::atomic::AtomicU64::new(0);
    let logs = fetch_logs_splitting_tracking(source, filter, from, to, true, &widest).await?;
    // Nothing served means nothing was asked for; report the full width so the caller changes nothing.
    let w = widest.load(std::sync::atomic::Ordering::SeqCst);
    Ok((logs, if w == 0 { to - from + 1 } else { w }))
}

/// [`fetch_logs_splitting_tracked`] with the served width discarded. **Test-only**: production
/// takes the tracked form, because the controller has to learn what width actually came back.
///
/// A wrapper rather than a second recursion, deliberately. There were two near-identical copies of
/// this walk for a while, and the speculative-split-once fault (#672) lived in exactly the half that
/// production used and the tests did not - a window ten times over a provider's cap was halved once,
/// the halves still refused, and the whole window failed and was retried forever.
#[cfg(test)]
async fn fetch_logs_splitting(
    source: &dyn Source,
    filter: &LogFilter,
    from: u64,
    to: u64,
) -> Result<Vec<crate::rpc::Log>> {
    fetch_logs_splitting_tracked(source, filter, from, to)
        .await
        .map(|(logs, _)| logs)
}

/// The body of [`fetch_logs_splitting`], plus whether a *speculative* split is still allowed for an
/// error we could not classify (RFC-0028 §3b).
///
/// Recognising a cap by its message text only works for providers whose phrasing we have already seen -
/// and that assumption cost us: `arb1.arbitrum.io`, an endpoint we ship as an Arbitrum default, says
/// `"logs matched by query exceeds limit of 10000"`, which matched none of the markers, so this
/// function never recursed and a busy Arbitrum backfill retried the same oversized window forever.
///
/// The markers are now wider, but the durable fix is not to depend on them alone: when a multi-block
/// window fails in a way we cannot classify, **try splitting once**. Splitting is safe by construction -
/// the halves tile the range exactly and rows are keyed by `(block, log_index)` - so the cost of being
/// wrong is one extra round trip, while the cost of *not* trying is a stalled backfill against any
/// provider we have not met.
///
/// The speculative split is deliberately **not** recursive: `speculative` is cleared for the halves, so
/// an endpoint that is simply down produces two extra requests rather than an exponential fan-out. A
/// genuine size failure re-triggers the *classified* path on the halves anyway, which recurses properly.
fn fetch_logs_splitting_tracking<'a>(
    source: &'a dyn Source,
    filter: &'a LogFilter,
    from: u64,
    to: u64,
    speculative: bool,
    widest: &'a std::sync::atomic::AtomicU64,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<crate::rpc::Log>>> + Send + 'a>>
{
    Box::pin(async move {
        match source.logs(filter, from, to).await {
            Ok(logs) => {
                widest.fetch_max(to - from + 1, std::sync::atomic::Ordering::SeqCst);
                Ok(logs)
            }
            Err(e) if narrowing_can_help(&e, from, to) => {
                if from >= to {
                    return Err(e).with_context(|| single_block_over_cap(from));
                }
                // Take the provider's own answer when it offers one. Alchemy replies to an oversized
                // range with "…this block range should work: [0x1000000, 0x1007fff]" - authoritative
                // and exact - so halving toward a number we were just handed wastes round trips
                // (RFC-0028 §3c). Anything unusable falls back to the midpoint.
                let split_at =
                    suggested_split_point(&e, from, to).unwrap_or(from + (to - from) / 2);
                let mut left =
                    fetch_logs_splitting_tracking(source, filter, from, split_at, true, widest)
                        .await?;
                let right =
                    fetch_logs_splitting_tracking(source, filter, split_at + 1, to, true, widest)
                        .await?;
                left.extend(right);
                Ok(left)
            }
            // Unclassifiable, but the window spans more than one block and we have a split to spend.
            Err(e) if speculative && from < to => {
                let mid = from + (to - from) / 2;
                tracing::debug!(
                    "getLogs {from}..={to} failed unclassifiably ({e:#}); splitting speculatively"
                );
                let left =
                    fetch_logs_splitting_tracking(source, filter, from, mid, false, widest).await;
                let right =
                    fetch_logs_splitting_tracking(source, filter, mid + 1, to, false, widest).await;
                match (left, right) {
                    (Ok(mut l), Ok(r)) => {
                        tracing::info!(
                            "getLogs {from}..={to} succeeded when split - the provider was refusing \
                             the range without saying so; treating it as a cap"
                        );
                        l.extend(r);
                        Ok(l)
                    }
                    // The split did not help, so the failure was never about size. Surface the
                    // *original* error - the halves' errors are the same fault seen twice.
                    _ => Err(e).with_context(|| format!("getLogs {from}..={to}")),
                }
            }
            Err(e) => Err(e).with_context(|| format!("getLogs {from}..={to}")),
        }
    })
}

/// Base backoff for a transient RPC failure during a seal-direct backfill window, and the ceiling its
/// doubling is capped at (#538).
///
/// A fixed attempt count used to give up after 5 tries spanning ~4s of backoff - shorter than even
/// an endpoint's ordinary 30s cooldown (`ENDPOINT_COOLDOWN_MS` in `rpc.rs`), let alone a 300s terminal
/// one, so it outlasted every retry and killed a multi-hour backfill over one bad window a bare
/// restart resumed past for free. There is no failure class here worth giving up on:
/// every caller of [`retry_transient`]/[`logs_with_retry`] is an RPC fetch on the sealed-history path,
/// the same class of failure `index_loop`'s tip-following getLogs fetch already retries forever (see
/// its `Err(e) => { warn!(...); sleep; }` arm) without anyone calling that a bug. So neither function
/// gives up any more; retrying in place - loud, on the same window, cursor never advancing past it -
/// is exactly what "no silent gap" requires, and it is exactly what a restart already proved safe.
///
/// #538 also reported a *second* apparent failure mode - a run dying right after the pool correctly
/// identified one endpoint as permanently bad (a 403) and cooled it down, despite a second, healthy
/// `--rpc` endpoint being configured. That is not a second bug: `RpcClient::call` already tries every
/// endpoint, cooling ones included, within a *single* `logs()`/`call()` invocation before giving up
/// (see `a_rejecting_endpoint_gets_the_long_cooldown_and_a_healthy_one_still_answers` in `rpc.rs`,
/// which proves a lone terminal failure never blocks the healthy endpoint from answering the same
/// call). For the whole call to fail, the "healthy" endpoint must *also* have failed on that
/// attempt - plausible and unremarkable under concurrency (a momentary rate-limit), and exactly the
/// same shape as the first failure mode: an outer ceiling shorter than the time an endpoint needs to
/// come back. Removing the ceiling here fixes both from one cause, with no change needed in `rpc.rs`.
/// **#863.** Consecutive attempts a window loop may spend without advancing its cursor before it stops
/// calling that normal.
///
/// The hazard is #672's own recorded failure: a request that fails whole and is retried forever at the
/// same width. The loop looks perfectly healthy - well-formed requests, orderly retry logs - and will
/// never finish. Two mutants reach that state cheaply (`is_result_too_large -> false` makes a provider
/// cap unrecognisable, so the narrowing retry never narrows), which is why they cost the nightly sweep
/// about 23 minutes each in timeouts instead of dying.
///
/// **Why 64.** It has to clear a legitimate narrowing descent, which halves from the window ceiling to
/// a single block: `log2(1_000_000)` is roughly 20 attempts, each possibly retrying internally with
/// backoff first. 64 is a shade over three times that, so a slow provider working its way down is never
/// mistaken for a stall, while a loop that genuinely cannot progress is named in seconds rather than
/// never. Deliberately not tight: a guard that fires on healthy behaviour gets raised until it means
/// nothing, which is exactly how the gates in #913 became decorative.
const NO_PROGRESS_LIMIT: usize = 64;

const BACKFILL_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(250);
const BACKFILL_RETRY_BACKOFF_CAP: std::time::Duration = std::time::Duration::from_secs(30);

/// The backoff for a given attempt: `base` doubling per attempt, capped at
/// [`BACKFILL_RETRY_BACKOFF_CAP`] so a long-stalled endpoint is polled steadily rather than at an
/// ever-growing interval.
fn backfill_backoff(base: std::time::Duration, attempt: usize) -> std::time::Duration {
    base.saturating_mul(1u32 << (attempt - 1).min(16))
        .min(BACKFILL_RETRY_BACKOFF_CAP)
}

/// Whether a backfill retry has earned the louder `error!` signal rather than the routine `warn!`:
/// only once the backoff has reached [`BACKFILL_RETRY_BACKOFF_CAP`] (the failure has outlasted a full
/// ordinary endpoint cooldown) *and* only once every ten attempts at that point (so it reads as a
/// periodic "still stuck" signal rather than a warn spammed every 30s forever). Split out as a pure
/// predicate - not just inlined in [`log_backfill_retry`] - so the escalation condition itself can be
/// asserted directly, without a `tracing` capture harness (unsafe to share across this crate's
/// parallel test run - see the comment on `with_default` in `analytics.rs`).
fn should_escalate_backfill_retry(backoff: std::time::Duration, attempt: usize) -> bool {
    backoff >= BACKFILL_RETRY_BACKOFF_CAP && attempt.is_multiple_of(10)
}

/// Log a backfill retry with the same escalating severity as [`escalate_stall`]: `warn!` on every
/// attempt (an operator watching the live progress line sees it moving again once this clears), and
/// `error!` once every ten attempts *once the backoff has reached its cap* - by then the failure has
/// outlasted a full ordinary endpoint cooldown, so it is worth a louder, less frequent signal that
/// something is genuinely stuck rather than merely slow, without spamming a warn every 30s forever.
fn log_backfill_retry(
    label: &str,
    attempt: usize,
    err: &anyhow::Error,
    backoff: std::time::Duration,
) {
    if should_escalate_backfill_retry(backoff, attempt) {
        tracing::error!(
            "{label} still failing after {attempt} attempts: {err:#}; retrying in {backoff:?} - \
             sealed history is safe and this resumes automatically once an endpoint recovers"
        );
    } else {
        tracing::warn!("{label} failed (attempt {attempt}): {err:#}; retrying in {backoff:?}");
    }
}

/// Retry a transient RPC operation forever, with capped exponential backoff (#538). A single attempt
/// already fails over across endpoints ([`RpcClient::call`]); this covers the case where *every*
/// endpoint is briefly unavailable at once - a shared rate-limit or a provider blip (e.g. a 403 from
/// one host while the others throttle under concurrency). Without it a single such window could abort
/// the whole seal-direct backfill; with it the window waits and retries, matching the tip loop's own
/// resilience to the identical failure class. `base` is parameterised so tests can pass `Duration::ZERO`.
async fn retry_transient<T, F, Fut>(label: &str, base: std::time::Duration, mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 1usize;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let backoff = backfill_backoff(base, attempt);
                log_backfill_retry(label, attempt, &e, backoff);
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

/// Fetch logs with the same never-give-up transient-retry as [`retry_transient`], but **pass a
/// result-cap error straight through** so the caller's own window-shrink logic handles it. The factory
/// backfill needs this because its cap strategy is an outer shrink (not the pipelined path's internal
/// split): "too many results" ⇒ shrink the window, "endpoint down" ⇒ back off and retry. Without it a
/// single transient RPC blip (a 521, an all-endpoints rate-limit) could abort a long factory backfill
/// mid-run - exactly what building the Uniswap-v3 nest surfaced, and exactly what #538 measured on a
/// real multi-hour run.
///
/// `base` is parameterised like [`retry_transient`] so tests can pass `Duration::ZERO`.
async fn logs_with_retry(
    source: &dyn Source,
    filter: &LogFilter,
    from: u64,
    to: u64,
    base: std::time::Duration,
) -> Result<Vec<crate::rpc::Log>> {
    let mut attempt = 1usize;
    loop {
        match source.logs(filter, from, to).await {
            Ok(l) => return Ok(l),
            // A result cap is not transient - hand it back so the caller shrinks the window. A
            // pool-wide 429 at a single block is not one of those (#916): there is nothing for the
            // caller to shrink, so it belongs in the backoff arm below like any other throttle.
            Err(e) if narrowing_can_help(&e, from, to) => return Err(e),
            Err(e) => {
                let backoff = backfill_backoff(base, attempt);
                log_backfill_retry(
                    &format!("factory getLogs {from}..={to}"),
                    attempt,
                    &e,
                    backoff,
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

/// Concurrent-fetch variant of [`backfill_direct`]: up to `concurrency` window fetches are in flight
/// at once (overlapping the RPC round-trip latency that dominates once the storage path is cheap),
/// while results are consumed strictly **in block order** - so the buffered rows, the batch
/// boundaries, and therefore the sealed segments are identical to the sequential path. `buffered`
/// preserves input order, which is what makes concurrency safe for content-addressed sealing.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_direct_pipelined(
    source: &dyn Source,
    registry: &DecodeRegistry,
    dir: &std::path::Path,
    addresses: &[String],
    topic0s: &[String],
    calls: &[crate::calls::CallDecl],
    state_rpc: Option<&crate::rpc::RpcClient>,
    chain_id: u64,
    from: u64,
    to: u64,
    window: u64,
    concurrency: usize,
    // Called after each segment seals, with the highest block now durably sealed - the caller
    // persists it as a resume watermark so a mid-backfill failure resumes here instead of restarting
    // from `from` (which would re-fetch, and on an adaptive path re-seal, already-sealed ranges).
    mut on_seal: impl FnMut(u64) -> Result<()>,
    // Called per completed window with (block reached, rows decoded) - drives the live progress line
    // (RFC-0015 slice 3). Fires every window, so a sparse range still shows honest block-position
    // movement between the (rare) seals. Pure presentation; must not touch stored state.
    mut on_progress: impl FnMut(u64, u64),
) -> Result<u64> {
    use futures::stream::StreamExt;

    // **Windows are generated lazily, not listed up front** (RFC-0029 §6f). The old code materialised
    // the whole range at a fixed width, which meant the `AdaptiveWindow` controller that
    // `backfill_direct` has always used was bypassed on the *concurrent* path - our fast path was the
    // one that could not adapt. On a long empty prefix (case 1's 0 → 19.89M) a fixed 10,000-block
    // window costs ~1,989 requests that return nothing, where a controller growing 4× per empty
    // response reaches the ceiling in a handful of steps.
    //
    // **This is only safe because seal boundaries are data-determined** (RFC-0028 §4): `take_sealable`
    // cuts at a row count and extends to a block boundary, reading nothing about the window width. So
    // varying the width cannot vary segment identity, and `pipelined_backfill_matches_sequential`
    // asserts exactly that - the two paths now adapt on *different* feedback sequences (this one
    // generates windows ahead of the results that would inform them) and must still seal identical
    // bytes. If that test ever fails, the boundary has stopped being data-determined; do not adjust
    // the test.
    // A `Mutex` rather than a `RefCell` because the whole backfill future is `tokio::spawn`ed and must
    // be `Send`. The generator and the consumer loop are in fact on the same task, so there is never
    // contention - but the compiler cannot know that, and one uncontended lock per window is free
    // beside an RPC round trip.
    // A blocks nest pays one header request per *block*, so its window ceiling is set by header cost
    // rather than log density (RFC-0036). Without this the zero-log ranges grow widest and demand the
    // most headers - which is how OBIB case 3 rate-limited itself into partial responses.
    let chunker = std::sync::Arc::new(std::sync::Mutex::new(if registry.blocks() {
        AdaptiveWindow::for_window_with_headers(window)
    } else {
        AdaptiveWindow::for_window(window)
    }));
    // The generator *owns* a handle rather than borrowing one. Borrowing across the generator's await
    // makes the whole backfill future carry a higher-ranked lifetime that `tokio::spawn` cannot
    // satisfy - which shows up far from here, as a "one type is more general than the other" error on
    // the `index_loop` spawn.
    // Built once for the whole backfill: the filter is fixed for the run, and `LogFilter::new`
    // returning `None` is the contract-free nest that must not fetch at all (#432).
    let filter = LogFilter::new(addresses, topic0s);
    let filter = &filter;
    let windows = futures::stream::unfold(
        (from, chunker.clone(), false),
        move |(next, ch, final_done)| async move {
            if next > to {
                if final_done {
                    return None;
                }
                // The last window has no window after it to refetch its tail (#1144): one more
                // window over the range's last `FETCH_TAIL_OVERLAP` blocks, flagged so it is not
                // widened again.
                return Some(((overlap_from(to + 1, from), to, true), (to + 1, ch, true)));
            }
            let w = ch.lock().expect("window controller").window();
            let chunk_to = (next.saturating_add(w - 1)).min(to);
            Some(((next, chunk_to, false), (chunk_to + 1, ch, final_done)))
        },
    );

    // Each window future fetches logs + timestamps and returns its decoded rows as JSON. Borrows
    // (`source`, `registry`, filters) are shared across the concurrent futures - fine, they run on
    // one task; `buffered` yields them back in window order.
    let stream = windows
        .map(|(w_from, w_to, final_pass)| async move {
            // Split-and-retry on a provider result cap instead of aborting the whole backfill (H2/H3),
            // and retry the whole fetch on a transient all-endpoints failure (rate-limit / provider
            // blip) so one bad window doesn't abort the run.
            // Non-zero only when the splitter actually had to recover (#672): the width that came
            // back, which is narrower than the one asked for. Zero means "no signal", and the last
            // window of a run - truncated by the end of the range rather than by the provider - must
            // produce no signal, or every run would cap itself on its final chunk.
            let mut served_width = 0u64;
            let mut whole_width = 0u64;
            // The tail of the previous window is asked for again (#1144).
            let fetch_from = if final_pass {
                w_from
            } else {
                overlap_from(w_from, from)
            };
            let logs = match &filter {
                // Nothing to match on either half means nothing to ask for - and asking anyway is
                // asking for every log on the chain (#432). The window still flows through the rest
                // of the pipeline, because a `blocks` nest derives its rows from the window itself.
                None => Vec::new(),
                Some(f) => {
                    // Tracked, so the controller learns what actually worked (#672). Without it the
                    // window grows on a success the splitter manufactured by cutting the range up.
                    let (logs, served) = retry_transient(
                        &format!("seal-direct getLogs {fetch_from}..={w_to}"),
                        BACKFILL_RETRY_BASE,
                        || fetch_logs_splitting_tracked(source, f, fetch_from, w_to),
                    )
                    .await?;
                    if served < w_to - fetch_from + 1 {
                        served_width = served;
                    } else {
                        whole_width = served;
                    }
                    logs
                }
            };
            // **The controller is fed raw logs, not decoded rows.** It is sizing a *response*, and a
            // log that matches no decoder still costs bytes on the wire and still counts against the
            // provider's result cap. Feeding it `rows.len()` would make a nest with a narrow event
            // allowlist - `events = ["Transfer"]` on a chatty contract - see almost every window as
            // empty and grow to the ceiling against genuinely dense history, which is the one place an
            // oversized window actually hurts.
            let fetched = logs.len() as u64;
            let mut rows: Vec<_> = logs
                .iter()
                .filter_map(|log| match registry.decode(log) {
                    Ok(Some(r)) => Some(r),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::debug!("decode skipped: {e:#}");
                        None
                    }
                })
                .collect();
            let mut blocks: Vec<u64> = rows.iter().map(|r| r.block_number).collect();
            // Include blocks sampled by [[calls]] so their timestamps are available for call rows.
            if state_rpc.is_some() {
                for d in calls {
                    blocks.extend(d.blocks_in(w_from, w_to));
                }
            }
            blocks.sort_unstable();
            blocks.dedup();
            let ts = retry_transient(
                &format!("seal-direct block_timestamps {w_from}..={w_to}"),
                BACKFILL_RETRY_BASE,
                || fetch_timestamps(source, registry, &blocks),
            )
            .await?;
            // Seal in canonical (block, log_index) order, not RPC-provider order, so a segment's bytes
            // (and its content address) are identical across providers - see `backfill_direct`.
            rows.sort_by_key(|r| (r.block_number, r.log_index));
            for r in &mut rows {
                r.block_timestamp = ts.get(&r.block_number).copied().unwrap_or(0);
            }
            // RFC-0023 tier-3: resolve declared [[calls]] and merge so sealed segments match the hot path.
            if let Some(rpc) = state_rpc {
                let call_rows = resolve_calls_for_window(
                    source,
                    calls,
                    rpc,
                    chain_id,
                    &rows,
                    w_from,
                    w_to,
                    &ts,
                    registry.timestamps(),
                )
                .await?;
                rows.extend(call_rows);
                rows.sort_by_key(|r| (r.block_number, r.log_index));
            }
            // Carry each row's block so the consumer can seal on a data-determined boundary
            // (RFC-0028 §4) instead of at whichever window filled the buffer.
            let mut json: Vec<SealRow> = rows
                .iter()
                .map(|r| (r.block_number, r.log_index, r.to_json().to_string()))
                .collect();
            // RFC-0036 §4.2: one row per block in the window. Enumerated from the **window**, not
            // from `rows` - a blocks table has to cover blocks that emitted nothing, and OBIB case 3
            // is 100,001 blocks with no contract in the nest at all.
            if registry.blocks() {
                let want: Vec<u64> = (w_from..=w_to).collect();
                let headers = retry_transient(
                    &format!("seal-direct block_headers {w_from}..={w_to}"),
                    BACKFILL_RETRY_BASE,
                    || source.block_headers(&want),
                )
                .await?;
                let mut block_rows: Vec<_> = want
                    .iter()
                    .filter_map(|b| {
                        headers
                            .get(b)
                            .and_then(|h| crate::registry::block_row(*b, h, registry.timestamps()))
                    })
                    .collect();
                block_rows.sort_by_key(|r| r.block_number);
                json.extend(
                    block_rows
                        .iter()
                        .map(|r| (r.block_number, r.log_index, r.to_json().to_string())),
                );
            }
            Ok::<(u64, u64, bool, u64, u64, u64, Vec<SealRow>), anyhow::Error>((
                fetch_from,
                w_to,
                final_pass,
                fetched,
                served_width,
                whole_width,
                json,
            ))
        })
        .buffered(concurrency.max(1));
    // `unfold`'s generator future is not `Unpin` (it borrows `chunker` across an await), so the stream
    // has to be pinned before it can be polled in a loop.
    let mut stream = std::pin::pin!(stream);

    // `(block, json)` so a segment can end on a data-determined block boundary (RFC-0028 §4).
    let mut buf: Vec<SealRow> = Vec::new();
    let mut batch_from = from;
    let mut total = 0u64;
    while let Some(res) = stream.next().await {
        let (fetch_from, w_to, final_pass, fetched, served_width, whole_width, json) = res?;
        // Feedback lags by up to `concurrency` windows - those are already in flight when this one
        // lands. That is fine and is not worth engineering away: the controller is damped to 4× per
        // step anyway, so a lag of a few windows costs a few steps of convergence, and the alternative
        // (waiting for feedback before generating the next window) is just the sequential path.
        {
            let mut ctl = chunker.lock().expect("window controller");
            // Order matters: cap first, then size within the cap. `observed(0)` on an empty window
            // grows fourfold, and doing that before the cap would let the window spend one more
            // round asking for a width the provider has already refused (#672).
            if whole_width > 0 {
                ctl.served_whole(whole_width);
            }
            if served_width > 0 {
                ctl.served_by_splitting(served_width);
            }
            ctl.observed(fetched);
        }
        // Windows complete in order (`buffered`, not `buffer_unordered`), so a window's refetched
        // tail is merged after the previous window's own rows are in the buffer (#1144).
        let n = merge_window_rows(&mut buf, fetch_from, json)?;
        total += n;
        on_progress(w_to, n);
        // `drain_all_sealable` is `while`, not `if` (#980, #1015) - see the note on the
        // direct path: one seal per chunk makes segment identity depend on the operator's window.
        drain_all_sealable(&mut buf, tail_hold(w_to, final_pass), |rows, seal_to| {
            seal::seal_range(dir, &rows, batch_from, seal_to)?;
            batch_from = seal_to + 1;
            on_seal(seal_to)?;
            Ok(())
        })?;
    }
    if !buf.is_empty() {
        seal::seal_range(dir, &drain_sealable(&mut buf), batch_from, to)?;
        on_seal(to)?;
    }
    Ok(total)
}

/// Factory-aware sequential seal-direct backfill (RFC-0009 §3). Per chunk, two passes: pass 1 fetches
/// with the current address filter (base contracts + children discovered so far) and updates the
/// child registry from the factory events it decodes; pass 2 (a fixpoint loop, for nested factories
/// within one chunk) re-fetches the same range for *only* the newly discovered children. All logs are
/// then decoded together with the full registry, stamped, sorted by `(block, log_index)`, and sealed -
/// so the segments are deterministic and (step 3a) will match the pipelined path byte-for-byte. Uses
/// the efficient address filter, not the tip loop's topic0-only fetch. Grows `children`.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_direct_factory(
    source: &dyn Source,
    registry: &DecodeRegistry,
    factory: &FactorySet,
    children: &mut ChildRegistry,
    dir: &std::path::Path,
    topic0s: &[String],
    calls: &[crate::calls::CallDecl],
    state_rpc: Option<&crate::rpc::RpcClient>,
    chain_id: u64,
    from: u64,
    to: u64,
    window: u64,
    force_topic0: bool,
    // Resume watermark callback - see [`backfill_direct_pipelined`]. The factory path uses an adaptive
    // window (non-deterministic boundaries), so resuming from the last sealed block instead of `from`
    // is what prevents a re-run from re-sealing overlapping ranges under new hashes (duplicate data).
    mut on_seal: impl FnMut(u64) -> Result<()>,
    // Per-chunk live progress (RFC-0015 slice 3): (block reached, rows decoded). See the pipelined path.
    mut on_progress: impl FnMut(u64, u64),
) -> Result<u64> {
    use std::collections::HashSet;
    let base: Vec<String> = registry
        .addresses()
        .iter()
        .map(|a| format!("0x{}", hex::encode(a)))
        .collect();
    let empty_ts = std::collections::HashMap::new();

    // `(block, json)` so a segment can end on a data-determined block boundary (RFC-0028 §4).
    let mut buf: Vec<SealRow> = Vec::new();
    let mut batch_from = from;
    let mut next = from;
    let mut total = 0u64;
    let mut flipped_logged = false;
    // A blocks nest pays per *block*, not per log, so its window ceiling is different (RFC-0036).
    let mut chunker = if registry.blocks() {
        AdaptiveWindow::for_window_with_headers(window)
    } else {
        AdaptiveWindow::for_window(window)
    };
    // Labelled because the two-pass body below has to be able to restart the *chunk* from inside the
    // pass-2 fixpoint loop when the provider refuses an over-large response.
    // See `backfill_direct`: the range's last `FETCH_TAIL_OVERLAP` blocks are fetched once more as
    // a window of their own before the final flush (#1144).
    let mut final_pass = false;
    'chunk: while next <= to {
        let chunk_to = (next + chunker.window() - 1).min(to);
        let fetch_from = if final_pass {
            next
        } else {
            overlap_from(next, from)
        };

        // Filter flip (RFC-0009 §4): a forced override or a discovered set past the threshold switches
        // this chunk from the address-list two-pass to a single topic0-only fetch + local filtering.
        let use_topic0 = force_topic0 || base.len() + children.len() > FACTORY_FLIP_THRESHOLD;
        if use_topic0 && !flipped_logged {
            tracing::info!(
                "factory backfill filter flipped to topic0-only + local filter ({} children)",
                children.len()
            );
            flipped_logged = true;
        }

        let mut all_logs;
        if use_topic0 {
            // Topic0-only: every matching log (contract + all children) is in hand in one fetch, so
            // there is no second pass; `decode_window` filters locally by registry membership.
            // A factory nest with no events declared has nothing to match on either half, and asking
            // for that asks for every log on the chain (#432). An empty window falls through to the
            // per-window tail unchanged, as it does in `backfill_direct`: the decode below discovers
            // nothing from no logs, and the seal/progress bookkeeping still has to run.
            all_logs = match LogFilter::new(&[], topic0s) {
                None => Vec::new(),
                Some(wide) => {
                    match logs_with_retry(source, &wide, fetch_from, chunk_to, BACKFILL_RETRY_BASE)
                        .await
                    {
                        Ok(l) => {
                            chunker.observed(l.len() as u64);
                            l
                        }
                        Err(e) if narrowing_can_help(&e, next, chunk_to) => {
                            if next >= chunk_to {
                                return Err(e).with_context(|| single_block_over_cap(next));
                            }
                            chunker.too_large();
                            continue;
                        }
                        Err(e) => {
                            return Err(e).with_context(|| format!("getLogs {next}..={chunk_to}"));
                        }
                    }
                }
            };
            let _ = decode_window(registry, Some(factory), children, &all_logs, &empty_ts);
        } else {
            // Pass 1: current filter = base contracts + all children discovered so far.
            let mut fetched: HashSet<String> =
                base.iter().map(|s| s.to_ascii_lowercase()).collect();
            let mut current: Vec<String> = base.clone();
            for c in children.addresses() {
                if fetched.insert(c.to_ascii_lowercase()) {
                    current.push(c.to_string());
                }
            }
            // No base contract, no child discovered yet and no topic0 either: nothing this chunk
            // could match, and the unfiltered fetch it would otherwise issue means every log on the
            // chain (#432). Empty window, same fall-through as above.
            let logs1 = match LogFilter::new(&current, topic0s) {
                None => Vec::new(),
                Some(pass1) => {
                    match logs_with_retry(source, &pass1, fetch_from, chunk_to, BACKFILL_RETRY_BASE)
                        .await
                    {
                        Ok(l) => {
                            chunker.observed(l.len() as u64);
                            l
                        }
                        Err(e) if narrowing_can_help(&e, next, chunk_to) => {
                            if next >= chunk_to {
                                return Err(e).with_context(|| single_block_over_cap(next));
                            }
                            chunker.too_large();
                            continue; // retry the same range with a smaller window
                        }
                        Err(e) => {
                            return Err(e).with_context(|| format!("getLogs {next}..={chunk_to}"));
                        }
                    }
                }
            };
            all_logs = logs1;
            // Decode to discover children (rows discarded here; the authoritative decode is below once
            // every child in this chunk is known and timestamps are in hand).
            let _ = decode_window(registry, Some(factory), children, &all_logs, &empty_ts);

            // Pass 2+ (fixpoint): re-fetch the chunk for children discovered here but not yet fetched.
            loop {
                let new: Vec<String> = children
                    .addresses()
                    .iter()
                    .filter(|c| !fetched.contains(&c.to_ascii_lowercase()))
                    .map(|c| c.to_string())
                    .collect();
                if new.is_empty() {
                    break;
                }
                for c in &new {
                    fetched.insert(c.to_ascii_lowercase());
                }
                // `new` is non-empty two lines up, so the filter has an address half and cannot be
                // the every-log-on-the-chain request that `LogFilter` refuses to build.
                let child_filter = LogFilter::new(&new, topic0s)
                    .expect("child filter has a non-empty address list");
                let more = match logs_with_retry(
                    source,
                    &child_filter,
                    fetch_from,
                    chunk_to,
                    BACKFILL_RETRY_BASE,
                )
                .await
                {
                    Ok(l) => l,
                    // The same provider refusal pass 1 already shrinks for. Pass 2 asks a *harder*
                    // question than pass 1 - the children discovered in this very chunk are the
                    // busiest ones, freshly created and trading - so it is the pass more likely to
                    // blow a response cap, and it was the only one of the nine sites in this file
                    // that treated the cap as fatal. Measured on `uniswap-v2` mainnet: pass 1
                    // succeeded at a 19,936-block window and pass 2 died on it.
                    //
                    // Restarting the chunk is safe rather than merely convenient: `fetched` is
                    // rebuilt per iteration, and a child already registered by pass 1's decode stays
                    // registered. A child cannot emit before the block that created it, so carrying
                    // it into a narrower window that ends before its creation costs nothing.
                    Err(e) if narrowing_can_help(&e, next, chunk_to) => {
                        if next >= chunk_to {
                            return Err(e).with_context(|| single_block_over_cap(next));
                        }
                        chunker.too_large();
                        continue 'chunk;
                    }
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("getLogs (children) {next}..={chunk_to}"));
                    }
                };
                let _ = decode_window(registry, Some(factory), children, &more, &empty_ts);
                all_logs.extend(more);
            }
        }

        // Authoritative decode with the full child set, then headers only for kept rows (#765).
        let mut rows = decode_window(registry, Some(factory), children, &all_logs, &empty_ts);
        let extra = if state_rpc.is_some() {
            calls
                .iter()
                .flat_map(|d| d.blocks_in(next, chunk_to))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let blocks = blocks_needing_timestamps(&rows, extra);
        let ts = retry_transient(
            &format!("factory block_timestamps {next}..={chunk_to}"),
            BACKFILL_RETRY_BASE,
            || fetch_timestamps(source, registry, &blocks),
        )
        .await?;
        apply_row_timestamps(&mut rows, &ts);
        children.apply_timestamps(&ts);
        // RFC-0023 tier-3: resolve declared [[calls]] and merge so sealed segments match the hot path.
        if let Some(rpc) = state_rpc {
            let call_rows = resolve_calls_for_window(
                source,
                calls,
                rpc,
                chain_id,
                &rows,
                next,
                chunk_to,
                &ts,
                registry.timestamps(),
            )
            .await?;
            rows.extend(call_rows);
            rows.sort_by_key(|r| (r.block_number, r.log_index));
        }
        let row_count = merge_window_rows(
            &mut buf,
            fetch_from,
            rows.iter()
                .map(|r| (r.block_number, r.log_index, r.to_json().to_string())),
        )?;
        total += row_count;
        next = chunk_to + 1;
        on_progress(chunk_to, row_count);
        if next > to && !final_pass {
            final_pass = true;
            next = overlap_from(to + 1, from);
            continue 'chunk;
        }

        // Data-determined seal boundary (RFC-0028 §4), same as the non-factory path - including
        // `drain_all_sealable`'s `while` rather than `if` (#980, #1015), so segment identity does
        // not depend on `--window`.
        drain_all_sealable(
            &mut buf,
            tail_hold(chunk_to, final_pass),
            |rows, seal_to| {
                // Stamp the discovered-child set that produced these rows (RFC-0009 step 4).
                seal::seal_range_with_snapshot(
                    dir,
                    &rows,
                    batch_from,
                    seal_to,
                    Some(&children.hash()),
                )?;
                batch_from = seal_to + 1;
                on_seal(seal_to)?;
                Ok(())
            },
        )?;
        if next > to && !buf.is_empty() {
            seal::seal_range_with_snapshot(
                dir,
                &drain_sealable(&mut buf),
                batch_from,
                to,
                Some(&children.hash()),
            )?;
            batch_from = next;
            on_seal(to)?;
        }
    }
    Ok(total)
}

/// All the per-nest state the tip-following loop owns and mutates, extracted from `index_loop`'s
/// argument list so a later change can drive many nests from one cursor (RFC-0012). This is a pure
/// mechanical grouping - the loop's behaviour is unchanged. The `Source` is deliberately NOT a field:
/// it is shared (`Arc<dyn Source>`) and stays borrowed into the two methods below.
/// Where a direct seal's watermark goes: the progress counter, the `sealed_through` gauge, and the
/// store's durable key, in that order. Every place that states "sealed through" reads one of the
/// three. The gauge is the one that was missing (#1163): `/ready` and `nuthatch_sealed_through` read
/// it, the nest seeds it from the store at construction, before the direct seal has written anything,
/// and only the tip-follower's own seal ever set it afterwards. On 8107 that meant 460M sealed
/// blocks reporting `sealed_through 0` for the hours until the finalized head crossed the hot store's
/// start; an operator's switch script read that as an unfit nest.
fn publish_direct_seal(
    metrics: &crate::metrics::NestMetrics,
    store: &dyn crate::store::HotStore,
    sealed_to: u64,
) -> Result<()> {
    metrics.set_seal_direct_completed(sealed_to);
    metrics.set_sealed_through(sealed_to);
    store.set_meta(SEALED_THROUGH_KEY, &sealed_to.to_string())
}

pub struct NestIngest {
    /// The nest's configured name - the label a quarantine is reported under (RFC-0026 §5) and the
    /// key its metrics are already registered by.
    name: String,
    dir: PathBuf,
    store: Arc<dyn crate::store::HotStore>,
    registry: Arc<DecodeRegistry>,
    balances: BalanceView,
    exposure: ExposureView,
    velocity: VelocityView,
    /// The nest's authored incremental entities (RFC-0041 §5.1), one circuit each.
    ///
    /// `Arc` rather than `Clone` because an `EntityView` owns a thread and a channel: two copies of
    /// the handle would be two writers to one circuit, and the whole design is one cursor, one
    /// writer, one observable failure boundary. Empty for the ordinary nest that declares none.
    entities: Arc<Vec<EntityView>>,
    labels: Arc<LabelSet>,
    screener: Arc<Option<LiveScreener>>,
    threshold: Option<i128>,
    velocity_cfg: Option<(i128, u64)>,
    router: Arc<AlertRouter>,
    webhooks: Arc<Vec<crate::config::Webhook>>,
    factory: Option<Arc<FactorySet>>,
    children: ChildRegistry,
    finality: Finality,
    /// Per-nest metrics handle (SEC-9): nest-scoped updates go here, which also feed the process-global
    /// aggregates. In a runtime each nest gets its own, keyed by name.
    metrics: Arc<crate::metrics::NestMetrics>,
    addresses: Vec<String>,
    topic0s: Vec<String>,
    /// The nest's earliest vendored deployment block (the min of the contracts' `start_block`s), or
    /// `None`. Used only by [`prepare`]'s cold-start origin computation.
    start_block: Option<u64>,
    /// RFC-0037: declared IPFS resolutions, in config order - which fixes each row's slot in the
    /// reserved band and therefore its key.
    ipfs: Vec<crate::ipfs::IpfsDecl>,
    /// The gateways (or local node) declared resolutions are fetched through. Never part of the
    /// nest's identity: a gateway is an access path, and content addressing is what makes two
    /// operators' answers comparable regardless of which one they used.
    ipfs_gateways: Vec<String>,
    /// RFC-0038 §5: whether this nest decodes **top-level calls** - transactions sent directly to its
    /// contracts, which is what a subgraph's `callHandlers` fire on.
    top_level_calls: bool,
    /// RFC-0038 §5: the call-decode surface, present when the nest sets `[extract] top_level_calls`
    /// or `traces`. `None` when it decodes events only, which is the default and the common case.
    call_registry: Option<Arc<crate::calldata::CallRegistry>>,
    /// The nest's chain id, needed to key a tier-3 call result: a `CallKey` is
    /// `(chain, block, contract, calldata)`, and omitting the chain would make two chains' answers
    /// to the same question share one content address.
    chain_id: u64,
    /// RFC-0023 tier 3: the nest's declared pinned reads, in config order. That order fixes each
    /// row's `log_index` inside the reserved band, so it is identity, not presentation.
    calls: Vec<crate::calls::CallDecl>,
    /// The **operator-supplied archive endpoint** a declared call is resolved against (`--state-rpc`).
    ///
    /// Deliberately not the ingestion `Source`: a source may be an ExEx, a mock, or a pool of
    /// pruned endpoints that serve logs perfectly well and cannot answer a historical `eth_call`.
    /// Tier 3 needs archive state and RFC-0024 is explicit that it comes from the operator, so it is
    /// its own client and its absence is refused at startup rather than discovered mid-backfill.
    state_rpc: Option<Arc<crate::rpc::RpcClient>>,
}

impl NestIngest {
    /// Run the one-time preamble before the tip loop, then return the block to begin tip-following
    /// from. Initialises webhook cursors, rebuilds the discovered-child registry on a warm restart,
    /// runs the `--seal-direct` phase-0 backfill on a cold start, and computes the cold-start `next`.
    /// Extracted verbatim from `index_loop` so a runtime can build many `NestIngest`s and drive them
    /// through the same code; `source` stays borrowed (not owned) and `window` is the chunker seed the
    /// phase-0 backfill uses.
    async fn prepare(
        &mut self,
        source: &dyn Source,
        backfill: Option<u64>,
        seal_direct: bool,
        concurrency: usize,
        window: u64,
    ) -> Result<u64> {
        // User webhooks (RFC-0010 Part B): initialise each subscription's cursor before any sealing, so a
        // `since = "registration"` webhook starts at the tip and a `--seal-direct` backfill doesn't fire
        // its history. Best-effort - a tip lookup failure just defers registration to the first live tip.
        if !self.webhooks.is_empty() {
            if let Ok(tip) = source.tip().await {
                if let Err(e) = crate::webhooks::init_cursors(&self.store, &self.webhooks, tip) {
                    tracing::warn!("webhook cursor init failed: {e:#}");
                }
            }
        }

        // The discovered-child registry (RFC-0009). Empty for a static nest; for a factory nest it is
        // rebuilt from stored factory events on a warm restart (a pure fold - determinism preserved) and
        // grown inline as the loop decodes new factory events.
        if let Some(fs) = self.factory.as_deref() {
            if self.store.get_meta(LAST_BLOCK_KEY)?.is_some() {
                // Propagated, not defaulted (#373): a rebuild that cannot read its own stored
                // factory events must fault the nest into quarantine, not start it watching a
                // silently-short set of children.
                self.children = rebuild_children(&self.dir, &self.store, &self.registry, fs)?;
                if !self.children.is_empty() {
                    tracing::info!(
                        "rebuilt child registry: {} discovered child contract(s)",
                        self.children.len()
                    );
                }
            }
        }
        // Phase 0 (cold start, `--seal-direct`): fast-seal the finalized history straight to Parquet,
        // bypassing the hot store, then rebuild the IVM view from those segments. The tip-following loop
        // below picks up from where this left off and handles the near-tip (un-finalized) window the
        // normal way. Nothing here can reorg - it is all strictly past finality.
        if seal_direct && self.store.get_meta(LAST_BLOCK_KEY)?.is_none() {
            let tip = source.tip().await.map_err(|e| {
                anyhow::Error::new(ColdStartUnreachable(format!(
                    "cold-start tip lookup failed: {e:#}"
                )))
            })?;
            let origin = cold_start_block(self.start_block, backfill, tip);
            let finalized_tag = match self.finality {
                Finality::FinalizedTag { .. } => source.finalized().await.ok().flatten(),
                Finality::Depth(_) => None,
            };
            let finalized_through = seal_ceiling(self.finality, tip, finalized_tag);
            // Resume a partial backfill instead of restarting from `origin`. A mid-backfill failure (a
            // transient RPC error) leaves `SEALED_THROUGH` at the last durably-sealed block but `LAST_BLOCK`
            // unset, so we re-enter here; resuming from the watermark re-fetches nothing already sealed -
            // which on the adaptive factory path also avoids re-sealing overlapping ranges under fresh
            // content hashes (duplicate, permanently double-counted segments). A fresh start has no
            // watermark and resumes from `origin`.
            let sealed_watermark = self
                .store
                .get_meta(SEALED_THROUGH_KEY)?
                .and_then(|s| s.parse::<u64>().ok());
            let resume_from = resume_from_watermark(sealed_watermark, origin);
            if resume_from <= finalized_through {
                // Record where the backfill *began* once; a resume keeps the original origin.
                if self.store.get_meta(START_BLOCK_KEY)?.is_none() {
                    self.store.set_meta(START_BLOCK_KEY, &origin.to_string())?;
                }
                if resume_from > origin {
                    tracing::info!(
                        "resuming seal-direct backfill from block {resume_from} (a prior run sealed through {})",
                        resume_from - 1
                    );
                }
                // Persist the sealed watermark after every segment, so the backfill is resumable rather
                // than all-or-nothing (deadlock-review finding C1).
                self.metrics
                    .begin_seal_direct(resume_from, finalized_through);
                let store = self.store.clone();
                let metrics = self.metrics.clone();
                let on_seal = {
                    let metrics = metrics.clone();
                    let store = store.clone();
                    move |sealed_to: u64| {
                        metrics.mark_poll_ok();
                        publish_direct_seal(&metrics, store.as_ref(), sealed_to)
                    }
                };
                // Live feedback for the multi-minute bulk seal (RFC-0015 slice 3).
                let mut prog = crate::progress::Backfill::new(
                    "sealing history",
                    resume_from,
                    finalized_through,
                );
                // A factory nest backfills with the sequential two-pass (RFC-0009 §3, address-filtered,
                // efficient, deterministic). Factory backfill is sequential regardless of `--concurrency`:
                // the child-event bulk is inherently ordered until the step-5 topic0-flip makes filters
                // version-independent, so pipelining below the flip buys little (RFC-0009 §3 risk note). A
                // static nest uses the pipelined path as before.
                let sealed = if let Some(fs) = self.factory.as_deref() {
                    if concurrency > 1 {
                        tracing::info!(
                            "factory backfill runs sequentially (--concurrency {concurrency} ignored until the step-5 filter flip)"
                        );
                    }
                    tracing::info!(
                        "seal-direct factory backfill: {resume_from}..={finalized_through} (tip {tip}, sequential two-pass)…"
                    );
                    backfill_direct_factory(
                        source,
                        &self.registry,
                        fs,
                        &mut self.children,
                        &self.dir,
                        &self.topic0s,
                        &self.calls,
                        self.state_rpc.as_deref(),
                        self.chain_id,
                        resume_from,
                        finalized_through,
                        window,
                        fs.force_topic0(),
                        on_seal,
                        |blk, n| {
                            metrics.set_seal_direct_completed(blk);
                            prog.tick(blk, n);
                        },
                    )
                    .await?
                } else {
                    tracing::info!(
                        "seal-direct backfill: {resume_from}..={finalized_through} (tip {tip}, {concurrency}-way)…"
                    );
                    backfill_direct_pipelined(
                        source,
                        &self.registry,
                        &self.dir,
                        &self.addresses,
                        &self.topic0s,
                        &self.calls,
                        self.state_rpc.as_deref(),
                        self.chain_id,
                        resume_from,
                        finalized_through,
                        window,
                        concurrency,
                        on_seal,
                        |blk, n| {
                            metrics.set_seal_direct_completed(blk);
                            prog.tick(blk, n);
                        },
                    )
                    .await?
                };
                let _ = sealed;
                prog.finish(finalized_through, false);
                self.metrics.end_seal_direct();
                publish_direct_seal(&self.metrics, self.store.as_ref(), finalized_through)?;
                self.store
                    .set_meta(LAST_BLOCK_KEY, &finalized_through.to_string())?;
                if let Err(e) = rebuild_views(
                    &self.dir,
                    &self.store,
                    &self.registry,
                    &DerivedViews {
                        labels: &self.labels,
                        balances: &self.balances,
                        exposure: &self.exposure,
                        velocity: &self.velocity,
                        velocity_window: self.velocity_cfg.map(|(_, w)| w),
                    },
                ) {
                    tracing::warn!("view rebuild after seal-direct failed: {e:#}");
                }
                // Fire webhooks for the freshly-sealed history (a `since = "genesis"`/block webhook wants
                // it; a `since = "registration"` one is cursored past it, so this is a no-op there).
                if !self.webhooks.is_empty() {
                    if let Err(e) = crate::webhooks::deliver_sealed(
                        &self.store,
                        &self.dir,
                        &self.webhooks,
                        finalized_through,
                    ) {
                        tracing::warn!("webhook delivery after seal-direct failed: {e:#}");
                    }
                }
            }
        }

        // Resume from the last committed block; on a cold start, backfill from the nest's earliest
        // vendored deployment block (full history) if it has one, else from `--backfill` behind the tip.
        let next = match self.store.get_meta(LAST_BLOCK_KEY)? {
            Some(v) => v.parse::<u64>().context("corrupt last_block")? + 1,
            None => {
                let tip = source.tip().await.map_err(|e| {
                    anyhow::Error::new(ColdStartUnreachable(format!(
                        "cold-start tip lookup failed: {e:#}"
                    )))
                })?;
                let start = cold_start_block(self.start_block, backfill, tip);
                self.store.set_meta(START_BLOCK_KEY, &start.to_string())?;
                let src = if backfill.is_none() && self.start_block.is_some() {
                    " (from deployment)"
                } else {
                    ""
                };
                tracing::info!("cold start: backfilling from block {start}{src} (tip {tip})");
                start
            }
        };
        Ok(next)
    }

    /// Does this log belong to this nest? Two demux modes, mirroring the two nest kinds:
    /// - **Static nest** (non-empty address filter): by emitting address - the runtime fetches the union
    ///   of every nest's addresses and each log routes to the nest(s) whose set contains it.
    /// - **Factory nest** (empty address filter - topic0-only, children discovered at runtime, RFC-0009):
    ///   by **topic0** - a child contract has an arbitrary address but its events carry a *template*
    ///   topic0 in this nest's set, so topic0 routing catches children (and factory-creation events)
    ///   regardless of address; `process_window`'s inline discovery then adopts them.
    ///
    /// Case-insensitive throughout (a provider may return checksummed hex while our filter is lowercase).
    /// Decode is the safety net either way - an over-routed log only yields rows this nest's registry
    /// (or discovered children) actually know, so per-nest output stays byte-identical to solo.
    fn owns(&self, log: &crate::rpc::Log) -> bool {
        log_owned(&self.addresses, &self.topic0s, log)
    }

    /// Detect and handle a reorg against the last committed block. Returns `Ok(Some(next))` - the
    /// block the caller should continue from - when a reorg was handled, `Ok(None)` when the chain
    /// stayed canonical (or there is nothing to check yet), and propagates the finality-violation
    /// `bail!` unchanged.
    async fn handle_reorg(&mut self, source: &dyn Source, next: u64) -> Result<Option<u64>> {
        // Reorg check: has the last block we committed against stayed canonical? If not, the
        // mutable hot store rolls back to the deepest surviving checkpoint (the only place a
        // reorg ever lands - sealed segments, once they exist, are strictly past finality).
        if next == 0 {
            return Ok(None);
        }
        match detect_reorg(source, &self.store, next - 1).await {
            Ok(Some(ancestor)) => {
                // Drop any block-number-keyed cache above the fork (RFC-0029 §6d) *before* rolling
                // back. The timestamp cache is keyed by height, and every block above `ancestor` has
                // just been replaced - so a re-index would otherwise re-seal the **pre-reorg**
                // timestamps and produce segments a re-execution against the canonical chain could not
                // reproduce.
                source.forget_cached_above(ancestor);
                self.rollback_reorg(ancestor)?;
                Ok(Some(ancestor + 1))
            }
            Ok(None) => Ok(None),
            Err(e) => {
                tracing::debug!("reorg check skipped: {e:#}");
                Ok(None)
            }
        }
    }

    /// Roll this nest's mutable hot store + IVM views back to `ancestor` (the deepest reorg-survivor
    /// block). Detection is the *caller's* job: a solo nest detects on its own cursor (`handle_reorg`);
    /// a runtime detects **once** at the shared boundary and fans this out to every nest (slice 3). A
    /// nest already at or below `ancestor` (e.g. a still-backfilling nest in a runtime while the tip
    /// reorgs) is a no-op - nothing above `ancestor` to undo, and its cursor must NOT be bumped up to
    /// `ancestor` (that would claim blocks it never indexed). Propagates the finality-violation bail.
    /// Fail loudly if any derived-view circuit thread has died. A dead IVM thread silently freezes the
    /// balance/exposure/velocity views while ingest keeps committing and serving - so a dead thread must
    /// surface as a fatal ingest error, never be served over (audit: correctness M1). A disabled view
    /// (no labels / no velocity flag) reports healthy, so this only bites a genuinely crashed circuit.
    fn ensure_views_healthy(&self) -> Result<()> {
        // Terminal (RFC-0026 §3): a crashed circuit thread cannot be revived in-process, so this nest
        // is quarantined until restart rather than retried.
        if !self.balances.is_healthy() {
            anyhow::bail!(TerminalFault(
                "the balance IVM circuit thread has died - refusing to serve a frozen view".into()
            ));
        }
        if !self.exposure.is_healthy() {
            anyhow::bail!(TerminalFault("the exposure IVM circuit thread has died - refusing to serve frozen compliance data".into()));
        }
        if !self.velocity.is_healthy() {
            anyhow::bail!(TerminalFault("the velocity IVM circuit thread has died - refusing to serve frozen compliance data".into()));
        }
        // §5.2: "Serving frozen derived state as healthy is not graceful degradation; it is a lie
        // with a pleasant HTTP status." The entity carries *why* it stopped, so the quarantine
        // reason names the cause rather than sending whoever is on call to the logs of a process
        // that may since have restarted.
        for entity in self.entities.iter() {
            if let Some(why) = entity.fault() {
                // Push it before bailing. A fault here is terminal and quarantines the nest, so the
                // operator finds out from `/ready` - if they are looking. `nuthatch_entity_faulted`
                // is the series to build a dashboard on; this is the one that arrives unasked.
                //
                // Best-effort on purpose: a webhook that will not enqueue must not change what
                // happens to the nest, and what happens to the nest is that it stops either way.
                if self.router.watches("entity_fault") {
                    let ann = serde_json::json!({
                        "nest": self.name,
                        "entity": entity.name(),
                        "reason": why,
                        "applied_through": entity.applied_through(),
                    });
                    if let Err(e) = alerts::enqueue(
                        &self.store,
                        &self.router,
                        &format!("entity `{}` stopped", entity.name()),
                        "entity_fault",
                        &ann,
                    ) {
                        tracing::warn!("could not enqueue the entity_fault alert: {e:#}");
                    }
                }
                anyhow::bail!(TerminalFault(format!(
                    "the circuit for entity `{}` has stopped: {why}",
                    entity.name()
                )));
            }
        }
        Ok(())
    }

    /// Stored JSON rows to decoded rows, against this nest's registry.
    ///
    /// One conversion, three callers - the reorg path, the restart seed, and whatever needs it next.
    /// A second one written separately is how a retraction stops cancelling its insertion (#864).
    fn decode_stored(&self, stored: &[String]) -> Result<Vec<crate::registry::DecodedRow>> {
        decode_stored_rows(&self.registry.schema(), stored)
    }

    /// Feed rolled-back rows to every authored entity at weight `-1`.
    ///
    /// A row belonging to a table no entity reads is skipped, not an error: a nest decodes many
    /// tables and an entity reads one or two. A row this nest's registry has no schema for *is* an
    /// error - it means the stored history and the live registry disagree about what was indexed,
    /// and retracting it against a guessed shape would produce rows that cancel nothing.
    fn retract_entities(&self, doomed: &[String], ancestor: u64) -> Result<()> {
        if self.entities.is_empty() || doomed.is_empty() {
            return Ok(());
        }
        let rows = self.decode_stored(doomed)?;
        for entity in self.entities.iter() {
            entity.apply_window(&rows, -1, ancestor).with_context(|| {
                format!(
                    "retracting rolled-back rows from entity `{}`",
                    entity.name()
                )
            })?;
        }
        Ok(())
    }

    fn rollback_reorg(&mut self, ancestor: u64) -> Result<()> {
        // Retract the rolled-back transfers from the IVM view *before* dropping them from the hot
        // store - a reorg is just the same facts re-fed with weight −1.
        let last_indexed = self
            .store
            .get_meta(LAST_BLOCK_KEY)?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(ancestor);
        // This nest hasn't reached past the fork: nothing to undo, and don't advance its cursor.
        if last_indexed <= ancestor {
            return Ok(());
        }
        // A reorg below the sealed watermark is a finality violation this model can't repair: the
        // doomed blocks are already in immutable sealed segments (and pruned from hot), so the
        // retraction below would be silently incomplete and the sealed layer would permanently disagree
        // with the canonical chain. Halt loudly instead (deadlock-review finding M6). The `--seal-direct`
        // finality depth / `finalized` tag is the contract; if it's being violated, it needs raising.
        let sealed_through = self
            .store
            .get_meta(SEALED_THROUGH_KEY)?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if ancestor < sealed_through {
            // Terminal (RFC-0026 §3): the next attempt re-derives the same watermark and bails
            // identically, so this nest is quarantined until an operator raises the finality depth.
            anyhow::bail!(TerminalFault(format!(
                "reorg to block {ancestor} is below the sealed/finalized watermark \
                 {sealed_through} - a finality violation this indexer cannot repair; \
                 halting. Raise the chain's finality depth."
            )));
        }
        let doomed = self.store.entities_in_range(ancestor + 1, last_indexed)?;
        // §5.2, for the authored entities: the same rows, at weight -1, *before* they are dropped
        // from the hot store. There is no rollback interface because there is nothing to roll back.
        //
        // These are reconstructed through `DecodedRow::from_stored` rather than parsed here, and
        // that is the whole point: DBSP cancels by key, so a retraction built by a different
        // converter than the insertion does not cancel it - it lands beside it and stays forever.
        // One conversion, both directions.
        self.retract_entities(&doomed, ancestor)?;
        self.balances.apply(retraction_batch(&doomed));
        self.exposure.apply(exposure_retraction_batch(
            &doomed,
            &self.registry,
            &self.labels,
        ));
        if let Some((_, w)) = self.velocity_cfg {
            self.velocity
                .apply(velocity_retraction_batch(&doomed, &self.registry, w));
        }
        // Drop children whose announcing factory event was rolled back (RFC-0009): the registry state
        // at B is a pure fold over factory events ≤ B.
        if self.factory.is_some() {
            let dropped = self.children.rollback_to(ancestor);
            if dropped > 0 {
                tracing::warn!("reorg: dropped {dropped} discovered child contract(s)");
            }
        }
        // Fire a `flag_retracted` alert for every rolled-back annotation a sink watches - a consumer
        // that acted on a flag learns the chain took it back (RFC-0008 C5).
        if !self.router.is_empty() {
            for j in &doomed {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(j) {
                    if let Some(kind) = v.get("kind").and_then(|k| k.as_str()) {
                        if self.router.watches(kind) {
                            alerts::enqueue(&self.store, &self.router, "flag_retracted", kind, &v)?;
                        }
                    }
                }
            }
        }

        // Roll the hot store back to the ancestor AND reset the `last_block` watermark in ONE txn: a
        // crash between a separate delete and watermark reset would leave `last_block` past the fork and
        // the rolled-back range permanently un-reindexed (a silent gap).
        let removed =
            self.store
                .rollback_to_and_set_meta(ancestor, LAST_BLOCK_KEY, &ancestor.to_string())?;

        self.metrics.inc_reorgs();
        self.metrics.set_last_block(ancestor);
        tracing::warn!(
            "reorg detected: rolled back to block {ancestor} (removed {removed} entities)"
        );
        Ok(())
    }

    /// Decode, store, IVM-feed, screen, checkpoint, seal and deliver webhooks for one fetched window
    /// `[next, to]` (with `tip` the current chain tip, used for the finality ceiling). Returns
    /// `Ok(Some(stored))` - the row count, caller advances the cursor - or `Ok(None)` when block
    /// timestamps were unavailable and the window must be retried WITHOUT advancing (the cursor stays
    /// put so a freshly-finalized window never seals `block_timestamp = 0`, deadlock-review H4).
    async fn process_window(
        &mut self,
        source: &dyn Source,
        logs: &[crate::rpc::Log],
        next: u64,
        to: u64,
        tip: u64,
    ) -> Result<Option<usize>> {
        // Decode first so factory discovery is inline (a child created at log i is in the
        // registry before its own activity at log j>i - RFC-0009 same-block handling), then
        // buy headers only for blocks that produced a kept row (#765). A topic0-only fetch
        // returns every matching event on the chain; stamping from the raw log list paid
        // eth_getBlockByNumber for foreign protocols we then discarded.
        let empty_ts = std::collections::HashMap::new();
        let mut rows = decode_window(
            &self.registry,
            self.factory.as_deref(),
            &mut self.children,
            logs,
            &empty_ts,
        );
        // Rows below the cursor are the refetched tail (#1144). Any the store already holds have
        // already been folded into every view and entity; folding them again would double-count, so
        // they are dropped here, before the fold, and only the rows the first answer missed go on.
        // Checked only below `next`, so the ordinary window costs no store reads.
        //
        // "Already holds" means the same row, not merely the same key: the stored row's `block_hash`
        // must match. The loop runs `handle_reorg` before every fetch, so a reorged tail block has
        // been rolled back before its refetch arrives and this never fires in practice - but if it
        // ever does, the stale row must not be mistaken for the current one and silently kept. The
        // window is refused instead and asked for again on the next poll, by which time the reorg
        // handler has run over it.
        let mut stale_tail: Option<(u64, u64)> = None;
        rows.retain(|r| {
            if r.block_number >= next {
                return true;
            }
            match self
                .store
                .get_entity(&Store::entity_key(r.block_number, r.log_index))
                .ok()
                .flatten()
            {
                None => true,
                Some(stored) => {
                    let same_block = serde_json::from_str::<serde_json::Value>(&stored)
                        .ok()
                        .and_then(|v| {
                            v.get("block_hash")
                                .and_then(|h| h.as_str())
                                .map(str::to_string)
                        })
                        .is_some_and(|h| h == r.block_hash);
                    if !same_block {
                        stale_tail = Some((r.block_number, r.log_index));
                    }
                    false
                }
            }
        });
        if let Some((b, i)) = stale_tail {
            tracing::warn!(
                "refetched tail row ({b}, {i}) is already stored under a different block hash: a \
                 reorg the handler has not yet rolled back - refusing this window, retrying"
            );
            sleep_secs(2).await;
            return Ok(None);
        }
        // RFC-0023 tier 3 samples blocks that may have emitted no log at all, and a stored row
        // with `block_timestamp = 0` seals that zero permanently once it finalizes (H4). Sampled
        // blocks join the timestamp fetch rather than being handled after it.
        let extra = self.calls.iter().flat_map(|d| d.blocks_in(next, to));
        let blocks = blocks_needing_timestamps(&rows, extra);
        let timestamps = match fetch_timestamps(source, &self.registry, &blocks).await {
            Ok(t) => t,
            Err(e) => {
                // Don't store this window with zeroed timestamps - once it finalizes it would
                // seal `block_timestamp = 0` permanently (deadlock-review finding H4). The
                // cursor hasn't advanced, so skip and re-fetch the same window next poll.
                tracing::warn!(
                    "block timestamps unavailable for {next}..={to}: {e:#} - retrying window"
                );
                sleep_secs(2).await;
                return Ok(None);
            }
        };
        apply_row_timestamps(&mut rows, &timestamps);
        self.children.apply_timestamps(&timestamps);

        let mut stored = 0usize;
        let mut deltas = Vec::new();
        let mut exp_deltas = Vec::new();
        let mut vel_deltas = Vec::new();
        // Transfers to screen this window (only collected when screening is on).
        let mut to_screen: Vec<TransferRow> = Vec::new();
        // PERF-2: accumulate every write and commit the whole window in ONE redb txn at the end,
        // instead of a `begin_write`/`commit` (fsync) per row. `(key, json)` for rows + annotations.
        let mut to_store: Vec<(String, String)> = Vec::with_capacity(rows.len());
        for row in &mut rows {
            let key = Store::entity_key(row.block_number, row.log_index);
            // Feed the IVM balance + exposure views for transfer rows (extracted before storing).
            if let Some((from, to_addr, value, _hex)) = row.erc20_transfer_fields() {
                if let Some(v) = value.as_deref().and_then(|s| s.parse::<i128>().ok()) {
                    deltas.extend(views::transfer_deltas(&from, &to_addr, v, 1));
                    // Direct exposure to the labeled set (empty when neither side is labeled).
                    exp_deltas.extend(exposure::exposure_deltas(
                        &from,
                        &to_addr,
                        v,
                        1,
                        &self.labels,
                    ));
                    // Velocity: the sender's outbound volume in this block's window (C3).
                    if let Some((_, w)) = self.velocity_cfg {
                        vel_deltas.extend(velocity::velocity_deltas(
                            &from,
                            row.block_number,
                            v,
                            1,
                            w,
                        ));
                    }
                    // Threshold flag: a single transfer at/above the configured amount (C3).
                    if let Some(t) = self.threshold {
                        if let Some((fkey, ann)) = crate::flags::threshold_annotation(
                            &from,
                            &to_addr,
                            v,
                            row.block_number,
                            row.log_index,
                            &row.tx_hash,
                            t,
                        ) {
                            to_store.push((fkey, ann.to_string()));
                            alerts::enqueue(
                                &self.store,
                                &self.router,
                                "flag",
                                "threshold_flag",
                                &ann,
                            )?;
                        }
                    }
                } else if value.is_some() {
                    // COR-8 (#814): a live transfer whose value exceeds `i128`. Skipped here exactly
                    // as the cold fold skips it, so hot and sealed agree - but counted now, so the
                    // balance says it is incomplete instead of looking complete.
                    self.balances.note_over_i128(1);
                    tracing::warn!(
                        block = row.block_number,
                        log_index = row.log_index,
                        "transfer value does not fit i128; excluded from balances (#814)"
                    );
                }
                if self.screener.is_some() {
                    to_screen.push(TransferRow {
                        block_number: row.block_number,
                        log_index: row.log_index,
                        from: from.to_ascii_lowercase(),
                        to: to_addr.to_ascii_lowercase(),
                        value: value.unwrap_or_default(),
                        tx_hash: row.tx_hash.clone(),
                    });
                }
            }
            // Every row is stored uniformly as typed JSON with a `table` field; per-table
            // sealing groups by it.
            to_store.push((key, row.to_json().to_string()));
            stored += 1;
        }

        // RFC-0036 §4.2: one row per block in the window for a blocks nest - same logic as the
        // backfill paths. Must enumerate from the window [next..=to], not from `logs`, because
        // a blocks table covers blocks that emitted no matching log at all (#447).
        if self.registry.blocks() {
            let want: Vec<u64> = (next..=to).collect();
            match source.block_headers(&want).await {
                Ok(headers) => {
                    let mut block_rows: Vec<_> = want
                        .iter()
                        .filter_map(|b| {
                            headers.get(b).and_then(|h| {
                                crate::registry::block_row(*b, h, self.registry.timestamps())
                            })
                        })
                        .collect();
                    block_rows.sort_by_key(|r| r.block_number);
                    for r in &block_rows {
                        to_store.push((
                            Store::entity_key(r.block_number, r.log_index),
                            r.to_json().to_string(),
                        ));
                        stored += 1;
                    }
                }
                Err(e) => {
                    tracing::warn!("block_headers unavailable for {next}..={to}: {e:#} - block rows skipped this window");
                }
            }
        }

        self.balances.apply(deltas);
        self.exposure.apply(exp_deltas);
        self.velocity.apply(vel_deltas);
        // §5.1: the same decoded window, at weight +1, to every authored entity. Backfill and tip
        // are the same call - they differ in how many rows `rows` holds, which is the whole of what
        // "backfill uses larger batches, but not different semantics" means.
        //
        // `to` and not the nest's head: an entity carries its own applied-through watermark, and
        // stamping it with a block it has not folded is how a partial relation gets served as
        // current (criterion 2).
        for entity in self.entities.iter() {
            entity
                .apply_window(&rows, 1, to)
                .with_context(|| format!("feeding this window to entity `{}`", entity.name()))?;
        }
        // A derived-view circuit thread that has died silently drops those applies and freezes
        // `/balances` + the compliance flags while ingest keeps committing - stale data served as
        // healthy. Surface it as fatal here (the dead-task-must-surface rule, extended to the IVM
        // threads that were previously exempt).
        self.ensure_views_healthy()?;

        // Live sanctions screening (RFC-0008 C2): screen this window's transfers against the
        // configured list snapshots and store `sanction_hit` annotations. They share the
        // transfers' block keys, so they seal and roll back with the same range. Stored before
        // `maybe_seal` below so a freshly-finalized window seals its hits alongside its rows.
        if let Some(s) = self.screener.as_ref() {
            let hits = s.screen_window(&to_screen);
            for (key, ann) in &hits {
                to_store.push((key.clone(), ann.to_string()));
                alerts::enqueue(&self.store, &self.router, "flag", "sanction_hit", ann)?;
            }
            if !hits.is_empty() {
                tracing::warn!(
                    "sanctions screening: {} hit(s) in {next}..={to}",
                    hits.len()
                );
            }
        }
        // Fetch the window boundary's canonical hash for future reorg detection, then commit the whole
        // window - rows + annotations + the checkpoint + the `last_block` watermark - in one atomic txn.
        let checkpoint = match source.block_hash(to).await {
            Ok(Some(hash)) => Some((to, hash)),
            _ => None,
        };
        // Off the runtime's worker threads (audit F-C3): this ends in an fsync, and the API is served
        // from the same runtime, so a contended commit here would surface as latency on unrelated
        // RFC-0037: resolve the IPFS documents this window's rows point at.
        //
        // Deduped by CID before any fetch, because a CID *is* a content address: a thousand rows
        // naming the same document are one fetch and one row. Every body is verified against its CID
        // before it is stored (`crate::cid`), so a gateway answering HTTP 200 with prose - which they
        // really do - cannot become a nest's data.
        //
        // Bounded, and failure is absence rather than error: an unresolved document simply has no
        // row, which is what the `LEFT JOIN` shape expects. Tip-following must never wait on a
        // gateway indefinitely.
        if !self.ipfs.is_empty() {
            const MAX_FETCHES_PER_WINDOW: usize = 64;
            let mut budget = MAX_FETCHES_PER_WINDOW;
            let mut per_block: std::collections::BTreeMap<u64, Vec<(usize, String)>> =
                std::collections::BTreeMap::new();
            for (i, d) in self.ipfs.iter().enumerate() {
                let col = d.column();
                let mut seen = std::collections::HashSet::new();
                let mut src: Vec<&crate::registry::DecodedRow> =
                    rows.iter().filter(|r| r.table == d.on).collect();
                src.sort_by_key(|r| (r.block_number, r.log_index));
                for r in src {
                    let Some(cid) = r.params.iter().find(|(k, _)| k == col).map(|(_, v)| v) else {
                        continue;
                    };
                    // The column may hold a bare CID, an `ipfs://` URI, a full gateway URL, or a
                    // raw 32-byte digest - a real subgraph port turned up all four. Only the content
                    // address is kept: the string comes from a log, so fetching the host it names
                    // would let whoever emitted the event choose what this process connects to.
                    //
                    // Matching on `Value::Str` here is what used to drop the `bytes32` form on the
                    // floor before the resolver ever saw it, so the match now lives in
                    // `cid_from_value` where the refusals can be stated once.
                    let Some(cid) = crate::ipfs::cid_from_value(cid) else {
                        continue;
                    };
                    if !seen.insert(cid.to_string()) {
                        continue;
                    }
                    per_block
                        .entry(r.block_number)
                        .or_default()
                        .push((i, cid.to_string()));
                }
            }
            for (block, items) in per_block {
                let hash = retry_transient(
                    &format!("block hash for {block}"),
                    BACKFILL_RETRY_BASE,
                    || async { source.block_hash(block).await },
                )
                .await?
                .unwrap_or_default();
                let ctx = crate::ipfs::BlockCtx {
                    number: block,
                    hash: &hash,
                    timestamp: timestamps.get(&block).copied().unwrap_or(0),
                    timestamps: self.registry.timestamps(),
                };
                for (slot, (i, cid)) in items.into_iter().enumerate() {
                    if budget == 0 {
                        tracing::warn!(
                            "ipfs: window {next}..={to} hit the {MAX_FETCHES_PER_WINDOW}-fetch \
                             budget; the remaining documents stay unresolved and will be retried \
                             when a resolver runs out of band (RFC-0037)"
                        );
                        break;
                    }
                    budget -= 1;
                    match crate::subgraph_import::fetch_ipfs(
                        &cid,
                        &self.ipfs_gateways,
                        crate::subgraph_import::Origin::Manifest,
                    )
                    .await
                    {
                        Ok(content) => {
                            // `fetch_ipfs` only returns a body that verified, or one it warned about
                            // as too large for single-block re-encoding. Record which, so a consumer
                            // can tell a proven document from an accepted-unverified one.
                            let verified = content.len() <= 256 * 1024;
                            let row = crate::ipfs::to_row(
                                &self.ipfs[i].name,
                                &cid,
                                &content,
                                verified,
                                slot,
                                &ctx,
                            );
                            to_store.push((
                                Store::entity_key(row.block_number, row.log_index),
                                row.to_json().to_string(),
                            ));
                            stored += 1;
                        }
                        Err(e) => tracing::warn!(
                            "ipfs: {cid} unresolved ({e:#}) - no row written, which is what a \
                             LEFT JOIN reads as 'not yet'"
                        ),
                    }
                }
            }
        }

        // RFC-0038 §5: decode **top-level calls** - transactions sent directly to this nest's
        // contracts. This is what a subgraph's `callHandlers` fire on, and unlike the internal call
        // tree it needs no node: a transaction is in the block body that ordinary RPC already serves.
        //
        // Bounded by the nest's own addresses before decode, so a busy chain costs this nest nothing
        // it did not ask for.
        if self.top_level_calls {
            if let Some(creg) = self.call_registry.clone() {
                let want: Vec<u64> = (next..=to).collect();
                let bodies = retry_transient(
                    &format!("block bodies for {} block(s)", want.len()),
                    BACKFILL_RETRY_BASE,
                    || async { source.block_bodies(&want).await },
                )
                .await?;
                for b in &want {
                    let Some(body) = bodies.get(b) else { continue };
                    let bhash = body
                        .get("hash")
                        .and_then(|h| h.as_str())
                        .unwrap_or_default()
                        .to_string();
                    // The body already carries the header, so the timestamp comes from it rather
                    // than from a second fetch - and unlike the `timestamps` map it covers blocks
                    // that emitted no matching log at all, which is most of them.
                    let ts = body
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .and_then(|t| u64::from_str_radix(t.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(0);
                    let txs = body.get("transactions").and_then(|t| t.as_array());
                    for tx in txs.into_iter().flatten() {
                        // `to` is absent for a contract creation, which is not a call to anything we
                        // index.
                        let Some(to_addr) = tx.get("to").and_then(|t| t.as_str()) else {
                            continue;
                        };
                        let lower = to_addr.to_ascii_lowercase();
                        if !self
                            .addresses
                            .iter()
                            .any(|a| a.eq_ignore_ascii_case(&lower))
                        {
                            continue;
                        }
                        let Ok(addr) = lower.parse::<alloy_primitives::Address>() else {
                            continue;
                        };
                        let input = hex::decode(
                            tx.get("input")
                                .and_then(|i| i.as_str())
                                .unwrap_or("0x")
                                .trim_start_matches("0x"),
                        )
                        .unwrap_or_default();
                        let idx = tx
                            .get("transactionIndex")
                            .and_then(|i| i.as_str())
                            .and_then(|i| u64::from_str_radix(i.trim_start_matches("0x"), 16).ok())
                            .unwrap_or(0);
                        let ctx = crate::calldata::CallContext {
                            block_number: *b,
                            block_hash: bhash.clone(),
                            block_timestamp: ts,
                            tx_hash: tx
                                .get("hash")
                                .and_then(|h| h.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            // The reserved band is applied here rather than at storage, so the row's
                            // own `log_index` is the key it lands under - one number, one meaning.
                            call_index: crate::registry::TX_CALL_ROW_LOG_INDEX_BASE + idx,
                            timestamps: self.registry.timestamps(),
                        };
                        if let Some(row) = creg.decode_call(addr, &input, &ctx) {
                            to_store.push((
                                Store::entity_key(row.block_number, row.log_index),
                                row.to_json().to_string(),
                            ));
                            stored += 1;
                        }
                    }
                }
            }
        }

        // RFC-0023 tier 3 + RFC-0038 §3: resolve the declared reads this window asks for.
        //
        // Two forms. A **sampled** declaration fires at the blocks `blocks_in` yields. A **row-driven**
        // one fires once per row of the table it names, which is what a subgraph mapping does - the
        // call happens as the row is produced, the same moment a handler would have made it.
        //
        // Ordering is identity, not presentation: declarations in config order, and within a
        // row-driven declaration its source rows in `log_index` order. Two operators running the same
        // nest must produce the same keys, not merely the same content addresses.
        if !self.calls.is_empty() {
            let rpc = self
                .state_rpc
                .clone()
                .context("tier-3 calls declared with no --state-rpc; refused at startup")?;
            let mut wanted: std::collections::BTreeMap<u64, Vec<(usize, String, String)>> =
                std::collections::BTreeMap::new();
            for (i, d) in self.calls.iter().enumerate() {
                if d.is_row_driven() {
                    let table = d.on.as_deref().unwrap_or_default();
                    let mut src: Vec<&crate::registry::DecodedRow> =
                        rows.iter().filter(|r| r.table == table).collect();
                    src.sort_by_key(|r| (r.block_number, r.log_index));
                    for r in src {
                        let (contract, calldata) = d.resolve_for_row(r)?;
                        wanted
                            .entry(r.block_number)
                            .or_default()
                            .push((i, contract, calldata));
                    }
                } else {
                    for b in d.blocks_in(next, to) {
                        wanted.entry(b).or_default().push((
                            i,
                            d.contract.to_ascii_lowercase(),
                            d.calldata.to_ascii_lowercase(),
                        ));
                    }
                }
            }

            let capacity =
                crate::registry::BLOCK_ROW_LOG_INDEX - crate::registry::CALL_ROW_LOG_INDEX_BASE;
            // One batched header fetch for every block this window's calls touch (#720), rather
            // than a sequential single-block `block_hash` per block below. `timestamps` above
            // already came from a batched fetch; the hash was the one field still paying an
            // unbatched round trip per sampled block.
            let wanted_blocks: Vec<u64> = wanted.keys().copied().collect();
            let headers = retry_transient(
                &format!("block headers for {} block(s)", wanted_blocks.len()),
                BACKFILL_RETRY_BASE,
                || async { source.block_headers(&wanted_blocks).await },
            )
            .await?;
            for (block, mut items) in wanted {
                // `CallKey` is a content address, so N rows asking the same question of the same
                // contract at the same block are one call and one row. Dedupe before the RPC, not
                // after: the saving is the request, not the storage.
                let mut seen = std::collections::HashSet::new();
                items.retain(|(i, c, d)| seen.insert((*i, c.clone(), d.clone())));

                if items.len() as u64 >= capacity {
                    anyhow::bail!(
                        "block {block} wants {} distinct pinned reads, and only {capacity} fit in the \
                         reserved row-index band.\n\n\
                         A row-driven `[[calls]]` declaration fires once per source row, so a dense \
                         table can ask for more reads than a block can hold. Narrow the source table \
                         (index fewer events), or make the declaration sampled instead.",
                        items.len()
                    );
                }

                let pairs: Vec<(String, String)> = items
                    .iter()
                    .map(|(_, c, d)| (c.clone(), d.clone()))
                    .collect();
                // Retried like every other RPC fetch on this path, and it was not, which cost a
                // 454M-block backfill 8 hours in at 87.6%: one `transport error: error sending
                // request` on a pinned batch propagated straight out and killed the nest. `getLogs`
                // and the timestamp fetches have gone through `retry_transient` since #538; this one
                // shipped in 2.6.0 with a bare `?`, so any long backfill declaring `[[calls]]` died
                // on the first blip from the provider.
                //
                // Never-give-up with capped backoff, matching the sealed-history path exactly: a
                // transient provider failure is not a reason to discard hours of work, and the
                // progress line resumes moving once it clears.
                let chain_id = self.chain_id;
                let results = retry_transient(
                    &format!("pinned eth_call batch at block {block}"),
                    BACKFILL_RETRY_BASE,
                    || async {
                        crate::calls::resolve_pairs_at(rpc.as_ref(), chain_id, &pairs, block).await
                    },
                )
                .await?;
                let hash = headers
                    .get(&block)
                    .and_then(|h| h.get("hash"))
                    .and_then(|h| h.as_str())
                    .unwrap_or_default()
                    .to_string();
                let ts = timestamps.get(&block).copied().unwrap_or(0);
                for (slot, ((i, _, _), r)) in items.iter().zip(results).enumerate() {
                    let row = r.to_row(
                        &self.calls[*i].name,
                        slot,
                        &hash,
                        ts,
                        self.registry.timestamps(),
                    );
                    to_store.push((
                        Store::entity_key(row.block_number, row.log_index),
                        row.to_json().to_string(),
                    ));
                    stored += 1;
                }
            }
        }

        // requests. `to_store` is moved rather than borrowed - the work outlives this borrow.
        self.store
            .commit_window_blocking(std::mem::take(&mut to_store), checkpoint, to)
            .await?;
        self.metrics.set_last_block(to);
        self.metrics.add_rows_decoded(stored as u64);
        if stored > 0 {
            // Per-window detail is debug: the live progress line (RFC-0015 slice 3) is the
            // user-facing narrative during catch-up, and this fires once per window - pure spam at
            // info over a long backfill. `count()` is only paid when debug is on.
            tracing::debug!(
                "blocks {next}..={to}: +{stored} rows (total {})",
                self.store.count()?
            );
        }

        // The highest block considered final under this chain's policy. For an L2 with the
        // `finalized` tag we ask the node; otherwise (and on tag failure) it's a fixed depth.
        let finalized_tag = match self.finality {
            Finality::FinalizedTag { .. } => source.finalized().await.ok().flatten(),
            Finality::Depth(_) => None,
        };
        let finalized_through = seal_ceiling(self.finality, tip, finalized_tag);

        // Seal any newly-finalized range to an immutable Parquet segment, stamping the
        // discovered-child registry snapshot for a factory nest (RFC-0009 step 4).
        let snapshot = self.factory.as_ref().map(|_| self.children.hash());
        if let Err(e) = maybe_seal(
            &self.dir,
            &self.store,
            source,
            finalized_through,
            snapshot.as_deref(),
            &self.metrics,
        )
        .await
        {
            tracing::warn!("sealing failed: {e:#}");
        }
        // Deliver user webhooks for whatever just sealed (RFC-0010 Part B) - enqueue only,
        // the background worker POSTs; a slow endpoint never blocks the loop.
        if !self.webhooks.is_empty() {
            if let Err(e) = crate::webhooks::deliver_sealed(
                &self.store,
                &self.dir,
                &self.webhooks,
                finalized_through,
            ) {
                tracing::warn!("webhook delivery failed: {e:#}");
            }
        }
        Ok(Some(stored))
    }
}

/// Drive [`NestIngest::prepare`] to completion, retrying a [`ColdStartUnreachable`] failure forever
/// (mirroring `index_loop`'s own tolerance of a dead pool once past this point) and propagating any
/// other failure immediately - a real bug (corrupt state, a dead IVM thread, …) must still fail the
/// process loudly rather than spin quietly (#510).
async fn prepare_retrying(
    nest: &mut NestIngest,
    source: &dyn Source,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window: u64,
) -> Result<u64> {
    let mut poll_failures = 0u32;
    loop {
        match nest
            .prepare(source, backfill, seal_direct, concurrency, window)
            .await
        {
            Ok(next) => return Ok(next),
            Err(e) if is_cold_start_unreachable(&e) => {
                nest.metrics.mark_poll_failed();
                poll_failures = escalate_stall(poll_failures, &e);
                sleep_secs(3).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn index_loop(
    source: Arc<dyn Source>,
    mut nest: NestIngest,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window: u64,
) -> Result<()> {
    let mut next = prepare_retrying(
        &mut nest,
        source.as_ref(),
        backfill,
        seal_direct,
        concurrency,
        window,
    )
    .await?;
    // The floor for the refetched tail (#1144): the nest's configured start, so a resume reaches
    // back across the restart into the previous run's unrefetched tail, and a cold start never
    // reaches behind an explicit `start_block`.
    let range_floor = nest.start_block.unwrap_or(0);

    // Adaptive getLogs sizing (RFC-0004 §2), seeded from the chain's default window.
    //
    // A blocks nest pays one header request per *block*, so its ceiling is header cost rather than log
    // density (RFC-0036) - the same branch all three backfill paths take. This loop did not, and the
    // omission only became reachable with the #432 fix above: while a contract-free nest was fetching
    // every log on the chain, the enormous count shrank the window and hid this. Fetching nothing feeds
    // `observed(0)` instead, which grows 4x per window to `MAX_WINDOW` (100,000) and would ask for a
    // hundred thousand headers in one window - trading a getLogs pathology for the header fan-out
    // pathology RFC-0036 exists to prevent. Capped, `observed(0)` settles at `HEADER_WINDOW_CAP`, which
    // is the intended steady state for a nest whose windows are all zero-log by construction.
    let mut chunker = if nest.registry.blocks() {
        AdaptiveWindow::for_window_with_headers(window)
    } else {
        AdaptiveWindow::for_window(window)
    };
    // Live catch-up feedback (RFC-0015 slice 3): a single progress line while the hot loop chases
    // the tip for the *first* time, ending on a crisp "caught up". `None` until there's actually a
    // backlog to report; `caught_up` latches after the first catch-up so steady-state tip-following
    // stays quiet - the "caught up" line fires exactly once, not on every new block.
    let mut progress: Option<crate::progress::Backfill> = None;
    let mut caught_up = false;
    // Consecutive failed polls - drives the escalating stall log (a transient blip vs a real outage).
    let mut poll_failures = 0u32;
    // A slow, log-visible "at tip / N behind" restated periodically (issue #302) - `progress` above
    // only ever announces catch-up once, and after that this is the only text an operator watching
    // logs (rather than Prometheus) gets.
    let mut heartbeat = crate::progress::TipHeartbeat::new();
    // #863. Counts attempts that reached a fetch and came back without moving `next`. Any window that
    // actually commits resets it, so a narrowing descent - which fails repeatedly on the way down and
    // then succeeds - never trips it.
    let mut no_progress = 0usize;
    loop {
        let tip = match source.tip().await {
            Ok(t) => {
                poll_failures = 0;
                // **Per-nest, not the process global.** `NestMetrics::{set_tip, mark_poll_ok}` fan out
                // to the global as well, but `METRICS::*` does not fan *in* - so calling the global
                // here left this nest's own `tip` and `last_poll_ok` at zero forever.
                //
                // That is invisible on `/ready`, which falls back to the global for a solo runtime,
                // and wrong on `/metrics`, which prefers the per-nest struct whenever one exists. The
                // two surfaces then disagree about one fact, and the wrong one is where Prometheus
                // looks - the same shape as #918's `sealed_through`.
                //
                // Measured live on the Lodestar box, three nests at tip: `/ready` reported
                // `"tip":500198892,"lag_blocks":3`, while `/metrics` reported `nuthatch_tip_height 0`,
                // `nuthatch_tip_lag_blocks 0` and `nuthatch_last_poll_unixtime 0`.
                //
                // **Lag is the dangerous one**: it renders as `tip.saturating_sub(last)`, so an unset
                // tip saturates to **0** - the healthiest possible reading. An alert on tip lag could
                // never fire, whatever the nest did. `last_block` was always correct because its
                // setter is the fanning-out one; this line was the only thing keeping tip behind.
                nest.metrics.mark_poll_ok();
                t
            }
            Err(e) => {
                nest.metrics.mark_poll_failed();
                poll_failures = escalate_stall(poll_failures, &e);
                sleep_secs(3).await;
                continue;
            }
        };
        nest.metrics.set_tip(tip);

        if let Some(new_next) = nest.handle_reorg(source.as_ref(), next).await? {
            next = new_next;
            continue;
        }

        heartbeat.maybe_log(next, tip);

        if next > tip {
            // Reached the tip. If this was the initial backfill, announce it once and latch.
            if let Some(p) = progress.take() {
                p.finish(next.saturating_sub(1), true);
            }
            caught_up = true;
            // Poll for new blocks.
            sleep_secs(2).await;
            continue;
        }

        // There's a backlog. During the *initial* catch-up, drive the live progress line; once we've
        // caught up once, new blocks are processed quietly (no reporter, no per-window log).
        if !caught_up {
            progress
                .get_or_insert_with(|| crate::progress::Backfill::new("backfilling", next, tip));
        }
        let to = (next + chunker.window() - 1).min(tip);
        // A contract-free nest (`blocks = true`, no `[[contracts]]` - OBIB case 3) has both halves of
        // its filter empty, which asks a node for every log on the chain. #421 and #429 guarded the
        // backfill paths and left this one, which is the worse of the two: it repeats for as long as
        // `nuthatch dev` runs (#432).
        //
        // The empty window is still *processed* rather than skipped - the cursor advances, the reorg
        // check runs, and sealing proceeds off the same window bookkeeping. Note what that does NOT
        // currently include: `process_window` derives its block list from `logs`, so a blocks nest
        // writes no rows for a window nothing matched. That is #447, not this fix, and it is why the
        // fall-through matters more once #447 lands than it does today.
        let filter = LogFilter::new(&nest.addresses, &nest.topic0s);
        let fetched = match &filter {
            // The tail of the previous window is asked for again (#1144); `process_window` drops
            // the rows the store already holds before anything is folded into a view.
            Some(f) => source.logs(f, overlap_from(next, range_floor), to).await,
            None => Ok(Vec::new()),
        };
        match fetched {
            Ok(logs) => {
                chunker.observed(logs.len() as u64);
                let n = logs.len() as u64;
                match nest
                    .process_window(source.as_ref(), &logs, next, to, tip)
                    .await?
                {
                    // Window processed and committed - advance the cursor past it.
                    Some(_stored) => {
                        next = to + 1;
                        no_progress = 0;
                        if let Some(p) = progress.as_mut() {
                            p.tick(to, n);
                        }
                    }
                    // Timestamps were unavailable; the cursor stayed put, retry the same window.
                    None => {
                        no_progress_tick(
                            &mut no_progress,
                            next,
                            to,
                            caught_up,
                            "timestamps unavailable",
                        )?;
                        continue;
                    }
                }
            }
            Err(e) if narrowing_can_help(&e, next, to) => {
                if next >= to {
                    return Err(e).with_context(|| single_block_over_cap(next)); // H3: can't shrink a block
                }
                // Provider capped the response - shrink and retry the same range immediately.
                chunker.too_large();
                no_progress_tick(&mut no_progress, next, to, caught_up, "narrowing")?;
                tracing::debug!("range {next}..={to} refused; shrinking and retrying");
            }
            Err(e) => {
                // A refusal carrying no width information - a 429 or a 403. Retrying at the same width is
                // right: endpoint failover happens beneath this, and the growth that used to walk into an
                // unserveable width is bounded by evidence in the chunker now (#672).
                tracing::warn!("get_logs {next}..={to} failed: {e:#}; retrying");
                no_progress_tick(&mut no_progress, next, to, caught_up, "fetch failing")?;
                sleep_secs(3).await;
            }
        }
    }
}

/// One attempt's worth of [`NO_PROGRESS_LIMIT`] bookkeeping (#863).
///
/// **What it does depends on where it is, and that is the decision the issue asked for.**
///
/// During the initial backfill it bails. A backfill has a defined end, so one that has stopped
/// advancing will never reach it: failing by name beats running until somebody notices the log has
/// been repeating itself for a week.
///
/// Once at tip it warns and keeps going. A tip loop has no end, and the ordinary reason it cannot
/// fetch is that somebody else's provider is having an afternoon - a condition that resolves itself.
/// Faulting there would turn a transient outage into a dead nest, and RFC-0026 is explicit that
/// quarantining a nest is the last escalation rather than the first. The same reasoning is why
/// `runtime_index_loop` is not given a bailing form of this: its cursor drives every co-tenant, so one
/// nest's bad provider would stop all of them.
fn no_progress_tick(
    count: &mut usize,
    from: u64,
    to: u64,
    caught_up: bool,
    why: &str,
) -> Result<()> {
    *count += 1;
    if *count < NO_PROGRESS_LIMIT {
        return Ok(());
    }
    if !caught_up {
        anyhow::bail!(
            "backfill made no progress across {NO_PROGRESS_LIMIT} consecutive attempts at blocks \
             {from}..={to} ({why}). The window is not narrowing and the cursor is not advancing, so \
             this will not finish on its own. Probe the endpoint's getLogs limits with `nuthatch \
             doctor <rpc-url>`, or start with a smaller `--window`."
        );
    }
    // At tip: say so loudly, then let the counter keep running so the warning repeats at a bounded
    // rate rather than once and never again.
    if (*count).is_multiple_of(NO_PROGRESS_LIMIT) {
        tracing::warn!(
            "no progress at blocks {from}..={to} across {} consecutive attempts ({why}) - this nest \
             is at tip and still trying, which is right if the provider is having an outage. If it \
             persists, it is not an outage.",
            *count
        );
    }
    Ok(())
}

/// If the checkpoint at `last` is no longer canonical, return the deepest checkpoint that still
/// is (the common ancestor to roll back to); otherwise None. Returns Some(0) if none survive.
async fn detect_reorg(
    source: &dyn Source,
    store: &dyn crate::store::HotStore,
    last: u64,
) -> Result<Option<u64>> {
    // Usually `last` itself, but if that boundary's hash couldn't be stored (a transient block_hash
    // failure at checkpoint time), fall back to the newest checkpoint we *do* have at/below `last`, so
    // a reorg is still verified against a real checkpoint instead of giving up entirely - the previous
    // "no hash here → nothing to verify" was a reorg blind spot (deadlock-review finding M7).
    let (checkpoint, stored) = match store.get_block_hash(last)? {
        Some(h) => (last, h),
        None => match store
            .checkpoints_desc()?
            .into_iter()
            .find(|(b, _)| *b <= last)
        {
            Some((b, h)) => (b, h),
            None => return Ok(None), // genuinely no checkpoint yet (cold start)
        },
    };
    let canonical = match source.block_hash(checkpoint).await? {
        Some(h) => h,
        None => return Ok(None), // source can't answer right now; try again next tick
    };
    if stored == canonical {
        return Ok(None);
    }
    for (block, hash) in store.checkpoints_desc()? {
        if block >= checkpoint {
            continue;
        }
        if let Some(canon) = source.block_hash(block).await? {
            if canon == hash {
                return Ok(Some(block));
            }
        }
    }
    // No checkpoint we hold is canonical, so the fork is deeper than our entire recorded history.
    // Rolling back everything is the CORRECT recovery here: re-indexing from the nest's origin
    // reconverges on the canonical chain (`e2e_reorg::reorg_converges_to_canonical` asserts exactly
    // this for a fork below the oldest checkpoint).
    //
    // It looks identical to a wrong-network endpoint (issue #150) - both make every stored hash
    // mismatch - and block hashes alone cannot tell them apart. So the wrong-chain case is caught
    // *upstream*, by `RpcClient::verify_chain_ids` at startup, and the established-nest case is caught
    // *downstream*, by `rollback_reorg`'s sealed-watermark bail. Refusing here instead would break the
    // legitimate deep-reorg recovery, which is a real correctness regression rather than a guard.
    tracing::warn!(
        "no checkpoint at or below block {checkpoint} is canonical - rolling back the whole hot store \
         and re-indexing from origin. If this repeats, check every url in `rpc_urls` is on this \
         nest's chain (a wrong-network endpoint looks exactly like this)."
    );
    Ok(Some(0))
}

/// Where a cold start begins backfilling. An explicit `--backfill N` always wins - "index the last N
/// blocks", overriding a vendored deploy block (this is what keeps the recent-history use working on
/// a nest that declares start blocks). Otherwise, the nest's earliest vendored `start_block` gives
/// full history from deployment; failing that, a default recent window. Pure, so it's unit-testable.
/// The seal-direct backfill concurrency that's safe for the configured endpoints. A *single* RPC host
/// can't absorb a high-concurrency backfill: many concurrent requests to one host stall the whole
/// tokio runtime - a lost wakeup that parks every worker and never fires, so even the per-request
/// timeout can't rescue it, and the backfill hangs forever (reproduced at `--concurrency 8` to one
/// host; multiple hosts spread the load over separate connections and never hit it). So a single
/// endpoint is capped to sequential; two or more keep the requested parallelism. The caller logs the
/// cap so the operator knows to add endpoints for a faster backfill.
pub fn safe_backfill_concurrency(endpoint_count: usize, requested: usize) -> usize {
    if endpoint_count <= 1 {
        1
    } else {
        requested
    }
}

/// Where a seal-direct backfill starts: one past the last durably-sealed block if a prior run left a
/// watermark (resume a partial backfill), else the computed `origin` (a fresh start). Resuming is what
/// keeps a mid-backfill failure from re-fetching - and, on the adaptive factory path, re-sealing under
/// fresh content hashes - ranges already sealed (deadlock-review finding C1).
fn resume_from_watermark(sealed_through: Option<u64>, origin: u64) -> u64 {
    match sealed_through {
        Some(s) => s.saturating_add(1),
        None => origin,
    }
}

/// The `eth_getLogs` window to use: an explicit `--window` override, else the chain default. A zero
/// override is ignored (a zero-block window can't make progress).
fn effective_window(override_: Option<u64>, chain_window: u64) -> u64 {
    match override_ {
        Some(w) if w > 0 => w,
        _ => chain_window,
    }
}

fn cold_start_block(start_block: Option<u64>, backfill: Option<u64>, tip: u64) -> u64 {
    match (backfill, start_block) {
        (Some(n), _) => tip.saturating_sub(n),
        (None, Some(b)) => b.min(tip),
        (None, None) => tip.saturating_sub(DEFAULT_BACKFILL),
    }
}

/// The highest block safe to seal under `finality`: the `finalized` tag when the chain uses it and
/// the node serves it, else a fixed depth below the tip. Pure, so the policy is unit-testable.
fn seal_ceiling(finality: Finality, tip: u64, finalized_tag: Option<u64>) -> u64 {
    match finality {
        Finality::Depth(d) => tip.saturating_sub(d),
        Finality::FinalizedTag { fallback_depth } => match finalized_tag {
            Some(n) => n.min(tip),
            None => tip.saturating_sub(fallback_depth),
        },
    }
}

/// Block at which the tip path should cut a segment, or `None` to keep holding in the hot store.
///
/// Same rule as [`take_sealable`]: wait until `SEAL_DIRECT_BATCH` rows have finalised, then cut at
/// the block that carried the buffer past the threshold. The cut is a function of the rows, not of
/// when finality advanced, so two operators whose tips move on different schedules still produce
/// identical segments (#1067). A range with no rows is `None` as well; the caller advances the
/// watermark in that case because there is nothing to batch.
fn tip_seal_cut(entities: &[String]) -> Option<u64> {
    if entities.len() < SEAL_DIRECT_BATCH {
        return None;
    }
    block_number_of(&entities[SEAL_DIRECT_BATCH - 1])
}

fn block_number_of(json: &str) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    match v.get("block_number")? {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Seal finalized rows that have accumulated to [`SEAL_DIRECT_BATCH`], cutting at a block boundary
/// chosen from the data. Rows short of the threshold stay in the hot store until the next call.
///
/// Earlier write-ups called this `seal_finalized`. It sealed `from..=ceiling` with no row threshold,
/// which is why 80% of a long-running nest's segments were under 20 KB (docs/bench/segment-layout.md).
async fn maybe_seal(
    dir: &std::path::Path,
    store: &dyn crate::store::HotStore,
    source: &dyn Source,
    finalized_through: u64,
    registry_snapshot: Option<&str>,
    metrics: &crate::metrics::NestMetrics,
) -> Result<()> {
    if finalized_through == 0 {
        return Ok(());
    }
    let last_indexed = match store.get_meta(LAST_BLOCK_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt last_block")?,
        None => return Ok(()),
    };
    let ceiling = finalized_through.min(last_indexed);

    let mut from = match store.get_meta(SEALED_THROUGH_KEY)? {
        Some(v) => v.parse::<u64>().context("corrupt sealed_through")? + 1,
        None => store
            .get_meta(START_BLOCK_KEY)?
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
    };
    if ceiling < from {
        return Ok(()); // nothing new has finalized
    }

    loop {
        if ceiling < from {
            return Ok(());
        }
        let entities = store.entities_in_range(from, ceiling)?;
        let cut = match tip_seal_cut(&entities) {
            None if entities.is_empty() => {
                // Finalized range with no transfers - just advance the watermark. Pinning a
                // checkpoint at the new watermark is what stops a later reorg from walking past it
                // to an older surviving checkpoint and tripping the finality guard on a block the
                // reorg never touched (#461). Best-effort: a source hiccup leaves the walk sparse.
                if let Ok(Some(hash)) = source.block_hash(ceiling).await {
                    store.set_block_hash(ceiling, &hash)?;
                }
                store.set_meta(SEALED_THROUGH_KEY, &ceiling.to_string())?;
                metrics.set_sealed_through(ceiling);
                tracing::debug!(
                    "blocks {from}..={ceiling} finalized with no transfers; watermark advanced"
                );
                return Ok(());
            }
            None => {
                // Held, not drained. A cursor-wide "seal because co-tenants are fat"
                // flush was considered and reverted: it made segment identity depend
                // on which nests share the cursor. The 2 GB budget is the multinest
                // RAM job (measured 372 MB for 20 nests × 12,010 rows against 2048).
                // Still pin the finalized ceiling. Holding rows does not mean the reorg
                // walker can do without a checkpoint: #461 was a walk that skipped past
                // sealed_through to an older sparse checkpoint and tripped the finality
                // guard on a block the reorg never touched. The empty-range arm already
                // pins; this one must too (#1067).
                if let Ok(Some(hash)) = source.block_hash(ceiling).await {
                    store.set_block_hash(ceiling, &hash)?;
                }
                tracing::debug!(
                    rows = entities.len(),
                    threshold = SEAL_DIRECT_BATCH,
                    from,
                    ceiling,
                    "tip path holding finalized rows until the batch threshold"
                );
                return Ok(());
            }
            Some(cut) => cut,
        };

        if let Ok(Some(hash)) = source.block_hash(cut).await {
            store.set_block_hash(cut, &hash)?;
        }

        let to_seal = store.entities_in_range(from, cut)?;
        match seal::seal_range_with_snapshot(dir, &to_seal, from, cut, registry_snapshot)? {
            Some(summary) => {
                let pruned =
                    store.prune_and_set_meta(from, cut, SEALED_THROUGH_KEY, &cut.to_string())?;
                metrics.set_sealed_through(cut);
                metrics.add_rows_sealed(summary.rows as u64);
                tracing::debug!(
                    "sealed blocks {from}..={cut}: {} rows across {} table(s); pruned {pruned} from hot",
                    summary.rows,
                    summary.tables
                );
            }
            None => {
                store.set_meta(SEALED_THROUGH_KEY, &cut.to_string())?;
                metrics.set_sealed_through(cut);
                tracing::debug!(
                    "blocks {from}..={cut} finalized with no transfers; watermark advanced"
                );
            }
        }
        from = cut + 1;
    }
}

/// Build a weight −1 retraction batch from stored transfer JSON (used on reorg rollback).
fn retraction_batch(entity_json: &[String]) -> views::WeightedBatch {
    let mut batch = Vec::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else {
            continue;
        };
        // Only transfer rows were fed to the balance view; retract only those.
        let is_transfer = v
            .get("table")
            .and_then(|t| t.as_str())
            .map(|t| t.ends_with("__transfer"))
            .unwrap_or(false);
        if !is_transfer {
            continue;
        }
        let (Some(from), Some(to)) = (v["from"].as_str(), v["to"].as_str()) else {
            continue;
        };
        if let Some(val) = v["value"].as_str().and_then(|s| s.parse::<i128>().ok()) {
            batch.extend(views::transfer_deltas(from, to, val, -1));
        }
    }
    batch
}

/// Build a weight −1 exposure retraction batch from rolled-back transfer rows (reorg). Reads each
/// table's (from, to, value) column names from the registry - they vary by token (USDC from/to/value,
/// WETH src/dst/wad) - then re-derives the same exposure deltas the live path fed, with weight −1, so
/// a reorged flag/exposure retracts exactly like a balance.
fn exposure_retraction_batch(
    entity_json: &[String],
    registry: &DecodeRegistry,
    labels: &LabelSet,
) -> exposure::ExposureBatch {
    // table → (from_col, to_col, value_col) for every transfer-shaped table.
    let cols: std::collections::HashMap<String, (String, String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns().map(|(f, t, v)| {
                (
                    d.table.clone(),
                    (f.to_string(), t.to_string(), v.to_string()),
                )
            })
        })
        .collect();

    let mut batch = Vec::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else {
            continue;
        };
        let Some(table) = v.get("table").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some((from_col, to_col, val_col)) = cols.get(table) else {
            continue; // not a transfer table
        };
        if let (Some(from), Some(to), Some(val)) = (
            v[from_col].as_str(),
            v[to_col].as_str(),
            v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
        ) {
            batch.extend(exposure::exposure_deltas(from, to, val, -1, labels));
        }
    }
    batch
}

/// Build a weight −1 velocity retraction batch from rolled-back transfer rows (reorg). Re-derives the
/// sender's outbound-volume delta the live path fed, with weight −1, so a reorged velocity flag drops.
fn velocity_retraction_batch(
    entity_json: &[String],
    registry: &DecodeRegistry,
    window: u64,
) -> velocity::VelocityBatch {
    let cols: std::collections::HashMap<String, (String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns()
                .map(|(f, _t, v)| (d.table.clone(), (f.to_string(), v.to_string())))
        })
        .collect();

    let mut batch = Vec::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(j) else {
            continue;
        };
        let Some(table) = v.get("table").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some((from_col, val_col)) = cols.get(table) else {
            continue;
        };
        if let (Some(from), Some(block), Some(val)) = (
            v[from_col].as_str(),
            v["block_number"].as_u64(),
            v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
        ) {
            batch.extend(velocity::velocity_deltas(from, block, val, -1, window));
        }
    }
    batch
}

/// The derived IVM views a restart has to reconstruct, and the two things that decide whether each is
/// fed at all. They travel together because they are always rebuilt together, off one pass over the
/// same facts - passed singly they made `rebuild_views` an eight-argument function.
#[derive(Clone, Copy)]
struct DerivedViews<'a> {
    /// Exposure joins transfers against this; empty means the view can only ever be empty.
    labels: &'a LabelSet,
    balances: &'a BalanceView,
    exposure: &'a ExposureView,
    velocity: &'a VelocityView,
    /// `Some(window)` only when a velocity flag is configured (RFC-0008 C3).
    velocity_window: Option<u64>,
}

/// Rebuild every derived IVM view - balances, exposure and velocity - from stored facts, in a single
/// pass over the hot store.
///
/// The views are derived state, not durable state: rather than persist them (and risk drift from the
/// canonical store) we reconstruct them from the facts that *are* durable, through the same circuits
/// that maintain them live. All three are built the same way. Cold (sealed, immutable) segments fold
/// to one pre-summed row per key directly in DuckDB - no need to replay millions of transfers - and
/// only the small un-sealed hot tail is replayed transfer by transfer. Hot and cold are disjoint
/// (sealed rows are pruned from hot), so nothing is double-counted, and the result is identical to
/// views grown from genesis.
///
/// This was three functions, each opening its own scan (issue #294). That was never *three* scans: it
/// was `3 × transfer_tables`, because `recent_by_table` walks the whole entity table and JSON-parses
/// every row just to test its `table` field, and each rebuild then parsed the rows it got back a
/// second time. `hot_rows_by_table` walks once, parses each row once, and returns them already
/// grouped, so one scan now feeds all three views. This runs on every restart and every crash
/// recovery, which is precisely when an operator is watching.
///
/// The scan is deliberately **unbounded**, unlike `/sql` and `/explain` which cap it and answer 503.
/// A query may refuse and be retried; a rebuild may not. Dropping hot rows here would leave a view
/// quietly missing its tip for the life of the process - those transfers are never decoded again - and
/// a silently wrong balance is far worse than a slow start.
fn rebuild_views(
    dir: &std::path::Path,
    store: &dyn crate::store::HotStore,
    registry: &DecodeRegistry,
    into: &DerivedViews<'_>,
) -> Result<()> {
    let DerivedViews {
        labels,
        balances,
        exposure,
        velocity,
        velocity_window,
    } = *into;

    // Each transfer table with its (from, to, value) column names - which vary by token (USDC:
    // from/to/value; WETH: src/dst/wad), so we read them from the registry, never hardcode them.
    // Velocity uses the same list and simply ignores `to`.
    let transfer_tables: Vec<(String, String, String, String)> = registry
        .tables()
        .iter()
        .filter_map(|d| {
            d.transfer_columns()
                .map(|(f, t, v)| (d.table.clone(), f.to_string(), t.to_string(), v.to_string()))
        })
        .collect();
    if transfer_tables.is_empty() {
        return Ok(());
    }

    // The gates the three separate rebuilds each applied. Exposure joins transfers against the
    // labeled set, so with no labels it can only ever be empty - there is nothing to be exposed *to*.
    // Velocity is fed only when a velocity flag is configured (RFC-0008 C3).
    let want_exposure = !labels.is_empty();

    let sealed_through = store.sealed_through();

    let mut balance_batch: views::WeightedBatch = Vec::new();
    let mut exposure_batch: exposure::ExposureBatch = Vec::new();
    let mut velocity_batch: velocity::VelocityBatch = Vec::new();

    // Cold seed. Three different aggregations over the same segments, so this stays three DuckDB
    // queries per table - they are pre-summed server-side and cheap, and it is the hot store, not
    // DuckDB, that #294 was about. A table with no sealed segment yet has no view; that just means it
    // has nothing cold to seed, so an error here is a debug line, not a failure.
    let mut cold_balances = 0usize;
    let mut cold_exposure = 0usize;
    let mut cold_velocity = 0usize;
    for (table, from_col, to_col, val_col) in &transfer_tables {
        match crate::analytics::net_balances(dir, table, from_col, to_col, val_col, sealed_through)
        {
            Ok(nets) => {
                cold_balances += nets.len();
                for (addr, net) in nets {
                    balance_batch.push(views::seed_delta(addr, net));
                }
            }
            Err(e) => tracing::debug!("no cold seed for {table}: {e:#}"),
        }
        // COR-8 (#814): the fold silently drops a transfer whose value exceeds `i128`. The drop is
        // correct - both legs go, or the balance would gain value from nowhere - but it used to be
        // invisible, so a balance missing a transfer was served exactly like a complete one.
        match crate::analytics::over_i128_transfers(dir, table, val_col, sealed_through) {
            Ok(0) => {}
            Ok(n) => {
                tracing::warn!(
                    "{n} transfer(s) in {table} have a value that does not fit i128 and are \
                     excluded from balances; /balances reports this as `dropped_over_i128` (#814)"
                );
                balances.note_over_i128(n);
            }
            // A table with no sealed segment has no view to count over; that is not a failure.
            Err(e) => tracing::debug!("no over-i128 count for {table}: {e:#}"),
        }
        if want_exposure {
            match crate::analytics::cold_exposure(
                dir,
                table,
                from_col,
                to_col,
                val_col,
                sealed_through,
            ) {
                Ok(rows) => {
                    cold_exposure += rows.len();
                    for (key, amount, count) in rows {
                        exposure_batch.push(exposure::seed_item(key, amount, count));
                    }
                }
                Err(e) => tracing::debug!("no cold exposure seed for {table}: {e:#}"),
            }
        }
        if let Some(window) = velocity_window {
            match crate::analytics::cold_velocity(
                dir,
                table,
                from_col,
                val_col,
                window,
                sealed_through,
            ) {
                Ok(rows) => {
                    cold_velocity += rows.len();
                    for (key, volume, count) in rows {
                        velocity_batch.push(velocity::seed_item(key, volume, count));
                    }
                }
                Err(e) => tracing::debug!("no cold velocity seed for {table}: {e:#}"),
            }
        }
    }

    // Hot replay: the un-sealed tip, fed through the same delta paths the live loop uses. One scan,
    // one parse, fanned out to all three views.
    //
    // A read failure propagates rather than being swallowed into an empty tail. The old per-view
    // `unwrap_or_default()` would have applied the cold seed alone and logged nothing - a view short
    // of its entire hot tail, presented as a successful rebuild. The caller warns and carries on.
    let hot_rows = store.hot_rows_by_table()?;
    let mut hot_balances = 0usize;
    let mut hot_exposure = 0usize;
    let mut hot_velocity = 0usize;
    for (table, from_col, to_col, val_col) in &transfer_tables {
        let Some(rows) = hot_rows.get(table) else {
            continue;
        };
        // Newest first, matching the order `recent_by_table` returned. The circuits fold weighted
        // deltas, so the summed result does not depend on it - but keeping the order identical means
        // "same views as the three-scan build" holds structurally rather than by argument.
        for v in rows.iter().rev() {
            if let (Some(from), Some(to), Some(val)) = (
                v[from_col].as_str(),
                v[to_col].as_str(),
                v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
            ) {
                balance_batch.extend(views::transfer_deltas(from, to, val, 1));
                hot_balances += 1;
                if want_exposure {
                    let d = exposure::exposure_deltas(from, to, val, 1, labels);
                    if !d.is_empty() {
                        hot_exposure += 1;
                        exposure_batch.extend(d);
                    }
                }
            }
            // Velocity needs (from, block, value) and not `to`, so it is judged on its own terms: a
            // row with an unreadable `to` still carries outbound volume.
            if let Some(window) = velocity_window {
                if let (Some(from), Some(block), Some(val)) = (
                    v[from_col].as_str(),
                    v["block_number"].as_u64(),
                    v[val_col].as_str().and_then(|s| s.parse::<i128>().ok()),
                ) {
                    velocity_batch.extend(velocity::velocity_deltas(from, block, val, 1, window));
                    hot_velocity += 1;
                }
            }
        }
    }

    if !balance_batch.is_empty() {
        balances.apply(balance_batch);
        balances.flush();
        tracing::info!(
            "rebuilt balance view: {} holders ({cold_balances} cold-seeded net(s) + {hot_balances} hot transfer(s) replayed)",
            balances.holders()
        );
    }
    if !exposure_batch.is_empty() {
        exposure.apply(exposure_batch);
        exposure.flush();
        tracing::info!(
            "rebuilt exposure view: {} entries ({cold_exposure} cold-seeded + {hot_exposure} hot transfer(s) replayed)",
            exposure.entries()
        );
    }
    if !velocity_batch.is_empty() {
        velocity.apply(velocity_batch);
        velocity.flush();
        tracing::info!(
            "rebuilt velocity view: {} bucket(s) ({cold_velocity} cold-seeded + {hot_velocity} hot transfer(s) replayed)",
            velocity.entries()
        );
    }
    Ok(())
}

/// Decode a window's logs in chain order (block, log_index), routing each to a contract decoder or -
/// for a factory nest - a discovered child's template decoder, and discovering new children inline so
/// same-window child activity decodes (RFC-0009). Timestamps may be empty here: #765 fetches headers
/// only after local filtering, then `apply_row_timestamps` / `ChildRegistry::apply_timestamps` fill
/// `block_timestamp` and `discovered_timestamp`. Pure aside from growing `children`.
fn decode_window(
    registry: &DecodeRegistry,
    factory: Option<&FactorySet>,
    children: &mut ChildRegistry,
    logs: &[crate::rpc::Log],
    timestamps: &std::collections::HashMap<u64, u64>,
) -> Vec<crate::registry::DecodedRow> {
    let mut ordered: Vec<&crate::rpc::Log> = logs.iter().collect();
    ordered.sort_by(|a, b| {
        a.block_number
            .cmp(&b.block_number)
            .then_with(|| a.log_index.cmp(&b.log_index))
    });

    let mut rows = Vec::new();
    for log in ordered {
        let decoded = match registry.decode(log) {
            Ok(Some(r)) => Some(r),
            Ok(None) => {
                // Not a contract event - route to a discovered child's template decoder, if any.
                factory.and_then(|_| {
                    let addr = log.address.to_ascii_lowercase();
                    children
                        .template_of(&addr)
                        .map(str::to_string)
                        .and_then(|tmpl| registry.decode_child(log, &tmpl).ok().flatten())
                })
            }
            Err(e) => {
                tracing::debug!("decode skipped: {e:#}");
                None
            }
        };
        if let Some(mut r) = decoded {
            r.block_timestamp = timestamps.get(&r.block_number).copied().unwrap_or(0);
            if let Some(fs) = factory {
                // Every child this event announces, not just the first: one event may name several
                // (issue #241).
                for child in fs.discover(&r) {
                    if children.insert(child.clone()) {
                        tracing::info!(
                            "factory discovered {} child {}… at block {}",
                            child.template,
                            &child.address[..12.min(child.address.len())],
                            child.discovered_block
                        );
                    }
                }
            }
            rows.push(r);
        }
    }
    rows
}

/// Rebuild the discovered-child registry on a warm restart by folding the stored factory events
/// (RFC-0009). Cold (sealed) and hot factory-event rows are read as JSON and re-discovered - a pure
/// fold, so the reconstructed registry is identical to the one grown live. Best-effort per table.
/// Rebuild the discovered-child registry from stored factory events (#373).
///
/// **Every read here is load-bearing and none of them may be swallowed.** What this returns is the
/// set of contracts the nest indexes; a rebuild that quietly yields *fewer* children than were
/// discovered does not fail, it silently stops indexing them, and the nest goes on looking healthy
/// with a hole in its data. That is the worst shape a fault can take in this codebase and it is the
/// one this function used to have three times over: a DuckDB failure was `if let Ok(cold)`, a hot
/// store failure was `.unwrap_or_default()`, and an unparseable stored row was `if let Ok(v)`.
///
/// The cold read is the only genuinely-expected absence, and it is now decided from the **segment
/// catalogue** rather than inferred from an error. That distinction is the whole fix: "this table
/// has never been sealed" and "DuckDB could not read this table" both arrived as `Err` and were
/// discarded together, so the benign case was hiding the fatal one.
fn rebuild_children(
    dir: &std::path::Path,
    store: &dyn crate::store::HotStore,
    _registry: &DecodeRegistry,
    factory: &FactorySet,
) -> Result<ChildRegistry> {
    let mut children = ChildRegistry::new();
    let manifest = crate::seal::load_manifest(dir)
        .context("reading the segment catalogue to rebuild the child registry")?;
    // Fold in block order so the earliest discovery of each child wins (matches the live path).
    for table in factory.factory_tables() {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        // Cold (sealed) rows via DuckDB - but only where the catalogue says a segment exists, so an
        // unsealed table is skipped rather than queried-and-forgiven.
        if manifest.tables.get(&table).is_some_and(|s| !s.is_empty()) {
            rows.extend(
                crate::analytics::query(dir, &format!("SELECT * FROM \"{table}\""))
                    .with_context(|| format!("reading sealed factory rows for '{table}' (#373)"))?,
            );
        }
        for raw in store
            .recent_by_table(&table, usize::MAX)
            .with_context(|| format!("reading hot factory rows for '{table}' (#373)"))?
        {
            rows.push(
                serde_json::from_str::<serde_json::Value>(&raw).with_context(|| {
                    format!("unparseable stored row in factory table '{table}' (#373)")
                })?,
            );
        }
        rows.sort_by(|a, b| {
            let key = |v: &serde_json::Value| {
                (
                    v.get("block_number")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    v.get("log_index")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                )
            };
            key(a).cmp(&key(b))
        });
        for v in &rows {
            for child in factory.discover_stored(&table, v) {
                children.insert(child);
            }
        }
    }
    Ok(children)
}

async fn sleep_secs(s: u64) {
    tokio::time::sleep(std::time::Duration::from_secs(s)).await;
}

/// Log a failed tip poll with escalating severity and return the incremented consecutive-failure count
/// (the loop resets it to 0 on the next success). A warn on the first failure (a transient blip), then
/// a loud error every ~60 s of sustained failure - every RPC endpoint is unreachable and indexing has
/// stalled. Paired with `METRICS.mark_poll_ok()` on success, this is the honest stall signal a
/// supervisor (and the `/ready` endpoint) reads. Retries never drop blocks - the same window re-fetches.
fn escalate_stall(failures: u32, e: &anyhow::Error) -> u32 {
    let n = failures + 1;
    match n {
        1 => tracing::warn!("tip lookup failed: {e:#}; retrying"),
        n if n % 20 == 0 => tracing::error!(
            "all RPC endpoints unreachable for ~{}s ({n} polls) - indexing STALLED; it resumes \
             automatically when an endpoint recovers",
            n * 3
        ),
        _ => {}
    }
    n
}

/// Reduce a webhook URL to `scheme://host[:port]` for display on the unauthenticated `/nest` surface -
/// dropping the path/query/userinfo where webhook secrets live (SEC review: `/nest` URL leak). A
/// best-effort string parse (no url dep); a malformed URL degrades to a bare "configured".
fn webhook_host(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    // Drop any `user:pass@` userinfo, keep `host[:port]`.
    let host = authority.rsplit('@').next().unwrap_or(authority);
    match (scheme.is_empty(), host.is_empty()) {
        (_, true) => "configured".to_string(),
        (true, false) => host.to_string(),
        (false, false) => format!("{scheme}://{host}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------------------
    // #980 - seal boundaries are a function of the data, not of the fetch batching.
    //
    // `tests/seal_batching_asymmetry.rs` used to assert this by reading `indexer.rs` as a string
    // and checking the 1400 characters before `fn take_sealable` contained the words
    // "from the **data**" and "identical". It sealed nothing. The window it searched *was* the doc
    // comment, so it was a gate matching its own documentation: an implementation could make cuts
    // depend on fetch batches, keep the prose intact, and stay green.
    //
    // These live here rather than in `tests/` because `take_sealable` is private, which is the
    // constraint that pushed the original at the source text in the first place.
    // ---------------------------------------------------------------------------------------

    /// Drive `take_all_sealable` the way the backfill loop actually does: fetch a **window of
    /// blocks**, append every row for those blocks, then seal whatever is sealable. Repeat.
    ///
    /// The unit is blocks, not rows, and that is load-bearing. `--window` and `--concurrency` size a
    /// *block range*, and the loop appends all rows for the range before calling `take_all_sealable`, so
    /// `buf` always ends on a whole block. A row-level partition would split a block mid-run, which
    /// no operator configuration can produce - and it would fail, because `partition_point` can only
    /// include the rows that are present. Modelling the wrong unit here would invent a defect.
    ///
    /// Returns one entry per sealed segment plus the unsealed remainder: together, the observable
    /// output that a differently-tuned operator has to reproduce byte for byte.
    /// One sealed segment: its rows, and the block it was cut at.
    type Segment = (Vec<String>, u64);
    /// Everything a run produces: the segments, and the unsealed remainder.
    type SealRun = (Vec<Segment>, Vec<String>);

    fn seal_through_windows(rows: &[SealRow], last_block: u64, window: u64) -> SealRun {
        let mut buf: Vec<SealRow> = Vec::new();
        let mut segments = Vec::new();
        let mut from = 0u64;
        while from <= last_block {
            let to = (from + window - 1).min(last_block);
            // Every window refetches the tail of the one before it, exactly as production does
            // (#1144), through the production merge.
            let fetch_from = overlap_from(from, 0);
            merge_window_rows(
                &mut buf,
                fetch_from,
                rows.iter()
                    .filter(|r| r.0 >= fetch_from && r.0 <= to)
                    .cloned(),
            )
            .unwrap();
            from = to + 1;
            // Production drain, not a copy of it (#1015). Reverting `drain_all_sealable`'s
            // `while` to `if` is this test going red.
            let _ = drain_all_sealable(
                &mut buf,
                tail_hold(to, to >= last_block),
                |rows, seal_to| {
                    segments.push((rows, seal_to));
                    Ok(())
                },
            );
        }
        (segments, drain_sealable(&mut buf))
    }

    /// Rows in `(block, log_index)` order with an uneven number per block, so a cut cannot land on a
    /// tidy multiple and a block genuinely straddles the seal threshold.
    fn corpus(blocks: u64) -> Vec<SealRow> {
        let mut rows = Vec::new();
        for b in 0..blocks {
            for i in 0..((b % 7) + 1) {
                rows.push((b, i, format!("{{\"block\":{b},\"i\":{i}}}")));
            }
        }
        rows
    }

    #[test]
    fn seal_boundaries_are_identical_across_fetch_windows() {
        let blocks = 30_000u64;
        let rows = corpus(blocks);
        assert!(
            rows.len() > SEAL_DIRECT_BATCH * 3,
            "corpus too small to cross the seal threshold repeatedly: {} rows vs batch {}",
            rows.len(),
            SEAL_DIRECT_BATCH
        );

        // Five operators with wildly different RPC tuning against the same chain. `--window` is the
        // knob; 320 is what `nuthatch doctor` recommends for a range-only provider, 163840 is the
        // upper end it measured, and the ugly ones exist so no boundary can be an artefact of a
        // round number.
        let mut reference: Option<SealRun> = None;
        for window in [1u64, 7, 320, 4_999, 163_840] {
            let got = seal_through_windows(&rows, blocks - 1, window);
            match &reference {
                None => {
                    assert!(
                        !got.0.is_empty(),
                        "the reference run sealed nothing, so every later comparison would be \
                         vacuously equal - the corpus never crossed SEAL_DIRECT_BATCH"
                    );
                    reference = Some(got);
                }
                Some(want) => {
                    assert_eq!(
                        got.0.len(),
                        want.0.len(),
                        "window={window} produced a different NUMBER of segments ({} vs {}). \
                         Segment identity would then depend on the operator's --window, which is \
                         the bug RFC-0028 §4 fixed and the property RFC-0019 bundles and RFC-0020 \
                         segment reuse both rest on (#947, #980)",
                        got.0.len(),
                        want.0.len()
                    );
                    for (i, (a, b)) in got.0.iter().zip(want.0.iter()).enumerate() {
                        assert_eq!(
                            a.1, b.1,
                            "window={window}: segment {i} was cut at block {} instead of {}",
                            a.1, b.1
                        );
                        assert_eq!(
                            a.0, b.0,
                            "window={window}: segment {i} carries different ROWS, so the two \
                             operators' segments are not content-identical and will not dedup"
                        );
                    }
                    assert_eq!(
                        got.1, want.1,
                        "window={window}: the unsealed remainder differs"
                    );
                }
            }
        }
    }

    #[test]
    fn a_block_is_never_split_across_two_segments() {
        // The other half of the documented rule, and what makes the cut data-derived: if one block
        // carries the buffer past the threshold, the whole block goes into that segment.
        let blocks = 30_000u64;
        let (segments, _) = seal_through_windows(&corpus(blocks), blocks - 1, 512);
        assert!(
            !segments.is_empty(),
            "nothing sealed - the corpus never crossed the threshold"
        );
        let mut home: std::collections::HashMap<u64, usize> = std::collections::HashMap::new();
        for (i, (rows, _)) in segments.iter().enumerate() {
            for r in rows {
                let b: u64 = r
                    .split(':')
                    .nth(1)
                    .and_then(|t| t.split(',').next())
                    .and_then(|t| t.parse().ok())
                    .expect("parse block out of the row json");
                if let Some(prev) = home.insert(b, i) {
                    assert_eq!(
                        prev, i,
                        "block {b} appears in segment {prev} and segment {i} - a block was split \
                         across a seal boundary, so the segment's contents depend on where the \
                         buffer happened to fill rather than on the data"
                    );
                }
            }
        }
    }

    #[test]
    fn the_observable_can_actually_see_a_boundary_move() {
        // The control, and the reason the two tests above are not vacuous. If a change made the
        // boundary depend on the fetch, this comparison has to be sensitive enough to notice - so
        // prove it by changing the DATA and watching the first cut move.
        let blocks = 30_000u64;
        let base = seal_through_windows(&corpus(blocks), blocks - 1, 512);
        let mut denser = corpus(blocks);
        // 600 extra rows in block 0 shifts which block sits at index SEAL_DIRECT_BATCH - 1.
        for i in 0..600 {
            denser.insert(
                0,
                (0, 100 + i, format!("{{\"block\":0,\"i\":{}}}", 100 + i)),
            );
        }
        let moved = seal_through_windows(&denser, blocks - 1, 512);
        assert_ne!(
            base.0.first().map(|s| s.1),
            moved.0.first().map(|s| s.1),
            "adding rows to the first block did not move the first seal boundary, so this \
             observable cannot see a boundary change at all and the assertions above would pass \
             against a broken implementation"
        );
    }

    /// #1150 - the window-independence property, end to end, on a nest of **many tables** with the
    /// table floor in play. The three tests above prove the *cut* is a function of the data through
    /// the in-memory drain; this one carries the same drain through the real `seal_range` into a
    /// directory per operator, with a busy table and two sparse ones, and compares what lands on
    /// disk: every segment's hash, range and provisional flag, per table. A sparse table folds
    /// across cuts, and the fold must be as window-independent as the cut is - which it is only
    /// because both are stated in rows counted from the data.
    #[test]
    fn many_table_seal_output_is_identical_across_fetch_windows_with_the_floor_in_play() {
        let blocks = 30_000u64;
        let mut rows: Vec<SealRow> = Vec::new();
        for b in 0..blocks {
            for i in 0..((b % 7) + 1) {
                rows.push((
                    b,
                    i,
                    format!(r#"{{"table":"busy","block_number":{b},"log_index":{i},"v":"{i}"}}"#),
                ));
            }
            // About one row in 200 of the busy table's: under the floor at every cut, so it folds.
            if b % 50 == 0 {
                rows.push((
                    b,
                    10,
                    format!(r#"{{"table":"sparse_a","block_number":{b},"log_index":10}}"#),
                ));
            }
            // Rarer still: a handful of rows across the whole corpus, one provisional file at the end.
            if b % 700 == 0 {
                rows.push((
                    b,
                    11,
                    format!(r#"{{"table":"sparse_b","block_number":{b},"log_index":11}}"#),
                ));
            }
        }
        assert!(rows.len() > SEAL_DIRECT_BATCH * 3);

        /// One segment as it lands in the manifest: `(hash, from_block, to_block, rows, provisional)`.
        type Landed = (String, u64, u64, usize, bool);
        let run = |window: u64| -> std::collections::BTreeMap<String, Vec<Landed>> {
            let dir = tempfile::tempdir().unwrap();
            let mut buf: Vec<SealRow> = Vec::new();
            let mut from = 0u64;
            let mut batch_from = 0u64;
            while from < blocks {
                let to = (from + window - 1).min(blocks - 1);
                let fetch_from = overlap_from(from, 0);
                merge_window_rows(
                    &mut buf,
                    fetch_from,
                    rows.iter()
                        .filter(|r| r.0 >= fetch_from && r.0 <= to)
                        .cloned(),
                )
                .unwrap();
                from = to + 1;
                drain_all_sealable(
                    &mut buf,
                    tail_hold(to, to >= blocks - 1),
                    |json, seal_to| {
                        seal::seal_range(dir.path(), &json, batch_from, seal_to)?;
                        batch_from = seal_to + 1;
                        Ok(())
                    },
                )
                .unwrap();
            }
            if !buf.is_empty() {
                seal::seal_range(
                    dir.path(),
                    &drain_sealable(&mut buf),
                    batch_from,
                    blocks - 1,
                )
                .unwrap();
            }
            seal::load_manifest(dir.path())
                .unwrap()
                .tables
                .into_iter()
                .map(|(t, segs)| {
                    (
                        t,
                        segs.into_iter()
                            .map(|s| (s.hash, s.from_block, s.to_block, s.rows, s.provisional))
                            .collect(),
                    )
                })
                .collect()
        };

        let reference = run(320);
        // The premise: the floor did something here. `busy` is final at every cut; each sparse
        // table is a single file that folded across many cuts rather than one file per cut.
        let cuts = reference["busy"].len();
        assert!(
            cuts > 3,
            "the corpus crossed the threshold only {cuts} times"
        );
        // Every cut of the busy table is final; only the range-end flush of its remainder may be
        // provisional, and that one folds into the next cut when the nest carries on.
        assert!(
            reference["busy"][..cuts - 1].iter().all(|s| !s.4),
            "busy is final at every cut"
        );
        for t in ["sparse_a", "sparse_b"] {
            assert_eq!(
                reference[t].len(),
                1,
                "{t} should be one folded file across {cuts} cuts, not one per cut: {:?}",
                reference[t]
            );
            assert!(
                reference[t][0].4,
                "{t} is under the floor and so provisional"
            );
        }
        assert_eq!(reference["sparse_a"][0].3, (blocks / 50) as usize);

        for window in [7u64, 4_999, 163_840] {
            let got = run(window);
            assert_eq!(
                got, reference,
                "window={window} produced different segments from window=320 on a many-table \
                 nest - the fold, not only the cut, has to be a function of the data (#1150)"
            );
        }
    }

    fn entity_json(block: u64, i: u64) -> String {
        format!(r#"{{"table":"t__x","block_number":{block},"log_index":{i},"v":"{i}"}}"#)
    }

    fn entities_for(blocks: u64, per_block: u64) -> Vec<String> {
        let mut out = Vec::new();
        for b in 0..blocks {
            for i in 0..per_block {
                out.push(entity_json(b, i));
            }
        }
        out
    }

    #[test]
    fn the_tip_path_holds_below_the_batch_threshold() {
        let short = entities_for(100, 3);
        assert!(
            short.len() < SEAL_DIRECT_BATCH,
            "fixture accidentally crossed the threshold"
        );
        assert_eq!(
            tip_seal_cut(&short),
            None,
            "the tip path sealed {n} rows, below SEAL_DIRECT_BATCH={SEAL_DIRECT_BATCH}. \
             That is the 6 KB median: each finality advance becoming its own Parquet file (#1067).",
            n = short.len()
        );
        assert_eq!(
            tip_seal_cut(&[]),
            None,
            "an empty range must also return None so the caller can advance the watermark \
             without inventing a segment"
        );
    }

    #[test]
    fn the_tip_path_cuts_at_the_same_block_as_the_backfill_path() {
        let mut rows = Vec::new();
        let mut json = Vec::new();
        for b in 0..30_000u64 {
            for i in 0..((b % 7) + 1) {
                rows.push((b, i, format!("{b}:{i}")));
                json.push(entity_json(b, i));
            }
        }
        assert!(json.len() > SEAL_DIRECT_BATCH);

        let backfill_cut = take_sealable(&mut rows, u64::MAX).map(|(_, b)| b);
        let tip_cut = tip_seal_cut(&json);
        assert_eq!(
            tip_cut, backfill_cut,
            "tip and backfill cuts disagree, so a nest that backfilled and then followed the tip \
             would produce a different segment than one that sealed the same rows in one pass"
        );
        let cut = tip_cut.expect("corpus crossed the threshold");
        let in_cut = json
            .iter()
            .filter(|j| block_number_of(j) == Some(cut))
            .count();
        assert!(in_cut >= 1, "the cut block has no rows in the corpus");
        let before_or_at: usize = json
            .iter()
            .take_while(|j| block_number_of(j).expect("block") <= cut)
            .count();
        assert!(
            before_or_at >= SEAL_DIRECT_BATCH,
            "the cut left the threshold row in the remainder"
        );
    }

    #[test]
    fn two_tip_schedules_produce_the_same_cuts() {
        let json = entities_for(9_000, 5);
        assert!(json.len() > SEAL_DIRECT_BATCH * 2);

        fn cuts_of(json: &[String]) -> Vec<u64> {
            let mut cuts = Vec::new();
            let mut start = 0usize;
            while start < json.len() {
                match tip_seal_cut(&json[start..]) {
                    None => break,
                    Some(cut) => {
                        let n = json[start..]
                            .iter()
                            .take_while(|j| block_number_of(j).expect("block") <= cut)
                            .count();
                        assert!(n > 0, "cut {cut} consumed no rows");
                        cuts.push(cut);
                        start += n;
                    }
                }
            }
            cuts
        }
        // One schedule: finality advances one block at a time, so each call to
        // `tip_seal_cut` sees only the prefix that has finalized. The other: the
        // whole corpus is already finalized. #1067 requires those to agree.
        fn cuts_as_finality_advances(json: &[String]) -> Vec<u64> {
            let mut cuts = Vec::new();
            let mut start = 0usize;
            let mut i = 0usize;
            while i < json.len() {
                let b = block_number_of(&json[i]).expect("block");
                while i < json.len() && block_number_of(&json[i]) == Some(b) {
                    i += 1;
                }
                if let Some(cut) = tip_seal_cut(&json[start..i]) {
                    cuts.push(cut);
                    let n = json[start..i]
                        .iter()
                        .take_while(|j| block_number_of(j).expect("block") <= cut)
                        .count();
                    start += n;
                }
            }
            cuts
        }
        let by_one = cuts_as_finality_advances(&json);
        let all_at_once = cuts_of(&json);
        assert!(
            !by_one.is_empty(),
            "the schedule never crossed the threshold, so the comparison is vacuous"
        );
        assert_eq!(
            by_one, all_at_once,
            "tip-path cuts depend on how finality was reported, which is the thing #1067 exists \
             to stop"
        );
    }

    #[test]
    fn dropping_the_tip_threshold_is_visible() {
        let short = entities_for(3, 10);
        assert!(short.len() < SEAL_DIRECT_BATCH);
        let old_rule_would_seal = !short.is_empty();
        let new_rule_seals = tip_seal_cut(&short).is_some();
        assert!(
            old_rule_would_seal && !new_rule_seals,
            "the old tip path and the new one agree on a {n}-row range, so this suite cannot \
             detect a revert to sealing every finality advance",
            n = short.len()
        );
    }

    struct HashOnly;

    #[async_trait::async_trait]
    impl Source for HashOnly {
        async fn tip(&self) -> Result<u64> {
            Ok(0)
        }
        async fn block_hash(&self, n: u64) -> Result<Option<String>> {
            Ok(Some(format!("{n:064x}")))
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            _from: u64,
            _to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            Ok(vec![])
        }
    }

    fn load_rows(store: &Store, n: u64) {
        let entities: Vec<(String, String)> = (0..n)
            .map(|i| (Store::entity_key(i, 0), entity_json(i, 0)))
            .collect();
        store
            .commit_window(&entities, Some((n - 1, "aa")), n - 1)
            .unwrap();
    }

    #[tokio::test]
    async fn maybe_seal_holds_a_short_finalized_range() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.redb")).unwrap();
        load_rows(&store, 100);
        let metrics = crate::metrics::NestMetrics::default();
        maybe_seal(tmp.path(), &store, &HashOnly, 99, None, &metrics)
            .await
            .unwrap();
        assert_eq!(
            store.get_meta(SEALED_THROUGH_KEY).unwrap(),
            None,
            "maybe_seal sealed a 100-row range. The batching lives in tip_seal_cut; if this \
             fires, maybe_seal stopped asking it (#1067)."
        );
        assert_eq!(
            store.entities_in_range(0, 99).unwrap().len(),
            100,
            "held rows were pruned from the hot store even though the watermark did not move"
        );
        assert!(
            seal::load_manifest(tmp.path())
                .map(|m| m.tables.is_empty())
                .unwrap_or(true),
            "a short range produced a segment file"
        );
        let pinned = format!("{:064x}", 99);
        assert_eq!(
            store.get_block_hash(99).unwrap().as_deref(),
            Some(pinned.as_str()),
            "a held finalized range must still pin a checkpoint at the ceiling, or a later \
             reorg walks past it to an older sparse checkpoint (#461 / #1067)"
        );
    }

    #[tokio::test]
    async fn maybe_seal_cuts_once_the_threshold_is_crossed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&tmp.path().join("t.redb")).unwrap();
        let n = SEAL_DIRECT_BATCH as u64 + 50;
        load_rows(&store, n);
        let metrics = crate::metrics::NestMetrics::default();
        maybe_seal(tmp.path(), &store, &HashOnly, n - 1, None, &metrics)
            .await
            .unwrap();
        let sealed = store
            .get_meta(SEALED_THROUGH_KEY)
            .unwrap()
            .expect("watermark must move once the threshold is crossed")
            .parse::<u64>()
            .unwrap();
        assert_eq!(
            sealed,
            SEAL_DIRECT_BATCH as u64 - 1,
            "one row per block, so the cut must be the block of row SEAL_DIRECT_BATCH-1"
        );
        let remain = store.entities_in_range(sealed + 1, n - 1).unwrap().len();
        assert_eq!(
            remain, 50,
            "the post-cut remainder must stay in the hot store"
        );
        let manifest = seal::load_manifest(tmp.path()).expect("manifest");
        assert!(
            !manifest.tables.is_empty(),
            "crossing the threshold produced no segment"
        );
    }

    #[tokio::test]
    async fn retry_transient_recovers_after_transient_failures() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let r: Result<u32> = retry_transient("op", std::time::Duration::ZERO, || async {
            // Fail the first two attempts (a rate-limit blip), succeed on the third.
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(anyhow::anyhow!("all RPC endpoints failed"))
            } else {
                Ok(42)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 42);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "should stop retrying once it succeeds"
        );
    }

    /// #538: a 5-attempt ceiling killed a multi-hour backfill over one bad window a bare restart
    /// resumed past for free, because it gave up faster than even an endpoint's ordinary 30s cooldown.
    /// `retry_transient` no longer has a ceiling at all - prove it by outliving the old one many times
    /// over and still recovering, exactly like `index_loop`'s tip-following getLogs fetch already does.
    #[tokio::test]
    async fn retry_transient_never_gives_up_and_recovers_past_the_old_five_attempt_ceiling() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let r: Result<u32> = retry_transient("op", std::time::Duration::ZERO, || async {
            // Fail 25 times - five times the old `BACKFILL_RETRY_ATTEMPTS = 5` - then recover.
            if calls.fetch_add(1, Ordering::SeqCst) < 25 {
                Err(anyhow::anyhow!("persistent 403"))
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(
            r.unwrap(),
            7,
            "must still recover no matter how many attempts it took"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 26);
    }

    #[test]
    fn backfill_backoff_doubles_then_caps() {
        let base = std::time::Duration::from_millis(250);
        assert_eq!(
            backfill_backoff(base, 1),
            std::time::Duration::from_millis(250)
        );
        assert_eq!(
            backfill_backoff(base, 2),
            std::time::Duration::from_millis(500)
        );
        assert_eq!(backfill_backoff(base, 3), std::time::Duration::from_secs(1));
        assert_eq!(backfill_backoff(base, 4), std::time::Duration::from_secs(2));
        // Attempt 7 (past the old 5-attempt ceiling) is still growing, not yet at the cap.
        assert_eq!(
            backfill_backoff(base, 7),
            std::time::Duration::from_secs(16)
        );
        // But it never exceeds the cap, however many attempts pile up - a stalled endpoint is polled
        // steadily, not at an ever-widening interval that would delay recovery once it comes back.
        assert_eq!(backfill_backoff(base, 100), BACKFILL_RETRY_BACKOFF_CAP);
        assert_eq!(
            backfill_backoff(base, usize::MAX),
            BACKFILL_RETRY_BACKOFF_CAP
        );
    }

    /// #559: the escalation condition in `log_backfill_retry` was only ever exercised through
    /// `tracing` macros no test asserted on, so `if false { error!(...) }` left the suite green.
    /// Asserting the extracted predicate directly kills that mutation (and an `attempt % 10`
    /// swapped for `% 1` or similar) without needing a `tracing` capture harness.
    #[test]
    fn should_escalate_backfill_retry_needs_both_the_cap_and_a_tenth_attempt() {
        let cap = BACKFILL_RETRY_BACKOFF_CAP;
        let below_cap = cap - std::time::Duration::from_millis(1);

        // Below the cap, never escalate - even on an attempt that is otherwise a multiple of ten.
        assert!(!should_escalate_backfill_retry(below_cap, 10));
        assert!(!should_escalate_backfill_retry(
            std::time::Duration::ZERO,
            10
        ));

        // At (or past) the cap, only every tenth attempt escalates.
        assert!(!should_escalate_backfill_retry(cap, 1));
        assert!(!should_escalate_backfill_retry(cap, 9));
        assert!(should_escalate_backfill_retry(cap, 10));
        assert!(!should_escalate_backfill_retry(cap, 11));
        assert!(should_escalate_backfill_retry(cap, 20));
        assert!(should_escalate_backfill_retry(
            cap + std::time::Duration::from_secs(1),
            30
        ));
    }

    /// With the ceiling gone (#538), the *pacing* is the only thing standing between a stalled endpoint
    /// and a loop hammering it as fast as the network will answer. [`backfill_backoff`] is proved above
    /// as a pure function and both retry loops are driven with `Duration::ZERO`, so nothing yet proved
    /// the loops sleep for what it returns: replacing both `sleep(backoff)` calls with
    /// `sleep(Duration::ZERO)` left all four tests in this group passing. Same shape as #361 - a
    /// mechanism tested everywhere except where it is wired in - so assert the schedule itself, on
    /// virtual time (`start_paused`, the `rpc.rs` idiom) so the assertion costs no wall-clock.
    #[tokio::test(start_paused = true)]
    async fn retry_transient_actually_sleeps_the_backoff_it_computes() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let start = tokio::time::Instant::now();
        // The real base, not ZERO: this is about the pacing an operator's endpoint actually gets.
        let r: Result<u32> = retry_transient("op", BACKFILL_RETRY_BASE, || async {
            // Twelve failures - five past the attempt at which the doubling reaches its cap.
            if calls.fetch_add(1, Ordering::SeqCst) < 12 {
                Err(anyhow::anyhow!("all RPC endpoints failed"))
            } else {
                Ok(7)
            }
        })
        .await;
        assert_eq!(r.unwrap(), 7);
        // 250ms doubling over attempts 1..=7 (250+500+1000+2000+4000+8000+16000 = 31.75s), then the
        // 30s cap over attempts 8..=12 (5 x 30s). Written out rather than summed from
        // `backfill_backoff`, so this stays an independent statement of the schedule.
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_millis(31_750) + std::time::Duration::from_secs(150),
            "the loop must sleep the computed backoff, not spin"
        );
    }

    /// `logs_with_retry` must keep retrying a plain transient failure past the old 5-attempt ceiling
    /// (#538) while still passing a result-cap error straight through on the first attempt, unretried,
    /// so the caller's window-shrink logic (not this function) handles it.
    #[tokio::test]
    async fn logs_with_retry_never_gives_up_on_transient_failure_but_passes_a_cap_through_at_once()
    {
        use crate::rpc::Log;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct FlakyThenCapped {
            calls: AtomicUsize,
            fail_until: usize,
            then_cap: bool,
        }
        #[async_trait::async_trait]
        impl Source for FlakyThenCapped {
            async fn tip(&self) -> Result<u64> {
                Ok(1000)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(&self, _filter: &LogFilter, from: u64, to: u64) -> Result<Vec<Log>> {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n < self.fail_until {
                    anyhow::bail!("transport error: connection reset");
                }
                if self.then_cap {
                    anyhow::bail!("query returned more than 10000 results");
                }
                Ok((from..=to)
                    .map(|b| Log {
                        address: "0xabc".into(),
                        topics: vec![],
                        data: "0x".into(),
                        block_number: b,
                        block_hash: "0x".into(),
                        tx_hash: "0x".into(),
                        log_index: 0,
                    })
                    .collect())
            }
        }
        let filter = LogFilter::new(&["0xabc".to_string()], &[]).expect("non-empty filter");

        // 20 transient failures - four times the old ceiling - then recovers.
        let flaky = FlakyThenCapped {
            calls: AtomicUsize::new(0),
            fail_until: 20,
            then_cap: false,
        };
        let logs = logs_with_retry(&flaky, &filter, 1, 10, std::time::Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(logs.len(), 10);
        assert_eq!(flaky.calls.load(Ordering::SeqCst), 21);

        // A result-cap error is handed back on the very first attempt, not retried.
        let capped = FlakyThenCapped {
            calls: AtomicUsize::new(0),
            fail_until: 0,
            then_cap: true,
        };
        // Bounded, and with a non-zero base, deliberately: without the pass-through arm this call never
        // returns at all, so the assertions below could only ever be reached by a green run. A
        // regression has to *fail*, not hang the job out to its timeout (the c0fb415 lesson on this
        // branch). A non-zero base makes the doomed loop sleep, so the deadline can actually fire; on
        // the correct path nothing sleeps and it costs nothing.
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            logs_with_retry(
                &capped,
                &filter,
                1,
                10,
                std::time::Duration::from_millis(50),
            ),
        )
        .await
        .expect("a cap error must be handed back at once, not retried")
        .unwrap_err();
        assert!(chunker::is_result_too_large(&err), "got: {err:#}");
        assert_eq!(
            capped.calls.load(Ordering::SeqCst),
            1,
            "a cap error must not be retried"
        );
    }

    #[test]
    fn webhook_host_drops_the_secret_bearing_path() {
        // A Slack-style webhook (secret in the path) reduces to scheme+host - the secret is gone.
        assert_eq!(
            webhook_host("https://hooks.slack.com/services/T00/B00/XXXXsecretXXXX"),
            "https://hooks.slack.com"
        );
        // Userinfo and port handled; query/fragment dropped.
        assert_eq!(
            webhook_host("https://user:pass@example.com:8443/hook?token=abc#f"),
            "https://example.com:8443"
        );
        assert_eq!(webhook_host("not a url"), "not a url");
        assert_eq!(webhook_host("https://"), "configured");
    }

    /// H2/H3: `fetch_logs_splitting` halves a range and retries when a provider caps the result, so an
    /// oversized window self-corrects instead of aborting the backfill; a single block that alone
    /// exceeds the cap can't be split, so it fails loudly rather than looping forever.
    #[tokio::test]
    async fn fetch_logs_splitting_shrinks_then_fails_on_a_single_block() {
        use crate::rpc::Log;
        struct CappedSource {
            cap: u64,
        }
        #[async_trait::async_trait]
        impl Source for CappedSource {
            async fn tip(&self) -> Result<u64> {
                Ok(1000)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                from: u64,
                to: u64,
            ) -> Result<Vec<Log>> {
                if to - from + 1 > self.cap {
                    anyhow::bail!("query returned more than 10000 results");
                }
                Ok((from..=to)
                    .map(|b| Log {
                        address: "0xabc".into(),
                        topics: vec![],
                        data: "0x".into(),
                        block_number: b,
                        block_hash: "0x".into(),
                        tx_hash: "0x".into(),
                        log_index: 0,
                    })
                    .collect())
            }
        }
        // A non-empty filter throughout. The empty-on-both-halves case cannot reach this function at
        // all any more - `LogFilter` refuses to represent it (#432) - so a filter built here is
        // always one the source is actually asked for, and no assertion below is vacuous.
        let filter = LogFilter::new(&["0xabc".to_string()], &[]).expect("non-empty filter");

        // A 100-block range against an 8-block cap splits all the way down and returns every log.
        let src = CappedSource { cap: 8 };
        let logs = fetch_logs_splitting(&src, &filter, 1, 100).await.unwrap();
        assert_eq!(logs.len(), 100);

        // A single block that itself exceeds the cap can't be split → a clear, loud error.
        let tiny = CappedSource { cap: 0 };
        let err = fetch_logs_splitting(&tiny, &filter, 42, 42)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("block 42 alone exceeds"), "got: {err}");
    }

    /// RFC-0028 §3b: a provider that refuses an oversized range **without saying so in words we
    /// recognise** must still get split.
    ///
    /// This is the measured failure: `arb1.arbitrum.io`, which nuthatch ships as an Arbitrum default,
    /// answers an oversized `getLogs` with `"logs matched by query exceeds limit of 10000"`. Before
    /// RFC-0028 that matched none of the cap markers, so this function never recursed and a busy
    /// Arbitrum backfill retried the same window forever. The marker is recognised now - but the
    /// durable guarantee is that an *unclassifiable* failure is split once anyway, so a provider whose
    /// phrasing we have never seen still works.
    #[tokio::test]
    async fn an_unclassifiable_wide_range_failure_is_split_speculatively() {
        use crate::rpc::Log;
        use std::sync::atomic::{AtomicU64, Ordering};
        /// Refuses ranges wider than `cap` with a message deliberately unlike anything in CAP_MARKERS.
        struct InscrutableSource {
            cap: u64,
            calls: AtomicU64,
        }
        #[async_trait::async_trait]
        impl Source for InscrutableSource {
            async fn tip(&self) -> Result<u64> {
                Ok(1000)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                from: u64,
                to: u64,
            ) -> Result<Vec<Log>> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                if to - from + 1 > self.cap {
                    // No "too large", no "response size", no "limit exceeded" - a provider we have
                    // never met, refusing in its own words.
                    anyhow::bail!("request rejected by upstream policy engine (code 7)");
                }
                Ok((from..=to)
                    .map(|b| Log {
                        address: "0xabc".into(),
                        topics: vec![],
                        data: "0x".into(),
                        block_number: b,
                        block_hash: "0x".into(),
                        tx_hash: "0x".into(),
                        log_index: 0,
                    })
                    .collect())
            }
        }

        // 10 blocks against a 5-block cap: the whole range fails unclassifiably, but each half fits.
        let src = InscrutableSource {
            cap: 5,
            calls: AtomicU64::new(0),
        };
        // Non-empty throughout - the empty-on-both-halves filter is unrepresentable (#432), so it
        // cannot silently turn these assertions into assertions about a request never made.
        let filter = LogFilter::new(&["0xabc".to_string()], &[]).expect("non-empty filter");
        let logs = fetch_logs_splitting(&src, &filter, 1, 10)
            .await
            .expect("a speculative split must rescue an unclassifiable range failure");
        assert_eq!(logs.len(), 10, "every log is returned, none dropped");

        // A genuinely dead endpoint must not fan out exponentially: the speculative split is tried
        // once (1 whole-range attempt + 2 halves), then the original error is surfaced.
        struct DeadSource {
            calls: AtomicU64,
        }
        #[async_trait::async_trait]
        impl Source for DeadSource {
            async fn tip(&self) -> Result<u64> {
                Ok(1000)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                _f: u64,
                _t2: u64,
            ) -> Result<Vec<Log>> {
                self.calls.fetch_add(1, Ordering::Relaxed);
                anyhow::bail!("connection reset by peer")
            }
        }
        let dead = DeadSource {
            calls: AtomicU64::new(0),
        };
        let err = fetch_logs_splitting(&dead, &filter, 1, 1024)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("getLogs 1..=1024"), "got: {err}");
        assert_eq!(
            dead.calls.load(Ordering::Relaxed),
            3,
            "one whole-range attempt plus one non-recursive split - not an exponential fan-out"
        );
    }

    /// An address-aware mock source: `logs` respects the address filter (empty = all), so a factory
    /// backfill's pass 1 (contracts) and pass 2 (children-only) return different logs, as on a real
    /// provider. Used to prove the two-pass discovery (RFC-0009 §3).
    struct FilteringSource {
        logs: Vec<crate::rpc::Log>,
    }

    #[async_trait::async_trait]
    impl Source for FilteringSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            let addrs = filter.addresses();
            let allow: std::collections::HashSet<String> =
                addrs.iter().map(|a| a.to_ascii_lowercase()).collect();
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .filter(|l| allow.is_empty() || allow.contains(&l.address.to_ascii_lowercase()))
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, b * 1000)).collect())
        }
    }

    /// RFC-0009 step 3 gate: the sequential two-pass backfill discovers a child in a chunk (pass 1's
    /// factory event) and re-fetches the chunk for that child (pass 2), so the child's *historical*
    /// activity is sealed - even though it wasn't in pass 1's address filter.
    #[tokio::test]
    async fn factory_backfill_two_pass_seals_child_activity() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;

        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_addr = "0x2222222222222222222222222222222222222222";
        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };
        let config: Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        // Pool created at block 10; its Swap at block 15 - both in the backfill range, but the Swap
        // is only reachable in pass 2 (the pool isn't in pass 1's contract-only filter).
        let source = FilteringSource {
            logs: vec![
                Log {
                    address: factory_addr.into(),
                    topics: vec![topic0("factory__pool_created")],
                    data: format!("0x{:0>64}", pool_addr.trim_start_matches("0x")),
                    block_number: 10,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt1".into(),
                    log_index: 0,
                },
                Log {
                    address: pool_addr.into(),
                    topics: vec![topic0("pool__swap")],
                    data: format!("0x{:064x}", 999u64),
                    block_number: 15,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt2".into(),
                    log_index: 0,
                },
            ],
        };

        let dir = tempfile::tempdir().unwrap();
        let mut children = ChildRegistry::new();
        let sealed = backfill_direct_factory(
            &source,
            &reg,
            &fs,
            &mut children,
            dir.path(),
            &[],
            &[],
            None,
            0,
            10,
            20,
            100,
            false,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(
            sealed, 2,
            "the factory event and the child's historical swap both sealed"
        );
        assert!(
            children.contains(pool_addr),
            "the pool was discovered during backfill"
        );
        // RFC-0009 step 4: every factory segment records the discovered-child registry snapshot.
        let manifest = crate::seal::load_manifest(dir.path()).unwrap();
        let snap = children.hash();
        assert!(
            manifest
                .tables
                .values()
                .flatten()
                .all(|s| s.registry_snapshot.as_deref() == Some(snap.as_str())),
            "factory segments carry the registry snapshot"
        );
        // The child's Swap is queryable from the sealed segment.
        let n = crate::analytics::query(dir.path(), r#"SELECT count(*) AS n FROM "pool__swap""#)
            .unwrap();
        assert_eq!(n[0]["n"], serde_json::Value::from(1u64));
        let row =
            crate::analytics::query(dir.path(), r#"SELECT address FROM "pool__swap""#).unwrap();
        assert_eq!(row[0]["address"], serde_json::Value::from(pool_addr));
    }

    /// Pass 2 must shrink the window on a provider response cap, exactly as pass 1 already does.
    ///
    /// Found in the field, not by reading: `uniswap-v2` on mainnet died with
    /// `getLogs (children) 25791463..=25811399: Log response size exceeded`. Pass 1 had *succeeded*
    /// at that same 19,936-block window, because its filter is the factory plus already-known
    /// children; pass 2 asks the harder question - the children born in this chunk, which are the
    /// freshly-created and therefore busiest ones - and it carried a bare `?` where every other
    /// cap-handling site in this file shrinks and retries.
    ///
    /// The source below refuses any child-filtered request wider than 8 blocks and answers narrower
    /// ones, so a run that cannot shrink pass 2 cannot finish. Before the fix this returned
    /// `Err(Log response size exceeded)`.
    #[tokio::test]
    async fn pass_two_shrinks_the_window_on_a_provider_cap() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_addr = "0x2222222222222222222222222222222222222222";

        /// Answers pass 1 at any width, refuses pass 2 above `max_child_width`.
        struct CappedChildSource {
            logs: Vec<Log>,
            factory: String,
            max_child_width: u64,
            refusals: AtomicUsize,
        }

        #[async_trait::async_trait]
        impl Source for CappedChildSource {
            async fn tip(&self) -> Result<u64> {
                Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                filter: &crate::source::LogFilter,
                from: u64,
                to: u64,
            ) -> Result<Vec<Log>> {
                let addrs = filter.addresses();
                let allow: std::collections::HashSet<String> =
                    addrs.iter().map(|a| a.to_ascii_lowercase()).collect();
                // Pass 2 is the request that does *not* carry the factory: it asks only about the
                // children discovered in this chunk.
                let is_child_pass =
                    !allow.is_empty() && !allow.contains(&self.factory.to_ascii_lowercase());
                if is_child_pass && to.saturating_sub(from) + 1 > self.max_child_width {
                    self.refusals.fetch_add(1, Ordering::SeqCst);
                    // Alchemy's wording, which `chunker::is_result_too_large` matches on
                    // ("response size") via the textual fallback for non-RpcClient sources.
                    anyhow::bail!("Log response size exceeded for {from}..={to}");
                }
                Ok(self
                    .logs
                    .iter()
                    .filter(|l| l.block_number >= from && l.block_number <= to)
                    .filter(|l| allow.is_empty() || allow.contains(&l.address.to_ascii_lowercase()))
                    .cloned()
                    .collect())
            }
            async fn block_timestamps(
                &self,
                blocks: &[u64],
            ) -> Result<std::collections::HashMap<u64, u64>> {
                Ok(blocks.iter().map(|&b| (b, b * 1000)).collect())
            }
        }

        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };
        let config: Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        let source = CappedChildSource {
            factory: factory_addr.into(),
            max_child_width: 8,
            refusals: AtomicUsize::new(0),
            logs: vec![
                Log {
                    address: factory_addr.into(),
                    topics: vec![topic0("factory__pool_created")],
                    data: format!("0x{:0>64}", pool_addr.trim_start_matches("0x")),
                    block_number: 2,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt1".into(),
                    log_index: 0,
                },
                Log {
                    address: pool_addr.into(),
                    topics: vec![topic0("pool__swap")],
                    data: format!("0x{:064x}", 7u64),
                    block_number: 5,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt2".into(),
                    log_index: 0,
                },
            ],
        };

        let dir = tempfile::tempdir().unwrap();
        let mut children = ChildRegistry::new();
        let sealed = backfill_direct_factory(
            &source,
            &reg,
            &fs,
            &mut children,
            dir.path(),
            &[],
            &[],
            None,
            0,
            0,
            40,
            64,
            false,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .expect("pass 2 must shrink and finish, not abort");

        assert!(
            source.refusals.load(Ordering::SeqCst) > 0,
            "the cap never fired - the test proves nothing unless pass 2 was actually refused"
        );
        assert_eq!(
            sealed, 2,
            "the factory event and the child's swap both sealed despite the cap"
        );
        assert!(
            children.contains(pool_addr),
            "the pool was still discovered"
        );
    }

    /// RFC-0009 step 3a: the factory backfill is **deterministic** - the same range over the same
    /// chain history seals byte-identical segments (identical content-address hashes). This is the
    /// reproducibility property content-addressing needs, and the equivalence a pipelined variant
    /// would have to preserve; factory backfill runs sequentially, so this is the guarantee that
    /// matters (the filter-version pipeline is deferred to the step-5 flip per the RFC risk note).
    #[tokio::test]
    async fn factory_backfill_is_byte_identical_across_runs() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;

        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_a = "0x2222222222222222222222222222222222222222";
        let pool_b = "0x3333333333333333333333333333333333333333";
        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
                ).unwrap(),
                events: Vec::new(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };
        let config: Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        let created = |block, li, pool: &str| Log {
            address: factory_addr.into(),
            topics: vec![topic0("factory__pool_created")],
            data: format!("0x{:0>64}", pool.trim_start_matches("0x")),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xt".into(),
            log_index: li,
        };
        let swap = |block, li, pool: &str, amt: u64| Log {
            address: pool.into(),
            topics: vec![topic0("pool__swap")],
            data: format!("0x{amt:064x}"),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xt".into(),
            log_index: li,
        };
        // Two pools, interleaved swaps across several blocks - a non-trivial discovered set.
        let logs = vec![
            created(10, 0, pool_a),
            swap(11, 0, pool_a, 100),
            created(12, 0, pool_b),
            swap(13, 0, pool_b, 200),
            swap(13, 1, pool_a, 150),
            swap(14, 0, pool_b, 250),
        ];

        // The nest's real topic0s, which is what `build_nest` hands this function in production. It
        // used to be `&[]`, and the topic0-flip half of this test only passed because an empty
        // address *and* topic filter is "every log on the chain" - so the flipped fetch was answered
        // by the mock returning everything, rather than by the filter it is supposed to be flipping
        // to. The filter is unrepresentable now (#432), which turned that into a failure: a fixture
        // that passed without exercising what it names.
        let nest_topic0s = [topic0("factory__pool_created"), topic0("pool__swap")];

        async fn seal_sig(
            logs: Vec<crate::rpc::Log>,
            reg: &crate::registry::DecodeRegistry,
            fs: &FactorySet,
            topic0s: &[String],
            force_topic0: bool,
        ) -> (tempfile::TempDir, Vec<String>) {
            let source = FilteringSource { logs };
            let dir = tempfile::tempdir().unwrap();
            let mut children = ChildRegistry::new();
            backfill_direct_factory(
                &source,
                reg,
                fs,
                &mut children,
                dir.path(),
                topic0s,
                &[],
                None,
                0,
                10,
                20,
                100,
                force_topic0,
                |_| Ok(()),
                |_, _| {},
            )
            .await
            .unwrap();
            let m = crate::seal::load_manifest(dir.path()).unwrap();
            let mut sig: Vec<String> = m
                .tables
                .iter()
                .flat_map(|(t, segs)| segs.iter().map(move |s| format!("{t}:{}", s.hash)))
                .collect();
            sig.sort();
            (dir, sig)
        }

        // Address-list mode is reproducible; and the RFC-0009 §4 topic0-flip produces byte-identical
        // segments (the flip changes only the fetch strategy, never the output).
        let (_d1, sig1) = seal_sig(logs.clone(), &reg, &fs, &nest_topic0s, false).await;
        let (_d2, sig2) = seal_sig(logs.clone(), &reg, &fs, &nest_topic0s, false).await;
        let (_d3, sig3) = seal_sig(logs.clone(), &reg, &fs, &nest_topic0s, true).await;
        assert!(!sig1.is_empty(), "something was sealed");
        assert_eq!(
            sig1, sig3,
            "topic0-flip mode seals byte-identical segments to address-list mode"
        );
        assert_eq!(
            sig1, sig2,
            "identical range + history → byte-identical sealed segments"
        );
    }

    /// RFC-0009 step 2 gate: a child created and active in the *same* window is decoded - the
    /// factory's `PoolCreated` (log 0) discovers the pool, so the pool's `Swap` (log 1) routes to the
    /// template decoder in one in-order pass, no extra RPC. Verifies both rows and the child registry.
    #[test]
    fn factory_same_block_discovery_and_child_decode() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;

        let factory_abi = serde_json::from_str(
            r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
        )
        .unwrap();
        let pool_abi = serde_json::from_str(
            r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_addr = "0x2222222222222222222222222222222222222222";

        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: factory_abi,
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: pool_abi,
                events: Vec::new(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };

        let config: Config = toml::from_str(
            r#"
[nest]
name = "t"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc"]
[[contracts]]
alias = "factory"
address = "0x1111111111111111111111111111111111111111"
abi = "abis/f.json"
[[templates]]
name = "pool"
abi = "abis/p.json"
[[factories]]
watch = "factory"
event = "PoolCreated"
child_param = "pool"
template = "pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();

        // PoolCreated(pool) at log 0, then the pool's Swap(500) at log 1 - same block.
        let logs = vec![
            Log {
                address: factory_addr.into(),
                topics: vec![topic0("factory__pool_created")],
                data: format!("0x{:0>64}", pool_addr.trim_start_matches("0x")),
                block_number: 100,
                block_hash: "0xbh".into(),
                tx_hash: "0xt1".into(),
                log_index: 0,
            },
            Log {
                address: pool_addr.into(),
                topics: vec![topic0("pool__swap")],
                data: format!("0x{:064x}", 500u64),
                block_number: 100,
                block_hash: "0xbh".into(),
                tx_hash: "0xt2".into(),
                log_index: 1,
            },
        ];

        let mut children = ChildRegistry::new();
        let ts = std::collections::HashMap::from([(100u64, 1_700_000_000u64)]);
        let rows = decode_window(&reg, Some(&fs), &mut children, &logs, &ts);

        assert_eq!(
            rows.len(),
            2,
            "both the factory event and the child event decoded"
        );
        assert_eq!(rows[0].table, "factory__pool_created");
        assert_eq!(
            rows[1].table, "pool__swap",
            "same-block child activity routed to the template"
        );
        assert_eq!(
            rows[1].address, pool_addr,
            "child row carries the child address"
        );
        assert!(children.contains(pool_addr), "the pool was discovered");
        assert_eq!(children.template_of(pool_addr), Some("pool"));
        // The child registry rolls the pool back on a reorg to before its creation block.
        assert_eq!(children.clone().rollback_to(99), 1);
    }

    #[test]
    fn cold_start_origin_policy() {
        // No --backfill + vendored start_block → full history from deployment (clamped to tip).
        assert_eq!(
            cold_start_block(Some(42_449_585), None, 484_000_000),
            42_449_585
        );
        assert_eq!(cold_start_block(Some(999), None, 500), 500); // clamp to tip
                                                                 // Explicit --backfill always wins - recent-history mode, even with a start_block present.
        assert_eq!(
            cold_start_block(Some(42_449_585), Some(200), 484_000_000),
            483_999_800
        );
        assert_eq!(cold_start_block(None, Some(5_000), 1_000_000), 995_000);
        assert_eq!(cold_start_block(None, Some(5_000), 100), 0); // no underflow
                                                                 // Neither → a default recent window.
        assert_eq!(cold_start_block(None, None, 1_000_000), 995_000);
    }

    #[test]
    fn window_override_policy() {
        // No override → the chain default window.
        assert_eq!(effective_window(None, 2_000), 2_000);
        // A positive override wins (sparse-contract long backfill).
        assert_eq!(effective_window(Some(50_000), 2_000), 50_000);
        // A zero override is ignored - a zero-block window can't make progress.
        assert_eq!(effective_window(Some(0), 2_000), 2_000);
    }

    #[test]
    fn backfill_resumes_from_the_sealed_watermark() {
        // No watermark → fresh start from origin.
        assert_eq!(resume_from_watermark(None, 100), 100);
        // A watermark → resume one past the last durably-sealed block (no re-fetch of sealed ranges).
        assert_eq!(resume_from_watermark(Some(150), 100), 151);
        // A watermark below origin still resumes from the watermark (keeps the partial work).
        assert_eq!(resume_from_watermark(Some(40), 100), 41);
        // No overflow at the ceiling.
        assert_eq!(resume_from_watermark(Some(u64::MAX), 100), u64::MAX);
    }

    #[test]
    fn single_endpoint_backfill_is_capped_to_sequential() {
        // One endpoint → forced sequential regardless of the requested concurrency (deadlock guard).
        assert_eq!(safe_backfill_concurrency(1, 8), 1);
        assert_eq!(safe_backfill_concurrency(0, 8), 1);
        assert_eq!(safe_backfill_concurrency(1, 1), 1);
        // Two or more endpoints → the requested concurrency is honored.
        assert_eq!(safe_backfill_concurrency(3, 8), 8);
        assert_eq!(safe_backfill_concurrency(2, 4), 4);
    }

    #[test]
    fn depth_finality_seals_behind_the_tip() {
        assert_eq!(seal_ceiling(Finality::Depth(64), 1000, None), 936);
        // Never underflow near genesis.
        assert_eq!(seal_ceiling(Finality::Depth(64), 10, None), 0);
    }

    #[test]
    fn finalized_tag_is_used_when_present_else_falls_back() {
        let f = Finality::FinalizedTag {
            fallback_depth: 1800,
        };
        // Tag present: seal up to it (clamped to tip).
        assert_eq!(seal_ceiling(f, 10_000, Some(8_500)), 8_500);
        assert_eq!(seal_ceiling(f, 10_000, Some(10_050)), 10_000);
        // Tag absent (endpoint doesn't serve it): fixed-depth fallback.
        assert_eq!(seal_ceiling(f, 10_000, None), 8_200);
    }

    #[test]
    fn addr_in_is_case_insensitive() {
        let set = vec!["0xabc123".to_string(), "0xdef456".to_string()];
        assert!(addr_in(&set, "0xABC123")); // checksummed provider address matches lowercase filter
        assert!(addr_in(&set, "0xdef456"));
        assert!(!addr_in(&set, "0x999999"));
        assert!(!addr_in(&[], "0xabc123")); // a topic0-only (factory) nest owns nothing by address
    }

    #[test]
    fn log_owned_static_by_address_factory_by_topic0() {
        let transfer = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let mut child_log = transfer_log(20, 0);
        child_log.address = "0xchildaddress0000000000000000000000000000".into(); // some runtime child

        // Static nest: owns by address. It watches a fixed set; the child's address isn't in it.
        let static_addrs = vec!["0xAAA0000000000000000000000000000000000000".to_string()];
        let static_topics = vec![transfer.to_string()];
        assert!(!log_owned(&static_addrs, &static_topics, &child_log)); // wrong address → not owned
        let mut own_log = transfer_log(20, 1);
        own_log.address = "0xaaa0000000000000000000000000000000000000".into(); // checksum differs
        assert!(log_owned(&static_addrs, &static_topics, &own_log)); // address match (case-insensitive)

        // Factory nest: empty addresses → owns by topic0. It catches the child regardless of address,
        // because the child's event carries a template topic0 in the nest's set.
        let factory_addrs: Vec<String> = Vec::new();
        let factory_topics = vec![transfer.to_string()];
        assert!(log_owned(&factory_addrs, &factory_topics, &child_log)); // topic0 match → owned
        let mut other_topic = transfer_log(20, 2);
        other_topic.topics[0] = "0xdeadbeef".into();
        assert!(!log_owned(&factory_addrs, &factory_topics, &other_topic)); // topic not watched → not owned
    }

    #[test]
    fn union_filter_goes_topic0_only_when_a_factory_is_present() {
        let transfer = "0xddf252...".to_string();
        let created = "0xpaircreated...".to_string();
        // A static nest (fixed addresses) co-mounted with a factory nest (empty addresses).
        let static_addrs = vec!["0xAAA".to_string()];
        let factory_addrs: Vec<String> = Vec::new();
        let (addrs, topics) = union_filter(
            [
                (static_addrs.as_slice(), [transfer.clone()].as_slice()),
                (factory_addrs.as_slice(), [created.clone()].as_slice()),
            ]
            .into_iter(),
        );
        // The factory forces the whole fetch topic0-only: no address filter, both topics unioned.
        assert!(
            addrs.is_empty(),
            "a factory co-tenant must drop the address filter"
        );
        assert_eq!(topics, vec![transfer, created]);
    }

    /// Build a minimal static ERC20 `NestIngest` on disk through the real `build_nest` path.
    /// A nest with one `[[factories]]` rule, so `FactorySet::build` is non-empty and `build_nest`
    /// marks it a factory nest - the state that forces the union topic0-only.
    async fn build_factory_test_nest(dir: &std::path::Path) -> NestIngest {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"f\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"fac\"\naddress = \"0x0000000000000000000000000000000000000022\"\n\
             abi = \"abis/fac.json\"\n\n\
             [[templates]]\nname = \"child\"\nabi = \"abis/child.json\"\n\n\
             [[factories]]\nwatch = \"fac\"\nevent = \"ChildCreated\"\nchild_param = \"child\"\n\
             template = \"child\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/fac.json"),
            r#"[{"type":"event","name":"ChildCreated","inputs":[{"name":"child","type":"address","indexed":true}],"anonymous":false}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/child.json"),
            r#"[{"type":"event","name":"Ping","inputs":[],"anonymous":false}]"#,
        )
        .unwrap();
        let config = Config::load(dir).unwrap();
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, _worker, _w) = build_nest(
            &source,
            dir.to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        assert!(nest.factory.is_some(), "fixture must be a factory nest");
        nest
    }

    /// A contract-free `[extract] blocks = true` nest - OBIB case 3 - through the real `build_nest`
    /// path. Buildable only since #445; before that this helper could not have existed, which is why
    /// the shape is absent from every fixture that predates it.
    async fn build_contract_free_test_nest(dir: &std::path::Path) -> NestIngest {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"b\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [extract]\nblocks = true\n",
        )
        .unwrap();
        let config = Config::load(dir).unwrap();
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, worker, _w) = build_nest(
            &source,
            dir.to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a contract-free blocks nest must build (#445)");
        if let Some(w) = worker {
            w.abort();
        }
        nest
    }

    /// #510: a fully dead RPC pool at cold start must not kill the process. Before this fix, `prepare`'s
    /// cold-start tip lookup was a bare `?`: the very first connectivity error propagated straight out
    /// of `index_loop` as a fatal `Result::Err`, and `run`'s `tokio::select!` treated that as the whole
    /// process failing - moments after `serve` had already logged "API live". A pool that is merely
    /// *briefly* unreachable (this fixture, one failure then recovery) must instead retry and proceed,
    /// exactly as the steady-state tip loop already tolerates the same failure once past cold start.
    #[tokio::test]
    async fn prepare_retries_a_cold_start_tip_lookup_instead_of_dying() {
        use crate::rpc::Log;
        use std::sync::atomic::{AtomicU64, Ordering};

        struct FlakyTipSource {
            fails_left: AtomicU64,
        }
        #[async_trait::async_trait]
        impl Source for FlakyTipSource {
            async fn tip(&self) -> Result<u64> {
                if self.fails_left.fetch_sub(1, Ordering::Relaxed) > 0 {
                    anyhow::bail!("connection refused (pool dead)");
                }
                Ok(1_000)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                _from: u64,
                _to: u64,
            ) -> Result<Vec<Log>> {
                Ok(Vec::new())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut nest = build_contract_free_test_nest(dir.path()).await;
        let source = FlakyTipSource {
            fails_left: AtomicU64::new(1),
        };

        let next = prepare_retrying(&mut nest, &source, None, false, 1, 5)
            .await
            .expect("a pool that eventually answers must not fail prepare - it should retry");
        // DEFAULT_BACKFILL is 5_000; tip is 1_000, so cold_start_block saturates to 0.
        assert_eq!(
            next, 0,
            "cold start begins at block 0 when tip < DEFAULT_BACKFILL"
        );
    }

    async fn build_test_nest(dir: &std::path::Path, addr: &str) -> NestIngest {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            format!(
                "[nest]\nname = \"n\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"tok\"\naddress = \"{addr}\"\nabi = \"abis/tok.json\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#,
        )
        .unwrap();
        let config = Config::load(dir).unwrap();
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, worker, _w) = build_nest(
            &source,
            dir.to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        if let Some(w) = worker {
            w.abort();
        }
        nest
    }

    /// #445: a contract-free `[extract] blocks = true` nest - OBIB case 3, RFC-0036 §4.2 - is a
    /// supported shape, and `build_nest` is the only way any nest is built. It used to refuse this
    /// one with `nest has no contracts`: a message about contracts the operator deliberately did not
    /// declare, raised because `AppState.address` was a `String` and so had to come from somewhere.
    ///
    /// Nothing caught it because case 3 had only ever run through the bench harness, which builds a
    /// `DecodeRegistry` directly and never calls `build_nest`. So the config key parsed, the bench
    /// path produced rows, and every operator-facing path - solo `dev` and the runtime alike - refused
    /// to start.
    #[tokio::test]
    async fn a_contract_free_blocks_nest_builds_and_names_no_address() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"b\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [extract]\nblocks = true\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert!(
            config.contracts.is_empty(),
            "fixture must declare no contracts"
        );
        assert!(config.extract.blocks, "fixture must be a blocks nest");
        // `blocks` is deliberately outside `Extract::enabled()` - it is sourceable from ordinary RPC -
        // so this must not hit the node-gated startup refusal either.
        assert!(!config.extract.enabled(), "blocks must not be node-gated");

        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (_nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a contract-free blocks nest must build");
        assert_eq!(
            state.address, None,
            "a nest with no contracts names no address"
        );
        if let Some(w) = worker {
            w.abort();
        }
    }

    /// Seed a nest's hot store with one row per block and set `LAST_BLOCK` to the max.
    fn seed_blocks(nest: &NestIngest, blocks: &[u64]) {
        for &b in blocks {
            let key = Store::entity_key(b, 0);
            nest.store
                .put_entity(&key, &format!(r#"{{"table":"t","block_number":{b}}}"#))
                .unwrap();
        }
        let last = *blocks.iter().max().unwrap();
        nest.store
            .set_meta(LAST_BLOCK_KEY, &last.to_string())
            .unwrap();
    }

    /// RFC-0012 slice 3: one shared reorg fans out to every nest. A caught-up nest rolls back to the
    /// fork; a still-backfilling nest below the fork is spared and - crucially - its cursor is NOT
    /// bumped up to the ancestor (that would claim blocks it never indexed).
    #[tokio::test]
    async fn runtime_reorg_fans_out_and_spares_behind_nests() {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let mut caught_up =
            build_test_nest(da.path(), "0x0000000000000000000000000000000000000001").await;
        let mut behind =
            build_test_nest(db.path(), "0x0000000000000000000000000000000000000002").await;

        // caught_up is at the tip (block 100); behind is still backfilling (block 30, below the fork).
        seed_blocks(&caught_up, &[10, 20, 30, 40, 50, 60, 80, 100]);
        seed_blocks(&behind, &[10, 20, 30]);

        // One shared reorg to ancestor 50, fanned to both nests (as `runtime_index_loop` does).
        caught_up.rollback_reorg(50).unwrap();
        behind.rollback_reorg(50).unwrap();

        // Caught-up nest: rolled back to 50 - nothing above survives, cursor at 50.
        assert!(caught_up
            .store
            .entities_in_range(51, 1_000)
            .unwrap()
            .is_empty());
        assert_eq!(caught_up.store.entities_in_range(10, 50).unwrap().len(), 5); // 10,20,30,40,50
        assert_eq!(
            caught_up.store.get_meta(LAST_BLOCK_KEY).unwrap().as_deref(),
            Some("50")
        );

        // Behind nest: below the fork → untouched; cursor stays at 30 (NOT bumped to 50).
        assert_eq!(behind.store.entities_in_range(10, 1_000).unwrap().len(), 3);
        assert_eq!(
            behind.store.get_meta(LAST_BLOCK_KEY).unwrap().as_deref(),
            Some("30")
        );
    }

    /// A supervisor over `n` nests named a, b, c… with a throwaway health surface.
    fn test_supervisor(n: usize) -> Supervisor {
        Supervisor::new(
            (0..n).map(|i| format!("nest{i}")).collect(),
            Arc::new(crate::health::RuntimeHealth::new()),
            false,
        )
    }

    /// RFC-0027 §6: an operator's unmount is **not** a fault, and the distinction is load-bearing.
    ///
    /// A retired nest leaves the working set like a quarantined one, but it must never be re-admitted,
    /// never appear in the cursor's death notice, and - the one that would actually hurt - never make
    /// the cursor look terminally dead. Conflating the two would mean unmounting your last nest exits
    /// the process you were about to mount the replacement into.
    #[test]
    fn a_retired_nest_leaves_the_working_set_without_looking_like_a_fault() {
        let mut sup = test_supervisor(2);
        sup.retire(0);

        assert_eq!(
            sup.live(),
            vec![1],
            "a retired nest stops driving the cursor"
        );
        assert!(
            !sup.all_terminal(),
            "one retirement and one live nest is not a dead cursor"
        );
        assert!(
            sup.reasons().is_empty(),
            "a retirement is not a failure reason"
        );

        // Re-admission must not resurrect it: the operator asked for it to be gone.
        sup.readmit_due(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        assert_eq!(sup.live(), vec![1], "a retired nest is never re-admitted");

        // Retiring the last one: no work left, but nothing broken.
        sup.retire(1);
        assert!(sup.all_retired());
        assert!(
            !sup.all_terminal(),
            "an all-retired cursor must not report itself terminally dead - that would exit the runtime"
        );
    }

    /// The mixed case, which is the one that decides whether an operator gets paged: a cursor holding
    /// one retired nest and one terminally quarantined nest **is** dead, because something did fail.
    #[test]
    fn a_retirement_alongside_a_real_fault_still_reports_the_cursor_dead() {
        let mut sup = test_supervisor(2);
        sup.retire(0);
        sup.quarantine(1, &anyhow::anyhow!(TerminalFault("boom".into())))
            .unwrap();
        assert!(
            sup.all_terminal(),
            "a retired nest must not mask a sibling's terminal fault"
        );
        assert!(!sup.all_retired());
        assert_eq!(sup.reasons().len(), 1, "only the real fault is a reason");
    }

    /// Commands are applied at the window boundary, and one naming a nest this cursor does not host is
    /// dropped rather than treated as an error - with one cursor per chain, a runtime-level command can
    /// legitimately reach the wrong cursor.
    #[test]
    fn lifecycle_commands_retire_the_named_nest_and_ignore_strangers() {
        let mut sup = test_supervisor(2);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle = Some(rx);
        // Slots matching the supervisor's nests. Already empty, which is all this test needs: it is
        // about the retirement bookkeeping, not about what dropping an ingest state releases. The
        // lengths must still line up - `drain_lifecycle` indexes by supervisor index deliberately, so
        // a mismatch is a bug rather than something to tolerate.
        let mut nests: Vec<Option<NestIngest>> = vec![None, None];
        let mut nexts: Vec<u64> = vec![0, 0];

        tx.send(CursorCommand::unmount("nest0")).unwrap();
        tx.send(CursorCommand::unmount("not-on-this-cursor"))
            .unwrap();
        drain_lifecycle(&mut lifecycle, &mut sup, &mut nests, &mut nexts);

        assert_eq!(sup.live(), vec![1], "the named nest retired");
        assert!(matches!(sup.states[0], NestState::Retired));

        // Idempotent: unmounting twice is not an error, and does not double-count anything.
        tx.send(CursorCommand::unmount("nest0")).unwrap();
        drain_lifecycle(&mut lifecycle, &mut sup, &mut nests, &mut nexts);
        assert_eq!(sup.live(), vec![1]);

        // No channel at all (today's `spawn_runtime`) is simply a no-op.
        let mut none = None;
        drain_lifecycle(&mut none, &mut sup, &mut nests, &mut nexts);
        assert_eq!(sup.live(), vec![1]);
    }

    /// RFC-0027 §3: admitting a nest grows the working set and the parallel arrays **together**.
    ///
    /// The index alignment between `names`/`states`/`attempts`/`prepared` and the loop's
    /// `nests`/`nexts` is what `live_nest`'s `expect` rests on, so a mount that grew one and not the
    /// others would turn a lifecycle operation into a panic - or worse, into a nest indexing under
    /// another nest's identity.
    #[test]
    fn admitting_a_nest_extends_the_working_set_in_step() {
        let mut sup = test_supervisor(1);
        let i = sup.admit("late-arrival");

        assert_eq!(i, 1, "admitted at the next index");
        assert_eq!(sup.live(), vec![0, 1], "it drives the cursor immediately");
        assert_eq!(sup.names.len(), 2);
        assert_eq!(sup.states.len(), 2);
        assert_eq!(sup.attempts.len(), 2);
        assert_eq!(sup.prepared.len(), 2);
        assert!(
            sup.prepared[i],
            "a mounted nest arrives already prepared - marking it unprepared would make the cursor \
             treat its `next` as unknown, i.e. genesis, and drag every co-tenant back with it"
        );
        assert_eq!(sup.index_of("late-arrival"), Some(1));
    }

    /// A mount is applied at a window boundary like any other command, keeps the arrays in step, and
    /// refuses to mount over a name already on the cursor - that case is an *upgrade* (RFC-0020), and
    /// letting the two silently overlap would leave two nests writing one store.
    #[test]
    fn mounting_over_an_existing_name_is_refused() {
        let mut sup = test_supervisor(2);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut lifecycle = Some(rx);
        let mut nests: Vec<Option<NestIngest>> = vec![None, None];
        let mut nexts: Vec<u64> = vec![10, 20];

        // `nest0` is already mounted, so this must be ignored rather than duplicating the entry.
        let (ack_tx, mut ack_rx) = tokio::sync::oneshot::channel();
        tx.send(CursorCommand::Unmount {
            name: "nest0".into(),
            ack: Some(ack_tx),
        })
        .unwrap();
        drain_lifecycle(&mut lifecycle, &mut sup, &mut nests, &mut nexts);
        assert!(ack_rx.try_recv().is_ok(), "the unmount was acknowledged");

        // Arrays stayed in step through the retirement.
        assert_eq!(nests.len(), 2);
        assert_eq!(nexts.len(), 2);
        assert_eq!(sup.names.len(), 2);
        assert_eq!(sup.live(), vec![1]);
    }

    /// RFC-0026 §3.1 - the trap the whole design turns on. A quarantined nest must leave the working
    /// set, not merely be skipped: the shared cursor advances from the *min* of the live cursors, so a
    /// quarantined nest left in that min pins the cursor at its dead position and stalls every healthy
    /// sibling - while the runtime still reports itself alive. Strictly worse than the crash it replaces.
    #[test]
    fn a_quarantined_nest_stops_pinning_the_shared_cursor() {
        let nexts = [30u64, 100, 120];
        let mut sup = test_supervisor(3);

        // All live: the laggard at 30 rightly holds the cursor - no nest may skip a block.
        let live = sup.live();
        assert_eq!(live, vec![0, 1, 2]);
        assert_eq!(live.iter().map(|&i| nexts[i]).min().unwrap(), 30);

        // Quarantine the laggard: the cursor jumps to the slowest *live* nest and indexing resumes.
        let boom = anyhow::anyhow!("store write failed");
        sup.quarantine(0, &boom).unwrap();
        let live = sup.live();
        assert_eq!(live, vec![1, 2]);
        assert_eq!(live.iter().map(|&i| nexts[i]).min().unwrap(), 100);
        // …and the reorg reference is still the most caught-up live nest.
        assert_eq!(live.iter().map(|&i| nexts[i]).max().unwrap(), 120);

        // Re-admission restores it: the nest rejoins *behind*, pulling the cursor back to re-fetch the
        // range it missed. Siblings ahead skip those windows via the loop's `nexts[i] > to` guard.
        sup.readmit_due(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        let live = sup.live();
        assert_eq!(live, vec![0, 1, 2]);
        assert_eq!(live.iter().map(|&i| nexts[i]).min().unwrap(), 30);
    }

    /// RFC-0026 §3 - terminal faults are quarantined until restart, never retried: they re-fail
    /// identically by construction. The classification must survive `anyhow` context wrapping, since
    /// that is how it reaches the loop.
    #[test]
    fn terminal_faults_are_recognised_through_context_and_never_scheduled_for_retry() {
        let terminal: anyhow::Error = anyhow::anyhow!(TerminalFault("finality violation".into()))
            .context("processing window");
        let transient: anyhow::Error =
            anyhow::anyhow!("connection reset").context("processing window");
        assert!(is_terminal(&terminal));
        assert!(!is_terminal(&transient));

        let mut sup = test_supervisor(2);
        sup.quarantine(0, &terminal).unwrap();
        sup.quarantine(1, &transient).unwrap();

        // Terminal → no retry deadline at all; a far-future re-admission sweep leaves it quarantined.
        assert!(matches!(
            sup.states[0],
            NestState::Quarantined { retry_at: None, .. }
        ));
        assert!(matches!(
            sup.states[1],
            NestState::Quarantined {
                retry_at: Some(_),
                ..
            }
        ));
        sup.readmit_due(std::time::Instant::now() + std::time::Duration::from_secs(86_400));
        assert_eq!(sup.live(), vec![1]);
        // …and the health surface agrees, which is what an operator actually looks at.
        assert_eq!(sup.health.json_for("nest0").0, "quarantined");
        assert_eq!(sup.health.json_for("nest1").0, "indexing");
    }

    /// RFC-0026 §3.1, the nastiest corner: a nest quarantined *during* `prepare` never established a
    /// cursor, so its `nexts` entry is 0 - meaning "unknown", not "start at genesis". If re-admission
    /// let it rejoin on that 0 it would become the new minimum and drag the whole shared cursor back to
    /// block 0, re-indexing every healthy co-tenant from scratch. The loop guards this with `prepared`;
    /// this test pins the invariant that guard exists to protect.
    #[test]
    fn an_unprepared_nest_never_drags_the_shared_cursor_to_genesis() {
        let nexts = [0u64, 5_000_000];
        let mut sup = test_supervisor(2);
        sup.prepared = vec![false, true];

        // Nest 0 failed `prepare` and is quarantined; nest 1 is indexing near the tip.
        sup.quarantine(0, &anyhow::anyhow!("backfill RPC died"))
            .unwrap();
        assert_eq!(sup.live(), vec![1]);

        // Its backoff elapses and it is re-admitted - the moment the bug would bite.
        sup.readmit_due(std::time::Instant::now() + std::time::Duration::from_secs(3600));
        assert_eq!(sup.live(), vec![0, 1]);

        // The loop must re-`prepare` it before deriving the cursor. Any live-but-unprepared nest at
        // this point is the bug: the cursor would be 0 rather than the healthy nest's 5,000,000.
        let unprepared_and_live: Vec<usize> = sup
            .live()
            .into_iter()
            .filter(|&i| !sup.prepared[i])
            .collect();
        assert_eq!(
            unprepared_and_live,
            vec![0],
            "the guard must have something to catch here"
        );
        let cursor_if_unguarded = sup.live().iter().map(|&i| nexts[i]).min().unwrap();
        assert_eq!(cursor_if_unguarded, 0, "this is the damage being prevented");
        // With the guard, only prepared nests contribute a cursor.
        let cursor_guarded = sup
            .live()
            .iter()
            .filter(|&&i| sup.prepared[i])
            .map(|&i| nexts[i])
            .min()
            .unwrap();
        assert_eq!(cursor_guarded, 5_000_000);
    }

    /// RFC-0026 §4 - backoff doubles and is capped, so a recovered endpoint is picked up within
    /// minutes rather than hours.
    #[test]
    fn quarantine_backoff_doubles_then_caps() {
        assert_eq!(quarantine_backoff_secs(0), 5);
        assert_eq!(quarantine_backoff_secs(1), 10);
        assert_eq!(quarantine_backoff_secs(2), 20);
        // Monotonic, and pinned at the ceiling however many attempts pile up.
        let mut prev = 0;
        for a in 0..64 {
            let w = quarantine_backoff_secs(a);
            assert!(w >= prev, "backoff went backwards at attempt {a}");
            assert!(w <= QUARANTINE_BACKOFF_MAX_SECS);
            prev = w;
        }
        assert_eq!(quarantine_backoff_secs(63), QUARANTINE_BACKOFF_MAX_SECS);
    }

    /// COR-5, the invariant that makes the fix safe: **whenever the union goes topic0-only, there is
    /// at least one nest to blame for it.**
    ///
    /// The cap fallback quarantines [`topic0_only_culprits`] instead of ending the cursor. That is
    /// only sound if the set is non-empty exactly when the address filter is empty - otherwise a cap
    /// breach on an unowned fetch would be silently ignored and the loop would spin. The two
    /// functions encode the same rule from opposite ends, so this pins them together: a change to
    /// `union_filter`'s "any factory → clear addresses" that is not mirrored in the culprit rule
    /// fails here rather than in production.
    #[tokio::test]
    async fn a_topic0_only_union_always_has_someone_to_blame() {
        let ds = tempfile::tempdir().unwrap();
        let df = tempfile::tempdir().unwrap();
        let stat = build_test_nest(ds.path(), "0x0000000000000000000000000000000000000011").await;
        let fact = build_factory_test_nest(df.path()).await;

        // A static nest alone: the union keeps its addresses, and nobody is to blame for a cap
        // breach - so the caller must still fail loudly, exactly as before this fix.
        let (addrs, _) =
            union_filter([(stat.addresses.as_slice(), stat.topic0s.as_slice())].into_iter());
        assert!(
            !addrs.is_empty(),
            "a static-only union must carry addresses"
        );
        assert!(
            topic0_only_culprits([(0usize, &stat)].into_iter()).is_empty(),
            "no factory nest → nobody to fault → the loud error is preserved"
        );

        // Add a factory nest and the union goes wide. Now there is a culprit, and it is the factory
        // nest - never the static one, whose addresses cannot have widened anything.
        let (addrs, _) = union_filter(
            [
                (stat.addresses.as_slice(), stat.topic0s.as_slice()),
                (fact.addresses.as_slice(), fact.topic0s.as_slice()),
            ]
            .into_iter(),
        );
        assert!(
            addrs.is_empty(),
            "a live factory nest must force the union topic0-only"
        );
        assert_eq!(
            topic0_only_culprits([(0usize, &stat), (1usize, &fact)].into_iter()),
            vec![1],
            "the factory nest is answerable; the static co-tenant is not"
        );

        // #445 made a third shape buildable, and it is the one that breaks the old reading of the
        // rule: a contract-free `[extract] blocks = true` nest has an empty address list *and* an
        // empty topic list. Empty-addresses therefore no longer implies factory-ness, and the two
        // functions read that signal from opposite ends - `union_filter` off the addresses,
        // `topic0_only_culprits` off `factory.is_some()`.
        //
        // Left alone, such a nest would clear a co-tenant's address filter (widening a fetch on
        // behalf of a nest that wants no logs at all), and then no nest would be answerable for the
        // width - so a single over-cap block would end the cursor and every nest riding it, which is
        // the RFC-0026 violation this whole mechanism exists to prevent. It wants no logs, so it
        // takes no part in the union.
        let db = tempfile::tempdir().unwrap();
        let blocks = build_contract_free_test_nest(db.path()).await;
        assert!(
            blocks.addresses.is_empty() && blocks.topic0s.is_empty(),
            "the fixture must be the no-filter shape, or this proves nothing"
        );
        let (addrs, _) = union_filter(
            [
                (stat.addresses.as_slice(), stat.topic0s.as_slice()),
                (blocks.addresses.as_slice(), blocks.topic0s.as_slice()),
            ]
            .into_iter(),
        );
        assert!(
            !addrs.is_empty(),
            "a contract-free blocks nest must not clear its co-tenant's address filter"
        );
        assert!(
            topic0_only_culprits([(0usize, &stat), (1usize, &blocks)].into_iter()).is_empty(),
            "and with the union still address-filtered, nobody is to blame - which is consistent \
             only because the union stayed narrow. The two ends agree again."
        );
    }

    /// **#863.** A backfill that stops advancing must fail by name rather than run forever.
    ///
    /// The recorded failure this guards is #672's: a request that fails whole and is retried at the
    /// same width indefinitely. Nothing about that loop looks wrong from outside - well-formed
    /// requests, orderly retry logs - which is why it went unnoticed and why two mutants sit in the
    /// baseline costing the nightly sweep ~23 minutes each in timeouts.
    #[test]
    fn a_backfill_that_stops_advancing_fails_by_name() {
        let mut n = 0usize;
        for i in 1..NO_PROGRESS_LIMIT {
            assert!(
                no_progress_tick(&mut n, 100, 200, false, "narrowing").is_ok(),
                "attempt {i} must not fault - a narrowing descent legitimately fails on the way down"
            );
        }
        let err = no_progress_tick(&mut n, 100, 200, false, "narrowing")
            .expect_err("attempt 64 must fault: nothing is advancing and nothing is narrowing");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no progress") && msg.contains("100..=200"),
            "the operator needs the range named: {msg}"
        );
        assert!(
            msg.contains("nuthatch doctor") || msg.contains("--window"),
            "a failure that names no recovery path is the shape #913 is about: {msg}"
        );
    }

    /// The limit must clear a real narrowing descent with room to spare, or the guard converts a slow
    /// provider into a fault. Halving a 1,000,000-block ceiling to a single block is ~20 attempts.
    #[test]
    fn a_full_narrowing_descent_fits_inside_the_limit() {
        let descent = (1_000_000f64).log2().ceil() as usize;
        assert!(
            descent < NO_PROGRESS_LIMIT,
            "a legitimate descent takes {descent} attempts and the limit is {NO_PROGRESS_LIMIT}"
        );
        let mut n = 0usize;
        for _ in 0..descent {
            no_progress_tick(&mut n, 0, 1_000_000, false, "narrowing")
                .expect("a descent must never fault");
        }
        // And the window committing resets it, which is what makes the guard about *stalls* rather
        // than about slowness.
        n = 0;
        assert!(no_progress_tick(&mut n, 0, 1, false, "narrowing").is_ok());
    }

    /// At tip the same condition must **not** fault. A tip loop has no end and the usual cause is
    /// somebody else's provider having an afternoon; faulting would turn a transient outage into a
    /// dead nest, which RFC-0026 reserves for last.
    #[test]
    fn at_tip_a_stall_warns_but_never_faults() {
        let mut n = 0usize;
        for _ in 0..(NO_PROGRESS_LIMIT * 4) {
            assert!(
                no_progress_tick(&mut n, 100, 200, true, "fetch failing").is_ok(),
                "a nest at tip must keep trying through a provider outage"
            );
        }
        assert_eq!(
            n,
            NO_PROGRESS_LIMIT * 4,
            "the counter must keep running so the warning repeats at a bounded rate rather than \
             firing once and never again"
        );
    }

    /// A pool-wide 429 error, shaped exactly as `escalate_pool_wide_rate_limit` shapes it.
    fn pool_wide_429() -> anyhow::Error {
        anyhow::Error::new(crate::rpc::ClassifiedError {
            class: crate::rpc::FailureClass::Narrowable {
                suggested: None,
                escalated_from_rate_limit: true,
            },
            detail: "every endpoint (2) rate-limited this request; treating it as too large: \
                     HTTP 429 Too Many Requests"
                .into(),
        })
    }

    /// A provider genuinely refusing the *result size*.
    fn real_cap() -> anyhow::Error {
        anyhow::Error::new(crate::rpc::ClassifiedError {
            class: crate::rpc::FailureClass::Narrowable {
                suggested: None,
                escalated_from_rate_limit: false,
            },
            detail: "query returned more than 10000 results".into(),
        })
    }

    /// **#916.** At the floor - one block, nothing left to narrow - a throttle and a size cap must
    /// part company. Anywhere above the floor they must not, or the fix has quietly repealed
    /// RFC-0028 §3d instead of bounding it.
    ///
    /// The third assertion is the one that stops this being a disable-the-feature patch: with range
    /// still available, a pool-wide 429 is *still* narrowable, exactly as the RFC intends.
    #[test]
    fn a_throttle_stops_being_narrowable_only_at_the_floor() {
        assert!(
            narrowing_can_help(&real_cap(), 100, 100),
            "a real size cap at a single block is still a cap - the block genuinely will not fit"
        );
        assert!(
            !narrowing_can_help(&pool_wide_429(), 100, 100),
            "a pool-wide 429 at a single block has nothing left to narrow: calling it a cap is the \
             false diagnosis that killed nuthatch-dips"
        );
        assert!(
            narrowing_can_help(&pool_wide_429(), 100, 200),
            "with range still to spend, RFC-0028 §3d stands: narrowing is also less load"
        );
        assert!(
            narrowing_can_help(&real_cap(), 100, 200),
            "and a real cap over a range is unchanged"
        );
    }

    /// The message an operator is left with. `use a provider with a higher/no cap` is not merely
    /// imprecise for a throttle, it names a **recovery path that does not exist**: buying a bigger
    /// provider does not stop a free endpoint rate-limiting you.
    #[tokio::test]
    async fn a_throttled_single_block_is_not_reported_as_a_cap() {
        struct Throttled;
        #[async_trait::async_trait]
        impl Source for Throttled {
            async fn tip(&self) -> Result<u64> {
                Ok(100)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _f: &crate::source::LogFilter,
                _from: u64,
                _to: u64,
            ) -> Result<Vec<crate::rpc::Log>> {
                Err(pool_wide_429())
            }
            async fn block_timestamps(
                &self,
                b: &[u64],
            ) -> Result<std::collections::HashMap<u64, u64>> {
                Ok(b.iter().map(|&x| (x, x * 1000)).collect())
            }
        }
        let filter = crate::source::LogFilter::new(&[], &["0xdead".into()]).unwrap();
        let err = fetch_logs_splitting(&Throttled, &filter, 100, 100)
            .await
            .expect_err("a throttled fetch must still fail");
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("alone exceeds"),
            "a throttle must not be reported as a result cap: {msg}"
        );
        assert!(
            msg.contains("rate-limited"),
            "and the true cause must survive to the operator: {msg}"
        );
    }

    /// COR-5: the fault raised for an over-cap single block must be **terminal**.
    ///
    /// A retryable quarantine is re-admitted after a backoff, which re-issues the identical fetch
    /// against the identical block and fails identically - a spin at the backoff ceiling that buries
    /// the operator's real job (raise the provider cap, or narrow the topic0 set with an `events`
    /// allowlist) under a repeating log.
    #[test]
    fn an_over_cap_block_is_a_terminal_fault() {
        let e = anyhow::Error::new(TerminalFault(format!(
            "{}: query returned more than 10000 results",
            single_block_over_cap(19_000_000)
        )));
        assert!(is_terminal(&e), "must not be re-admitted on a backoff");
        let msg = format!("{e:#}");
        assert!(
            msg.contains("block 19000000 alone exceeds"),
            "the operator needs the block named: {msg}"
        );
    }

    /// An address-aware source that **refuses the topic0-only fetch** the way a capped provider does:
    /// an empty address filter (= "any address") returns the cap error, anything narrower is answered.
    /// That is the shape of the COR-5 failure - the fetch is not too big for the provider, the *filter*
    /// is - and it is what lets a test tell a real address-filtered fallback apart from a retry.
    struct CappedSource {
        logs: Vec<crate::rpc::Log>,
        /// Every `getLogs` call, as `(addresses, from, to)` - so a test can assert the fallback
        /// narrowed rather than reissued, and that the in-block fixpoint ran.
        calls: std::sync::Mutex<Vec<(Vec<String>, u64, u64)>>,
    }

    #[async_trait::async_trait]
    impl Source for CappedSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            let addrs = filter.addresses();
            self.calls.lock().unwrap().push((addrs.to_vec(), from, to));
            if addrs.is_empty() {
                anyhow::bail!("query returned more than 10000 results");
            }
            let allow: std::collections::HashSet<String> =
                addrs.iter().map(|a| a.to_ascii_lowercase()).collect();
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .filter(|l| allow.contains(&l.address.to_ascii_lowercase()))
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, b * 1000)).collect())
        }
    }

    /// The topic0 of a nest's table, from its own built registry (never a hand-copied hash).
    fn nest_topic0(nest: &NestIngest, table: &str) -> String {
        format!(
            "0x{}",
            hex::encode(
                nest.registry
                    .tables()
                    .iter()
                    .find(|d| d.table == table)
                    .unwrap_or_else(|| panic!("no table '{table}' in this nest's registry"))
                    .topic0
            )
        )
    }

    /// COR-5, the half that was missing: a factory nest whose topic0-only tip fetch breaks the
    /// provider's cap on a single block **recovers without an operator**.
    ///
    /// The block is refetched with `base ∪ discovered children` and the in-block discovery fixpoint,
    /// so the nest keeps ingesting *and* keeps discovering - a fallback that quietly stopped finding
    /// children would be worse than the loud quarantine it replaces, because it would look healthy.
    ///
    /// Against `main` this test fails on its first assertion: the factory nest is terminally
    /// quarantined and neither it nor its co-tenant indexes block 7 at all.
    #[tokio::test]
    async fn an_over_cap_factory_block_recovers_address_filtered() {
        let ds = tempfile::tempdir().unwrap();
        let df = tempfile::tempdir().unwrap();
        let stat = build_test_nest(ds.path(), "0x0000000000000000000000000000000000000011").await;
        let fact = build_factory_test_nest(df.path()).await;

        let token = "0x0000000000000000000000000000000000000011";
        let factory_addr = "0x0000000000000000000000000000000000000022";
        let child = "0x0000000000000000000000000000000000000033";
        let pad = |a: &str| format!("0x{:0>64}", a.trim_start_matches("0x"));

        // Block 7, the hot one. The child is created and pings *in the same block* - the case the tip
        // loop exists to catch, and the one a naive "just narrow the filter" fallback would miss: the
        // child is in nobody's address list until log 0 is decoded.
        let source = CappedSource {
            logs: vec![
                crate::rpc::Log {
                    address: factory_addr.into(),
                    topics: vec![nest_topic0(&fact, "fac__child_created"), pad(child)],
                    data: "0x".into(),
                    block_number: 7,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt1".into(),
                    log_index: 0,
                },
                crate::rpc::Log {
                    address: child.into(),
                    topics: vec![nest_topic0(&fact, "child__ping")],
                    data: "0x".into(),
                    block_number: 7,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt2".into(),
                    log_index: 1,
                },
                crate::rpc::Log {
                    address: token.into(),
                    topics: vec![
                        nest_topic0(&stat, "tok__transfer"),
                        pad("0x00000000000000000000000000000000000000aa"),
                        pad("0x00000000000000000000000000000000000000bb"),
                    ],
                    data: format!("0x{:064x}", 42u64),
                    block_number: 7,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt3".into(),
                    log_index: 2,
                },
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        };

        // Exactly the state the loop is in when it enters the recovery: the union went topic0-only,
        // the window has collapsed to one block, and that fetch came back over the cap.
        let (u_addrs, u_topics) = union_filter(
            [
                (stat.addresses.as_slice(), stat.topic0s.as_slice()),
                (fact.addresses.as_slice(), fact.topic0s.as_slice()),
            ]
            .into_iter(),
        );
        assert!(u_addrs.is_empty(), "premise: the union is topic0-only");
        let cause = anyhow::anyhow!("query returned more than 10000 results");
        assert!(
            chunker::is_result_too_large(&cause),
            "premise: this is the cap error the loop branches on"
        );

        let mut nests = vec![Some(stat), Some(fact)];
        let mut nexts = vec![7u64, 7];
        let mut sup = test_supervisor(2);
        recover_over_cap_block(
            &source,
            &mut nests,
            &mut nexts,
            &mut sup,
            &[0, 1],
            &[1],
            &u_topics,
            7,
            7,
            &cause,
        )
        .await
        .unwrap();

        // The factory nest survived the block that used to end it - no operator involved.
        assert!(
            matches!(sup.states[1], NestState::Live),
            "the factory nest must recover, not quarantine: {:?}",
            sup.states[1]
        );
        assert!(matches!(sup.states[0], NestState::Live));
        assert_eq!(nexts, vec![8, 8], "both nests advanced past the hot block");

        // Discovery survived too: the child's own `Ping`, emitted in the block it was created in, is
        // stored. This is the assertion a filter-narrowing fallback without the fixpoint fails.
        let fact_rows = nests[1]
            .as_ref()
            .unwrap()
            .store
            .entities_in_range(7, 7)
            .unwrap();
        assert!(
            fact_rows.iter().any(|r| r.contains("child__ping")),
            "the child created in this block must still be discovered and indexed: {fact_rows:?}"
        );
        assert!(
            fact_rows.iter().any(|r| r.contains("fac__child_created")),
            "the factory event itself is indexed: {fact_rows:?}"
        );

        // The static co-tenant indexed the same block off the same narrowed fetch - the one-fetch
        // density win is preserved, not paid for with a second cursor.
        let stat_rows = nests[0]
            .as_ref()
            .unwrap()
            .store
            .entities_in_range(7, 7)
            .unwrap();
        assert!(
            stat_rows.iter().any(|r| r.contains("tok__transfer")),
            "the static nest keeps indexing throughout: {stat_rows:?}"
        );

        // And the shape of the recovery: never a reissue of the wide fetch, and a second round for
        // the child that round 1 discovered.
        let calls = source.calls.lock().unwrap().clone();
        assert!(
            calls.iter().all(|(a, _, _)| !a.is_empty()),
            "the fallback must never reissue the topic0-only fetch: {calls:?}"
        );
        assert_eq!(
            calls.len(),
            2,
            "one narrowed fetch, one fixpoint round: {calls:?}"
        );
        assert!(
            calls[0].0.iter().any(|a| a.eq_ignore_ascii_case(token))
                && calls[0]
                    .0
                    .iter()
                    .any(|a| a.eq_ignore_ascii_case(factory_addr)),
            "round 1 asks for every live nest's known addresses: {:?}",
            calls[0].0
        );
        assert_eq!(
            calls[1].0,
            vec![child.to_string()],
            "round 2 asks for exactly the child discovered in round 1"
        );
        assert!(calls.iter().all(|(_, f, t)| *f == 7 && *t == 7));
    }

    /// The same recovery, but reached the way production reaches it: through the real
    /// [`runtime_index_loop`], whose cap branch is the only caller that matters. The unit test above
    /// proves the recovery works; this one proves the tip loop takes it - a distinction worth a test,
    /// because deleting the call site leaves that test green and the operator's nest dead.
    ///
    /// Against `main` this hangs on the poll and fails on the deadline: the cursor quarantines the
    /// factory nest terminally and block 7 is never indexed by anyone.
    #[tokio::test]
    async fn the_tip_loop_takes_the_cap_recovery_rather_than_quarantining() {
        let ds = tempfile::tempdir().unwrap();
        let df = tempfile::tempdir().unwrap();
        let stat = build_test_nest(ds.path(), "0x0000000000000000000000000000000000000011").await;
        let fact = build_factory_test_nest(df.path()).await;

        let token = "0x0000000000000000000000000000000000000011";
        let factory_addr = "0x0000000000000000000000000000000000000022";
        let child = "0x0000000000000000000000000000000000000033";
        let pad = |a: &str| format!("0x{:0>64}", a.trim_start_matches("0x"));
        let source: Arc<dyn Source> = Arc::new(CappedSource {
            logs: vec![
                crate::rpc::Log {
                    address: factory_addr.into(),
                    topics: vec![nest_topic0(&fact, "fac__child_created"), pad(child)],
                    data: "0x".into(),
                    block_number: 7,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt1".into(),
                    log_index: 0,
                },
                crate::rpc::Log {
                    address: child.into(),
                    topics: vec![nest_topic0(&fact, "child__ping")],
                    data: "0x".into(),
                    block_number: 7,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt2".into(),
                    log_index: 1,
                },
                crate::rpc::Log {
                    address: token.into(),
                    topics: vec![
                        nest_topic0(&stat, "tok__transfer"),
                        pad("0x00000000000000000000000000000000000000aa"),
                        pad("0x00000000000000000000000000000000000000bb"),
                    ],
                    data: format!("0x{:064x}", 42u64),
                    block_number: 7,
                    block_hash: "0xbh".into(),
                    tx_hash: "0xt3".into(),
                    log_index: 2,
                },
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        });

        // Keep the stores; the loop takes the nests.
        let fact_store = fact.store.clone();
        let stat_store = stat.store.clone();
        let health = Arc::new(crate::health::RuntimeHealth::new());
        // `--backfill 0` starts both nests at the tip, and a 1-block window puts the cursor straight
        // on block 7 with nothing left to shrink - the exact `global_next >= to` corner COR-5 lives in.
        let loop_task = tokio::spawn(runtime_index_loop(
            source.clone(),
            vec![stat, fact],
            Some(0),
            false,
            1,
            1,
            health.clone(),
            false,
            None,
        ));

        // Poll rather than sleep a fixed time: the loop is a real tip-follower and this is the only
        // honest way to wait for it. A dead cursor fails here on the deadline.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut fact_rows = Vec::new();
        while std::time::Instant::now() < deadline {
            fact_rows = fact_store.entities_in_range(7, 7).unwrap();
            if fact_rows.iter().any(|r| r.contains("child__ping")) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        loop_task.abort();

        assert!(
            fact_rows.iter().any(|r| r.contains("child__ping")),
            "the tip loop must recover the over-cap block, children and all: {fact_rows:?}"
        );
        assert_eq!(
            health.json_for("f").0,
            "indexing",
            "the factory nest must not be quarantined on the operator's health surface"
        );
        assert!(
            stat_store
                .entities_in_range(7, 7)
                .unwrap()
                .iter()
                .any(|r| r.contains("tok__transfer")),
            "the static co-tenant indexes the same block off the same narrowed fetch"
        );
    }

    /// The other side of the "not both, and not neither" rule: when the narrowed fetch cannot clear the
    /// cap either, the factory nest is quarantined **terminally**, and the log the operator reads names
    /// the two things that would actually fix it. The static co-tenant is still not faulted for it.
    #[tokio::test]
    async fn an_over_cap_block_with_no_narrowing_left_quarantines_with_the_operator_action() {
        let ds = tempfile::tempdir().unwrap();
        let df = tempfile::tempdir().unwrap();
        let stat = build_test_nest(ds.path(), "0x0000000000000000000000000000000000000011").await;
        let fact = build_factory_test_nest(df.path()).await;

        // A provider that is over the cap however the question is asked.
        struct AlwaysOverCap;
        #[async_trait::async_trait]
        impl Source for AlwaysOverCap {
            async fn tip(&self) -> Result<u64> {
                Ok(7)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                _f: u64,
                _t2: u64,
            ) -> Result<Vec<crate::rpc::Log>> {
                anyhow::bail!("query returned more than 10000 results")
            }
        }

        let (_, u_topics) = union_filter(
            [
                (stat.addresses.as_slice(), stat.topic0s.as_slice()),
                (fact.addresses.as_slice(), fact.topic0s.as_slice()),
            ]
            .into_iter(),
        );
        let mut nests = vec![Some(stat), Some(fact)];
        let mut nexts = vec![7u64, 7];
        let mut sup = test_supervisor(2);
        let cause = anyhow::anyhow!("query returned more than 10000 results");
        recover_over_cap_block(
            &AlwaysOverCap,
            &mut nests,
            &mut nexts,
            &mut sup,
            &[0, 1],
            &[1],
            &u_topics,
            7,
            7,
            &cause,
        )
        .await
        .unwrap();

        match &sup.states[1] {
            NestState::Quarantined { reason, retry_at } => {
                assert!(
                    retry_at.is_none(),
                    "retrying re-asks a question already answered twice"
                );
                assert!(
                    reason.contains("block 7 alone exceeds"),
                    "the operator needs the block named: {reason}"
                );
                assert!(
                    reason.contains("address-filtered refetch"),
                    "and needs to know the cheap fix was already tried: {reason}"
                );
                assert!(
                    reason.contains("higher/no cap") && reason.contains("`events` allowlist"),
                    "and needs the exact actions available to them: {reason}"
                );
            }
            other => panic!("the factory nest must be terminally quarantined, got {other:?}"),
        }
        // The static nest forced nothing and is faulted for nothing; it simply has not indexed this
        // block, and the cursor is still alive to retry it.
        assert!(matches!(sup.states[0], NestState::Live));
        assert_eq!(nexts, vec![7, 7], "an unfetched block advances nobody");
    }

    /// Issue #147, the headline scenario, on the real fan-out path: a reorg drops below one nest's
    /// sealed watermark. That nest cannot repair itself and is terminally quarantined - but its
    /// co-tenant on the same cursor, whose own watermark is below the fork, rolls back cleanly and
    /// keeps its cursor. Before RFC-0026 the first nest's `bail!` killed the shared cursor here,
    /// taking the healthy sibling down with it.
    #[tokio::test]
    async fn a_finality_violation_quarantines_one_nest_and_spares_its_co_tenant() {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let doomed = build_test_nest(da.path(), "0x0000000000000000000000000000000000000001").await;
        let healthy =
            build_test_nest(db.path(), "0x0000000000000000000000000000000000000002").await;
        seed_blocks(&doomed, &[10, 20, 30, 40, 50, 60, 80, 100]);
        seed_blocks(&healthy, &[10, 20, 30, 40, 50, 60, 80, 100]);

        // The doomed nest has sealed past the coming fork (60 > 50) - a finality violation it cannot
        // repair. Its co-tenant sealed only to 40, so a rollback to 50 is entirely routine for it.
        doomed.store.set_meta(SEALED_THROUGH_KEY, "60").unwrap();
        healthy.store.set_meta(SEALED_THROUGH_KEY, "40").unwrap();

        let mut nests = vec![Some(doomed), Some(healthy)];
        let mut nexts = vec![101u64, 101];
        let mut sup = test_supervisor(2);

        fan_out_rollback(&mut nests, &mut nexts, &mut sup, &[0, 1], 50).unwrap();

        // The doomed nest: terminally quarantined, with the finality reason recorded for the operator.
        match &sup.states[0] {
            NestState::Quarantined { reason, retry_at } => {
                assert!(
                    retry_at.is_none(),
                    "a finality violation must not be retried"
                );
                assert!(
                    reason.contains("finality violation"),
                    "reason should name the fault: {reason}"
                );
            }
            other => {
                panic!(
                    "the nest whose sealed watermark was violated must quarantine, got {other:?}"
                )
            }
        }
        // Its cursor is untouched - it claims nothing it did not index.
        assert_eq!(nexts[0], 101);

        // The co-tenant: rolled back cleanly, cursor moved to the fork, data intact below it. It is
        // still live, and (§3.1) it alone now drives the shared cursor.
        assert!(matches!(sup.states[1], NestState::Live));
        assert_eq!(nexts[1], 51);
        let healthy_nest = nests[1].as_ref().unwrap();
        assert_eq!(
            healthy_nest.store.entities_in_range(10, 50).unwrap().len(),
            5
        );
        assert!(healthy_nest
            .store
            .entities_in_range(51, 1_000)
            .unwrap()
            .is_empty());
        assert_eq!(sup.live(), vec![1]);
    }

    /// A fork deeper than every stored checkpoint yields `Some(0)` - roll back the whole hot store and
    /// re-index from origin. That is the *correct* recovery, and it is deliberately NOT guarded against
    /// even though it looks identical to a wrong-network endpoint (issue #150): block hashes alone
    /// cannot distinguish the two, so the wrong-chain case is caught upstream by
    /// `RpcClient::verify_chain_ids` and the established-nest case downstream by the sealed-watermark
    /// bail. An earlier attempt to refuse here broke `e2e_reorg::reorg_converges_to_canonical`; this
    /// test exists so nobody (including me, twice) makes that mistake again.
    #[tokio::test]
    async fn a_fork_below_every_checkpoint_rolls_all_the_way_back() {
        /// Agrees with none of our checkpoints - a fork deeper than our whole history.
        struct DeepForkSource;
        #[async_trait::async_trait]
        impl Source for DeepForkSource {
            async fn tip(&self) -> Result<u64> {
                Ok(1_000)
            }
            async fn block_hash(&self, n: u64) -> Result<Option<String>> {
                Ok(Some(format!("0xtheirs{n}")))
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                _f: u64,
                _to: u64,
            ) -> Result<Vec<crate::rpc::Log>> {
                Ok(vec![])
            }
            async fn block_timestamps(
                &self,
                blocks: &[u64],
            ) -> Result<std::collections::HashMap<u64, u64>> {
                Ok(blocks.iter().map(|&b| (b, b)).collect())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        for b in [100u64, 200, 300] {
            store.set_block_hash(b, &format!("0xours{b}")).unwrap();
        }
        assert_eq!(
            detect_reorg(&DeepForkSource, &store, 300).await.unwrap(),
            Some(0),
            "must roll back fully so re-indexing can reconverge on the canonical chain"
        );
    }

    /// The counterpart: a *genuine* reorg still resolves normally. The guard above must not have made
    /// ordinary rollbacks impossible - it only fires when NO checkpoint is canonical.
    #[tokio::test]
    async fn detect_reorg_still_finds_a_real_common_ancestor() {
        /// Canonical below 250, forked at and above it - an ordinary reorg to block 200.
        struct ForkedSource;
        #[async_trait::async_trait]
        impl Source for ForkedSource {
            async fn tip(&self) -> Result<u64> {
                Ok(1_000)
            }
            async fn block_hash(&self, n: u64) -> Result<Option<String>> {
                Ok(Some(if n >= 250 {
                    format!("0xforked{n}")
                } else {
                    format!("0xours{n}")
                }))
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                _f: u64,
                _to: u64,
            ) -> Result<Vec<crate::rpc::Log>> {
                Ok(vec![])
            }
            async fn block_timestamps(
                &self,
                blocks: &[u64],
            ) -> Result<std::collections::HashMap<u64, u64>> {
                Ok(blocks.iter().map(|&b| (b, b)).collect())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        for b in [100u64, 200, 300] {
            store.set_block_hash(b, &format!("0xours{b}")).unwrap();
        }
        assert_eq!(
            detect_reorg(&ForkedSource, &store, 300).await.unwrap(),
            Some(200),
            "the deepest surviving checkpoint below the fork"
        );
    }

    #[test]
    fn union_filter_dedups_across_nests_case_insensitively() {
        // Two nests, overlapping on one address ("0xAAA"/"0xaaa") and one topic (the Transfer sig).
        let a_addrs = vec!["0xAAA".to_string(), "0xBBB".to_string()];
        let a_topics = vec!["0xtransfer".to_string()];
        let b_addrs = vec!["0xaaa".to_string(), "0xCCC".to_string()];
        let b_topics = vec!["0xTRANSFER".to_string(), "0xapproval".to_string()];
        let (addrs, topics) = union_filter(
            [
                (a_addrs.as_slice(), a_topics.as_slice()),
                (b_addrs.as_slice(), b_topics.as_slice()),
            ]
            .into_iter(),
        );
        // 0xAAA and 0xaaa collapse to one; BBB and CCC distinct → 3 addresses, first-seen casing kept.
        assert_eq!(addrs, vec!["0xAAA", "0xBBB", "0xCCC"]);
        // The Transfer topic collapses across casing; Approval is B-only → 2 topics.
        assert_eq!(topics, vec!["0xtransfer", "0xapproval"]);
    }

    #[test]
    fn owns_demux_reproduces_the_solo_address_filter() {
        // The core byte-identity claim of slice 2a: routing the union fetch through `owns` hands a nest
        // exactly the logs a solo, address-filtered fetch would have - no more, no less. So its decode
        // input is identical, and therefore its stored output is too.
        let mut a = transfer_log(10, 0);
        a.address = "0xAAA0000000000000000000000000000000000000".into();
        let mut b = transfer_log(10, 1);
        b.address = "0xBBB0000000000000000000000000000000000000".into();
        let mut a2 = transfer_log(11, 0);
        a2.address = "0xaaa0000000000000000000000000000000000000".into(); // same nest A, checksummed differently
        let union = [a.clone(), b.clone(), a2.clone()];

        let nest_a_addrs = vec!["0xAAA0000000000000000000000000000000000000".to_string()];
        // Compare by a stable key (Log isn't PartialEq): (address-lowercased, block, log_index).
        let key =
            |l: &crate::rpc::Log| (l.address.to_ascii_lowercase(), l.block_number, l.log_index);
        // What the runtime feeds nest A: union filtered by A's ownership.
        let runtime_input: Vec<_> = union
            .iter()
            .filter(|l| addr_in(&nest_a_addrs, &l.address))
            .map(key)
            .collect();
        // What a solo, address-filtered source would return for nest A: only A's own logs.
        let solo_input: Vec<_> = [&a, &a2].into_iter().map(key).collect();
        assert_eq!(runtime_input, solo_input);
        // Nest B's log is never routed to A.
        assert!(!runtime_input
            .iter()
            .any(|(addr, _, _)| addr.eq_ignore_ascii_case(&b.address)));
    }

    // A Source backed by canned logs - lets us drive both backfill paths deterministically, offline.
    struct MockSource {
        logs: Vec<crate::rpc::Log>,
    }

    /// A source whose `block_hash` fails a fixed number of times before answering - a transport blip
    /// with a known end, which is what a provider dropping a connection actually looks like.
    struct FlakySource {
        logs: Vec<crate::rpc::Log>,
        fails_left: std::sync::Mutex<usize>,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Source for FlakySource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, n: u64) -> Result<Option<String>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut left = self.fails_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                anyhow::bail!("transport error: error sending request for url (mock)");
            }
            // Must agree with the hash the fixture logs carry, or the window reads as a reorg and is
            // discarded - which looks exactly like the bug under test and is not it.
            Ok(Some(format!("0x{n:064x}")))
        }
        async fn logs(
            &self,
            _f: &crate::source::LogFilter,
            _from: u64,
            _to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            Ok(self.logs.clone())
        }
    }

    /// #651. A transport blip during a window must be retried, not kill the nest.
    ///
    /// Driven through `process_window` with a **flaky `Source`** and a healthy gateway, because the
    /// bug was never in `retry_transient` - that helper's own unit tests all passed while a 454M-block
    /// backfill was dying eight hours in at 87.6% on one dropped connection. The bug was a call site
    /// that did not use it, so only a test that exercises the call site can see it.
    ///
    /// Proven by mutation: restore the bare `?` on the IPFS path's `block_hash` fetch and this fails,
    /// while every other test in the suite stays green.
    // NOT `start_paused`: this drives a real HTTP request at the stub gateway, and a paused clock
    // auto-advances past it so the fetch never completes. Three retries at a 250ms base is ~1.75s.
    #[tokio::test]
    async fn a_flaky_source_mid_window_is_retried_rather_than_killing_the_nest() {
        const DOC: &str = r#"{"n":1}"#;
        let (gateway, handle) = stub_gateway(DOC).await;
        let cid = crate::cid::cid_v0_for(DOC.as_bytes());
        let dir = tempfile::tempdir().unwrap();

        let flaky = Arc::new(FlakySource {
            logs: Vec::new(),
            fails_left: std::sync::Mutex::new(3),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let probe = flaky.clone();

        // No `expect` needed here - `run_ipfs_nest_with_source` unwraps the window itself, so a
        // propagated transport error panics and fails this test, which is precisely the regression.
        let rows = run_ipfs_nest_with_source(dir.path(), gateway, &cid, flaky).await;
        handle.abort();

        assert_eq!(
            rows.iter()
                .filter(|(_, v)| v["table"] == "token_metadata")
                .count(),
            1,
            "the document must still be resolved and stored after the blips, got {rows:?}"
        );
        // The premise: the failures must actually have happened, or this passed on the happy path.
        assert_eq!(
            *probe.fails_left.lock().unwrap(),
            0,
            "all three transient failures must have been consumed"
        );
        assert!(
            probe.calls.load(std::sync::atomic::Ordering::SeqCst) >= 4,
            "block_hash must have been retried past the failures"
        );
    }

    /// #653. A store must not serve rows under a registry that did not produce them.
    ///
    /// Three states, because only asserting the refusal would leave the two accepting paths free to
    /// break silently - and "accepts everything" is exactly what the bug was.
    #[test]
    fn a_store_refuses_a_registry_that_did_not_produce_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();

        // 1. Fresh store: adopts, and records what it adopted.
        guard_registry_identity(&store, "aaaa").expect("a fresh store adopts");
        assert_eq!(
            store.get_meta(REGISTRY_KEY).unwrap().as_deref(),
            Some("aaaa"),
            "the adopted hash must be recorded, or the next start adopts again and never compares"
        );

        // 2. Same registry: still fine.
        guard_registry_identity(&store, "aaaa").expect("an unchanged registry must be accepted");

        // 3. Different registry: refused, and the message must name both hashes - a refusal that
        //    does not say what changed sends the operator to the source to find out.
        let err = guard_registry_identity(&store, "bbbb")
            .expect_err("a changed registry must be refused, not adopted");
        let msg = format!("{err:#}");
        assert!(msg.contains("aaaa"), "must name the stored hash: {msg}");
        assert!(msg.contains("bbbb"), "must name the config hash: {msg}");
    }

    /// The upgrade path, which is the half that is easy to get wrong in the other direction: a store
    /// written before this guard existed has no recorded hash, and refusing it would break every
    /// running deployment on upgrade for a fault it may well not have. It adopts instead.
    #[test]
    fn a_store_predating_the_guard_adopts_rather_than_refusing() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        // Rows already indexed, no registry hash recorded - exactly an older nest's store.
        store.set_meta(LAST_BLOCK_KEY, "12345").unwrap();
        assert_eq!(store.get_meta(REGISTRY_KEY).unwrap(), None, "premise");

        guard_registry_identity(&store, "cccc").expect("an older store must not be refused");
        assert_eq!(
            store.get_meta(REGISTRY_KEY).unwrap().as_deref(),
            Some("cccc")
        );
        // And having adopted, it is now held to it.
        guard_registry_identity(&store, "dddd")
            .expect_err("once adopted, a later change must be refused");
    }

    #[async_trait::async_trait]
    impl Source for MockSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, b * 1000)).collect())
        }
    }

    /// A source that answers a window short **once** (#1144): the first ask whose `to` is in
    /// `short_top` comes back without the logs of the block *below* the top - a backend two blocks
    /// behind the head it was asked for - and every later ask is complete. Dropping the block below
    /// the top rather than the top itself is deliberate: its rows then arrive after the top block's
    /// rows are already in the buffer, which is the case the merge's re-sort exists for.
    struct ShortThenCompleteSource {
        logs: Vec<crate::rpc::Log>,
        short_top: std::sync::Mutex<std::collections::HashSet<u64>>,
    }

    #[async_trait::async_trait]
    impl Source for ShortThenCompleteSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            let drop_top = self.short_top.lock().unwrap().remove(&to);
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .filter(|l| !(drop_top && l.block_number + 1 == to))
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, b * 1000)).collect())
        }
        /// One header per block, enough for `block_row` (`hash` is all it requires), so a nest with
        /// `[extract] blocks` produces a block row alongside the block's events.
        async fn block_headers(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
            Ok(blocks
                .iter()
                .map(|&b| {
                    (
                        b,
                        serde_json::json!({
                            "hash": format!("0x{:064x}", b),
                            "timestamp": format!("0x{:x}", b * 1000),
                        }),
                    )
                })
                .collect())
        }
    }

    fn erc20_registry_with_filters() -> (DecodeRegistry, Vec<String>, Vec<String>) {
        use crate::registry::ContractSpec;
        const ERC20: &str = r#"[{"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true},
            {"name":"to","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#;
        let abi: alloy_json_abi::JsonAbi = serde_json::from_str(ERC20).unwrap();
        let addr: alloy_primitives::Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let reg = DecodeRegistry::build(vec![ContractSpec {
            alias: "usdc".into(),
            address: addr,
            abi,
            events: Vec::new(),
        }])
        .unwrap();
        let addresses = reg
            .addresses()
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect();
        let topic0s = reg
            .topic0s()
            .iter()
            .map(|t| format!("0x{}", hex::encode(t)))
            .collect();
        (reg, addresses, topic0s)
    }

    /// `(rows, hashes)` of every sealed segment, for comparing two runs.
    fn sealed(dir: &std::path::Path) -> (usize, Vec<(String, String)>) {
        let m = seal::load_manifest(dir).unwrap();
        let rows = m.tables.values().flatten().map(|s| s.rows).sum();
        let mut hashes: Vec<(String, String)> = m
            .tables
            .iter()
            .flat_map(|(t, segs)| segs.iter().map(move |s| (t.clone(), s.hash.clone())))
            .collect();
        hashes.sort();
        (rows, hashes)
    }

    /// #1144: a provider that answers a window's top block short is corrected by the next window,
    /// which asks for that block again; the rows land once, in order, and the sealed segments are
    /// byte-identical to a run against a provider that never answered short.
    ///
    /// Windows of five over 10..=39 end at 14, 19, 24, 29, 34 and 39; the source drops block 23 from
    /// the third window's first answer, 33 from the fifth's, and 38 from the last's - which has no
    /// window after it and is completed by the final pass. Without the overlap the run seals 56 rows
    /// and calls it done; without the final pass 58; without the dedup more than 60; without the
    /// re-sort the rows are all there and the content addresses differ, because 23's rows sit after
    /// 24's.
    #[tokio::test]
    async fn a_short_top_block_is_completed_by_the_next_window_and_sealed_exactly_once() {
        let (reg, addresses, topic0s) = erc20_registry_with_filters();
        let logs: Vec<_> = (10u64..40)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        let honest = MockSource { logs: logs.clone() };
        // 39 is the range's end: its short answer has no next window, so it is the final-pass case.
        let short = || ShortThenCompleteSource {
            logs: logs.clone(),
            short_top: std::sync::Mutex::new([24u64, 34, 39].into_iter().collect()),
        };

        let d_honest = tempfile::tempdir().unwrap();
        let n_honest = backfill_direct(
            &honest,
            &reg,
            d_honest.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            true,
        )
        .await
        .unwrap();
        let d_short = tempfile::tempdir().unwrap();
        let n_short = backfill_direct(
            &short(),
            &reg,
            d_short.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            true,
        )
        .await
        .unwrap();
        assert_eq!(n_honest, 60);
        assert_eq!(
            n_short, 60,
            "the two short answers must be completed by the next window"
        );
        assert_eq!(
            sealed(d_short.path()),
            sealed(d_honest.path()),
            "the short run must seal the same rows under the same content addresses"
        );

        // The pipelined path merges windows that were fetched concurrently, in order.
        let d_pipe = tempfile::tempdir().unwrap();
        let n_pipe = backfill_direct_pipelined(
            &short(),
            &reg,
            d_pipe.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            4,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(n_pipe, 60);
        assert_eq!(sealed(d_pipe.path()), sealed(d_honest.path()));
    }

    /// #1144, review: a block row and an event row of the same block are distinct rows to the merge,
    /// because block rows sit at `BLOCK_ROW_LOG_INDEX` in the reserved band (#642) and events cannot.
    #[test]
    fn a_block_row_and_the_first_event_of_its_block_both_survive_the_merge() {
        let mut buf: Vec<SealRow> = Vec::new();
        let block_row = (
            7u64,
            crate::registry::BLOCK_ROW_LOG_INDEX,
            "{\"block\":7}".to_string(),
        );
        let event = (7u64, 0u64, "{\"event\":0}".to_string());
        assert_eq!(
            merge_window_rows(&mut buf, 7, vec![event.clone(), block_row.clone()]).unwrap(),
            2
        );
        // The refetched tail brings both again; neither is new.
        assert_eq!(
            merge_window_rows(&mut buf, 7, vec![block_row, event]).unwrap(),
            0
        );
        assert_eq!(buf.len(), 2);
        assert_eq!(
            buf[0].1, 0,
            "the event sorts before the block row that summarises it"
        );
    }

    /// #1144, review: a refetched row under a held key with *different* content is neither a
    /// duplicate nor a second row - the store could hold only one - so the merge refuses it by name
    /// rather than keeping whichever answer came first.
    #[test]
    fn a_refetched_row_with_different_content_under_a_held_key_is_refused() {
        let mut buf: Vec<SealRow> = Vec::new();
        merge_window_rows(&mut buf, 7, vec![(7u64, 3u64, "{\"v\":1}".to_string())]).unwrap();
        let err = merge_window_rows(&mut buf, 7, vec![(7u64, 3u64, "{\"v\":2}".to_string())])
            .expect_err("different content under the same key must not merge");
        assert!(err.to_string().contains("block 7 log 3"), "{err}");
        assert_eq!(buf.len(), 1, "nothing was added or replaced");
    }

    /// #1144, review: a nest with `[extract] blocks` seals one block row per block *and* every
    /// event, through the refetch and the final pass, identically to an honest run.
    #[tokio::test]
    async fn blocks_and_event_rows_keep_distinct_identities_through_the_merge() {
        let (reg, addresses, topic0s) = erc20_registry_with_filters();
        let reg = reg.with_blocks(true);
        let logs: Vec<_> = (10u64..40)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        let with_short = |short: &[u64]| ShortThenCompleteSource {
            logs: logs.clone(),
            short_top: std::sync::Mutex::new(short.iter().copied().collect()),
        };
        let per_table = |dir: &std::path::Path| -> Vec<(String, usize)> {
            let m = seal::load_manifest(dir).unwrap();
            let mut v: Vec<(String, usize)> = m
                .tables
                .iter()
                .map(|(t, segs)| (t.clone(), segs.iter().map(|s| s.rows).sum()))
                .collect();
            v.sort();
            v
        };

        let d_honest = tempfile::tempdir().unwrap();
        backfill_direct(
            &with_short(&[]),
            &reg,
            d_honest.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            true,
        )
        .await
        .unwrap();
        let d_short = tempfile::tempdir().unwrap();
        backfill_direct(
            &with_short(&[24, 34, 39]),
            &reg,
            d_short.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            true,
        )
        .await
        .unwrap();

        assert_eq!(
            per_table(d_honest.path()),
            vec![
                ("blocks".to_string(), 30),
                ("usdc__transfer".to_string(), 60)
            ],
            "thirty blocks, one block row each, plus sixty events"
        );
        assert_eq!(per_table(d_short.path()), per_table(d_honest.path()));
        assert_eq!(sealed(d_short.path()), sealed(d_honest.path()));
    }

    /// #1144: the cut never enters the tail the next window will ask for again. With the tail held
    /// the buffer waits; once the tail is final (the range is complete) the same cut is taken.
    #[test]
    fn take_sealable_holds_the_refetched_tail_back() {
        let mut buf = corpus(30_000);
        let would_cut = buf[SEAL_DIRECT_BATCH - 1].0;
        // Hold from a block at or below the cut: nothing is sealable yet.
        assert!(take_sealable(&mut buf, would_cut).is_none());
        assert!(take_sealable(&mut buf, would_cut.saturating_sub(1)).is_none());
        // Hold from past the cut: the cut is exactly the one the data dictates.
        let (rows, cut) =
            take_sealable(&mut buf, would_cut + 1).expect("sealable once the tail is out");
        assert_eq!(cut, would_cut);
        assert!(rows.len() >= SEAL_DIRECT_BATCH);
        assert!(buf.iter().all(|r| r.0 > cut));
    }

    /// #1144: a refetched row the store already holds is dropped before the fold, so the balance view
    /// and the entity store both see it once. The second window here asks for blocks 11 and 12
    /// again, exactly as the tip loop does.
    #[tokio::test]
    async fn the_tip_path_folds_a_refetched_row_exactly_once() {
        let addr = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let d = tempfile::tempdir().unwrap();
        let mut nest = build_test_nest(d.path(), addr).await;
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });

        let first: Vec<_> = (10u64..=12)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        nest.process_window(source.as_ref(), &first, 10, 12, 100)
            .await
            .unwrap()
            .expect("first window commits");
        let recipient = "0xdb5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0";
        let after_first = nest
            .balances
            .balance(recipient)
            .expect("recipient was credited");
        assert!(after_first > 0);

        // Cursor at 13; the fetch started at 11 (the tail), so 11 and 12 arrive a second time.
        let second: Vec<_> = (11u64..=15)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        nest.process_window(source.as_ref(), &second, 13, 15, 100)
            .await
            .unwrap()
            .expect("second window commits");
        let after_second = nest.balances.balance(recipient).unwrap();
        assert_eq!(
            after_second,
            after_first * 2,
            "three new blocks after three: the two refetched blocks must not be folded again"
        );
        drop(nest);
        let store = Store::open(&d.path().join(DB_FILE)).unwrap();
        assert_eq!(
            store.entity_keys().unwrap().len(),
            12,
            "six blocks of two rows, each stored exactly once"
        );
    }

    /// #1144, review: a refetched tail row that is already stored under a *different* block hash is
    /// a reorg the handler has not yet rolled back. The window is refused and nothing is folded or
    /// stored, rather than the stale row being kept as if it were current.
    #[tokio::test]
    async fn a_refetched_tail_row_under_a_different_block_hash_refuses_the_window() {
        let addr = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48";
        let d = tempfile::tempdir().unwrap();
        let mut nest = build_test_nest(d.path(), addr).await;
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let first: Vec<_> = (10u64..=12)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        nest.process_window(source.as_ref(), &first, 10, 12, 100)
            .await
            .unwrap()
            .expect("first window commits");
        let recipient = "0xdb5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0";
        let before = nest.balances.balance(recipient).unwrap();

        // The tail comes back for blocks 11 and 12 under another hash: those blocks were reorged.
        let mut second: Vec<_> = (11u64..=15)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        for l in second.iter_mut().filter(|l| l.block_number <= 12) {
            l.block_hash = "0xreorged".into();
        }
        let outcome = nest
            .process_window(source.as_ref(), &second, 13, 15, 100)
            .await
            .unwrap();
        assert!(
            outcome.is_none(),
            "a stale tail must refuse the window, not store around it"
        );
        assert_eq!(
            nest.balances.balance(recipient).unwrap(),
            before,
            "nothing folded"
        );
        drop(nest);
        let store = Store::open(&d.path().join(DB_FILE)).unwrap();
        assert_eq!(store.entity_keys().unwrap().len(), 6, "nothing stored");
    }

    fn transfer_log(block: u64, li: u64) -> crate::rpc::Log {
        crate::rpc::Log {
            address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
            topics: vec![
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef".into(),
                "0x000000000000000000000000943f303a8019652d3a14b29954b2d780dde42ca3".into(),
                "0x000000000000000000000000db5985dbd132b9e5cc4bf0a18a8fb04a396ba0a0".into(),
            ],
            data: "0x000000000000000000000000000000000000000000000000000000001cd4ad20".into(),
            block_number: block,
            block_hash: "0xbh".into(),
            tx_hash: "0xtx".into(),
            log_index: li,
        }
    }

    /// RFC-0004 §3: the pipelined (concurrent-fetch) backfill produces **byte-identical** segments to
    /// the sequential path - concurrency overlaps latency without changing the output.
    #[tokio::test]
    async fn pipelined_backfill_matches_sequential() {
        use crate::registry::{ContractSpec, DecodeRegistry};
        const ERC20: &str = r#"[{"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true},
            {"name":"to","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#;
        let abi: alloy_json_abi::JsonAbi = serde_json::from_str(ERC20).unwrap();
        let addr: alloy_primitives::Address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            .parse()
            .unwrap();
        let reg = DecodeRegistry::build(vec![ContractSpec {
            alias: "usdc".into(),
            address: addr,
            abi,
            events: Vec::new(),
        }])
        .unwrap();

        let logs: Vec<_> = (10u64..40)
            .flat_map(|b| [transfer_log(b, 0), transfer_log(b, 1)])
            .collect();
        let source = MockSource { logs };
        let addresses: Vec<String> = reg
            .addresses()
            .iter()
            .map(|a| format!("0x{}", hex::encode(a)))
            .collect();
        let topic0s: Vec<String> = reg
            .topic0s()
            .iter()
            .map(|t| format!("0x{}", hex::encode(t)))
            .collect();

        let d_seq = tempfile::tempdir().unwrap();
        let seq = backfill_direct(
            &source,
            &reg,
            d_seq.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            true,
        )
        .await
        .unwrap();
        let d_pipe = tempfile::tempdir().unwrap();
        let pipe = backfill_direct_pipelined(
            &source,
            &reg,
            d_pipe.path(),
            &addresses,
            &topic0s,
            &[],
            None,
            0,
            10,
            39,
            5,
            8,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(seq, pipe, "same event count");
        assert!(seq > 0);
        let hashes = |dir: &std::path::Path| -> Vec<(String, String)> {
            let m = seal::load_manifest(dir).unwrap();
            m.tables
                .iter()
                .flat_map(|(t, segs)| segs.iter().map(move |s| (t.clone(), s.hash.clone())))
                .collect()
        };
        assert_eq!(
            hashes(d_seq.path()),
            hashes(d_pipe.path()),
            "concurrency must not change the sealed bytes"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // RFC-0029 slice 4: demand-driven timestamps.
    // ---------------------------------------------------------------------------------------------

    /// Counts the timestamp round trips a backfill makes, which is the thing slice 4 is trying to
    /// stop paying for. Everything else is the minimum a backfill needs.
    struct CountingSource {
        logs: Vec<crate::rpc::Log>,
        ts_calls: std::sync::atomic::AtomicUsize,
        ts_blocks: std::sync::atomic::AtomicUsize,
    }

    impl CountingSource {
        fn new(logs: Vec<crate::rpc::Log>) -> CountingSource {
            CountingSource {
                logs,
                ts_calls: std::sync::atomic::AtomicUsize::new(0),
                ts_blocks: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Source for CountingSource {
        async fn tip(&self) -> Result<u64> {
            Ok(self.logs.iter().map(|l| l.block_number).max().unwrap_or(0))
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            self.ts_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.ts_blocks
                .fetch_add(blocks.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(blocks.iter().map(|&b| (b, 1_700_000_000 + b)).collect())
        }
    }

    fn transfer_registry() -> DecodeRegistry {
        use crate::registry::ContractSpec;
        DecodeRegistry::build(vec![ContractSpec {
            alias: "tok".into(),
            address: "0x1111111111111111111111111111111111111111"
                .parse()
                .unwrap(),
            abi: serde_json::from_str(
                r#"[{"type":"event","name":"Ping","anonymous":false,"inputs":[{"name":"n","type":"uint256","indexed":false}]}]"#,
            )
            .unwrap(),
            events: Vec::new(),
        }])
        .unwrap()
    }

    fn ping_logs(reg: &DecodeRegistry, blocks: &[u64]) -> Vec<crate::rpc::Log> {
        let topic0 = format!("0x{}", hex::encode(reg.tables()[0].topic0));
        blocks
            .iter()
            .map(|&b| crate::rpc::Log {
                address: "0x1111111111111111111111111111111111111111".into(),
                topics: vec![topic0.clone()],
                data: format!("0x{:064x}", b),
                block_number: b,
                block_hash: format!("0x{b:064x}"),
                log_index: 0,
                tx_hash: format!("0xaa{b:062x}"),
            })
            .collect()
    }

    /// #765: a topic0 match on a foreign address must not pull a header. Collecting stamp
    /// blocks from the raw log list is the mutation this fails under.
    #[test]
    fn timestamp_blocks_exclude_logs_that_do_not_decode() {
        use crate::factory::ChildRegistry;
        let reg = transfer_registry();
        let kept = ping_logs(&reg, &[10]);
        let mut foreign = kept[0].clone();
        foreign.address = "0x9999999999999999999999999999999999999999".into();
        foreign.block_number = 99;
        foreign.log_index = 1;
        let logs = vec![kept[0].clone(), foreign];
        let mut children = ChildRegistry::new();
        let empty = std::collections::HashMap::new();
        let rows = decode_window(&reg, None, &mut children, &logs, &empty);
        assert_eq!(rows.len(), 1, "only the nest's own address decodes");
        assert_eq!(rows[0].block_number, 10);
        let stamp = blocks_needing_timestamps(&rows, std::iter::empty());
        assert_eq!(stamp, vec![10]);
        assert!(
            !stamp.contains(&99),
            "a discarded topic0 match must not buy a header"
        );
    }

    /// #765 acceptance: a topic0-only factory backfill must not buy headers for foreign addresses
    /// that share the event shape. `CountingSource` returns every log in range (the topic0-only
    /// shape); deleting the filter-before-stamp and collecting blocks from the raw log list makes
    /// `ts_blocks` 3 instead of 2.
    #[tokio::test]
    async fn factory_topic0_backfill_does_not_stamp_foreign_blocks() {
        use crate::registry::{ContractSpec, DecodeRegistry, TemplateSpec};
        use crate::rpc::Log;
        use std::sync::atomic::Ordering;

        let factory_addr = "0x1111111111111111111111111111111111111111";
        let pool_addr = "0x2222222222222222222222222222222222222222";
        let foreign_addr = "0x9999999999999999999999999999999999999999";

        let reg = DecodeRegistry::build_with_templates(
            vec![ContractSpec {
                alias: "factory".into(),
                address: factory_addr.parse().unwrap(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[{"name":"pool","type":"address","indexed":false}]}]"#,
                )
                .unwrap(),
                events: Vec::new(),
            }],
            vec![TemplateSpec {
                name: "pool".into(),
                abi: serde_json::from_str(
                    r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[{"name":"amount","type":"uint256","indexed":false}]}]"#,
                )
                .unwrap(),
                events: Vec::new(),
            }],
        )
        .unwrap();
        let topic0 = |table: &str| {
            format!(
                "0x{}",
                hex::encode(
                    reg.tables()
                        .iter()
                        .find(|d| d.table == table)
                        .unwrap()
                        .topic0
                )
            )
        };
        let config: crate::config::Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = crate::factory::FactorySet::build(&config).unwrap();

        let source = CountingSource::new(vec![
            Log {
                address: factory_addr.into(),
                topics: vec![topic0("factory__pool_created")],
                data: format!("0x{:0>64}", pool_addr.trim_start_matches("0x")),
                block_number: 10,
                block_hash: "0xbh".into(),
                tx_hash: "0xt1".into(),
                log_index: 0,
            },
            Log {
                address: pool_addr.into(),
                topics: vec![topic0("pool__swap")],
                data: format!("0x{:064x}", 7u64),
                block_number: 15,
                block_hash: "0xbh".into(),
                tx_hash: "0xt2".into(),
                log_index: 0,
            },
            Log {
                address: foreign_addr.into(),
                topics: vec![topic0("factory__pool_created")],
                data: format!("0x{:0>64}", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                block_number: 19,
                block_hash: "0xbh".into(),
                tx_hash: "0xt3".into(),
                log_index: 0,
            },
        ]);

        let dir = tempfile::tempdir().unwrap();
        let mut children = crate::factory::ChildRegistry::new();
        let sealed = backfill_direct_factory(
            &source,
            &reg,
            &fs,
            &mut children,
            dir.path(),
            &[topic0("factory__pool_created"), topic0("pool__swap")],
            &[],
            None,
            0,
            10,
            20,
            100,
            true,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(
            sealed, 2,
            "factory event and child swap; not the foreign log"
        );
        assert!(children.contains(pool_addr));
        assert_eq!(
            children.get(pool_addr).unwrap().discovered_timestamp,
            1_700_000_000 + 10,
            "decode-then-stamp must still fill discovered_timestamp"
        );
        assert_eq!(
            source.ts_blocks.load(Ordering::SeqCst),
            2,
            "headers only for kept rows (blocks 10 and 15), not the foreign topic0 at 19"
        );
    }

    /// **The RFC-0029 acceptance criterion for slice 4.** A nest declaring no use of `block_timestamp`
    /// backfills issuing *zero* timestamp round trips - and the rows it seals are byte-identical to a
    /// timestamped run modulo that one column.
    ///
    /// Both halves matter and neither is sufficient alone. Zero calls without the row comparison could
    /// be achieved by breaking the backfill; identical rows without the call count could be achieved by
    /// fetching timestamps and then discarding them, which is the version that saves nothing.
    #[tokio::test]
    async fn a_timestamp_free_nest_backfills_without_a_single_timestamp_call() {
        let reg = transfer_registry();
        let logs = ping_logs(&reg, &[10, 11, 12, 20, 21]);

        let with_ts = CountingSource::new(logs.clone());
        let d_with = tempfile::tempdir().unwrap();
        let n_with = backfill_direct(
            &with_ts,
            &reg,
            d_with.path(),
            &["0x1111111111111111111111111111111111111111".into()],
            &[],
            &[],
            None,
            0,
            10,
            21,
            100,
            true,
        )
        .await
        .unwrap();

        let without_ts = CountingSource::new(logs);
        let d_without = tempfile::tempdir().unwrap();
        let reg_off = transfer_registry().with_timestamps(false);
        let n_without = backfill_direct(
            &without_ts,
            &reg_off,
            d_without.path(),
            &["0x1111111111111111111111111111111111111111".into()],
            &[],
            &[],
            None,
            0,
            10,
            21,
            100,
            true,
        )
        .await
        .unwrap();

        assert_eq!(n_with, n_without, "the same rows are indexed either way");
        assert!(n_with > 0, "the fixture must actually produce rows");

        assert!(
            with_ts.ts_calls.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "control: a timestamped nest must still fetch timestamps, or this test proves nothing"
        );
        assert_eq!(
            without_ts
                .ts_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a timestamp-free nest must not issue a single block-header round trip"
        );
        assert_eq!(
            without_ts
                .ts_blocks
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "…and must not ask about a single block"
        );

        // "Byte-identical modulo that column" - read the sealed Parquet back rather than trusting the
        // manifest's row counts, which would still match if every value had been mangled.
        let with_cols = sealed_columns(d_with.path());
        let without_cols = sealed_columns(d_without.path());
        assert!(
            with_cols.contains_key("block_timestamp"),
            "control: the timestamped run sealed the column"
        );
        assert!(
            !without_cols.contains_key("block_timestamp"),
            "the timestamp-free run must not seal the column at all: {:?}",
            without_cols.keys().collect::<Vec<_>>()
        );
        let mut expected = with_cols.clone();
        expected.remove("block_timestamp");
        assert_eq!(
            expected, without_cols,
            "every other column must be identical, values included"
        );
    }

    /// A `Source` that counts `getLogs` calls and answers `block_headers` with a real header, so a
    /// blocks-only backfill still produces rows while the log traffic is observable.
    struct LogCountingSource {
        log_calls: std::sync::atomic::AtomicUsize,
    }

    impl LogCountingSource {
        fn new() -> LogCountingSource {
            LogCountingSource {
                log_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.log_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Source for LogCountingSource {
        async fn tip(&self) -> Result<u64> {
            Ok(100)
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            _from: u64,
            _to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            self.log_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        }
        async fn block_headers(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
            Ok(blocks
                .iter()
                .map(|&b| {
                    (
                        b,
                        serde_json::json!({
                            "hash": format!("0x{b:064x}"),
                            "parentHash": format!("0x{:064x}", b.saturating_sub(1)),
                            "miner": "0x0000000000000000000000000000000000000000",
                            "gasUsed": "0x0",
                            "gasLimit": "0x1388",
                            "size": "0x220",
                            "timestamp": format!("0x{:x}", 1_700_000_000 + b),
                            "transactions": [],
                        }),
                    )
                })
                .collect())
        }
    }

    /// A nest with **both** a contract and `[extract] blocks = true`, through the real `build_nest`.
    /// Nothing in `config.rs` refuses this combination, so it is a shape an operator can write.
    async fn build_blocks_and_contract_nest(dir: &std::path::Path) -> NestIngest {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"bc\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"tok\"\naddress = \"0x1111111111111111111111111111111111111111\"\n\
             abi = \"abis/tok.json\"\n\n\
             [extract]\nblocks = true\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/tok.json"),
            r#"[{"type":"event","name":"Ping","inputs":[],"anonymous":false}]"#,
        )
        .unwrap();
        let config = Config::load(dir).unwrap();
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, worker, _w) = build_nest(
            &source,
            dir.to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a nest with a contract and [extract] blocks must build");
        if let Some(w) = worker {
            w.abort();
        }
        nest
    }

    /// A stub JSON-RPC that answers every `eth_call` in a batch with the same fixed word, and
    /// records the blocks it was asked about so a test can assert the *pin* rather than just the
    /// answer.
    async fn stub_state_rpc(
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Router};
        async fn handler(
            axum::extract::State(seen): axum::extract::State<
                std::sync::Arc<std::sync::Mutex<Vec<String>>>,
            >,
            body: String,
        ) -> axum::Json<serde_json::Value> {
            let req: serde_json::Value =
                serde_json::from_str(&body).unwrap_or(serde_json::json!([]));
            let one = |r: &serde_json::Value| {
                // `eth_call` params are `[{to,data}, block]`. Record `block|to|data` so a test can
                // assert the *pin* and the *argument*, not merely that something was called.
                let p = r.get("params");
                let at = |k: &str| {
                    p.and_then(|p| p.get(0))
                        .and_then(|o| o.get(k))
                        .and_then(|v| v.as_str())
                        .unwrap_or("?")
                        .to_string()
                };
                let blk = p
                    .and_then(|p| p.get(1))
                    .and_then(|b| b.as_str())
                    .unwrap_or("?");
                seen.lock()
                    .unwrap()
                    .push(format!("{blk}|{}|{}", at("to"), at("data")));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": r.get("id").cloned().unwrap_or(serde_json::json!(1)),
                    "result": format!("0x{:064x}", 42),
                })
            };
            axum::Json(match req.as_array() {
                Some(rs) => serde_json::Value::Array(rs.iter().map(one).collect()),
                None => one(&req),
            })
        }
        let app = Router::new()
            .route("/", post(handler))
            .route("/{*rest}", post(handler))
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle)
    }

    fn write_calls_nest(dir: &std::path::Path) {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"c\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"tok\"\naddress = \"0x1111111111111111111111111111111111111111\"\n\
             abi = \"abis/tok.json\"\n\n\
             [[calls]]\nname = \"oracle_answer\"\n\
             contract = \"0x2222222222222222222222222222222222222222\"\n\
             calldata = \"0x18160ddd\"\nevery = 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/tok.json"),
            r#"[{"type":"event","name":"Ping","inputs":[],"anonymous":false}]"#,
        )
        .unwrap();
    }

    /// RFC-0023 tier 3, end to end: a declared `[[calls]]` read is resolved at the blocks it samples
    /// and stored as rows.
    ///
    /// Before this, `resolve_at` had no caller at all - the machinery was built, tested and reachable
    /// from config, and a declaration was accepted, validated and then ignored forever (#262). This
    /// is the test that the wire exists, driven through the real `build_nest` and the real
    /// `process_window` against a stub archive endpoint.
    #[tokio::test]
    async fn a_declared_call_is_resolved_at_every_sampled_block_and_stored() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a calls nest with a state RPC must build");
        if let Some(w) = worker {
            w.abort();
        }

        nest.process_window(source.as_ref(), &[], 1, 3, 100)
            .await
            .unwrap()
            .expect("the window must commit");
        // Both handles hold the store: redb takes its exclusive flock at `Database::open`, so a
        // second handle is refused while either is alive.
        drop(nest);
        drop(state);

        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let keys = store.entity_keys().unwrap();
        assert_eq!(
            keys.len(),
            3,
            "`every = 1` over blocks 1-3 owes three resolved rows, got {}: {keys:?}",
            keys.len()
        );

        // The pin is the point: each sample must be asked at *its own* block, not at `latest`.
        let asked: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.split('|').next().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            asked,
            vec!["0x1", "0x2", "0x3"],
            "each sample must be pinned to its own block - `latest` would break determinism"
        );

        let row: serde_json::Value =
            serde_json::from_str(&store.get_entity(&keys[0]).unwrap().unwrap()).unwrap();
        assert_eq!(row["table"], "oracle_answer", "row: {row}");
        assert_eq!(
            row["reverted"], false,
            "a stub that answers is not a revert"
        );
        handle.abort();
    }

    /// `--seal-direct` with declared `[[calls]]` resolves them and seals their rows, rather than
    /// silently producing an empty table over the backfill range.
    ///
    /// The previous guard (RFC-0038 §6e) refused this combination because the seal-direct paths did
    /// not resolve calls. That guard is gone: the three seal-direct functions now call
    /// `resolve_calls_for_window` per window, matching what `process_window` does on the hot path.
    /// This test is the #262-pattern check: it would pass with the mechanism removed only if the
    /// fixture produces zero sampled blocks, so `every = 1` ensures there is always at least one.
    #[tokio::test]
    async fn seal_direct_with_declared_calls_resolves_and_seals_them() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        // A source with no events, so any rows in the sealed segment come from [[calls]] only.
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        if let Some(w) = worker {
            w.abort();
        }

        // `backfill = Some(5)` asks for blocks 1..=5, `seal_direct = true` takes the fast path.
        // `every = 1` in the fixture means the call fires at every block in the window.
        let result = nest.prepare(source.as_ref(), Some(5), true, 1, 100).await;
        assert!(
            result.is_ok(),
            "seal-direct with calls must succeed now: {result:?}"
        );

        // The stub RPC was hit: at least one `eth_call` was issued for the sampled blocks.
        let calls = seen.lock().unwrap().clone();
        assert!(
            !calls.is_empty(),
            "seal-direct must have issued at least one eth_call to the state RPC, got none"
        );

        // The sealed segment carries `oracle_answer` rows - not silently absent.
        let m = crate::seal::load_manifest(dir.path()).unwrap();
        assert!(
            m.tables.contains_key("oracle_answer"),
            "the [[calls]] table must appear in the sealed manifest; got tables: {:?}",
            m.tables.keys().collect::<Vec<_>>()
        );

        drop(nest);
        drop(state);
        handle.abort();
    }

    /// #745: `backfill_direct_factory` is the third seal-direct path. The other two go red if their
    /// `state_rpc` branch is deleted. This one did not: 659 tests still passed. A factory nest
    /// with `[[calls]]` is the production shape that reaches it.
    /// #1163: the gauge `/ready` reports is seeded from the store when the nest is built, which is
    /// before a direct seal has written anything, and the ordinary seal is the only other writer. So
    /// the direct seal must publish to it itself - this is the one seam both its per-segment callback
    /// and its completion go through. (The end-to-end seal-direct fixture above cannot pin this: its
    /// ordinary seal runs afterwards and sets the gauge regardless.)
    #[test]
    fn a_direct_seal_publishes_its_watermark_to_the_gauge_ready_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&dir.path().join("t.redb")).unwrap();
        let m = crate::metrics::METRICS.nest("direct-seal-watermark-1163");
        assert_eq!(m.sealed_through(), 0, "fresh nest metrics start at 0");
        publish_direct_seal(&m, &store, 501_993_721).unwrap();
        assert_eq!(
            m.sealed_through(),
            501_993_721,
            "the gauge carries the direct seal's watermark"
        );
        assert_eq!(m.seal_direct_completed(), 501_993_721);
        assert_eq!(
            store.sealed_through(),
            501_993_721,
            "and so does the durable key"
        );
    }

    #[tokio::test]
    async fn seal_direct_factory_with_declared_calls_resolves_them() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"f\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"fac\"\naddress = \"0x0000000000000000000000000000000000000022\"\n\
             abi = \"abis/fac.json\"\n\n\
             [[templates]]\nname = \"child\"\nabi = \"abis/child.json\"\n\n\
             [[factories]]\nwatch = \"fac\"\nevent = \"ChildCreated\"\nchild_param = \"child\"\n\
             template = \"child\"\n\n\
             [[calls]]\nname = \"oracle_answer\"\n\
             contract = \"0x2222222222222222222222222222222222222222\"\n\
             calldata = \"0x18160ddd\"\nevery = 1\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("abis/fac.json"),
            r#"[{"type":"event","name":"ChildCreated","inputs":[{"name":"child","type":"address","indexed":true}],"anonymous":false}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("abis/child.json"),
            r#"[{"type":"event","name":"Ping","inputs":[],"anonymous":false}]"#,
        )
        .unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let source: Arc<dyn Source> = Arc::new(MockSource {
            logs: vec![crate::rpc::Log {
                address: "0x0000000000000000000000000000000000000022".into(),
                topics: vec!["0x".into()],
                data: "0x".into(),
                block_number: 5,
                block_hash: format!("0x{:064x}", 5),
                tx_hash: "0x1".into(),
                log_index: 0,
            }],
        });
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        assert!(nest.factory.is_some(), "fixture must be a factory nest");
        if let Some(w) = worker {
            w.abort();
        }

        let result = nest.prepare(source.as_ref(), Some(5), true, 1, 100).await;
        assert!(
            result.is_ok(),
            "seal-direct factory with calls must succeed: {result:?}"
        );
        let calls = seen.lock().unwrap().clone();
        assert!(
            !calls.is_empty(),
            "backfill_direct_factory must have issued at least one eth_call, got none - #745 is \
             that this path can be deleted and the suite stays green"
        );
        drop(nest);
        drop(state);
        handle.abort();
    }

    /// Counts hash-fetch round trips the tier-3 `[[calls]]` resolution makes: `block_hash` (the
    /// unbatched, single-block call #720 found) versus `block_headers` (the batched call it should
    /// use instead, since the same blocks' timestamps already came from a batched fetch a few lines
    /// above the calls-resolution loop).
    struct CallHashCountingSource {
        block_hash_calls: std::sync::atomic::AtomicUsize,
        block_headers_calls: std::sync::atomic::AtomicUsize,
        block_headers_blocks: std::sync::atomic::AtomicUsize,
    }

    impl CallHashCountingSource {
        fn new() -> CallHashCountingSource {
            CallHashCountingSource {
                block_hash_calls: std::sync::atomic::AtomicUsize::new(0),
                block_headers_calls: std::sync::atomic::AtomicUsize::new(0),
                block_headers_blocks: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl Source for CallHashCountingSource {
        async fn tip(&self) -> Result<u64> {
            Ok(3)
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            self.block_hash_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Some("0xbh".into()))
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            _from: u64,
            _to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            Ok(Vec::new())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, 1_700_000_000 + b)).collect())
        }
        async fn block_headers(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
            self.block_headers_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.block_headers_blocks
                .fetch_add(blocks.len(), std::sync::atomic::Ordering::SeqCst);
            Ok(blocks
                .iter()
                .map(|&b| (b, serde_json::json!({"hash": format!("0x{b:064x}")})))
                .collect())
        }
    }

    /// #720: tier-3 `[[calls]]` resolution fetched each sampled block's hash with its own unbatched
    /// `block_hash` round trip, even though the same blocks' timestamps had just come from a batched
    /// fetch a few lines above. Proven on request shape rather than the clock - a wall-clock
    /// assertion in this suite has never been reliably green in CI (#736) - so this counts round
    /// trips instead, on the enforcing surface: delete the batched `block_headers` call and this
    /// reds because `block_hash_calls` stops being 1.
    ///
    /// The floor is 1, not 0: every window - `[[calls]]` or not - fetches the window boundary's
    /// hash once for the reorg checkpoint (`src/indexer.rs`, the `block_hash(to)` call after
    /// screening). Confirmed against the pre-fix code before writing this assertion: it reported 4,
    /// the checkpoint's 1 plus one unbatched call per sampled block (three, for `every = 1` over
    /// blocks 1..=3) - proving the defect was reachable through the real constructor before fixing
    /// it, not just reasoned about.
    #[tokio::test]
    async fn tier3_calls_hash_fetch_is_batched_not_per_block() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let counting = std::sync::Arc::new(CallHashCountingSource::new());
        let source: Arc<dyn Source> = counting.clone();
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a calls nest with a state RPC must build");
        if let Some(w) = worker {
            w.abort();
        }

        // `every = 1` over blocks 1..=3 samples all three, so a per-block hash fetch would be three
        // `block_hash` round trips; a batched one is a single `block_headers` call for all three.
        nest.process_window(source.as_ref(), &[], 1, 3, 100)
            .await
            .unwrap()
            .expect("the window must commit");
        drop(nest);
        drop(state);
        handle.abort();

        assert_eq!(
            counting
                .block_hash_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "tier-3 calls resolution must not fetch any block's hash with the unbatched, \
             single-block call - the one remaining call is the window's own reorg-checkpoint \
             hash, unrelated to [[calls]], not a per-sampled-block fetch"
        );
        assert_eq!(
            counting
                .block_headers_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the three sampled blocks' hashes must come from one batched call, not three"
        );
        assert_eq!(
            counting
                .block_headers_blocks
                .load(std::sync::atomic::Ordering::SeqCst),
            3,
            "the batched call must cover all three sampled blocks"
        );
    }

    /// Wraps [`CallHashCountingSource`] for the seal-direct test only, overriding `tip`/`finalized`
    /// so `prepare`'s cold-start math actually seals the range under test. The hot-path tests above
    /// rely on the inner source's `finalized() -> None` (Arbitrum's 1800-block fallback depth) to
    /// keep `maybe_seal` a no-op, so rows stay visible in the hot store for their own assertions -
    /// giving the inner source real finality would silently seal those rows out from under them.
    struct SealDirectSource(std::sync::Arc<CallHashCountingSource>);

    #[async_trait::async_trait]
    impl Source for SealDirectSource {
        async fn tip(&self) -> Result<u64> {
            Ok(5)
        }
        async fn finalized(&self) -> Result<Option<u64>> {
            Ok(Some(5))
        }
        async fn block_hash(&self, n: u64) -> Result<Option<String>> {
            self.0.block_hash(n).await
        }
        async fn logs(
            &self,
            filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            self.0.logs(filter, from, to).await
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            self.0.block_timestamps(blocks).await
        }
        async fn block_headers(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
            self.0.block_headers(blocks).await
        }
    }

    /// NIG-325 review probe (not part of #720's own PR): #720's own request-count test only drives
    /// `process_window` (the streaming hot path). The PR's own body says `resolve_calls_for_window`
    /// - the seal-direct / backfill_direct / factory path - got the identical batching change, but
    /// no test in the PR counts requests on *that* path. This does, via seal-direct's own
    /// `nest.prepare(..., seal_direct=true, ...)` entry point.
    #[tokio::test]
    async fn tier3_seal_direct_calls_hash_fetch_is_batched_not_per_block() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let counting = std::sync::Arc::new(CallHashCountingSource::new());
        let source: Arc<dyn Source> = std::sync::Arc::new(SealDirectSource(counting.clone()));
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        if let Some(w) = worker {
            w.abort();
        }

        // `tip = 5`, `backfill = Some(4)` asks for blocks 1..=5 via seal-direct (`cold_start_block`);
        // `every = 1` samples all five.
        let result = nest.prepare(source.as_ref(), Some(4), true, 1, 100).await;
        assert!(
            result.is_ok(),
            "seal-direct with calls must succeed: {result:?}"
        );
        drop(nest);
        drop(state);
        handle.abort();

        let hash_calls = counting
            .block_hash_calls
            .load(std::sync::atomic::Ordering::SeqCst);
        let headers_calls = counting
            .block_headers_calls
            .load(std::sync::atomic::Ordering::SeqCst);
        let headers_blocks = counting
            .block_headers_blocks
            .load(std::sync::atomic::Ordering::SeqCst);
        eprintln!(
            "seal-direct over 5 blocks: block_hash_calls={hash_calls} block_headers_calls={headers_calls} block_headers_blocks={headers_blocks}"
        );
        assert!(
            hash_calls <= headers_calls.max(1) + 1,
            "seal-direct's calls resolution should not be issuing an unbatched block_hash per \
             sampled block: block_hash_calls={hash_calls}, block_headers_calls={headers_calls} \
             (5 sampled blocks would mean 5 unbatched calls pre-fix)"
        );
        assert!(
            headers_blocks >= 5,
            "the batched header call(s) must cover all 5 sampled blocks, covered only {headers_blocks}"
        );
    }

    /// NIG-325 review probe (not part of #720's own PR): the mapping test below
    /// (`tier3_calls_stored_hash_matches_its_own_sampled_block`) only drives `process_window`, the
    /// hot-path fold - it cannot see a misindexed batched header map on the seal-direct path
    /// (`resolve_calls_for_window`), which got the identical change. This drives seal-direct instead
    /// and reads the sealed Parquet (`sealed_columns`, defined further down this module) rather than
    /// the hot store, since a seal-direct backfill never writes through redb at all.
    #[tokio::test]
    async fn tier3_seal_direct_calls_stored_hash_matches_its_own_sampled_block() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let counting = std::sync::Arc::new(CallHashCountingSource::new());
        let source: Arc<dyn Source> = std::sync::Arc::new(SealDirectSource(counting.clone()));
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        if let Some(w) = worker {
            w.abort();
        }

        // `tip = 5`, `backfill = Some(4)` asks for blocks 1..=5 via seal-direct; `every = 1` samples
        // all five, so the sealed segment owes five `oracle_answer` rows.
        let result = nest.prepare(source.as_ref(), Some(4), true, 1, 100).await;
        assert!(
            result.is_ok(),
            "seal-direct with calls must succeed: {result:?}"
        );
        drop(nest);
        drop(state);
        handle.abort();

        let cols = sealed_columns(dir.path());
        let blocks = cols
            .get("block_number")
            .expect("sealed oracle_answer rows must carry block_number");
        let hashes = cols
            .get("block_hash")
            .expect("sealed oracle_answer rows must carry block_hash");
        assert_eq!(
            blocks.len(),
            hashes.len(),
            "block_number and block_hash must be the same length"
        );
        assert_eq!(
            blocks.len(),
            5,
            "every = 1 over blocks 1-5 owes five sealed rows: {blocks:?}"
        );
        // `sealed_columns` formats each cell as `format!("{:?}", array_value_to_string(...))`, i.e.
        // `Ok("1")` / `Ok("0x00..01")` - strip that wrapper rather than assume a bare value.
        let unwrap_cell = |s: &str| -> String {
            s.strip_prefix("Ok(\"")
                .and_then(|s| s.strip_suffix("\")"))
                .unwrap_or_else(|| panic!("unexpected sealed cell format: {s}"))
                .to_string()
        };
        for (b, h) in blocks.iter().zip(hashes) {
            let block: u64 = unwrap_cell(b)
                .parse()
                .unwrap_or_else(|_| panic!("block_number {b} did not parse as u64"));
            let expected_hash = format!("0x{block:064x}");
            assert_eq!(
                unwrap_cell(h),
                expected_hash,
                "sealed row for block {block} carries a hash belonging to a different block \
                 (block_number column: {blocks:?}, block_hash column: {hashes:?})"
            );
        }
    }

    /// NIG-325 review probe (not part of #720's own PR): the count-based test above cannot tell a
    /// correct block->hash mapping from a shuffled one, because `CallHashCountingSource` never
    /// checks which hash landed on which row. This does: `CallHashCountingSource::block_headers`
    /// already returns a *distinct* hash per block (`0x{b:064x}`), so if the batched map were ever
    /// misindexed - a neighbouring block's hash landing on this block's row - it would show up here.
    #[tokio::test]
    async fn tier3_calls_stored_hash_matches_its_own_sampled_block() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let counting = std::sync::Arc::new(CallHashCountingSource::new());
        let source: Arc<dyn Source> = counting.clone();
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a calls nest with a state RPC must build");
        if let Some(w) = worker {
            w.abort();
        }

        nest.process_window(source.as_ref(), &[], 1, 3, 100)
            .await
            .unwrap()
            .expect("the window must commit");
        drop(nest);
        drop(state);
        handle.abort();

        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let keys = store.entity_keys().unwrap();
        assert_eq!(keys.len(), 3, "every = 1 over blocks 1-3 owes three rows");
        for key in &keys {
            let row: serde_json::Value =
                serde_json::from_str(&store.get_entity(key).unwrap().unwrap()).unwrap();
            let block = row["block_number"].as_u64().unwrap();
            let expected = format!("0x{block:064x}");
            assert_eq!(
                row["block_hash"], expected,
                "row for block {block} carries a hash belonging to a different block: {row}"
            );
        }
    }

    /// A stub IPFS gateway serving one fixed body for any path.
    async fn stub_gateway(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::get, Router};
        let app = Router::new().route("/{*rest}", get(move || async move { body.to_string() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/ipfs/"), handle)
    }

    /// ABI-encode a single non-indexed `string` into a log's data field.
    fn abi_string(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = format!("0x{:064x}{:064x}", 32, b.len());
        let mut padded = b.to_vec();
        padded.resize(b.len().div_ceil(32) * 32, 0);
        out.push_str(&hex::encode(padded));
        out
    }

    async fn run_ipfs_nest(
        dir: &std::path::Path,
        gateway: String,
        uri: &str,
    ) -> Vec<(String, serde_json::Value)> {
        run_ipfs_nest_with_source(dir, gateway, uri, Arc::new(MockSource { logs: Vec::new() }))
            .await
    }

    /// The same harness with the `Source` injected, so a test can make the *ingestion* side flaky
    /// while the gateway stays healthy - the only way to reach the `block_hash` fetch inside the
    /// IPFS path from a test.
    async fn run_ipfs_nest_with_source(
        dir: &std::path::Path,
        gateway: String,
        uri: &str,
        source: Arc<dyn Source>,
    ) -> Vec<(String, serde_json::Value)> {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"ip\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"nft\"\naddress = \"0x1111111111111111111111111111111111111111\"\n\
             abi = \"abis/nft.json\"\n\n\
             [[ipfs]]\nname = \"token_metadata\"\non = \"nft__uri_set\"\ncid_column = \"uri\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/nft.json"),
            r#"[{"type":"event","name":"UriSet","inputs":[{"name":"uri","type":"string","indexed":false}],"anonymous":false}]"#,
        )
        .unwrap();
        let mut config = Config::load(dir).unwrap();
        config.ipfs_gateways = vec![gateway];

        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("an ipfs nest must build");
        if let Some(w) = worker {
            w.abort();
        }
        let topic0 = format!(
            "0x{}",
            hex::encode(
                nest.registry
                    .tables()
                    .iter()
                    .find(|d| d.table == "nft__uri_set")
                    .expect("fixture must expose nft__uri_set")
                    .topic0
            )
        );
        // Two events naming the *same* document, so dedupe is exercised rather than assumed.
        let logs: Vec<crate::rpc::Log> = (0..2)
            .map(|i| crate::rpc::Log {
                address: "0x1111111111111111111111111111111111111111".into(),
                topics: vec![topic0.clone()],
                data: abi_string(uri),
                block_number: 9,
                block_hash: format!("0x{:064x}", 9),
                tx_hash: format!("0xt{i}"),
                log_index: i,
            })
            .collect();
        nest.process_window(source.as_ref(), &logs, 9, 9, 100)
            .await
            .unwrap()
            .expect("the window must commit");
        drop(nest);
        drop(state);

        let store = Store::open(&dir.join(DB_FILE)).unwrap();
        store
            .entity_keys()
            .unwrap()
            .into_iter()
            .map(|k| {
                let v: serde_json::Value =
                    serde_json::from_str(&store.get_entity(&k).unwrap().unwrap()).unwrap();
                (k, v)
            })
            .collect()
    }

    /// **RFC-0037, end to end: a document a row points at is fetched, verified and stored.**
    ///
    /// The last thing a subgraph could do that a nest could not. `file/ipfs` data sources index the
    /// content behind a CID; `subgraph_import` said nuthatch "indexes the metadata hash as a column
    /// value and stops there".
    #[tokio::test]
    async fn a_declared_ipfs_document_is_resolved_verified_and_stored() {
        const DOC: &str = r#"{"name":"Nuthatch #1","image":"ipfs://Qm..."}"#;
        let cid = crate::cid::cid_v0_for(DOC.as_bytes());
        let (gateway, handle) = stub_gateway(DOC).await;
        let dir = tempfile::tempdir().unwrap();
        let rows = run_ipfs_nest(dir.path(), gateway, &cid).await;

        let resolved: Vec<_> = rows
            .iter()
            .filter(|(_, v)| v["table"] == "token_metadata")
            .collect();
        assert_eq!(
            resolved.len(),
            1,
            "two events naming one document owe ONE fetch and ONE row - a CID is a content address: \
             {rows:?}"
        );
        let (key, row) = resolved[0];
        assert_eq!(row["content"], DOC, "the document itself must be stored");
        assert_eq!(row["cid"], cid);
        assert_eq!(row["verified"], true, "a small document is provable");
        let ordinal: u64 = key.rsplit('-').next().unwrap().parse().unwrap();
        assert!(
            (crate::registry::IPFS_ROW_LOG_INDEX_BASE..crate::registry::TX_CALL_ROW_LOG_INDEX_BASE)
                .contains(&ordinal),
            "resolutions belong in their own slice of the reserved band, clear of logs, reads and \
             calls: {ordinal}"
        );
        handle.abort();
    }

    /// **A gateway that answers with something else must produce no row at all.**
    ///
    /// This is the whole argument for letting IPFS feed canonical state. An HTTP enricher can hand
    /// two operators different answers with neither able to tell; a CID cannot, because the bytes are
    /// checkable. Absence is the honest outcome and a `LEFT JOIN` reads it as "not yet" - a *wrong*
    /// document stored as if it were right would be the silent-wrong-answer failure this project
    /// cares most about.
    ///
    /// The body is one a real gateway really returned while this was being written.
    #[tokio::test]
    async fn a_gateway_serving_the_wrong_document_yields_no_row() {
        const DOC: &str = r#"{"name":"Nuthatch #1","image":"ipfs://Qm..."}"#;
        let cid = crate::cid::cid_v0_for(DOC.as_bytes());
        let (gateway, handle) = stub_gateway(
            "Unable to retrieve content within timeout period: timeout occurred after finding 3 \
             provider(s) and connecting to 3 (phase: connecting to providers)",
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let rows = run_ipfs_nest(dir.path(), gateway, &cid).await;

        assert!(
            !rows.iter().any(|(_, v)| v["table"] == "token_metadata"),
            "a body that does not hash to the CID must not be stored as the document: {rows:?}"
        );
        // The events themselves are still indexed - only the resolution is absent, which is exactly
        // the shape the side-table design exists to express.
        assert_eq!(
            rows.iter()
                .filter(|(_, v)| v["table"] == "nft__uri_set")
                .count(),
            2,
            "the source rows are unaffected: {rows:?}"
        );
        handle.abort();
    }

    /// **RFC-0038 §5, end to end: a top-level call is decoded from ordinary RPC.**
    ///
    /// This is what a subgraph's `callHandlers` fire on. `[extract] traces` bundled it with the
    /// *internal* call tree behind the node gate, which was RFC-0036's bundling-by-shape error a
    /// second time: internal calls genuinely need `debug_*`, but a top-level call is a transaction and
    /// `eth_getBlockByNumber(b, true)` returns it.
    ///
    /// Asserts three things a weaker test would miss: the call row exists, a transaction to an
    /// address this nest does **not** index is ignored, and the row's key sits in the reserved band so
    /// it cannot collide with a log at the same ordinal - the gap `CallContext::call_index` recorded
    /// and #642 proved was live for block rows.
    ///
    /// **No `[extract] contracts` scope, deliberately.** With one set, `CallRegistry::in_scope` does
    /// the filtering and this test passes with the indexer's own address filter deleted - it would be
    /// asserting somebody else's guard. Unscoped, the nest's own addresses are the only bound, which
    /// is the one that matters: `scope_check` guards `traces`/`state` and returns early for a
    /// top-level-calls nest, so without that filter an unscoped nest would decode every transaction
    /// on the chain.
    #[tokio::test]
    async fn a_top_level_call_is_decoded_without_a_node() {
        struct BodySource;
        #[async_trait::async_trait]
        impl Source for BodySource {
            async fn tip(&self) -> Result<u64> {
                Ok(100)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _f: &crate::source::LogFilter,
                _from: u64,
                _to: u64,
            ) -> Result<Vec<crate::rpc::Log>> {
                Ok(Vec::new())
            }
            async fn block_bodies(
                &self,
                blocks: &[u64],
            ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
                Ok(blocks
                    .iter()
                    .map(|&b| {
                        (
                            b,
                            serde_json::json!({
                                "hash": format!("0x{b:064x}"),
                                "timestamp": "0x65000000",
                                "transactions": [
                                    // A call to the contract this nest indexes: `ping()`.
                                    {
                                        "hash": "0xaa",
                                        "to": "0x1111111111111111111111111111111111111111",
                                        "input": "0x5c36b186",
                                        "transactionIndex": "0x0"
                                    },
                                    // A call to some other contract entirely. Decoding this would
                                    // mean the nest pays for chain traffic it never asked for.
                                    {
                                        "hash": "0xbb",
                                        "to": "0x9999999999999999999999999999999999999999",
                                        "input": "0x5c36b186",
                                        "transactionIndex": "0x1"
                                    },
                                    // A contract creation has no `to` at all.
                                    {
                                        "hash": "0xcc",
                                        "input": "0x6080",
                                        "transactionIndex": "0x2"
                                    },
                                ],
                            }),
                        )
                    })
                    .collect())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"tc\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"tok\"\naddress = \"0x1111111111111111111111111111111111111111\"\n\
             abi = \"abis/tok.json\"\n\n\
             [extract]\ntop_level_calls = true\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("abis/tok.json"),
            r#"[{"type":"function","name":"ping","inputs":[],"outputs":[],"stateMutability":"nonpayable"},
                {"type":"event","name":"Ping","inputs":[],"anonymous":false}]"#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert!(
            !config.extract.enabled(),
            "top_level_calls must NOT be node-gated, or this nest is refused at startup"
        );

        let source: Arc<dyn Source> = Arc::new(BodySource);
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a top-level-calls nest must build with no node");
        if let Some(w) = worker {
            w.abort();
        }

        nest.process_window(source.as_ref(), &[], 4, 4, 100)
            .await
            .unwrap()
            .expect("the window must commit");
        drop(nest);
        drop(state);

        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let keys = store.entity_keys().unwrap();
        assert_eq!(
            keys.len(),
            1,
            "only the call to *our* contract is decoded - the foreign call and the creation are not: \
             {keys:?}"
        );
        let ordinal: u64 = keys[0].rsplit('-').next().unwrap().parse().unwrap();
        assert_eq!(
            ordinal,
            crate::registry::TX_CALL_ROW_LOG_INDEX_BASE,
            "transaction 0 must land in the reserved band, not at log_index 0 where a log lives"
        );
        let row: serde_json::Value =
            serde_json::from_str(&store.get_entity(&keys[0]).unwrap().unwrap()).unwrap();
        // `call_` prefixed, so a function and an event of the same name cannot collide in one nest -
        // this ABI has both a `ping()` function and a `Ping` event, which is why the fixture carries
        // both.
        assert_eq!(row["table"], "tok__call_ping", "row: {row}");
        assert_eq!(
            row["tx_hash"], "0xaa",
            "the row must carry the transaction it came from"
        );
        assert_eq!(
            row["block_timestamp"], 1_694_498_816u64,
            "the timestamp comes from the body we already fetched, not a second round trip"
        );
    }

    /// **RFC-0038 §3, end to end: a declaration names an event's parameter.**
    ///
    /// This is the claim the whole parity argument rests on. A subgraph mapping reads
    /// `c.balanceOf(event.params.to)`; before this, a nest could only declare a fixed calldata
    /// sampled every N blocks, which expresses an oracle read and not a mapping.
    ///
    /// Two transfers in one block to two different recipients must produce **two** calls with
    /// **different arguments** and **distinct keys** - not one, and not two rows overwriting each
    /// other, which is exactly how #642 went wrong for block rows.
    #[tokio::test]
    async fn a_row_driven_call_fires_once_per_event_with_the_events_own_argument() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = stub_state_rpc(seen.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"tok\"\naddress = \"0x1111111111111111111111111111111111111111\"\n\
             abi = \"abis/tok.json\"\n\n\
             [[calls]]\nname = \"recipient_balance\"\n\
             contract = \"0x1111111111111111111111111111111111111111\"\n\
             on = \"tok__transfer\"\nsignature = \"balanceOf(address)\"\nargs = [\"{to}\"]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#,
        )
        .unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.state_rpc_urls = vec![url];

        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (mut nest, state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a row-driven calls nest must build");
        if let Some(w) = worker {
            w.abort();
        }

        let topic0 = format!(
            "0x{}",
            hex::encode(
                nest.registry
                    .tables()
                    .iter()
                    .find(|d| d.table == "tok__transfer")
                    .expect("fixture must expose tok__transfer")
                    .topic0
            )
        );
        let pad = |b: u8| format!("0x{:064x}", b);
        let logs: Vec<crate::rpc::Log> = (1u8..=2)
            .map(|i| crate::rpc::Log {
                address: "0x1111111111111111111111111111111111111111".into(),
                topics: vec![topic0.clone(), pad(0xaa), pad(0xb0 + i)],
                data: format!("0x{:064x}", 1000 * i as u64),
                block_number: 5,
                block_hash: format!("0x{:064x}", 5),
                tx_hash: format!("0xt{i}"),
                log_index: u64::from(i),
            })
            .collect();

        nest.process_window(source.as_ref(), &logs, 5, 5, 100)
            .await
            .unwrap()
            .expect("the window must commit");
        drop(nest);
        drop(state);

        let asked = seen.lock().unwrap().clone();
        assert_eq!(
            asked.len(),
            2,
            "two transfers to two recipients owe two calls, got {asked:?}"
        );
        for (i, a) in asked.iter().enumerate() {
            assert!(
                a.starts_with("0x5|"),
                "pinned to the event's own block: {a}"
            );
            // `balanceOf(address)` is selector 0x70a08231 - a published value, not ours.
            assert!(a.contains("|0x70a08231"), "calldata must be balanceOf: {a}");
            assert!(
                a.ends_with(&format!("{:0>64x}", 0xb1 + i as u32)),
                "the argument must be *this* event's recipient: {a}"
            );
        }
        assert_ne!(asked[0], asked[1], "two recipients, two distinct questions");

        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let mut call_keys: Vec<String> = store
            .entity_keys()
            .unwrap()
            .into_iter()
            .filter(|k| {
                k.rsplit('-')
                    .next()
                    .and_then(|i| i.parse::<u64>().ok())
                    .is_some_and(|i| i >= crate::registry::CALL_ROW_LOG_INDEX_BASE)
            })
            .collect();
        call_keys.sort();
        assert_eq!(
            call_keys.len(),
            2,
            "two results must land under two keys, not overwrite each other (#642's lesson): \
             {call_keys:?}"
        );
        handle.abort();
    }

    /// A declared call with no archive endpoint is refused at startup, not discovered thousands of
    /// blocks into a backfill as a wall of identical failures.
    #[tokio::test]
    async fn calls_without_a_state_rpc_are_refused_at_startup() {
        let dir = tempfile::tempdir().unwrap();
        write_calls_nest(dir.path());
        let config = Config::load(dir.path()).unwrap();
        assert!(
            config.state_rpc_urls.is_empty(),
            "the endpoint must never come from nuthatch.toml - it would enter the content address"
        );
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let err = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await;
        // `unwrap_err` would need `Debug` on the Ok type, and `NestIngest`/`AppState` do not carry it.
        let err = match err {
            Ok(_) => panic!("a calls nest with no --state-rpc must refuse"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("--state-rpc"), "name the flag: {err}");
        assert!(
            err.contains("recipe add") || err.contains("metadata fetch"),
            "a refusal should name the cheaper thing to do instead: {err}"
        );
    }

    /// **#642: a block row must not destroy the log at index 0 in its block.**
    ///
    /// `Store::entity_key` is `(block, log_index)` and assumes every row descends from a log. Block
    /// rows (RFC-0036) descend from none, and were written with `log_index: 0` - indistinguishable
    /// from the first log in the block. In `process_window` the log rows are pushed to `to_store`
    /// first and the block rows second, and a repeated key in redb is an overwrite, so the block row
    /// won and the log was gone. Silently: no warning, no error, no gap in the cursor.
    ///
    /// Fixed by [`crate::registry::BLOCK_ROW_LOG_INDEX`], which reserves the top of the index range
    /// for rows that descend from no log. This is the test that found it.
    ///
    /// Driven through the real `process_window` because the seal-direct path buffers `(block, json)`
    /// into append-only Parquet and cannot collide - testing that path proves nothing about this one,
    /// and it passed while this bug was live.
    #[tokio::test]
    async fn a_block_row_does_not_overwrite_the_log_at_index_zero() {
        struct HeadersAndOneLog;
        #[async_trait::async_trait]
        impl Source for HeadersAndOneLog {
            async fn tip(&self) -> Result<u64> {
                Ok(100)
            }
            async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
                Ok(None)
            }
            async fn logs(
                &self,
                _filter: &crate::source::LogFilter,
                _from: u64,
                _to: u64,
            ) -> Result<Vec<crate::rpc::Log>> {
                Ok(Vec::new())
            }
            async fn block_headers(
                &self,
                blocks: &[u64],
            ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
                Ok(blocks
                    .iter()
                    .map(|&b| {
                        (
                            b,
                            serde_json::json!({
                                "hash": format!("0x{b:064x}"),
                                "parentHash": format!("0x{:064x}", b.saturating_sub(1)),
                                "miner": "0x0000000000000000000000000000000000000000",
                                "gasUsed": "0x0",
                                "gasLimit": "0x1388",
                                "size": "0x220",
                                "timestamp": format!("0x{:x}", 1_700_000_000 + b),
                                "transactions": [],
                            }),
                        )
                    })
                    .collect())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mut nest = build_blocks_and_contract_nest(dir.path()).await;
        let topic0 = format!(
            "0x{}",
            hex::encode(
                nest.registry
                    .tables()
                    .iter()
                    .find(|d| d.table == "tok__ping")
                    .expect("fixture must expose tok__ping")
                    .topic0
            )
        );
        let src = HeadersAndOneLog;
        let logs = vec![crate::rpc::Log {
            address: "0x1111111111111111111111111111111111111111".into(),
            topics: vec![topic0],
            data: "0x".into(),
            block_number: 2,
            block_hash: format!("0x{:064x}", 2),
            tx_hash: "0xdeadbeef".into(),
            log_index: 0,
        }];

        nest.process_window(&src, &logs, 1, 3, 100)
            .await
            .unwrap()
            .expect("the window must commit");

        // Drop the nest first: redb takes its exclusive flock at `Database::open`, so a second
        // handle is refused while this one is alive.
        drop(nest);
        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();
        let keys = store.entity_keys().unwrap();

        assert_eq!(
            keys.len(),
            4,
            "blocks 1-3 plus one Ping at block 2 log_index 0 owe four rows; three means the block \
             row overwrote the log again (#642). Keys: {keys:?}"
        );
    }

    /// #429: a nest with **no contract at all** must not issue a single `getLogs`, on **any** backfill
    /// path.
    ///
    /// An empty address *and* topic filter is not "no logs" to a node, it is *every log on the chain* -
    /// so a blocks-only nest (OBIB case 3) asked for the lot, per window, and discarded all of it. The
    /// pipelined path then made it worse: an over-cap response is *split and retried*, so the mistake
    /// amplified into a fan-out.
    ///
    /// Parameterised over both seal-direct paths deliberately. The guard was first written inline in
    /// `backfill_direct` and the pipelined path - the one production actually takes for a static nest -
    /// was missed, precisely because one path was tested and the other was not.
    #[tokio::test]
    async fn a_contract_free_nest_issues_no_getlogs_on_any_backfill_path() {
        let blocks_only = DecodeRegistry::build(Vec::new()).unwrap().with_blocks(true);

        // Sequential.
        let seq_src = LogCountingSource::new();
        let d_seq = tempfile::tempdir().unwrap();
        let n_seq = backfill_direct(
            &seq_src,
            &blocks_only,
            d_seq.path(),
            &[],
            &[],
            &[],
            None,
            0,
            1,
            20,
            5,
            true,
        )
        .await
        .unwrap();

        // Pipelined - the path `nuthatch dev` takes for a static nest.
        let pipe_src = LogCountingSource::new();
        let d_pipe = tempfile::tempdir().unwrap();
        let n_pipe = backfill_direct_pipelined(
            &pipe_src,
            &blocks_only,
            d_pipe.path(),
            &[],
            &[],
            &[],
            None,
            0,
            1,
            20,
            5,
            4,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        for (path, src, rows) in [
            ("backfill_direct", &seq_src, n_seq),
            ("backfill_direct_pipelined", &pipe_src, n_pipe),
        ] {
            assert_eq!(
                src.calls(),
                0,
                "{path}: a nest with no address and no topic filter must not ask for logs at all - \
                 an unfiltered getLogs is every log on the chain"
            );
            // Not an inert backfill: it still did its actual job, one row per block.
            assert_eq!(
                rows, 20,
                "{path}: the blocks table must still cover every block in the range"
            );
        }

        // Control. With a contract in the nest the same paths *do* fetch logs - without this the
        // assertions above would pass just as well against a backfill that fetches nothing ever.
        let with_contract = transfer_registry();
        let addr = ["0x1111111111111111111111111111111111111111".to_string()];

        let ctl_seq = LogCountingSource::new();
        let d_ctl_seq = tempfile::tempdir().unwrap();
        backfill_direct(
            &ctl_seq,
            &with_contract,
            d_ctl_seq.path(),
            &addr,
            &[],
            &[],
            None,
            0,
            1,
            20,
            5,
            true,
        )
        .await
        .unwrap();

        let ctl_pipe = LogCountingSource::new();
        let d_ctl_pipe = tempfile::tempdir().unwrap();
        backfill_direct_pipelined(
            &ctl_pipe,
            &with_contract,
            d_ctl_pipe.path(),
            &addr,
            &[],
            &[],
            None,
            0,
            1,
            20,
            5,
            4,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        assert!(
            ctl_seq.calls() > 0,
            "control: backfill_direct must fetch logs when the nest has a contract"
        );
        assert!(
            ctl_pipe.calls() > 0,
            "control: backfill_direct_pipelined must fetch logs when the nest has a contract"
        );
    }

    /// A `Source` with a deep backlog that records the span of every `getLogs` it is asked for, so a
    /// test can see how wide the window controller grew. Returns no logs, which is what makes the
    /// controller grow: `observed(0)` is its "nothing there, cover more ground" signal.
    struct WindowRecordingSource {
        spans: std::sync::Mutex<Vec<u64>>,
    }

    impl WindowRecordingSource {
        fn new() -> WindowRecordingSource {
            WindowRecordingSource {
                spans: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn widest(&self) -> u64 {
            self.spans
                .lock()
                .unwrap()
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
        }
        fn count(&self) -> usize {
            self.spans.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl Source for WindowRecordingSource {
        async fn tip(&self) -> Result<u64> {
            // Large enough that a controller growing unclamped toward `MAX_WINDOW` (100,000) across
            // a couple of dozen 4x steps still has room before `global_next` catches it - the
            // retirement test needs headroom past the point an uncapped controller would have
            // saturated, not just past its own `--backfill` starting gap.
            Ok(10_000_000)
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            self.spans.lock().unwrap().push(to - from + 1);
            Ok(Vec::new())
        }
        async fn block_headers(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
            Ok(blocks
                .iter()
                .map(|&b| {
                    (
                        b,
                        serde_json::json!({
                            "hash": format!("0x{b:064x}"),
                            "parentHash": format!("0x{:064x}", b.saturating_sub(1)),
                            "miner": "0x0000000000000000000000000000000000000000",
                            "gasUsed": "0x0",
                            "gasLimit": "0x1388",
                            "size": "0x220",
                            "timestamp": format!("0x{:x}", 1_700_000_000 + b),
                            "transactions": [],
                        }),
                    )
                })
                .collect())
        }
    }

    /// A nest with a contract *and* `[extract] blocks = true`: one header request per block, so its
    /// window ceiling is header cost rather than log density (RFC-0036).
    /// The catalogue must list every table the nest serves, not only the ones a decoder produces.
    ///
    /// This is the regression guard for a defect measured on two live nests: `[[calls]]` and
    /// `[[ipfs]]` rows were queryable through `/sql` and absent from `/tables`, `/schema` and
    /// `/table/{name}` - so the point-read endpoint answered 404 for a table holding 3,509 rows, and
    /// the AI-native schema surface told an agent it did not exist. The `semantic.toml` drift check
    /// read from the same short list and warned that a correctly-described table "has no decoder".
    ///
    /// Asserted on the *composed* list rather than through the HTTP surface deliberately: all three
    /// symptoms had one cause, and a test per endpoint would pass again the moment a fourth consumer
    /// of the catalogue is added without being told about it.
    #[test]
    fn the_catalogue_lists_call_and_ipfs_tables_not_just_decoder_tables() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"uri","type":"string","indexed":false}],"anonymous":false}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"n\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [[contracts]]\nalias = \"tok\"\naddress = \"0x0000000000000000000000000000000000000001\"\n\
             abi = \"abis/tok.json\"\n\n\
             [[calls]]\nname = \"total_supply\"\n\
             contract = \"0x0000000000000000000000000000000000000002\"\ncalldata = \"0x18160ddd\"\nevery = 1000\n\n\
             [[ipfs]]\nname = \"token_metadata\"\non = \"tok__transfer\"\ncid_column = \"uri\"\n",
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.calls.len(), 1, "fixture must declare a call");
        assert_eq!(
            config.ipfs.len(),
            1,
            "fixture must declare an ipfs document"
        );
        let registry = crate::registry::from_nest(dir.path(), &config).unwrap();

        let decoder_only: Vec<String> = registry.schema().iter().map(|t| t.table.clone()).collect();
        let served: Vec<String> = full_schema(&registry, &config)
            .iter()
            .map(|t| t.table.clone())
            .collect();

        // The premise: these two lists must actually differ, or the assertions below prove nothing.
        assert!(
            !decoder_only.contains(&"total_supply".to_string())
                && !decoder_only.contains(&"token_metadata".to_string()),
            "the registry alone should not know these tables: {decoder_only:?}"
        );
        for want in ["tok__transfer", "total_supply", "token_metadata"] {
            assert!(
                served.contains(&want.to_string()),
                "`{want}` must be in the served catalogue, got {served:?}"
            );
        }
    }

    async fn build_blocks_nest_with_contract(dir: &std::path::Path, addr: &str) -> NestIngest {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            format!(
                "[nest]\nname = \"n\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"tok\"\naddress = \"{addr}\"\nabi = \"abis/tok.json\"\n\n\
                 [extract]\nblocks = true\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#,
        )
        .unwrap();
        let config = Config::load(dir).unwrap();
        assert!(config.extract.blocks, "the fixture must be a blocks nest");
        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, worker, _w) = build_nest(
            &source,
            dir.to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .unwrap();
        if let Some(w) = worker {
            w.abort();
        }
        nest
    }

    /// Drive `index_loop` over a deep backlog and report the widest `getLogs` span it asked for.
    async fn widest_window_over_a_backlog(nest: NestIngest) -> u64 {
        let src = Arc::new(WindowRecordingSource::new());
        let recorder = src.clone();
        let task = tokio::spawn(index_loop(
            src as Arc<dyn Source>,
            nest,
            // 20,000 blocks behind a 1,000,000 tip: a real backlog, so the window controller has
            // room to grow rather than being clipped by `.min(tip)` after one step.
            Some(20_000),
            false,
            1,
            5,
        ));
        // Six windows is past the point where 4x growth from a seed of 5 clears the header ceiling
        // (5, 20, 80, 320, 1280, ...), so an uncapped controller has visibly exceeded it by here.
        let grew = within_deadline(|| recorder.count() >= 6).await;
        task.abort();
        assert!(grew, "the loop must have asked for at least six windows");
        recorder.widest()
    }

    /// The same, through `runtime_index_loop`. Worth driving separately rather than trusting the
    /// solo loop's result: the two apply the ceiling by different mechanisms. A cursor's nest set
    /// changes under it, so the runtime bounds each window as it is used rather than picking a
    /// controller once, and that is the half a reviewer should be able to see fail.
    async fn widest_runtime_window_over_a_backlog(nest: NestIngest) -> u64 {
        let src = Arc::new(WindowRecordingSource::new());
        let recorder = src.clone();
        let task = tokio::spawn(runtime_index_loop(
            src as Arc<dyn Source>,
            vec![nest],
            Some(20_000),
            false,
            1,
            5,
            Arc::new(crate::health::RuntimeHealth::new()),
            false,
            None,
        ));
        let grew = within_deadline(|| recorder.count() >= 6).await;
        task.abort();
        assert!(grew, "the cursor must have asked for at least six windows");
        recorder.widest()
    }

    /// RFC-0036's window ceiling applies to the tip loop, not only to the three backfill paths.
    ///
    /// This is the other half of the #432 fix, and it is only *reachable* because of it. While a
    /// contract-free nest was fetching every log on the chain, the enormous result count shrank the
    /// window and hid the omission. Fetching nothing feeds `observed(0)` instead, which grows the
    /// window 4x per step to `MAX_WINDOW` (100,000) - so fixing the filter alone would trade a
    /// getLogs pathology for the header fan-out pathology RFC-0036 exists to prevent: one window
    /// demanding a hundred thousand `eth_getBlockByNumber` calls, which is how OBIB case 3
    /// rate-limited itself into partial responses.
    ///
    /// The control is the point of the test. A blocks nest capped at `HEADER_WINDOW_CAP` proves
    /// nothing on its own - a loop whose window never grew would pass it too - so the same source and
    /// the same backlog are driven with a non-blocks nest, which must grow *past* the ceiling.
    #[tokio::test]
    async fn the_tip_loop_caps_a_blocks_nest_window_at_the_header_ceiling() {
        let addr = "0x1111111111111111111111111111111111111111";

        let d_blocks = tempfile::tempdir().unwrap();
        let blocks_nest = build_blocks_nest_with_contract(d_blocks.path(), addr).await;
        let blocks_widest = widest_window_over_a_backlog(blocks_nest).await;

        let d_plain = tempfile::tempdir().unwrap();
        let plain_nest = build_test_nest(d_plain.path(), addr).await;
        let plain_widest = widest_window_over_a_backlog(plain_nest).await;

        let d_rt_blocks = tempfile::tempdir().unwrap();
        let rt_blocks_nest = build_blocks_nest_with_contract(d_rt_blocks.path(), addr).await;
        let rt_blocks_widest = widest_runtime_window_over_a_backlog(rt_blocks_nest).await;

        let d_rt_plain = tempfile::tempdir().unwrap();
        let rt_plain_nest = build_test_nest(d_rt_plain.path(), addr).await;
        let rt_plain_widest = widest_runtime_window_over_a_backlog(rt_plain_nest).await;

        // The request is the window plus the refetched tail (#1144): the tail buys no headers,
        // since headers are fetched for the window `next..=to` and never for the overlap.
        assert!(
            blocks_widest <= crate::chunker::HEADER_WINDOW_CAP + FETCH_TAIL_OVERLAP,
            "a blocks nest pays one header request per block, so the tip loop must not grow its \
             window past {} - grew to {blocks_widest}",
            crate::chunker::HEADER_WINDOW_CAP
        );
        assert!(
            plain_widest > crate::chunker::HEADER_WINDOW_CAP,
            "control: the same loop on a non-blocks nest must grow past {} - only got to \
             {plain_widest}, so the assertion above is about a window that never grew",
            crate::chunker::HEADER_WINDOW_CAP
        );
        assert!(
            rt_blocks_widest <= crate::chunker::HEADER_WINDOW_CAP + FETCH_TAIL_OVERLAP,
            "the runtime cursor must cap a live blocks nest's window at {} - grew to \
             {rt_blocks_widest}",
            crate::chunker::HEADER_WINDOW_CAP
        );
        assert!(
            rt_plain_widest > crate::chunker::HEADER_WINDOW_CAP,
            "control: the runtime cursor on a non-blocks nest must grow past {} - only got to \
             {rt_plain_widest}",
            crate::chunker::HEADER_WINDOW_CAP
        );
    }

    /// #458: the previous test proves the *live* half of the header cap - it never retires a nest,
    /// so a controller that is capped only at the use site (leaving its own `window` free to drift
    /// toward `MAX_WINDOW` unseen) passes it too. This drives a runtime cursor with a blocks nest
    /// co-mounted with a log-shaped nest well past the point an uncapped controller would have
    /// saturated at `MAX_WINDOW`, retires the blocks nest mid-run, and asserts the windows issued
    /// *after* that point climb back up gradually rather than jumping straight to the drifted value.
    #[tokio::test]
    async fn the_tip_loop_bounds_the_controller_through_a_blocks_nest_retirement() {
        let addr = "0x1111111111111111111111111111111111111111";
        let d_blocks = tempfile::tempdir().unwrap();
        let mut blocks_nest = build_blocks_nest_with_contract(d_blocks.path(), addr).await;
        blocks_nest.name = "blocks".to_string();

        let d_plain = tempfile::tempdir().unwrap();
        let mut plain_nest =
            build_test_nest(d_plain.path(), "0x2222222222222222222222222222222222222222").await;
        plain_nest.name = "plain".to_string();

        let src = Arc::new(WindowRecordingSource::new());
        let recorder = src.clone();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // A generous `--backfill` gap (not the default-sized one the mount-late test uses): a
        // controller that has silently drifted toward `MAX_WINDOW` while capped needs room past
        // that drift to prove it does NOT resume from there in one step, and 20,000 blocks is only
        // enough to observe the pre-retirement cap, not the recovery past it.
        let task = tokio::spawn(runtime_index_loop(
            src as Arc<dyn Source>,
            vec![blocks_nest, plain_nest],
            Some(2_000_000),
            false,
            1,
            5,
            Arc::new(crate::health::RuntimeHealth::new()),
            false,
            Some(rx),
        ));

        // Growth from a seed of 5 under `observed(0)` is 5, 20, 80, 320, 1280, 5120, 20480, 81920,
        // 100000(clamped)... - so an uncapped controller has already saturated at `MAX_WINDOW` well
        // before twelve windows, while every span *issued* stays <= `HEADER_WINDOW_CAP` regardless,
        // because the blocks nest is live throughout. The bug is invisible until retirement.
        let drove = within_deadline(|| recorder.count() >= 12).await;
        assert!(
            drove,
            "the cursor must have asked for at least twelve windows before retiring"
        );
        let before_retire = recorder.count();

        tx.send(CursorCommand::unmount("blocks")).unwrap();

        // One more window is enough to tell the two designs apart: bounding only the *use* leaves
        // `window` free to have drifted toward `MAX_WINDOW` unseen while capped, so the very first
        // post-retirement span jumps straight there. Bounding the controller keeps `window` clamped
        // down live, so that same span is only the ordinary next 4x step up from the cap.
        let advanced = within_deadline(|| recorder.count() > before_retire).await;
        task.abort();
        assert!(
            advanced,
            "the cursor must keep advancing after the blocks nest retires"
        );

        let first_post_retire = recorder.spans.lock().unwrap()[before_retire];
        assert!(
            first_post_retire <= 4 * crate::chunker::HEADER_WINDOW_CAP,
            "the first window after the blocks nest retires must be the controller's ordinary next \
             4x step up from {} (<= {}), not a jump to wherever it drifted to unseen while capped - \
             got {first_post_retire}",
            crate::chunker::HEADER_WINDOW_CAP,
            4 * crate::chunker::HEADER_WINDOW_CAP
        );
    }

    /// Wait for `f` to hold, or give up after 20s. The tip loops never return, so every assertion
    /// about them is really an assertion about what has happened *by* some point.
    async fn within_deadline(mut f: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if f() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        f()
    }

    /// `index_loop` writes one block row per block for a tip window on a blocks nest, including
    /// windows where no log matched (#447). `LogCountingSource` tips at 100; `--backfill 10` starts
    /// the cursor at block 90, so the loop must store rows for 90..=100 without issuing a single
    /// `getLogs` (no address filter → every log on the chain, #432).
    #[tokio::test]
    async fn the_tip_loop_writes_block_rows_for_every_block_in_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let mut nest =
            build_test_nest(dir.path(), "0x1111111111111111111111111111111111111111").await;
        nest.registry = Arc::new(DecodeRegistry::build(Vec::new()).unwrap().with_blocks(true));
        nest.addresses = Vec::new();
        nest.topic0s = Vec::new();
        assert!(
            LogFilter::new(&nest.addresses, &nest.topic0s).is_none(),
            "the fixture must be in the state where no getLogs can be issued at all"
        );
        let store = nest.store.clone();

        let src = Arc::new(LogCountingSource::new());
        let counter = src.clone();
        let task = tokio::spawn(index_loop(
            src as Arc<dyn Source>,
            nest,
            Some(10),
            false,
            1,
            5,
        ));
        // Wait until at least one block row lands in the hot store.
        let wrote_rows =
            within_deadline(|| !store.entities_in_range(90, 100).unwrap().is_empty()).await;
        task.abort();

        assert!(
            wrote_rows,
            "the tip loop must write block rows for the window - #447 regressed"
        );
        assert_eq!(
            counter.calls(),
            0,
            "must reach tip without a single getLogs: an empty address AND topic filter is every \
             log on the chain (#432)"
        );
    }

    /// Same as above, through `runtime_index_loop` (#447 acceptance criterion 2): the runtime cursor
    /// writes block rows for the blocks nests on it.
    #[tokio::test]
    async fn the_runtime_tip_loop_writes_block_rows_for_every_block_in_the_window() {
        let d_blocks = tempfile::tempdir().unwrap();
        let blocks_nest = build_blocks_nest_with_contract(
            d_blocks.path(),
            "0x1111111111111111111111111111111111111111",
        )
        .await;
        let blocks_store = blocks_nest.store.clone();

        let src = Arc::new(LogCountingSource::new());
        let task = tokio::spawn(runtime_index_loop(
            src as Arc<dyn Source>,
            vec![blocks_nest],
            Some(10),
            false,
            1,
            5,
            Arc::new(crate::health::RuntimeHealth::new()),
            false,
            None,
        ));
        let wrote_rows =
            within_deadline(|| !blocks_store.entities_in_range(0, 100).unwrap().is_empty()).await;
        task.abort();

        assert!(
            wrote_rows,
            "runtime_index_loop must write block rows for a blocks nest - #447 regressed"
        );
    }

    /// The replacement `a_contract_free_nest_cannot_be_built_at_all_today` asked for by name: #445 is
    /// fixed, so a contract-free nest (`[extract] blocks = true`, no `[[contracts]]`) now builds and
    /// reaches the tip loop, and this drives the loop with a real one.
    ///
    /// The distinction that matters is where the no-`getLogs` state comes from. Its sibling
    /// (`the_tip_loop_writes_no_block_rows_for_a_window_yet`) has to *manufacture* it - emptying
    /// `addresses` and `topic0s` on an already-built ERC20 nest - because until #445 no real config
    /// could produce it. Here nothing is forced: the operator declares a blocks nest and no
    /// contracts, `build_nest` accepts it, and the loop arrives at an unrepresentable filter on its
    /// own. That is the day #432's empty-filter guard stops being unreachable defence and starts
    /// carrying weight, which is exactly what the retired test said to check for.
    #[tokio::test]
    async fn a_contract_free_nest_reaches_the_tip_loop_and_asks_for_no_logs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"b\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
             [extract]\nblocks = true\n",
        )
        .unwrap();
        let config = Config::load(dir.path()).unwrap();
        assert!(
            config.contracts.is_empty() && config.extract.blocks,
            "the fixture is the contract-free blocks nest, not a stand-in"
        );

        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        // `match` rather than `unwrap`: neither `NestIngest` nor `AppState` is `Debug`.
        let (nest, state, worker, _w) = match build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        {
            Ok(built) => built,
            Err(e) => panic!("a contract-free nest must build (#445): {e:#}"),
        };
        if let Some(w) = worker {
            w.abort();
        }
        // The nest has no single contract to name, and the summary says so rather than inventing one.
        assert_eq!(
            state.address, None,
            "a nest with no contracts names no address"
        );
        // Unforced, straight out of `build_nest`: no address filter and no topic filter, so there is
        // no `getLogs` this nest could legally issue.
        assert!(
            LogFilter::new(&nest.addresses, &nest.topic0s).is_none(),
            "a contract-free nest must reach the loop with an unrepresentable filter, without a \
             test having to empty it by hand"
        );

        let src = Arc::new(LogCountingSource::new());
        let counter = src.clone();
        let task = tokio::spawn(index_loop(
            src as Arc<dyn Source>,
            nest,
            Some(10),
            false,
            1,
            5,
        ));
        // Give the loop the same deadline the other loop tests use. There is nothing to wait *for* -
        // the point is that a window passes without a log request - so wait for the cursor to move.
        let _ = within_deadline(|| counter.calls() > 0).await;
        task.abort();

        assert_eq!(
            counter.calls(),
            0,
            "whatever the loop does with the window, it must get there without asking for logs at \
             all: an empty address AND topic filter is every log on the chain (#432)"
        );
    }

    /// A source that counts tip polls as well as log fetches, so "the loop never asked for logs" can
    /// be told apart from "the loop never ran".
    struct TipAndLogCountingSource {
        tip_calls: std::sync::atomic::AtomicUsize,
        log_calls: std::sync::atomic::AtomicUsize,
    }

    impl TipAndLogCountingSource {
        fn new() -> Self {
            Self {
                tip_calls: std::sync::atomic::AtomicUsize::new(0),
                log_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn tips(&self) -> usize {
            self.tip_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
        fn logs_asked(&self) -> usize {
            self.log_calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Source for TipAndLogCountingSource {
        async fn tip(&self) -> Result<u64> {
            self.tip_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(100)
        }
        async fn block_hash(&self, n: u64) -> Result<Option<String>> {
            Ok(Some(format!("0x{n:064x}")))
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            _from: u64,
            _to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            self.log_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Vec::new())
        }
        async fn block_headers(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, serde_json::Value>> {
            Ok(blocks
                .iter()
                .map(|&b| {
                    (
                        b,
                        serde_json::json!({
                            "hash": format!("0x{b:064x}"),
                            "parentHash": format!("0x{:064x}", b.saturating_sub(1)),
                            "miner": "0x0000000000000000000000000000000000000000",
                            "timestamp": format!("0x{:x}", 1_700_000_000u64 + b),
                        }),
                    )
                })
                .collect())
        }
    }

    /// The tip loops' empty filter is **reachable today** - just not by the route #432 assumed.
    ///
    /// `a_contract_free_nest_cannot_be_built_at_all_today` pins that `build_nest` refuses a nest with
    /// no `[[contracts]]`, and that is right. The conclusion drawn from it - that the empty-filter case
    /// therefore cannot be driven end-to-end, so a type-level test is the honest coverage available -
    /// is not. `registry.addresses()` derives its address list from registered event **decoders**, not
    /// from `config.contracts`, so an ABI that declares no events leaves *both* halves of the filter
    /// empty while `config.primary()` still succeeds. The nest builds, starts, and reaches the loop.
    ///
    /// This shape is not contrived. It is the proxy trap `report_abi_fit` already warns about by name
    /// ("the resolved ABI declares no events at all", "the usual cause is a proxy"), and it warns
    /// rather than refuses - so the nest indexes. Before #432 this configuration asked a public
    /// endpoint for every log on the chain, every couple of seconds, for as long as `nuthatch dev`
    /// ran. This is the end-to-end wiring test, with a real `build_nest` nest and no faked fixture.
    #[tokio::test]
    async fn an_event_free_abi_reaches_the_tip_loop_with_an_empty_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            "[nest]\nname = \"proxyish\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
             rpc_urls = []\n\n[[contracts]]\nalias = \"p\"\n\
             address = \"0x1111111111111111111111111111111111111111\"\nabi = \"abis/p.json\"\n",
        )
        .unwrap();
        // A proxy's public ABI: functions, no events. Perfectly valid JSON ABI, and the resolvers
        // return exactly this for a proxied contract.
        std::fs::write(
            dir.path().join("abis/p.json"),
            r#"[{"type":"function","name":"implementation","inputs":[],"outputs":[{"name":"","type":"address"}],"stateMutability":"view"}]"#,
        )
        .unwrap();

        let config = Config::load(dir.path()).unwrap();
        assert!(
            !config.contracts.is_empty(),
            "the fixture has a contract - this is not the #445 shape"
        );

        let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
        let (nest, _state, worker, _w) = build_nest(
            &source,
            dir.path().to_path_buf(),
            &config,
            None,
            false,
            None,
            None,
            serve::new_sql_gate(),
        )
        .await
        .expect("a nest with a contract whose ABI has no events builds - primary() is satisfied");
        if let Some(w) = worker {
            w.abort();
        }

        // The point: a built, runnable nest whose getLogs filter is empty on both halves.
        assert!(
            nest.addresses.is_empty() && nest.topic0s.is_empty(),
            "expected both filter halves empty (addresses come from event decoders, not from \
             config.contracts) - got {} address(es) and {} topic0(s)",
            nest.addresses.len(),
            nest.topic0s.len()
        );

        let store = nest.store.clone();
        let src = Arc::new(TipAndLogCountingSource::new());
        let counter = src.clone();
        let task = tokio::spawn(index_loop(
            src as Arc<dyn Source>,
            nest,
            Some(0),
            false,
            1,
            5,
        ));
        // Liveness first, so `logs_asked() == 0` below cannot be satisfied by a loop that never ran.
        let alive = within_deadline(|| counter.tips() >= 3).await;
        let advanced = within_deadline(|| {
            store
                .get_meta(LAST_BLOCK_KEY)
                .ok()
                .flatten()
                .and_then(|v| v.parse::<u64>().ok())
                .is_some_and(|b| b >= 100)
        })
        .await;
        task.abort();

        assert!(
            alive,
            "the loop must have polled the tip at least three times"
        );
        assert_eq!(
            counter.logs_asked(),
            0,
            "a nest whose filter is empty on both halves must not issue getLogs at all - that \
             request is every log on the chain (#432)"
        );
        assert!(
            advanced,
            "the empty window must still be processed: the cursor has to reach the tip, or a \
             blocks-only nest never seals and never advances"
        );
    }

    /// #432 on the two paths its sibling test does not reach - the **live tip loops**.
    ///
    /// The empty-and-empty filter is now unrepresentable (`LogFilter::new` returns `None`), so what is
    /// left to prove about the loops is that the conversion did not break the case that *does* fetch:
    /// a real nest must still ask for its logs through both loops. Combined with
    /// `a_contract_free_nest_cannot_be_built_at_all_today`, which pins why the empty case cannot be
    /// driven end-to-end yet, and `the_empty_filter_is_unrepresentable`, which proves the guard
    /// itself, this is the honest coverage available today - a test that faked a contract-free
    /// `NestIngest` would prove the fixture, not the wiring.
    #[tokio::test]
    async fn both_tip_loops_still_fetch_logs_for_a_nest_with_contracts() {
        let d_solo = tempfile::tempdir().unwrap();
        let solo_nest =
            build_test_nest(d_solo.path(), "0x1111111111111111111111111111111111111111").await;
        let solo_src = Arc::new(LogCountingSource::new());
        let solo_counter = solo_src.clone();
        let solo_task = tokio::spawn(index_loop(
            solo_src as Arc<dyn Source>,
            solo_nest,
            Some(0),
            false,
            1,
            5,
        ));
        let solo_fetched = within_deadline(|| solo_counter.calls() > 0).await;
        solo_task.abort();
        assert!(
            solo_fetched,
            "index_loop must still fetch logs for a nest with a contract"
        );

        let d_rt = tempfile::tempdir().unwrap();
        let rt_nest =
            build_test_nest(d_rt.path(), "0x1111111111111111111111111111111111111111").await;
        let rt_src = Arc::new(LogCountingSource::new());
        let rt_counter = rt_src.clone();
        let rt_task = tokio::spawn(runtime_index_loop(
            rt_src as Arc<dyn Source>,
            vec![rt_nest],
            Some(0),
            false,
            1,
            5,
            Arc::new(crate::health::RuntimeHealth::new()),
            false,
            None,
        ));
        let rt_fetched = within_deadline(|| rt_counter.calls() > 0).await;
        rt_task.abort();
        assert!(
            rt_fetched,
            "runtime_index_loop must still fetch logs for a live nest with a contract"
        );
    }

    /// Every sealed segment's columns, as `name -> values` in row order, across all of a nest's
    /// tables. Reads the Parquet the way a consumer would, so a test comparing two runs is comparing
    /// what was actually written rather than what the manifest claims about it.
    fn sealed_columns(dir: &std::path::Path) -> std::collections::BTreeMap<String, Vec<String>> {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        let mut out: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        let m = seal::load_manifest(dir).unwrap();
        for segs in m.tables.values() {
            for seg in segs {
                let f = std::fs::File::open(dir.join(crate::seal::SEGMENTS_DIR).join(&seg.file))
                    .unwrap();
                let reader = ParquetRecordBatchReaderBuilder::try_new(f)
                    .unwrap()
                    .build()
                    .unwrap();
                for batch in reader {
                    let batch = batch.unwrap();
                    for (i, field) in batch.schema().fields().iter().enumerate() {
                        let col = batch.column(i);
                        let vals = out.entry(field.name().clone()).or_default();
                        for r in 0..col.len() {
                            vals.push(format!(
                                "{:?}",
                                arrow::util::display::array_value_to_string(col, r)
                            ));
                        }
                    }
                }
            }
        }
        out
    }

    /// The sealed *schema* differs by exactly one column - the column is **absent**, not null.
    ///
    /// A null would keep the schema stable and cost only bytes, which is why it is tempting; it also
    /// makes `ORDER BY block_timestamp` return an arbitrary order rather than an error, and a query
    /// that silently answers wrongly is worse than one that refuses. This asserts the choice.
    #[tokio::test]
    async fn the_timestamp_column_is_absent_from_sealed_rows_not_null() {
        let reg = transfer_registry().with_timestamps(false);
        let src = CountingSource::new(ping_logs(&reg, &[5, 6]));
        let dir = tempfile::tempdir().unwrap();
        backfill_direct(
            &src,
            &reg,
            dir.path(),
            &["0x1111111111111111111111111111111111111111".into()],
            &[],
            &[],
            None,
            0,
            5,
            6,
            100,
            true,
        )
        .await
        .unwrap();

        // What was actually sealed. The advertised schema is checked below, but this comes first:
        // the test is named for the sealed rows and must fail if they carry the column, whatever
        // `/tables` happens to claim.
        let sealed = sealed_columns(dir.path());
        assert!(
            !sealed.contains_key("block_timestamp"),
            "sealed rows must not carry the column: {:?}",
            sealed.keys().collect::<Vec<_>>()
        );
        assert!(
            sealed.contains_key("block_number"),
            "control: the segment must have sealed something"
        );

        // The schema the nest advertises must agree with what it seals - the two disagreeing is the
        // failure this whole slice is arranged to prevent.
        let advertised: Vec<String> = reg.schema()[0]
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert!(
            !advertised.contains(&"block_timestamp".to_string()),
            "a timestamp-free nest must not advertise the column: {advertised:?}"
        );
        assert!(
            advertised.contains(&"block_number".to_string()),
            "the other implicit columns are untouched: {advertised:?}"
        );

        let on = transfer_registry();
        let advertised_on: Vec<String> = on.schema()[0]
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert!(
            advertised_on.contains(&"block_timestamp".to_string()),
            "control: the default nest still advertises it"
        );
        assert_eq!(
            advertised_on.len(),
            advertised.len() + 1,
            "exactly one column differs"
        );
    }

    /// The declaration is `init`-time: a nest that has already indexed refuses to flip it.
    ///
    /// This is the guard that makes the whole design honest. Without it, `block_timestamps = false`
    /// pasted into an existing `nuthatch.toml` would leave one nest holding two schemas - segments
    /// written before the edit carrying the column, everything after not - and nothing would say so.
    #[test]
    fn flipping_the_timestamp_declaration_on_an_indexed_nest_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join(DB_FILE)).unwrap();

        // First start records what the nest is built with.
        guard_timestamp_policy(&store, true).unwrap();
        assert_eq!(
            store.get_meta(TIMESTAMPS_KEY).unwrap().as_deref(),
            Some("1")
        );
        // Restarting unchanged is fine, repeatedly.
        guard_timestamp_policy(&store, true).unwrap();

        let err = guard_timestamp_policy(&store, false)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("breaking schema change"),
            "the error must name what it is, not just refuse: {err}"
        );
        assert!(
            err.contains("init"),
            "…and must say what to do instead: {err}"
        );
        // The refusal must not have quietly rewritten the record it just refused to honour.
        assert_eq!(
            store.get_meta(TIMESTAMPS_KEY).unwrap().as_deref(),
            Some("1")
        );
    }

    /// The mirror case, and the one a pure equality check would miss: a nest that indexed *before*
    /// this key existed has no record at all. It must adopt its declaration when it holds no data, and
    /// be refused when it does - because "no key" and "no data" are different questions.
    #[test]
    fn a_pre_existing_nest_cannot_adopt_a_timestamp_free_declaration() {
        // Untouched nest: adopts whatever it declares.
        let fresh = tempfile::tempdir().unwrap();
        let s1 = Store::open(&fresh.path().join(DB_FILE)).unwrap();
        guard_timestamp_policy(&s1, false).unwrap();
        assert_eq!(s1.get_meta(TIMESTAMPS_KEY).unwrap().as_deref(), Some("0"));

        // Nest with history but no key - as every nest built before slice 4 will be.
        let old = tempfile::tempdir().unwrap();
        let s2 = Store::open(&old.path().join(DB_FILE)).unwrap();
        s2.set_meta(LAST_BLOCK_KEY, "1234").unwrap();
        let err = guard_timestamp_policy(&s2, false).unwrap_err().to_string();
        assert!(
            err.contains("already indexed"),
            "must explain it is the existing data that blocks this: {err}"
        );
        // It recorded the truth (it *has* timestamps), so the next start gives the same answer rather
        // than depending on whether `last_block` happens to still be there.
        assert_eq!(s2.get_meta(TIMESTAMPS_KEY).unwrap().as_deref(), Some("1"));
        assert!(guard_timestamp_policy(&s2, false).is_err());
        guard_timestamp_policy(&s2, true).unwrap();
    }

    /// Upgrading an existing nest must be a no-op: `block_timestamps` absent from `nuthatch.toml`
    /// means `true`, which is what every nest before slice 4 produced.
    #[test]
    fn an_older_nest_config_still_indexes_timestamps() {
        let cfg: Config = toml::from_str(
            r#"
[nest]
name = "old"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]
"#,
        )
        .unwrap();
        assert!(
            cfg.nest.block_timestamps,
            "absent must mean on, or upgrading silently drops a column from every table"
        );
    }

    // ---------------------------------------------------------------------------------------------
    // RFC-0029 slice 5: adaptive windows on the pipelined path.
    // ---------------------------------------------------------------------------------------------

    /// Counts `eth_getLogs` calls, which is the cost §6f is trying to remove.
    struct RequestCountingSource {
        logs: Vec<crate::rpc::Log>,
        calls: std::sync::atomic::AtomicUsize,
        widest: std::sync::atomic::AtomicU64,
    }

    /// A provider with a hard `eth_getLogs` **range** cap, which refuses an over-wide range the way
    /// the public mainnet endpoints actually do (#672): with a rate-limit-shaped error, not a
    /// "range too large" one that `is_result_too_large` recognises.
    ///
    /// That distinction is the whole point. Alchemy answers an oversized range with a message naming
    /// its 10,000-block cap, which the chunker understands and halves for. `eth.drpc.org`,
    /// `eth-pokt.nodies.app` and `eth.api.onfinality.io/public` answer HTTP 429 or 403 under the same
    /// conditions, and that says nothing about width. This reproduces the second case with no network
    /// in it, so the cost is a fixed number instead of a sample from a distribution that spanned 2 to
    /// 198 events across four identical 90-second runs.
    ///
    /// Distinct from [`CappedSource`], which refuses on *filter* breadth for the COR-5 path.
    /// How many width refusals a healthy caller should need to find its way under a cap. Generous:
    /// halving from 16,000 to under 10,000 is one step.
    const REFUSAL_GIVE_UP: usize = 50;

    struct RangeCappedSource {
        logs: Vec<crate::rpc::Log>,
        /// Widest range this provider will serve, in blocks.
        cap: u64,
        served: std::sync::atomic::AtomicUsize,
        refused: std::sync::atomic::AtomicUsize,
        widest_attempted: std::sync::atomic::AtomicU64,
    }

    #[async_trait::async_trait]
    impl Source for RangeCappedSource {
        async fn tip(&self) -> Result<u64> {
            Ok(u64::MAX)
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            let width = to - from + 1;
            self.widest_attempted
                .fetch_max(width, std::sync::atomic::Ordering::SeqCst);
            if width > self.cap {
                let n = self
                    .refused
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                // A real provider refuses indefinitely under sustained load, and so did this one -
                // which made the test hang rather than fail. A hang is a worse test than a failure:
                // it reports nothing. So the fake gives up after a bound, turning "never terminates"
                // into a number the assertion can name.
                if n > REFUSAL_GIVE_UP {
                    anyhow::bail!(
                        "gave up after {n} refusals - the caller never narrowed below {width} \
                         blocks against a {} block cap",
                        self.cap
                    );
                }
                anyhow::bail!("HTTP 429 Too Many Requests: rate limited");
            }
            self.served
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, 1_700_000_000 + b)).collect())
        }
    }

    #[async_trait::async_trait]
    impl Source for RequestCountingSource {
        async fn tip(&self) -> Result<u64> {
            Ok(u64::MAX)
        }
        async fn block_hash(&self, _n: u64) -> Result<Option<String>> {
            Ok(None)
        }
        async fn logs(
            &self,
            _filter: &crate::source::LogFilter,
            from: u64,
            to: u64,
        ) -> Result<Vec<crate::rpc::Log>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.widest
                .fetch_max(to - from + 1, std::sync::atomic::Ordering::SeqCst);
            Ok(self
                .logs
                .iter()
                .filter(|l| l.block_number >= from && l.block_number <= to)
                .cloned()
                .collect())
        }
        async fn block_timestamps(
            &self,
            blocks: &[u64],
        ) -> Result<std::collections::HashMap<u64, u64>> {
            Ok(blocks.iter().map(|&b| (b, 1_700_000_000 + b)).collect())
        }
    }

    /// #676 / #672. What the flagship first run *costs*, as a fixed number.
    ///
    /// The shape of the real thing: a contract with a long empty prefix, behind a provider that caps
    /// `eth_getLogs` at 10,000 blocks - Alchemy's documented limit - and refuses anything wider with a
    /// 429 rather than a width complaint. The chunker grows 4x per empty window, so it *will* walk
    /// into the cap; the question this pins down is how much it wastes discovering that, and whether
    /// it learns.
    ///
    /// This exists because the same measurement over the network could not be made: four identical
    /// 90-second runs of the real demo indexed 2, 15, 28 and 198 events. No fix can be evaluated
    /// against a spread like that, and no regression can hide from a number like the one below.
    ///
    /// The assertion is deliberately loose - it pins the *order of magnitude* of the waste, not the
    /// exact count, so ordinary retuning of the growth factor does not fail it while a return to
    /// unbounded retrying does.
    /// **No longer ignored, which is the point.** It hung indefinitely when written: the splitter
    /// recovered from a refused range silently, the controller read the merged result as a success
    /// at the width it had asked for, grew fourfold, and did it again. It completes in hundredths of
    /// a second now.
    #[tokio::test]
    async fn the_cost_of_a_first_run_behind_a_capped_provider_is_a_fixed_number() {
        let reg = transfer_registry();
        let src = RangeCappedSource {
            // A late-deployed contract: 200,000 empty blocks, then two logs.
            logs: ping_logs(&reg, &[199_998, 199_999]),
            cap: 10_000,
            served: std::sync::atomic::AtomicUsize::new(0),
            refused: std::sync::atomic::AtomicUsize::new(0),
            widest_attempted: std::sync::atomic::AtomicU64::new(0),
        };
        let dir = tempfile::tempdir().unwrap();
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];

        let rows = backfill_direct_pipelined(
            &src,
            &reg,
            dir.path(),
            &addresses,
            &[],
            &[],
            None,
            0,
            0,
            199_999,
            1_000,
            4,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        let served = src.served.load(std::sync::atomic::Ordering::SeqCst);
        let refused = src.refused.load(std::sync::atomic::Ordering::SeqCst);
        let widest = src
            .widest_attempted
            .load(std::sync::atomic::Ordering::SeqCst);
        eprintln!("first-run cost: served={served} refused={refused} widest_attempted={widest}");

        assert_eq!(rows, 2, "it must still find the logs it was looking for");
        // **Not zero, and that is deliberate.** An earlier version of this asserted zero refusals.
        // That is unachievable, and asking for it is a trap: a provider that refuses without saying
        // why cannot be queried for its cap, so the only way to learn one is to be refused by it at
        // least once. The single configuration that reaches zero is one that never grows - and never
        // growing costs the case `the_pipelined_path_grows_its_window_across_an_empty_prefix`
        // defends, where 200,000 empty blocks in front of a late-deployed contract are crossed
        // cheaply *because* the window grows. Both requirements are real. Unbounded growth is the
        // fault, not growth.
        //
        // The numbers are what the fix delivers rather than what would read well: 24 refusals
        // reaching 100,000 blocks before, 4 reaching 16,000 after. Each refusal carries a backoff
        // against a real provider, which is why that difference was 70 seconds of stall.
        assert!(
            refused <= 8,
            "{refused} refusals crossing a 200k-block prefix behind a 10k cap. Discovering an \
             unqueryable cap costs at least one, but each is a round trip that returned nothing"
        );
        assert!(
            refused < served,
            "more refused ({refused}) than served ({served}) - the run is spending itself on \
             discovery rather than on indexing"
        );
        assert!(
            widest <= 20_000,
            "asked for {widest} blocks against a 10,000-block cap - the controller grew far past \
             anything the provider had ever served"
        );
    }

    /// **The RFC-0029 §6f case.** A long empty prefix at a fixed window costs one request per window
    /// and returns nothing each time. The controller grows 4× per empty response, so the same range
    /// costs a handful of requests instead.
    ///
    /// Asserted as an *order-of-magnitude* reduction rather than an exact count, because the exact
    /// count is a function of the damping factor and the ceiling - both of which we should be free to
    /// tune without rewriting this test. What must not regress is that the pipelined path adapts at
    /// all, which is what it did not do before.
    #[tokio::test]
    async fn the_pipelined_path_grows_its_window_across_an_empty_prefix() {
        let reg = transfer_registry();
        // 200,000 empty blocks, then two logs at the very end - the shape of a contract deployed late
        // in a chain's history, which is most of them.
        let src = RequestCountingSource {
            logs: ping_logs(&reg, &[199_998, 199_999]),
            calls: std::sync::atomic::AtomicUsize::new(0),
            widest: std::sync::atomic::AtomicU64::new(0),
        };
        let dir = tempfile::tempdir().unwrap();
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];

        let rows = backfill_direct_pipelined(
            &src,
            &reg,
            dir.path(),
            &addresses,
            &[],
            &[],
            None,
            0,
            0,
            199_999,
            1_000, // a fixed 1,000-block window would need 200 requests
            4,
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        let calls = src.calls.load(std::sync::atomic::Ordering::SeqCst);
        let widest = src.widest.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(rows, 2, "it must still find the logs it was looking for");
        assert!(
            widest > 1_000,
            "the window must actually have grown past its starting width (widest was {widest})"
        );
        assert!(
            calls < 40,
            "a fixed 1,000-block window costs 200 requests here; adaptation should cost a small \
             fraction of that, but took {calls}"
        );
    }

    /// Adaptation must not run away on *dense* history - the direction that actually hurts, because an
    /// oversized window against a busy contract is what trips a provider's result cap.
    ///
    /// The controller is fed raw logs rather than decoded rows for exactly this reason: a nest with a
    /// narrow event allowlist would otherwise see almost every window as empty and grow to the ceiling
    /// against history that is anything but.
    #[tokio::test]
    async fn a_dense_range_does_not_grow_the_window() {
        let reg = transfer_registry();
        // 20,000 dense blocks, not 3,000. A short range cannot distinguish a controller that is
        // behaving from one that is running away: with only three windows the growth has nowhere to
        // go before `to` clamps it, and the first version of this test passed against a deliberately
        // broken controller for exactly that reason.
        let blocks: Vec<u64> = (0..20_000).collect();
        let src = RequestCountingSource {
            logs: ping_logs(&reg, &blocks),
            calls: std::sync::atomic::AtomicUsize::new(0),
            widest: std::sync::atomic::AtomicU64::new(0),
        };
        let dir = tempfile::tempdir().unwrap();
        let addresses = vec!["0x1111111111111111111111111111111111111111".to_string()];

        let rows = backfill_direct_pipelined(
            &src,
            &reg,
            dir.path(),
            &addresses,
            &[],
            &[],
            None,
            0,
            0,
            19_999,
            1_000,
            1, // sequential, so every window's feedback lands before the next is generated
            |_| Ok(()),
            |_, _| {},
        )
        .await
        .unwrap();

        assert_eq!(rows, 20_000);
        let widest = src.widest.load(std::sync::atomic::Ordering::SeqCst);
        // One log per block at a 1,000-block window is 1,000 logs against a 2,000 target, so a working
        // controller settles around 2,000 blocks. A controller that thinks every window came back
        // empty grows 4× a step and is past 4,000 by the third window - so this bound separates the
        // two, which the earlier `<= 4_000` over a 3,000-block range did not.
        assert!(
            widest <= 2_500,
            "dense history must not push the window toward the ceiling (widest was {widest})"
        );
    }

    // --- restart rebuild: one pass over the hot store (issue #294) -------------------------------

    /// A transfer row shaped exactly as `DecodedRow::to_json` writes one: `value` is a decimal
    /// *string* (uint256 does not fit a JSON number) and `block_number` is a number.
    ///
    /// `to` is optional and writes JSON `null` when absent, which is how an undecodable recipient
    /// reaches the hot store. Such a row still carries outbound volume, so the rebuild must feed it
    /// to velocity while withholding it from balances and exposure.
    fn transfer_row(
        block: u64,
        log_index: u64,
        from: &str,
        to: Option<&str>,
        value: &str,
    ) -> String {
        let to = to.map_or("null".to_string(), |t| format!("\"{t}\""));
        format!(
            r#"{{"table":"usdc__transfer","block_number":{block},"block_hash":"0xbh","block_timestamp":{ts},"tx_hash":"0xtx","log_index":{log_index},"address":"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48","from":"{from}","to":{to},"value":"{value}"}}"#,
            ts = 1_700_000_000 + block
        )
    }

    /// A registry with one ERC-20 table, so `transfer_columns()` yields `("from","to","value")`.
    fn erc20_registry() -> DecodeRegistry {
        const ERC20: &str = r#"[{"type":"event","name":"Transfer","inputs":[
            {"name":"from","type":"address","indexed":true},
            {"name":"to","type":"address","indexed":true},
            {"name":"value","type":"uint256","indexed":false}],"anonymous":false}]"#;
        let abi: alloy_json_abi::JsonAbi = serde_json::from_str(ERC20).unwrap();
        DecodeRegistry::build(vec![crate::registry::ContractSpec {
            alias: "usdc".into(),
            address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
                .parse()
                .unwrap(),
            abi,
            events: Vec::new(),
        }])
        .unwrap()
    }

    /// A `LabelSet` containing `pairs`, built the only way one can be: written to disk and loaded.
    fn labelset(pairs: &[(&str, &str)]) -> crate::labels::LabelSet {
        let entries: Vec<crate::labels::LabelEntry> = pairs
            .iter()
            .map(|(a, l)| crate::labels::LabelEntry {
                address: a.to_string(),
                label: l.to_string(),
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(crate::labels::LABELS_DIR);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("snap.json"),
            serde_json::to_string(&entries).unwrap(),
        )
        .unwrap();
        crate::labels::load(dir.path())
    }

    /// The acceptance test for #294: collapsing three scans into one must leave the views **the same**,
    /// not merely arrive faster.
    ///
    /// The reference is a transfer-by-transfer replay through the live delta paths - which is exactly
    /// what the three separate rebuilds did, and what the views would hold had they been grown from
    /// genesis. Every observable of all three views is compared, so dropping any one view's fan-out
    /// from the merged loop fails here rather than being discovered on an operator's restart.
    #[test]
    fn rebuild_views_matches_a_transfer_by_transfer_replay() {
        const MIXER: &str = "0x3333333333333333333333333333333333333333";
        const ALICE: &str = "0x1111111111111111111111111111111111111111";
        const BOB: &str = "0x2222222222222222222222222222222222222222";
        const CAROL: &str = "0x4444444444444444444444444444444444444444";
        const WINDOW: u64 = 100;

        // Two blocks in one velocity bucket and one far outside it, so bucketing is exercised; a
        // labelled counterparty on both sides, so exposure has "out" and "in" rows to build.
        //
        // CAROL's row has an unreadable `to` (JSON null) with `from` and `value` intact. It is the
        // row that pins the shape of the hot-replay guard: balances and exposure need a counterparty
        // and must skip it, velocity needs only (from, block, value) and must still count its
        // outbound volume. Without such a row, folding velocity inside the `(from, to, val)` guard
        // silently drops outbound volume with every assertion still green.
        let fixture = [
            (10u64, 0u64, ALICE, Some(MIXER), "100"),
            (11, 0, ALICE, Some(BOB), "250"),
            (11, 1, BOB, Some(MIXER), "7"),
            (210, 0, MIXER, Some(ALICE), "42"),
            (12, 0, CAROL, None, "500"),
        ];

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("t.redb")).unwrap();
        for (b, li, from, to, val) in fixture {
            store
                .put_entity(
                    &Store::entity_key(b, li),
                    &transfer_row(b, li, from, to, val),
                )
                .unwrap();
        }
        let registry = erc20_registry();
        let labels = labelset(&[(MIXER, "mixer")]);
        assert!(
            !labels.is_empty(),
            "fixture needs labels or exposure no-ops"
        );

        // Under test: one pass, all three views.
        let balances = BalanceView::start().unwrap();
        let exposure_v = ExposureView::start(true).unwrap();
        let velocity_v = VelocityView::start(true).unwrap();
        rebuild_views(
            dir.path(),
            &store,
            &registry,
            &DerivedViews {
                labels: &labels,
                balances: &balances,
                exposure: &exposure_v,
                velocity: &velocity_v,
                velocity_window: Some(WINDOW),
            },
        )
        .unwrap();

        // Reference: the same facts, fed one transfer at a time exactly as the live loop feeds them.
        // Nothing is sealed, so there is no cold seed on either side - this is purely the hot tail.
        let want_balances = BalanceView::start().unwrap();
        let want_exposure = ExposureView::start(true).unwrap();
        let want_velocity = VelocityView::start(true).unwrap();
        for (b, _li, from, to, val) in fixture {
            let v: i128 = val.parse().unwrap();
            // Balances and exposure are both two-sided, so a row with no readable `to` has nothing
            // to move between and nothing to be exposed to - the live loop never feeds them one.
            if let Some(to) = to {
                want_balances.apply(views::transfer_deltas(from, to, v, 1));
                want_exposure.apply(exposure::exposure_deltas(from, to, v, 1, &labels));
            }
            // Velocity is one-sided - "how much did `from` push out" - so it is fed unconditionally,
            // `to` readable or not. This asymmetry is the thing under test.
            want_velocity.apply(velocity::velocity_deltas(from, b, v, 1, WINDOW));
        }
        want_balances.flush();
        want_exposure.flush();
        want_velocity.flush();

        // Balances: every holder, not just a spot check.
        assert_eq!(balances.holders(), want_balances.holders());
        assert_eq!(balances.top(usize::MAX), want_balances.top(usize::MAX));
        assert!(balances.holders() > 0, "fixture must move some balance");

        // Exposure: entry count and the full row set for every address in the fixture.
        assert_eq!(exposure_v.entries(), want_exposure.entries());
        for addr in [ALICE, BOB, MIXER] {
            assert_eq!(
                exposure_v.exposure(addr),
                want_exposure.exposure(addr),
                "exposure rows differ for {addr}"
            );
        }
        assert!(exposure_v.entries() > 0, "fixture must build exposure");

        // Velocity: bucket count and every flagged bucket.
        assert_eq!(
            velocity_v.entries(),
            want_velocity.entries(),
            "velocity bucket count differs from the replay - a missing bucket means the rebuild \
             skipped a sender the live loop would have counted"
        );
        assert_eq!(
            velocity_v.flags(1),
            want_velocity.flags(1),
            "velocity flags differ from the replay"
        );
        assert!(
            velocity_v.entries() > 1,
            "fixture must land in more than one window bucket"
        );

        // The unreadable-`to` row, asserted directly rather than only against the reference, so both
        // sides of the guard's asymmetry are pinned even if the reference above drifts.
        let carol = velocity_v
            .flags(1)
            .into_iter()
            .find(|f| f.address == CAROL)
            .expect("velocity must count outbound volume for a transfer with an unreadable `to`");
        assert_eq!(
            carol.volume, 500,
            "velocity must count the whole outbound value of an unreadable-`to` transfer"
        );
        assert!(
            !balances.top(usize::MAX).iter().any(|(a, _)| a == CAROL),
            "balances must not move value for a transfer with no readable counterparty"
        );
        assert!(
            exposure_v.exposure(CAROL).is_empty(),
            "exposure must not build a row for a transfer with no readable counterparty"
        );
    }

    /// #294 itself: the restart rebuild must walk the hot store **once**.
    ///
    /// The correctness test above is deliberately blind to this - it passes just as well against the
    /// three-scan version, because three scans produce the same answer, only slower. This is the test
    /// that goes red if the scans come back.
    ///
    /// `recent_by_table` is the expensive shape: it iterates the entire entity table and JSON-parses
    /// every row merely to compare its `table` field, so one call per view per transfer table is
    /// `3 × tables` full walks on a path that runs at every restart and every crash recovery.
    #[test]
    fn the_restart_rebuild_walks_the_hot_store_once() {
        let src = include_str!("indexer.rs");
        let start = src
            .find("\nfn rebuild_views(")
            .expect("rebuild_views must exist - if it was renamed, retarget this test");
        // The function ends where the next item at column 0 begins.
        let body = &src[start + 1..];
        let end = body.find("\n}\n").expect("unterminated rebuild_views") + 2;
        let body = &body[..end];

        let code: Vec<&str> = body
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("///")
            })
            .collect();
        let count = |needle: &str| code.iter().filter(|l| l.contains(needle)).count();

        // Split so these needles never match the line they are written on.
        let scans = count(concat!("hot_rows_by", "_table()"));
        let per_table = count(concat!("recent_by", "_table("));

        assert_eq!(
            scans, 1,
            "the restart rebuild must walk the hot store exactly once and fan the rows out to all \
             three views; found {scans} unbounded scan(s) in rebuild_views"
        );
        assert_eq!(
            per_table, 0,
            "rebuild_views must not use recent_by_table: it re-walks and re-parses the whole entity \
             table per call, which is what issue #294 removed; found {per_table} call(s)"
        );
    }

    /// A factory set with one rule, so `factory_tables()` yields exactly one announcing table.
    fn one_rule_factory_set() -> (FactorySet, String) {
        let config: Config = toml::from_str(
            r#"
[nest]
name="t"
chain="mainnet"
chain_id=1
rpc_urls=["https://rpc"]
[[contracts]]
alias="factory"
address="0x1111111111111111111111111111111111111111"
abi="abis/f.json"
[[templates]]
name="pool"
abi="abis/p.json"
[[factories]]
watch="factory"
event="PoolCreated"
child_param="pool"
template="pool"
"#,
        )
        .unwrap();
        let fs = FactorySet::build(&config).unwrap();
        let table = fs.factory_tables().first().cloned().expect("one rule");
        (fs, table)
    }

    /// #373: an unsealed factory table is a legitimate absence and must still rebuild cleanly.
    ///
    /// This is the half that makes the fix safe rather than merely strict. Before it, the cold read
    /// was attempted unconditionally and *any* error forgiven, so this case and the failing one below
    /// were indistinguishable. The catalogue is what separates them: no segment entry, no query.
    #[test]
    fn rebuild_children_is_fine_when_a_factory_table_has_never_been_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let (fs, _table) = one_rule_factory_set();
        let store = crate::store::Store::open(&dir.path().join("t.redb")).unwrap();
        let reg = DecodeRegistry::build(Vec::new()).unwrap();

        let children = rebuild_children(dir.path(), &store, &reg, &fs)
            .expect("an unsealed factory table must not be an error");
        assert!(children.is_empty(), "nothing discovered, nothing sealed");
    }

    /// #373: a cold read that fails on a table the catalogue says IS sealed must surface, not be
    /// silently forgiven into a short child registry.
    ///
    /// The mutation this kills is the original line, `if let Ok(cold) = analytics::query(...)`.
    /// Restore it and this test goes green while the bug is back: `rebuild_children` returns a
    /// registry missing every child that lived in the unreadable segment, the nest starts, and it
    /// simply stops indexing those contracts. Nothing logs, nothing reports degraded, and the only
    /// symptom is data that never arrives.
    #[test]
    fn rebuild_children_surfaces_a_cold_read_failure_instead_of_a_short_registry() {
        let dir = tempfile::tempdir().unwrap();
        let (fs, table) = one_rule_factory_set();
        let store = crate::store::Store::open(&dir.path().join("t.redb")).unwrap();
        let reg = DecodeRegistry::build(Vec::new()).unwrap();

        // A catalogue that promises a sealed segment whose parquet is not there. That is exactly the
        // shape a half-written seal or a deleted segments dir leaves behind, and the read must fail.
        let segments = dir.path().join(crate::seal::SEGMENTS_DIR);
        std::fs::create_dir_all(&segments).unwrap();
        let manifest = serde_json::json!({
            "tables": {
                table.clone(): [{
                    "hash": "0".repeat(64),
                    "from_block": 1,
                    "to_block": 2,
                    "rows": 1,
                    "file": "definitely-not-here.parquet",
                }]
            }
        });
        std::fs::write(
            segments.join(crate::seal::MANIFEST_FILE),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();

        let err = rebuild_children(dir.path(), &store, &reg, &fs)
            .expect_err("an unreadable sealed segment must fail the rebuild, not shorten it");
        let msg = format!("{err:#}");
        assert!(
            msg.contains(&table),
            "the error must name the table that could not be read, got: {msg}"
        );
    }

    /// #727: the identity-keyed skip logged at `debug!`, invisible at the default `nuthatch=info` an
    /// operator actually runs with - so hand-editing `nuthatch.toml` under a `data/<nid>` dataset and
    /// restarting explained nothing about why `schema.json` did not move. Likely how the Lodestar GNS
    /// nest's `schema.json` fell behind in the first place.
    ///
    /// Alternates both arms in one test, captured at the default level, so a regression back to
    /// `debug!` fails it: an authored, mutable-keyed nest must *not* emit this line (it refreshes
    /// instead, via `refresh_stale_artifacts`), and an identity-keyed dataset must, at `info!`. An
    /// absence test on only one arm would pass just as happily with the mechanism deleted outright;
    /// asserting presence alone would pass just as happily left at `debug!` if the capture were not
    /// pinned to the default filter.
    ///
    /// The identity-keyed dir is the real path, not a hand-built stand-in: a nest is hashed with
    /// `blob::nest_nid` and moved to `data/<that nid>` - the exact shape `migrate` produces and
    /// `MountTable::is_identity_keyed` checks in production.
    #[tokio::test]
    async fn identity_keyed_skip_logs_at_info_not_debug() {
        use tracing_subscriber::layer::SubscriberExt as _;

        fn write_minimal_nest(dir: &std::path::Path, name: &str) {
            std::fs::create_dir_all(dir.join("abis")).unwrap();
            std::fs::write(
                dir.join(crate::config::CONFIG_FILE),
                format!(
                    "[nest]\nname = \"{name}\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
                     rpc_urls = []\n\n[[contracts]]\nalias = \"t\"\n\
                     address = \"0x0000000000000000000000000000000000000001\"\nabi = \"abis/t.json\"\n"
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join("abis/t.json"),
                r#"[{"type":"event","name":"Ping","inputs":[],"anonymous":false}]"#,
            )
            .unwrap();
        }

        async fn build(dir: &std::path::Path) {
            let config = Config::load(dir).unwrap();
            let source: Arc<dyn Source> = Arc::new(MockSource { logs: Vec::new() });
            let (_nest, _state, worker, _w) = build_nest(
                &source,
                dir.to_path_buf(),
                &config,
                None,
                false,
                None,
                None,
                serve::new_sql_gate(),
            )
            .await
            .unwrap();
            if let Some(w) = worker {
                w.abort();
            }
        }

        // Same capture shape as `analytics.rs`'s `CapturedLogs`: `tracing`'s per-callsite `Interest`
        // is a global, process-wide cache, so a positive assertion here is only trustworthy because
        // this test is the *only* place in the suite that ever builds a `data/<nid>`-shaped dir through
        // `build_nest` - nothing else can have reached this call site first with no subscriber
        // installed and cached it uninterested.
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

        const SKIP: &str = "identity-keyed dataset: leaving derived artifacts alone";

        let captured = CapturedLogs::default();
        let dispatch = tracing::Dispatch::new(
            tracing_subscriber::registry()
                .with(tracing_subscriber::filter::LevelFilter::INFO)
                .with(captured.clone()),
        );

        // Arm 1: an authored, mutable-keyed nest - a plain temp dir, nothing shaped like `data/<64-hex>`.
        let mutable = tempfile::tempdir().unwrap();
        write_minimal_nest(mutable.path(), "m");
        {
            let _guard = tracing::dispatcher::set_default(&dispatch);
            build(mutable.path()).await;
        }
        assert_eq!(
            captured.mentioning(SKIP),
            0,
            "an authored, mutable-keyed nest refreshes its derived artifacts directly - it must never \
             log the identity-keyed skip"
        );

        // Arm 2: the same shape of nest, moved to the identity it genuinely hashes to.
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join("staging");
        write_minimal_nest(&staging, "id");
        let nid = crate::blob::nest_nid(&staging).unwrap();
        let identity_dir = crate::runtime::MountTable::data_dir(root.path(), &nid);
        std::fs::create_dir_all(identity_dir.parent().unwrap()).unwrap();
        std::fs::rename(&staging, &identity_dir).unwrap();
        assert!(
            crate::runtime::MountTable::is_identity_keyed(&identity_dir),
            "fixture must actually be shaped like data/<nid>, or this test proves nothing"
        );
        {
            let _guard = tracing::dispatcher::set_default(&dispatch);
            build(&identity_dir).await;
        }
        assert_eq!(
            captured.mentioning(SKIP),
            1,
            "an identity-keyed dataset must log the skip at the default level - #727: this fired only \
             at debug! before, invisible under the default `nuthatch=info` filter this test pins to"
        );
    }
}
