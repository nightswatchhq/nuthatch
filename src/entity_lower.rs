//! RFC-0041 §4 step 3: lowering an authored `SELECT` into a [`Plan`] (#870).
//!
//! [`entities::validate_sql`](crate::entities) already decides **whether** a statement is in the
//! §3.3 subset - the allowlist #836 landed, plus the syntax-form refusals beside it. This is the
//! other half: turning the ones that are admitted into the relational shape the circuit is built
//! from.
//!
//! Both halves read the same parse, [`parse`], so there is one parser and one opinion about what the
//! author wrote. A second parser here would eventually disagree with the gate about some corner, and
//! the disagreement would show up as an entity that validated and then would not build.
//!
//! The parser is sqlparser's DuckDB dialect, read through [`Node`]: the shape DuckDB's own parser
//! gave these statements when it was the parser here, so the admitted subset did not move with it.
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
use anyhow::{anyhow, bail, Result};
use sqlparser::ast::{
    self as ast, BinaryOperator, CastKind, DuplicateTreatment, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, JoinConstraint, JoinOperator, SelectItem, SetExpr, Statement,
    TableFactor, TableWithJoins, UnaryOperator, Value, Visit, Visitor,
};
use sqlparser::dialect::DuckDbDialect;
use sqlparser::parser::Parser;
use std::ops::ControlFlow;

/// Parse and lower one authored entity `SELECT`, discarding its output column names.
///
/// Most callers want only the relational shape: a circuit and a batch evaluator index by position and
/// have no use for a name. [`lower_with_columns`] is for the serving surface, which does.
pub fn lower(sql: &str) -> Result<Plan> {
    lower_with_columns(sql).map(|(plan, _)| plan)
}

/// Lower, and keep the names the author gave the output columns (#822).
///
/// **`Plan` deliberately does not carry these.** It is positional because the circuit and the oracle
/// are positional, and 57 hand-built `Plan` literals across the tree would have had to gain a field
/// they never read. The names belong to the *served* entity, which is the only thing that needs them.
///
/// Each output column takes the author's `AS` alias when there is one. Without an alias: a bare column
/// reference keeps its own name, and an aggregate is named after what it does to which column -
/// `count`, `sum_amount`, `avg_tokens`. **Duplicates are refused at load**, because a SQL view with
/// two columns of one name is not a thing, and finding that out at first query rather than at nest
/// start is the failure mode this whole slice keeps removing.
pub fn lower_with_columns(sql: &str) -> Result<(Plan, Vec<String>)> {
    let statements = match parse(sql) {
        Ok(statements) => statements,
        Err(_) if crate::entities::uses_sample(sql) => {
            bail!("USING SAMPLE is not incremental v1 SQL; keep this as views/*.sql")
        }
        Err(e) => bail!("no statement to lower ({e})"),
    };
    let shape = shape(&statements)?;
    let plan = lower_shape(&shape)?;
    let columns = output_columns(&shape.items)?;
    // A consistency check between two traversals of one select list, not a validation of input:
    // `output_columns` names every item and `split_select` divides the same items into key and
    // aggregates, so this cannot fire today and **no test reaches it**. It is here because the two
    // walks are independent and an edit to either could desync them, at which point every column's
    // name would shift by one - silently, and only in the serving surface.
    if columns.len() != plan.key.len() + plan.aggregates.len() {
        bail!(
            "lowered {} key + {} aggregate columns but named {} of them; this is a lowering bug",
            plan.key.len(),
            plan.aggregates.len(),
            columns.len()
        )
    }
    Ok((plan, columns))
}

/// The one parse both halves of the gate read.
///
/// sqlparser reads a few forms DuckDB's parser refused outright; those are refused here as a parse
/// failure too, so the port cannot widen what is admitted.
pub(crate) fn parse(sql: &str) -> std::result::Result<Vec<Statement>, String> {
    let statements = Parser::parse_sql(&DuckDbDialect {}, sql).map_err(|e| e.to_string())?;
    let mut refusal = DuckDbRefuses(None);
    let _ = statements.visit(&mut refusal);
    match refusal.0 {
        Some(what) => Err(format!("{what} does not parse in DuckDB's SQL")),
        None => Ok(statements),
    }
}

struct DuckDbRefuses(Option<String>);

impl Visitor for DuckDbRefuses {
    type Break = ();

