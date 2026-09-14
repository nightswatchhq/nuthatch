//! Resolving IPFS documents a nest's rows point at (RFC-0037 slices 2-3).
//!
//! A subgraph's `file/ipfs` data sources index the *content* behind a CID. `subgraph_import` says
//! what nuthatch did instead: it "indexes the metadata hash as a column value and stops there". So
//! the CID is already a column and **the join key already exists** - this is a second *source* of
//! rows, not a second storage path, which is the distinction RFC-0036 §3 turned on.
//!
//! ## Why this may feed canonical state
//!
//! The purity rule exiles effectful components to annotations because an HTTP enricher can hand two
//! operators different answers with neither able to tell. **Content addressing breaks that
//! symmetry.** `CID → bytes` is checkable ([`crate::cid`]), so two operators either agree or one of
//! them has nothing: the failure mode is *unavailability*, not *divergence*. That is a categorically
//! weaker failure than the rule was written to prevent, and it is the same property `CallKey` leans
//! on for pinned reads.
//!
//! Absence is therefore representable rather than papered over: a row whose CID has not resolved
//! simply has no counterpart, and a `LEFT JOIN` says so. That is why resolution is a side table and
//! not a column on the event row - enrichment forces a decision at write time about data that may
//! arrive later or never, and a join does not.
//!
//! ## The honest limit
//!
//! Resolution happens inline in the window, bounded by a per-window budget, and a gateway that does
//! not answer in time leaves the document **unresolved** rather than failing the nest. RFC-0037 asks
//! for resolution strictly behind the cursor and out of band; that wants a queue and a worker, and is
//! the follow-up. What is here must never make tip-following wait indefinitely, which the budget
//! enforces - and never claim a document it did not verify, which [`crate::cid`] enforces.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeMap;

/// A nest's declaration that a column holds CIDs worth resolving.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpfsDecl {
    /// Table name for the resolved documents, e.g. `token_metadata`.
    pub name: String,
    /// The table whose rows carry the CIDs.
    pub on: String,
    /// The column holding the CID. `{column}` or a bare column name; both are accepted because a
    /// CID column is never a literal, so there is nothing for the braces to disambiguate.
    pub cid_column: String,
    /// The column holds JSON that names the CID under this top-level key, rather than the CID itself.
    /// Edge & Node's QoS oracle posts `{"topic", "hash", "timestamp"}` as a `bytes` calldata argument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cid_json_path: Option<String>,
    /// Resolve only the JSON objects whose string fields equal these values. Declining is declared
    /// intent rather than a failure, so it happens before a fetch and is not counted as unreadable.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub json_match: BTreeMap<String, String>,
    /// Explode each proven document into typed rows in a table of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<IpfsRows>,
}

/// One typed row per element of a proven JSON document, in a table of its own (RFC-0037 slice 8), so a
/// rollup reads columns rather than parsing megabytes of JSON on every query.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IpfsRows {
    /// The typed table, e.g. `qos_indexer_attempt`.
    pub table: String,
    /// The most elements a document may have. Also the room each document is allotted in its block's
    /// key space, which is what lets a row's key follow from the plan rather than from resolution order.
    pub max_rows: u64,
    /// `false` stores the document row with empty `content`: the rows carry the data and the CID still
    /// names the bytes they came from.
    #[serde(default = "keep_content")]
    pub keep_content: bool,
    pub columns: Vec<RowColumn>,
}

fn keep_content() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RowColumn {
    /// The element's top-level key, and the column's name.
    pub name: String,
    #[serde(rename = "type")]
    pub ty: RowType,
}

/// What a typed column holds. `number` keeps a JSON number as its decimal text rather than choosing a
/// float or a rounding here; a view casts it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RowType {
    String,
    Number,
    U64,
    I64,
    Bool,
    Address,
}

impl RowType {
    pub fn name(self) -> &'static str {
        match self {
            RowType::String => "string",
            RowType::Number => "number",
            RowType::U64 => "u64",
            RowType::I64 => "i64",
            RowType::Bool => "bool",
            RowType::Address => "address",
        }
    }

    fn storage(self) -> &'static str {
        match self {
            RowType::Number => "string",
            other => other.name(),
        }
    }
}

/// The columns every typed row carries ahead of its declared ones: the document it came from, the row
/// that named that document, and its position in the document.
pub const ROW_COLUMNS: [&str; 4] = ["cid", "document_log_index", "source_log_index", "element"];

