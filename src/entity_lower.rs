//! RFC-0041 §4 step 3: lowering an authored `SELECT` into a [`Plan`] (#870).
//!
//! [`entities::validate_sql`](crate::entities) already decides **whether** a statement is in the
//! §3.3 subset - the allowlist #836 landed, plus the syntax-form refusals beside it. This is the
//! other half: turning the ones that are admitted into the relational shape the circuit is built
//! from.
//!
//! Both halves read the same serialized parse, `json_serialize_sql`, so there is one parser and one
//! opinion about what the author wrote. A second parser here would eventually disagree with the gate
//! about some corner, and the disagreement would show up as an entity that validated and then would
//! not build.
//!
//! ## What v1 admits, beyond the gate
//!
//! The gate refuses by *construct*. This refuses by *shape*, and the two are not the same list:
//!
//! - one table, or two joined by a single `INNER JOIN ... ON a = b`
//! - a `WHERE` whose every conjunct reads one side only. A conjunct spanning both sides is a join
//!   predicate the plan has no room for, and silently pushing it to one side would change the answer
//! - a select list of the grouping expressions first, then the aggregates
//! - no `HAVING`, no CTEs, no `QUALIFY`
//!
//! Every refusal names what to do instead, because an author reading it has a working query and a
//! tool telling them no.

use crate::entity_expr::{Cmp, Expr, Type};
use crate::entity_plan::{Agg, Join, Plan, Source};
use crate::entity_row::Scalar;
use anyhow::{anyhow, bail, Context, Result};
use duckdb::Connection;
use serde_json::Value;

/// Parse and lower one authored entity `SELECT`.
pub fn lower(sql: &str) -> Result<Plan> {
    let conn = Connection::open_in_memory()?;
    let literal = format!("'{}'", sql.replace('\'', "''"));
    let raw: String = conn
        .query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
            r.get(0)
        })
        .context("parsing the entity SQL")?;
    let ast: Value = serde_json::from_str(&raw)?;
    lower_ast(&ast)
}

/// Lower an already-parsed statement. Split out so tests and the validator can share one parse.
pub fn lower_ast(ast: &Value) -> Result<Plan> {
    let node = ast
        .pointer("/statements/0/node")
        .ok_or_else(|| anyhow!("no statement to lower"))?;
    if node.get("type").and_then(Value::as_str) != Some("SELECT_NODE") {
        bail!("an entity is one SELECT; keep other SQL as views/*.sql")
    }
    for (field, what) in [
        ("having", "HAVING"),
        ("qualify", "QUALIFY"),
        ("sample", "USING SAMPLE"),
    ] {
        if node.get(field).is_some_and(|v| !v.is_null()) {
            bail!("{what} is not incremental v1 SQL; keep this as views/*.sql")
        }
    }
    if node
        .pointer("/cte_map/map")
        .and_then(Value::as_array)
        .is_some_and(|m| !m.is_empty())
    {
        bail!("CTEs are not incremental v1 SQL; keep this as views/*.sql")
    }

    let from = node
        .get("from_table")
        .ok_or_else(|| anyhow!("an entity must read a table"))?;
    let (tables, join_on) = read_from(from)?;

    let select = node
        .get("select_list")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("an entity must select something"))?;
    let group = node
        .get("group_expressions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    // Column indices are assigned before anything is lowered, because an expression cannot be
    // lowered until its columns have positions. The order is the order they are first mentioned,
    // walking the statement in a fixed order - deterministic, and stable against an unrelated edit
    // elsewhere in the query.
    let mut columns = Columns::new(&tables);
    if let Some(w) = node.get("where_clause").filter(|v| !v.is_null()) {
        columns.collect(w)?;
    }
    if let Some((l, r)) = &join_on {
        columns.collect(l)?;
        columns.collect(r)?;
    }
    for e in group {
        columns.collect(e)?;
    }
    for e in select {
        columns.collect(e)?;
    }

    let (key_items, agg_items) = split_select(select)?;
    check_group_matches_key(&key_items, group, &columns)?;

    let joined = Scope::Joined;
    let key = key_items
        .iter()
        .map(|e| columns.expr(e, joined))
        .collect::<Result<Vec<_>>>()?;
    let aggregates = agg_items
        .iter()
        .map(|e| columns.aggregate(e, joined))
        .collect::<Result<Vec<_>>>()?;
    if aggregates.is_empty() {
        bail!(
            "an incremental entity must aggregate something. A SELECT that only projects rows is a \
             view; keep it as views/*.sql"
        )
    }

    let (left_filter, right_filter) = match node.get("where_clause").filter(|v| !v.is_null()) {
        None => (None, None),
        Some(w) => columns.split_where(w)?,
    };

    let left = Source {
        table: tables.left.table.clone(),
        columns: columns.left.clone(),
    };
    let join = match (&tables.right, &join_on) {
        (Some(right), Some((l, r))) => Some(Join {
            right: Source {
                table: right.table.clone(),
                columns: columns.right.clone(),
            },
            right_filter,
            on: columns.join_indices(l, r)?,
        }),
        _ => None,
    };

    Ok(Plan {
        left,
        left_filter,
        join,
        key,
        aggregates,
    })
}

