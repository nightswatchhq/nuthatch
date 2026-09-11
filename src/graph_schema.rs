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
    // Descriptions are neutralised before anything else looks at the text. A GraphQL block string is
    // a legal place to write `type Fake @entity {`, and both `starts_decl` (line-start only) and
    // `match_brace` (skips `"…"`, not `"""…"""`) would have read that as syntax - inventing an
    // entity, or matching the wrong closing brace (Jules on #1282). One pre-pass rather than three
    // functions each learning about block strings, and byte offsets are preserved so every slice
    // below still lines up.
    //
    // Ordinary strings are left alone on purpose: `@derivedFrom(field: "pool")` is one, and its
    // contents are load-bearing.
    let blanked = blank_block_strings(text);
    let text: &str = &blanked;
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
        // Anything else with a body gets skipped whole - `interface`, `input`, `union`, a `schema`
        // block. `starts_decl` is a token test rather than a line test now, so without this a field
        // named `type` or `enum` inside one of those blocks would read as a declaration. The arms
        // above already consume their own bodies, so reaching a `{` here means the block belongs to a
        // declaration this parser does not read.
        if b[i] == b'{' {
            match match_brace(text, i) {
                Some(c) => {
                    i = c + 1;
                    continue;
                }
                None => bail!("unclosed block in schema.graphql"),
            }
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
/// Is `kw` a declaration keyword starting at `i`?
///
/// A token boundary, not a line start. This used to require that nothing but whitespace preceded `i`
/// on the same physical line, so `type A @entity { id: ID! } type B @entity { id: ID! }` - legal
/// GraphQL - parsed `A` and then never saw `B`: an entity, its roots, its filters and its object type
/// all silently absent from the generated schema, and a client's query against `B` answering
/// `UnknownRoot` (Jules on #1282). The caller only tests this at the top level, because it skips every
/// braced block it does not recognise, so a field named `type` inside an `interface` or `input` cannot
/// be mistaken for one.
fn starts_decl(text: &str, i: usize, kw: &str) -> bool {
    if !text.is_char_boundary(i) || !text[i..].starts_with(kw) {
        return false;
    }
    // Nothing, or something that cannot be part of a name. `}type B` is as legal as `} type B`.
    text[..i]
        .chars()
        .next_back()
        .is_none_or(|c| !(c.is_alphanumeric() || c == '_'))
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
    // Whitespace-separated, not line-separated: `enum OrderDirection { asc desc }` is legal, and
    // reading it a line at a time yields the single value `asc desc`.
    let values = text[open + 1..close]
        .lines()
        .flat_map(|l| strip_comment(l).split_whitespace())
        .filter(|v| !v.is_empty() && *v != ",")
        .map(|v| v.trim_end_matches(',').to_string())
        .filter(|v| !v.is_empty())
        .collect();
    Ok((name, values, close + 1))
}

/// The fields of one type body.
///
/// **Declaration-oriented, not line-oriented.** GraphQL puts no significance on newlines, and
/// `type Token @entity { id: ID! symbol: String! }` is a perfectly ordinary way to write a schema.
/// Reading one field per line took `id` and silently dropped everything after it, which would have
/// made the generated introspection *omit* fields a client then asks for. The recorded reference
/// schema happens to be one field per line, so no golden test could see it.
fn parse_fields(body: &str) -> Vec<Field> {
    // Comments are the one thing that is line-bounded, so they go first.
    let cleaned: String = body
        .lines()
        .map(strip_comment)
        .collect::<Vec<_>>()
        .join("\n");
    let b = cleaned.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let is_name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        // Whitespace and commas separate declarations and mean nothing else.
        if b[i].is_ascii_whitespace() || b[i] == b',' {
            i += 1;
            continue;
        }
        if !is_name(b[i]) {
            i += 1;
            continue;
        }
        let ns = i;
        while i < b.len() && is_name(b[i]) {
            i += 1;
        }
        let name = &cleaned[ns..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        // Not a field declaration - a stray word, or an interface's `implements` list. Skip it
        // rather than consume the next identifier as its type.
        if i >= b.len() || b[i] != b':' {
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let ts = i;
        while i < b.len() && (is_name(b[i]) || matches!(b[i], b'[' | b']' | b'!')) {
            i += 1;
        }
        let decl = &cleaned[ts..i];
        // Directives belong to this field and carry `@derivedFrom`, so read them before moving on -
        // and read the parenthesised argument as a unit, or a `)` would end the scan early.
        let ds = i;
        loop {
            let mut j = i;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= b.len() || b[j] != b'@' {
                break;
            }
            i = j + 1;
            while i < b.len() && is_name(b[i]) {
                i += 1;
            }
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < b.len() && b[i] == b'(' {
                let mut depth = 0usize;
                while i < b.len() {
                    match b[i] {
                        b'(' => depth += 1,
                        b')' => {
                            depth -= 1;
                            i += 1;
                            if depth == 0 {
                                break;
                            }
                            continue;
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
        }
        let tail = &cleaned[ds..i];
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

/// Replace the contents of every `"""…"""` block string with spaces, keeping newlines and the
/// overall byte length so every offset into the text stays valid.
fn blank_block_strings(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i..].starts_with(b"\"\"\"") {
            out.extend_from_slice(b"\"\"\"");
            i += 3;
            while i < b.len() && !b[i..].starts_with(b"\"\"\"") {
                // Newlines survive, so a line-start test still sees the same lines.
                out.push(if b[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < b.len() {
                out.extend_from_slice(b"\"\"\"");
                i += 3;
            }
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    // Only ASCII bytes were substituted, so this cannot split a multi-byte character.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
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
/// **These sets are not uniform, `Bytes` is not `String`, and an enum is not a scalar.** `Bytes` has no
/// `_starts_with`, no `_ends_with` and no `_nocase` variant anywhere - ten operators against `String`'s
/// twenty. Generalising the string set over `Bytes` would advertise ten operators graph-node does not
/// have, and a client validating against us would then send queries the real endpoint refuses.
///
/// Checked against graph-node's own `field_filter_ops` and `field_enum_filter_input_values`
/// (`graph/src/schema/api.rs`, MIT/Apache, so readable), which is a better reference than a recording:
/// it gives the complete table for every type at once rather than whichever shapes one subgraph
/// happens to use. The recorded Uniswap V4 schema declares **no author enum at all** - every enum in it
/// is a graph-node builtin or a generated `_orderBy` - so the enum row below was generalised from the
/// numeric set and was wrong by four operators (#1306). Confirmed on three unrelated live deployments.
pub fn filter_suffixes(scalar: &str, is_enum: bool) -> Vec<&'static str> {
    let ordered = ["", "_not", "_gt", "_lt", "_gte", "_lte", "_in", "_not_in"];
    if is_enum {
        // `field_enum_filter_input_values` returns exactly these. An enum has no ordering, so there is
        // no `_gt`/`_lt`/`_gte`/`_lte` to offer.
        return vec!["", "_not", "_in", "_not_in"];
    }
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
        // BigInt, BigDecimal, Int, Int8, Timestamp.
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
    /// | scalar | that scalar's operator set, `_in`/`_not_in` in list form |
    /// | enum | **four** - bare, `_not`, `_in`, `_not_in`. An enum has no ordering (#1306) |
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
                    for suffix in filter_suffixes(scalar, self.enums.contains_key(scalar)) {
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
        // A leaf. The kind is filled in by `resolve_kinds` from the document's own type list, because
        // nothing here can know whether `Token` is an object, an enum or an input object - and
        // guessing `SCALAR` advertised every relation, every `orderBy` and every `where` as a scalar,
        // which a validating client rejects before it executes anything (Jules on #1282).
        json!({"kind":UNRESOLVED,"name":rendered,"ofType":Value::Null})
    }

    /// The placeholder `type_ref` leaves for [`resolve_kinds`] to replace.
    const UNRESOLVED: &str = "__UNRESOLVED__";

    /// Rewrite every leaf type reference's `kind` to the kind its own declaration carries, returning
    /// the names no declaration covers.
    ///
    /// **Derived from the rendered type list, not classified by name.** A hand-written "`_filter`
    /// means input object, `_orderBy` means enum" table is one more list to keep in step with the
    /// renderer; taking the kind from the declaration cannot drift from it by construction.
    ///
    /// A reference to an undeclared type makes the whole document invalid - `_Meta_.block` pointed at
    /// `_Block_` while `_Block_` was never emitted - so the caller treats a non-empty result as a bug.
    fn resolve_kinds(
        doc: &mut Value,
        kinds: &std::collections::BTreeMap<String, String>,
    ) -> Vec<String> {
        let mut dangling = Vec::new();
        walk(doc, kinds, &mut dangling);
        dangling.sort();
        dangling.dedup();
        dangling
    }

    fn walk(
        v: &mut Value,
        kinds: &std::collections::BTreeMap<String, String>,
        dangling: &mut Vec<String>,
    ) {
        match v {
            Value::Array(a) => a.iter_mut().for_each(|x| walk(x, kinds, dangling)),
            Value::Object(o) => {
                if o.get("kind").and_then(Value::as_str) == Some(UNRESOLVED) {
                    let name = o
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    match kinds.get(&name) {
                        Some(k) => {
                            o.insert("kind".into(), Value::String(k.clone()));
                        }
                        None => {
                            dangling.push(name);
                            o.insert("kind".into(), Value::String("SCALAR".into()));
                        }
                    }
                }
                o.iter_mut().for_each(|(_, x)| walk(x, kinds, dangling));
            }
            _ => {}
        }
    }

    /// A `__Type` with every key a standard client selects, built **complete at the construction site**.
    ///
    /// Four constructors rather than fifteen `json!` literals plus a pass that fills in what they left
    /// out: a type cannot be built half-shaped, so there is nothing for a later reader - human or
    /// reviewer - to have to find two hundred lines away before believing the document is whole.
    ///
    /// The defaults are the reference's, measured: `description` null throughout (graph-node carries the
    /// schema author's doc comments there, which are not part of the contract), `interfaces` `[]` on an
    /// object and null elsewhere, `possibleTypes` always null because a generated subgraph schema has
    /// neither unions nor interfaces.
    fn ty(kind: &str, name: &str, payload: Option<(&str, Value)>) -> Value {
        let mut t = json!({
            "kind": kind,
            "name": name,
            "description": Value::Null,
            "fields": Value::Null,
            "inputFields": Value::Null,
            "interfaces": if kind == "OBJECT" { json!([]) } else { Value::Null },
            "enumValues": Value::Null,
            "possibleTypes": Value::Null,
        });
        if let Some((key, v)) = payload {
            t[key] = v;
        }
        t
    }

    fn scalar_type(name: &str) -> Value {
        ty("SCALAR", name, None)
    }

    fn object_type(name: &str, fields: Vec<Value>) -> Value {
        ty("OBJECT", name, Some(("fields", Value::Array(fields))))
    }

    fn input_object_type(name: &str, input_fields: Vec<Value>) -> Value {
        ty(
            "INPUT_OBJECT",
            name,
            Some(("inputFields", Value::Array(input_fields))),
        )
    }

    fn enum_type(name: &str, values: &[&str]) -> Value {
        ty(
            "ENUM",
            name,
            Some((
                "enumValues",
                Value::Array(values.iter().map(|v| enum_value(v)).collect()),
            )),
        )
    }

    /// An enum value, with the deprecation pair a client always selects.
    fn enum_value(name: &str) -> Value {
        json!({"name":name,"description":Value::Null,
               "isDeprecated":false,"deprecationReason":Value::Null})
    }

    /// A field on an object type, likewise complete.
    fn field(name: &str, args: Vec<Value>, ty: Value) -> Value {
        json!({"name":name,"description":Value::Null,"args":args,"type":ty,
               "isDeprecated":false,"deprecationReason":Value::Null})
    }

    fn arg(name: &str, ty: &str, default: Option<&str>) -> Value {
        json!({"name":name,"description":Value::Null,
               "type":type_ref(ty),"defaultValue":default})
    }

    /// The five arguments every list field takes - stored or derived, and never `block`.
    ///
    /// **With the same defaults as the root plural**: `skip = 0` and `first = 100`. A name-only
    /// comparison could not see this and the argument-level diff found it immediately. It matters
    /// because a client relies on the server's default page size rather than sending one.
    fn collection_args(entity: &str) -> Vec<Value> {
        vec![
            arg("skip", "Int", Some("0")),
            arg("first", "Int", Some("100")),
            arg("orderBy", &format!("{entity}_orderBy"), None),
            arg("orderDirection", "OrderDirection", None),
            arg("where", &format!("{entity}_filter"), None),
        ]
    }

    pub fn render(s: &Schema) -> Value {
        let mut types: Vec<Value> = Vec::new();

        for name in BUILTIN_SCALARS.iter().chain(GRAPH_SCALARS.iter()) {
            types.push(scalar_type(name));
        }

        // Fixed supporting types. `Aggregation_*` and `LogLevel` are emitted whether or not the
        // schema uses the feature - measured, not assumed.
        types.push(enum_type("OrderDirection", &["asc", "desc"]));
        types.push(enum_type("_SubgraphErrorPolicy_", &["allow", "deny"]));
        types.push(enum_type("Aggregation_interval", &["hour", "day"]));
        types.push(enum_type("Aggregation_current", &["exclude", "include"]));
        types.push(enum_type(
            "LogLevel",
            &["CRITICAL", "ERROR", "WARNING", "INFO", "DEBUG"],
        ));
        types.push(input_object_type(
            "Block_height",
            vec![
                arg("hash", "Bytes", None),
                arg("number", "Int", None),
                arg("number_gte", "Int", None),
            ],
        ));
        types.push(input_object_type(
            "BlockChangedFilter",
            vec![arg("number_gte", "Int!", None)],
        ));
        // `_Block_` is referenced by `_Meta_.block`, and `_meta` is a root this endpoint answers, so
        // omitting it left the served document pointing at a type it did not define.
        types.push(object_type(
            "_Block_",
            vec![
                field("hash", vec![], type_ref("Bytes")),
                field("number", vec![], type_ref("Int!")),
                field("timestamp", vec![], type_ref("Int")),
                field("parentHash", vec![], type_ref("Bytes")),
            ],
        ));
        // The `_logs` family. Emitted because the generated schema has to be *complete* for a client
        // that validates it, even though a nest has no mapping logs to return: a schema declaring
        // `LogLevel` and no `_Log_` is not a schema graph-node would serve.
        types.push(object_type(
            "_LogArgument_",
            vec![
                field("key", vec![], type_ref("String!")),
                field("value", vec![], type_ref("String!")),
            ],
        ));
        types.push(object_type(
            "_LogMeta_",
            vec![
                field("module", vec![], type_ref("String!")),
                field("line", vec![], type_ref("Int!")),
                field("column", vec![], type_ref("Int!")),
            ],
        ));
        types.push(object_type(
            "_Log_",
            vec![
                field("id", vec![], type_ref("String!")),
                field("subgraphId", vec![], type_ref("String!")),
                field("timestamp", vec![], type_ref("String!")),
                field("level", vec![], type_ref("LogLevel!")),
                field("text", vec![], type_ref("String!")),
                field("arguments", vec![], type_ref("[_LogArgument_!]!")),
                field("meta", vec![], type_ref("_LogMeta_!")),
            ],
        ));
        types.push(object_type(
            "_Meta_",
            vec![
                field("block", vec![], type_ref("_Block_!")),
                field("deployment", vec![], type_ref("String!")),
                field("hasIndexingErrors", vec![], type_ref("Boolean!")),
            ],
        ));

        for (name, values) in &s.enums {
            let vs: Vec<&str> = values.iter().map(String::as_str).collect();
            types.push(enum_type(name, &vs));
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
                    field(&n, a, type_ref(&ty))
                })
                .collect();
            types.push(object_type(&e.name, fields));

            let inputs: Vec<Value> = s
                .filter_fields(&e.name)
                .into_iter()
                .map(|(n, ty)| arg(&n, &ty, None))
                .collect();
            types.push(input_object_type(&format!("{}_filter", e.name), inputs));

            let vals = s.order_by_values(&e.name);
            let vals: Vec<&str> = vals.iter().map(String::as_str).collect();
            types.push(enum_type(&format!("{}_orderBy", e.name), &vals));
        }

        let mut roots: Vec<Value> = Vec::new();
        for e in &s.entities {
            roots.push(field(
                &lower_first(&e.name),
                vec![
                    arg("id", "ID!", None),
                    arg("block", "Block_height", None),
                    arg("subgraphError", "_SubgraphErrorPolicy_!", Some("deny")),
                ],
                type_ref(&e.name),
            ));
            let mut a = vec![
                arg("skip", "Int", Some("0")),
                arg("first", "Int", Some("100")),
                arg("orderBy", &format!("{}_orderBy", e.name), None),
                arg("orderDirection", "OrderDirection", None),
                arg("where", &format!("{}_filter", e.name), None),
            ];
            a.push(arg("block", "Block_height", None));
            a.push(arg("subgraphError", "_SubgraphErrorPolicy_!", Some("deny")));
            roots.push(field(
                &plural(&e.name),
                a,
                type_ref(&format!("[{}!]!", e.name)),
            ));
        }
        roots.push(field(
            "_meta",
            vec![arg("block", "Block_height", None)],
            type_ref("_Meta_"),
        ));
        // `_logs` completes the root inventory `Schema::root_field_names` already claims. The
        // arguments and their defaults are the reference's, `orderDirection` included - it defaults to
        // `desc` here and nowhere else in the schema.
        roots.push(field(
            "_logs",
            vec![
                arg("level", "LogLevel", None),
                arg("from", "String", None),
                arg("to", "String", None),
                arg("search", "String", None),
                arg("first", "Int", Some("100")),
                arg("skip", "Int", Some("0")),
                arg("orderDirection", "OrderDirection", Some("desc")),
            ],
            type_ref("[_Log_!]!"),
        ));
        types.push(object_type("Query", roots));

        // Every leaf reference's `kind` comes from the declaration it names. A name no declaration
        // covers leaves the document invalid and there is no sensible way to serve it, so it panics
        // here rather than reaching a client: only the renderer can create the condition, and the
        // golden test covers it.
        let kinds: std::collections::BTreeMap<String, String> = types
            .iter()
            .filter_map(|t| {
                Some((
                    t.get("name")?.as_str()?.to_string(),
                    t.get("kind")?.as_str()?.to_string(),
                ))
            })
            .collect();
        // The `__Schema` fields a standard client asks for, all of them. Omitting them meant a
        // normal generated introspection request got a response missing fields it had selected, and
        // the recorded reference could not catch it because the *recording query* never asked for them
        // either - a golden diff cannot compare what was never captured (Jules on #1282).
        //
        // Measured against the live deployment: both operation types are `null`, which agrees with
        // this compiler refusing `mutation` and `subscription` outright, and there are exactly five
        // directives - GraphQL's own `skip` and `include` plus graph-node's `entity`, `subgraphId` and
        // `derivedFrom`.
        let bool_arg = |name: &str| {
            json!({"name":name,"description":Value::Null,
                   "type":type_ref("Boolean!"),"defaultValue":Value::Null})
        };
        let string_arg = |name: &str| {
            json!({"name":name,"description":Value::Null,
                   "type":type_ref("String!"),"defaultValue":Value::Null})
        };
        const ON_SELECTION: [&str; 3] = ["FIELD", "FRAGMENT_SPREAD", "INLINE_FRAGMENT"];
        let directives = json!([
            {"name":"skip","description":Value::Null,"locations":ON_SELECTION,
             "args":[bool_arg("if")]},
            {"name":"include","description":Value::Null,"locations":ON_SELECTION,
             "args":[bool_arg("if")]},
            {"name":"entity","description":Value::Null,"locations":["OBJECT"],"args":[]},
            {"name":"subgraphId","description":Value::Null,"locations":["OBJECT"],
             "args":[string_arg("id")]},
            {"name":"derivedFrom","description":Value::Null,"locations":["FIELD_DEFINITION"],
             "args":[string_arg("field")]},
        ]);
        let mut doc = json!({"__schema":{
            "queryType":{"name":"Query"},
            // A nest serves queries only, which is why `parse` refuses the other two outright.
            "mutationType": Value::Null,
            "subscriptionType": Value::Null,
            "types": types,
            "directives": directives,
        }});
        let dangling = resolve_kinds(&mut doc, &kinds);
        assert!(
            dangling.is_empty(),
            "the generated schema references types it does not declare: {dangling:?}"
        );
        doc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GraphQL puts no significance on newlines, so a schema may put a whole type on one line.
    ///
    /// Found by an S2 join test failing as though `Token.symbol` did not exist. The parse was
    /// line-oriented: it took `id` and silently dropped every field after it, which would have made
    /// the generated introspection **omit** fields a client goes on to ask for. No golden test could
    /// have caught it, because the recorded reference schema is one field per line throughout.
    #[test]
    fn a_type_written_on_one_line_keeps_all_of_its_fields() {
        let s = parse(concat!(
            "type Token @entity { id: ID! symbol: String! decimals: Int! }\n",
            "enum Dir { asc desc }\n",
            "type Pool @entity { id: ID! token0: Token! ",
            "swaps: [Swap!]! @derivedFrom(field: \"pool\") dir: Dir }\n",
            "type Swap @entity { id: ID! pool: Pool! }\n",
        ))
        .expect("a one-line schema parses");

        let names = |e: &Entity| {
            e.fields
                .iter()
                .map(|f| f.name.clone())
                .collect::<Vec<String>>()
        };
        let token = s.entities.iter().find(|e| e.name == "Token").unwrap();
        assert_eq!(names(token), ["id", "symbol", "decimals"]);

        let pool = s.entities.iter().find(|e| e.name == "Pool").unwrap();
        assert_eq!(
            names(pool),
            ["id", "token0", "swaps", "dir"],
            "a directive with a parenthesised argument must not end the scan"
        );
        let swaps = pool.fields.iter().find(|f| f.name == "swaps").unwrap();
        assert_eq!(swaps.derived_from.as_deref(), Some("pool"));
        assert!(swaps.inner_non_null, "[Swap!]! keeps its inner bang");
        let token0 = pool.fields.iter().find(|f| f.name == "token0").unwrap();
        assert!(
            matches!(&token0.ty, FieldType::Entity(t) if t == "Token"),
            "{:?}",
            token0.ty
        );
        let dir = pool.fields.iter().find(|f| f.name == "dir").unwrap();
        assert!(
            matches!(&dir.ty, FieldType::Enum(e) if e == "Dir"),
            "{:?}",
            dir.ty
        );

        // The same fault, and the same fix, in an enum body: one line yielded the single value
        // "asc desc", which is not a value any client will ever send.
        assert_eq!(s.enums.get("Dir").unwrap(), &["asc", "desc"]);
    }

    /// A GraphQL block string is a legal place to write something that looks like a declaration.
    ///
    /// The generated `<Entity>_filter` gives an enum field four inputs, and the `_in` form is a list of
    /// the enum.
    ///
    /// Separate from the `filter_suffixes` assertion in `tests/graph_schema_golden.rs`, because that one
    /// checks the table and this one checks that `filter_fields` consults it. A mutation making the
    /// generation site pass `is_enum: false` survived until this existed: the helper was right and the
    /// document still advertised eight (#1306).
    #[test]
    fn the_generated_filter_gives_an_enum_field_four_inputs() {
        let s = parse(
            "enum OrderType { order0 order1 }\n\
             type Order @entity { id: ID! type: OrderType! size: BigInt! }\n",
        )
        .unwrap();
        let fields = s.filter_fields("Order");
        let ops: Vec<(String, String)> = fields
            .iter()
            .filter(|(n, _)| n == "type" || n.starts_with("type_"))
            .cloned()
            .collect();
        assert_eq!(
            ops,
            vec![
                ("type".to_string(), "OrderType".to_string()),
                ("type_not".to_string(), "OrderType".to_string()),
                ("type_in".to_string(), "[OrderType!]".to_string()),
                ("type_not_in".to_string(), "[OrderType!]".to_string()),
            ],
            "four inputs, and `_in`/`_not_in` take `[OrderType!]`"
        );

        // The numeric field beside it keeps its eight, so this is about the enum and not about the
        // whole filter shrinking.
        let numeric = fields
            .iter()
            .filter(|(n, _)| n == "size" || n.starts_with("size_"))
            .count();
        assert_eq!(numeric, 8, "BigInt keeps the ordered set: {fields:?}");
    }

    /// A declaration boundary is lexical, not a line start.
    ///
    /// `type A @entity { id: ID! } type B @entity { id: ID! }` is legal GraphQL and the scan used to
    /// parse `A`, resume inside the same line, and never see `B` - so an entity, its roots, its filters
    /// and its object type were all absent from the generated schema and a client querying `B` got
    /// `UnknownRoot`. The same omission with no error anywhere is what made the earlier line-oriented
    /// field parser dangerous, and this is the outer half of it.
    #[test]
    fn declarations_sharing_a_line_are_all_seen() {
        let s = parse(
            "type A @entity { id: ID! name: String! } type B @entity { id: ID! } \
             enum Dir { asc desc } enum Side { buy sell }",
        )
        .unwrap();
        let names: Vec<&str> = s.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"], "both types must be read");
        assert_eq!(
            s.entities[0]
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "name"],
            "and the first type's fields must survive the one-line form"
        );
        let mut enums: Vec<&str> = s.enums.keys().map(String::as_str).collect();
        enums.sort();
        assert_eq!(enums, vec!["Dir", "Side"], "both enums must be read");
        assert_eq!(
            s.enums.get("Dir").map(Vec::as_slice),
            Some(["asc".to_string(), "desc".to_string()].as_slice())
        );

        // `}type B` with no space is as legal as `} type B`.
        let s = parse("type A @entity { id: ID! }type B @entity { id: ID! }").unwrap();
        assert_eq!(
            s.entities.len(),
            2,
            "a missing space is not a declaration end"
        );

        // A token test rather than a line test would read `type` and `enum` inside a block this parser
        // does not understand as declarations, so those blocks are skipped whole.
        let s = parse(
            "interface Named { type: String! enum: Int! }\n\
             input Filter { type : String enum : Int }\n\
             type Real @entity { id: ID! }\n",
        )
        .unwrap();
        let names: Vec<&str> = s.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Real"],
            "a field named `type` or `enum` is not a declaration"
        );
        assert!(s.enums.is_empty(), "nor is it an enum: {:?}", s.enums);

        // A name that merely *ends* in `type` or `enum` is not a declaration, and this is the half two
        // mutations survived without. `union my_type = A | B` puts `type ` one character after an
        // underscore at the top level, where there is no block to skip and no line start to save us:
        // a permissive boundary test matches there, then hunts for the next `{`, and swallows the real
        // declaration's body whole.
        let s = parse(
            "union Footype = A | B\n\
             union my_type = A | B\n\
             union my_enum = A | B\n\
             type Real @entity { id: ID! liquidity: BigInt! }\n",
        )
        .unwrap();
        let names: Vec<&str> = s.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["Real"],
            "a name ending in `type`/`enum` must not be read as a declaration"
        );
        assert_eq!(
            s.entities[0]
                .fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "liquidity"],
            "and the real declaration's body must be intact, not swallowed"
        );
        assert!(
            s.enums.is_empty(),
            "`my_enum` is a union, not an enum: {:?}",
            s.enums
        );
    }

    /// `starts_decl` only tested for a line start and `match_brace` skipped `"…"` but not `"""…"""`,
    /// so a description could invent an entity or send the brace matcher to the wrong closing brace.
    #[test]
    fn a_declaration_inside_a_description_is_not_a_declaration() {
        let s = parse(concat!(
            "\"\"\"\n",
            "A description. It may legally contain:\n",
            "    type Fake @entity {\n",
            "      id: ID!\n",
            "    }\n",
            "and an unbalanced brace } as prose.\n",
            "\"\"\"\n",
            "type Real @entity { id: ID! n: BigInt! }\n",
        ))
        .expect("a described schema parses");
        assert_eq!(
            s.entities
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            ["Real"],
            "a description must not become an entity"
        );
        let real = &s.entities[0];
        assert_eq!(
            real.fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["id", "n"],
            "and the brace matcher must still find the real type's body"
        );
    }

    /// Every leaf `kind` in the rendered document is a real GraphQL kind.
    ///
    /// `type_ref` leaves a placeholder for `resolve_kinds` to fill from the type list. One left
    /// behind is a type reference nothing declares, which makes the whole document invalid to a
    /// client that resolves it.
    #[test]
    fn no_rendered_type_reference_is_left_unresolved() {
        let s = parse(concat!(
            "type Pool @entity { id: ID! token0: Token! dir: Dir }\n",
            "type Token @entity { id: ID! symbol: String! }\n",
            "enum Dir { asc desc }\n",
        ))
        .unwrap();
        let doc = introspection::render(&s);
        let mut kinds = std::collections::BTreeSet::new();
        fn walk(v: &serde_json::Value, out: &mut std::collections::BTreeSet<String>) {
            match v {
                serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
                serde_json::Value::Object(o) => {
                    if let Some(k) = o.get("kind").and_then(|k| k.as_str()) {
                        out.insert(k.to_string());
                    }
                    o.values().for_each(|x| walk(x, out));
                }
                _ => {}
            }
        }
        walk(&doc, &mut kinds);
        let legal: std::collections::BTreeSet<String> = [
            "SCALAR",
            "OBJECT",
            "ENUM",
            "INPUT_OBJECT",
            "NON_NULL",
            "LIST",
            "INTERFACE",
            "UNION",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let strays: Vec<&String> = kinds.difference(&legal).collect();
        assert!(strays.is_empty(), "unresolved or invalid kinds: {strays:?}");

        // And the kinds a client actually validates against are the right ones.
        let types = doc["__schema"]["types"].as_array().unwrap();
        let pool = types.iter().find(|t| t["name"] == "Pool").unwrap();
        let f = |n: &str| {
            pool["fields"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(
            f("token0")["type"]["ofType"]["kind"],
            "OBJECT",
            "a relation is an object"
        );
        assert_eq!(f("dir")["type"]["kind"], "ENUM", "an enum field is an enum");
        assert_eq!(f("id")["type"]["ofType"]["kind"], "SCALAR");
        let q = types.iter().find(|t| t["name"] == "Query").unwrap();
        let pools = q["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == "pools")
            .unwrap();
        let arg = |n: &str| {
            pools["args"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["name"] == n)
                .unwrap()
                .clone()
        };
        assert_eq!(arg("where")["type"]["kind"], "INPUT_OBJECT");
        assert_eq!(arg("orderBy")["type"]["kind"], "ENUM");
        assert_eq!(arg("first")["type"]["kind"], "SCALAR");
    }
}
