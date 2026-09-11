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
    /// Running totals emitted as RFC-0041 incremental entities rather than as views (#1214).
    pub entities: Vec<EmittedEntity>,
    /// Entities no view answers, because not one of their exact fields reached a column. Named
    /// rather than served by a placeholder that returns `1` (#1277); the per-field reasons are in
    /// `skipped_fields` and in the report.
    pub entities_without_views: Vec<String>,
    /// How much of what the report promised the overlay actually answers (#1277).
    pub coverage: Coverage,
}

/// Fields answered over fields promised, which is the one number that says whether a port is done.
///
/// The report classifies a field `exact` when the mapping computes it purely from decoded events.
/// That is a claim about *reproducibility in principle*. Whether this overlay reproduces it is a
/// different question, and on the RFC-0044 S3 acceptance port the two answers were **205 and 27**.
/// Nothing printed the second one, so a reader saw a report promising 205 byte-identical fields and
/// an overlay of nineteen view files, and had no way to tell that 178 of those fields could not be
/// asked for at all (#1277).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Fields the report classified `exact` - what was promised.
    pub classified_exact: usize,
    /// Of those, fields a `views/*.sql` projection names.
    pub in_views: usize,
    /// Of those, fields maintained incrementally as an RFC-0041 entity. Answered, just not by a view.
    pub incremental: usize,
    /// Of those, `@derivedFrom` reverse relations. **Answered, and they never wanted a column.**
    ///
    /// A `@derivedFrom` field stores nothing: it is a reverse lookup that RFC-0053 S2 lowers to a JSON
    /// aggregation over the forward reference. Counting it unanswered understated the overlay by 8 on
    /// the pinned target (#1284) and would have had a porter hunting for columns that must not exist.
    pub derived: usize,
}

impl Coverage {
    /// Answered by any of the three routes. A field is in exactly one: `view_for_entity` skips a
    /// materialised field, and a `@derivedFrom` field reaches no column by construction.
    pub fn answered(&self) -> usize {
        self.in_views + self.incremental + self.derived
    }

    /// `exact` fields no artefact answers.
    pub fn unanswered(&self) -> usize {
        self.classified_exact.saturating_sub(self.answered())
    }

