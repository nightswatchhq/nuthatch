//! Scaffold a nest from a Graph Protocol subgraph manifest (#241 item 5).
//!
//! A subgraph manifest already states almost everything a nest needs: which
//! contracts, on which chain, from which block, and — crucially — which ABIs,
//! pinned by CID. For a codebase of beacon proxies those pinned ABIs are
//! strictly better than Sourcify, which returns the proxy ABI carrying none of
//! the events the contract actually emits. The manifest is the only good
//! source, and it is already content-addressed.
//!
//! What the manifest **cannot** tell us is which template a factory creates:
//! that lives in the mapping WASM, as a `Template.create(address)` call. So
//! this module infers what it defensibly can and reports the rest by name,
//! with candidates, rather than guessing. A 70%-correct scaffold plus an
//! honest account of the other 30% is the deliverable — a scaffold that
//! quietly invents factory rules would be worse than none.
//!
//! The parse is deliberately tolerant. Manifests are machine-written by
//! `graph build`, but they are also hand-edited, and they vary across
//! `specVersion`s. A missing field becomes a note, not an error; the only
//! fatal conditions are "this isn't YAML" and "this has no dataSources".

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use yaml_rust2::{Yaml, YamlLoader};

/// Public IPFS gateways tried in order. The Graph's own gateway first: it is
/// the authoritative host for published manifests and is fastest for them.
pub const DEFAULT_IPFS_GATEWAYS: &[&str] = &[
    "https://ipfs.thegraph.com/api/v0/cat?arg=",
    "https://ipfs.io/ipfs/",
    "https://cloudflare-ipfs.com/ipfs/",
];

/// One ABI referenced by a manifest, pinned by CID.
#[derive(Debug, Clone, PartialEq)]
pub struct AbiRef {
    pub name: String,
    pub cid: String,
}

/// A `dataSources[]` or `templates[]` entry, reduced to what a nest needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ManifestSource {
    /// `ethereum` / `ethereum/contract` for indexable sources; `file/ipfs` and
    /// friends are carried through so we can report why they were skipped.
    pub kind: String,
    pub name: String,
    pub network: Option<String>,
    pub address: Option<String>,
    pub start_block: Option<u64>,
    /// Which entry of `mapping.abis` is *this* source's own ABI.
    pub abi_name: Option<String>,
    pub abis: Vec<AbiRef>,
    /// Event signatures as written in the manifest, e.g.
    /// `Transfer(indexed address,indexed address,uint256)`.
    pub events: Vec<String>,
}

impl ManifestSource {
    /// True for sources that index EVM logs. `file/ipfs` dataSources index the
    /// *content* behind a CID, which nuthatch does not do — it indexes the
    /// metadata hash as a column value and stops there.
    pub fn is_evm(&self) -> bool {
        self.kind.is_empty() || self.kind.starts_with("ethereum")
    }

