//! RFC-0044 S1: classify a subgraph's `schema.graphql` and mappings, emit the port report.
//!
//! Nothing here runs in the data path and nothing executes AssemblyScript. The mappings are
//! read as text. The report is the deliverable; scaffolding a nest is RFC-0044 S2.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use yaml_rust2::{Yaml, YamlLoader};

/// RFC-0044 §5a. Ordered so `max` is the most severe class a field can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Exact,
    CallDerived,
    FixedPoint,
    Unreachable,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Exact => "exact",
            Class::CallDerived => "call-derived",
            Class::FixedPoint => "fixed-point",
            Class::Unreachable => "unreachable",
        }
    }

    pub fn heading(self) -> &'static str {
        match self {
            Class::Exact => "exact",
            Class::CallDerived => "call-derived",
            Class::FixedPoint => "fixed point",
            Class::Unreachable => "unreachable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub file: String,
    pub line: usize,
}

impl Citation {
    fn display(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub entity: String,
    pub field: String,
    pub class: Class,
    pub citation: Citation,
    pub reason: String,
}

impl FieldRow {
    pub fn name(&self) -> String {
        format!("{}.{}", self.entity, self.field)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub source: String,
    pub fields: Vec<FieldRow>,
}

impl Report {
    pub fn counts(&self) -> [usize; 4] {
        let mut n = [0; 4];
        for f in &self.fields {
            n[f.class as usize] += 1;
        }
        n
    }
}

/// Classify `dir` and print the report to stdout. Hidden `nuthatch port-report` entry point.
pub fn print_report(dir: &Path) -> Result<()> {
    let report = classify_dir(dir)?;
    print!("{}", render_report(&report));
    Ok(())
}

/// Read `schema.graphql` and the mappings under `dir`, classify every entity field.
pub fn classify_dir(dir: &Path) -> Result<Report> {
    let schema_path = dir.join("schema.graphql");
    let schema_text = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("read {}", schema_path.display()))?;
    let schema = parse_schema(&schema_text)?;
    let mappings = load_mappings(dir)?;
    let fields = classify(&schema, &mappings);
    let source = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".")
        .to_string();
    Ok(Report { source, fields })
}

pub fn render_report(report: &Report) -> String {
    let mut out = String::new();
    out.push_str("# Port report\n\n");
    out.push_str(
        "Every entity field in `schema.graphql`, classified against the mappings. Nothing has been \
         scaffolded. A field this report calls exact must match byte-for-byte; a field it names as \
         fixed point, call-derived or unreachable will not reproduce, and the reason is on that row. \
         An unpredicted divergence is a defect in this report.\n\n",
    );
    out.push_str(&format!("Source: `{}`\n\n", report.source));
    out.push_str(
        "Path: `--from-subgraph`. The proxy trap does not apply: the manifest pins implementation ABIs.\n\n",
    );

    let [exact, call, fixed, unreachable] = report.counts();
    out.push_str("## Summary\n\n");
    out.push_str("| Class | Count |\n|---|---:|\n");
    out.push_str(&format!("| exact | {exact} |\n"));
    out.push_str(&format!("| call-derived | {call} |\n"));
    out.push_str(&format!("| fixed point | {fixed} |\n"));
    out.push_str(&format!("| unreachable | {unreachable} |\n\n"));

    let groups = [
        (
            Class::Exact,
            "A pure function of decoded events. Port as a view or entity; byte-identical.",
        ),
        (
            Class::CallDerived,
            "Reads contract state at the row's block. Port as `[[calls]]` (RFC-0038 §3). Needs `--state-rpc`.",
        ),
        (
            Class::FixedPoint,
            "Reads back stored entity output. A nest can converge; the number will be different. This field will not reproduce.",
        ),
        (
            Class::Unreachable,
            "Will not be ported. This field will not reproduce.",
        ),
    ];
    for (class, blurb) in groups {
        out.push_str(&format!("## {}\n\n", class.heading()));
        out.push_str(blurb);
        out.push_str("\n\n");
        let rows: Vec<&FieldRow> = report.fields.iter().filter(|f| f.class == class).collect();
        if rows.is_empty() {
            out.push_str("_none_\n\n");
            continue;
        }
        out.push_str("| Field | Citation | Why |\n|---|---|---|\n");
        for row in rows {
            out.push_str(&format!(
                "| `{}` | `{}` | {} |\n",
                row.name(),
                row.citation.display(),
                escape_table(&row.reason)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Traps\n\n");
    out.push_str(
        "These are the ones the three hand ports paid for, and they apply on this path:\n\n",
    );
    out.push_str(
        "- The proxy trap applies to `nuthatch init 0xAddr` and **not** to `--from-subgraph`. \
         This report is the latter.\n",
    );
    out.push_str(
        "- `[[factories]] watch` takes a contract alias or template name, never an address, \
         whatever `config-reference.md` shows.\n",
    );
    out.push_str(
        "- One proxy may need several ABIs across its history. Horizon renamed every staking \
         event; a nest carrying only the current ABI loses 366 million blocks silently.\n",
    );
    out.push_str(
        "- The snake_caser explodes acronyms: `ServiceURIUpdate` becomes `service_u_r_i_update`.\n",
    );
    out.push_str(
        "- Verify against the chain (`cast call` on the canonical getters), not the gateway. \
         On-chain sentinels (`deactivationRound = 2^256-1`) map to a view's `null`.\n",
    );
    out
}

fn escape_table(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

// ── Schema ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Schema {
    entities: Vec<Entity>,
    fulltext: Vec<Fulltext>,
}

#[derive(Debug, Clone)]
struct Entity {
    name: String,
    fields: Vec<SchemaField>,
}

#[derive(Debug, Clone)]
struct SchemaField {
    name: String,
    line: usize,
    derived_from: Option<String>,
}

#[derive(Debug, Clone)]
struct Fulltext {
    name: String,
    line: usize,
}

fn parse_schema(text: &str) -> Result<Schema> {
    let mut entities = Vec::new();
    let mut fulltext = Vec::new();
    let chars: Vec<(usize, char)> = {
        let mut line = 1usize;
        let mut out = Vec::new();
        for c in text.chars() {
            out.push((line, c));
            if c == '\n' {
                line += 1;
            }
        }
        out
    };
    let mut i = 0;
    while i < chars.len() {
        skip_ws_and_graphql_trivia(&chars, &mut i);
        if i >= chars.len() {
            break;
        }
        if ident_at(&chars, i, "type") {
            let after = i + 4;
            if after < chars.len() && !is_ident_continue(chars[after].1) {
                i = after;
                skip_ws_and_graphql_trivia(&chars, &mut i);
                let Some(name) = take_ident(&chars, &mut i) else {
                    i += 1;
                    continue;
                };
                let header_line = chars[i.min(chars.len() - 1)].0;
                let header_end = find_char(&chars, i, '{').unwrap_or(chars.len());
                let header: String = chars[i.min(chars.len())..header_end]
                    .iter()
                    .map(|(_, c)| *c)
                    .collect();
                if name == "_Schema_" {
                    // Directive-only in this dialect: the next `{` belongs to the
                    // following type, not to `_Schema_`.
                    let next = find_next_type_decl(&chars, i).unwrap_or(chars.len());
                    let header: String = chars[i.min(chars.len())..next]
                        .iter()
                        .map(|(_, c)| *c)
                        .collect();
                    collect_fulltext(&header, header_line, &mut fulltext);
                    i = next;
                    continue;
                }
                if !header.contains("@entity") {
                    if let Some(open) = find_char(&chars, i, '{') {
                        if let Some(close) = match_brace(&chars, open) {
                            i = close + 1;
                            continue;
                        }
                    }
                    i += 1;
                    continue;
                }
                let Some(open) = find_char(&chars, i, '{') else {
                    i += 1;
                    continue;
                };
                let Some(close) = match_brace(&chars, open) else {
                    bail!("unclosed entity `{name}` in schema.graphql");
                };
                let body: String = chars[open + 1..close].iter().map(|(_, c)| *c).collect();
                let body_start_line = chars[open].0;
                let fields = parse_fields(&body, body_start_line);
                entities.push(Entity { name, fields });
                i = close + 1;
                continue;
            }
        }
        i += 1;
    }
    if entities.is_empty() {
        bail!("schema.graphql declares no @entity types");
    }
    Ok(Schema { entities, fulltext })
}

fn collect_fulltext(header: &str, header_line: usize, out: &mut Vec<Fulltext>) {
    let mut rest = header;
    while let Some(at) = rest.find("@fulltext") {
        rest = &rest[at + "@fulltext".len()..];
        if let Some(name) = directive_arg(rest, "name") {
            let line = header_line + header[..header.len() - rest.len()].matches('\n').count();
            out.push(Fulltext { name, line });
        }
    }
}

fn directive_arg(after_directive: &str, key: &str) -> Option<String> {
    let pat = format!("{key}:");
    let at = after_directive.find(&pat)?;
    let s = after_directive[at + pat.len()..].trim_start();
    if let Some(stripped) = s.strip_prefix('"') {
        let end = stripped.find('"')?;
        return Some(stripped[..end].to_string());
    }
    let ident: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if ident.is_empty() {
        None
    } else {
        Some(ident)
    }
}

/// Field definitions in an entity body, in order, with the line each starts on.
///
/// **Not line-based.** GraphQL puts no significance on newlines, and
/// `type Token @entity { id: ID! derivedETH: BigDecimal! }` is as valid as the multi-line form.
/// Splitting on lines and taking the first `name:` from each recorded only `id` and dropped every
/// later field on the line - silently, which for a report whose whole promise is "every field,
/// classified" means the author is told about a field that does not exist and not told about one
/// that does (#1210).
///
/// A field starts at an identifier followed by `:` **at depth zero**: not inside `[..]` (list
/// types), not inside `(..)` (a directive's arguments, which contain their own colons, as
/// `@derivedFrom(field: "pool")` does), and not inside a string or a `#` comment. Everything from
/// one field start to the next is that field's type and directives, so a directive on its own line
/// still attaches to the field above it.
fn parse_fields(body: &str, start_line: usize) -> Vec<SchemaField> {
    let starts = field_starts(body);
    let mut fields = Vec::new();
    for (idx, (name, at)) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).map(|(_, n)| *n).unwrap_or(body.len());
        let segment = &body[*at..end];
        let dirs: String = segment
            .match_indices('@')
            .map(|(i, _)| segment[i..].trim())
            .collect::<Vec<_>>()
            .join(" ");
        // `start_line` is the 1-based line the `{` is on and `line_of` is 1-based within the
        // body, so the two overlap on that first line. The old per-line loop had the same overlap
        // (`start_line + offset + 1`) and reported every schema field one line below itself.
        let line = start_line + line_of(body, *at) - 1;
        flush_field(&mut fields, Some((name.clone(), line)), &dirs);
    }
    fields
}

/// `(name, byte offset)` for every field definition in the body.
fn field_starts(body: &str) -> Vec<(String, usize)> {
    let bytes = body.as_bytes();
    let mut out: Vec<(String, usize)> = Vec::new();
    let mut brackets = 0i32;
    let mut parens = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'(' => parens += 1,
            b')' => parens -= 1,
            _ => {}
        }
        let at_word_start =
            i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if brackets == 0
            && parens == 0
            && at_word_start
            && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
        {
            let mut k = i + 1;
            while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                k += 1;
            }
            let mut n = k;
            while n < bytes.len() && bytes[n].is_ascii_whitespace() {
                n += 1;
            }
            if n < bytes.len() && bytes[n] == b':' {
                out.push((body[i..k].to_string(), i));
                i = n + 1;
                continue;
            }
            i = k;
            continue;
        }
        i += 1;
    }
    out
}

