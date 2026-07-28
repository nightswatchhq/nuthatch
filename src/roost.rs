//! The roost (RFC-0012 §1-4; multichain per RFC-0021): one runtime hosting many nests across **one or
//! more chains** - one isolated cursor per distinct chain (`group_by_chain` → a `spawn_roost` each),
//! held to a **per-cursor** RSS budget. A single-chain roost (top-level `chain`) is the N=1 case, still
//! byte-identical to solo `dev`. The single-cursor law holds per chain: never multiplex two chains
//! behind one cursor. Below is the original RFC-0012 single-chain history.
//!
//! (RFC-0012) one runtime hosting many nests on the same chain. Slice 1 landed the
//! **layout + serving surface** - a `roost.toml` naming the chain and the mounted nests, a `/nests`
//! roster, and every nest's full API under a `/<name>/…` prefix. Slice 2a landed the **shared cursor**:
//! `dev` now drives all nests from ONE `indexer::spawn_roost` task - one `getLogs` per window fanned
//! out to the owning nests (see `indexer::roost_index_loop`), so N nests cost one nest's worth of RPC
//! chatter. Per-nest tables stay byte-identical to running each nest solo (the same per-window code
//! runs either way). Static and factory nests can be co-mounted (slice 2b - a factory forces the union
//! fetch topic0-only, demuxing by topic0 instead of address); shared reorg fan-out is slice 3; and a
//! per-runtime footprint projection + `max_rss` refusal is slice 4.
//!
//! Isolation is by construction: each nest keeps its own directory (`nests/<name>/` - its own
//! `nuthatch.redb`, `segments/`, views), so one nest's bad view or runaway factory can't touch
//! another's data (the CLAUDE.md non-negotiable). The roost shares the *chain identity* and the
//! *cursor* - never the stores.

use crate::config::Config;
use crate::indexer;
use crate::rpc::{self, RpcClient};
use crate::source::Source;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The roost manifest file, at the roost directory root. Sibling of a nest's `nuthatch.toml`.
pub const ROOST_FILE: &str = "roost.toml";

/// Where mounted nests live under the roost dir: `nests/<name>/` is a nest directory, exactly as a
/// standalone nest is today.
pub const NESTS_DIR: &str = "nests";

/// A roost manifest: the mounted nests plus the chain(s) they follow. A roost may host nests across
/// **one or more chains** (RFC-0021) - one isolated cursor per distinct chain. The single-chain form
/// keeps the top-level `chain`/`chain_id`/`rpc_urls`; a multichain roost lists its chains under
/// `[[chains]]` and lets each nest declare its own chain. The single-cursor law holds **per chain**:
/// never multiplex two chains behind one cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roost {
    pub roost: RoostMeta,
    /// Multichain: each chain the roost serves, with its own RPC endpoints (RFC-0021). Mutually
    /// exclusive with the top-level `chain`/`chain_id`. Empty → the single-chain top-level form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainEndpoint>,
}

/// One chain a roost follows, plus how to reach it - a cursor's substrate (RFC-0021).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainEndpoint {
    pub chain: String,
    pub chain_id: u64,
    #[serde(default)]
    pub rpc_urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoostMeta {
    /// Human name for the roost (logging/roster only).
    pub name: String,
    /// Single-chain form: the one chain the cursor follows. Omit (with `chain_id`) for a multichain
    /// roost that declares its chains under `[[chains]]` instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<String>,
    /// Single-chain form: the one chain id. Omit for a multichain roost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_id: Option<u64>,
    /// Single-chain form: RPC endpoints for the one chain. Overridable at runtime with `--rpc`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rpc_urls: Vec<String>,
    /// The mounted nests, by directory name under `nests/`.
    pub nests: Vec<String>,
    /// Resident-set ceiling **per active-chain cursor**, in MB (RFC-0021 - the footprint budget is
    /// per-cursor; a roost's total is Σ cursors). A cursor whose *projected* RSS exceeds this is refused
    /// before it starts. Absent → the CLAUDE.md 2 GB budget ([`DEFAULT_MAX_RSS_MB`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rss_mb: Option<u64>,
}

/// The default per-cursor RSS ceiling: the CLAUDE.md ≤2 GB budget (RFC-0021 - now per active-chain
/// cursor, not per whole runtime).
pub const DEFAULT_MAX_RSS_MB: u64 = 2048;

// A deliberately rough, *honest* per-runtime footprint model (RFC-0012 §3). These are order-of-
// magnitude estimates for the pre-mount projection, not measurements - the roster reports the real
// `rss_bytes()` alongside so an operator can calibrate. The shared serving/runtime cost is paid once;
// each nest adds its hot-store working set + decode registry, plus a chunk per active IVM view.
const ROOST_BASE_RSS_MB: u64 = 120; // serving + async runtime + on-demand DuckDB, paid once
const NEST_BASE_RSS_MB: u64 = 90; // redb hot store + decode registry + the always-on balance view
const NEST_VIEW_RSS_MB: u64 = 40; // each extra load: exposure view, velocity view, or child registry

/// Rough projected RSS (MB) for one nest: base + a chunk per active IVM view / factory child registry.
/// `has_labels` gates the exposure view (only spun up when the nest has labeled addresses).
pub fn estimate_nest_rss_mb(config: &Config, has_labels: bool) -> u64 {
    let mut mb = NEST_BASE_RSS_MB;
    if has_labels {
        mb += NEST_VIEW_RSS_MB; // exposure view (RFC-0008 C1)
    }
    if config.flags.velocity().is_some() {
        mb += NEST_VIEW_RSS_MB; // velocity view (RFC-0008 C3)
    }
    if !config.factories.is_empty() {
        mb += NEST_VIEW_RSS_MB; // discovered-child registry (RFC-0009)
    }
    mb
}

