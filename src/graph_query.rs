//! RFC-0053 S2 (#1266): compile an accepted Graph query dialect to SQL over a nest.
//!
//! S1 (#1265) makes a client willing to talk to us. This is the half where its query returns rows.
//!
//! **Why not reuse `graph_validate`'s parser.** That one is deliberately selection-only - its doc
//! says it "skips what cannot change which fields were requested: arguments, directives, variable
//! definitions" - because its job is to know what to compare. S2's job is the opposite: the arguments
//! *are* the work, and a `where` clause it misreads is a wrong answer rather than a missing
//! comparison. Different job, different failure mode, so a separate parser whose rule is to refuse
//! anything it cannot lower exactly.
//!
//! **Refuse, never approximate.** RFC-0053 §Design: "It must never silently approximate a value and
//! call the endpoint drop-in." The same applies to a query - an operator or argument this compiler
//! does not implement is an error in the Graph envelope, never a query quietly run without it. A
//! dropped `where` clause returns more rows than asked for, which is worse than returning none.

use std::collections::BTreeMap;
use std::fmt;

use crate::graph_schema::{self, Schema};

/// One root field of an operation, with its arguments and the fields it selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootField {
    /// The key this root answers under: the alias if one was written, otherwise `name`.
    ///
    /// Separate from `name` because `{ p: pools { id } }` looks up `pools` in the schema and answers
    /// under `p`. Using one string for both either rejects the query or looks up the wrong root.
    pub key: String,
    /// The schema's field name: `pools`, `pool`, `_meta`.
    pub name: String,
    pub args: BTreeMap<String, Value>,
    /// What the caller selected, in the order they wrote it, sub-selections included.
    pub sel: Vec<Selection>,
}

/// One selected field and its sub-selection.
///
/// **Nesting is recorded here and refused at lowering, not during the parse.** `_meta { block {
/// number } }` is a legitimate nested selection the handler answers itself, and refusing nesting
/// while parsing made `_meta` unaskable - found by the HTTP test. The compiler is the only place
/// that knows whether a join exists for a given relation, so it is the only place that can decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// The key this field answers under - the alias if written, otherwise `name`.
    pub key: String,
    /// The schema's field name, which is what the column is called.
    pub name: String,
    /// Arguments written on this field. Recorded rather than refused for the same reason as the
    /// sub-selection below: a real introspection query writes `fields(includeDeprecated: true)`, and
    /// the handler answers introspection itself.
    pub args: BTreeMap<String, Value>,
    /// Empty for a scalar.
    pub sub: Vec<Selection>,
}

/// A GraphQL argument value, as far as S2 needs to understand one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Str(String),
    Bool(bool),
    Enum(String),
    List(Vec<Value>),
    Object(BTreeMap<String, Value>),
}

impl Value {
    /// A JSON value from a request's `variables`, as this dialect's value.
    ///
    /// A JSON number that is not an integer has no place in this dialect - `BigDecimal` and `BigInt`
    /// both travel as strings over GraphQL, precisely because a float would lose them - so a
    /// fractional number is `None` and refused by name rather than rounded into a filter.
    pub fn from_json(v: &serde_json::Value) -> Option<Value> {
        Some(match v {
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Int(n.as_i64()?),
            serde_json::Value::Array(a) => {
                Value::List(a.iter().map(Value::from_json).collect::<Option<_>>()?)
            }
            serde_json::Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, v)| Value::from_json(v).map(|v| (k.clone(), v)))
                    .collect::<Option<_>>()?,
            ),
            serde_json::Value::Null => return None,
        })
    }
    /// The SQL literal for this value. Strings are single-quoted with internal quotes doubled; there
    /// is no other escaping because there is no other type that reaches a literal position.
    fn sql_literal(&self) -> Option<String> {
        Some(match self {
            Value::Int(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) | Value::Enum(s) => format!("'{}'", s.replace('\'', "''")),
            Value::List(_) | Value::Object(_) => return None,
        })
    }
}

/// Why a query could not be compiled. Every variant is a refusal the caller sees, never a silent
/// fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    Syntax(String),
    /// A root field the schema does not declare.
    UnknownRoot(String),
    /// A selected field the entity does not have.
    UnknownField {
        entity: String,
        field: String,
    },
    /// A nested selection. Lowering one needs a join or a JSON aggregation, not an N+1 walk, and that
    /// is the next slice rather than something to fake.
    NestedSelection(String),
    /// An argument this compiler does not implement.
    Argument(String),
    /// A `where` operator this compiler does not implement. Named so the caller knows which.
    Operator(String),
    /// `block:` needs a block-ranged entity store, which the nest has not got (#1267).
    TimeTravel,
    /// A `$name` the operation used but neither the request nor a header default supplies.
    UnboundVariable(String),
    /// A `mutation` or `subscription`. A nest has neither.
    NotAQuery(String),
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unsupported::Syntax(w) => write!(f, "could not parse the operation: {w}"),
            Unsupported::UnknownRoot(n) => {
                write!(f, "`{n}` is not a field of this subgraph's Query type")
            }
            Unsupported::UnknownField { entity, field } => {
                write!(f, "`{field}` is not a field of `{entity}`")
            }
            Unsupported::NestedSelection(n) => write!(
                f,
                "nested selection on `{n}` is not implemented yet (RFC-0053 S2 lowers these to a \
                 join rather than an N+1 walk); select scalar fields meanwhile"
            ),
            Unsupported::Argument(a) => write!(f, "argument `{a}` is not implemented yet"),
            Unsupported::UnboundVariable(v) => write!(
                f,
                "no value was supplied for variable `${v}`, and its definition declares no default"
            ),
            Unsupported::NotAQuery(k) => {
                write!(
                    f,
                    "a nest serves queries; `{k}` operations do not exist here"
                )
            }
            Unsupported::Operator(o) => write!(
                f,
                "filter operator `{o}` is not implemented yet; it is refused rather than ignored, \
                 because a dropped filter returns more rows than you asked for"
            ),
            Unsupported::TimeTravel => write!(
                f,
                "`block:` needs a block-ranged entity store, which this nest does not have yet \
                 (nuthatch#1267). Every answer here is as of the nest's current head, reported in \
                 `_meta`"
            ),
        }
    }
}

/// Parse the root fields of an operation, with their arguments.
/// Levels of relation traversal `compile` will lower.
///
/// One, matching `E_orderBy`'s traversal depth in the recorded reference: graph-node emits
/// `token0__symbol` and no `token0__whitelistPools__id`, so one level is the depth the schema itself
/// advertises. Deeper parses - the handler needs deep selections for introspection - and is refused
/// by name at lowering rather than answered with an N+1 walk.
const MAX_TRAVERSAL: usize = 1;

/// The field at which `sel` exceeds `budget` levels of traversal, if any.
///
/// Recursive rather than a one-level `!sub.is_empty()` test, so [`MAX_TRAVERSAL`] is the thing being
/// enforced instead of a comment next to a hand-unrolled check of it.
fn too_deep(sel: &[Selection], budget: usize) -> Option<&Selection> {
    for s in sel {
        if s.sub.is_empty() {
            continue;
        }
        if budget == 0 {
            return Some(s);
        }
        if let Some(deeper) = too_deep(&s.sub, budget - 1) {
            return Some(deeper);
        }
    }
    None
}

/// The base table's alias. Every column is qualified with it; see `compile`.
const BASE: &str = "b";

pub fn parse(query: &str) -> Result<Vec<RootField>, Unsupported> {
    parse_with(query, &BTreeMap::new())
}

