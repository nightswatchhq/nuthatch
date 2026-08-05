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

/// The **mount table**, at the runtime directory root (RFC-0032 §4).
///
/// Runtime state that the runtime owns, not authored config an operator maintains. It records which
/// alias, for which tenant, serves which nest identity - and nothing else. `roost.toml` was its name
/// until 2.0, when the roost was retired as a concept; a directory still carrying the old file is
/// pointed at `nuthatch migrate` rather than silently ignored.
pub const MOUNTS_FILE: &str = "mounts.toml";

/// The pre-2.0 name for [`MOUNTS_FILE`]. Retained **only** so a directory that has not been migrated
/// can be recognised and named in an error, and so `migrate` can read it.
pub const LEGACY_ROOST_FILE: &str = "roost.toml";

/// Where mounted nests live under the roost dir: `nests/<name>/` is a nest directory, exactly as a
/// standalone nest is today.
///
/// **Pre-2.0 layout.** RFC-0032 replaces it with [`DATA_DIR`] keyed by nest identity; this constant
/// stays for un-migrated roosts and for `migrate`'s source side.
pub const NESTS_DIR: &str = "nests";

/// Where a nest's inputs and data live once addressed by identity: `data/<nid>/` (RFC-0032 §4).
///
/// The directory does not know what it is called or who mounted it - [`Mount`] records hold all of
/// that. That separation is the whole point: it is what lets two mounts name one dataset.
pub const DATA_DIR: &str = "data";

/// One mount record: an alias, and the identity of the nest whose dataset it serves (RFC-0032 §4).
///
/// **Slice 1 carries `alias` and `nid` only.** The `tenant` column and refcount semantics land on
/// this struct in slices 2-3 rather than beside it, so the growth is additive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Mount {
    /// Who mounted it (RFC-0032 §6). An **opaque string** - nuthatch refcounts it and knows nothing
    /// else about it. No authz, no quotas, no metering; identity stays the gateway's job.
    ///
    /// Always a real value, never `Option<String>` and never null. Single-tenant is `N=1` with
    /// [`DEFAULT_TENANT`], not a special case, so there is one code path rather than two - and the
    /// one almost every user is on is the one that would otherwise rot.
    #[serde(default = "default_tenant")]
    pub tenant: String,
    /// The name this mount is served under. Free-form, unique **within a tenant**, and *not* part of
    /// the nest's identity.
    pub alias: String,
    /// The nest identity ([`crate::blob::Manifest::nid`]) - the dataset key.
    pub nid: String,
    /// How much SQL surface this mount exposes (RFC-0034 §2). **Mount config, not manifest**, so
    /// changing it leaves the NID untouched, re-indexes nothing, and lets two tenants share one
    /// dataset while exposing different surfaces.
    #[serde(default)]
    pub sql: crate::allowlist::SqlAccess,
    /// The queries this mount answers by name, when `sql = "allowlist"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queries: Vec<crate::allowlist::NamedQuery>,
}

impl Mount {
    /// This mount's SQL surface.
    pub fn surface(&self) -> crate::allowlist::Surface {
        crate::allowlist::Surface {
            access: self.sql,
            queries: self.queries.clone(),
        }
    }
}

/// The tenant a mount belongs to when nobody said otherwise. Operator-configurable per roost via
/// `[roost] default_tenant`.
pub const DEFAULT_TENANT: &str = "default";

fn default_tenant() -> String {
    DEFAULT_TENANT.to_string()
}

/// One mount's coordinates: who mounted it, and what they call it (RFC-0032 §4, §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountRef {
    pub tenant: String,
    pub alias: String,
}

impl std::fmt::Display for MountRef {
    /// Always tenant-qualified, unlike [`MountRef::route_key`]. Logs and errors must stay
    /// unambiguous even in a single-tenant runtime whose routes omit the tenant.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.tenant, self.alias)
    }
}

impl MountRef {
    /// The path this mount is served under (RFC-0032 §7), and the key its health and footprint are
    /// recorded against.
    ///
    /// `multi_tenant` decides whether the tenant appears: with one tenant in the runtime the segment
    /// would be pure ceremony, and today's URLs must not move for the overwhelming majority of
    /// users who will never type the word "tenant".
    pub fn route_key(&self, multi_tenant: bool) -> String {
        if multi_tenant {
            format!("{}/{}", self.tenant, self.alias)
        } else {
            self.alias.clone()
        }
    }
}

