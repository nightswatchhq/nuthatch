//! RFC-0042 slice 6, experiment A4: how many distinct plan shapes does the admitted query surface
//! actually produce?
//!
//! The question A4 settles is whether a Rust-native path is *a layer of operators over a rented
//! executor* or *an engine*. A small, closed set of shapes is the first; a long tail is the second,
//! and the second is a quantified blocker whatever A1-A3 say.
//!
//! Method. Define the base-table views exactly as `analytics::define_views` does - `SELECT *` plus
//! the derived `_dec`/`_overflow` casts for every `word16`/`word32` column, over
//! `read_parquet([...], union_by_name=true)` - then load the nest's authored `views/*.sql` and any
//! `checks/*.sql`, and `EXPLAIN (FORMAT JSON)` each one against the same DuckDB the product links.
//! A plan is reduced to its operator kinds and nesting; names, predicates, constants, cardinalities
//! and file lists are stripped, so two queries differing only in which column they sum collapse to
//! one shape. That reduction is the whole point: it counts *shapes a Rust path would have to
//! implement*, not queries.
//!
//! Deliberately uses the `duckdb` crate at the version nuthatch links, not a system CLI: a plan is
//! an engine-version artefact and comparing against a different build would measure the wrong
//! optimiser.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use duckdb::Connection;
use serde_json::Value;

fn is_bigint(storage: &str) -> bool {
    storage == "word16" || storage == "word32"
}

/// The operator tree, reduced to kinds and nesting. Everything identifying *which* table, column or
/// constant is dropped; what survives is the structure an executor must be able to run.
fn shape(node: &Value) -> String {
    let name = node
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_ascii_uppercase();
    let empty = vec![];
    let kids: Vec<String> = node
        .get("children")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty)
        .iter()
        .map(shape)
        .collect();
    if kids.is_empty() {
        name
    } else {
        format!("{name}({})", kids.join(","))
    }
}

fn operators(node: &Value, into: &mut BTreeSet<String>) {
    if let Some(n) = node.get("name").and_then(|v| v.as_str()) {
        into.insert(n.to_ascii_uppercase());
    }
    if let Some(ks) = node.get("children").and_then(|v| v.as_array()) {
        for k in ks {
            operators(k, into);
        }
    }
}

