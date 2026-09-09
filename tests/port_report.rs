//! RFC-0044 S1: the committed four-class fixture report is the drift gate. A class change or a
//! deleted citation is a red test, the same shape `tests/skill_refs.rs` is for the builder skill.

use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("skills/nuthatch-subgraph-port/fixtures/four-classes")
}

#[test]
fn four_classes_fixture_matches_committed_report() {
    let dir = fixture_dir();
    let report = nuthatch::port_report::classify_dir(&dir).expect("classify fixture");
    let got = nuthatch::port_report::render_report(&report);
    let expected_path = dir.join("expected.md");
    let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|e| {
        panic!(
            "expected.md must be committed next to the fixture ({e}). Re-run this test after \
             writing the classifier's output to {}",
            expected_path.display()
        )
    });
    assert_eq!(
        got, expected,
        "four-classes expected.md drifted - if the classes are right, commit the new report; \
         if a class changed, that is the bug"
    );
}

#[test]
fn four_classes_fixture_hits_each_class_on_the_known_fields() {
    let report = nuthatch::port_report::classify_dir(&fixture_dir()).unwrap();
    let class = |entity: &str, field: &str| {
        report
            .fields
            .iter()
            .find(|r| r.entity == entity && r.field == field)
            .unwrap_or_else(|| panic!("missing {entity}.{field}"))
            .class
    };
    use nuthatch::port_report::Class;
    assert_eq!(class("Pool", "sqrtPrice"), Class::Exact);
    assert_eq!(class("Bundle", "ethPriceUSD"), Class::Exact);
    assert_eq!(class("Pool", "swaps"), Class::Exact);
    assert_eq!(class("Token", "symbol"), Class::CallDerived);
    assert_eq!(class("Token", "decimals"), Class::CallDerived);
    assert_eq!(class("Token", "derivedETH"), Class::FixedPoint);
    assert_eq!(class("_Schema_", "tokenSearch"), Class::Unreachable);
    assert_eq!(class("Token", "name"), Class::Unreachable);
    assert_eq!(class("BlockStat", "blockNumber"), Class::Unreachable);

    let derived = report
        .fields
        .iter()
        .find(|r| r.entity == "Token" && r.field == "derivedETH")
        .unwrap();
    assert!(
        derived.reason.contains("will not reproduce"),
        "fixed-point must say it will not reproduce: {}",
        derived.reason
    );
    assert!(
        derived.citation.file.contains("pricing.ts") || derived.citation.file.contains("core.ts"),
        "derivedETH citation should name the mapping, got {}",
        derived.citation.file
    );
}

#[test]
fn missing_schema_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = nuthatch::port_report::classify_dir(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("schema.graphql"),
        "must name the missing file: {msg}"
    );
}
