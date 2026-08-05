//! The derivation reuse key (RFC-0033 slice 1) - a stable identity for *one derivation*, below the
//! nest's NID.
//!
//! Content addressing means one edited character changes the nest's identity, so without a finer key
//! every edit re-indexes everything. This module computes the finer key. It does **not** reuse
//! anything yet: slice 1 is the key alone, because a reuse mechanism built on an unsound key is worse
//! than no reuse at all.
//!
//! ## The one failure mode that matters
//!
//! A **missed** match costs a recompute. A **false** match ships wrong data silently and is
//! discovered by a user. Every choice here is shaped by that asymmetry, which is why the matcher is
//! strictly syntactic and why anything uncertain hashes as *different*.
//!
//! ## Why DuckDB's own parser
//!
//! Canonicalisation runs over `json_serialize_sql` - the AST of the engine that will actually execute
//! the query. A second parser could disagree with DuckDB about what a statement means, and a
//! disagreement in the permissive direction is a false match. Using the engine's own parse makes that
//! class of error unreachable rather than unlikely.
//!
//! It also satisfies §2.2 structurally: the serialized AST *is* version-shaped, so a DuckDB release
//! that changes how a statement parses changes the key by construction, rather than by us remembering
//! to bump something. The explicit engine version stays in the key as well, for the changes that
//! alter *evaluation* without altering the parse (DuckDB 1.4's CTE materialisation switch is exactly
//! that).
//!
//! **A parse failure falls back to the raw text.** That can only cost a match, never invent one.

use anyhow::{Context, Result};
use duckdb::Connection;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Bump to invalidate every key globally (RFC-0033 §8). Changing *what goes into* a key is a
/// migration, not a patch: entries computed under an older meaning must never be matched against
/// entries computed under a newer one.
pub const CACHE_FORMAT_VERSION: u32 = 1;

/// Fields the AST carries that describe the *source text* rather than the query.
///
/// `query_location` is a byte offset into the original SQL, so it is the only thing that differs
/// between two statements that vary by whitespace, indentation or comments. Dropping it *is* §3's
/// whitespace-and-comments normalisation - no tokenizer, no string-literal edge cases, and no risk of
/// stripping a `--` that lives inside a string.
const POSITIONAL_FIELDS: &[&str] = &["query_location"];

/// What a nest's SQL reads, bound to identity rather than to a name (RFC-0033 §2.1).
///
/// The trap this exists for: a view reading `usdc__transfer` must key on *what that table is*, not on
/// the fourteen characters naming it. Re-`init` a nest against a different contract under the same
/// alias and the SQL is byte-identical while the data is unrelated. PostgreSQL's
/// `pg_stat_statements` hashes table OIDs rather than names for the same reason.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceIdentity {
    /// The table name as the SQL refers to it. Present so a key is debuggable; **not** what makes it
    /// sound - everything below it is.
    pub table: String,
    pub chain_id: u64,
    pub contract: String,
    /// The event signature this table decodes, e.g. `Transfer(address,address,uint256)`.
    pub event_signature: String,
    /// Hash of the ABI the decode was generated from.
    pub abi_hash: String,
    pub schema_version: u32,
}

impl SourceIdentity {
    fn feed(&self, h: &mut Sha256) {
        for part in [
            self.table.as_str(),
            &self.chain_id.to_string(),
            self.contract.as_str(),
            self.event_signature.as_str(),
            self.abi_hash.as_str(),
            &self.schema_version.to_string(),
        ] {
            h.update((part.len() as u64).to_le_bytes()); // length-prefixed: no field can bleed into the next
            h.update(part.as_bytes());
        }
    }
}

/// Whether the range a derivation covers is settled or still provisional (RFC-0033 §2.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finality {
    /// Entirely below the finality watermark. Re-executable and safe to key on the range alone.
    Final,
    /// Includes provisional blocks, so the **block hash** enters the key and a reorg invalidates it
    /// by construction rather than by us noticing.
    Provisional { tip_hash: String },
}

/// The canonical form of a statement, and how it was arrived at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalPlan {
    /// Canonicalised from the engine's own AST.
    Ast(String),
    /// The parse was unavailable, so the raw text stands in. Sound but coarse: two formattings of one
    /// query will not match. Never the reverse.
    RawText(String),
}

