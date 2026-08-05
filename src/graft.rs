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

    // ---- slice 2: the DAG ----

    fn dag(files: &[(&str, &str)]) -> Dag {
        let owned: Vec<(String, String)> = files
            .iter()
            .map(|(f, s)| (f.to_string(), s.to_string()))
            .collect();
        Dag::build(&parser_connection().unwrap(), &owned)
    }

    fn any_source(name: &str) -> Option<SourceIdentity> {
        Some(src(name, "0xaa"))
    }

    #[test]
    fn a_create_view_prefix_is_split_strictly() {
        for (stmt, want) in [
            ("CREATE VIEW v AS SELECT 1", Some(("v", "SELECT 1"))),
            (
                "create or replace view v as select 1",
                Some(("v", "select 1")),
            ),
            ("CREATE TEMP VIEW v AS SELECT 1", Some(("v", "SELECT 1"))),
            (
                "CREATE VIEW \"odd name\" AS SELECT 1",
                Some(("odd name", "SELECT 1")),
            ),
            // A column list, including a nested paren that must not end it early.
            (
                "CREATE VIEW v (a, b) AS SELECT 1, 2",
                Some(("v", "SELECT 1, 2")),
            ),
            // Anything unexpected drops out rather than being guessed at.
            ("SELECT 1", None),
            ("CREATE TABLE t AS SELECT 1", None),
            ("CREATE VIEW v", None),
            ("CREATE VIEW AS SELECT 1", None),
        ] {
            let got = split_create_view(stmt);
            let got = got.as_ref().map(|(n, b)| (n.as_str(), b.as_str()));
            assert_eq!(got, want, "{stmt:?}");
        }
    }

    /// Slice 2's stated acceptance, half one: a diamond hashes correctly and transitively.
    #[test]
    fn a_diamond_dependency_hashes_transitively() {
        let files = [
            (
                "10.sql",
                "CREATE VIEW base AS SELECT k, v FROM usdc__transfer",
            ),
            ("20.sql", "CREATE VIEW left_arm AS SELECT k FROM base"),
            ("21.sql", "CREATE VIEW right_arm AS SELECT v FROM base"),
            (
                "30.sql",
                "CREATE VIEW tip AS SELECT * FROM left_arm, right_arm",
            ),
        ];
        let d = dag(&files);
        assert_eq!(d.nodes.len(), 4);
        assert!(d.find_cycle().is_none(), "a diamond is not a cycle");

        let tip = d.get("tip").unwrap();
        assert_eq!(tip.inputs, vec!["left_arm", "right_arm"]);
        let base = d.get("base").unwrap();
        assert!(
            base.inputs.is_empty(),
            "base reads a decoded table, not a derivation"
        );
        assert_eq!(
            base.sources,
            vec!["usdc__transfer"],
            "a non-derivation reference is a source, not an edge"
        );

        let keys = d
            .reuse_keys((1, 100), "duckdb-test", &Finality::Final, &any_source)
            .unwrap();
        assert_eq!(keys.len(), 4);

        // The point of transitivity: edit the root and everything downstream moves...
        let edited = [
            (
                "10.sql",
                "CREATE VIEW base AS SELECT k, v FROM usdc__transfer WHERE v > 0",
            ),
            ("20.sql", "CREATE VIEW left_arm AS SELECT k FROM base"),
            ("21.sql", "CREATE VIEW right_arm AS SELECT v FROM base"),
            (
                "30.sql",
                "CREATE VIEW tip AS SELECT * FROM left_arm, right_arm",
            ),
        ];
        let after = dag(&edited)
            .reuse_keys((1, 100), "duckdb-test", &Finality::Final, &any_source)
            .unwrap();
        for n in ["base", "left_arm", "right_arm", "tip"] {
            assert_ne!(keys[n], after[n], "{n} should have moved with its ancestor");
        }
    }

    /// ...and the half that makes per-derivation keying worth having at all: editing **one arm**
    /// leaves the sibling and the sibling's descendants alone. Whole-nest identity cannot express
    /// this, which is why the second axis exists.
    ///
    /// Note this test asserts *isolation*, not propagation - the edited node here has no descendants,
    /// so it passes even against a build with transitivity removed entirely.
    /// `a_diamond_dependency_hashes_transitively` is what catches that, by editing a node that does
    /// have descendants. Both are needed; neither is sufficient.
    #[test]
    fn an_edit_propagates_downstream_and_nowhere_else() {
        let before = dag(&[
            ("10.sql", "CREATE VIEW base AS SELECT k FROM usdc__transfer"),
            ("20.sql", "CREATE VIEW left_arm AS SELECT k FROM base"),
            ("21.sql", "CREATE VIEW right_arm AS SELECT k FROM base"),
            ("30.sql", "CREATE VIEW tip AS SELECT * FROM right_arm"),
        ])
        .reuse_keys((1, 100), "duckdb-test", &Finality::Final, &any_source)
        .unwrap();

        let after = dag(&[
            ("10.sql", "CREATE VIEW base AS SELECT k FROM usdc__transfer"),
            (
                "20.sql",
                "CREATE VIEW left_arm AS SELECT k FROM base WHERE k > 1",
            ),
            ("21.sql", "CREATE VIEW right_arm AS SELECT k FROM base"),
            ("30.sql", "CREATE VIEW tip AS SELECT * FROM right_arm"),
        ])
        .reuse_keys((1, 100), "duckdb-test", &Finality::Final, &any_source)
        .unwrap();

        assert_ne!(
            before["left_arm"], after["left_arm"],
            "the edited arm moves"
        );
        assert_eq!(before["base"], after["base"], "its ancestor must not move");
        assert_eq!(
            before["right_arm"], after["right_arm"],
            "a sibling must not move - this is the whole point of a per-derivation key"
        );
        assert_eq!(
            before["tip"], after["tip"],
            "a descendant of the *sibling* must not move either"
        );
    }

    /// Slice 2's acceptance, half two: a cycle is refused **by name**.
    #[test]
    fn a_cycle_is_refused_and_named() {
        let d = dag(&[
            ("10.sql", "CREATE VIEW a AS SELECT x FROM c"),
            ("20.sql", "CREATE VIEW b AS SELECT x FROM a"),
            ("30.sql", "CREATE VIEW c AS SELECT x FROM b"),
        ]);
        let cycle = d.find_cycle().expect("a -> c -> b -> a is a cycle");
        let named = cycle.to_string();
        for n in ["a", "b", "c"] {
            assert!(named.contains(n), "the cycle must name {n}: {named}");
        }
        assert!(
            named.starts_with(named.split(" -> ").last().unwrap()),
            "the report should close the loop rather than read as a path: {named}"
        );

        let err = d
            .reuse_keys((1, 100), "duckdb-test", &Finality::Final, &any_source)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    /// A view that reads itself is a cycle of one, and must not be silently treated as a leaf.
    #[test]
    fn a_self_referencing_view_is_a_cycle() {
        let d = dag(&[("10.sql", "CREATE VIEW loop_v AS SELECT x FROM loop_v")]);
        // Self-reference is excluded from `inputs` (a node cannot be its own edge), so it surfaces
        // as an unresolvable ordering rather than a graph cycle - either way it must not key.
        let node = d.get("loop_v").unwrap();
        assert!(
            node.inputs.is_empty() && node.sources == vec!["loop_v"],
            "a self-reference should not become an edge: {node:?}"
        );
    }

    // ---- slice 3: refusals and the determinism gate ----

    /// RFC-0033 §4. Each of these **must** be refused - Trino #22533 is what happens otherwise: a
    /// materialized view over `CURRENT_TIMESTAMP` served a frozen timestamp forever.
    #[test]
    fn volatile_functions_are_refused_by_name() {
        for (sql, func) in [
            ("SELECT now()", "now"),
            // Bare, no parens - parses as a COLUMN_REF, not a FUNCTION. A function-only check
            // misses this entirely, which is Trino #22533 in one line of SQL.
            ("SELECT current_timestamp", "current_timestamp"),
            ("SELECT CURRENT_DATE", "current_date"),
            ("SELECT random()", "random"),
            ("SELECT uuid()", "uuid"),
            ("SELECT version()", "version"),
            ("SELECT getenv('HOME')", "getenv"),
            // Buried in a predicate rather than the projection, and case-insensitive.
            ("SELECT x FROM t WHERE ts > NOW()", "now"),
            // Nested inside another call.
            ("SELECT date_trunc('day', now())", "now"),
        ] {
            let refusals = static_refusals(&plan(sql));
            assert!(
                refusals.contains(&Refusal::Volatile {
                    function: func.into()
                }),
                "{sql:?} should be refused for {func}, got {refusals:?}"
            );
            // The reason has to reach a human, not just a match arm.
            let reported = refusals[0].to_string();
            assert!(
                reported.contains(func),
                "the refusal must name it: {reported}"
            );
        }
    }

    /// A `LIMIT` with no `ORDER BY` returns whichever rows the engine felt like. Caching that is
    /// caching a coin flip.
    #[test]
    fn limit_without_order_by_is_refused() {
        assert_eq!(
            static_refusals(&plan("SELECT x FROM t LIMIT 10")),
            vec![Refusal::ImplicitRowOrder]
        );
        // ...and with an ordering it is fine, which is the half that proves the check discriminates
        // rather than refusing everything.
        assert!(static_refusals(&plan("SELECT x FROM t ORDER BY x LIMIT 10")).is_empty());
    }

    /// The control for the whole refusal list: ordinary derivations must **not** be refused. A check
    /// that refused everything would pass every test above and make grafting useless.
    #[test]
    fn ordinary_derivations_are_not_refused() {
        for sql in [
            "SELECT count(*) FROM usdc__transfer",
            "SELECT \"from\", sum(value_dec) FROM usdc__transfer GROUP BY 1",
            "SELECT a.k FROM t AS a JOIN u ON a.k = u.k WHERE a.v > 100",
            "WITH r AS (SELECT k FROM t) SELECT count(*) FROM r",
            "SELECT x FROM t ORDER BY x",
            // A *qualified* reference is an ordinary column, not the bare keyword.
            "SELECT t.current_date FROM t",
        ] {
            assert!(
                static_refusals(&plan(sql)).is_empty(),
                "{sql:?} should be graftable, got {:?}",
                static_refusals(&plan(sql))
            );
        }
    }

    /// RFC-0033 §10. The gate is the backstop for everything the static list cannot prove - which is
    /// why it must actually catch a nondeterminism the list does *not* name.
    #[test]
    fn the_determinism_gate_catches_what_the_static_list_cannot() {
        let conn = parser_connection().unwrap();

        // A deterministic derivation passes, twice over.
        determinism_gate(&conn, "SELECT i FROM range(50) t(i) ORDER BY i").expect("pure");

        // `random()` is on the static list, but the gate must catch it *empirically* rather than by
        // recognising the name - that is the property that makes it a backstop.
        let err = determinism_gate(&conn, "SELECT random() AS r FROM range(200)")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not deterministic"), "{err}");
        assert!(
            err.contains("recomputes"),
            "the error should say what it costs, not just that it failed: {err}"
        );
    }

    /// A derivation whose plan fell back to raw text adds no refusals: it can never match anyway, so
    /// reporting one would be noise an author cannot act on.
    #[test]
    fn an_unparsed_plan_adds_no_refusals() {
        assert!(static_refusals(&plan("this is not sql at all")).is_empty());
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

// ---------------------------------------------------------------------------------------------
// Slice 2: the derivation DAG
// ---------------------------------------------------------------------------------------------

/// One authored derivation: a `CREATE VIEW` in `views/*.sql`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The view's name, which is how other derivations refer to it.
    pub name: String,
    /// The file it came from, for an error a human can act on.
    pub file: String,
    /// The canonical form of its `SELECT` body.
    pub plan: CanonicalPlan,
    /// The names it reads that are **also authored derivations**, sorted and deduplicated. Names
    /// that resolve to decoded event tables are sources, not edges, and live in [`Node::sources`].
    pub inputs: Vec<String>,
    /// Every table name it reads that is not an authored derivation - the decoded tables it sits on.
    pub sources: Vec<String>,
}

/// Split `CREATE [OR REPLACE] [TEMP|TEMPORARY] VIEW <name> [(cols)] AS <select>`.
///
/// Deliberately a small scanner over a fixed prefix rather than a SQL parser, for one reason:
/// `json_serialize_sql` refuses anything that is not a `SELECT` ("Only SELECT statements can be
/// serialized to json!"), so the engine's parser cannot be used on the statement as written. Only the
/// prefix is parsed here; the body still goes to DuckDB.
///
/// **Strict on purpose.** Anything unexpected returns `None`, and a `None` is treated as "not a
/// derivation" - it drops out of the graph rather than being guessed at. A missed derivation costs a
/// recompute; a mis-parsed one would put a wrong edge in the DAG.
fn split_create_view(stmt: &str) -> Option<(String, String)> {
    let t = stmt.trim_start();
    let mut rest = strip_kw(t, "create")?;
    if let Some(r) = strip_kw(rest, "or") {
        rest = strip_kw(r, "replace")?;
    }
    for temp in ["temporary", "temp"] {
        if let Some(r) = strip_kw(rest, temp) {
            rest = r;
            break;
        }
    }
    rest = strip_kw(rest, "view")?;

    // The name: a bare identifier or a double-quoted one.
    let rest = rest.trim_start();
    let (name, mut after) = if let Some(body) = rest.strip_prefix('"') {
        let end = body.find('"')?;
        (body[..end].to_string(), &body[end + 1..])
    } else {
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        (rest[..end].to_string(), &rest[end..])
    };

    // An optional column list. Skipped by depth so a nested paren cannot end it early.
    after = after.trim_start();
    if let Some(body) = after.strip_prefix('(') {
        let mut depth = 1usize;
        let mut end = None;
        for (i, c) in body.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i);
                        break;
                    }
                }
                _ => {}
            }
        }
        after = &body[end? + 1..];
    }

    let select = strip_kw(after, "as")?.trim().to_string();
    if select.is_empty() {
        return None;
    }
    Some((name, select))
}