/// A roost manifest: the mounted nests plus the chain(s) they follow. A roost may host nests across
/// **one or more chains** (RFC-0021) - one isolated cursor per distinct chain. The single-chain form
/// keeps the top-level `chain`/`chain_id`/`rpc_urls`; a multichain roost lists its chains under
/// `[[chains]]` and lets each nest declare its own chain. The single-cursor law holds **per chain**:
/// never multiplex two chains behind one cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Roost {
    /// Serialised as `[runtime]`. Was `[roost]` until 2.0, when the roost was retired as a concept
    /// (RFC-0032 slice 5); `migrate` rewrites the section along with everything else.
    #[serde(rename = "runtime", alias = "roost")]
    pub roost: RoostMeta,
    /// Multichain: each chain the roost serves, with its own RPC endpoints (RFC-0021). Mutually
    /// exclusive with the top-level `chain`/`chain_id`. Empty → the single-chain top-level form.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainEndpoint>,
    /// Mount records (RFC-0032 §4): which alias serves which nest identity. Empty means this roost
    /// has not been migrated and still resolves nests by name under [`NESTS_DIR`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
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
    /// The mounted nests, by directory name under `nests/`. Superseded by `[[mounts]]` once the
    /// roost is migrated (RFC-0032 §4) - see [`Roost::mount_refs`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nests: Vec<String>,
    /// The tenant a mount belongs to when it does not say (RFC-0032 §6). Operator-configurable so a
    /// single-tenant deployment can call its tenant whatever it likes; absent → [`DEFAULT_TENANT`].
    /// Never `None` in effect - [`Roost::tenant_default`] always yields a real string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tenant: Option<String>,
    /// Resident-set ceiling **per active-chain cursor**, in MB (RFC-0021 - the footprint budget is
    /// per-cursor; a roost's total is Σ cursors). A cursor whose *projected* RSS exceeds this is refused
    /// before it starts. Absent → the CLAUDE.md 2 GB budget ([`DEFAULT_MAX_RSS_MB`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rss_mb: Option<u64>,
}

/// SEC-10: anything that becomes a filesystem path segment *and* a route segment gets this charset.
///
/// Tenants and aliases are both. A tenant being **opaque** to nuthatch (RFC-0032 §6) means we never
/// interpret it, not that it may contain `..` or a separator - opacity is about meaning, not about
/// what a path resolver will do with it.
fn safe_segment(value: &str, what: &str) -> Result<()> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        bail!("{what} '{value}' is invalid (allowed: letters, digits, '_', '-')");
    }
    Ok(())
}

/// The default per-cursor RSS ceiling: the CLAUDE.md ≤2 GB budget (RFC-0021 - now per active-chain
/// cursor, not per whole runtime).
pub const DEFAULT_MAX_RSS_MB: u64 = 2048;