impl IpfsDecl {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            bail!("an [[ipfs]] declaration needs a `name` - it becomes the result table");
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!(
                "ipfs `{}`: name must be [A-Za-z0-9_] - it is used as a table identifier",
                self.name
            );
        }
        if self.on.is_empty() {
            bail!(
                "ipfs `{}`: needs `on` - the table whose rows carry the CIDs",
                self.name
            );
        }
        if self.column().is_empty() {
            bail!(
                "ipfs `{}`: needs `cid_column` - which column of `{}` holds the CID",
                self.name,
                self.on
            );
        }
        let key_ok =
            |k: &str| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if let Some(key) = &self.cid_json_path {
            if !key_ok(key) {
                bail!(
                    "ipfs `{}`: `cid_json_path` is one top-level JSON key ([A-Za-z0-9_]), not a path \
                     expression - got {key:?}",
                    self.name
                );
            }
        }
        if !self.json_match.is_empty() && self.cid_json_path.is_none() {
            bail!(
                "ipfs `{}`: `json_match` filters JSON documents, so it needs `cid_json_path`",
                self.name
            );
        }
        if let Some(k) = self.json_match.keys().find(|k| !key_ok(k)) {
            bail!(
                "ipfs `{}`: `json_match` keys are top-level JSON keys ([A-Za-z0-9_]) - got {k:?}",
                self.name
            );
        }
        if let Some(rows) = &self.rows {
            if !key_ok(&rows.table) || rows.table == self.name {
                bail!(
                    "ipfs `{}`: `rows.table` must be [A-Za-z0-9_] and differ from `name` - got {:?}",
                    self.name,
                    rows.table
                );
            }
            let room = crate::registry::IPFS_DOCUMENT_ROW_LOG_INDEX_END
                - crate::registry::IPFS_DOCUMENT_ROW_LOG_INDEX_BASE
                + 1;
            if rows.max_rows == 0 || rows.max_rows > room {
                bail!(
                    "ipfs `{}`: `rows.max_rows` must be 1 to {room}, the room a block has for typed \
                     rows - got {}",
                    self.name,
                    rows.max_rows
                );
            }
            if rows.columns.is_empty() {
                bail!(
                    "ipfs `{}`: `rows.columns` is empty - a typed table needs a column",
                    self.name
                );
            }
            let implicit = crate::registry::implicit_columns(true);
            let mut seen = std::collections::HashSet::new();
            for c in &rows.columns {
                let reserved = ROW_COLUMNS.contains(&c.name.as_str())
                    || implicit.iter().any(|i| i.name == c.name);
                if !key_ok(&c.name) || reserved || !seen.insert(c.name.as_str()) {
                    bail!(
                        "ipfs `{}`: row column {:?} must be [A-Za-z0-9_], declared once, and not a \
                         column every row already carries",
                        self.name,
                        c.name
                    );
                }
            }
        }
        Ok(())
    }

    /// Every CID one column value names under this declaration, and how many it should have named and
    /// did not.
    ///
    /// Without `cid_json_path` that is at most one, exactly as before. With it the value is JSON (a
    /// string, or bytes that are UTF-8): an object names one document and an array one per element.
    /// The second number exists because a declaration that parses and then resolves nothing is the
    /// failure RFC-0037 §5 names, and a skipped row used to leave no trace at all.
    pub fn cids_in(&self, v: &crate::registry::Value) -> (Vec<String>, usize) {
        use crate::registry::Value;
        let Some(key) = self.cid_json_path.as_deref() else {
            return match cid_from_value(v) {
                Some(cid) => (vec![cid.into_owned()], 0),
                None => (Vec::new(), 1),
            };
        };
        let text = match v {
            Value::Str(s) => Some(s.as_str()),
            Value::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        };
        let Some(doc) = text.and_then(|t| serde_json::from_str::<serde_json::Value>(t).ok()) else {
            return (Vec::new(), 1);
        };
        let elements: Vec<&serde_json::Value> = match &doc {
            serde_json::Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        let (mut cids, mut unreadable) = (Vec::new(), 0);
        for e in elements {
            let Some(obj) = e.as_object() else {
                unreadable += 1;
                continue;
            };
            let wanted = self
                .json_match
                .iter()
                .all(|(k, want)| obj.get(k).and_then(|x| x.as_str()) == Some(want.as_str()));
            if !wanted {
                continue;
            }
            match obj.get(key).and_then(|c| c.as_str()).and_then(cid_from_any) {
                Some(cid) => cids.push(cid.to_string()),
                None => unreadable += 1,
            }
        }
        (cids, unreadable)
    }

    /// The column name, with `{}` stripped if the operator wrote them.
    pub fn column(&self) -> &str {
        self.cid_column
            .strip_prefix('{')
            .and_then(|c| c.strip_suffix('}'))
            .unwrap_or(&self.cid_column)
    }
}

/// The CID inside whatever a column happens to hold.
///
/// **Found by porting a real subgraph.** DOUDOCHAIN_V2's `seriesMetaDataURI` holds no bare CID at
/// all: its values are `https://gateway.pinata.cloud/ipfs/QmR7XF…` and
/// `https://lime-basic-thrush-351.mypinata.cloud/ipfs/QmWyCg…`. A resolver that understood only bare
/// CIDs resolved nothing whatever, which is what the first run of that port showed.
///
/// **The location is discarded and only the content address is kept**, and that is a security
/// property rather than tidiness. The string comes from a log, so anybody who can emit an event can
/// choose it - fetching the URL it names would let a stranger point this process at any host they
/// like. Instead the CID is pulled out and asked for through *our* configured gateways, which is also
/// the honest reading of content addressing: the CID says what the document is, the host merely says
/// where somebody once kept a copy.
pub fn cid_from_any(s: &str) -> Option<&str> {
    let s = s.trim();
    // `https://<cid>.ipfs.<host>/…` - the subdomain gateway form.
    if let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    {
        if let Some((sub, _)) = rest.split_once(".ipfs.") {
            return looks_like_cid(sub).then_some(sub);
        }
    }
    // Any `…/ipfs/<cid>…` - the path gateway form, and also a bare `/ipfs/<cid>`.
    if let Some((_, after)) = s.rsplit_once("/ipfs/") {
        let cid = after.split(['/', '?', '#']).next().unwrap_or_default();
        return looks_like_cid(cid).then_some(cid);
    }
    // Kubo's API form, `…/api/v0/cat?arg=<cid>`.
    if let Some((_, after)) = s.rsplit_once("arg=") {
        let cid = after.split(['&', '#']).next().unwrap_or_default();
        return looks_like_cid(cid).then_some(cid);
    }
    let bare = s.strip_prefix("ipfs://").unwrap_or(s).trim_matches('/');
    looks_like_cid(bare).then_some(bare)
}