impl Roost {
    /// Load and validate `roost.toml` from a roost directory.
    pub fn load(dir: &Path) -> Result<Roost> {
        let path = dir.join(ROOST_FILE);
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no {ROOST_FILE} in {}", dir.display()))?;
        let roost: Roost =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if roost.roost.nests.is_empty() {
            bail!(
                "roost '{}' mounts no nests (empty `nests` list)",
                roost.roost.name
            );
        }
        // Reject duplicate mounts and any name that would collide with a reserved top-level route
        // (`/nests`, `/health`) - the roster and per-nest prefixes share one path namespace.
        let mut seen = std::collections::HashSet::new();
        for n in &roost.roost.nests {
            // SEC-10: a nest name is both a filesystem path segment (`nests/<name>/`) and a route
            // prefix (`/<name>/…`), so restrict it to a safe charset - no `/`, `..`, or empties that
            // could escape the nests dir or produce surprising routes (matters once names come from a
            // resolved blob roster, not just an operator-authored toml).
            if n.is_empty()
                || !n
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                bail!("nest name '{n}' is invalid (allowed: letters, digits, '_', '-')");
            }
            if n == "nests" || n == "health" {
                bail!("nest name '{n}' is reserved (collides with a roost route)");
            }
            if !seen.insert(n) {
                bail!("nest '{n}' is mounted more than once");
            }
        }
        Ok(roost)
    }

    /// The on-disk directory of a mounted nest, relative to the roost dir.
    pub fn nest_dir(dir: &Path, name: &str) -> PathBuf {
        dir.join(NESTS_DIR).join(name)
    }

    /// The chains this roost serves, each with its RPC endpoints (RFC-0021). A single-chain roost
    /// synthesizes one entry from the top-level `chain`/`chain_id`/`rpc_urls`; a multichain roost
    /// returns its `[[chains]]`. Errors if both forms are present (ambiguous) or neither (no chain).
    pub fn chain_endpoints(&self) -> Result<Vec<ChainEndpoint>> {
        let has_top = self.roost.chain.is_some() || self.roost.chain_id.is_some();
        if !self.chains.is_empty() {
            if has_top {
                bail!(
                    "roost '{}' declares both a top-level chain and [[chains]] - use one form",
                    self.roost.name
                );
            }
            return Ok(self.chains.clone());
        }
        match (self.roost.chain.clone(), self.roost.chain_id) {
            (Some(chain), Some(chain_id)) => Ok(vec![ChainEndpoint {
                chain,
                chain_id,
                rpc_urls: self.roost.rpc_urls.clone(),
            }]),
            _ => bail!(
                "roost '{}' declares no chain - set [roost] chain/chain_id/rpc_urls, or [[chains]]",
                self.roost.name
            ),
        }
    }
}

/// A chain's cursor unit (RFC-0021): the endpoint (RPC) plus the mounted nests that follow that chain.
/// Each becomes one isolated cursor - the single-cursor law, held per chain.
#[derive(Debug)]
pub struct ChainGroup {
    pub endpoint: ChainEndpoint,
    pub nests: Vec<(String, PathBuf, Config)>,
}

/// Load a mounted nest's config (chain grouping is validated by [`group_by_chain`], not here).
fn load_mounted_nest(roost_dir: &Path, name: &str) -> Result<(PathBuf, Config)> {
    let dir = Roost::nest_dir(roost_dir, name);
    let config = Config::load(&dir)
        .with_context(|| format!("loading mounted nest '{name}' from {}", dir.display()))?;
    Ok((dir, config))
}

/// Group loaded nests by their declared chain, matching each to a roost chain endpoint (RFC-0021).
/// A nest whose chain the roost doesn't declare is a hard error; declared-but-unused chains are dropped
/// (a cursor with no nests is pointless). Deterministic order (endpoints as declared).
pub fn group_by_chain(
    endpoints: &[ChainEndpoint],
    mounted: Vec<(String, PathBuf, Config)>,
) -> Result<Vec<ChainGroup>> {
    let mut groups: Vec<ChainGroup> = endpoints
        .iter()
        .map(|e| ChainGroup {
            endpoint: e.clone(),
            nests: Vec::new(),
        })
        .collect();
    for (name, path, config) in mounted {
        let idx = groups.iter().position(|g| {
            g.endpoint.chain == config.nest.chain && g.endpoint.chain_id == config.nest.chain_id
        });
        match idx {
            Some(i) => groups[i].nests.push((name, path, config)),
            None => bail!(
                "nest '{name}' is on {} (chain_id {}), which this roost doesn't declare - add it under \
                 [[chains]] (or [roost] chain/chain_id)",
                config.nest.chain,
                config.nest.chain_id
            ),
        }
    }
    groups.retain(|g| !g.nests.is_empty());
    if groups.is_empty() {
        bail!("roost mounts nests but none matched a declared chain");
    }
    Ok(groups)
}