    /// The CID of this source's own ABI, resolved through `mapping.abis`.
    pub fn own_abi(&self) -> Option<&AbiRef> {
        match &self.abi_name {
            Some(want) => self
                .abis
                .iter()
                .find(|a| &a.name == want)
                .or_else(|| self.abis.first()),
            // No `source.abi` (common on templates from older specVersions):
            // fall back to the entry named after the source, then the first.
            None => self
                .abis
                .iter()
                .find(|a| a.name == self.name)
                .or_else(|| self.abis.first()),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Manifest {
    pub spec_version: Option<String>,
    pub description: Option<String>,
    pub data_sources: Vec<ManifestSource>,
    pub templates: Vec<ManifestSource>,
}

impl Manifest {
    /// The network every EVM source agrees on. A manifest may in principle mix
    /// networks; a nest is single-chain, so disagreement is a hard error rather
    /// than a silent pick — indexing against the wrong chain corrupts state.
    pub fn network(&self) -> Result<String> {
        let mut seen: Vec<&str> = self
            .data_sources
            .iter()
            .filter(|d| d.is_evm())
            .filter_map(|d| d.network.as_deref())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        match seen.as_slice() {
            [] => bail!("manifest declares no network on any EVM dataSource"),
            [one] => Ok((*one).to_string()),
            many => bail!(
                "manifest spans {} networks ({}) - a nest indexes exactly one chain, so import \
                 the sources for a single network or split the subgraph",
                many.len(),
                many.join(", ")
            ),
        }
    }
}

// ── Fetching ─────────────────────────────────────────────────────────────

/// Accepts a bare CID, `ipfs://<cid>`, `/ipfs/<cid>`, or a full http(s) URL.
/// Returns the list of URLs to try in order.
pub fn candidate_urls(source: &str, gateways: &[String]) -> Result<Vec<String>> {
    let s = source.trim();
    if s.is_empty() {
        bail!("empty subgraph source");
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return Ok(vec![s.to_string()]);
    }
    let cid = s
        .strip_prefix("ipfs://")
        .or_else(|| s.strip_prefix("/ipfs/"))
        .unwrap_or(s)
        .trim_matches('/');
    if cid.is_empty() || cid.contains('/') || cid.contains(' ') {
        bail!("'{source}' is not a CID or URL - expected e.g. QmVPhL… or ipfs://QmVPhL…");
    }
    Ok(gateways.iter().map(|g| format!("{g}{cid}")).collect())
}

/// Fetch a document, trying each gateway until one answers. Gateways fail
/// often and individually, so a single failure is never fatal; only running
/// out of them is, and then we say which ones we tried.
pub async fn fetch_ipfs(source: &str, gateways: &[String]) -> Result<String> {
    let urls = candidate_urls(source, gateways)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let mut failures = Vec::new();
    for url in &urls {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(body) if !body.trim().is_empty() => return Ok(body),
                Ok(_) => failures.push(format!("{url}: empty body")),
                Err(e) => failures.push(format!("{url}: {e}")),
            },
            Ok(resp) => failures.push(format!("{url}: HTTP {}", resp.status())),
            Err(e) => failures.push(format!("{url}: {e}")),
        }
    }
    Err(anyhow!(
        "could not fetch '{source}' from any gateway:\n  {}",
        failures.join("\n  ")
    ))
}

// ── Parsing ──────────────────────────────────────────────────────────────

fn as_str(y: &Yaml) -> Option<String> {
    match y {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        _ => None,
    }
}

fn get<'a>(y: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    y.as_hash()
        .and_then(|h| h.get(&Yaml::String(key.to_string())))
}

fn get_str(y: &Yaml, key: &str) -> Option<String> {
    get(y, key).and_then(as_str)
}

/// A manifest link is `{ "/": "/ipfs/Qm…" }`. Return the bare CID.
fn link_cid(y: &Yaml) -> Option<String> {
    let raw = get(y, "/").and_then(as_str)?;
    Some(
        raw.trim_start_matches("/ipfs/")
            .trim_matches('/')
            .to_string(),
    )
}

fn parse_source(y: &Yaml) -> ManifestSource {
    let source = get(y, "source");
    let mapping = get(y, "mapping");

    let abis = mapping
        .and_then(|m| get(m, "abis"))
        .and_then(|a| a.as_vec())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    Some(AbiRef {
                        name: get_str(e, "name")?,
                        cid: get(e, "file").and_then(link_cid)?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let events = mapping
        .and_then(|m| get(m, "eventHandlers"))
        .and_then(|h| h.as_vec())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| get_str(e, "event"))
                // Folded scalars (`event: >-`) arrive with the newline already
                // resolved to a space; collapse the rest so the signature
                // matches the ABI-derived one character for character.
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(""))
                .collect()
        })
        .unwrap_or_default();

    ManifestSource {
        kind: get_str(y, "kind").unwrap_or_default(),
        name: get_str(y, "name").unwrap_or_default(),
        network: get_str(y, "network"),
        address: source.and_then(|s| get_str(s, "address")),
        start_block: source
            .and_then(|s| get(s, "startBlock"))
            .and_then(|b| match b {
                Yaml::Integer(i) if *i >= 0 => Some(*i as u64),
                other => as_str(other).and_then(|s| s.parse().ok()),
            }),
        abi_name: source.and_then(|s| get_str(s, "abi")),
        abis,
        events,
    }
}

pub fn parse_manifest(text: &str) -> Result<Manifest> {
    let docs = YamlLoader::load_from_str(text).context("subgraph manifest is not valid YAML")?;
    let doc = docs
        .first()
        .ok_or_else(|| anyhow!("subgraph manifest is empty"))?;

    let collect = |key: &str| -> Vec<ManifestSource> {
        get(doc, key)
            .and_then(|v| v.as_vec())
            .map(|v| v.iter().map(parse_source).collect())
            .unwrap_or_default()
    };

    let manifest = Manifest {
        spec_version: get_str(doc, "specVersion"),
        description: get_str(doc, "description"),
        data_sources: collect("dataSources"),
        templates: collect("templates"),
    };

    if manifest.data_sources.is_empty() && manifest.templates.is_empty() {
        bail!("manifest declares neither dataSources nor templates - is this a subgraph manifest?");
    }
    Ok(manifest)
}

