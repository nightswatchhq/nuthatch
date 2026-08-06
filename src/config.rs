//! The one config file a nest has: `nuthatch.toml`.
//!
//! v2 (RFC-0001) is a `[nest]` header plus a `[[contracts]]` array - many contracts per nest. A
//! v1 file (single top-level `address`) is migrated transparently on load, so existing projects
//! keep working.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CONFIG_FILE: &str = "nuthatch.toml";
pub const DB_FILE: &str = "nuthatch.redb";
/// v1 default ABI filename, retained for migration of old single-contract projects.
pub const ABI_FILE: &str = "abi.json";

/// The nest-config schema this build understands. A nest declaring a higher version is rejected on
/// load (it was authored by a newer nuthatch) - the guard that makes `init --from` safe.
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

/// An absent `schema_version` means **1**, not "current".
///
/// Every nest written before the field existed is a v1 nest, and treating it as current would mean a
/// future v3 build silently accepted a file it had no reason to believe it understood. The default is
/// a statement about the file, not about this binary.
fn default_schema_version() -> u32 {
    1
}

/// The lowest config-schema version that can express this nest.
///
/// **v2 exists for exactly one reason:** `block_timestamps = false` (RFC-0029 §6b). A pre-0.9 binary
/// does not know the field, so it would parse such a nest, ignore the declaration, and index
/// timestamps into a store built without them - the two-schemas-in-one-nest failure the runtime guard
/// catches locally but an older binary cannot. Stamping v2 makes that binary refuse the nest instead.
///
/// A nest that indexes timestamps is stamped **v1 even though this build is v2**, because it is
/// genuinely a v1 file: nothing in it needs a v2 reader, and gratuitously raising the floor would stop
/// 0.8.x opening nests it can serve perfectly well. The version records what a *reader* must
/// understand, not which binary happened to write it.
pub fn required_schema_version(block_timestamps: bool) -> u32 {
    if block_timestamps {
        1
    } else {
        2
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub nest: Nest,
    #[serde(default)]
    pub contracts: Vec<Contract>,
    /// Optional sanctions-screening stage (RFC-0008 C2). When present with a non-empty `lists`, the
    /// indexer screens every transfer against those list snapshots live and records `sanction_hit`
    /// annotations. Absent → no screening, zero cost. Not serialised when empty (keeps nests clean).
    #[serde(default, skip_serializing_if = "Screening::is_empty")]
    pub screening: Screening,
    /// Optional threshold & velocity flags (RFC-0008 C3). Absent → no flags, zero cost.
    #[serde(default, skip_serializing_if = "Flags::is_empty")]
    pub flags: Flags,
    /// Optional alert webhook sinks (RFC-0008 C5). Each routes annotations of the named kinds to a
    /// URL. Absent → no alerts. Delivery is at-least-once via a durable outbox; a stalled sink never
    /// blocks indexing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<Alert>,
    /// Optional child-contract templates (RFC-0009). A template is an ABI applied to contracts
    /// discovered at runtime by a [`Factory`], rather than a fixed address. Absent → no factories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub templates: Vec<Template>,
    /// Optional factory rules (RFC-0009): a watched contract's event announces a child contract to
    /// index with a template. Absent → static nest (no dynamic discovery).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factories: Vec<Factory>,
    /// Optional user webhooks (RFC-0010 Part B): POST rows of an event table matching a predicate to
    /// a URL as they seal. Feeds the same host-side delivery engine as the compliance alerts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub webhooks: Vec<Webhook>,
    /// Optional firehose-class extraction (RFC-0014): call traces and storage diffs beside event
    /// decode. Absent → events only, which is every nest today. **Declaring this does not make it
    /// run**: the only extraction source for traces/state is a colocated node (RFC-0003), so a nest
    /// that asks for it is refused at startup rather than served silently-empty tables. The config,
    /// decode and schema exist ahead of the source deliberately - see [`Extract`].
    #[serde(default, skip_serializing_if = "Extract::is_empty")]
    pub extract: Extract,
    /// Optional irreducible `eth_call` reads (RFC-0023 tier 3). Each is resolved at **pinned
    /// historical blocks** and content-addressed by `(chain, block, contract, calldata)`.
    ///
    /// Reach for this **last**. Tier 1's recipes derive most contract state from events already
    /// indexed - no RPC, no archive node, deterministic and free - and tier 2 caches immutable
    /// metadata. A `[[calls]]` entry is for what genuinely cannot be derived: an oracle read, an
    /// ungoverned parameter, a view on a contract whose events the nest does not fully cover.
    ///
    /// Declaring any of these means the nest **needs an archive RPC** for the range it backfills.
    /// That is the one external dependency tier 3 introduces, and it is why the derive-first tiers
    /// exist: they keep the zero-dependency default intact for everyone who does not need this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<crate::calls::CallDecl>,
}

/// A user webhook subscription (RFC-0010 Part B): rows of `table` matching `where` are POSTed to `url`.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Webhook {
    pub name: String,
    /// The event table to watch, e.g. `staking__stake_delegated`.
    pub table: String,
    /// Optional SQL predicate over the table's columns (operator-authored, trusted like nest views).
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub where_clause: Option<String>,
    pub url: String,
    /// Max rows per delivery POST (default [`crate::webhooks::DEFAULT_BATCH_MAX`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_max: Option<usize>,
    /// `"sealed"` (default - never lies, finality-gated) or `"tip"` (fast, may send retractions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finality: Option<String>,
    /// Where the cursor starts on first registration: `"registration"` (default - only rows sealed
    /// after the webhook is added, so a `--seal-direct` backfill doesn't fire history), `"genesis"`,
    /// or a block number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Optional HMAC-SHA256 secret. When set, each delivery carries an `X-Nuthatch-Signature:
    /// sha256=<hex>` header over the POST body, so the receiver can verify it came from this nest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// One alert webhook sink: annotations whose kind is in `kinds` are POSTed to `url` (RFC-0008 C5).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Alert {
    /// Annotation kinds to deliver, e.g. `["sanction_hit", "threshold_flag"]`.
    pub kinds: Vec<String>,
    /// The webhook endpoint. The operator configures it - it is the delivery allowlist (a sink only
    /// ever POSTs to the URLs a nest declares here).
    pub url: String,
}