/// Parse an operation, binding `$name` arguments from `vars`.
///
/// Every generated client sends its arguments this way - `query Pools($first: Int!) { pools(first:
/// $first) … }` - so a compiler that refuses variables refuses in practice every query a client
/// produces, whatever it can do with a hand-written one. A `$name` with no supplied value and no
/// header default is [`Unsupported::UnboundVariable`], never a silently-dropped argument.
pub fn parse_with(
    query: &str,
    vars: &BTreeMap<String, Value>,
) -> Result<Vec<RootField>, Unsupported> {
    let b = query.as_bytes();
    let mut c = Cursor {
        b,
        i: 0,
        vars,
        defaults: BTreeMap::new(),
    };
    // A document is a list of definitions in any order, so fragments are collected as they are met
    // and spreads are resolved once the whole document has been read - a fragment may legally be
    // defined after the operation that uses it.
    let mut fragments: BTreeMap<String, Vec<Sel>> = BTreeMap::new();
    let mut operation: Option<Vec<Sel>> = None;
    loop {
        c.trivia();
        if c.peek().is_none() {
            break;
        }
        if c.peek() == Some(b'{') {
            // The anonymous shorthand operation.
            if operation.is_some() {
                return Err(Unsupported::Syntax("more than one operation".into()));
            }
            operation = Some(c.selection_set()?);
            continue;
        }
        let kw = c.ident()?;
        match kw.as_str() {
            "fragment" => {
                c.trivia();
                let name = c.ident()?;
                c.trivia();
                // `on TypeCondition`. The condition is consumed and not checked: this compiler
                // validates every field against the entity anyway, so a spread naming fields the
                // entity has not got is already refused by name.
                if c.ident()? != "on" {
                    return Err(Unsupported::Syntax(format!(
                        "fragment `{name}` has no type condition"
                    )));
                }
                c.trivia();
                c.type_ref()?;
                c.trivia();
                if c.peek() != Some(b'{') {
                    return Err(Unsupported::Syntax(format!(
                        "fragment `{name}` has no body"
                    )));
                }
                let body = c.selection_set()?;
                fragments.insert(name, body);
            }
            "query" => {
                if operation.is_some() {
                    return Err(Unsupported::Syntax("more than one operation".into()));
                }
                c.operation_tail()?;
                if c.peek() != Some(b'{') {
                    return Err(Unsupported::Syntax("no selection set".into()));
                }
                operation = Some(c.selection_set()?);
            }
            other @ ("mutation" | "subscription") => {
                return Err(Unsupported::NotAQuery(other.to_string()))
            }
            other => {
                return Err(Unsupported::Syntax(format!(
                    "expected an operation or a fragment, found `{other}`"
                )))
            }
        }
    }
    let Some(operation) = operation else {
        return Err(Unsupported::Syntax("no selection set".into()));
    };
    let roots = resolve_spreads(&operation, &fragments, &mut Vec::new())?;
    Ok(roots
        .into_iter()
        .map(|s| RootField {
            key: s.key,
            name: s.name,
            args: s.args,
            sel: s.sub,
        })
        .collect())
}

/// One entry of a selection set before fragment spreads have been resolved.
#[derive(Debug, Clone)]
enum Sel {
    Field(Selection, Vec<Sel>),
    /// `...Name`
    Spread(String),
    /// `... on Type { … }`, whose selections are spliced in place.
    Inline(Vec<Sel>),
}

/// Splice every fragment spread into the selection set that used it.
///
/// `visiting` is the spread stack, so a fragment that refers to itself is an error rather than a
/// stack overflow - a client cannot send one by accident, but a malformed document should not take the
/// node down.
fn resolve_spreads(
    sel: &[Sel],
    fragments: &BTreeMap<String, Vec<Sel>>,
    visiting: &mut Vec<String>,
) -> Result<Vec<Selection>, Unsupported> {
    let mut out: Vec<Selection> = Vec::new();
    for s in sel {
        match s {
            Sel::Field(f, sub) => out.push(Selection {
                key: f.key.clone(),
                name: f.name.clone(),
                args: f.args.clone(),
                sub: resolve_spreads(sub, fragments, visiting)?,
            }),
            Sel::Inline(inner) => out.extend(resolve_spreads(inner, fragments, visiting)?),
            Sel::Spread(name) => {
                let Some(body) = fragments.get(name) else {
                    return Err(Unsupported::Syntax(format!(
                        "fragment `{name}` is spread but never defined"
                    )));
                };
                if visiting.iter().any(|v| v == name) {
                    return Err(Unsupported::Syntax(format!(
                        "fragment `{name}` spreads itself"
                    )));
                }
                visiting.push(name.clone());
                out.extend(resolve_spreads(body, fragments, visiting)?);
                visiting.pop();
            }
        }
    }
    Ok(out)
}

struct Cursor<'a> {
    b: &'a [u8],
    i: usize,
    /// Values supplied with the request, then the operation header's declared defaults. Resolved at
    /// the value site rather than carried as a `Value::Var`, so nothing downstream of the parser can
    /// forget that a variable existed.
    vars: &'a BTreeMap<String, Value>,
    defaults: BTreeMap<String, Value>,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    /// Whitespace, commas (insignificant in GraphQL) and `#` comments.
    fn trivia(&mut self) {
        while let Some(c) = self.peek() {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' | b',' => self.i += 1,
                b'#' => {
                    while self.peek().is_some_and(|c| c != b'\n') {
                        self.i += 1;
                    }
                }
                _ => break,
            }
        }
    }
    fn ident(&mut self) -> Result<String, Unsupported> {
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        {
            self.i += 1;
        }
        if start == self.i {
            return Err(Unsupported::Syntax(format!(
                "expected a name at byte {start}"
            )));
        }
        Ok(String::from_utf8_lossy(&self.b[start..self.i]).into_owned())
    }
    /// A selection set, to any depth, with the arguments each field carries.
    ///
    /// **No depth limit and no argument refusal here.** Both belong to lowering: an introspection
    /// query is about twenty levels deep and writes `fields(includeDeprecated: true)`, and the
    /// handler answers introspection without the compiler ever seeing it. Refusing depth while
    /// parsing made introspection unaskable through the parser, which is what forced the handler to
    /// detect it by searching the raw text - and a filter value of `"__schema"` then routed a
    /// perfectly ordinary query to the schema document (Jules on #1282).
    fn selection_set(&mut self) -> Result<Vec<Sel>, Unsupported> {
        self.i += 1; // '{'
        let mut out = Vec::new();
        loop {
            self.trivia();
            match self.peek() {
                None => return Err(Unsupported::Syntax("unclosed selection set".into())),
                Some(b'}') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {}
            }
            // `...Name` or `... on Type { … }`. The standard introspection document a generated
            // client sends is built out of these, so refusing them refused the one request that has
            // to work before any other can (Jules on #1282).
            if self.b[self.i..].starts_with(b"...") {
                self.i += 3;
                self.trivia();
                let word = self.ident()?;
                if word == "on" {
                    self.trivia();
                    self.type_ref()?;
                    self.trivia();
                    if self.peek() != Some(b'{') {
                        return Err(Unsupported::Syntax("an inline fragment has no body".into()));
                    }
                    out.push(Sel::Inline(self.selection_set()?));
                } else {
                    out.push(Sel::Spread(word));
                }
                continue;
            }
            // `alias: field`. The first identifier is the alias only if a colon follows it, which is
            // why the colon has to be looked for before anything else is decided.
            let first = self.ident()?;
            self.trivia();
            let (key, name) = if self.peek() == Some(b':') {
                self.i += 1;
                self.trivia();
                let real = self.ident()?;
                (first, real)
            } else {
                (first.clone(), first)
            };
            self.trivia();
            let args = if self.peek() == Some(b'(') {
                self.args()?
            } else {
                BTreeMap::new()
            };
            self.trivia();
            let sub = if self.peek() == Some(b'{') {
                self.selection_set()?
            } else {
                Vec::new()
            };
            out.push(Sel::Field(
                Selection {
                    key,
                    name,
                    args,
                    sub: Vec::new(),
                },
                sub,
            ));
        }
    }
    /// Consume an optional operation header, leaving the cursor on the selection set's `{`.
    ///
    /// **Parsed rather than skipped to the first brace.** A variable definition may carry a default
    /// that is itself an object - `$w: Pool_filter = { id: "a" }` - and skipping to the first `{`
    /// lands inside it, so the whole operation then reads as garbage.
    fn operation_tail(&mut self) -> Result<(), Unsupported> {
        self.trivia();
        // An optional operation name.
        if self
            .peek()
            .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
        {
            self.ident()?;
            self.trivia();
        }
        if self.peek() == Some(b'(') {
            self.variable_definitions()?;
            self.trivia();
        }
        // Directives on the operation change nothing this compiler lowers, and skipping one
        // silently would be the same class of mistake as a dropped filter, so refuse.
        if self.peek() == Some(b'@') {
            return Err(Unsupported::Syntax(
                "directives on an operation are not implemented yet".into(),
            ));
        }
        Ok(())
    }
    /// `($first: Int! = 10, $w: Pool_filter)`. Only the defaults are kept: the declared types are
    /// the client's assertion about its own values, and this compiler reads the values themselves.
    fn variable_definitions(&mut self) -> Result<(), Unsupported> {
        self.i += 1; // '('
        loop {
            self.trivia();
            match self.peek() {
                None => return Err(Unsupported::Syntax("unclosed variable definitions".into())),
                Some(b')') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(b'$') => self.i += 1,
                Some(c) => {
                    return Err(Unsupported::Syntax(format!(
                        "expected `$` in variable definitions, found `{}`",
                        c as char
                    )))
                }
            }
            let name = self.ident()?;
            self.trivia();
            if self.peek() != Some(b':') {
                return Err(Unsupported::Syntax(format!("`${name}` has no type")));
            }
            self.i += 1;
            self.type_ref()?;
            self.trivia();
            if self.peek() == Some(b'=') {
                self.i += 1;
                let v = self.value()?;
                self.defaults.insert(name, v);
                self.trivia();
            }
            if self.peek() == Some(b',') {
                self.i += 1;
            }
        }
    }
    /// A type reference - `Int`, `[Bytes!]!` - consumed and discarded.
    fn type_ref(&mut self) -> Result<(), Unsupported> {
        self.trivia();
        if self.peek() == Some(b'[') {
            self.i += 1;
            self.type_ref()?;
            self.trivia();
            if self.peek() != Some(b']') {
                return Err(Unsupported::Syntax("unclosed list type".into()));
            }
            self.i += 1;
        } else {
            self.ident()?;
        }
        while self.peek() == Some(b'!') {
            self.i += 1;
        }
        Ok(())
    }
    fn args(&mut self) -> Result<BTreeMap<String, Value>, Unsupported> {
        self.i += 1; // '('
        let mut out = BTreeMap::new();
        loop {
            self.trivia();
            match self.peek() {
                None => return Err(Unsupported::Syntax("unclosed arguments".into())),
                Some(b')') => {
                    self.i += 1;
                    return Ok(out);
                }
                _ => {}
            }
            let name = self.ident()?;
            self.trivia();
            if self.peek() != Some(b':') {
                return Err(Unsupported::Syntax(format!(
                    "argument `{name}` has no value"
                )));
            }
            self.i += 1;
            let v = self.value()?;
            out.insert(name, v);
        }
    }
    fn value(&mut self) -> Result<Value, Unsupported> {
        self.trivia();
        match self.peek() {
            Some(b'"') => {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c != b'"') {
                    if self.peek() == Some(b'\\') {
                        self.i += 1;
                    }
                    self.i += 1;
                }
                let s = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
                self.i += 1;
                Ok(Value::Str(s))
            }
            Some(b'[') => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.trivia();
                    match self.peek() {
                        None => return Err(Unsupported::Syntax("unclosed list".into())),
                        Some(b']') => {
                            self.i += 1;
                            return Ok(Value::List(v));
                        }
                        _ => v.push(self.value()?),
                    }
                }
            }
            Some(b'{') => {
                self.i += 1;
                let mut m = BTreeMap::new();
                loop {
                    self.trivia();
                    match self.peek() {
                        None => return Err(Unsupported::Syntax("unclosed object".into())),
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(Value::Object(m));
                        }
                        _ => {}
                    }
                    let k = self.ident()?;
                    self.trivia();
                    if self.peek() != Some(b':') {
                        return Err(Unsupported::Syntax(format!("`{k}` has no value")));
                    }
                    self.i += 1;
                    let v = self.value()?;
                    m.insert(k, v);
                }
            }
            Some(b'$') => {
                self.i += 1;
                let name = self.ident()?;
                self.vars
                    .get(&name)
                    .or_else(|| self.defaults.get(&name))
                    .cloned()
                    .ok_or(Unsupported::UnboundVariable(name))
            }
            Some(c) if c == b'-' || c.is_ascii_digit() => {
                let start = self.i;
                if c == b'-' {
                    self.i += 1;
                }
                while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    self.i += 1;
                }
                let raw = String::from_utf8_lossy(&self.b[start..self.i]).into_owned();
                raw.parse::<i64>()
                    .map(Value::Int)
                    .map_err(|_| Unsupported::Syntax(format!("`{raw}` is not an integer")))
            }
            _ => {
                let w = self.ident()?;
                Ok(match w.as_str() {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    _ => Value::Enum(w),
                })
            }
        }
    }
}

