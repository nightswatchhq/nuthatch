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
    /// The literal `null`, or a `variables` entry that is JSON null.
    ///
    /// Carried rather than dropped, and refused rather than lowered. It used to fall through the enum
    /// branch of the parser, so `where: { hooks: null }` compiled to `hooks = 'null'` and matched rows
    /// whose `hooks` is the four-character string - a filter that quietly selects the wrong rows, which
    /// is the one thing this module may not do. From the variables side it was dropped by `filter_map`
    /// and re-surfaced as "unbound variable", which is a refusal for the wrong reason (Jules, #1282).
    Null,
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
            serde_json::Value::Null => Value::Null,
        })
    }
    /// The SQL literal for this value. Strings are single-quoted with internal quotes doubled; there
    /// is no other escaping because there is no other type that reaches a literal position.
    fn sql_literal(&self) -> Option<String> {
        Some(match self {
            Value::Int(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Str(s) | Value::Enum(s) => format!("'{}'", s.replace('\'', "''")),
            // SQL `NULL` is not what GraphQL null means in a filter, and guessing which it meant is
            // exactly the approximation this module refuses. The caller names it instead.
            Value::Null | Value::List(_) | Value::Object(_) => return None,
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
    /// A required argument the client did not supply. Distinct from `Argument`, which means "this
    /// compiler does not implement it yet": a missing `id` on a singular root is the client's error
    /// and graph-node rejects it rather than answering.
    MissingArgument(String),
    /// A `where` operator this compiler does not implement. Named so the caller knows which.
    Operator(String),
    /// `block:` needs a block-ranged entity store, which the nest has not got (#1267).
    TimeTravel,
    /// A `$name` the operation used but neither the request nor a header default supplies.
    UnboundVariable(String),
    /// A `mutation` or `subscription`. A nest has neither.
    NotAQuery(String),
    /// Several operations in one document and no `operationName` saying which to run.
    OperationNameRequired,
    /// An `operationName` no operation in the document carries.
    OperationNotFound(String),
    /// A `null` where this dialect has no lowering for one. Named rather than guessed at: in a filter it
    /// could mean `IS NULL`, or the absence of the condition, and the two select different rows.
    NullValue(String),
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // graph-node's own wording, verbatim: `OperationNameRequired => write!(f, "Operation name
            // required")` and `OperationNotFound(s) => write!(f, "Operation name not found `{}`", s)` in
            // graph/src/data/query/error.rs:158. Both are attestable there, so both are a deterministic
            // refusal rather than a transient one, and a client that matches on the text still matches.
            Unsupported::OperationNameRequired => write!(f, "Operation name required"),
            Unsupported::OperationNotFound(n) => write!(f, "Operation name not found `{n}`"),
            Unsupported::NullValue(w) => write!(
                f,
                "`{w}` was given `null`, which this dialect does not lower yet - in a filter it could \
                 mean `IS NULL` or no condition at all, and those select different rows"
            ),
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
            // graph-node's own wording, probed against the live reference on 2026-09-11:
            // `{ token { symbol } }` answers `No value provided for required argument: `id``.
            // A client that surfaces this string should see the same one.
            Unsupported::MissingArgument(a) => {
                write!(f, "No value provided for required argument: `{a}`")
            }
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
    parse_named(query, vars, None)
}

/// The same, selecting among several operations by the request's `operationName`.
///
/// A document may legally carry more than one operation, and every generated client sends
/// `operationName` alongside the document whether or not it needs to - Apollo and urql both do, on
/// every request. Refusing the whole document was a refusal of requests graph-node answers (Jules,
/// #1282), so the selection follows it: one operation and no name runs that one, several and no name is
/// `Operation name required`, and a name nothing carries is `Operation name not found`.
pub fn parse_named(
    query: &str,
    vars: &BTreeMap<String, Value>,
    operation_name: Option<&str>,
) -> Result<Vec<RootField>, Unsupported> {
    let b = query.as_bytes();
    let mut c = Cursor {
        b,
        s: query,
        i: 0,
        vars,
        defaults: BTreeMap::new(),
    };
    // A document is a list of definitions in any order, so fragments are collected as they are met
    // and spreads are resolved once the whole document has been read - a fragment may legally be
    // defined after the operation that uses it.
    let mut fragments: BTreeMap<String, Vec<Sel>> = BTreeMap::new();
    // Every operation, in document order, each with the name it declared. The shorthand `{ … }` and a
    // `query` with no name are both anonymous.
    let mut operations: Vec<(Option<String>, Vec<Sel>)> = Vec::new();
    loop {
        c.trivia();
        if c.peek().is_none() {
            break;
        }
        if c.peek() == Some(b'{') {
            // The anonymous shorthand operation.
            operations.push((None, c.selection_set()?));
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
                let name = c.operation_tail()?;
                if c.peek() != Some(b'{') {
                    return Err(Unsupported::Syntax("no selection set".into()));
                }
                operations.push((name, c.selection_set()?));
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
    let operation = match operation_name {
        // A name the client asked for, matched exactly. An anonymous operation carries no name, so it is
        // never what `operationName` selects, even when it is the only one in the document.
        Some(want) => operations
            .into_iter()
            .find(|(n, _)| n.as_deref() == Some(want))
            .map(|(_, sel)| sel)
            .ok_or_else(|| Unsupported::OperationNotFound(want.to_string()))?,
        None => match operations.len() {
            0 => return Err(Unsupported::Syntax("no selection set".into())),
            1 => operations.pop().expect("one operation").1,
            _ => return Err(Unsupported::OperationNameRequired),
        },
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

/// Whether graph-node sends this scalar as a GraphQL string rather than in its storage type.
///
/// From `graph/src/data/store/mod.rs:554`, where every stored value is mapped to `q::Value`:
/// `Int` is the one numeric scalar that stays a number (`q::Value::Int`), while `Int8`, `Timestamp`,
/// `BigInt` and `BigDecimal` all become `q::Value::String`. `Bytes` is already text in a nest's
/// decoded tables and `Boolean` is a boolean on both sides, so neither needs a cast.
///
/// `Timestamp` is **not** here. graph-node sends microseconds since the epoch, which is a unit
/// conversion rather than a cast, and a nest's column is in whatever unit its view put there; guessing
/// would turn a known unit into a wrong number. It is named in the dialect doc instead.
fn wire_string_cast(ty: &graph_schema::FieldType) -> bool {
    match ty {
        graph_schema::FieldType::Scalar(n) => {
            matches!(n.as_str(), "BigInt" | "BigDecimal" | "Int8")
        }
        _ => false,
    }
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
    /// The same text as `b`. Held so a string value can take a whole code point at a byte offset
    /// without a length dispatch whose error arms no test could ever reach.
    s: &'a str,
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
    /// A GraphQL string value, **decoded**.
    ///
    /// The previous version skipped over an escape and then took the raw byte slice, so `"a\"b"`
    /// reached `sql_literal` as `a\"b` and `\n` as a backslash and an `n`. `sql_literal` only doubles
    /// single quotes, so the generated `WHERE` compared against a value the client never wrote: a
    /// wrong answer with no error, which is the one failure shape this endpoint must not have. It
    /// also advanced twice for a trailing backslash, walking `self.i` past the end of the input, and
    /// the slice that followed panicked in the request handler.
    ///
    /// `self.s` is the query text, so a byte offset the scanner has reached begins a whole code
    /// point and the non-escape branch can simply take it.
    fn string(&mut self) -> Result<String, Unsupported> {
        if self.b[self.i..].starts_with(br#"""""#) {
            // Refused rather than decoded: the dedent rules are a separate piece of work (#1288).
            // The point of refusing is that the old path read `"""x"""` as an empty string and
            // carried on, and a visible refusal beats a silent substitution every time.
            return Err(Unsupported::Syntax(
                "a block string (`\"\"\"`) as an argument value".into(),
            ));
        }
        self.i += 1; // the opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(Unsupported::Syntax("unterminated string".into()));
            };
            self.i += 1;
            match c {
                b'"' => return Ok(out),
                // A single-quoted string may not span lines; that is what `"""` is for.
                b'\n' | b'\r' => {
                    return Err(Unsupported::Syntax("a line break inside a string".into()))
                }
                b'\\' => {
                    let Some(e) = self.peek() else {
                        return Err(Unsupported::Syntax("unterminated string".into()));
                    };
                    self.i += 1;
                    out.push(match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => self.unicode_escape()?,
                        other => {
                            return Err(Unsupported::Syntax(format!(
                                "`\\{}` is not a GraphQL string escape",
                                other as char
                            )))
                        }
                    });
                }
                _ => {
                    let start = self.i - 1;
                    let ch = self.s[start..]
                        .chars()
                        .next()
                        .expect("a byte was read at `start`, so a code point begins there");
                    self.i = start + ch.len_utf8();
                    out.push(ch);
                }
            }
        }
    }

    /// `\\uXXXX`, including the surrogate pair that carries anything above the BMP. A lone surrogate
    /// is an error: it is not a character, and `char::from_u32` would refuse it anyway - better to
    /// say which input was wrong than to answer with a replacement glyph.
    fn unicode_escape(&mut self) -> Result<char, Unsupported> {
        let hex4 = |c: &mut Self| -> Result<u32, Unsupported> {
            let end = c.i + 4;
            if end > c.b.len() {
                return Err(Unsupported::Syntax("a truncated `\\u` escape".into()));
            }
            let s = std::str::from_utf8(&c.b[c.i..end])
                .map_err(|_| Unsupported::Syntax("a malformed `\\u` escape".into()))?;
            let v = u32::from_str_radix(s, 16)
                .map_err(|_| Unsupported::Syntax(format!("`\\u{s}` is not four hex digits")))?;
            c.i = end;
            Ok(v)
        };
        let first = hex4(self)?;
        let code = match first {
            0xd800..=0xdbff => {
                if self.peek() != Some(b'\\') || self.b.get(self.i + 1) != Some(&b'u') {
                    return Err(Unsupported::Syntax(
                        "a high surrogate with no low surrogate after it".into(),
                    ));
                }
                self.i += 2;
                let low = hex4(self)?;
                if !(0xdc00..=0xdfff).contains(&low) {
                    return Err(Unsupported::Syntax(
                        "a high surrogate followed by something that is not a low surrogate".into(),
                    ));
                }
                0x10000 + ((first - 0xd800) << 10) + (low - 0xdc00)
            }
            0xdc00..=0xdfff => {
                return Err(Unsupported::Syntax("a lone low surrogate".into()));
            }
            v => v,
        };
        char::from_u32(code).ok_or_else(|| {
            Unsupported::Syntax(format!("`\\u` escape {code:#x} is not a character"))
        })
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
    fn operation_tail(&mut self) -> Result<Option<String>, Unsupported> {
        self.trivia();
        // An optional operation name. **Kept**, because a document may carry several and the request's
        // `operationName` says which one to run.
        let mut name = None;
        if self
            .peek()
            .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
        {
            name = Some(self.ident()?);
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
        Ok(name)
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
            Some(b'"') => Ok(Value::Str(self.string()?)),
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
                    // `null` is a GraphQL literal, not an enum value. Reading it as one compiled
                    // `hooks: null` into a comparison with the string `'null'` (Jules, #1282).
                    "null" => Value::Null,
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
/// A sort key that orders a canonical decimal string **numerically**.
///
/// A nest stores every big number as canonical text (`analytics.rs:2253`: columns are `UBIGINT`,
/// everything else is text), so `ORDER BY b."value"` compared strings: `9000351` ranked above
/// `60000353` because `'9' > '6'`. Text order agrees with numeric order only while every value has the
/// same digit count, which is why it survives a fixture and not a real pool (#1325).
///
/// **`TRY_CAST(.. AS DECIMAL(38,0))` is the wrong fix.** A `uint256` reaches 78 digits, so the cast is
/// NULL past 38 and a row does not merely sort oddly - it disappears from a filter it satisfies.
/// Trading a wrong order for a missing row is not an improvement.
///
/// So the key is built from the text, and it is exact at any precision:
///
/// - a leading `'0'` for negatives and `'1'` for the rest, so sign dominates;
/// - the integer part's digit count, zero-padded, so magnitude dominates within a sign. Negatives carry
///   `100000 - length`, because a longer magnitude is a *smaller* number;
/// - then the digits with the point removed, which compares left-aligned and so compares numerically
///   once the integer lengths match. Negatives take the nines complement, which reverses that order.
///
/// The cast means it works whether the column is text or a real integer, and it works under `DESC`
/// because it is one key rather than several. It assumes canonical text - no leading zeros, no trailing
/// zeros past the point - which is what both the decode registry and graph-node's `normalized()` produce.
///
/// **Bounded, not unbounded.** The length term is six padded digits, so the key is exact for an integer
/// part up to 99,999 digits and wrong above it. A `uint256` is 78 and a `BigInt` in practice is an EVM
/// word, so the bound is four orders of magnitude clear of anything a mapping can store - but "at any
/// precision" was an overclaim, and the number is written here rather than left to be rediscovered.
fn numeric_sort_key(expr: &str) -> String {
    let v = format!("CAST({expr} AS VARCHAR)");
    // Written as one line: a raw-string continuation leaves runs of spaces in the emitted SQL.
    [
        format!("CASE WHEN {v} LIKE '-%'"),
        format!("THEN '0' || lpad(CAST(100000 - length(split_part(substr({v}, 2), '.', 1)) AS VARCHAR), 6, '0')"),
        format!("|| translate(replace(substr({v}, 2), '.', ''), '0123456789', '9876543210')"),
        format!("ELSE '1' || lpad(CAST(length(split_part({v}, '.', 1)) AS VARCHAR), 6, '0')"),
        format!("|| replace({v}, '.', '') END"),
    ]
    .join(" ")
}

/// Whether this field's values must be compared through [`numeric_sort_key`] rather than directly.
///
/// The same set as [`wire_string_cast`], and not by coincidence: a scalar graph-node sends as a string is
/// one a nest stores as text, so the scalars whose wire type needs a cast are exactly those whose
/// ordering needs a key. `Int` is a real integer column on both sides and orders itself.
fn needs_numeric_key(ty: &graph_schema::FieldType) -> bool {
    wire_string_cast(ty)
}

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
        /// A column holding the target's id, non-null exactly when the joined row exists.
        marker: String,
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
            // **The wire type, not the storage type.** graph-node sends `BigInt`, `BigDecimal` and
            // `Int8` as GraphQL *strings* and only `Int` as a number - `graph/src/data/store/mod.rs:554`
            // maps every stored value to `q::Value`, and three of the numeric scalars become
            // `q::Value::String`. A nest storing `value` as `DECIMAL(38,0)` answered a JSON number, so a
            // client doing `BigInt.from(r.value)` got a number it cannot hold above 2^53 - a wrong value
            // presented as a right one, which is the one thing this surface may not do.
            //
            // Cast here rather than in the view, so `where` and `orderBy` still compare the numeric
            // column. Casting in the view made `orderBy: value` lexicographic and ranked 9000351 above
            // 60000353.
            let cast = wire_string_cast(&field.ty);
            let expr = if cast {
                format!("CAST({BASE}.\"{}\" AS VARCHAR)", sel.name)
            } else {
                format!("{BASE}.\"{}\"", sel.name)
            };
            let col = if sel.key == sel.name {
                // A cast needs an alias to land under the field's name; a bare column already has one.
                if cast {
                    cols.push(format!("{expr} AS \"{}\"", sel.name));
                } else {
                    cols.push(expr);
                }
                sel.name.clone()
            } else {
                let c = format!("a{i}");
                cols.push(format!("{expr} AS \"{c}\""));
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
                let Some(cf) = child.fields.iter().find(|f| f.name == sub.name) else {
                    return Err(Unsupported::UnknownField {
                        entity: target.to_string(),
                        field: sub.name.clone(),
                    });
                };
                if let Some(inner_target) = cf.ty.entity_name() {
                    return Err(Unsupported::Syntax(format!(
                        "`{target}.{}` returns `{inner_target}`, which needs a selection set",
                        sub.name
                    )));
                }
                // **Keyed by the alias, valued from the field.** This packed `sub.name` on both sides, so
                // `{ pools { swaps { sid: id } } }` answered under `id` - a key the client never asked for,
                // inside a list where its own alias test could not see it (Jules, #1282).
                //
                // Cast on the same rule as a top-level scalar: a `BigInt` packed raw is a JSON number
                // inside the array, which is the wire-type defect one level down.
                let value = if wire_string_cast(&cf.ty) {
                    format!("CAST({alias}.\"{}\" AS VARCHAR)", sub.name)
                } else {
                    format!("{alias}.\"{}\"", sub.name)
                };
                packed.push(format!("\"{}\" := {value}", sub.key));
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
        // The target's own id, always selected, so shaping can tell "no such row" from "the row
        // exists and every selected field happens to be null". Using the selected values themselves
        // answered `null` for a relation that was really there (Jules on #1282).
        let marker = format!("{alias}__present");
        cols.push(format!("{alias}.\"id\" AS \"{marker}\""));
        let mut sub = Vec::new();
        for s in &sel.sub {
            if !s.args.is_empty() {
                return Err(Unsupported::NestedSelection(s.name.clone()));
            }
            let Some(cf) = tent.fields.iter().find(|x| x.name == s.name) else {
                return Err(Unsupported::UnknownField {
                    entity: target.clone(),
                    field: s.name.clone(),
                });
            };
            // The same rule as the outer selection: a composite field needs a selection set. Without
            // this a traversed child's own relation was emitted as a scalar column, so
            // `{ pools { token0 { whitelistPools } } }` returned a stored id list under a field the
            // schema declares as an object list (Jules on #1282).
            if let Some(inner_target) = cf.ty.entity_name() {
                return Err(Unsupported::Syntax(format!(
                    "`{target}.{}` returns `{inner_target}`, which needs a selection set",
                    s.name
                )));
            }
            let col = format!("{alias}__{}", s.name);
            // The wire type is the schema's, wherever the field is selected from. This loop emitted the
            // child's column raw, so `{ pools { token0 { decimals } } }` answered a JSON number for a
            // field graph-node sends as a string - the same defect as the top-level selection and the
            // packed list, in the third of the three places a scalar reaches a client (Jules on #1282).
            let expr = if wire_string_cast(&cf.ty) {
                format!("CAST({alias}.\"{}\" AS VARCHAR)", s.name)
            } else {
                format!("{alias}.\"{}\"", s.name)
            };
            cols.push(format!("{expr} AS \"{col}\""));
            sub.push((s.key.clone(), col));
        }
        shape.push(Shape::Object {
            key: sel.key.clone(),
            marker,
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
                if matches!(value, Value::Null) {
                    return Err(Unsupported::NullValue("id".into()));
                }
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
                    wheres.push(lower_predicate(schema, ent, key, v, BASE, 0)?);
                }
            }
            "first" | "skip" | "orderBy" | "orderDirection" if !singular => {}
            other => return Err(Unsupported::Argument(other.to_string())),
        }
    }
    // **A singular root without `id` is an error, not the first row.**
    //
    // The generated schema declares `pool(id: ID!, ...)`, so a client validating against it is told
    // the argument is required; `compile` only added the predicate when the argument was present and
    // never checked that it was. `{ pool { id } }` therefore reached SQL as the pool view with
    // `LIMIT 1` and answered an arbitrary row as though it were the one asked for - a wrong answer
    // with no error, which is the one failure shape this endpoint must not have (Jules on #1282).
    //
    // Checked after the loop rather than inside it, because the loop only sees arguments that were
    // supplied, and the defect is the absence of one.
    if singular && !root.args.contains_key("id") {
        return Err(Unsupported::MissingArgument("id".into()));
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
                let Some(field) = ent.fields.iter().find(|x| &x.name == f) else {
                    return Err(Unsupported::UnknownField {
                        entity: entity.clone(),
                        field: f.clone(),
                    });
                };
                // **Declared is not stored.** graph-node's generated `orderBy` enum includes every field
                // of the entity, `@derivedFrom` lists among them, so `orderBy: swaps` is a value the
                // schema advertises and the parent view has no column for. Emitting `ORDER BY b."swaps"`
                // sent it to DuckDB to fail, or - worse, once the same query selects the list - sorted the
                // rows by a JSON aggregate (Jules, #1282). Ordering by a relation is the same join a
                // traversal needs, and it is refused for the same reason.
                if field.derived_from.is_some()
                    || matches!(field.ty, graph_schema::FieldType::List(_))
                    || field.ty.entity_name().is_some()
                {
                    return Err(Unsupported::Argument(format!(
                        "orderBy `{f}` is a relation rather than a stored value, which needs the join                          this slice does not lower yet"
                    )));
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
        // Ordered by the numeric key where the field is a big number, so `orderBy: value` does not rank
        // `9000351` above `60000353` (#1325).
        let order_expr = format!("{BASE}.\"{order_field}\"");
        let order_expr = if ent
            .fields
            .iter()
            .find(|x| x.name == order_field)
            .is_some_and(|f| needs_numeric_key(&f.ty))
        {
            numeric_sort_key(&order_expr)
        } else {
            order_expr
        };
        sql.push_str(&format!(" ORDER BY {order_expr} {dir}"));

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
    schema: &Schema,
    ent: &graph_schema::Entity,
    key: &str,
    v: &Value,
    base: &str,
    depth: usize,
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
                .map(|(k, vv)| lower_predicate(schema, ent, k, vv, base, depth))
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
    // `token0_: Token_filter` - a nested filter on the related entity, which the generated schema
    // advertises for every relation. It used to fall through the suffix loop below (the key ends in
    // `_`) and come back as `UnknownField`, so our own schema validated a query the endpoint then
    // refused for the wrong reason (Jules on #1282).
    if let Some(field_name) = key.strip_suffix('_') {
        let Some(field) = ent.fields.iter().find(|f| f.name == field_name) else {
            return Err(Unsupported::UnknownField {
                entity: ent.name.clone(),
                field: key.to_string(),
            });
        };
        let Value::Object(inner) = v else {
            return Err(Unsupported::Argument(format!(
                "`{key}` takes a filter object"
            )));
        };
        if inner.is_empty() {
            return Err(Unsupported::Argument(format!(
                "`{key}` is an empty filter, which has no meaning this slice has verified"
            )));
        }
        // A **to-one** reference only. The reference advertises `swaps_` for `@derivedFrom` lists too,
        // but a derived-list nested filter **504s on graph-node itself** for this deployment, so its
        // semantics cannot be measured here - and "probably matches if any child matches" is a guess
        // about which rows come back. Refused by name, with the reason.
        let target = match (&field.ty, &field.derived_from) {
            (graph_schema::FieldType::Entity(t), None) => t.clone(),
            _ => {
                return Err(Unsupported::Operator(format!(
                "{key} (a nested filter across a list relation needs a child-existence subquery \
                     whose semantics the reference endpoint times out rather than demonstrates)"
            )))
            }
        };
        let child = schema
            .entities
            .iter()
            .find(|e| e.name == target)
            .ok_or_else(|| Unsupported::UnknownField {
                entity: ent.name.clone(),
                field: key.to_string(),
            })?;
        let alias = format!("n{depth}");
        let view = crate::subgraph_import::to_alias(&target);
        let parts: Result<Vec<String>, Unsupported> = inner
            .iter()
            .map(|(k, vv)| lower_predicate(schema, child, k, vv, &alias, depth + 1))
            .collect();
        // `EXISTS` rather than a join: the parent's row count must not change, and `first` still means
        // what it says.
        return Ok(format!(
            "EXISTS (SELECT 1 FROM \"{view}\" {alias} WHERE {alias}.\"id\" = {base}.\"{field_name}\" AND {})",
            parts?.join(" AND ")
        ));
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
        // The operator has to be one the generated schema declares for *this* field's type. `Bytes`
        // carries ten operators and `String` twenty - no `_starts_with`, no `_nocase` on `Bytes` - so
        // accepting one here would answer a query a client's own validator, built from our schema, would
        // have refused.
        //
        // **An enum carries four and none of them is a comparison.** It has no ordering, so `type_gt`
        // lowered to `b."type" > 'order0'` answered a plausible row set for an ordering graph-node does
        // not define - a wrong answer with no error, from a query the real endpoint rejects (#1306).
        let allowed =
            f.ty.filter_scalar()
                .map(|sc| graph_schema::filter_suffixes(sc, schema.enums.contains_key(sc)))
                .unwrap_or_default();
        if !allowed.contains(suffix) {
            return Err(Unsupported::Operator(key.to_string()));
        }
        let col = format!("{base}.\"{field}\"");
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
                    // Named separately from "has no literal", which reads as a shape this dialect has not
                    // implemented. A `null` is a value the client deliberately supplied and there is more
                    // than one thing it could mean, so it gets its own refusal (Jules, #1282).
                    if matches!(v, Value::Null) {
                        return Err(Unsupported::NullValue(key.to_string()));
                    }
                    let lit = v
                        .sql_literal()
                        .ok_or_else(|| Unsupported::Argument(format!("`{key}` has no literal")))?;
                    // **An ordering comparison on a big number compares keys, not text.** `value_gt: "250"`
                    // was `b."value" > '250'`, which excludes `9000351` because `'9' > '2'` is the only
                    // thing being asked. Equality and inequality are left alone: canonical text compares
                    // equal exactly when the numbers do.
                    //
                    // The literal goes through the *same* expression rather than a key built in Rust, so
                    // the two cannot drift - one rule, one implementation.
                    if needs_numeric_key(&f.ty)
                        && matches!(*suffix, "_gt" | "_gte" | "_lt" | "_lte")
                    {
                        return Ok(format!(
                            "{} {op} {}",
                            numeric_sort_key(&col),
                            numeric_sort_key(&lit)
                        ));
                    }
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

    /// The numeric sort key abbreviated to `KEY(<inner>)`, so an exact-SQL assertion stays readable.
    ///
    /// The key is four hundred characters of `CASE`, and three of these tests are about *which arguments
    /// reach the SQL* rather than about the key's own shape. Abbreviating keeps them exact on the thing
    /// they are for; that the key orders numerically is asserted against DuckDB in
    /// `the_numeric_key_orders_by_value_not_by_text`, which is where it belongs.
    fn compact(sql: &str) -> String {
        let mut out = sql.to_string();
        while let Some(at) = out.find("CASE WHEN CAST(") {
            let Some(end) = out[at..]
                .find("'.', '') END")
                .map(|e| at + e + "'.', '') END".len())
            else {
                break;
            };
            let inner = {
                let head = &out[at + "CASE WHEN CAST(".len()..];
                let stop = head.find(" AS VARCHAR)").expect("a cast");
                head[..stop].to_string()
            };
            out.replace_range(at..end, &format!("KEY({inner})"));
        }
        out
    }

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

    /// graph-node decodes GraphQL string escapes; this parser skipped over them and then took the
    /// raw byte slice, so `"a\"b"` reached SQL as `a\"b` and `\n` as a backslash and an `n`. A
    /// filter silently compares against a value the client did not ask for, which is the worst
    /// shape this endpoint has.
    #[test]
    fn a_string_escape_reaches_sql_decoded() {
        let cases: &[(&str, &str)] = &[
            (r#"a\"b"#, "a\"b"),
            (r#"a\\b"#, "a\\b"),
            (r#"a\nb"#, "a\nb"),
            (r#"a\tb"#, "a\tb"),
            (r#"a\/b"#, "a/b"),
            (r#"a\rb"#, "a\rb"),
            (r#"a\bb"#, "a\u{8}b"),
            (r#"a\fb"#, "a\u{c}b"),
            (r#"Aé€"#, "A\u{e9}\u{20ac}"),
            (r#"😀"#, "\u{1f600}"),
            (r#"a\u0041b"#, "aAb"),
            (r#"\u00e9"#, "\u{e9}"),
            // A surrogate pair is how `\u` carries anything above the BMP.
            (r#"\ud83d\ude00"#, "\u{1f600}"),
        ];
        for (literal, want) in cases {
            let q = format!("{{ pools(where: {{ hooks: \"{literal}\" }}) {{ id }} }}");
            let got = one(&q).args.get("where").cloned().expect("where");
            let Value::Object(m) = got else {
                panic!("where is not an object")
            };
            assert_eq!(
                m.get("hooks"),
                Some(&Value::Str((*want).to_string())),
                "literal {literal:?} decoded wrong"
            );
        }
    }

    /// A malformed string must be a syntax error, not a panic and not a silent success. The escape
    /// loop advanced twice for a trailing backslash, so `self.i` could pass the end of the input and
    /// the slice that followed it indexed out of bounds - a panic in a request handler.
    ///
    /// The expected wording is asserted, not just `is_err()`. Four of twelve mutations survived the
    /// first version of this test: each broke the scanner, the scanner then ran off the end of the
    /// string, and the *object* parser raised "unclosed object" a moment later. A test that only asks
    /// whether something failed cannot tell a guard working from a guard gone.
    #[test]
    fn a_malformed_string_is_refused_rather_than_crashing() {
        let cases: &[(&str, &str)] = &[
            // The exact shape that indexed out of bounds: the backslash is the last byte, so the
            // escape skip walked `self.i` to `len + 1`. Measured before the fix: "range end index 29
            // out of range for slice of length 28", from a 28-byte request.
            (r#"{ pools(where: { hooks: "ab\"#, "unterminated string"),
            // A backslash and a space is an invalid escape, which is a better message than
            // "unterminated" and is why this case is spelled out separately from the one above.
            (
                r#"{ pools(where: { hooks: "ab\ "#,
                "is not a GraphQL string escape",
            ),
            (
                r#"{ pools(where: { hooks: "unterminated "#,
                "unterminated string",
            ),
            (
                "{ pools(where: { hooks: \"a\nb\" }) { id } }",
                "line break inside a string",
            ),
            (
                r#"{ pools(where: { hooks: "bad \q escape" }) { id } }"#,
                "is not a GraphQL string escape",
            ),
            (
                r#"{ pools(where: { hooks: """a""" }) { id } }"#,
                "block string",
            ),
            (
                r#"{ pools(where: { hooks: "\u00zz" }) { id } }"#,
                "not four hex digits",
            ),
            (
                r#"{ pools(where: { hooks: "\u00" }) { id } }"#,
                "not four hex digits",
            ),
            // Genuinely truncated: fewer than four bytes remain after the `\u`, so the window cannot
            // be read at all. The `}`-terminated case above has four bytes and merely is not hex.
            (r#"{ pools(where: { hooks: "\u00"#, "truncated"),
            // `\u` reads four bytes, and four bytes can land inside a code point. The first is valid
            // UTF-8 that is not hex; the second splits `\u{20ac}` and is not UTF-8 at all.
            (
                "{ pools(where: { hooks: \"\\u00\u{e9}\" }) { id } }",
                "not four hex digits",
            ),
            (
                "{ pools(where: { hooks: \"\\u00\u{20ac}\" }) { id } }",
                "malformed `\\u` escape",
            ),
            (
                r#"{ pools(where: { hooks: "\ud83d only half" }) { id } }"#,
                "no low surrogate",
            ),
            (
                r#"{ pools(where: { hooks: "\ud83d\u0041 not a low" }) { id } }"#,
                "not a low surrogate",
            ),
            (
                r#"{ pools(where: { hooks: "\udc00 lone low" }) { id } }"#,
                "lone low surrogate",
            ),
        ];
        for (q, want) in cases {
            match parse(q) {
                Ok(got) => panic!("{q:?} parsed as {got:?} instead of being refused"),
                Err(Unsupported::Syntax(msg)) => assert!(
                    msg.contains(want),
                    "{q:?} was refused as {msg:?}, which does not mention {want:?} - so this case \
                     is no longer testing the guard it was written for"
                ),
                Err(other) => panic!("{q:?} was refused as {other:?}, not a syntax error"),
            }
        }
    }

    /// An enum field takes four operators, and a comparison is not one of them.
    ///
    /// An enum has no ordering. `type_gt: order0` lowered to `b."type" > 'order0'` and answered a
    /// plausible row set off the *string* ordering of the value names - a wrong answer with no error,
    /// from a query the real endpoint rejects outright.
    ///
    /// The recorded reference could not catch this: Uniswap V4's schema declares no author enum, so the
    /// row was generalised from the numeric set. graph-node's `field_enum_filter_input_values` returns
    /// exactly `["", "not", "in", "not_in"]`, and three unrelated live deployments answered four
    /// (#1306).
    #[test]
    fn a_comparison_on_an_enum_field_is_refused() {
        let sch = graph_schema::parse(
            "enum OrderType { order0 order1 }\n\
             type Order @entity { id: ID! type: OrderType! size: BigInt! }\n",
        )
        .expect("parse");

        // The four it does have.
        for q in [
            r#"{ orders(where: { type: order0 }) { id } }"#,
            r#"{ orders(where: { type_not: order0 }) { id } }"#,
            r#"{ orders(where: { type_in: [order0, order1] }) { id } }"#,
            r#"{ orders(where: { type_not_in: [order0] }) { id } }"#,
        ] {
            compile(&sch, &one(q)).unwrap_or_else(|e| panic!("{q} must compile: {e}"));
        }

        // The four it does not.
        for q in [
            r#"{ orders(where: { type_gt: order0 }) { id } }"#,
            r#"{ orders(where: { type_lt: order0 }) { id } }"#,
            r#"{ orders(where: { type_gte: order0 }) { id } }"#,
            r#"{ orders(where: { type_lte: order0 }) { id } }"#,
        ] {
            let e = compile(&sch, &one(q)).expect_err(&format!("{q} must be refused"));
            assert!(
                matches!(e, Unsupported::Operator(_)),
                "{q} was refused as {e:?}, which is not an operator refusal"
            );
        }

        // And the numeric field beside it keeps its comparisons, so the refusal is about the enum and
        // not about comparisons generally.
        compile(&sch, &one(r#"{ orders(where: { size_gt: "5" }) { id } }"#))
            .expect("a numeric comparison still compiles");
    }

    /// A singular root without `id` is refused, in graph-node's words.
    ///
    /// The generated schema declares `pool(id: ID!, ...)`, so a client is told the argument is
    /// required. `compile` added the predicate only when the argument was present and never checked
    /// that it was, so `{ pool { id } }` lowered to the pool view with `LIMIT 1` and answered an
    /// arbitrary row as though it were the one asked for (Jules on #1282).
    ///
    /// The wording is graph-node's, probed against the live reference: `{ token { symbol } }` answers
    /// `No value provided for required argument: `id``. Asserted rather than `is_err()`, because the
    /// compiler already had an `Argument` error reading "is not implemented yet" and a client reading
    /// that would go looking for a missing feature instead of fixing its query.
    #[test]
    fn a_singular_root_without_an_id_is_refused() {
        let err = compile(&schema(), &one("{ pool { id liquidity } }")).expect_err("must refuse");
        assert_eq!(
            err.to_string(),
            "No value provided for required argument: `id`",
            "graph-node's wording, not ours"
        );

        // The same root with an id still compiles, so the guard has not closed the door.
        let c = compile(&schema(), &one(r#"{ pool(id: "0xaaa") { id } }"#)).expect("with an id");
        assert!(
            c.sql.contains("\"id\" = '0xaaa'"),
            "the id must still become a predicate: {}",
            c.sql
        );

        // A collection needs no id, and must not have acquired the requirement.
        compile(&schema(), &one("{ pools { id } }")).expect("a collection needs no id");

        // `where` is not a substitute: graph-node requires `id` on the singular root whatever else
        // was supplied, so accepting this would be our invention rather than its behaviour.
        let err = compile(
            &schema(),
            &one(r#"{ pool(where: { id: "0xaaa" }) { id } }"#),
        )
        .expect_err("`where` does not satisfy a required `id`");
        assert_eq!(
            err.to_string(),
            "No value provided for required argument: `id`"
        );
    }

    #[test]
    fn a_plain_collection_gets_graph_nodes_defaults() {
        let c = compile(&schema(), &one("{ pools { id liquidity } }")).unwrap();
        // `first = 100`, `skip = 0` and ascending by id are graph-node's defaults, read off the
        // recorded reference. A client that omits them relies on them, and an unordered result with
        // `skip` would be a different page each call.
        assert_eq!(
            c.sql,
            r#"SELECT b."id", CAST(b."liquidity" AS VARCHAR) AS "liquidity" FROM "pool" b ORDER BY b."id" ASC LIMIT 100 OFFSET 0"#
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
            compact(&c.sql),
            r#"SELECT b."id" FROM "pool" b WHERE b."hooks" = '0xabc' AND KEY(b."liquidity") > KEY(100) ORDER BY KEY(b."liquidity") DESC LIMIT 5 OFFSET 10"#
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
                // `j1__present` is the target's id, carried so shaping can tell a missing row from
                // one whose selected fields are all null.
                r#"SELECT b."id", j1."id" AS "j1__present", j1."symbol" AS "j1__symbol" FROM "pool" b"#,
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
                    marker: "j1__present".into(),
                    fields: vec![("symbol".into(), "j1__symbol".into())],
                },
            ],
            "only the shape knows `j1__symbol` belongs under `token0`"
        );

        // A composite field inside a traversal needs a selection set too. Without this the child's own
        // relation was emitted as a scalar column, returning a stored id under a field the schema
        // declares as an object.
        let e = compile(&schema(), &one("{ pools { token0 { pools } } }"))
            .expect_err("a composite child needs a selection set");
        assert!(
            matches!(&e, Unsupported::Syntax(m) if m.contains("Token.pools")),
            "{e:?}"
        );
        // And the same inside a derived list.
        let e = compile(&schema(), &one("{ pools { swaps { pool } } }"))
            .expect_err("a composite child of a derived list too");
        assert!(
            matches!(&e, Unsupported::Syntax(m) if m.contains("Swap.pool")),
            "{e:?}"
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
        assert!(
            compact(&c.sql).contains(r#"KEY(b."liquidity") > KEY('1')"#),
            "{}",
            compact(&c.sql)
        );

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
            r#"SELECT b."id" AS "a0", CAST(b."liquidity" AS VARCHAR) AS "liquidity" FROM "pool" b ORDER BY b."id" ASC LIMIT 100 OFFSET 0"#,
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
                marker: "j0__present".into(),
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

    /// `token0_: Token_filter` - a nested filter on a related entity, which the generated schema
    /// advertises for every relation.
    #[test]
    fn a_nested_relation_filter_lowers_to_an_exists_subquery() {
        let c = compile(
            &schema(),
            &one(r#"{ pools(where: { token0_: { symbol: "WETH" } }) { id } }"#),
        )
        .expect("a nested relation filter lowers");
        // EXISTS rather than a join: the parent's row count must not change, or `first` stops meaning
        // what it says.
        assert!(
            c.sql.contains(
                r#"EXISTS (SELECT 1 FROM "token" n0 WHERE n0."id" = b."token0" AND n0."symbol" = 'WETH')"#
            ),
            "{}",
            c.sql
        );

        // It composes with the parent's own conditions, and the inner operators are the child's.
        let c = compile(
            &schema(),
            &one(
                r#"{ pools(where: { liquidity_gt: "1", token0_: { symbol_contains: "ET" } }) { id } }"#,
            ),
        )
        .unwrap();
        assert!(
            compact(&c.sql).contains(r#"KEY(b."liquidity") > KEY('1')"#),
            "{}",
            compact(&c.sql)
        );
        assert!(
            c.sql.contains(r#"n0."symbol" LIKE '%ET%' ESCAPE '\'"#),
            "the child's text operator is lowered against the child: {}",
            c.sql
        );

        // An unknown field inside the nested filter is named against the *child* entity.
        let e = compile(
            &schema(),
            &one(r#"{ pools(where: { token0_: { nope: "x" } }) { id } }"#),
        )
        .expect_err("unknown child field");
        assert!(
            matches!(&e, Unsupported::UnknownField { entity, field } if entity == "Token" && field == "nope"),
            "{e:?}"
        );

        // A nested filter across a **list** relation is refused by name and says why: the reference
        // endpoint 504s on one, so its semantics are not something this slice has measured, and a
        // guess about which rows come back is a guess about the answer.
        let e = compile(
            &schema(),
            &one(r#"{ pools(where: { swaps_: { id: "s1" } }) { id } }"#),
        )
        .expect_err("a list relation is refused");
        assert!(
            matches!(&e, Unsupported::Operator(o) if o.starts_with("swaps_") && o.contains("child-existence")),
            "{e:?}"
        );

        // An empty nested filter is refused rather than treated as no condition.
        assert!(compile(
            &schema(),
            &one(r#"{ pools(where: { token0_: {} }) { id } }"#)
        )
        .is_err());

        // And a `_`-suffixed key naming no relation at all is still an unknown field.
        let e = compile(
            &schema(),
            &one(r#"{ pools(where: { nope_: { id: "x" } }) { id } }"#),
        )
        .expect_err("no such relation");
        assert!(
            matches!(&e, Unsupported::UnknownField { field, .. } if field == "nope_"),
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
            compact(&c.sql),
            r#"SELECT b."id" FROM "pool" b WHERE KEY(b."liquidity") > KEY('5') ORDER BY b."id" ASC LIMIT 3 OFFSET 0"#,
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