/// A child-contract template (RFC-0009): a name + a vendored ABI, applied to every contract a
/// factory discovers. All children of one template share tables (`{template}__{event}`),
/// distinguished by the implicit `address` column.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Template {
    pub name: String,
    /// ABI path relative to the nest dir, e.g. "abis/uniswap_v3_pool.json".
    pub abi: String,
    /// Backfill filter strategy override (RFC-0009 §4): `"topic0"` forces the topic0-only fetch (with
    /// local registry-lookup filtering) instead of the address list - useful when a template is known
    /// to have many children. Omit for the automatic address-list → topic0 flip above ~500 children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Event names to decode for every child of this template, matching `[[contracts]].events`.
    /// Empty (the default) decodes every event the template ABI defines.
    ///
    /// Distinct from `filter`, which chooses *how the range is fetched*; this chooses *what is
    /// decoded from it*. Without it the vendored ABI is the only filter, so a full `UniswapV2Pair`
    /// ABI decodes Swap, Sync, Mint, Burn, Transfer and Approval when the nest wanted Swap - not
    /// wrong, but a different workload, and nothing said so. A name here the ABI does not define is
    /// a config error, caught at registry build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

/// A factory rule (RFC-0009): when `watch`'s `event` fires, the child address in `child_param` is
/// indexed under `template`. `watch` is a `[[contracts]]` alias or another template (nested).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Factory {
    /// The alias of the watched contract (or a template, for nested factories).
    pub watch: String,
    /// The announcing event name, e.g. "PoolCreated".
    pub event: String,
    /// The event parameter holding the child contract address, e.g. "pool".
    pub child_param: String,
    /// Which [`Template`] to apply to the discovered child.
    pub template: String,
    /// Optional: only honour discoveries at or after this block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
}

/// One alert webhook sink: annotations whose kind is in `kinds` are POSTed to `url` (RFC-0008 C5).
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Screening {
    /// Content-addressed list-snapshot hashes to screen against (see `nuthatch lists fetch`).
    #[serde(default)]
    pub lists: Vec<String>,
}

impl Screening {
    fn is_empty(&self) -> bool {
        self.lists.is_empty()
    }
}

