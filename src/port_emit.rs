//! RFC-0044 S2: emit a nest on top of `nuthatch init --from-subgraph`.
//!
//! Consumes the importer's nest plus the S1 report. Does not reimplement the importer.
//! `[[calls]]` come from mapping contract reads; Exact fields become `views/*.sql`;
//! fixed-point and unreachable fields are named in the README and not emitted.

use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::calls::CallDecl;
use crate::cli::PortEmitArgs;
use crate::config::Config;
use crate::port_report::{
    assignment_event_column, event_column, event_handler_for, mapping_calls, render_report,
    resolved_contract, Citation, Class, MappingCall, Report, ResolvedContract,
};
use crate::registry::snake_case;
use crate::subgraph_import::to_alias;

/// One `[[calls]]` stanza plus the mapping line it was derived from.
#[derive(Debug, Clone)]
pub struct EmittedCall {
    pub decl: CallDecl,
    pub citation: Citation,
    pub entity: String,
    pub field: String,
}

#[derive(Debug, Clone)]
pub struct EmitResult {
    pub calls: Vec<EmittedCall>,
    pub views: Vec<EmittedView>,
    pub report: String,
    /// Reads that could not be pinned to an ABI signature, with the reason. Named so an author can
    /// add the stanza by hand; never guessed, and never a reason to abandon the rest of the port.
    pub skipped_calls: Vec<SkippedCall>,
    /// Exact fields that reached no column. Same contract as `skipped_calls`: named, never silent.
    pub skipped_fields: Vec<SkippedField>,
}

#[derive(Debug, Clone)]
pub struct SkippedCall {
    pub signature: String,
    pub citation: Citation,
    pub why: String,
}

/// An Exact field the emitter could not render into its view, named rather than dropped (#1248).
///
/// The report promises that a field it calls Exact matches byte-for-byte, so a field that reaches
/// no column must be visible to the porter. Silence here used to take two forms and both were
/// worse than this: the field vanished from the SQL while `exact_fields` still listed it, or the
/// mapping's operation was discarded and the bare event column answered in its place.
#[derive(Debug, Clone)]
pub struct SkippedField {
    pub entity: String,
    pub field: String,
    pub citation: Citation,
    pub why: String,
}

impl SkippedField {
    pub fn name(&self) -> String {
        format!("{}.{}", self.entity, self.field)
    }
}

#[derive(Debug, Clone)]
pub struct EmittedView {
    pub entity: String,
    pub file: String,
    pub sql: String,
    pub exact_fields: Vec<String>,
}

/// Hidden `nuthatch port-emit --dir <subgraph> --out <nest>` entry point.
pub fn run(args: PortEmitArgs) -> Result<()> {
    let subgraph = Path::new(&args.dir);
    let nest = Path::new(&args.out);
    let result = emit(subgraph, nest)?;
    println!(
        "✓ emitted {} [[calls]] and {} view(s) into {}",
        result.calls.len(),
        result.views.len(),
        nest.display()
    );
    for c in &result.calls {
        println!(
            "  [[calls]] {}  {}  ← {}:{}",
            c.decl.name,
            c.decl.signature.as_deref().unwrap_or("?"),
            c.citation.file,
            c.citation.line
        );
    }
    // Loudly, and by name. A skipped read the author never hears about is a silent hole in the
    // port, which is worse than the abort this replaced.
    for s in &result.skipped_calls {
        println!(
            "  ! not emitted: {} at {} - {}",
            s.signature,
            s.citation.display(),
            s.why
        );
    }
    // Same contract for fields. A field the report calls Exact that reached no column is the one
    // thing a porter must not learn from a gateway diff three days later (#1248).
    for s in &result.skipped_fields {
        println!(
            "  ! exact but not in a view: {} at {} - {}",
            s.name(),
            s.citation.display(),
            s.why
        );
    }
    Ok(())
}