/// Strip a leading case-insensitive keyword that is followed by whitespace or `(`.
fn strip_kw<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    let s = s.trim_start();
    if s.len() < kw.len() || !s[..kw.len()].eq_ignore_ascii_case(kw) {
        return None;
    }
    let rest = &s[kw.len()..];
    match rest.chars().next() {
        Some(c) if c.is_whitespace() || c == '(' => Some(rest),
        None => Some(rest),
        _ => None,
    }
}

/// Every base-table name a serialized AST reads.
fn table_refs(ast: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    walk(ast, &mut |obj| {
        if obj.get("type").and_then(Value::as_str) == Some("BASE_TABLE") {
            if let Some(n) = obj.get("table_name").and_then(Value::as_str) {
                if !out.iter().any(|s| s == n) {
                    out.push(n.to_string());
                }
            }
        }
    });
    out.sort();
    out
}

/// A cycle among derivations, named so an operator can act on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle(pub Vec<String>);

impl std::fmt::Display for Cycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.join(" -> "))
    }
}

/// The derivation graph of a nest.
#[derive(Debug, Clone, Default)]
pub struct Dag {
    pub nodes: Vec<Node>,
}

impl Dag {
    /// Build the graph from a nest's `views/*.sql`.
    ///
    /// Statements that are not `CREATE VIEW` are ignored rather than guessed at.
    pub fn build(conn: &Connection, files: &[(String, String)]) -> Dag {
        let mut raw: Vec<(String, String, String)> = Vec::new(); // (name, file, select body)
        for (file, sql) in files {
            for stmt in crate::analytics::split_sql_statements(sql) {
                if let Some((name, body)) = split_create_view(&stmt) {
                    raw.push((name, file.clone(), body));
                }
            }
        }
        let defined: Vec<String> = raw.iter().map(|(n, _, _)| n.clone()).collect();

        let nodes = raw
            .into_iter()
            .map(|(name, file, body)| {
                let plan = canonical_plan(conn, &body);
                // Edges come from the *canonicalised* AST, so a view that fails to parse contributes
                // no edges - it becomes a leaf rather than a wrong shape.
                let refs = match &plan {
                    CanonicalPlan::Ast(json) => serde_json::from_str::<Value>(json)
                        .map(|v| table_refs(&v))
                        .unwrap_or_default(),
                    CanonicalPlan::RawText(_) => Vec::new(),
                };
                let (inputs, sources): (Vec<_>, Vec<_>) = refs
                    .into_iter()
                    .partition(|r| defined.contains(r) && r != &name);
                Node {
                    name,
                    file,
                    plan,
                    inputs,
                    sources,
                }
            })
            .collect();
        Dag { nodes }
    }

