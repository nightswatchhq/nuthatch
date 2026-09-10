//! RFC-0053 S1 (#1265): generate graph-node's schema from an imported `schema.graphql`.
//!
//! A consumer's generated client validates against introspection before a useful query reaches the
//! server, so the generated type system has to match graph-node's shape, not merely expose the same
//! data. Every rule implemented here was extracted from a recorded introspection of a real
//! graph-node rather than from documentation - see `docs/graph-schema-generation-rules.md` and the
//! fixture it names. The golden test diffs against that fixture.
//!
//! **Why a second schema parser.** `port_report::parse_schema` carries `name`, `line` and
//! `derived_from` per field and no type, because classification never needed one. Here the field's
//! type decides its entire operator set - `Bytes` gets ten operators and `String` eighteen - so the
//! type is the load-bearing part.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

/// A field's declared type, after stripping `!`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// A built-in or graph-node scalar: `String`, `BigInt`, `Bytes`, …
    Scalar(String),
    /// A reference to another `@entity` type. Stored by name; resolved against the entity set.
    Entity(String),
    /// An enum declared in the schema.
    Enum(String),
    /// A list of the inner type.
    List(Box<FieldType>),
}

impl FieldType {
    /// The name graph-node uses for this field in the generated object type.
    pub fn render(&self) -> String {
        match self {
            FieldType::Scalar(n) | FieldType::Enum(n) => n.clone(),
            // A relation is exposed as the entity object on the output side.
            FieldType::Entity(n) => n.clone(),
            FieldType::List(inner) => format!("[{}]", inner.render()),
        }
    }

    /// The scalar a `*_filter` compares this field with. A relation is filtered **by its id**, which
    /// graph-node types as `String` rather than `ID` - measured on `Pool.token0`.
    pub fn filter_scalar(&self) -> Option<&str> {
        match self {
            FieldType::Scalar(n) => Some(n.as_str()),
            FieldType::Entity(_) => Some("String"),
            FieldType::Enum(n) => Some(n.as_str()),
            // Lists are not filtered by comparison operators.
            FieldType::List(_) => None,
        }
    }

