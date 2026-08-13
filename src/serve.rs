//! The API surface. Point-reads hit redb directly (the hot path). Everything is local; nothing
//! phones home. This is where the MCP server and SQL surface will grow in later slices.

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures::stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use crate::analytics;
use crate::exposure::ExposureView;
use crate::registry::TableSchema;
use crate::velocity::VelocityView;
use crate::views::BalanceView;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// How many analytical (DuckDB) queries may run at once across `/sql` and cold `/table` reads. Each
/// DuckDB query is already capped at 512 MB / 2 threads (see `analytics`), so this bounds the whole
/// analytical surface's worst-case footprint - the real DoS multiplier is *concurrency*, not any one
/// query. Kept small to stay well inside the embedded RAM budget; this is node self-protection, not
/// per-caller rate-limiting (that needs identity and belongs in a gateway).
pub const SQL_MAX_CONCURRENCY: usize = 2;
/// Wall-clock deadline for a single analytical query; a runaway (e.g. cartesian) is interrupted.
const SQL_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on rows materialised from one analytical query - bounds the Rust-side result buffer, which
/// lives outside DuckDB's own memory limit. Beyond this the result is truncated and flagged.
const SQL_MAX_ROWS: usize = 50_000;

/// The most unsealed rows `/sql` will materialise for one query.
///
/// The hot store holds everything between the sealed watermark and the tip, and every `/sql` call
/// parses all of it into memory. On a deep-finality chain with a busy contract that is the largest RAM
/// risk the process carries - and in a runtime it is a co-tenant's problem too, because the budget is
/// per cursor. This is the ceiling that turns "the box fell over" into a `503` naming the reason.
///
/// It is generous on purpose: a nest at tip on a normal chain is nowhere near it, so the guard is
/// invisible until something is genuinely wrong. Under the CLAUDE.md division of labour resource
/// safety is the node's job, not the gateway's - which is why this is a hard limit here rather than
/// advice in the docs.
///
/// `pub(crate)` rather than private: it is the default [`AppState::sql_max_hot_rows`] is built
/// with, both in `indexer.rs` and in `test_state` (#378) - a real handler test can lower the seam
/// instead of genuinely putting two million rows in the hot store to reach the 503 arm.
pub(crate) const SQL_MAX_HOT_ROWS: usize = 2_000_000;
/// Reject absurdly long query strings before they reach the planner.
const SQL_MAX_QUERY_LEN: usize = 16 * 1024;

#[derive(Clone)]
pub struct AppState {
    /// The hot store behind [`crate::store::HotStore`] (RFC-0022 slice 1). `Arc<dyn _>` rather than a
    /// concrete `Store` because serving must not care which backend answers - that is the whole point
    /// of the seam, and an FE node under RFC-0022 §1 will be handed a Postgres-backed one.
    pub store: std::sync::Arc<dyn crate::store::HotStore>,
    /// The first declared contract's address - `None` for a nest that declares none.
    ///
    /// A contract-free `[extract] blocks = true` nest is a supported shape (RFC-0036 §4.2, OBIB
    /// case 3), and this field being `String` is what refused to let one start: `build_nest` read
    /// it through `Config::primary()?`, which errors "nest has no contracts", so the operator-facing
    /// path rejected an entire configuration over a field only `GET /`'s summary reads. Optional
    /// rather than an arbitrary pick, because a nest with no contracts has no address to name and
    /// saying so is the honest answer; a nest with several has always shown only its first here.
    pub address: Option<String>,
    pub chain: String,
    pub dir: PathBuf,
    pub balances: BalanceView,
    /// Direct counterparty-exposure to the labeled set (RFC-0008 C1) - served at `/exposure/{addr}`.
    pub exposure: ExposureView,
    /// Windowed per-address velocity view (RFC-0008 C3) - served at `/flags?kind=velocity`.
    pub velocity: VelocityView,
    /// Single-transfer threshold in base units, if configured (RFC-0008 C3) - for `/`'s flag summary.
    pub threshold: Option<i128>,
    /// Velocity flag threshold in base units, if configured - the cutoff `/flags?kind=velocity` uses.
    pub velocity_threshold: Option<i128>,
    /// Whether the built-in admin UI (`/_admin/`) is served (RFC-0010 Part A).
    pub admin_enabled: bool,
    /// When the bind is off-localhost, the token a request must present (`?token=…`) to reach the admin
    /// UI (SEC-5). `None` on a localhost bind (open) - the env var merely *enabling* the route off-
    /// localhost, without checking it per request, was security theater.
    pub admin_token: Option<String>,
    /// Static nest metadata for the admin UI's Nest tab (`/nest`): contracts, templates, factories,
    /// webhooks, registry hash. Computed once at startup.
    pub nest_info: Arc<serde_json::Value>,
    /// The nest's table schemas (from the decode registry) - the source of truth for `/tables`.
    pub tables: Arc<Vec<TableSchema>>,
    /// Admission control for the analytical (DuckDB) surface: bounds how many `/sql` and cold
    /// `/table` queries run at once so a burst can't multiply DuckDB's per-query footprint past the
    /// process budget. Constructed with [`SQL_MAX_CONCURRENCY`] permits.
    pub sql_gate: Arc<Semaphore>,
    /// The most unsealed rows `/sql` and `/explain` will materialise for one query before refusing
    /// with a `503` (`HotScanTooLarge`) - the live value [`SQL_MAX_HOT_ROWS`] documents. A field
    /// rather than the handlers reading the const directly, so a test can lower the ceiling instead
    /// of genuinely putting two million rows in the hot store to reach the refusal arm (#378).
    pub sql_max_hot_rows: usize,
    /// The SQL surface this mount exposes (RFC-0034). Default is [`Open`](crate::allowlist::SqlAccess::Open) -
    /// arbitrary `/sql`, exactly as before - because a local `nuthatch dev` is an exploration tool and
    /// a security control that turns itself on is a support ticket.
    pub surface: Arc<crate::allowlist::Surface>,
    /// The identity of the dataset serving this mount (RFC-0032 §3), stamped into `provenance` so an
    /// answer can be cited against the data that produced it. `None` for a solo `dev` nest, which has
    /// no mount record and therefore no identity to report.
    pub nid: Option<Arc<str>>,
    /// This nest's name and the runtime health surface it should answer `/ready` from (RFC-0026 §5).
    /// `None` for a solo `dev` nest, which has no mounts around it and falls back to the global
    /// poll-freshness check.
    pub runtime_health: Option<(String, Arc<crate::health::RuntimeHealth>)>,
}

/// A hot-swappable handle to the `AppState` backing one served endpoint (RFC-0020 slice 2). The router
/// binds this instead of a fixed `AppState`; every request resolves the *current* version through
/// [`FromRef`](axum::extract::FromRef), so a compatible upgrade can **atomically re-point** the
/// endpoint at a new version with no rebind and no dropped request (in-flight requests finish against
/// the version they started on). Cheap: a lock-free atomic load plus the same per-request `AppState`
/// clone axum already does. The flip is the mechanism slice 2b's "index new, swap when caught up"
/// orchestration drives; this slice lands and proves the mechanism.
#[derive(Clone)]
pub struct SharedNest(Arc<arc_swap::ArcSwap<AppState>>);

impl SharedNest {
    /// A handle initially backing `state`.
    pub fn new(state: AppState) -> SharedNest {
        SharedNest(Arc::new(arc_swap::ArcSwap::from_pointee(state)))
    }

    /// Atomically re-point this endpoint at a new backing version. The next request sees it.
    pub fn swap(&self, state: AppState) {
        self.0.store(Arc::new(state));
    }

    /// The current backing (a cheap `Arc` clone) - what serving resolves per request.
    pub fn current(&self) -> Arc<AppState> {
        self.0.load_full()
    }
}

// Lets every existing `State<AppState>` handler stay untouched: axum resolves `AppState` from the
// swappable `SharedNest` per request, always seeing the current version.
impl axum::extract::FromRef<SharedNest> for AppState {
    fn from_ref(shared: &SharedNest) -> AppState {
        (*shared.0.load_full()).clone()
    }
}

/// Build a nest's router - every per-nest route plus the request-count layer, bound to a swappable
/// [`SharedNest`]. Split out of [`run`] so a runtime (RFC-0012) can mount many of these under
/// `/<nest>/…` prefixes; a solo `dev` serves exactly one at the root. Identical routes either way - a
/// nest can't tell it's co-hosted, nor that its backing can be hot-swapped underneath it.
///
/// The one route set that is *not* identical either way is the admin UI: with the surface disabled
/// (`--no-admin`, or a public bind with no token) the `/_admin*` routes are never registered, so those
/// paths fall through to the ordinary not-found - which is what `--no-admin` promises and what the
/// runtime's [`crate::runtime::lifecycle_routes`] already did for the mount/unmount half. Handing back
/// a 404 *in the admin UI's own words* still tells an unauthenticated caller that this is a nuthatch
/// with the admin surface switched off (#292).
pub fn router(backing: SharedNest) -> Router {
    // Read once, at composition time: `admin_enabled` is derived from the process's flags and bind
    // (`indexer::admin_enabled`), so it is constant for the life of the endpoint - a hot swap (RFC-0020
    // slice 2) re-points the *data*, never the admin decision.
    let admin_enabled = backing.current().admin_enabled;
    let admin = |r: Router<SharedNest>| {
        if !admin_enabled {
            return r;
        }
        r.route("/_admin", get(admin_index))
            .route("/_admin/", get(admin_index))
            .route("/_admin/events", get(admin_events))
    };
    admin(
        Router::new()
            .route("/", get(summary))
            .route("/health", get(|| async { "ok" }))
            .route("/ready", get(ready))
            .route("/metrics", get(metrics_handler))
            .route("/tables", get(tables))
            .route("/schema", get(schema_doc))
            .route("/table/{name}", get(table))
            .route("/entities", get(entities))
            .route("/entity/{id}", get(entity))
            .route("/sql", get(sql))
            .route("/explain", get(explain))
            .route("/queries", get(queries))
            .route("/q/{name}", get(named_query))
            .route("/balances", get(balances))
            .route("/balance/{address}", get(balance))
            .route("/exposure/{address}", get(exposure))
            .route("/flags", get(flags))
            .route("/nest", get(nest))
            .route("/shape", get(shape)),
    )
    // Count every served request for `/metrics` (the operator's billing signal).
    .layer(axum::middleware::from_fn(count_request))
    .with_state(backing)
}

pub async fn run(listen: &str, state: AppState) -> Result<()> {
    run_shared(listen, SharedNest::new(state)).await
}

/// Serve a caller-held [`SharedNest`] - the variant a hot upgrade uses so it can keep the handle and
/// atomically flip the backing (RFC-0020 slice 2b) while serving stays up on the same listener.
pub async fn run_shared(listen: &str, shared: SharedNest) -> Result<()> {
    bind_and_serve(listen, router(shared)).await
}

/// Serve **two versions** of a nest on distinct endpoints behind one listener (RFC-0020 slice 3, the
/// breaking path). The OLD version stays at its existing **root** path - unchanged, so current
/// consumers keep working - but every response now carries a `Deprecation: true` header and a `Link`
/// to its successor; the NEW version is served under `new_prefix` (e.g. `/next`). Both index
/// concurrently and neither flips: downstream migrate from the old endpoint to the new on their own
/// clock, then the operator sunsets the old. The deprecation signal is standards-shaped (RFC 8594).
pub async fn run_two_versions(
    listen: &str,
    old: SharedNest,
    new_prefix: &str,
    new: SharedNest,
) -> Result<()> {
    bind_and_serve(listen, two_version_router(old, new_prefix, new)).await
}