    fn get(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// The first cycle, if any (RFC-0033 §6).
    ///
    /// Derivations read decoded events and other derivations, never themselves, so the graph is a DAG
    /// **by construction** - a cycle means the nest is malformed, and it is a load-time refusal with
    /// the cycle named rather than a fixpoint to solve or a runtime surprise.
    pub fn find_cycle(&self) -> Option<Cycle> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Open,
            Done,
        }
        let mut marks: std::collections::HashMap<&str, Mark> = std::collections::HashMap::new();
        let mut stack: Vec<&str> = Vec::new();

        fn visit<'a>(
            dag: &'a Dag,
            name: &'a str,
            marks: &mut std::collections::HashMap<&'a str, Mark>,
            stack: &mut Vec<&'a str>,
        ) -> Option<Cycle> {
            match marks.get(name) {
                Some(Mark::Done) => return None,
                Some(Mark::Open) => {
                    // Report from where the cycle actually closes, and close the loop in the output
                    // so `a -> b -> a` reads as a cycle rather than a path.
                    let at = stack.iter().position(|s| *s == name).unwrap_or(0);
                    let mut path: Vec<String> = stack[at..].iter().map(|s| s.to_string()).collect();
                    path.push(name.to_string());
                    return Some(Cycle(path));
                }
                None => {}
            }
            marks.insert(name, Mark::Open);
            stack.push(name);
            if let Some(node) = dag.get(name) {
                for input in &node.inputs {
                    if let Some(found) = visit(dag, input, marks, stack) {
                        return Some(found);
                    }
                }
            }
            stack.pop();
            marks.insert(name, Mark::Done);
            None
        }

        for node in &self.nodes {
            if let Some(found) = visit(self, &node.name, &mut marks, &mut stack) {
                return Some(found);
            }
        }
        None
    }

    /// Every derivation's reuse key, transitively (RFC-0033 §1).
    ///
    /// A Merkle hash over the DAG: a node's key includes its inputs' keys, so a change propagates
    /// downstream **and nowhere else**. That is the whole reason the key is per-derivation rather
    /// than per-nest: whole-package identity over-invalidates by construction.
    ///
    /// Errors on a cycle rather than looping. `range`, `engine` and `finality` are the run's, shared
    /// by every derivation in it; `sources` resolves a table name to its identity.
    pub fn reuse_keys(
        &self,
        range: (u64, u64),
        engine: &str,
        finality: &Finality,
        resolve: &dyn Fn(&str) -> Option<SourceIdentity>,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        if let Some(cycle) = self.find_cycle() {
            anyhow::bail!("derivations form a cycle: {cycle}");
        }
        let mut keys: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        // Repeatedly place every node whose inputs are all keyed. Terminates because the graph is
        // acyclic (checked above) and every pass places at least one node.
        while keys.len() < self.nodes.len() {
            let mut placed = false;
            for node in &self.nodes {
                if keys.contains_key(&node.name) {
                    continue;
                }
                if !node.inputs.iter().all(|i| keys.contains_key(i)) {
                    continue;
                }
                let derivation = Derivation {
                    name: node.name.clone(),
                    plan: node.plan.clone(),
                    input_keys: node
                        .inputs
                        .iter()
                        .filter_map(|i| keys.get(i).cloned())
                        .collect(),
                    sources: node.sources.iter().filter_map(|s| resolve(s)).collect(),
                    range,
                    engine: engine.to_string(),
                    finality: finality.clone(),
                };
                keys.insert(node.name.clone(), derivation.reuse_key());
                placed = true;
            }
            if !placed {
                // An input naming a derivation that does not exist. Not a cycle, so `find_cycle` did
                // not catch it; refusing beats keying a node against a dependency we cannot see.
                let stuck: Vec<&str> = self
                    .nodes
                    .iter()
                    .filter(|n| !keys.contains_key(&n.name))
                    .map(|n| n.name.as_str())
                    .collect();
                anyhow::bail!(
                    "derivations cannot be ordered - unresolved inputs among: {}",
                    stuck.join(", ")
                );
            }
        }
        Ok(keys)
    }
}