// A deliberately rough, *honest* per-runtime footprint model (RFC-0012 §3). These are order-of-
// magnitude estimates for the pre-mount projection, not measurements - the roster reports the real
// `rss_bytes()` alongside so an operator can calibrate. The shared serving/runtime cost is paid once;
// each nest adds its hot-store working set + decode registry, plus a chunk per active IVM view.
pub const ROOST_BASE_RSS_MB: u64 = 120; // serving + async runtime + on-demand DuckDB, paid once
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
        Self::load_inner(dir, false)
    }

    /// Load for **migration**, accepting the pre-2.0 file.
    ///
    /// [`Roost::load`] refuses a directory still holding `roost.toml` and points at `nuthatch
    /// migrate`. `migrate` therefore cannot use it: the one command that fixes the problem would
    /// refuse to run *because* the problem exists. This is that door, and it is the only caller.
    pub fn load_for_migration(dir: &Path) -> Result<Roost> {
        Self::load_inner(dir, true)
    }

    fn load_inner(dir: &Path, accept_legacy: bool) -> Result<Roost> {
        let legacy = dir.join(LEGACY_ROOST_FILE);
        let path = if accept_legacy && !dir.join(MOUNTS_FILE).exists() && legacy.exists() {
            legacy
        } else {
            dir.join(MOUNTS_FILE)
        };
        // A directory still carrying the pre-2.0 file is a *migration that has not been run*, not a
        // missing file. Saying so beats "no mounts.toml", which sends an operator looking for a file
        // they never had.
        if !accept_legacy && !path.exists() && dir.join(LEGACY_ROOST_FILE).exists() {
            bail!(
                "{} holds a pre-2.0 {LEGACY_ROOST_FILE}. The roost was retired in 2.0: run \
                 `nuthatch migrate --dir {}` to move its data to identity-keyed datasets and write a \
                 {MOUNTS_FILE}. It moves data and never re-indexes; `--dry-run` prints the plan first.",
                dir.display(),
                dir.display()
            );
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("no {MOUNTS_FILE} in {}", dir.display()))?;
        let roost: Roost =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        if roost.roost.nests.is_empty() && roost.mounts.is_empty() {
            bail!(
                "roost '{}' mounts nothing (no [[mounts]] records and an empty `nests` list)",
                roost.roost.name
            );
        }
        roost.validate_mounts()?;
        // Every mount, whichever form declared it, must be a safe path segment and must not collide
        // with a reserved top-level route - the roster and the per-nest prefixes share one namespace.
        let mut seen = std::collections::HashSet::new();
        for m in roost.mount_refs() {
            safe_segment(&m.alias, "nest name")?;
            safe_segment(&m.tenant, "tenant")?;
            if m.alias == "nests" || m.alias == "health" {
                bail!(
                    "nest name '{}' is reserved (collides with a roost route)",
                    m.alias
                );
            }
            // In a multi-tenant runtime the tenant is the *first* path segment, so it collides with
            // the same two routes an alias would.
            if m.tenant == "nests" || m.tenant == "health" {
                bail!(
                    "tenant '{}' is reserved (collides with a roost route)",
                    m.tenant
                );
            }
            if !seen.insert((m.tenant.clone(), m.alias.clone())) {
                bail!("tenant '{}' mounts '{}' more than once", m.tenant, m.alias);
            }
        }
        Ok(roost)
    }

    /// Validate the mount records (RFC-0032 §4).
    ///
    /// A NID is a **filesystem path segment**, exactly as a nest name is, so it gets the same SEC-10
    /// treatment: hex only, fixed length, no `..`, no separators. The records are written by
    /// `migrate` rather than by hand, but a roost dir is an operator-editable file and a bundle
    /// roster is untrusted input, so the check is on load, not on write.
    fn validate_mounts(&self) -> Result<()> {
        // The primary key is `(tenant, alias)`: two tenants may each call their mount "usdc" and
        // both must work, so uniqueness is *within* a tenant and never global (RFC-0032 §4.1).
        let mut seen = std::collections::HashSet::new();
        for m in &self.mounts {
            if m.nid.len() != 64 || !m.nid.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!(
                    "mount '{}' has an invalid nest identity '{}' (expected 64 hex characters)",
                    m.alias,
                    m.nid
                );
            }
            // SEC-10, again: a tenant is a path segment now, so it gets the same charset as an alias.
            // It is opaque to nuthatch, which is not the same as being allowed to contain `../`.
            safe_segment(&m.tenant, "tenant")?;
            safe_segment(&m.alias, "nest name")?;
            m.surface().validate(&m.alias)?;
            if !seen.insert((&m.tenant, &m.alias)) {
                bail!("tenant '{}' mounts '{}' more than once", m.tenant, m.alias);
            }
        }
        Ok(())
    }

    /// Every mount this roost serves, in serving order (RFC-0032 §4).
    ///
    /// **`[[mounts]]` first, then any `nests` entry no record covers.** The records are the only
    /// form that can express `(acme, usdc)` and `(globex, usdc)` as two distinct mounts, which a
    /// flat list of names cannot - but the list is not simply ignored, because a **half-migrated**
    /// roost is a supported state and `migrate` produces one whenever it refuses a nest. Dropping
    /// the uncovered names would silently unmount exactly the nests that failed to migrate.
    pub fn mount_refs(&self) -> Vec<MountRef> {
        let mut out: Vec<MountRef> = self
            .mounts
            .iter()
            .map(|m| MountRef {
                tenant: m.tenant.clone(),
                alias: m.alias.clone(),
            })
            .collect();
        let tenant = self.tenant_default();
        for alias in &self.roost.nests {
            if !self.mounts.iter().any(|m| &m.alias == alias) {
                out.push(MountRef {
                    tenant: tenant.clone(),
                    alias: alias.clone(),
                });
            }
        }
        out
    }

    /// The tenant a mount with no explicit one belongs to. Operator-configurable, defaulted, never
    /// absent.
    pub fn tenant_default(&self) -> String {
        self.roost
            .default_tenant
            .clone()
            .unwrap_or_else(default_tenant)
    }

    /// Whether more than one tenant is present, which is what decides if routes carry a tenant
    /// segment (RFC-0032 §7). With one tenant the segment is ceremony, so today's URLs do not move.
    pub fn is_multi_tenant(&self) -> bool {
        let mut tenants = self.mount_refs().into_iter().map(|m| m.tenant);
        let Some(first) = tenants.next() else {
            return false;
        };
        tenants.any(|t| t != first)
    }

    /// The on-disk directory of a mounted nest, in the pre-2.0 name-keyed layout.
    ///
    /// Prefer [`Roost::dir_for`], which consults the mount records first. This stays for un-migrated
    /// roosts and as `migrate`'s source side.
    pub fn nest_dir(dir: &Path, name: &str) -> PathBuf {
        dir.join(NESTS_DIR).join(name)
    }

    /// A dataset's directory, keyed by nest identity (RFC-0032 §4).
    pub fn data_dir(dir: &Path, nid: &str) -> PathBuf {
        dir.join(DATA_DIR).join(nid)
    }

    /// The distinct **datasets** this roost mounts, each with every alias serving it (RFC-0032 §4).
    ///
    /// This is the function that makes sharing real. Two aliases naming one nest identity are one
    /// dataset: one store, one place in the cursor, **one backfill**. Iterating `nests` directly
    /// instead - the pre-2.0 shape - would open the same store twice and index the same chain data
    /// twice, which is the entire cost this design exists to remove.
    ///
    /// Order is deterministic: datasets in first-alias order, aliases within a dataset in `nests`
    /// order. `aliases[0]` is the canonical mount - the one that indexes.
    pub fn datasets(&self, dir: &Path) -> Vec<Dataset> {
        let mut out: Vec<Dataset> = Vec::new();
        for m in self.mount_refs() {
            let nid = self
                .mounts
                .iter()
                .find(|r| r.tenant == m.tenant && r.alias == m.alias)
                .map(|r| r.nid.clone());
            let path = match &nid {
                Some(nid) => Self::data_dir(dir, nid),
                None => Self::nest_dir(dir, &m.alias),
            };
            match out.iter_mut().find(|d| d.dir == path) {
                Some(d) => d.mounts.push(m),
                None => out.push(Dataset {
                    dir: path,
                    mounts: vec![m],
                    nid,
                }),
            }
        }
        out
    }

    /// Where `alias` is served from: `data/<nid>` when a mount record exists, else the pre-2.0
    /// `nests/<alias>`.
    ///
    /// Both layouts resolve through this one function so a half-migrated roost - some nests adopted,
    /// a new one dropped into `nests/` - is a supported state rather than an accident.
    ///
    /// Alias-only, so it resolves the **first** tenant mounting that alias. That is unambiguous in a
    /// single-tenant runtime and correct in a multi-tenant one *because the dataset is shared*: two
    /// tenants with the same alias and the same identity resolve to the same directory anyway. Where
    /// the tenant matters - routing, health, footprint - use [`Roost::datasets`], which carries it.
    pub fn dir_for(&self, dir: &Path, alias: &str) -> PathBuf {
        match self.mounts.iter().find(|m| m.alias == alias) {
            Some(m) => Self::data_dir(dir, &m.nid),
            None => Self::nest_dir(dir, alias),
        }
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

/// Give every extra alias of a shared dataset its own route onto the *same* state (RFC-0032 §7).
///
/// One dataset was indexed, under its canonical alias. Each further alias gets a **clone** of that
/// `AppState` - the same `Arc` store, the same directory, the same views. Two doors, one room: no
/// second store, no second backfill, and nothing to keep in sync afterwards because there is only
/// ever one of everything.
///
/// A dataset whose canonical mount is absent (its cursor was refused) is skipped rather than
/// aliased onto nothing.
pub fn fan_out_aliases(
    datasets: &[Dataset],
    mut states: Vec<(String, crate::serve::AppState)>,
    health: &crate::health::RoostHealth,
    estimates: &mut std::collections::HashMap<String, u64>,
    multi_tenant: bool,
) -> Vec<(String, crate::serve::AppState)> {
    let mut extra = Vec::new();
    for ds in datasets {
        let canonical = ds.canonical().route_key(multi_tenant);
        let Some((_, state)) = states.iter().find(|(n, _)| n == &canonical) else {
            continue;
        };
        for m in &ds.mounts[1..] {
            let key = m.route_key(multi_tenant);
            health.register_alias(&key, &canonical, &state.chain);
            // The footprint was charged once, to the dataset. Charging it again per mount would make
            // the per-cursor budget refuse a mount that costs nothing - sharing must not be taxed.
            estimates.insert(key.clone(), 0);
            extra.push((key, state.clone()));
        }
    }
    states.extend(extra);
    states
}

/// One dataset and every alias serving it (RFC-0032 §4-§5) - "two doors, one room".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dataset {
    /// Where the data lives: `data/<nid>`, or `nests/<alias>` for a nest not yet migrated.
    pub dir: PathBuf,
    /// Every mount serving this dataset, in serving order. Never empty.
    pub mounts: Vec<MountRef>,
    /// The nest identity, when this dataset is identity-keyed. `None` for the pre-2.0 layout.
    pub nid: Option<String>,
}

