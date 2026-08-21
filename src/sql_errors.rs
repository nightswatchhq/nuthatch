//! Errors as prompts (RFC-0016 §3). A failed agent query is a teaching opportunity: instead of
//! relaying a raw `Binder Error: …` and costing a round-trip to rediscover what `schema` already
//! knows, we classify the failure against the registry and append a one-line, actionable hint so the
//! agent self-corrects in one shot. The raw engine message is always preserved (we never lie about
//! what the engine said); the hint is appended.
//!
//! This is pure text-in / text-out over the schema - nothing here touches the data path. It enriches
//! the `/sql` (and thus MCP `sql`) error surface and the `nuthatch sql` REPL alike.

use crate::registry::TableSchema;
use crate::semantic::derive_footguns;

/// Classify a DuckDB error for `query` against the nest `schema`, returning an actionable hint if the
/// failure matches a known class (`None` otherwise - an unrecognised error is relayed raw, unadorned).
/// The classes mirror the RFC-0016 §4 table; each is matched off DuckDB's real message text.
pub fn enrich(raw: &str, query: &str, schema: &[TableSchema]) -> Option<String> {
    // Unknown table: `Catalog Error: Table with name <X> does not exist!`
    if let Some(name) = between(raw, "Table with name ", " does not exist") {
        let name = name.trim();
        // #663: `name` can be a table this nest genuinely declares - the config is correct, the event
        // just has never fired on this chain (or the call/state read has never populated). That is a
        // different fault from a typo or a table nobody declared, and conflating them is exactly what
        // left an operator staring at a bare catalog error with nothing explaining why. `define_views`
        // should already turn this into an empty view rather than a missing table (this schema is the
        // live one, not a possibly-stale `schema.json`), so reaching this branch for a declared table
        // means the empty-view path itself didn't cover this query - still worth naming precisely.
        if let Some(t) = schema.iter().find(|t| t.table == name) {
            return Some(match t.kind {
                crate::registry::TableKind::Event => format!(
                    "`{name}` is declared (event `{}`) but has no data yet - it has likely never \
                     fired on this chain. The table exists once the event does; until then it reads \
                     as empty, not absent.",
                    t.event
                ),
                _ => format!(
                    "`{name}` is declared but has no data yet - it has likely never been populated on \
                     this chain. The table exists once it is; until then it reads as empty, not absent."
                ),
            });
        }
        let tables: Vec<&str> = schema.iter().map(|t| t.table.as_str()).collect();
        return Some(match closest(name, &tables) {
            Some(c) => format!(
                "no table `{name}`; the closest is `{c}`. Call the `schema` tool for the full list."
            ),
            None => format!("no table `{name}`. Call the `schema` tool for the list of tables."),
        });
    }

    // Unknown column: `Binder Error: Referenced column "<X>" not found in FROM clause!`
    if let Some(col) = quoted_after(raw, "Referenced column ") {
        // A big-int helper the agent guessed wrong: they wrote `foo` but meant the `foo_dec` companion,
        // or vice-versa. Suggest the sibling if it exists before a generic fuzzy match.
        let all_cols: Vec<String> = schema
            .iter()
            .flat_map(|t| t.columns.iter().map(|c| c.name.clone()))
            .collect();
        let big_ints: Vec<String> = schema
            .iter()
            .flat_map(|t| derive_footguns(t).big_ints)
            .collect();
        if big_ints.iter().any(|b| format!("{b}_dec") == col)
            && !all_cols.contains(&col.to_string())
        {
            return Some(format!(
                "`{col}` is derived on the fly - it isn't a stored column, but you *can* select it. If \
                 the binder rejected it, the base column exists as `{}` (exact text).",
                col.trim_end_matches("_dec")
            ));
        }
        // A `{col}_dec` whose *base* column the schema doesn't know either: the schema is very likely
        // stale - e.g. the author hand-added a `[[templates]]`/`[[factories]]` to `nuthatch.toml` (whose
        // `{template}__{event}` tables `init`/`add` never generated a schema for), so the derived `_dec`
        // columns were never created. Point at the regen command, not a fuzzy typo match.
        if let Some(base) = col.strip_suffix("_dec") {
            if !all_cols.iter().any(|c| c == base) {
                return Some(format!(
                    "`{col}` doesn't exist because its base column `{base}` isn't in the schema. If you \
                     hand-edited `nuthatch.toml` (e.g. added a factory template), run `nuthatch schema` \
                     to regenerate `schema.json` and the derived `_dec` columns, then retry."
                ));
            }
        }
        let refs: Vec<&str> = all_cols.iter().map(String::as_str).collect();
        return Some(match closest(&col, &refs) {
            Some(c) => format!(
                "no column `{col}`; the closest is `{c}`. Call `schema` for this table's columns."
            ),
            None => format!("no column `{col}`. Call `schema` for the columns."),
        });
    }

    // Reserved word: `Parser Error: syntax error at or near …` when a reserved-word column appears
    // unquoted in the query. DuckDB reports the *next* token, not the column, so we detect via the
    // schema: a reserved-word column mentioned bare is the culprit.
    if raw.contains("syntax error") {
        for t in schema {
            for rc in derive_footguns(t).reserved_words {
                if mentions_unquoted(query, &rc) {
                    return Some(format!(
                        "`{rc}` is a SQL reserved word and a column of `{}` - double-quote it: SELECT \"{rc}\" …",
                        t.table
                    ));
                }
            }
        }
    }

    // Big-int arithmetic on the raw text column: `Binder Error: No function matches … 'sum(VARCHAR)'`.
    if raw.contains("No function matches") && raw.contains("VARCHAR") {
        for t in schema {
            for bc in derive_footguns(t).big_ints {
                if mentions_in_aggregate(query, &bc) {
                    return Some(format!(
                        "`{bc}` is an exact-text big integer (uint/int > 64-bit); use `{bc}_dec` for \
                         SUM/AVG/comparisons, not the raw column."
                    ));
                }
            }
        }
    }

    // #539: a Solidity bool stored as exact text `'true'`/`'false'`, used somewhere that needs a real
    // BOOLEAN. `enabled = true`/`AND`/`NOT` implicitly cast against a VARCHAR and just work, but a
    // function requiring a uniform type across its arguments does not:
    //   `Binder Error: Cannot mix values of type VARCHAR and BOOLEAN in COALESCE operator - an
    //    explicit cast is required` (COALESCE, CASE, UNION - order of the two types in the message
    //    varies by which side DuckDB names first, so check both), or
    //   `Binder Error: No function matches the given name and argument types 'bool_and(VARCHAR)'`
    //    (an aggregate that only accepts BOOLEAN, e.g. bool_and/bool_or).
    let mixed_types = raw.contains("Cannot mix values of type")
        && raw.contains("VARCHAR")
        && raw.contains("BOOLEAN");
    let bool_aggregate = raw.contains("No function matches")
        && (raw.contains("bool_and(VARCHAR)") || raw.contains("bool_or(VARCHAR)"));
    if mixed_types || bool_aggregate {
        let lowered = query.to_ascii_lowercase();
        for t in schema {
            for bc in derive_footguns(t).bools {
                if contains_word(&lowered, &bc.to_ascii_lowercase()) {
                    return Some(format!(
                        "`{bc}` is a Solidity bool stored as exact text `'true'`/`'false'`, not a SQL \
                         boolean; direct comparisons (`{bc} = true`) and `AND`/`NOT` implicitly cast \
                         and work, but COALESCE/CASE/bool_and/bool_or/UNION need matching types - write \
                         `{bc} = 'true'` or `CAST({bc} AS BOOLEAN)`."
                    ));
                }
            }
        }
    }

    // #433: a sealed segment whose *data region* is corrupt but whose Parquet footer is intact binds
    // cleanly and then fails at execution, taking the whole query with it. The engine names nothing -
    // the observed message is `Invalid Error: don't know what type: ` - so an operator cannot tell a
    // bad file from a bad query. Worse, the two corruption classes read as unrelated problems: a
    // footer-corrupt segment fails `prepare`, so #430 drops it and quietly reduces the table, while
    // this one binds and dies at execution pointing nowhere.
    //
    // **Matched last, and on the engine-prefixed form only.** DuckDB echoes the caller's own text
    // back in binder errors, so a bare substring test against `raw` is a test against attacker input:
    // `SELECT "don't know what type: " FROM t` produces a `Binder Error` carrying the phrase, and an
    // eager match here would tell an operator their healthy nest holds a corrupt file *and* shadow
    // the "no column" hint that query actually wanted. Running after the precise classifiers means
    // the specific hint always wins; requiring `Invalid Error:` means the phrase alone is not enough.
    //
    // Deliberately without an integrity scan. Hashing the nest's segments would name the exact file,
    // but it would put an unbounded, caller-triggered sweep on the query path - the cost bound #476
    // and #478 are already about, reachable by anyone who can send a query.
    if raw.contains("Invalid Error: don't know what type:") {
        // Case-folded, like the sibling `mentions_unquoted`: DuckDB resolves unquoted identifiers
        // case-insensitively, so `FROM USDC__Transfer` is a valid way to name `usdc__transfer`, and
        // failing to fold here would drop the table name - the exact "names nothing" complaint #433
        // was filed about.
        let lowered = query.to_ascii_lowercase();
        let touched: Vec<&str> = schema
            .iter()
            .map(|t| t.table.as_str())
            .filter(|t| contains_word(&lowered, &t.to_ascii_lowercase()))
            .collect();
        let which = match touched.as_slice() {
            [] => "a sealed segment behind this query".to_string(),
            [one] => format!("a sealed segment of `{one}`"),
            many => format!("a sealed segment of one of `{}`", many.join("`, `")),
        };
        return Some(format!(
            "{which} binds but cannot be read - its Parquet footer is intact, so the view built over \
             it and the failure landed at execution instead. This is a corrupt file on disk, not a \
             problem with your query: re-running it unchanged fails the same way. Restart the nest \
             and the startup integrity pass hashes every segment against the manifest. A segment in \
             this nest's own directory is quarantined and the table then serves the rows that \
             remain; a segment in a runtime's *shared* store is reported in the log and deliberately \
             left in place, because other datasets reference those bytes (RFC-0033 §11a) - that one \
             is yours to remove once you know what else it feeds."
        ));
    }

    None
}