/// Overlay `[[calls]]`, Exact views, checks and a README onto an already-imported nest.
pub fn emit(subgraph: &Path, nest: &Path) -> Result<EmitResult> {
    if !nest.join("nuthatch.toml").exists() {
        bail!(
            "no nuthatch.toml in {} - run `nuthatch init --from-subgraph` first, then emit onto that nest",
            nest.display()
        );
    }
    let report = crate::port_report::classify_dir(subgraph)?;
    let report_text = render_report(&report);
    let mappings = crate::port_report::load_mappings(subgraph)?;
    let calls_raw = mapping_calls(subgraph)?;

    let mut config =
        Config::load(nest).with_context(|| format!("load nest at {}", nest.display()))?;

    let (emitted_calls, skipped_calls) = calls_to_decls(&calls_raw, &mappings, &config)?;
    config.calls = emitted_calls.iter().map(|c| c.decl.clone()).collect();
    config.save(nest)?;
    crate::project::regen(crate::cli::SchemaArgs {
        dir: nest.display().to_string(),
    })?;

    // The real decoded columns, so a mapped column name is *resolved* rather than guessed (#1250).
    // Built after `regen` so it reflects the `[[calls]]` just written.
    let schema = crate::registry::from_nest(nest, &config)
        .with_context(|| format!("build the decode registry for {}", nest.display()))?
        .schema();

    let (views, skipped_fields) = write_exact_views(nest, &report, &mappings, &config, &schema)?;
    write_checks(nest, &views)?;
    std::fs::write(nest.join("README.md"), &report_text)
        .with_context(|| format!("write {}/README.md", nest.display()))?;

    Ok(EmitResult {
        calls: emitted_calls,
        views,
        report: report_text,
        skipped_calls,
        skipped_fields,
    })
}

/// Emittable calls, and the reads that were skipped with the reason why.
///
/// A parameterized read is still **refused, never guessed**: the mapping expression carries no ABI
/// parameter types, so any signature we wrote would be invented. What changed is the blast radius.
/// Aborting the whole overlay meant one such read cost the author their views, checks and README
/// too, and it was the only unpinnable read treated that way - an unresolvable handler, contract or
/// argument below is skipped and the port carries on. A partial port is the contract; this is now
/// the same skip as the others, reported by name so the stanza can be written by hand.
fn calls_to_decls(
    raw: &[MappingCall],
    mappings: &crate::port_report::Mappings,
    config: &Config,
) -> Result<(Vec<EmittedCall>, Vec<SkippedCall>)> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    let mut used_names: BTreeSet<String> = BTreeSet::new();
    for call in raw {
        if !call.args.is_empty() {
            skipped.push(SkippedCall {
                signature: call.signature.clone(),
                citation: call.citation.clone(),
                why: "parameterized call: the mapping expression does not carry ABI parameter \
                      types, so refusing to guess a Solidity signature. Add this [[calls]] stanza \
                      by hand with the ABI signature"
                    .to_string(),
            });
            continue;
        }
        let Some(on) = table_for_handler(&call.handler, mappings, config) else {
            // No event table for this handler: inventing `on` would be a guess.
            continue;
        };
        let (contract, contract_column) = match resolved_contract(&call.contract_arg) {
            ResolvedContract::Column(col) => (String::new(), Some(format!("{{{col}}}"))),
            ResolvedContract::Address(addr) => (addr, None),
            ResolvedContract::Unknown => continue,
        };
        let args: Vec<String> = call
            .args
            .iter()
            .map(|a| match event_column(a) {
                Some(col) => format!("{{{col}}}"),
                None => a.clone(),
            })
            .collect();
        if call.args.iter().any(|a| {
            event_column(a).is_none()
                && resolved_contract(a) == ResolvedContract::Unknown
                && !a.is_empty()
        }) {
            // An argument we cannot pin to a column or a literal is a guess.
            continue;
        }
        let name = unique_call_name(call, contract_column.as_deref(), &mut used_names);
        let decl = CallDecl {
            name,
            contract,
            calldata: String::new(),
            on: Some(on),
            signature: Some(call.signature.clone()),
            args,
            contract_column,
            every: 1000,
            start: None,
        };
        decl.validate().with_context(|| {
            format!(
                "derived [[calls]] `{}` from {}:{}",
                decl.name, call.citation.file, call.citation.line
            )
        })?;
        out.push(EmittedCall {
            decl,
            citation: call.citation.clone(),
            entity: call.entity.clone(),
            field: call.field.clone(),
        });
    }
    Ok((out, skipped))
}