/// The two-version app (RFC-0020 slice 3): old at the root, wrapped in a `Deprecation`/`Link` layer;
/// new nested under `new_prefix`. Split out of [`run_two_versions`] so it's testable over HTTP.
pub fn two_version_router(old: SharedNest, new_prefix: &str, new: SharedNest) -> Router {
    let successor = format!("{new_prefix}/");
    let deprecate = axum::middleware::from_fn(
        move |req: axum::extract::Request, next: axum::middleware::Next| {
            let successor = successor.clone();
            async move {
                let mut resp = next.run(req).await;
                let headers = resp.headers_mut();
                headers.insert("deprecation", axum::http::HeaderValue::from_static("true"));
                if let Ok(link) = axum::http::HeaderValue::from_str(&format!(
                    "<{successor}>; rel=\"successor-version\""
                )) {
                    headers.insert("link", link);
                }
                resp
            }
        },
    );
    Router::new()
        .nest(new_prefix, router(new))
        .merge(router(old).layer(deprecate))
}

/// Serve many nests behind one listener (RFC-0012 mounts, slice 1): a `/nests` roster plus every nest's
/// full API under its `/<name>/…` prefix. Chain identity and the cursor are still per-nest at this
/// slice (the shared cursor is slice 2); this lands the routing + per-nest isolation of the serving
/// surface first. Each nest's routes are byte-identical to a solo `dev`, just prefixed.
pub async fn run_runtime(
    listen: &str,
    roster: serde_json::Value,
    nests: Vec<(String, AppState)>,
    health: Arc<crate::health::RuntimeHealth>,
) -> Result<()> {
    let live = LiveRuntime::new(compose_runtime(roster, nests, health));
    bind_and_serve(listen, live.service()).await
}

/// Compose the runtime's routes for a given nest set: the root endpoints plus every nest nested under
/// its `/<name>` prefix.
///
/// Split out of [`run_runtime`] so the set can be re-composed at runtime (RFC-0027). Routing semantics
/// are unchanged from the static version - still `Router::nest` - which is what makes the parity test
/// meaningful rather than a tautology.
pub fn compose_runtime(
    roster: serde_json::Value,
    nests: Vec<(String, AppState)>,
    health: Arc<crate::health::RuntimeHealth>,
) -> Router {
    let roster = Arc::new(roster);
    let roster_health = health.clone();
    let ready_health = health.clone();
    let mut app = Router::new()
        .route("/health", get(|| async { "ok" }))
        // `GET /nests` - the roster (name, chain, registry hash, table count) across mounted nests,
        // merged per request with each nest's LIVE health (RFC-0026 §5). The static half is computed
        // once at startup; the health half must not be, or a quarantined nest would keep reporting the
        // state it had when the process booted.
        .route(
            "/nests",
            get(move || {
                let r = roster.clone();
                let h = roster_health.clone();
                async move { Json(merge_roster_health(&r, &h)) }
            }),
        )
        // `GET /ready` at the runtime root - the runtime-wide readiness a supervisor polls. The per-nest
        // `/ready` under `/<name>/` answers only for that nest.
        .route(
            "/ready",
            get(move || {
                let h = ready_health.clone();
                async move { roost_ready(&h) }
            }),
        );
    for (name, mut state) in nests {
        // A nest served through this composition answers `/ready` and `/metrics` from `health`'s
        // per-nest counters, never the process-global aggregate - `spawn_runtime` and `mount()` stamp
        // `runtime_health` before a real nest ever reaches here. Filled in here too, only if the caller
        // left it unset, so a fixture (or any future caller) that forgets the stamp cannot silently fall
        // back to the solo path - the fault that let three fixtures in a row (#292, #356, #388) prove
        // the handler and never the wiring. `get_or_insert_with` rather than an unconditional
        // assignment: an alias mount's state is deliberately pre-stamped with its *canonical* nest's
        // name (`fan_out_aliases`), and that must survive composition unchanged.
        state
            .runtime_health
            .get_or_insert_with(|| (name.clone(), health.clone()));
        // `Router::nest` re-roots the whole per-nest router under `/<name>`, so `/lodestar/tables`,
        // `/lodestar/sql`, `/lodestar/_admin/` … all resolve to that nest's isolated state.
        app = app.nest(&format!("/{name}"), router(SharedNest::new(state)));
    }
    app
}

/// A mounts's routes behind a swappable handle (RFC-0027 slice 1).
///
/// Today a runtime's nest set is frozen at boot: `axum::Router` is composed before it is served and has
/// no insertion point afterwards, so adding or removing a nest means restarting the process - which
/// stops every *co-tenant* nest too. That makes the blast radius of a configuration change larger than
/// the blast radius of a fault, which RFC-0026 already fixed.
///
/// This holds the composed router in an [`arc_swap::ArcSwap`] and serves a thin outer router that
/// delegates every request to whatever is current. Swapping the whole composed router - rather than
/// dispatching on the first path segment by hand - keeps `Router::nest` doing the routing, so the
/// serving semantics are *identical* to the static path by construction rather than by careful
/// re-implementation. Re-composition is rare (a mount or unmount), so rebuilding is not a hot path.
///
/// **Slice 1 changes no behaviour**: nothing calls [`LiveRuntime::swap`] yet. It exists so the lifecycle
/// slices have somewhere to stand, and so the parity test can prove the indirection is free.
pub struct LiveRuntime {
    current: Arc<arc_swap::ArcSwap<Router>>,
}

impl LiveRuntime {
    pub fn new(router: Router) -> Self {
        Self {
            current: Arc::new(arc_swap::ArcSwap::from_pointee(router)),
        }
    }

    /// Replace the served routes. The next request sees the new set; requests already in flight
    /// complete against the router they started on.
    pub fn swap(&self, router: Router) {
        self.current.store(Arc::new(router));
    }

    /// The router to serve: a fallback that resolves the current composition per request.
    pub fn service(&self) -> Router {
        let current = self.current.clone();
        Router::new().fallback(move |req: axum::extract::Request| {
            let current = current.clone();
            async move {
                use tower::ServiceExt;
                // `Router::clone` is cheap (its state is behind an `Arc`), and `oneshot` drives this
                // request through the composed router exactly as `axum::serve` would have.
                let router = (**current.load()).clone();
                match router.oneshot(req).await {
                    Ok(resp) => resp,
                    // `Router`'s error type is `Infallible`, so this is unreachable; matching keeps it
                    // honest rather than unwrapping.
                    Err(e) => match e {},
                }
            }
        })
    }
}

/// Merge each roster entry's live health into the startup snapshot (RFC-0026 §5). A nest that is
/// indexing carries `"health": "indexing"` and no `quarantine` key; a quarantined one carries the
/// reason, class, and retry deadline an operator needs to act on.
fn merge_roster_health(
    roster: &serde_json::Value,
    health: &crate::health::RuntimeHealth,
) -> serde_json::Value {
    let mut out = roster.clone();
    if let Some(entries) = out.get_mut("nests").and_then(|n| n.as_array_mut()) {
        for e in entries {
            let Some(name) = e.get("name").and_then(|n| n.as_str()).map(str::to_string) else {
                continue;
            };
            let (status, quarantine) = health.json_for(&name);
            if let Some(obj) = e.as_object_mut() {
                obj.insert("health".into(), json!(status));
                match quarantine {
                    Some(q) => obj.insert("quarantine".into(), q),
                    // Explicitly remove rather than leave a stale entry, so a recovered nest's roster
                    // row does not keep describing a quarantine that has since been lifted.
                    None => obj.remove("quarantine"),
                };
            }
        }
    }
    out["all_indexing"] = json!(health.all_indexing());
    out
}

/// MountTable-wide readiness (RFC-0026 §5): **200** while every mounted nest is indexing, **503** as soon as
/// any nest or cursor is quarantined, with the offenders named.
///
/// 503-on-any-quarantine is the deliberately conservative choice. A supervisor should treat a
/// partly-broken mounts as not-ready and fetch a human, while the healthy nests carry on serving reads
/// to consumers who ask for them directly - readiness is advice to a supervisor, it does not gate
/// traffic. The healthy/unhealthy split stays visible per nest on `/nests` and `/<name>/ready`.
fn roost_ready(health: &crate::health::RuntimeHealth) -> impl IntoResponse {
    let unhealthy = health.unhealthy();
    let ready = unhealthy.is_empty();
    let body = json!({
        "ready": ready,
        "quarantined": unhealthy.iter().map(|(n, r)| json!({"nest": n, "reason": r})).collect::<Vec<_>>(),
    });
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (code, Json(body))
}

/// Bind `listen` and serve `app` until a shutdown signal - the shared tail of [`run`]/[`run_runtime`].
pub async fn bind_and_serve(listen: &str, app: Router) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot bind {listen}"))?;
    tracing::info!("API live on http://{listen}  (try GET /  and  /metrics)");
    // A loud one-liner when bound off-localhost: the guards bound *how much*, but *who* is the
    // operator's gateway's job - never expose this straight to the internet without one.
    if !is_localhost(listen) {
        tracing::warn!(
            "listening on {listen} (not localhost): the /sql surface is guarded (timeout + row cap \
             + {SQL_MAX_CONCURRENCY} concurrent) but has NO authentication - put a gateway in front \
             before exposing it publicly. See docs/operators.md."
        );
    }
    // Graceful shutdown on SIGTERM/SIGINT: axum drains in-flight requests, then this returns so the
    // caller can abort the ingest task(s) (progress is checkpointed, so a restart resumes cleanly).
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    tracing::info!("shutdown signal received; API stopped");
    Ok(())
}

/// The built-in admin UI (RFC-0010 Part A) - a single self-contained page, embedded in the binary.
const ADMIN_HTML: &str = include_str!("admin.html");

/// `GET /_admin/` - serve the admin UI when enabled, else 404 (it's off, or the bind is public with
/// no token). The page is read-only and talks only to this same-origin API; no external requests.
#[derive(Deserialize)]
struct AdminQuery {
    token: Option<String>,
}

/// Whether a request may reach the admin surface (SEC-5; header support is audit L7).
///
/// On a localhost bind `admin_token` is `None` and the surface is open. Off-localhost a token is
/// required per request, and may arrive either way:
///
/// - **`?token=…`** - the original form, and it has to stay: the admin UI's own live updates use
///   `EventSource`, which cannot set request headers. It is already constant-time compared and
///   same-origin.
/// - **`Authorization: Bearer …`** - preferred wherever the caller *can* set headers, because a query
///   string leaks into proxy access logs, shell history and `Referer`, and a token in a log file
///   outlives the request that carried it.
///
/// The scheme is matched case-insensitively per RFC 7235; the token compare is constant-time either
/// way, so a timing side-channel cannot recover it byte-by-byte.
/// Whether a request carries the admin credential, as `?token=` or `Authorization: Bearer`.
///
/// Shared with the runtime's lifecycle routes (RFC-0027 §5) so mount/unmount are gated by exactly the
/// same rule as the admin UI - one credential, one comparison, no second auth concept to get subtly
/// wrong. `None` means a localhost bind, which is open by design.
pub fn token_ok(
    required: Option<&str>,
    token: Option<&str>,
    headers: &axum::http::HeaderMap,
) -> bool {
    let Some(required) = required else {
        return true; // localhost bind: open
    };
    if token.is_some_and(|t| ct_eq(t, required)) {
        return true;
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        })
        .is_some_and(|t| ct_eq(t, required))
}

fn admin_authorized(state: &AppState, q: &AdminQuery, headers: &axum::http::HeaderMap) -> bool {
    let Some(required) = &state.admin_token else {
        return true; // localhost bind: open
    };
    if q.token.as_deref().is_some_and(|t| ct_eq(t, required)) {
        return true;
    }
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, rest) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        })
        .is_some_and(|t| ct_eq(t, required))
}

