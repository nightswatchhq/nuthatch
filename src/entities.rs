//! RFC-0041 slice one: explicit authored incremental-entity declarations and conservative refusal.

use anyhow::{bail, Context, Result};
use duckdb::Connection;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

pub const ENTITY_FILE: &str = "entities.toml";
pub const ENTITY_COMPILER_ID: &str = "nuthatch-rfc0041-v1";

/// The durable definition of an authored incremental entity. Unlike an ordinary query result it
/// deliberately has no block range: this identifies the compiler contract and is stable while the
/// maintained state advances. A snapshot or cache adds its covered range separately.
#[derive(Debug, Clone)]
pub struct EntityIdentity {
    pub plan: crate::graft::CanonicalPlan,
    /// Reuse keys of earlier entities read by this entity, not their names.
    pub input_entity_keys: Vec<String>,
    /// Resolved decoded-table identities, never just aliases such as `usdc__transfer`.
    pub sources: Vec<crate::graft::SourceIdentity>,
    /// Ordered because it defines the point-read tuple exposed by the entity.
    pub key: Vec<String>,
    /// Ordered because a relation's output is positional at the compiler boundary.
    pub output_schema: Vec<String>,
    pub engine: String,
}

impl EntityIdentity {
    /// Stable identity for one entity compiler contract. A changed lowerer, DuckDB evaluation
    /// version, decoded source, upstream entity, key shape or output shape must rebuild rather
    /// than graft. `Derivation` supplies the carefully length-delimited source and input hashing;
    /// this extension adds the parts unique to maintained keyed state.
    pub fn reuse_key(&self) -> String {
        let derivation = crate::graft::Derivation {
            name: String::new(),
            plan: self.plan.clone(),
            input_keys: self.input_entity_keys.clone(),
            sources: self.sources.clone(),
            // This is a definition key. Using a fixed range prevents a newly indexed block from
            // making an otherwise unchanged entity look like a different program.
            range: (0, 0),
            engine: format!("{ENTITY_COMPILER_ID}/{}", self.engine),
            finality: crate::graft::Finality::Final,
        };
        let mut hash = Sha256::new();
        hash.update(b"nuthatch-entity-reuse-key-v1\0");
        for part in std::iter::once(derivation.reuse_key())
            .chain(self.key.iter().cloned())
            .chain(self.output_schema.iter().cloned())
        {
            hash.update((part.len() as u64).to_le_bytes());
            hash.update(part.as_bytes());
        }
        hex::encode(hash.finalize())
    }
}

/// Construct the entity definition identity from DuckDB's parser. A parser failure is represented
/// as raw text by `canonical_plan`, which can only forfeit reuse, never make two meanings collide.
pub fn identity(
    sql: &str,
    input_entity_keys: Vec<String>,
    sources: Vec<crate::graft::SourceIdentity>,
    key: Vec<String>,
    output_schema: Vec<String>,
) -> Result<EntityIdentity> {
    let parser = crate::graft::Parser::new()?;
    Ok(EntityIdentity {
        plan: parser.canonical_plan(sql),
        input_entity_keys,
        sources,
        key,
        output_schema,
        engine: parser.engine_version(),
    })
}

#[derive(Debug, Deserialize)]
struct EntityFile {
    #[serde(default)]
    entities: Vec<EntityDecl>,
}

/// One author-declared maintained relation. Parsing is centralised here so startup, `check`, and a
/// future serving surface all act on exactly the file whose bytes form the nest identity.
#[derive(Debug, Clone, Deserialize)]
pub struct EntityDecl {
    pub name: String,
    pub sql: String,
    pub key: Vec<String>,
    pub max_rows: usize,
}

/// Validation failures are collected so `nuthatch check` names every bad declaration at once.
#[derive(Debug, Clone)]
pub struct EntityIssue {
    pub name: String,
    pub error: String,
}