fn unique_call_name(
    call: &MappingCall,
    contract_column: Option<&str>,
    used: &mut BTreeSet<String>,
) -> String {
    let entity = to_alias(&call.entity);
    let field = to_alias(&call.field);
    let mut base = format!("{entity}_{field}");
    if let Some(col) = contract_column {
        let col = col.trim_matches(['{', '}']);
        if !col.is_empty() && col != field {
            base = format!("{base}_{col}");
        }
    }
    let mut name = base.clone();
    let mut n = 2u32;
    while !used.insert(name.clone()) {
        name = format!("{base}_{n}");
        n += 1;
    }
    name
}

fn table_for_handler(
    handler: &str,
    mappings: &crate::port_report::Mappings,
    config: &Config,
) -> Option<String> {
    let binding = mappings.handlers.iter().find(|h| h.handler == handler)?;
    let alias = nest_alias(&binding.source, config);
    let event = snake_case(&binding.event);
    Some(format!("{alias}__{event}"))
}

fn nest_alias(source: &str, config: &Config) -> String {
    let want = to_alias(source);
    if let Some(c) = config.contracts.iter().find(|c| c.alias == want) {
        return c.alias.clone();
    }
    if let Some(t) = config.templates.iter().find(|t| t.name == want) {
        return t.name.clone();
    }
    if let Some(c) = config.contracts.iter().find(|c| to_alias(&c.alias) == want) {
        return c.alias.clone();
    }
    want
}