async fn admin_index(
    State(s): State<AppState>,
    Query(q): Query<AdminQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Not reachable over HTTP any more - [`router`] does not mount this handler when the surface is
    // disabled - and kept deliberately: the handler states its own precondition, so wiring it up from
    // somewhere new cannot serve the admin UI by omission.
    if !s.admin_enabled {
        return (StatusCode::NOT_FOUND, "admin UI disabled").into_response();
    }
    if !admin_authorized(&s, &q, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            "admin UI requires a valid ?token= or Authorization: Bearer token",
        )
            .into_response();
    }
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        ADMIN_HTML,
    )
        .into_response()
}

/// `GET /nest` - static nest metadata for the admin UI's Nest tab (RFC-0010 Part A).
async fn nest(State(s): State<AppState>) -> impl IntoResponse {
    Json((*s.nest_info).clone())
}

/// The nest's capability *shape* (RFC-0025): which capability-gated MCP surfaces are live, so the
/// `nuthatch mcp` bridge advertises only the tools that will actually return data - never, say,
/// `top_balances` on a Uniswap-pool nest, which would answer `{"count":0}` and read to an agent as
/// "the index is empty". Derived from what the nest *has* - a transfer-shaped decoder (the same gate
/// the balance view uses) and whether RFC-0008 compliance is configured - so the surface stays honest
/// by construction, with no operator knob.
async fn shape(State(s): State<AppState>) -> Json<Value> {
    let transfers = s.tables.iter().any(|t| t.is_transfer_shaped());
    let compliance = s.threshold.is_some()
        || s.velocity_threshold.is_some()
        || s.dir.join(crate::labels::LABELS_DIR).is_dir()
        || s.dir.join(crate::lists::LISTS_DIR).is_dir();
    Json(json!({ "transfers": transfers, "compliance": compliance }))
}

/// Whether `listen` binds only the loopback interface.
pub fn is_localhost(listen: &str) -> bool {
    let host = listen.rsplit_once(':').map(|(h, _)| h).unwrap_or(listen);
    matches!(host, "127.0.0.1" | "::1" | "localhost" | "[::1]")
}

/// Resolves when the process is asked to stop - SIGTERM (systemd/Docker) or Ctrl-C.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}

/// Middleware: bump the request counter, then pass through.
async fn count_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    crate::metrics::METRICS.inc_http();
    next.run(req).await
}

/// `GET /metrics` - Prometheus text exposition (RFC-0005 §6).
async fn metrics_handler(State(s): State<AppState>) -> impl IntoResponse {
    let mut body = crate::metrics::METRICS.render();
    // In a runtime, append the health series (RFC-0026 §5) so an operator can alert on "anything
    // quarantined" without polling `/nests`. Like the existing per-nest series, these describe the
    // whole mounts regardless of which nest's `/metrics` is scraped.
    if let Some((_, health)) = &s.runtime_health {
        body.push_str(&health.render_metrics());
    }
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
}

/// No successful source poll within this many seconds ⇒ stalled. The tip loop polls every ~2-3 s even
/// when caught up, so a minute-plus of silence means every RPC endpoint is unreachable, not idleness.
const READINESS_STALL_SECS: u64 = 90;

/// Stalled = polled successfully at least once (`last_poll != 0`) but not within `threshold` seconds. A
/// never-polled node (`last_poll == 0`) is *starting up*, not stalled - it gets grace, but the grace is
/// bounded by `started_at` (#510): a pool that has been dead since before the very first poll must
/// eventually report stalled too, or `/ready` stays a false "ready:true" forever. `started_at == 0`
/// (not yet stamped, e.g. an un-stamped test fixture) falls back to the old unconditional grace.
fn poll_stalled(last_poll: u64, started_at: u64, now: u64, threshold: u64) -> bool {
    let since = if last_poll != 0 {
        last_poll
    } else {
        started_at
    };
    since != 0 && now.saturating_sub(since) > threshold
}

/// Readiness for a supervisor (k8s-style). `/health` is liveness - the process is up and serving, and
/// stays a plain `200 "ok"`. `/ready` answers "is it *healthy*" - still reaching the chain: **200** with
/// a status body when fresh, **503** when stalled (no successful poll within [`READINESS_STALL_SECS`],
/// i.e. every RPC endpoint is down). A just-started node that has never polled is *not* stalled (grace).
async fn ready(State(s): State<AppState>) -> impl IntoResponse {
    use crate::metrics::{now_unix, METRICS};
    // In a runtime, this nest answers for ITSELF (RFC-0026 §5): a consumer polling `/lodestar/ready`
    // must not be told the runtime is unwell because some unrelated co-tenant is quarantined - nor told
    // all is well when *this* nest is the one that is frozen.
    if let Some((name, health)) = &s.runtime_health {
        if let Some(q) = health.status(name) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "ready": false,
                    "quarantined": true,
                    "kind": q.kind,
                    "class": q.class,
                    "reason": q.reason,
                    "since_unixtime": q.since_unixtime,
                    "next_retry_unixtime": q.next_retry_unixtime,
                })),
            )
                .into_response();
        }
    }
    // Answer from **this nest's** counters when it is one of several in a runtime. The process-global
    // gauges are shared by every cursor, so in a multichain runtime whichever cursor polled last wins -
    // and this endpoint then reports another chain's block heights. Observed live in a two-chain mounts:
    // the mainnet nest reported `tip: 488677305` (Arbitrum) while mainnet was at 25,632,906, alongside
    // a mainnet `sealed_through`. One body, two chains, no way for an operator to tell.
    //
    // A solo `dev` has exactly one nest feeding the globals, so both paths agree there.
    let nest = s
        .runtime_health
        .as_ref()
        .map(|(name, _)| METRICS.nest(name));
    let (last_poll, tip, last, sealed, started_at) = match &nest {
        Some(m) => (
            m.last_poll_ok(),
            m.tip(),
            m.last_block(),
            m.sealed_through(),
            m.started_at(),
        ),
        None => (
            METRICS.last_poll_ok(),
            METRICS.tip_height(),
            METRICS.last_block(),
            METRICS.sealed_through_val(),
            METRICS.started_at(),
        ),
    };
    let now = now_unix();
    let age = (last_poll != 0).then(|| now.saturating_sub(last_poll));
    let stalled = poll_stalled(last_poll, started_at, now, READINESS_STALL_SECS);
    let body = json!({
        "ready": !stalled,
        "stalled": stalled,
        "tip": tip,
        "last_block": last,
        "lag_blocks": tip.saturating_sub(last),
        "sealed_through": sealed,
        "last_poll_unixtime": last_poll,
        "seconds_since_poll": age,
    });
    let code = if stalled {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    (code, Json(body)).into_response()
}

/// The live status payload - nest identity, tip/sealed watermarks, and the IVM view counts. Built once
/// here and served two ways: as the `GET /` JSON body, and as each frame of the `/_admin/events` SSE
/// stream, so the polled and pushed surfaces can never disagree.
fn summary_value(s: &AppState) -> Value {
    let count = s.store.count().unwrap_or(0);
    let last_block = s.store.get_meta("last_block").ok().flatten();
    json!({
        "name": "nuthatch",
        "chain": s.chain,
        "address": s.address,
        "entities": count,
        "last_block": last_block,
        "sealed_through": s.store.get_meta("sealed_through").ok().flatten(),
        "holders": s.balances.holders(),
        "exposure_entries": s.exposure.entries(),
        "velocity_buckets": s.velocity.entries(),
        "alert_outbox": s.store.outbox_len(),
        "tables": s.tables.len(),
        "views": ["balances (IVM)", "exposure (IVM)", "velocity (IVM)"],
        "endpoints": [
            "/health",
            "/tables",
            "/table/{name}?limit=100",
            "/entities?limit=100",
            "/entity/{block:012}-{log_index:06}",
            "/sql?q=SELECT count(*) FROM \"<alias>__<event>\"",
            "/balances?limit=100",
            "/balance/{address}",
            "/exposure/{address}",
            "/flags?kind=threshold|velocity",
        ],
    })
}

async fn summary(State(s): State<AppState>) -> impl IntoResponse {
    Json(summary_value(&s))
}

/// How often the `/_admin/events` SSE stream pushes a fresh status frame. Matches the 2 s cadence the
/// admin UI used to poll at, so the live view feels identical - just server-pushed, not client-pulled.
const SSE_INTERVAL: Duration = Duration::from_secs(2);

/// `GET /_admin/events` - a Server-Sent Events stream of the status summary, pushed every
/// [`SSE_INTERVAL`], so the admin UI updates live without polling (RFC-0010 Part A). Gated exactly like
/// the admin page: 404 when the admin UI is disabled, and off-localhost it requires a valid `?token=`.
/// Each frame is byte-identical to `GET /` (both call [`summary_value`]). Purely a serving-layer read -
/// nothing here touches the ingest/decode data path.
async fn admin_events(
    State(s): State<AppState>,
    Query(q): Query<AdminQuery>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    // Same as [`admin_index`]: unreachable while the route is unmounted, kept as the handler's own
    // precondition rather than a rule that lives only in the router.
    if !s.admin_enabled {
        return (StatusCode::NOT_FOUND, "admin UI disabled").into_response();
    }
    if !admin_authorized(&s, &q, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            "admin events require a valid ?token= or Authorization: Bearer token",
        )
            .into_response();
    }
    // `unfold` carries the state and a `first` flag: emit immediately, then once per interval. Cloning
    // `AppState` per frame is cheap - its fields are `Arc`/handle types, not owned data.
    let stream = stream::unfold((s, true), |(s, first)| async move {
        if !first {
            tokio::time::sleep(SSE_INTERVAL).await;
        }
        let ev = Event::default().data(summary_value(&s).to_string());
        Some((Ok::<_, std::convert::Infallible>(ev), (s, false)))
    });
    // Keep-alive comment every 15 s so an idle proxy doesn't close a quiet stream between frames.
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// List every table and its columns (the decoded data model).
async fn tables(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({ "count": s.tables.len(), "tables": &*s.tables }))
}

#[derive(Deserialize)]
struct TableQuery {
    limit: Option<usize>,
    from_block: Option<u64>,
    to_block: Option<u64>,
}

/// The enriched schema document (RFC-0016 §2): the composition of registry **structure**, the
/// authored **meaning** from `semantic.toml`, the derived **footguns**, and live **coverage** (the
/// hot/cold seam as numbers). Assembled per call from this running nest - the MCP `schema` tool
/// relays it, so an agent reads *this* nest's data model, not a static string. Plain text.
async fn schema_doc(State(s): State<AppState>) -> impl IntoResponse {
    let sem = crate::semantic::load(&s.dir).ok().flatten();
    if let Some(sem) = &sem {
        for w in crate::semantic::drift(&s.tables, sem) {
            tracing::warn!("semantic.toml drift: {w}");
        }
    }
    let coverage = crate::semantic::Coverage {
        sealed_through: s.store.sealed_through(),
        tip: s
            .store
            .get_meta("last_block")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    };
    let doc = crate::semantic::compose(&s.tables, sem.as_ref(), Some(&coverage));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        doc,
    )
}

