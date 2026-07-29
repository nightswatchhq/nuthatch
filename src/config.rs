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
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
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
        // RFC-0018 §2: a nest may be authored as `nest.star` (Starlark) that *computes* its config.
        // When present it takes precedence and is evaluated hermetically to the same `Config` a TOML
        // file would produce; TOML remains what `init` emits and the default for everyone else.
        let star = dir.join("nest.star");
        if star.exists() {
            return crate::starlark_config::load_star(&star, dir);
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
        if cfg.nest.schema_version > CURRENT_SCHEMA_VERSION {
            bail!(
                "this nest needs config schema v{} but this nuthatch supports up to v{} - upgrade nuthatch",
                cfg.nest.schema_version,
                CURRENT_SCHEMA_VERSION
            );
        }
        Ok(cfg)
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
}
