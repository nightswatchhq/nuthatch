//! Scaffold a nest from a Graph Protocol subgraph manifest (#241 item 5).
//!
//! A subgraph manifest already states almost everything a nest needs: which
//! contracts, on which chain, from which block, and — crucially — which ABIs,
//! pinned by CID. For a codebase of beacon proxies those pinned ABIs are
//! strictly better than Sourcify, which returns the proxy ABI carrying none of
//! the events the contract actually emits. The manifest is the only good
//! source, and it is already content-addressed.
//!
//! Content-addressed, but **not verified here**: nothing recomputes the multihash over the bytes
//! a gateway returns, so a hostile or compromised gateway can serve any document for any CID and
//! this module will vendor it. The CID buys a stable name to ask for, not proof of what came
//! back. Verifying it is worth doing and is not done yet — say so rather than let the phrase
//! "pinned by CID" imply a guarantee the code does not provide.
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
    // Cloudflare retired `cloudflare-ipfs.com` - it no longer resolves at all, so it was a
    // guaranteed-dead third try. Pinata's public gateway answers the path-prefix form, which is
    // what this list needs: `dweb.link` and `w3s.link` redirect to a `<cid>.ipfs.<host>` subdomain
    // that prefix concatenation cannot express.
    "https://gateway.pinata.cloud/ipfs/",
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
    /// `source.endBlock`, where the subgraph stops. A nest has no way to express one, so this
    /// exists to be *reported* - diverging from the manifest silently is the thing this module
    /// is not allowed to do.
    pub end_block: Option<u64>,
    /// True when the source declares `blockHandlers` or `callHandlers`. nuthatch indexes logs,
    /// so those handlers have no equivalent - and a source carrying only them parses to an empty
    /// event list, which in `[[contracts]]` means "index every event in the ABI", the opposite of
    /// what the subgraph did.
    pub has_non_event_handlers: bool,
    /// Event signatures with whitespace removed, e.g.
    /// `Transfer(indexedaddress,indexedaddress,uint256)` - folded scalars are collapsed so the
    /// signature matches the ABI-derived one character for character. Only [`event_name`] reads
    /// this, and it stops at the first `(`.
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
    ///
    /// Falls back to the first entry when the named one is absent. See [`Self::abi_is_fallback`]:
    /// the fallback is a guess, and a guess this module makes has to be reported.
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

    /// True when `source.abi` names an entry `mapping.abis` does not contain, so [`Self::own_abi`]
    /// fell back to the first one. The manifest asked for a specific ABI and we vendored a
    /// different one; that is exactly the kind of divergence this module is supposed to say out
    /// loud rather than absorb.
    pub fn abi_is_fallback(&self) -> bool {
        match &self.abi_name {
            Some(want) => !self.abis.iter().any(|a| &a.name == want) && !self.abis.is_empty(),
            None => false,
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

/// Who supplied a reference - which decides whether it may be a URL at all.
///
/// **This distinction is the security boundary of subgraph import.** The argument an operator types
/// is theirs to point anywhere they like. Every other reference - the ABI links inside the manifest -
/// arrives from a document fetched off a public gateway, and is therefore attacker-controlled input
/// that happens to be handed to an HTTP client. Letting those be URLs turns `init --from-subgraph`
/// into a request forgery: a manifest carrying `file: {"/": "http://169.254.169.254/…"}` would make
/// the operator's own machine fetch cloud-instance credentials and write the reply into `abis/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The `--from-subgraph` argument. May be a CID or a URL; the operator chose it.
    Operator,
    /// A link read out of a manifest. **CID only** - never a URL, whatever it claims to be.
    Manifest,
}

/// The most sources one manifest may declare.
///
/// Every dataSource and template costs a gateway fetch and a file under `abis/`, and the
/// manifest - not the operator - decides how many there are. A 16 MiB document (the
/// [`MAX_FETCH_BYTES`] ceiling) holds on the order of 100k minimal entries, which is 100k
/// requests issued from the operator's address. Real manifests are tens; the POA DAO stack
/// that motivated this feature has 19.
const MAX_SOURCES: usize = 512;

/// A CID is base58btc or base32 - alphanumeric throughout. Anything else is a path, a query, a
/// scheme, or an attempt at one, and none of those belong in a gateway URL we build by
/// concatenation. The upper bound matters for the same reason: shape alone would accept a
/// multi-megabyte alphanumeric run, which we would then concatenate into three gateway URLs and
/// echo back inside an error message. CIDv0 is 46 characters, CIDv1 base32 is 59; 80 leaves room
/// for longer multihashes. There is deliberately no lower bound - a short string is merely a
/// 404 at the gateway, and inventing a minimum would reject identifiers that are legitimately
/// short without buying anything.
fn is_cid_shaped(s: &str) -> bool {
    !s.is_empty() && s.len() <= 80 && s.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Accepts a bare CID, `ipfs://<cid>` or `/ipfs/<cid>` from anywhere, and a full http(s) URL only
/// from the operator (see [`Origin`]). Returns the list of URLs to try in order.
pub fn candidate_urls(source: &str, gateways: &[String], origin: Origin) -> Result<Vec<String>> {
    let s = source.trim();
    if s.is_empty() {
        bail!("empty subgraph source");
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        return match origin {
            Origin::Operator => Ok(vec![s.to_string()]),
            Origin::Manifest => bail!(
                "this manifest asks us to fetch a URL rather than a CID: {s:?}. Refusing - a \
                 manifest comes from a public gateway, so honouring that would let whoever wrote it \
                 choose what your machine connects to. Only the --from-subgraph argument you type \
                 may be a URL."
            ),
        };
    }
    let cid = s
        .strip_prefix("ipfs://")
        .or_else(|| s.strip_prefix("/ipfs/"))
        .unwrap_or(s)
        .trim_matches('/');
    if !is_cid_shaped(cid) {
        bail!("'{source}' is not a CID or URL - expected e.g. QmVPhL… or ipfs://QmVPhL…");
    }
    Ok(gateways.iter().map(|g| format!("{g}{cid}")).collect())
}

/// Fetch a document, trying each gateway until one answers. Gateways fail
/// often and individually, so a single failure is never fatal; only running
/// out of them is, and then we say which ones we tried.
pub async fn fetch_ipfs(source: &str, gateways: &[String], origin: Origin) -> Result<String> {
    let urls = candidate_urls(source, gateways, origin)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")?;

    let mut failures = Vec::new();
    for url in &urls {
        match client.get(url).send().await {
            Ok(resp) if resp.status().is_success() => match read_capped(resp).await {
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

/// The most we will hold in memory for one manifest or ABI.
///
/// A gateway is a third party, and the reply size is its choice rather than ours. Streaming with a
/// ceiling means a hostile or broken one costs a bounded amount of RAM instead of whatever it feels
/// like sending - the same reasoning as the `/sql` result cap, applied to the one place `init`
/// trusts someone else's server. Real manifests are kilobytes; the largest ABI we have seen is far
/// under a megabyte.
const MAX_FETCH_BYTES: usize = 16 * 1024 * 1024;

/// Read a response body, refusing past [`MAX_FETCH_BYTES`] rather than buffering whatever arrives.
async fn read_capped(resp: reqwest::Response) -> Result<String> {
    use futures::StreamExt;
    let mut out: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading response body")?;
        if out.len() + chunk.len() > MAX_FETCH_BYTES {
            bail!(
                "response exceeds {} MiB - refusing to buffer it",
                MAX_FETCH_BYTES / 1024 / 1024
            );
        }
        out.extend_from_slice(&chunk);
    }
    String::from_utf8(out).context("response is not UTF-8")
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

/// Make a manifest-supplied string safe to print, and bounded.
///
/// The import's report is the deliverable as much as the config is - an operator reads it to
/// decide which warnings to act on. Names come from the manifest, so a `\r` can erase the line
/// above and a `\x1b[` sequence can repaint it: a source that was skipped can be made to look
/// like one that was imported. Controls become their escape form, and the length is capped so a
/// single name cannot scroll the rest of the report off the screen.
fn sanitise(s: &str) -> String {
    const MAX: usize = 120;
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_control() {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
        if out.len() >= MAX {
            out.push('…');
            break;
        }
    }
    out
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
                        name: sanitise(&get_str(e, "name")?),
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
        // Sanitised at the boundary, so no unescaped copy exists downstream to be printed.
        kind: sanitise(&get_str(y, "kind").unwrap_or_default()),
        name: sanitise(&get_str(y, "name").unwrap_or_default()),
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
        end_block: source
            .and_then(|s| get(s, "endBlock"))
            .and_then(|b| match b {
                Yaml::Integer(i) if *i >= 0 => Some(*i as u64),
                other => as_str(other).and_then(|s| s.parse().ok()),
            }),
        has_non_event_handlers: mapping
            .map(|m| {
                ["blockHandlers", "callHandlers"].iter().any(|k| {
                    get(m, k)
                        .and_then(|h| h.as_vec())
                        .is_some_and(|v| !v.is_empty())
                })
            })
            .unwrap_or(false),
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
    let declared = manifest.data_sources.len() + manifest.templates.len();
    if declared > MAX_SOURCES {
        bail!(
            "manifest declares {declared} sources, more than the {MAX_SOURCES} this import will \
             follow. Each one costs a gateway fetch and a file on disk, and the manifest chooses \
             how many there are - a hostile one would have your machine make the requests. If a \
             real subgraph is this large, import it in pieces with `nuthatch init` per contract."
        );
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
///
/// Every name this hands out is recorded, not just the base it was derived from. Counting only
/// bases is not enough, because the suffix form is itself a name a manifest can ask for:
/// `Pool`, `Pool`, `Pool 2` aliases to `pool`, `pool_2`, and then — with a base-only counter —
/// `pool_2` again, because `pool_2` had never been counted. Two contracts would own
/// `abis/pool_2.json` and the second write would silently clobber the first, leaving a real
/// contract decoded against another contract's ABI. The manifest chooses these names, so the
/// collision is arranged, not stumbled into.
pub fn dedupe_alias(alias: &str, taken: &mut BTreeMap<String, u32>) -> String {
    let mut n = *taken
        .entry(alias.to_string())
        .and_modify(|n| *n += 1)
        .or_insert(1);
    let mut candidate = if n == 1 {
        alias.to_string()
    } else {
        format!("{alias}_{n}")
    };
    // Walk past anything already issued under a different base.
    while taken.contains_key(&candidate) && candidate != *alias {
        n += 1;
        candidate = format!("{alias}_{n}");
    }
    taken.insert(alias.to_string(), n);
    taken.entry(candidate.clone()).or_insert(1);
    candidate
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
      eventHandlers:
        - event: VoteCast(indexed address,indexed uint256,uint8)
          handler: handleVoteCast
"#;

    #[test]
    fn parses_data_sources_templates_and_links() {
        let m = parse_manifest(MANIFEST).unwrap();
        assert_eq!(m.spec_version.as_deref(), Some("0.0.9"));
        assert_eq!(m.data_sources.len(), 2);
        assert_eq!(m.templates.len(), 1);

        // A template's `eventHandlers` say which of its ABI's events the subgraph actually decoded,
        // and the import carries them into `[[templates]].events`. Asserted here because the parse
        // is the *source* of that allowlist: the import discarded it for as long as this data was
        // parsed and unread, and an empty list here would silently restore the superset decode.
        assert_eq!(
            m.templates[0].events,
            vec!["VoteCast(indexedaddress,indexeduint256,uint8)"],
            "a template's event handlers must survive the parse"
        );
        assert_eq!(event_name(&m.templates[0].events[0]), "VoteCast");

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

    /// **The one that matters.** A manifest is fetched from a public gateway, so its ABI links are
    /// attacker-controlled strings handed to an HTTP client. Before the `Origin` split they were
    /// passed through verbatim whenever they looked like a URL, which made
    /// `init --from-subgraph <hostile-cid>` fetch whatever the manifest's author chose - including
    /// `169.254.169.254`, where a cloud host serves instance credentials - and write the reply into
    /// `abis/`. Reproduced end to end from the manifest, not asserted against the helper in isolation.
    #[test]
    fn a_manifest_cannot_make_us_fetch_a_url() {
        let m = r#"
specVersion: 0.0.5
dataSources:
  - kind: ethereum/contract
    name: Evil
    network: mainnet
    source:
      address: "0x0000000000000000000000000000000000000001"
      startBlock: 1
    mapping:
      abis:
        - name: Evil
          file:
            /: "http://169.254.169.254/latest/meta-data/iam/security-credentials/"
      eventHandlers:
        - event: Transfer(address,address,uint256)
"#;
        let man = parse_manifest(m).unwrap();
        let link = &man.data_sources[0].abis[0].cid;
        let err = candidate_urls(link, &["https://gw/".into()], Origin::Manifest)
            .expect_err("a manifest link that is a URL must be refused");
        let err = format!("{err:#}");
        assert!(err.contains("169.254.169.254"), "name the target: {err}");
        assert!(
            err.contains("--from-subgraph"),
            "say which reference may be a URL: {err}"
        );
    }

    /// The operator's own argument is theirs to point anywhere - that is the whole difference.
    #[test]
    fn the_operator_may_still_pass_a_url() {
        let urls = candidate_urls("https://example.com/subgraph.yaml", &[], Origin::Operator)
            .expect("an operator-supplied URL is intentional");
        assert_eq!(urls, vec!["https://example.com/subgraph.yaml"]);
    }

    /// A CID is concatenated onto a gateway prefix, so anything that is not alphanumeric can reach
    /// outside the path the gateway meant to serve - `../`, a query, a second scheme.
    #[test]
    fn a_cid_that_is_not_cid_shaped_is_refused() {
        for bad in [
            "../../etc/passwd",
            "Qm..%2f..%2fadmin",
            "Qm?redirect=http://evil",
            "Qm#frag",
            "Qm x",
            "",
        ] {
            assert!(
                candidate_urls(bad, &["https://gw/".into()], Origin::Manifest).is_err(),
                "{bad:?} should not be accepted as a CID"
            );
        }
        assert!(candidate_urls(
            "QmVPhLwuv9zn2c761Ua7eJ",
            &["https://gw/".into()],
            Origin::Manifest
        )
        .is_ok());
    }

    /// The alias a manifest can arrange to collide with. `Pool`, `Pool`, `Pool 2` walks the
    /// base counter to `pool_2` and then asks for `pool_2` directly - which a base-only counter
    /// hands out a second time, putting two contracts on one `abis/pool_2.json`.
    #[test]
    fn a_manifest_cannot_arrange_two_sources_onto_one_alias() {
        let mut taken = BTreeMap::new();
        let issued: Vec<String> = ["Pool", "Pool", "Pool 2", "Pool_2", "Pool"]
            .iter()
            .map(|n| dedupe_alias(&to_alias(n), &mut taken))
            .collect();
        let unique: std::collections::BTreeSet<&String> = issued.iter().collect();
        assert_eq!(
            unique.len(),
            issued.len(),
            "every alias must be unique, got {issued:?}"
        );
        assert_eq!(issued[0], "pool");
        assert_eq!(issued[1], "pool_2");
    }

    /// Three ways the import can quietly diverge from the manifest, each of which has to reach
    /// the operator as a note rather than being absorbed: an `endBlock` a nest cannot express,
    /// handlers nuthatch has no equivalent for, and a `source.abi` naming an entry that is not
    /// there. Asserted on the parse, which is what the reporting reads.
    #[test]
    fn divergences_from_the_manifest_are_visible() {
        let m = r#"
specVersion: 0.0.5
dataSources:
  - kind: ethereum/contract
    name: Blocky
    network: mainnet
    source:
      address: "0x0000000000000000000000000000000000000001"
      startBlock: 1
      endBlock: 500
      abi: NotListed
    mapping:
      abis:
        - name: Actual
          file:
            /: QmAbc
      blockHandlers:
        - handler: handleBlock
"#;
        let ds = &parse_manifest(m).unwrap().data_sources[0];
        assert_eq!(
            ds.end_block,
            Some(500),
            "endBlock must be carried to report it"
        );
        assert!(ds.has_non_event_handlers, "blockHandlers must be noticed");
        assert!(
            ds.events.is_empty(),
            "no eventHandlers means an empty allowlist"
        );
        assert!(
            ds.abi_is_fallback(),
            "`abi: NotListed` is absent from mapping.abis, so own_abi() guessed"
        );
        assert_eq!(ds.own_abi().unwrap().name, "Actual");
    }

    /// A manifest that declares more sources than we will follow is refused before the first
    /// fetch, because each source is a request issued from the operator's address.
    #[test]
    fn an_oversized_manifest_is_refused_before_any_fetch() {
        let mut m = String::from("specVersion: 0.0.5\ndataSources:\n");
        for i in 0..(MAX_SOURCES + 1) {
            m.push_str(&format!(
                "  - kind: ethereum/contract\n    name: C{i}\n    network: mainnet\n    source:\n      address: \"0x0000000000000000000000000000000000000001\"\n"
            ));
        }
        let err = format!("{:#}", parse_manifest(&m).expect_err("must refuse"));
        assert!(err.contains("more than the"), "explain the limit: {err}");
    }

    /// A name is manifest-chosen and ends up in the report an operator reads to decide what to
    /// act on. Escape sequences there can erase or repaint lines that were already printed.
    #[test]
    fn manifest_names_cannot_repaint_the_report() {
        let m = "specVersion: 0.0.5\ndataSources:\n  - kind: ethereum/contract\n    name: \"Good\\r\\u001b[2KEvil\"\n    network: mainnet\n    source:\n      address: \"0x0000000000000000000000000000000000000001\"\n";
        let parsed = parse_manifest(m).unwrap();
        let name = &parsed.data_sources[0].name;
        assert!(
            !name.contains('\r') && !name.contains('\u{1b}'),
            "controls must not survive parsing: {name:?}"
        );
    }

    /// Shape without size still lets a manifest hand us a multi-megabyte "CID" to concatenate
    /// into three gateway URLs and echo back in an error.
    #[test]
    fn an_absurdly_long_cid_is_refused() {
        let huge = "Q".repeat(100_000);
        assert!(candidate_urls(&huge, &["https://gw/".into()], Origin::Manifest).is_err());
        assert!(candidate_urls(
            "QmVPhLwuv9zn2c761Ua7eJAFXBmrNsW32XUYsj1GKtNX4X",
            &["https://gw/".into()],
            Origin::Manifest
        )
        .is_ok());
    }

    #[test]
    fn cid_urls_are_built_for_every_gateway() {
        let gws: Vec<String> = DEFAULT_IPFS_GATEWAYS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let urls = candidate_urls(
            "QmVPhLwuv9zn2c761Ua7eJAFXBmrNsW32XUYsj1GKtNX4X",
            &gws,
            Origin::Manifest,
        )
        .unwrap();
        assert_eq!(urls.len(), gws.len());
        assert!(urls[0].ends_with("QmVPhLwuv9zn2c761Ua7eJAFXBmrNsW32XUYsj1GKtNX4X"));

        // ipfs:// and /ipfs/ prefixes resolve to the same CID.
        for form in ["ipfs://QmAbc", "/ipfs/QmAbc", "QmAbc"] {
            assert!(
                candidate_urls(form, &gws, Origin::Manifest).unwrap()[0].ends_with("QmAbc"),
                "{form}"
            );
        }
        // A full URL is used verbatim, not gatewayed - but only when the operator supplied it.
        assert_eq!(
            candidate_urls("https://example.com/subgraph.yaml", &gws, Origin::Operator).unwrap(),
            vec!["https://example.com/subgraph.yaml".to_string()]
        );
        assert!(candidate_urls("", &gws, Origin::Operator).is_err());
        assert!(candidate_urls("not a cid", &gws, Origin::Operator).is_err());
    }
}