fn flush_field(fields: &mut Vec<SchemaField>, pending: Option<(String, usize)>, dirs: &str) {
    let Some((name, line)) = pending else {
        return;
    };
    if name.starts_with('_') && name != "id" {
        // skip graph-node internals if any
    }
    let derived_from = directive_arg(dirs, "field").filter(|_| dirs.contains("@derivedFrom"));
    fields.push(SchemaField {
        name,
        line,
        derived_from,
    });
}

fn skip_ws_and_graphql_trivia(chars: &[(usize, char)], i: &mut usize) {
    loop {
        if *i >= chars.len() {
            return;
        }
        let c = chars[*i].1;
        if c.is_whitespace() {
            *i += 1;
            continue;
        }
        if c == '#' {
            while *i < chars.len() && chars[*i].1 != '\n' {
                *i += 1;
            }
            continue;
        }
        if c == '"' {
            // skip "..." or """..."""
            if chars.get(*i + 1).map(|x| x.1) == Some('"')
                && chars.get(*i + 2).map(|x| x.1) == Some('"')
            {
                *i += 3;
                while *i + 2 < chars.len()
                    && !(chars[*i].1 == '"' && chars[*i + 1].1 == '"' && chars[*i + 2].1 == '"')
                {
                    *i += 1;
                }
                *i = (*i + 3).min(chars.len());
            } else {
                *i += 1;
                while *i < chars.len() && chars[*i].1 != '"' {
                    if chars[*i].1 == '\\' {
                        *i += 2;
                    } else {
                        *i += 1;
                    }
                }
                if *i < chars.len() {
                    *i += 1;
                }
            }
            continue;
        }
        return;
    }
}

fn ident_at(chars: &[(usize, char)], i: usize, want: &str) -> bool {
    for (k, wc) in (i..).zip(want.chars()) {
        if k >= chars.len() || chars[k].1 != wc {
            return false;
        }
    }
    true
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn take_ident(chars: &[(usize, char)], i: &mut usize) -> Option<String> {
    if *i >= chars.len() {
        return None;
    }
    let c = chars[*i].1;
    if !(c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    let start = *i;
    *i += 1;
    while *i < chars.len() && is_ident_continue(chars[*i].1) {
        *i += 1;
    }
    Some(chars[start..*i].iter().map(|(_, c)| *c).collect())
}

fn find_next_type_decl(chars: &[(usize, char)], start: usize) -> Option<usize> {
    let mut i = start;
    let mut paren = 0i32;
    let mut bracket = 0i32;
    let mut brace = 0i32;
    while i < chars.len() {
        skip_ws_and_graphql_trivia(chars, &mut i);
        if i >= chars.len() {
            return None;
        }
        match chars[i].1 {
            '(' => paren += 1,
            ')' => paren -= 1,
            '[' => bracket += 1,
            ']' => bracket -= 1,
            '{' => brace += 1,
            '}' => brace -= 1,
            _ => {
                if paren == 0
                    && bracket == 0
                    && brace == 0
                    && ident_at(chars, i, "type")
                    && (i == 0 || !is_ident_continue(chars[i - 1].1))
                {
                    let after = i + 4;
                    if after >= chars.len() || !is_ident_continue(chars[after].1) {
                        return Some(i);
                    }
                }
            }
        }
        i += 1;
    }
    None
}

fn find_char(chars: &[(usize, char)], start: usize, want: char) -> Option<usize> {
    let mut i = start;
    while i < chars.len() {
        skip_ws_and_graphql_trivia(chars, &mut i);
        if i >= chars.len() {
            return None;
        }
        if chars[i].1 == want {
            return Some(i);
        }
        // Don't skip past `{` looking for `{` from a later token: if this isn't it, advance one.
        if chars[i].1 == '{' || chars[i].1 == '}' {
            return if chars[i].1 == want { Some(i) } else { None };
        }
        i += 1;
    }
    None
}

fn match_brace(chars: &[(usize, char)], open: usize) -> Option<usize> {
    if open >= chars.len() || chars[open].1 != '{' {
        return None;
    }
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        let c = chars[i].1;
        if c == '"' {
            skip_ws_and_graphql_trivia(chars, &mut i);
            continue;
        }
        if c == '#' {
            while i < chars.len() && chars[i].1 != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

// ── Mappings ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandlerKind {
    Event,
    Call,
    Block,
    Helper,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    kind: HandlerKind,
    assignments: Vec<Assignment>,
    calls: BTreeSet<String>,
    contract_call: Option<Citation>,
    has_loop_load: bool,
    field_reads: Vec<(String, String)>, // (entity, field)
}

#[derive(Debug, Clone)]
struct Assignment {
    entity: String,
    field: String,
    citation: Citation,
    expr: String,
    /// Whether the receiver is known to be an entity at all. `x.field = ..` where `x` is not a
    /// resolved binding is only an entity write if something saves `x`; without that, the
    /// unique-field fallback in [`classify`] would read any plain object assignment as a write to
    /// whichever entity happens to own that field name.
    receiver_is_entity: bool,
}

#[derive(Debug, Clone)]
struct Mappings {
    functions: BTreeMap<String, FunctionInfo>,
}

fn load_mappings(dir: &Path) -> Result<Mappings> {
    let mut handler_kinds: BTreeMap<String, HandlerKind> = BTreeMap::new();
    let mut yaml_files: Vec<PathBuf> = Vec::new();
    for name in ["subgraph.yaml", "subgraph.yml"] {
        let p = dir.join(name);
        if p.exists() {
            if let Ok(text) = std::fs::read_to_string(&p) {
                collect_manifest_hints(dir, &text, &mut handler_kinds, &mut yaml_files);
            }
        }
    }
    let mut files: BTreeSet<PathBuf> = yaml_files.into_iter().collect();
    walk_ts(dir, &mut files);
    let mut functions = BTreeMap::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        // Node scripts (templatify, hardhat) sit next to mappings. They are not AssemblyScript.
        if text.contains("require(\"") || text.contains("require('") {
            continue;
        }
        let rel = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        for mut func in parse_functions(&text, &rel) {
            if let Some(kind) = handler_kinds.get(&func.name) {
                func.kind = *kind;
            }
            functions.insert(func.name.clone(), func);
        }
    }
    Ok(Mappings { functions })
}

fn collect_manifest_hints(
    dir: &Path,
    text: &str,
    kinds: &mut BTreeMap<String, HandlerKind>,
    files: &mut Vec<PathBuf>,
) {
    let Ok(docs) = YamlLoader::load_from_str(text) else {
        return;
    };
    let Some(doc) = docs.first() else {
        return;
    };
    for key in ["dataSources", "templates"] {
        let Some(arr) = get_yaml(doc, key).and_then(|v| v.as_vec()) else {
            continue;
        };
        for src in arr {
            let Some(mapping) = get_yaml(src, "mapping") else {
                continue;
            };
            if let Some(file) = mapping_file_path(dir, mapping) {
                files.push(file);
            }
            take_handlers(mapping, "eventHandlers", HandlerKind::Event, kinds);
            take_handlers(mapping, "callHandlers", HandlerKind::Call, kinds);
            take_handlers(mapping, "blockHandlers", HandlerKind::Block, kinds);
        }
    }
}

fn get_yaml<'a>(y: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    y.as_hash()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

fn mapping_file_path(dir: &Path, mapping: &Yaml) -> Option<PathBuf> {
    let file = get_yaml(mapping, "file")?;
    let rel = match file {
        Yaml::String(s) => s.clone(),
        Yaml::Hash(_) => return None, // CID link; local tree has the path form
        _ => return None,
    };
    let p = dir.join(rel.trim_start_matches("./"));
    p.exists().then_some(p)
}

fn take_handlers(
    mapping: &Yaml,
    key: &str,
    kind: HandlerKind,
    kinds: &mut BTreeMap<String, HandlerKind>,
) {
    let Some(arr) = get_yaml(mapping, key).and_then(|v| v.as_vec()) else {
        return;
    };
    for entry in arr {
        if let Some(name) = get_yaml(entry, "handler").and_then(|v| v.as_str()) {
            kinds
                .entry(name.to_string())
                .and_modify(|k| {
                    if kind == HandlerKind::Block {
                        *k = HandlerKind::Block;
                    }
                })
                .or_insert(kind);
        }
    }
}

impl PartialOrd for HandlerKind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HandlerKind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn rank(k: HandlerKind) -> u8 {
            match k {
                HandlerKind::Helper => 0,
                HandlerKind::Event => 1,
                HandlerKind::Call => 2,
                HandlerKind::Block => 3,
            }
        }
        rank(*self).cmp(&rank(*other))
    }
}