// ── Aliases ──────────────────────────────────────────────────────────────

/// `GovernanceFactory` → `governance_factory`, within nuthatch's
/// `[a-z][a-z0-9_]*` alias grammar. Collisions are resolved by the caller.
pub fn to_alias(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() {
            let prev_lower =
                i > 0 && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit());
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
            if i > 0 && (prev_lower || next_lower) && !out.ends_with('_') {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    // Must start with a letter.
    match out.chars().next() {
        Some(c) if c.is_ascii_lowercase() => out,
        Some(_) => format!("c_{out}"),
        None => "c0".to_string(),
    }
}

/// Make `alias` unique against `taken`, appending `_2`, `_3`, … as needed.
pub fn dedupe_alias(alias: &str, taken: &mut BTreeMap<String, u32>) -> String {
    let n = taken.entry(alias.to_string()).or_insert(0);
    *n += 1;
    if *n == 1 {
        alias.to_string()
    } else {
        format!("{alias}_{n}")
    }
}

// ── Factory inference ────────────────────────────────────────────────────

/// One template we could not wire to a creating event, with whatever
/// candidates we saw — so the user edits a shortlist rather than reads WASM.
#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedTemplate {
    pub template: String,
    pub candidates: Vec<String>,
}

/// A factory rule we are confident enough to write.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredFactory {
    pub watch: String,
    pub event: String,
    pub child_param: String,
    pub template: String,
    /// Why we believe this, shown in the import report. An inference the user
    /// cannot audit is an inference they cannot correct.
    pub because: String,
}

/// An address-typed event parameter, resolved from a vendored ABI.
#[derive(Debug, Clone)]
pub struct AddressParam {
    pub watch_alias: String,
    pub event: String,
    pub param: String,
}

/// Pull every address-typed event parameter out of a vendored ABI.
pub fn address_params(watch_alias: &str, abi: &Value) -> Vec<AddressParam> {
    let mut out = Vec::new();
    let Some(items) = abi.as_array() else {
        return out;
    };
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("event") {
            continue;
        }
        let Some(event) = item.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(inputs) = item.get("inputs").and_then(Value::as_array) else {
            continue;
        };
        for input in inputs {
            if input.get("type").and_then(Value::as_str) != Some("address") {
                continue;
            }
            let Some(param) = input.get("name").and_then(Value::as_str) else {
                continue;
            };
            if param.is_empty() {
                continue;
            }
            out.push(AddressParam {
                watch_alias: watch_alias.to_string(),
                event: event.to_string(),
                param: param.to_string(),
            });
        }
    }
    out
}

