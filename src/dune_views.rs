//! RFC-0055 S3: the authored views that translate exactly into DuneSQL.
//!
//! DuckDB's own parser reads each view (`json_serialize_sql`), and the query is rendered again from an
//! allowlist of constructs that give the same result in DuckDB and in Trino, which DuneSQL is built
//! on. Anything outside it refuses the view with a reason; nothing is approximated. The query reads
//! the uploaded tables, which carry the nest's own column text (RFC-0055 §4), so each table it names
//! becomes a CTE of the same name projecting `_dec` and `_overflow` with the nest's own expression.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::analytics::NestViewFile;
use crate::registry::{TableKind, TableSchema};

/// One statement of a `views/*.sql` file and what became of it.
pub struct ViewOutcome {
    /// `views/<file>`.
    pub file: String,
    /// Its position in the file, from 1.
    pub statement: usize,
    /// The name it declares, lowercased, when it is a `CREATE VIEW`.
    pub name: Option<String>,
    pub result: std::result::Result<Translated, String>,
}

#[derive(Clone, Debug)]
pub struct Translated {
    /// The whole DuneSQL query, without a comment header.
    pub sql: String,
    /// Its output column names, in order.
    pub columns: Vec<String>,
    /// Every event table it reads, including through the views it builds on.
    pub reads: BTreeSet<String>,
}

type Refusal<T> = std::result::Result<T, String>;

/// Every statement of every view file, in load order, each translated or refused. A view may build
/// on one defined before it, as the nest's loader allows, and is refused if that one was.
pub fn translate(
    files: &[NestViewFile],
    tables: &[TableSchema],
    source: &str,
) -> Result<Vec<ViewOutcome>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let conn = duckdb::Connection::open_in_memory().context("open DuckDB to parse the views")?;
    let mut events = BTreeMap::new();
    let mut others = BTreeMap::new();
    for t in tables {
        let key = t.table.to_ascii_lowercase();
        match t.kind {
            TableKind::Event => {
                events.insert(key, t);
            }
            TableKind::Block => {
                others.insert(key, "block");
            }
            TableKind::Call => {
                others.insert(key, "call");
            }
            TableKind::State => {
                others.insert(key, "state");
            }
        }
    }
    let mut done: BTreeMap<String, Translated> = BTreeMap::new();
    let mut refused: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for f in files {
        let file = format!("views/{}", f.file);
        for (i, stmt) in crate::analytics::split_sql_statements(&f.sql)
            .iter()
            .enumerate()
        {
            let is_create = stmt.trim_start().to_ascii_lowercase().starts_with("create");
            let name = is_create
                .then(|| crate::analytics::view_name(stmt))
                .flatten();
            let result = match (&name, crate::analytics::view_body(stmt)) {
                (Some(_), Some(body)) => {
                    let mut tr = Translator {
                        events: &events,
                        others: &others,
                        views: &done,
                        refused: &refused,
                        source,
                        reads: BTreeSet::new(),
                        used: BTreeSet::new(),
                        depth: 0,
                    };
                    parse(&conn, body).and_then(|s| tr.view(&s))
                }
                _ => Err("not a `CREATE VIEW`, so it defines nothing to translate".to_string()),
            };
            if let Some(n) = &name {
                if done.contains_key(n) || refused.contains(n) {
                    bail!("{file} defines view `{n}` a second time; the emitter needs one definition per name");
                }
                if let Ok(t) = &result {
                    done.insert(n.clone(), t.clone());
                } else {
                    refused.insert(n.clone());
                }
            }
            out.push(ViewOutcome {
                file: file.clone(),
                statement: i + 1,
                name,
                result,
            });
        }
    }
    Ok(out)
}

fn parse(conn: &duckdb::Connection, body: &str) -> Refusal<Value> {
    // A bound parameter is refused: `json_serialize_sql` wants a constant VARCHAR.
    let literal = format!("'{}'", body.replace('\'', "''"));
    let ast: String = conn
        .query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get(0)
        })
        .map_err(|e| format!("DuckDB could not serialize it: {e}"))?;
    let v: Value = serde_json::from_str(&ast)
        .map_err(|e| format!("DuckDB's parse tree did not read as JSON: {e}"))?;
    if v.get("error").and_then(Value::as_bool) == Some(true) {
        let msg = v
            .get("error_message")
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Err(format!("DuckDB could not parse it: {msg}"));
    }
    match v
        .get("statements")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        Some([one]) => Ok(one.clone()),
        Some(many) => Err(format!("its body holds {} statements", many.len())),
        None => Err("DuckDB returned no statement for it".to_string()),
    }
}

struct Translator<'a> {
    events: &'a BTreeMap<String, &'a TableSchema>,
    others: &'a BTreeMap<String, &'static str>,
    views: &'a BTreeMap<String, Translated>,
    refused: &'a BTreeSet<String>,
    source: &'a str,
    /// Event tables named directly.
    reads: BTreeSet<String>,
    /// Earlier views named directly.
    used: BTreeSet<String>,
    /// Nesting depth; only the outermost query is laid out on several lines.
    depth: usize,
}

struct Modifiers {
    distinct: bool,
    order: Option<String>,
    limit: Option<String>,
}

impl Modifiers {
    fn push_tail(&self, sql: &mut String, nl: &str) {
        if let Some(o) = &self.order {
            sql.push_str(&format!("{nl}ORDER BY {o}"));
        }
        if let Some(l) = &self.limit {
            sql.push_str(&format!("{nl}LIMIT {l}"));
        }
    }
}