fn walk_ts(dir: &Path, files: &mut BTreeSet<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if matches!(
                name.as_ref(),
                "node_modules" | "generated" | "build" | "tests" | ".git" | "abis"
            ) {
                continue;
            }
            walk_ts(&path, files);
        } else if name.ends_with(".ts") || name.ends_with(".as") {
            files.insert(path);
        }
    }
}

fn parse_functions(text: &str, file: &str) -> Vec<FunctionInfo> {
    let stripped = strip_ts_comments(text);
    let mut out = Vec::new();
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !stripped.is_char_boundary(i) {
            i += 1;
            continue;
        }
        if let Some(name_range) = match_function_name(&stripped, i) {
            let name = stripped[name_range.clone()].to_string();
            let after_name = name_range.end;
            if let Some(brace) = stripped[after_name..].find('{') {
                let open = after_name + brace;
                if let Some(close) = match_ts_brace(&stripped, open) {
                    let sig = &stripped[after_name..open];
                    let params = parse_params(sig);
                    let body = &stripped[open + 1..close];
                    let start_line = line_of(&stripped, open);
                    out.push(analyse_function(name, file, body, start_line, params));
                    i = close + 1;
                    continue;
                }
            }
            i = after_name;
            continue;
        }
        i += 1;
    }
    out
}

fn match_function_name(text: &str, i: usize) -> Option<std::ops::Range<usize>> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let rest = &text[i..];
    // Word-boundary `function NAME`.
    let at = rest.find("function")?;
    if at != 0 {
        return None;
    }
    if i > 0 {
        let prev = text.as_bytes()[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let mut k = i + 8;
    let bytes = text.as_bytes();
    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    let start = k;
    if k >= bytes.len() || !(bytes[k].is_ascii_alphabetic() || bytes[k] == b'_') {
        return None;
    }
    k += 1;
    while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
        k += 1;
    }
    Some(start..k)
}

fn match_ts_brace(text: &str, open: usize) -> Option<usize> {
    match_ts_delim(text, open, b'{', b'}')
}

fn match_ts_delim(text: &str, open: usize, o: u8, c: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    if open >= bytes.len() || bytes[open] != o {
        return None;
    }
    let mut depth = 0;
    let mut i = open;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' || b == b'`' {
            in_str = Some(b);
            i += 1;
            continue;
        }
        if b == o {
            depth += 1;
        } else if b == c {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn line_of(text: &str, idx: usize) -> usize {
    1 + text[..idx].bytes().filter(|b| *b == b'\n').count()
}

fn strip_ts_comments(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            out.push(b as char);
            if b == b'\\' && i + 1 < bytes.len() {
                out.push(bytes[i + 1] as char);
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' || b == b'`' {
            in_str = Some(b);
            out.push(b as char);
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            out.push(' ');
            out.push(' ');
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                i += 1;
            }
            if i + 1 < bytes.len() {
                out.push(' ');
                out.push(' ');
                i += 2;
            }
            continue;
        }
        out.push(b as char);
        i += 1;
    }
    out
}

fn parse_params(sig: &str) -> BTreeMap<String, String> {
    let start = match sig.find('(') {
        Some(i) => i + 1,
        None => return BTreeMap::new(),
    };
    let end = match sig[start..].find(')') {
        Some(i) => start + i,
        None => return BTreeMap::new(),
    };
    let mut out = BTreeMap::new();
    for part in sig[start..end].split(',') {
        let part = part.trim();
        let Some(colon) = part.find(':') else {
            continue;
        };
        let var = part[..colon].trim();
        let ty = part[colon + 1..]
            .trim()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()
            .unwrap_or("");
        if !var.is_empty() && !ty.is_empty() {
            out.insert(var.to_string(), ty.to_string());
        }
    }
    out
}

fn analyse_function(
    name: String,
    file: &str,
    body: &str,
    body_start_line: usize,
    params: BTreeMap<String, String>,
) -> FunctionInfo {
    let mut bindings = params;
    bindings.extend(collect_bindings(body));
    let mut assignments = collect_assignments(body, file, body_start_line, &bindings);
    assignments.extend(collect_constructor_ids(
        body,
        file,
        body_start_line,
        &bindings,
    ));
    let calls = collect_calls(body);
    let contract_call = find_contract_call(body, file, body_start_line);
    let has_loop_load = loop_reads_a_loaded_entity_field(body, &bindings);
    let field_reads = collect_field_reads(body, &bindings);
    FunctionInfo {
        name,
        kind: HandlerKind::Helper,
        assignments,
        calls,
        contract_call,
        has_loop_load,
        field_reads,
    }
}

fn collect_bindings(body: &str) -> BTreeMap<String, String> {
    let mut bind = BTreeMap::new();
    // function params live on the signature, which is outside `body`. Bindings from
    // `let x = new Token` / `let x = Token.load` inside the body are what we get here.
    // Parameter bindings are recovered in `parse_functions` via a second pass... so fold
    // them in here by scanning a reconstructed head: callers pass body only. We also
    // accept `changetype<Token>(...)`.
    let re_new = regex_find(
        body,
        r"(?:let|const|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*new\s+([A-Za-z_][A-Za-z0-9_]*)\s*\(",
    );
    for (var, ent) in re_new {
        bind.insert(var, ent);
    }
    let re_load = regex_find(
        body,
        r"(?:let|const|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*load\s*\(",
    );
    for (var, ent) in re_load {
        bind.insert(var, ent);
    }
    let mut i = 0;
    while i < body.len() {
        if let Some((var, ent, next)) = match_bare_new(body, i) {
            bind.insert(var, ent);
            i = next;
            continue;
        }
        if let Some((var, ent, next)) = match_let_create(body, i) {
            bind.insert(var, ent);
            i = next;
            continue;
        }
        i += 1;
    }
    bind
}

/// Tiny two-group finder. Not a general regex engine; the patterns above are fixed.
fn regex_find(text: &str, kind: &str) -> Vec<(String, String)> {
    // kind is a tag, not a regex, so the matchers stay reviewable.
    let mut out = Vec::new();
    match kind {
        k if k.contains("new") => {
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if let Some((var, ent, next)) = match_let_new(text, i) {
                    out.push((var, ent));
                    i = next;
                } else {
                    i += 1;
                }
            }
        }
        _ => {
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if let Some((var, ent, next)) = match_let_load(text, i) {
                    out.push((var, ent));
                    i = next;
                } else {
                    i += 1;
                }
            }
        }
    }
    out
}