    /// One line, for the CLI and for `README.md`. Percent of promised, floored, so a port that
    /// answers 27 of 205 cannot round itself up to anything reassuring.
    pub fn summary(&self) -> String {
        let pct = (self.answered() * 100)
            .checked_div(self.classified_exact)
            .unwrap_or(0);
        format!(
            "{} of {} fields the report calls exact are answered ({}%): {} in views, {} maintained \
             incrementally, {} derived by reverse lookup, {} not answered at all",
            self.answered(),
            self.classified_exact,
            pct,
            self.in_views,
            self.incremental,
            self.derived,
            self.unanswered(),
        )
    }
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

/// One authored incremental entity emitted for a subgraph's running totals (RFC-0044 S5, #1214).
#[derive(Debug, Clone)]
pub struct EmittedEntity {
    /// The GraphQL entity name.
    pub entity: String,
    /// The declaration name, which is also the file stem `entities.toml` requires them to share.
    pub name: String,
    pub file: String,
    pub sql: String,
    /// Fields materialised incrementally rather than at query time.
    pub fields: Vec<String>,
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
        "✓ emitted {} [[calls]], {} view(s) and {} incremental entit(ies) into {}",
        result.calls.len(),
        result.views.len(),
        result.entities.len(),
        nest.display()
    );
    for e in &result.entities {
        println!(
            "  entities/{}.sql  maintained incrementally: {}",
            e.name,
            e.fields.join(", ")
        );
    }
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
    // And the entities no view answers at all. Stated as a count first, because nine of nineteen
    // is the shape of the problem and a reader scanning a long skipped-field list will not add it up
    // (#1277). A caller querying one of these gets an error from DuckDB, which is the point.
    if !result.entities_without_views.is_empty() {
        println!(
            "  ! {} of {} entit(ies) have no view - not one of their exact fields reached a column: {}",
            result.entities_without_views.len(),
            result.entities_without_views.len() + result.views.len(),
            result.entities_without_views.join(", ")
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
    // Last, because it is the line that says whether any of the above adds up to a port. The entity
    // count above is the shape of the problem; this is its size (#1277).
    println!("  coverage: {}", result.coverage.summary());
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

    let (entities, mut skipped_fields) =
        write_entities(nest, &report, &mappings, &config, &schema)?;
    // Where each field landed, so the view path reports "materialised as an entity" rather than
    // "not in this view" - RFC-0044 §6 requires the artefacts to say which of the two it was.
    let materialised: BTreeSet<(String, String)> = entities
        .iter()
        .flat_map(|e| e.fields.iter().map(|f| (e.entity.clone(), f.clone())))
        .collect();
    let (views, view_skipped, entities_without_views) =
        write_exact_views(nest, &report, &mappings, &config, &schema, &materialised)?;
    skipped_fields.extend(view_skipped);
    write_checks(nest, &views)?;

    // `in_views` counts distinct (entity, field) pairs a view projection names. `exact_fields` is the
    // key set of the map `exact_select_sql` renders, so this counts the projection rather than a
    // parallel list that could disagree with it - which is what #1248 was and what a metric derived
    // from bookkeeping would have inherited.
    let coverage = Coverage {
        classified_exact: report
            .fields
            .iter()
            .filter(|f| f.class == crate::port_report::Class::Exact)
            .count(),
        in_views: views
            .iter()
            .flat_map(|v| v.exact_fields.iter().map(|f| (v.entity.clone(), f.clone())))
            .collect::<BTreeSet<_>>()
            .len(),
        incremental: materialised.len(),
        // Counted from the report's own reason, which is where the directive is recorded. A reverse
        // relation is answered by a join at request time and must never become a stored column.
        derived: report
            .fields
            .iter()
            .filter(|f| f.class == crate::port_report::Class::Exact)
            .filter(|f| f.reason.contains("@derivedFrom"))
            .count(),
    };

    // **`README.md` carries the figure.** It was the report verbatim, and the report's summary table
    // says `exact 205` - a promise about the mapping, not about this overlay. A porter reading it had
    // every reason to believe the nest answered 205 fields (#1277).
    let readme = format!(
        "{report_text}\n## Overlay coverage\n\n{}\n\nThe class counts above describe the *mapping*: \
         a field is `exact` when it is a pure function of decoded events, which is a claim about what \
         is reproducible in principle. This line describes *this overlay*: what it actually answers. \
         A field counted unanswered is named with its reason in the `-- NOT IN THIS VIEW` comments of \
         the relevant `views/*.sql`, and on stdout when `port-emit` ran.\n",
        coverage.summary()
    );
    std::fs::write(nest.join("README.md"), &readme)
        .with_context(|| format!("write {}/README.md", nest.display()))?;

    Ok(EmitResult {
        calls: emitted_calls,
        views,
        report: report_text,
        skipped_calls,
        skipped_fields,
        entities,
        entities_without_views,
        coverage,
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

/// Emit `entities/*.sql` plus the `entities.toml` that declares them (RFC-0044 S5, #1214).
///
/// **Only accumulations become entities.** RFC-0044 §9 calls this slice "one file" on the
/// assumption that only the emit target changes, and that turns out not to survive contact with
/// RFC-0041's v1 shape gate: the overlay view folds with `last(.. ORDER BY ..)`, and neither `last`
/// nor `ORDER BY` is incremental v1 SQL, so the exact-field view cannot become an entity by
/// retargeting it. A running total can, and it is the one field shape a view answers *wrongly*
/// today, so this is the swap that was actually worth making.
///
/// **An entity is emitted only when there is one to declare.** `entities.toml` declaring nothing is
/// itself a `nuthatch check` error ("declares no entities; remove entities.toml until an incremental
/// relation is ready"), so a port with no running totals must leave no file behind at all. That is
/// also the sprint's absence case: a nest with no entities is still a nest.
fn write_entities(
    nest: &Path,
    report: &Report,
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> Result<(Vec<EmittedEntity>, Vec<SkippedField>)> {
    let mut exact_by_entity: BTreeMap<String, Vec<&crate::port_report::FieldRow>> = BTreeMap::new();
    for f in &report.fields {
        if f.class == Class::Exact && f.entity != "_Schema_" {
            exact_by_entity.entry(f.entity.clone()).or_default().push(f);
        }
    }

    let mut emitted: Vec<EmittedEntity> = Vec::new();
    let mut skipped: Vec<SkippedField> = Vec::new();
    for (entity, fields) in &exact_by_entity {
        let accumulated = map_accumulating_fields(entity, fields, mappings, config, schema);
        if accumulated.is_empty() {
            continue;
        }
        // **One entity, several arms.** A counter counts events from more than one table -
        // `pool.txCount` increments in both `handleModifyLiquidity` and `handleSwap` - and a field can
        // be written twice from one table under different keys, as `token0.txCount` and
        // `token1.txCount` are from one swap. Both used to be named as skipped with "one entity is one
        // relation in v1", which gated 45 fields on Uniswap V4 (#1313).
        //
        // An arm is a (table, key column) pair. Each arm projects its own fields and **zero** for every
        // other field, the arms are `UNION ALL`ed, and one outer aggregate folds the lot by key. That
        // also unifies the two sources: a column contributes its checked cast, a row count contributes
        // `1`, and the outer `sum` does the same job for both.
        //
        // The wrapping matters. RFC-0041 v1's validator requires exactly one `SELECT_NODE`, and a bare
        // `UNION ALL` parses as a `SET_OPERATION_NODE` - measured against DuckDB's own
        // `json_serialize_sql`. Wrapped in an outer `SELECT .. FROM ( .. )` it is a `SELECT_NODE` and a
        // derived table in `FROM` is not an expression subquery, so `nuthatch check` still validates it.
        let mut arms: BTreeMap<(String, String), Vec<AccumulatedField>> = BTreeMap::new();
        for a in accumulated {
            // A decoded column, or - for a singleton the mapping keys with a constant - that constant.
            // `new PoolManager('1')` has no key *in the data* because the key is in the mapping, and
            // reporting "nothing identifies which `PoolManager` a row belongs to" was true and the wrong
            // conclusion (#1313). Quoted at the point of resolution, so both drop into the `SELECT` list.
            let key = id_column_for_table(entity, &a.table, mappings, config)
                .and_then(|c| resolve_column(&a.table, &c, schema))
                .map(|c| format!("\"{c}\""))
                .or_else(|| literal_key_for_table(entity, &a.table, mappings, config));
            let Some(key) = key else {
                skipped.push(SkippedField {
                    entity: entity.clone(),
                    field: a.field.clone(),
                    citation: a.citation.clone(),
                    why: format!(
                        "accumulates over `{}` but nothing in the mapping identifies which `{entity}` a \
                         row belongs to, so there is no key to group by",
                        a.table
                    ),
                });
                continue;
            };
            arms.entry((a.table.clone(), key)).or_default().push(a);
        }
        if arms.is_empty() {
            continue;
        }
        // Every field any arm contributes, so each arm can zero-fill the rest.
        let mut fields: Vec<String> = arms.values().flatten().map(|a| a.field.clone()).collect();
        fields.sort();
        fields.dedup();
        // A field written twice *within one arm* is still ambiguous: two contributions to the same key
        // from the same row, which is a different thing from two arms and is not this change.
        let mut dropped: BTreeSet<String> = BTreeSet::new();
        for ((table, _), list) in &arms {
            let mut seen = BTreeSet::new();
            for a in list {
                if !seen.insert(a.field.clone()) {
                    dropped.insert(a.field.clone());
                    skipped.push(SkippedField {
                        entity: entity.clone(),
                        field: a.field.clone(),
                        citation: a.citation.clone(),
                        why: format!(
                            "has two accumulating assignments on `{table}` under the same key; that is \
                             two contributions from one row rather than two arms, so add it by hand"
                        ),
                    });
                }
            }
        }
        fields.retain(|f| !dropped.contains(f));
        if fields.is_empty() {
            continue;
        }

        let name = to_alias(entity);
        let mut sql = String::new();
        sql.push_str(&format!(
            "-- Running totals of `{entity}`, maintained incrementally (RFC-0041).\n"
        ));
        sql.push_str(
            "-- A subgraph accumulates these one event at a time; the decoded table holds the\n             -- deltas, so the total is their sum and never the latest value. The exact fields that\n             -- *are* latest-value live in views/, and README.md says which field went where.\n",
        );
        for ((table, key), list) in &arms {
            for a in list.iter().filter(|a| fields.contains(&a.field)) {
                let how = match (&a.source, a.negated) {
                    (AccumulationSource::Column(c), false) => format!("sums `{c}`"),
                    (AccumulationSource::Column(c), true) => format!("sums the negation of `{c}`"),
                    (AccumulationSource::Rows, false) => "counts rows".to_string(),
                    (AccumulationSource::Rows, true) => "counts rows, negated".to_string(),
                };
                sql.push_str(&format!(
                    "-- `{entity}.{}` {how} of `{table}` keyed by {key}: {}\n",
                    a.field,
                    a.citation.display()
                ));
            }
        }

        // **The cast is checked, and its failure is reported rather than swallowed.** Every event
        // param is sealed as exact decimal *text* (RFC-0047 §1), so `sum("col")` does not even
        // type-check - `sum(VARCHAR)` has no candidate. `TRY_CAST` to `DECIMAL(38,0)` is the
        // conversion RFC-0047 §2 C1 names, but on its own it yields NULL past 38 digits and `sum`
        // skips NULLs, which would drop a real uint256 out of a total in silence. C1's rule is
        // explicit that no conversion out of the exact text may narrow silently, so each total
        // carries an `_overflow` companion that is 1 when any contributing row could not be
        // represented. `max` is one of the six aggregates v1 maintains, so the flag costs nothing
        // the total does not already cost.
        //
        // A row count casts nothing and cannot narrow, so a field sourced only from row counts carries
        // no flag - a column that is always 0 would read as evidence that something could overflow.
        let needs_overflow: BTreeSet<String> = arms
            .values()
            .flatten()
            .filter(|a| matches!(a.source, AccumulationSource::Column(_)))
            .map(|a| a.field.clone())
            .collect();

        let mut inner: Vec<String> = Vec::new();
        for ((table, key), list) in &arms {
            let mut cols = vec![format!("    {key} AS \"id\"")];
            for f in &fields {
                match list.iter().find(|a| &a.field == f) {
                    Some(a) => match &a.source {
                        AccumulationSource::Column(column) => {
                            let cast = format!("TRY_CAST(\"{column}\" AS DECIMAL(38,0))");
                            let term = if a.negated {
                                format!("-{cast}")
                            } else {
                                cast.clone()
                            };
                            cols.push(format!("    {term} AS \"{f}\""));
                            if needs_overflow.contains(f) {
                                cols.push(format!(
                                    "    CASE WHEN \"{column}\" IS NOT NULL AND {cast} IS NULL THEN 1 ELSE 0 END AS \"{f}_overflow\""
                                ));
                            }
                        }
                        AccumulationSource::Rows => {
                            cols.push(format!(
                                "    {} AS \"{f}\"",
                                if a.negated { "-1" } else { "1" }
                            ));
                            if needs_overflow.contains(f) {
                                cols.push(format!("    0 AS \"{f}_overflow\""));
                            }
                        }
                    },
                    // This arm does not touch the field, so it contributes nothing to its total.
                    None => {
                        cols.push(format!("    CAST(0 AS DECIMAL(38,0)) AS \"{f}\""));
                        if needs_overflow.contains(f) {
                            cols.push(format!("    0 AS \"{f}_overflow\""));
                        }
                    }
                }
            }
            inner.push(format!(
                "  SELECT\n{}\n  FROM \"{table}\"",
                cols.join(",\n")
            ));
        }

        let mut outer = vec!["  \"id\"".to_string()];
        for f in &fields {
            outer.push(format!("  sum(\"{f}\") AS \"{f}\""));
            if needs_overflow.contains(f) {
                outer.push(format!("  max(\"{f}_overflow\") AS \"{f}_overflow\""));
            }
        }
        sql.push_str(&format!(
            "SELECT\n{}\nFROM (\n{}\n)\nGROUP BY \"id\"\n",
            outer.join(",\n"),
            inner.join("\n  UNION ALL\n")
        ));

        emitted.push(EmittedEntity {
            entity: entity.clone(),
            name: name.clone(),
            file: format!("entities/{name}.sql"),
            sql,
            fields: fields.clone(),
        });
    }

    let dir = nest.join("entities");
    let operator_declarations = load_operator_entity_declarations(nest, &dir)?;
    remove_generated_entities(&dir)?;
    if emitted.is_empty() {
        // Leave nothing behind. An `entities.toml` that declares nothing fails `check`, and an
        // empty `entities/` directory makes `has_declarations` claim this nest has maintained state.
        if operator_declarations.is_empty() {
            let _ = std::fs::remove_file(nest.join(crate::entities::ENTITY_FILE));
        } else {
            write_operator_entity_declarations(nest, &operator_declarations)?;
        }
        return Ok((emitted, skipped));
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let mut toml = String::from(
        "# Authored incremental entities emitted by `port-emit` (RFC-0041, RFC-0044 S5).\n         #\n         # `max_rows` is an admission and runtime bound, not documentation: crossing it faults the\n         # nest loudly. The value below is a starting point, not a measurement. Set it from the\n         # cardinality this relation actually reaches on your chain before running unattended.\n",
    );
    for e in &emitted {
        std::fs::write(nest.join(&e.file), &e.sql).with_context(|| format!("write {}", e.file))?;
        toml.push_str(&format!(
            "\n[[entities]]\nname = \"{}\"\nsql = \"{}\"\nkey = [\"id\"]\nmax_rows = 500_000\n",
            e.name, e.file
        ));
    }
    for declaration in &operator_declarations {
        let mut root = toml::map::Map::new();
        root.insert(
            "entities".into(),
            toml::Value::Array(vec![toml::Value::Table(declaration.clone())]),
        );
        toml.push('\n');
        toml.push_str(&toml::to_string(&toml::Value::Table(root))?);
    }
    std::fs::write(nest.join(crate::entities::ENTITY_FILE), toml).context("write entities.toml")?;
    Ok((emitted, skipped))
}

fn load_operator_entity_declarations(
    nest: &Path,
    dir: &Path,
) -> Result<Vec<toml::map::Map<String, toml::Value>>> {
    let manifest = nest.join(crate::entities::ENTITY_FILE);
    let raw = match std::fs::read_to_string(&manifest) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", manifest.display())),
    };
    let value: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parse {}", manifest.display()))?;
    let Some(entries) = value.get("entities").and_then(toml::Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut operator = Vec::new();
    for entry in entries {
        let Some(table) = entry.as_table() else {
            continue;
        };
        let generated = table
            .get("sql")
            .and_then(toml::Value::as_str)
            .map(|sql| dir.join(sql.strip_prefix("entities/").unwrap_or(sql)))
            .filter(|path| path.is_file())
            .and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|sql| sql.starts_with("-- Running totals of `"));
        if !generated {
            operator.push(table.clone());
        }
    }
    Ok(operator)
}

fn write_operator_entity_declarations(
    nest: &Path,
    declarations: &[toml::map::Map<String, toml::Value>],
) -> Result<()> {
    let entities = toml::Value::Array(
        declarations
            .iter()
            .cloned()
            .map(toml::Value::Table)
            .collect(),
    );
    let mut root = toml::map::Map::new();
    root.insert("entities".into(), entities);
    std::fs::write(
        nest.join(crate::entities::ENTITY_FILE),
        toml::to_string(&toml::Value::Table(root))?,
    )
    .context("write entities.toml")?;
    Ok(())
}

/// Remove only SQL files this generator previously owned. A nest may also contain an entity written
/// by its operator, so `port-emit` must not treat the whole directory as disposable merely because
/// it no longer emits a running total.
fn remove_generated_entities(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("read entry in {}", dir.display()))?
            .path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("sql") {
            continue;
        }
        let sql =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if sql.starts_with("-- Running totals of `") {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    if std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .next()
        .is_none()
    {
        std::fs::remove_dir(dir).with_context(|| format!("remove {}", dir.display()))?;
    }
    Ok(())
}

/// Remove view files this emitter generated on a previous run. Keyed on the header
/// `-- Exact fields of ` + "`"` that `view_for_entity` writes, so an operator's own
/// `views/*.sql` in the same directory is never removed - the same contract
/// `remove_generated_entities` keeps for `entities/`.
fn remove_generated_views(dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let path = entry
            .with_context(|| format!("read entry in {}", dir.display()))?
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let sql =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        if sql.starts_with("-- Exact fields of `") {
            std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        }
    }
    Ok(())
}

fn write_exact_views(
    nest: &Path,
    report: &Report,
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
    materialised: &BTreeSet<(String, String)>,
) -> Result<(Vec<EmittedView>, Vec<SkippedField>, Vec<String>)> {
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

    // Views are regenerated, not merged: a previous run's file for an entity that now emits none
    // would otherwise survive as a stale table nothing re-examines. Only files carrying the
    // generated header are touched, so a hand-written view in the same directory is left alone.
    remove_generated_views(&views_dir)?;

    let mut emitted = Vec::new();
    let mut skipped = Vec::new();
    let mut without_views = Vec::new();
    for (entity, fields) in &exact_by_entity {
        let view = view_for_entity(entity, fields, mappings, config, schema, materialised);
        let file = format!("20-{}.sql", to_alias(entity));
        skipped.extend(view.skipped);
        // Nothing resolved to a column, so there is no view - only the account of why.
        if view.exact_fields.is_empty() {
            without_views.push(entity.clone());
            continue;
        }
        std::fs::write(views_dir.join(&file), &view.sql)
            .with_context(|| format!("write views/{file}"))?;
        emitted.push(EmittedView {
            entity: entity.clone(),
            file,
            sql: view.sql,
            exact_fields: view.exact_fields,
        });
    }
    Ok((emitted, skipped, without_views))
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

/// The constant a field is **always** assigned, if every assignment to it in every mapping is the same
/// literal.
///
/// `pool.collectedFeesToken0 = ZERO_BD` and nothing ever touches it again, so the field is zero for the
/// life of the subgraph and the view's answer is the literal. Ten fields on Uniswap V4 were reported as
/// having no column, which is true and unhelpful: there is no column because there is no variation
/// (#1313).
///
/// **Every assignment, for this entity and field, across all mappings.** Missing one would freeze a total
/// that really grows - a wrong number rather than a missing one, and the worst outcome available here. A
/// field assigned two different constants in different branches is not constant and gets nothing.
fn always_constant(
    entity: &str,
    field: &str,
    mappings: &crate::port_report::Mappings,
) -> Option<String> {
    // No "was anything assigned at all" flag: a non-literal assignment returns below, so `seen` is
    // `Some` exactly when every assignment was the same literal and there was at least one.
    let mut seen: Option<String> = None;
    for func in mappings.functions.values() {
        for asg in &func.assignments {
            if asg.entity != entity || asg.field != field {
                continue;
            }
            let lit = constant_literal(&asg.expr)?;
            match &seen {
                Some(prev) if prev != &lit => return None,
                _ => seen = Some(lit),
            }
        }
    }
    seen
}

/// A subgraph's zero and one conventions, as SQL. Nothing else: an unrecognised constant yields no
/// literal, so the field is reported as unanswered rather than guessed at.
fn constant_literal(expr: &str) -> Option<String> {
    match expr.trim() {
        "ZERO_BI" | "ZERO_BD" | "BigInt.zero()" | "BigDecimal.zero()" => Some("0".into()),
        "ONE_BI" | "ONE_BD" => Some("1".into()),
        _ => None,
    }
}

fn view_for_entity(
    entity: &str,
    fields: &[&crate::port_report::FieldRow],
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
    materialised: &BTreeSet<(String, String)>,
) -> ViewDraft {
    let view_name = to_alias(entity);
    let mut comments = Vec::new();
    // field → table → **SQL expression**. One field may be written from several triggering tables, and
    // what answers it is not always a bare column: a composed id is a concatenation of two of them. A
    // plain column is stored already quoted, so what the arms render is unchanged for it.
    let mut selects: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    // Fields with no column because they have no variation; projected as literals beside the rest.
    let mut constants: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped = Vec::new();

    for f in fields {
        // **Claim the field only once it has a column.** `exact_fields` used to be pushed before
        // the mapping was attempted, so a field that reached no table was still advertised as
        // being in this view while the SQL never mentioned it (#1248).
        // A running total is materialised incrementally and is deliberately absent from the
        // overlay: the view's `last()` fold would answer with the most recent delta rather than the
        // total, which is the wrong number rather than a missing one.
        if materialised.contains(&(f.entity.clone(), f.field.clone())) {
            comments.push(format!(
                "-- `{}.{}` exact, maintained incrementally in entities/{}.sql, not here: {}",
                f.entity,
                f.field,
                to_alias(&f.entity),
                f.reason.replace('\n', " ")
            ));
            continue;
        }
        // **A field the mappings only ever set to a constant is that constant.** Checked before the
        // column search, because there is no column to find: `pool.collectedFeesToken0 = ZERO_BD` and
        // nothing ever touches it again (#1313).
        if let Some(lit) = always_constant(&f.entity, &f.field, mappings) {
            comments.push(format!(
                "-- `{}.{}` exact, and constant: every mapping assigns {} - {}:{}",
                f.entity, f.field, lit, f.citation.file, f.citation.line
            ));
            constants.insert(f.field.clone(), lit);
            continue;
        }
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

    // **One source for the list and for the SQL.**
    //
    // This used to be a parallel `Vec` pushed inside the loop above. The two were built from the same
    // information and still diverged (#1248), and a divergence here is invisible: `exact_fields` feeds
    // the generated check's projection and the coverage figure, so a field listed but not projected
    // reads as verified and as answered while DuckDB cannot answer it. Taking the list from `selects`
    // - the same map `exact_select_sql` renders - makes that impossible rather than unlikely, and
    // `every_emitted_view_projects_exactly_the_fields_it_lists` holds the invariant.
    //
    // Sorted, because `selects` is a `BTreeMap`. Nothing downstream depends on schema order.
    // Constants are answered fields too, so they belong in the list the generated check projects and the
    // coverage figure counts. `every_emitted_view_projects_exactly_the_fields_it_lists` is what would catch
    // this being forgotten.
    let exact_fields: Vec<String> = selects.keys().chain(constants.keys()).cloned().collect();

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
        // **No view, rather than a view that answers `1`.**
        //
        // This used to emit `CREATE VIEW "<entity>" AS SELECT 1 AS port_placeholder`, so that
        // `nuthatch check` had something to bind. On the RFC-0044 S3 acceptance port that put nine
        // of nineteen entities - `Bundle`, `PoolManager`, `Transaction`, `Tick` and all four
        // interval entities - behind a view returning one row holding the integer 1, with a full
        // provenance block and no degraded flag. `SELECT count(*) FROM "bundle"` answered `1`, and
        // the reference has exactly one `Bundle`, so a comparison on row counts agreed (#1277).
        //
        // A caller asking for an entity we cannot answer must get an error, not a row. The field
        // list is not lost: the comments above are returned in `sql` for the report, and the entity
        // is named in `EmitResult::entities_without_views`.
        //
        // It also repaired the check. The projection was `*` for exactly these views, because there
        // was no promised column to name, so those rows asserted nothing at all while reading as
        // `binds: true`. With no such view emitted, every row of `port_views.sql` names real
        // columns and the binder is a gate for all of them.
        // **A constant needs a row to ride on.** With no table there is no row source, so a field that is
        // always zero is still unanswerable here - there is nothing to project it from. Named rather than
        // left in `exact_fields`, which would claim a field for a view that does not exist: the #1248 shape
        // this early return was written to prevent.
        for (field, lit) in &constants {
            skipped.push(SkippedField {
                entity: entity.to_string(),
                field: field.clone(),
                citation: Citation {
                    file: "schema.graphql".into(),
                    line: 0,
                },
                why: format!(
                    "is always {lit}, but no field of `{entity}` reaches a column, so there is no row to \
                     project it from"
                ),
            });
        }
        return ViewDraft {
            sql,
            exact_fields: exact_fields
                .into_iter()
                .filter(|f| !constants.contains_key(f))
                .collect(),
            skipped,
        };
    }

    sql.push_str(&format!(
        "CREATE VIEW \"{view_name}\" AS\n{}\n",
        exact_select_sql(&selects, &constants)
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
fn exact_select_sql(
    selects: &BTreeMap<String, BTreeMap<String, String>>,
    constants: &BTreeMap<String, String>,
) -> String {
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
                        Some(expr) => [
                            format!("  {expr} AS \"{field}\""),
                            format!("  TRUE AS \"{PRESENT}{field}\""),
                        ],
                        None => [
                            format!("  NULL AS \"{field}\""),
                            format!("  FALSE AS \"{PRESENT}{field}\""),
                        ],
                    },
                )
                .collect();
            // A constant has no column and no arm of its own: with nothing to fold it is projected once
            // by the caller below. Inside an arm it would need a presence marker that is `TRUE`
            // unconditionally, which is a guard that cannot fail.
            if !fold {
                for (field, lit) in constants {
                    cols.push(format!("  {lit} AS \"{field}\""));
                }
            }
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
    let mut outer: Vec<String> = fields
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
    // The literal, once. Every row of every arm would carry the same value, so there is nothing for
    // `last()` to choose between and no arm that could fail to carry it.
    for (field, lit) in constants {
        outer.push(format!("  {lit} AS \"{field}\""));
    }
    format!(
        "SELECT\n{}\nFROM (\n{}\n)\nGROUP BY \"id\";",
        outer.join(",\n"),
        inner
    )
}

/// One accumulating field resolved to a real decoded column.
struct AccumulatedField {
    field: String,
    table: String,
    /// What to fold: a decoded column, or the rows themselves.
    source: AccumulationSource,
    negated: bool,
    citation: Citation,
}

/// What an accumulating assignment folds.
///
/// A subgraph's counters are not sums of a column - `pool.txCount = pool.txCount.plus(ONE_BI)` adds one per
/// event, which is `count(*)`. The matcher required the operand to be a decoded column, so every counter was
/// rejected and the field fell back to its *initialising* assignment (`= ZERO_BI`), which is why the family
/// read as "zero initialiser" rather than "running total". Eight fields on Uniswap V4 (#1310).
#[derive(Debug, Clone, PartialEq, Eq)]
enum AccumulationSource {
    /// `sum(TRY_CAST("col" AS DECIMAL(38,0)))`, with an `_overflow` companion.
    Column(String),
    /// `count(*)`. A unit increment, so there is no column to cast and nothing to overflow.
    Rows,
}

/// Is this operand a literal one?
///
/// The subgraph conventions (`ONE_BI`, `ONE_BD`) plus the explicit forms. Nothing else: a constant this does
/// not recognise falls through and is reported as skipped, rather than being folded as if it were one.
fn is_unit_constant(operand: &str) -> bool {
    matches!(
        operand.trim(),
        "ONE_BI"
            | "ONE_BD"
            | "BigInt.fromI32(1)"
            | "BigInt.fromString('1')"
            | "BigDecimal.fromString('1')"
    )
}

/// Fields of `entity` written as running totals, resolved to the table and column that feed them.
///
/// Deliberately the same resolution path as [`map_exact_field_tables`]: an operand that reaches no
/// real column yields nothing here either, so an accumulation the emitter cannot render is reported
/// as a skipped field rather than turned into a `sum()` over a column that does not exist.
fn map_accumulating_fields(
    entity: &str,
    fields: &[&crate::port_report::FieldRow],
    mappings: &crate::port_report::Mappings,
    config: &Config,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> Vec<AccumulatedField> {
    let mut out = Vec::new();
    for f in fields {
        for func in mappings.functions.values() {
            if func.kind == crate::port_report::HandlerKind::Block {
                continue;
            }
            for asg in &func.assignments {
                if asg.entity != entity || asg.field != f.field {
                    continue;
                }
                let Some(acc) = crate::port_report::accumulation(asg) else {
                    continue;
                };
                let Some(handler) = event_handler_for(func, mappings) else {
                    continue;
                };
                let Some(table) = table_for_handler(&handler.name, mappings, config) else {
                    continue;
                };
                // A **unit increment** counts rows. Only a unit: `plus(TWO_BI)` would be
                // `2 * count(*)`, which is expressible and is a second shape to get right, so it is
                // left to fall through and be named as skipped rather than guessed at.
                let source = if is_unit_constant(&acc.operand) {
                    AccumulationSource::Rows
                } else {
                    let Some(raw) = crate::port_report::event_column(&acc.operand) else {
                        continue;
                    };
                    let Some(column) = resolve_column(&table, &raw, schema) else {
                        continue;
                    };
                    AccumulationSource::Column(column)
                };
                out.push(AccumulatedField {
                    field: f.field.clone(),
                    table,
                    source,
                    negated: acc.negated,
                    citation: f.citation.clone(),
                });
            }
        }
    }
    out
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
            // A bare column first, then a composition of them. Order matters only for clarity: a single
            // value is not a concatenation, and `expr_concat_parts` refuses one.
            let simple = assignment_event_column(asg, func, &mappings.functions);
            let composed = if simple.is_none() {
                crate::port_report::expr_concat_parts(&asg.expr, func, &mappings.functions)
            } else {
                None
            };
            if simple.is_none() && composed.is_none() {
                continue;
            }
            let Some(handler) = event_handler_for(func, mappings) else {
                continue;
            };
            let Some(table) = table_for_handler(&handler.name, mappings, config) else {
                continue;
            };
            let expr = match (simple, composed) {
                (Some(col), _) => match resolve_column(&table, &col, schema) {
                    Some(c) => format!("\"{c}\""),
                    None => continue,
                },
                (None, Some(parts)) => match concat_sql(&parts, &table, schema) {
                    Some(sql) => sql,
                    None => continue,
                },
                (None, None) => continue,
            };
            out.entry(table).or_insert(expr);
        }
    }
    out
}

/// A composed value as SQL: `"tx_hash" || '-' || CAST("log_index" AS VARCHAR)`.
///
/// Every column is cast to text, because the pieces are being concatenated into a string id and DuckDB's
/// `||` on a non-text operand is not the same expression. A literal is escaped by doubling its quotes, so
/// a separator containing one cannot end the literal early.
///
/// `None` if any column does not resolve against this table, so a key with a missing piece is never
/// emitted - that would be a different key, which is a wrong row rather than a missing one.
fn concat_sql(
    parts: &[crate::port_report::ConcatPart],
    table: &str,
    schema: &[nuthatch_decode::registry::TableSchema],
) -> Option<String> {
    use crate::port_report::ConcatPart;
    let mut out: Vec<String> = Vec::new();
    for part in parts {
        out.push(match part {
            ConcatPart::Literal(text) => format!("'{}'", text.replace('\'', "''")),
            ConcatPart::Column(col) => {
                let c = resolve_column(table, col, schema)?;
                format!("CAST(\"{c}\" AS VARCHAR)")
            }
        });
    }
    // A concatenation of literals only is not a column-backed value; it would be the same string on every
    // row, which is `always_constant`'s job and not reached through here.
    if !parts.iter().any(|p| matches!(p, ConcatPart::Column(_))) {
        return None;
    }
    Some(out.join(" || "))
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
            selects
                .entry("id".into())
                .or_default()
                .insert(table, format!("\"{col}\""));
        }
    }
}

/// The literal key a singleton entity is constructed with, in a handler that writes `table`.
///
/// Mirrors [`id_column_for_table`] - same traversal, same handler-to-table resolution - but looks for the
/// constant rather than a column. Kept separate rather than folded in, because a column key and a literal
/// key render differently at the call site and one `Option<String>` hiding which it got is the shape that
/// invites a mistake.
fn literal_key_for_table(
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
        if let Some(lit) = crate::port_report::entity_id_literal(entity, func) {
            return Some(lit);
        }
    }
    None
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
        if let Some(col) =
            crate::port_report::entity_id_event_column(entity, func, &mappings.functions)
        {
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
            // Every emitted view names at least one column: an entity whose fields all reached no
            // column emits no view at all (#1277). So there is no `*` case left, and that matters -
            // `*` was the projection for exactly those views, which made their rows read
            // `binds: true` while asserting nothing the binder could refuse.
            debug_assert!(!fields.is_empty(), "{entity}: emitted view with no columns");
            let projection = fields
                .iter()
                .map(|f| format!("\"{f}\""))
                .collect::<Vec<_>>()
                .join(", ");
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
        let sql = exact_select_sql(&selects, &BTreeMap::new());

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
        // Takes a column name and stores the expression for it, so these cases read as they did before
        // the map started carrying expressions.
        selects
            .entry(field.into())
            .or_default()
            .insert(table.into(), format!("\"{col}\""));
    }

    #[test]
    fn exact_select_keeps_fields_from_every_table() {
        let mut selects = BTreeMap::new();
        put(&mut selects, "id", "factory__pool_created", "token0");
        put(&mut selects, "id", "factory__token_updated", "token");
        put(&mut selects, "symbol", "factory__pool_created", "token0");
        put(&mut selects, "name", "factory__token_updated", "name");
        let sql = exact_select_sql(&selects, &BTreeMap::new());
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
        let sql = exact_select_sql(&selects, &BTreeMap::new());
        assert!(!sql.contains("UNION ALL"), "{sql}");
        assert!(!sql.contains("NULL AS"), "{sql}");
        assert!(sql.contains("GROUP BY \"id\""), "{sql}");
        assert!(sql.contains("last(\"symbol\""), "{sql}");
        assert!(sql.contains("\"token0\" AS \"id\""), "{sql}");
        assert!(sql.contains("\"token0\" AS \"symbol\""), "{sql}");
        assert!(sql.contains("FROM \"factory__pool_created\""), "{sql}");
    }
}