    /// The entity this type refers to, through a list if need be.
    pub fn entity_name(&self) -> Option<&str> {
        match self {
            FieldType::Entity(n) => Some(n.as_str()),
            FieldType::List(inner) => inner.entity_name(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    /// `true` when the declaration carried `!`. Output nullability only; filters ignore it.
    pub non_null: bool,
    /// For a list, whether the *inner* type carried `!`. graph-node renders the author's declaration
    /// verbatim and this schema uses both shapes: `Pool.swaps: [Swap!]!` against
    /// `Transaction.swaps: [Swap]!`. Assuming `[Inner!]!` put five fields of `Transaction` wrong, and
    /// the object-type sweep caught it on its first run. Kept on the field rather than inside
    /// `FieldType` because only rendering needs it and a subgraph schema never nests lists.
    pub inner_non_null: bool,
    /// `@derivedFrom(field: "x")`. A derived field is a reverse lookup, rendered with collection
    /// arguments but **no `block`** - it inherits the parent query's.
    pub derived_from: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Entity {
    pub name: String,
    pub fields: Vec<Field>,
    pub immutable: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Schema {
    pub entities: Vec<Entity>,
    /// Enum types declared in the schema, in declaration order.
    pub enums: BTreeMap<String, Vec<String>>,
}

/// The operator set a **list** field gets, every one list-typed. Read off `Token.whitelistPools`
/// (`[Pool!]!`), which generates exactly these six and no comparison operators at all.
pub const LIST_SUFFIXES: &[&str] = &[
    "",
    "_not",
    "_contains",
    "_contains_nocase",
    "_not_contains",
    "_not_contains_nocase",
];

/// Scalars graph-node defines on top of GraphQL's built-ins.
pub const GRAPH_SCALARS: &[&str] = &["BigInt", "BigDecimal", "Bytes", "Int8", "Timestamp"];
/// GraphQL's own, which appear in the generated schema whether or not the author used them.
pub const BUILTIN_SCALARS: &[&str] = &["String", "Int", "Float", "Boolean", "ID"];

/// Parse the entity and enum declarations of a subgraph `schema.graphql`.
///
/// Hand-rolled, like the rest of this tree's GraphQL handling. It needs the type of every field and
/// the `@derivedFrom` target, and nothing else: directives it does not understand are skipped rather
/// than refused, because a schema may carry `@fulltext`, `@aggregation` and others that do not
/// change the generated shape for S1.
pub fn parse(text: &str) -> Result<Schema> {
    let mut out = Schema::default();
    let b = text.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        // `type X @entity` and `enum Y` are the only declarations S1 reads.
        if starts_decl(text, i, "type ") {
            let (entity, next) = parse_entity(text, i)?;
            if let Some(e) = entity {
                out.entities.push(e);
            }
            i = next.max(i + 1);
            continue;
        }
        if starts_decl(text, i, "enum ") {
            let (name, values, next) = parse_enum(text, i)?;
            out.enums.insert(name, values);
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }
    if out.entities.is_empty() {
        bail!("schema.graphql declares no @entity types");
    }
    // A field naming a type that is neither a known scalar nor a declared enum is a relation. The
    // parse cannot know that until every declaration has been read, so resolve it here.
    let names: Vec<String> = out.entities.iter().map(|e| e.name.clone()).collect();
    let enums: Vec<String> = out.enums.keys().cloned().collect();
    for e in &mut out.entities {
        for f in &mut e.fields {
            resolve(&mut f.ty, &names, &enums);
        }
    }
    Ok(out)
}

fn resolve(ty: &mut FieldType, entities: &[String], enums: &[String]) {
    match ty {
        FieldType::List(inner) => resolve(inner, entities, enums),
        FieldType::Scalar(n) => {
            if entities.iter().any(|e| e == n) {
                *ty = FieldType::Entity(n.clone());
            } else if enums.iter().any(|e| e == n) {
                *ty = FieldType::Enum(n.clone());
            }
        }
        _ => {}
    }
}

/// A declaration keyword at `i` that is not the tail of a longer identifier and sits at a line start
/// (possibly after whitespace). Without the line-start test, `type` inside a description would match.
fn starts_decl(text: &str, i: usize, kw: &str) -> bool {
    if !text.is_char_boundary(i) || !text[i..].starts_with(kw) {
        return false;
    }
    let before = text[..i].rsplit('\n').next().unwrap_or("");
    before.trim().is_empty()
}

fn parse_entity(text: &str, start: usize) -> Result<(Option<Entity>, usize)> {
    let open = match text[start..].find('{') {
        Some(o) => start + o,
        None => return Ok((None, start + 5)),
    };
    let header = &text[start..open];
    let close = match match_brace(text, open) {
        Some(c) => c,
        None => bail!("unclosed type declaration in schema.graphql"),
    };
    // Only `@entity` types become queryable roots. An interface or a plain `type` does not.
    if !header.contains("@entity") {
        return Ok((None, close + 1));
    }
    let name = header
        .trim_start_matches("type ")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    if name.is_empty() {
        return Ok((None, close + 1));
    }
    let immutable = header.contains("immutable: true");
    let fields = parse_fields(&text[open + 1..close]);
    Ok((
        Some(Entity {
            name,
            fields,
            immutable,
        }),
        close + 1,
    ))
}

fn parse_enum(text: &str, start: usize) -> Result<(String, Vec<String>, usize)> {
    let open = match text[start..].find('{') {
        Some(o) => start + o,
        None => bail!("enum without a body in schema.graphql"),
    };
    let name = text[start + 5..open]
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();
    let close = match match_brace(text, open) {
        Some(c) => c,
        None => bail!("unclosed enum in schema.graphql"),
    };
    let values = text[open + 1..close]
        .lines()
        .map(|l| strip_comment(l).trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    Ok((name, values, close + 1))
}

fn parse_fields(body: &str) -> Vec<Field> {
    let mut out = Vec::new();
    for raw in body.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        // The type runs to the first space or `@`; directives follow.
        let rest = rest.trim();
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '@')
            .unwrap_or(rest.len());
        let (decl, tail) = (&rest[..end], &rest[end..]);
        let Some(ty) = parse_type(decl) else { continue };
        // `[Swap!]!` -> inner non-null; `[Swap]!` -> not. Read before the outer `!` is stripped.
        let inner_non_null = decl
            .trim()
            .trim_end_matches('!')
            .trim_end_matches(']')
            .ends_with('!');
        out.push(Field {
            name: name.to_string(),
            non_null: decl.ends_with('!'),
            inner_non_null,
            ty,
            derived_from: derived_target(tail),
        });
    }
    out
}

/// `BigInt!`, `[Swap!]!`, `Token`. Nullability is recorded on the field, not inside the type, because
/// a filter's operator set does not depend on it.
fn parse_type(decl: &str) -> Option<FieldType> {
    let d = decl.trim().trim_end_matches('!');
    if d.is_empty() {
        return None;
    }
    if let Some(inner) = d.strip_prefix('[') {
        let inner = inner.trim_end_matches(']').trim_end_matches('!');
        return Some(FieldType::List(Box::new(parse_type(inner)?)));
    }
    if !d.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(FieldType::Scalar(d.to_string()))
}

fn derived_target(tail: &str) -> Option<String> {
    let at = tail.find("@derivedFrom")?;
    let rest = &tail[at..];
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let inner = &rest[open + 1..close];
    let q = inner.find('"')?;
    let end = inner[q + 1..].find('"')? + q + 1;
    Some(inner[q + 1..end].to_string())
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(h) => &line[..h],
        None => line,
    }
}

fn match_brace(text: &str, open: usize) -> Option<usize> {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < b.len() {
        match b[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            b'"' => {
                // Skip a string so a brace inside a description is not counted.
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The plural root field name for an entity.
///
/// **Not `+ "s"`.** Measured against the reference: `modifyLiquidity` → `modifyLiquidities`,
/// `poolDayData` → `poolDayDatas`, `subscribe` → `subscribes`, `pool` → `pools`. A `y` preceded by a
/// consonant becomes `ies`. Getting this wrong means a generated client asks for a root field that
/// does not exist, which fails before a row is read - and it is invisible to any test that only
/// checks entities whose plural happens to be `+ s`.
///
/// The `@derivedFrom` field name is **not** produced by this function: that name is the schema
/// author's own, which is why the reference carries both `modifyLiquidities` (root) and
/// `Transaction.modifyLiquiditys` (derived) for one entity.
pub fn plural(name: &str) -> String {
    let lower = lower_first(name);
    let mut cs = lower.chars().rev();
    match (cs.next(), cs.next()) {
        (Some('y'), Some(prev)) if !"aeiou".contains(prev) => {
            format!("{}ies", &lower[..lower.len() - 1])
        }
        (Some('s'), _) | (Some('x'), _) | (Some('z'), _) => format!("{lower}es"),
        _ => format!("{lower}s"),
    }
}

/// The singular root field name: the entity name with a lower-cased first character.
pub fn lower_first(name: &str) -> String {
    let mut cs = name.chars();
    match cs.next() {
        Some(c) => c.to_lowercase().collect::<String>() + cs.as_str(),
        None => String::new(),
    }
}

/// Comparison suffixes for a filter field, by the scalar it compares.
///
/// **These sets are not uniform and `Bytes` is not `String`.** Measured on the reference:
/// `Bytes` has no `_starts_with`, no `_ends_with` and no `_nocase` variant anywhere - ten operators
/// against `String`'s eighteen. Generalising the string set over `Bytes` would advertise eight
/// operators graph-node does not have, and a client validating against us would then send queries the
/// real endpoint refuses.
pub fn filter_suffixes(scalar: &str) -> Vec<&'static str> {
    let ordered = ["", "_not", "_gt", "_lt", "_gte", "_lte", "_in", "_not_in"];
    let mut v: Vec<&'static str> = match scalar {
        "Boolean" => vec!["", "_not", "_in", "_not_in"],
        "Bytes" => vec![
            "",
            "_not",
            "_gt",
            "_lt",
            "_gte",
            "_lte",
            "_in",
            "_not_in",
            "_contains",
            "_not_contains",
        ],
        "String" | "ID" => {
            let mut s: Vec<&'static str> = ordered.to_vec();
            if scalar == "String" {
                s.extend([
                    "_contains",
                    "_contains_nocase",
                    "_not_contains",
                    "_not_contains_nocase",
                    "_starts_with",
                    "_starts_with_nocase",
                    "_not_starts_with",
                    "_not_starts_with_nocase",
                    "_ends_with",
                    "_ends_with_nocase",
                    "_not_ends_with",
                    "_not_ends_with_nocase",
                ]);
            }
            s
        }
        // BigInt, BigDecimal, Int, Int8, Timestamp, and any schema enum.
        _ => ordered.to_vec(),
    };
    v.dedup();
    v
}

/// Whether a suffix takes the list form of the field type (`[BigInt]` rather than `BigInt`).
pub fn suffix_is_list(suffix: &str) -> bool {
    suffix == "_in" || suffix == "_not_in"
}

impl Schema {
    /// Every scalar the generated schema declares, in the reference's order-independent set.
    pub fn scalars(&self) -> Vec<String> {
        let mut v: Vec<String> = BUILTIN_SCALARS
            .iter()
            .chain(GRAPH_SCALARS.iter())
            .map(|s| s.to_string())
            .collect();
        v.sort();
        v
    }

    /// The generated type names, which is what the first golden test compares. Object types, one
    /// `_filter` input and one `_orderBy` enum per entity, the supporting types, and the scalars.
    pub fn generated_type_names(&self) -> Vec<String> {
        let mut v = vec![
            "Query".to_string(),
            "_Block_".to_string(),
            "_Meta_".to_string(),
            "_Log_".to_string(),
            "_LogMeta_".to_string(),
            "_LogArgument_".to_string(),
            "Block_height".to_string(),
            "BlockChangedFilter".to_string(),
            "OrderDirection".to_string(),
            "_SubgraphErrorPolicy_".to_string(),
            // **Emitted whether or not the schema uses the feature.** The reference schema declares
            // no `@aggregation` and no timeseries at all, and graph-node still generates
            // `Aggregation_interval { hour day }` and `Aggregation_current { exclude include }`;
            // `LogLevel` likewise belongs to the `_logs` root field rather than to anything the
            // author wrote. Three types the documentation does not lead you to and the recorded
            // introspection does.
            "Aggregation_current".to_string(),
            "Aggregation_interval".to_string(),
            "LogLevel".to_string(),
        ];
        v.extend(self.scalars());
        v.extend(self.enums.keys().cloned());
        for e in &self.entities {
            v.push(e.name.clone());
            v.push(format!("{}_filter", e.name));
            v.push(format!("{}_orderBy", e.name));
        }
        v.sort();
        v.dedup();
        v
    }

    /// `Query`'s root field names: a singular and a plural per entity, plus `_meta` and `_logs`.
    pub fn root_field_names(&self) -> Vec<String> {
        let mut v = vec!["_meta".to_string(), "_logs".to_string()];
        for e in &self.entities {
            v.push(lower_first(&e.name));
            v.push(plural(&e.name));
        }
        v.sort();
        v
    }

    /// The output fields of an entity's object type, as `(name, rendered type, arg names)`.
    ///
    /// **Every list field carries the five collection arguments, stored or derived.** The reference
    /// gives `Token.whitelistPools` - a stored `[Pool!]!`, not a `@derivedFrom` - the same
    /// `skip, first, orderBy, orderDirection, where` as `Token.tokenDayData`, so the rule is "is a
    /// list", not "is derived". None of them takes `block`: a nested selection inherits the parent
    /// query's block.
    pub fn object_fields(&self, entity: &str) -> Vec<(String, String, Vec<&'static str>)> {
        const COLLECTION_ARGS: &[&str] = &["skip", "first", "orderBy", "orderDirection", "where"];
        let Some(e) = self.entities.iter().find(|e| e.name == entity) else {
            return Vec::new();
        };
        e.fields
            .iter()
            .map(|f| {
                let (rendered, args) = match &f.ty {
                    FieldType::List(inner) => (
                        format!(
                            "[{}{}]{}",
                            inner.render(),
                            if f.inner_non_null { "!" } else { "" },
                            if f.non_null { "!" } else { "" }
                        ),
                        COLLECTION_ARGS.to_vec(),
                    ),
                    ty => (
                        format!("{}{}", ty.render(), if f.non_null { "!" } else { "" }),
                        Vec::new(),
                    ),
                };
                (f.name.clone(), rendered, args)
            })
            .collect()
    }

    /// The input fields of `<Entity>_filter`, as `(name, rendered type)`.
    ///
    /// Four shapes, all read off the recorded reference rather than generalised from one of them:
    ///
    /// | field shape | what graph-node generates |
    /// |---|---|
    /// | scalar or enum | that scalar's operator set, `_in`/`_not_in` in list form |
    /// | relation (`Token`) | the `String` set on the bare name, **plus** `<name>_: Token_filter` |
    /// | list (`[Pool!]!`) | six list-typed operators - bare, `_not`, `_contains`, `_contains_nocase`, `_not_contains`, `_not_contains_nocase` - plus `<name>_: Pool_filter` |
    /// | `@derivedFrom` | **only** `<name>_: Target_filter`, no scalar comparison at all |
    ///
    /// The last two are the ones a first pass gets wrong. Skipping derived and list fields entirely -
    /// which is what "a reverse lookup has no column to compare" suggests - loses `whitelistPools`,
    /// its five list operators and both nested filters, and the sweep across all 19 entities is what
    /// caught it.
    pub fn filter_fields(&self, entity: &str) -> Vec<(String, String)> {
        let Some(e) = self.entities.iter().find(|e| e.name == entity) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for f in &e.fields {
            match (&f.ty, f.derived_from.is_some()) {
                // A derived field is filterable only through the far side.
                (_, true) => {
                    if let Some(target) = f.ty.entity_name() {
                        out.push((format!("{}_", f.name), format!("{target}_filter")));
                    }
                }
                (FieldType::List(inner), false) => {
                    // A list compares as a list of the inner type's filter scalar - `[String]` for a
                    // list of relations, because it compares the ids.
                    let Some(scalar) = inner.filter_scalar() else {
                        continue;
                    };
                    for suffix in LIST_SUFFIXES {
                        out.push((format!("{}{}", f.name, suffix), format!("[{scalar}!]")));
                    }
                    if let Some(target) = inner.entity_name() {
                        out.push((format!("{}_", f.name), format!("{target}_filter")));
                    }
                }
                (ty, false) => {
                    let Some(scalar) = ty.filter_scalar() else {
                        continue;
                    };
                    for suffix in filter_suffixes(scalar) {
                        // `[BigInt!]`: the inner type is non-null, the list itself is not. Read off
                        // the reference - a first pass wrote `[BigInt]` and only the full
                        // introspection diff could see it, because the name-level sweeps compare
                        // field names and not their types.
                        let rendered = if suffix_is_list(suffix) {
                            format!("[{scalar}!]")
                        } else {
                            scalar.to_string()
                        };
                        out.push((format!("{}{}", f.name, suffix), rendered));
                    }
                    if let Some(target) = ty.entity_name() {
                        out.push((format!("{}_", f.name), format!("{target}_filter")));
                    }
                }
            }
        }
        out.push(("_change_block".into(), "BlockChangedFilter".into()));
        out.push(("and".into(), format!("[{entity}_filter]")));
        out.push(("or".into(), format!("[{entity}_filter]")));
        out
    }

    /// `<Entity>_orderBy` values: the entity's own non-derived, non-list fields, plus one level of
    /// relation traversal joined by `__`. One level only - the reference has `token0__symbol` and no
    /// `token0__whitelistPools__id`.
    pub fn order_by_values(&self, entity: &str) -> Vec<String> {
        let by_name: BTreeMap<&str, &Entity> =
            self.entities.iter().map(|e| (e.name.as_str(), e)).collect();
        let Some(e) = by_name.get(entity) else {
            return Vec::new();
        };
        let mut v = Vec::new();
        for f in &e.fields {
            // **Every field by bare name, derived and list included.** `Pool_orderBy` carries
            // `swaps`, `ticks`, `poolDayData` and `modifyLiquiditys`; skipping them because they are
            // not orderable columns loses values the reference has.
            v.push(f.name.clone());
            // One level of traversal, and **only the target's scalar fields**. `Tick_orderBy` has 28
            // of `Pool`'s 35 under `pool__`: the seven absent are its five lists and its two
            // relations, `token0` and `token1`. So the nested level does not recurse into relations
            // either, which is a different rule from "one level deep".
            if let FieldType::Entity(target) = &f.ty {
                if f.derived_from.is_some() {
                    continue;
                }
                if let Some(t) = by_name.get(target.as_str()) {
                    for tf in &t.fields {
                        let scalar_ish = matches!(tf.ty, FieldType::Scalar(_) | FieldType::Enum(_));
                        if !scalar_ish {
                            continue;
                        }
                        v.push(format!("{}__{}", f.name, tf.name));
                    }
                }
            }
        }
        v
    }
}

/// Render the generated schema as an introspection response.
///
/// RFC-0053 §Acceptance asks that "generated introspection matches a recorded graph-node reference
/// except for a reviewed, machine-readable divergence list". This is the document that comparison is
/// made against, and it is also what a generated client actually fetches before it will send a query.
///
/// Only the parts a client validates are emitted: `kind`, `name`, `fields` with their arguments and
/// types, `inputFields`, `enumValues`. Descriptions are deliberately absent - graph-node carries the
/// schema author's doc comments there, they are not part of the contract, and the golden diff ignores
/// them.
pub mod introspection {
    use serde_json::{json, Value};

    use super::{lower_first, plural, Schema, BUILTIN_SCALARS, GRAPH_SCALARS};

    /// Turn a rendered type string - `BigInt!`, `[Swap!]!`, `Token` - into introspection's nested
    /// `NON_NULL`/`LIST` wrappers. Written as the inverse of the renderer so the two cannot drift.
    pub fn type_ref(rendered: &str) -> Value {
        if let Some(inner) = rendered.strip_suffix('!') {
            return json!({"kind":"NON_NULL","name":Value::Null,"ofType":type_ref(inner)});
        }
        if rendered.starts_with('[') && rendered.ends_with(']') {
            let inner = &rendered[1..rendered.len() - 1];
            return json!({"kind":"LIST","name":Value::Null,"ofType":type_ref(inner)});
        }
        // A leaf. The kind a client cares about here is the name; `ofType` is null.
        json!({"kind":"SCALAR","name":rendered,"ofType":Value::Null})
    }

    fn arg(name: &str, ty: &str, default: Option<&str>) -> Value {
        json!({"name":name,"type":type_ref(ty),"defaultValue":default})
    }

    /// The five arguments every list field takes - stored or derived, and never `block`.
    fn collection_args(entity: &str) -> Vec<Value> {
        vec![
            arg("skip", "Int", None),
            arg("first", "Int", None),
            arg("orderBy", &format!("{entity}_orderBy"), None),
            arg("orderDirection", "OrderDirection", None),
            arg("where", &format!("{entity}_filter"), None),
        ]
    }

    pub fn render(s: &Schema) -> Value {
        let mut types: Vec<Value> = Vec::new();

        for name in BUILTIN_SCALARS.iter().chain(GRAPH_SCALARS.iter()) {
            types.push(json!({"kind":"SCALAR","name":name}));
        }

        // Fixed supporting types. `Aggregation_*` and `LogLevel` are emitted whether or not the
        // schema uses the feature - measured, not assumed.
        types.push(json!({"kind":"ENUM","name":"OrderDirection",
            "enumValues":[{"name":"asc"},{"name":"desc"}]}));
        types.push(json!({"kind":"ENUM","name":"_SubgraphErrorPolicy_",
            "enumValues":[{"name":"allow"},{"name":"deny"}]}));
        types.push(json!({"kind":"ENUM","name":"Aggregation_interval",
            "enumValues":[{"name":"hour"},{"name":"day"}]}));
        types.push(json!({"kind":"ENUM","name":"Aggregation_current",
            "enumValues":[{"name":"exclude"},{"name":"include"}]}));
        types.push(json!({"kind":"ENUM","name":"LogLevel","enumValues":[
            {"name":"CRITICAL"},{"name":"ERROR"},{"name":"WARNING"},{"name":"INFO"},{"name":"DEBUG"}]}));
        types.push(
            json!({"kind":"INPUT_OBJECT","name":"Block_height","inputFields":[
            arg("hash","Bytes",None), arg("number","Int",None), arg("number_gte","Int",None)]}),
        );
        types.push(
            json!({"kind":"INPUT_OBJECT","name":"BlockChangedFilter","inputFields":[
            arg("number_gte","Int!",None)]}),
        );
        types.push(json!({"kind":"OBJECT","name":"_Meta_","fields":[
            {"name":"block","args":[],"type":type_ref("_Block_!")},
            {"name":"deployment","args":[],"type":type_ref("String!")},
            {"name":"hasIndexingErrors","args":[],"type":type_ref("Boolean!")}]}));

        for (name, values) in &s.enums {
            let vs: Vec<Value> = values.iter().map(|v| json!({"name":v})).collect();
            types.push(json!({"kind":"ENUM","name":name,"enumValues":vs}));
        }

        for e in &s.entities {
            let fields: Vec<Value> = s
                .object_fields(&e.name)
                .into_iter()
                .map(|(n, ty, args)| {
                    // A list field's arguments are typed against the *target* entity, which is the
                    // inner type rather than this one.
                    let target = e
                        .fields
                        .iter()
                        .find(|f| f.name == n)
                        .and_then(|f| f.ty.entity_name())
                        .unwrap_or(e.name.as_str())
                        .to_string();
                    let a: Vec<Value> = if args.is_empty() {
                        Vec::new()
                    } else {
                        collection_args(&target)
                    };
                    json!({"name":n,"args":a,"type":type_ref(&ty)})
                })
                .collect();
            types.push(json!({"kind":"OBJECT","name":e.name,"fields":fields}));

            let inputs: Vec<Value> = s
                .filter_fields(&e.name)
                .into_iter()
                .map(|(n, ty)| arg(&n, &ty, None))
                .collect();
            types.push(
                json!({"kind":"INPUT_OBJECT","name":format!("{}_filter",e.name),
                "inputFields":inputs}),
            );

            let vals: Vec<Value> = s
                .order_by_values(&e.name)
                .into_iter()
                .map(|v| json!({"name":v}))
                .collect();
            types.push(json!({"kind":"ENUM","name":format!("{}_orderBy",e.name),
                "enumValues":vals}));
        }

        let mut roots: Vec<Value> = Vec::new();
        for e in &s.entities {
            roots.push(
                json!({"name":lower_first(&e.name),"type":type_ref(&e.name),"args":[
                arg("id","ID!",None),
                arg("block","Block_height",None),
                arg("subgraphError","_SubgraphErrorPolicy_!",Some("deny"))]}),
            );
            let mut a = vec![
                arg("skip", "Int", Some("0")),
                arg("first", "Int", Some("100")),
                arg("orderBy", &format!("{}_orderBy", e.name), None),
                arg("orderDirection", "OrderDirection", None),
                arg("where", &format!("{}_filter", e.name), None),
            ];
            a.push(arg("block", "Block_height", None));
            a.push(arg("subgraphError", "_SubgraphErrorPolicy_!", Some("deny")));
            roots.push(json!({"name":plural(&e.name),
                "type":type_ref(&format!("[{}!]!",e.name)),"args":a}));
        }
        roots.push(json!({"name":"_meta","type":type_ref("_Meta_"),
            "args":[arg("block","Block_height",None)]}));
        types.push(json!({"kind":"OBJECT","name":"Query","fields":roots}));

        json!({"__schema":{"queryType":{"name":"Query"},"types":types}})
    }
}