fn match_let_new(text: &str, i: usize) -> Option<(String, String, usize)> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let rest = &text[i..];
    let kw = if rest.starts_with("let ") {
        4
    } else if rest.starts_with("const ") {
        6
    } else if rest.starts_with("var ") {
        4
    } else {
        return None;
    };
    if i > 0 {
        let p = text.as_bytes()[i - 1];
        if p.is_ascii_alphanumeric() || p == b'_' {
            return None;
        }
    }
    let mut k = i + kw;
    let var = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('=') {
        return None;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with("new ") {
        return None;
    }
    k += 4;
    skip_ws_str(text, &mut k);
    let ent = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('(') {
        return None;
    }
    Some((var, ent, k))
}

fn match_bare_new(text: &str, i: usize) -> Option<(String, String, usize)> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let bytes = text.as_bytes();
    if i > 0 {
        let p = bytes[i - 1];
        if p.is_ascii_alphanumeric() || p == b'_' {
            return None;
        }
    }
    let mut k = i;
    let var = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('=') {
        return None;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with("new ") {
        return None;
    }
    k += 4;
    skip_ws_str(text, &mut k);
    let ent = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('(') {
        return None;
    }
    Some((var, ent, k))
}

fn match_let_create(text: &str, i: usize) -> Option<(String, String, usize)> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let rest = &text[i..];
    let kw = if rest.starts_with("let ") {
        4
    } else if rest.starts_with("const ") {
        6
    } else if rest.starts_with("var ") {
        4
    } else {
        return None;
    };
    if i > 0 {
        let p = text.as_bytes()[i - 1];
        if p.is_ascii_alphanumeric() || p == b'_' {
            return None;
        }
    }
    let mut k = i + kw;
    let var = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('=') {
        return None;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    if text[k..].starts_with("createOrLoad") {
        k += "createOrLoad".len();
    } else if text[k..].starts_with("create") {
        k += "create".len();
    } else {
        return None;
    }
    let ent = take_ident_str(text, &mut k)?;
    if !ent.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return None;
    }
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('(') {
        return None;
    }
    Some((var, ent, k))
}

fn match_let_load(text: &str, i: usize) -> Option<(String, String, usize)> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let rest = &text[i..];
    let kw = if rest.starts_with("let ") {
        4
    } else if rest.starts_with("const ") {
        6
    } else if rest.starts_with("var ") {
        4
    } else {
        return None;
    };
    if i > 0 {
        let p = text.as_bytes()[i - 1];
        if p.is_ascii_alphanumeric() || p == b'_' {
            return None;
        }
    }
    let mut k = i + kw;
    let var = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('=') {
        return None;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    let ent = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('.') {
        return None;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with("load") {
        return None;
    }
    k += 4;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with('(') {
        return None;
    }
    Some((var, ent, k))
}

fn take_ident_str(text: &str, i: &mut usize) -> Option<String> {
    if !text.is_char_boundary(*i) {
        return None;
    }
    let bytes = text.as_bytes();
    if *i >= bytes.len() {
        return None;
    }
    if !(bytes[*i].is_ascii_alphabetic() || bytes[*i] == b'_') {
        return None;
    }
    let start = *i;
    *i += 1;
    while *i < bytes.len() && (bytes[*i].is_ascii_alphanumeric() || bytes[*i] == b'_') {
        *i += 1;
    }
    Some(text[start..*i].to_string())
}

fn skip_ws_str(text: &str, i: &mut usize) {
    let bytes = text.as_bytes();
    while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
        *i += 1;
    }
}

fn collect_assignments(
    body: &str,
    file: &str,
    body_start_line: usize,
    bindings: &BTreeMap<String, String>,
) -> Vec<Assignment> {
    let saved = collect_saved_idents(body);
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some((var, field, eq_at)) = match_assign(body, i) {
            let entity = bindings.get(&var).cloned().unwrap_or_default();
            let receiver_is_entity = bindings.contains_key(&var) || saved.contains(&var);
            let expr = resolve_local(body, &take_expr(body, eq_at + 1));
            let line = body_start_line + line_of(body, eq_at) - 1;
            out.push(Assignment {
                entity,
                field,
                citation: Citation {
                    file: file.to_string(),
                    line,
                },
                expr: collapse_ws(&expr),
                receiver_is_entity,
            });
            i = eq_at + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Identifiers this body calls `.save()` on. In graph-ts an entity is only persisted by `save()`,
/// so a receiver that is saved is an entity even when the binding collector could not see where it
/// came from (`getOrCreateToken(..)` and friends). A receiver that is never saved is a plain
/// object, and its fields are not entity writes.
fn collect_saved_idents(body: &str) -> BTreeSet<String> {
    let bytes = body.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            let mut k = i + 1;
            skip_ws_str(body, &mut k);
            if body[k..].starts_with("save") {
                let mut n = k + 4;
                skip_ws_str(body, &mut n);
                if n < bytes.len() && bytes[n] == b'(' {
                    // Walk back over the receiver identifier.
                    let mut e = i;
                    while e > 0 && bytes[e - 1].is_ascii_whitespace() {
                        e -= 1;
                    }
                    let mut b = e;
                    while b > 0 && (bytes[b - 1].is_ascii_alphanumeric() || bytes[b - 1] == b'_') {
                        b -= 1;
                    }
                    if b < e && !bytes[b].is_ascii_digit() && body.is_char_boundary(b) {
                        out.insert(body[b..e].to_string());
                    }
                }
            }
        }
        i += 1;
    }
    out
}