fn normalise(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

const CREATION_VERBS: &[&str] = &[
    "created",
    "deployed",
    "registered",
    "added",
    "new",
    "launched",
    "spawned",
    "initialised",
    "initialized",
    "cloned",
    "minted",
];

/// Score how strongly `param` on `event` looks like it announces `template`.
///
/// Deliberately conservative: an exact param↔template name match is the only
/// signal strong enough to stand alone. Everything else needs corroboration
/// from a creation-shaped event name, and anything ambiguous is reported
/// rather than guessed.
fn score(template: &str, p: &AddressParam) -> (u32, String) {
    let t = normalise(template);
    let param = normalise(&p.param);
    let event = normalise(&p.event);
    let creation_verb = CREATION_VERBS.iter().find(|v| event.contains(*v));

    if param == t {
        return (
            100,
            format!("param `{}` names the template exactly", p.param),
        );
    }
    if event.contains(&t) && creation_verb.is_some() {
        return (
            70,
            format!(
                "event `{}` names the template and reads as a creation event",
                p.event
            ),
        );
    }
    if (param.contains(&t) || t.contains(&param)) && creation_verb.is_some() {
        return (
            50,
            format!(
                "param `{}` overlaps the template name on a creation event `{}`",
                p.param, p.event
            ),
        );
    }
    (0, String::new())
}

/// Confidence floor for writing a rule unasked. Below this we report.
const MIN_SCORE: u32 = 50;

/// Match templates to creating events. Returns rules we stand behind, plus
/// every template we could not resolve with its candidate shortlist.
///
/// Two rules govern what gets written, and both exist because the alternative
/// is a config that looks authoritative and indexes the wrong thing:
///
/// - **A template with tied top candidates is not resolved.** Two parameters
///   that look equally like its creator make the choice a coin flip.
/// - **A parameter creates at most one template.** `passkeyAccountFactoryBeacon`
///   substring-matches both `PasskeyAccountFactory` and `PasskeyAccount`; at
///   most one of those is true, so the stronger claim takes it and the other
///   is reported with the param as a candidate.
pub fn infer_factories(
    templates: &[String],
    params: &[AddressParam],
) -> (Vec<InferredFactory>, Vec<UnresolvedTemplate>) {
    // Pass 1: per-template shortlist, with the tie rule applied.
    struct Claim<'a> {
        template: &'a String,
        score: u32,
        /// Length of the normalised template name. Breaks score ties between
        /// templates whose names nest inside one another: against
        /// `passkeyAccountFactoryBeacon`, `PasskeyAccountFactory` is a longer
        /// and therefore more specific match than `PasskeyAccount`.
        specificity: usize,
        why: String,
        param: &'a AddressParam,
        shortlist: Vec<String>,
    }
    let mut claims: Vec<Claim> = Vec::new();
    let mut unresolved: Vec<UnresolvedTemplate> = Vec::new();

    for template in templates {
        let mut scored: Vec<(u32, String, &AddressParam)> = params
            .iter()
            .map(|p| {
                let (s, why) = score(template, p);
                (s, why, p)
            })
            .filter(|(s, _, _)| *s >= MIN_SCORE)
            .collect();
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        let shortlist: Vec<String> = scored
            .iter()
            .take(4)
            .map(|(_, _, p)| format!("{}.{} → {}", p.watch_alias, p.event, p.param))
            .collect();

        let top_is_unique = match scored.as_slice() {
            [] => false,
            [_] => true,
            [first, second, ..] => first.0 > second.0,
        };
        if top_is_unique {
            let (s, why, p) = &scored[0];
            claims.push(Claim {
                template,
                score: *s,
                specificity: normalise(template).len(),
                why: why.clone(),
                param: p,
                shortlist,
            });
        } else {
            unresolved.push(UnresolvedTemplate {
                template: template.clone(),
                candidates: shortlist,
            });
        }
    }

    // Pass 2: resolve contention over the same parameter. Strongest claim wins;
    // an equal-scored rival means neither is defensible, so both are reported.
    let mut rules = Vec::new();
    for (i, claim) in claims.iter().enumerate() {
        let key = (
            &claim.param.watch_alias,
            &claim.param.event,
            &claim.param.param,
        );
        let mine = (claim.score, claim.specificity);
        let rival = claims.iter().enumerate().find(|(j, other)| {
            *j != i
                && (
                    &other.param.watch_alias,
                    &other.param.event,
                    &other.param.param,
                ) == key
                && (other.score, other.specificity) >= mine
        });
        match rival {
            None => rules.push(InferredFactory {
                watch: claim.param.watch_alias.clone(),
                event: claim.param.event.clone(),
                child_param: claim.param.param.clone(),
                template: claim.template.clone(),
                because: claim.why.clone(),
            }),
            Some((_, winner)) => unresolved.push(UnresolvedTemplate {
                template: claim.template.clone(),
                candidates: if claim.shortlist.is_empty() {
                    Vec::new()
                } else {
                    let mut c = claim.shortlist.clone();
                    c.push(format!(
                        "(`{}` was claimed by template `{}` on a stronger match)",
                        claim.param.param, winner.template
                    ));
                    c
                },
            }),
        }
    }
    (rules, unresolved)
}

