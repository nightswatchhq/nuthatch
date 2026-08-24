//! RFC-0041 slice one: explicit authored incremental-entity declarations and conservative refusal.

use anyhow::{bail, Result};
use serde::Deserialize;
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
    let statements = crate::analytics::split_sql_statements(sql);
    if statements.len() != 1
        || !statements[0]
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("SELECT")
    {
        bail!("entity must contain exactly one SELECT; keep other SQL as views/*.sql")
    }
    let upper = statements[0].to_ascii_uppercase();
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
