//! **RFC-0042 slice 4: can DataFusion *plan* the views our nests declare, not merely parse them?**
//!
//! `view_dialect` established there is no dialect gap - DataFusion's parser reads all 27 authored view
//! bodies. Parsing is the cheap half. This is the half that finds missing functions, unsupported
//! constructs and type mismatches, by registering each nest's real tables and asking DataFusion to
//! build a logical plan for every view.
//!
//! **The stub schema is nuthatch's own, not a guess.** `seal.rs` builds every sealed batch with
//! `block_number`, `log_index`, `_seq` and `block_timestamp` as non-null `UInt64` and **every other
//! column as nullable `Utf8`** - uint256, addresses and hashes all ship as strings. A stub built from
//! `schema.json`'s `storage` field instead would invent types nuthatch never writes and fail plans for
//! a reason that does not exist.
//!
//! Views are registered as they plan, in filename order (`10-`, `20-`), because a later view may read
//! an earlier one - which is how nuthatch resolves them.
//!
//! Usage: `view_plan <nest-dir>...`
use std::sync::Arc;

use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::prelude::*;

/// `(name, storage)` pairs from `schema.json`, plus the derived columns nuthatch adds that the schema
/// file does not list.
///
/// **The `{col}_dec` companions are not optional.** `semantic.rs::is_bigint_storage` gives every
/// `word16`/`word32` column a decimal companion, created by the analytics layer rather than written
/// into `schema.json` - and authored views use them, because summing the raw text column is the
/// footgun that companion exists to remove. A stub built from `schema.json` alone reports
/// `No field named tokens_dec` and looks exactly like a DataFusion gap. It is not one.
fn stub_schema(cols: &[(String, String)]) -> Schema {
    let mut fields = Vec::new();
    for (name, storage) in cols {
        if matches!(
            name.as_str(),
            "block_number" | "log_index" | "_seq" | "block_timestamp"
        ) {
            fields.push(Field::new(name, DataType::UInt64, false));
        } else {
            // Everything else ships as text - `seal.rs` writes uint256, addresses and hashes as Utf8.
            fields.push(Field::new(name, DataType::Utf8, true));
        }
        if storage == "word16" || storage == "word32" {
            fields.push(Field::new(
                format!("{name}_dec"),
                DataType::Decimal128(38, 0),
                true,
            ));
        }
    }
    Schema::new(fields)
}

/// Split a `views/*.sql` file into its `CREATE VIEW <name> AS <select>` pairs.
fn views_in(sql: &str) -> Vec<(String, String)> {
    let body: String = sql
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut out = Vec::new();
    let up = body.to_ascii_uppercase();
    let mut starts: Vec<usize> = up.match_indices("CREATE VIEW").map(|(i, _)| i).collect();
    if starts.is_empty() {
        return out;
    }
    starts.push(body.len());
    for w in starts.windows(2) {
        let chunk = &body[w[0]..w[1]];
        let after = &chunk["CREATE VIEW".len()..];
        let name: String = after
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // Take from the first SELECT/WITH token - *not* `find(" AS ")`, which needs a trailing space
        // and so skips a header ending `AS\n`, matching the ` AS ` inside `CAST(x AS VARCHAR)` instead.
        let cu = chunk.to_ascii_uppercase();
        let start = ["SELECT", "WITH"]
            .iter()
            .filter_map(|kw| cu.match_indices(kw).map(|(i, _)| i).next())
            .min();
        if let (false, Some(st)) = (name.is_empty(), start) {
            out.push((
                name,
                chunk[st..].trim().trim_end_matches(';').trim().to_string(),
            ));
        }
    }
    out
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut total, mut planned) = (0usize, 0usize);
    for nest in std::env::args().skip(1) {
        let schema_path = std::path::Path::new(&nest).join("schema.json");
        let Ok(raw) = std::fs::read_to_string(&schema_path) else {
            eprintln!("NO-SCHEMA\t{nest}");
            continue;
        };
        let doc: serde_json::Value = serde_json::from_str(&raw)?;
        // **Identifier normalisation off.** DataFusion lowercases unquoted identifiers by default;
        // DuckDB matches them case-insensitively against the stored name. Our columns are camelCase
        // Solidity event parameters (`subgraphID`, `newIssuancePerBlock`), so the default rejects
        // authored SQL that DuckDB accepts. Testing with it disabled answers whether that is a
        // limitation or a setting - reporting a gap a config option closes would be unfair to the
        // candidate.
        let cfg = SessionConfig::new()
            .set_bool("datafusion.sql_parser.enable_ident_normalization", false);
        let ctx = SessionContext::new_with_config(cfg);

        for t in doc["tables"].as_array().into_iter().flatten() {
            let Some(name) = t["table"].as_str() else {
                continue;
            };
            let cols: Vec<(String, String)> = t["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some((
                        c["name"].as_str()?.to_string(),
                        c["storage"].as_str().unwrap_or("").to_string(),
                    ))
                })
                .collect();
            if cols.is_empty() {
                continue;
            }
            let mem = datafusion::datasource::MemTable::try_new(
                Arc::new(stub_schema(&cols)),
                vec![vec![]],
            )?;
            ctx.register_table(name, Arc::new(mem))?;
        }

        let vdir = std::path::Path::new(&nest).join("views");
        let mut files: Vec<_> = std::fs::read_dir(&vdir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "sql"))
            .collect();
        files.sort();

        for f in files {
            let sql = std::fs::read_to_string(&f)?;
            for (name, select) in views_in(&sql) {
                total += 1;
                match ctx.sql(&select).await {
                    Ok(df) => {
                        planned += 1;
                        // Register it so a later view in this nest can read it, as nuthatch does.
                        let s: Schema = df.schema().as_arrow().clone();
                        let mem =
                            datafusion::datasource::MemTable::try_new(Arc::new(s), vec![vec![]])?;
                        let _ = ctx.register_table(name.as_str(), Arc::new(mem));
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        println!(
                            "PLAN-FAIL\t{}\t{name}\t{}",
                            f.display(),
                            msg.lines().next().unwrap_or("")
                        );
                    }
                }
            }
        }
    }
    println!(
        "\nviews={total}  datafusion_plans={planned}  failed={}",
        total - planned
    );
    Ok(())
}