/// Recent rows of one table, merged across the hot store and the sealed cold segments.
async fn table(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<TableQuery>,
) -> impl IntoResponse {
    if !s.tables.iter().any(|t| t.table == name) {
        return not_found(&name);
    }
    let limit = q.limit.unwrap_or(100).min(1000);
    let in_range = |v: &Value| {
        let b = v.get("block_number").and_then(Value::as_u64).unwrap_or(0);
        q.from_block.map(|f| b >= f).unwrap_or(true) && q.to_block.map(|t| b <= t).unwrap_or(true)
    };

    // Hot rows (tip), newest first.
    let mut items: Vec<Value> = s
        .store
        .recent_by_table(&name, limit)
        .unwrap_or_default()
        .iter()
        .filter_map(|r| serde_json::from_str::<Value>(r).ok())
        .filter(&in_range)
        .collect();

    // Fill from cold (sealed segments) if the hot store didn't satisfy the limit. This runs under
    // the same analytical admission gate as `/sql`; if it's saturated we serve the hot rows we
    // already have rather than pile more scan-heavy work on. Cold is an enrichment of the hot result,
    // so best-effort (`try_acquire`) is the right degradation - a point-read-ish endpoint shouldn't
    // 503 just because the analytical surface is busy.
    if items.len() < limit {
        if let Ok(permit) = Arc::clone(&s.sql_gate).try_acquire_owned() {
            let need = limit - items.len();
            let mut where_ = String::new();
            if let Some(f) = q.from_block {
                where_.push_str(&format!(" AND block_number >= {f}"));
            }
            if let Some(t) = q.to_block {
                where_.push_str(&format!(" AND block_number <= {t}"));
            }
            let sql = format!(
                "SELECT * FROM \"{name}\" WHERE 1=1{where_} ORDER BY block_number DESC, log_index DESC LIMIT {need}"
            );
            let dir = s.dir.clone();
            let guard = analytics::QueryGuard {
                timeout: SQL_TIMEOUT,
                max_rows: need,
            };
            if let Ok(Ok(out)) = tokio::task::spawn_blocking(move || {
                let _permit = permit; // held for the whole blocking query
                analytics::query_guarded(&dir, &sql, guard)
            })
            .await
            {
                items.extend(out.rows);
            }
        }
    }

    // Dedup by (block, log_index); hot wins over cold.
    let mut seen = std::collections::HashSet::new();
    items.retain(|v| {
        let id = (
            v.get("block_number").and_then(Value::as_u64),
            v.get("log_index").and_then(Value::as_u64),
        );
        seen.insert(id)
    });
    items.truncate(limit);
    Json(json!({ "table": name, "count": items.len(), "items": items })).into_response()
}

#[derive(Deserialize)]
struct EntitiesQuery {
    limit: Option<usize>,
}

async fn entities(State(s): State<AppState>, Query(q): Query<EntitiesQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    match s.store.recent(limit) {
        Ok(rows) => {
            let items: Vec<Value> = rows
                .iter()
                .filter_map(|r| serde_json::from_str::<Value>(r).ok())
                .collect();
            Json(json!({ "count": items.len(), "items": items })).into_response()
        }
        Err(e) => error(format!("{e:#}")),
    }
}

async fn entity(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    // Hot path first (redb). On a miss, fall back to the sealed segments (DuckDB), so a point-read
    // keeps working across the hot→cold seam even after the hot store has been pruned.
    match s.store.get_entity(&id) {
        Ok(Some(raw)) => match serde_json::from_str::<Value>(&raw) {
            Ok(v) => return Json(v).into_response(),
            Err(e) => return error(format!("{e:#}")),
        },
        Ok(None) => {}
        Err(e) => return error(format!("{e:#}")),
    }

    match parse_id(&id) {
        Some((block, log_index)) => {
            let dir = s.dir.clone();
            let sealed =
                tokio::task::spawn_blocking(move || analytics::get_row(&dir, block, log_index))
                    .await;
            match sealed {
                Ok(Ok(Some(v))) => Json(v).into_response(),
                Ok(Ok(None)) => not_found(&id),
                Ok(Err(e)) => error(format!("{e:#}")),
                Err(e) => error(format!("{e}")),
            }
        }
        None => not_found(&id),
    }
}

#[derive(Deserialize)]
struct SqlQuery {
    q: String,
    /// Optional row cap for this request, clamped to `[1, SQL_MAX_ROWS]`. The MCP bridge passes a
    /// small value (default 200) so an agent's context isn't flooded (RFC-0016 §4); curl gets the
    /// full cap. Absent → the node cap.
    #[serde(default)]
    max_rows: Option<usize>,
}

/// The refusal a bounded mount gives free-form SQL (RFC-0034 §2).
///
/// `403`, not `400`: the query may be perfectly valid: this nest simply does not answer arbitrary
/// SQL. And it names what *can* be asked, in RFC-0016's errors-as-prompts style, so an agent hitting
/// a bounded nest is told the surface rather than left guessing at it.
fn refuse_free_form(surface: &crate::allowlist::Surface) -> axum::response::Response {
    crate::metrics::METRICS.inc_sql_rejected();
    let names = surface.names();
    let body = if names.is_empty() {
        json!({
            "error": "this nest does not answer SQL",
            "hint": "the operator has disabled /sql and /explain for this mount; the typed routes \
                     (/tables, /table/{name}, /entity/{id}, /balances) still work",
        })
    } else {
        json!({
            "error": "this nest answers only its declared queries, not arbitrary SQL",
            "allowed_queries": names,
            "hint": "call GET /q/{name} with the query's parameters as the query string; \
                     GET /queries lists them with their parameters",
        })
    };
    (StatusCode::FORBIDDEN, Json(body)).into_response()
}

/// `GET /queries` - what this mount will answer, and with what parameters.
///
/// Always present, whatever the mode, so a caller can discover a *bounded* nest's surface without
/// first getting refused by it.
async fn queries(State(s): State<AppState>) -> impl IntoResponse {
    Json(json!({
        "sql": match s.surface.access {
            crate::allowlist::SqlAccess::Open => "open",
            crate::allowlist::SqlAccess::Deny => "deny",
            crate::allowlist::SqlAccess::Allowlist => "allowlist",
        },
        "free_form": s.surface.free_form_allowed(),
        "queries": s.surface.queries.iter().map(|q| json!({
            "name": q.name,
            "params": q.param_list(),
            "path": format!("/q/{}", q.name),
        })).collect::<Vec<_>>(),
    }))
}

/// `GET /q/{name}?param=value` - run one declared query.
///
/// The caller sends a **name and arguments, never SQL**. Everything the node's own guards do to
/// `/sql` still applies here: a declared query can still be expensive, and the timeout, row cap and
/// concurrency limit are what stop it.
async fn named_query(
    State(s): State<AppState>,
    axum::extract::Path(name): axum::extract::Path<String>,
    Query(args): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use crate::metrics::METRICS;
    let Some(q) = s.surface.get(&name) else {
        METRICS.inc_sql_rejected();
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": format!("no query named '{name}' on this nest"),
                "allowed_queries": s.surface.names(),
            })),
        )
            .into_response();
    };
    let sql = match q.render(&args) {
        Ok(sql) => sql,
        Err(e) => {
            METRICS.inc_sql_rejected();
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("{e:#}"), "query": name })),
            )
                .into_response();
        }
    };
    run_sql_query(s, sql, None).await
}

/// Read-only analytical SQL over the sealed segments; one view per `{alias}__{event}` table.
///
/// Self-protecting, so a public `/sql` isn't a DoS vector: bounded concurrency (503 when the
/// analytical surface is saturated - a growing backlog is itself the attack), a per-query wall-clock
/// budget (a runaway is interrupted), a max query length, and a row cap (bounds the result buffer).
/// What's deliberately *absent* is authn / per-caller quotas: those need caller identity a sovereign
/// node doesn't have, so gating *who* may query and *how much* is a gateway's job, not the node's.
/// **Bounded surfaces (RFC-0034).** What the guards above cannot express is *which* queries a nest
/// will answer at all. A mount may set `sql = "deny"` or `sql = "allowlist"`, and this route then
/// refuses free-form text and points at `/q/{name}` instead.
async fn sql(State(s): State<AppState>, Query(q): Query<SqlQuery>) -> impl IntoResponse {
    use crate::metrics::METRICS;
    if !s.surface.free_form_allowed() {
        return refuse_free_form(&s.surface);
    }
    if q.q.len() > SQL_MAX_QUERY_LEN {
        METRICS.inc_sql_rejected();
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("query too long: {} bytes (max {SQL_MAX_QUERY_LEN})", q.q.len()) })),
        )
            .into_response();
    }
    run_sql_query(s, q.q, q.max_rows).await
}

/// Execute SQL and shape the response - shared by `/sql` (caller-supplied text, only when the
/// surface is open) and `/q/{name}` (a declared query, rendered from typed arguments).
///
/// **Every node guard lives here**, so a declared query is bounded exactly as free-form SQL is. A
/// name on the allowlist says the operator is willing to answer it, not that it is cheap.
async fn run_sql_query(
    s: AppState,
    sql_text: String,
    requested_max_rows: Option<usize>,
) -> axum::response::Response {
    use crate::metrics::METRICS;
    let q = SqlQuery {
        q: sql_text,
        max_rows: requested_max_rows,
    };
    // Fail fast when the analytical surface is saturated rather than queue: a backlog of pending
    // DuckDB queries would itself exhaust memory/threads.
    let permit = match Arc::clone(&s.sql_gate).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            METRICS.inc_sql_rejected();
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "server busy: too many concurrent SQL queries" })),
            )
                .into_response();
        }
    };
    METRICS.inc_sql();
    let dir = s.dir.clone();
    let sql = q.q.clone();
    let store = s.store.clone();
    let sql_max_hot_rows = s.sql_max_hot_rows;
    // Per-request row cap (RFC-0016 §4): the MCP bridge asks for a small number so an agent's context
    // isn't flooded; curl omits it and gets the node cap. Clamped so it can only ever tighten.
    let max_rows = q.max_rows.unwrap_or(SQL_MAX_ROWS).clamp(1, SQL_MAX_ROWS);
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit; // held for the whole blocking query, released on return
                              // Scan the hot tip (redb, blocking) inside the same blocking task, so `/sql` sees the unsealed
                              // rows alongside the sealed segments (RFC-0013). A scan failure degrades to cold-only.
                              // A scan failure degrades to cold-only, *except* an over-budget tip: that must surface, or a
                              // query would quietly answer from sealed data alone and report a different number.
        let (hot, tip_unavailable) = match store.hot_rows_by_table_bounded(sql_max_hot_rows) {
            Ok(hot) => (hot, false),
            Err(e) if e.downcast_ref::<crate::store::HotScanTooLarge>().is_some() => return Err(e),
            // Same level as a segment read failure (#472): a hot store that will not scan is at least
            // as serious, and the fallback below must not be the only trace of it.
            Err(e) => {
                tracing::error!("hot-tip scan failed - serving cold-only for this query: {e:#}");
                (Default::default(), true)
            }
        };
        let sealed_through = store.sealed_through();
        let mut out = analytics::query_hot_cold(
            &dir,
            &sql,
            analytics::QueryGuard {
                timeout: SQL_TIMEOUT,
                max_rows,
            },
            &hot,
            sealed_through,
        )?;
        out.tip_unavailable = tip_unavailable;
        Ok(out)
    })
    .await;
    match result {
        // Provenance stamp (RFC-0016 §4): an agent can cite its answer against content-addressed data -
        // as of which block, what's sealed, and the registry it decoded with.
        Ok(Ok(out)) => Json(json!({
            "count": out.rows.len(),
            "truncated": out.truncated,
            // Cold data was incomplete when this answer was computed (#435): a sealed segment the
            // manifest lists could not be read, so its table was *reduced* and the query succeeded
            // with quietly less data. Reduction is the right policy (#430) - a bad segment must not
            // delete a table - but it is only defensible if the caller is told, or `SUM(value)`
            // comes back wrong rather than absent. Always present, so a caller cannot mistake the
            // healthy shape for an older build that never reported it.
            //
            // `degraded_tables` names the tables and nothing else. Table names are already public on
            // this surface (`/schema` lists them, and you must name one to query it); segment paths
            // and content addresses are not, and must never appear here - `/sql` is untrusted, which
            // is the same reason errors go through `sanitize_sql_error`.
            "degraded": out.degraded(),
            "degraded_tables": out.degraded_tables,
            // Tip failure, not per-table cold reduction (#472): the hot-tip scan itself errored, so
            // this answer is sealed-history-only regardless of what `degraded_tables` says. Distinct
            // cause (a damaged/unreadable hot store, not a bad segment) and distinct remedy, so it does
            // not belong inside `degraded_tables` - see `QueryOutput::tip_unavailable`.
            "tip_unavailable": out.tip_unavailable,
            "rows": out.rows,
            // Provenance (RFC-0016 §4, extended by RFC-0035 §3). `registry_hash` says *how* the rows
            // were decoded; it does not say **which dataset answered**, and since RFC-0033's early
            // cutoff a result may legitimately come from data a *different* identity produced - the
            // adoption is correct, and the old stamp could not express it. `nid` closes that: an agent
            // citing an answer can now name the dataset, not just the decode.
            "provenance": {
                "as_of": s.store.get_meta("last_block").ok().flatten()
                    .and_then(|v| v.parse::<u64>().ok()),
                "sealed_through": s.store.sealed_through(),
                "source": "hot+sealed",
                "registry_hash": s.nest_info.get("registry_hash").and_then(Value::as_str),
                "nid": s.nid.as_deref(),
            },
        }))
        .into_response(),
        // The tip is too large to serve in one scan. A `503` rather than a `400`: the query is fine,
        // the node is refusing to spend the memory - so a caller should retry later or narrow to
        // sealed data, not rewrite their SQL.
        Ok(Err(e)) if e.downcast_ref::<crate::store::HotScanTooLarge>().is_some() => {
            METRICS.inc_sql_rejected();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": format!("{e}"),
                    "sealed_through": s.store.sealed_through(),
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            // A guard rejection (timeout / interrupt) or a bad query - counted as a rejection.
            METRICS.inc_sql_rejected();
            // Errors as prompts (RFC-0016 §3): classify the failure against the schema and append an
            // actionable hint so an agent (or the REPL user) self-corrects in one round-trip. The raw
            // engine message is preserved but path-scrubbed (SEC review) - DuckDB embeds absolute
            // segment paths, which would leak the on-disk layout; the useful table/column detail stays.
            let raw = sanitize_sql_error(&format!("{e:#}"), &s.dir);
            let msg = match crate::sql_errors::enrich(&raw, &q.q, &s.tables) {
                Some(hint) => format!("{raw}\n\nhint: {hint}"),
                None => raw,
            };
            (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
        }
        Err(e) => error(format!("{e}")),
    }
}

