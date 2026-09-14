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

/// A copy of the fixture nest, so an output check that fails to refuse writes nowhere real.
fn nest_copy() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let nest = tmp.path().join("nest");
    copy_tree(&fixture("nest"), &nest);
    (tmp, nest)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let dest = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

fn listing(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(path.strip_prefix(dir).unwrap().to_path_buf());
        }
    }
    out.sort();
    out
}

fn emit_to(dir: &Path, out: &Path) -> Result<(), String> {
    nuthatch::dune_emit::run(nuthatch::cli::EmitDuneArgs {
        dir: dir.to_string_lossy().to_string(),
        out: out.to_string_lossy().to_string(),
        source: "fixture_ns".to_string(),
    })
    .map_err(|e| format!("{e:#}"))
}

#[test]
fn dune_emit_refuses_an_output_directory_inside_the_nest() {
    let (tmp, nest) = nest_copy();
    std::fs::create_dir(tmp.path().join("elsewhere")).unwrap();
    let before = listing(&nest);

    let mut cases: Vec<(&str, PathBuf, PathBuf)> = vec![
        ("equal to --dir", nest.clone(), nest.clone()),
        (
            "a new subdirectory",
            nest.clone(),
            nest.join("dune/queries"),
        ),
        (
            "`..` inside the nest",
            nest.clone(),
            nest.join("views/../out"),
        ),
        (
            "`..` from a sibling back into the nest",
            nest.clone(),
            tmp.path().join("elsewhere/../nest/out"),
        ),
        // On macOS the tempdir is `/var/...` and its canonical form `/private/var/...`.
        (
            "--dir canonical, --out through the tempdir as given",
            nest.canonicalize().unwrap(),
            nest.join("out"),
        ),
    ];
    #[cfg(unix)]
    {
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&nest, &link).unwrap();
        cases.push(("a symlink to the nest", nest.clone(), link.join("out")));
    }

    for (case, dir, out) in cases {
        let err = emit_to(&dir, &out).expect_err(case);
        assert!(
            err.contains("refusing to write into the nest")
                && err.contains(&format!("--out `{}`", out.display())),
            "{case}: {err}"
        );
        assert_eq!(listing(&nest), before, "{case}: the nest was written to");
    }
}

#[test]
fn dune_emit_allows_a_directory_beside_the_nest() {
    let (tmp, nest) = nest_copy();
    let before = listing(&nest);
    // Shares the nest's name as a string prefix, which a component-wise comparison must allow.
    let out = tmp.path().join("nest-dune");
    emit_to(&nest, &out).unwrap();
    assert!(out.join("README.md").is_file());
    assert_eq!(listing(&nest), before, "the nest was written to");
}