/// Where the caller's text has to appear in the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextShape {
    Anywhere,
    Prefix,
    Suffix,
}

impl TextShape {
    /// A quoted `LIKE` pattern for `text`, with the caller's own wildcards escaped.
    ///
    /// **This is the whole point of the function.** `%` and `_` are data in a caller's filter and
    /// wildcards in `LIKE`, so `symbol_contains: "50%"` must match a literal `50%` rather than
    /// anything beginning `50`. Backslash is escaped first, or escaping the wildcards would introduce
    /// backslashes this then re-escapes.
    fn pattern(self, text: &str) -> String {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
            // The SQL literal's own quoting, applied last so it cannot be undone by the above.
            .replace('\'', "''");
        match self {
            TextShape::Anywhere => format!("'%{escaped}%'"),
            TextShape::Prefix => format!("'{escaped}%'"),
            TextShape::Suffix => format!("'%{escaped}'"),
        }
    }
}

/// `(negated, case-insensitive, where the text sits)` for a text-matching suffix.
fn text_match(suffix: &str) -> Option<(bool, bool, TextShape)> {
    let (negated, rest) = match suffix.strip_prefix("_not") {
        Some(r) => (true, r),
        None => (false, suffix),
    };
    let (nocase, rest) = match rest.strip_suffix("_nocase") {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let shape = match rest {
        "_contains" => TextShape::Anywhere,
        "_starts_with" => TextShape::Prefix,
        "_ends_with" => TextShape::Suffix,
        _ => return None,
    };
    Some((negated, nocase, shape))
}

/// The `where` suffixes this slice lowers, and the SQL they become.
///
/// Deliberately the comparison set and not the text set. `_contains`, `_starts_with` and the
/// `_nocase` family lower to `LIKE` with escaping that wants its own slice and its own tests; until
/// then they are refused by name, which a caller can act on.
fn comparison(suffix: &str) -> Option<&'static str> {
    Some(match suffix {
        "" => "=",
        "_not" => "<>",
        "_gt" => ">",
        "_gte" => ">=",
        "_lt" => "<",
        "_lte" => "<=",
        _ => return None,
    })
}

/// One field of a response object, and where in the SQL row its value comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    /// A scalar: the key to answer under, and the column it arrives in. The two differ when the
    /// caller wrote an alias.
    Scalar { key: String, col: String },
    /// A to-one relation flattened into the row by a join.
    Object {
        /// The key to answer under - `token0`, or its alias.
        key: String,
        /// Each selected sub-field's key, and the column alias it arrives under.
        fields: Vec<(String, String)>,
    },
    /// A `@derivedFrom` list, aggregated into one JSON array by a correlated subquery.
    List {
        /// The key to answer under - `swaps`, or its alias.
        key: String,
        /// The column alias the JSON array arrives under.
        col: String,
    },
}

/// A compiled query: the SQL, and the shape of the object each row becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub sql: String,
    /// How to build one response object from one SQL row, in the caller's field order.
    ///
    /// Needed because a relation traversal flattens into the same row: `token0 { symbol }` arrives as
    /// a column named `j0__symbol`, and only this says it belongs under `token0`.
    pub shape: Vec<Shape>,
    /// The entity this root field returns, for shaping the response.
    pub entity: String,
    /// `true` for a singular root (`pool`), which returns one object rather than a list.
    pub singular: bool,
}