impl CanonicalPlan {
    fn as_str(&self) -> &str {
        match self {
            CanonicalPlan::Ast(s) | CanonicalPlan::RawText(s) => s,
        }
    }

    /// Whether this plan was canonicalised, as opposed to falling back to raw text. A nest whose
    /// views all fall back still works; it just grafts less, and an operator should be able to see
    /// that rather than wonder why nothing matches.
    pub fn is_canonical(&self) -> bool {
        matches!(self, CanonicalPlan::Ast(_))
    }
}

/// Canonicalise `sql` for the reuse key (RFC-0033 §3).
///
/// Applies exactly two normalisations, both provably safe:
///
/// 1. **Whitespace and comments** - by dropping `query_location`, which is the only field they touch.
/// 2. **Alias α-renaming** - table aliases are rewritten to positional names, and references through
///    them are rewritten to match. Only names that are *declared as aliases* are touched, so a real
///    table name can never be renamed into collision with another.
///
/// Everything else in §3's unsafe list stays significant, and needs no work to stay so: the AST is
/// already type-aware (`5/2` carries `INTEGER` where `5/2.0` carries `DECIMAL`), already ordered, and
/// already distinguishes `DISTINCT`.
pub fn canonical_plan(conn: &Connection, sql: &str) -> CanonicalPlan {
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let Ok(raw) = conn.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
        r.get::<_, String>(0)
    }) else {
        return CanonicalPlan::RawText(sql.trim().to_string());
    };
    let Ok(mut ast) = serde_json::from_str::<Value>(&raw) else {
        return CanonicalPlan::RawText(sql.trim().to_string());
    };
    // DuckDB reports a parse failure in-band rather than as an error.
    if ast.get("error").and_then(Value::as_bool) != Some(false) {
        return CanonicalPlan::RawText(sql.trim().to_string());
    }

    strip_positional(&mut ast);
    let aliases = collect_aliases(&ast);
    rename_aliases(&mut ast, &aliases);

    // `serde_json` preserves object key order as parsed, and DuckDB emits it deterministically for a
    // given version - which is the version already in the key.
    CanonicalPlan::Ast(ast.to_string())
}

/// Remove source-position fields everywhere in the tree.
fn strip_positional(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for f in POSITIONAL_FIELDS {
                map.remove(*f);
            }
            for (_, child) in map.iter_mut() {
                strip_positional(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_positional),
        _ => {}
    }
}

/// Every non-empty `alias` declared on a table reference, in traversal order.
///
/// **Only `alias` fields on things that are tables.** An expression alias (`SELECT x AS y`) also uses
/// the key `alias`, but renaming one would change the *output column name*, which is observable - so
/// this is deliberately narrow, and anything it does not recognise simply stays significant.
fn collect_aliases(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(v, &mut |obj| {
        if is_table_ref(obj) {
            if let Some(a) = obj.get("alias").and_then(Value::as_str) {
                if !a.is_empty() && !out.iter().any(|s: &String| s == a) {
                    out.push(a.to_string());
                }
            }
        }
    });
    out
}

/// A node that introduces a table into scope, and can therefore carry a table alias.
fn is_table_ref(obj: &serde_json::Map<String, Value>) -> bool {
    matches!(
        obj.get("type").and_then(Value::as_str),
        Some("BASE_TABLE") | Some("SUBQUERY") | Some("TABLE_FUNCTION")
    )
}

/// Rewrite declared table aliases to positional names, and every reference through them.
///
/// A `COLUMN_REF`'s `column_names` is a qualified path: `["a", "x"]` for `a.x`. The leading element is
/// rewritten **only** when it names a declared alias - so `["t", "x"]`, where `t` is the real table,
/// is untouched. Getting that wrong in the other direction would rename two different tables to the
/// same thing, which is the false match this whole module exists to prevent.
fn rename_aliases(v: &mut Value, aliases: &[String]) {
    if aliases.is_empty() {
        return;
    }
    let renamed = |name: &str| -> Option<String> {
        aliases
            .iter()
            .position(|a| a == name)
            .map(|i| format!("__a{i}"))
    };
    walk_mut(v, &mut |obj| {
        let table_ref = is_table_ref(obj);
        if table_ref {
            if let Some(new) = obj.get("alias").and_then(Value::as_str).and_then(&renamed) {
                obj.insert("alias".into(), Value::String(new));
            }
        }
        if obj.get("type").and_then(Value::as_str) == Some("COLUMN_REF") {
            if let Some(Value::Array(parts)) = obj.get_mut("column_names") {
                // Only the qualifier, and only when the path is qualified at all.
                if parts.len() > 1 {
                    if let Some(new) = parts[0].as_str().and_then(&renamed) {
                        parts[0] = Value::String(new);
                    }
                }
            }
        }
    });
}