/// Threshold & velocity flag configuration (RFC-0008 C3). Amounts are token **base units** as decimal
/// strings (i128 - no currency conversion in-core, per the RFC). Both flavours are opt-in.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Flags {
    /// Flag any single transfer whose value ≥ this many base units (travel-rule style).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<String>,
    /// Flag an address whose outbound volume within a block-window reaches this many base units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_amount: Option<String>,
    /// The velocity block-window size. Blocks, not wall-clock: an honest approximation of "~24h"
    /// (≈ 7200 blocks on 12s-block mainnet). Defaults to [`DEFAULT_VELOCITY_WINDOW`] when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub velocity_window: Option<u64>,
}

/// Firehose-class extraction config (RFC-0014). Both surfaces are **opt-in per nest and default
/// off**, because they are the only rows in nuthatch whose volume is unbounded by the nest's own
/// configuration: events are bounded by "how often does this contract emit", whereas `state = true`
/// on a busy contract yields a row per `SSTORE` and `traces = true` a row per internal call.
///
/// That is why [`Extract::scope_check`] refuses an unscoped nest instead of warning about it. The
/// house rule is RFC-0012's: a budget stops being a budget the moment something may quietly exceed
/// it. `unbounded = true` is the deliberate opt-out, and has to be typed by a human.
#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct Extract {
    /// Emit a row per **call** (top-level and internal), calldata decoded by 4-byte selector.
    #[serde(default)]
    pub traces: bool,
    /// Emit a row per **storage write**: `(address, slot, prev, new)`, raw slots, no ABI needed.
    #[serde(default)]
    pub state: bool,
    /// Restrict extraction to these contract aliases. Empty means *every address on the chain*,
    /// which is the unbounded case - not merely "every contract in this nest".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<String>,
    /// Restrict trace extraction to these 4-byte selectors (`0x` + 8 hex). Empty means every
    /// function. Ignored when `traces = false`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selectors: Vec<String>,
    /// Accept an unscoped extraction nest anyway. Requires a human to have typed it, and is
    /// reported in the startup log so it is never a silent state.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unbounded: bool,
}

impl Extract {
    pub fn is_empty(&self) -> bool {
        !self.traces
            && !self.state
            && self.contracts.is_empty()
            && self.selectors.is_empty()
            && !self.unbounded
    }

    /// Is any extraction surface actually switched on?
    pub fn enabled(&self) -> bool {
        self.traces || self.state
    }

    /// Scoped means "bounded by something the operator named". A selector allowlist bounds traces
    /// but says nothing about state diffs, so it only counts when `state` is off - otherwise a nest
    /// could look scoped while its storage half was still chain-wide.
    pub fn is_scoped(&self) -> bool {
        if !self.contracts.is_empty() {
            return true;
        }
        !self.state && self.traces && !self.selectors.is_empty()
    }

    /// The volume guard (RFC-0014 §3). `Ok(())` to proceed, `Err` with a message that says what to
    /// do about it. Called before any extraction work is scheduled.
    pub fn scope_check(&self) -> Result<()> {
        if !self.enabled() || self.is_scoped() || self.unbounded {
            return Ok(());
        }
        let which = match (self.traces, self.state) {
            (true, true) => "traces and state",
            (true, false) => "traces",
            _ => "state",
        };
        bail!(
            "[extract] {which} = true with no `contracts` scope is unbounded by construction: it \
             extracts every address on the chain, not just this nest's, so its row count is a \
             property of chain traffic rather than of your config. Add `contracts = [\"alias\", …]` \
             to scope it{}, or set `unbounded = true` to accept a nest whose footprint nobody can \
             predict.",
            if self.traces && !self.state {
                " (or `selectors = [\"0x…\"]` for traces alone)"
            } else {
                ""
            }
        )
    }

    /// Normalised selector allowlist as raw 4-byte keys, rejecting anything malformed rather than
    /// silently ignoring it - a typo'd selector that filtered nothing would look like it worked.
    pub fn selector_keys(&self) -> Result<Vec<[u8; 4]>> {
        self.selectors
            .iter()
            .map(|s| {
                let hex_part = s.strip_prefix("0x").unwrap_or(s);
                let raw = hex::decode(hex_part)
                    .map_err(|e| anyhow!("[extract] selector `{s}` is not hex: {e}"))?;
                let four: [u8; 4] = raw.as_slice().try_into().map_err(|_| {
                    anyhow!(
                        "[extract] selector `{s}` is {} bytes; a function selector is exactly 4",
                        raw.len()
                    )
                })?;
                Ok(four)
            })
            .collect()
    }
}

