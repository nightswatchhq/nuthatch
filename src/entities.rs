//! RFC-0041 slice one: explicit authored incremental-entity declarations and conservative refusal.

use anyhow::{bail, Result};
use duckdb::Connection;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

pub const ENTITY_FILE: &str = "entities.toml";

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
        } else if let Some(schema) = &schema {
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
                        match crate::analytics::query(dir, &sql) {
                            Ok(rows) => {
                                let mut seen = BTreeSet::new();
                                for row in rows {
                                    let values: Vec<&Value> =
                                        entity.key.iter().filter_map(|key| row.get(key)).collect();
                                    if values.len() != entity.key.len()
                                        || values.iter().any(|value| value.is_null())
                                    {
                                        issues.push(issue(
                                            &name,
                                            "declared key is nullable in the reference result",
                                        ));
                                        break;
                                    }
                                    if !seen
                                        .insert(serde_json::to_string(&values).unwrap_or_default())
                                    {
                                        issues.push(issue(
                                            &name,
                                            "declared key is not unique in the reference result",
                                        ));
                                        break;
                                    }
                                }
                            }
                            Err(e) => issues
                                .push(issue(&name, format!("entity reference query failed: {e}"))),
                        }
                    }
                }
                Err(e) => issues.push(issue(&name, format!("entity SQL does not bind: {e}"))),
            }
        }
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
    let upper = sql.to_ascii_uppercase();
    for (needle, why) in [
        (" DISTINCT ", "DISTINCT"),
        (" ORDER BY ", "ORDER BY"),
        (" LIMIT ", "LIMIT"),
        (" OVER ", "window functions"),
        (" LEFT JOIN ", "outer joins"),
        (" RIGHT JOIN ", "outer joins"),
        (" FULL JOIN ", "outer joins"),
        (" RECURSIVE ", "recursive CTEs"),
    ] {
        if upper.contains(needle) {
            bail!("{why} is not incremental v1 SQL; keep this as views/*.sql")
        }
    }
    Ok(())
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
}