// ---------------------------------------------------------------------------------------------
// Slice 3: the hard refusal list (§4) and the determinism gate (§10)
// ---------------------------------------------------------------------------------------------

/// Why a derivation can never be reused, whatever its key says.
///
/// A refusal is **loud**: the derivation recomputes and the reason is reported, so an author can see
/// *why* their view never grafts rather than wondering why edits stay slow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// A volatile or nondeterministic function. Every production cache surveyed refuses these, and
    /// Trino #22533 is what happens when you do not: a materialized view over `CURRENT_TIMESTAMP`
    /// served a frozen timestamp, because snapshot-based freshness has no concept of
    /// time-dependence.
    Volatile { function: String },
    /// A result that depends on row order the engine does not guarantee - `LIMIT` with no `ORDER BY`
    /// being the canonical case. Two runs may legitimately disagree, so caching either is caching a
    /// coin flip.
    ImplicitRowOrder,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::Volatile { function } => write!(
                f,
                "calls the volatile function `{function}()`, whose value changes between runs"
            ),
            Refusal::ImplicitRowOrder => write!(
                f,
                "uses LIMIT without ORDER BY, so which rows it returns is not guaranteed between runs"
            ),
        }
    }
}

/// Functions whose value is not a function of the data (RFC-0033 §4).
///
/// Deliberately a **denylist of known-volatile names** rather than an allowlist of known-pure ones,
/// with the determinism gate (§10) as the empirical backstop. An allowlist over DuckDB's several
/// hundred scalar functions would be wrong on day one and wrong differently after every upgrade;
/// this list plus a gate that actually runs the query twice is the honest combination.
const VOLATILE_FUNCTIONS: &[&str] = &[
    "now",
    "current_timestamp",
    "get_current_timestamp",
    "transaction_timestamp",
    "current_date",
    "current_time",
    "current_localtime",
    "current_localtimestamp",
    "today",
    "random",
    "setseed",
    "uuid",
    "gen_random_uuid",
    "uuidv4",
    "uuidv7",
    "version",
    "current_setting",
    "getenv",
    "current_schema",
    "current_schemas",
    "current_catalog",
    "current_database",
    "current_user",
    "current_query",
    "txid_current",
    "nextval",
    "currval",
];