/// The substring strictly between the first occurrence of `a` and the next occurrence of `b` after it.
fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let start = s.find(a)? + a.len();
    let rest = &s[start..];
    let end = rest.find(b)?;
    Some(&rest[..end])
}

/// The text inside the first pair of double-quotes that appears after `marker`.
fn quoted_after(s: &str, marker: &str) -> Option<String> {
    let after = &s[s.find(marker)? + marker.len()..];
    let open = after.find('"')? + 1;
    let rest = &after[open..];
    let close = rest.find('"')?;
    Some(rest[..close].to_string())
}

/// Does `col` appear in `query` as a bare (un-double-quoted) identifier? If it were quoted the query
/// would have parsed, so a reserved-word column that shows up in the text unquoted is the culprit.
fn mentions_unquoted(query: &str, col: &str) -> bool {
    let q = query.to_ascii_lowercase();
    let c = col.to_ascii_lowercase();
    contains_word(&q, &c) && !q.contains(&format!("\"{c}\""))
}

/// Does `col` (raw) appear inside an aggregate call in `query`? Matches `sum(col)`, `avg(col,` etc.
/// after stripping whitespace/quotes, with a closing `)`/`,` so `col` isn't a prefix of `col_dec`.
fn mentions_in_aggregate(query: &str, col: &str) -> bool {
    let stripped: String = query
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"')
        .collect::<String>()
        .to_ascii_lowercase();
    let c = col.to_ascii_lowercase();
    for op in ["sum(", "avg(", "min(", "max(", "total("] {
        for close in [")", ","] {
            if stripped.contains(&format!("{op}{c}{close}")) {
                return true;
            }
        }
    }
    false
}