impl Dataset {
    /// The mount that indexes. The others are additional doors onto the same room.
    pub fn canonical(&self) -> &MountRef {
        &self.mounts[0]
    }

    /// How many mounts want this dataset (RFC-0032 §5).
    ///
    /// **Derived, never stored.** A count over the mount table cannot drift out of sync with the
    /// table, which is the failure mode every hand-maintained refcount eventually reaches.
    pub fn refcount(&self) -> usize {
        self.mounts.len()
    }
}

/// A chain's cursor unit (RFC-0021): the endpoint (RPC) plus the mounted nests that follow that chain.
/// Each becomes one isolated cursor - the single-cursor law, held per chain.
#[derive(Debug)]
pub struct ChainGroup {
    pub endpoint: ChainEndpoint,
    pub nests: Vec<(String, PathBuf, Config)>,
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

/// `nuthatch dev --dir <dir>` where `<dir>` holds a mount table: bring up every mounted nest and
/// serve them behind one listener. Reached from the same `dev` a single nest uses (RFC-0032).
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

    // Load every mounted **dataset** - not every alias (RFC-0032 §4). Aliases sharing one nest
    // identity share one store and one place in the cursor; only the canonical one indexes, and the
    // rest become extra routes onto it further down.
    let datasets = roost.datasets(&dir);
    let multi_tenant = roost.is_multi_tenant();
    let mut mounted = Vec::with_capacity(datasets.len());
    for ds in &datasets {
        let config = Config::load(&ds.dir).with_context(|| {
            format!(
                "loading mounted nest '{}' from {}",
                ds.canonical(),
                ds.dir.display()
            )
        })?;
        mounted.push((
            ds.canonical().route_key(multi_tenant),
            ds.dir.clone(),
            config,
        ));
    }
    let groups = group_by_chain(&endpoints, mounted)?;