/// The CID a decoded event parameter names, whatever shape the contract chose to store it in.
///
/// Two shapes reach here and they are genuinely different things:
///
/// - `Value::Str` - a bare CID, an `ipfs://` URI, or a whole gateway URL. Handed to
///   [`cid_from_any`], which keeps only the content address and throws the host away.
/// - `Value::Bytes` of exactly 32 - the raw sha2-256 digest, which is what a `bytes32` column holds.
///   Re-framed as a CIDv0 by [`crate::cid::cid_v0_from_digest`]. Without this a nest reading such a
///   column resolves nothing and says nothing about it, which is the worst of the available
///   behaviours.
///
/// `Value::Hash32` is deliberately **not** accepted, and this is the same refusal the tier-3 call
/// path makes for the same reason: that variant is an *indexed dynamic* parameter, where the topic
/// holds `keccak(value)` rather than the value. Those 32 bytes look exactly like a digest and are
/// not one, so accepting them would mint a well-formed CID for a document that has never existed.
/// A `bytes32` is a fixed type and stays `Value::Bytes` whether indexed or not, so nothing real is
/// lost by refusing.
pub fn cid_from_value(v: &crate::registry::Value) -> Option<Cow<'_, str>> {
    match v {
        crate::registry::Value::Str(s) => cid_from_any(s).map(Cow::Borrowed),
        crate::registry::Value::Bytes(b) if b.len() == 32 => {
            let mut d = [0u8; 32];
            d.copy_from_slice(b);
            Some(Cow::Owned(crate::cid::cid_v0_from_digest(&d)))
        }
        _ => None,
    }
}

/// Cheap shape check. [`crate::cid::Cid::parse`] is the real one; this only decides whether a string
/// is worth handing to it.
fn looks_like_cid(s: &str) -> bool {
    (s.starts_with("Qm") && s.len() == 46 && s.chars().all(|c| c.is_ascii_alphanumeric()))
        || (s.starts_with('b') && s.len() >= 50 && s.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// The table shape a declaration produces, in the form `/tables`, MCP and `schema.json` consume.
pub fn schema(decls: &[IpfsDecl], timestamps: bool) -> Vec<crate::registry::TableSchema> {
    use crate::registry::{ColumnSchema, TableKind, TableSchema};
    let own = |name: &str, sol: &str, storage: &str| ColumnSchema {
        name: name.to_string(),
        sol_type: sol.to_string(),
        storage: storage.to_string(),
        indexed: false,
        components: Vec::new(),
    };
    let table = |name: &str, columns: Vec<ColumnSchema>| TableSchema {
        table: name.to_string(),
        alias: name.to_string(),
        kind: TableKind::Call,
        event: String::new(),
        topic0: String::new(),
        function: String::new(),
        selector: String::new(),
        columns,
    };
    let mut out = Vec::new();
    for d in decls {
        let mut columns = crate::registry::implicit_columns(timestamps);
        columns.push(own("cid", "string", "string"));
        columns.push(own("content", "string", "string"));
        // Verification is a property of the row, not of the run. A document accepted UNVERIFIED
        // (over the 256 KiB single-block limit) must be distinguishable from one whose bytes were
        // proven, or a consumer cannot tell which it is holding.
        columns.push(own("verified", "bool", "bool"));
        out.push(table(&d.name, columns));
        if let Some(rows) = &d.rows {
            let mut columns = crate::registry::implicit_columns(timestamps);
            columns.push(own("cid", "string", "string"));
            for name in &ROW_COLUMNS[1..] {
                columns.push(own(name, "u64", "u64"));
            }
            for c in &rows.columns {
                columns.push(own(&c.name, c.ty.name(), c.ty.storage()));
            }
            out.push(table(&rows.table, columns));
        }
    }
    out
}

/// The block a batch of resolutions belongs to. Grouped for the same reason `CallContext` is: every
/// row built from one block shares it, and passing four loose fields around invites transposing two.
pub struct BlockCtx<'a> {
    pub number: u64,
    pub hash: &'a str,
    pub timestamp: u64,
    pub timestamps: bool,
}

/// One resolved document, ready to store.
///
/// `slot` is its position within the block, assigned in a deterministic order (declarations in config
/// order, and within one declaration its distinct CIDs in first-seen row order), so two operators
/// produce the same keys and not merely the same content.
pub fn to_row(
    table: &str,
    cid: &str,
    content: &str,
    verified: bool,
    slot: usize,
    ctx: &BlockCtx<'_>,
) -> crate::registry::DecodedRow {
    use crate::registry::Value;
    crate::registry::DecodedRow {
        table: table.to_string(),
        params: vec![
            ("cid".to_string(), Value::Str(cid.to_string())),
            ("content".to_string(), Value::Str(content.to_string())),
            ("verified".to_string(), Value::Bool(verified)),
        ],
        block_number: ctx.number,
        block_hash: ctx.hash.to_string(),
        block_timestamp: ctx.timestamp,
        timestamps: ctx.timestamps,
        log_index: crate::registry::IPFS_ROW_LOG_INDEX_BASE + slot as u64,
        // A document has no transaction. Borrowing the block hash would be a lie that reads like data.
        tx_hash: String::new(),
        address: String::new(),
    }
}

/// Why a proven document's content did not fit its declared rows. It follows from the bytes alone, so
/// every operator refuses the same document for the same reason.
#[derive(Debug)]
pub struct RowsRefused(pub String);

impl std::fmt::Display for RowsRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "its rows do not fit the declaration: {}", self.0)
    }
}