/// One table as the statement names it.
struct TableRef {
    table: String,
    /// The alias if the author gave one, else the table name - the thing a qualified column ref will
    /// actually say.
    name: String,
}

struct Tables {
    left: TableRef,
    right: Option<TableRef>,
}

fn base_table(v: &Value) -> Result<TableRef> {
    if v.get("type").and_then(Value::as_str) != Some("BASE_TABLE") {
        bail!(
            "an entity reads tables directly; subqueries and table functions are not incremental \
             v1 SQL"
        )
    }
    let table = v
        .get("table_name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("a table with no name"))?
        .to_string();
    let alias = v.get("alias").and_then(Value::as_str).unwrap_or_default();
    Ok(TableRef {
        name: if alias.is_empty() {
            table.clone()
        } else {
            alias.to_string()
        },
        table,
    })
}

type JoinOn = Option<(Value, Value)>;

fn read_from(from: &Value) -> Result<(Tables, JoinOn)> {
    match from.get("type").and_then(Value::as_str) {
        Some("BASE_TABLE") => Ok((
            Tables {
                left: base_table(from)?,
                right: None,
            },
            None,
        )),
        Some("JOIN") => {
            if from.get("join_type").and_then(Value::as_str) != Some("INNER") {
                bail!("only INNER JOIN is incremental v1 SQL; keep this as views/*.sql")
            }
            if from
                .get("using_columns")
                .and_then(Value::as_array)
                .is_some_and(|c| !c.is_empty())
            {
                bail!("JOIN ... USING is not incremental v1 SQL; write the ON condition out")
            }
            let condition = from
                .get("condition")
                .filter(|v| !v.is_null())
                .ok_or_else(|| anyhow!("a join with no ON condition is a cross join"))?;
            if condition.get("type").and_then(Value::as_str) != Some("COMPARE_EQUAL") {
                bail!(
                    "only an equijoin on one column from each side is incremental v1 SQL. A \
                     compound or inequality join is not maintainable under retraction here"
                )
            }
            Ok((
                Tables {
                    left: base_table(from.get("left").unwrap_or(&Value::Null))?,
                    right: Some(base_table(from.get("right").unwrap_or(&Value::Null))?),
                },
                Some((
                    condition.get("left").cloned().unwrap_or(Value::Null),
                    condition.get("right").cloned().unwrap_or(Value::Null),
                )),
            ))
        }
        _ => bail!(
            "an entity reads one table, or two joined by INNER JOIN. Anything else is not \
             incremental v1 SQL"
        ),
    }
}

/// Which row an expression is being lowered against. The same column ref becomes a different index
/// depending on this, which is exactly why it is a parameter rather than an assumption: a `WHERE`
/// conjunct is evaluated against one side alone, while a key or an aggregate sees the joined row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Left,
    Right,
    Joined,
}

/// Which side a column belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Left,
    Right,
}

struct Columns<'a> {
    tables: &'a Tables,
    left: Vec<String>,
    right: Vec<String>,
}

impl<'a> Columns<'a> {
    fn new(tables: &'a Tables) -> Self {
        Columns {
            tables,
            left: Vec::new(),
            right: Vec::new(),
        }
    }

    /// Resolve a column reference to its side and name.
    fn resolve(&self, names: &[Value]) -> Result<(Side, String)> {
        let parts: Vec<&str> = names.iter().filter_map(Value::as_str).collect();
        match parts.as_slice() {
            [column] => {
                if let Some(right) = &self.tables.right {
                    bail!(
                        "`{column}` is not qualified, and this entity reads two tables. Write \
                         `{}.{column}` or `{}.{column}` so it cannot mean either",
                        self.tables.left.name,
                        right.name
                    )
                }
                Ok((Side::Left, (*column).to_string()))
            }
            [table, column] => {
                if *table == self.tables.left.name {
                    Ok((Side::Left, (*column).to_string()))
                } else if self.tables.right.as_ref().is_some_and(|r| r.name == *table) {
                    Ok((Side::Right, (*column).to_string()))
                } else {
                    bail!("`{table}` is not a table this entity reads")
                }
            }
            _ => bail!(
                "`{}` is not a column reference this entity can use",
                parts.join(".")
            ),
        }
    }