fn match_assign(text: &str, i: usize) -> Option<(String, String, usize)> {
    if !text.is_char_boundary(i) {
        return None;
    }
    let bytes = text.as_bytes();
    if i > 0 {
        let p = bytes[i - 1];
        if p.is_ascii_alphanumeric() || p == b'_' {
            return None;
        }
    }
    let mut k = i;
    let var = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if k >= bytes.len() || bytes[k] != b'.' {
        return None;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    let field = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if k >= bytes.len() || bytes[k] != b'=' {
        return None;
    }
    if k + 1 < bytes.len() && (bytes[k + 1] == b'=' || bytes[k + 1] == b'>') {
        return None;
    }
    Some((var, field, k))
}

fn take_expr(text: &str, start: usize) -> String {
    let bytes = text.as_bytes();
    let mut i = start;
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' || b == b'`' {
            in_str = Some(b);
            i += 1;
            continue;
        }
        if b == b'(' || b == b'[' || b == b'{' {
            depth += 1;
        } else if b == b')' || b == b']' || b == b'}' {
            if depth == 0 {
                break;
            }
            depth -= 1;
        } else if b == b';' && depth == 0 {
            break;
        } else if b == b'\n' && depth == 0 {
            // Keep a wrapped RHS (`=\n  fetchTokenSymbol(...)`). Stop on a new
            // assignment, `entity.save()`, or a blank line.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() && bytes[j] != b'\n' {
                j += 1;
            }
            if continues_expr(text, j) {
                i += 1;
                continue;
            }
            break;
        }
        i += 1;
    }
    text[start..i].trim().to_string()
}

fn continues_expr(text: &str, j: usize) -> bool {
    let bytes = text.as_bytes();
    if j >= bytes.len() {
        return false;
    }
    let b = bytes[j];
    if b == b'.' || b == b'+' || b == b'(' {
        return true;
    }
    if !(b.is_ascii_alphabetic() || b == b'_') {
        return false;
    }
    if match_assign(text, j).is_some() || is_entity_save(text, j) {
        return false;
    }
    let mut k = j;
    match take_ident_str(text, &mut k).as_deref() {
        Some(
            "let" | "const" | "var" | "return" | "if" | "for" | "while" | "function" | "export",
        ) => false,
        Some(_) => true,
        None => false,
    }
}

fn is_entity_save(text: &str, i: usize) -> bool {
    let mut k = i;
    if take_ident_str(text, &mut k).is_none() {
        return false;
    }
    skip_ws_str(text, &mut k);
    let bytes = text.as_bytes();
    if k >= bytes.len() || bytes[k] != b'.' {
        return false;
    }
    k += 1;
    skip_ws_str(text, &mut k);
    take_ident_str(text, &mut k).as_deref() == Some("save")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `const decimals = fetchTokenDecimals(addr); token.decimals = decimals` should still
/// see the helper, not the local name.
fn resolve_local(body: &str, expr: &str) -> String {
    let ident = expr.trim();
    if ident.is_empty()
        || !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || ident.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        return expr.to_string();
    }
    let mut i = 0;
    while i < body.len() {
        if !body.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &body[i..];
        let kw = if rest.starts_with("let ") {
            4
        } else if rest.starts_with("const ") {
            6
        } else if rest.starts_with("var ") {
            4
        } else {
            i += 1;
            continue;
        };
        if i > 0 {
            let p = body.as_bytes()[i - 1];
            if p.is_ascii_alphanumeric() || p == b'_' {
                i += 1;
                continue;
            }
        }
        let mut k = i + kw;
        let Some(var) = take_ident_str(body, &mut k) else {
            i += 1;
            continue;
        };
        if var != ident {
            i += 1;
            continue;
        }
        skip_ws_str(body, &mut k);
        if !body[k..].starts_with('=') {
            i += 1;
            continue;
        }
        let rhs = take_expr(body, k + 1);
        if rhs != ident {
            return collapse_ws(&rhs);
        }
        i += 1;
    }
    expr.to_string()
}

fn collect_constructor_ids(
    body: &str,
    file: &str,
    body_start_line: usize,
    _bindings: &BTreeMap<String, String>,
) -> Vec<Assignment> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let hit = match_let_new(body, i).or_else(|| match_bare_new(body, i));
        if let Some((var, ent, next)) = hit {
            let _ = var;
            let mut k = next;
            skip_ws_str(body, &mut k);
            if body[k..].starts_with('(') {
                let expr = take_expr(body, k + 1);
                let line = body_start_line + line_of(body, k) - 1;
                out.push(Assignment {
                    entity: ent,
                    field: "id".into(),
                    citation: Citation {
                        file: file.to_string(),
                        line,
                    },
                    expr: collapse_ws(&expr),
                    // `new Entity(..)` names the entity outright.
                    receiver_is_entity: true,
                });
            }
            i = next;
            continue;
        }
        i += 1;
    }
    out
}

fn collect_calls(body: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (i == 0
            || !(bytes[i - 1].is_ascii_alphanumeric()
                || bytes[i - 1] == b'_'
                || bytes[i - 1] == b'.'))
            && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
        {
            let mut k = i + 1;
            while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                k += 1;
            }
            let mut j = k;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'(' {
                let name = &body[i..k];
                if !matches!(
                    name,
                    "if" | "for"
                        | "while"
                        | "switch"
                        | "return"
                        | "new"
                        | "load"
                        | "save"
                        | "BigInt"
                        | "BigDecimal"
                        | "Address"
                        | "Bytes"
                        | "changetype"
                        | "require"
                ) {
                    out.insert(name.to_string());
                }
            }
            i = k;
            continue;
        }
        i += 1;
    }
    out
}

fn find_contract_call(body: &str, file: &str, body_start_line: usize) -> Option<Citation> {
    let needles = [".bind(", "ethereum.call(", ".try_"];
    let mut best: Option<usize> = None;
    for n in needles {
        if let Some(at) = body.find(n) {
            best = Some(best.map_or(at, |b| b.min(at)));
        }
    }
    best.map(|at| Citation {
        file: file.to_string(),
        line: body_start_line + line_of(body, at) - 1,
    })
}

/// A fixed point is a loop that **reads entity state it has loaded**, not a body that merely
/// contains a loop somewhere and a `.load(` somewhere.
///
/// `has_loop(body) && body.contains(".load(")` was body-wide, so a load outside every loop set the
/// class, and a loop that only loads and saves - which reproduces exactly - set it too. Both
/// report a field as non-reproducible when it is not, which is the wrong direction to be wrong in
/// for a port report: the author does the work of hand-checking a field that was fine.
///
/// `findEthPerToken` is the shape this exists for. It loops over whitelisted pools, loads each, and
/// reads a stored field off the loaded entity, so its answer depends on state derived from earlier
/// blocks. A loop with no such read has no such dependency. A braceless single-statement loop is
/// not treated as a fixed point rather than guessed at.
fn loop_reads_a_loaded_entity_field(body: &str, bindings: &BTreeMap<String, String>) -> bool {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let at_word_start =
            i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let kw = if body.is_char_boundary(i) && body[i..].starts_with("for") {
            3
        } else if body.is_char_boundary(i) && body[i..].starts_with("while") {
            5
        } else {
            0
        };
        if at_word_start && kw > 0 {
            let after = i + kw;
            let boundary = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if boundary {
                if let Some(inner) = loop_body(body, after) {
                    if inner.contains(".load(") && !collect_field_reads(inner, bindings).is_empty()
                    {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

/// The braced body of the loop whose keyword ends at `after`, or `None` when the header does not
/// have the `( .. ) { .. }` shape. A nested loop is reached by the outer scan continuing past here.
fn loop_body(body: &str, after: usize) -> Option<&str> {
    let bytes = body.as_bytes();
    let mut k = after;
    skip_ws_str(body, &mut k);
    if k >= bytes.len() || bytes[k] != b'(' {
        return None;
    }
    let close = match_ts_delim(body, k, b'(', b')')?;
    let mut b = close + 1;
    skip_ws_str(body, &mut b);
    if b >= bytes.len() || bytes[b] != b'{' {
        return None;
    }
    let end = match_ts_brace(body, b)?;
    Some(&body[b + 1..end])
}

fn collect_field_reads(body: &str, bindings: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
            && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_')
        {
            let mut k = i + 1;
            while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                k += 1;
            }
            let var = &body[i..k];
            let mut j = k;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'.' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let start = j;
                if j < bytes.len() && (bytes[j].is_ascii_alphabetic() || bytes[j] == b'_') {
                    j += 1;
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    let field = &body[start..j];
                    let mut n = j;
                    while n < bytes.len() && bytes[n].is_ascii_whitespace() {
                        n += 1;
                    }
                    let is_call = n < bytes.len() && bytes[n] == b'(';
                    let is_assign = n < bytes.len()
                        && bytes[n] == b'='
                        && !(n + 1 < bytes.len() && bytes[n + 1] == b'=');
                    if !is_call && !is_assign && var != "event" {
                        if let Some(entity) = bindings.get(var) {
                            if field != "id" && field != "save" {
                                out.push((entity.clone(), field.to_string()));
                            }
                        }
                    }
                }
            }
            i = k;
            continue;
        }
        i += 1;
    }
    out
}

// ── Classify ────────────────────────────────────────────────────────────────

fn classify(schema: &Schema, mappings: &Mappings) -> Vec<FieldRow> {
    let entity_names: BTreeSet<&str> = schema.entities.iter().map(|e| e.name.as_str()).collect();
    let unique_fields = unique_field_owners(schema);
    let functions = &mappings.functions;

    let mut writers: BTreeMap<(String, String), Vec<(String, Assignment)>> = BTreeMap::new();
    for func in functions.values() {
        for a in &func.assignments {
            // The unique-field fallback answers *which* entity, never *whether* the receiver is
            // one. Without that guard `let metadata = loadMetadata(); metadata.symbol = ..` is
            // recorded as a write to `Token.symbol` whenever `symbol` is uniquely owned, and the
            // field is then reported exact on the strength of an unrelated object assignment.
            let entity = if entity_names.contains(a.entity.as_str()) {
                a.entity.clone()
            } else if !a.receiver_is_entity {
                continue;
            } else if let Some(owner) = unique_fields.get(&a.field) {
                owner.clone()
            } else {
                continue;
            };
            if !entity_names.contains(entity.as_str()) {
                continue;
            }
            let mut a2 = a.clone();
            a2.entity = entity.clone();
            writers
                .entry((entity, a.field.clone()))
                .or_default()
                .push((func.name.clone(), a2));
        }
    }

    // Return class of a helper: what `x = helper(...)` inherits. Not applied to every
    // assignment inside a mixed event handler. Walk the call graph so a wrapper around
    // a contract-call helper is itself call-derived.
    let mut fn_class: BTreeMap<String, Class> = BTreeMap::new();
    for (name, func) in functions {
        let mut c = Class::Exact;
        if func.contract_call.is_some() {
            c = Class::CallDerived;
        }
        if func.has_loop_load {
            c = c.max(Class::FixedPoint);
        }
        if func.kind == HandlerKind::Block {
            c = Class::Unreachable;
        }
        fn_class.insert(name.clone(), c);
    }
    propagate_fn_class(functions, &mut fn_class);

    let mut field_class: BTreeMap<(String, String), (Class, Citation, String)> = BTreeMap::new();

    for ft in &schema.fulltext {
        field_class.insert(
            ("_Schema_".into(), ft.name.clone()),
            (
                Class::Unreachable,
                Citation {
                    file: "schema.graphql".into(),
                    line: ft.line,
                },
                format!("`@fulltext` search index `{}`", ft.name),
            ),
        );
    }

    for ent in &schema.entities {
        for f in &ent.fields {
            let key = (ent.name.clone(), f.name.clone());
            if let Some(src) = &f.derived_from {
                field_class.insert(
                    key,
                    (
                        Class::Exact,
                        Citation {
                            file: "schema.graphql".into(),
                            line: f.line,
                        },
                        format!("`@derivedFrom(field: \"{src}\")` - reverse lookup, a SQL join"),
                    ),
                );
                continue;
            }
            let Some(sites) = writers.get(&key) else {
                field_class.insert(
                    key,
                    (
                        Class::Unreachable,
                        Citation {
                            file: "schema.graphql".into(),
                            line: f.line,
                        },
                        "no mapping writes this field".into(),
                    ),
                );
                continue;
            };
            let (worst, citation, reason) = worst_writer(sites, functions, &fn_class, &field_class);
            field_class.insert(key, (worst, citation, reason));
        }
    }

    // A helper that reads a fixed-point field returns a fixed-point value. Call-derived
    // (decimals as a scale factor) does not leak: RFC-0038 §6a prices stay exact.
    for _ in 0..16 {
        let mut changed = false;
        let snapshot = field_class.clone();
        for (name, func) in functions {
            let mut bump = false;
            for (ent, field) in &func.field_reads {
                if snapshot
                    .get(&(ent.clone(), field.clone()))
                    .is_some_and(|(c, _, _)| *c == Class::FixedPoint)
                {
                    bump = true;
                }
            }
            for (field, owner) in &unique_fields {
                if (func
                    .assignments
                    .iter()
                    .any(|a| a.expr.contains(&format!(".{field}")))
                    || func.field_reads.iter().any(|(_, f)| f == field))
                    && snapshot
                        .get(&(owner.clone(), field.clone()))
                        .is_some_and(|(c, _, _)| *c == Class::FixedPoint)
                {
                    bump = true;
                }
            }
            if bump {
                let cur = fn_class.get(name).copied().unwrap_or(Class::Exact);
                if cur < Class::FixedPoint {
                    fn_class.insert(name.clone(), Class::FixedPoint);
                    changed = true;
                }
            }
        }
        if propagate_fn_class(functions, &mut fn_class) {
            changed = true;
        }
        for (key, sites) in &writers {
            let (worst, citation, reason) = worst_writer(sites, functions, &fn_class, &snapshot);
            let new = (worst, citation, reason);
            if field_class.get(key) != Some(&new) {
                changed = true;
            }
            field_class.insert(key.clone(), new);
        }
        if !changed {
            break;
        }
    }

    let mut rows: Vec<FieldRow> = field_class
        .into_iter()
        .map(|((entity, field), (class, citation, reason))| FieldRow {
            entity,
            field,
            class,
            citation,
            reason,
        })
        .collect();
    rows.sort_by(|a, b| a.entity.cmp(&b.entity).then(a.field.cmp(&b.field)));
    rows
}

