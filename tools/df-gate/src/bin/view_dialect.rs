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

        // **Compare like with like: the SELECT body, not the DDL wrapper.**
        //
        // `json_serialize_sql` returns `{"error":true,"error_type":"not implemented","error_message":
        // "Only SELECT statements can be serialized to json!"}` for a `CREATE VIEW`. That is a
        // serialisation limit, not a parse failure - and reading it as one made an earlier run of this
        // probe report DuckDB rejecting 18 of its own shipped views, which should have been
        // unbelievable on its face. An authored view is `CREATE VIEW <name> AS <select>`, so the
        // comparable unit is the select.
        //
        // **#1013: one file can hold several views, and this used to take one blob per file.** It ran
        // from the first `SELECT` to the file's final semicolon, so `50-lodestar-epochs.sql` - which
        // defines `epoch_boundaries` and `lodestar_epochs` - handed both engines a 2,371-character
        // string containing `SELECT ...; CREATE VIEW ... AS SELECT ...` as a single supposed view
        // body. Whatever the two parsers then did, they were not doing the per-view comparison the
        // published counts claim. Each `CREATE VIEW` body is now extracted independently, and the
        // unit of the count is a **view** rather than a file.
        for (view_name, select) in view_bodies(&body) {
            n += 1;
            let label = format!("{f}::{view_name}");
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
                println!("DIALECT-GAP\t{label}\t{}", e.lines().next().unwrap_or(""));
            }
            if !d {
                println!(
                    "DUCK-REJECTS\t{label}\t(reference rejected it - not counted against the candidate)"
                );
            }
        }
    }
    println!("\nviews={n}  duckdb_parses={duck_ok}  datafusion_parses={df_ok}  both={both}");
}