/// Whether this directory asks `check` to validate authored incremental state at all. A valid
/// entity-only nest is useful before it has parity checks, so it must not be mistaken for a nest
/// with nothing to validate merely because validation found no errors.
pub fn has_declarations(dir: &Path) -> bool {
    dir.join(ENTITY_FILE).is_file()
        || std::fs::read_dir(dir.join("entities"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .any(|path| path.extension().and_then(|x| x.to_str()) == Some("sql"))
}

/// Load the authored entity declarations. An absent manifest means this nest has no incremental
/// entities, which is the ordinary case. A present but malformed manifest is an error rather than an
/// empty list: treating a typo as "no runtime needed" would make maintained state disappear.
pub fn load(dir: &Path) -> Result<Vec<EntityDecl>> {
    let path = dir.join(ENTITY_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    let file: EntityFile =
        toml::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    Ok(file.entities)
}

pub fn validate(dir: &Path) -> Vec<EntityIssue> {
    let schema = crate::config::Config::load(dir)
        .ok()
        .and_then(|cfg| crate::registry::from_nest(dir, &cfg).ok())
        .map(|registry| registry.schema());
    let path = dir.join(ENTITY_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return undeclared_files(dir),
        Err(e) => {
            return vec![issue(
                ENTITY_FILE,
                format!("cannot read {}: {e}", path.display()),
            )]
        }
    };
    let file: EntityFile = match toml::from_str(&raw) {
        Ok(file) => file,
        Err(e) => return vec![issue(ENTITY_FILE, format!("invalid entities.toml: {e}"))],
    };
    let mut issues = Vec::new();
    if file.entities.is_empty() {
        issues.push(issue(
            ENTITY_FILE,
            "declares no entities; remove entities.toml until an incremental relation is ready",
        ));
    }
    let mut graph = Vec::new();
    let mut names = BTreeSet::new();
    let mut declared = BTreeSet::new();
    for entity in file.entities {
        let name = entity.name.clone();
        if !names.insert(name.clone()) {
            issues.push(issue(&name, "duplicate entity name"));
        }
        if entity.key.is_empty() {
            issues.push(issue(&name, "key must name at least one output column"));
        }
        if entity.max_rows == 0 {
            issues.push(issue(&name, "max_rows must be greater than zero"));
        }
        let rel = Path::new(&entity.sql);
        if rel.components().count() != 2
            || rel.parent() != Some(Path::new("entities"))
            || rel.extension().and_then(|x| x.to_str()) != Some("sql")
        {
            issues.push(issue(&name, "sql must name one entities/<name>.sql file"));
            continue;
        }
        declared.insert(entity.sql.clone());
        if rel.file_stem().and_then(|s| s.to_str()) != Some(name.as_str()) {
            issues.push(issue(
                &name,
                "entity name must match the declared SQL filename",
            ));
        }
        let sql_path = dir.join(rel);
        let sql = match std::fs::read_to_string(&sql_path) {
            Ok(sql) => sql,
            Err(e) => {
                issues.push(issue(&name, format!("cannot read {}: {e}", entity.sql)));
                continue;
            }
        };
        if let Err(e) = validate_sql(&sql) {
            issues.push(issue(&name, e.to_string()));
        } else {
            match dependencies(&sql) {
                Ok(deps) => graph.push((name.clone(), deps)),
                Err(e) => issues.push(issue(
                    &name,
                    format!("cannot determine entity dependencies: {e}"),
                )),
            }
            if let Some(schema) = &schema {
                match crate::analytics::entity_output_columns(dir, schema, &sql) {
                    Ok(columns) => {
                        for key in &entity.key {
                            if !columns
                                .iter()
                                .any(|column| column.eq_ignore_ascii_case(key))
                            {
                                issues.push(issue(
                                    &name,
                                    format!("declared key `{key}` is not an output column"),
                                ));
                            }
                        }
                        if entity.key.len() != entity.key.iter().collect::<BTreeSet<_>>().len() {
                            issues.push(issue(&name, "declared key repeats a column"));
                        }
                        if !entity.key.is_empty() && !issues.iter().any(|i| i.name == name) {
                            match crate::analytics::query_guarded(
                                dir,
                                &sql,
                                crate::analytics::QueryGuard {
                                    // Authoring validation must be bounded too. `max_rows` is an
                                    // executable admission contract, not permission to materialise
                                    // an unlimited reference result during `nuthatch check`.
                                    timeout: Duration::from_secs(60),
                                    max_rows: entity.max_rows.saturating_add(1),
                                },
                            ) {
                                Ok(result) => {
                                    let rows = result.rows;
                                    if result.truncated || rows.len() > entity.max_rows {
                                        issues.push(issue(
                                            &name,
                                            format!(
                                                "reference result exceeds declared max_rows ({})",
                                                entity.max_rows
                                            ),
                                        ));
                                        continue;
                                    }
                                    let mut seen = BTreeSet::new();
                                    for row in rows {
                                        let values: Vec<&Value> = entity
                                            .key
                                            .iter()
                                            .filter_map(|key| row.get(key))
                                            .collect();
                                        if values.len() != entity.key.len()
                                            || values.iter().any(|value| value.is_null())
                                        {
                                            issues.push(issue(
                                                &name,
                                                "declared key is nullable in the reference result",
                                            ));
                                            break;
                                        }
                                        if !seen.insert(
                                            serde_json::to_string(&values).unwrap_or_default(),
                                        ) {
                                            issues.push(issue(
                                            &name,
                                            "declared key is not unique in the reference result",
                                        ));
                                            break;
                                        }
                                    }
                                }
                                Err(e) => issues.push(issue(
                                    &name,
                                    format!("entity reference query failed: {e}"),
                                )),
                            }
                        }
                    }
                    Err(e) => issues.push(issue(&name, format!("entity SQL does not bind: {e}"))),
                }
            }
        }
    }
    if let Some(cycle) = dependency_cycle(&graph) {
        issues.push(issue(
            ENTITY_FILE,
            format!(
                "incremental entity dependency cycle: {}",
                cycle.join(" -> ")
            ),
        ));
    }
    for mut missing in undeclared_files(dir) {
        let path = missing
            .name
            .strip_prefix("entities/")
            .unwrap_or(&missing.name)
            .to_string();
        if declared.contains(&format!("entities/{path}")) {
            continue;
        }
        missing.error = "entity SQL file has no entities.toml declaration".into();
        issues.push(missing);
    }
    issues
}

/// The serialized AST for `sql`, parsed. Shared by the shape gate and the allowlist so both judge
/// exactly the same parse.
fn plan_ast(conn: &Connection, sql: &str) -> Result<Value> {
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let raw: String =
        conn.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get(0)
        })?;
    Ok(serde_json::from_str(&raw)?)
}

/// Aggregates whose maintenance under insert **and retraction** the v1 lowerer can express.
///
/// Short by design and an **allowlist**, which is the whole point (#836). The refusal list used to
/// enumerate what was forbidden - `MEDIAN`, `MODE`, `PERCENTILE_*` - over a vocabulary DuckDB owns
/// and grows, and it was wrong in both the ways `analytics.rs` predicts a denylist is wrong. About
/// **coverage**: this build knows 88 distinct aggregate names, of which the list named three, so
/// `quantile_cont`, `arg_max`, `string_agg`, `list`, `first`, `histogram` and the rest were admitted
/// as incrementally maintainable. And about **spelling**: `PERCENTILE_CONT` is the SQL-standard alias
/// while `quantile_cont` is the name DuckDB actually uses, so the list blocked the alias and admitted
/// the real thing.
///
/// `count_star` is DuckDB's internal name for `count(*)`.
const INCREMENTAL_AGGREGATES: &[&str] = &["sum", "min", "max", "avg", "count", "count_star"];

/// Every `function_name` the parsed statement mentions, at any depth.
fn function_names(ast: &Value, out: &mut BTreeSet<String>) {
    match ast {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("FUNCTION") {
                if let Some(name) = map.get("function_name").and_then(Value::as_str) {
                    out.insert(name.to_ascii_lowercase());
                }
            }
            for child in map.values() {
                function_names(child, out);
            }
        }
        Value::Array(values) => values.iter().for_each(|v| function_names(v, out)),
        _ => {}
    }
}