fn propagate_fn_class(
    functions: &BTreeMap<String, FunctionInfo>,
    fn_class: &mut BTreeMap<String, Class>,
) -> bool {
    let mut any = false;
    for _ in 0..16 {
        let mut changed = false;
        for (name, func) in functions {
            let mut c = fn_class.get(name).copied().unwrap_or(Class::Exact);
            for callee in &func.calls {
                if let Some(cc) = fn_class.get(callee) {
                    c = c.max(*cc);
                }
            }
            if fn_class.get(name).copied() != Some(c) {
                fn_class.insert(name.clone(), c);
                changed = true;
                any = true;
            }
        }
        if !changed {
            break;
        }
    }
    any
}

fn worst_writer(
    sites: &[(String, Assignment)],
    functions: &BTreeMap<String, FunctionInfo>,
    fn_class: &BTreeMap<String, Class>,
    field_class: &BTreeMap<(String, String), (Class, Citation, String)>,
) -> (Class, Citation, String) {
    let mut worst = Class::Exact;
    let mut citation = sites[0].1.citation.clone();
    let mut reason = format!("assigned from `{}`", sites[0].1.expr);
    for (fn_name, asg) in sites {
        let c = class_of_assignment(fn_name, asg, functions, fn_class, field_class);
        if c > worst {
            worst = c;
            citation = asg.citation.clone();
            reason = reason_for(c, fn_name, asg, functions.get(fn_name));
        }
    }
    (worst, citation, reason)
}

fn class_of_assignment(
    fn_name: &str,
    asg: &Assignment,
    functions: &BTreeMap<String, FunctionInfo>,
    fn_class: &BTreeMap<String, Class>,
    field_class: &BTreeMap<(String, String), (Class, Citation, String)>,
) -> Class {
    let Some(func) = functions.get(fn_name) else {
        return Class::Exact;
    };
    if func.kind == HandlerKind::Block {
        return Class::Unreachable;
    }
    let mut c = Class::Exact;
    if expr_has_contract_call(&asg.expr) {
        c = Class::CallDerived;
    }
    for callee in &func.calls {
        if expr_calls(&asg.expr, callee) {
            if let Some(cc) = fn_class.get(callee) {
                c = c.max(*cc);
            }
        }
    }
    // Only this assignment's RHS. A mixed event handler's fn_class is not applied
    // to every write; an own-field fold next to a fixed-point copy stays exact.
    for (ent, field) in &func.field_reads {
        if expr_reads_field(&asg.expr, field)
            && field_class
                .get(&(ent.clone(), field.clone()))
                .is_some_and(|(cl, _, _)| *cl == Class::FixedPoint)
        {
            c = c.max(Class::FixedPoint);
        }
    }
    c
}

fn expr_calls(expr: &str, name: &str) -> bool {
    let needle = format!("{name}(");
    if let Some(at) = expr.find(&needle) {
        if at > 0 {
            let p = expr.as_bytes()[at - 1];
            if p.is_ascii_alphanumeric() || p == b'_' || p == b'.' {
                return false;
            }
        }
        return true;
    }
    false
}

fn expr_reads_field(expr: &str, field: &str) -> bool {
    let needle = format!(".{field}");
    let bytes = expr.as_bytes();
    let mut from = 0;
    while let Some(rel) = expr[from..].find(&needle) {
        let at = from + rel;
        let after = at + needle.len();
        let boundary =
            after == bytes.len() || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
        if boundary {
            let mut n = after;
            while n < bytes.len() && bytes[n].is_ascii_whitespace() {
                n += 1;
            }
            let is_call = n < bytes.len() && bytes[n] == b'(';
            let is_assign = n < bytes.len()
                && bytes[n] == b'='
                && !(n + 1 < bytes.len() && bytes[n + 1] == b'=');
            if !is_call && !is_assign {
                return true;
            }
        }
        from = at + 1;
    }
    false
}

fn expr_has_contract_call(expr: &str) -> bool {
    expr.contains(".bind(") || expr.contains("ethereum.call(") || expr.contains(".try_")
}

fn unique_field_owners(schema: &Schema) -> BTreeMap<String, String> {
    let mut counts: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for e in &schema.entities {
        for f in &e.fields {
            counts
                .entry(f.name.clone())
                .or_default()
                .push(e.name.clone());
        }
    }
    counts
        .into_iter()
        .filter_map(|(f, ents)| {
            if ents.len() == 1 {
                Some((f, ents[0].clone()))
            } else {
                None
            }
        })
        .collect()
}