impl Translator<'_> {
    fn view(&mut self, stmt: &Value) -> Refusal<Translated> {
        if !is_empty(stmt.get("named_param_map")) {
            return Err("uses a prepared-statement parameter".into());
        }
        let node = &stmt["node"];
        let columns = output_columns(node, true)?;
        let (own, body) = self.parts(node, &[])?;
        for key in cte_keys(node) {
            if self.reads.contains(&key) || self.used.contains(&key) {
                return Err(format!(
                    "defines a CTE named `{key}`, which it also reads as a table or view"
                ));
            }
        }
        let mut reads = self.reads.clone();
        let mut ctes: Vec<String> = self.reads.iter().map(|t| self.base_cte(t)).collect();
        for v in &self.used {
            let dep = &self.views[v];
            reads.extend(dep.reads.iter().cloned());
            ctes.push(format!("{} AS ({})", ident(v), dep.sql));
        }
        ctes.extend(own);
        let sql = if ctes.is_empty() {
            body
        } else {
            format!("WITH\n    {}\n{body}", ctes.join(",\n    "))
        };
        Ok(Translated {
            sql,
            columns,
            reads,
        })
    }

    /// The table as the nest's own DuckDB view defines it: every column, plus the companions.
    fn base_cte(&self, table: &str) -> String {
        let t = self.events[table];
        let cols: Vec<(String, String)> = t
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.storage.clone()))
            .collect();
        format!(
            "{} AS (SELECT *{} FROM dune.{}.{})",
            ident(&t.table),
            crate::analytics::derived_bigint_cols(&cols),
            self.source,
            t.table
        )
    }

    fn nl(&self) -> &'static str {
        if self.depth == 0 {
            "\n"
        } else {
            " "
        }
    }

    fn parts(&mut self, node: &Value, scope: &[String]) -> Refusal<(Vec<String>, String)> {
        let mut scope = scope.to_vec();
        let mut ctes = Vec::new();
        for entry in arr(&node["cte_map"]["map"]) {
            let key = str_of(&entry["key"]);
            let value = &entry["value"];
            if !arr(&value["aliases"]).is_empty() {
                return Err(format!(
                    "names the columns of CTE `{key}`, which this slice does not translate"
                ));
            }
            if !arr(&value["key_targets"]).is_empty() {
                return Err(format!("uses `USING KEY` in CTE `{key}`"));
            }
            output_columns(&value["query"]["node"], false)
                .map_err(|e| format!("reads CTE `{key}`, which {e}"))?;
            let body = self.nested(&value["query"]["node"], &scope)?;
            scope.push(key.to_ascii_lowercase());
            ctes.push(format!("{} AS ({body})", ident(key)));
        }
        let body = match str_of(&node["type"]) {
            "SELECT_NODE" => self.select(node, &scope)?,
            "SET_OPERATION_NODE" => self.set_operation(node, &scope)?,
            other => {
                return Err(format!(
                    "is a `{other}` query, which this slice does not translate"
                ))
            }
        };
        Ok((ctes, body))
    }

    fn nested(&mut self, node: &Value, scope: &[String]) -> Refusal<String> {
        self.depth += 1;
        let r = self.parts(node, scope).map(|(ctes, body)| {
            if ctes.is_empty() {
                body
            } else {
                format!("WITH {} {body}", ctes.join(", "))
            }
        });
        self.depth -= 1;
        r
    }

    fn select(&mut self, node: &Value, scope: &[String]) -> Refusal<String> {
        if !node["sample"].is_null() {
            return Err("samples its input".into());
        }
        if !node["qualify"].is_null() {
            return Err("uses `QUALIFY`, which DuneSQL does not have".into());
        }
        if !matches!(
            str_of(&node["aggregate_handling"]),
            "" | "STANDARD_HANDLING"
        ) {
            return Err("uses `GROUP BY ALL`, which DuneSQL does not have".into());
        }
        let m = self.modifiers(node, scope)?;
        let nl = self.nl();
        let mut items = Vec::new();
        for e in arr(&node["select_list"]) {
            let mut s = if str_of(&e["class"]) == "STAR" {
                star(e)?
            } else {
                self.expr(e, scope)?
            };
            if let Some(a) = alias(e) {
                s = format!("{s} AS {}", ident(a));
            }
            items.push(s);
        }
        let mut sql = format!(
            "SELECT{}{}",
            if m.distinct { " DISTINCT" } else { "" },
            if self.depth == 0 {
                format!("\n    {}", items.join(",\n    "))
            } else {
                format!(" {}", items.join(", "))
            }
        );
        if let Some(from) = self.from(&node["from_table"], scope)? {
            sql.push_str(&format!("{nl}FROM {from}"));
        }
        if !node["where_clause"].is_null() {
            let w = self.expr(&node["where_clause"], scope)?;
            sql.push_str(&format!("{nl}WHERE {w}"));
        }
        let groups = arr(&node["group_expressions"]);
        if !groups.is_empty() {
            let sets = arr(&node["group_sets"]);
            let plain = sets.len() == 1
                && arr(&sets[0])
                    .iter()
                    .map(Value::as_u64)
                    .eq((0..groups.len() as u64).map(Some));
            if !plain {
                return Err("uses `GROUPING SETS`, `ROLLUP` or `CUBE`".into());
            }
            let mut g = Vec::new();
            for e in groups {
                g.push(self.expr(e, scope)?);
            }
            sql.push_str(&format!("{nl}GROUP BY {}", g.join(", ")));
        }
        if !node["having"].is_null() {
            let h = self.expr(&node["having"], scope)?;
            sql.push_str(&format!("{nl}HAVING {h}"));
        }
        m.push_tail(&mut sql, nl);
        Ok(sql)
    }

    fn modifiers(&mut self, node: &Value, scope: &[String]) -> Refusal<Modifiers> {
        let mut m = Modifiers {
            distinct: false,
            order: None,
            limit: None,
        };
        for md in arr(&node["modifiers"]) {
            match str_of(&md["type"]) {
                "DISTINCT_MODIFIER" => {
                    if !arr(&md["distinct_on_targets"]).is_empty() {
                        return Err("uses `DISTINCT ON`, which DuneSQL does not have".into());
                    }
                    m.distinct = true;
                }
                "ORDER_MODIFIER" => {
                    let mut items = Vec::new();
                    for o in arr(&md["orders"]) {
                        let dir = match str_of(&o["type"]) {
                            "ASCENDING" | "ORDER_DEFAULT" => "ASC",
                            "DESCENDING" => "DESC",
                            other => return Err(format!("orders by `{other}`")),
                        };
                        // DuckDB's default is NULLS LAST in both directions, and so is Trino's;
                        // written out so neither engine's setting is relied on.
                        let nulls = match str_of(&o["null_order"]) {
                            "NULLS FIRST" | "NULLS_FIRST" => "NULLS FIRST",
                            "NULLS LAST" | "NULLS_LAST" | "ORDER_DEFAULT" => "NULLS LAST",
                            other => return Err(format!("orders nulls by `{other}`")),
                        };
                        let e = self.expr(&o["expression"], scope)?;
                        items.push(format!("{e} {dir} {nulls}"));
                    }
                    if !items.is_empty() {
                        m.order = Some(items.join(", "));
                    }
                }
                "LIMIT_MODIFIER" => {
                    if !md["offset"].is_null() {
                        return Err("uses `OFFSET`, which this slice does not translate".into());
                    }
                    m.limit = Some(limit_count(&md["limit"])?);
                }
                other => {
                    return Err(format!(
                        "uses a `{other}` modifier, which this slice does not translate"
                    ))
                }
            }
        }
        Ok(m)
    }

    fn set_operation(&mut self, node: &Value, scope: &[String]) -> Refusal<String> {
        let all = node["setop_all"].as_bool().unwrap_or(false);
        let kw = match (str_of(&node["setop_type"]), all) {
            ("UNION", false) => "UNION",
            ("UNION", true) => "UNION ALL",
            ("EXCEPT", false) => "EXCEPT",
            ("INTERSECT", false) => "INTERSECT",
            (k @ ("EXCEPT" | "INTERSECT"), true) => {
                return Err(format!(
                    "uses `{k} ALL`, which this slice does not translate"
                ))
            }
            (other, _) => {
                return Err(format!(
                    "uses a `{other}` set operation, which this slice does not translate"
                ))
            }
        };
        let children: Vec<&Value> = match (node.get("left"), node.get("right")) {
            (Some(l), Some(r)) if l.is_object() && r.is_object() => vec![l, r],
            _ => arr(&node["children"]).iter().collect(),
        };
        if children.len() < 2 {
            return Err("is a set operation with fewer than two branches".into());
        }
        let m = self.modifiers(node, scope)?;
        if m.distinct {
            return Err("puts `DISTINCT` on a set operation".into());
        }
        let nl = self.nl();
        let mut parts = Vec::new();
        for c in children {
            parts.push(format!("({})", self.nested(c, scope)?));
        }
        let mut sql = parts.join(&format!("{nl}{kw}{nl}"));
        m.push_tail(&mut sql, nl);
        Ok(sql)
    }

    fn from(&mut self, t: &Value, scope: &[String]) -> Refusal<Option<String>> {
        let alias = match str_of(&t["alias"]) {
            "" => String::new(),
            a => format!(" AS {}", ident(a)),
        };
        if !t["sample"].is_null() {
            return Err("samples a table".into());
        }
        if !arr(&t["column_name_alias"]).is_empty() {
            return Err("renames a table's columns in `FROM`".into());
        }
        Ok(Some(match str_of(&t["type"]) {
            "EMPTY" => return Ok(None),
            "BASE_TABLE" => {
                let name = str_of(&t["table_name"]);
                if !str_of(&t["schema_name"]).is_empty() || !str_of(&t["catalog_name"]).is_empty() {
                    return Err(format!(
                        "reads `{name}` through a schema or catalog qualifier"
                    ));
                }
                if !t["at_clause"].is_null() {
                    return Err(format!("reads `{name}` at a version"));
                }
                self.table(name, scope)?;
                format!("{}{alias}", ident(name))
            }
            "JOIN" => {
                if !alias.is_empty() {
                    return Err("aliases a join".into());
                }
                if !arr(&t["using_columns"]).is_empty() {
                    return Err("joins with `USING`, which this slice does not translate".into());
                }
                let kw = match (str_of(&t["ref_type"]), str_of(&t["join_type"])) {
                    ("CROSS", _) => "CROSS JOIN",
                    ("REGULAR", "INNER") => "JOIN",
                    ("REGULAR", "LEFT") => "LEFT JOIN",
                    ("REGULAR", "RIGHT") => "RIGHT JOIN",
                    ("REGULAR", "OUTER") => "FULL JOIN",
                    (r, j) => {
                        return Err(format!(
                            "uses a `{r}` `{j}` join, which this slice does not translate"
                        ))
                    }
                };
                let left = self.from(&t["left"], scope)?.ok_or("joins nothing")?;
                let right = self.from(&t["right"], scope)?.ok_or("joins nothing")?;
                let right = if str_of(&t["right"]["type"]) == "JOIN" {
                    format!("({right})")
                } else {
                    right
                };
                if kw == "CROSS JOIN" {
                    format!("{left} CROSS JOIN {right}")
                } else {
                    if t["condition"].is_null() {
                        return Err("joins without a condition".into());
                    }
                    let on = self.expr(&t["condition"], scope)?;
                    format!("{left} {kw} {right} ON {on}")
                }
            }
            "SUBQUERY" => {
                output_columns(&t["subquery"]["node"], false)
                    .map_err(|e| format!("reads a subquery in `FROM`, which {e}"))?;
                format!("({}){alias}", self.nested(&t["subquery"]["node"], scope)?)
            }
            other => {
                return Err(format!(
                    "reads from a `{other}`, which has no DuneSQL counterpart here"
                ))
            }
        }))
    }

    fn table(&mut self, name: &str, scope: &[String]) -> Refusal<()> {
        let key = name.to_ascii_lowercase();
        if scope.contains(&key) {
            return Ok(());
        }
        if self.events.contains_key(&key) {
            self.reads.insert(key);
            return Ok(());
        }
        if self.views.contains_key(&key) {
            self.used.insert(key);
            return Ok(());
        }
        if self.refused.contains(&key) {
            return Err(format!("reads view `{name}`, which did not translate"));
        }
        if let Some(kind) = self.others.get(&key) {
            return Err(format!(
                "reads `{name}`, a {kind} table; RFC-0055 §3.3 names only event tables"
            ));
        }
        Err(format!(
            "reads `{name}`, which is neither an event table of this nest nor a view defined before this one"
        ))
    }

    fn list(&mut self, v: &Value, scope: &[String]) -> Refusal<Vec<String>> {
        arr(v).iter().map(|e| self.expr(e, scope)).collect()
    }

    fn expr(&mut self, e: &Value, scope: &[String]) -> Refusal<String> {
        Ok(match str_of(&e["class"]) {
            "COLUMN_REF" => {
                let names = arr(&e["column_names"]);
                if names.is_empty() || names.len() > 2 {
                    return Err("uses a column name of more than two parts".into());
                }
                names
                    .iter()
                    .map(|n| ident(str_of(n)))
                    .collect::<Vec<_>>()
                    .join(".")
            }
            "CONSTANT" => constant(&e["value"])?,
            "CAST" => {
                let child = &e["child"];
                let try_cast = e["try_cast"].as_bool().unwrap_or(false);
                // DuckDB parses `true` and `false` as a cast of 't' or 'f' to BOOLEAN.
                if !try_cast
                    && str_of(&e["cast_type"]["id"]) == "BOOLEAN"
                    && str_of(&child["class"]) == "CONSTANT"
                    && str_of(&child["value"]["type"]["id"]) == "VARCHAR"
                {
                    match child["value"]["value"].as_str() {
                        Some("t") => return Ok("TRUE".into()),
                        Some("f") => return Ok("FALSE".into()),
                        _ => {}
                    }
                }
                let ty = cast_type(&e["cast_type"])?;
                let inner = self.expr(child, scope)?;
                format!(
                    "{}({inner} AS {ty})",
                    if try_cast { "try_cast" } else { "CAST" }
                )
            }
            "COMPARISON" => {
                let op = match str_of(&e["type"]) {
                    "COMPARE_EQUAL" => "=",
                    "COMPARE_NOTEQUAL" => "<>",
                    "COMPARE_LESSTHAN" => "<",
                    "COMPARE_GREATERTHAN" => ">",
                    "COMPARE_LESSTHANOREQUALTO" => "<=",
                    "COMPARE_GREATERTHANOREQUALTO" => ">=",
                    "COMPARE_DISTINCT_FROM" => "IS DISTINCT FROM",
                    "COMPARE_NOT_DISTINCT_FROM" => "IS NOT DISTINCT FROM",
                    other => return Err(format!("compares with `{other}`")),
                };
                let l = self.expr(&e["left"], scope)?;
                let r = self.expr(&e["right"], scope)?;
                format!("({l} {op} {r})")
            }
            "CONJUNCTION" => {
                let op = match str_of(&e["type"]) {
                    "CONJUNCTION_AND" => " AND ",
                    "CONJUNCTION_OR" => " OR ",
                    other => return Err(format!("uses `{other}`")),
                };
                format!("({})", self.list(&e["children"], scope)?.join(op))
            }
            "OPERATOR" => {
                let kids = self.list(&e["children"], scope)?;
                match (str_of(&e["type"]), kids.as_slice()) {
                    ("OPERATOR_NOT", [a]) => format!("(NOT {a})"),
                    ("OPERATOR_IS_NULL", [a]) => format!("({a} IS NULL)"),
                    ("OPERATOR_IS_NOT_NULL", [a]) => format!("({a} IS NOT NULL)"),
                    ("COMPARE_IN", [a, rest @ ..]) if !rest.is_empty() => {
                        format!("({a} IN ({}))", rest.join(", "))
                    }
                    ("COMPARE_NOT_IN", [a, rest @ ..]) if !rest.is_empty() => {
                        format!("({a} NOT IN ({}))", rest.join(", "))
                    }
                    ("OPERATOR_COALESCE", all) if !all.is_empty() => {
                        format!("coalesce({})", all.join(", "))
                    }
                    (other, _) => {
                        return Err(format!(
                            "uses the `{other}` operator, which this slice does not translate"
                        ))
                    }
                }
            }
            "BETWEEN" => {
                let x = self.expr(&e["input"], scope)?;
                let lo = self.expr(&e["lower"], scope)?;
                let hi = self.expr(&e["upper"], scope)?;
                format!("({x} BETWEEN {lo} AND {hi})")
            }
            "CASE" => {
                let mut s = String::from("(CASE");
                for check in arr(&e["case_checks"]) {
                    let w = self.expr(&check["when_expr"], scope)?;
                    let t = self.expr(&check["then_expr"], scope)?;
                    s.push_str(&format!(" WHEN {w} THEN {t}"));
                }
                if !e["else_expr"].is_null() {
                    let x = self.expr(&e["else_expr"], scope)?;
                    s.push_str(&format!(" ELSE {x}"));
                }
                s.push_str(" END)");
                s
            }
            "FUNCTION" => self.function(e, scope)?,
            "SUBQUERY" => {
                let q = self.nested(&e["subquery"]["node"], scope)?;
                match (str_of(&e["subquery_type"]), str_of(&e["comparison_type"])) {
                    ("SCALAR", _) => format!("({q})"),
                    ("EXISTS", _) => format!("(EXISTS ({q}))"),
                    ("NOT_EXISTS", _) => format!("(NOT EXISTS ({q}))"),
                    ("ANY", "COMPARE_EQUAL") => {
                        format!("({} IN ({q}))", self.expr(&e["child"], scope)?)
                    }
                    (t, c) => {
                        return Err(format!(
                            "uses a `{t}` `{c}` subquery, which this slice does not translate"
                        ))
                    }
                }
            }
            "STAR" => return Err("uses `*` outside a select list".into()),
            "WINDOW" => {
                return Err("uses a window function, which this slice does not translate".into())
            }
            other => {
                return Err(format!(
                    "uses a `{other}` expression, which this slice does not translate"
                ))
            }
        })
    }

    fn function(&mut self, e: &Value, scope: &[String]) -> Refusal<String> {
        let name = str_of(&e["function_name"]);
        if !str_of(&e["schema"]).is_empty() || !str_of(&e["catalog"]).is_empty() {
            return Err(format!("calls a qualified function `{name}`"));
        }
        if e["export_state"].as_bool() == Some(true) {
            return Err(format!("exports the state of `{name}`"));
        }
        if !arr(&e["order_bys"]["orders"]).is_empty() {
            return Err(format!(
                "orders the input of `{name}`, which this slice does not translate"
            ));
        }
        let distinct = e["distinct"].as_bool().unwrap_or(false);
        let filter = if e["filter"].is_null() {
            String::new()
        } else {
            format!(" FILTER (WHERE {})", self.expr(&e["filter"], scope)?)
        };
        let args = self.list(&e["children"], scope)?;
        if e["is_operator"].as_bool() == Some(true) {
            return Ok(match (name, args.as_slice()) {
                ("-", [a]) => format!("(- {a})"),
                ("+" | "-" | "*", [a, b]) => format!("({a} {name} {b})"),
                ("||", [a, b]) => format!("({a} || {b})"),
                ("~~", [a, b]) => format!("({a} LIKE {b})"),
                ("!~~", [a, b]) => format!("({a} NOT LIKE {b})"),
                ("/" | "//", _) => {
                    return Err(
                        "divides with `/`, which divides integers as integers in DuneSQL \
                                and as doubles in DuckDB"
                            .into(),
                    )
                }
                (op, _) => {
                    return Err(format!(
                        "uses the `{op}` operator, which this slice does not translate"
                    ))
                }
            });
        }
        let d = if distinct { "DISTINCT " } else { "" };
        let plain = !distinct && filter.is_empty();
        Ok(match (name, args.as_slice()) {
            ("count_star", []) if !distinct => format!("count(*){filter}"),
            ("count" | "sum" | "min" | "max", [a]) => format!("{name}({d}{a}){filter}"),
            ("lower" | "upper", [a]) if plain => format!("{name}({a})"),
            ("coalesce", all) if plain && !all.is_empty() => {
                format!("coalesce({})", all.join(", "))
            }
            ("avg", _) => {
                return Err(
                    "uses `avg`, which returns a double in DuckDB and a decimal in DuneSQL".into(),
                )
            }
            (f, _) => {
                return Err(format!(
                    "calls `{f}`, which is not among the functions this slice translates"
                ))
            }
        })
    }
}