/// Whether any node of `kind` appears, and whether any aggregate carries `DISTINCT`.
/// An **expression** subquery: a scalar `(SELECT …)`, `IN (SELECT …)`, or `EXISTS (…)`.
///
/// Deliberately not "any node of type SUBQUERY". DuckDB gives a derived table in `FROM` the same
/// node type, and a derived table is an ordinary relation the lowerer has no trouble with - refusing
/// those would reject `FROM (VALUES …) t(k)` and most real authored SQL with it. The two are told
/// apart by `class`: an expression subquery carries `class: "SUBQUERY"` and a `subquery_type`
/// (`SCALAR`/`ANY`/`EXISTS`), a derived table carries neither.
fn has_expression_subquery(ast: &Value) -> bool {
    match ast {
        Value::Object(map) => {
            (map.get("class").and_then(Value::as_str) == Some("SUBQUERY")
                && map.get("subquery_type").is_some())
                || map.values().any(has_expression_subquery)
        }
        Value::Array(values) => values.iter().any(has_expression_subquery),
        _ => false,
    }
}

fn has_distinct_aggregate(ast: &Value) -> bool {
    match ast {
        Value::Object(map) => {
            (map.get("type").and_then(Value::as_str) == Some("FUNCTION")
                && map.get("distinct").and_then(Value::as_bool) == Some(true))
                || map.values().any(has_distinct_aggregate)
        }
        Value::Array(values) => values.iter().any(has_distinct_aggregate),
        _ => false,
    }
}

