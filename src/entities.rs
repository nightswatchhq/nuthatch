//! RFC-0041 slice one: explicit authored incremental-entity declarations and conservative refusal.

use anyhow::{bail, Result};
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
    let conn = crate::graft::parser_connection()?;
    Ok(EntityIdentity {
        plan: crate::graft::canonical_plan(&conn, sql),
        input_entity_keys,
        sources,
        key,
        output_schema,
        engine: crate::graft::engine_version(&conn),
    })
}

#[derive(Debug, Deserialize)]
struct EntityFile {
    #[serde(default)]
    entities: Vec<EntityDecl>,
}

#[derive(Debug, Deserialize)]
struct EntityDecl {
    name: String,
    sql: String,
    key: Vec<String>,
    max_rows: usize,
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
            State::Code if matches!(ch, '\'' | '"') => {
                flush(&mut current, &mut tokens);
                state = State::Quote(ch);
            }
            State::Code if ch.is_ascii_alphanumeric() || ch == '_' => current.push(ch),
            State::Code if ch == '(' => {
                flush(&mut current, &mut tokens);
                tokens.push("(".into());
            }
            State::Code => flush(&mut current, &mut tokens),
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
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let raw: String =
        conn.query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get(0)
        })?;
    let ast: Value = serde_json::from_str(&raw)?;
    Ok(crate::graft::table_refs(&ast))
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

    #[test]
    fn refusal_tokens_ignore_comments_and_quoted_text() {
        assert_eq!(
            sql_tokens("SELECT 'ORDER BY', \"LIMIT\" -- DISTINCT\n/* RANDOM() */"),
            vec!["SELECT"]
        );
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
