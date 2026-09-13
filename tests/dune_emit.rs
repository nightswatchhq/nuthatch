//! RFC-0055 S1: `nuthatch emit dune` over a fixture covering every §3.1 row must produce the golden
//! files byte for byte, and the same bytes on a second run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn fixture(part: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/dune_emit")
        .join(part)
}

fn read_dir(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().to_string(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect()
}

fn emit_into(out: &Path) -> BTreeMap<String, Vec<u8>> {
    nuthatch::dune_emit::run(nuthatch::cli::EmitDuneArgs {
        dir: fixture("nest").to_string_lossy().to_string(),
        out: out.to_string_lossy().to_string(),
        source: "fixture_ns".to_string(),
    })
    .unwrap();
    read_dir(out)
}

#[test]
fn dune_emit_matches_the_golden_files_and_is_stable_across_runs() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let first = emit_into(a.path());
    let second = emit_into(b.path());
    assert_eq!(first, second, "two runs over the same nest differ");

    let golden = read_dir(&fixture("expected"));
    assert_eq!(
        first.keys().collect::<Vec<_>>(),
        golden.keys().collect::<Vec<_>>(),
        "emitted file names differ from tests/fixtures/dune_emit/expected"
    );
    for (name, bytes) in &golden {
        assert_eq!(
            String::from_utf8_lossy(&first[name]),
            String::from_utf8_lossy(bytes),
            "{name} differs from its golden file"
        );
    }
}

#[test]
fn dune_emit_output_carries_no_path_from_the_machine_it_ran_on() {
    let out = tempfile::tempdir().unwrap();
    let nest = fixture("nest").to_string_lossy().to_string();
    for (name, bytes) in emit_into(out.path()) {
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains(&nest), "{name} names the nest directory");
        assert!(
            !text.contains(env!("CARGO_MANIFEST_DIR")),
            "{name} names the repo"
        );
    }
}

/// RFC-0055 S2 (#1358): every description in `semantic.toml` reaches the query its table became.
#[test]
fn dune_emit_carries_every_semantic_description_into_its_query() {
    let out = tempfile::tempdir().unwrap();
    let files = emit_into(out.path());
    let sem = nuthatch::semantic::load(&fixture("nest")).unwrap().unwrap();
    let readme = String::from_utf8(files["README.md"].clone()).unwrap();
    // The header is line comments, so multi-line text is compared joined onto one line.
    let flat = |s: &str| {
        s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    };

    let mut checked = 0;
    for (table, ts) in &sem.tables {
        let from = format!("FROM dune.fixture_ns.{table}\n");
        let Some(sql) = files
            .values()
            .map(|b| String::from_utf8_lossy(b).to_string())
            .find(|s| s.ends_with(&from))
        else {
            assert!(
                readme.contains(&format!("| `{table}` |")),
                "{table} is neither emitted nor listed as not emitted"
            );
            continue;
        };
        let header: Vec<&str> = sql.lines().take_while(|l| l.starts_with("--")).collect();
        if !ts.description.is_empty() {
            let want = format!("-- {}", flat(&ts.description));
            assert!(header.contains(&want.as_str()), "{table}: missing `{want}`");
            checked += 1;
        }
        if !ts.grain.is_empty() {
            let want = format!("-- Grain: {}", flat(&ts.grain));
            assert!(header.contains(&want.as_str()), "{table}: missing `{want}`");
            checked += 1;
        }
        for (col, desc) in &ts.columns {
            let prefix = format!("--   {col} ");
            let want = format!(": {}", flat(desc));
            assert!(
                header
                    .iter()
                    .any(|l| l.starts_with(&prefix) && l.contains(&want)),
                "{table}.{col}: missing description `{}`",
                flat(desc)
            );
            checked += 1;
        }
    }
    // 4 table descriptions, 3 grains and 16 column descriptions on the emitted tables.
    assert_eq!(
        checked, 23,
        "the fixture's descriptions were not all checked"
    );
}
