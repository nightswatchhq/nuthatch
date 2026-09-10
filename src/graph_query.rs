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
    /// As written: `pools`, `pool`, `_meta`.
    pub name: String,
    pub args: BTreeMap<String, Value>,
    /// Leaf field names, in the order the caller asked for them.
    pub fields: Vec<String>,
    /// Selected fields that carried a sub-selection.
    ///
    /// **Recorded here rather than refused during the parse.** `_meta { block { number } }` is a
    /// legitimate nested selection that the handler answers itself, and refusing nesting while
    /// parsing made `_meta` unaskable - found by the HTTP test. Parse what is there; refuse at
    /// lowering, where the decision belongs.
    pub nested: Vec<String>,
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
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
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
    if query.contains("fragment ") || query.contains("...") {
        return Err(Unsupported::Syntax(
            "fragments are not implemented yet".into(),
        ));
    }
    let b = query.as_bytes();
    let mut c = Cursor {
        b,
        i: 0,
        vars,
        defaults: BTreeMap::new(),
    };
    c.operation_header()?;
    if c.peek() != Some(b'{') {
        return Err(Unsupported::Syntax("no selection set".into()));
    }
    c.i += 1;
    let mut out = Vec::new();
    loop {
        c.trivia();
        match c.peek() {
            None => return Err(Unsupported::Syntax("unclosed selection set".into())),
            // The closing brace of the operation's selection set: nothing follows it, so the
            // cursor is not advanced.
            Some(b'}') => return Ok(out),
            _ => {}
        }
        let name = c.ident()?;
        c.trivia();
        let args = if c.peek() == Some(b'(') {
            c.args()?
        } else {
            BTreeMap::new()
        };
        c.trivia();
        let mut fields = Vec::new();
        let mut nested = Vec::new();
        if c.peek() == Some(b'{') {
            c.i += 1;
            loop {
                c.trivia();
                match c.peek() {
                    None => return Err(Unsupported::Syntax("unclosed inner set".into())),
                    Some(b'}') => {
                        c.i += 1;
                        break;
                    }
                    _ => {}
                }
                let f = c.ident()?;
                c.trivia();
                // A nested set under a selected field is a relation traversal.
                if c.peek() == Some(b'(') {
                    let _ = c.args()?;
                    c.trivia();
                }
                if c.peek() == Some(b'{') {
                    // Consume the sub-selection and note the field carried one.
                    let mut depth = 0usize;
                    while let Some(ch) = c.peek() {
                        match ch {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    c.i += 1;
                                    break;
                                }
                            }
                            _ => {}
                        }
                        c.i += 1;
                    }
                    nested.push(f);
                    continue;
                }
                fields.push(f);
            }
        }
        out.push(RootField {
            name,
            args,
            fields,
            nested,
        });
    }
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
    /// Consume an optional operation header, leaving the cursor on the selection set's `{`.
    ///
    /// **Parsed rather than skipped to the first brace.** A variable definition may carry a default
    /// that is itself an object - `$w: Pool_filter = { id: "a" }` - and skipping to the first `{`
    /// lands inside it, so the whole operation then reads as garbage.
    fn operation_header(&mut self) -> Result<(), Unsupported> {
        self.trivia();
        if self.peek() == Some(b'{') {
            return Ok(()); // anonymous shorthand
        }
        let kw = self.ident()?;
        match kw.as_str() {
            "query" => {}
            other @ ("mutation" | "subscription") => {
                return Err(Unsupported::NotAQuery(other.to_string()))
            }
            other => {
                return Err(Unsupported::Syntax(format!(
                    "expected an operation, found `{other}`"
                )))
            }
        }
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

/// A compiled query: the SQL, and the fields the caller asked for in their order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    pub sql: String,
    pub fields: Vec<String>,
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

    if let Some(n) = root.nested.first() {
        return Err(Unsupported::NestedSelection(n.clone()));
    }
    for f in &root.fields {
        if !ent.fields.iter().any(|x| &x.name == f) {
            return Err(Unsupported::UnknownField {
                entity: entity.clone(),
                field: f.clone(),
            });
        }
    }
    let select = if root.fields.is_empty() {
        "*".to_string()
    } else {
        root.fields
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let view = crate::subgraph_import::to_alias(&entity);
    let mut sql = format!("SELECT {select} FROM \"{view}\"");
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
                wheres.push(format!("\"id\" = {lit}"));
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
        sql.push_str(&format!(" ORDER BY \"{order_field}\" {dir}"));

        // graph-node's defaults, taken from the recorded reference: first = 100, skip = 0, and
        // §Query semantics caps first at 1000.
        let first = root
            .args
            .get("first")
            .and_then(Value::as_i64)
            .unwrap_or(100);
        if !(0..=1000).contains(&first) {
            return Err(Unsupported::Argument(format!(
                "first must be between 0 and 1000, got {first}"
            )));
        }
        let skip = root.args.get("skip").and_then(Value::as_i64).unwrap_or(0);
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
        fields: root.fields.clone(),
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
        return Err(Unsupported::Operator(key.to_string()));
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
        if !ent.fields.iter().any(|x| x.name == field) {
            continue;
        }
        let col = format!("\"{field}\"");
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
  token0: Token!
  swaps: [Swap!]! @derivedFrom(field: "pool")
}
type Token @entity { id: ID! symbol: String! }
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
            r#"SELECT "id", "liquidity" FROM "pool" ORDER BY "id" ASC LIMIT 100 OFFSET 0"#
        );
        assert!(!c.singular);
        assert_eq!(c.entity, "Pool");
    }

    #[test]
    fn arguments_lower_in_the_order_a_caller_wrote_them() {
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
            r#"SELECT "id" FROM "pool" WHERE "hooks" = '0xabc' AND "liquidity" > 100 ORDER BY "liquidity" DESC LIMIT 5 OFFSET 10"#
        );
    }

    #[test]
    fn a_singular_root_filters_by_id_and_takes_one() {
        let c = compile(&schema(), &one(r#"{ pool(id: "0x1") { id hooks } }"#)).unwrap();
        assert!(c.singular);
        assert_eq!(
            c.sql,
            r#"SELECT "id", "hooks" FROM "pool" WHERE "id" = '0x1' LIMIT 1"#
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
            r#"SELECT "id" FROM "pool" WHERE "liquidity" > '5' ORDER BY "id" ASC LIMIT 3 OFFSET 0"#,
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
        // handler answers itself, so the parser records nesting rather than refusing it. Skipping
        // the sub-selection must leave the cursor after its closing brace and no further, or every
        // field written after a nested one is silently dropped.
        let q = "{ pools { id swaps { id transaction { id } } createdAtTimestamp } }";
        let roots = parse(q).expect("nesting parses");
        assert_eq!(roots.len(), 1);
        assert_eq!(
            roots[0].fields,
            vec!["id".to_string(), "createdAtTimestamp".to_string()],
            "the field after a nested selection is not dropped"
        );
        assert_eq!(
            roots[0].nested,
            vec!["swaps".to_string()],
            "the nested field is recorded, not refused"
        );

        // And it is still refused, one layer later, when it reaches an entity root.
        let e = compile(&schema(), &roots[0]).expect_err("an entity root refuses nesting");
        assert!(
            matches!(&e, Unsupported::NestedSelection(n) if n == "swaps"),
            "{e:?}"
        );
    }

    #[test]
    fn everything_unlowerable_is_refused_by_name() {
        let s = schema();
        /// A query and the predicate its refusal must satisfy. Named because clippy is right that
        /// the tuple is unreadable inline.
        type Case = (&'static str, fn(&Unsupported) -> bool);
        let cases: Vec<Case> = vec![
            // A dropped filter returns more rows than were asked for.
            (
                r#"{ pools(where: { hooks_contains: "ab" }) { id } }"#,
                |e| matches!(e, Unsupported::Operator(o) if o == "hooks_contains"),
            ),
            (
                r#"{ pools(where: { or: [{ id: "a" }] }) { id } }"#,
                |e| matches!(e, Unsupported::Operator(o) if o == "or"),
            ),
            // A silently-ignored `block:` answers as of head while claiming a past block.
            ("{ pools(block: { number: 1 }) { id } }", |e| {
                matches!(e, Unsupported::TimeTravel)
            }),
            (
                "{ pools { swaps { id } } }",
                |e| matches!(e, Unsupported::NestedSelection(n) if n == "swaps"),
            ),
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
            // A refusal may come from either stage: a nested selection is caught while parsing,
            // an unknown field while compiling. What matters is that it is refused and named.
            let got = match parse(q) {
                Err(e) => e,
                Ok(roots) => compile(&s, &roots[0]).expect_err(q),
            };
            assert!(
                want(&got),
                "{q}: refused as {got:?}, which is not the reason"
            );
        }
        // A variable is a refusal at parse time rather than compile time.
        assert!(parse("query ($n: Int) { pools(first: $n) { id } }").is_err());
        // And so is a fragment, because resolving a spread needs the definition.
        assert!(parse("{ pools { ...F } }").is_err());
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