    /// Register every column an expression mentions, in first-mention order.
    fn collect(&mut self, e: &Value) -> Result<()> {
        if e.get("class").and_then(Value::as_str) == Some("COLUMN_REF") {
            let names = e
                .get("column_names")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let (side, column) = self.resolve(names)?;
            let into = match side {
                Side::Left => &mut self.left,
                Side::Right => &mut self.right,
            };
            if !into.contains(&column) {
                into.push(column);
            }
            return Ok(());
        }
        for child in children(e) {
            self.collect(child)?;
        }
        Ok(())
    }

    fn index(&self, side: Side, column: &str, scope: Scope) -> Result<usize> {
        let list = match side {
            Side::Left => &self.left,
            Side::Right => &self.right,
        };
        let within = list
            .iter()
            .position(|c| c == column)
            .ok_or_else(|| anyhow!("`{column}` was never registered; this is a lowering bug"))?;
        Ok(match (scope, side) {
            (Scope::Left, Side::Left) | (Scope::Right, Side::Right) => within,
            (Scope::Joined, Side::Left) => within,
            (Scope::Joined, Side::Right) => self.left.len() + within,
            // A filter on one side reading the other is the case `split_where` refuses before it
            // gets here; reaching this would be a lowering bug rather than an authoring mistake.
            (Scope::Left, Side::Right) | (Scope::Right, Side::Left) => bail!(
                "`{column}` belongs to the other side of the join than the expression using it"
            ),
        })
    }

    fn join_indices(&self, left: &Value, right: &Value) -> Result<(usize, usize)> {
        let a = self.column_ref(left)?;
        let b = self.column_ref(right)?;
        // `ON i.indexer = d.indexer` is the same join as `ON d.indexer = i.indexer`, and an author
        // writing it the other way round should not get a refusal.
        match (a, b) {
            ((Side::Left, l), (Side::Right, r)) => Ok((
                self.index(Side::Left, &l, Scope::Left)?,
                self.index(Side::Right, &r, Scope::Right)?,
            )),
            ((Side::Right, r), (Side::Left, l)) => Ok((
                self.index(Side::Left, &l, Scope::Left)?,
                self.index(Side::Right, &r, Scope::Right)?,
            )),
            _ => bail!(
                "the join condition must compare one column from each side. Comparing two columns \
                 of the same table is a filter, not a join"
            ),
        }
    }

    fn column_ref(&self, e: &Value) -> Result<(Side, String)> {
        if e.get("class").and_then(Value::as_str) != Some("COLUMN_REF") {
            bail!("the join condition must compare two columns")
        }
        self.resolve(
            e.get("column_names")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )
    }

    /// Which sides an expression reads. Empty means it reads none - a constant.
    fn sides(&self, e: &Value) -> Result<Vec<Side>> {
        let mut out = Vec::new();
        self.sides_into(e, &mut out)?;
        Ok(out)
    }

