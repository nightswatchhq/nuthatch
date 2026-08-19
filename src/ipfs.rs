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
}

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
        Ok(())
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
    decls
        .iter()
        .map(|d| {
            let mut columns = crate::registry::implicit_columns(timestamps);
            let own = |name: &str, sol: &str, storage: &str| ColumnSchema {
                name: name.to_string(),
                sol_type: sol.to_string(),
                storage: storage.to_string(),
                indexed: false,
            };
            columns.push(own("cid", "string", "string"));
            columns.push(own("content", "string", "string"));
            // Verification is a property of the row, not of the run. A document accepted UNVERIFIED
            // (over the 256 KiB single-block limit) must be distinguishable from one whose bytes were
            // proven, or a consumer cannot tell which it is holding.
            columns.push(own("verified", "bool", "bool"));
            TableSchema {
                table: d.name.clone(),
                alias: d.name.clone(),
                kind: TableKind::Call,
                event: String::new(),
                topic0: String::new(),
                function: String::new(),
                selector: String::new(),
                columns,
            }
        })
        .collect()
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
}