fn reason_for(
    class: Class,
    fn_name: &str,
    asg: &Assignment,
    func: Option<&FunctionInfo>,
) -> String {
    match class {
        Class::Unreachable if func.is_some_and(|f| f.kind == HandlerKind::Block) => {
            format!("written from blockHandler `{fn_name}`; nuthatch indexes logs")
        }
        Class::CallDerived => {
            if let Some(c) = func.and_then(|f| f.contract_call.as_ref()) {
                format!("`{fn_name}` reads contract state (`{}`)", c.display())
            } else {
                format!(
                    "`{fn_name}` reads contract state; assigned from `{}`",
                    asg.expr
                )
            }
        }
        Class::FixedPoint => {
            format!(
                "`{fn_name}` reads stored entity output (`{}`); a nest can converge, this will not reproduce",
                asg.expr
            )
        }
        Class::Exact => format!("assigned from `{}`", asg.expr),
        Class::Unreachable => format!("assigned in `{fn_name}` from `{}`", asg.expr),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_and_mappings(schema: &str, file: &str, mapping: &str) -> (Schema, Mappings) {
        let schema = parse_schema(schema).unwrap();
        let mut functions = BTreeMap::new();
        for func in parse_functions(mapping, file) {
            functions.insert(func.name.clone(), func);
        }
        (schema, Mappings { functions })
    }

    fn class_of(rows: &[FieldRow], entity: &str, field: &str) -> Class {
        rows.iter()
            .find(|r| r.entity == entity && r.field == field)
            .unwrap_or_else(|| panic!("missing {entity}.{field}"))
            .class
    }

    fn reason_of(rows: &[FieldRow], entity: &str, field: &str) -> String {
        rows.iter()
            .find(|r| r.entity == entity && r.field == field)
            .unwrap()
            .reason
            .clone()
    }

    #[test]
    fn event_assignment_is_exact() {
        let schema = r#"
type Swap @entity {
  id: ID!
  amount0: BigDecimal!
  timestamp: BigInt!
}
"#;
        let mapping = r#"
export function handleSwap(event: SwapEvent): void {
  let swap = new Swap(event.transaction.hash.toHex())
  swap.amount0 = event.params.amount0
  swap.timestamp = event.block.timestamp
  swap.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/mappings/core.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Swap", "amount0"), Class::Exact);
        assert_eq!(class_of(&rows, "Swap", "timestamp"), Class::Exact);
        assert_eq!(class_of(&rows, "Swap", "id"), Class::Exact);
        assert!(reason_of(&rows, "Swap", "amount0").contains("event.params.amount0"));
    }

    #[test]
    fn local_holding_a_call_is_still_call_derived() {
        let schema = r#"
type Token @entity {
  id: ID!
  decimals: BigInt!
}
"#;
        let mapping = r#"
export function fetchTokenDecimals(tokenAddress: Address): BigInt {
  let contract = ERC20.bind(tokenAddress)
  return contract.try_decimals().value
}
export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  const decimals = fetchTokenDecimals(event.params.token0)
  token0.decimals = decimals
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/factory.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "decimals"), Class::CallDerived);
    }

    /// Jules on #1242. `metadata` is a plain object with no binding, so before the guard the
    /// unique-field fallback took `metadata.symbol = ..` for a write to `Token.symbol` on the sole
    /// evidence that `symbol` is uniquely owned, and reported the field exact.
    #[test]
    fn an_unresolved_receiver_that_is_never_saved_is_not_an_entity_write() {
        let schema_src = r#"
type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let plain_object = r#"
export function handleTransfer(event: Transfer): void {
  let metadata = loadMetadata(event.address)
  metadata.symbol = event.params.symbol
}
"#;
        let (schema, mappings) = schema_and_mappings(schema_src, "src/token.ts", plain_object);
        let rows = classify(&schema, &mappings);
        assert_eq!(
            class_of(&rows, "Token", "symbol"),
            Class::Unreachable,
            "an assignment to an object nothing saves is not a write to Token.symbol: {}",
            reason_of(&rows, "Token", "symbol")
        );
        assert!(
            reason_of(&rows, "Token", "symbol").contains("no mapping writes this field"),
            "{}",
            reason_of(&rows, "Token", "symbol")
        );

        // Saved, so it is an entity, and the unique field then says which one. This is the case
        // the fallback exists for, and it must keep working.
        let saved = r#"
export function handleTransfer(event: Transfer): void {
  let token = getOrMakeToken(event.address)
  token.symbol = event.params.symbol
  token.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema_src, "src/token.ts", saved);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::Exact);
        assert!(reason_of(&rows, "Token", "symbol").contains("event.params.symbol"));
    }

    #[test]
    fn create_or_load_binds_the_entity() {
        let schema = r#"
type Transcoder @entity {
  id: ID!
  serviceURI: String!
}
"#;
        let mapping = r#"
export function serviceURIUpdate(event: ServiceURIUpdate): void {
  let transcoder = createOrLoadTranscoder(event.params.addr.toHex())
  transcoder.serviceURI = event.params.serviceURI
  transcoder.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/serviceRegistry.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Transcoder", "serviceURI"), Class::Exact);
        assert!(reason_of(&rows, "Transcoder", "serviceURI").contains("event.params.serviceURI"));
    }

    #[test]
    fn contract_try_is_call_derived() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let mapping = r#"
export function fetchTokenSymbol(tokenAddress: Address): string {
  let contract = ERC20.bind(tokenAddress)
  let result = contract.try_symbol()
  if (!result.reverted) {
    return result.value
  }
  return 'unknown'
}

export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  token0.symbol = fetchTokenSymbol(event.params.token0)
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/token.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::CallDerived);
        assert!(reason_of(&rows, "Token", "symbol").contains("contract state"));
    }

    #[test]
    fn multiline_helper_rhs_is_call_derived() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
  name: String!
}
"#;
        let mapping = r#"
export function fetchTokenSymbol(tokenAddress: Address): string {
  let contract = ERC20.bind(tokenAddress)
  return contract.try_symbol().value
}
export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  token0.symbol =
    fetchTokenSymbol(event.params.token0)
  token0.name = event.params.name
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/token.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::CallDerived);
        assert_eq!(class_of(&rows, "Token", "name"), Class::Exact);
        assert!(reason_of(&rows, "Token", "symbol").contains("fetchTokenSymbol"));
    }

    #[test]
    fn nested_helper_is_call_derived() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let mapping = r#"
export function readSymbol(tokenAddress: Address): string {
  let contract = ERC20.bind(tokenAddress)
  return contract.try_symbol().value
}
export function fetchSymbol(tokenAddress: Address): string {
  return readSymbol(tokenAddress)
}
export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  token0.symbol = fetchSymbol(event.params.token0)
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/token.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::CallDerived);
        assert!(reason_of(&rows, "Token", "symbol").contains("contract state"));
    }

    const LOOP_SCHEMA: &str = r#"