/// Compile one root field against the generated schema.
///
/// The view name is the entity's snake_case alias, which is what `port-emit` writes.
pub fn compile(schema: &Schema, root: &RootField) -> Result<Compiled, Unsupported> {
    let (entity, singular) = resolve_root(schema, &root.name)?;
    let ent = schema
        .entities
        .iter()
        .find(|e| e.name == entity)
        .expect("resolve_root returned an entity the schema has");

    if let Some(s) = too_deep(&root.sel, MAX_TRAVERSAL) {
        return Err(Unsupported::NestedSelection(s.name.clone()));
    }
    // Every column is qualified with the base alias whether or not this query joins. One shape
    // rather than two, and a relation whose target happens to share a column name - `id` always
    // does - cannot then turn a working query into an ambiguous one on some other schema.
    let mut cols: Vec<String> = Vec::new();
    let mut shape: Vec<Shape> = Vec::new();
    let mut joins = String::new();
    for (i, sel) in root.sel.iter().enumerate() {
        let field = ent
            .fields
            .iter()
            .find(|x| x.name == sel.name)
            .ok_or_else(|| Unsupported::UnknownField {
                entity: entity.clone(),
                field: sel.name.clone(),
            })?;
        if sel.sub.is_empty() {
            // A composite field needs a selection set, and GraphQL validation rejects one without.
            // Lowering `{ pools { token0 } }` to `SELECT b."token0"` returned the relation's id under
            // a field the schema declares as `Token!`, so the endpoint accepted a query a client's
            // own validator would have refused and answered it in a shape the schema does not
            // describe (Jules on #1282).
            if let Some(target) = field.ty.entity_name() {
                return Err(Unsupported::Syntax(format!(
                    "`{}.{}` returns `{target}`, which needs a selection set",
                    entity, sel.name
                )));
            }
            // An aliased scalar needs its own column alias, or two aliases on one field would
            // collide in the row. `a{i}` rather than the caller's key, because a key is arbitrary
            // text and could collide with a join's `j{i}__…`.
            let col = if sel.key == sel.name {
                cols.push(format!("{BASE}.\"{}\"", sel.name));
                sel.name.clone()
            } else {
                let c = format!("a{i}");
                cols.push(format!("{BASE}.\"{}\" AS \"{c}\"", sel.name));
                c
            };
            shape.push(Shape::Scalar {
                key: sel.key.clone(),
                col,
            });
            continue;
        }
        // Arguments on a traversed field are refused **before** any traversal is lowered. This guard
        // used to sit after the derived-list branch below, so `swaps(first: 5)` compiled with the
        // `first` silently dropped and every related row returned - caught by an existing test, and
        // the same class of fault as everything else this module refuses.
        if !sel.args.is_empty() {
            return Err(Unsupported::NestedSelection(sel.name.clone()));
        }
        // A `@derivedFrom` list is aggregated rather than joined. A plain join would multiply the
        // parent row once per child, so `first` would stop meaning what it says; one correlated
        // subquery per parent keeps the parent's row count and the child's own page size separate.
        if let (graph_schema::FieldType::List(inner), Some(back)) = (&field.ty, &field.derived_from)
        {
            let Some(target) = inner.entity_name() else {
                return Err(Unsupported::NestedSelection(sel.name.clone()));
            };
            let child = schema
                .entities
                .iter()
                .find(|e| e.name == target)
                .ok_or_else(|| Unsupported::UnknownField {
                    entity: entity.clone(),
                    field: sel.name.clone(),
                })?;
            // The back-reference is the schema author's, named in `@derivedFrom(field: …)`. One that
            // the child has not got is a broken schema rather than a query this can answer.
            if !child.fields.iter().any(|f| &f.name == back) {
                return Err(Unsupported::UnknownField {
                    entity: target.to_string(),
                    field: back.clone(),
                });
            }
            let alias = format!("c{i}");
            let cview = crate::subgraph_import::to_alias(target);
            let mut packed = Vec::new();
            for sub in &sel.sub {
                if !sub.sub.is_empty() || !sub.args.is_empty() {
                    return Err(Unsupported::NestedSelection(sub.name.clone()));
                }
                if !child.fields.iter().any(|f| f.name == sub.name) {
                    return Err(Unsupported::UnknownField {
                        entity: target.to_string(),
                        field: sub.name.clone(),
                    });
                }
                packed.push(format!("\"{}\" := {alias}.\"{}\"", sub.name, sub.name));
            }
            let col = format!("{alias}__{}", sel.name);
            // `to_json(list(…))` rather than a bare `LIST` of `STRUCT`, so the column arrives as a
            // plain JSON string and nothing depends on how the row serialiser handles a nested DuckDB
            // type. `ORDER BY`/`LIMIT` cannot sit inside the aggregate, hence the inner subquery.
            //
            // **`coalesce` is not decoration**: `list()` over zero rows is `NULL`, so a parent with no
            // children would answer `null` for a field the generated schema types `[{target}!]!`.
            cols.push(format!(
                "coalesce((SELECT to_json(list(t.s)) FROM (SELECT struct_pack({}) AS s \
                 FROM \"{cview}\" {alias} WHERE {alias}.\"{back}\" = {BASE}.\"id\" \
                 ORDER BY {alias}.\"id\" ASC LIMIT 100) t), '[]') AS \"{col}\"",
                packed.join(", ")
            ));
            shape.push(Shape::List {
                key: sel.key.clone(),
                col,
            });
            continue;
        }
        // A **to-one** reference is a join on the id this row already holds, and because the target's
        // id is unique the join cannot multiply rows - so `first` still means what it says. Anything
        // else - a stored array of ids, which the reference has as `Token.whitelistPools` - needs
        // `unnest` and its own slice, so it is refused by name.
        let target = match (&field.ty, &field.derived_from) {
            (graph_schema::FieldType::Entity(t), None) => t.clone(),
            _ => return Err(Unsupported::NestedSelection(sel.name.clone())),
        };
        let tent = schema
            .entities
            .iter()
            .find(|e| e.name == target)
            .ok_or_else(|| Unsupported::UnknownField {
                entity: entity.clone(),
                field: sel.name.clone(),
            })?;
        let alias = format!("j{i}");
        let tview = crate::subgraph_import::to_alias(&target);
        // LEFT, not INNER: a reference whose target row is absent must leave the parent in the
        // answer with a null relation, exactly as graph-node does, rather than drop the parent.
        joins.push_str(&format!(
            " LEFT JOIN \"{tview}\" {alias} ON {alias}.\"id\" = {BASE}.\"{}\"",
            sel.name
        ));
        let mut sub = Vec::new();
        for s in &sel.sub {
            if !s.args.is_empty() {
                return Err(Unsupported::NestedSelection(s.name.clone()));
            }
            if !tent.fields.iter().any(|x| x.name == s.name) {
                return Err(Unsupported::UnknownField {
                    entity: target.clone(),
                    field: s.name.clone(),
                });
            }
            let col = format!("{alias}__{}", s.name);
            cols.push(format!("{alias}.\"{}\" AS \"{col}\"", s.name));
            sub.push((s.key.clone(), col));
        }
        shape.push(Shape::Object {
            key: sel.key.clone(),
            fields: sub,
        });
    }
    // An entity root with no selection set is not a legal GraphQL query - a composite type must be
    // selected from - and answering `*` for one would invent a field list the caller never asked for.
    if cols.is_empty() {
        return Err(Unsupported::Syntax(format!(
            "`{}` returns `{entity}`, which needs a selection set",
            root.name
        )));
    }
    let select = cols.join(", ");

    let view = crate::subgraph_import::to_alias(&entity);
    let mut sql = format!("SELECT {select} FROM \"{view}\" {BASE}{joins}");
    let mut wheres: Vec<String> = Vec::new();

    for (name, value) in &root.args {
        match name.as_str() {
            "block" => return Err(Unsupported::TimeTravel),
            // Accepted and ignored on purpose: it selects an error policy, and a nest has no
            // subgraph indexing errors to report either way.
            "subgraphError" => {}
            "id" if singular => {
                let lit = value
                    .sql_literal()
                    .ok_or_else(|| Unsupported::Argument("id must be a string".into()))?;
                wheres.push(format!("{BASE}.\"id\" = {lit}"));
            }
            "where" => {
                let Value::Object(m) = value else {
                    return Err(Unsupported::Argument("`where` must be an object".into()));
                };
                for (key, v) in m {
                    wheres.push(lower_predicate(ent, key, v)?);
                }
            }
            "first" | "skip" | "orderBy" | "orderDirection" if !singular => {}
            other => return Err(Unsupported::Argument(other.to_string())),
        }
    }
    if !wheres.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
    }

    if !singular {
        // graph-node's default order is ascending by id, and a client that omits `orderBy` relies on
        // it: an unordered result with `skip` is a different page each time.
        let order_field = match root.args.get("orderBy") {
            Some(Value::Enum(f)) | Some(Value::Str(f)) => {
                if f.contains("__") {
                    return Err(Unsupported::Argument(format!(
                        "orderBy `{f}` traverses a relation, which needs the join this slice does \
                         not lower yet"
                    )));
                }
                if !ent.fields.iter().any(|x| &x.name == f) {
                    return Err(Unsupported::UnknownField {
                        entity: entity.clone(),
                        field: f.clone(),
                    });
                }
                f.clone()
            }
            Some(_) => return Err(Unsupported::Argument("orderBy must be an enum".into())),
            None => "id".to_string(),
        };
        let dir = match root.args.get("orderDirection") {
            Some(Value::Enum(d)) | Some(Value::Str(d)) if d == "desc" => "DESC",
            Some(Value::Enum(d)) | Some(Value::Str(d)) if d == "asc" => "ASC",
            None => "ASC",
            Some(_) => {
                return Err(Unsupported::Argument(
                    "orderDirection must be asc or desc".into(),
                ))
            }
        };
        sql.push_str(&format!(" ORDER BY {BASE}.\"{order_field}\" {dir}"));

        // graph-node's defaults, taken from the recorded reference: first = 100, skip = 0, and
        // §Query semantics caps first at 1000.
        // A supplied argument is never silently replaced by the default. `first: "1"` used to fall
        // through `as_i64` to `LIMIT 100`, which is this module's one prohibition - approximating a
        // value and calling the endpoint drop-in (Jules on #1282).
        let first = match root.args.get("first") {
            None => 100,
            Some(Value::Int(n)) => *n,
            Some(_) => {
                return Err(Unsupported::Argument(
                    "first must be an integer".to_string(),
                ))
            }
        };
        if !(0..=1000).contains(&first) {
            return Err(Unsupported::Argument(format!(
                "first must be between 0 and 1000, got {first}"
            )));
        }
        let skip = match root.args.get("skip") {
            None => 0,
            Some(Value::Int(n)) => *n,
            Some(_) => return Err(Unsupported::Argument("skip must be an integer".to_string())),
        };
        if skip < 0 {
            return Err(Unsupported::Argument(format!(
                "skip must not be negative, got {skip}"
            )));
        }
        sql.push_str(&format!(" LIMIT {first} OFFSET {skip}"));
    } else {
        sql.push_str(" LIMIT 1");
    }

    Ok(Compiled {
        sql,
        shape,
        entity,
        singular,
    })
}