fn main() -> anyhow::Result<()> {
    let dir: PathBuf = std::env::var("NEST").expect("NEST=<nest dir>").into();
    let conn = Connection::open_in_memory()?;

    // 1. Base-table views over the sealed segments, built the way the product builds them.
    let schema: Value = serde_json::from_slice(&std::fs::read(dir.join("schema.json"))?)?;
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(dir.join("segments/manifest.json"))?)?;
    // **The table name is `schema.json`'s own `table` field.** Reconstructing it as
    // `{alias}__{event}` yields `service__IndexingRewardsCollected(address,...)` - the event
    // *signature*, not the snake_case table - which silently matches no manifest key, so every
    // `_dec` column goes missing and four authored views fail to bind on columns that do exist.
    let mut cols_of: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for t in schema["tables"].as_array().unwrap() {
        let table = t["table"].as_str().unwrap().to_string();
        let cols: Vec<(String, String)> = t["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| {
                (
                    c["name"].as_str().unwrap().to_string(),
                    c["storage"].as_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        cols_of.insert(table, cols);
    }

    let mut defined = 0usize;
    let mut skipped: Vec<String> = vec![];
    for (table, segs) in manifest["tables"].as_object().unwrap() {
        let files: Vec<String> = segs
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                format!(
                    "'{}'",
                    dir.join("segments")
                        .join(s["file"].as_str().unwrap())
                        .display()
                )
            })
            .collect();
        if files.is_empty() {
            continue;
        }
        let dec: String = cols_of
            .get(table)
            .map(|cs| {
                cs.iter()
                    .filter(|(_, st)| is_bigint(st))
                    .map(|(c, _)| {
                        format!(
                            ", TRY_CAST(\"{c}\" AS DECIMAL(38,0)) AS \"{c}_dec\", (\"{c}\" IS NOT NULL AND TRY_CAST(\"{c}\" AS DECIMAL(38,0)) IS NULL) AS \"{c}_overflow\""
                        )
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        let ddl = format!(
            "CREATE OR REPLACE VIEW \"{table}\" AS SELECT *{dec} FROM read_parquet([{}], union_by_name=true)",
            files.join(", ")
        );
        match conn.execute_batch(&ddl) {
            Ok(_) => defined += 1,
            Err(e) => skipped.push(format!("{table}: {e}")),
        }
    }
    // Declared-but-never-sealed tables get the empty typed view `define_views` gives them, or an
    // authored view referencing one fails to bind and the count of shapes is short by however many
    // views touch a table whose event has not fired yet (#663). Typed by column NAME, as
    // `empty_view_ddl` does, not by storage - see COR-4 in `analytics.rs`.
    let mut empty = 0usize;
    let sealed: BTreeSet<&str> = manifest["tables"]
        .as_object()
        .unwrap()
        .keys()
        .map(|s| s.as_str())
        .collect();
    for (table, cols) in &cols_of {
        if sealed.contains(table.as_str()) || cols.is_empty() {
            continue;
        }
        let sel: Vec<String> = cols
            .iter()
            .flat_map(|(name, storage)| {
                let ty = if matches!(name.as_str(), "block_number" | "log_index" | "_seq" | "block_timestamp") {
                    "UBIGINT"
                } else {
                    "VARCHAR"
                };
                let mut v = vec![format!("CAST(NULL AS {ty}) AS \"{name}\"")];
                if is_bigint(storage) {
                    v.push(format!("CAST(NULL AS DECIMAL(38,0)) AS \"{name}_dec\""));
                    v.push(format!("CAST(NULL AS BOOLEAN) AS \"{name}_overflow\""));
                }
                v
            })
            .collect();
        let ddl = format!(
            "CREATE OR REPLACE VIEW \"{table}\" AS SELECT {} WHERE false",
            sel.join(", ")
        );
        match conn.execute_batch(&ddl) {
            Ok(_) => empty += 1,
            Err(e) => skipped.push(format!("{table} (empty): {e}")),
        }
    }
    println!("BASE\tsealed_views={defined}\tempty_views={empty}\tskipped={}", skipped.len());
    for s in &skipped {
        println!("BASE-SKIP\t{s}");
    }

    // 2. The authored views, in filename order, exactly as `define_nest_views` loads them.
    let mut view_files: Vec<PathBuf> = std::fs::read_dir(dir.join("views"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    view_files.sort();
    let mut view_names: Vec<String> = vec![];
    for f in &view_files {
        let body = std::fs::read_to_string(f)?;
        for stmt in body.split(';') {
            let s = stmt.trim();
            if s.is_empty() {
                continue;
            }
            let lower = s.to_ascii_lowercase();
            if let Some(i) = lower.find("create view ") {
                let rest = &s[i + "create view ".len()..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let runnable = format!("CREATE OR REPLACE VIEW{}", &s[i + "create view".len()..]);
                match conn.execute_batch(&runnable) {
                    Ok(_) => view_names.push(name),
                    Err(e) => println!("VIEW-FAIL\t{name}\t{e}"),
                }
            }
        }
    }
    println!("VIEWS\tloaded={}\t{}", view_names.len(), view_names.join(","));

    // 3. The admitted query set: one `SELECT * FROM <view>` per authored view (which is what a
    //    caller asking for that view by name executes), plus every `checks/*.sql` statement.
    let mut queries: Vec<(String, String)> = view_names
        .iter()
        .map(|v| (format!("view:{v}"), format!("SELECT * FROM \"{v}\"")))
        .collect();
    let checks = dir.join("checks");
    if checks.is_dir() {
        let mut cf: Vec<PathBuf> = std::fs::read_dir(&checks)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        cf.sort();
        for f in cf {
            let body = std::fs::read_to_string(&f)?;
            let name = f.file_stem().unwrap().to_string_lossy().to_string();
            for (i, stmt) in body.split(';').enumerate() {
                let s = stmt.trim();
                if s.is_empty() || s.starts_with("--") && !s.to_ascii_lowercase().contains("select")
                {
                    continue;
                }
                // Strip leading comment lines so the statement starts at its keyword.
                let cleaned: String = s
                    .lines()
                    .filter(|l| !l.trim_start().starts_with("--"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if cleaned.trim().is_empty() {
                    continue;
                }
                queries.push((format!("check:{name}#{i}"), cleaned));
            }
        }
    }
    // Any extra statements the caller wants counted (e.g. a /sql request log), one per line.
    if let Ok(extra) = std::env::var("EXTRA_SQL_FILE") {
        if Path::new(&extra).exists() {
            for (i, line) in std::fs::read_to_string(&extra)?.lines().enumerate() {
                let l = line.trim();
                if !l.is_empty() && !l.starts_with('#') {
                    queries.push((format!("log:{i}"), l.to_string()));
                }
            }
        }
    }

    // 4. Explain each, reduce to a shape, count distinct.
    let mut shapes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut all_ops: BTreeSet<String> = BTreeSet::new();
    let mut failed = 0usize;
    for (label, sql) in &queries {
        let mut st = match conn.prepare(&format!("EXPLAIN (FORMAT JSON) {sql}")) {
            Ok(s) => s,
            Err(e) => {
                println!("EXPLAIN-FAIL\t{label}\t{e}");
                failed += 1;
                continue;
            }
        };
        let mut rows = st.query([])?;
        let mut plan_json = String::new();
        while let Some(r) = rows.next()? {
            // EXPLAIN returns (explain_key, explain_value); the JSON is in the value column.
            if let Ok(v) = r.get::<_, String>(1) {
                plan_json = v;
            }
        }
        let parsed: Value = match serde_json::from_str(&plan_json) {
            Ok(v) => v,
            Err(e) => {
                println!("PARSE-FAIL\t{label}\t{e}\t{}", &plan_json.chars().take(120).collect::<String>());
                failed += 1;
                continue;
            }
        };
        let root = if parsed.is_array() {
            parsed.as_array().unwrap().first().cloned().unwrap_or(Value::Null)
        } else {
            parsed
        };
        operators(&root, &mut all_ops);
        shapes.entry(shape(&root)).or_default().push(label.clone());
    }

    println!("\n== A4 result ==");
    println!("queries_explained\t{}", queries.len() - failed);
    println!("queries_failed\t{failed}");
    println!("distinct_shapes\t{}", shapes.len());
    println!("distinct_operators\t{}", all_ops.len());
    println!("operators\t{}", all_ops.iter().cloned().collect::<Vec<_>>().join(","));
    println!("\n== shapes ==");
    for (i, (sh, qs)) in shapes.iter().enumerate() {
        println!("SHAPE\t{}\tn={}\tqueries={}\t{}", i + 1, qs.len(), qs.join("|"), sh);
    }
    Ok(())
}