/// The names a relation exposes. DuckDB names an unaliased expression after its own text and Trino
/// names it `_colN`, so only aliases and bare columns are names both engines agree on. A nested `*`
/// keeps the names of what it expands, and those are checked where they are defined.
fn output_columns(node: &Value, top: bool) -> Refusal<Vec<String>> {
    match str_of(&node["type"]) {
        "SELECT_NODE" => arr(&node["select_list"])
            .iter()
            .enumerate()
            .map(|(i, e)| {
                if let Some(a) = alias(e) {
                    return Ok(a.to_string());
                }
                match str_of(&e["class"]) {
                    "COLUMN_REF" => arr(&e["column_names"])
                        .last()
                        .map(|n| str_of(n).to_string())
                        .ok_or_else(|| "selects a column with no name".to_string()),
                    "STAR" if !top => Ok("*".to_string()),
                    "STAR" => Err(
                        "selects `*`, whose columns follow the upload's column order rather than \
                         the nest's"
                            .into(),
                    ),
                    _ => Err(format!(
                        "leaves select item {} without an alias, and DuckDB and DuneSQL name an \
                         unaliased expression differently",
                        i + 1
                    )),
                }
            })
            .collect(),
        "SET_OPERATION_NODE" => {
            let first = node
                .get("left")
                .filter(|l| l.is_object())
                .or_else(|| arr(&node["children"]).first())
                .ok_or_else(|| "is a set operation with no branches".to_string())?;
            output_columns(first, top)
        }
        other => Err(format!(
            "is a `{other}` query, which this slice does not translate"
        )),
    }
}