/// `nuthatch roost dev <dir>`: bring up every mounted nest and serve them behind one listener.
///
/// One shared source drives all nests through a single `indexer::spawn_roost` task per chain (the
/// shared cursor - one `getLogs` per window fanned out to the owning nests). Before starting it
/// projects the roost's RSS and refuses a mount that would exceed `max_rss` (§3). A cursor that dies
/// is **quarantined, not fatal** (RFC-0026): its siblings keep indexing and serving, and the roost
/// exits only when every cursor is gone - the per-cursor blast-radius rule, actually held.
#[allow(clippy::too_many_arguments)]
pub async fn dev(
    dir: PathBuf,
    listen: String,
    rpc_override: Vec<String>,
    backfill: Option<u64>,
    seal_direct: bool,
    concurrency: usize,
    window_override: Option<u64>,
    no_admin: bool,
    fail_fast: bool,
) -> Result<()> {
    let roost = Roost::load(&dir)?;
    let meta = &roost.roost;
    let endpoints = roost.chain_endpoints()?;

    // Load every mounted nest, then group by chain - one isolated cursor per distinct chain (RFC-0021).
    let mut mounted = Vec::with_capacity(meta.nests.len());
    for name in &meta.nests {
        let (nest_path, config) = load_mounted_nest(&dir, name)?;
        mounted.push((name.clone(), nest_path, config));
    }
    let groups = group_by_chain(&endpoints, mounted)?;

    // `--rpc` is ambiguous once a roost spans chains (which chain would it override?). Allow it only for
    // a single-chain roost; a multichain roost sets rpc_urls per chain under [[chains]].
    if !rpc_override.is_empty() && groups.len() > 1 {
        bail!(
            "--rpc is ambiguous for a multichain roost ({} chains) - set rpc_urls per chain under [[chains]]",
            groups.len()
        );
    }
    tracing::info!(
        "roost '{}': mounting {} nest(s) across {} chain(s) - one isolated cursor per chain",
        meta.name,
        meta.nests.len(),
        groups.len(),
    );

    let admin_enabled = indexer::admin_enabled(no_admin, &listen);
    let admin_token = indexer::admin_required_token(admin_enabled, &listen);
    // The RSS budget is now **per active-chain cursor** (RFC-0021), not per whole runtime.
    let max_rss = meta.max_rss_mb.unwrap_or(DEFAULT_MAX_RSS_MB);

    // Bring up one cursor per chain group: its own source + `spawn_roost`, isolated tip/finality/reorg,
    // and held to the per-cursor RSS budget. A cursor's failure quarantines that cursor alone (RFC-0026).
    let mut all_states: Vec<(String, crate::serve::AppState)> = Vec::new();
    let mut ingests: Vec<(String, tokio::task::JoinHandle<Result<()>>)> = Vec::new();
    let mut alert_workers: Vec<(String, tokio::task::JoinHandle<()>)> = Vec::new();
    // Chain -> that cursor's command channel, so an unmount reaches the cursor hosting the nest.
    let mut lifecycle: std::collections::HashMap<
        String,
        tokio::sync::mpsc::UnboundedSender<indexer::CursorCommand>,
    > = std::collections::HashMap::new();
    let mut estimates: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut sources: std::collections::HashMap<String, Arc<dyn Source>> =
        std::collections::HashMap::new();
    let mut roost_total_mb = ROOST_BASE_RSS_MB;
    // The live health surface (RFC-0026 §5): the cursors write quarantine state here, the API reads it
    // per request. Replaces the roster snapshot that was built once at startup and could not express
    // "partly working".
    let health = Arc::new(crate::health::RoostHealth::new());

    for group in groups {
        let rpc_urls = rpc::merge_rpcs(&rpc_override, group.endpoint.rpc_urls.clone());
        if rpc_urls.is_empty() {
            bail!(
                "roost '{}' chain {} has no rpc_urls (set them under [[chains]], or pass --rpc for a \
                 single-chain roost)",
                meta.name,
                group.endpoint.chain
            );
        }
        let concurrency = indexer::safe_backfill_concurrency(rpc_urls.len(), concurrency);

        // Per-cursor footprint budget (RFC-0021): this chain's nests must fit ≤ max_rss.
        let mut cursor_mb = 0u64;
        for (name, path, config) in &group.nests {
            let has_labels = !crate::labels::load(path).is_empty();
            let mb = estimate_nest_rss_mb(config, has_labels);
            estimates.insert(name.clone(), mb);
            cursor_mb += mb;
        }
        tracing::info!(
            "roost cursor on {} (chain_id {}): {} nest(s), ~{cursor_mb} MB projected; budget {max_rss} MB/cursor",
            group.endpoint.chain,
            group.endpoint.chain_id,
            group.nests.len(),
        );
        if cursor_mb > max_rss {
            bail!(
                "roost '{}' cursor on {} projects ~{cursor_mb} MB but max_rss is {max_rss} MB/cursor - \
                 raise max_rss, drop a nest, or move it to another roost",
                meta.name,
                group.endpoint.chain
            );
        }
        roost_total_mb += cursor_mb;

        // Attribute each nest to this chain's cursor, so a cursor fault marks all of them (§5).
        for (name, _, _) in &group.nests {
            health.register(name, &group.endpoint.chain);
        }

        // One source + one shared cursor per chain - per-nest tables stay byte-identical to solo `dev`.
        // Verify the whole pool is on THIS chain first (issue #150). It matters more in a roost than
        // solo: with several chains in one runtime, pasting one chain's endpoint under another's
        // `[[chains]]` entry is an easy slip, and failover would mask it indefinitely.
        let rpc = RpcClient::new(rpc_urls)?;
        rpc.verify_chain_ids(group.endpoint.chain_id)
            .await
            .with_context(|| {
                format!(
                    "verifying rpc_urls for roost '{}' cursor on {}",
                    meta.name, group.endpoint.chain
                )
            })?;
        let source: Arc<dyn Source> = Arc::new(rpc);
        // Retained so a mount can build a nest against the same source its co-tenants use - a nest
        // mounted at runtime must be indistinguishable from one mounted at boot.
        sources.insert(group.endpoint.chain.clone(), source.clone());
        let cursor = indexer::spawn_roost(
            source,
            group.nests,
            backfill,
            seal_direct,
            concurrency,
            window_override,
            admin_enabled,
            admin_token.clone(),
            health.clone(),
            fail_fast,
        )
        .await
        .with_context(|| {
            format!(
                "bringing up roost '{}' cursor on {}",
                meta.name, group.endpoint.chain
            )
        })?;
        // Retain the per-nest handles: the driver needs them to re-compose the router (and abort the
        // right alert worker) when a nest is unmounted (RFC-0027 §6).
        lifecycle.insert(group.endpoint.chain.clone(), cursor.lifecycle);
        all_states.extend(cursor.states);
        ingests.push((group.endpoint.chain.clone(), cursor.ingest));
        alert_workers.extend(cursor.alert_workers);
    }

    tracing::info!(
        "roost footprint: ~{roost_total_mb} MB projected across {} cursor(s)",
        ingests.len()
    );

    // Roster (`GET /nests`) across every cursor's nests, with per-nest footprint attribution and the
    // roost's real resident set alongside the projection so operators can calibrate.
    let roster_entries: Vec<_> = all_states
        .iter()
        .map(|(name, state)| {
            serde_json::json!({
                "name": name,
                "chain": state.chain,
                "registry_hash": state.nest_info.get("registry_hash").cloned().unwrap_or_default(),
                "table_count": state.tables.len(),
                "base_path": format!("/{name}"),
                "estimated_rss_mb": estimates.get(name).copied().unwrap_or(0),
            })
        })
        .collect();
    let roster = serde_json::json!({
        "roost": meta.name,
        "chains": endpoints.iter().map(|e| e.chain.clone()).collect::<Vec<_>>(),
        "projected_rss_mb": roost_total_mb,
        "max_rss_mb_per_cursor": max_rss,
        "rss_bytes": crate::metrics::rss_bytes(),
        "nests": roster_entries,
    });

    // The live handles: what makes the nest set changeable at runtime instead of frozen at boot
    // (RFC-0027). Everything the driver needs to re-compose the router lives here rather than being
    // moved into it and forgotten.
    let live = crate::serve::LiveRoost::new(crate::serve::compose_roost(
        roster.clone(),
        all_states.clone(),
        health.clone(),
    ));
    let handles = Arc::new(tokio::sync::Mutex::new(RoostHandles {
        live,
        states: all_states,
        alert_workers: std::mem::take(&mut alert_workers),
        lifecycle,
        health: health.clone(),
        roster,
        estimates: estimates.clone(),
        mount_ctx: MountContext {
            dir: dir.clone(),
            sources,
            backfill,
            seal_direct,
            concurrency,
            window_override,
            admin_enabled,
            admin_token: admin_token.clone(),
            max_rss_mb: max_rss,
        },
    }));

    // The server and the cursor supervisor race; whichever ends first decides the exit (RFC-0026 §6).
    // A *single* cursor's death no longer ends anything - that is the supervisor's job to absorb.
    let service = handles.lock().await.live.service().merge(lifecycle_routes(
        handles.clone(),
        admin_enabled,
        admin_token.clone(),
    ));
    let result = tokio::select! {
        r = crate::serve::bind_and_serve(&listen, service) => r,
        r = supervise_cursors(&mut ingests, &health, fail_fast) => r,
    };
    for (_, h) in &ingests {
        h.abort();
    }
    for (_, w) in &handles.lock().await.alert_workers {
        w.abort();
    }
    result
}