/// `GET /explain?q=…` - validate a query without executing it (RFC-0016 §3): bind it (catching
/// unknown tables/columns/type errors, with the same enriched hints as `/sql`) but scan nothing, by
/// wrapping it as `SELECT * FROM (<q>) LIMIT 0`. An agent checks a query's shape before spending a
/// concurrency slot on the real thing. Returns `{valid:true}` or the enriched error.
async fn explain(State(s): State<AppState>, Query(q): Query<SqlQuery>) -> impl IntoResponse {
    use crate::metrics::METRICS;
    // `/explain` gets the same treatment as `/sql`. It plans caller-supplied SQL, so leaving it open
    // on a bounded mount would leak the schema and the cost model of every table. Bounding the
    // surface closes that; the hot-row ceiling below closes the scan cost (#293).
    if !s.surface.free_form_allowed() {
        return refuse_free_form(&s.surface);
    }
    if q.q.len() > SQL_MAX_QUERY_LEN {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "valid": false, "error": "query too long" })),
        )
            .into_response();
    }
    let permit = match Arc::clone(&s.sql_gate).try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "valid": false, "error": "server busy" })),
            )
                .into_response()
        }
    };
    let dir = s.dir.clone();
    // Bind-only: the LIMIT 0 wrapper forces the planner to resolve every table/column/type without
    // materialising rows. A CTE (`WITH …`) is legal inside the subquery, so this covers both shapes.
    let probe = format!("SELECT * FROM ({}) AS _explain LIMIT 0", q.q);
    let store = s.store.clone();
    let sql_max_hot_rows = s.sql_max_hot_rows;
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        // The `LIMIT 0` probe stops DuckDB materialising result rows, but the tip is still parsed
        // into temp tables first - so `/explain` carries the full scan cost. Without the same
        // ceiling `/sql` enforces, an endpoint reachable by anyone who can reach `/sql` could push
        // the process past the budget `/sql` refuses to cross (#293). As there, an over-budget tip
        // surfaces rather than degrading to cold-only: answering "valid" off sealed data alone
        // would bind against a narrower schema than the one a subsequent `/sql` would see.
        let (hot, tip_unavailable) = match store.hot_rows_by_table_bounded(sql_max_hot_rows) {
            Ok(hot) => (hot, false),
            Err(e) if e.downcast_ref::<crate::store::HotScanTooLarge>().is_some() => return Err(e),
            // Same level as a segment read failure (#472): a hot store that will not scan is at
            // least as serious, and the fallback below must not be the only trace of it - #528.
            Err(e) => {
                tracing::error!("hot-tip scan failed - serving cold-only for this query: {e:#}");
                (Default::default(), true)
            }
        };
        let sealed_through = store.sealed_through();
        let mut out = analytics::query_hot_cold(
            &dir,
            &probe,
            analytics::QueryGuard {
                timeout: SQL_TIMEOUT,
                max_rows: 1,
            },
            &hot,
            sealed_through,
        )?;
        out.tip_unavailable = tip_unavailable;
        Ok(out)
    })
    .await;
    match result {
        Ok(Ok(out)) => Json(json!({
            "valid": true,
            "note": "query binds; run it with the sql tool",
            // Same field, same meaning as `/sql` (#528): the hot-tip scan failed, so this bind
            // answer is against sealed history alone and may miss a table/column the tip would add.
            "tip_unavailable": out.tip_unavailable,
        }))
        .into_response(),
        // Same contract as `/sql`: a `503`, not a `400`. The query may well be valid - the node is
        // refusing to spend the memory to find out, so a caller should retry later, not rewrite
        // their SQL. `valid` is deliberately absent rather than `false`: bindability is unknown
        // here, and reporting `false` would tell a caller their query is broken when it is not.
        Ok(Err(e)) if e.downcast_ref::<crate::store::HotScanTooLarge>().is_some() => {
            METRICS.inc_sql_rejected();
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({
                    "error": format!("{e}"),
                    "sealed_through": s.store.sealed_through(),
                })),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            METRICS.inc_sql_rejected();
            let raw = sanitize_sql_error(&format!("{e:#}"), &s.dir);
            let msg = match crate::sql_errors::enrich(&raw, &q.q, &s.tables) {
                Some(hint) => format!("{raw}\n\nhint: {hint}"),
                None => raw,
            };
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "valid": false, "error": msg })),
            )
                .into_response()
        }
        Err(e) => error(format!("{e}")),
    }
}

/// Top balances from the IVM view, descending. Balances are in i64 token base units.
async fn balances(State(s): State<AppState>, Query(q): Query<EntitiesQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    // Balances are i128 base units, serialised as decimal strings - JSON numbers can't carry i128
    // losslessly, and a client parsing a huge balance as an f64 would silently corrupt it.
    let items: Vec<Value> = s
        .balances
        .top(limit)
        .into_iter()
        .map(|(address, balance)| json!({ "address": address, "balance": balance.to_string() }))
        .collect();
    Json(json!({ "holders": s.balances.holders(), "count": items.len(), "items": items }))
}

/// Point-read a single address's derived balance.
async fn balance(State(s): State<AppState>, Path(address): Path<String>) -> impl IntoResponse {
    let address = address.to_ascii_lowercase();
    match s.balances.balance(&address) {
        Some(b) => Json(json!({ "address": address, "balance": b.to_string() })).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no balance", "address": address })),
        )
            .into_response(),
    }
}

/// Direct counterparty-exposure for an address: how much it has transacted, directly, with the
/// labeled set (RFC-0008 C1). Amounts are i128 base units, serialised as decimal strings (same reason
/// as balances). A reorg retracts through the IVM view, so this is always the canonical-chain figure.
async fn exposure(State(s): State<AppState>, Path(address): Path<String>) -> impl IntoResponse {
    let address = address.to_ascii_lowercase();
    let items: Vec<Value> = s
        .exposure
        .exposure(&address)
        .into_iter()
        .map(|r| {
            json!({
                "label": r.label,
                "direction": r.direction,
                "count": r.count.to_string(),
                "amount": r.amount.to_string(),
            })
        })
        .collect();
    Json(json!({ "address": address, "count": items.len(), "exposure": items }))
}

#[derive(Deserialize)]
struct FlagsQuery {
    kind: Option<String>,
    limit: Option<usize>,
}

/// Compliance flags (RFC-0008 C3). `?kind=velocity` returns the live windowed velocity flags (address
/// volume ≥ the configured threshold within a block-window); `?kind=threshold` returns recent
/// `threshold_flag` annotations (hot store; the full sealed history is at `/sql SELECT * FROM
/// threshold_flag`). Omit `kind` for both. Amounts are i128 base units, serialised as decimal strings.
async fn flags(State(s): State<AppState>, Query(q): Query<FlagsQuery>) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).min(1000);
    let kind = q.kind.as_deref();

    let velocity = |s: &AppState| -> Vec<Value> {
        let threshold = s.velocity_threshold.unwrap_or(i128::MIN);
        s.velocity
            .flags(threshold)
            .into_iter()
            .take(limit)
            .map(|f| {
                json!({
                    "address": f.address,
                    "window_start": f.window_start,
                    "count": f.count.to_string(),
                    "volume": f.volume.to_string(),
                })
            })
            .collect()
    };
    // Recent threshold_flag annotations from the hot store (newest first). Sealed history via /sql.
    let threshold = |s: &AppState| -> Vec<Value> {
        s.store
            .recent_by_table("threshold_flag", limit)
            .unwrap_or_default()
            .iter()
            .filter_map(|r| serde_json::from_str::<Value>(r).ok())
            .collect()
    };

    match kind {
        Some("velocity") => Json(json!({
            "kind": "velocity",
            "threshold": s.velocity_threshold.map(|t| t.to_string()),
            "flags": velocity(&s),
        }))
        .into_response(),
        Some("threshold") => Json(json!({
            "kind": "threshold",
            "threshold": s.threshold.map(|t| t.to_string()),
            "flags": threshold(&s),
            "note": "recent hot flags; full sealed history: /sql?q=SELECT * FROM threshold_flag",
        }))
        .into_response(),
        Some(other) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown flag kind '{other}' (want: threshold, velocity)") })),
        )
            .into_response(),
        None => Json(json!({
            "threshold": { "configured": s.threshold.map(|t| t.to_string()), "flags": threshold(&s) },
            "velocity": { "configured": s.velocity_threshold.map(|t| t.to_string()), "flags": velocity(&s) },
        }))
        .into_response(),
    }
}

/// Parse an entity id `{block:012}-{log_index:06}` back into its components.
fn parse_id(id: &str) -> Option<(u64, u64)> {
    let (b, l) = id.split_once('-')?;
    Some((b.parse().ok()?, l.parse().ok()?))
}

fn not_found(id: &str) -> axum::response::Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not found", "id": id })),
    )
        .into_response()
}

fn error(msg: String) -> axum::response::Response {
    // SEC-11: internal error chains include absolute segment/dir paths (a filesystem-layout disclosure).
    // Log the detail server-side; hand the client a generic message.
    tracing::warn!("internal error serving request: {msg}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal error" })),
    )
        .into_response()
}