fn star(e: &Value) -> Refusal<String> {
    for (k, v) in e.as_object().into_iter().flatten() {
        let inert = match v {
            Value::Null => true,
            Value::Bool(b) => !b,
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            Value::String(_) | Value::Number(_) => true,
        };
        if !inert && k != "class" && k != "type" {
            return Err(
                "uses `*` with `EXCLUDE`, `REPLACE`, `RENAME` or `COLUMNS`, which DuneSQL does not have"
                    .into(),
            );
        }
    }
    Ok(match str_of(&e["relation_name"]) {
        "" => "*".into(),
        r => format!("{}.*", ident(r)),
    })
}

fn constant(v: &Value) -> Refusal<String> {
    let id = str_of(&v["type"]["id"]);
    if v["is_null"].as_bool() == Some(true) {
        return if id == "NULL" {
            Ok("NULL".into())
        } else {
            Err(format!("uses a typed `{id}` NULL literal"))
        };
    }
    match id {
        "INTEGER" | "BIGINT" | "SMALLINT" | "TINYINT" => v["value"]
            .as_i64()
            .map(|n| n.to_string())
            .ok_or_else(|| format!("uses a `{id}` literal that does not read as an integer")),
        "VARCHAR" => v["value"]
            .as_str()
            .map(literal)
            .ok_or_else(|| "uses a string literal that does not read as text".to_string()),
        "BOOLEAN" => v["value"]
            .as_bool()
            .map(|b| if b { "TRUE" } else { "FALSE" }.to_string())
            .ok_or_else(|| "uses a boolean literal that does not read as one".to_string()),
        "DECIMAL" => {
            let scale = v["type"]["type_info"]["scale"].as_u64().unwrap_or(0) as usize;
            let raw = v["value"]
                .as_i64()
                .filter(|n| *n >= 0)
                .ok_or_else(|| "uses a decimal literal wider than this slice reads".to_string())?;
            let digits = format!("{raw:0>width$}", width = scale + 1);
            let at = digits.len() - scale;
            Ok(if scale == 0 {
                digits
            } else {
                format!("{}.{}", &digits[..at], &digits[at..])
            })
        }
        "DOUBLE" | "FLOAT" => Err(
            "uses a floating-point literal, which the two engines round and print differently"
                .into(),
        ),
        other => Err(format!(
            "uses a `{other}` literal, which this slice does not translate"
        )),
    }
}