    fn sides_into(&self, e: &Value, out: &mut Vec<Side>) -> Result<()> {
        if e.get("class").and_then(Value::as_str) == Some("COLUMN_REF") {
            let (side, _) = self.resolve(
                e.get("column_names")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
            if !out.contains(&side) {
                out.push(side);
            }
            return Ok(());
        }
        for child in children(e) {
            self.sides_into(child, out)?;
        }
        Ok(())
    }

    /// Split a `WHERE` into a per-side filter each, or refuse.
    ///
    /// A conjunct reading both sides is a join predicate. The plan has one equijoin and no room for
    /// it, and pushing it to either side would change the answer, so it is refused rather than
    /// approximated.
    fn split_where(&self, where_clause: &Value) -> Result<(Option<Expr>, Option<Expr>)> {
        let mut left: Option<Expr> = None;
        let mut right: Option<Expr> = None;
        for conjunct in conjuncts(where_clause) {
            let sides = self.sides(conjunct)?;
            let (scope, slot) = match sides.as_slice() {
                // A conjunct reading no column at all is a constant. It belongs to the left, which
                // always exists.
                [] | [Side::Left] => (Scope::Left, &mut left),
                [Side::Right] => (Scope::Right, &mut right),
                _ => bail!(
                    "this WHERE condition reads both sides of the join. Move it into the ON \
                     condition, or keep the query as views/*.sql - an incremental entity filters \
                     each side before joining"
                ),
            };
            let lowered = self.expr(conjunct, scope)?;
            *slot = Some(match slot.take() {
                None => lowered,
                Some(prior) => Expr::And(prior.into(), lowered.into()),
            });
        }
        Ok((left, right))
    }

    /// Lower one expression.
    fn expr(&self, e: &Value, scope: Scope) -> Result<Expr> {
        let class = e.get("class").and_then(Value::as_str).unwrap_or("");
        let kind = e.get("type").and_then(Value::as_str).unwrap_or("");
        match class {
            "COLUMN_REF" => {
                let (side, column) = self.resolve(
                    e.get("column_names")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                )?;
                Ok(Expr::Column(self.index(side, &column, scope)?))
            }
            "CONSTANT" => constant(e),
            "COMPARISON" => {
                let cmp = match kind {
                    "COMPARE_EQUAL" => Cmp::Eq,
                    "COMPARE_NOTEQUAL" => Cmp::Ne,
                    "COMPARE_LESSTHAN" => Cmp::Lt,
                    "COMPARE_GREATERTHAN" => Cmp::Gt,
                    "COMPARE_LESSTHANOREQUALTO" => Cmp::Le,
                    "COMPARE_GREATERTHANOREQUALTO" => Cmp::Ge,
                    other => bail!("`{other}` is not a comparison incremental v1 SQL admits"),
                };
                let (l, r) = binary(e)?;
                Ok(Expr::Compare(
                    cmp,
                    self.expr(l, scope)?.into(),
                    self.expr(r, scope)?.into(),
                ))
            }
            "CONJUNCTION" => {
                let mut parts = children(e).map(|c| self.expr(c, scope));
                let first = parts
                    .next()
                    .ok_or_else(|| anyhow!("an AND/OR with nothing in it"))??;
                parts.try_fold(first, |acc, next| {
                    let next = next?;
                    Ok(match kind {
                        "CONJUNCTION_AND" => Expr::And(acc.into(), next.into()),
                        "CONJUNCTION_OR" => Expr::Or(acc.into(), next.into()),
                        other => bail!("`{other}` is not incremental v1 SQL"),
                    })
                })
            }
            "OPERATOR" => match kind {
                "OPERATOR_NOT" => Ok(Expr::Not(self.expr(only_child(e)?, scope)?.into())),
                "OPERATOR_IS_NULL" => Ok(Expr::IsNull(self.expr(only_child(e)?, scope)?.into())),
                "OPERATOR_IS_NOT_NULL" => Ok(Expr::Not(
                    Expr::IsNull(self.expr(only_child(e)?, scope)?.into()).into(),
                )),
                "OPERATOR_COALESCE" => Ok(Expr::Coalesce(
                    children(e)
                        .map(|c| self.expr(c, scope))
                        .collect::<Result<Vec<_>>>()?,
                )),
                other => bail!("`{other}` is not incremental v1 SQL"),
            },
            "CASE" => self.case(e, scope),
            "CAST" => {
                let to = e
                    .pointer("/cast_type/id")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let ty = match to {
                    "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT"
                    | "USMALLINT" | "UINTEGER" | "UBIGINT" => Type::Int,
                    "VARCHAR" => Type::Str,
                    "BOOLEAN" => Type::Bool,
                    other => bail!(
                        "a cast to {other} is not incremental v1 SQL. §3.3 admits exact integers, \
                         strings and booleans; floating point is refused deliberately"
                    ),
                };
                let child = e.get("child").ok_or_else(|| anyhow!("a cast of nothing"))?;
                Ok(Expr::Cast(self.expr(child, scope)?.into(), ty))
            }
            "FUNCTION" => {
                let name = e
                    .get("function_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let (l, r) = binary(e)?;
                match name {
                    "+" => Ok(Expr::Add(
                        self.expr(l, scope)?.into(),
                        self.expr(r, scope)?.into(),
                    )),
                    "-" => Ok(Expr::Sub(
                        self.expr(l, scope)?.into(),
                        self.expr(r, scope)?.into(),
                    )),
                    "*" => Ok(Expr::Mul(
                        self.expr(l, scope)?.into(),
                        self.expr(r, scope)?.into(),
                    )),
                    other => bail!(
                        "`{other}` is not a function incremental v1 SQL admits. §3.3 admits \
                         `+`, `-`, `*` and the six aggregates"
                    ),
                }
            }
            other => bail!("`{other}` is not an expression incremental v1 SQL admits"),
        }
    }

    fn case(&self, e: &Value, scope: Scope) -> Result<Expr> {
        let checks = e
            .get("case_checks")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("a CASE with no WHEN"))?;
        let whens = checks
            .iter()
            .map(|c| {
                let when = c
                    .get("when_expr")
                    .ok_or_else(|| anyhow!("a WHEN with no condition"))?;
                let then = c
                    .get("then_expr")
                    .ok_or_else(|| anyhow!("a WHEN with no result"))?;
                Ok((self.expr(when, scope)?, self.expr(then, scope)?))
            })
            .collect::<Result<Vec<_>>>()?;
        let otherwise = match e.get("else_expr").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => Some(Box::new(self.expr(v, scope)?)),
        };
        Ok(Expr::Case { whens, otherwise })
    }

