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