/// Watch every chain cursor, quarantining the ones that die instead of taking the roost down with them
/// (RFC-0026 §6, issue #147).
///
/// The old behaviour was `select_all` over the cursors: the **first** to finish - success or failure -
/// aborted every sibling and exited the process. So a reorg below finality on one chain tore down a
/// perfectly healthy cursor on another, which is precisely what `CLAUDE.md`'s per-cursor blast-radius
/// rule forbids. Now a dead cursor is retired from the set and logged; its nests keep serving the data
/// they had (frozen but correct - slice 3 marks them unhealthy so nobody mistakes it for fresh).
///
/// This returns - ending the roost - only when **every** cursor is gone, because at that point nothing
/// will ever advance again and a restart is the only thing that can help. Exiting non-zero under a
/// supervisor beats staying up serving permanently-frozen data.
async fn supervise_cursors(
    ingests: &mut Vec<(String, tokio::task::JoinHandle<Result<()>>)>,
    health: &crate::health::RoostHealth,
    fail_fast: bool,
) -> Result<()> {
    let total = ingests.len();
    let mut failures: Vec<String> = Vec::new();
    while !ingests.is_empty() {
        // Scope the borrow so the finished handle can be removed from the set afterwards.
        let (joined, idx) = {
            let (joined, idx, _rest) =
                futures::future::select_all(ingests.iter_mut().map(|(_, h)| h)).await;
            (joined, idx)
        };
        let (chain, _) = ingests.remove(idx);
        let outcome = match joined {
            Ok(inner) => inner,
            Err(e) if e.is_panic() => Err(anyhow::anyhow!("the ingestion loop panicked")),
            Err(e) => Err(anyhow::anyhow!("the ingestion loop task failed: {e}")),
        };
        match outcome {
            Ok(()) => tracing::info!(
                "roost cursor on {chain} finished cleanly; {} cursor(s) still indexing",
                ingests.len()
            ),
            Err(e) => {
                if fail_fast {
                    bail!("--fail-fast: roost cursor on {chain} died: {e:#}");
                }
                tracing::error!(
                    "roost cursor on {chain} QUARANTINED: {e:#} - its nests keep serving their last \
                     indexed state; {} sibling cursor(s) continue unaffected",
                    ingests.len()
                );
                // Every nest on this cursor is now out of service, however healthy it was itself.
                health.quarantine_cursor(&chain, format!("{e:#}"));
                failures.push(format!("{chain}: {e:#}"));
            }
        }
    }
    if failures.is_empty() {
        tracing::warn!("every roost cursor ({total}) finished cleanly - nothing left to index");
        return Ok(());
    }
    bail!(
        "every roost cursor is dead, so nothing will advance again - {}",
        failures.join("; ")
    )
}

/// The lifecycle control surface (RFC-0027 §5): mount and unmount a nest on a running roost.
///
/// Mounted on the **outer** router rather than the composed one, which is what avoids a cycle - the
/// inner composition is swapped underneath on every change, so routes living there would be replaced
/// by the very operation that invoked them.
///
/// Gated by the same credential as the admin UI via [`crate::serve::token_ok`], deliberately: who may
/// mount is the operator's gateway's decision, and a second auth concept here would be one more thing
/// to get subtly wrong. `--no-admin` removes these routes entirely, for operators who front their own
/// control plane and want the runtime to have no lifecycle surface at all.
pub fn lifecycle_routes(
    handles: Arc<tokio::sync::Mutex<RoostHandles>>,
    admin_enabled: bool,
    admin_token: Option<String>,
) -> axum::Router {
    use axum::extract::{Path as AxPath, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{delete, post};
    use axum::Json;

    #[derive(serde::Deserialize)]
    struct TokenQuery {
        token: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct MountBody {
        name: String,
    }

    type Shared = (Arc<tokio::sync::Mutex<RoostHandles>>, Option<String>);

    if !admin_enabled {
        return axum::Router::new();
    }

    /// Map a refusal to its status code (RFC-0027 §3). Typed rather than string-matched, so the
    /// mapping cannot drift from the reasons.
    fn status_for(err: &anyhow::Error) -> StatusCode {
        match err.downcast_ref::<MountRefusal>() {
            Some(MountRefusal::AlreadyMounted(_)) | Some(MountRefusal::UndeclaredChain { .. }) => {
                StatusCode::CONFLICT
            }
            Some(MountRefusal::OverBudget { .. }) => StatusCode::INSUFFICIENT_STORAGE,
            None => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    async fn mount_nest(
        State((handles, required)): State<Shared>,
        Query(q): Query<TokenQuery>,
        headers: HeaderMap,
        Json(body): Json<MountBody>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        if !crate::serve::token_ok(required.as_deref(), q.token.as_deref(), &headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "admin token required"})),
            );
        }
        let mut h = handles.lock().await;
        match h.mount(&body.name).await {
            Ok(()) => (
                StatusCode::OK,
                Json(serde_json::json!({"mounted": body.name})),
            ),
            Err(e) => (
                status_for(&e),
                Json(serde_json::json!({"error": format!("{e:#}")})),
            ),
        }
    }

    async fn unmount_nest(
        State((handles, required)): State<Shared>,
        AxPath(name): AxPath<String>,
        Query(q): Query<TokenQuery>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<serde_json::Value>) {
        if !crate::serve::token_ok(required.as_deref(), q.token.as_deref(), &headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "admin token required"})),
            );
        }
        let mut h = handles.lock().await;
        match h.unmount(&name).await {
            Ok(()) => (StatusCode::OK, Json(serde_json::json!({"unmounted": name}))),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("{e:#}")})),
            ),
        }
    }

    axum::Router::new()
        .route("/_admin/nests", post(mount_nest))
        .route("/_admin/nests/{name}", delete(unmount_nest))
        .with_state((handles, admin_token))
}