fn cast_type(t: &Value) -> Refusal<String> {
    let info = &t["type_info"];
    Ok(match str_of(&t["id"]) {
        "VARCHAR" if info.is_null() => "varchar".into(),
        "BIGINT" => "bigint".into(),
        "INTEGER" => "integer".into(),
        "BOOLEAN" => "boolean".into(),
        "DECIMAL" => {
            let w = info["width"].as_u64().unwrap_or(0);
            let s = info["scale"].as_u64().unwrap_or(0);
            if !(1..=38).contains(&w) || s > w {
                return Err(format!("casts to `DECIMAL({w},{s})`"));
            }
            format!("decimal({w},{s})")
        }
        other => {
            return Err(format!(
                "casts to `{other}`, which has no exact DuneSQL counterpart here"
            ))
        }
    })
}

fn limit_count(v: &Value) -> Refusal<String> {
    if str_of(&v["class"]) == "CONSTANT"
        && matches!(str_of(&v["value"]["type"]["id"]), "INTEGER" | "BIGINT")
    {
        if let Some(n) = v["value"]["value"].as_u64() {
            return Ok(n.to_string());
        }
    }
    Err("limits by something other than a constant row count".into())
}

fn cte_keys(node: &Value) -> Vec<String> {
    arr(&node["cte_map"]["map"])
        .iter()
        .map(|e| str_of(&e["key"]).to_ascii_lowercase())
        .collect()
}