/// Event *name* from a manifest signature: `Transfer(indexed address,…)` → `Transfer`.
pub fn event_name(signature: &str) -> &str {
    signature.split('(').next().unwrap_or(signature).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
specVersion: 0.0.9
description: test fixture
dataSources:
  - kind: ethereum
    name: GovernanceFactory
    network: arbitrum-one
    source:
      abi: GovernanceFactory
      address: '0x24Fd3b269905AF10A6E5c67D93F0502Cd11Af875'
      startBlock: 447059876
    mapping:
      abis:
        - file:
            /: /ipfs/Qmco6j6G3fpC1VVoBFFYjTY6hvJxUxUrtaqgFCftA6RW4s
          name: GovernanceFactory
      eventHandlers:
        - event: >-
            ModuleDeployed(indexed bytes32,indexed
            bytes32,address,address,bool,address)
          handler: handleModuleDeployed
  - kind: file/ipfs
    name: OrgMetadata
    network: arbitrum-one
    mapping:
      abis: []
templates:
  - kind: ethereum/contract
    name: HybridVoting
    network: arbitrum-one
    source:
      abi: HybridVoting
    mapping:
      abis:
        - file:
            /: /ipfs/QmHybridVotingAbiCid000000000000000000000000
          name: HybridVoting
"#;

    #[test]
    fn parses_data_sources_templates_and_links() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.spec_version.as_deref(), Some("0.0.9"));
        assert_eq!(m.data_sources.len(), 2);
        assert_eq!(m.templates.len(), 1);

        let ds = &m.data_sources[0];
        assert_eq!(ds.name, "GovernanceFactory");
        assert_eq!(
            ds.address.as_deref(),
            Some("0x24Fd3b269905AF10A6E5c67D93F0502Cd11Af875")
        );
        assert_eq!(ds.start_block, Some(447_059_876));
        assert_eq!(
            ds.own_abi().unwrap().cid,
            "Qmco6j6G3fpC1VVoBFFYjTY6hvJxUxUrtaqgFCftA6RW4s"
        );
    }

    #[test]
    fn folded_event_signatures_are_collapsed() {
        // `event: >-` wraps long signatures across lines. If we keep the
        // newline the signature never matches the ABI and every handler looks
        // unknown, which is a silent, total failure.
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(
            m.data_sources[0].events[0],
            "ModuleDeployed(indexedbytes32,indexedbytes32,address,address,bool,address)"
        );
        assert_eq!(event_name(&m.data_sources[0].events[0]), "ModuleDeployed");
    }

    #[test]
    fn file_data_sources_are_not_evm() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert!(m.data_sources[0].is_evm());
        assert!(!m.data_sources[1].is_evm());
    }

    #[test]
    fn network_is_unanimous_or_an_error() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.network().unwrap(), "arbitrum-one");

        let mixed = MANIFEST.replace(
            "network: arbitrum-one\n    source:\n      abi: HybridVoting",
            "network: mainnet\n    source:\n      abi: HybridVoting",
        );
        let m2 = parse_manifest(&mixed).unwrap();
        // Templates don't vote on the nest's chain — only dataSources do.
        assert_eq!(m2.network().unwrap(), "arbitrum-one");
    }

    #[test]
    fn rejects_non_manifests() {
        assert!(parse_manifest("just: a map").is_err());
        assert!(parse_manifest("").is_err());
    }

    #[test]
    fn aliases_follow_the_nest_grammar() {
        for (input, want) in [
            ("GovernanceFactory", "governance_factory"),
            ("PoaManagerSatellite", "poa_manager_satellite"),
            ("ERC20", "erc20"),
            // A digit before a capital is a word boundary — this is what keeps
            // `ERC20Token` from collapsing into `erc20token`.
            ("ERC20Token", "erc20_token"),
            ("Uniswap V3 Pool", "uniswap_v3_pool"),
            ("hybrid-voting", "hybrid_voting"),
            // Leading digit can't start an alias, so it gets the `c_` prefix.
            ("9Lives", "c_9_lives"),
        ] {
            let got = to_alias(input);
            assert_eq!(got, want, "alias for {input}");
            let mut chars = got.chars();
            assert!(matches!(chars.next(), Some(c) if c.is_ascii_lowercase()));
            assert!(chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
        }
    }

    #[test]
    fn duplicate_aliases_are_suffixed() {
        let mut taken = BTreeMap::new();
        assert_eq!(dedupe_alias("pool", &mut taken), "pool");
        assert_eq!(dedupe_alias("pool", &mut taken), "pool_2");
        assert_eq!(dedupe_alias("pool", &mut taken), "pool_3");
    }

    fn param(watch: &str, event: &str, p: &str) -> AddressParam {
        AddressParam {
            watch_alias: watch.into(),
            event: event.into(),
            param: p.into(),
        }
    }

    #[test]
    fn infers_a_rule_when_the_param_names_the_template() {
        let params = vec![param("factory", "PoolCreated", "pool")];
        let (rules, unresolved) = infer_factories(&["Pool".into()], &params);
        assert_eq!(unresolved.len(), 0);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].child_param, "pool");
        assert_eq!(rules[0].watch, "factory");
        assert_eq!(rules[0].event, "PoolCreated");
    }

    #[test]
    fn reports_rather_than_guesses_when_two_params_tie() {
        // OrgDeployed announces ten modules; nothing in the manifest says which
        // one is the HybridVoting. A coin flip written into someone's config is
        // worse than a line telling them to choose.
        let params = vec![
            param("deployer", "OrgDeployed", "hybridVoting"),
            param("deployer", "OrgDeployed", "quickJoin"),
        ];
        let (rules, unresolved) = infer_factories(&["Executor".into()], &params);
        assert!(rules.is_empty());
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].template, "Executor");
    }

    #[test]
    fn an_exact_param_match_wins_against_a_weaker_candidate() {
        let params = vec![
            param("deployer", "OrgDeployed", "hybridVoting"),
            param("deployer", "OrgDeployed", "somethingElse"),
        ];
        let (rules, _) = infer_factories(&["HybridVoting".into()], &params);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].child_param, "hybridVoting");
    }

    #[test]
    fn one_param_cannot_create_two_templates() {
        // Caught running the real POA manifest: `passkeyAccountFactoryBeacon`
        // substring-matches both PasskeyAccountFactory and PasskeyAccount, and
        // both were being written as rules. At most one is true.
        let params = vec![param(
            "poa_manager",
            "InfrastructureDeployed",
            "passkeyAccountFactoryBeacon",
        )];
        let (rules, unresolved) = infer_factories(
            &["PasskeyAccountFactory".into(), "PasskeyAccount".into()],
            &params,
        );
        assert_eq!(rules.len(), 1, "exactly one template may claim the param");
        assert_eq!(unresolved.len(), 1);
        // The loser is told who took it, so the user can correct the choice.
        assert!(unresolved[0]
            .candidates
            .iter()
            .any(|c| c.contains("claimed by template")));
    }

    #[test]
    fn a_stronger_claim_beats_a_weaker_one_on_the_same_param() {
        // Exact name match (100) must outrank substring overlap (50).
        let params = vec![param("f", "PoolCreated", "pool")];
        let (rules, _) = infer_factories(&["PoolCreatedThing".into(), "Pool".into()], &params);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].template, "Pool");
    }

    #[test]
    fn unrelated_addresses_never_become_rules() {
        // `to`/`from` on a Transfer are addresses, but nothing to do with a
        // template. Inventing a factory from them would index garbage.
        let params = vec![
            param("token", "Transfer", "from"),
            param("token", "Transfer", "to"),
        ];
        let (rules, unresolved) = infer_factories(&["Pool".into()], &params);
        assert!(rules.is_empty());
        assert_eq!(unresolved[0].candidates.len(), 0);
    }

    #[test]
    fn address_params_are_read_from_the_abi() {
        let abi: Value = serde_json::from_str(
            r#"[
              {"type":"event","name":"PoolCreated","inputs":[
                {"name":"token0","type":"address","indexed":true},
                {"name":"fee","type":"uint24","indexed":true},
                {"name":"pool","type":"address","indexed":false}]},
              {"type":"function","name":"owner","inputs":[]}
            ]"#,
        )
        .unwrap();
        let got = address_params("factory", &abi);
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].param, "pool");
        assert_eq!(got[1].event, "PoolCreated");
    }

    #[test]
    fn cid_urls_are_built_for_every_gateway() {
        let gws: Vec<String> = DEFAULT_IPFS_GATEWAYS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let urls = candidate_urls("QmVPhLwuv9zn2c761Ua7eJAFXBmrNsW32XUYsj1GKtNX4X", &gws).unwrap();
        assert_eq!(urls.len(), gws.len());
        assert!(urls[0].ends_with("QmVPhLwuv9zn2c761Ua7eJAFXBmrNsW32XUYsj1GKtNX4X"));

        // ipfs:// and /ipfs/ prefixes resolve to the same CID.
        for form in ["ipfs://QmAbc", "/ipfs/QmAbc", "QmAbc"] {
            assert!(
                candidate_urls(form, &gws).unwrap()[0].ends_with("QmAbc"),
                "{form}"
            );
        }
        // A full URL is used verbatim, not gatewayed.
        assert_eq!(
            candidate_urls("https://example.com/subgraph.yaml", &gws).unwrap(),
            vec!["https://example.com/subgraph.yaml".to_string()]
        );
        assert!(candidate_urls("", &gws).is_err());
        assert!(candidate_urls("not a cid", &gws).is_err());
    }
}