/// Which of `names` DuckDB itself classifies as aggregates.
///
/// Asked of the engine rather than kept in a table here, so the set is whatever this build actually
/// supports and cannot drift from it. `duckdb_functions()` is the same catalogue the binder uses.
fn aggregates_among(conn: &Connection, names: &BTreeSet<String>) -> Result<BTreeSet<String>> {
    if names.is_empty() {
        return Ok(BTreeSet::new());
    }
    let list = names
        .iter()
        .map(|n| format!("'{}'", n.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT DISTINCT lower(function_name) FROM duckdb_functions() \
         WHERE function_type = 'aggregate' AND lower(function_name) IN ({list})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = BTreeSet::new();
    for r in rows {
        out.insert(r?);
    }
    Ok(out)
}

fn validate_sql(sql: &str) -> Result<()> {
    let conn = Connection::open_in_memory()?;
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let ast: String =
        conn.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get(0)
        })?;
    let ast: Value = serde_json::from_str(&ast)?;
    if ast
        .pointer("/statements")
        .and_then(Value::as_array)
        .is_none_or(|s| s.len() != 1)
        || ast
            .pointer("/statements/0/node/type")
            .and_then(Value::as_str)
            != Some("SELECT_NODE")
    {
        bail!("entity must contain exactly one SELECT; keep other SQL as views/*.sql")
    }
    // Reuse the AST-level volatile detector rather than trying to maintain a second string list.
    // DuckDB represents `CURRENT_DATE` as an unqualified column reference, not a function call. A
    // text matcher that gets this wrong turns time into silently frozen state.
    let plan = crate::graft::canonical_plan(&conn, sql);
    if crate::graft::static_refusals(&plan)
        .iter()
        .any(|refusal| matches!(refusal, crate::graft::Refusal::Volatile { .. }))
    {
        bail!("volatile functions are not incremental v1 SQL; keep this as views/*.sql")
    }
    // **The allowlist, and the control meant to outlive the token pass below** (#836).
    //
    // Asks DuckDB which of the functions this statement names are aggregates, then admits only the
    // ones the v1 lowerer can maintain. A function the engine gains tomorrow is refused by default,
    // which is the property the token list could never have - and the name comes from the parsed
    // AST, so `"median"(x)` cannot spell its way past it either.
    let ast = plan_ast(&conn, sql)?;
    let mut named = BTreeSet::new();
    function_names(&ast, &mut named);
    for aggregate in aggregates_among(&conn, &named)? {
        if !INCREMENTAL_AGGREGATES.contains(&aggregate.as_str()) {
            bail!(
                "`{aggregate}` is not an aggregate incremental v1 can maintain (only {}); \
                 keep this as views/*.sql",
                INCREMENTAL_AGGREGATES.join(", ")
            )
        }
    }
    if has_expression_subquery(&ast) {
        bail!(
            "correlated and scalar subqueries are not incremental v1 SQL; keep this as views/*.sql"
        )
    }
    if has_distinct_aggregate(&ast) {
        bail!("DISTINCT aggregates are not incremental v1 SQL; keep this as views/*.sql")
    }
    if ast
        .pointer("/statements/0/node/sample")
        .is_some_and(|v| !v.is_null())
    {
        bail!("USING SAMPLE is not incremental v1 SQL; keep this as views/*.sql")
    }
    if ast
        .pointer("/statements/0/node/group_sets")
        .and_then(Value::as_array)
        .is_some_and(|sets| sets.len() > 1)
    {
        bail!("GROUPING SETS/ROLLUP/CUBE are not incremental v1 SQL; keep this as views/*.sql")
    }

    // Kept *beside* the allowlist rather than replaced by it: two independent controls that must both
    // pass, so a gap in either is covered. These are syntax forms, not function names, so the
    // catalogue above cannot see them.
    let tokens = sql_tokens(sql);
    for (needle, why) in [
        ("DISTINCT", "DISTINCT"),
        ("LIMIT", "LIMIT"),
        ("OVER", "window functions"),
        ("RECURSIVE", "recursive CTEs"),
        ("OUTER", "outer joins"),
        ("EXISTS", "correlated subqueries"),
    ] {
        if tokens.iter().any(|token| token == needle) {
            bail!("{why} is not incremental v1 SQL; keep this as views/*.sql")
        }
    }
    if tokens
        .windows(2)
        .any(|pair| pair[0] == "ORDER" && pair[1] == "BY")
    {
        bail!("ORDER BY is not incremental v1 SQL; keep this as views/*.sql")
    }
    if tokens
        .windows(2)
        .any(|pair| matches!(pair[0].as_str(), "LEFT" | "RIGHT" | "FULL") && pair[1] == "JOIN")
    {
        bail!("outer joins are not incremental v1 SQL; keep this as views/*.sql")
    }
    if tokens.windows(2).any(|pair| {
        (matches!(pair[0].as_str(), "MEDIAN" | "MODE") || pair[0].starts_with("PERCENTILE_"))
            && pair[1] == "("
    }) {
        bail!("holistic aggregates are not incremental v1 SQL; keep this as views/*.sql")
    }
    Ok(())
}