fn lower_predicate(
    ent: &graph_schema::Entity,
    key: &str,
    v: &Value,
) -> Result<String, Unsupported> {
    if key == "and" || key == "or" {
        let Value::List(items) = v else {
            return Err(Unsupported::Argument(format!(
                "`{key}` needs a list of filters"
            )));
        };
        // An empty `and`/`or`, and an empty filter object inside one, are refused rather than given a
        // meaning. `_in []` could be reasoned about from SQL - there is no empty `IN` list and the
        // empty set matches nothing - but "no conditions" has no such forced reading, graph-node's
        // behaviour here is not something this slice has measured, and guessing it would be
        // approximating a predicate.
        if items.is_empty() {
            return Err(Unsupported::Argument(format!(
                "`{key}` is empty, and an empty condition list has no meaning this slice has verified"
            )));
        }
        let mut parts = Vec::new();
        for item in items {
            let Value::Object(m) = item else {
                return Err(Unsupported::Argument(format!(
                    "`{key}` takes filter objects"
                )));
            };
            if m.is_empty() {
                return Err(Unsupported::Argument(format!(
                    "`{key}` contains an empty filter"
                )));
            }
            let inner: Result<Vec<String>, Unsupported> = m
                .iter()
                .map(|(k, vv)| lower_predicate(ent, k, vv))
                .collect();
            // Conditions within one filter object are ANDed, which is what `where` itself does.
            parts.push(format!("({})", inner?.join(" AND ")));
        }
        let joiner = if key == "and" { " AND " } else { " OR " };
        // Parenthesised as a whole: `a OR b` spliced unbracketed into the `WHERE`'s AND list would
        // bind as `x AND a OR b`, which is a different query.
        return Ok(format!("({})", parts.join(joiner)));
    }
    if key == "_change_block" {
        return Err(Unsupported::Operator("_change_block".into()));
    }
    // Longest suffix first, so `_not_in` is not read as `_not`.
    const SUFFIXES: &[&str] = &[
        "_not_contains_nocase",
        "_not_starts_with_nocase",
        "_not_ends_with_nocase",
        "_starts_with_nocase",
        "_ends_with_nocase",
        "_contains_nocase",
        "_not_contains",
        "_not_starts_with",
        "_not_ends_with",
        "_starts_with",
        "_ends_with",
        "_contains",
        "_not_in",
        "_not",
        "_gte",
        "_lte",
        "_gt",
        "_lt",
        "_in",
        "",
    ];
    for suffix in SUFFIXES {
        let Some(field) = key.strip_suffix(suffix) else {
            continue;
        };
        // A nested relation filter is `token0_`, which strips to `token0` with an empty suffix.
        if field.ends_with('_') || field.is_empty() {
            continue;
        }
        let Some(f) = ent.fields.iter().find(|x| x.name == field) else {
            continue;
        };
        // The operator has to be one the generated schema declares for *this* field's type, which
        // `filter_suffixes` derives from the recorded reference. `Bytes` carries ten operators and
        // `String` eighteen - no `_starts_with`, no `_nocase` on `Bytes` - so accepting one here would
        // answer a query a client's own validator, built from our schema, would have refused.
        let allowed =
            f.ty.filter_scalar()
                .map(graph_schema::filter_suffixes)
                .unwrap_or_default();
        if !allowed.contains(suffix) {
            return Err(Unsupported::Operator(key.to_string()));
        }
        let col = format!("{BASE}.\"{field}\"");
        return match *suffix {
            "_in" | "_not_in" => {
                let Value::List(items) = v else {
                    return Err(Unsupported::Argument(format!("`{key}` needs a list")));
                };
                let lits: Option<Vec<String>> = items.iter().map(Value::sql_literal).collect();
                let Some(lits) = lits else {
                    return Err(Unsupported::Argument(format!("`{key}` has a nested value")));
                };
                if lits.is_empty() {
                    // `IN ()` is not valid SQL and an empty list matches nothing.
                    return Ok(if *suffix == "_in" {
                        "FALSE".into()
                    } else {
                        "TRUE".into()
                    });
                }
                let neg = if *suffix == "_in" { "IN" } else { "NOT IN" };
                Ok(format!("{col} {neg} ({})", lits.join(", ")))
            }
            s if text_match(s).is_some() => {
                let (negated, nocase, shape) = text_match(s).expect("just matched");
                let Value::Str(text) = v else {
                    return Err(Unsupported::Argument(format!("`{key}` needs a string")));
                };
                let pattern = shape.pattern(text);
                let op = match (negated, nocase) {
                    (false, false) => "LIKE",
                    (true, false) => "NOT LIKE",
                    (false, true) => "ILIKE",
                    (true, true) => "NOT ILIKE",
                };
                // The escape character is declared, because the caller's own `%` and `_` are data.
                Ok(format!("{col} {op} {pattern} ESCAPE '\\'"))
            }
            s => match comparison(s) {
                Some(op) => {
                    let lit = v
                        .sql_literal()
                        .ok_or_else(|| Unsupported::Argument(format!("`{key}` has no literal")))?;
                    Ok(format!("{col} {op} {lit}"))
                }
                None => Err(Unsupported::Operator(key.to_string())),
            },
        };
    }
    Err(Unsupported::UnknownField {
        entity: ent.name.clone(),
        field: key.to_string(),
    })
}