/// Whole-word containment: `col` bounded by non-alphanumeric/underscore (so `to` doesn't match
/// `token`). Cheap and dependency-free.
fn contains_word(haystack: &str, word: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(word) {
        let i = from + pos;
        let before_ok = i == 0 || !is_ident(bytes[i - 1]);
        let after = i + word.len();
        let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        from = i + word.len();
    }
    false
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The closest real candidate to `name`. Tries, in order: an exact (case-insensitive) hit; then
/// **containment** - the most common agent slip is dropping the `{alias}__` prefix (`transfers` for
/// `usdc__transfer`) or pluralising, so a candidate that contains the de-pluralised guess wins; then
/// **Levenshtein** within a sane budget (≤ 3, or half the name) for genuine typos (`valu` → `value`).
/// `None` if nothing is close enough - suggestions therefore always come from the real schema, never
/// hallucinated.
fn closest<'a>(name: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let n = name.to_ascii_lowercase();
    let n_sing = n.strip_suffix('s').unwrap_or(&n);

    if let Some(c) = candidates.iter().find(|c| c.eq_ignore_ascii_case(name)) {
        return Some(c);
    }
    // Containment on the de-pluralised guess (`transfer` ⊆ `usdc__transfer`). Prefer the shortest
    // matching candidate - the most specific.
    if let Some(c) = candidates
        .iter()
        .filter(|c| {
            let cl = c.to_ascii_lowercase();
            (n.len() >= 3 && cl.contains(&n)) || (n_sing.len() >= 3 && cl.contains(n_sing))
        })
        .min_by_key(|c| c.len())
    {
        return Some(c);
    }
    // Genuine typo: nearest by edit distance, within budget.
    let budget = (name.len() / 2).max(3);
    candidates
        .iter()
        .map(|c| (*c, levenshtein(&n, &c.to_ascii_lowercase())))
        .filter(|(_, d)| *d <= budget)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}

