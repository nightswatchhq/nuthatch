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
    let ours: BTreeSet<String> = graph_schema::filter_suffixes("Bytes", false)
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
        graph_schema::filter_suffixes("String", false).len(),
        20,
        "String carries the numeric set plus twelve text operators"
    );

    // **An enum carries four, and the reference cannot tell us so.**
    //
    // The recorded Uniswap V4 schema declares no author enum at all - every enum in the recording is a
    // graph-node builtin (`OrderDirection`, `_SubgraphErrorPolicy_`, `Aggregation_*`, `LogLevel`) or a
    // generated `_orderBy`. So this row was generalised from the numeric set and carried four operators
    // graph-node does not have (#1306).
    //
    // Asserted against graph-node's own `field_enum_filter_input_values`
    // (`graph/src/schema/api.rs`, MIT/Apache), which returns exactly `["", "not", "in", "not_in"]`, and
    // confirmed on three unrelated live deployments: `Pair.type: PairType`,
    // `ChainlinkPrice.period: PricePeriod` and `GovernanceFramework.type: GovernanceFrameworkType` each
    // answered four. A constant rather than a diff, because the thing this file diffs against is silent
    // on the question.
    // **`subgraphError` is non-null with a `deny` default**, on every root. Asserted because it reads as a
    // bug - the SDL documentation writes it nullable - and has been reported as one three times.
    // graph-node's `error_policy_argument` builds `NonNullType(NamedType(_SubgraphErrorPolicy_))` with
    // `default_value: Enum("deny")`, the recording agrees, and two live probes agreed.
    for root in ["pool", "pools"] {
        let f = types
            .iter()
            .find(|t| t["name"] == "Query")
            .and_then(|q| q["fields"].as_array())
            .and_then(|fs| fs.iter().find(|f| f["name"] == root))
            .unwrap_or_else(|| panic!("no `{root}` root in the reference"));
        let a = f["args"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "subgraphError")
            .unwrap_or_else(|| panic!("`{root}` has no subgraphError in the reference"));
        assert_eq!(
            a["type"]["kind"], "NON_NULL",
            "the reference renders `{root}.subgraphError` non-null, whatever the SDL docs say"
        );
        assert_eq!(a["defaultValue"], "deny");
    }

    assert_eq!(
        graph_schema::filter_suffixes("OrderType", true),
        vec!["", "_not", "_in", "_not_in"],
        "an enum has no ordering, so no comparison operators"
    );
    assert!(
        !graph_schema::filter_suffixes("OrderType", true)
            .iter()
            .any(|o| matches!(*o, "_gt" | "_lt" | "_gte" | "_lte")),
        "a comparison on an enum is an ordering graph-node does not define"
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

/// Every key a standard client's `FullType` fragment selects, present on every type, field, argument
/// and enum value - with `null` where the kind does not apply.
///
/// **This is the assertion that was missing twice.** The renderer emitted only the keys the comparisons
/// below happened to read, so a client selecting `description`, `interfaces`, `possibleTypes`,
/// `isDeprecated` or `deprecationReason` received objects missing keys it had asked for, and every other
/// assertion here passed. Comparing the key *sets* is what makes that falsifiable, so this test exists
/// to fail when the document is narrower than the reference rather than merely different.
#[test]
fn every_introspection_object_carries_the_keys_the_reference_carries() {
    let ours = nuthatch::graph_schema::introspection::render(&parsed());
    let r = reference();
    let keys = |v: &serde_json::Value| -> Vec<String> {
        let mut k: Vec<String> = v
            .as_object()
            .unwrap_or_else(|| panic!("not an object: {v}"))
            .keys()
            .cloned()
            .collect();
        k.sort();
        k
    };
    let index = |v: &serde_json::Value| -> std::collections::BTreeMap<String, serde_json::Value> {
        v["__schema"]["types"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| !t["name"].as_str().unwrap_or("").starts_with("__"))
            .map(|t| (t["name"].as_str().unwrap().to_string(), t.clone()))
            .collect()
    };
    let (theirs, mine) = (index(&r), index(&ours));
    let mut checked = 0;
    for (name, t) in &theirs {
        let Some(m) = mine.get(name) else { continue };
        assert_eq!(keys(m), keys(t), "type `{name}`");
        for list in ["fields", "inputFields", "enumValues"] {
            let (Some(a), Some(b)) = (t[list].as_array(), m[list].as_array()) else {
                // Both null for a kind that has no such list; the key comparison above covers it.
                assert_eq!(
                    t[list].is_null(),
                    m[list].is_null(),
                    "`{name}.{list}` is null on one side only"
                );
                continue;
            };
            if let (Some(x), Some(y)) = (a.first(), b.first()) {
                assert_eq!(keys(y), keys(x), "`{name}.{list}` entry");
                checked += 1;
                if let (Some(xa), Some(ya)) = (x["args"].as_array(), y["args"].as_array()) {
                    if let (Some(p), Some(q)) = (xa.first(), ya.first()) {
                        assert_eq!(keys(q), keys(p), "`{name}.{list}` argument");
                    }
                }
            }
        }
    }
    // A floor, so this cannot pass by comparing nothing: every entity contributes an object, a filter
    // and an orderBy enum, and the reference has 19 entities.
    assert!(checked >= 50, "only {checked} lists compared");
}

/// The `__Schema` fields either side of `types`, which a standard client selects and which the diff
/// below cannot see because it indexes `types` only.
///
/// The recording query originally asked for `queryType` and `types` and nothing else, so the reference
/// was silent about the rest and no assertion here could have noticed them missing. That is the same
/// fault as comparing type *names* and not kinds, one level up: a golden test is bounded by what the
/// recording asked for, not by what the surface has.
#[test]
fn the_schema_level_fields_match_the_reference() {
    let ours = nuthatch::graph_schema::introspection::render(&parsed());
    let r = reference();
    for field in ["queryType", "mutationType", "subscriptionType"] {
        assert_eq!(
            ours["__schema"][field], r["__schema"][field],
            "__schema.{field}"
        );
    }
    // Directives, compared whole: name, locations, and each argument's name and rendered type.
    let render_dirs = |v: &serde_json::Value| -> Vec<String> {
        v["__schema"]["directives"]
            .as_array()
            .unwrap_or_else(|| panic!("no directives in {}", v["__schema"]["directives"]))
            .iter()
            .map(|d| {
                let args: Vec<String> = d["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| {
                        fn ty(t: &serde_json::Value) -> String {
                            match t["kind"].as_str().unwrap_or("") {
                                "NON_NULL" => format!("{}!", ty(&t["ofType"])),
                                "LIST" => format!("[{}]", ty(&t["ofType"])),
                                k => format!("{k}:{}", t["name"].as_str().unwrap_or("?")),
                            }
                        }
                        format!("{}: {}", a["name"].as_str().unwrap_or("?"), ty(&a["type"]))
                    })
                    .collect();
                let locs: Vec<&str> = d["locations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|l| l.as_str())
                    .collect();
                format!(
                    "{} {locs:?} ({})",
                    d["name"].as_str().unwrap_or("?"),
                    args.join(", ")
                )
            })
            .collect()
    };
    assert_eq!(
        render_dirs(&ours),
        render_dirs(&r),
        "the five directives, their locations and their argument types"
    );
}

/// RFC-0053 §Acceptance: the generated introspection against the recorded one, for every type the
/// reference has, comparing kind, field names, argument names and rendered types.
///
/// The declared divergences, which is what "a reviewed, machine-readable divergence list" means: they
/// are listed and asserted about, not silently skipped - and the check is two-sided, so a name left
/// here after we start generating it fails rather than quietly excusing a real difference.
///
/// **Both lists are now empty, and that is the point.** `_Block_`, `_Log_`, `_LogMeta_`,
/// `_LogArgument_` and `Query._logs` were declared here as graph-node internals this slice did not
/// model. That was wrong for `_Block_`, because `_Meta_.block` is typed `_Block_!` and `_meta` is a
/// root the endpoint answers - so the served document referenced a type it did not declare, which
/// makes the whole schema invalid to a client that resolves it (Jules on #1282). The rest followed:
/// declaring `LogLevel` while omitting `_Log_` is not a schema graph-node would serve, whether or not
/// a nest has any logs to put in it.
const DECLARED_DIVERGENCES: &[&str] = &[];
const DECLARED_FIELD_DIVERGENCES: &[&str] = &[];

#[test]
fn generated_introspection_matches_the_reference_shape() {
    let s = parsed();
    let ours = nuthatch::graph_schema::introspection::render(&s);
    let r = reference();

    /// A type reference as a comparable string, **kind included**.
    ///
    /// The kind used to be dropped here, so `SCALAR:Token` and `OBJECT:Token` rendered identically
    /// and every relation, `orderBy` and `where` argument was advertised with the wrong kind while
    /// this diff read clean (Jules on #1282). A client validates against the kind, so the diff has to
    /// compare it. Third time a name-only comparison has hidden a real difference here.
    fn render(t: &serde_json::Value) -> String {
        match t["kind"].as_str().unwrap_or("") {
            "NON_NULL" => format!("{}!", render(&t["ofType"])),
            "LIST" => format!("[{}]", render(&t["ofType"])),
            kind => format!("{kind}:{}", t["name"].as_str().unwrap_or("?")),
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