type Token @entity {
  id: ID!
  derivedETH: BigDecimal!
}
type Pool @entity {
  id: ID!
  token1Price: BigDecimal!
  liquidity: BigInt!
}
"#;

    /// Jules on #1242. `has_loop && body.contains(".load(")` was body-wide, so a `.load()` sitting
    /// outside every loop still made the helper a fixed point and every field written from it
    /// non-reproducible. The class only reaches a field through a call, so the helper is called.
    #[test]
    fn a_load_outside_every_loop_is_not_a_fixed_point() {
        let mapping = r#"
export function sumAmounts(event: Mint): BigDecimal {
  const pool = Pool.load(event.address.toHex())
  let total = ZERO_BD
  for (let i = 0; i < event.params.amounts.length; ++i) {
    total = total.plus(event.params.amounts[i])
  }
  return total
}

export function handleMint(event: Mint): void {
  let token = new Token(event.params.token.toHex())
  token.derivedETH = sumAmounts(event)
  token.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(LOOP_SCHEMA, "src/pool.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(
            class_of(&rows, "Token", "derivedETH"),
            Class::Exact,
            "the load is outside the loop, so nothing depends on earlier-block state: {}",
            reason_of(&rows, "Token", "derivedETH")
        );
    }

    /// A loop that loads and writes, reading nothing off what it loaded, reproduces exactly.
    #[test]
    fn a_loop_that_loads_without_reading_a_field_is_not_a_fixed_point() {
        let mapping = r#"
export function countPools(event: Sync): BigDecimal {
  let n = ZERO_BD
  for (let i = 0; i < event.params.pools.length; ++i) {
    const pool = Pool.load(event.params.pools[i])
    if (pool) {
      n = n.plus(ONE_BD)
    }
  }
  return n
}

export function handleSync(event: Sync): void {
  let token = new Token(event.params.token.toHex())
  token.derivedETH = countPools(event)
  token.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(LOOP_SCHEMA, "src/pool.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(
            class_of(&rows, "Token", "derivedETH"),
            Class::Exact,
            "a loop that only loads and counts has no cross-block dependency: {}",
            reason_of(&rows, "Token", "derivedETH")
        );
    }

    /// And the guard must not have removed the class: the same shape with a read of the loaded
    /// entity's stored field is still a fixed point.
    #[test]
    fn a_loop_reading_a_loaded_entitys_field_is_still_a_fixed_point() {
        let mapping = r#"
export function priceFromPools(event: Sync): BigDecimal {
  let priceSoFar = ZERO_BD
  for (let i = 0; i < event.params.pools.length; ++i) {
    const pool = Pool.load(event.params.pools[i])
    if (pool) {
      priceSoFar = priceSoFar.plus(pool.token1Price)
    }
  }
  return priceSoFar
}

export function handleSync(event: Sync): void {
  let token = new Token(event.params.token.toHex())
  token.derivedETH = priceFromPools(event)
  token.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(LOOP_SCHEMA, "src/pool.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(
            class_of(&rows, "Token", "derivedETH"),
            Class::FixedPoint,
            "reading pool.token1Price inside the loop is the dependency: {}",
            reason_of(&rows, "Token", "derivedETH")
        );
    }

    #[test]
    fn find_eth_per_token_is_fixed_point() {
        let schema = r#"
type Token @entity {
  id: ID!
  derivedETH: BigDecimal!
  whitelistPools: [Pool!]!
}
type Pool @entity {
  id: ID!
  token0: Token!
  token1: Token!
  token0Price: BigDecimal!
  token1Price: BigDecimal!
  totalValueLockedToken0: BigDecimal!
  totalValueLockedToken1: BigDecimal!
  liquidity: BigInt!
}
type Bundle @entity {
  id: ID!
  ethPriceUSD: BigDecimal!
}
"#;
        let mapping = r#"
export function findEthPerToken(token: Token): BigDecimal {
  let whiteList = token.whitelistPools
  let priceSoFar = ZERO_BD
  for (let i = 0; i < whiteList.length; ++i) {
    const pool = Pool.load(whiteList[i])
    if (pool) {
      if (pool.token0 == token.id) {
        const token1 = Token.load(pool.token1)
        if (token1) {
          priceSoFar = pool.token1Price.times(token1.derivedETH as BigDecimal)
        }
      }
    }
  }
  return priceSoFar
}

export function handleSwap(event: SwapEvent): void {
  let token0 = Token.load(event.address.toHex())!
  token0.derivedETH = findEthPerToken(token0 as Token)
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/pricing.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "derivedETH"), Class::FixedPoint);
        let reason = reason_of(&rows, "Token", "derivedETH");
        assert!(
            reason.contains("will not reproduce"),
            "fixed-point reason must say it will not reproduce, got {reason}"
        );
        assert!(reason.contains("pricing.ts") || reason.contains("findEthPerToken"));
    }

    #[test]
    fn assignment_reading_fixed_point_field_is_fixed_point() {
        let schema = r#"
type Token @entity { id: ID! derivedETH: BigDecimal! }
type Pool @entity { id: ID! usd: BigDecimal! }
"#;
        let mapping = r#"
export function findEthPerToken(token: Token): BigDecimal {
  let priceSoFar = ZERO_BD
  for (let i = 0; i < 1; ++i) {
    const pool = Pool.load(token.id)
    if (pool) {
      const other = Token.load(pool.id)
      if (other) {
        priceSoFar = other.derivedETH
      }
    }
  }
  return priceSoFar
}

export function handleSwap(event: SwapEvent): void {
  let token = Token.load(event.address.toHex())!
  token.derivedETH = findEthPerToken(token as Token)
  token.save()
  let pool = Pool.load(event.address.toHex())!
  pool.usd = token.derivedETH
  pool.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/pricing.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "derivedETH"), Class::FixedPoint);
        assert_eq!(class_of(&rows, "Pool", "usd"), Class::FixedPoint);
    }

    #[test]
    fn get_eth_price_is_exact() {
        let schema = r#"
type Pool @entity {
  id: ID!
  token0: Token!
  token1Price: BigDecimal!
  token0Price: BigDecimal!
}
type Token @entity { id: ID! }
type Bundle @entity {
  id: ID!
  ethPriceUSD: BigDecimal!
}
"#;
        let mapping = r#"
export function getEthPriceInUSD(): BigDecimal {
  let usdcPool = Pool.load(Address.fromString(stablePool))
  if (usdcPool) {
    if (usdcPool.token0 == Address.fromString(REFERENCE_TOKEN)) return usdcPool.token1Price
    else return usdcPool.token0Price
  }
  return ZERO_BD
}

export function handleSwap(event: SwapEvent): void {
  let bundle = new Bundle('1')
  bundle.ethPriceUSD = getEthPriceInUSD()
  bundle.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/pricing.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Bundle", "ethPriceUSD"), Class::Exact);
    }

    #[test]
    fn derived_from_is_exact_with_schema_citation() {
        let schema = r#"
type Pool @entity {
  id: ID!
  swaps: [Swap!]! @derivedFrom(field: "pool")
}
type Swap @entity {
  id: ID!
  pool: Pool!
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/x.ts", "");
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Pool", "swaps"), Class::Exact);
        let row = rows.iter().find(|r| r.field == "swaps").unwrap();
        assert_eq!(row.citation.file, "schema.graphql");
        assert!(row.reason.contains("@derivedFrom"));
    }

    #[test]
    fn fulltext_is_unreachable() {
        let schema = r#"
type _Schema_
  @fulltext(
    name: "tokenSearch"
    language: en
    algorithm: rank
    include: [{ entity: "Token", fields: [{ name: "symbol" }] }]
  )

type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/x.ts", "");
        let rows = classify(&schema, &mappings);
        assert_eq!(
            class_of(&rows, "_Schema_", "tokenSearch"),
            Class::Unreachable
        );
        assert!(reason_of(&rows, "_Schema_", "tokenSearch").contains("@fulltext"));
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::Unreachable);
        assert!(reason_of(&rows, "Token", "symbol").contains("no mapping writes"));
    }

    #[test]
    fn schema_underscore_does_not_swallow_the_next_entity() {
        let schema = r#"
type _Schema_
  @fulltext(
    name: "tokenSearch"
    language: en
    algorithm: rank
  )

type Bundle @entity {
  id: ID!
  ethPriceUSD: BigDecimal!
}

type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/x.ts", "");
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Bundle", "id"), Class::Unreachable);
        assert_eq!(class_of(&rows, "Bundle", "ethPriceUSD"), Class::Unreachable);
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::Unreachable);
        assert_eq!(
            class_of(&rows, "_Schema_", "tokenSearch"),
            Class::Unreachable
        );
    }

    #[test]
    fn fold_of_own_field_with_event_stays_exact() {
        let schema = r#"
type Pool @entity {
  id: ID!
  totalValueLockedToken0: BigDecimal!
}
"#;
        let mapping = r#"
export function handleSwap(event: SwapEvent): void {
  let pool = Pool.load(event.address.toHex())!
  pool.totalValueLockedToken0 = pool.totalValueLockedToken0.plus(event.params.amount0)
  pool.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/core.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(
            class_of(&rows, "Pool", "totalValueLockedToken0"),
            Class::Exact
        );
    }

    #[test]
    fn worst_class_wins_across_writers() {
        let schema = r#"
type Token @entity {
  id: ID!
  derivedETH: BigDecimal!
}
type Pool @entity {
  id: ID!
  token1: Token!
  token1Price: BigDecimal!
}
"#;
        let mapping = r#"
export function findEthPerToken(token: Token): BigDecimal {
  for (let i = 0; i < 1; ++i) {
    const pool = Pool.load(token.id)
    const token1 = Token.load(pool.token1)
    return pool.token1Price.times(token1.derivedETH)
  }
  return ZERO_BD
}
export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  token0.derivedETH = ZERO_BD
  token0.save()
}
export function handleSwap(event: SwapEvent): void {
  let token0 = Token.load(event.address.toHex())!
  token0.derivedETH = findEthPerToken(token0)
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/pricing.ts", mapping);
        let rows = classify(&schema, &mappings);
        assert_eq!(class_of(&rows, "Token", "derivedETH"), Class::FixedPoint);
    }

    #[test]
    fn report_names_every_schema_field() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/x.ts", "");
        let rows = classify(&schema, &mappings);
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn render_says_non_exact_will_not_reproduce() {
        let schema = r#"
type Token @entity { id: ID! leftover: String! }
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/x.ts", "");
        let rows = classify(&schema, &mappings);
        let report = Report {
            source: "fixture".into(),
            fields: rows,
        };
        let text = render_report(&report);
        assert!(text.contains("will not reproduce"));
        assert!(text.contains("--from-subgraph"));
        assert!(text.contains("watch"));
        assert!(text.contains("service_u_r_i_update"));
        assert!(!text.contains("nuthatch init 0x") || text.contains("not"));
    }

    /// Jules on #1242. GraphQL puts no meaning on newlines. Splitting the body by line and taking
    /// the first `name:` from each recorded `id` and silently dropped every field beside it, so the
    /// report told the author about fields that were not there and never mentioned ones that were.
    #[test]
    fn a_compact_entity_declaration_keeps_every_field() {
        let schema = parse_schema(
            "type Token @entity { id: ID! derivedETH: BigDecimal! symbol: String! }\n",
        )
        .unwrap();
        let token = schema.entities.iter().find(|e| e.name == "Token").unwrap();
        let names: Vec<&str> = token.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["id", "derivedETH", "symbol"], "{names:?}");
    }

    /// A colon inside a directive's arguments or a list type is not a field boundary, and a `#`
    /// comment is not a field at all.
    #[test]
    fn a_directive_argument_colon_is_not_a_field() {
        let schema = parse_schema(
            "type Pool @entity {\n  id: ID!\n  # ignored: NotAField\n  ticks: [Tick!]! \n               swaps: [Swap!]! @derivedFrom(field: \"pool\")\n  fee: BigInt!\n}\n",
        )
        .unwrap();
        let pool = schema.entities.iter().find(|e| e.name == "Pool").unwrap();
        let names: Vec<&str> = pool.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["id", "ticks", "swaps", "fee"], "{names:?}");
        let swaps = pool.fields.iter().find(|f| f.name == "swaps").unwrap();
        assert_eq!(
            swaps.derived_from.as_deref(),
            Some("pool"),
            "the directive must still attach to its field"
        );
    }

    /// The citation is the line the field is on, and a compact declaration puts several on one.
    #[test]
    fn a_field_citation_is_its_own_line() {
        let schema =
            parse_schema("\n\ntype Token @entity {\n  id: ID!\n  symbol: String!\n}\n").unwrap();
        let token = schema.entities.iter().find(|e| e.name == "Token").unwrap();
        let line_of_field = |n: &str| token.fields.iter().find(|f| f.name == n).unwrap().line;
        assert_eq!(line_of_field("id"), 4, "id is on line 4");
        assert_eq!(line_of_field("symbol"), 5, "symbol is on line 5");
    }

    #[test]
    fn schema_parse_rejects_empty() {
        assert!(parse_schema("enum Foo { A }").is_err());
    }

    #[test]
    fn citation_is_the_assignment_line() {
        let schema = r#"
type Swap @entity {
  id: ID!
  amount0: BigDecimal!
}
"#;
        let mapping = "export function handleSwap(event: SwapEvent): void {\n  let swap = new Swap('x')\n  swap.amount0 = event.params.amount0\n}\n";
        let (schema, mappings) = schema_and_mappings(schema, "src/core.ts", mapping);
        let rows = classify(&schema, &mappings);
        let row = rows.iter().find(|r| r.field == "amount0").unwrap();
        assert_eq!(row.citation.file, "src/core.ts");
        assert_eq!(row.citation.line, 3);
    }
}