/// Persist the mounted-nest list to `roost.toml` (RFC-0027 §5).
///
/// This is the embedded stand-in for RFC-0022's control-plane DB: desired state lives in the *same*
/// file the static path reads, so a restart converges on whatever the operator last asked for. Without
/// it, a mount would silently vanish on the next restart - the worst kind of bug, because it looks
/// like it worked.
///
/// Written temp-then-rename so a crash mid-write cannot leave a roost with a truncated manifest and no
/// nests at all.
///
/// The conflict this creates is named rather than left to be discovered: **at runtime nuthatch owns
/// this list.** An operator who manages `roost.toml` with configuration management should run
/// `--no-admin` and restart to change the set, because fighting a config-management tool over a file
/// is a losing game.
fn persist_mounted_nests(dir: &Path, nests: &[String]) -> Result<()> {
    let path = dir.join(ROOST_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} to persist the nest list", path.display()))?;
    let mut roost: Roost = toml::from_str(&raw)
        .with_context(|| format!("parsing {} before rewriting it", path.display()))?;
    roost.roost.nests = nests.to_vec();
    let out = toml::to_string_pretty(&roost).context("serialising roost.toml")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// The handles a roost driver keeps so it can change its nest set while running (RFC-0027 §6).
///
/// Before this, `roost::dev` moved every `AppState` into the composed router and kept nothing, so the
/// only way to change the mounted set was to restart the process - which stops every co-tenant nest
/// too. Retaining them is what makes an unmount possible at all.
pub struct RoostHandles {
    /// The swappable composition being served (RFC-0027 slice 1).
    pub live: crate::serve::LiveRoost,
    /// Per-nest serving state, in roster order.
    pub states: Vec<(String, crate::serve::AppState)>,
    /// Alert delivery workers keyed by nest - each holds that nest's `Store` clone.
    pub alert_workers: Vec<(String, tokio::task::JoinHandle<()>)>,
    /// Chain -> that cursor's command channel.
    pub lifecycle: std::collections::HashMap<
        String,
        tokio::sync::mpsc::UnboundedSender<indexer::CursorCommand>,
    >,
    pub health: Arc<crate::health::RoostHealth>,
    /// The static half of the roster, re-merged with live health per request.
    pub roster: serde_json::Value,
    /// Per-nest projected RSS, so a mount can price the cursor it is joining without re-reading
    /// every co-tenant's config.
    pub estimates: std::collections::HashMap<String, u64>,
    /// What a mount needs that an unmount does not: where nests live, how to reach each chain, and the
    /// settings a new nest must be built with so it behaves identically to one mounted at boot.
    pub mount_ctx: MountContext,
}

/// The context a running roost needs in order to build and admit a nest (RFC-0027 §3).
///
/// Deliberately captured at startup rather than re-derived per mount: a nest mounted at 3am must be
/// built with the same backfill mode, concurrency, window and admin posture as its co-tenants, or two
/// nests in one roost would behave differently for no reason an operator could see.
#[derive(Clone)]
pub struct MountContext {
    /// The roost directory; a nest lives at `nests/<name>/`.
    pub dir: PathBuf,
    /// Chain -> the source driving that chain's cursor. A nest whose chain is absent cannot be mounted.
    pub sources: std::collections::HashMap<String, Arc<dyn Source>>,
    pub backfill: Option<u64>,
    pub seal_direct: bool,
    pub concurrency: usize,
    pub window_override: Option<u64>,
    pub admin_enabled: bool,
    pub admin_token: Option<String>,
    /// The per-cursor RSS ceiling a mount must not breach (`CLAUDE.md`; RFC-0021 §0).
    pub max_rss_mb: u64,
}

/// Why a mount was refused (RFC-0027 §3). Typed so the control surface can map each to its status
/// code without parsing strings.
#[derive(Debug)]
pub enum MountRefusal {
    /// Mounting over a live name is an *upgrade*, and that is RFC-0020's job.
    AlreadyMounted(String),
    /// The roost declares no cursor for this nest's chain. Adding a chain at runtime is a non-goal.
    UndeclaredChain { nest: String, chain: String },
    /// The cursor's projected footprint would exceed its ceiling. A refusal, not a warning - the
    /// budget stops being a budget the moment it becomes advisory.
    OverBudget {
        nest: String,
        chain: String,
        projected_mb: u64,
        ceiling_mb: u64,
    },
}

impl std::fmt::Display for MountRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MountRefusal::AlreadyMounted(n) => write!(
                f,
                "nest '{n}' is already mounted - changing a mounted nest is `nest upgrade`, not a mount"
            ),
            MountRefusal::UndeclaredChain { nest, chain } => write!(
                f,
                "nest '{nest}' is on {chain}, which this roost declares no cursor for - add it under \
                 [[chains]] and restart"
            ),
            MountRefusal::OverBudget {
                nest,
                chain,
                projected_mb,
                ceiling_mb,
            } => write!(
                f,
                "mounting '{nest}' would put the {chain} cursor at ~{projected_mb} MB against a \
                 {ceiling_mb} MB ceiling - raise max_rss_mb, unmount something, or use another roost"
            ),
        }
    }
}

impl std::error::Error for MountRefusal {}