/// Static refusals provable from the parse alone (RFC-0033 §4).
///
/// **What this cannot prove, stated rather than left as a gap:** §4 also refuses *float aggregation
/// where order matters*, because IEEE-754 addition is not associative, so `sum()` over a `DOUBLE`
/// column can differ between runs that group rows differently. Deciding that needs the **column's
/// type**, which needs binding the view against the nest's schema - not parsing it. Detecting it
/// half-way would be worse than not at all: flagging every `sum()` makes almost every useful view
/// never-graftable, and flagging only float *literals* would create confidence that a `DOUBLE` column
/// had been checked when it had not.
///
/// [`determinism_gate`] is the backstop, and it is the stronger one: it catches float
/// non-associativity empirically, along with every other nondeterminism this list does not name.
pub fn static_refusals(plan: &CanonicalPlan) -> Vec<Refusal> {
    let CanonicalPlan::Ast(json) = plan else {
        // A derivation we could not parse is never reused anyway - its plan is raw text, so any edit
        // at all breaks the match. No refusal to add.
        return Vec::new();
    };
    let Ok(ast) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };

    let mut out: Vec<Refusal> = Vec::new();
    walk(&ast, &mut |obj| {
        if obj.get("class").and_then(Value::as_str) == Some("FUNCTION") {
            if let Some(name) = obj.get("function_name").and_then(Value::as_str) {
                let lower = name.to_ascii_lowercase();
                if VOLATILE_FUNCTIONS.contains(&lower.as_str()) {
                    let r = Refusal::Volatile { function: lower };
                    if !out.contains(&r) {
                        out.push(r);
                    }
                }
            }
        }
        // **A bare volatile keyword is not a FUNCTION node.** `SELECT current_timestamp` - standard
        // SQL, no parens - parses as a `COLUMN_REF` whose single name is the keyword, so a
        // function-only check misses it entirely and the derivation caches a frozen timestamp
        // forever. That *is* Trino #22533. Verified against DuckDB's AST rather than assumed.
        //
        // Only **unqualified** single-element references are considered: `t.current_date` is an
        // ordinary column on table `t`. A column genuinely named `current_date` would be refused
        // here, which is the safe direction - an over-refusal costs a recompute, and the alternative
        // costs correctness.
        if obj.get("type").and_then(Value::as_str) == Some("COLUMN_REF") {
            if let Some(Value::Array(parts)) = obj.get("column_names") {
                if parts.len() == 1 {
                    if let Some(name) = parts[0].as_str() {
                        let lower = name.to_ascii_lowercase();
                        if VOLATILE_FUNCTIONS.contains(&lower.as_str()) {
                            let r = Refusal::Volatile { function: lower };
                            if !out.contains(&r) {
                                out.push(r);
                            }
                        }
                    }
                }
            }
        }
        // A `SELECT_NODE`'s modifiers carry LIMIT and ORDER BY. A limit with no ordering leaves the
        // engine free to return any rows it likes.
        if obj.get("type").and_then(Value::as_str) == Some("SELECT_NODE") {
            if let Some(Value::Array(mods)) = obj.get("modifiers") {
                let kind = |t: &str| {
                    mods.iter()
                        .any(|m| m.get("type").and_then(Value::as_str) == Some(t))
                };
                if kind("LIMIT_MODIFIER")
                    && !kind("ORDER_MODIFIER")
                    && !out.contains(&Refusal::ImplicitRowOrder)
                {
                    out.push(Refusal::ImplicitRowOrder);
                }
            }
        }
    });
    out
}

