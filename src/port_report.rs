//! RFC-0044 S1: classify a subgraph's `schema.graphql` and mappings, emit the port report.
//!
//! Nothing here runs in the data path and nothing executes AssemblyScript. The mappings are
//! read as text. The report is the deliverable; scaffolding a nest is RFC-0044 S2
//! (`port_emit`).

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
    pub fn display(&self) -> String {
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

fn parse_fields(body: &str, start_line: usize) -> Vec<SchemaField> {
    let mut fields = Vec::new();
    let mut pending_name: Option<(String, usize)> = None;
    let mut pending_dirs = String::new();
    for (offset, raw) in body.lines().enumerate() {
        let line_no = start_line + offset + 1;
        let stripped = strip_graphql_line_comment(raw);
        let line = stripped.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((name, ty_and_dir)) = parse_field_line(line) {
            flush_field(&mut fields, pending_name.take(), &pending_dirs);
            pending_dirs.clear();
            if let Some(dir) = ty_and_dir {
                pending_dirs.push_str(dir);
            }
            pending_name = Some((name, line_no));
        } else if pending_name.is_some() && line.starts_with('@') {
            pending_dirs.push(' ');
            pending_dirs.push_str(line);
        }
    }
    flush_field(&mut fields, pending_name, &pending_dirs);
    fields
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

fn parse_field_line(line: &str) -> Option<(String, Option<&str>)> {
    let line = line.trim();
    if line.starts_with('@') || line.starts_with('}') || line.starts_with('#') {
        return None;
    }
    let colon = line.find(':')?;
    let name = line[..colon].trim();
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return None;
    }
    let rest = line[colon + 1..].trim();
    let dir = rest.find('@').map(|i| rest[i..].trim());
    Some((name.to_string(), dir))
}

fn strip_graphql_line_comment(line: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    let chars = line.chars().peekable();
    for c in chars {
        if c == '"' {
            in_str = !in_str;
            out.push(c);
            continue;
        }
        if !in_str && c == '#' {
            break;
        }
        out.push(c);
    }
    out
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
pub(crate) enum HandlerKind {
    Event,
    Call,
    Block,
    Helper,
}

#[derive(Debug, Clone)]
pub(crate) struct FunctionInfo {
    pub name: String,
    pub kind: HandlerKind,
    pub assignments: Vec<Assignment>,
    calls: BTreeSet<String>,
    contract_call: Option<Citation>,
    has_loop_load: bool,
    field_reads: Vec<(String, String)>, // (entity, field)
    /// Source order, so `fetchTokenSymbol(event.params.token0)` can map the helper's bind
    /// argument back onto the triggering row.
    param_names: Vec<String>,
    body: String,
    file: String,
    body_start_line: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct Assignment {
    pub entity: String,
    pub field: String,
    pub citation: Citation,
    pub expr: String,
}

/// An `eventHandlers` entry: which source and event the named handler is wired to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandlerBinding {
    pub handler: String,
    pub source: String,
    pub event: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Mappings {
    pub functions: BTreeMap<String, FunctionInfo>,
    pub handlers: Vec<HandlerBinding>,
}

/// A contract read the mapping actually makes, tied to the Call-derived field it feeds.
///
/// `signature` is the ABI form (`symbol()`, `decimals()`), taken from `.try_*` / `.bind`.
/// `contract_arg` is the bind argument after substituting the helper's parameters for the
/// caller's, still in mapping syntax (`event.params.token0`). Emit turns that into a
/// `contract_column` or a literal address. A call we cannot trace is not returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappingCall {
    pub entity: String,
    pub field: String,
    pub handler: String,
    pub signature: String,
    pub contract_arg: String,
    pub args: Vec<String>,
    pub citation: Citation,
}

pub(crate) fn load_mappings(dir: &Path) -> Result<Mappings> {
    let mut handler_kinds: BTreeMap<String, HandlerKind> = BTreeMap::new();
    let mut handler_bindings: Vec<HandlerBinding> = Vec::new();
    let mut yaml_files: Vec<PathBuf> = Vec::new();
    for name in ["subgraph.yaml", "subgraph.yml"] {
        let p = dir.join(name);
        if p.exists() {
            if let Ok(text) = std::fs::read_to_string(&p) {
                collect_manifest_hints(
                    dir,
                    &text,
                    &mut handler_kinds,
                    &mut handler_bindings,
                    &mut yaml_files,
                );
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
    Ok(Mappings {
        functions,
        handlers: handler_bindings,
    })
}

fn collect_manifest_hints(
    dir: &Path,
    text: &str,
    kinds: &mut BTreeMap<String, HandlerKind>,
    bindings: &mut Vec<HandlerBinding>,
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
            let source = get_yaml(src, "name").and_then(|v| v.as_str()).unwrap_or("");
            take_event_bindings(mapping, source, bindings);
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

fn take_event_bindings(mapping: &Yaml, source: &str, bindings: &mut Vec<HandlerBinding>) {
    let Some(arr) = get_yaml(mapping, "eventHandlers").and_then(|v| v.as_vec()) else {
        return;
    };
    for entry in arr {
        let Some(handler) = get_yaml(entry, "handler").and_then(|v| v.as_str()) else {
            continue;
        };
        let event = get_yaml(entry, "event")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let event_name = event.split('(').next().unwrap_or(event).trim();
        if event_name.is_empty() || source.is_empty() {
            continue;
        }
        bindings.push(HandlerBinding {
            handler: handler.to_string(),
            source: source.to_string(),
            event: event_name.to_string(),
        });
    }
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
                    let param_names = param_names_of(sig);
                    let body = &stripped[open + 1..close];
                    let start_line = line_of(&stripped, open);
                    out.push(analyse_function(
                        name,
                        file,
                        body,
                        start_line,
                        params,
                        param_names,
                    ));
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
    let bytes = text.as_bytes();
    if open >= bytes.len() || bytes[open] != b'{' {
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
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
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

fn param_names_of(sig: &str) -> Vec<String> {
    let start = match sig.find('(') {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    let end = match sig[start..].find(')') {
        Some(i) => start + i,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for part in sig[start..end].split(',') {
        let part = part.trim();
        let Some(colon) = part.find(':') else {
            continue;
        };
        let var = part[..colon].trim();
        if !var.is_empty() {
            out.push(var.to_string());
        }
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
    param_names: Vec<String>,
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
    let has_loop = has_loop(body);
    let has_load = body.contains(".load(");
    let field_reads = collect_field_reads(body, &bindings);
    FunctionInfo {
        name,
        kind: HandlerKind::Helper,
        assignments,
        calls,
        contract_call,
        has_loop_load: has_loop && has_load,
        field_reads,
        param_names,
        body: body.to_string(),
        file: file.to_string(),
        body_start_line,
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
    if ent.is_empty() || !ent.starts_with(|c: char| c.is_ascii_uppercase()) {
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
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some((var, field, eq_at)) = match_assign(body, i) {
            let entity = bindings.get(&var).cloned().unwrap_or_default();
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
            });
            i = eq_at + 1;
            continue;
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

fn has_loop(body: &str) -> bool {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
            && (body[i..].starts_with("for") || body[i..].starts_with("while"))
        {
            let kw_len = if body[i..].starts_with("for") { 3 } else { 5 };
            let after = i + kw_len;
            if after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_')
            {
                return true;
            }
        }
        i += 1;
    }
    false
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
            let entity = if entity_names.contains(a.entity.as_str()) {
                a.entity.clone()
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

// ── Mapping calls (RFC-0044 S2) ─────────────────────────────────────────────

/// Contract reads that feed Call-derived fields. Each entry cites the `.try_*` / `.bind` line,
/// not a guessed getter. Fields the classifier did not call Call-derived are skipped, so dropping
/// the read from the mapping drops the entry.
pub fn mapping_calls(dir: &Path) -> Result<Vec<MappingCall>> {
    let report = classify_dir(dir)?;
    let mappings = load_mappings(dir)?;
    Ok(calls_from_mappings(&report, &mappings))
}

pub(crate) fn calls_from_mappings(report: &Report, mappings: &Mappings) -> Vec<MappingCall> {
    let call_fields: BTreeSet<(String, String)> = report
        .fields
        .iter()
        .filter(|f| f.class == Class::CallDerived)
        .map(|f| (f.entity.clone(), f.field.clone()))
        .collect();
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for func in mappings.functions.values() {
        if func.kind == HandlerKind::Block {
            continue;
        }
        for asg in &func.assignments {
            if !call_fields.contains(&(asg.entity.clone(), asg.field.clone())) {
                continue;
            }
            if let Some(call) = derive_mapping_call(func, asg, mappings) {
                let key = (
                    call.entity.clone(),
                    call.field.clone(),
                    call.signature.clone(),
                    call.contract_arg.clone(),
                    call.args.clone(),
                );
                if seen.insert(key) {
                    out.push(call);
                }
            }
        }
    }
    out.sort_by(|a, b| {
        a.entity
            .cmp(&b.entity)
            .then(a.field.cmp(&b.field))
            .then(a.contract_arg.cmp(&b.contract_arg))
            .then(a.signature.cmp(&b.signature))
    });
    out
}

fn derive_mapping_call(
    writer: &FunctionInfo,
    asg: &Assignment,
    mappings: &Mappings,
) -> Option<MappingCall> {
    if let Some(call) = call_from_body(writer, asg, None, &asg.expr) {
        return Some(call);
    }
    let mut seen = BTreeSet::new();
    seen.insert(writer.name.clone());
    call_via_helpers(writer, asg, mappings, writer, &asg.expr, &mut seen)
}

/// A wrapper that only forwards (`fetchTokenSymbol` → `readTokenSymbol` → `try_symbol`)
/// still has to emit the [[calls]] stanza; otherwise classify and emit disagree.
fn call_via_helpers(
    func: &FunctionInfo,
    asg: &Assignment,
    mappings: &Mappings,
    writer: &FunctionInfo,
    expr: &str,
    seen: &mut BTreeSet<String>,
) -> Option<MappingCall> {
    let at_writer = func.name == writer.name;
    for callee in &func.calls {
        if at_writer && !expr_calls(expr, callee) {
            continue;
        }
        if !seen.insert(callee.clone()) {
            continue;
        }
        let Some(helper) = mappings.functions.get(callee) else {
            continue;
        };
        let invoke = if at_writer {
            expr.to_string()
        } else {
            return_exprs(&func.body)
                .into_iter()
                .find(|r| expr_calls(r, callee))
                .unwrap_or_else(|| func.body.clone())
        };
        let caller_args = call_arg_list(&invoke, callee);
        let found = if helper.contract_call.is_some() || helper.body.contains(".try_") {
            call_from_body(helper, asg, Some(writer), &asg.expr)
        } else {
            call_via_helpers(helper, asg, mappings, writer, &invoke, seen)
        };
        if let Some(call) = found {
            let contract_arg = subst_params(&call.contract_arg, &helper.param_names, &caller_args);
            let args = call
                .args
                .iter()
                .map(|a| subst_params(a, &helper.param_names, &caller_args))
                .collect();
            return Some(MappingCall {
                handler: writer.name.clone(),
                contract_arg,
                args,
                ..call
            });
        }
    }
    None
}

fn call_from_body(
    func: &FunctionInfo,
    asg: &Assignment,
    writer: Option<&FunctionInfo>,
    expr: &str,
) -> Option<MappingCall> {
    let sites = find_try_sites(&func.body, &func.file, func.body_start_line);
    let use_returns = writer.is_some() || func.kind == HandlerKind::Helper;
    let site = pick_try_site(&sites, &func.body, expr, use_returns)?;
    let bind_arg = site
        .bind_arg
        .clone()
        .or_else(|| bind_arg_for(&func.body, &site.receiver))?;
    let sig_types: Vec<&str> = site
        .args
        .iter()
        .map(|a| sol_type_of_expr(a, func))
        .collect::<Option<Vec<_>>>()?;
    let signature = if sig_types.is_empty() {
        format!("{}()", site.method)
    } else {
        format!("{}({})", site.method, sig_types.join(","))
    };
    Some(MappingCall {
        entity: asg.entity.clone(),
        field: asg.field.clone(),
        handler: writer.unwrap_or(func).name.clone(),
        signature,
        contract_arg: bind_arg,
        args: site.args.clone(),
        citation: site.citation.clone(),
    })
}

/// The `.try_*` that actually feeds `expr` (the assignment) or, in a helper, the returned value.
/// A helper that calls `try_symbol` and then returns `try_decimals` must not emit `symbol()` for
/// the decimals field.
fn pick_try_site<'a>(
    sites: &'a [TrySite],
    body: &str,
    expr: &str,
    use_returns: bool,
) -> Option<&'a TrySite> {
    if sites.is_empty() {
        return None;
    }
    let locals = locals_bound_to_try(body, sites);
    if let Some(site) = site_for_text(expr, sites, &locals) {
        return Some(site);
    }
    if use_returns {
        for ret in return_exprs(body) {
            if let Some(site) = site_for_text(&ret, sites, &locals) {
                return Some(site);
            }
        }
        if sites.len() == 1 {
            return sites.first();
        }
    }
    None
}

fn site_for_text<'a>(
    text: &str,
    sites: &'a [TrySite],
    locals: &BTreeMap<String, &'a TrySite>,
) -> Option<&'a TrySite> {
    for site in sites {
        if ident_in(text, &format!("try_{}", site.method)) {
            return Some(site);
        }
    }
    for (var, site) in locals {
        if uses_local(text, var) {
            return Some(site);
        }
    }
    None
}

fn locals_bound_to_try<'a>(body: &str, sites: &'a [TrySite]) -> BTreeMap<String, &'a TrySite> {
    let mut map = BTreeMap::new();
    let mut i = 0;
    while i < body.len() {
        if let Some((var, rhs, next)) = match_let_rhs(body, i) {
            if let Some(site) = sites
                .iter()
                .find(|s| ident_in(&rhs, &format!("try_{}", s.method)))
            {
                map.insert(var, site);
            }
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }
    map
}

fn match_let_rhs(text: &str, i: usize) -> Option<(String, String, usize)> {
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
    if k >= text.len() || text.as_bytes()[k] != b'=' {
        return None;
    }
    if k + 1 < text.len() {
        let n = text.as_bytes()[k + 1];
        if n == b'=' || n == b'>' {
            return None;
        }
    }
    let rhs = take_expr(text, k + 1);
    Some((var, collapse_ws(&rhs), k + 1))
}

fn return_exprs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if body[i..].starts_with("return") {
            let before_ok =
                i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
            let after = i + 6;
            let after_ok = after >= bytes.len()
                || !(bytes[after].is_ascii_alphanumeric() || bytes[after] == b'_');
            if before_ok && after_ok {
                let expr = take_expr(body, after);
                if !expr.is_empty() {
                    out.push(collapse_ws(&expr));
                }
            }
        }
        i += 1;
    }
    out
}

fn ident_in(text: &str, ident: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = ident.as_bytes();
    if needle.is_empty() || needle.len() > bytes.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || {
                let p = bytes[i - 1];
                !(p.is_ascii_alphanumeric() || p == b'_')
            };
            let after_ok = i + needle.len() == bytes.len() || {
                let n = bytes[i + needle.len()];
                !(n.is_ascii_alphanumeric() || n == b'_')
            };
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn uses_local(text: &str, var: &str) -> bool {
    let bytes = text.as_bytes();
    let needle = var.as_bytes();
    if needle.is_empty() || needle.len() > bytes.len() {
        return false;
    }
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || {
                let p = bytes[i - 1];
                !(p.is_ascii_alphanumeric() || p == b'_' || p == b'.')
            };
            let after_ok = i + needle.len() == bytes.len() || {
                let n = bytes[i + needle.len()];
                !(n.is_ascii_alphanumeric() || n == b'_')
            };
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[derive(Debug, Clone)]
struct TrySite {
    method: String,
    receiver: String,
    bind_arg: Option<String>,
    args: Vec<String>,
    citation: Citation,
}

fn find_try_sites(body: &str, file: &str, body_start_line: usize) -> Vec<TrySite> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &body[i..];
        let Some(at) = rest.find(".try_") else {
            break;
        };
        let dot = i + at;
        let method_start = dot + 5;
        let mut k = method_start;
        while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
            k += 1;
        }
        if k == method_start {
            i = method_start;
            continue;
        }
        let method = body[method_start..k].to_string();
        let mut j = k;
        skip_ws_str(body, &mut j);
        if j >= bytes.len() || bytes[j] != b'(' {
            i = k;
            continue;
        }
        let args_raw = take_paren_list(body, j);
        let receiver = ident_before(body, dot).unwrap_or_default();
        let bind_arg = chained_bind_arg(body, dot);
        let line = body_start_line + line_of(body, dot) - 1;
        out.push(TrySite {
            method,
            receiver,
            bind_arg,
            args: args_raw,
            citation: Citation {
                file: file.to_string(),
                line,
            },
        });
        i = k;
    }
    out
}

fn chained_bind_arg(body: &str, try_dot: usize) -> Option<String> {
    // `ERC20.bind(addr).try_symbol()` - the bind sits immediately before this `.try_`.
    let before = &body[..try_dot];
    let bind_at = before.rfind(".bind(")?;
    let close = match_paren(body, bind_at + 5)?;
    if close >= try_dot {
        return None;
    }
    if !body[close + 1..try_dot].trim().is_empty() {
        return None;
    }
    let inner = body[bind_at + 6..close].trim();
    if inner.is_empty() {
        return None;
    }
    Some(collapse_ws(inner))
}

fn bind_arg_for(body: &str, receiver: &str) -> Option<String> {
    if receiver.is_empty() {
        return first_bind_arg(body);
    }
    // `let contract = ERC20.bind(tokenAddress)`
    let mut i = 0;
    while i < body.len() {
        if !body.is_char_boundary(i) {
            i += 1;
            continue;
        }
        if let Some((var, arg, next)) = match_bind_assign(body, i) {
            if var == receiver {
                return Some(arg);
            }
            i = next;
            continue;
        }
        i += 1;
    }
    first_bind_arg(body)
}

fn first_bind_arg(body: &str) -> Option<String> {
    let at = body.find(".bind(")?;
    let close = match_paren(body, at + 5)?;
    let inner = body[at + 6..close].trim();
    if inner.is_empty() {
        None
    } else {
        Some(collapse_ws(inner))
    }
}

fn match_bind_assign(text: &str, i: usize) -> Option<(String, String, usize)> {
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
    let _ty = take_ident_str(text, &mut k)?;
    skip_ws_str(text, &mut k);
    if !text[k..].starts_with(".bind(") {
        return None;
    }
    let open = k + 5;
    let close = match_paren(text, open)?;
    let arg = collapse_ws(text[open + 1..close].trim());
    Some((var, arg, close + 1))
}

fn match_paren(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if open >= bytes.len() || bytes[open] != b'(' {
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
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn take_paren_list(text: &str, open: usize) -> Vec<String> {
    let Some(close) = match_paren(text, open) else {
        return Vec::new();
    };
    let inner = text[open + 1..close].trim();
    if inner.is_empty() {
        return Vec::new();
    }
    split_args(inner)
}

fn split_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let bytes = inner.as_bytes();
    let mut i = 0;
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
        if b == b'(' || b == b'[' {
            depth += 1;
        } else if b == b')' || b == b']' {
            depth -= 1;
        } else if b == b',' && depth == 0 {
            let part = inner[start..i].trim();
            if !part.is_empty() {
                out.push(collapse_ws(part));
            }
            start = i + 1;
        }
        i += 1;
    }
    let part = inner[start..].trim();
    if !part.is_empty() {
        out.push(collapse_ws(part));
    }
    out
}

fn ident_before(text: &str, dot: usize) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = dot;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 {
        return None;
    }
    let end = i;
    while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    if i == end {
        return None;
    }
    Some(text[i..end].to_string())
}

fn call_arg_list(expr: &str, name: &str) -> Vec<String> {
    let needle = format!("{name}(");
    let Some(at) = expr.find(&needle) else {
        return Vec::new();
    };
    if at > 0 {
        let p = expr.as_bytes()[at - 1];
        if p.is_ascii_alphanumeric() || p == b'_' || p == b'.' {
            return Vec::new();
        }
    }
    take_paren_list(expr, at + name.len())
}

fn subst_params(expr: &str, params: &[String], args: &[String]) -> String {
    let ident = strip_converters(expr);
    if let Some(idx) = params.iter().position(|p| p == &ident) {
        if let Some(arg) = args.get(idx) {
            return strip_converters(arg);
        }
    }
    strip_converters(expr)
}

/// Drop `.toHex()` / `.toHexString()` / `.toString()` so `event.params.token0.toHex()` is still
/// the `token0` column.
pub(crate) fn strip_converters(expr: &str) -> String {
    let mut s = collapse_ws(expr);
    loop {
        let next = s
            .strip_suffix(".toHex()")
            .or_else(|| s.strip_suffix(".toHexString()"))
            .or_else(|| s.strip_suffix(".toString()"))
            .or_else(|| s.strip_suffix(".toI32()"))
            .or_else(|| s.strip_suffix(".toU32()"));
        match next {
            Some(n) => s = n.trim().to_string(),
            None => break,
        }
    }
    s
}

/// Event column that identifies `entity` in this function: an `id` assignment, else
/// `new Entity(event.params.x)` / `Entity.load(event.params.x)`.
pub(crate) fn entity_id_event_column(entity: &str, func: &FunctionInfo) -> Option<String> {
    for asg in &func.assignments {
        if asg.entity == entity && asg.field == "id" {
            if let Some(col) = assignment_event_column(asg, func) {
                return Some(col);
            }
        }
    }
    id_from_new_or_load(entity, &func.body)
}

fn id_from_new_or_load(entity: &str, body: &str) -> Option<String> {
    let mut i = 0;
    while i < body.len() {
        if let Some((_var, ent, next)) = match_let_new(body, i).or_else(|| match_bare_new(body, i))
        {
            if ent == entity {
                let mut k = next;
                skip_ws_str(body, &mut k);
                if body[k..].starts_with('(') {
                    if let Some(col) = event_column(&take_expr(body, k + 1)) {
                        return Some(col);
                    }
                }
            }
            i = next.max(i + 1);
            continue;
        }
        if let Some((_var, ent, next)) = match_let_load(body, i) {
            if ent == entity {
                let mut k = next;
                skip_ws_str(body, &mut k);
                if body[k..].starts_with('(') {
                    if let Some(col) = event_column(&take_expr(body, k + 1)) {
                        return Some(col);
                    }
                }
            }
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }
    None
}

/// Event-table column this assignment copies. Constructor ids (`new Token(event.params.token0)`)
/// use the constructor argument; a local that only aliases that argument is chased for `id`.
pub(crate) fn assignment_event_column(asg: &Assignment, func: &FunctionInfo) -> Option<String> {
    if let Some(col) = event_column(&asg.expr) {
        return Some(col);
    }
    if asg.field != "id" {
        return None;
    }
    let ident = local_root(&asg.expr)?;
    let arg = constructor_arg_for(&func.body, &ident)
        .or_else(|| load_arg_for(&func.body, &ident))
        .or_else(|| create_arg_for(&func.body, &ident))?;
    event_column(&arg)
}

/// The event/call handler that wrote this, or a handler that calls this helper. Block handlers
/// are never a table source.
pub(crate) fn event_handler_for<'a>(
    func: &'a FunctionInfo,
    mappings: &'a Mappings,
) -> Option<&'a FunctionInfo> {
    if func.kind == HandlerKind::Event || func.kind == HandlerKind::Call {
        return Some(func);
    }
    if func.kind == HandlerKind::Block {
        return None;
    }
    mappings.functions.values().find(|f| {
        (f.kind == HandlerKind::Event || f.kind == HandlerKind::Call)
            && f.calls.contains(&func.name)
    })
}

fn local_root(expr: &str) -> Option<String> {
    let e = strip_converters(expr);
    let e = e.strip_suffix(".id").unwrap_or(&e).trim();
    if e.is_empty() {
        return None;
    }
    if !e.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    if e.chars().next()?.is_ascii_digit() {
        return None;
    }
    Some(e.to_string())
}

fn constructor_arg_for(body: &str, var: &str) -> Option<String> {
    let mut i = 0;
    while i < body.len() {
        let hit = match_let_new(body, i).or_else(|| match_bare_new(body, i));
        if let Some((v, _ent, next)) = hit {
            if v == var {
                let mut k = next;
                skip_ws_str(body, &mut k);
                if body[k..].starts_with('(') {
                    return Some(collapse_ws(&take_expr(body, k + 1)));
                }
            }
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }
    None
}

fn load_arg_for(body: &str, var: &str) -> Option<String> {
    arg_after_match(body, var, match_let_load)
}

fn create_arg_for(body: &str, var: &str) -> Option<String> {
    arg_after_match(body, var, match_let_create)
}

type ArgumentMatcher = fn(&str, usize) -> Option<(String, String, usize)>;

fn arg_after_match(body: &str, var: &str, matcher: ArgumentMatcher) -> Option<String> {
    let mut i = 0;
    while i < body.len() {
        if let Some((v, _ent, next)) = matcher(body, i) {
            if v == var {
                let mut k = next;
                skip_ws_str(body, &mut k);
                if body[k..].starts_with('(') {
                    return Some(collapse_ws(&take_expr(body, k + 1)));
                }
            }
            i = next.max(i + 1);
            continue;
        }
        i += 1;
    }
    None
}

/// Map a mapping expression onto a nest column (`token0`, `address`, …). `None` if it is not
/// a field of the triggering event - emit then refuses to guess.
pub(crate) fn event_column(expr: &str) -> Option<String> {
    let e = strip_converters(expr);
    if let Some(rest) = e.strip_prefix("event.params.") {
        let mut k = 0;
        let name = take_ident_str(rest, &mut k)?;
        if name.is_empty() {
            return None;
        }
        return Some(nuthatch_decode::registry::snake_case(&name));
    }
    match e.as_str() {
        "event.address" => Some("address".into()),
        "event.block.timestamp" => Some("block_timestamp".into()),
        "event.block.number" => Some("block_number".into()),
        "event.transaction.hash" => Some("tx_hash".into()),
        "event.logIndex" => Some("log_index".into()),
        _ => None,
    }
}

fn sol_type_of_expr(expr: &str, func: &FunctionInfo) -> Option<&'static str> {
    let e = strip_converters(expr);
    if e.starts_with("event.params.") || e == "event.address" {
        return Some("address");
    }
    if func.param_names.iter().any(|p| p == &e) {
        return Some("address");
    }
    if e.starts_with("Address.") || e.starts_with("Address.from") {
        return Some("address");
    }
    None
}

fn literal_address(expr: &str) -> Option<String> {
    let e = strip_converters(expr);
    // Address.fromString('0x…') / Address.fromHexString("0x…")
    for prefix in ["Address.fromString(", "Address.fromHexString("] {
        if let Some(rest) = e.strip_prefix(prefix) {
            let rest = rest.trim_start_matches(['\'', '"']).trim_end_matches(')');
            let rest = rest.trim_end_matches(['\'', '"', ' ', ')']);
            if rest.starts_with("0x") && rest.len() == 42 {
                return Some(rest.to_ascii_lowercase());
            }
        }
    }
    if e.starts_with("0x") && e.len() == 42 {
        return Some(e.to_ascii_lowercase());
    }
    None
}

pub(crate) fn resolved_contract(expr: &str) -> ResolvedContract {
    if let Some(col) = event_column(expr) {
        return ResolvedContract::Column(col);
    }
    if let Some(addr) = literal_address(expr) {
        return ResolvedContract::Address(addr);
    }
    ResolvedContract::Unknown
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolvedContract {
    Column(String),
    Address(String),
    Unknown,
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
        (
            schema,
            Mappings {
                functions,
                handlers: Vec::new(),
            },
        )
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
    fn constructor_id_maps_to_the_event_column() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
}
type Pool @entity {
  id: ID!
  token0: Token!
}
"#;
        let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  token0.symbol = 'x'
  token0.save()
  let pool = new Pool(event.params.pool.toHex())
  pool.token0 = token0.id
  pool.save()
}
"#;
        let (_schema, mappings) = schema_and_mappings(schema, "src/mappings/core.ts", mapping);
        let func = mappings.functions.get("handlePoolCreated").unwrap();
        let token_id = func
            .assignments
            .iter()
            .find(|a| a.entity == "Token" && a.field == "id")
            .expect("constructor id");
        assert_eq!(
            assignment_event_column(token_id, func).as_deref(),
            Some("token0")
        );
        let pool_id = func
            .assignments
            .iter()
            .find(|a| a.entity == "Pool" && a.field == "id")
            .expect("pool constructor id");
        assert_eq!(
            assignment_event_column(pool_id, func).as_deref(),
            Some("pool")
        );
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

    #[test]
    fn schema_parse_rejects_empty() {
        assert!(parse_schema("enum Foo { A }").is_err());
    }

    #[test]
    fn mapping_call_signature_cites_the_try_line() {
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
        let calls = calls_from_mappings(
            &Report {
                source: "t".into(),
                fields: rows,
            },
            &mappings,
        );
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].signature, "symbol()");
        assert_eq!(calls[0].contract_arg, "event.params.token0");
        assert_eq!(calls[0].citation.file, "src/common/token.ts");
        assert_eq!(
            calls[0].citation.line, 4,
            "try_symbol is line 4 of the mapping"
        );
        assert_eq!(calls[0].handler, "handlePoolCreated");
    }

    #[test]
    fn helper_with_two_reads_emits_the_returned_method() {
        let schema = r#"
type Token @entity {
  id: ID!
  decimals: BigInt!
}
"#;
        let mapping = r#"
export function fetchTokenDecimals(tokenAddress: Address): BigInt {
  let contract = ERC20.bind(tokenAddress)
  let _sym = contract.try_symbol()
  return contract.try_decimals().value
}

export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  token0.decimals = fetchTokenDecimals(event.params.token0)
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/common/token.ts", mapping);
        let rows = classify(&schema, &mappings);
        let calls = calls_from_mappings(
            &Report {
                source: "t".into(),
                fields: rows,
            },
            &mappings,
        );
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(
            calls[0].signature, "decimals()",
            "the returned try_decimals must win over the earlier try_symbol: {calls:?}"
        );
        assert_eq!(calls[0].contract_arg, "event.params.token0");
        assert_eq!(
            calls[0].citation.line, 5,
            "must cite try_decimals, not try_symbol, got {}",
            calls[0].citation.line
        );
    }

    #[test]
    fn two_inline_reads_attribute_each_field_to_its_try() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
  decimals: BigInt!
}
"#;
        let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let token0 = new Token(event.params.token0.toHex())
  let contract = ERC20.bind(event.params.token0)
  token0.symbol = contract.try_symbol().value
  token0.decimals = contract.try_decimals().value
  token0.save()
}
"#;
        let (schema, mappings) = schema_and_mappings(schema, "src/mappings/core.ts", mapping);
        let rows = classify(&schema, &mappings);
        let calls = calls_from_mappings(
            &Report {
                source: "t".into(),
                fields: rows,
            },
            &mappings,
        );
        let sig = |field: &str| {
            calls
                .iter()
                .find(|c| c.field == field)
                .map(|c| c.signature.as_str())
                .unwrap_or("missing")
        };
        assert_eq!(sig("symbol"), "symbol()", "{calls:?}");
        assert_eq!(sig("decimals"), "decimals()", "{calls:?}");
    }

    #[test]
    fn mapping_call_vanishes_when_the_try_is_dropped() {
        let schema = r#"
type Token @entity {
  id: ID!
  symbol: String!
}
"#;
        let mapping = r#"
export function fetchTokenSymbol(tokenAddress: Address): string {
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
        assert_eq!(class_of(&rows, "Token", "symbol"), Class::Exact);
        let calls = calls_from_mappings(
            &Report {
                source: "t".into(),
                fields: rows,
            },
            &mappings,
        );
        assert!(
            calls.is_empty(),
            "dropping try_symbol must not invent a [[calls]]: {calls:?}"
        );
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