    /// Lower one aggregate from the select list.
    fn aggregate(&self, e: &Value, scope: Scope) -> Result<Agg> {
        let name = e
            .get("function_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let args: Vec<&Value> = children(e).collect();
        if name == "count_star" {
            return Ok(Agg::Count);
        }
        let arg = match args.as_slice() {
            [one] => *one,
            _ => bail!("`{name}` takes exactly one argument in incremental v1 SQL"),
        };
        let inner = self.expr(arg, scope)?;
        Ok(match name.as_str() {
            "sum" => Agg::Sum(inner),
            "min" => Agg::Min(inner),
            "max" => Agg::Max(inner),
            "avg" => Agg::Avg(inner),
            // `count(x)` counts non-NULL `x`; the plan's `Count` is `count(*)`. Rather than lower a
            // different aggregate under the same name, say so.
            "count" => bail!(
                "`count(x)` counts non-NULL values and incremental v1 maintains `count(*)`. Write \
                 `count(*)`, or `sum(CASE WHEN x IS NULL THEN 0 ELSE 1 END)`"
            ),
            other => bail!("`{other}` is not an aggregate incremental v1 SQL maintains"),
        })
    }
}

/// Split the select list into the leading grouping expressions and the trailing aggregates.
fn split_select(select: &[Value]) -> Result<(Vec<&Value>, Vec<&Value>)> {
    let first_agg = select.iter().position(is_aggregate).unwrap_or(select.len());
    let (key, aggs) = select.split_at(first_agg);
    if let Some(stray) = aggs.iter().position(|e| !is_aggregate(e)) {
        bail!(
            "select item {} is not an aggregate but follows one. An incremental entity selects its \
             grouping expressions first, then its aggregates",
            first_agg + stray + 1
        )
    }
    Ok((key.iter().collect(), aggs.iter().collect()))
}

fn is_aggregate(e: &Value) -> bool {
    e.get("class").and_then(Value::as_str) == Some("FUNCTION")
        && matches!(
            e.get("function_name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "sum" | "min" | "max" | "avg" | "count" | "count_star"
        )
}

/// The grouping expressions and the leading select items must be the same set.
///
/// Not the same *sequence*: `SELECT b, a, count(*) ... GROUP BY a, b` is perfectly ordinary SQL and
/// the entity's key is what the select list says, because that is the order the author will read the
/// answer in.
fn check_group_matches_key(key: &[&Value], group: &[Value], columns: &Columns) -> Result<()> {
    let mut want: Vec<String> = group.iter().map(canonical).collect();
    let mut got: Vec<String> = key.iter().map(|e| canonical(e)).collect();
    want.sort();
    got.sort();
    if want != got {
        bail!(
            "the grouping expressions and the non-aggregate select items must be the same set. \
             GROUP BY has [{}]; the select list has [{}]",
            want.join(", "),
            got.join(", ")
        )
    }
    let _ = columns;
    Ok(())
}

/// A stable spelling of an expression, for comparing GROUP BY against the select list. Positions in
/// the source text differ between the two and must not count as a difference.
fn canonical(e: &Value) -> String {
    let mut stripped = e.clone();
    strip_locations(&mut stripped);
    stripped.to_string()
}

fn strip_locations(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("query_location");
            map.remove("alias");
            for (_, child) in map.iter_mut() {
                strip_locations(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_locations),
        _ => {}
    }
}

/// Flatten an `AND` chain into its conjuncts, so each can be classified by the side it reads.
fn conjuncts(e: &Value) -> Vec<&Value> {
    if e.get("class").and_then(Value::as_str) == Some("CONJUNCTION")
        && e.get("type").and_then(Value::as_str) == Some("CONJUNCTION_AND")
    {
        return children(e).flat_map(conjuncts).collect();
    }
    vec![e]
}

/// Every sub-expression of a node, whatever the node calls them.
fn children(e: &Value) -> impl Iterator<Item = &Value> {
    const NAMED: &[&str] = &[
        "left",
        "right",
        "child",
        "when_expr",
        "then_expr",
        "else_expr",
    ];
    e.get("children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .chain(
            e.get("case_checks")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .flat_map(|c| {
                    NAMED
                        .iter()
                        .filter_map(move |k| c.get(*k).filter(|v| !v.is_null()))
                }),
        )
        .chain(NAMED.iter().filter_map(move |k| {
            // A join's `left`/`right` are table refs rather than expressions, and a CASE's checks
            // are reached above; everything else named here is a sub-expression.
            e.get(*k)
                .filter(|v| !v.is_null() && v.get("class").is_some())
        }))
}

fn binary(e: &Value) -> Result<(&Value, &Value)> {
    let named = (e.get("left"), e.get("right"));
    if let (Some(l), Some(r)) = named {
        if !l.is_null() && !r.is_null() {
            return Ok((l, r));
        }
    }
    let kids: Vec<&Value> = e
        .get("children")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .collect();
    match kids.as_slice() {
        [l, r] => Ok((l, r)),
        _ => bail!("this operator takes two operands in incremental v1 SQL"),
    }
}

fn only_child(e: &Value) -> Result<&Value> {
    let kids: Vec<&Value> = children(e).collect();
    match kids.as_slice() {
        [one] => Ok(one),
        _ => bail!("this operator takes one operand"),
    }
}

/// A literal, exactly. §3.3 admits integers, strings and booleans and refuses floating point, so a
/// `DOUBLE` literal is a refusal rather than a rounded integer.
fn constant(e: &Value) -> Result<Expr> {
    let value = e
        .get("value")
        .ok_or_else(|| anyhow!("a constant with no value"))?;
    if value.get("is_null").and_then(Value::as_bool) == Some(true) {
        return Ok(Expr::Literal(Scalar::Null));
    }
    let id = value
        .pointer("/type/id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let raw = value
        .get("value")
        .ok_or_else(|| anyhow!("a constant with no value"))?;
    Ok(Expr::Literal(match id {
        "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT" | "USMALLINT"
        | "UINTEGER" | "UBIGINT" => Scalar::Int(
            raw.as_i64()
                .map(i128::from)
                .or_else(|| raw.as_str().and_then(|s| s.parse().ok()))
                .ok_or_else(|| anyhow!("{raw} does not fit an exact integer"))?,
        ),
        "VARCHAR" => Scalar::Str(
            raw.as_str()
                .ok_or_else(|| anyhow!("a VARCHAR constant that is not a string"))?
                .to_string(),
        ),
        "BOOLEAN" => Scalar::Bool(
            raw.as_bool()
                .ok_or_else(|| anyhow!("a BOOLEAN constant that is not a boolean"))?,
        ),
        other => bail!(
            "a {other} literal is not incremental v1 SQL. §3.3 admits exact integers, strings and \
             booleans, and refuses floating point so an entity cannot drift"
        ),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity_row::Row;

    fn col(i: usize) -> Expr {
        Expr::Column(i)
    }
    fn int(i: i128) -> Expr {
        Expr::Literal(Scalar::Int(i))
    }
    fn s(v: &str) -> Scalar {
        Scalar::Str(v.into())
    }
    fn src(t: &str, cols: &[&str]) -> Source {
        Source {
            table: t.into(),
            columns: cols.iter().map(|c| c.to_string()).collect(),
        }
    }
    fn refusal(sql: &str) -> String {
        format!("{:#}", lower(sql).unwrap_err())
    }

    /// **The Lodestar shape, from SQL this time.** `entity_plan`'s tests hand-build this plan and
    /// `entity_circuit`'s run it; here the same plan comes out of the author's own query. That is
    /// the last link: what a person writes and what the circuit does are now the same relation by
    /// construction rather than by somebody transcribing it.
    #[test]
    fn the_lodestar_query_lowers_to_the_plan_its_tests_hand_built() {
        let plan = lower(
            "SELECT d.indexer, d.delegator, SUM(d.amount) \
             FROM delegations d JOIN indexers i ON d.indexer = i.indexer \
             WHERE d.amount > 0 AND i.active \
             GROUP BY d.indexer, d.delegator",
        )
        .unwrap();

        assert_eq!(
            plan,
            Plan {
                left: src("delegations", &["amount", "indexer", "delegator"]),
                left_filter: Some(Expr::Compare(Cmp::Gt, col(0).into(), int(0).into())),
                join: Some(Join {
                    right: src("indexers", &["active", "indexer"]),
                    right_filter: Some(col(0)),
                    on: (1, 1),
                }),
                key: vec![col(1), col(2)],
                aggregates: vec![Agg::Sum(col(0))],
            },
            "columns are numbered in first-mention order: the WHERE is walked before the select list"
        );
    }

    /// End to end on the corpus slice 0 used: SQL in, entity out. Evaluated through the batch
    /// oracle rather than the circuit, because the circuit is a separate change and this one must
    /// not depend on it - the two meet on `main`, and `entity_circuit`'s own tests assert they
    /// agree on every plan they are given (§8).
    #[test]
    fn a_lowered_plan_evaluates_to_the_answer_the_query_describes() {
        let plan = lower(
            "SELECT d.indexer, d.delegator, SUM(d.amount) \
             FROM delegations d JOIN indexers i ON d.indexer = i.indexer \
             WHERE d.amount > 0 AND i.active \
             GROUP BY d.indexer, d.delegator",
        )
        .unwrap();

        // Column order is the plan's, which is the lowerer's, which is why these are built from it
        // rather than written out by hand: (amount, indexer, delegator) and (active, indexer).
        let d = |indexer: &str, delegator: &str, amount: i128| {
            Row(vec![Scalar::Int(amount), s(indexer), s(delegator)])
        };
        let i = |indexer: &str, active: bool| Row(vec![Scalar::Bool(active), s(indexer)]);

        let left = [
            d("i1", "a", 7),
            d("i1", "a", 5),
            d("i1", "b", -3),
            d("i2", "c", 11),
        ];
        let right = [i("i1", true), i("i2", false)];

        let got = plan.evaluate(&left, &right).unwrap();
        assert_eq!(
            got.get(&Row(vec![s("i1"), s("a")])),
            Some(&Row(vec![Scalar::Int(12)])),
            "7+5, with the negative filtered and the inactive indexer dropped"
        );
        assert_eq!(got.len(), 1, "{got:?}");
    }

    /// One table, no join, `count(*)`.
    #[test]
    fn a_single_table_count_lowers_without_a_join() {
        let plan = lower("SELECT owner, count(*) FROM transfers GROUP BY owner").unwrap();
        assert_eq!(
            plan,
            Plan {
                left: src("transfers", &["owner"]),
                left_filter: None,
                join: None,
                key: vec![col(0)],
                aggregates: vec![Agg::Count],
            }
        );
    }

    /// The grouping expressions and the non-aggregate select items must be the same set, but need
    /// not be in the same order - the key follows the select list, which is the order the author
    /// reads the answer in.
    #[test]
    fn the_key_follows_the_select_list_not_the_group_by() {
        let plan = lower("SELECT b, a, count(*) FROM t GROUP BY a, b").unwrap();
        assert_eq!(plan.left.columns, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(plan.key, vec![col(1), col(0)], "b then a, as selected");
    }

    /// A `WHERE` conjunct reading both sides is a join predicate the plan has no room for. Pushing
    /// it to either side would change the answer, so it is refused rather than approximated.
    #[test]
    fn a_where_condition_spanning_both_sides_is_refused_with_a_way_out() {
        let err = refusal(
            "SELECT l.k, count(*) FROM l JOIN r ON l.k = r.k WHERE l.amount > r.floor GROUP BY l.k",
        );
        assert!(err.contains("reads both sides"), "{err}");
        assert!(
            err.contains("ON condition"),
            "the refusal must say what to do: {err}"
        );
    }

    /// Two tables and an unqualified column is ambiguous to a reader as well as to a lowerer, and
    /// the refusal names both ways of resolving it.
    #[test]
    fn an_unqualified_column_across_a_join_is_refused_and_the_error_offers_both_spellings() {
        let err = refusal(
            "SELECT l.k, count(*) FROM l JOIN r ON l.k = r.k WHERE amount > 0 GROUP BY l.k",
        );
        assert!(err.contains("not qualified"), "{err}");
        assert!(
            err.contains("l.amount") && err.contains("r.amount"),
            "{err}"
        );
    }

    /// `ON i.indexer = d.indexer` is the same join written the other way round, and an author should
    /// not have to know which side the lowerer prefers.
    #[test]
    fn the_join_condition_may_name_its_sides_in_either_order() {
        let forwards =
            lower("SELECT d.k, count(*) FROM d JOIN i ON d.k = i.k GROUP BY d.k").unwrap();
        let backwards =
            lower("SELECT d.k, count(*) FROM d JOIN i ON i.k = d.k GROUP BY d.k").unwrap();
        assert_eq!(forwards, backwards);
        assert_eq!(forwards.join.unwrap().on, (0, 0));
    }

    /// Both join columns being first in their own list makes `(left, right)` and `(right, left)`
    /// indistinguishable. This query puts each one second, and writes the ON condition right-hand
    /// side first for good measure.
    #[test]
    fn each_join_column_is_indexed_within_its_own_side() {
        let plan = lower(
            "SELECT d.owner, SUM(d.amt) FROM d JOIN i ON i.name = d.owner \
             WHERE d.amt > 0 AND i.active GROUP BY d.owner",
        )
        .unwrap();

        assert_eq!(
            plan.left.columns,
            vec!["amt".to_string(), "owner".to_string()]
        );
        let join = plan.join.unwrap();
        assert_eq!(
            join.right.columns,
            vec!["active".to_string(), "name".to_string()]
        );
        assert_eq!(
            join.on,
            (1, 1),
            "column 1 of the left row against column 1 of the right row"
        );
    }

    /// A key or an aggregate reads the *joined* row, so a right-hand column sits past the left
    /// row's width. Every other test here reads only left columns, which cannot tell the difference.
    #[test]
    fn a_key_reading_a_right_hand_column_is_offset_past_the_left_row() {
        let plan =
            lower("SELECT r.region, SUM(l.amount) FROM l JOIN r ON l.k = r.k GROUP BY r.region")
                .unwrap();

        assert_eq!(
            plan.left.columns,
            vec!["k".to_string(), "amount".to_string()]
        );
        let join = plan.join.clone().unwrap();
        assert_eq!(
            join.right.columns,
            vec!["k".to_string(), "region".to_string()]
        );
        assert_eq!(
            plan.key,
            vec![col(3)],
            "the right side's `region` is column 1 of a 2-column right row, so column 3 of the join"
        );
        assert_eq!(plan.aggregates, vec![Agg::Sum(col(1))]);
    }

    /// Grouping by one column and selecting another is a SQL error in any engine, and a lowerer that
    /// shrugged at it would produce an entity keyed by something the author never asked for.
    #[test]
    fn selecting_a_column_that_is_not_grouped_is_refused() {
        let err = refusal("SELECT a, count(*) FROM t GROUP BY b");
        assert!(err.contains("same set"), "{err}");
    }

    #[test]
    fn an_outer_join_is_refused() {
        let err = refusal("SELECT l.k, count(*) FROM l LEFT JOIN r ON l.k = r.k GROUP BY l.k");
        assert!(err.contains("INNER JOIN"), "{err}");
    }

    /// A `SELECT` that only projects is a view. Saying so is more use than lowering it to a plan
    /// with no aggregates, which the circuit would build and then maintain nothing in.
    #[test]
    fn a_select_with_no_aggregate_is_refused_as_a_view() {
        let err = refusal("SELECT a, b FROM t GROUP BY a, b");
        assert!(err.contains("must aggregate"), "{err}");
        assert!(err.contains("views/*.sql"), "{err}");
    }

    /// `count(x)` and `count(*)` are different aggregates, and quietly lowering one as the other
    /// would give a wrong answer for every NULL in the column.
    #[test]
    fn count_of_a_column_is_refused_rather_than_lowered_as_count_star() {
        let err = refusal("SELECT a, count(b) FROM t GROUP BY a");
        assert!(err.contains("count(*)"), "{err}");
        assert!(err.contains("non-NULL"), "{err}");
    }

    /// §3.3 refuses floating point so an entity cannot drift. A `DOUBLE` literal is where that would
    /// otherwise slip in unnoticed.
    #[test]
    fn a_floating_point_literal_is_refused_however_it_is_spelled() {
        // `1.5` parses as DECIMAL and `1e0` as DOUBLE. Testing only the first leaves the second
        // admitted, which is the spelling an author reaching for a float actually writes.
        for sql in [
            "SELECT a, SUM(b) FROM t WHERE b > 1.5 GROUP BY a",
            "SELECT a, SUM(b) FROM t WHERE b > 1e0 GROUP BY a",
        ] {
            let err = refusal(sql);
            assert!(err.contains("not incremental v1 SQL"), "{sql}: {err}");
            assert!(err.contains("floating point"), "{sql}: {err}");
        }
    }

    /// A non-aggregate after an aggregate has nowhere to go in a plan whose output is key then
    /// aggregates, and the refusal says exactly which item and what to do.
    #[test]
    fn a_grouping_column_after_an_aggregate_is_refused_by_position() {
        let err = refusal("SELECT count(*), a FROM t GROUP BY a");
        assert!(err.contains("select item 2"), "{err}");
        assert!(err.contains("grouping expressions first"), "{err}");
    }

    /// The whole of §3.3's expression subset, in one query, so a lowering that quietly drops one of
    /// them has somewhere to fail.
    #[test]
    fn arithmetic_case_coalesce_and_null_tests_all_lower() {
        let plan = lower(
            "SELECT t.a, SUM(t.b * 2 + t.c - 1) \
             FROM t \
             WHERE t.d IS NOT NULL AND COALESCE(t.e, 0) > 0 \
               AND CASE WHEN t.f THEN t.g ELSE 0 END > 1 \
             GROUP BY t.a",
        )
        .unwrap();

        let summed = format!("{:?}", plan.aggregates[0]);
        for expected in ["Mul", "Add", "Sub"] {
            assert!(
                summed.contains(expected),
                "{expected} missing from {summed}"
            );
        }

        let filter = format!("{:?}", plan.left_filter.unwrap());
        for expected in ["Not(IsNull", "Coalesce", "Case"] {
            assert!(
                filter.contains(expected),
                "{expected} missing from {filter}"
            );
        }
    }

    #[test]
    fn a_cast_to_a_type_outside_the_subset_is_refused() {
        let err = refusal("SELECT a, SUM(CAST(b AS DOUBLE)) FROM t GROUP BY a");
        assert!(err.contains("not incremental v1 SQL"), "{err}");
    }

    #[test]
    fn a_cte_is_refused() {
        let err = refusal("WITH x AS (SELECT 1 AS a) SELECT a, count(*) FROM x GROUP BY a");
        assert!(
            err.contains("CTE") || err.contains("reads tables directly"),
            "{err}"
        );
    }
}