fn write_exact_views(
    nest: &Path,
    report: &Report,
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> Result<(Vec<EmittedView>, Vec<SkippedField>)> {
    let views_dir = nest.join("views");
    std::fs::create_dir_all(&views_dir)
        .with_context(|| format!("create {}", views_dir.display()))?;
    // Replace the init starter if it is still the commented no-op.
    let starter = views_dir.join("10-example.sql");
    if starter.exists() {
        if let Ok(text) = std::fs::read_to_string(&starter) {
            if text.contains("Uncomment to enable") {
                let _ = std::fs::remove_file(&starter);
            }
        }
    }

    let exact_by_entity: BTreeMap<String, Vec<&crate::port_report::FieldRow>> = {
        let mut m: BTreeMap<String, Vec<&crate::port_report::FieldRow>> = BTreeMap::new();
        for f in &report.fields {
            if f.class == Class::Exact && f.entity != "_Schema_" {
                m.entry(f.entity.clone()).or_default().push(f);
            }
        }
        m
    };

    let mut emitted = Vec::new();
    let mut skipped = Vec::new();
    for (entity, fields) in &exact_by_entity {
        let view = view_for_entity(entity, fields, mappings, config, schema);
        let file = format!("20-{}.sql", to_alias(entity));
        std::fs::write(views_dir.join(&file), &view.sql)
            .with_context(|| format!("write views/{file}"))?;
        emitted.push(EmittedView {
            entity: entity.clone(),
            file,
            sql: view.sql,
            exact_fields: view.exact_fields,
        });
        skipped.extend(view.skipped);
    }
    Ok((emitted, skipped))
}

struct ViewDraft {
    sql: String,
    exact_fields: Vec<String>,
    skipped: Vec<SkippedField>,
}

/// Why an Exact field reached no decoded column, phrased for the porter rather than the compiler.
///
/// The report's `reason` already quotes the right-hand side, which is the only evidence that
/// matters here, so this classifies that text rather than re-parsing the mapping. Three shapes
/// account for every case seen so far and the fallback is honest about not knowing.
fn skip_reason(report_reason: &str) -> String {
    let r = report_reason.replace('\n', " ");
    if r.contains("event.params.") {
        format!(
            "the mapping applies an operation this emitter cannot render, so no column answers it ({r}). \
             Add the field to a view by hand, or see #1248"
        )
    } else if r.contains(".plus(") || r.contains(".minus(") {
        format!(
            "accumulates its own prior value ({r}); a running total's proper home is an incremental \
             entity rather than a view, see #1214"
        )
    } else {
        format!("no decoded column corresponds to it ({r})")
    }
}

fn view_for_entity(
    entity: &str,
    fields: &[&crate::port_report::FieldRow],
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> ViewDraft {
    let view_name = to_alias(entity);
    let mut comments = Vec::new();
    // field → table → column. One field may be written from several triggering tables.
    let mut selects: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut exact_fields = Vec::new();
    let mut skipped = Vec::new();

    for f in fields {
        // **Claim the field only once it has a column.** `exact_fields` used to be pushed before
        // the mapping was attempted, so a field that reached no table was still advertised as
        // being in this view while the SQL never mentioned it (#1248).
        let tables = map_exact_field_tables(f, mappings, config, schema);
        if tables.is_empty() {
            comments.push(format!(
                "-- `{}.{}` exact but NOT IN THIS VIEW: {}:{} - {}",
                f.entity,
                f.field,
                f.citation.file,
                f.citation.line,
                skip_reason(&f.reason)
            ));
            skipped.push(SkippedField {
                entity: f.entity.clone(),
                field: f.field.clone(),
                citation: f.citation.clone(),
                why: skip_reason(&f.reason),
            });
            continue;
        }
        exact_fields.push(f.field.clone());
        comments.push(format!(
            "-- `{}.{}` exact: {}:{} - {}",
            f.entity,
            f.field,
            f.citation.file,
            f.citation.line,
            f.reason.replace('\n', " ")
        ));
        for (table, col) in tables {
            selects
                .entry(f.field.clone())
                .or_default()
                .entry(table)
                .or_insert(col);
        }
    }
    fill_id_columns(entity, &mut selects, mappings, config);

    let mut sql = String::new();
    sql.push_str(&format!(
        "-- Exact fields of `{entity}`. Call-derived, fixed-point and unreachable fields are not here; see README.md.\n"
    ));
    for c in &comments {
        sql.push_str(c);
        sql.push('\n');
    }
    sql.push('\n');

    if selects.is_empty() {
        // Still a valid view so `nuthatch check` has something that binds, and the field
        // names appear in the file (the comments above).
        sql.push_str(&format!(
            "CREATE VIEW \"{view_name}\" AS SELECT 1 AS port_placeholder;\n"
        ));
        return ViewDraft {
            sql,
            exact_fields,
            skipped,
        };
    }

    sql.push_str(&format!(
        "CREATE VIEW \"{view_name}\" AS\n{}\n",
        exact_select_sql(&selects)
    ));
    ViewDraft {
        sql,
        exact_fields,
        skipped,
    }
}

/// Latest mapped value per entity id. Several tables overlay with UNION ALL (not a JOIN);
/// `last() FILTER` skips a later arm's NULL so it cannot wipe an earlier write.
/// Prefix for the per-field presence marker in the inner union. `__` is reserved in GraphQL
/// (introspection), so no entity field can collide with it.
const PRESENT: &str = "__present__";

/// The exact-field view: one arm per triggering table, folded to the last value per id.
///
/// **The fold filters on presence, not on NULL.** An arm for a table that does not carry a field
/// has to emit something for it, and that something is NULL; folding with `FILTER (WHERE field IS
/// NOT NULL)` stopped that structural NULL erasing a real value from another arm, but it could not
/// tell it from a genuine one. A nullable entity field cleared by a later event kept its old value
/// for good, and the view called that exact. The marker says which arm actually carried the field,
/// so a real NULL wins the fold and a structural one still does not.
fn exact_select_sql(selects: &BTreeMap<String, BTreeMap<String, String>>) -> String {
    let mut tables = BTreeSet::new();
    for by_table in selects.values() {
        tables.extend(by_table.keys().cloned());
    }
    let fields: Vec<&str> = selects.keys().map(String::as_str).collect();
    let fold = selects.contains_key("id");
    let arms: Vec<String> = tables
        .iter()
        .map(|table| {
            let mut cols: Vec<String> = fields
                .iter()
                .flat_map(
                    |field| match selects.get(*field).and_then(|m| m.get(table)) {
                        Some(col) => [
                            format!("  \"{col}\" AS \"{field}\""),
                            format!("  TRUE AS \"{PRESENT}{field}\""),
                        ],
                        None => [
                            format!("  NULL AS \"{field}\""),
                            format!("  FALSE AS \"{PRESENT}{field}\""),
                        ],
                    },
                )
                .collect();
            if fold {
                cols.push("  \"block_number\"".into());
                cols.push("  \"log_index\"".into());
            }
            format!("SELECT\n{}\nFROM \"{table}\"", cols.join(",\n"))
        })
        .collect();
    let inner = arms.join("\nUNION ALL\n");
    if !fold {
        return format!("{inner};");
    }
    let outer: Vec<String> = fields
        .iter()
        .map(|field| {
            if *field == "id" {
                "  \"id\"".into()
            } else {
                format!(
                    "  last(\"{field}\" ORDER BY \"block_number\", \"log_index\") FILTER (WHERE \"{PRESENT}{field}\") AS \"{field}\""
                )
            }
        })
        .collect();
    format!(
        "SELECT\n{}\nFROM (\n{}\n)\nGROUP BY \"id\";",
        outer.join(",\n"),
        inner
    )
}

/// The decoded column this name refers to, or `None` if the table has no such column.
///
/// **Resolved, never guessed** (#1250). A column is the ABI parameter name verbatim, except that an
/// indexed dynamic type lands as `{name}_hash` because the log carries `keccak(value)` rather than
/// the value (RFC-0001). Those are the only two shapes, and both are checked against the real
/// schema, so a name that matches neither yields nothing and the field is reported as skipped rather
/// than written into a view that cannot bind. There is deliberately no fuzzy or case-insensitive
/// fallback: a near-miss resolved to the wrong column is the failure mode this whole change exists
/// to remove.
fn resolve_column(
    table: &str,
    name: &str,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> Option<String> {
    let cols = &schema.iter().find(|t| t.table == table)?.columns;
    let has = |c: &str| cols.iter().any(|col| col.name == c);
    if has(name) {
        return Some(name.to_string());
    }
    let hashed = format!("{name}_hash");
    if has(&hashed) {
        return Some(hashed);
    }
    None
}

fn map_exact_field_tables(
    field: &crate::port_report::FieldRow,
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for func in mappings.functions.values() {
        if func.kind == crate::port_report::HandlerKind::Block {
            continue;
        }
        for asg in &func.assignments {
            if asg.entity != field.entity || asg.field != field.field {
                continue;
            }
            let Some(col) = assignment_event_column(asg, func) else {
                continue;
            };
            let Some(handler) = event_handler_for(func, mappings) else {
                continue;
            };
            let Some(table) = table_for_handler(&handler.name, mappings, config) else {
                continue;
            };
            let Some(col) = resolve_column(&table, &col, schema) else {
                continue;
            };
            out.entry(table).or_insert(col);
        }
    }
    out
}

/// A table that writes other Exact fields still needs an id column so UNION ALL can fold
/// on the entity, not on a NULL-padded arm. `Token.load(event.params.token)` is that column.
fn fill_id_columns(
    entity: &str,
    selects: &mut BTreeMap<String, BTreeMap<String, String>>,
    mappings: &crate::port_report::Mappings,
    config: &Config,
) {
    if !selects.contains_key("id") {
        return;
    }
    let tables: Vec<String> = selects
        .values()
        .flat_map(|m| m.keys().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for table in tables {
        if selects.get("id").is_some_and(|m| m.contains_key(&table)) {
            continue;
        }
        if let Some(col) = id_column_for_table(entity, &table, mappings, config) {
            selects.entry("id".into()).or_default().insert(table, col);
        }
    }
}

fn id_column_for_table(
    entity: &str,
    table: &str,
    mappings: &crate::port_report::Mappings,
    config: &Config,
) -> Option<String> {
    for func in mappings.functions.values() {
        if func.kind == crate::port_report::HandlerKind::Block {
            continue;
        }
        let Some(handler) = event_handler_for(func, mappings) else {
            continue;
        };
        let Some(t) = table_for_handler(&handler.name, mappings, config) else {
            continue;
        };
        if t != table {
            continue;
        }
        if let Some(col) = crate::port_report::entity_id_event_column(entity, func) {
            return Some(col);
        }
    }
    None
}

fn write_checks(nest: &Path, views: &[EmittedView]) -> Result<()> {
    let dir = nest.join("checks");
    std::fs::create_dir_all(dir.join("expected"))
        .with_context(|| format!("create {}/checks/expected", nest.display()))?;
    // **Structural, and deliberately not a row count.** `port-emit` is an overlay onto a nest that
    // may already hold data, so an expectation of zero rows is true only until the nest indexes its
    // first matching event and false for the whole rest of the nest's life. Each view is bound and
    // projected under `LIMIT 0`: the query fails if a view is missing or its columns do not
    // type-check, and its answer does not move as the nest fills. The aggregate is what guarantees
    // one row per view - `SELECT true FROM (.. LIMIT 0)` would return none.
    //
    // **The projection names the promised columns.** `SELECT *` binds a view whatever it contains,
    // so it could not see a field that `exact_fields` advertised and the SQL never emitted - the
    // defect in #1248, which passed every generated check. Naming each column makes DuckDB the
    // independent party: the two lists were computed in the same loop and still diverged, so the
    // engine refusing to bind an absent column is worth more than either list agreeing with itself.
    let mut labels: Vec<(String, String, Vec<String>)> = views
        .iter()
        .map(|v| {
            (
                v.entity.clone(),
                to_alias(&v.entity),
                v.exact_fields.clone(),
            )
        })
        .collect();
    labels.sort();
    let sql = if labels.is_empty() {
        "SELECT 1 AS port_ok;\n".to_string()
    } else {
        let mut s = String::from(
            "-- Structural: every exact view binds and projects. Not a row count - this stays true \
             once the nest holds data.\n",
        );
        for (i, (entity, alias, fields)) in labels.iter().enumerate() {
            let head = if i == 0 {
                format!("SELECT '{entity}' AS view, count(*) >= 0 AS binds")
            } else {
                format!("UNION ALL SELECT '{entity}', count(*) >= 0")
            };
            // An entity whose fields were all skipped emits the placeholder view, which has no
            // promised column to name; `*` is then the only honest projection.
            let projection = if fields.is_empty() {
                "*".to_string()
            } else {
                fields
                    .iter()
                    .map(|f| format!("\"{f}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            s.push_str(&format!(
                "{head} FROM (SELECT {projection} FROM \"{alias}\" LIMIT 0)\n"
            ));
        }
        s.push_str("ORDER BY 1;\n");
        s
    };
    std::fs::write(dir.join("port_views.sql"), sql).context("write checks/port_views.sql")?;
    // `nuthatch check` compares row for row, so the fixture is the same constant answer the query
    // gives on an empty nest and on a fully backfilled one. Nothing here needs re-recording.
    let expected = if labels.is_empty() {
        serde_json::json!([{ "port_ok": 1 }])
    } else {
        let rows: Vec<serde_json::Value> = labels
            .iter()
            .map(|(entity, _, _)| serde_json::json!({ "view": entity, "binds": true }))
            .collect();
        serde_json::Value::Array(rows)
    };
    std::fs::write(
        dir.join("expected/port_views.json"),
        serde_json::to_string_pretty(&expected)?,
    )
    .context("write checks/expected/port_views.json")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(entity: &str) -> EmittedView {
        EmittedView {
            entity: entity.into(),
            file: format!("{}.sql", to_alias(entity)),
            sql: String::new(),
            exact_fields: vec!["id".into()],
        }
    }

    /// Jules on #1244: the emitted check used to expect `count(*) = 0` per view, so it passed on a
    /// fresh nest and failed for good the moment the nest indexed one matching event. `port-emit`
    /// is an overlay onto a nest that may already hold data, so the check has to answer the same
    /// thing either way. Run against real DuckDB, empty and populated.
    #[test]
    fn the_emitted_check_answers_the_same_on_a_populated_nest() {
        let dir = tempfile::tempdir().unwrap();
        let views = [view("Token"), view("Pool")];
        write_checks(dir.path(), &views).unwrap();

        let sql = std::fs::read_to_string(dir.path().join("checks/port_views.sql")).unwrap();
        let expected: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("checks/expected/port_views.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            expected,
            serde_json::json!([
                { "view": "Pool", "binds": true },
                { "view": "Token", "binds": true },
            ]),
            "the fixture must be the constant answer, not a row count: {expected}"
        );

        let conn = duckdb::Connection::open_in_memory().unwrap();
        for alias in ["token", "pool"] {
            conn.execute_batch(&format!(
                "CREATE TABLE {alias}_rows (id VARCHAR); \
                 CREATE VIEW \"{alias}\" AS SELECT * FROM {alias}_rows;"
            ))
            .unwrap();
        }

        let answer = |conn: &duckdb::Connection| -> Vec<(String, bool)> {
            let mut stmt = conn.prepare(sql.trim_end().trim_end_matches(';')).unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };

        let empty = answer(&conn);
        assert_eq!(
            empty,
            vec![("Pool".to_string(), true), ("Token".to_string(), true)],
            "the check must bind on an empty nest"
        );

        conn.execute_batch(
            "INSERT INTO token_rows VALUES ('0xaa'), ('0xbb'); \
             INSERT INTO pool_rows VALUES ('0xcc');",
        )
        .unwrap();
        assert_eq!(
            answer(&conn),
            empty,
            "the check must give the same answer once the nest holds data"
        );
    }

    /// A view the nest does not have must still fail the check - the point of it binding.
    #[test]
    fn the_emitted_check_fails_when_a_view_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        write_checks(dir.path(), &[view("Token")]).unwrap();
        let sql = std::fs::read_to_string(dir.path().join("checks/port_views.sql")).unwrap();
        let conn = duckdb::Connection::open_in_memory().unwrap();
        assert!(
            conn.prepare(sql.trim_end().trim_end_matches(';')).is_err(),
            "a check that binds nothing is not a check:\n{sql}"
        );
    }

    /// Jules on #1244. The fold used to filter on `field IS NOT NULL`, which cannot tell an arm
    /// that never carried the field from an event that genuinely cleared it. A nullable field set
    /// and then cleared kept its old value for good, and the view called that exact. Run against
    /// real DuckDB, because the whole claim is about what the SQL returns.
    #[test]
    fn a_later_explicit_null_clears_the_field_and_a_missing_column_does_not() {
        let mut selects = BTreeMap::new();
        put(&mut selects, "id", "factory__pool_created", "token0");
        put(&mut selects, "id", "factory__token_updated", "token");
        put(&mut selects, "symbol", "factory__pool_created", "sym");
        put(&mut selects, "name", "factory__token_updated", "name");
        let sql = exact_select_sql(&selects);

        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE factory__pool_created (token0 VARCHAR, sym VARCHAR, \
               block_number BIGINT, log_index BIGINT);
             CREATE TABLE factory__token_updated (token VARCHAR, name VARCHAR, \
               block_number BIGINT, log_index BIGINT);
             INSERT INTO factory__pool_created VALUES ('0xaa', 'AAA', 1, 0);
             INSERT INTO factory__token_updated VALUES ('0xaa', 'old name', 2, 0);",
        )
        .unwrap();

        let read = |conn: &duckdb::Connection| -> (Option<String>, Option<String>) {
            let mut stmt = conn.prepare(sql.trim_end().trim_end_matches(';')).unwrap();
            let rows: Vec<(Option<String>, Option<String>)> = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, Option<String>>("name")?,
                        r.get::<_, Option<String>>("symbol")?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(rows.len(), 1, "one id, one row: {rows:?}");
            rows[0].clone()
        };

        let (name, symbol) = read(&conn);
        assert_eq!(name.as_deref(), Some("old name"));
        assert_eq!(
            symbol.as_deref(),
            Some("AAA"),
            "the token_updated arm carries no symbol, and its structural NULL must not erase this"
        );

        // A later event clears the field. This is the one the old filter could not see.
        conn.execute_batch("INSERT INTO factory__token_updated VALUES ('0xaa', NULL, 3, 0);")
            .unwrap();
        let (name, symbol) = read(&conn);
        assert_eq!(
            name, None,
            "an explicit NULL at a later block clears the field; it returned {name:?}"
        );
        assert_eq!(
            symbol.as_deref(),
            Some("AAA"),
            "and the other arm's value is still not erased"
        );
    }

    #[test]
    fn unique_call_name_distinguishes_columns() {
        let mut used = BTreeSet::new();
        let a = MappingCall {
            entity: "Token".into(),
            field: "symbol".into(),
            handler: "handlePoolCreated".into(),
            signature: "symbol()".into(),
            contract_arg: "event.params.token0".into(),
            args: vec![],
            citation: Citation {
                file: "src/token.ts".into(),
                line: 6,
            },
        };
        let n0 = unique_call_name(&a, Some("{token0}"), &mut used);
        let n1 = unique_call_name(&a, Some("{token1}"), &mut used);
        assert_eq!(n0, "token_symbol_token0");
        assert_eq!(n1, "token_symbol_token1");
        assert_ne!(n0, n1);
    }

    fn put(
        selects: &mut BTreeMap<String, BTreeMap<String, String>>,
        field: &str,
        table: &str,
        col: &str,
    ) {
        selects
            .entry(field.into())
            .or_default()
            .insert(table.into(), col.into());
    }

    #[test]
    fn exact_select_keeps_fields_from_every_table() {
        let mut selects = BTreeMap::new();
        put(&mut selects, "id", "factory__pool_created", "token0");
        put(&mut selects, "id", "factory__token_updated", "token");
        put(&mut selects, "symbol", "factory__pool_created", "token0");
        put(&mut selects, "name", "factory__token_updated", "name");
        let sql = exact_select_sql(&selects);
        assert!(sql.contains("UNION ALL"), "{sql}");
        assert!(sql.contains("GROUP BY \"id\""), "{sql}");
        assert!(
            sql.contains("last(\"symbol\" ORDER BY \"block_number\", \"log_index\")")
                && sql.contains("FILTER (WHERE \"__present__symbol\")"),
            "{sql}"
        );
        assert!(
            sql.contains("last(\"name\" ORDER BY \"block_number\", \"log_index\")")
                && sql.contains("FILTER (WHERE \"__present__name\")"),
            "{sql}"
        );
        assert!(sql.contains("\"token0\" AS \"symbol\""), "{sql}");
        assert!(sql.contains("\"name\" AS \"name\""), "{sql}");
        assert!(sql.contains("NULL AS \"symbol\""), "{sql}");
        assert!(sql.contains("NULL AS \"name\""), "{sql}");
        assert!(sql.contains("FROM \"factory__pool_created\""), "{sql}");
        assert!(sql.contains("FROM \"factory__token_updated\""), "{sql}");
        assert!(!sql.to_ascii_lowercase().contains(" join "), "{sql}");
    }

    #[test]
    fn exact_select_stays_one_table_when_all_columns_share_it() {
        let mut selects = BTreeMap::new();
        put(&mut selects, "id", "factory__pool_created", "token0");
        put(&mut selects, "symbol", "factory__pool_created", "token0");
        let sql = exact_select_sql(&selects);
        assert!(!sql.contains("UNION ALL"), "{sql}");
        assert!(!sql.contains("NULL AS"), "{sql}");
        assert!(sql.contains("GROUP BY \"id\""), "{sql}");
        assert!(sql.contains("last(\"symbol\""), "{sql}");
        assert!(sql.contains("\"token0\" AS \"id\""), "{sql}");
        assert!(sql.contains("\"token0\" AS \"symbol\""), "{sql}");
        assert!(sql.contains("FROM \"factory__pool_created\""), "{sql}");
    }
}