/// SQL tokens relevant to the refusal list. DuckDB owns parsing and the statement-shape gate above;
/// this only recognises constructs whose AST forms are deliberately not yet lowered. Quoted text and
/// comments are discarded first, so an entity may quite safely produce the string `"ORDER BY"`.
fn sql_tokens(sql: &str) -> Vec<String> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Code,
        /// Inside `"..."` - a name, so its characters accumulate into the current token.
        Ident,
        Quote(char),
        LineComment,
        BlockComment,
    }

    let mut state = State::Code;
    let mut current = String::new();
    let mut tokens = Vec::new();
    let chars: Vec<char> = sql.chars().collect();
    let mut at = 0;
    let flush = |current: &mut String, tokens: &mut Vec<String>| {
        if !current.is_empty() {
            tokens.push(current.to_ascii_uppercase());
            current.clear();
        }
    };
    while at < chars.len() {
        let ch = chars[at];
        let next = chars.get(at + 1).copied();
        match state {
            State::Code if ch == '-' && next == Some('-') => {
                flush(&mut current, &mut tokens);
                state = State::LineComment;
                at += 1;
            }
            State::Code if ch == '/' && next == Some('*') => {
                flush(&mut current, &mut tokens);
                state = State::BlockComment;
                at += 1;
            }
            // A single quote opens a string literal, whose contents are text and are discarded. A
            // double quote opens a **quoted identifier**, whose contents are a *name* - discarding
            // those is what let `"median"(v)` past the refusal list (#836). `analytics.rs` learned
            // the same lesson from `"read_csv"('/etc/passwd')` and fixed it by stripping the quotes
            // rather than the contents; this does the same.
            State::Code if ch == '\'' => {
                flush(&mut current, &mut tokens);
                state = State::Quote(ch);
            }
            State::Code if ch == '"' => {
                flush(&mut current, &mut tokens);
                state = State::Ident;
            }
            State::Code if ch.is_ascii_alphanumeric() || ch == '_' => current.push(ch),
            State::Code if ch == '(' => {
                flush(&mut current, &mut tokens);
                tokens.push("(".into());
            }
            State::Code => flush(&mut current, &mut tokens),
            State::Ident if ch == '"' && next == Some('"') => {
                current.push('"');
                at += 1;
            }
            State::Ident if ch == '"' => state = State::Code,
            State::Ident => current.push(ch),
            State::Quote(quote) if ch == quote && next == Some(quote) => at += 1,
            State::Quote(quote) if ch == quote => state = State::Code,
            State::Quote(_) => {}
            State::LineComment if ch == '\n' => state = State::Code,
            State::LineComment => {}
            State::BlockComment if ch == '*' && next == Some('/') => {
                state = State::Code;
                at += 1;
            }
            State::BlockComment => {}
        }
        at += 1;
    }
    flush(&mut current, &mut tokens);
    tokens
}

/// Referenced relations from the same DuckDB AST used for the statement-shape gate. The caller
/// resolves these names against fact tables and earlier entities to form the entity DAG.
pub fn dependencies(sql: &str) -> Result<Vec<String>> {
    let conn = Connection::open_in_memory()?;
    Ok(crate::graft::table_refs(&plan_ast(&conn, sql)?))
}

/// Return one named cycle among entity-to-entity dependencies. Fact tables are absent from `nodes`
/// and therefore terminate a walk; only declared entities can form an invalid recursive graph.
pub fn dependency_cycle(nodes: &[(String, Vec<String>)]) -> Option<Vec<String>> {
    let graph: std::collections::BTreeMap<_, _> = nodes.iter().cloned().collect();
    fn visit(
        node: &str,
        graph: &std::collections::BTreeMap<String, Vec<String>>,
        visiting: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if let Some(at) = visiting.iter().position(|n| n == node) {
            let mut cycle = visiting[at..].to_vec();
            cycle.push(node.into());
            return Some(cycle);
        }
        if !done.insert(node.into()) {
            return None;
        }
        visiting.push(node.into());
        if let Some(deps) = graph.get(node) {
            for dep in deps {
                if graph.contains_key(dep) {
                    if let Some(cycle) = visit(dep, graph, visiting, done) {
                        return Some(cycle);
                    }
                }
            }
        }
        visiting.pop();
        None
    }
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        if let Some(cycle) = visit(node, &graph, &mut Vec::new(), &mut done) {
            return Some(cycle);
        }
    }
    None
}

fn undeclared_files(dir: &Path) -> Vec<EntityIssue> {
    let Ok(entries) = std::fs::read_dir(dir.join("entities")) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sql"))
        .map(|p| {
            issue(
                &p.strip_prefix(dir).unwrap_or(&p).display().to_string(),
                "entity SQL file has no entities.toml declaration",
            )
        })
        .collect()
}

