//! RFC-0042 slice 3, role 2: does a candidate engine's AST speak its own registry's vocabulary?
//!
//! `entities.rs` refuses an aggregate the v1 DBSP lowerer cannot maintain by asking the engine which
//! of the names a statement mentions are aggregates. That gate is only closed if the **name in the
//! AST** is the name the **catalogue** classifies. DuckDB gets this right by rewriting aliases during
//! parse: `percentile_cont` (zero catalogue rows) becomes `quantile_cont` (26 rows, aggregate) before
//! `json_serialize_sql` runs.
//!
//! `sqlparser-rs` is a syntactic parser with no catalogue to canonicalise against, so the question is
//! whether DataFusion's *logical planning* closes the gap. This probe asks, for each alias pair, what
//! the raw AST says, what the planned expression says, and what the function registry knows.

use std::collections::BTreeSet;

use datafusion::prelude::*;
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;

/// Every function-call name in the raw sqlparser AST, found by walking the Debug rendering - crude,
/// but it cannot miss a nesting depth and needs no visitor API that may move between versions.
fn ast_function_names(sql: &str) -> Vec<String> {
    let ast = match Parser::parse_sql(&GenericDialect {}, sql) {
        Ok(a) => a,
        Err(e) => return vec![format!("<parse error: {e}>")],
    };
    let dbg = format!("{ast:?}");
    let mut out = BTreeSet::new();
    // sqlparser renders a call as `Function { name: ObjectName([Identifier(Ident { value: "sum", ...`
    let mut rest = dbg.as_str();
    while let Some(i) = rest.find("Function { name: ObjectName(") {
        rest = &rest[i + "Function { name: ObjectName(".len()..];
        if let Some(j) = rest.find("value: \"") {
            let after = &rest[j + "value: \"".len()..];
            if let Some(k) = after.find('"') {
                out.insert(after[..k].to_ascii_lowercase());
            }
        }
    }
    out.into_iter().collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ctx = SessionContext::new();
    ctx.sql("CREATE TABLE t (k INT, v DOUBLE) AS VALUES (1, 2.0), (1, 4.0)")
        .await?
        .collect()
        .await?;

    let state = ctx.state();
    let registry: BTreeSet<String> = state.aggregate_functions().keys().cloned().collect();
    println!("registry knows {} aggregate names", registry.len());

    if std::env::args().any(|a| a == "--list") {
        for n in &registry {
            println!("DF_AGG\t{n}");
        }
        // DuckDB files window functions under `aggregate` in `duckdb_functions()`; DataFusion keeps a
        // separate registry. Comparing only the aggregate lists overstates the gap by every window
        // function DataFusion has.
        for n in state.window_functions().keys() {
            println!("DF_WIN\t{}", n.to_ascii_lowercase());
        }
        for n in state.scalar_functions().keys() {
            println!("DF_SCALAR\t{}", n.to_ascii_lowercase());
        }
        return Ok(());
    }

    // Alias pairs: the SQL-standard or short spelling, and the name DataFusion registers it under.
    let cases: &[(&str, &str)] = &[
        ("stddev",              "SELECT k, stddev(v) AS m FROM t GROUP BY k"),
        ("stddev_samp",         "SELECT k, stddev_samp(v) AS m FROM t GROUP BY k"),
        ("var",                 "SELECT k, var(v) AS m FROM t GROUP BY k"),
        ("var_samp",            "SELECT k, var_samp(v) AS m FROM t GROUP BY k"),
        ("median",              "SELECT k, median(v) AS m FROM t GROUP BY k"),
        ("approx_median",       "SELECT k, approx_median(v) AS m FROM t GROUP BY k"),
        ("array_agg",           "SELECT k, array_agg(v) AS m FROM t GROUP BY k"),
        ("percentile_cont",     "SELECT k, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS m FROM t GROUP BY k"),
        ("sum (control)",       "SELECT k, sum(v) AS m FROM t GROUP BY k"),
    ];

    println!("\n{:<18} {:<24} {:<10} {}", "case", "ast names", "in reg?", "planned expression");
    println!("{}", "-".repeat(110));
    for (label, sql) in cases {
        let ast = ast_function_names(sql);
        let in_reg: Vec<String> = ast
            .iter()
            .map(|n| format!("{n}={}", registry.contains(n)))
            .collect();
        let planned = match ctx.sql(sql).await {
            Ok(df) => {
                let plan = format!("{}", df.logical_plan().display_indent());
                plan.lines()
                    .find(|l| l.contains("Aggregate:"))
                    .unwrap_or("<no Aggregate node>")
                    .trim()
                    .to_string()
            }
            Err(e) => format!("<plan error: {}>", e.to_string().lines().next().unwrap_or("")),
        };
        println!("{:<18} {:<24} {:<10} {}", label, ast.join(","), in_reg.join(","), planned);
    }
    Ok(())
}
