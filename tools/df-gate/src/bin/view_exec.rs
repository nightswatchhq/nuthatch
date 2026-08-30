//! **RFC-0042 slice 4 (#996): do the authored views return the *same answers* under DataFusion?**
//!
//! `view_dialect` showed no dialect gap; `view_plan` showed 16 of 24 views planning. Both are static.
//! This runs them over a **real nest's sealed segments** and compares full result sets, because a plan
//! says a query is expressible and nothing about whether the answer is right.
//!
//! **Parity before timing**, per #981 - a comparison between engines that disagree is not a
//! measurement, so a mismatch prints the differing rows and no duration.
//!
//! **The `{col}_dec` columns are synthesised, not stored.** `semantic.rs::is_bigint_storage` gives
//! every `word16`/`word32` column a `DECIMAL(38,0)` companion that the analytics layer derives at
//! query time; the Parquet holds only the raw text. A harness that skipped that would not be running
//! what nuthatch runs, so both engines get base views that add them.
//!
//! Usage: `view_exec <nest-dir>`
use std::time::Instant;

use datafusion::prelude::*;

fn is_bigint(storage: &str) -> bool {
    storage == "word16" || storage == "word32"
}

/// The base-view SQL for one table: every stored column, plus a `{col}_dec` companion for each
/// big-int column. `TRY_CAST` yields NULL past 38 digits, which is what `overflows_dec` describes.
fn base_view(table: &str, cols: &[(String, String)], glob: &str) -> String {
    let mut sel: Vec<String> = Vec::new();
    for (name, storage) in cols {
        sel.push(format!("\"{name}\""));
        if is_bigint(storage) {
            sel.push(format!(
                "TRY_CAST(\"{name}\" AS DECIMAL(38,0)) AS \"{name}_dec\""
            ));
        }
    }
    format!(
        "CREATE OR REPLACE VIEW \"{table}\" AS SELECT {} FROM read_parquet('{glob}')",
        sel.join(", ")
    )
}