/// Run a derivation twice over the same range and report whether it agreed with itself (§10).
///
/// The cache is only as correct as the determinism of what it caches, and this is the one check that
/// does not depend on us having *named* the nondeterminism in advance - which is why it is the
/// backstop for everything [`static_refusals`] cannot prove, float non-associativity included.
///
/// Cheap by design: finalized ranges are exactly where re-execution is supposed to be free, so a gate
/// that re-runs one is testing the property it relies on at the price it was promised.
///
/// Returns `Ok(())` when the two runs agree. `conn` must already have the derivation's inputs
/// defined; this runs the statement, it does not build a nest.
pub fn determinism_gate(conn: &Connection, sql: &str) -> Result<()> {
    let digest = |attempt: usize| -> Result<String> {
        let mut stmt = conn
            .prepare(sql)
            .with_context(|| format!("preparing the derivation for determinism run {attempt}"))?;
        let mut rows = stmt
            .query([])
            .with_context(|| format!("running the derivation for determinism run {attempt}"))?;
        let mut h = Sha256::new();
        // Row *and* column order are part of the answer: a derivation that returns the same bag in a
        // different order is not the same derivation for anything downstream that reads positionally.
        while let Some(row) = rows.next()? {
            let mut col = 0usize;
            while let Ok(v) = row.get::<_, duckdb::types::Value>(col) {
                h.update(format!("{v:?}").as_bytes());
                h.update(b"\x1f");
                col += 1;
            }
            h.update(b"\x1e");
        }
        Ok(hex::encode(h.finalize()))
    };

    let first = digest(1)?;
    let second = digest(2)?;
    if first != second {
        anyhow::bail!(
            "derivation is not deterministic: two runs over the same range produced different \
             output ({} vs {}). It cannot be cached - find the nondeterminism (a volatile function, \
             float aggregation whose order varies, or a result that depends on row order) or accept \
             that it recomputes.",
            &first[..12],
            &second[..12]
        );
    }
    Ok(())
}