/// Default velocity window (~24h of 12s mainnet blocks). Documented as a block-count approximation.
pub const DEFAULT_VELOCITY_WINDOW: u64 = 7_200;

impl Flags {
    fn is_empty(&self) -> bool {
        self.threshold.is_none() && self.velocity_amount.is_none() && self.velocity_window.is_none()
    }

    /// The single-transfer threshold in base units, if configured and parseable.
    pub fn threshold_amount(&self) -> Option<i128> {
        self.threshold.as_deref().and_then(|s| s.parse().ok())
    }

    /// The velocity `(amount, window)` in `(base units, blocks)`, if an amount is configured.
    pub fn velocity(&self) -> Option<(i128, u64)> {
        let amount = self
            .velocity_amount
            .as_deref()
            .and_then(|s| s.parse::<i128>().ok())?;
        let window = self
            .velocity_window
            .unwrap_or(DEFAULT_VELOCITY_WINDOW)
            .max(1);
        Some((amount, window))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Nest {
    pub name: String,
    pub chain: String,
    pub chain_id: u64,
    pub rpc_urls: Vec<String>,
    /// Config schema version (see `CURRENT_SCHEMA_VERSION`). Absent in older nests → treated as 1.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Whether every row carries the implicit `block_timestamp` column (RFC-0029 §6b).
    ///
    /// **This is an `init`-time declaration, not a tuning flag**, and the default is `true` because
    /// that is what every nest before it produced. Turning it off removes a column from *every* table,
    /// which RFC-0020's classifier calls `ColumnRemoved` - breaking - and it changes the bytes of every
    /// sealed segment, so a switched nest cannot reuse its own content-addressed segments. Changing it
    /// on a nest that has already indexed is refused at startup (see `TIMESTAMPS_KEY` in `indexer.rs`)
    /// and must go through the ordinary breaking-update path: a new nest, served alongside the old one
    /// for its existing consumers.
    ///
    /// What it buys is real - timestamp acquisition measured at ~85% of backfill wall clock - but it is
    /// a *new-nest* win, which is a narrower claim than "backfills get faster".
    #[serde(default = "default_block_timestamps")]
    pub block_timestamps: bool,
}

fn default_block_timestamps() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Contract {
    pub alias: String,
    pub address: String,
    /// Deployment block (auto-detected at init); None → backfill from a tip offset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_block: Option<u64>,
    /// ABI path relative to the nest dir, e.g. "abis/usdc.json".
    pub abi: String,
    /// Optional per-contract event allowlist (RFC-0011): only these events (by ABI name) are decoded
    /// and stored for this contract. Empty (the default) indexes every event the ABI defines. This is
    /// how a nest indexing e.g. GraphToken keeps only `Transfer` instead of millions of irrelevant
    /// rows. A name here that the ABI doesn't define is a config error, caught at registry build.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<String>,
}

impl Config {
    pub fn load(dir: &Path) -> Result<Config> {
        // RFC-0018 §2's Starlark front-end was retired on 2026-07-21 (one graph nest, plain TOML) and
        // the loader is removed in 2.0. A directory still carrying `nest.star` is told so rather than
        // having it silently ignored - the file used to *take precedence*, so quietly skipping it
        // would change what a nest indexes without saying anything.
        if dir.join("nest.star").exists() {
            bail!(
                "{} has a nest.star, and the Starlark front-end was removed in 2.0 (RFC-0018 §2, \
                 retired 2026-07-21). Port it to nuthatch.toml - `nuthatch init` emits that shape, \
                 and it is what every other nest already uses.",
                dir.display()
            );
        }
        let path = dir.join(CONFIG_FILE);
        let raw = std::fs::read_to_string(&path).with_context(|| {
            format!(
                "no {CONFIG_FILE} in {} - run `nuthatch init` first",
                dir.display()
            )
        })?;
        // v2 first; fall back to migrating a v1 file.
        let cfg = match toml::from_str::<Config>(&raw) {
            Ok(cfg) => cfg,
            Err(v2_err) => Self::from_v1(&raw).map_err(|v1_err| {
                anyhow!("nuthatch.toml is neither v2 ({v2_err}) nor v1 ({v1_err})")
            })?,
        };
        cfg.check_schema_version()?;
        // Declarations are validated at load, not at the first round trip: a typo'd address would
        // otherwise surface thousands of blocks into a backfill as a wall of identical failures.
        for c in &cfg.calls {
            c.validate()?;
        }
        cfg.refuse_unwired_calls()?;
        Ok(cfg)
    }

    /// Refuse a `[[calls]]` block while RFC-0023 tier 3 has no executor (issue #262).
    ///
    /// Everything around this is built - `CallKey`, `CallDecl::validate`, `resolve_at`, and the
    /// content addressing - and nothing calls it. So a declared call was **accepted, validated, and
    /// then ignored forever**: no rows, no warning, no error. That is the same shape as the writer
    /// pool that took leases and never indexed (#250), and it is the failure mode this project can
    /// least afford, because the config looks like it worked.
    ///
    /// Refusing costs an operator one clear message. Accepting costs them a silent wrong answer, and
    /// they find out from a consumer. Delete this the moment an executor exists - the test named
    /// after it will fail loudly and tell you to.
    fn refuse_unwired_calls(&self) -> Result<()> {
        if self.calls.is_empty() {
            return Ok(());
        }
        bail!(
            "this nest declares {} `[[calls]]` entr{}, and nuthatch cannot run {} yet.\n\n\
             RFC-0023 tier 3 (pinned-block `eth_call`) has the decoding, validation and content \
             addressing in place but no executor, so a declared call would be accepted and then \
             silently produce nothing - see issue #262.\n\n\
             Remove the `[[calls]]` block. Most contract state is *derivable* from events you already \
             index, which is faster, free, and needs no archive node: try `nuthatch recipe add \
             total_supply` (also `balances`, `holder_count`, `reserves`), or `nuthatch metadata fetch` \
             for immutable `decimals`/`symbol`/`name`.",
            self.calls.len(),
            if self.calls.len() == 1 { "y" } else { "ies" },
            if self.calls.len() == 1 { "it" } else { "them" },
        )
    }

    /// Both gates on `schema_version`, shared by the TOML and Starlark load paths.
    fn check_schema_version(&self) -> Result<()> {
        // Too new for us: the nest was authored by a later nuthatch and may mean things by fields we
        // do not know. This is the guard that makes `init --from` safe.
        if self.nest.schema_version > CURRENT_SCHEMA_VERSION {
            bail!(
                "this nest needs config schema v{} but this nuthatch supports up to v{} - upgrade nuthatch",
                self.nest.schema_version,
                CURRENT_SCHEMA_VERSION
            );
        }
        // Too *old* for what it declares - the mirror case, and the dangerous one. A file claiming v1
        // while declaring a v2-only feature is exactly the file an older binary would accept and get
        // wrong. We cannot fix that binary; we can refuse to be the source of such a file, and anyone
        // hand-editing `block_timestamps` finds out here rather than from a nest that quietly
        // disagrees with itself on someone else's machine.
        let need = required_schema_version(self.nest.block_timestamps);
        if self.nest.schema_version < need {
            bail!(
                "this nest declares `block_timestamps = {}`, which needs `schema_version = {}` \
                 (it says {}). Older nuthatch builds don't know the field and would index timestamps \
                 into a nest built without them; the version is what makes them refuse instead.",
                self.nest.block_timestamps,
                need,
                self.nest.schema_version
            );
        }
        Ok(())
    }

    fn from_v1(raw: &str) -> Result<Config> {
        #[derive(Deserialize)]
        struct V1 {
            chain: String,
            chain_id: u64,
            address: String,
            rpc_urls: Vec<String>,
        }
        let v1: V1 = toml::from_str(raw)?;
        Ok(Config {
            nest: Nest {
                name: "nest".to_string(),
                chain: v1.chain,
                chain_id: v1.chain_id,
                rpc_urls: v1.rpc_urls,
                schema_version: CURRENT_SCHEMA_VERSION,
                block_timestamps: true,
            },
            contracts: vec![Contract {
                alias: "c0".to_string(),
                address: v1.address,
                start_block: None,
                abi: ABI_FILE.to_string(),
                events: Vec::new(),
            }],
            screening: Screening::default(),
            flags: Flags::default(),
            alerts: Vec::new(),
            templates: Vec::new(),
            factories: Vec::new(),
            webhooks: Vec::new(),
            extract: Extract::default(),
            calls: Vec::new(),
        })
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = dir.join(CONFIG_FILE);
        let raw = toml::to_string_pretty(self).context("failed to serialise config")?;
        std::fs::write(&path, raw)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// The first contract - the indexer's single-contract path uses this until step 3 generalises
    /// decode + storage to every contract in the nest.
    pub fn primary(&self) -> Result<&Contract> {
        self.contracts
            .first()
            .ok_or_else(|| anyhow!("nest has no contracts"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_a_v1_file() {
        let v1 = r#"
            chain = "mainnet"
            chain_id = 1
            address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
            rpc_urls = ["https://rpc.example"]
            event = "Transfer"
        "#;
        let cfg = Config::from_v1(v1).unwrap();
        assert_eq!(cfg.nest.chain, "mainnet");
        assert_eq!(cfg.nest.chain_id, 1);
        assert_eq!(cfg.contracts.len(), 1);
        assert_eq!(cfg.contracts[0].alias, "c0");
        assert_eq!(cfg.contracts[0].abi, ABI_FILE);
        assert!(cfg.contracts[0].start_block.is_none());
    }

    #[test]
    fn roundtrips_a_v2_file() {
        let cfg = Config {
            nest: Nest {
                name: "my-nest".into(),
                chain: "mainnet".into(),
                chain_id: 1,
                rpc_urls: vec!["https://rpc.example".into()],
                schema_version: CURRENT_SCHEMA_VERSION,
                block_timestamps: true,
            },
            contracts: vec![
                Contract {
                    alias: "usdc".into(),
                    address: "0xaaaa".into(),
                    start_block: Some(6_082_465),
                    abi: "abis/usdc.json".into(),
                    events: Vec::new(),
                },
                Contract {
                    alias: "weth".into(),
                    address: "0xbbbb".into(),
                    start_block: None,
                    abi: "abis/weth.json".into(),
                    events: vec!["Transfer".into()],
                },
            ],
            screening: Screening::default(),
            flags: Flags::default(),
            alerts: Vec::new(),
            templates: Vec::new(),
            factories: Vec::new(),
            webhooks: Vec::new(),
            extract: Extract::default(),
            calls: Vec::new(),
        };
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.contracts.len(), 2);
        assert_eq!(back.contracts[0].start_block, Some(6_082_465));
        assert_eq!(back.contracts[1].start_block, None);
        // The per-contract event allowlist (RFC-0011) round-trips; an empty one stays empty.
        assert!(back.contracts[0].events.is_empty());
        assert_eq!(back.contracts[1].events, vec!["Transfer".to_string()]);
        assert_eq!(back.primary().unwrap().alias, "usdc");
    }

    // ---- RFC-0014 `[extract]` and the volume guard ----------------------------------------------

    fn extract_of(toml_src: &str) -> Extract {
        let cfg: Config = toml::from_str(toml_src).expect("parses");
        cfg.extract
    }

    const BASE: &str = r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]
"#;

    #[test]
    fn a_nest_without_extract_has_none_and_serialises_none() {
        let cfg: Config = toml::from_str(BASE).unwrap();
        assert!(cfg.extract.is_empty());
        assert!(!cfg.extract.enabled());
        let raw = toml::to_string_pretty(&cfg).unwrap();
        assert!(
            !raw.contains("[extract]"),
            "an unused section must not appear in a nest file: {raw}"
        );
    }

    /// The guard's whole point. An unscoped extraction nest is unbounded by *chain traffic*, not by
    /// anything the operator wrote, so it is refused rather than warned about.
    #[test]
    fn unscoped_extraction_is_refused() {
        for src in [
            "[extract]\ntraces = true\n",
            "[extract]\nstate = true\n",
            "[extract]\ntraces = true\nstate = true\n",
        ] {
            let e = extract_of(&format!("{BASE}{src}"));
            let err = match e.scope_check() {
                Err(err) => err,
                Ok(()) => panic!("{src} must not be accepted unscoped"),
            };
            assert!(
                err.to_string().contains("unbounded by construction"),
                "the refusal must say why: {err}"
            );
        }
    }

    #[test]
    fn a_contract_scope_satisfies_the_guard() {
        let e = extract_of(&format!(
            "{BASE}[extract]\ntraces = true\nstate = true\ncontracts = [\"usdc\"]\n"
        ));
        assert!(e.is_scoped());
        assert!(e.scope_check().is_ok());
    }

    /// A selector allowlist bounds *traces*. It says nothing about storage writes, so it must not be
    /// accepted as scoping for a nest whose state half is still chain-wide.
    #[test]
    fn a_selector_allowlist_scopes_traces_but_not_state() {
        let traces_only = extract_of(&format!(
            "{BASE}[extract]\ntraces = true\nselectors = [\"0xa9059cbb\"]\n"
        ));
        assert!(traces_only.scope_check().is_ok());

        let with_state = extract_of(&format!(
            "{BASE}[extract]\ntraces = true\nstate = true\nselectors = [\"0xa9059cbb\"]\n"
        ));
        assert!(
            with_state.scope_check().is_err(),
            "selectors cannot bound storage writes, so they must not appear to"
        );
    }

    #[test]
    fn unbounded_is_the_deliberate_escape_hatch() {
        let e = extract_of(&format!(
            "{BASE}[extract]\ntraces = true\nunbounded = true\n"
        ));
        assert!(e.scope_check().is_ok());
    }

    /// A guard that only fires when extraction is on: an `[extract]` section with both surfaces off
    /// is inert config, not a refusal.
    #[test]
    fn the_guard_is_silent_when_extraction_is_off() {
        let e = extract_of(&format!("{BASE}[extract]\ncontracts = [\"usdc\"]\n"));
        assert!(!e.enabled());
        assert!(e.scope_check().is_ok());
    }

    #[test]
    fn selectors_are_validated_rather_than_silently_ignored() {
        let good = extract_of(&format!(
            "{BASE}[extract]\ntraces = true\nselectors = [\"0xa9059cbb\", \"095ea7b3\"]\n"
        ));
        assert_eq!(
            good.selector_keys().unwrap(),
            vec![[0xa9, 0x05, 0x9c, 0xbb], [0x09, 0x5e, 0xa7, 0xb3]],
            "the 0x prefix is optional, as it is everywhere else in the config"
        );

        // A typo'd selector that filtered nothing would look like it worked - the worst kind of bug.
        let short = extract_of(&format!("{BASE}[extract]\nselectors = [\"0xa905\"]\n"));
        assert!(short.selector_keys().unwrap_err().to_string().contains("4"));
        let nonhex = extract_of(&format!("{BASE}[extract]\nselectors = [\"0xzzzzzzzz\"]\n"));
        assert!(nonhex
            .selector_keys()
            .unwrap_err()
            .to_string()
            .contains("not hex"));
    }

    #[test]
    fn extract_survives_a_round_trip() {
        let src = format!(
            "{BASE}[extract]\ntraces = true\nstate = true\ncontracts = [\"usdc\"]\nselectors = [\"0xa9059cbb\"]\n"
        );
        let cfg: Config = toml::from_str(&src).unwrap();
        let back: Config = toml::from_str(&toml::to_string_pretty(&cfg).unwrap()).unwrap();
        assert!(back.extract.traces && back.extract.state);
        assert_eq!(back.extract.contracts, vec!["usdc".to_string()]);
        assert_eq!(back.extract.selectors, vec!["0xa9059cbb".to_string()]);
    }

    /// A timestamped nest is stamped **v1**, not "whatever version this build is".
    ///
    /// Getting this wrong is invisible until someone tries to open a freshly-scaffolded nest with the
    /// previous release and is told to upgrade for no reason. The version describes the file's
    /// requirements; only `block_timestamps = false` actually needs a v2 reader.
    #[test]
    fn only_a_timestamp_free_nest_raises_the_schema_version() {
        assert_eq!(required_schema_version(true), 1);
        assert_eq!(required_schema_version(false), 2);
        assert!(
            required_schema_version(false) <= CURRENT_SCHEMA_VERSION,
            "this build must be able to read what it writes"
        );
    }

    /// A file claiming v1 while declaring a v2-only feature is refused - it is precisely the file an
    /// older binary would accept and get wrong.
    /// A declared call must be **refused**, not accepted and ignored (issue #262).
    #[test]
    fn a_declared_call_is_refused_rather_than_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]
schema_version = 1

[[contracts]]
alias = "c"
address = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
abi = "abis/c.json"

[[calls]]
name = "price"
contract = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
calldata = "0x1234abcd"
every = 100
"#,
        )
        .unwrap();
        let err = format!("{:#}", Config::load(dir.path()).unwrap_err());
        assert!(err.contains("#262"), "point at the issue: {err}");
        assert!(
            err.contains("recipe add") || err.contains("metadata fetch"),
            "a refusal should name the thing to do instead: {err}"
        );
    }

    /// **Delete `refuse_unwired_calls` when this fails.**
    ///
    /// The refusal above exists only because nothing executes a declared call. The day someone wires
    /// `resolve_at` into the indexer, that refusal silently becomes a bug of its own - a working
    /// feature rejected at config load - and nothing else would catch it, because every test of the
    /// refusal would still pass. So this watches for the executor rather than the refusal.
    #[test]
    fn the_calls_refusal_must_go_when_an_executor_appears() {
        let mut callers = Vec::new();
        for entry in std::fs::read_dir("src").expect("src/ is readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|f| f != "calls.rs")
            {
                // Built at runtime so this file never contains the string it searches for. Written
                // as a literal, the test matched *itself* and reported `config.rs` as the executor -
                // twice, once per spelling. A scanner that scans its own source has to say the thing
                // it is looking for without ever spelling it.
                let needle = format!("{}::{}(", "calls", "resolve_at");
                let src = std::fs::read_to_string(&path).unwrap_or_default();
                if src.contains(&needle) {
                    callers.push(path.display().to_string());
                }
            }
        }
        assert!(
            callers.is_empty(),
            "the tier-3 executor now has a caller ({}), so tier 3 is wired - delete \
             `Config::refuse_unwired_calls` and this test, and close issue #262",
            callers.join(", ")
        );
    }

    #[test]
    fn a_v1_file_cannot_declare_block_timestamps_false() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]
schema_version = 1
block_timestamps = false
"#,
        )
        .unwrap();
        let err = Config::load(dir.path()).unwrap_err().to_string();
        assert!(
            err.contains("schema_version = 2"),
            "must say which version it needs: {err}"
        );

        // The same file at v2 loads.
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]
schema_version = 2
block_timestamps = false
"#,
        )
        .unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert!(!cfg.nest.block_timestamps);
    }

    /// A pre-slice-4 nest - no `schema_version`, no `block_timestamps` - still loads as v1 with
    /// timestamps on. This is every nest in existence today, so it had better keep working.
    #[test]
    fn a_nest_written_before_any_of_this_still_loads_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]
"#,
        )
        .unwrap();
        let cfg = Config::load(dir.path()).unwrap();
        assert_eq!(cfg.nest.schema_version, 1);
        assert!(cfg.nest.block_timestamps);
    }

    /// RFC-0035 §2: the Starlark front-end is removed in 2.0, and a `nest.star` is **refused** rather
    /// than ignored.
    ///
    /// Silently skipping it would be the dangerous option: `nest.star` used to take *precedence* over
    /// `nuthatch.toml`, so a nest carrying both would quietly start indexing something different from
    /// what it indexed yesterday, with no error to explain why.
    #[test]
    fn a_nest_star_is_refused_rather_than_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("nest.star"), "# computed config\n").unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE),
            "[nest]\nname = \"t\"\nchain = \"mainnet\"\nchain_id = 1\nrpc_urls = []\n",
        )
        .unwrap();

        let err = Config::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("removed in 2.0"), "{err}");
        assert!(
            err.contains("nuthatch.toml"),
            "the refusal must name the way forward: {err}"
        );

        // Without the `.star`, the same directory loads normally.
        std::fs::remove_file(dir.path().join("nest.star")).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap().nest.name, "t");
    }
}