/// How long to wait for a cursor to acknowledge that it has released a nest.
///
/// Generous, because the cursor applies lifecycle commands at a **window boundary** - it may be
/// mid-window against a slow provider when the command arrives. Timing out is not a failure of the
/// unmount so much as a refusal to guess: we would rather report that the cursor has not let go than
/// tear the routes down while it is still writing.
const UNMOUNT_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl RoostHandles {
    /// Mount a nest into the running roost (RFC-0027 §3-§4).
    ///
    /// Admission first, work second: every refusal is decided before a store is opened or a block is
    /// fetched, so a rejected mount costs nothing and leaves nothing behind.
    ///
    /// Then phase 1 - build and `prepare` the nest **outside** the cursor, so it catches up on its own
    /// before joining. Phase 2 hands it over at a window boundary. Doing it the other way round would
    /// drag every co-tenant back to the new nest's start block, because the cursor advances from the
    /// min of its live nests.
    ///
    /// Routes appear only after the cursor has acknowledged, so a nest is never reachable before it is
    /// actually indexing.
    pub async fn mount(&mut self, name: &str) -> Result<()> {
        if self.states.iter().any(|(n, _)| n == name) {
            return Err(MountRefusal::AlreadyMounted(name.to_string()).into());
        }
        let dir = Roost::nest_dir(&self.mount_ctx.dir, name);
        let config = Config::load(&dir)
            .with_context(|| format!("loading nest '{name}' from {}", dir.display()))?;
        let chain = config.nest.chain.clone();

        let Some(source) = self.mount_ctx.sources.get(&chain).cloned() else {
            return Err(MountRefusal::UndeclaredChain {
                nest: name.to_string(),
                chain,
            }
            .into());
        };
        let Some(lifecycle) = self.lifecycle.get(&chain).cloned() else {
            return Err(MountRefusal::UndeclaredChain {
                nest: name.to_string(),
                chain,
            }
            .into());
        };

        // The budget check is the reason this is a refusal rather than a warning: `CLAUDE.md`'s
        // per-cursor ceiling stops being a budget the moment a mount may quietly exceed it. Projected
        // against *this cursor's* current membership, not the whole roost - the ceiling is per cursor.
        let has_labels = !crate::labels::load(&dir).is_empty();
        let incoming = estimate_nest_rss_mb(&config, has_labels);
        let existing: u64 = self
            .states
            .iter()
            .filter(|(_, s)| s.chain == chain)
            .map(|(n, _)| self.estimates.get(n).copied().unwrap_or(NEST_BASE_RSS_MB))
            .sum();
        let projected = ROOST_BASE_RSS_MB + existing + incoming;
        if projected > self.mount_ctx.max_rss_mb {
            return Err(MountRefusal::OverBudget {
                nest: name.to_string(),
                chain,
                projected_mb: projected,
                ceiling_mb: self.mount_ctx.max_rss_mb,
            }
            .into());
        }

        // Phase 1: build and catch up, off to one side of the cursor.
        let (nest, mut state, worker, next) = indexer::build_and_prepare_nest(
            &source,
            dir,
            &config,
            self.mount_ctx.backfill,
            self.mount_ctx.seal_direct,
            self.mount_ctx.concurrency,
            self.mount_ctx.window_override,
            self.mount_ctx.admin_enabled,
            self.mount_ctx.admin_token.clone(),
        )
        .await
        .with_context(|| format!("preparing nest '{name}' for mount"))?;
        state.roost_health = Some((name.to_string(), self.health.clone()));

        // Phase 2: hand it to the cursor at a window boundary, and wait for it to be in the set.
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        lifecycle
            .send(indexer::CursorCommand::Mount {
                nest: Box::new(nest),
                next,
                ack: Some(ack_tx),
            })
            .map_err(|_| anyhow::anyhow!("cursor on {chain} is gone; cannot mount '{name}'"))?;
        tokio::time::timeout(UNMOUNT_ACK_TIMEOUT, ack_rx)
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "cursor on {chain} did not acknowledge mounting '{name}' within {}s",
                    UNMOUNT_ACK_TIMEOUT.as_secs()
                )
            })?
            .map_err(|_| anyhow::anyhow!("cursor on {chain} stopped while mounting '{name}'"))?;

        // Only now do the routes appear.
        if let Some(worker) = worker {
            self.alert_workers.push((name.to_string(), worker));
        }
        self.estimates.insert(name.to_string(), incoming);
        self.states.push((name.to_string(), state));
        self.live.swap(crate::serve::compose_roost(
            self.roster.clone(),
            self.states.clone(),
            self.health.clone(),
        ));
        self.persist();
        tracing::info!("nest '{name}' mounted onto the {chain} cursor at block {next}");
        Ok(())
    }

    /// Write the current mounted set to `roost.toml`.
    ///
    /// Best-effort by design: the mount or unmount has *already happened* in the running process, and
    /// failing the operation because the manifest could not be rewritten would leave the caller with a
    /// reported failure and a completed change - the worst of both. A loud warning is the honest
    /// outcome, and the operator can fix the file.
    fn persist(&self) {
        let names: Vec<String> = self.states.iter().map(|(n, _)| n.clone()).collect();
        if let Err(e) = persist_mounted_nests(&self.mount_ctx.dir, &names) {
            tracing::warn!(
                "the roost's nest set changed but {ROOST_FILE} could not be updated ({e:#}) - the \
                 change is live now but will not survive a restart"
            );
        }
    }

    /// Unmount a nest: drain its cursor, release every handle to its store, then remove its routes.
    ///
    /// The ordering is the contract (RFC-0027 §6). The cursor is asked first and acknowledged before
    /// anything is torn down, because a route removed while the cursor is still committing a window
    /// would leave the nest writing data nobody can read - and, worse, would make "the store is
    /// closed" a race rather than a fact.
    ///
    /// Three holders of the nest's `Store` must drop before redb releases the file: the cursor's (via
    /// the ack), the alert delivery worker's (aborted here), and the serving state's (dropped when the
    /// router is re-composed without it). Miss any one and the file stays locked - which is exactly
    /// what the acceptance test checks, by reopening it.
    ///
    /// Idempotent: unmounting a nest that is not mounted is a no-op, not an error.
    pub async fn unmount(&mut self, name: &str) -> Result<()> {
        let Some(idx) = self.states.iter().position(|(n, _)| n == name) else {
            tracing::debug!("nest '{name}' is not mounted; nothing to unmount");
            return Ok(());
        };
        let chain = self.states[idx].1.chain.clone();

        // 1. Drain the cursor and wait for it to let go.
        //
        // No channel for this nest's chain means we cannot ask the cursor to stop, and tearing the
        // routes down regardless would leave it writing to a store nobody can read - the exact failure
        // §6 orders this sequence to prevent. So this is an error, not a skip. (An early draft skipped
        // silently; the acceptance test then failed on a *held* store, which is how the gap surfaced.)
        {
            let tx = self.lifecycle.get(&chain).ok_or_else(|| {
                anyhow::anyhow!(
                    "no cursor channel for chain '{chain}' hosting '{name}' - refusing to unmount \
                     without draining it first"
                )
            })?;
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            if tx
                .send(indexer::CursorCommand::Unmount {
                    name: name.to_string(),
                    ack: Some(ack_tx),
                })
                .is_ok()
            {
                match tokio::time::timeout(UNMOUNT_ACK_TIMEOUT, ack_rx).await {
                    Ok(Ok(())) => {}
                    // A closed channel means the cursor is already gone, which is as released as it
                    // gets. A timeout is not: report it rather than tearing down regardless.
                    Ok(Err(_)) => tracing::debug!("cursor on {chain} already stopped"),
                    Err(_) => bail!(
                        "cursor on {chain} did not acknowledge unmounting '{name}' within {}s - \
                         refusing to remove its routes while it may still be writing",
                        UNMOUNT_ACK_TIMEOUT.as_secs()
                    ),
                }
            }
        }

        // 2. Stop and drop the nest's alert worker - the second holder of its store.
        if let Some(pos) = self.alert_workers.iter().position(|(n, _)| n == name) {
            let (_, worker) = self.alert_workers.remove(pos);
            worker.abort();
            // `abort()` only *requests* cancellation - the task keeps its `Store` clone until the
            // runtime actually drops it. Awaiting the handle waits for that to have happened. Skipping
            // this makes the release a race: the acceptance test caught it on the first run, failing
            // with "Database already open" a few microseconds after the abort.
            let _ = worker.await;
        }

        // 3. Drop the serving state - the third - and re-compose without it. Requests already in
        //    flight finish against the old composition; new ones 404.
        self.states.remove(idx);
        self.live.swap(crate::serve::compose_roost(
            self.roster.clone(),
            self.states.clone(),
            self.health.clone(),
        ));
        self.persist();
        tracing::info!("nest '{name}' unmounted from the roost");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CONFIG_FILE;

    /// Write a minimal roost.toml + one nest dir on the given chain.
    fn write_roost(dir: &Path, chain: &str, chain_id: u64, nest_chain: &str, nest_chain_id: u64) {
        std::fs::write(
            dir.join(ROOST_FILE),
            format!(
                "[roost]\nname = \"test\"\nchain = \"{chain}\"\nchain_id = {chain_id}\n\
                 rpc_urls = [\"http://localhost:8545\"]\nnests = [\"a\"]\n"
            ),
        )
        .unwrap();
        let nest = Roost::nest_dir(dir, "a");
        std::fs::create_dir_all(&nest).unwrap();
        std::fs::write(
            nest.join(CONFIG_FILE),
            format!(
                "[nest]\nname = \"a\"\nchain = \"{nest_chain}\"\nchain_id = {nest_chain_id}\n\
                 rpc_urls = []\n\n[[contracts]]\nalias = \"t\"\naddress = \"0x0000000000000000000000000000000000000001\"\nabi = \"abi.json\"\n"
            ),
        )
        .unwrap();
        // A trivially-valid ABI so Config::load's downstream users don't choke (load itself doesn't read it).
        std::fs::write(nest.join("abi.json"), "[]").unwrap();
    }

    /// Write a nest dir on a given chain under a roost (for multichain grouping tests).
    fn write_nest_dir(roost_dir: &Path, name: &str, chain: &str, chain_id: u64) {
        let nest = Roost::nest_dir(roost_dir, name);
        std::fs::create_dir_all(&nest).unwrap();
        std::fs::write(
            nest.join(CONFIG_FILE),
            format!(
                "[nest]\nname = \"{name}\"\nchain = \"{chain}\"\nchain_id = {chain_id}\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"t\"\naddress = \"0x0000000000000000000000000000000000000001\"\nabi = \"abi.json\"\n"
            ),
        )
        .unwrap();
        std::fs::write(nest.join("abi.json"), "[]").unwrap();
    }

    fn mounted(roost_dir: &Path, name: &str) -> (String, PathBuf, Config) {
        let (p, c) = load_mounted_nest(roost_dir, name).unwrap();
        (name.to_string(), p, c)
    }

    #[test]
    fn loads_a_valid_roost() {
        let d = tempfile::tempdir().unwrap();
        write_roost(d.path(), "arbitrum-one", 42161, "arbitrum-one", 42161);
        let r = Roost::load(d.path()).unwrap();
        assert_eq!(r.roost.chain.as_deref(), Some("arbitrum-one"));
        assert_eq!(r.roost.nests, vec!["a"]);
        // A single-chain roost resolves to exactly one endpoint.
        assert_eq!(r.chain_endpoints().unwrap().len(), 1);
    }

    #[test]
    fn rejects_a_nest_whose_chain_isnt_declared() {
        let d = tempfile::tempdir().unwrap();
        // Roost declares arbitrum-one; the nest claims mainnet → hard error at grouping.
        write_roost(d.path(), "arbitrum-one", 42161, "mainnet", 1);
        let roost = Roost::load(d.path()).unwrap();
        let err = group_by_chain(
            &roost.chain_endpoints().unwrap(),
            vec![mounted(d.path(), "a")],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("doesn't declare"), "got: {err}");
    }

    #[test]
    fn multichain_roost_groups_nests_by_chain() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"multi\"\nnests = [\"a\", \"b\"]\n\n\
             [[chains]]\nchain = \"base\"\nchain_id = 8453\nrpc_urls = [\"http://base\"]\n\n\
             [[chains]]\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = [\"http://arb\"]\n",
        )
        .unwrap();
        write_nest_dir(d.path(), "a", "base", 8453);
        write_nest_dir(d.path(), "b", "arbitrum-one", 42161);
        let roost = Roost::load(d.path()).unwrap();
        let endpoints = roost.chain_endpoints().unwrap();
        assert_eq!(endpoints.len(), 2, "two declared chains");
        let groups = group_by_chain(
            &endpoints,
            vec![mounted(d.path(), "a"), mounted(d.path(), "b")],
        )
        .unwrap();
        assert_eq!(groups.len(), 2, "one cursor per chain");
        for g in &groups {
            assert_eq!(g.nests.len(), 1, "each chain has its one nest");
        }
    }

    #[test]
    fn rejects_both_top_level_and_multichain_forms() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"x\"\nchain = \"base\"\nchain_id = 8453\nrpc_urls = [\"u\"]\nnests = [\"a\"]\n\n\
             [[chains]]\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = [\"v\"]\n",
        )
        .unwrap();
        let roost = Roost::load(d.path()).unwrap();
        let err = roost.chain_endpoints().unwrap_err().to_string();
        assert!(
            err.contains("both a top-level chain and [[chains]]"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unsafe_nest_names() {
        // SEC-10: a nest name that could escape the nests dir or make a surprising route is refused.
        for bad in ["../etc", "a/b", "", "has space"] {
            let d = tempfile::tempdir().unwrap();
            std::fs::write(
                d.path().join(ROOST_FILE),
                format!("[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"{bad}\"]\n"),
            )
            .unwrap();
            let err = Roost::load(d.path()).unwrap_err().to_string();
            assert!(
                err.contains("invalid") || err.contains("reserved"),
                "name {bad:?} should be rejected, got: {err}"
            );
        }
    }

    #[test]
    fn rejects_reserved_and_duplicate_nest_names() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"nests\"]\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("reserved"));

        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"a\", \"a\"]\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("more than once"));
    }

    #[test]
    fn rejects_an_empty_nest_list() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(ROOST_FILE),
            "[roost]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = []\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("no nests"));
    }

    #[test]
    fn footprint_estimate_scales_with_views() {
        fn cfg(extra: &str) -> Config {
            let toml = format!(
                "[nest]\nname = \"n\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = []\n\n\
                 [[contracts]]\nalias = \"t\"\naddress = \"0x1\"\nabi = \"a.json\"\n{extra}"
            );
            toml::from_str(&toml).unwrap()
        }
        // Plain static nest, no labels: just the per-nest base.
        assert_eq!(estimate_nest_rss_mb(&cfg(""), false), NEST_BASE_RSS_MB);
        // Labels present → the exposure view adds a chunk.
        assert_eq!(
            estimate_nest_rss_mb(&cfg(""), true),
            NEST_BASE_RSS_MB + NEST_VIEW_RSS_MB
        );
        // A velocity flag → the velocity view.
        let vel = cfg("\n[flags]\nvelocity_amount = \"1000\"\n");
        assert_eq!(
            estimate_nest_rss_mb(&vel, false),
            NEST_BASE_RSS_MB + NEST_VIEW_RSS_MB
        );
        // A factory → the discovered-child registry.
        let fac = cfg("\n[[templates]]\nname = \"p\"\nabi = \"p.json\"\n\n\
             [[factories]]\nwatch = \"t\"\nevent = \"E\"\nchild_param = \"c\"\ntemplate = \"p\"\n");
        assert_eq!(
            estimate_nest_rss_mb(&fac, false),
            NEST_BASE_RSS_MB + NEST_VIEW_RSS_MB
        );
        // All three loads stack on top of the base.
        let all = cfg(
            "\n[flags]\nvelocity_amount = \"1000\"\n\n[[templates]]\nname = \"p\"\nabi = \"p.json\"\n\n\
             [[factories]]\nwatch = \"t\"\nevent = \"E\"\nchild_param = \"c\"\ntemplate = \"p\"\n",
        );
        assert_eq!(
            estimate_nest_rss_mb(&all, true),
            NEST_BASE_RSS_MB + 3 * NEST_VIEW_RSS_MB
        );
    }

    /// Issue #147, the headline scenario and the acceptance test for RFC-0026: one chain's cursor dies
    /// (a reorg below its sealed watermark), and the other chain's cursor must carry on indexing with
    /// the process still up. Before this, `select_all` returned on the first cursor's death, aborted
    /// every sibling, and exited - so a Base reorg took down a perfectly healthy Arbitrum cursor.
    #[tokio::test]
    async fn a_dead_cursor_does_not_take_its_siblings_down() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::Duration;

        let doomed = tokio::spawn(async {
            Err::<(), anyhow::Error>(anyhow::anyhow!(
                "reorg to block 100 is below the sealed/finalized watermark 200 - a finality \
                 violation this indexer cannot repair"
            ))
        });
        // A cursor that keeps working, ticking a counter so "still indexing" is observable rather
        // than merely "not yet finished".
        let progress = Arc::new(AtomicU64::new(0));
        let p = progress.clone();
        let healthy = tokio::spawn(async move {
            for _ in 0..10_000 {
                p.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(())
        });

        let mut ingests = vec![
            ("base".to_string(), doomed),
            ("arbitrum-one".to_string(), healthy),
        ];
        // The supervisor must NOT return while a healthy cursor is still indexing: returning is what
        // ends the roost.
        let health = crate::health::RoostHealth::new();
        health.register("nest-a", "base");
        health.register("nest-b", "arbitrum-one");
        let returned = tokio::time::timeout(
            Duration::from_millis(250),
            supervise_cursors(&mut ingests, &health, false),
        )
        .await;
        assert!(
            returned.is_err(),
            "the roost ended even though a healthy cursor was still indexing"
        );

        // The dead cursor was retired from the set; the healthy one is untouched and still working.
        assert_eq!(ingests.len(), 1, "only the dead cursor should be retired");
        assert_eq!(ingests[0].0, "arbitrum-one");
        assert!(
            !ingests[0].1.is_finished(),
            "the surviving cursor must not have been aborted"
        );
        assert!(
            progress.load(Ordering::Relaxed) > 0,
            "the surviving cursor must keep making progress after its sibling died"
        );
        // The health surface tells the truth about both: the dead chain's nest is quarantined, the
        // living chain's is not (RFC-0026 §5).
        assert_eq!(health.json_for("nest-a").0, "quarantined");
        assert_eq!(health.json_for("nest-b").0, "indexing");
        assert!(!health.all_indexing(), "a partly-broken roost is not ready");
        ingests[0].1.abort();
    }

    /// RFC-0026 §6: the roost exits only once **every** cursor is gone - at that point nothing will
    /// ever advance again, so exiting non-zero under a supervisor beats serving permanently-frozen
    /// data. The error must name every dead chain, since that is the operator's starting point.
    #[tokio::test]
    async fn the_roost_exits_when_the_last_cursor_dies_and_names_every_chain() {
        let a =
            tokio::spawn(async { Err::<(), anyhow::Error>(anyhow::anyhow!("finality violation")) });
        let b = tokio::spawn(async {
            Err::<(), anyhow::Error>(anyhow::anyhow!("a single block exceeds the response cap"))
        });
        let mut ingests = vec![("base".to_string(), a), ("arbitrum-one".to_string(), b)];

        let health = crate::health::RoostHealth::new();
        let err = supervise_cursors(&mut ingests, &health, false)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("base"),
            "should name the first dead chain: {msg}"
        );
        assert!(
            msg.contains("arbitrum-one"),
            "should name the second dead chain: {msg}"
        );
        assert!(ingests.is_empty(), "every cursor should have been retired");
    }
}