fn walk(v: &Value, f: &mut impl FnMut(&serde_json::Map<String, Value>)) {
    match v {
        Value::Object(map) => {
            f(map);
            map.values().for_each(|c| walk(c, f));
        }
        Value::Array(items) => items.iter().for_each(|c| walk(c, f)),
        _ => {}
    }
}

fn walk_mut(v: &mut Value, f: &mut impl FnMut(&mut serde_json::Map<String, Value>)) {
    match v {
        Value::Object(map) => {
            f(map);
            map.values_mut().for_each(|c| walk_mut(c, f));
        }
        Value::Array(items) => items.iter_mut().for_each(|c| walk_mut(c, f)),
        _ => {}
    }
}

/// The engine that will evaluate a derivation, and its version (RFC-0033 §2.2).
///
/// Not bookkeeping - a correctness requirement. **DuckDB changed CTE semantics at 1.4**, making them
/// materialized by default where they had been inlined; PostgreSQL flipped the same switch the other
/// way at 12. Same SQL, different version, different evaluation and potentially different results. A
/// key without this is unsound across our *own* upgrades, which is the worst place to be unsound
/// because it is discovered in production rather than in CI.
pub fn engine_version(conn: &Connection) -> String {
    conn.query_row("SELECT version()", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|_| "duckdb-unknown".to_string())
}

/// Everything a derivation's identity depends on. Assembled explicitly so a reader can see the whole
/// key in one place rather than inferring it from a hash function.
#[derive(Debug, Clone)]
pub struct Derivation {
    /// This derivation's name within the nest (a `views/*.sql` stem).
    pub name: String,
    pub plan: CanonicalPlan,
    /// The reuse keys of the derivations this one reads. Transitive by construction (RFC-0033 §1):
    /// a change upstream propagates here and nowhere else. Sorted before hashing.
    pub input_keys: Vec<String>,
    /// The resolved identity of every decoded source this reads. Sorted before hashing.
    pub sources: Vec<SourceIdentity>,
    /// The block range covered, inclusive.
    pub range: (u64, u64),
    pub engine: String,
    pub finality: Finality,
}

impl Derivation {
    /// The reuse key (RFC-0033 §2).
    ///
    /// Every component is length-prefixed so no two different field splits can produce the same
    /// bytes - the classic concatenation collision, and a collision here is a false match.
    pub fn reuse_key(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"nuthatch-reuse-key-v1\0");
        h.update(CACHE_FORMAT_VERSION.to_le_bytes());

        fn feed(h: &mut Sha256, s: &str) {
            h.update((s.len() as u64).to_le_bytes());
            h.update(s.as_bytes());
        }
        feed(&mut h, self.plan.as_str());

        let mut inputs = self.input_keys.clone();
        inputs.sort();
        h.update((inputs.len() as u64).to_le_bytes());
        inputs.iter().for_each(|k| feed(&mut h, k));

        let mut sources = self.sources.clone();
        sources.sort();
        h.update((sources.len() as u64).to_le_bytes());
        for s in &sources {
            s.feed(&mut h);
        }

        h.update(self.range.0.to_le_bytes());
        h.update(self.range.1.to_le_bytes());
        feed(&mut h, &self.engine);
        match &self.finality {
            Finality::Final => feed(&mut h, "final"),
            // The block hash, so a reorg invalidates the key by construction.
            Finality::Provisional { tip_hash } => {
                feed(&mut h, "provisional");
                feed(&mut h, tip_hash);
            }
        }
        hex::encode(h.finalize())
    }
}

