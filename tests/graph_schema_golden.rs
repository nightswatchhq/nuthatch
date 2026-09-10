//! RFC-0053 S1 (#1265): the generated schema, diffed against a recorded graph-node.
//!
//! The reference is a real introspection of deployment
//! `Qmda2K4NcKWXB2AqyGUZEU35DgxSqFFRhkCmrJ8oC9po7i` (Uniswap V4 mainnet), captured 2026-09-10, and
//! the schema beside it is that deployment's own `schema.graphql`. Diffing a generated inventory
//! against a live engine's is the only way to know the shape is right; `docs/graph-schema-generation-rules.md`
//! records what was derived from it.

use std::collections::BTreeSet;
use std::path::PathBuf;

use nuthatch::graph_schema::{self, FieldType};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph-node")
}

fn reference() -> serde_json::Value {
    let raw = std::fs::read_to_string(fixtures().join("graph-node-introspection-uniswap-v4.json"))
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn parsed() -> graph_schema::Schema {
    let raw = std::fs::read_to_string(fixtures().join("uniswap-v4-schema.graphql")).unwrap();
    graph_schema::parse(&raw).expect("parse the reference schema")
}

fn ref_type_names(r: &serde_json::Value) -> BTreeSet<String> {
    r["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        // Introspection's own meta-types are not part of a generated schema.
        .filter(|n| !n.starts_with("__"))
        .collect()
}

#[test]
fn the_schema_parses_to_the_entities_the_reference_exposes() {
    let s = parsed();
    let r = reference();
    let ref_entities: BTreeSet<String> = r["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["kind"] == "OBJECT")
        .map(|t| t["name"].as_str().unwrap().to_string())
        .filter(|n| !n.starts_with('_') && n != "Query")
        .collect();
    let ours: BTreeSet<String> = s.entities.iter().map(|e| e.name.clone()).collect();
    assert_eq!(
        ours, ref_entities,
        "the parsed @entity set must equal the object types graph-node generated"
    );
    assert_eq!(ours.len(), 19, "the reference schema has 19 entities");
}

#[test]
fn the_generated_type_inventory_matches_the_reference() {
    let s = parsed();
    let r = reference();
    let ours: BTreeSet<String> = s.generated_type_names().into_iter().collect();
    let theirs = ref_type_names(&r);
    let missing: Vec<&String> = theirs.difference(&ours).collect();
    let extra: Vec<&String> = ours.difference(&theirs).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "type inventory differs from the recorded graph-node.\n  missing {missing:?}\n  extra {extra:?}"
    );
}

#[test]
fn the_query_root_fields_match_the_reference() {
    let s = parsed();
    let r = reference();
    let theirs: BTreeSet<String> = r["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Query")
        .unwrap()["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    let ours: BTreeSet<String> = s.root_field_names().into_iter().collect();
    let missing: Vec<&String> = theirs.difference(&ours).collect();
    let extra: Vec<&String> = ours.difference(&theirs).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "Query root fields differ.\n  missing {missing:?}\n  extra {extra:?}"
    );
    assert_eq!(theirs.len(), 40, "19 entities x2, plus _meta and _logs");
}

/// The irregular plurals are the point. `modifyLiquidity` → `modifyLiquidities` and
/// `poolDayData` → `poolDayDatas` both appear in the reference, and a `+ "s"` pluraliser produces
/// `modifyLiquiditys`, which no client would find. This asserts them by name so a regression cannot
/// hide behind the entities whose plural happens to be regular.
#[test]
fn the_irregular_plurals_are_the_reference_ones() {
    for (entity, want) in [
        ("ModifyLiquidity", "modifyLiquidities"),
        ("PoolDayData", "poolDayDatas"),
        ("PoolHourData", "poolHourDatas"),
        ("Pool", "pools"),
        ("Subscribe", "subscribes"),
        ("Transfer", "transfers"),
        ("Token", "tokens"),
        ("UniswapDayData", "uniswapDayDatas"),
    ] {
        assert_eq!(
            graph_schema::plural(entity),
            want,
            "plural of {entity} must be the reference's"
        );
    }
}

/// `Bytes` is not `String`: ten operators against eighteen, and no `_starts_with`, `_ends_with` or
/// `_nocase` anywhere. Advertising the string set over `Bytes` would have a client send queries the
/// real endpoint refuses.
#[test]
fn bytes_and_string_operator_sets_are_the_reference_ones() {
    let r = reference();
    let types = r["__schema"]["types"].as_array().unwrap();
    let swap_filter = types.iter().find(|t| t["name"] == "Swap_filter").unwrap();
    // `Swap.sender` is Bytes in the reference schema.
    let bytes_ops: BTreeSet<String> = swap_filter["inputFields"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["name"].as_str())
        .filter(|n| *n == "sender" || n.starts_with("sender_"))
        .map(|n| n.trim_start_matches("sender").to_string())
        .collect();
    let ours: BTreeSet<String> = graph_schema::filter_suffixes("Bytes")
        .into_iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        ours, bytes_ops,
        "Bytes operator set must match the reference"
    );
    assert!(
        !ours
            .iter()
            .any(|o| o.contains("starts_with") || o.contains("nocase")),
        "Bytes has no prefix or case-insensitive operators in the reference: {ours:?}"
    );
    assert_eq!(
        graph_schema::filter_suffixes("String").len(),
        20,
        "String carries the numeric set plus twelve text operators"
    );
}

/// One level of relation traversal in `*_orderBy`, and exactly one.
#[test]
fn order_by_traverses_one_relation_level_only() {
    let s = parsed();
    let r = reference();
    let theirs: BTreeSet<String> = r["__schema"]["types"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "Pool_orderBy")
        .unwrap()["enumValues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        theirs.iter().any(|v| v == "token0__symbol"),
        "the reference traverses one level"
    );
    assert!(
        !theirs.iter().any(|v| v.matches("__").count() > 1),
        "and never two"
    );
    let ours: BTreeSet<String> = s.order_by_values("Pool").into_iter().collect();
    assert!(
        ours.contains("token0__symbol"),
        "ours must traverse one level too; got {} values",
        ours.len()
    );

    // **Across every entity, not just `Pool`.** Checking `Pool` alone cannot see a second level: its
    // relations go to `Token`, whose own relations are all lists and therefore skipped, so a mutation
    // that recursed a second level produced nothing extra for `Pool` and a Pool-only assertion stayed
    // green. `Swap.pool -> Pool.token0 -> Token` is a real two-level path. Measured: that mutation
    // leaves the Pool-only form passing and fails this sweep.
    let mut two_level: Vec<String> = Vec::new();
    for e in &s.entities {
        for v in s.order_by_values(&e.name) {
            if v.matches("__").count() > 1 {
                two_level.push(format!("{}.{}", e.name, v));
            }
        }
    }
    assert!(
        two_level.is_empty(),
        "no *_orderBy value may traverse two relation levels; the reference has none: {:?}",
        &two_level[..two_level.len().min(5)]
    );
    // And the sweep must be capable of seeing one level, or it proves nothing about depth.
    let one_level = s
        .entities
        .iter()
        .flat_map(|e| s.order_by_values(&e.name))
        .filter(|v| v.contains("__"))
        .count();
    assert!(
        one_level > 50,
        "the sweep must actually traverse relations; found only {one_level} nested values"
    );
}

/// A relation is filtered by its id, which graph-node types as `String`, not `ID`.
#[test]
fn a_relation_filters_as_a_string_not_an_id() {
    let s = parsed();
    let pool = s.entities.iter().find(|e| e.name == "Pool").unwrap();
    let token0 = pool.fields.iter().find(|f| f.name == "token0").unwrap();
    assert!(
        matches!(token0.ty, FieldType::Entity(ref t) if t == "Token"),
        "Pool.token0 is a relation to Token, got {:?}",
        token0.ty
    );
    assert_eq!(token0.ty.filter_scalar(), Some("String"));
}

/// The operator rules, checked against **every** filter graph-node generated, not a sample.
///
/// This is the assertion S1 actually rests on: 19 entities, and for each one the exact set of input
/// field names on its `*_filter`. A single wrong operator, a missing nested relation filter, or a
/// derived field treated as filterable shows up here as a named difference.
#[test]
fn every_filter_input_matches_the_reference_field_for_field() {
    let s = parsed();
    let r = reference();
    let types = r["__schema"]["types"].as_array().unwrap();
    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for e in &s.entities {
        let want = format!("{}_filter", e.name);
        let Some(t) = types.iter().find(|t| t["name"] == want.as_str()) else {
            problems.push(format!("{want}: not in the reference"));
            continue;
        };
        let theirs: BTreeSet<String> = t["inputFields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap().to_string())
            .collect();
        let ours: BTreeSet<String> = s
            .filter_fields(&e.name)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        checked += theirs.len();
        for m in theirs.difference(&ours) {
            problems.push(format!("{want}: missing {m}"));
        }
        for x in ours.difference(&theirs) {
            problems.push(format!("{want}: extra {x}"));
        }
    }
    assert!(
        checked > 600,
        "the sweep must cover the real filters; only {checked} input fields seen"
    );
    assert!(
        problems.is_empty(),
        "{} filter input differences across {} entities, first 12:\n  {}",
        problems.len(),
        s.entities.len(),
        problems
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// And the same for every `*_orderBy`, which is where the one-level traversal rule is really tested.
#[test]
fn every_order_by_enum_matches_the_reference_value_for_value() {
    let s = parsed();
    let r = reference();
    let types = r["__schema"]["types"].as_array().unwrap();
    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for e in &s.entities {
        let want = format!("{}_orderBy", e.name);
        let Some(t) = types.iter().find(|t| t["name"] == want.as_str()) else {
            problems.push(format!("{want}: not in the reference"));
            continue;
        };
        let theirs: BTreeSet<String> = t["enumValues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect();
        let ours: BTreeSet<String> = s.order_by_values(&e.name).into_iter().collect();
        checked += theirs.len();
        for m in theirs.difference(&ours) {
            problems.push(format!("{want}: missing {m}"));
        }
        for x in ours.difference(&theirs) {
            problems.push(format!("{want}: extra {x}"));
        }
    }
    assert!(
        checked > 400,
        "the sweep must cover the real enums; only {checked} values seen"
    );
    assert!(
        problems.is_empty(),
        "{} orderBy differences, first 12:\n  {}",
        problems.len(),
        problems
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// The object types, field for field and type for type, across every entity.
///
/// With the two sweeps above this completes S1's structural claim: the generated schema's object
/// fields, filter inputs and order enums all equal a real graph-node's for this schema.
#[test]
fn every_object_type_matches_the_reference_field_for_field() {
    let s = parsed();
    let r = reference();
    let types = r["__schema"]["types"].as_array().unwrap();
    fn render(t: &serde_json::Value) -> String {
        match t["kind"].as_str().unwrap() {
            "NON_NULL" => format!("{}!", render(&t["ofType"])),
            "LIST" => format!("[{}]", render(&t["ofType"])),
            _ => t["name"].as_str().unwrap().to_string(),
        }
    }
    let mut problems: Vec<String> = Vec::new();
    let mut checked = 0usize;
    for e in &s.entities {
        let Some(t) = types.iter().find(|t| t["name"] == e.name.as_str()) else {
            problems.push(format!("{}: not in the reference", e.name));
            continue;
        };
        let theirs: BTreeSet<String> = t["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                let args: Vec<&str> = f["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a["name"].as_str().unwrap())
                    .collect();
                format!(
                    "{}: {} [{}]",
                    f["name"].as_str().unwrap(),
                    render(&f["type"]),
                    args.join(",")
                )
            })
            .collect();
        let ours: BTreeSet<String> = s
            .object_fields(&e.name)
            .into_iter()
            .map(|(n, ty, args)| format!("{n}: {ty} [{}]", args.join(",")))
            .collect();
        checked += theirs.len();
        for m in theirs.difference(&ours) {
            problems.push(format!("{}: missing {m}", e.name));
        }
        for x in ours.difference(&theirs) {
            problems.push(format!("{}: extra   {x}", e.name));
        }
    }
    assert!(
        checked > 200,
        "the sweep must cover the real object types; only {checked} fields seen"
    );
    assert!(
        problems.is_empty(),
        "{} object field differences, first 12:\n  {}",
        problems.len(),
        problems
            .iter()
            .take(12)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

/// RFC-0053 §Acceptance: the generated introspection against the recorded one, for every type the
/// reference has, comparing kind, field names, argument names and rendered types.
///
/// The declared divergences, which is what "a reviewed, machine-readable divergence list" means: they
/// are listed and asserted about, not silently skipped.
///
/// `_Block_`, `_Log_`, `_LogMeta_` and `_LogArgument_` are fixed graph-node internals this slice does
/// not model. `_Log_` exists to serve graph-node's own log-query surface rather than any subgraph
/// data, and `Query._logs` - `[_Log_!]!` taking `level, from, to, search, first, skip,
/// orderDirection` - is that surface's root field, so it is declared with them.
const DECLARED_DIVERGENCES: &[&str] = &["_Block_", "_Log_", "_LogMeta_", "_LogArgument_"];
const DECLARED_FIELD_DIVERGENCES: &[&str] = &["_logs"];

#[test]
fn generated_introspection_matches_the_reference_shape() {
    let s = parsed();
    let ours = nuthatch::graph_schema::introspection::render(&s);
    let r = reference();

    fn render(t: &serde_json::Value) -> String {
        match t["kind"].as_str().unwrap_or("") {
            "NON_NULL" => format!("{}!", render(&t["ofType"])),
            "LIST" => format!("[{}]", render(&t["ofType"])),
            _ => t["name"].as_str().unwrap_or("?").to_string(),
        }
    }
    fn index(v: &serde_json::Value) -> std::collections::BTreeMap<String, &serde_json::Value> {
        v["__schema"]["types"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| !t["name"].as_str().unwrap_or("").starts_with("__"))
            .map(|t| (t["name"].as_str().unwrap().to_string(), t))
            .collect()
    }
    let theirs = index(&r);
    let mine = index(&ours);
    let mut problems: Vec<String> = Vec::new();
    let mut compared = 0usize;

    for (name, t) in &theirs {
        if DECLARED_DIVERGENCES.contains(&name.as_str()) {
            continue;
        }
        let Some(m) = mine.get(name) else {
            problems.push(format!("{name}: absent from ours"));
            continue;
        };
        if t["kind"] != m["kind"] {
            problems.push(format!("{name}: kind {} vs {}", t["kind"], m["kind"]));
            continue;
        }
        for key in ["fields", "inputFields", "enumValues"] {
            let a = t[key].as_array();
            let b = m[key].as_array();
            if a.is_none() && b.is_none() {
                continue;
            }
            let sig = |arr: Option<&Vec<serde_json::Value>>| -> BTreeSet<String> {
                arr.map(|v| {
                    v.iter()
                        .map(|f| {
                            // **Argument types and defaults, not just names.** Dropping the
                            // `subgraphError: deny` default passed a name-only comparison, and a
                            // client relies on `first: 100` and `skip: 0` being the server's
                            // defaults rather than sending them explicitly. Measured.
                            let args: Vec<String> = f["args"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .map(|x| {
                                            format!(
                                                "{}:{}={}",
                                                x["name"].as_str().unwrap_or("?"),
                                                if x["type"].is_null() {
                                                    String::new()
                                                } else {
                                                    render(&x["type"])
                                                },
                                                x["defaultValue"].as_str().unwrap_or("-")
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let ty = if f["type"].is_null() {
                                String::new()
                            } else {
                                render(&f["type"])
                            };
                            format!(
                                "{}:{}[{}]",
                                f["name"].as_str().unwrap_or("?"),
                                ty,
                                args.join(",")
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
            };
            let drop_declared = |set: BTreeSet<String>| -> BTreeSet<String> {
                set.into_iter()
                    .filter(|m| {
                        !DECLARED_FIELD_DIVERGENCES
                            .iter()
                            .any(|d| m.starts_with(&format!("{d}:")))
                    })
                    .collect()
            };
            let (ta, mb) = (drop_declared(sig(a)), drop_declared(sig(b)));
            compared += ta.len();
            for x in ta.difference(&mb) {
                problems.push(format!("{name}.{key}: missing {x}"));
            }
            for x in mb.difference(&ta) {
                problems.push(format!("{name}.{key}: extra   {x}"));
            }
        }
    }
    // **A divergence must be present in the reference and absent from ours.**
    //
    // The first version only asserted the declared name exists in the reference, which let anything
    // be excused: adding `Pool_filter` to the list silenced a real difference and the suite stayed
    // green. Measured, which is why the rule is two-sided - that is the shape of "graph-node has
    // this and we do not model it yet", and nothing else qualifies.
    for d in DECLARED_DIVERGENCES {
        assert!(
            theirs.contains_key(*d),
            "declared divergence {d} is not in the reference; the list has rotted"
        );
        assert!(
            !mine.contains_key(*d),
            "{d} is declared a divergence but we generate it - the list is excusing a real \
             difference rather than recording an unmodelled type"
        );
    }
    let root_fields: BTreeSet<String> = theirs["Query"]["fields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_string())
        .collect();
    for d in DECLARED_FIELD_DIVERGENCES {
        assert!(
            root_fields.contains(*d),
            "declared field divergence {d} is not a root field of the reference"
        );
    }
    assert!(
        compared > 1200,
        "the diff must cover the reference; only {compared} members compared"
    );
    assert!(
        problems.is_empty(),
        "{} introspection differences, first 15:\n  {}",
        problems.len(),
        problems
            .iter()
            .take(15)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