/// Which entity a root field names, and whether it is the singular form.
fn resolve_root(schema: &Schema, name: &str) -> Result<(String, bool), Unsupported> {
    for e in &schema.entities {
        if graph_schema::lower_first(&e.name) == name {
            return Ok((e.name.clone(), true));
        }
        if graph_schema::plural(&e.name) == name {
            return Ok((e.name.clone(), false));
        }
    }
    Err(Unsupported::UnknownRoot(name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Schema {
        graph_schema::parse(
            r#"
type Pool @entity {
  id: ID!
  liquidity: BigInt!
  hooks: String!
  sender: Bytes!
  token0: Token!
  swaps: [Swap!]! @derivedFrom(field: "pool")
}
type Token @entity { id: ID! symbol: String! pools: [Pool!]! }
type Swap @entity { id: ID! pool: Pool! }
"#,
        )
        .unwrap()
    }

    fn one(q: &str) -> RootField {
        parse(q).expect("parse").into_iter().next().expect("a root")
    }

    #[test]
    fn a_plain_collection_gets_graph_nodes_defaults() {
        let c = compile(&schema(), &one("{ pools { id liquidity } }")).unwrap();
        // `first = 100`, `skip = 0` and ascending by id are graph-node's defaults, read off the
        // recorded reference. A client that omits them relies on them, and an unordered result with
        // `skip` would be a different page each call.
        assert_eq!(
            c.sql,
            r#"SELECT b."id", b."liquidity" FROM "pool" b ORDER BY b."id" ASC LIMIT 100 OFFSET 0"#
        );
        assert!(!c.singular);
        assert_eq!(c.entity, "Pool");
    }

    #[test]
    /// Every argument reaches the SQL.
    ///
    /// **Not an order test, despite what this used to be called.** Arguments and `where` conditions
    /// live in a `BTreeMap`, so they lower in name order; `hooks` before `liquidity_gt` happens to be
    /// both, which is why the old name went unchallenged. The order is immaterial - `AND` is
    /// commutative and the other arguments lower to distinct clauses - but a test should not claim
    /// something it cannot see.
    fn every_argument_reaches_the_sql() {
        let c = compile(
            &schema(),
            &one(
                r#"{ pools(first: 5, skip: 10, orderBy: liquidity, orderDirection: desc,
                        where: { hooks: "0xabc", liquidity_gt: 100 }) { id } }"#,
            ),
        )
        .unwrap();
        assert_eq!(
            c.sql,
            r#"SELECT b."id" FROM "pool" b WHERE b."hooks" = '0xabc' AND b."liquidity" > 100 ORDER BY b."liquidity" DESC LIMIT 5 OFFSET 10"#
        );
    }

    #[test]
    fn a_singular_root_filters_by_id_and_takes_one() {
        let c = compile(&schema(), &one(r#"{ pool(id: "0x1") { id hooks } }"#)).unwrap();
        assert!(c.singular);
        assert_eq!(
            c.sql,
            r#"SELECT b."id", b."hooks" FROM "pool" b WHERE b."id" = '0x1' LIMIT 1"#
        );
    }

    #[test]
    fn in_and_not_in_lower_to_sql_sets_and_an_empty_list_is_not_in_parens() {
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { id_in: ["a", "b"] }) { id } }"#),
        )
        .unwrap();
        assert!(c.sql.contains(r#""id" IN ('a', 'b')"#), "{}", c.sql);
        // `IN ()` is not valid SQL and an empty list matches nothing, so it lowers to a constant
        // rather than to a syntax error.
        let e = compile(&schema(), &one("{ pools(where: { id_in: [] }) { id } }")).unwrap();
        assert!(e.sql.contains("FALSE"), "{}", e.sql);
        let n = compile(
            &schema(),
            &one("{ pools(where: { id_not_in: [] }) { id } }"),
        )
        .unwrap();
        assert!(n.sql.contains("TRUE"), "{}", n.sql);
    }

    #[test]
    fn a_string_literal_with_a_quote_is_doubled_not_dropped() {
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { hooks: "o'brien" }) { id } }"#),
        )
        .unwrap();
        assert!(c.sql.contains("'o''brien'"), "{}", c.sql);
    }

    /// **The refusals, which are the module's contract.** Each of these is a wrong answer if
    /// silently ignored, so each must be an error a caller can read.
    /// A `@derivedFrom` list, aggregated rather than joined.
    ///
    /// A plain join would multiply the parent row once per child, so `first` would stop meaning what
    /// it says. One correlated subquery per parent keeps the parent's row count and the child's own
    /// page size separate.
    #[test]
    fn a_derived_list_aggregates_into_one_json_column() {
        let c = compile(&schema(), &one("{ pools { id swaps { id } } }"))
            .expect("a derived list lowers");
        // The join key is the schema author's: `@derivedFrom(field: "pool")` says `Swap.pool` holds
        // the parent id.
        assert!(
            c.sql.contains(r#"c1."pool" = b."id""#),
            "the back-reference comes from @derivedFrom: {}",
            c.sql
        );
        assert!(
            c.sql.contains(r#"struct_pack("id" := c1."id")"#),
            "{}",
            c.sql
        );
        // `list()` over zero rows is NULL in DuckDB, so a parent with no children would answer null
        // for a field the generated schema types `[Swap!]!`. Measured with the CLI, not assumed.
        assert!(
            c.sql.contains("coalesce(") && c.sql.contains("'[]'"),
            "a childless parent must answer [] rather than null: {}",
            c.sql
        );
        // The child's own ordering and page size, which the reference gives a derived field. Asserted
        // here and not only over HTTP: a mutation dropping this passed because the only test that
        // could see it was in a different test function than the mutation runner's filter named.
        assert!(
            c.sql.contains(r#"ORDER BY c1."id" ASC LIMIT 100) t)"#),
            "the child is ordered and paged inside the aggregate: {}",
            c.sql
        );
        // The parent's own paging is untouched by the aggregation.
        assert!(c.sql.ends_with("LIMIT 100 OFFSET 0"), "{}", c.sql);
        assert_eq!(
            c.shape,
            vec![
                Shape::Scalar {
                    key: "id".into(),
                    col: "id".into(),
                },
                Shape::List {
                    key: "swaps".into(),
                    col: "c1__swaps".into(),
                },
            ]
        );

        // A field the *child* has not got is named against the child.
        let e = compile(&schema(), &one("{ pools { swaps { nope } } }")).expect_err("unknown");
        assert!(
            matches!(&e, Unsupported::UnknownField { entity, field } if entity == "Swap" && field == "nope"),
            "{e:?}"
        );

        // A stored array of ids - `Token.whitelistPools` in the reference - is a different shape
        // needing `unnest`, so it is refused by name rather than aggregated as though it were derived.
        let e = compile(&schema(), &one("{ tokens { pools { id } } }"))
            .expect_err("a stored id array is not a derived list");
        assert!(
            matches!(&e, Unsupported::NestedSelection(n) if n == "pools"),
            "{e:?}"
        );
    }

    /// A to-one relation traversal, which is what the canonical Uniswap query is made of:
    /// `pools { token0 { symbol } }`. Lowered to one `LEFT JOIN`, never an N+1 walk.
    #[test]
    fn a_to_one_relation_lowers_to_a_left_join_and_a_nested_shape() {
        let c = compile(
            &schema(),
            &one("{ pools(first: 2) { id token0 { symbol } } }"),
        )
        .expect("a to-one traversal lowers");
        assert_eq!(
            c.sql,
            concat!(
                r#"SELECT b."id", j1."symbol" AS "j1__symbol" FROM "pool" b"#,
                r#" LEFT JOIN "token" j1 ON j1."id" = b."token0""#,
                r#" ORDER BY b."id" ASC LIMIT 2 OFFSET 0"#
            ),
            "the join is on the id the parent row already holds"
        );
        // LEFT rather than INNER: a reference whose target is missing must leave the parent in the
        // answer, as graph-node does, rather than silently drop it.
        assert!(c.sql.contains("LEFT JOIN"), "{}", c.sql);
        assert_eq!(
            c.shape,
            vec![
                Shape::Scalar {
                    key: "id".into(),
                    col: "id".into(),
                },
                Shape::Object {
                    key: "token0".into(),
                    fields: vec![("symbol".into(), "j1__symbol".into())],
                },
            ],
            "only the shape knows `j1__symbol` belongs under `token0`"
        );

        // A derived list is not joined at all - joining it would multiply the parent row per child
        // and `first` would stop meaning what it says - it is aggregated, and the `LEFT JOIN` above
        // must not appear for one.
        let c = compile(&schema(), &one("{ pools { swaps { id } } }")).expect("aggregated");
        assert!(
            !c.sql.contains("LEFT JOIN"),
            "a derived list is aggregated, not joined: {}",
            c.sql
        );

        // One level only, matching the depth `E_orderBy` advertises in the reference. Refused at
        // lowering rather than while parsing, because an introspection query is far deeper and the
        // handler has to be able to see its root field name.
        let e = compile(&schema(), &one("{ pools { token0 { pool { id } } } }"))
            .expect_err("two levels");
        assert!(
            matches!(&e, Unsupported::NestedSelection(n) if n == "pool"),
            "{e:?}"
        );
        // But it does parse, so `__schema { types { fields { … } } }` can reach the handler.
        assert!(parse("{ pools { token0 { pool { id } } } }").is_ok());

        // Arguments on a traversed field need the same join plus its own LIMIT; dropping `first`
        // there would return every related row. Also refused at lowering - a real introspection
        // query writes `fields(includeDeprecated: true)`.
        let e = compile(&schema(), &one("{ pools { swaps(first: 5) { id } } }"))
            .expect_err("nested arguments");
        assert!(
            matches!(&e, Unsupported::NestedSelection(n) if n == "swaps"),
            "{e:?}"
        );
        // And arguments on a leaf *inside* a traversal, which is a different guard: the case above
        // is caught before the relation is resolved, so removing this one changed nothing and no
        // test noticed - found by mutation, not by reading.
        let e = compile(&schema(), &one(r#"{ pools { token0 { symbol(x: 1) } } }"#))
            .expect_err("arguments on a traversed leaf");
        assert!(
            matches!(&e, Unsupported::NestedSelection(n) if n == "symbol"),
            "{e:?}"
        );

        // An unknown field on the *target* entity is named against the target, not the parent.
        let e = compile(&schema(), &one("{ pools { token0 { nope } } }")).expect_err("unknown");
        assert!(
            matches!(&e, Unsupported::UnknownField { entity, field } if entity == "Token" && field == "nope"),
            "{e:?}"
        );

        // `first` and `skip` must be integers. Falling through to the default meant `first: "1"`
        // silently became `LIMIT 100` - a supplied argument replaced by a guess, which is the one
        // thing this module says it never does (Jules on #1282).
        for (q, which) in [
            (r#"{ pools(first: "1") { id } }"#, "first"),
            (r#"{ pools(skip: "10") { id } }"#, "skip"),
            ("{ pools(first: true) { id } }", "first"),
        ] {
            let e = compile(&schema(), &one(q)).expect_err(q);
            assert!(
                matches!(&e, Unsupported::Argument(a) if a.contains(which)),
                "{q}: {e:?}"
            );
        }
        // And a variable carrying the wrong type is refused the same way, since that is how a client
        // actually sends one.
        let roots = parse_with(
            "query P($n: Int!) { pools(first: $n) { id } }",
            &BTreeMap::from([("n".to_string(), Value::Str("1".into()))]),
        )
        .unwrap();
        assert!(compile(&schema(), &roots[0]).is_err());

        // A composite root with no selection set is not a legal query, and answering `*` for one
        // would invent a field list the caller never asked for.
        assert!(compile(&schema(), &one("{ pools }")).is_err());

        // Nor is a composite *field* without one. Lowering `{ pools { token0 } }` to
        // `SELECT b."token0"` returned the relation's id under a field the schema declares as
        // `Token!`, so the endpoint answered a query a client's own validator would have refused, in
        // a shape the advertised schema does not describe.
        let e =
            compile(&schema(), &one("{ pools { token0 } }")).expect_err("needs a selection set");
        assert!(
            matches!(&e, Unsupported::Syntax(m) if m.contains("Pool.token0") && m.contains("Token")),
            "{e:?}"
        );
        // A derived list is the same: composite, so it needs one too.
        assert!(compile(&schema(), &one("{ pools { swaps } }")).is_err());
    }

    impl Selection {
        fn sel_names(&self) -> Vec<&str> {
            self.sub.iter().map(|s| s.name.as_str()).collect()
        }
    }

    fn leaf(name: &str) -> Selection {
        Selection {
            key: name.into(),
            name: name.into(),
            args: BTreeMap::new(),
            sub: vec![],
        }
    }

    /// Text operators lower to `LIKE`, and the caller's own wildcards stay data.
    #[test]
    fn text_operators_lower_to_like_with_the_callers_wildcards_escaped() {
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { hooks_contains: "ab" }) { id } }"#),
        )
        .unwrap();
        assert!(
            c.sql.contains(r#"b."hooks" LIKE '%ab%' ESCAPE '\'"#),
            "{}",
            c.sql
        );

        // The whole reason this needs its own slice: `%` and `_` are wildcards in `LIKE` and data in
        // a filter, so `50%` must match a literal `50%` and not everything starting `50`.
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { hooks_starts_with: "50%_x" }) { id } }"#),
        )
        .unwrap();
        assert!(
            c.sql.contains(r#"b."hooks" LIKE '50\%\_x%' ESCAPE '\'"#),
            "the caller's own wildcards are escaped, the trailing one is ours: {}",
            c.sql
        );

        // A quote still closes the literal correctly after escaping.
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { hooks_ends_with: "o'clock" }) { id } }"#),
        )
        .unwrap();
        assert!(c.sql.contains(r#"'%o''clock'"#), "{}", c.sql);

        // Negation and case-insensitivity are the four combinations of two flags.
        for (op, sql) in [
            ("hooks_not_contains", "NOT LIKE '%a%'"),
            ("hooks_contains_nocase", "ILIKE '%a%'"),
            ("hooks_not_contains_nocase", "NOT ILIKE '%a%'"),
        ] {
            let q = format!(r#"{{ pools(where: {{ {op}: "a" }}) {{ id }} }}"#);
            let c = compile(&schema(), &one(&q)).unwrap();
            assert!(c.sql.contains(sql), "{op}: {}", c.sql);
        }
    }

    /// `and` / `or`, and the bracketing that keeps `or` from rebinding the rest of the `WHERE`.
    #[test]
    fn and_or_lower_to_a_bracketed_boolean_tree() {
        let c = compile(
            &schema(),
            &one(
                r#"{ pools(where: { liquidity_gt: "1", or: [{ id: "a" }, { hooks: "b" }] }) { id } }"#,
            ),
        )
        .unwrap();
        // `liquidity > '1' AND a OR b` would bind as `(liquidity > '1' AND a) OR b`, which answers a
        // different question, so the whole tree is parenthesised.
        assert!(
            c.sql.contains(r#"((b."id" = 'a') OR (b."hooks" = 'b'))"#),
            "{}",
            c.sql
        );
        assert!(c.sql.contains(r#"b."liquidity" > '1'"#), "{}", c.sql);

        // Conditions inside one filter object are ANDed, as `where` itself is.
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { and: [{ id: "a", hooks: "b" }] }) { id } }"#),
        )
        .unwrap();
        // Name order, not written order: a filter object is a `BTreeMap`. Immaterial here because
        // `AND` is commutative, but asserted as it actually is.
        assert!(
            c.sql.contains(r#"((b."hooks" = 'b' AND b."id" = 'a'))"#),
            "{}",
            c.sql
        );

        // And they nest.
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { or: [{ and: [{ id: "a" }] }] }) { id } }"#),
        )
        .unwrap();
        assert!(c.sql.contains(r#"(((b."id" = 'a')))"#), "{}", c.sql);
    }

    /// The introspection document a generated client actually sends is built from fragments.
    #[test]
    fn a_clients_fragment_based_introspection_query_parses() {
        // Shortened, but the shape is the standard one: a named fragment on `__Type`, spread from a
        // field selection, and a fragment defined *after* the operation that uses it.
        let q = r#"
            query IntrospectionQuery {
              __schema {
                queryType { name }
                types { ...FullType }
              }
            }
            fragment FullType on __Type {
              kind
              name
              fields(includeDeprecated: true) { name ...TypeRef }
            }
            fragment TypeRef on __Type { kind name }
        "#;
        let roots = parse(q).expect("a client introspection document parses");
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name, "__schema", "the handler routes on this name");
        // The spread was spliced, not dropped: `types` carries the fragment's fields.
        let types = roots[0]
            .sel
            .iter()
            .find(|s| s.name == "types")
            .expect("types survived");
        let names: Vec<&str> = types.sel_names();
        assert_eq!(
            names,
            ["kind", "name", "fields"],
            "the fragment's fields are spliced in place"
        );
        // And the nested spread inside the fragment resolved too.
        let fields = types.sub.iter().find(|s| s.name == "fields").unwrap();
        assert_eq!(
            fields.sel_names(),
            ["name", "kind", "name"],
            "a fragment spread inside a fragment resolves"
        );
        // An inline fragment splices the same way.
        let roots = parse("{ __schema { types { ... on __Type { kind } } } }").unwrap();
        let types = &roots[0].sel[0];
        assert_eq!(types.sel_names(), ["kind"]);
    }

    /// Aliases: the response key and the schema field name are different strings.
    ///
    /// `{ p: pools { id } }` is ordinary client-generated GraphQL. Reading one identifier for both
    /// rejected it outright, and using the alias as the field name would have looked up the wrong root
    /// and the wrong column (Jules on #1282).
    #[test]
    fn an_alias_answers_under_its_own_key_while_looking_up_the_real_field() {
        let roots = parse(r#"{ p: pools { n: id liquidity } }"#).expect("an alias parses");
        assert_eq!(roots[0].key, "p");
        assert_eq!(
            roots[0].name, "pools",
            "the schema lookup uses the real name"
        );

        let c = compile(&schema(), &roots[0]).expect("and lowers");
        // The column is the real field; the alias only names the column so two aliases on one field
        // cannot collide in the row.
        assert_eq!(
            c.sql,
            r#"SELECT b."id" AS "a0", b."liquidity" FROM "pool" b ORDER BY b."id" ASC LIMIT 100 OFFSET 0"#,
            "{}",
            c.sql
        );
        assert_eq!(
            c.shape,
            vec![
                Shape::Scalar {
                    key: "n".into(),
                    col: "a0".into(),
                },
                Shape::Scalar {
                    key: "liquidity".into(),
                    col: "liquidity".into(),
                },
            ]
        );

        // The same field twice under two aliases must produce two columns, or one would overwrite the
        // other in the row and the caller would see the same value under both keys.
        let c = compile(&schema(), &one(r#"{ pools { a: id b: id } }"#)).unwrap();
        assert_eq!(
            c.sql,
            r#"SELECT b."id" AS "a0", b."id" AS "a1" FROM "pool" b ORDER BY b."id" ASC LIMIT 100 OFFSET 0"#,
            "{}",
            c.sql
        );

        // An alias on a traversal, and on a field inside one.
        let c = compile(&schema(), &one(r#"{ pools { t: token0 { s: symbol } } }"#)).unwrap();
        assert_eq!(
            c.shape,
            vec![Shape::Object {
                key: "t".into(),
                fields: vec![("s".into(), "j0__symbol".into())],
            }],
            "the join is still on token0, the answer is still under t"
        );
        assert!(c.sql.contains(r#"= b."token0""#), "{}", c.sql);

        // And on a derived list.
        let c = compile(&schema(), &one(r#"{ pools { s: swaps { id } } }"#)).unwrap();
        assert!(
            matches!(&c.shape[0], Shape::List { key, .. } if key == "s"),
            "{:?}",
            c.shape
        );
        assert!(c.sql.contains(r#"c0."pool" = b."id""#), "{}", c.sql);

        // An alias naming a field the entity has not got is still refused against the real name.
        let e = compile(&schema(), &one(r#"{ pools { x: nope } }"#)).expect_err("unknown");
        assert!(
            matches!(&e, Unsupported::UnknownField { field, .. } if field == "nope"),
            "{e:?}"
        );
    }

    #[test]
    fn a_generated_clients_operation_binds_its_variables() {
        // The shape an Apollo- or graph-client-generated query actually has: a named operation, a
        // variable definition list, one variable with a default, and `$name` in argument position.
        // Nothing in the earlier tests looks like this, and a compiler that only handles the
        // hand-written form handles no real client at all.
        let q = r#"
            query Pools($first: Int!, $min: BigInt = "5") {
              pools(first: $first, where: { liquidity_gt: $min }) { id }
            }
        "#;
        let vars = BTreeMap::from([("first".to_string(), Value::Int(3))]);
        let roots = parse_with(q, &vars).expect("a client operation parses");
        let c = compile(&schema(), &roots[0]).expect("and lowers");
        assert_eq!(
            c.sql,
            r#"SELECT b."id" FROM "pool" b WHERE b."liquidity" > '5' ORDER BY b."id" ASC LIMIT 3 OFFSET 0"#,
            "the supplied variable and the header default both reach the SQL"
        );

        // An unbound variable is refused by name. Silently dropping it would widen the filter, and
        // widening a filter is the one failure this compiler exists to prevent.
        let e = parse_with(q, &BTreeMap::new()).expect_err("no value for $first");
        assert!(
            matches!(&e, Unsupported::UnboundVariable(v) if v == "first"),
            "{e:?}"
        );

        // A default that is itself an object: the old header skip ran to the first `{` and landed
        // inside this one, so the whole operation read as garbage.
        let q = r#"query P($w: Pool_filter = { id: "0xaaa" }) { pools(where: $w) { id } }"#;
        let roots = parse_with(q, &BTreeMap::new()).expect("an object default parses");
        let c = compile(&schema(), &roots[0]).expect("and lowers");
        assert!(
            c.sql.contains(r#""id" = '0xaaa'"#),
            "the object default reached the predicate: {}",
            c.sql
        );

        // A nest has no mappings, so it has nothing to mutate and nothing to stream.
        for kw in ["mutation", "subscription"] {
            let e = parse(&format!("{kw} M {{ pools {{ id }} }}")).expect_err("{kw} is refused");
            assert!(
                matches!(&e, Unsupported::NotAQuery(k) if k == kw),
                "{kw}: {e:?}"
            );
        }
    }

    #[test]
    fn a_sub_selection_is_recorded_and_the_fields_after_it_still_parse() {
        // `_meta { block { number } hasIndexingErrors }` is a legitimate nested selection the
        // handler answers itself, so nesting is recorded rather than refused while parsing. Reading
        // a sub-selection must leave the cursor after its closing brace and no further, or every
        // field written after a nested one is silently dropped - which is what this pins.
        let q = "{ pools { id swaps { id } hooks } }";
        let roots = parse(q).expect("nesting parses");
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0].sel,
            vec![
                leaf("id"),
                Selection {
                    key: "swaps".into(),
                    name: "swaps".into(),
                    args: BTreeMap::new(),
                    sub: vec![leaf("id")],
                },
                leaf("hooks"),
            ],
            "the sub-selection is recorded and the field after it is not dropped"
        );

        // And it lowers: a derived list is aggregated rather than joined, so the sub-selection this
        // test recorded is the child's field list. See
        // `a_derived_list_aggregates_into_one_json_column`.
        assert!(compile(&schema(), &roots[0]).is_ok());
    }

    #[test]
    fn everything_unlowerable_is_refused_by_name() {
        let s = schema();
        /// A query and the predicate its refusal must satisfy. Named because clippy is right that
        /// the tuple is unreadable inline.
        type Case = (&'static str, fn(&Unsupported) -> bool);
        let cases: Vec<Case> = vec![
            // A dropped filter returns more rows than were asked for. `_contains` and `or` are
            // lowered now, so the refusals that remain are the operators the *schema* does not
            // declare for that field's type: `liquidity` is a `BigInt` and has no text operators.
            (
                r#"{ pools(where: { liquidity_contains: "ab" }) { id } }"#,
                |e| matches!(e, Unsupported::Operator(o) if o == "liquidity_contains"),
            ),
            // `Bytes` carries ten operators in the reference and `String` eighteen: no prefix forms
            // and no `_nocase` anywhere. Accepting one on `sender` would answer a query a client's
            // own validator, built from the schema we advertise, would have refused.
            (
                r#"{ pools(where: { sender_starts_with: "0xab" }) { id } }"#,
                |e| matches!(e, Unsupported::Operator(o) if o == "sender_starts_with"),
            ),
            (
                r#"{ pools(where: { sender_contains_nocase: "0xab" }) { id } }"#,
                |e| matches!(e, Unsupported::Operator(o) if o == "sender_contains_nocase"),
            ),
            // And an empty condition list is refused rather than given a meaning this slice has not
            // measured against graph-node.
            (
                r#"{ pools(where: { or: [] }) { id } }"#,
                |e| matches!(e, Unsupported::Argument(a) if a.contains("or")),
            ),
            // A silently-ignored `block:` answers as of head while claiming a past block.
            ("{ pools(block: { number: 1 }) { id } }", |e| {
                matches!(e, Unsupported::TimeTravel)
            }),
            (
                "{ pools { nope } }",
                |e| matches!(e, Unsupported::UnknownField { field, .. } if field == "nope"),
            ),
            (
                "{ nope { id } }",
                |e| matches!(e, Unsupported::UnknownRoot(n) if n == "nope"),
            ),
            ("{ pools(first: 5000) { id } }", |e| {
                matches!(e, Unsupported::Argument(_))
            }),
            ("{ pools(orderBy: token0__symbol) { id } }", |e| {
                matches!(e, Unsupported::Argument(_))
            }),
            (
                r#"{ pools(where: { nope: "x" }) { id } }"#,
                |e| matches!(e, Unsupported::UnknownField { field, .. } if field == "nope"),
            ),
        ];
        for (q, want) in cases {
            // A refusal may come from either stage: a selection too deep to traverse is caught
            // while parsing, an unknown field while compiling. What matters is that it is refused
            // and named.
            let got = match parse(q) {
                Err(e) => e,
                Ok(roots) => compile(&s, &roots[0]).expect_err(q),
            };
            assert!(
                want(&got),
                "{q}: refused as {got:?}, which is not the reason"
            );
        }
        // An unsupplied variable is a refusal at parse time rather than compile time, and it is
        // refused *as* an unbound variable: this assertion used to mean "variables are not
        // implemented" and would otherwise have gone on passing for a different reason entirely.
        let e = parse("query ($n: Int) { pools(first: $n) { id } }").expect_err("no $n");
        assert!(
            matches!(&e, Unsupported::UnboundVariable(v) if v == "n"),
            "{e:?}"
        );
        // And so is a fragment, because resolving a spread needs the definition.
        // A spread with no definition is refused by name - it is not silently nothing, which would
        // drop every field the fragment was carrying.
        let e = parse("{ pools { ...F } }").expect_err("undefined fragment");
        assert!(
            matches!(&e, Unsupported::Syntax(m) if m.contains("`F`") && m.contains("never defined")),
            "{e:?}"
        );
        // And one that spreads itself is an error rather than a stack overflow.
        assert!(parse("fragment F on Pool { ...F } { pools { ...F } }").is_err());
    }

    /// `_not_in` must not be read as `_not`, which would compare against a list and produce a
    /// syntactically valid query with the wrong meaning - the worst failure available here.
    #[test]
    fn the_longest_suffix_wins() {
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { id_not_in: ["a"] }) { id } }"#),
        )
        .unwrap();
        assert!(c.sql.contains(r#""id" NOT IN ('a')"#), "{}", c.sql);
        let n = compile(
            &schema(),
            &one(r#"{ pools(where: { id_not: "a" }) { id } }"#),
        )
        .unwrap();
        assert!(n.sql.contains(r#""id" <> 'a'"#), "{}", n.sql);
    }

    #[test]
    fn subgraph_error_is_accepted_and_ignored() {
        // It selects an error policy, and a nest has no subgraph indexing errors either way. It must
        // not be refused: every generated client sends it, because it is a defaulted argument.
        let c = compile(&schema(), &one("{ pools(subgraphError: deny) { id } }")).unwrap();
        assert!(c.sql.contains("FROM \"pool\""), "{}", c.sql);
    }
}