    // A mount may narrow its author's ceiling, never widen it (RFC-0034 §3). Checked before any
    // store is opened, so a nest exposing more than its author sanctioned fails to start rather than
    // serving the excess until somebody notices.
    for ds in &datasets {
        if let Some(ceiling) = crate::allowlist::Ceiling::load(&ds.dir)? {
            for m in &ds.mounts {
                if let Some(rec) = roost
                    .mounts
                    .iter()
                    .find(|r| r.tenant == m.tenant && r.alias == m.alias)
                {
                    rec.surface()
                        .validate_within(&ceiling, &m.route_key(multi_tenant))?;
                }
            }
        }
    }

    for ds in &datasets {
        if ds.refcount() > 1 {
            tracing::info!(
                "dataset {} is shared by {} mounts ({}) - indexed once",
                ds.nid.as_deref().unwrap_or("<un-migrated>"),
                ds.refcount(),
                ds.mounts
                    .iter()
                    .map(|m| m.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
    }
    if multi_tenant {
        tracing::info!(
            "multi-tenant: routes are /<tenant>/<nest>/… ({} tenants)",
            roost
                .mount_refs()
                .iter()
                .map(|m| m.tenant.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
    }

    // `--rpc` is ambiguous once a roost spans chains (which chain would it override?). Allow it only for
    // a single-chain roost; a multichain roost sets rpc_urls per chain under [[chains]].
    if !rpc_override.is_empty() && groups.len() > 1 {
        bail!(
            "--rpc is ambiguous for a multichain roost ({} chains) - set rpc_urls per chain under [[chains]]",
            groups.len()
        );
    }
    tracing::info!(
        "runtime '{}': mounting {} nest(s) across {} chain(s) - one isolated cursor per chain",
        meta.name,
        roost.mount_refs().len(), // NOT `meta.nests`, which is empty once the mount table is authoritative
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

    let mut all_states =
        fan_out_aliases(&datasets, all_states, &health, &mut estimates, multi_tenant);

    // Overlay each mount's SQL surface (RFC-0034 §2). Applied **per mount, not per dataset**: that is
    // the whole reason the allowlist is mount config rather than manifest. Two tenants sharing one
    // dataset can expose different surfaces over it, and neither the NID nor the data is affected.
    for (key, state) in all_states.iter_mut() {
        let Some(m) = roost.mounts.iter().find(|m| {
            &MountRef {
                tenant: m.tenant.clone(),
                alias: m.alias.clone(),
            }
            .route_key(multi_tenant)
                == key
        }) else {
            continue;
        };
        let surface = m.surface();
        if surface.access != crate::allowlist::SqlAccess::Open {
            tracing::info!(
                "mount {key}: sql = {:?}, {} declared quer{}",
                surface.access,
                surface.queries.len(),
                if surface.queries.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
        }
        state.surface = Arc::new(surface);
    }
    let all_states = all_states;

    // Roster (`GET /nests`) across every cursor's nests, with per-nest footprint attribution and the
    // roost's real resident set alongside the projection so operators can calibrate.
    let roster_entries: Vec<_> = all_states
        .iter()
        .map(|(name, state)| {
            // Which dataset backs this mount, and who else is on it (RFC-0032 §4). Without this an
            // operator seeing two entries has no way to tell one shared dataset from two backfills.
            let ds = datasets
                .iter()
                .find(|d| d.mounts.iter().any(|m| &m.route_key(multi_tenant) == name));
            let this =
                ds.and_then(|d| d.mounts.iter().find(|m| &m.route_key(multi_tenant) == name));
            let tenant = this.map(|m| m.tenant.clone());
            let shared_with: Vec<String> = ds
                .map(|d| {
                    d.mounts
                        .iter()
                        .map(|m| m.route_key(multi_tenant))
                        .filter(|k| k != name)
                        .collect()
                })
                .unwrap_or_default();
            serde_json::json!({
                "name": name,
                "chain": state.chain,
                "tenant": tenant,
                "nid": ds.and_then(|d| d.nid.clone()),
                "shared_with": shared_with,
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
            mounts: roost.mounts.clone(),
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
    let path = dir.join(MOUNTS_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} to persist the nest list", path.display()))?;
    let mut roost: Roost = toml::from_str(&raw)
        .with_context(|| format!("parsing {} before rewriting it", path.display()))?;
    // `nests` here are **route keys** - tenant-qualified when the runtime is multi-tenant - because
    // that is what the serving layer and the admin API name a mount by. Matching them against
    // `alias` alone would drop every mount in a multi-tenant roost on the first unmount.
    let multi_tenant = roost.is_multi_tenant();
    if roost.mounts.is_empty() {
        roost.roost.nests = nests.to_vec();
    } else {
        // Drop the records for mounts that are gone, or the next `load` refuses the file it just
        // wrote. **The dataset under `data/<nid>` is deliberately left on disk** - RFC-0032 §5 makes
        // collection explicit, because re-backfilling is precisely the cost this design exists to
        // avoid and an accidental unmount must not trigger one.
        roost.mounts.retain(|m| {
            let key = MountRef {
                tenant: m.tenant.clone(),
                alias: m.alias.clone(),
            }
            .route_key(multi_tenant);
            nests.contains(&key)
        });
        roost.roost.nests.clear(); // `[[mounts]]` is authoritative; a stale list beside it lies
    }
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
    /// The roost directory; a nest lives at `data/<nid>/`, or `nests/<name>/` if un-migrated.
    pub dir: PathBuf,
    /// The mount records as of startup (RFC-0032 §4), so a live mount resolves its dataset the same
    /// way the static path does. A runtime mount of a nest with no record still resolves through
    /// the pre-2.0 layout - a half-migrated roost is a supported state, not an accident.
    pub mounts: Vec<Mount>,
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
        // `name` is a route key: `<alias>` single-tenant, `<tenant>/<alias>` multi-tenant. Split it
        // so a record lookup works either way, rather than silently missing every multi-tenant mount
        // and falling through to the pre-2.0 directory - which would come up empty and re-backfill.
        let (tenant, alias) = match name.split_once('/') {
            Some((t, a)) => (Some(t), a),
            None => (None, name),
        };
        let record = self
            .mount_ctx
            .mounts
            .iter()
            .find(|m| m.alias == alias && tenant.is_none_or(|t| m.tenant == t));
        let dir = match record {
            Some(m) => Roost::data_dir(&self.mount_ctx.dir, &m.nid),
            None => Roost::nest_dir(&self.mount_ctx.dir, alias),
        };
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
            None,
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
                "the roost's nest set changed but {MOUNTS_FILE} could not be updated ({e:#}) - the \
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
            dir.join(MOUNTS_FILE),
            format!(
                "[runtime]\nname = \"test\"\nchain = \"{chain}\"\nchain_id = {chain_id}\n\
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
        let roost = Roost::load(roost_dir).unwrap();
        let p = roost.dir_for(roost_dir, name);
        let c = Config::load(&p).unwrap();
        (name.to_string(), p, c)
    }

    fn aliases(ds: &Dataset) -> Vec<String> {
        ds.mounts.iter().map(|m| m.alias.clone()).collect()
    }

    /// The unmount path rewrites `roost.toml` from **route keys**, which are tenant-qualified in a
    /// multi-tenant runtime. Matching them against `alias` alone would retain nothing and silently
    /// unmount every co-tenant on the first unmount - so this asserts the survivor, not the casualty.
    #[test]
    fn persisting_after_an_unmount_keeps_the_other_tenants_mount() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let nid = "aa11".repeat(16);
        std::fs::write(
            root.join(MOUNTS_FILE),
            format!(
                "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
                 rpc_urls = []\n\n\
                 [[mounts]]\ntenant = \"acme\"\nalias = \"usdc\"\nnid = \"{nid}\"\n\n\
                 [[mounts]]\ntenant = \"globex\"\nalias = \"usdc\"\nnid = \"{nid}\"\n"
            ),
        )
        .unwrap();

        // acme unmounts; the runtime persists what is left, by route key.
        persist_mounted_nests(root, &["globex/usdc".to_string()]).unwrap();

        let after = Roost::load(root).expect("the rewritten file must still load");
        assert_eq!(after.mounts.len(), 1, "the co-tenant's mount was dropped");
        assert_eq!(after.mounts[0].tenant, "globex");
        assert_eq!(after.mounts[0].alias, "usdc");
        assert_eq!(
            after.mounts[0].nid, nid,
            "the surviving mount must still point at the same dataset"
        );
    }

    /// RFC-0032 slice 5: the roost is retired, so a directory still holding the pre-2.0 file is a
    /// **migration that has not been run** - and must say so, rather than reporting a missing
    /// `mounts.toml` an operator never had.
    #[test]
    fn a_pre_2_0_directory_is_pointed_at_migrate() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(LEGACY_ROOST_FILE),
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
             rpc_urls = []\nnests = [\"a\"]\n",
        )
        .unwrap();
        let err = Roost::load(d.path()).unwrap_err().to_string();
        assert!(
            err.contains("nuthatch migrate"),
            "the error must name the fix: {err}"
        );
        assert!(
            err.contains("never re-indexes"),
            "and reassure about the cost: {err}"
        );
        assert!(
            err.contains("retired in 2.0"),
            "and say why the file stopped working: {err}"
        );

        // Once migrated the old file is irrelevant: `mounts.toml` is what is read.
        std::fs::write(
            d.path().join(MOUNTS_FILE),
            format!(
                "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
                 rpc_urls = []\n\n[[mounts]]\nalias = \"a\"\nnid = \"{}\"\n",
                "aa11".repeat(16)
            ),
        )
        .unwrap();
        let roost = Roost::load(d.path()).expect("a migrated directory loads");
        assert_eq!(roost.mounts.len(), 1);
    }

    /// RFC-0032 §4: grouping is by *dataset*, and a roost may be half-migrated while it happens.
    #[test]
    fn datasets_group_by_identity_across_both_layouts() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(
            root.join(MOUNTS_FILE),
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
             rpc_urls = []\nnests = [\"a\", \"b\", \"legacy\"]\n\n\
             [[mounts]]\nalias = \"a\"\nnid = \"aa11\"\n\n\
             [[mounts]]\nalias = \"b\"\nnid = \"aa11\"\n"
                .replace("aa11", &"aa11".repeat(16)),
        )
        .unwrap();
        let roost = Roost::load(root).unwrap();
        let ds = roost.datasets(root);

        // Two migrated aliases collapse to one dataset; the un-migrated nest keeps its own.
        assert_eq!(ds.len(), 2, "expected 2 datasets, got {ds:?}");
        assert_eq!(aliases(&ds[0]), vec!["a", "b"]);
        assert_eq!(ds[0].refcount(), 2);
        assert_eq!(ds[0].canonical().alias, "a", "the first mount indexes");
        assert!(ds[0].dir.starts_with(root.join(DATA_DIR)));

        assert_eq!(aliases(&ds[1]), vec!["legacy"]);
        assert_eq!(ds[1].refcount(), 1);
        assert_eq!(
            ds[1].nid, None,
            "an un-migrated nest has no identity recorded"
        );
        assert!(ds[1].dir.starts_with(root.join(NESTS_DIR)));
    }

    /// A mount record is untrusted input - `roost.toml` is operator-editable and a roster may come
    /// from a resolved bundle - so the identity and the path segments are checked at load.
    #[test]
    fn a_mount_record_is_validated() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let base = "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
                    rpc_urls = []\n\n[[mounts]]\n";
        let good = "aa11".repeat(16);

        for (record, expect) in [
            // Too short, and not hex - a NID is a path segment before it is anything else.
            ("alias = \"a\"\nnid = \"aa\"\n".to_string(), "invalid nest identity"),
            (
                format!("alias = \"a\"\nnid = \"{}\"\n", "zz".repeat(32)),
                "invalid nest identity",
            ),
            (
                format!("alias = \"../esc\"\nnid = \"{good}\"\n"),
                "nest name '../esc' is invalid",
            ),
            (
                format!("alias = \"health\"\nnid = \"{good}\"\n"),
                "reserved",
            ),
            // The primary key is (tenant, alias), so the *same* pair twice is the collision.
            (
                format!(
                    "alias = \"a\"\nnid = \"{good}\"\n\n[[mounts]]\nalias = \"a\"\nnid = \"{good}\"\n"
                ),
                "more than once",
            ),
        ] {
            std::fs::write(root.join(MOUNTS_FILE), format!("{base}{record}")).unwrap();
            let err = Roost::load(root).unwrap_err().to_string();
            assert!(err.contains(expect), "expected {expect:?}, got: {err}");
        }

        // ...and the same alias under two *different* tenants is fine, which is the whole point.
        std::fs::write(
            root.join(MOUNTS_FILE),
            format!(
                "{base}tenant = \"acme\"\nalias = \"a\"\nnid = \"{good}\"\n\n\
                 [[mounts]]\ntenant = \"globex\"\nalias = \"a\"\nnid = \"{good}\"\n"
            ),
        )
        .unwrap();
        let roost = Roost::load(root).expect("two tenants may share an alias");
        assert!(roost.is_multi_tenant());
        assert_eq!(roost.mount_refs().len(), 2);
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
            d.path().join(MOUNTS_FILE),
            "[runtime]\nname = \"multi\"\nnests = [\"a\", \"b\"]\n\n\
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
            d.path().join(MOUNTS_FILE),
            "[runtime]\nname = \"x\"\nchain = \"base\"\nchain_id = 8453\nrpc_urls = [\"u\"]\nnests = [\"a\"]\n\n\
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
                d.path().join(MOUNTS_FILE),
                format!("[runtime]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"{bad}\"]\n"),
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
            d.path().join(MOUNTS_FILE),
            "[runtime]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"nests\"]\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("reserved"));

        std::fs::write(
            d.path().join(MOUNTS_FILE),
            "[runtime]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = [\"a\", \"a\"]\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("more than once"));
    }

    #[test]
    fn rejects_a_roost_that_mounts_nothing() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join(MOUNTS_FILE),
            "[runtime]\nname = \"t\"\nchain = \"c\"\nchain_id = 1\nrpc_urls = [\"u\"]\nnests = []\n",
        )
        .unwrap();
        assert!(Roost::load(d.path())
            .unwrap_err()
            .to_string()
            .contains("mounts nothing"));
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