/// Classic Levenshtein edit distance (two-row DP). Small strings, so O(n·m) is fine.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ColumnSchema, TableSchema};

    fn schema() -> Vec<TableSchema> {
        vec![TableSchema {
            table: "usdc__transfer".into(),
            alias: "usdc".into(),
            kind: crate::registry::TableKind::Event,
            function: String::new(),
            selector: String::new(),
            event: "Transfer".into(),
            topic0: "0xddf2".into(),
            columns: vec![
                ColumnSchema {
                    name: "from".into(),
                    sol_type: "address".into(),
                    storage: "address".into(),
                    indexed: true,
                },
                ColumnSchema {
                    name: "to".into(),
                    sol_type: "address".into(),
                    storage: "address".into(),
                    indexed: true,
                },
                ColumnSchema {
                    name: "value".into(),
                    sol_type: "uint256".into(),
                    storage: "word32".into(),
                    indexed: false,
                },
                ColumnSchema {
                    name: "enabled".into(),
                    sol_type: "bool".into(),
                    storage: "bool".into(),
                    indexed: false,
                },
                ColumnSchema {
                    name: "block_number".into(),
                    sol_type: "implicit".into(),
                    storage: "u64".into(),
                    indexed: false,
                },
            ],
        }]
    }

    /// #433: the page-corrupt-segment failure. Reproduced there by overwriting a sealed segment's
    /// data region and leaving its footer intact, which yields exactly this message - `prepare`
    /// succeeds, `CREATE VIEW` succeeds, and execution dies naming nothing.
    #[test]
    fn a_page_corrupt_segment_is_named_as_a_bad_file_not_a_bad_query() {
        let raw = "Invalid Error: don't know what type: : Error code 1: Unknown error code";
        let hint = enrich(raw, "SELECT count(*) FROM usdc__transfer", &schema()).unwrap();
        // Names the table whose segments to suspect.
        assert!(
            hint.contains("usdc__transfer"),
            "must name the queried table: {hint}"
        );
        // Says whose fault it is. The operator's next move differs entirely from a bad query.
        assert!(
            hint.contains("corrupt file on disk"),
            "must say this is a file, not a query: {hint}"
        );
        assert!(
            hint.contains("Restart"),
            "must point at the pass that identifies the file: {hint}"
        );
        // `doctor` does not run `verify_and_quarantine` - only `build_nest` does - so it must not be
        // offered as the remedy.
        assert!(
            !hint.contains("doctor"),
            "must not recommend a command that does not run the integrity pass: {hint}"
        );
        // And the remedy must not over-promise: `verify_and_quarantine` deliberately refuses to
        // quarantine a segment in a runtime's shared store (RFC-0033 §11a), so telling every
        // operator that restarting clears it would be a restart loop for anyone on that layout.
        assert!(
            hint.contains("shared"),
            "must say the shared-store case is not auto-quarantined: {hint}"
        );
    }

    /// The classifier must not fire on the caller's own text. DuckDB echoes query text back in binder
    /// errors, so a bare substring match would let anyone who can send a query make a healthy nest
    /// report a corrupt file - and would shadow the hint the query actually needed.
    #[test]
    fn a_query_echoing_the_phrase_is_not_reported_as_a_corrupt_file() {
        let raw =
            r#"Binder Error: Referenced column "don't know what type: " not found in FROM clause!"#;
        let hint = enrich(
            raw,
            r#"SELECT "don't know what type: " FROM usdc__transfer"#,
            &schema(),
        )
        .unwrap();
        assert!(
            !hint.contains("corrupt file on disk"),
            "a binder error must not be read as segment corruption: {hint}"
        );
        assert!(
            hint.contains("no column"),
            "and the precise classifier must still win: {hint}"
        );
    }

    /// The prefix itself is load-bearing, not just the ordering.
    ///
    /// `a_query_echoing_the_phrase_is_not_reported_as_a_corrupt_file` uses a `Binder Error`, which an
    /// earlier classifier claims - so it passes whether or not this branch requires the engine prefix,
    /// and a bare `raw.contains("don't know what type:")` survives it. Found by mutation.
    ///
    /// This one carries the phrase in a message no earlier classifier matches, so it reaches the
    /// corrupt-file branch and can only be turned away by the prefix. Drop `Invalid Error: ` from the
    /// test at line ~119 and this goes red.
    #[test]
    fn the_corrupt_file_classifier_requires_the_engine_prefix_not_just_the_phrase() {
        let raw = "Conversion Error: could not convert string \'don\'t know what type: \' to INT32";
        let hint = enrich(raw, "SELECT CAST(x AS INT) FROM usdc__transfer", &schema());
        assert!(
            !hint
                .as_deref()
                .unwrap_or("")
                .contains("corrupt file on disk"),
            "only the engine-prefixed form means segment corruption; caller text must never reach \
             this branch: {hint:?}"
        );
    }

    /// DuckDB resolves unquoted identifiers case-insensitively, so the table must still be named.
    #[test]
    fn the_corrupt_segment_hint_names_the_table_whatever_its_casing() {
        let raw = "Invalid Error: don't know what type: ";
        let hint = enrich(raw, "SELECT count(*) FROM USDC__Transfer", &schema()).unwrap();
        assert!(
            hint.contains("`usdc__transfer`"),
            "must fold case before matching table names: {hint}"
        );
    }

    /// A query naming no known table still gets the class, without inventing a table name.
    #[test]
    fn a_page_corrupt_segment_hint_survives_an_unrecognised_query() {
        let raw = "Invalid Error: don't know what type: ";
        let hint = enrich(raw, "SELECT 1", &schema()).unwrap();
        assert!(
            hint.contains("a sealed segment behind this query"),
            "{hint}"
        );
        assert!(!hint.contains("usdc__transfer"), "invents no table: {hint}");
    }

    #[test]
    fn unknown_table_suggests_the_closest_real_table() {
        let raw = "Catalog Error: Table with name transfers does not exist!";
        let hint = enrich(raw, "SELECT count(*) FROM transfers", &schema()).unwrap();
        assert!(hint.contains("no table `transfers`"));
        assert!(hint.contains("usdc__transfer"), "suggests the real table");
    }

    /// #663: a table the nest genuinely declares (present in `schema` - the live registry, not
    /// `schema.json`) hitting the catalog error is a different fault from a typo, and must read
    /// differently. Before this, `closest("usdc__transfer", [..., "usdc__transfer"])` matched itself
    /// and said "no table `usdc__transfer`; the closest is `usdc__transfer`" - true, and useless.
    #[test]
    fn a_declared_table_with_no_data_yet_is_told_apart_from_an_unknown_one() {
        let raw = "Catalog Error: Table with name usdc__transfer does not exist!";
        let hint = enrich(raw, "SELECT count(*) FROM usdc__transfer", &schema()).unwrap();
        assert!(
            hint.contains("declared"),
            "must say this table IS declared, not just closest-matched to itself: {hint}"
        );
        assert!(
            hint.contains("Transfer"),
            "names the event that would create it: {hint}"
        );
        assert!(
            hint.contains("never fired") || hint.contains("has no data"),
            "must explain WHY it's missing, per #663's acceptance bar: {hint}"
        );
        assert!(
            !hint.contains("the closest is"),
            "an exact declared match is not a fuzzy suggestion: {hint}"
        );
    }

    #[test]
    fn unknown_column_suggests_the_closest_real_column() {
        let raw = r#"Binder Error: Referenced column "valu" not found in FROM clause!"#;
        let hint = enrich(raw, "SELECT valu FROM usdc__transfer", &schema()).unwrap();
        assert!(hint.contains("no column `valu`"));
        assert!(hint.contains("value"), "suggests value");
    }

    #[test]
    fn reserved_word_column_is_told_to_double_quote() {
        let raw = r#"Parser Error: syntax error at or near "FROM""#;
        let hint = enrich(raw, "SELECT from FROM usdc__transfer", &schema()).unwrap();
        assert!(hint.contains("reserved word"));
        assert!(hint.contains("\"from\""), "shows the quoted form");
    }

    #[test]
    fn bigint_aggregate_is_pointed_at_the_dec_companion() {
        let raw =
            "Binder Error: No function matches the given name and argument types 'sum(VARCHAR)'.";
        let hint = enrich(raw, "SELECT sum(value) FROM usdc__transfer", &schema()).unwrap();
        assert!(hint.contains("value_dec"), "points at value_dec");
    }

    /// #539: the issue's own repro, `COALESCE(enabled, false)`, against DuckDB's real message -
    /// captured by actually running the query, not guessed.
    #[test]
    fn bool_column_in_coalesce_is_explained_not_left_as_a_raw_type_error() {
        let raw = "Binder Error: Cannot mix values of type VARCHAR and BOOLEAN in COALESCE \
                    operator - an explicit cast is required";
        let hint = enrich(
            raw,
            "SELECT pool, COALESCE(enabled, false) AS override_enabled FROM usdc__transfer",
            &schema(),
        )
        .unwrap();
        assert!(hint.contains("`enabled`"), "names the bool column: {hint}");
        assert!(
            hint.contains("'true'") || hint.contains("CAST"),
            "gives the working alternative: {hint}"
        );
    }

    /// The message names the two types in the opposite order for `CASE` - the classifier must not be
    /// order-sensitive.
    #[test]
    fn bool_column_in_case_is_explained_regardless_of_type_order_in_the_message() {
        let raw = "Binder Error: Cannot mix values of type BOOLEAN and VARCHAR in CASE expression \
                    - an explicit cast is required";
        let hint = enrich(
            raw,
            "SELECT CASE WHEN true THEN enabled ELSE false END FROM usdc__transfer",
            &schema(),
        )
        .unwrap();
        assert!(hint.contains("`enabled`"), "{hint}");
    }

    /// The other real shape: an aggregate that only accepts BOOLEAN (`bool_and`/`bool_or`).
    #[test]
    fn bool_column_in_a_bool_aggregate_is_explained() {
        let raw = "Binder Error: No function matches the given name and argument types \
                    'bool_and(VARCHAR)'. You might need to add explicit type casts.";
        let hint = enrich(
            raw,
            "SELECT bool_and(enabled) FROM usdc__transfer",
            &schema(),
        )
        .unwrap();
        assert!(hint.contains("`enabled`"), "{hint}");
    }

    /// A COALESCE type-mismatch between two columns that are *not* the bool footgun must not be
    /// misattributed to `enabled` just because it mentions VARCHAR/BOOLEAN.
    #[test]
    fn a_mixed_type_error_naming_no_bool_column_gets_no_bool_hint() {
        let raw = "Binder Error: Cannot mix values of type VARCHAR and BOOLEAN in COALESCE \
                    operator - an explicit cast is required";
        let hint = enrich(
            raw,
            "SELECT COALESCE(\"from\", false) FROM usdc__transfer",
            &schema(),
        );
        assert!(
            hint.is_none_or(|h| !h.contains("enabled")),
            "must not name a column the query never mentioned"
        );
    }

    #[test]
    fn a_quoted_reserved_word_query_is_not_flagged() {
        // If the agent already quoted "from", a *different* syntax error must not be misread as the
        // reserved-word case.
        let raw = r#"Parser Error: syntax error at or near ")""#;
        let hint = enrich(
            raw,
            r#"SELECT "from" FROM usdc__transfer WHERE )"#,
            &schema(),
        );
        assert!(hint.is_none(), "quoted from is not the culprit");
    }

    #[test]
    fn a_dec_column_with_no_known_base_points_at_schema_regen() {
        // Hand-added factory template: `amount0_dec` is queried but the schema never learned about the
        // `amount0` base column (no `nuthatch schema` after editing the toml). Hint at the regen, not a
        // fuzzy typo match.
        let raw = r#"Binder Error: Referenced column "amount0_dec" not found in FROM clause!"#;
        let hint = enrich(raw, "SELECT sum(amount0_dec) FROM pool__swap", &schema()).unwrap();
        assert!(
            hint.contains("nuthatch schema"),
            "points at the regen command: {hint}"
        );
        assert!(hint.contains("amount0"), "names the missing base column");
    }

    #[test]
    fn an_unrecognised_error_gets_no_hint() {
        assert!(enrich("Some internal error", "SELECT 1", &schema()).is_none());
    }

    #[test]
    fn suggestions_only_ever_come_from_the_schema() {
        // A wild table name gets no suggestion rather than a hallucinated one.
        let raw = "Catalog Error: Table with name zzzzzzzzzz does not exist!";
        let hint = enrich(raw, "SELECT * FROM zzzzzzzzzz", &schema()).unwrap();
        assert!(hint.contains("no table"));
        assert!(!hint.contains("usdc__transfer"), "too far to suggest");
    }
}