fn alias(e: &Value) -> Option<&str> {
    e.get("alias")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn is_empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ColumnSchema, StorageKind};

    fn col(name: &str, sol_type: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            sol_type: sol_type.to_string(),
            storage: StorageKind::from_sol(sol_type, false).as_str().to_string(),
            indexed: false,
            components: Vec::new(),
        }
    }

    fn table(name: &str, kind: TableKind, params: Vec<ColumnSchema>) -> TableSchema {
        let mut columns = crate::registry::implicit_columns(true);
        columns.extend(params);
        TableSchema {
            table: name.to_string(),
            alias: name.split("__").next().unwrap().to_string(),
            kind,
            event: String::new(),
            topic0: String::new(),
            function: String::new(),
            selector: String::new(),
            columns,
        }
    }

    fn tables() -> Vec<TableSchema> {
        vec![
            table(
                "vault__transfer",
                TableKind::Event,
                vec![
                    col("from", "address"),
                    col("to", "address"),
                    col("value", "uint256"),
                ],
            ),
            table("total_supply", TableKind::Call, vec![col("out", "uint256")]),
        ]
    }

    fn run(files: &[(&str, &str)]) -> Vec<ViewOutcome> {
        let files: Vec<NestViewFile> = files
            .iter()
            .map(|(f, s)| NestViewFile {
                file: f.to_string(),
                sql: s.to_string(),
            })
            .collect();
        translate(&files, &tables(), "src").unwrap()
    }

    fn one(body: &str) -> Refusal<Translated> {
        let sql = format!("CREATE VIEW v AS {body};");
        run(&[("10-v.sql", sql.as_str())]).pop().unwrap().result
    }

    #[test]
    fn a_big_integer_companion_is_the_nest_s_own_expression() {
        let t = one("SELECT sum(value_dec) AS total FROM vault__transfer").unwrap();
        let nest = crate::analytics::derived_bigint_cols(&[("value".into(), "word16".into())]);
        assert!(
            t.sql.contains(&format!(
                "\"vault__transfer\" AS (SELECT *{nest} FROM dune.src.vault__transfer)"
            )),
            "{}",
            t.sql
        );
        assert_eq!(t.columns, ["total"]);
        assert_eq!(t.reads, BTreeSet::from(["vault__transfer".to_string()]));
    }

    #[test]
    fn the_allowlist_renders_to_exact_dunesql() {
        let t = one(
            "WITH big AS (SELECT \"to\", value_dec FROM vault__transfer \
               WHERE value_dec > 1.50 AND block_number BETWEEN 1 AND 9 AND \"from\" LIKE '0x%' \
               AND log_index IN (0, 1) AND NOT value_overflow AND tx_hash IS NOT NULL) \
             SELECT lower(b.\"to\") AS holder, count(DISTINCT b.value_dec) AS n, \
               count(*) FILTER (WHERE b.value_dec > 0) AS positive, \
               CASE WHEN sum(b.value_dec) >= 10 THEN 'big' ELSE 'small' END AS band, \
               CAST(sum(- b.value_dec) AS VARCHAR) AS negated, coalesce(max(s.\"to\"), 'none') AS seen, \
               true AS flag \
             FROM big b LEFT JOIN (SELECT \"to\" FROM vault__transfer) s ON s.\"to\" = b.\"to\" \
             GROUP BY 1 HAVING count(*) > 0 ORDER BY holder DESC LIMIT 5",
        )
        .unwrap();
        let nest = crate::analytics::derived_bigint_cols(&[("value".into(), "word16".into())]);
        assert_eq!(
            t.sql,
            format!(
                "WITH\n    \"vault__transfer\" AS (SELECT *{nest} FROM dune.src.vault__transfer),\n    \
                 \"big\" AS (SELECT \"to\", \"value_dec\" FROM \"vault__transfer\" WHERE ((\"value_dec\" > 1.50) \
                 AND (\"block_number\" BETWEEN 1 AND 9) AND (\"from\" LIKE '0x%') AND (\"log_index\" IN (0, 1)) \
                 AND (NOT \"value_overflow\") AND (\"tx_hash\" IS NOT NULL)))\n\
                 SELECT\n    lower(\"b\".\"to\") AS \"holder\",\n    count(DISTINCT \"b\".\"value_dec\") AS \"n\",\n    \
                 count(*) FILTER (WHERE (\"b\".\"value_dec\" > 0)) AS \"positive\",\n    \
                 (CASE WHEN (sum(\"b\".\"value_dec\") >= 10) THEN 'big' ELSE 'small' END) AS \"band\",\n    \
                 CAST(sum((- \"b\".\"value_dec\")) AS varchar) AS \"negated\",\n    \
                 coalesce(max(\"s\".\"to\"), 'none') AS \"seen\",\n    TRUE AS \"flag\"\n\
                 FROM \"big\" AS \"b\" LEFT JOIN (SELECT \"to\" FROM \"vault__transfer\") AS \"s\" ON (\"s\".\"to\" = \"b\".\"to\")\n\
                 GROUP BY 1\nHAVING (count(*) > 0)\nORDER BY \"holder\" DESC NULLS LAST\nLIMIT 5"
            )
        );
        assert_eq!(
            t.columns,
            ["holder", "n", "positive", "band", "negated", "seen", "flag"]
        );
    }

    #[test]
    fn a_set_operation_parenthesises_each_branch_and_keeps_its_order() {
        let t = one(
            "SELECT \"to\" AS who FROM vault__transfer UNION ALL SELECT \"from\" FROM vault__transfer \
             ORDER BY who",
        )
        .unwrap();
        assert!(
            t.sql.ends_with(
                "(SELECT \"to\" AS \"who\" FROM \"vault__transfer\")\nUNION ALL\n\
                 (SELECT \"from\" FROM \"vault__transfer\")\nORDER BY \"who\" ASC NULLS LAST"
            ),
            "{}",
            t.sql
        );
        assert_eq!(t.columns, ["who"]);
    }

    #[test]
    fn each_construct_outside_the_allowlist_is_refused_by_name() {
        for (body, why) in [
            ("SELECT sum(value_dec) / 2 AS half FROM vault__transfer", "divides with `/`"),
            ("SELECT avg(value_dec) AS mean FROM vault__transfer", "uses `avg`"),
            ("SELECT \"to\" FROM vault__transfer GROUP BY ALL", "`GROUP BY ALL`"),
            (
                "SELECT \"to\" FROM vault__transfer QUALIFY row_number() OVER (PARTITION BY \"to\") = 1",
                "`QUALIFY`",
            ),
            ("SELECT DISTINCT ON (\"to\") \"to\" FROM vault__transfer", "`DISTINCT ON`"),
            (
                "SELECT row_number() OVER () AS n FROM vault__transfer",
                "window function",
            ),
            ("SELECT TRY_CAST(value AS HUGEINT) AS v FROM vault__transfer", "`HUGEINT`"),
            ("SELECT 1.5e3 AS f", "floating-point literal"),
            ("SELECT * FROM vault__transfer", "selects `*`"),
            ("SELECT count(*) FROM vault__transfer", "without an alias"),
            ("SELECT count(*) AS n FROM read_parquet('x.parquet')", "`TABLE_FUNCTION`"),
            ("SELECT count(*) AS n FROM total_supply", "a call table"),
            ("SELECT count(*) AS n FROM nowhere", "neither an event table"),
            ("SELECT \"to\" FROM vault__transfer LIMIT 1 OFFSET 2", "`OFFSET`"),
            (
                "SELECT a.\"to\" FROM vault__transfer a JOIN vault__transfer b USING (\"to\")",
                "`USING`",
            ),
            (
                "SELECT \"to\" FROM vault__transfer EXCEPT ALL SELECT \"from\" FROM vault__transfer",
                "`EXCEPT ALL`",
            ),
            (
                "WITH RECURSIVE r AS (SELECT 1 AS n UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
                "query, which this slice does not translate",
            ),
            (
                "SELECT * EXCLUDE (\"to\") FROM vault__transfer",
                "selects `*`",
            ),
            ("SELECT \"to\" FROM main.vault__transfer", "qualifier"),
            ("SELECT substr(\"to\", 1, 4) AS p FROM vault__transfer", "calls `substr`"),
            (
                "SELECT q.\"1\" AS value FROM (SELECT 1) q",
                "reads a subquery in `FROM`, which leaves select item 1 without an alias",
            ),
            (
                "WITH c AS (SELECT count(*) FROM vault__transfer) SELECT 1 AS one FROM c",
                "reads CTE `c`, which leaves select item 1 without an alias",
            ),
        ] {
            match one(body) {
                Ok(t) => panic!("`{body}` translated, and must not have:\n{}", t.sql),
                Err(e) => assert!(e.contains(why), "`{body}` refused for `{e}`, expected `{why}`"),
            }
        }
    }

    #[test]
    fn a_nested_star_and_an_unaliased_scalar_subquery_are_still_translated() {
        let t = one(
            "WITH x AS (SELECT * FROM vault__transfer WHERE log_index = 0) \
             SELECT count(*) AS n, (SELECT max(block_number) FROM x) AS last FROM x",
        )
        .unwrap();
        assert_eq!(t.columns, ["n", "last"]);
    }

    #[test]
    fn a_view_reads_an_earlier_view_through_a_cte_carrying_its_tables() {
        let out = run(&[
            (
                "10-holders.sql",
                "CREATE VIEW holders AS SELECT \"to\" AS holder, sum(value_dec) AS total \
                 FROM vault__transfer GROUP BY 1;",
            ),
            (
                "20-top.sql",
                "CREATE VIEW top AS SELECT holder FROM holders WHERE total > 0;",
            ),
        ]);
        let holders = out[0].result.as_ref().unwrap();
        let top = out[1].result.as_ref().unwrap();
        assert!(
            top.sql
                .starts_with(&format!("WITH\n    \"holders\" AS ({})\n", holders.sql)),
            "{}",
            top.sql
        );
        assert_eq!(top.reads, BTreeSet::from(["vault__transfer".to_string()]));
    }

    #[test]
    fn a_view_over_a_refused_view_is_refused_and_says_which() {
        let out = run(&[
            (
                "10-a.sql",
                "CREATE VIEW a AS SELECT avg(value_dec) AS m FROM vault__transfer;",
            ),
            ("20-b.sql", "CREATE VIEW b AS SELECT m FROM a;"),
        ]);
        assert_eq!(
            out[1].result.as_ref().unwrap_err(),
            "reads view `a`, which did not translate"
        );
    }

    #[test]
    fn a_statement_that_defines_no_view_is_named_rather_than_skipped() {
        let out = run(&[(
            "10-x.sql",
            "-- a note\nSELECT 1;\nCREATE VIEW ok AS SELECT 1 AS one;",
        )]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].statement, 1);
        assert!(out[0].name.is_none());
        assert!(out[0]
            .result
            .as_ref()
            .unwrap_err()
            .contains("not a `CREATE VIEW`"));
        assert_eq!(
            out[1].result.as_ref().unwrap().sql,
            "SELECT\n    1 AS \"one\""
        );
    }

    #[test]
    fn a_view_defined_twice_fails_the_run() {
        let files = [NestViewFile {
            file: "10-x.sql".into(),
            sql: "CREATE VIEW x AS SELECT 1 AS a; CREATE OR REPLACE VIEW x AS SELECT 2 AS a;"
                .into(),
        }];
        let err = translate(&files, &tables(), "src").err().unwrap();
        assert!(format!("{err:#}").contains("a second time"), "{err:#}");
    }
}