impl std::error::Error for RowsRefused {}

/// Where one document's typed rows go.
pub struct RowPlace<'a> {
    pub cid: &'a str,
    pub document_log_index: u64,
    pub source_log_index: u64,
    pub first_log_index: u64,
}

/// A proven document's typed rows, one per element, or the reason there are none. A document is
/// refused whole: rows from part of one would roll up as if the rest had never been served.
pub fn typed_rows(
    rows: &IpfsRows,
    content: &str,
    place: &RowPlace<'_>,
    ctx: &BlockCtx<'_>,
) -> std::result::Result<Vec<crate::registry::DecodedRow>, RowsRefused> {
    use crate::registry::Value;
    use serde_json::Value as Json;
    let doc: Json = serde_json::from_str(content)
        .map_err(|e| RowsRefused(format!("the document is not JSON ({e})")))?;
    let elements: Vec<&Json> = match &doc {
        Json::Array(a) => a.iter().collect(),
        Json::Object(_) => vec![&doc],
        _ => {
            return Err(RowsRefused(
                "the document is neither an array nor an object".into(),
            ))
        }
    };
    if elements.len() as u64 > rows.max_rows {
        return Err(RowsRefused(format!(
            "{} elements, over max_rows {}",
            elements.len(),
            rows.max_rows
        )));
    }
    let mut out = Vec::with_capacity(elements.len());
    for (i, element) in elements.into_iter().enumerate() {
        let obj = element
            .as_object()
            .ok_or_else(|| RowsRefused(format!("element {i} is not an object")))?;
        let mut params = vec![
            ("cid".to_string(), Value::Str(place.cid.to_string())),
            (
                "document_log_index".to_string(),
                Value::U64(place.document_log_index),
            ),
            (
                "source_log_index".to_string(),
                Value::U64(place.source_log_index),
            ),
            ("element".to_string(), Value::U64(i as u64)),
        ];
        for c in &rows.columns {
            let raw = obj.get(&c.name).filter(|v| !v.is_null());
            let bad = || {
                RowsRefused(format!(
                    "element {i}: `{}` is {} where the declaration says {}",
                    c.name,
                    raw.map_or_else(|| "absent".to_string(), Json::to_string),
                    c.ty.name()
                ))
            };
            // An absent string or number is empty text, which a view reads as NULL. Every other type
            // has no honest empty value, so its absence refuses the document rather than inventing one.
            let value = match (c.ty, raw) {
                (RowType::String | RowType::Number, None) => Value::Str(String::new()),
                (RowType::String, Some(Json::String(s))) => Value::Str(s.clone()),
                (RowType::Number, Some(Json::Number(n))) => Value::Str(n.to_string()),
                (RowType::U64, Some(Json::Number(n))) => Value::U64(n.as_u64().ok_or_else(bad)?),
                (RowType::I64, Some(Json::Number(n))) => Value::I64(n.as_i64().ok_or_else(bad)?),
                (RowType::Bool, Some(Json::Bool(b))) => Value::Bool(*b),
                (RowType::Address, Some(Json::String(s))) => {
                    Value::Address(parse_address(s).ok_or_else(bad)?)
                }
                _ => return Err(bad()),
            };
            params.push((c.name.clone(), value));
        }
        out.push(crate::registry::DecodedRow {
            table: rows.table.clone(),
            params,
            block_number: ctx.number,
            block_hash: ctx.hash.to_string(),
            block_timestamp: ctx.timestamp,
            timestamps: ctx.timestamps,
            log_index: place.first_log_index + i as u64,
            tx_hash: String::new(),
            address: String::new(),
        });
    }
    Ok(out)
}

fn parse_address(s: &str) -> Option<[u8; 20]> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    if hex.len() != 40 {
        return None;
    }
    hex::decode(hex).ok()?.try_into().ok()
}

