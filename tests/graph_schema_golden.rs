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
    let raw =
        std::fs::read_to_string(fixtures().join("graph-node-introspection-uniswap-v4.json")).unwrap();
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
    assert_eq!(ours, bytes_ops, "Bytes operator set must match the reference");
    assert!(
        !ours.iter().any(|o| o.contains("starts_with") || o.contains("nocase")),
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
        !ours.iter().any(|v| v.matches("__").count() > 1),
        "nor may ours: {:?}",
        ours.iter().filter(|v| v.matches("__").count() > 1).collect::<Vec<_>>()
    );
    assert!(
        ours.contains("token0__symbol"),
        "ours must traverse one level too; got {} values",
        ours.len()
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