fn views_in(sql: &str) -> Vec<(String, String)> {
    let body: String = sql
        .lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    let up = body.to_ascii_uppercase();
    let mut starts: Vec<usize> = up.match_indices("CREATE VIEW").map(|(i, _)| i).collect();
    if starts.is_empty() {
        return Vec::new();
    }
    starts.push(body.len());
    let mut out = Vec::new();
    for w in starts.windows(2) {
        let chunk = &body[w[0]..w[1]];
        let name: String = chunk["CREATE VIEW".len()..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let cu = chunk.to_ascii_uppercase();
        if let (false, Some(st)) = (
            name.is_empty(),
            ["SELECT", "WITH"]
                .iter()
                .filter_map(|k| cu.match_indices(k).map(|(i, _)| i).next())
                .min(),
        ) {
            out.push((
                name,
                chunk[st..].trim().trim_end_matches(';').trim().to_string(),
            ));
        }
    }
    out
}

/// `HUGEINT` is DuckDB's 128-bit integer; `DECIMAL(38,0)` is DataFusion's equivalent width. Applied
/// **only** to the DataFusion side and recorded as a cost: §3 calls requiring users to rewrite valid
/// nuthatch SQL a regression without a compatibility layer.
fn for_datafusion(sql: &str) -> String {
    sql.replace("HUGEINT", "DECIMAL(38,0)")
        .replace("hugeint", "DECIMAL(38,0)")
}

/// One DuckDB value as the same text DataFusion's `array_value_to_string` produces, so a comparison
/// is about the data rather than about two libraries' formatting conventions.
fn render(v: &duckdb::types::Value) -> String {
    use duckdb::types::Value as V;
    match v {
        V::Null => String::new(),
        V::Boolean(b) => b.to_string(),
        V::Text(t) => t.clone(),
        V::TinyInt(n) => n.to_string(),
        V::SmallInt(n) => n.to_string(),
        V::Int(n) => n.to_string(),
        V::BigInt(n) => n.to_string(),
        V::HugeInt(n) => n.to_string(),
        V::UTinyInt(n) => n.to_string(),
        V::USmallInt(n) => n.to_string(),
        V::UInt(n) => n.to_string(),
        V::UBigInt(n) => n.to_string(),
        V::Float(f) => f.to_string(),
        V::Double(f) => f.to_string(),
        V::Decimal(d) => d.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_of_duck(conn: &duckdb::Connection, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut st = conn.prepare(sql)?;
    // Column count must be read *after* execution - `column_count()` on a prepared-but-unexecuted
    // statement panics with "The statement was not executed yet". Take it from the row instead.
    let mut q = st.query([])?;
    let mut out = Vec::new();
    while let Some(r) = q.next()? {
        let mut cells = Vec::new();
        let mut i = 0usize;
        while let Ok(v) = r.get::<_, duckdb::types::Value>(i) {
            // **Display form, not `Debug`.** `{v:?}` renders `Text("0x00..")`, `HugeInt(990..)` and
            // `Null`, while DataFusion renders `0x00..`, `990..` and an empty string. Comparing those
            // reported four parity failures on runs whose row counts and values matched exactly - a
            // harness bug dressed as a finding.
            cells.push(render(&v));
            i += 1;
        }
        out.push(cells.join("\u{1f}"));
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nest = std::env::args()
        .nth(1)
        .expect("usage: view_exec <nest-dir>");
    let root = std::path::Path::new(&nest);
    let raw = std::fs::read_to_string(root.join("schema.json"))?;
    let doc: serde_json::Value = serde_json::from_str(&raw)?;

    let duck = duckdb::Connection::open_in_memory()?;
    let cfg =
        SessionConfig::new().set_bool("datafusion.sql_parser.enable_ident_normalization", false);
    let ctx = SessionContext::new_with_config(cfg);

    let mut tables = 0usize;
    for t in doc["tables"].as_array().into_iter().flatten() {
        let Some(name) = t["table"].as_str() else {
            continue;
        };
        let glob = root.join("segments").join(format!("{name}-*.parquet"));
        let glob = glob.to_string_lossy().to_string();
        if glob::glob(&glob)?.next().is_none() {
            continue; // a declared table with nothing sealed yet
        }
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
        let ddl = base_view(name, &cols, &glob);
        duck.execute_batch(&ddl)?;
        // DataFusion has no `read_parquet`; register the glob then define the same projection.
        ctx.register_parquet(
            format!("{name}__raw"),
            &glob,
            ParquetReadOptions::default().parquet_pruning(true),
        )
        .await?;
        let proj = ddl
            .split_once(" AS SELECT ")
            .map(|(_, r)| r)
            .unwrap_or_default()
            .rsplit_once(" FROM ")
            .map(|(l, _)| l.to_string())
            .unwrap_or_default();
        ctx.sql(&format!(
            "CREATE OR REPLACE VIEW \"{name}\" AS SELECT {proj} FROM \"{name}__raw\""
        ))
        .await?
        .collect()
        .await?;
        tables += 1;
    }
    println!("registered {tables} tables from {}", root.display());

    let mut vdir: Vec<_> = std::fs::read_dir(root.join("views"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    vdir.sort();

    let (mut ok, mut mismatch, mut errored) = (0usize, 0usize, 0usize);
    for f in vdir {
        for (name, select) in views_in(&std::fs::read_to_string(&f)?) {
            // **Medians, not single runs.** `docs/bench/noise-floor.md`: "A single measurement is
            // worthless here" - compare medians of at least 15 runs, never one, never the mean.
            // REPEATS is lower by default because these are seconds-scale queries over 38k segments.
            let repeats: usize = std::env::var("REPEATS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5)
                .max(1);
            let mut d_times = Vec::new();
            let mut d = Vec::new();
            let mut duck_failed = None;
            for _ in 0..repeats {
                let t = Instant::now();
                match rows_of_duck(&duck, &select) {
                    Ok(v) => {
                        d_times.push(t.elapsed().as_millis());
                        d = v;
                    }
                    Err(e) => {
                        duck_failed = Some(e.to_string());
                        break;
                    }
                }
            }
            if let Some(e) = duck_failed {
                println!("DUCK-ERR\t{name}\t{}", e.lines().next().unwrap_or(""));
                errored += 1;
                continue;
            }
            d_times.sort_unstable();
            let d_ms = d_times[d_times.len() / 2];

            let df_sql = for_datafusion(&select);
            let rewritten = df_sql != select;
            let mut f_times = Vec::new();
            let t = Instant::now();
            let f_rows = match ctx.sql(&df_sql).await {
                Ok(df) => {
                    match df.collect().await {
                        Ok(b) => {
                            let mut v = Vec::new();
                            for batch in b {
                                for i in 0..batch.num_rows() {
                                    let mut cells = Vec::new();
                                    for c in batch.columns() {
                                        cells.push(
                                        datafusion::arrow::util::display::array_value_to_string(c, i)
                                            .unwrap_or_default(),
                                    );
                                    }
                                    v.push(cells.join("\u{1f}"));
                                }
                            }
                            v
                        }
                        Err(e) => {
                            println!(
                                "DF-EXEC-ERR\t{name}\t{}",
                                e.to_string().lines().next().unwrap_or("")
                            );
                            errored += 1;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "DF-PLAN-ERR\t{name}\t{}",
                        e.to_string().lines().next().unwrap_or("")
                    );
                    errored += 1;
                    continue;
                }
            };
            f_times.push(t.elapsed().as_millis());
            // Repeat the DataFusion side the same number of times, discarding the rows - parity was
            // established on the first execution and re-comparing every repeat would time the
            // comparison rather than the query.
            for _ in 1..repeats {
                let t = Instant::now();
                if let Ok(df) = ctx.sql(&df_sql).await {
                    if df.collect().await.is_ok() {
                        f_times.push(t.elapsed().as_millis());
                    }
                }
            }
            f_times.sort_unstable();
            let f_ms = f_times[f_times.len() / 2];

            // Sets, not sequences: an authored view without ORDER BY has no defined row order, so
            // comparing sequences would report an ordering difference as a wrong answer.
            let (mut a, mut b) = (d.clone(), f_rows.clone());
            a.sort();
            b.sort();
            if a == b {
                ok += 1;
                // Register it in both engines, as nuthatch does - `port_queue` reads
                // `deployment_signal`, and without this every dependent view fails with "Table with
                // name X does not exist", which reads as a corpus gap and is not one.
                let _ =
                    duck.execute_batch(&format!("CREATE OR REPLACE VIEW \"{name}\" AS {select}"));
                let _ = ctx
                    .sql(&format!("CREATE OR REPLACE VIEW \"{name}\" AS {df_sql}"))
                    .await;
                println!(
                    "PARITY-OK\t{name}\trows={}\tduck_ms={d_ms}\tdf_ms={f_ms}\trewritten={rewritten}",
                    d.len()
                );
            } else {
                mismatch += 1;
                let first: Vec<String> = a
                    .iter()
                    .zip(b.iter())
                    .filter(|(x, y)| x != y)
                    .take(2)
                    .map(|(x, y)| format!("duck[{x}] df[{y}]"))
                    .collect();
                println!(
                    "PARITY-FAIL\t{name}\tduck_rows={}\tdf_rows={}\tfirst={first:?}",
                    d.len(),
                    f_rows.len()
                );
            }
        }
    }
    println!(
        "\nviews_run={} parity_ok={ok} parity_fail={mismatch} errored={errored}",
        ok + mismatch + errored
    );
    Ok(())
}
