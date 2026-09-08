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

    let emitted_calls = calls_to_decls(&calls_raw, &mappings, &config)?;
    config.calls = emitted_calls.iter().map(|c| c.decl.clone()).collect();
    config.save(nest)?;
    crate::project::regen(crate::cli::SchemaArgs {
        dir: nest.display().to_string(),
    })?;

    let views = write_exact_views(nest, &report, &mappings, &config)?;
    write_checks(nest, &views)?;
    std::fs::write(nest.join("README.md"), &report_text)
        .with_context(|| format!("write {}/README.md", nest.display()))?;

    Ok(EmitResult {
        calls: emitted_calls,
        views,
        report: report_text,
    })
}

fn calls_to_decls(
    raw: &[MappingCall],
    mappings: &crate::port_report::Mappings,
    config: &Config,
) -> Result<Vec<EmittedCall>> {
    let mut out = Vec::new();
    let mut used_names: BTreeSet<String> = BTreeSet::new();
    for call in raw {
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
    Ok(out)
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
) -> Result<Vec<EmittedView>> {
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
    for (entity, fields) in &exact_by_entity {
        let view = view_for_entity(entity, fields, mappings, config);
        let file = format!("20-{}.sql", to_alias(entity));
        std::fs::write(views_dir.join(&file), &view.sql)
            .with_context(|| format!("write views/{file}"))?;
        emitted.push(EmittedView {
            entity: entity.clone(),
            file,
            sql: view.sql,
            exact_fields: view.exact_fields,
        });
    }
    Ok(emitted)
}

struct ViewDraft {
    sql: String,
    exact_fields: Vec<String>,
}

fn view_for_entity(
    entity: &str,
    fields: &[&crate::port_report::FieldRow],
    mappings: &crate::port_report::Mappings,
    config: &Config,
) -> ViewDraft {
    let view_name = to_alias(entity);
    let mut comments = Vec::new();
    let mut selects: BTreeMap<String, (String, String)> = BTreeMap::new();
    // field → (table, column)
    let mut exact_fields = Vec::new();

    for f in fields {
        exact_fields.push(f.field.clone());
        comments.push(format!(
            "-- `{}.{}` exact: {}:{} — {}",
            f.entity,
            f.field,
            f.citation.file,
            f.citation.line,
            f.reason.replace('\n', " ")
        ));
        if let Some((table, col)) = map_exact_field(f, mappings, config) {
            selects.entry(f.field.clone()).or_insert((table, col));
        }
    }

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
        return ViewDraft { sql, exact_fields };
    }

    // One table: the one that supplies the most mapped columns. Remaining columns from other
    // tables stay as comments rather than a guessed join.
    let mut table_counts: BTreeMap<String, usize> = BTreeMap::new();
    for (table, _) in selects.values() {
        *table_counts.entry(table.clone()).or_default() += 1;
    }
    let primary = table_counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(t, _)| t)
        .unwrap();

    let mut cols = Vec::new();
    for (field, (table, col)) in &selects {
        if table == &primary {
            cols.push(format!("  \"{col}\" AS \"{field}\""));
        }
    }
    sql.push_str(&format!(
        "CREATE VIEW \"{view_name}\" AS\nSELECT\n{}\nFROM \"{primary}\";\n",
        cols.join(",\n")
    ));
    ViewDraft { sql, exact_fields }
}

fn map_exact_field(
    field: &crate::port_report::FieldRow,
    mappings: &crate::port_report::Mappings,
    config: &Config,
) -> Option<(String, String)> {
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
            return Some((table, col));
        }
    }
    None
}

fn write_checks(nest: &Path, views: &[EmittedView]) -> Result<()> {
    let dir = nest.join("checks");
    std::fs::create_dir_all(dir.join("expected"))
        .with_context(|| format!("create {}/checks/expected", nest.display()))?;
    let sql = if views.is_empty() {
        "SELECT 1 AS port_ok;\n".to_string()
    } else {
        let mut s = String::from(
            "-- Structural: the exact views bind. Expected is a fresh nest (zero rows) until a range is sealed.\n",
        );
        for (i, v) in views.iter().enumerate() {
            let name = to_alias(&v.entity);
            if i == 0 {
                s.push_str(&format!("SELECT count(*) AS n FROM \"{name}\"\n"));
            } else {
                s.push_str(&format!("UNION ALL SELECT count(*) FROM \"{name}\"\n"));
            }
        }
        s.push_str(";\n");
        s
    };
    std::fs::write(dir.join("port_views.sql"), sql)
        .with_context(|| format!("write checks/port_views.sql"))?;
    // `nuthatch check` compares against this fixture. A fresh nest has empty views, so the
    // counts are zero; `--update` after a real backfill is the author's next step.
    let expected = if views.is_empty() {
        serde_json::json!([{ "port_ok": 1 }])
    } else {
        let rows: Vec<serde_json::Value> = views
            .iter()
            .enumerate()
            .map(|(i, _)| {
                if i == 0 {
                    serde_json::json!({ "n": 0 })
                } else {
                    serde_json::json!({ "n": 0 })
                }
            })
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
}