/// Split an authored views file into `(view name, select body)` per `CREATE VIEW`.
///
/// #1013. The previous extraction took one span per *file* - first `SELECT` to final semicolon -
/// which silently concatenated every view in a multi-view file into one supposed body. Two of the
/// nests we ship views for have such a file.
///
/// Deliberately a small scanner rather than a SQL parse: the whole point of this probe is to find
/// out what each engine's parser accepts, so parsing the input with one of them first would beg the
/// question. It looks for `CREATE [OR REPLACE] VIEW <name> AS`, takes everything to the next such
/// header or end of input, and trims a trailing `;`.
///
/// A file with no `CREATE VIEW` at all is treated as a single bare select, which is what the
/// standalone query files look like.
fn view_bodies(body: &str) -> Vec<(String, String)> {
    // **Positions inside a string literal or a quoted identifier are not code** (#1023, caught in
    // review). The first version of this scanner searched the raw uppercased text, so
    //
    //     CREATE VIEW v AS SELECT 'CREATE VIEW fake AS SELECT 1;' AS text;
    //
    // reported **two** views, invented one called `fake`, and truncated `v` at the literal - after
    // which both failed to parse on both engines. A probe whose job is to count views must not be
    // able to hallucinate one out of a string. The earlier false-positive test used `CREATE TABLE`,
    // a sequence this scanner never matched, so it passed while covering nothing.
    //
    // SQL escapes a quote inside a string by doubling it; double quotes delimit identifiers. Line
    // comments are already stripped by the caller.
    let bytes = body.as_bytes();
    let mut code = vec![false; bytes.len()];
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {
                code[i] = true;
                i += 1;
            }
        }
    }

    let up = body.to_ascii_uppercase();
    let mut heads: Vec<(usize, usize, String)> = Vec::new();
    let mut at = 0usize;
    while let Some(rel) = up[at..].find("CREATE") {
        let c = at + rel;
        at = c + 6;
        if !code.get(c).copied().unwrap_or(false) {
            continue;
        }
        if c > 0 && (up.as_bytes()[c - 1] as char).is_alphanumeric() {
            continue;
        }
        let rest = &up[c..];
        let Some(v) = rest.find("VIEW") else { continue };
        if !code.get(c + v).copied().unwrap_or(false) {
            continue;
        }
        let between = &rest[6..v];
        if !between
            .split_whitespace()
            .all(|w| matches!(w, "OR" | "REPLACE" | "TEMP" | "TEMPORARY"))
        {
            continue;
        }
        let after_view = c + v + 4;
        let name: String = body[after_view..]
            .trim_start()
            .chars()
            .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '.' || *ch == '"')
            .collect();
        let Some(as_rel) = up[after_view..].find(" AS") else {
            continue;
        };
        let as_end = after_view + as_rel + 3;
        heads.push((c, as_end, name.trim_matches('"').to_string()));
        at = as_end;
    }

    if heads.is_empty() {
        let sel = body.trim().trim_end_matches(';').trim().to_string();
        return if sel.is_empty() {
            Vec::new()
        } else {
            vec![("<bare select>".to_string(), sel)]
        };
    }

    let mut out = Vec::new();
    for (i, (_, as_end, name)) in heads.iter().enumerate() {
        let end = heads.get(i + 1).map(|(c, _, _)| *c).unwrap_or(body.len());
        let sel = body[*as_end..end].trim().trim_end_matches(';').trim();
        if !sel.is_empty() {
            out.push((name.clone(), sel.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::view_bodies;

    #[test]
    fn a_file_with_two_views_yields_two_bodies_neither_containing_a_ddl_header() {
        let sql = "CREATE VIEW a AS SELECT 1 AS x;\nCREATE VIEW b AS SELECT 2 AS y;\n";
        let got = view_bodies(sql);
        assert_eq!(got.len(), 2, "expected two views, got {got:?}");
        assert_eq!(got[0].0, "a");
        assert_eq!(got[1].0, "b");
        for (name, body) in &got {
            assert!(
                !body.to_ascii_uppercase().contains("CREATE"),
                "view `{name}` body still carries a DDL header, which is the #1013 defect: {body}"
            );
        }
    }

    #[test]
    fn a_single_view_is_unchanged() {
        let got = view_bodies("CREATE VIEW solo AS SELECT 1 AS x;\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "SELECT 1 AS x");
    }

    #[test]
    fn or_replace_and_a_leading_with_are_handled() {
        let got =
            view_bodies("CREATE OR REPLACE VIEW w AS WITH t AS (SELECT 1 AS x) SELECT * FROM t;");
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].0, "w");
        assert!(got[0].1.starts_with("WITH"), "{:?}", got[0].1);
    }

    #[test]
    fn a_bare_select_file_still_counts_as_one() {
        let got = view_bodies("SELECT 1 AS x;\n");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "SELECT 1 AS x");
    }

    /// The false-positive direction, and the case the first version of this test **missed**.
    ///
    /// It used `'CREATE TABLE x'`, a sequence this scanner never matched, so it passed while
    /// covering nothing. What matters is `CREATE VIEW` inside a literal - review caught it (#1023),
    /// and unfixed it reported two views for this one-view file and truncated the real one.
    #[test]
    fn create_view_inside_a_string_literal_is_not_a_view() {
        let got = view_bodies("CREATE VIEW v AS SELECT 'CREATE VIEW fake AS SELECT 1;' AS text;");
        assert_eq!(
            got.len(),
            1,
            "a literal containing CREATE VIEW invented a view: {got:?}"
        );
        assert_eq!(got[0].0, "v");
        assert!(
            got[0].1.contains("AS text"),
            "the real view's body was truncated at the literal: {:?}",
            got[0].1
        );
    }

    #[test]
    fn create_table_in_a_literal_is_not_a_view_either() {
        let got = view_bodies("CREATE VIEW v AS SELECT * FROM t WHERE k = 'CREATE TABLE x';");
        assert_eq!(
            got.len(),
            1,
            "a literal mentioning CREATE TABLE split the file: {got:?}"
        );
    }

    /// A doubled quote escapes a quote inside a SQL string. A scanner that reads the second as a
    /// terminator resumes "code" mode in the middle of a literal.
    #[test]
    fn an_escaped_quote_does_not_end_the_literal() {
        let got = view_bodies("CREATE VIEW v AS SELECT 'it''s CREATE VIEW nope AS x' AS t;");
        assert_eq!(
            got.len(),
            1,
            "an escaped quote let the scanner out of the string: {got:?}"
        );
        assert_eq!(got[0].0, "v");
    }

    /// A quoted identifier is not code either.
    #[test]
    fn a_quoted_identifier_containing_the_keyword_is_not_a_view() {
        let got = view_bodies("CREATE VIEW v AS SELECT x AS \"CREATE VIEW y AS\" FROM t;");
        assert_eq!(got.len(), 1, "a quoted identifier split the file: {got:?}");
    }
}