/// Constant-time string equality for the admin token - no early return on the first differing byte, so
/// an attacker can't time-recover the secret. The length check leaks only the length, which is fixed
/// for a random token.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Redact the nest-directory prefix from a DuckDB error before it's relayed to a `/sql`/`/explain`
/// client (SEC review). DuckDB embeds absolute segment paths (`…/mynest/segments/foo.parquet`) in
/// binder/IO errors; the useful part for the caller is the table/column/type detail, not the on-disk
/// layout. We keep the message and replace only the dir prefix with `<nest>`.
fn sanitize_sql_error(raw: &str, dir: &std::path::Path) -> String {
    let mut out = raw.to_string();
    // Both the canonical and as-configured forms - the error could carry either.
    for p in [dir.canonicalize().ok(), Some(dir.to_path_buf())]
        .into_iter()
        .flatten()
    {
        // Only an *absolute* prefix is worth redacting, and only an absolute one is safe to. The
        // default `--dir` is `.`, which as a plain `replace` target matches every full stop in the
        // message: `1.5` became `1<nest>5` and DuckDB's `...` ellipsis became `<nest><nest><nest>`.
        // The absolute form above is what DuckDB actually embeds, so nothing is lost by skipping
        // the relative one.
        if !p.is_absolute() {
            continue;
        }
        let s = p.display().to_string();
        if !s.is_empty() {
            out = out.replace(&s, "<nest>");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;

    #[test]
    fn readiness_stall_logic() {
        let now = 1_000_000u64;
        // Never polled, just started → starting up, not stalled (grace).
        assert!(!poll_stalled(0, now - 10, now, 90));
        // Polled recently → healthy.
        assert!(!poll_stalled(now - 10, now - 1_000, now, 90));
        // Polled long ago → stalled.
        assert!(poll_stalled(now - 100, now - 1_000, now, 90));
        // Exactly at the threshold is not yet stalled (strictly greater).
        assert!(!poll_stalled(now - 90, now - 1_000, now, 90));
        // Neither timestamp stamped (an un-stamped fixture) → old unconditional grace, not stalled.
        assert!(!poll_stalled(0, 0, now, 90));
    }

    /// #510: a pool that has been unreachable since before the very first poll must eventually report
    /// `stalled`, not stay a permanent "just starting up". Never-polled (`last_poll == 0`) alone used to
    /// mean forever-fresh; the fix bounds that grace by how long ago the nest started trying.
    #[test]
    fn a_pool_dead_since_cold_start_eventually_stalls() {
        let now = 2_000_000u64;
        // Started 91s ago, never once polled successfully - past the 90s grace, so stalled.
        assert!(poll_stalled(0, now - 91, now, 90));
        // Started 10s ago, never polled - still within grace.
        assert!(!poll_stalled(0, now - 10, now, 90));
    }

    #[test]
    fn ct_eq_matches_only_identical_strings() {
        assert!(ct_eq("s3cret", "s3cret"));
        assert!(!ct_eq("s3cret", "s3creT"));
        assert!(!ct_eq("s3cret", "s3cre")); // length differs
        assert!(!ct_eq("", "x"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn sanitize_sql_error_redacts_the_nest_dir() {
        let dir = std::path::Path::new("/var/lib/nuthatch/mynest");
        let raw = "IO Error: No files found that match the pattern \
                   \"/var/lib/nuthatch/mynest/segments/usdc__transfer-abc.parquet\"";
        let out = sanitize_sql_error(raw, dir);
        assert!(
            !out.contains("/var/lib/nuthatch/mynest"),
            "dir prefix redacted"
        );
        assert!(out.contains("<nest>/segments/usdc__transfer-abc.parquet"));
        // The useful DuckDB detail (the message + filename) survives.
        assert!(out.contains("No files found"));
    }

    /// The default `--dir` is `.`, and a bare `replace(".", "<nest>")` corrupted every message that
    /// contained a full stop - decimals, qualified names, and DuckDB's `...` ellipsis alike. Observed
    /// live on `nuthatch dev`: `SELECT 1.5 + bogus` came back as `SELECT 1<nest>5`.
    #[test]
    fn sanitize_sql_error_leaves_full_stops_alone_for_a_relative_dir() {
        let raw = "Binder Error: Referenced column \"bogus\" not found!\n\
                   LINE 1: SELECT 1.5 + bogus FROM usdc__transfer ORDER BY block_number DESC...";
        for dir in [".", "..", "./"] {
            let out = sanitize_sql_error(raw, std::path::Path::new(dir));
            assert_eq!(
                out, raw,
                "a relative --dir ({dir}) must not rewrite the message"
            );
        }
        // ...and the redaction it exists for still happens via the canonical (absolute) form.
        let tmp = std::env::temp_dir();
        let raw_abs = format!(
            "IO Error: no such file \"{}/segments/x.parquet\"",
            tmp.display()
        );
        let out = sanitize_sql_error(&raw_abs, &tmp);
        assert!(
            out.contains("<nest>/segments/x.parquet"),
            "absolute prefix still redacted"
        );
    }

    /// A minimal but real `AppState` - enough to drive the analytical handlers directly (no HTTP
    /// harness). `permits` seeds the admission gate so a test can saturate it.
    fn test_state(dir: &std::path::Path, permits: usize) -> AppState {
        AppState {
            store: std::sync::Arc::new(Store::open(&dir.join("t.redb")).unwrap()),
            address: Some("0x0".into()),
            chain: "ethereum".into(),
            dir: dir.to_path_buf(),
            balances: BalanceView::start().unwrap(),
            exposure: ExposureView::start(true).unwrap(),
            velocity: VelocityView::start(true).unwrap(),
            threshold: None,
            velocity_threshold: None,
            tables: Arc::new(vec![]),
            sql_gate: Arc::new(Semaphore::new(permits)),
            sql_max_hot_rows: SQL_MAX_HOT_ROWS,
            surface: Arc::new(crate::allowlist::Surface::default()),
            nid: None,
            admin_enabled: true,
            admin_token: None,
            nest_info: Arc::new(json!({ "name": "t" })),
            runtime_health: None,
        }
    }

    /// Clock-derived fields, which legitimately differ between two calls a moment apart.
    ///
    /// `/ready` reports `seconds_since_poll` and `last_poll_unixtime`; comparing those byte-for-byte
    /// across two sequential requests asserts that no second boundary was crossed between them, which
    /// is a statement about scheduling rather than about routing. It duly failed in CI on
    /// `"seconds_since_poll":5` vs `6`.
    const VOLATILE: &[&str] = &["seconds_since_poll", "last_poll_unixtime"];

    /// Drive one GET through a router and return `(status, body)` - the whole observable response, so
    /// a parity assertion cannot pass on status alone.
    ///
    /// JSON bodies are normalised by dropping [`VOLATILE`] keys, so the comparison stays byte-exact on
    /// everything the router actually decides and blind only to the wall clock.
    async fn get(router: Router, path: &str) -> (StatusCode, Vec<u8>) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec();
        // Normalise only if it parses as a JSON object; anything else is compared verbatim.
        let body = match serde_json::from_slice::<serde_json::Value>(&body) {
            Ok(mut v) if v.is_object() => {
                let map = v.as_object_mut().unwrap();
                for k in VOLATILE {
                    map.remove(*k);
                }
                serde_json::to_vec(&v).unwrap()
            }
            _ => body,
        };
        (status, body)
    }

    /// RFC-0021's fourth testing criterion, and the one nothing covered (#356): **stall isolation
    /// across cursors**. The RFC's words are "killing chain A's RPC escalates for A only; B keeps
    /// serving and ingesting", and the serving half is where an actual mechanism can be broken.
    ///
    /// The trap this guards is specific and was live once. `/ready`'s stall verdict comes from
    /// `last_poll_ok`, and the process-global aggregate is written by *whichever cursor polled last*.
    /// A runtime hosting a dead chain alongside a healthy one therefore has a permanently fresh global
    /// - so reading it would report the dead chain **ready**, and an operator watching
    /// `/<dead-chain>/ready` would see 200 while nothing was being indexed. The handler reads the
    /// per-nest counter instead, and this pins that.
    ///
    /// Note the shape of the assertions: "chain B still answers 200" is the half that passes trivially
    /// when the mechanism is missing (a fresh global says ready to everyone). The load-bearing half is
    /// that chain **A** answers 503 *at the same time*, which only per-nest attribution can produce.
    ///
    /// Deliberately built from **un-stamped** [`test_state`] - `runtime_health` is left `None`, same as
    /// every other test in this module - and composed via [`compose_runtime`] rather than stamped by
    /// hand. That is the fix for #388: three fixtures in a row (#292, #356, this one) forgot to
    /// replicate `spawn_runtime`'s stamp and silently proved the solo path instead, so the stamp now
    /// lives once in `compose_runtime` itself and this test exists to pin that it actually fires - a
    /// version of this test that reverted `compose_runtime`'s `get_or_insert_with` back to a no-op would
    /// turn red here, both nests reporting via the process globals.
    #[tokio::test]
    async fn one_chains_dead_endpoints_leave_its_co_tenant_serving_and_report_only_itself_stalled()
    {
        let dir = tempfile::tempdir().unwrap();
        // Names unique to this test: `METRICS.nest(..)` is a process-global map shared by every test
        // in the binary, so reusing `alpha`/`beta` would have them writing each other's counters.
        let (dead, live) = ("stall-iso-dead", "stall-iso-live");
        std::fs::create_dir_all(dir.path().join(dead)).unwrap();
        std::fs::create_dir_all(dir.path().join(live)).unwrap();
        let roster = json!({"runtime": "t", "nests": [{"name": dead}, {"name": live}]});
        let health = Arc::new(crate::health::RuntimeHealth::new());
        let nests: Vec<(String, AppState)> = [dead, live]
            .into_iter()
            .map(|name| {
                let st = test_state(&dir.path().join(name), 4);
                (name.to_string(), st)
            })
            .collect();
        let router = compose_runtime(roster, nests, health);

        // Chain A's endpoints went dark ten minutes ago. Chain B polled a second ago - and, being
        // healthy, is still writing the global aggregate. That is the trap, set deliberately.
        let now = crate::metrics::now_unix();
        crate::metrics::METRICS
            .nest(dead)
            .set_last_poll_ok_for_test(now.saturating_sub(600));
        crate::metrics::METRICS
            .nest(live)
            .set_last_poll_ok_for_test(now.saturating_sub(1));
        crate::metrics::METRICS.mark_poll_ok();

        let (code_dead, body_dead) = get(router.clone(), &format!("/{dead}/ready")).await;
        let (code_live, body_live) = get(router, &format!("/{live}/ready")).await;
        let json_of = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();
        let (dead_json, live_json) = (json_of(&body_dead), json_of(&body_live));

        // The load-bearing half: the dead chain is reported dead, despite a fresh global.
        assert_eq!(
            code_dead,
            StatusCode::SERVICE_UNAVAILABLE,
            "the stalled cursor must fail readiness: {dead_json}"
        );
        assert_eq!(dead_json["stalled"], json!(true));
        assert_eq!(dead_json["ready"], json!(false));

        // …and the isolation half: its co-tenant is untouched by the neighbour's dead provider.
        assert_eq!(
            code_live,
            StatusCode::OK,
            "the healthy cursor must keep serving: {live_json}"
        );
        assert_eq!(live_json["stalled"], json!(false));
        assert_eq!(live_json["ready"], json!(true));
    }

    /// #510: a nest whose RPC pool has been dead since before its very first successful poll must
    /// eventually answer `/ready` with `stalled`, not a permanent healthy-looking
    /// `{"stalled":false,"last_poll_unixtime":0}` - the exact body a fully dead pool served forever
    /// before this fix (the process used to crash before this endpoint even got a chance to answer).
    #[tokio::test]
    async fn a_never_polled_nest_stalls_once_its_start_grace_expires() {
        let dir = tempfile::tempdir().unwrap();
        let name = "cold-start-dead";
        std::fs::create_dir_all(dir.path().join(name)).unwrap();
        let roster = json!({"runtime": "t", "nests": [{"name": name}]});
        let health = Arc::new(crate::health::RuntimeHealth::new());
        let nests = vec![(name.to_string(), test_state(&dir.path().join(name), 4))];
        let router = compose_runtime(roster, nests, health);

        let now = crate::metrics::now_unix();
        let handle = crate::metrics::METRICS.nest(name);
        // Never successfully polled, and started well past the 90s grace window.
        handle.set_started_at_for_test(now.saturating_sub(200));
        assert_eq!(handle.last_poll_ok(), 0, "premise: this nest never polled");

        let (code, body) = get(router, &format!("/{name}/ready")).await;
        let json = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
        assert_eq!(
            code,
            StatusCode::SERVICE_UNAVAILABLE,
            "a pool dead since before the first poll must fail readiness: {json}"
        );
        assert_eq!(json["stalled"], json!(true));
        assert_eq!(json["ready"], json!(false));
    }

    /// A two-nest mounts composition, built the same way `run_runtime` builds one.
    fn two_nest_roost(dir: &std::path::Path, health: Arc<crate::health::RuntimeHealth>) -> Router {
        let roster = json!({"runtime": "t", "nests": [{"name": "alpha"}, {"name": "beta"}]});
        let nests = vec![
            ("alpha".to_string(), test_state(&dir.join("a"), 4)),
            ("beta".to_string(), test_state(&dir.join("b"), 4)),
        ];
        compose_runtime(roster, nests, health)
    }

    /// RFC-0027 slice 1: serving through the swappable handle must be **indistinguishable** from
    /// serving the composed router directly.
    ///
    /// This is the whole point of slice 1. The lifecycle slices are only safe to build on top of an
    /// indirection that provably changes nothing, and "it looked fine when I clicked around" is not
    /// that proof - so every root route, a per-nest route under its prefix, and a 404 are compared
    /// **status and body bytes**, both ways.
    #[tokio::test]
    async fn the_dispatcher_serves_byte_identically_to_the_static_router() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();
        let health = Arc::new(crate::health::RuntimeHealth::new());
        health.register("alpha", "mainnet");
        health.register("beta", "mainnet");

        for path in [
            "/health",
            "/ready",
            "/nests",
            "/alpha/health",
            "/beta/health",
            "/alpha/ready",
            "/nope",         // 404 at the root
            "/alpha/nope",   // 404 within a mounted nest
            "/gamma/health", // 404 for a nest that isn't mounted
        ] {
            let direct = get(two_nest_roost(tmp.path(), health.clone()), path).await;
            let dispatched = get(
                LiveRuntime::new(two_nest_roost(tmp.path(), health.clone())).service(),
                path,
            )
            .await;
            assert_eq!(
                direct, dispatched,
                "{path} must be identical through the dispatcher; got {direct:?} vs {dispatched:?}"
            );
        }
    }

    /// The capability slice 1 exists to enable, proven now so the lifecycle slices inherit it rather
    /// than discover it: swapping the composition changes what is served, without rebinding anything.
    #[tokio::test]
    async fn swapping_the_composition_changes_what_is_served() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("b")).unwrap();
        let health = Arc::new(crate::health::RuntimeHealth::new());
        health.register("alpha", "mainnet");
        health.register("beta", "mainnet");

        let live = LiveRuntime::new(two_nest_roost(tmp.path(), health.clone()));
        let (status, _) = get(live.service(), "/beta/health").await;
        assert_eq!(status, StatusCode::OK, "beta is mounted to begin with");

        // Re-compose with beta absent - the shape an unmount will take.
        //
        // The replacement `alpha` state opens a *different* store directory, because redb is
        // single-writer and the original composition still holds `a/t.redb`. That is not a test
        // artefact: it is exactly why RFC-0027 §6 makes unmount a **drain** - the old nest's store has
        // to be closed cleanly before anything reopens it, and a half-closed store is the one way a
        // lifecycle operation could corrupt data that faults never do.
        std::fs::create_dir_all(tmp.path().join("a2")).unwrap();
        let roster = json!({"runtime": "t", "nests": [{"name": "alpha"}]});
        let only_alpha = compose_runtime(
            roster,
            vec![("alpha".to_string(), test_state(&tmp.path().join("a2"), 4))],
            health.clone(),
        );
        live.swap(only_alpha);

        let (alpha, _) = get(live.service(), "/alpha/health").await;
        let (beta, _) = get(live.service(), "/beta/health").await;
        assert_eq!(alpha, StatusCode::OK, "alpha keeps serving across the swap");
        assert_eq!(beta, StatusCode::NOT_FOUND, "beta is gone from the new set");
    }

    /// RFC-0026 §5: the roster is a *live* view. The static half is computed at startup, but health is
    /// merged per request - and a quarantined nest must never come back reported as indexing.
    #[test]
    fn the_roster_reports_live_health_per_nest() {
        use crate::health::RuntimeHealth;
        let health = RuntimeHealth::new();
        health.register("alpha", "base");
        health.register("beta", "base");
        let roster = json!({
            "runtime": "test",
            "nests": [ {"name": "alpha", "chain": "base"}, {"name": "beta", "chain": "base"} ],
        });

        // All healthy: every entry says so, and nothing carries a quarantine block.
        let merged = merge_roster_health(&roster, &health);
        assert_eq!(merged["nests"][0]["health"], "indexing");
        assert!(merged["nests"][0].get("quarantine").is_none());
        assert_eq!(merged["all_indexing"], json!(true));

        // Quarantine one: only that entry changes, and it carries the reason an operator needs.
        health.quarantine_nest(
            "alpha",
            "the balance IVM circuit thread has died".into(),
            0,
            None,
        );
        let merged = merge_roster_health(&roster, &health);
        assert_eq!(merged["nests"][0]["health"], "quarantined");
        assert_eq!(merged["nests"][0]["quarantine"]["class"], "terminal");
        assert!(merged["nests"][0]["quarantine"]["reason"]
            .as_str()
            .unwrap()
            .contains("balance IVM"));
        assert_eq!(merged["nests"][1]["health"], "indexing");
        assert_eq!(merged["all_indexing"], json!(false));

        // Recovery clears the quarantine block rather than leaving a stale one behind.
        health.mark_indexing("alpha");
        let merged = merge_roster_health(&roster, &health);
        assert_eq!(merged["nests"][0]["health"], "indexing");
        assert!(merged["nests"][0].get("quarantine").is_none());
    }

    /// RFC-0026 §5: mounts-root `/ready` is 200 only while **everything** is indexing, and 503 names the
    /// offenders. Conservative on purpose - a supervisor should treat a partly-broken mounts as
    /// not-ready, while the healthy nests carry on serving anyone who asks for them directly.
    #[tokio::test]
    async fn roost_readiness_goes_unavailable_as_soon_as_anything_is_quarantined() {
        use crate::health::RuntimeHealth;
        let health = RuntimeHealth::new();
        health.register("alpha", "base");
        health.register("beta", "arbitrum-one");
        assert_eq!(
            roost_ready(&health).into_response().status(),
            StatusCode::OK
        );

        // One chain's cursor dies: the runtime is no longer ready, even though `beta` is perfectly fine.
        health.quarantine_cursor(
            "base",
            "every nest on this cursor is terminally quarantined".into(),
        );
        let resp = roost_ready(&health).into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["ready"], json!(false));
        assert_eq!(v["quarantined"][0]["nest"], "alpha");
        assert_eq!(
            v["quarantined"].as_array().unwrap().len(),
            1,
            "beta is fine"
        );
    }

    /// #445: a contract-free blocks nest has no address to name, and `GET /` says so with `null`
    /// rather than inventing one. The key stays present either way - a client reading `.address`
    /// gets a value it can test, not a missing field it has to guess about.
    #[test]
    fn the_summary_names_no_address_for_a_contract_free_nest() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = test_state(dir.path(), 1);
        s.address = None;
        let v = summary_value(&s);
        assert!(
            v.get("address").is_some(),
            "the address key stays present for a contract-free nest"
        );
        assert!(
            v["address"].is_null(),
            "and its value is null, not an invented address: {}",
            v["address"]
        );
    }

    /// The RFC-0020 hot-swap mechanism: re-pointing a `SharedNest` atomically changes what serving
    /// resolves - both `current()` and the per-request `FromRef` a handler goes through - with no
    /// rebind. This is what lets a compatible upgrade flip an endpoint's backing underneath live
    /// traffic (slice 2b drives the flip; this proves the mechanism).
    #[tokio::test]
    async fn shared_nest_swaps_the_backing_atomically() {
        use axum::extract::FromRef;
        let d1 = tempfile::tempdir().unwrap();
        let mut v1 = test_state(d1.path(), 1);
        v1.address = Some("0xv1".into());
        let shared = SharedNest::new(v1);
        assert_eq!(shared.current().address.as_deref(), Some("0xv1"));
        assert_eq!(AppState::from_ref(&shared).address.as_deref(), Some("0xv1"));

        // Flip to a new backing version - same handle, next request sees v2.
        let d2 = tempfile::tempdir().unwrap();
        let mut v2 = test_state(d2.path(), 1);
        v2.address = Some("0xv2".into());
        shared.swap(v2);
        assert_eq!(shared.current().address.as_deref(), Some("0xv2"));
        assert_eq!(AppState::from_ref(&shared).address.as_deref(), Some("0xv2"));
    }

    /// When the analytical gate is saturated, `/sql` fails fast with 503 rather than piling on.
    #[tokio::test]
    async fn sql_returns_503_when_gate_saturated() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), 1);
        // Hold the only permit - the gate is now saturated for the duration of the call.
        let held = Arc::clone(&state.sql_gate).try_acquire_owned().unwrap();
        let resp = sql(
            State(state.clone()),
            Query(SqlQuery {
                q: "SELECT 1".into(),
                max_rows: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(held);
    }

    /// A multichain runtime runs one cursor per chain, so `/<nest>/ready` must answer from **that
    /// nest's** counters.
    ///
    /// Found by the RFC-0021 live two-chain run rather than by any test: a mainnet nest reported
    /// `tip: 488677305` - an Arbitrum height - while mainnet was at 25,632,906, next to a mainnet
    /// `sealed_through`. One body, two chains, and nothing in it to tell an operator which was which.
    /// Whichever cursor polled last simply overwrote the shared gauge.
    #[tokio::test]
    async fn per_nest_readiness_does_not_report_another_chains_tip() {
        use crate::metrics::METRICS;
        let tmp = tempfile::tempdir().unwrap();
        let health = Arc::new(crate::health::RuntimeHealth::new());
        health.register("on-mainnet", "mainnet");
        health.register("on-arbitrum", "arbitrum-one");

        // Two cursors publishing wildly different heights, as two real chains do.
        let eth = METRICS.nest("on-mainnet");
        let arb = METRICS.nest("on-arbitrum");
        eth.set_last_block(25_632_840);
        eth.set_tip(25_632_906);
        eth.mark_poll_ok();
        // Arbitrum polls *after* mainnet, so it is the one that owns the global gauge.
        arb.set_last_block(488_677_300);
        arb.set_tip(488_677_305);
        arb.mark_poll_ok();

        let mut state = test_state(tmp.path(), 1);
        state.runtime_health = Some(("on-mainnet".to_string(), health.clone()));
        let body = ready(State(state)).await.into_response();
        let bytes = axum::body::to_bytes(body.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            v["tip"].as_u64(),
            Some(25_632_906),
            "the mainnet nest must report mainnet's tip, not whichever cursor polled last: {v}"
        );
        assert_eq!(v["last_block"].as_u64(), Some(25_632_840));
        assert_eq!(
            v["lag_blocks"].as_u64(),
            Some(66),
            "lag computed across two chains is meaningless - it was ~463 million before this fix"
        );
    }

    /// The `/sql` RAM guard (the "node owns resource safety" half of the CLAUDE.md division of
    /// labour): the unsealed tip is materialised per query, so an unbounded one is the largest RAM
    /// risk the process carries - and in a runtime it is a co-tenant's problem too.
    ///
    /// It must **fail**, not truncate. Serving a partial tip would silently change the answer to an
    /// aggregate, and a `count(*)` quietly missing rows is far worse than a query that refuses.
    #[tokio::test]
    async fn an_oversized_hot_tip_is_refused_rather_than_partially_served() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        // Three rows in the hot store, then a cap of two.
        for b in 1..=3u64 {
            state
                .store
                .put_entity(
                    &format!("k{b}"),
                    &json!({"table": "t", "block_number": b}).to_string(),
                )
                .unwrap();
        }
        let err = state
            .store
            .hot_rows_by_table_bounded(2)
            .expect_err("a tip over the cap must refuse");
        assert!(
            err.downcast_ref::<crate::store::HotScanTooLarge>().is_some(),
            "the refusal must be typed so the handler can map it to 503 without matching prose: {err:#}"
        );
        // Under the cap it behaves exactly as the unbounded scan does.
        let ok = state
            .store
            .hot_rows_by_table_bounded(3)
            .expect("at the cap");
        assert_eq!(ok.get("t").map(|v| v.len()), Some(3));
    }

    /// No request handler may reach the tip through the unbounded scan (#293).
    ///
    /// `/sql` has always been bounded; `/explain` was not, and because the `LIMIT 0` probe hides
    /// the cost — no rows come back — the gap was invisible from the outside while still parsing
    /// the whole tip into temp tables. The cap itself is 2,000,000 rows, far too many to
    /// materialise in a test, so the invariant is checked where it is actually expressible: no
    /// handler in this file calls the unbounded variant at all.
    ///
    /// `main.rs` (the local REPL backend) and `bench.rs` legitimately still use it — neither is
    /// reachable over the network, which is the whole distinction this guard draws.
    #[test]
    fn no_request_path_uses_the_unbounded_hot_scan() {
        let src = include_str!("serve.rs");
        // Split so the needle never appears verbatim in this file - otherwise the scan matches
        // the line it is written on and the test fails against itself.
        let needle = concat!("hot_rows_by_table", "()");
        let offenders: Vec<usize> = src
            .lines()
            .enumerate()
            .filter(|(_, l)| l.contains(needle) && !l.trim_start().starts_with("//"))
            .map(|(i, _)| i + 1)
            .collect();
        assert!(
            offenders.is_empty(),
            "serve.rs serves HTTP, so every hot scan here must be bounded by SQL_MAX_HOT_ROWS; \
             unbounded call(s) at line(s) {offenders:?}. Use hot_rows_by_table_bounded and map \
             HotScanTooLarge to a 503, as /sql and /explain do."
        );
    }

    /// An over-length query string is rejected (400) before it ever reaches the planner.
    #[tokio::test]
    async fn sql_rejects_overlong_query() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        let long = format!("SELECT {}", "1,".repeat(SQL_MAX_QUERY_LEN)); // well past the cap
        let resp = sql(
            State(state),
            Query(SqlQuery {
                q: long,
                max_rows: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A well-formed query passes the gate and runs (200) - the guard doesn't block legitimate use.
    #[tokio::test]
    async fn sql_serves_a_normal_query() {
        let tmp = tempfile::tempdir().unwrap();
        let state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        let resp = sql(
            State(state),
            Query(SqlQuery {
                q: "SELECT 1 AS n".into(),
                max_rows: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// #378: an over-budget tip is refused by both `/sql` and `/explain`, not just by the store layer
    /// underneath them. Neither handler was ever driven directly by a test before this - the typed
    /// error (`HotScanTooLarge`) and the call site (`no_request_path_uses_the_unbounded_hot_scan`)
    /// were each covered, but the mapping between them - `503`, this exact body shape - was not, so
    /// deleting either handler's refusal arm left the suite green.
    ///
    /// `SQL_MAX_HOT_ROWS` is `2_000_000`; reaching the refusal for real would mean putting two
    /// million rows in the hot store. `AppState::sql_max_hot_rows` exists so this test can lower the
    /// seam instead, exactly as production leaves it at the real constant (`test_state`, `indexer.rs`).
    #[tokio::test]
    async fn a_hot_tip_over_the_configured_cap_is_refused_by_sql_and_explain() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        state.sql_max_hot_rows = 2;
        for b in 1..=3u64 {
            state
                .store
                .put_entity(
                    &format!("k{b}"),
                    &json!({"table": "t", "block_number": b}).to_string(),
                )
                .unwrap();
        }
        let q = || {
            Query(SqlQuery {
                q: "SELECT 1 AS n".into(),
                max_rows: None,
            })
        };

        let resp = sql(State(state.clone()), q()).await.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "/sql");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("will not materialise"),
            "/sql body: {v}"
        );
        assert_eq!(v["sealed_through"], json!(0));

        let resp = explain(State(state), q()).await.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE, "/explain");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            v["error"]
                .as_str()
                .unwrap()
                .contains("will not materialise"),
            "/explain body: {v}"
        );
        assert_eq!(v["sealed_through"], json!(0));
        // `valid` is deliberately absent, not `false` - bindability is unknown, not disproved.
        assert!(
            v.get("valid").is_none(),
            "/explain must omit `valid` on refusal: {v}"
        );
    }

    /// RFC-0010 Part A: the admin UI serves when enabled and 404s when disabled (`--no-admin` or a
    /// public bind without a token). `/nest` returns the static nest metadata either way.
    #[tokio::test]
    async fn admin_ui_gated_and_nest_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        let no_tok = || Query(AdminQuery { token: None });
        let no_hdr = axum::http::HeaderMap::new;

        // Localhost (admin_token None): open.
        let resp = admin_index(State(state.clone()), no_tok(), no_hdr())
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK, "admin UI open on localhost");

        state.admin_enabled = false;
        let resp = admin_index(State(state.clone()), no_tok(), no_hdr())
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "admin UI 404s when disabled"
        );

        let resp = nest(State(state)).await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn admin_token_enforced_off_localhost() {
        // SEC-5: with a required token set (off-localhost), the route must actually CHECK it per
        // request - not merely be enabled by the env var's presence.
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);
        state.admin_token = Some("s3cret".into());

        let tok = |t: Option<&str>| {
            Query(AdminQuery {
                token: t.map(str::to_string),
            })
        };
        let no_hdr = axum::http::HeaderMap::new;
        /// An `Authorization` header carrying `value`.
        fn bearer(value: &str) -> axum::http::HeaderMap {
            let mut h = axum::http::HeaderMap::new();
            h.insert(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_str(value).unwrap(),
            );
            h
        }
        let status = |s: AppState, q, h| async move {
            admin_index(State(s), q, h).await.into_response().status()
        };

        // --- The query-parameter form (SEC-5), unchanged. ---
        assert_eq!(
            status(state.clone(), tok(None), no_hdr()).await,
            StatusCode::UNAUTHORIZED,
            "no token"
        );
        assert_eq!(
            status(state.clone(), tok(Some("nope")), no_hdr()).await,
            StatusCode::UNAUTHORIZED,
            "wrong token"
        );
        assert_eq!(
            status(state.clone(), tok(Some("s3cret")), no_hdr()).await,
            StatusCode::OK,
            "correct token"
        );

        // --- The header form (audit L7): a caller that can set headers need not put the secret in a
        // query string, where it would land in proxy logs and `Referer`. ---
        assert_eq!(
            status(state.clone(), tok(None), bearer("Bearer s3cret")).await,
            StatusCode::OK,
            "Authorization: Bearer must be accepted"
        );
        // RFC 7235: the scheme is case-insensitive.
        assert_eq!(
            status(state.clone(), tok(None), bearer("bearer s3cret")).await,
            StatusCode::OK,
            "the scheme is case-insensitive"
        );
        // …but nothing else is loosened.
        for bad in [
            "Bearer nope",
            "Basic s3cret",
            "s3cret",
            "Bearer",
            "Bearer ",
            "Bearer s3cret extra",
        ] {
            assert_eq!(
                status(state.clone(), tok(None), bearer(bad)).await,
                StatusCode::UNAUTHORIZED,
                "must reject Authorization: {bad}"
            );
        }
        // A valid header still wins when the query param is wrong (either channel may authorise).
        assert_eq!(
            status(state, tok(Some("nope")), bearer("Bearer s3cret")).await,
            StatusCode::OK
        );
    }

    #[test]
    fn admin_html_is_embedded_and_bounded() {
        assert!(ADMIN_HTML.contains("<title>nuthatch</title>"));
        // The RFC-0010 budget: the embedded UI stays well under 150 KB and pulls in nothing external.
        assert!(ADMIN_HTML.len() < 150 * 1024, "admin UI ≤ 150 KB");
        assert!(
            !ADMIN_HTML.contains("http://") && !ADMIN_HTML.contains("https://"),
            "admin UI makes no external requests (same-origin only)"
        );
        // The status view is server-pushed (SSE) with a polling fallback - not poll-only.
        assert!(ADMIN_HTML.contains("EventSource"), "admin UI uses SSE");
        assert!(
            ADMIN_HTML.contains("_admin/events"),
            "admin UI subscribes to the events stream"
        );
        assert!(
            ADMIN_HTML.contains("startPolling"),
            "admin UI keeps a polling fallback"
        );
        // The #435 caveat, pinned the same way and for the same reason as the three above: substring,
        // no render. Of the four surfaces carrying the reduced-cold-data signal this is the only one
        // with no coverage of any kind, so deleting the caveat here would be silent.
        assert!(
            ADMIN_HTML.contains("degraded_tables"),
            "admin UI renders the reduced-cold-data caveat (#435)"
        );
        // Same reasoning, same pin, for the other kind of incomplete (#472): a lost tip is nest-wide
        // and every-table, not per-table, so it needs its own substring or a deleted caveat here is
        // just as silent as the arm that started this.
        assert!(
            ADMIN_HTML.contains("tip_unavailable"),
            "admin UI renders the lost-tip caveat (#472)"
        );
    }

    #[tokio::test]
    async fn admin_events_gated_like_the_admin_page() {
        let tmp = tempfile::tempdir().unwrap();
        let mut state = test_state(tmp.path(), SQL_MAX_CONCURRENCY);

        // A pushed frame is byte-identical to `GET /`: both go through `summary_value`.
        let v = summary_value(&state);
        assert_eq!(v["name"], "nuthatch");
        assert!(
            v.get("last_block").is_some(),
            "frame carries the tip watermark"
        );
        assert!(v["views"].is_array(), "frame lists the IVM views");

        let tok = |t: Option<&str>| {
            Query(AdminQuery {
                token: t.map(str::to_string),
            })
        };

        // Localhost, no token required → the stream opens.
        assert_eq!(
            admin_events(
                State(state.clone()),
                tok(None),
                axum::http::HeaderMap::new()
            )
            .await
            .into_response()
            .status(),
            StatusCode::OK
        );
        // Admin disabled → 404, exactly like the page.
        state.admin_enabled = false;
        assert_eq!(
            admin_events(
                State(state.clone()),
                tok(None),
                axum::http::HeaderMap::new()
            )
            .await
            .into_response()
            .status(),
            StatusCode::NOT_FOUND
        );
        // Off-localhost token enforced (SEC-5): missing → 401, correct → 200.
        state.admin_enabled = true;
        state.admin_token = Some("s3cret".into());
        assert_eq!(
            admin_events(
                State(state.clone()),
                tok(None),
                axum::http::HeaderMap::new()
            )
            .await
            .into_response()
            .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            admin_events(
                State(state.clone()),
                tok(Some("s3cret")),
                axum::http::HeaderMap::new()
            )
            .await
            .into_response()
            .status(),
            StatusCode::OK
        );
        // The SSE stream accepts the header form too (audit L7), even though the admin UI's own
        // `EventSource` cannot send one - a scripted consumer can.
        let mut hdr = axum::http::HeaderMap::new();
        hdr.insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer s3cret"),
        );
        assert_eq!(
            admin_events(State(state), tok(None), hdr)
                .await
                .into_response()
                .status(),
            StatusCode::OK
        );
    }
}