/// Open a connection suitable for canonicalisation. No data is attached: parsing needs no catalogue.
pub fn parser_connection() -> Result<Connection> {
    Connection::open_in_memory().context("opening DuckDB to canonicalise a derivation")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(sql: &str) -> CanonicalPlan {
        canonical_plan(&parser_connection().unwrap(), sql)
    }

    fn src(table: &str, contract: &str) -> SourceIdentity {
        SourceIdentity {
            table: table.into(),
            chain_id: 1,
            contract: contract.into(),
            event_signature: "Transfer(address,address,uint256)".into(),
            abi_hash: "ab".repeat(32),
            schema_version: 1,
        }
    }

    fn derivation(sql: &str, sources: Vec<SourceIdentity>) -> Derivation {
        Derivation {
            name: "v".into(),
            plan: plan(sql),
            input_keys: vec![],
            sources,
            range: (1, 100),
            engine: "duckdb-test".into(),
            finality: Finality::Final,
        }
    }

    /// RFC-0033 §3, safe normalisation 1. Formatting is not meaning.
    #[test]
    fn whitespace_and_comments_do_not_change_the_plan() {
        let a = plan("SELECT x FROM t");
        for equivalent in [
            "SELECT   x\n   FROM   t",
            "-- a leading note\nSELECT x FROM t",
            "SELECT x /* inline */ FROM t",
            "\n\tSELECT x FROM t\n\n",
        ] {
            assert!(a.is_canonical(), "the fixture must actually parse");
            assert_eq!(
                a,
                plan(equivalent),
                "formatting changed the plan: {equivalent:?}"
            );
        }
    }

    /// RFC-0033 §3, safe normalisation 2, and slice 1's stated acceptance.
    #[test]
    fn alias_renaming_does_not_change_the_plan() {
        assert_eq!(
            plan("SELECT a.x FROM t AS a"),
            plan("SELECT b.x FROM t AS b"),
            "α-equivalent views must hash identically"
        );
        assert_eq!(
            plan("SELECT p.x, q.y FROM t AS p, u AS q"),
            plan("SELECT q.x, p.y FROM t AS q, u AS p"),
            "positional renaming must follow the declaration order, not the letter"
        );
    }

    /// The other half of α-renaming, and the dangerous half: a **real table name** must never be
    /// rewritten. Renaming two different tables to the same positional name is exactly the false
    /// match this module exists to prevent.
    #[test]
    fn a_real_table_name_is_never_renamed() {
        // The load-bearing case, and it needs both halves to be dangerous: **an alias is declared**
        // (so renaming actually runs - with none declared the whole pass early-returns) **and** an
        // unaliased qualifier is present. The two queries differ only in which real table the first
        // projection qualifies against. A renamer that rewrote every qualifier rather than only the
        // declared aliases collapses these into one plan: two different queries, one key, wrong data
        // served silently.
        assert_ne!(
            plan("SELECT t.x, c.y FROM t, u AS c"),
            plan("SELECT u.x, c.y FROM t, u AS c"),
            "an unaliased qualifier was rewritten - this is a false match, the one outcome that \
             must be impossible"
        );
        assert_ne!(
            plan("SELECT t.x FROM t"),
            plan("SELECT u.x FROM u"),
            "two different tables must not collapse to one plan"
        );
        // An alias that happens to share a name with a different table stays distinguishable.
        assert_ne!(plan("SELECT a.x FROM t AS a"), plan("SELECT a.x FROM a"));
    }

    /// RFC-0033 §3's unsafe list, verified rather than assumed. Each of these *must* stay significant.
    #[test]
    fn the_unsafe_normalisations_all_stay_significant() {
        for (a, b, why) in [
            (
                "SELECT x, y FROM t",
                "SELECT y, x FROM t",
                "projection order is positional",
            ),
            (
                "SELECT 5/2",
                "SELECT 5/2.0",
                "integer division: 2 versus 2.5",
            ),
            (
                "SELECT DISTINCT x FROM t",
                "SELECT x FROM t",
                "bag versus set is where equivalence becomes undecidable",
            ),
            (
                "SELECT x FROM t ORDER BY x",
                "SELECT x FROM t",
                "ORDER BY changes what anything order-dependent downstream sees",
            ),
            (
                "SELECT x FROM t WHERE a = 1 AND b = 2",
                "SELECT x FROM t WHERE b = 2 AND a = 1",
                "SQL guarantees no short-circuit, so reordering can change whether it throws",
            ),
        ] {
            assert_ne!(plan(a), plan(b), "{why}");
        }
    }

    /// Two identical-looking views over *different contracts* must not share a key (RFC-0033 §2.1).
    ///
    /// This is the correctness trap the whole section exists for. The SQL text is byte-identical; only
    /// the resolved source differs, and it has to be enough.
    #[test]
    fn identical_sql_over_a_different_contract_keys_differently() {
        let sql = "SELECT count(*) FROM usdc__transfer";
        let usdc = derivation(sql, vec![src("usdc__transfer", "0xa0b8")]);
        let other = derivation(sql, vec![src("usdc__transfer", "0xdeadbeef")]);

        assert_eq!(usdc.plan, other.plan, "the text really is identical");
        assert_ne!(
            usdc.reuse_key(),
            other.reuse_key(),
            "a re-init against a different contract under the same table name must not reuse data - \
             binding to a name rather than to identity is how pg_stat_statements learned to hash OIDs"
        );
    }

    /// Every other component of the key must move it (RFC-0033 §2).
    #[test]
    fn every_key_component_is_load_bearing() {
        let base = derivation("SELECT 1", vec![src("t", "0xaa")]);
        let key = base.reuse_key();

        let mut engine = base.clone();
        engine.engine = "duckdb-v1.4.0".into();
        assert_ne!(
            engine.reuse_key(),
            key,
            "DuckDB changed CTE semantics at 1.4"
        );

        let mut range = base.clone();
        range.range = (1, 101);
        assert_ne!(
            range.reuse_key(),
            key,
            "the covered range must be in the key"
        );

        let mut provisional = base.clone();
        provisional.finality = Finality::Provisional {
            tip_hash: "0xabc".into(),
        };
        assert_ne!(
            provisional.reuse_key(),
            key,
            "above-finality data must key on the block hash so a reorg invalidates it"
        );

        let mut reorged = provisional.clone();
        reorged.finality = Finality::Provisional {
            tip_hash: "0xdef".into(),
        };
        assert_ne!(
            reorged.reuse_key(),
            provisional.reuse_key(),
            "a different tip hash is a different chain"
        );

        let mut upstream = base.clone();
        upstream.input_keys = vec!["deadbeef".into()];
        assert_ne!(
            upstream.reuse_key(),
            key,
            "a change upstream must propagate - the key is transitive"
        );

        let mut abi = base.clone();
        abi.sources[0].abi_hash = "cd".repeat(32);
        assert_ne!(
            abi.reuse_key(),
            key,
            "decoding differently is a different derivation"
        );
    }

    /// Input order is not information: the same inputs listed differently are the same derivation.
    #[test]
    fn input_and_source_order_do_not_change_the_key() {
        let mut a = derivation("SELECT 1", vec![src("t", "0xaa"), src("u", "0xbb")]);
        a.input_keys = vec!["one".into(), "two".into()];
        let mut b = a.clone();
        b.sources.reverse();
        b.input_keys.reverse();
        assert_eq!(a.reuse_key(), b.reuse_key());
    }

    /// Length-prefixing: no two different field splits may produce the same bytes.
    #[test]
    fn adjacent_fields_cannot_bleed_into_each_other() {
        let mut a = derivation("SELECT 1", vec![src("ab", "0xcc")]);
        let mut b = a.clone();
        a.sources[0].table = "ab".into();
        a.sources[0].contract = "c".into();
        b.sources[0].table = "a".into();
        b.sources[0].contract = "bc".into();
        assert_ne!(
            a.reuse_key(),
            b.reuse_key(),
            "concatenation without length prefixes collides, and a collision is a false match"
        );
    }

    /// A statement the engine cannot parse falls back to raw text - sound, just coarse.
    #[test]
    fn an_unparseable_statement_falls_back_rather_than_failing() {
        let p = plan("this is not sql at all");
        assert!(!p.is_canonical(), "a parse failure must be visible");
        assert_eq!(p, plan("this is not sql at all"), "and still deterministic");
        // Coarse in exactly the expected direction: formatting now matters.
        assert_ne!(p, plan("this  is not sql at all"));
    }
}