/// A stable hash over the declarations, folded into a nest's decode identity.
///
/// Two nests differing only in what they *resolve* are different decode versions, for the same reason
/// two differing in what they read are (`calls::decl_hash`).
pub fn decl_hash(decls: &[IpfsDecl]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for d in decls {
        h.update(d.name.as_bytes());
        h.update(b"\x1f");
        h.update(d.on.as_bytes());
        h.update(b"\x1f");
        h.update(d.column().as_bytes());
        // Only when set, so every nest declared before these keys keeps its content address. Length
        // prefixed because a `json_match` value is arbitrary text and may contain any separator.
        if let Some(key) = &d.cid_json_path {
            for part in std::iter::once(key).chain(d.json_match.iter().flat_map(|(k, v)| [k, v])) {
                h.update(b"\x1f");
                h.update((part.len() as u64).to_be_bytes());
                h.update(part.as_bytes());
            }
        }
        if let Some(rows) = &d.rows {
            let max = rows.max_rows.to_string();
            let keep = if rows.keep_content { "keep" } else { "drop" };
            h.update(b"\x1drows");
            let columns = rows
                .columns
                .iter()
                .flat_map(|c| [c.name.as_str(), c.ty.name()]);
            for part in [rows.table.as_str(), max.as_str(), keep]
                .into_iter()
                .chain(columns)
            {
                h.update(b"\x1f");
                h.update((part.len() as u64).to_be_bytes());
                h.update(part.as_bytes());
            }
        }
        h.update(b"\x1e");
    }
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The URL shapes a real subgraph actually put on chain, plus the ones a gateway list uses.
    ///
    /// The first two are verbatim from DOUDOCHAIN_V2's `seriesMetaDataURI` column - the values that
    /// made the first run of that port resolve nothing.
    #[test]
    fn a_cid_is_found_inside_every_shape_a_uri_column_holds() {
        const CID: &str = "QmR7XFYe7q9XLCFG3tzWFTHSc9iAnfS1Ra1pgSvJDhDERu";
        for s in [
            CID,
            &format!("ipfs://{CID}"),
            &format!("/ipfs/{CID}"),
            &format!("https://gateway.pinata.cloud/ipfs/{CID}"),
            &format!("https://lime-basic-thrush-351.mypinata.cloud/ipfs/{CID}"),
            &format!("https://ipfs.io/ipfs/{CID}?filename=x.json"),
            &format!("https://ipfs.thegraph.com/api/v0/cat?arg={CID}"),
            &format!("https://{CID}.ipfs.dweb.link/"),
        ] {
            assert_eq!(cid_from_any(s), Some(CID), "failed on {s}");
        }
    }

    /// A string that names no CID must yield none - the resolver then writes no row, rather than
    /// fetching whatever host a log author chose.
    #[test]
    fn a_uri_with_no_cid_in_it_resolves_to_nothing() {
        for s in [
            "",
            "not a uri",
            "https://example.com/metadata.json",
            "https://example.com/ipfs/not-a-cid",
            "ipfs://",
        ] {
            assert_eq!(cid_from_any(s), None, "must not accept {s:?}");
        }
    }

    #[test]
    fn a_declaration_names_its_table_its_source_and_its_column() {
        let d = IpfsDecl {
            name: "meta".into(),
            on: "nft__uri_set".into(),
            cid_column: "{uri}".into(),
            cid_json_path: None,
            json_match: BTreeMap::new(),
            rows: None,
        };
        assert!(d.validate().is_ok());
        assert_eq!(
            d.column(),
            "uri",
            "braces are optional on a column reference"
        );

        let mut bad = d.clone();
        bad.on = String::new();
        assert!(bad
            .validate()
            .unwrap_err()
            .to_string()
            .contains("needs `on`"));
    }

    /// The `bytes32` form, end to end through the same entry point the indexer uses. The vector is
    /// a real `SubgraphMetadataUpdated` payload from Arbitrum GNS whose document The Graph's gateway
    /// actually serves - see the matching test in `crate::cid`.
    #[test]
    fn a_bytes32_column_resolves_to_the_cid_it_names() {
        use crate::registry::Value;
        let d = hex::decode("6283b77fbdf020ce43a55149457f8ca1a3bec1ca60cd177163a7402e1a3945e4")
            .unwrap();
        assert_eq!(
            cid_from_value(&Value::Bytes(d.clone())).as_deref(),
            Some("QmUyD9wPyVCkDotF9oUoQHcMrhCMLU9Sqi6HY7BrttLPsq")
        );
        // Anything that is not exactly 32 bytes is not a sha2-256 digest, whatever else it may be.
        for n in [0usize, 20, 31, 33, 64] {
            assert_eq!(
                cid_from_value(&Value::Bytes(vec![0xab; n])),
                None,
                "{n} bytes must not be read as a digest"
            );
        }
    }

    /// The refusal that matters. `Value::Hash32` holds `keccak(value)` for an *indexed dynamic*
    /// parameter, not the value - so those 32 bytes have exactly the shape of a digest and are not
    /// one. Accepting them would mint a perfectly well-formed CID for a document that has never
    /// existed anywhere, and the fetch would simply time out against every gateway in turn with
    /// nothing to say about why.
    ///
    /// Written against the *same bytes* as the test above, so it cannot pass by accident: the only
    /// thing separating them is the variant, which is precisely the thing under test.
    #[test]
    fn an_indexed_dynamic_topic_is_refused_even_though_it_is_32_bytes() {
        use crate::registry::Value;
        let mut d = [0u8; 32];
        d.copy_from_slice(
            &hex::decode("6283b77fbdf020ce43a55149457f8ca1a3bec1ca60cd177163a7402e1a3945e4")
                .unwrap(),
        );
        assert!(cid_from_value(&Value::Bytes(d.to_vec())).is_some());
        assert_eq!(
            cid_from_value(&Value::Hash32(d)),
            None,
            "keccak(value) is not a content address and must never be fetched as one"
        );
    }

    /// The string forms still route through `cid_from_any` unchanged - the new variant must not have
    /// cost the old one anything.
    #[test]
    fn the_string_forms_still_resolve_through_the_value_entry_point() {
        use crate::registry::Value;
        for s in [
            "QmR7XFmkBnAsRwZTUt4kx4Fp5FEHwWKgSJvcGnnHNcvNAB",
            "ipfs://QmR7XFmkBnAsRwZTUt4kx4Fp5FEHwWKgSJvcGnnHNcvNAB",
            "https://gateway.pinata.cloud/ipfs/QmR7XFmkBnAsRwZTUt4kx4Fp5FEHwWKgSJvcGnnHNcvNAB",
        ] {
            assert_eq!(
                cid_from_value(&Value::Str(s.to_string())).as_deref(),
                Some("QmR7XFmkBnAsRwZTUt4kx4Fp5FEHwWKgSJvcGnnHNcvNAB"),
                "failed on {s}"
            );
        }
        assert_eq!(cid_from_value(&Value::Str("not a cid".into())), None);
        assert_eq!(cid_from_value(&Value::U64(42)), None);
    }

    /// A real `submitQoSPayload(bytes)` input, Gnosis tx `0xd977169c…8030` in block 48,231,985, as
    /// Edge & Node's QoS oracle posted it on 2026-09-13.
    const QOS_CALLDATA: &str = "53b734470000000000000000000000000000000000000000000000000000000000000020000000000000000000000000000000000000000000000000000000000000008d7b22746f706963223a2022676174657761795f696e64657865725f617474656d70745f716f735f355f6d696e757465735f70726f645f7633222c202268617368223a2022516d646863565470536a6d43427671674c396d366e617a52733233584245624a367a79676f6a5641716962376f61222c202274696d657374616d70223a20313738393331333430307d00000000000000000000000000000000000000";
    const QOS_CID: &str = "QmdhcVTpSjmCBvqgL9m6nazRs23XBEbJ6zygojVAqib7oa";
    const QOS_TOPIC: &str = "gateway_indexer_attempt_qos_5_minutes_prod_v3";

    fn qos_decl(json_match: &[(&str, &str)]) -> IpfsDecl {
        IpfsDecl {
            name: "qos_payload".into(),
            on: "data_edge__call_submit_qo_s_payload".into(),
            cid_column: "_payload".into(),
            cid_json_path: Some("hash".into()),
            json_match: json_match
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            rows: None,
        }
    }

    fn attempt_rows(max_rows: u64, keep_content: bool) -> IpfsRows {
        IpfsRows {
            table: "qos_indexer_attempt".into(),
            max_rows,
            keep_content,
            columns: [
                ("indexer_wallet", RowType::Address),
                ("chain", RowType::String),
                ("start_epoch", RowType::U64),
                ("query_count", RowType::U64),
                ("avg_indexer_latency_ms", RowType::Number),
                ("num_indexer_200_responses", RowType::Number),
            ]
            .into_iter()
            .map(|(name, ty)| RowColumn {
                name: name.into(),
                ty,
            })
            .collect(),
        }
    }

    /// Two elements in the shape of a real indexer-attempt payload, the second with a null count.
    const ATTEMPTS: &str = r#"[
        {"indexer_wallet":"0xF92F430DD8567B0D466358C79594AB58D919A6D4","chain":"base","start_epoch":1788652500,
         "query_count":2364,"avg_indexer_latency_ms":252.1945854484,"num_indexer_200_responses":2363,"stdev":1.5},
        {"indexer_wallet":"0x63c9dc729ba7a22bb8605216b24a34b902e5fe94","chain":"mainnet","start_epoch":1788652500,
         "query_count":7,"avg_indexer_latency_ms":0.0004477994,"num_indexer_200_responses":null}
    ]"#;

    fn row_ctx() -> BlockCtx<'static> {
        BlockCtx {
            number: 48_119_265,
            hash: "0xabc",
            timestamp: 1_789_000_000,
            timestamps: true,
        }
    }

    fn row_place() -> RowPlace<'static> {
        RowPlace {
            cid: QOS_CID,
            document_log_index: 625_000,
            source_log_index: 750_000,
            first_log_index: 626_000,
        }
    }

    /// A proven document becomes one typed row per element, keyed from where the plan placed it.
    #[test]
    fn a_document_becomes_one_typed_row_per_element() {
        let rows = typed_rows(&attempt_rows(4, true), ATTEMPTS, &row_place(), &row_ctx()).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.log_index).collect::<Vec<_>>(),
            [626_000, 626_001],
            "rows start where the document was allotted and run in element order"
        );
        let first = rows[0].to_json();
        assert_eq!(first["table"], "qos_indexer_attempt");
        assert_eq!(
            first["indexer_wallet"], "0xf92f430dd8567b0d466358c79594ab58d919a6d4",
            "an address is stored in one spelling whatever case the document used"
        );
        assert_eq!(first["query_count"], 2364);
        assert_eq!(
            first["avg_indexer_latency_ms"], "252.1945854484",
            "a number keeps its decimal text rather than a rounding chosen here"
        );
        assert_eq!(first["cid"], QOS_CID);
        assert_eq!(first["document_log_index"], 625_000);
        assert_eq!(first["source_log_index"], 750_000);
        assert_eq!(first["element"], 0);
        assert!(
            first.get("stdev").is_none(),
            "only declared keys become columns"
        );
        assert_eq!(rows[1].to_json()["avg_indexer_latency_ms"], "0.0004477994");
    }

    /// Reading an absent count as zero would chart a success rate of nought for a bucket the oracle said
    /// nothing about.
    #[test]
    fn an_absent_number_is_empty_text_never_a_zero() {
        let rows = typed_rows(&attempt_rows(4, true), ATTEMPTS, &row_place(), &row_ctx()).unwrap();
        assert_eq!(rows[1].to_json()["num_indexer_200_responses"], "");
    }

    /// Rows from part of a document would roll up as if the rest had never been served, so a document is
    /// refused whole, and for a reason its bytes alone decide.
    #[test]
    fn a_document_that_does_not_fit_its_declaration_is_refused_whole() {
        let refused = |rows: &IpfsRows, doc: &str| {
            typed_rows(rows, doc, &row_place(), &row_ctx())
                .unwrap_err()
                .to_string()
        };
        let four = attempt_rows(4, true);
        assert!(refused(&attempt_rows(1, true), ATTEMPTS).contains("over max_rows 1"));
        for (doc, column) in [
            (
                ATTEMPTS.replacen("\"query_count\":2364", "\"query_count\":\"2364\"", 1),
                "query_count",
            ),
            (
                ATTEMPTS.replacen("\"query_count\":7,", "\"query_count\":7.5,", 1),
                "query_count",
            ),
            (
                ATTEMPTS.replacen("\"start_epoch\":1788652500,", "", 1),
                "start_epoch",
            ),
            (
                ATTEMPTS.replacen("0x63c9dc729ba7a22bb8605216b24a34b902e5fe94", "0x63c9", 1),
                "indexer_wallet",
            ),
            (
                ATTEMPTS.replacen("\"chain\":\"base\"", "\"chain\":8453", 1),
                "chain",
            ),
        ] {
            assert!(
                refused(&four, &doc).contains(column),
                "{column}: {}",
                refused(&four, &doc)
            );
        }
        assert!(refused(&four, "not json").contains("not JSON"));
        assert!(refused(&four, "[1]").contains("element 0 is not an object"));
        assert!(refused(&four, "7").contains("neither an array nor an object"));
    }

    /// A typed row has to rebuild from what the hot store holds, or the reorg path and the restart path
    /// disagree about the same row.
    #[test]
    fn a_typed_row_rebuilds_from_storage() {
        let decl = IpfsDecl {
            rows: Some(attempt_rows(4, true)),
            ..qos_decl(&[])
        };
        let tables = schema(std::slice::from_ref(&decl), true);
        assert_eq!(
            tables.iter().map(|t| t.table.as_str()).collect::<Vec<_>>(),
            ["qos_payload", "qos_indexer_attempt"],
            "the typed table is declared beside the document table"
        );
        let rows = typed_rows(
            decl.rows.as_ref().unwrap(),
            ATTEMPTS,
            &row_place(),
            &row_ctx(),
        )
        .unwrap();
        for row in &rows {
            let rebuilt =
                crate::registry::DecodedRow::from_stored(&row.to_json(), &tables[1]).unwrap();
            assert_eq!(rebuilt.to_json(), row.to_json());
        }
    }

    /// Every nest declared before typed rows keeps its address, and each thing that changes what the rows
    /// hold or where they go changes it.
    #[test]
    fn typed_rows_enter_the_identity_only_when_declared() {
        let plain = qos_decl(&[("topic", QOS_TOPIC)]);
        let without = decl_hash(std::slice::from_ref(&plain));
        let with = |rows: IpfsRows| {
            decl_hash(&[IpfsDecl {
                rows: Some(rows),
                ..plain.clone()
            }])
        };
        let base = with(attempt_rows(4, true));
        assert_ne!(base, without);
        assert_ne!(with(attempt_rows(5, true)), base, "max_rows moves row keys");
        assert_ne!(
            with(attempt_rows(4, false)),
            base,
            "dropping content changes what is stored"
        );
        let mut retyped = attempt_rows(4, true);
        retyped.columns[3].ty = RowType::Number;
        assert_ne!(
            with(retyped),
            base,
            "a column's type changes what is stored"
        );
        let mut renamed = attempt_rows(4, true);
        renamed.table = "attempts".into();
        assert_ne!(with(renamed), base, "the table is part of the declaration");
    }

    #[test]
    fn a_rows_declaration_that_cannot_be_honoured_is_refused_at_load() {
        let refused = |rows: IpfsRows| {
            IpfsDecl {
                rows: Some(rows),
                ..qos_decl(&[])
            }
            .validate()
            .unwrap_err()
            .to_string()
        };
        assert!(refused(IpfsRows {
            max_rows: 0,
            ..attempt_rows(1, true)
        })
        .contains("max_rows"));
        assert!(refused(IpfsRows {
            max_rows: 124_001,
            ..attempt_rows(1, true)
        })
        .contains("max_rows"));
        assert!(refused(IpfsRows {
            table: "qos_payload".into(),
            ..attempt_rows(1, true)
        })
        .contains("rows.table"));
        assert!(refused(IpfsRows {
            columns: Vec::new(),
            ..attempt_rows(1, true)
        })
        .contains("empty"));
        for name in ["block_number", "cid", "element", "bad-name"] {
            let mut rows = attempt_rows(1, true);
            rows.columns[0].name = name.into();
            assert!(refused(rows).contains(name), "{name}");
        }
        let mut twice = attempt_rows(1, true);
        twice.columns[1].name = "indexer_wallet".into();
        assert!(refused(twice).contains("declared once"));
        IpfsDecl {
            rows: Some(attempt_rows(124_000, true)),
            ..qos_decl(&[])
        }
        .validate()
        .expect("the whole room is a valid max_rows");
    }

    #[test]
    fn a_rows_declaration_reads_from_toml() {
        let toml = |ty: &str| {
            format!(
                "name = \"qos_indexer_payload\"\non = \"data_edge__call_submit_qo_s_payload\"\n\
                 cid_column = \"_payload\"\ncid_json_path = \"hash\"\n\n[rows]\ntable = \"qos_indexer_attempt\"\n\
                 max_rows = 4096\nkeep_content = false\n\
                 columns = [{{ name = \"indexer_wallet\", type = \"address\" }}, {{ name = \"query_count\", type = \"{ty}\" }}]\n"
            )
        };
        let d: IpfsDecl = ::toml::from_str(&toml("number")).unwrap();
        let rows = d.rows.expect("the rows table parses");
        assert_eq!(rows.max_rows, 4096);
        assert!(!rows.keep_content);
        assert_eq!(rows.columns[1].ty, RowType::Number);
        assert!(
            ::toml::from_str::<IpfsDecl>(&toml("float")).is_err(),
            "an unknown column type is refused, not read as text"
        );
    }

    /// The `bytes` argument of the real calldata, cut out by its own ABI offset and length rather than
    /// re-typed, so the fixture is the bytes the decoder hands the resolver.
    fn qos_payload_bytes() -> Vec<u8> {
        let input = hex::decode(QOS_CALLDATA).unwrap();
        let args = &input[4..];
        let offset = usize::from_be_bytes(args[24..32].try_into().unwrap());
        let len = usize::from_be_bytes(args[offset + 24..offset + 32].try_into().unwrap());
        args[offset + 32..offset + 32 + len].to_vec()
    }

    /// **The QoS oracle's CID is inside JSON, and `cid_from_value` never saw it.** A 141-byte payload
    /// is neither a string nor a 32-byte digest, so the plain declaration counts one unreadable row
    /// and resolves nothing, which is what every such nest silently did before.
    #[test]
    fn the_qos_oracle_calldata_names_its_payload_through_cid_json_path() {
        use crate::registry::Value;
        let bytes = qos_payload_bytes();
        assert_eq!(bytes.len(), 141);

        let plain = IpfsDecl {
            cid_json_path: None,
            ..qos_decl(&[])
        };
        assert_eq!(plain.cids_in(&Value::Bytes(bytes.clone())), (vec![], 1));

        let d = qos_decl(&[("topic", QOS_TOPIC)]);
        assert_eq!(
            d.cids_in(&Value::Bytes(bytes.clone())),
            (vec![QOS_CID.to_string()], 0)
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            d.cids_in(&Value::Str(text)),
            (vec![QOS_CID.to_string()], 0),
            "a string column holding the same JSON must read the same"
        );
    }

    /// An array names one document per element, `json_match` declines the rest before any fetch, and
    /// what cannot be read is counted rather than dropped.
    #[test]
    fn json_match_declines_what_it_does_not_match_and_counts_what_it_cannot_read() {
        use crate::registry::Value;
        let other = "QmcySPs9y7a4wGYxCtbdNrt9kjryce9iYguRVe6GdKvZw5";
        let array = format!(
            r#"[{{"topic":"{QOS_TOPIC}","hash":"{QOS_CID}"}},
               {{"topic":"gateway_query_result_qos_5_minutes_prod_v3","hash":"{other}"}},
               {{"topic":"{QOS_TOPIC}"}},
               "not an object"]"#
        );
        let d = qos_decl(&[("topic", QOS_TOPIC)]);
        assert_eq!(
            d.cids_in(&Value::Str(array.clone())),
            (vec![QOS_CID.to_string()], 2),
            "the query-result element is declined, the keyless and non-object elements are unreadable"
        );
        assert_eq!(
            qos_decl(&[]).cids_in(&Value::Str(array)),
            (vec![QOS_CID.to_string(), other.to_string()], 2),
            "with no json_match every element that names a CID is wanted"
        );
        for bad in [
            Value::Str("not json".into()),
            Value::Bytes(vec![0xff, 0xfe]),
            Value::U64(7),
        ] {
            assert_eq!(
                d.cids_in(&bad),
                (vec![], 1),
                "{bad:?} names nothing and must say so"
            );
        }
    }

    #[test]
    fn cid_json_path_is_a_key_and_json_match_needs_it() {
        assert!(qos_decl(&[("topic", QOS_TOPIC)]).validate().is_ok());
        for path in ["", "payload.hash", "$.hash", "items[0]"] {
            let d = IpfsDecl {
                cid_json_path: Some(path.into()),
                ..qos_decl(&[])
            };
            assert!(
                d.validate()
                    .unwrap_err()
                    .to_string()
                    .contains("one top-level JSON key"),
                "{path:?} must be refused, not quietly matched against nothing"
            );
        }
        let d = IpfsDecl {
            cid_json_path: None,
            ..qos_decl(&[("topic", QOS_TOPIC)])
        };
        assert!(d
            .validate()
            .unwrap_err()
            .to_string()
            .contains("needs `cid_json_path`"));
    }

    /// Every nest declared before these keys existed must keep its content address, and the new keys
    /// must move it when they are set. Written against the layout the hash had before, not against
    /// `decl_hash` itself, so an unconditional append cannot pass.
    #[test]
    fn the_json_keys_enter_the_identity_only_when_set() {
        use sha2::{Digest, Sha256};
        let plain = IpfsDecl {
            cid_json_path: None,
            ..qos_decl(&[])
        };
        let mut h = Sha256::new();
        h.update(b"qos_payload\x1fdata_edge__call_submit_qo_s_payload\x1f_payload\x1e");
        let before: [u8; 32] = h.finalize().into();
        assert_eq!(decl_hash(std::slice::from_ref(&plain)), before);

        let json = qos_decl(&[]);
        let matched = qos_decl(&[("topic", QOS_TOPIC)]);
        let other = qos_decl(&[("topic", "gateway_query_result_qos_5_minutes_prod_v3")]);
        let ids = [plain, json, matched, other].map(|d| decl_hash(&[d]));
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                assert_ne!(
                    ids[i], ids[j],
                    "declarations {i} and {j} must be different nests"
                );
            }
        }
    }
}