fn issue(name: &str, error: impl Into<String>) -> EntityIssue {
    EntityIssue {
        name: name.into(),
        error: error.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("entities")).unwrap();
        dir
    }

    fn configured_nest() -> tempfile::TempDir {
        let dir = nest();
        std::fs::write(
            dir.path().join("nuthatch.toml"),
            "[nest]\nname = \"entity-test\"\nchain = \"mainnet\"\nchain_id = 1\nrpc_urls = []\n",
        )
        .unwrap();
        dir
    }

    fn source(contract: &str) -> crate::graft::SourceIdentity {
        crate::graft::SourceIdentity {
            table: "facts".into(),
            chain_id: 1,
            contract: contract.into(),
            event_signature: "Fact(uint256)".into(),
            abi_hash: "ab".repeat(32),
            schema_version: 1,
        }
    }

    #[test]
    fn declaration_is_ast_parsed_and_rejects_non_incremental_shapes() {
        let dir = nest();
        std::fs::write(
            dir.path().join(ENTITY_FILE),
            "[[entities]]\nname='totals'\nsql='entities/totals.sql'\nkey=['owner']\nmax_rows=10\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("entities/totals.sql"), "SELECT 1 AS owner").unwrap();
        assert!(validate(dir.path()).is_empty());

        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT 1 AS owner; SELECT 2",
        )
        .unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("exactly one SELECT")));

        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT DISTINCT 1 AS owner",
        )
        .unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("DISTINCT")));
        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT random() AS owner",
        )
        .unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("volatile")));

        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT current_date AS owner",
        )
        .unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("volatile")));

        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT l.owner FROM lefts AS l LEFT OUTER JOIN rights AS r ON l.owner = r.owner",
        )
        .unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("outer joins")));

        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT count(DISTINCT owner) AS owner FROM facts",
        )
        .unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("DISTINCT")));

        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT 'ORDER BY' AS owner -- LIMIT is prose here\n",
        )
        .unwrap();
        assert!(validate(dir.path()).is_empty());
    }

    /// #836 corrected the second half of this. A single-quoted **string** is text and is rightly
    /// discarded; a double-quoted **identifier** is a *name*, and discarding it is what let
    /// `"median"(v)` past the refusal list. The identifier now survives tokenisation.
    ///
    /// The cost is over-refusal: an entity that quotes a reserved word as a column alias - `SELECT x
    /// AS "limit"` - is now refused. That is the trade `analytics.rs` already makes explicitly for
    /// the same reason, and it is the safe direction.
    #[test]
    fn a_quoted_identifier_is_a_name_and_survives_tokenisation() {
        assert_eq!(
            sql_tokens("SELECT 'ORDER BY', \"LIMIT\" -- DISTINCT\n/* RANDOM() */"),
            vec!["SELECT", "LIMIT"],
            "the literal and the comments go; the identifier stays"
        );
        assert_eq!(
            sql_tokens("SELECT \"me\"\"dian\"(x)"),
            vec!["SELECT", "ME\"DIAN", "(", "X"],
            "an escaped inner quote is part of the name"
        );
    }

    /// #836 - the refusal list must be **closed**: every construct v1 cannot maintain is refused,
    /// and the check is that not one of them slips through.
    ///
    /// The list this replaces refused 1 of these 13. It named `MEDIAN`, `MODE` and `PERCENTILE_*`
    /// over a vocabulary DuckDB owns and grows - this build knows 88 aggregate names - so the real
    /// spellings (`quantile_cont`) were admitted while the SQL-standard alias was blocked, and any
    /// of them could be hidden behind a double quote regardless.
    #[test]
    fn every_ineligible_construct_is_refused() {
        let ineligible: &[(&str, &str)] = &[
            ("median", "SELECT 1 AS k, median(v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("quoted median", "SELECT 1 AS k, \"median\"(v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("quantile_cont", "SELECT 1 AS k, quantile_cont(v, 0.5) AS m FROM (VALUES (1),(2)) t(v)"),
            // The SQL-standard alias for the same aggregate. Present because the *old* denylist
            // named `PERCENTILE_*` and missed `quantile_cont`; this list must not have the
            // inverse hole. It is refused today only because DuckDB's parser rewrites the alias
            // to `quantile_cont` before serialisation - see
            // `the_allowlist_depends_on_the_parser_canonicalising_aliases`.
            ("percentile_cont alias", "SELECT 1 AS k, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("quantile_disc", "SELECT 1 AS k, quantile_disc(v, 0.5) AS m FROM (VALUES (1),(2)) t(v)"),
            ("approx_quantile", "SELECT 1 AS k, approx_quantile(v, 0.5) AS m FROM (VALUES (1),(2)) t(v)"),
            ("arg_max", "SELECT 1 AS k, arg_max(v, v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("first", "SELECT 1 AS k, first(v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("string_agg", "SELECT 1 AS k, string_agg(v::VARCHAR, ',') AS m FROM (VALUES (1),(2)) t(v)"),
            ("list", "SELECT 1 AS k, list(v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("histogram", "SELECT 1 AS k, histogram(v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("any_value", "SELECT 1 AS k, any_value(v) AS m FROM (VALUES (1),(2)) t(v)"),
            ("in-subquery", "SELECT v AS k FROM (VALUES (1),(2)) t(v) WHERE v IN (SELECT 1)"),
            ("scalar subquery", "SELECT v AS k, (SELECT max(w) FROM (VALUES (9)) u(w)) AS m FROM (VALUES (1)) t(v)"),
            // A *single* grouping set is deliberately absent: `GROUP BY GROUPING SETS ((v))`
            // serialises byte-identically to `GROUP BY v` because it is the same query. The
            // NULL-padding forms are the ones v1 cannot maintain.
            ("two grouping sets", "SELECT a AS k, sum(b) AS m FROM (VALUES (1,2)) t(a,b) GROUP BY GROUPING SETS ((a),(b))"),
            ("rollup", "SELECT a AS k, sum(b) AS m FROM (VALUES (1,2)) t(a,b) GROUP BY ROLLUP (a,b)"),
            ("cube", "SELECT a AS k, sum(b) AS m FROM (VALUES (1,2)) t(a,b) GROUP BY CUBE (a,b)"),
            ("using sample", "SELECT v AS k FROM (VALUES (1),(2)) t(v) USING SAMPLE 1"),
            ("distinct aggregate", "SELECT 1 AS k, count(DISTINCT v) AS m FROM (VALUES (1),(2)) t(v)"),
        ];
        let admitted: Vec<&str> = ineligible
            .iter()
            .filter(|(_, sql)| validate_sql(sql).is_ok())
            .map(|(name, _)| *name)
            .collect();
        assert!(
            admitted.is_empty(),
            "admitted as incrementally maintainable: {admitted:?}"
        );
    }

    /// **The allowlist's closure rests on the parser, not on the catalogue** - pinned here because
    /// nothing else states it and a replacement engine must reproduce it (RFC-0042 slice 3, #966).
    ///
    /// `percentile_cont` has **zero rows** in `duckdb_functions()`. The catalogue cannot classify it
    /// as an aggregate, so `aggregates_among` would return an empty set and the refusal loop would
    /// never fire. It is refused only because DuckDB's parser rewrites the alias to `quantile_cont`
    /// before `json_serialize_sql` runs, and *that* name is in the catalogue.
    ///
    /// So the gate needs two properties from its engine, and only one of them is written down: a
    /// queryable aggregate classification, **and** an AST rendered in the same vocabulary that
    /// classification uses. An engine whose parser preserved the source spelling - a purely syntactic
    /// parser, which is the common design - would admit a quantile into a DBSP circuit that cannot
    /// maintain it, with every existing test still green.
    #[test]
    fn the_allowlist_depends_on_the_parser_canonicalising_aliases() {
        let conn = Connection::open_in_memory().unwrap();

        let catalogued: i64 = conn
            .query_row(
                "SELECT count(*) FROM duckdb_functions() WHERE lower(function_name) = 'percentile_cont'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            catalogued, 0,
            "the premise of this test is that the catalogue does NOT know the alias; if DuckDB has \
             started listing `percentile_cont`, the gate no longer depends on canonicalisation and \
             this test should be re-read rather than adjusted"
        );

        let ast = plan_ast(
            &conn,
            "SELECT k, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS m \
             FROM (VALUES (1,2)) t(k,v) GROUP BY k",
        )
        .unwrap();
        let mut named = BTreeSet::new();
        function_names(&ast, &mut named);
        assert!(
            named.contains("quantile_cont"),
            "the parser must canonicalise the alias into catalogue vocabulary, got {named:?}"
        );
        assert!(
            !named.contains("percentile_cont"),
            "the source spelling must not survive into the AST, or the catalogue cannot classify it"
        );

        // And the consequence the two properties buy together.
        let err = validate_sql(
            "SELECT k, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS m \
             FROM (VALUES (1,2)) t(k,v) GROUP BY k",
        )
        .expect_err("a quantile must not be admitted as incrementally maintainable");
        assert!(
            format!("{err:#}").contains("quantile_cont"),
            "the refusal must come from the aggregate allowlist, not an unrelated gate: {err:#}"
        );
    }

    /// The other side of the same gate: the allowlist must not refuse what v1 *can* maintain, or
    /// authors route around it. A closed list that refuses everything is not a win.
    #[test]
    fn the_maintainable_aggregates_are_still_admitted() {
        for sql in [
            "SELECT k, sum(v) AS s FROM (VALUES (1,2)) t(k,v) GROUP BY k",
            "SELECT k, count(*) AS n FROM (VALUES (1,2)) t(k,v) GROUP BY k",
            "SELECT k, min(v) AS a, max(v) AS b, avg(v) AS c FROM (VALUES (1,2)) t(k,v) GROUP BY k",
            "SELECT lower(s) AS k FROM (VALUES ('A')) t(s)",
        ] {
            assert!(
                validate_sql(sql).is_ok(),
                "wrongly refused: {sql} -> {:?}",
                validate_sql(sql)
            );
        }
    }

    #[test]
    fn every_entity_sql_file_requires_a_declaration() {
        let dir = nest();
        std::fs::write(
            dir.path().join("entities/forgotten.sql"),
            "SELECT 1 AS owner",
        )
        .unwrap();
        let issues = validate(dir.path());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].error.contains("no entities.toml declaration"));
    }

    #[test]
    fn load_is_empty_only_when_the_manifest_is_absent() {
        let dir = nest();
        assert!(load(dir.path()).unwrap().is_empty());
        std::fs::write(
            dir.path().join(ENTITY_FILE),
            "[[entities]]\nname='totals'\nsql='entities/totals.sql'\nkey=['owner']\nmax_rows=10\n",
        )
        .unwrap();
        let declarations = load(dir.path()).unwrap();
        assert_eq!(declarations.len(), 1);
        assert_eq!(declarations[0].name, "totals");
        assert_eq!(declarations[0].key, vec!["owner"]);
    }

    #[test]
    fn empty_entity_manifest_is_not_a_successful_no_op() {
        let dir = nest();
        std::fs::write(dir.path().join(ENTITY_FILE), "# nothing yet\n").unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|issue| issue.error.contains("declares no entities")));
    }

    #[test]
    fn entity_name_and_filename_are_one_derivation_identity() {
        let dir = nest();
        std::fs::write(
            dir.path().join(ENTITY_FILE),
            "[[entities]]\nname='a'\nsql='entities/b.sql'\nkey=['x']\nmax_rows=1\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("entities/b.sql"), "SELECT 1 AS x").unwrap();
        assert!(validate(dir.path())
            .iter()
            .any(|i| i.error.contains("match the declared SQL filename")));
    }

    #[test]
    fn declared_key_must_be_unique_in_the_reference_result() {
        let dir = configured_nest();
        std::fs::write(
            dir.path().join(ENTITY_FILE),
            "[[entities]]\nname='totals'\nsql='entities/totals.sql'\nkey=['owner']\nmax_rows=10\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT owner FROM (VALUES (1), (1)) AS source(owner)",
        )
        .unwrap();

        let issues = validate(dir.path());
        assert!(
            issues
                .iter()
                .any(|issue| issue.error.contains("not unique")),
            "expected duplicate key to be refused, got {issues:?}"
        );
    }

    #[test]
    fn reference_result_cannot_exceed_declared_max_rows() {
        let dir = configured_nest();
        std::fs::write(
            dir.path().join(ENTITY_FILE),
            "[[entities]]\nname='totals'\nsql='entities/totals.sql'\nkey=['owner']\nmax_rows=1\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("entities/totals.sql"),
            "SELECT owner FROM (VALUES (1), (2)) AS source(owner)",
        )
        .unwrap();

        let issues = validate(dir.path());
        assert!(
            issues
                .iter()
                .any(|issue| issue.error.contains("exceeds declared max_rows (1)")),
            "expected oversized reference result to be refused, got {issues:?}"
        );
    }

    #[test]
    fn dependencies_come_from_duckdbs_ast() {
        assert_eq!(
            dependencies("SELECT a.x FROM facts a JOIN earlier e ON a.x = e.x").unwrap(),
            vec!["earlier", "facts"]
        );
    }

    #[test]
    fn dependency_cycle_names_the_actual_entity_loop() {
        let nodes = vec![
            ("a".into(), vec!["facts".into(), "b".into()]),
            ("b".into(), vec!["c".into()]),
            ("c".into(), vec!["a".into()]),
        ];
        assert_eq!(
            dependency_cycle(&nodes),
            Some(vec!["a".into(), "b".into(), "c".into(), "a".into()])
        );
    }

    #[test]
    fn validation_reports_a_declared_entity_cycle() {
        let dir = nest();
        std::fs::write(dir.path().join(ENTITY_FILE), "[[entities]]\nname='a'\nsql='entities/a.sql'\nkey=['x']\nmax_rows=1\n[[entities]]\nname='b'\nsql='entities/b.sql'\nkey=['x']\nmax_rows=1\n").unwrap();
        std::fs::write(dir.path().join("entities/a.sql"), "SELECT x FROM b").unwrap();
        std::fs::write(dir.path().join("entities/b.sql"), "SELECT x FROM a").unwrap();
        let issues = validate(dir.path());
        assert!(
            issues.iter().any(|i| i.error.contains("a -> b -> a")),
            "{issues:?}"
        );
    }

    #[test]
    fn reuse_key_changes_for_every_entity_contract_input() {
        let base = identity(
            "SELECT owner FROM facts",
            vec!["upstream".into()],
            vec![source("0xaaa")],
            vec!["owner".into()],
            vec!["owner".into()],
        )
        .unwrap();
        let key = base.reuse_key();
        assert_ne!(
            key,
            identity(
                "SELECT other FROM facts",
                vec!["upstream".into()],
                vec![source("0xaaa")],
                vec!["owner".into()],
                vec!["owner".into()]
            )
            .unwrap()
            .reuse_key()
        );
        assert_ne!(
            key,
            identity(
                "SELECT owner FROM facts",
                vec!["other-upstream".into()],
                vec![source("0xaaa")],
                vec!["owner".into()],
                vec!["owner".into()]
            )
            .unwrap()
            .reuse_key()
        );
        assert_ne!(
            key,
            identity(
                "SELECT owner FROM facts",
                vec!["upstream".into()],
                vec![source("0xbbb")],
                vec!["owner".into()],
                vec!["owner".into()]
            )
            .unwrap()
            .reuse_key()
        );
        assert_ne!(
            key,
            identity(
                "SELECT owner FROM facts",
                vec!["upstream".into()],
                vec![source("0xaaa")],
                vec!["id".into()],
                vec!["owner".into()]
            )
            .unwrap()
            .reuse_key()
        );
        assert_ne!(
            key,
            identity(
                "SELECT owner FROM facts",
                vec!["upstream".into()],
                vec![source("0xaaa")],
                vec!["owner".into()],
                vec!["owner".into(), "amount".into()]
            )
            .unwrap()
            .reuse_key()
        );
    }
}