    fn pre_visit_expr(&mut self, e: &ast::Expr) -> ControlFlow<()> {
        if let ast::Expr::Function(f) = e {
            let name = function_name(f).1;
            if !f.within_group.is_empty()
                && !matches!(
                    name.as_str(),
                    "percentile_cont" | "percentile_disc" | "mode"
                )
            {
                self.0 = Some(format!("`{name}(...) WITHIN GROUP`"));
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_select(&mut self, select: &ast::Select) -> ControlFlow<()> {
        let (GroupByExpr::Expressions(_, modifiers) | GroupByExpr::All(modifiers)) =
            &select.group_by;
        if let Some(m) = modifiers.first() {
            self.0 = Some(format!("`GROUP BY ... {m}`"));
            return ControlFlow::Break(());
        }
        self.joins(&select.from)
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        match factor {
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.joins(std::slice::from_ref(table_with_joins)),
            _ => ControlFlow::Continue(()),
        }
    }
}

impl DuckDbRefuses {
    fn joins(&mut self, from: &[TableWithJoins]) -> ControlFlow<()> {
        for join in from.iter().flat_map(|t| &t.joins) {
            if join_type(&join.join_operator).is_none() {
                self.0 = Some(format!("`{join}`"));
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }
}

/// The function name as DuckDB recorded it: lowercased, with any schema split off.
pub(crate) fn function_name(f: &ast::Function) -> (String, String) {
    let parts = object_name_parts(&f.name);
    let (name, schema) = parts
        .split_last()
        .map_or((String::new(), &[][..]), |(n, s)| {
            (n.to_ascii_lowercase(), s)
        });
    (schema.join("."), name)
}

pub(crate) fn object_name_parts(name: &ast::ObjectName) -> Vec<String> {
    name.0
        .iter()
        .map(|p| match p {
            ast::ObjectNamePart::Identifier(i) => i.value.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// The aggregate-relevant name of a call, after the rewrites DuckDB's parser made: `count(*)` is
/// `count_star`, and the ordered-set forms are renamed into the vocabulary its catalogue classifies.
pub(crate) fn canonical_function_name(f: &ast::Function) -> String {
    let name = function_name(f).1;
    match name.as_str() {
        "count" if is_count_star(f) => "count_star".into(),
        "percentile_cont" if !f.within_group.is_empty() => "quantile_cont".into(),
        "percentile_disc" if !f.within_group.is_empty() => "quantile_disc".into(),
        _ => name,
    }
}

fn is_count_star(f: &ast::Function) -> bool {
    match &f.args {
        FunctionArguments::List(list) => {
            list.args.is_empty()
                || matches!(
                    list.args.as_slice(),
                    [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)]
                )
        }
        FunctionArguments::None | FunctionArguments::Subquery(_) => false,
    }
}

/// One expression as DuckDB's parser shaped it, which is what the lowering below was written against.
///
/// The differences from sqlparser's tree are the ones that change an answer or a refusal: `AND`/`OR`
/// chains are flat, a negated numeric literal is a literal, `true` is `CAST('t' AS BOOLEAN)`, a simple
/// `CASE x WHEN` compares, a `CASE` without `ELSE` has a NULL one, and `count(*)` is `count_star`.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Column(Vec<String>),
    Constant(Lit),
    Compare(&'static str, Box<Node>, Box<Node>),
    Conjunction(&'static str, Vec<Node>),
    Operator(&'static str, Vec<Node>),
    Case(Vec<(Node, Node)>, Box<Node>),
    Cast {
        to: String,
        child: Box<Node>,
        try_cast: bool,
    },
    Function {
        schema: String,
        name: String,
        args: Vec<Node>,
        distinct: bool,
        /// FILTER, ORDER BY and the like: significant to equality, never read by the lowering.
        extra: String,
    },
    /// Anything the lowering refuses by class. `children` are what DuckDB's tree let a walk reach.
    Other {
        class: &'static str,
        children: Vec<Node>,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
enum Lit {
    Null,
    Int(&'static str, i128),
    Str(String),
    Other(String, String),
}

fn node(e: &ast::Expr) -> Node {
    use ast::Expr as E;
    if let Some(n) = number(e) {
        return Node::Constant(literal(n));
    }
    let boxed = |e: &ast::Expr| Box::new(node(e));
    let not = |n: Node, negated: bool| {
        if negated {
            Node::Operator("OPERATOR_NOT", vec![n])
        } else {
            n
        }
    };
    let other = |class: &'static str, children: Vec<Node>| Node::Other {
        class,
        children,
        text: e.to_string(),
    };
    match e {
        E::Nested(inner) => node(inner),
        E::Identifier(i) => Node::Column(vec![i.value.clone()]),
        E::CompoundIdentifier(parts) => {
            Node::Column(parts.iter().map(|p| p.value.clone()).collect())
        }
        E::Value(v) => match &v.value {
            Value::Null => Node::Constant(Lit::Null),
            Value::Boolean(b) => boolean(*b),
            Value::Placeholder(_) => other("PARAMETER", Vec::new()),
            // DuckDB's parser read `x'ab'` as the text `xab`.
            Value::HexStringLiteral(s) => Node::Constant(Lit::Str(format!("x{s}"))),
            Value::SingleQuotedString(s)
            | Value::EscapedStringLiteral(s)
            | Value::NationalStringLiteral(s)
            | Value::TripleSingleQuotedString(s)
            | Value::TripleDoubleQuotedString(s) => Node::Constant(Lit::Str(s.clone())),
            Value::DollarQuotedString(s) => Node::Constant(Lit::Str(s.value.clone())),
            other => Node::Constant(Lit::Other("VARCHAR".into(), other.to_string())),
        },
        E::UnaryOp { op, expr } => match op {
            UnaryOperator::Not => Node::Operator("OPERATOR_NOT", vec![node(expr)]),
            UnaryOperator::Minus => operator_function("-", vec![node(expr)]),
            UnaryOperator::Plus => operator_function("+", vec![node(expr)]),
            UnaryOperator::BitwiseNot => operator_function("~", vec![node(expr)]),
            op => operator_function(&op.to_string(), vec![node(expr)]),
        },
        E::BinaryOp { left, op, right } => binary_op(left, op, right),
        E::IsNull(x) => Node::Operator("OPERATOR_IS_NULL", vec![node(x)]),
        E::IsNotNull(x) => Node::Operator("OPERATOR_IS_NOT_NULL", vec![node(x)]),
        E::IsTrue(x) => Node::Compare("COMPARE_NOT_DISTINCT_FROM", boxed(x), boolean(true).into()),
        E::IsNotTrue(x) => Node::Compare("COMPARE_DISTINCT_FROM", boxed(x), boolean(true).into()),
        E::IsFalse(x) => {
            Node::Compare("COMPARE_NOT_DISTINCT_FROM", boxed(x), boolean(false).into())
        }
        E::IsNotFalse(x) => Node::Compare("COMPARE_DISTINCT_FROM", boxed(x), boolean(false).into()),
        E::IsDistinctFrom(a, b) => Node::Compare("COMPARE_DISTINCT_FROM", boxed(a), boxed(b)),
        E::IsNotDistinctFrom(a, b) => {
            Node::Compare("COMPARE_NOT_DISTINCT_FROM", boxed(a), boxed(b))
        }
        E::InList {
            expr,
            list,
            negated,
        } => Node::Operator(
            if *negated {
                "COMPARE_NOT_IN"
            } else {
                "COMPARE_IN"
            },
            std::iter::once(node(expr))
                .chain(list.iter().map(node))
                .collect(),
        ),
        E::InSubquery { expr, negated, .. } => not(other("SUBQUERY", vec![node(expr)]), *negated),
        E::Exists { negated, .. } => not(other("SUBQUERY", Vec::new()), *negated),
        E::Subquery(_) => other("SUBQUERY", Vec::new()),
        E::AnyOp { left, right, .. } | E::AllOp { left, right, .. }
            if matches!(right.as_ref(), E::Subquery(_)) =>
        {
            other("SUBQUERY", vec![node(left)])
        }
        E::Between { negated, .. } => not(other("BETWEEN", Vec::new()), *negated),
        E::Like {
            negated,
            expr,
            pattern,
            escape_char,
            any: false,
        } => like(
            *negated,
            expr,
            pattern,
            escape_char,
            ("~~", "!~~", "like_escape"),
        ),
        E::ILike {
            negated,
            expr,
            pattern,
            escape_char,
            any: false,
        } => like(
            *negated,
            expr,
            pattern,
            escape_char,
            ("~~*", "!~~*", "ilike_escape"),
        ),
        E::SimilarTo {
            negated,
            expr,
            pattern,
            escape_char: None,
        } => not(
            operator_function("regexp_full_match", vec![node(expr), node(pattern)]),
            *negated,
        ),
        E::Cast {
            kind,
            expr,
            data_type,
            ..
        } => Node::Cast {
            to: type_id(data_type),
            child: boxed(expr),
            try_cast: matches!(kind, CastKind::TryCast | CastKind::SafeCast),
        },
        E::TypedString(typed) => Node::Cast {
            to: type_id(&typed.data_type),
            child: Box::new(Node::Constant(Lit::Str(
                typed.value.value.clone().into_string().unwrap_or_default(),
            ))),
            try_cast: false,
        },
        E::Function(f) => function(f, e),
        E::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let operand = operand.as_deref().map(node);
            let whens = conditions
                .iter()
                .map(|c| {
                    let when = node(&c.condition);
                    let when = match &operand {
                        Some(o) => Node::Compare("COMPARE_EQUAL", Box::new(o.clone()), when.into()),
                        None => when,
                    };
                    (when, node(&c.result))
                })
                .collect();
            let otherwise = else_result
                .as_deref()
                .map_or(Node::Constant(Lit::Null), node);
            Node::Case(whens, otherwise.into())
        }
        E::Collate { expr, .. } => other("COLLATE", vec![node(expr)]),
        E::Lambda(_) => other("LAMBDA", Vec::new()),
        E::Wildcard(_) | E::QualifiedWildcard(..) => other("STAR", Vec::new()),
        E::CompoundFieldAccess { root, .. } => Node::Operator("ARRAY_EXTRACT", vec![node(root)]),
        E::Extract { field, expr, .. } => operator_function(
            "date_part",
            vec![
                Node::Constant(Lit::Str(field.to_string().to_ascii_lowercase())),
                node(expr),
            ],
        ),
        E::Ceil { expr, .. } => operator_function("ceil", vec![node(expr)]),
        E::Floor { expr, .. } => operator_function("floor", vec![node(expr)]),
        E::Tuple(items) => operator_function("row", items.iter().map(node).collect()),
        E::Array(array) => operator_function("list_value", array.elem.iter().map(node).collect()),
        E::Interval(interval) => operator_function("interval", vec![node(&interval.value)]),
        _ => other("FUNCTION", Vec::new()),
    }
}

fn boolean(b: bool) -> Node {
    Node::Cast {
        to: "BOOLEAN".into(),
        child: Box::new(Node::Constant(Lit::Str(if b { "t" } else { "f" }.into()))),
        try_cast: false,
    }
}

fn operator_function(name: &str, args: Vec<Node>) -> Node {
    Node::Function {
        schema: String::new(),
        name: name.to_string(),
        args,
        distinct: false,
        extra: String::new(),
    }
}

fn like(
    negated: bool,
    expr: &ast::Expr,
    pattern: &ast::Expr,
    escape: &Option<ast::ValueWithSpan>,
    (op, negated_op, escaped): (&str, &str, &str),
) -> Node {
    match escape {
        None => operator_function(
            if negated { negated_op } else { op },
            vec![node(expr), node(pattern)],
        ),
        Some(c) => operator_function(
            &if negated {
                format!("not_{escaped}")
            } else {
                escaped.to_string()
            },
            vec![
                node(expr),
                node(pattern),
                node(&ast::Expr::Value(c.clone())),
            ],
        ),
    }
}

fn binary_op(left: &ast::Expr, op: &BinaryOperator, right: &ast::Expr) -> Node {
    use BinaryOperator as B;
    let compare = |kind| Node::Compare(kind, Box::new(node(left)), Box::new(node(right)));
    match op {
        B::And | B::Or => {
            let kind = if *op == B::And {
                "CONJUNCTION_AND"
            } else {
                "CONJUNCTION_OR"
            };
            let mut children = Vec::new();
            for side in [left, right] {
                match node(side) {
                    Node::Conjunction(k, inner) if k == kind => children.extend(inner),
                    other => children.push(other),
                }
            }
            Node::Conjunction(kind, children)
        }
        B::Eq => compare("COMPARE_EQUAL"),
        B::NotEq => compare("COMPARE_NOTEQUAL"),
        B::Lt => compare("COMPARE_LESSTHAN"),
        B::Gt => compare("COMPARE_GREATERTHAN"),
        B::LtEq => compare("COMPARE_LESSTHANOREQUALTO"),
        B::GtEq => compare("COMPARE_GREATERTHANOREQUALTO"),
        _ => {
            let name = match op {
                B::Plus => "+".to_string(),
                B::Minus => "-".into(),
                B::Multiply => "*".into(),
                B::Divide => "/".into(),
                B::Modulo => "%".into(),
                B::StringConcat => "||".into(),
                B::DuckIntegerDivide | B::MyIntegerDivide => "//".into(),
                B::BitwiseAnd => "&".into(),
                B::BitwiseOr => "|".into(),
                B::BitwiseXor | B::PGExp => "^".into(),
                B::PGBitwiseShiftLeft => "<<".into(),
                B::PGBitwiseShiftRight => ">>".into(),
                B::PGRegexMatch => "regexp_full_match".into(),
                B::PGLikeMatch => "~~".into(),
                B::PGNotLikeMatch => "!~~".into(),
                B::PGILikeMatch => "~~*".into(),
                B::PGNotILikeMatch => "!~~*".into(),
                other => other.to_string().to_ascii_lowercase(),
            };
            operator_function(&name, vec![node(left), node(right)])
        }
    }
}

fn function(f: &ast::Function, e: &ast::Expr) -> Node {
    let (schema, name) = function_name(f);
    let (args, distinct, clauses) = match &f.args {
        // A bare keyword such as `CURRENT_DATE` was a column reference to DuckDB's parser.
        FunctionArguments::None => return Node::Column(object_name_parts(&f.name)),
        FunctionArguments::Subquery(_) => (
            vec![Node::Other {
                class: "SUBQUERY",
                children: Vec::new(),
                text: e.to_string(),
            }],
            false,
            String::new(),
        ),
        FunctionArguments::List(list) => (
            list.args
                .iter()
                .map(|a| match a {
                    FunctionArg::Unnamed(arg)
                    | FunctionArg::Named { arg, .. }
                    | FunctionArg::ExprNamed { arg, .. } => match arg {
                        FunctionArgExpr::Expr(x) => node(x),
                        other => Node::Other {
                            class: "STAR",
                            children: Vec::new(),
                            text: other.to_string(),
                        },
                    },
                })
                .collect::<Vec<_>>(),
            matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct)),
            list.clauses
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(" "),
        ),
    };
    if f.over.is_some() {
        return Node::Other {
            class: "WINDOW",
            children: args,
            text: e.to_string(),
        };
    }
    if schema.is_empty() && matches!(name.as_str(), "coalesce" | "ifnull") {
        return Node::Operator("OPERATOR_COALESCE", args);
    }
    let args = if schema.is_empty() && name == "if" {
        match <[Node; 3]>::try_from(args) {
            Ok([when, then, otherwise]) => return Node::Case(vec![(when, then)], otherwise.into()),
            Err(args) => args,
        }
    } else {
        args
    };
    let within: Vec<Node> = f.within_group.iter().map(|o| node(&o.expr)).collect();
    let (name, args) = match canonical_function_name(f).as_str() {
        "count_star" => ("count_star".to_string(), Vec::new()),
        renamed @ ("quantile_cont" | "quantile_disc") => (renamed.to_string(), within),
        _ if !f.within_group.is_empty() => (name, Vec::new()),
        _ => (name, args),
    };
    Node::Function {
        schema,
        name,
        args,
        distinct,
        extra: format!(
            "{clauses}|{}|{:?}",
            f.filter.as_ref().map(|x| x.to_string()).unwrap_or_default(),
            f.within_group
                .iter()
                .map(|o| o.to_string())
                .collect::<Vec<_>>()
        ),
    }
}

/// A numeric literal as the Postgres grammar DuckDB used reads it: one that fits `i32` is an integer
/// that negates as an integer; anything else stays text and a minus toggles its sign, so
/// `-2147483648` is a BIGINT. Parentheses and repeated minus signs fold away.
enum Num {
    Int(i64),
    Text(String),
}

fn number(e: &ast::Expr) -> Option<Num> {
    match e {
        ast::Expr::Nested(inner) => number(inner),
        ast::Expr::Value(v) => match &v.value {
            Value::Number(s, _) => Some(match s.parse::<i32>() {
                Ok(i) if s.bytes().all(|b| b.is_ascii_digit()) => Num::Int(i.into()),
                _ => Num::Text(s.clone()),
            }),
            _ => None,
        },
        ast::Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => number(expr).map(|n| match n {
            Num::Int(i) => Num::Int(-i),
            Num::Text(t) => Num::Text(match t.strip_prefix('-') {
                Some(positive) => positive.to_string(),
                None => format!("-{t}"),
            }),
        }),
        _ => None,
    }
}

fn literal(n: Num) -> Lit {
    let text = match n {
        Num::Int(i) => return Lit::Int("INTEGER", i.into()),
        Num::Text(t) => t,
    };
    let digits = text.strip_prefix('-').unwrap_or(&text);
    let id = if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(v) = text.parse::<i64>() {
            return Lit::Int("BIGINT", v.into());
        }
        if let Ok(v) = text.parse::<i128>() {
            return Lit::Int("HUGEINT", v);
        }
        if text.parse::<u128>().is_ok() {
            "UHUGEINT"
        } else {
            "DOUBLE"
        }
    } else if digits.contains(['e', 'E'])
        || digits
            .trim_start_matches(['0', '.'])
            .bytes()
            .filter(u8::is_ascii_digit)
            .count()
            > 38
    {
        "DOUBLE"
    } else {
        "DECIMAL"
    };
    Lit::Other(id.into(), text)
}

/// The type id DuckDB's parser resolved a cast target to. Only the integer, VARCHAR and BOOLEAN
/// families are admitted; the rest only name the refusal.
fn type_id(data_type: &ast::DataType) -> String {
    let text = data_type.to_string();
    if text.ends_with("[]") {
        return "LIST".into();
    }
    if text.ends_with(']') {
        return "ARRAY".into();
    }
    let base = text
        .split('(')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_matches('"')
        .to_ascii_uppercase();
    match base.as_str() {
        "INT" | "INTEGER" | "INT4" | "INT32" | "SIGNED" | "INTEGRAL" => "INTEGER",
        "BIGINT" | "INT8" | "INT64" | "LONG" | "OID" => "BIGINT",
        "SMALLINT" | "INT2" | "INT16" | "SHORT" => "SMALLINT",
        "TINYINT" | "INT1" => "TINYINT",
        "HUGEINT" | "INT128" => "HUGEINT",
        "UTINYINT" | "UINT8" => "UTINYINT",
        "USMALLINT" | "UINT16" => "USMALLINT",
        "UINTEGER" | "UINT32" => "UINTEGER",
        "UBIGINT" | "UINT64" => "UBIGINT",
        "UHUGEINT" | "UINT128" => "UHUGEINT",
        "VARCHAR" | "CHAR" | "BPCHAR" | "TEXT" | "STRING" | "NVARCHAR" | "CHARACTER"
        | "CHARACTER VARYING" | "CHAR VARYING" => "VARCHAR",
        "BOOLEAN" | "BOOL" | "LOGICAL" => "BOOLEAN",
        "DOUBLE" | "FLOAT8" | "DOUBLE PRECISION" => "DOUBLE",
        "FLOAT" | "FLOAT4" | "REAL" => "FLOAT",
        "DECIMAL" | "NUMERIC" | "DEC" => "DECIMAL",
        "DATE" => "DATE",
        "TIME" => "TIME",
        "TIMESTAMP" | "DATETIME" | "TIMESTAMP_US" => "TIMESTAMP",
        "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => "TIMESTAMP WITH TIME ZONE",
        "INTERVAL" => "INTERVAL",
        "BLOB" | "BYTEA" | "BINARY" | "VARBINARY" => "BLOB",
        "UUID" | "GUID" => "UUID",
        "BIT" | "BITSTRING" => "BIT",
        "BIGNUM" | "VARINT" => "BIGNUM",
        "STRUCT" | "ROW" => "STRUCT",
        "MAP" => "MAP",
        "UNION" => "UNION",
        _ => "UNBOUND",
    }
    .into()
}

/// A table reference as DuckDB's parser shaped the `FROM` clause: a comma list and a chain of joins
/// are one left-deep tree, and parentheses around a join vanish.
enum Rel {
    Base {
        table: String,
        alias: String,
    },
    Join {
        join_type: &'static str,
        using: bool,
        on: Option<Node>,
        left: Box<Rel>,
        right: Box<Rel>,
    },
    Other,
}

fn join_type(op: &JoinOperator) -> Option<(&'static str, &JoinConstraint)> {
    use JoinOperator as J;
    Some(match op {
        J::Join(c) | J::Inner(c) | J::CrossJoin(c) => ("INNER", c),
        J::Left(c) | J::LeftOuter(c) => ("LEFT", c),
        J::Right(c) | J::RightOuter(c) => ("RIGHT", c),
        J::FullOuter(c) => ("OUTER", c),
        J::Semi(c) | J::LeftSemi(c) => ("SEMI", c),
        J::Anti(c) | J::LeftAnti(c) => ("ANTI", c),
        J::RightSemi(c) => ("RIGHT_SEMI", c),
        J::RightAnti(c) => ("RIGHT_ANTI", c),
        _ => return None,
    })
}

fn relation(factor: &TableFactor) -> Rel {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => Rel::Base {
            table: object_name_parts(name).pop().unwrap_or_default(),
            alias: alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_default(),
        },
        TableFactor::NestedJoin {
            table_with_joins, ..
        } => joined(table_with_joins),
        _ => Rel::Other,
    }
}

fn joined(t: &TableWithJoins) -> Rel {
    t.joins.iter().fold(relation(&t.relation), |left, j| {
        // `parse` has already refused an operator with no DuckDB spelling.
        let (join_type, constraint) =
            join_type(&j.join_operator).unwrap_or(("INNER", &JoinConstraint::None));
        Rel::Join {
            join_type,
            using: matches!(constraint, JoinConstraint::Using(c) if !c.is_empty()),
            on: match constraint {
                JoinConstraint::On(e) => Some(node(e)),
                _ => None,
            },
            left: left.into(),
            right: relation(&j.relation).into(),
        }
    })
}

fn from_clause(from: &[TableWithJoins]) -> Rel {
    let mut tables = from.iter().map(joined);
    let Some(first) = tables.next() else {
        return Rel::Other;
    };
    tables.fold(first, |left, right| Rel::Join {
        join_type: "INNER",
        using: false,
        on: None,
        left: left.into(),
        right: right.into(),
    })
}

/// One select item: the expression, the author's alias, and its text for a refusal to quote.
struct Item {
    node: Node,
    alias: String,
    text: String,
}

/// The parts of the statement the lowering reads, after the statement-level refusals.
struct Shape {
    items: Vec<Item>,
    from: Rel,
    where_clause: Option<Node>,
    /// Deduplicated, as DuckDB's parser left them, with their text.
    group: Vec<(Node, String)>,
}

/// The one `SELECT` a statement list lowers, unwrapping `(SELECT ...)`, with whether a `WITH` was
/// seen on the way in. `None` for a set operation.
pub(crate) fn select_of(query: &ast::Query) -> Option<(Option<&ast::Select>, bool)> {
    let mut with = query
        .with
        .as_ref()
        .is_some_and(|w| !w.cte_tables.is_empty());
    let mut body = query.body.as_ref();
    loop {
        match body {
            SetExpr::Select(select) => return Some((Some(select), with)),
            SetExpr::Query(inner) => {
                with |= inner
                    .with
                    .as_ref()
                    .is_some_and(|w| !w.cte_tables.is_empty());
                body = inner.body.as_ref();
            }
            SetExpr::Values(_) => return Some((None, with)),
            _ => return None,
        }
    }
}

fn shape(statements: &[Statement]) -> Result<Shape> {
    // DuckDB serialised no statement list containing anything but queries.
    let query = match statements {
        [Statement::Query(query), rest @ ..]
            if rest.iter().all(|s| matches!(s, Statement::Query(_))) =>
        {
            query
        }
        _ => bail!("no statement to lower"),
    };
    let Some((select, with)) = select_of(query) else {
        bail!("an entity is one SELECT; keep other SQL as views/*.sql")
    };
    if let Some(select) = select {
        for (clause, what) in [(&select.having, "HAVING"), (&select.qualify, "QUALIFY")] {
            if clause.is_some() {
                bail!("{what} is not incremental v1 SQL; keep this as views/*.sql")
            }
        }
    }
    if with {
        bail!("CTEs are not incremental v1 SQL; keep this as views/*.sql")
    }
    // `VALUES` was a SELECT reading an expression list, which no entity reads.
    let Some(select) = select else {
        return Ok(Shape {
            items: Vec::new(),
            from: Rel::Other,
            where_clause: None,
            group: Vec::new(),
        });
    };

    let items = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(e) => Item {
                node: node(e),
                alias: String::new(),
                text: e.to_string(),
            },
            SelectItem::ExprWithAlias { expr, alias } => Item {
                node: node(expr),
                alias: alias.value.clone(),
                text: expr.to_string(),
            },
            other => Item {
                node: Node::Other {
                    class: "STAR",
                    children: Vec::new(),
                    text: other.to_string(),
                },
                alias: String::new(),
                text: other.to_string(),
            },
        })
        .collect();

    let mut group: Vec<(Node, String)> = Vec::new();
    if let GroupByExpr::Expressions(exprs, _) = &select.group_by {
        for e in exprs {
            let sets = match e {
                ast::Expr::Rollup(sets) | ast::Expr::Cube(sets) | ast::Expr::GroupingSets(sets) => {
                    sets.iter().flatten().collect()
                }
                e => vec![e],
            };
            for e in sets {
                let n = node(e);
                if !group.iter().any(|(g, _)| *g == n) {
                    group.push((n, e.to_string()));
                }
            }
        }
    }

    Ok(Shape {
        items,
        from: from_clause(&select.from),
        where_clause: select.selection.as_ref().map(node),
        group,
    })
}

/// The name each select item carries, or the one it earns.
fn output_columns(items: &[Item]) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let name = if !item.alias.is_empty() {
            item.alias.clone()
        } else {
            match &item.node {
                Node::Column(parts) => parts.last().cloned().unwrap_or_else(|| "column".into()),
                Node::Function { name, args, .. } => {
                    let arg = match args.first() {
                        Some(Node::Column(parts)) => parts.last().map(String::as_str),
                        _ => None,
                    };
                    match (name.as_str(), arg) {
                        ("count_star", _) | ("count", None) => "count".to_string(),
                        (f, Some(col)) => format!("{f}_{col}"),
                        (f, None) => f.to_string(),
                    }
                }
                _ => format!("column_{}", i + 1),
            }
        };
        names.push(name);
    }

    let mut seen = std::collections::BTreeSet::new();
    for n in &names {
        if !seen.insert(n.clone()) {
            bail!(
                "two output columns are both named `{n}`. A view cannot have that, so name them with \
                 `AS` - refused here rather than at the first query against the entity"
            )
        }
    }
    Ok(names)
}

/// Lower an already-parsed statement list. Split out so tests and the validator can share one parse.
pub fn lower_ast(statements: &[Statement]) -> Result<Plan> {
    lower_shape(&shape(statements)?)
}

fn lower_shape(shape: &Shape) -> Result<Plan> {
    let (tables, join_on) = read_from(&shape.from)?;
    let select = &shape.items;

    // Column indices are assigned before anything is lowered, because an expression cannot be
    // lowered until its columns have positions. The order is the order they are first mentioned,
    // walking the statement in a fixed order - deterministic, and stable against an unrelated edit
    // elsewhere in the query.
    let mut columns = Columns::new(&tables);
    if let Some(w) = &shape.where_clause {
        columns.collect(w)?;
    }
    if let Some((l, r)) = join_on {
        columns.collect(l)?;
        columns.collect(r)?;
    }
    for (e, _) in &shape.group {
        columns.collect(e)?;
    }
    for item in select {
        columns.collect(&item.node)?;
    }

    let (key_items, agg_items) = split_select(select)?;
    check_group_matches_key(&key_items, &shape.group)?;

    let joined = Scope::Joined;
    let key = key_items
        .iter()
        .map(|item| columns.expr(&item.node, joined))
        .collect::<Result<Vec<_>>>()?;
    let aggregates = agg_items
        .iter()
        .map(|item| columns.aggregate(&item.node, joined))
        .collect::<Result<Vec<_>>>()?;
    if aggregates.is_empty() {
        bail!(
            "an incremental entity must aggregate something. A SELECT that only projects rows is a \
             view; keep it as views/*.sql"
        )
    }

    let (left_filter, right_filter) = match &shape.where_clause {
        None => (None, None),
        Some(w) => columns.split_where(w)?,
    };

    let left = Source {
        table: tables.left.table.clone(),
        columns: columns.left.clone(),
    };
    let join = match (&tables.right, join_on) {
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

fn base_table(r: &Rel) -> Result<TableRef> {
    let Rel::Base { table, alias } = r else {
        bail!(
            "an entity reads tables directly; subqueries and table functions are not incremental \
             v1 SQL"
        )
    };
    Ok(TableRef {
        name: if alias.is_empty() {
            table.clone()
        } else {
            alias.clone()
        },
        table: table.clone(),
    })
}

type JoinOn<'a> = Option<(&'a Node, &'a Node)>;

fn read_from(from: &Rel) -> Result<(Tables, JoinOn<'_>)> {
    match from {
        Rel::Base { .. } => Ok((
            Tables {
                left: base_table(from)?,
                right: None,
            },
            None,
        )),
        Rel::Join {
            join_type,
            using,
            on,
            left,
            right,
        } => {
            if *join_type != "INNER" {
                bail!("only INNER JOIN is incremental v1 SQL; keep this as views/*.sql")
            }
            if *using {
                bail!("JOIN ... USING is not incremental v1 SQL; write the ON condition out")
            }
            let condition = on
                .as_ref()
                .ok_or_else(|| anyhow!("a join with no ON condition is a cross join"))?;
            let Node::Compare("COMPARE_EQUAL", l, r) = condition else {
                bail!(
                    "only an equijoin on one column from each side is incremental v1 SQL. A \
                     compound or inequality join is not maintainable under retraction here"
                )
            };
            Ok((
                Tables {
                    left: base_table(left)?,
                    right: Some(base_table(right)?),
                },
                Some((l.as_ref(), r.as_ref())),
            ))
        }
        Rel::Other => bail!(
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
    fn resolve(&self, parts: &[String]) -> Result<(Side, String)> {
        match parts {
            [column] => {
                if let Some(right) = &self.tables.right {
                    bail!(
                        "`{column}` is not qualified, and this entity reads two tables. Write \
                         `{}.{column}` or `{}.{column}` so it cannot mean either",
                        self.tables.left.name,
                        right.name
                    )
                }
                Ok((Side::Left, column.clone()))
            }
            [table, column] => {
                if *table == self.tables.left.name {
                    Ok((Side::Left, column.clone()))
                } else if self.tables.right.as_ref().is_some_and(|r| r.name == *table) {
                    Ok((Side::Right, column.clone()))
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
    fn collect(&mut self, e: &Node) -> Result<()> {
        if let Node::Column(parts) = e {
            let (side, column) = self.resolve(parts)?;
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

    fn join_indices(&self, left: &Node, right: &Node) -> Result<(usize, usize)> {
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

    fn column_ref(&self, e: &Node) -> Result<(Side, String)> {
        let Node::Column(parts) = e else {
            bail!("the join condition must compare two columns")
        };
        self.resolve(parts)
    }

    /// Which sides an expression reads. Empty means it reads none - a constant.
    fn sides(&self, e: &Node) -> Result<Vec<Side>> {
        let mut out = Vec::new();
        self.sides_into(e, &mut out)?;
        Ok(out)
    }

    fn sides_into(&self, e: &Node, out: &mut Vec<Side>) -> Result<()> {
        if let Node::Column(parts) = e {
            let (side, _) = self.resolve(parts)?;
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
    fn split_where(&self, where_clause: &Node) -> Result<(Option<Expr>, Option<Expr>)> {
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
    fn expr(&self, e: &Node, scope: Scope) -> Result<Expr> {
        match e {
            Node::Column(parts) => {
                let (side, column) = self.resolve(parts)?;
                Ok(Expr::Column(self.index(side, &column, scope)?))
            }
            Node::Constant(lit) => constant(lit),
            Node::Compare(kind, l, r) => {
                let cmp = match *kind {
                    "COMPARE_EQUAL" => Cmp::Eq,
                    "COMPARE_NOTEQUAL" => Cmp::Ne,
                    "COMPARE_LESSTHAN" => Cmp::Lt,
                    "COMPARE_GREATERTHAN" => Cmp::Gt,
                    "COMPARE_LESSTHANOREQUALTO" => Cmp::Le,
                    "COMPARE_GREATERTHANOREQUALTO" => Cmp::Ge,
                    other => bail!("`{other}` is not a comparison incremental v1 SQL admits"),
                };
                Ok(Expr::Compare(
                    cmp,
                    self.expr(l, scope)?.into(),
                    self.expr(r, scope)?.into(),
                ))
            }
            Node::Conjunction(kind, parts) => {
                let mut parts = parts.iter().map(|c| self.expr(c, scope));
                let first = parts
                    .next()
                    .ok_or_else(|| anyhow!("an AND/OR with nothing in it"))??;
                parts.try_fold(first, |acc, next| {
                    let next = next?;
                    Ok(match *kind {
                        "CONJUNCTION_AND" => Expr::And(acc.into(), next.into()),
                        "CONJUNCTION_OR" => Expr::Or(acc.into(), next.into()),
                        other => bail!("`{other}` is not incremental v1 SQL"),
                    })
                })
            }
            Node::Operator(kind, operands) => match *kind {
                "OPERATOR_NOT" => Ok(Expr::Not(self.expr(only_child(e)?, scope)?.into())),
                "OPERATOR_IS_NULL" => Ok(Expr::IsNull(self.expr(only_child(e)?, scope)?.into())),
                "OPERATOR_IS_NOT_NULL" => Ok(Expr::Not(
                    Expr::IsNull(self.expr(only_child(e)?, scope)?.into()).into(),
                )),
                "OPERATOR_COALESCE" => Ok(Expr::Coalesce(
                    operands
                        .iter()
                        .map(|c| self.expr(c, scope))
                        .collect::<Result<Vec<_>>>()?,
                )),
                other => bail!("`{other}` is not incremental v1 SQL"),
            },
            Node::Case(whens, otherwise) => {
                let whens = whens
                    .iter()
                    .map(|(when, then)| Ok((self.expr(when, scope)?, self.expr(then, scope)?)))
                    .collect::<Result<Vec<_>>>()?;
                // DuckDB's parser gave every CASE an ELSE, NULL when none was written.
                let otherwise = Some(Box::new(self.expr(otherwise, scope)?));
                Ok(Expr::Case { whens, otherwise })
            }
            Node::Cast { to, child, .. } => {
                let ty = match to.as_str() {
                    "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "HUGEINT" | "UTINYINT"
                    | "USMALLINT" | "UINTEGER" | "UBIGINT" => Type::Int,
                    "VARCHAR" => Type::Str,
                    "BOOLEAN" => Type::Bool,
                    other => bail!(
                        "a cast to {other} is not incremental v1 SQL. §3.3 admits exact integers, \
                         strings and booleans; floating point is refused deliberately"
                    ),
                };
                let inner = self.expr(child, scope)?;
                // **`true` is a cast, in this parser.** DuckDB serialised the boolean literals as
                // `CAST('t' AS BOOLEAN)` and `CAST('f' AS BOOLEAN)`, and [`node`] keeps that shape,
                // so `WHERE i.active = true` arrives here as a VARCHAR-to-BOOLEAN cast, which the
                // evaluator refuses at the row. Found on the captured Horizon corpus (#835).
                //
                // Folded at load rather than admitted at runtime: the refusal is still right for a
                // VARCHAR *column*, and folding a literal costs nothing per row.
                if let (Expr::Literal(Scalar::Str(text)), Type::Bool) = (&inner, ty) {
                    return match text.as_str() {
                        "t" | "true" => Ok(Expr::Literal(Scalar::Bool(true))),
                        "f" | "false" => Ok(Expr::Literal(Scalar::Bool(false))),
                        other => bail!("`{other}` is not a boolean literal"),
                    };
                }
                Ok(Expr::Cast(inner.into(), ty))
            }
            Node::Function { name, args, .. } => {
                let [l, r] = args.as_slice() else {
                    bail!("this operator takes two operands in incremental v1 SQL")
                };
                match name.as_str() {
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
            Node::Other { class, .. } => {
                bail!("`{class}` is not an expression incremental v1 SQL admits")
            }
        }
    }

    /// Lower one aggregate from the select list.
    fn aggregate(&self, e: &Node, scope: Scope) -> Result<Agg> {
        let Node::Function { name, args, .. } = e else {
            bail!("an aggregate that is not a call; this is a lowering bug")
        };
        if name == "count_star" {
            return Ok(Agg::Count);
        }
        let arg = match args.as_slice() {
            [one] => one,
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
fn split_select(select: &[Item]) -> Result<(Vec<&Item>, Vec<&Item>)> {
    let first_agg = select
        .iter()
        .position(|i| is_aggregate(&i.node))
        .unwrap_or(select.len());
    let (key, aggs) = select.split_at(first_agg);
    if let Some(stray) = aggs.iter().position(|i| !is_aggregate(&i.node)) {
        bail!(
            "select item {} is not an aggregate but follows one. An incremental entity selects its \
             grouping expressions first, then its aggregates",
            first_agg + stray + 1
        )
    }
    Ok((key.iter().collect(), aggs.iter().collect()))
}

fn is_aggregate(e: &Node) -> bool {
    matches!(e, Node::Function { name, .. }
        if matches!(name.as_str(), "sum" | "min" | "max" | "avg" | "count" | "count_star"))
}

/// The grouping expressions and the leading select items must be the same set.
///
/// Not the same *sequence*: `SELECT b, a, count(*) ... GROUP BY a, b` is perfectly ordinary SQL and
/// the entity's key is what the select list says, because that is the order the author will read the
/// answer in.
fn check_group_matches_key(key: &[&Item], group: &[(Node, String)]) -> Result<()> {
    let mut want: Vec<(String, &str)> = group
        .iter()
        .map(|(n, t)| (format!("{n:?}"), t.as_str()))
        .collect();
    let mut got: Vec<(String, &str)> = key
        .iter()
        .map(|i| (format!("{:?}", i.node), i.text.as_str()))
        .collect();
    want.sort();
    got.sort();
    let same = want.len() == got.len() && want.iter().zip(&got).all(|(a, b)| a.0 == b.0);
    if !same {
        let texts = |v: &[(String, &str)]| v.iter().map(|(_, t)| *t).collect::<Vec<_>>().join(", ");
        bail!(
            "the grouping expressions and the non-aggregate select items must be the same set. \
             GROUP BY has [{}]; the select list has [{}]",
            texts(&want),
            texts(&got)
        )
    }
    Ok(())
}

/// Flatten an `AND` chain into its conjuncts, so each can be classified by the side it reads.
fn conjuncts(e: &Node) -> Vec<&Node> {
    if let Node::Conjunction("CONJUNCTION_AND", parts) = e {
        return parts.iter().flat_map(conjuncts).collect();
    }
    vec![e]
}

/// Every sub-expression a walk reaches, in the order DuckDB's tree presented them.
fn children(e: &Node) -> Vec<&Node> {
    match e {
        Node::Column(_) | Node::Constant(_) => Vec::new(),
        Node::Compare(_, l, r) => vec![l, r],
        Node::Conjunction(_, c) | Node::Operator(_, c) => c.iter().collect(),
        Node::Function { args, .. } => args.iter().collect(),
        Node::Other { children, .. } => children.iter().collect(),
        Node::Case(whens, otherwise) => whens
            .iter()
            .flat_map(|(w, t)| [w, t])
            .chain(std::iter::once(otherwise.as_ref()))
            .collect(),
        Node::Cast { child, .. } => vec![child],
    }
}

fn only_child(e: &Node) -> Result<&Node> {
    match children(e).as_slice() {
        [one] => Ok(one),
        _ => bail!("this operator takes one operand"),
    }
}

/// A literal, exactly. §3.3 admits integers, strings and booleans and refuses floating point, so a
/// `DOUBLE` literal is a refusal rather than a rounded integer.
fn constant(lit: &Lit) -> Result<Expr> {
    Ok(Expr::Literal(match lit {
        Lit::Null => Scalar::Null,
        Lit::Int("HUGEINT", v) => bail!(
            "{{\"lower\":{},\"upper\":{}}} does not fit an exact integer",
            *v as u64,
            (*v >> 64) as i64
        ),
        Lit::Int(_, v) => Scalar::Int(*v),
        Lit::Str(s) => Scalar::Str(s.clone()),
        Lit::Other(id, _) => bail!(
            "a {id} literal is not incremental v1 SQL. §3.3 admits exact integers, strings and \
             booleans, and refuses floating point so an entity cannot drift"
        ),
    }))
}

/// The DuckDB-JSON lowering this module replaced, moved here verbatim as the differential oracle.
#[cfg(test)]
#[allow(dead_code)]
pub(crate) mod duck_oracle {
    use crate::entity_expr::{Cmp, Expr, Type};
    use crate::entity_plan::{Agg, Join, Plan, Source};
    use crate::entity_row::Scalar;
    use anyhow::{anyhow, bail, Context, Result};
    use duckdb::Connection;
    use serde_json::Value;

    /// Parse and lower one authored entity `SELECT`, discarding its output column names.
    ///
    /// Most callers want only the relational shape: a circuit and a batch evaluator index by position and
    /// have no use for a name. [`lower_with_columns`] is for the serving surface, which does.
    pub fn lower(sql: &str) -> Result<Plan> {
        lower_with_columns(sql).map(|(plan, _)| plan)
    }

    /// Lower, and keep the names the author gave the output columns (#822).
    ///
    /// **`Plan` deliberately does not carry these.** It is positional because the circuit and the oracle
    /// are positional, and 57 hand-built `Plan` literals across the tree would have had to gain a field
    /// they never read. The names belong to the *served* entity, which is the only thing that needs them.
    ///
    /// Each output column takes the author's `AS` alias when there is one. Without an alias: a bare column
    /// reference keeps its own name, and an aggregate is named after what it does to which column -
    /// `count`, `sum_amount`, `avg_tokens`. **Duplicates are refused at load**, because a SQL view with
    /// two columns of one name is not a thing, and finding that out at first query rather than at nest
    /// start is the failure mode this whole slice keeps removing.
    pub fn lower_with_columns(sql: &str) -> Result<(Plan, Vec<String>)> {
        let conn = Connection::open_in_memory()?;
        let literal = format!("'{}'", sql.replace('\'', "''"));
        let raw: String = conn
            .query_row(&format!("SELECT json_serialize_sql({literal})"), [], |r| {
                r.get(0)
            })
            .context("parsing the entity SQL")?;
        let ast: Value = serde_json::from_str(&raw)?;
        let plan = lower_ast(&ast)?;
        let columns = output_columns(&ast)?;
        // A consistency check between two traversals of one select list, not a validation of input:
        // `output_columns` names every item and `split_select` divides the same items into key and
        // aggregates, so this cannot fire today and **no test reaches it**. It is here because the two
        // walks are independent and an edit to either could desync them, at which point every column's
        // name would shift by one - silently, and only in the serving surface.
        if columns.len() != plan.key.len() + plan.aggregates.len() {
            bail!(
                "lowered {} key + {} aggregate columns but named {} of them; this is a lowering bug",
                plan.key.len(),
                plan.aggregates.len(),
                columns.len()
            )
        }
        Ok((plan, columns))
    }

    /// The name each select item carries, or the one it earns.
    fn output_columns(ast: &Value) -> Result<Vec<String>> {
        let select = ast
            .pointer("/statements/0/node/select_list")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("an entity must select something"))?;

        let mut names = Vec::with_capacity(select.len());
        for (i, item) in select.iter().enumerate() {
            let alias = item
                .get("alias")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = if !alias.is_empty() {
                alias.to_string()
            } else if item.get("class").and_then(Value::as_str) == Some("COLUMN_REF") {
                item.get("column_names")
                    .and_then(Value::as_array)
                    .and_then(|p| p.last())
                    .and_then(Value::as_str)
                    .unwrap_or("column")
                    .to_string()
            } else if item.get("class").and_then(Value::as_str) == Some("FUNCTION") {
                let f = item
                    .get("function_name")
                    .and_then(Value::as_str)
                    .unwrap_or("agg")
                    .to_ascii_lowercase();
                // `count(*)` has no argument to name after, and DuckDB calls it `count_star`.
                let arg = children(item)
                    .next()
                    .filter(|c| c.get("class").and_then(Value::as_str) == Some("COLUMN_REF"))
                    .and_then(|c| c.get("column_names").and_then(Value::as_array))
                    .and_then(|p| p.last())
                    .and_then(Value::as_str);
                match (f.as_str(), arg) {
                    ("count_star", _) | ("count", None) => "count".to_string(),
                    (f, Some(col)) => format!("{f}_{col}"),
                    (f, None) => f.to_string(),
                }
            } else {
                format!("column_{}", i + 1)
            };
            names.push(name);
        }

        let mut seen = std::collections::BTreeSet::new();
        for n in &names {
            if !seen.insert(n.clone()) {
                bail!(
                    "two output columns are both named `{n}`. A view cannot have that, so name them with \
                     `AS` - refused here rather than at the first query against the entity"
                )
            }
        }
        Ok(names)
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
            let within = list.iter().position(|c| c == column).ok_or_else(|| {
                anyhow!("`{column}` was never registered; this is a lowering bug")
            })?;
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
                    "OPERATOR_IS_NULL" => {
                        Ok(Expr::IsNull(self.expr(only_child(e)?, scope)?.into()))
                    }
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
                    let inner = self.expr(child, scope)?;
                    // **`true` is a cast, in this parser.** DuckDB serialises the boolean literals as
                    // `CAST('t' AS BOOLEAN)` and `CAST('f' AS BOOLEAN)`, so `WHERE i.active = true` -
                    // about as ordinary as SQL gets - arrives here as a VARCHAR-to-BOOLEAN cast, which
                    // the evaluator refuses at the row. Found on the captured Horizon corpus (#835); no
                    // test in this module had a boolean literal in it.
                    //
                    // Folded at load rather than admitted at runtime: the refusal is still right for a
                    // VARCHAR *column*, and folding a literal costs nothing per row.
                    if let (Expr::Literal(Scalar::Str(text)), Type::Bool) = (&inner, ty) {
                        return match text.as_str() {
                            "t" | "true" => Ok(Expr::Literal(Scalar::Bool(true))),
                            "f" | "false" => Ok(Expr::Literal(Scalar::Bool(false))),
                            other => bail!("`{other}` is not a boolean literal"),
                        };
                    }
                    Ok(Expr::Cast(inner.into(), ty))
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

    /// `WHERE flag = true` is ordinary SQL and did not lower until #835's corpus met it: DuckDB
    /// serialises the boolean literals as `CAST('t' AS BOOLEAN)`, and a VARCHAR-to-BOOLEAN cast is
    /// refused at evaluation. Folded at load instead, so the refusal still stands for a real column.
    #[test]
    fn a_boolean_literal_folds_rather_than_arriving_as_a_varchar_cast() {
        for (sql, want) in [
            (
                "SELECT t.a, count(*) FROM t WHERE t.flag = true GROUP BY t.a",
                true,
            ),
            (
                "SELECT t.a, count(*) FROM t WHERE t.flag = false GROUP BY t.a",
                false,
            ),
        ] {
            let filter = lower(sql).unwrap().left_filter.expect("the WHERE lowered");
            assert_eq!(
                filter,
                Expr::Compare(
                    Cmp::Eq,
                    Expr::Column(0).into(),
                    Expr::Literal(Scalar::Bool(want)).into()
                ),
                "{sql}"
            );
        }
    }

    /// The fold is for the parser's own spelling of a boolean, not a licence to cast text generally:
    /// a cast of a *column* must stay a cast, so the evaluator still refuses it at the row.
    #[test]
    fn casting_a_column_to_boolean_stays_a_cast() {
        let filter = lower("SELECT t.a, count(*) FROM t WHERE CAST(t.b AS BOOLEAN) GROUP BY t.a")
            .unwrap()
            .left_filter
            .unwrap();
        assert!(
            matches!(filter, Expr::Cast(_, Type::Bool)),
            "a column cast must stay a cast: {filter:?}"
        );
    }

    /// #822: the serving surface needs column names, and the author already wrote them. The old
    /// lowering threw every `AS` away, so `SELECT * FROM <entity>` had no honest answer.
    #[test]
    fn output_columns_take_the_authors_aliases() {
        let (_, cols) = lower_with_columns(
            "SELECT d.indexer AS who, COUNT(*) AS n, SUM(d.amount) AS total \
             FROM delegations d GROUP BY d.indexer",
        )
        .unwrap();
        assert_eq!(cols, vec!["who", "n", "total"]);
    }

    /// Without an alias a column keeps its own name, and an aggregate is named after what it does to
    /// which column. `agg0, agg1` would have worked and would have been the wrong kind of decision -
    /// the sort that becomes the public shape of `/sql` because nobody chose it.
    #[test]
    fn an_unaliased_column_earns_a_name_that_says_what_it_is() {
        let (_, cols) = lower_with_columns(
            "SELECT d.indexer, COUNT(*), SUM(d.amount), MAX(d.amount) \
             FROM delegations d GROUP BY d.indexer",
        )
        .unwrap();
        assert_eq!(cols, vec!["indexer", "count", "sum_amount", "max_amount"]);
    }

    /// A view cannot have two columns of one name. Refused at load, not at the first query against
    /// the entity - which is the failure this slice keeps removing.
    #[test]
    fn two_columns_of_one_name_are_refused_at_load() {
        let err = format!(
            "{:#}",
            lower_with_columns(
                "SELECT d.indexer AS x, COUNT(*) AS x FROM delegations d GROUP BY d.indexer"
            )
            .expect_err("a view cannot have two columns named x")
        );
        assert!(err.contains("both named `x`"), "{err}");
        assert!(err.contains("AS"), "the refusal must say what to do: {err}");
    }

    /// One name per output column, always - the count is what the serving surface zips against, and a
    /// mismatch would silently shift every column's name by one.
    #[test]
    fn there_is_exactly_one_name_per_output_column() {
        let (plan, cols) = lower_with_columns(
            "SELECT d.a, d.b, COUNT(*), SUM(d.v) FROM delegations d GROUP BY d.a, d.b",
        )
        .unwrap();
        assert_eq!(cols.len(), plan.key.len() + plan.aggregates.len());
        assert_eq!(cols, vec!["a", "b", "count", "sum_v"]);
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

    /// The port against the DuckDB-JSON lowering it replaced, on every entity SQL string the tree
    /// uses and on probes at each seam between the two parsers.
    ///
    /// Identical Plans, identical output names, identical refusals. Two refusal families may differ
    /// only after a fixed prefix: a parse failure now says why, and the GROUP BY mismatch quotes the
    /// SQL rather than DuckDB's serialised tree. [`corpus::PARSER_GAPS`] is everything else, each one a
    /// refusal where DuckDB parsed the statement.
    #[test]
    fn the_port_lowers_every_statement_exactly_as_duckdbs_parse_did() {
        let mut diffs = Vec::new();
        for sql in corpus::USED.iter().chain(corpus::PROBES) {
            let old = duck_oracle::lower_with_columns(sql).map_err(|e| format!("{e:#}"));
            let new = lower_with_columns(sql).map_err(|e| format!("{e:#}"));
            if corpus::PARSER_GAPS.contains(sql) {
                assert!(
                    new.is_err() && old != new,
                    "listed as a parser gap but the port agrees or admits it: {sql:?}\n  duckdb: {old:?}\n  port:   {new:?}"
                );
                continue;
            }
            if !agree(&old, &new) {
                diffs.push(format!("{sql:?}\n  duckdb: {old:?}\n  port:   {new:?}"));
            }
        }
        assert!(
            diffs.is_empty(),
            "{} differ:\n{}",
            diffs.len(),
            diffs.join("\n")
        );
    }

    pub(crate) fn agree(
        old: &std::result::Result<(Plan, Vec<String>), String>,
        new: &std::result::Result<(Plan, Vec<String>), String>,
    ) -> bool {
        match (old, new) {
            (Err(o), Err(n)) => {
                o == n
                    || (o == "no statement to lower" && n.starts_with("no statement to lower ("))
                    || same_prefix(o, n, "GROUP BY has [")
            }
            _ => old == new,
        }
    }

    pub(crate) fn same_prefix(old: &str, new: &str, marker: &str) -> bool {
        matches!((old.find(marker), new.find(marker)), (Some(a), Some(b)) if old[..a] == new[..b])
    }
}

/// Every entity SQL string the tree's tests lower or validate, plus probes at the seams between
/// DuckDB's parser and sqlparser. Shared by the differential tests here and in `entities`.
#[cfg(test)]
pub(crate) mod corpus {
    /// From the tests of `entity_lower`, `entities`, `runtime`, `check`, `indexer`, the spike, and
    /// `tests/` (lodestar_panel, seed_scale's documented example, e2e_early_cutoff, e2e_entity_reorg).
    pub(crate) const USED: &[&str] = &[
        "SELECT d.indexer, d.delegator, SUM(d.amount) FROM delegations d JOIN indexers i ON d.indexer = i.indexer WHERE d.amount > 0 AND i.active GROUP BY d.indexer, d.delegator",
        "SELECT owner, count(*) FROM transfers GROUP BY owner",
        "SELECT b, a, count(*) FROM t GROUP BY a, b",
        "SELECT l.k, count(*) FROM l JOIN r ON l.k = r.k WHERE l.amount > r.floor GROUP BY l.k",
        "SELECT l.k, count(*) FROM l JOIN r ON l.k = r.k WHERE amount > 0 GROUP BY l.k",
        "SELECT d.k, count(*) FROM d JOIN i ON d.k = i.k GROUP BY d.k",
        "SELECT d.k, count(*) FROM d JOIN i ON i.k = d.k GROUP BY d.k",
        "SELECT d.owner, SUM(d.amt) FROM d JOIN i ON i.name = d.owner WHERE d.amt > 0 AND i.active GROUP BY d.owner",
        "SELECT r.region, SUM(l.amount) FROM l JOIN r ON l.k = r.k GROUP BY r.region",
        "SELECT a, count(*) FROM t GROUP BY b",
        "SELECT l.k, count(*) FROM l LEFT JOIN r ON l.k = r.k GROUP BY l.k",
        "SELECT a, b FROM t GROUP BY a, b",
        "SELECT a, count(b) FROM t GROUP BY a",
        "SELECT a, SUM(b) FROM t WHERE b > 1.5 GROUP BY a",
        "SELECT a, SUM(b) FROM t WHERE b > 1e0 GROUP BY a",
        "SELECT count(*), a FROM t GROUP BY a",
        "SELECT t.a, SUM(t.b * 2 + t.c - 1) FROM t WHERE t.d IS NOT NULL AND COALESCE(t.e, 0) > 0 AND CASE WHEN t.f THEN t.g ELSE 0 END > 1 GROUP BY t.a",
        "SELECT t.a, count(*) FROM t WHERE t.flag = true GROUP BY t.a",
        "SELECT t.a, count(*) FROM t WHERE t.flag = false GROUP BY t.a",
        "SELECT t.a, count(*) FROM t WHERE CAST(t.b AS BOOLEAN) GROUP BY t.a",
        "SELECT d.indexer AS who, COUNT(*) AS n, SUM(d.amount) AS total FROM delegations d GROUP BY d.indexer",
        "SELECT d.indexer, COUNT(*), SUM(d.amount), MAX(d.amount) FROM delegations d GROUP BY d.indexer",
        "SELECT d.indexer AS x, COUNT(*) AS x FROM delegations d GROUP BY d.indexer",
        "SELECT d.a, d.b, COUNT(*), SUM(d.v) FROM delegations d GROUP BY d.a, d.b",
        "SELECT a, SUM(CAST(b AS DOUBLE)) FROM t GROUP BY a",
        "WITH x AS (SELECT 1 AS a) SELECT a, count(*) FROM x GROUP BY a",
        // entities
        "SELECT 1 AS owner",
        "SELECT 1 AS owner; SELECT 2",
        "SELECT DISTINCT 1 AS owner",
        "SELECT random() AS owner",
        "SELECT current_date AS owner",
        "SELECT l.owner FROM lefts AS l LEFT OUTER JOIN rights AS r ON l.owner = r.owner",
        "SELECT count(DISTINCT owner) AS owner FROM facts",
        "SELECT 'ORDER BY' AS owner -- LIMIT is prose here\n",
        "SELECT 1 AS k, median(v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, \"median\"(v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, quantile_cont(v, 0.5) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, quantile_disc(v, 0.5) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, approx_quantile(v, 0.5) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, arg_max(v, v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, first(v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, string_agg(v::VARCHAR, ',') AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, list(v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, histogram(v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT 1 AS k, any_value(v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT v AS k FROM (VALUES (1),(2)) t(v) WHERE v IN (SELECT 1)",
        "SELECT v AS k, (SELECT max(w) FROM (VALUES (9)) u(w)) AS m FROM (VALUES (1)) t(v)",
        "SELECT a AS k, sum(b) AS m FROM (VALUES (1,2)) t(a,b) GROUP BY GROUPING SETS ((a),(b))",
        "SELECT a AS k, sum(b) AS m FROM (VALUES (1,2)) t(a,b) GROUP BY ROLLUP (a,b)",
        "SELECT a AS k, sum(b) AS m FROM (VALUES (1,2)) t(a,b) GROUP BY CUBE (a,b)",
        "SELECT v AS k FROM (VALUES (1),(2)) t(v) USING SAMPLE 1",
        "SELECT 1 AS k, count(DISTINCT v) AS m FROM (VALUES (1),(2)) t(v)",
        "SELECT k, percentile_cont(0.5) WITHIN GROUP (ORDER BY v) AS m FROM (VALUES (1,2)) t(k,v) GROUP BY k",
        "SELECT k, sum(v) AS s FROM (VALUES (1,2)) t(k,v) GROUP BY k",
        "SELECT k, count(*) AS n FROM (VALUES (1,2)) t(k,v) GROUP BY k",
        "SELECT k, min(v) AS a, max(v) AS b, avg(v) AS c FROM (VALUES (1,2)) t(k,v) GROUP BY k",
        "SELECT lower(s) AS k FROM (VALUES ('A')) t(s)",
        "SELECT 1 AS x",
        "SELECT owner FROM (VALUES (1), (1)) AS source(owner)",
        "SELECT owner FROM (VALUES (1), (2)) AS source(owner)",
        "SELECT a.x FROM facts a JOIN earlier e ON a.x = e.x",
        "SELECT x FROM b",
        "SELECT x FROM a",
        "SELECT owner FROM facts",
        "SELECT other FROM facts",
        "SELECT symbol, count(*) AS n FROM offchain__prices GROUP BY symbol",
        "SELECT symbol FROM offchain__prices",
        // runtime, check, indexer
        "SELECT t.tier, sum(x.value) AS v FROM usdc__transfer x JOIN offchain__tiers t ON x.\"to\" = t.account GROUP BY t.tier",
        "SELECT \"to\", sum(value) AS v FROM usdc__transfer GROUP BY \"to\"",
        "SELECT 1 AS id",
        "SELECT \"to\", SUM(value) AS total FROM usdc__transfer GROUP BY \"to\"",
        "SELECT t.tier, sum(x.value) AS v FROM usdc__transfer x JOIN offchain__tiers t ON x.\"to\" = t.wallet GROUP BY t.tier",
        "SELECT a.k, sum(b.v) AS v FROM offchain__a a JOIN offchain__b b ON a.k = b.k GROUP BY a.k",
        // authored_entity_spike, including DELEGATION_SQL and its variants
        "SELECT d.indexer, COUNT(*), SUM(d.amount) FROM delegations d JOIN indexers i ON d.indexer = i.indexer WHERE d.amount > 0 AND i.active = true GROUP BY d.indexer",
        "SELECT d.indexer, d.delegator, SUM(d.amount + 0) AS delegated FROM delegations d JOIN indexers i ON d.indexer = i.indexer WHERE d.amount > 0 AND i.active = true GROUP BY d.indexer, d.delegator",
        "SELECT d.indexer, d.delegator, SUM(d.amount + 1) AS delegated FROM delegations d JOIN indexers i ON d.indexer = i.indexer WHERE d.amount > 0 AND i.active = true GROUP BY d.indexer, d.delegator",
        "SELECT d.indexer, d.delegator, SUM(d.amount + 0) AS delegated FROM delegations d JOIN indexers i ON d.delegator = i.indexer WHERE d.amount > 0 AND i.active = true GROUP BY d.indexer, d.delegator",
        "SELECT d.indexer, d.delegator, SUM(d.amount + 0) AS delegated FROM delegations d JOIN indexers i ON d.indexer = i.indexer WHERE d.amount > 0 AND i.active = false GROUP BY d.indexer, d.delegator",
        "SELECT d.indexer, d.delegator, SUM(d.amount + 0) AS delegated\nFROM delegations d JOIN indexers i ON d.indexer = i.indexer\nWHERE d.amount > 0 AND i.active = true\nGROUP BY d.indexer, d.delegator",
        "SELECT indexer, count(*) FROM delegations GROUP BY indexer",
        // tests/
        "SELECT indexer, SUM(tokensRewards) FROM service__indexing_rewards_collected GROUP BY indexer",
        "SELECT delegator, SUM(tokens) AS total, COUNT(*) AS n\n                   FROM staking_legacy__stake_delegated GROUP BY delegator",
        "SELECT t.to, SUM(t.value) FROM alpha__transfer t GROUP BY t.to",
        "SELECT t.to, SUM(CAST(t.value AS HUGEINT)) AS sum_value FROM usdc__transfer t GROUP BY t.to",
        "SELECT t.tier, SUM(x.value) FROM usdc__transfer x JOIN offchain__tiers t ON x.to = t.account GROUP BY t.tier",
        "SELECT t.to, SUM(t.value) FROM usdc__transfer t GROUP BY t.to",
        "SELECT t.to, SUM(t.value) FROM doomed__transfer t GROUP BY t.to",
        "SELECT t.block_number, SUM(t.value) FROM usdc__transfer t GROUP BY t.block_number",
        "SELECT t.from, COUNT(*) FROM usdc__transfer t GROUP BY t.from",
        "SELECT t.to, COUNT(*) FROM usdc__transfer t GROUP BY t.to",
        "SELECT p.symbol, SUM(x.value * p.price_e8) AS usd_e8 FROM usdc__transfer x JOIN offchain__prices p ON x.address = p.token GROUP BY p.symbol",
        "SELECT delegator, count(*) AS n FROM events GROUP BY delegator",
        "SELECT id FROM factory__pool_created",
        // port_emit's entities/pool.sql for the ACCUM fixture, and its two-arm shape
        "-- Running totals of `Pool`, maintained incrementally (RFC-0041).\n-- A subgraph accumulates these one event at a time; the decoded table holds the\n             -- deltas, so the total is their sum and never the latest value. The exact fields that\n             -- *are* latest-value live in views/, and README.md says which field went where.\n-- `Pool.totalFees` sums `fee` of `factory__pool_created` keyed by \"pool\": src/mappings/core.ts:5\nSELECT\n  \"id\",\n  sum(\"totalFees\") AS \"totalFees\",\n  max(\"totalFees_overflow\") AS \"totalFees_overflow\"\nFROM (\n  SELECT\n    \"pool\" AS \"id\",\n    TRY_CAST(\"fee\" AS DECIMAL(38,0)) AS \"totalFees\",\n    CASE WHEN \"fee\" IS NOT NULL AND TRY_CAST(\"fee\" AS DECIMAL(38,0)) IS NULL THEN 1 ELSE 0 END AS \"totalFees_overflow\"\n  FROM \"factory__pool_created\"\n)\nGROUP BY \"id\"\n",
        "SELECT\n  \"id\",\n  sum(\"swapVolume\") AS \"swapVolume\",\n  max(\"swapVolume_overflow\") AS \"swapVolume_overflow\"\nFROM (\n  SELECT \"pool\" AS \"id\", CAST(0 AS DECIMAL(38,0)) AS \"swapVolume\", 0 AS \"swapVolume_overflow\" FROM \"factory__pool_created\"\n  UNION ALL\n  SELECT \"address\" AS \"id\", TRY_CAST(\"amount1\" AS DECIMAL(38,0)) AS \"swapVolume\", CASE WHEN \"amount1\" IS NOT NULL AND TRY_CAST(\"amount1\" AS DECIMAL(38,0)) IS NULL THEN 1 ELSE 0 END AS \"swapVolume_overflow\" FROM \"pool__swap\"\n)\nGROUP BY \"id\"\n",
    ];

    /// Where the two parsers could plausibly part company: literals, flattening, desugaring, joins,
    /// statement shapes, and every refusal class.
    /// Statements DuckDB's parser read and sqlparser's DuckDB dialect reads differently or not at
    /// all. Each is a refusal now, whatever DuckDB made of it.
    pub(crate) const PARSER_GAPS: &[&str] = &[
        // sqlparser reads `1_000` as `1 AS _000`, `E'x'` as `E AS 'x'`
        "SELECT a, count(*) FROM t WHERE b = 1_000 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = E'x' GROUP BY a",
        // no postfix ISNULL, no `**`, no `#1`, no GLOB, no TABLE statement
        "SELECT a, count(*) FROM t WHERE b ISNULL GROUP BY a",
        "SELECT a, SUM(b ** 2) FROM t GROUP BY a",
        "SELECT a, count(*) FROM t GROUP BY #1",
        "SELECT a, count(*) FROM t WHERE b GLOB 'x' GROUP BY a",
        "TABLE t",
        // DuckDB's ASOF JOIN, which the old lowering took for an INNER equijoin
        "SELECT a.k, count(*) FROM a ASOF JOIN b ON a.k = b.k GROUP BY a.k",
    ];

    pub(crate) const PROBES: &[&str] = &[
        "SELECT a, count(*) FROM t WHERE a = -1 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -2147483648 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 2147483648 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = - -1 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -(1) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -(-(2147483648)) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 9223372036854775807 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -9223372036854775808 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 9223372036854775808 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -9223372036854775809 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 170141183460469231731687303715884105728 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 340282366920938463463374607431768211456 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -170141183460469231731687303715884105729 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 0005 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = .5 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 5. GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -1.5 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 1e3 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -0 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = +1 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = -a GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = 'it''s' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = NULL GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = x'ab' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = X'AB' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a AND (b AND c) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a OR (b OR c) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE (a OR b) AND c GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a AND b OR c AND d GROUP BY a",
        "SELECT a, count(*) FROM t WHERE ((a)) GROUP BY a",
        "SELECT a, SUM(CASE b WHEN 1 THEN 2 WHEN 3 THEN 4 ELSE 5 END) FROM t GROUP BY a",
        "SELECT a, SUM(CASE WHEN b THEN 1 END) FROM t GROUP BY a",
        "SELECT a, SUM(IF(b, 1, 0)) FROM t GROUP BY a",
        "SELECT a, SUM(IFNULL(b, 0)) FROM t GROUP BY a",
        "SELECT a, SUM(COALESCE(b, c, 0)) FROM t GROUP BY a",
        "SELECT a, count() FROM t GROUP BY a",
        "SELECT a, COUNT(*) FROM t GROUP BY a, a",
        "SELECT a, count(*) FROM t GROUP BY (a)",
        "SELECT (a), count(*) FROM t GROUP BY a",
        "SELECT A, count(*) FROM t GROUP BY a",
        "SELECT \"a\", count(*) FROM t GROUP BY a",
        "SELECT a, a, count(*) FROM t GROUP BY a",
        "SELECT a, count(*) FROM t GROUP BY ALL",
        "SELECT count(*) FROM t GROUP BY ALL",
        "SELECT count(*) FROM t",
        "SELECT count(*), sum(b) FROM t WHERE b > 0",
        "SELECT a, count(*) FROM t GROUP BY 1",
        "SELECT a, b, count(*) FROM t GROUP BY ROLLUP (a, b)",
        "SELECT a, count(*) FROM t GROUP BY GROUPING SETS ((a))",
        "SELECT a, b, count(*) FROM t GROUP BY GROUPING SETS ((a), (a, b))",
        "SELECT a, count(*) FROM t GROUP BY a HAVING count(*) > 1",
        "SELECT a, count(*) FROM t GROUP BY a QUALIFY a > 1",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY a LIMIT 3",
        "SELECT DISTINCT a, count(*) FROM t GROUP BY a",
        "SELECT a, SUM(b) FILTER (WHERE b > 0) FROM t GROUP BY a",
        "SELECT a, count(*) FILTER (WHERE c > 0) FROM t GROUP BY a",
        "SELECT a, SUM(DISTINCT b) FROM t GROUP BY a",
        "SELECT a, sum(b ORDER BY c) FROM t GROUP BY a",
        "SELECT a, sum(b) OVER () FROM t GROUP BY a",
        "SELECT a, sum(b) OVER (PARTITION BY c) AS s, count(*) FROM t GROUP BY a",
        "SELECT a, main.sum(b) FROM t GROUP BY a",
        "SELECT a, \"SUM\"(b) FROM t GROUP BY a",
        "SELECT a, sum(b, c) FROM t GROUP BY a",
        "SELECT a, sum() FROM t GROUP BY a",
        "SELECT a, median(b) FROM t GROUP BY a",
        "SELECT a, SUM(b) + 1 FROM t GROUP BY a",
        "SELECT a, avg(b), min(b), max(b) FROM t GROUP BY a",
        "SELECT a + 1, count(*) FROM t GROUP BY a + 1",
        "SELECT a + 1, count(*) FROM t GROUP BY (a + 1)",
        "SELECT 1 + a, count(*) FROM t GROUP BY 1 + a",
        "SELECT a * (b + c), count(*) FROM t GROUP BY a * (b + c)",
        "SELECT a * b + c, count(*) FROM t GROUP BY (a * b) + c",
        "SELECT a * (b + c), count(*) FROM t GROUP BY a * b + c",
        "SELECT CAST(a AS INT), count(*) FROM t GROUP BY a::INTEGER",
        "SELECT CAST(a AS INT), count(*) FROM t GROUP BY TRY_CAST(a AS INT)",
        "SELECT a = true, count(*) FROM t GROUP BY a = CAST('t' AS BOOLEAN)",
        "SELECT CASE a WHEN 1 THEN 2 END, count(*) FROM t GROUP BY CASE WHEN a = 1 THEN 2 ELSE NULL END",
        "SELECT a, SUM(b / 2) FROM t GROUP BY a",
        "SELECT a, SUM(b % 2) FROM t GROUP BY a",
        "SELECT a, SUM(b || c) FROM t GROUP BY a",
        "SELECT a, SUM(-b) FROM t GROUP BY a",
        "SELECT a, SUM(+b) FROM t GROUP BY a",
        "SELECT a, SUM(~b) FROM t GROUP BY a",
        "SELECT a, SUM(b // 2) FROM t GROUP BY a",
        "SELECT a, SUM(b ^ 2) FROM t GROUP BY a",
        "SELECT a, SUM(b & 1) FROM t GROUP BY a",
        "SELECT a, SUM(b | 1) FROM t GROUP BY a",
        "SELECT a, SUM(b << 1) FROM t GROUP BY a",
        "SELECT a, SUM(b >> 1) FROM t GROUP BY a",
        "SELECT a, SUM(lower(b)) FROM t GROUP BY a",
        "SELECT a, SUM(concat(b, c)) FROM t GROUP BY a",
        "SELECT a, SUM(NULLIF(b, c)) FROM t GROUP BY a",
        "SELECT a, SUM(b[1]) FROM t GROUP BY a",
        "SELECT a, SUM(EXTRACT(year FROM b)) FROM t GROUP BY a",
        "SELECT a, SUM([b, c]) FROM t GROUP BY a",
        "SELECT a, SUM(CEIL(b)) FROM t GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b LIKE 'x%' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b NOT LIKE 'x%' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b ILIKE 'x%' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b LIKE 'x%' ESCAPE '!' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b SIMILAR TO 'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b NOT SIMILAR TO 'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b ~ 'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IN (1, 2) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b NOT IN (1, 2) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IN (SELECT 1) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE EXISTS (SELECT 1) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE NOT EXISTS (SELECT 1) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = ANY (SELECT 1) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > (SELECT 1) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b BETWEEN 1 AND 2 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b NOT BETWEEN 1 AND 2 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IS TRUE GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IS NOT TRUE GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IS FALSE GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IS DISTINCT FROM c GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IS NOT DISTINCT FROM c GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b <> c AND b != c AND b == c GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b < c AND b <= c AND b >= c GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b <=> c GROUP BY a",
        "SELECT a, count(*) FROM t WHERE NOT b GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b IS NULL GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b COLLATE nocase = 'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = $1 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = ? GROUP BY a",
        "SELECT a, count(*) FROM t WHERE lower(b) = 'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = DATE '2020-01-01' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > INTERVAL 1 DAY GROUP BY a",
        "SELECT a, count(*) FROM t WHERE TRY_CAST(b AS INT) > 0 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE (b, c) = (1, 2) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE CAST(b AS VARCHAR) = 'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE CAST(b AS BOOL) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE CAST('yes' AS BOOLEAN) GROUP BY a",
        "SELECT a, count(*) FROM t WHERE 'true'::BOOLEAN GROUP BY a",
        "SELECT a, count(*) FROM t WHERE current_date > b GROUP BY a",
        "SELECT a, count(*) FROM t WHERE t.current_date > b GROUP BY a",
        "SELECT a, count(*) FROM t WHERE 1 = 1 GROUP BY a",
        "SELECT d.k, count(*) FROM d JOIN i ON d.k = i.k WHERE 1 = 1 AND i.x > 0 GROUP BY d.k",
        "SELECT a, count(*) FROM t WHERE t.b.c = 1 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE u.b = 1 GROUP BY a",
        "SELECT a, count(*) FROM t x WHERE t.b = 1 GROUP BY a",
        "SELECT x.a, count(*) FROM main.t AS x(p, q) GROUP BY x.a",
        "SELECT t.a, count(*) FROM main.t GROUP BY t.a",
        "SELECT a.k, count(*) FROM a, b GROUP BY a.k",
        "SELECT a.k, count(*) FROM a CROSS JOIN b GROUP BY a.k",
        "SELECT a.k, count(*) FROM a NATURAL JOIN b GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b USING (k) GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k = b.k AND a.x = b.x GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k < b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON (a.k = b.k) GROUP BY a.k",
        "SELECT a.k, count(*) FROM (a JOIN b ON a.k = b.k) GROUP BY a.k",
        "SELECT a.k, count(*) FROM a CROSS APPLY b GROUP BY a.k",
        "SELECT a.k, count(*) FROM c JOIN (a CROSS APPLY b) ON c.k = a.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM (a STRAIGHT_JOIN b ON a.k = b.k) GROUP BY a.k",
        "SELECT a.k, count(*) FROM a INNER JOIN b ON a.k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k = b.k JOIN c ON b.k = c.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k = b.k, c GROUP BY a.k",
        "SELECT a.k, count(*) FROM a SEMI JOIN b ON a.k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a ANTI JOIN b ON a.k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a FULL OUTER JOIN b ON a.k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a RIGHT JOIN b ON a.k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k = 1 GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k = a.j GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN b ON k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN (SELECT 1 AS k) b ON a.k = b.k GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN read_csv('x') b ON a.k = b.k GROUP BY a.k",
        "SELECT x.k, count(*) FROM a AS x JOIN b AS y ON x.k = y.k WHERE y.z = 1 GROUP BY x.k",
        "SELECT a.k, count(*) FROM a JOIN b ON a.k = b.k WHERE c.x = 1 GROUP BY a.k",
        "SELECT a.k, count(*) FROM a JOIN a ON a.k = a.k GROUP BY a.k",
        "SELECT a, count(*) FROM (SELECT a FROM t) GROUP BY a",
        "SELECT a, count(*) FROM read_csv('x') GROUP BY a",
        "SELECT a, count(*) FROM range(3) t(a) GROUP BY a",
        "SELECT a, count(*) FROM t TABLESAMPLE 10 GROUP BY a",
        "SELECT 1",
        "SELECT count(*)",
        "VALUES (1)",
        "(SELECT a, count(*) FROM t GROUP BY a)",
        "SELECT a, count(*) FROM t GROUP BY a UNION SELECT 1, 2",
        "FROM t SELECT a, count(*) GROUP BY a",
        "",
        ";",
        "SELEC 1",
        "CREATE VIEW v AS SELECT 1",
        "SELECT a, count(*) FROM t GROUP BY a; SELECT 1",
        "SELECT a, count(*) FROM t GROUP BY a; CREATE VIEW v AS SELECT 1",
        "SELECT * FROM t",
        "SELECT *, count(*) FROM t GROUP BY ALL",
        "SELECT t.*, count(*) FROM t GROUP BY ALL",
        "SELECT a, count(*) AS a FROM t GROUP BY a",
        "SELECT a AS count, count(*) FROM t GROUP BY a",
        "SELECT a, count(*), count(*) FROM t GROUP BY a",
        "SELECT a, sum(b), sum(b) FROM t GROUP BY a",
        "SELECT a, sum(b + c), sum(b - c) FROM t GROUP BY a",
        "SELECT current_date, count(*) FROM t GROUP BY current_date",
        "SELECT a, count(*) FROM t WHERE b > now() GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > CURRENT_TIMESTAMP GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > LOCALTIMESTAMP GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > main.random() GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > uuid() GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > current_user GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b > random() OVER () GROUP BY a",
        "SELECT a, count(*) FROM (SELECT a, random() AS r FROM t) GROUP BY a",
        "WITH x AS (SELECT a FROM t WHERE b > now()) SELECT a, count(*) FROM x GROUP BY a",
        "WITH RECURSIVE x AS (SELECT 1 AS a) SELECT a, count(*) FROM x GROUP BY a",
        "SELECT a, count(*) FROM t GROUP BY a LIMIT 1",
        "SELECT a, count(*) FROM t GROUP BY a ORDER BY 2 DESC",
        "SELECT a, count(*) OVER () FROM t",
        "SELECT a, row_number() OVER () FROM t",
        "SELECT a, bool_and(b) FROM t GROUP BY a",
        "SELECT a, product(b), sum(b) FROM t GROUP BY a",
        "SELECT a, rank() FROM t GROUP BY a",
        "SELECT a, mode() WITHIN GROUP (ORDER BY b) FROM t GROUP BY a",
        "SELECT a, percentile_disc(0.5) WITHIN GROUP (ORDER BY b) FROM t GROUP BY a",
        "SELECT a, percentile_cont(b, 0.5) FROM t GROUP BY a",
        "SELECT a, listagg(b, ',') WITHIN GROUP (ORDER BY b) FROM t GROUP BY a",
        "SELECT a, sum(b) WITHIN GROUP (ORDER BY b) FROM t GROUP BY a",
        "SELECT a, string_agg(b, ',' ORDER BY b) FROM t GROUP BY a",
        "SELECT a, array_agg(b) FROM t GROUP BY a",
        "SELECT a, group_concat(b) FROM t GROUP BY a",
        "SELECT a, sum(b) FROM t WHERE b IN (SELECT c FROM u WHERE u.x = t.a) GROUP BY a",
        "SELECT a, (SELECT 1) FROM t",
        "SELECT a, count(*) FROM t WHERE a = 'MEDIAN(' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE a = \"limit\" GROUP BY a",
        "SELECT a, count(*) FROM t -- ORDER BY a\nGROUP BY a",
        "SELECT a, count(*) FROM t /* DISTINCT */ GROUP BY a",
        "SELECT a, count(*) FROM t GROUP BY CUBE (a)",
        "SELECT a, count(*) FROM t GROUP BY ROLLUP (a)",
        "SELECT a, count(*) FROM t GROUP BY a, ROLLUP (a)",
        "SELECT a, count(*) FROM t GROUP BY GROUPING SETS ((a), (a))",
        "SELECT a, count(*) FROM t WHERE b IS NOT NULL GROUP BY a",
        "SELECT a, count(*) FROM lower('x') GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = 1_000 GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b = E'x' GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b ISNULL GROUP BY a",
        "SELECT a, count(*) FROM t WHERE b NOTNULL GROUP BY a",
        "SELECT a, SUM(b ** 2) FROM t GROUP BY a",
        "SELECT a, count(*) FROM t GROUP BY #1",
        "SELECT a, count(*) FROM t WHERE b GLOB 'x' GROUP BY a",
        "SELECT a.k, count(*) FROM a POSITIONAL JOIN b GROUP BY a.k",
        "SELECT a.k, count(*) FROM a ASOF JOIN b ON a.k = b.k GROUP BY a.k",
        "SELECT a, count(*) FROM t GROUP BY a WITH ROLLUP",
        "TABLE t",
    ];
}
