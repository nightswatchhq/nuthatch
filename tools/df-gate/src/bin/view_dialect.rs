//! **RFC-0042 slice 4: can DataFusion read the SQL our nests actually declare?**
//!
//! #964 measured DataFusion on `net_balances` and found it 2.5-2.9x slower. But in §5.3's composed
//! path the heavy fold goes to a specialised operator (#987), so DataFusion would only ever carry
//! *general and ad-hoc* SQL - a different workload from the one that number describes.
//!
//! Before timing anything, the cheaper question: **can it parse what we already ship?** A dialect gap
//! is a hard blocker that no amount of speed fixes, and it is answerable in seconds.
//!
//! Usage: `view_dialect <file>...` - one authored `views/*.sql` per argument.
use datafusion::sql::sqlparser::dialect::GenericDialect;
use datafusion::sql::sqlparser::parser::Parser;

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    let (mut duck_ok, mut df_ok, mut both, mut n) = (0usize, 0usize, 0usize, 0usize);
    let conn = duckdb::Connection::open_in_memory().expect("duckdb");

    for f in &files {
        // **Loud, not skipped.** The first run of this probe reported `files=0` because every path
        // was relative to a different directory and an unreadable file silently `continue`d - a
        // confident zero from a probe that never read anything.
        let sql = match std::fs::read_to_string(f) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("UNREADABLE\t{f}\t{e}");
                std::process::exit(2);
            }
        };
        // Strip line comments; a bare comment file is not a query.
        let body: String = sql
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        if body.trim().is_empty() {
            continue;
        }
        n += 1;

        // **Compare like with like: the SELECT body, not the DDL wrapper.**
        //
        // `json_serialize_sql` returns `{"error":true,"error_type":"not implemented","error_message":
        // "Only SELECT statements can be serialized to json!"}` for a `CREATE VIEW`. That is a
        // serialisation limit, not a parse failure - and reading it as one made an earlier run of this
        // probe report DuckDB rejecting 18 of its own shipped views, which should have been
        // unbelievable on its face. An authored view is `CREATE VIEW <name> AS <select>`, so the
        // comparable unit is the select.
        // Take from the first `SELECT` or `WITH` token. **Not `find(" AS ")`** - that needs a trailing
        // space and a view header ends `AS\n`, so it skipped past and matched the ` AS ` inside
        // `CAST(log_index AS VARCHAR)` in the select list, handing the parsers a fragment. Both
        // engines then "rejected" 18 of 27 files, agreeing with each other and with nothing real.
        let up = body.to_ascii_uppercase();
        let start = ["SELECT", "WITH"]
            .iter()
            .filter_map(|kw| {
                up.match_indices(kw).find(|(i, _)| {
                    let before_ok = *i == 0 || !up.as_bytes()[i - 1].is_ascii_alphanumeric();
                    let after = i + kw.len();
                    let after_ok = up
                        .as_bytes()
                        .get(after)
                        .is_none_or(|c| !c.is_ascii_alphanumeric());
                    before_ok && after_ok
                })
            })
            .map(|(i, _)| i)
            .min()
            .unwrap_or(0);
        let select = body[start..].trim().trim_end_matches(';').to_string();
        let lit = format!("'{}'", select.replace('\'', "''"));
        let d = conn
            .query_row::<String, _, _>(&format!("SELECT json_serialize_sql({lit})"), [], |r| {
                r.get(0)
            })
            .map(|j| !j.contains("\"error\":true"))
            .unwrap_or(false);

        let p = Parser::parse_sql(&GenericDialect {}, &select);
        let f_ok = p.is_ok();

        if d {
            duck_ok += 1;
        }
        if f_ok {
            df_ok += 1;
        }
        if d && f_ok {
            both += 1;
        }
        if d && !f_ok {
            let e = p.err().map(|e| e.to_string()).unwrap_or_default();
            println!("DIALECT-GAP\t{f}\t{}", e.lines().next().unwrap_or(""));
        }
        if !d {
            println!(
                "DUCK-REJECTS\t{f}\t(reference rejected it - not counted against the candidate)"
            );
        }
    }
    println!("\nfiles={n}  duckdb_parses={duck_ok}  datafusion_parses={df_ok}  both={both}");
}
