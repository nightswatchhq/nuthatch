//! RFC-0059 packaging: folds stay outside the default binary, and a nest that needs them is refused
//! rather than served without them. The deletion test for every S1 slice builds on this file.

mod common;

use std::path::PathBuf;

use common::tape::*;
use nuthatch::config::Config;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn nest_with_folds() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    scaffold_nest(dir.path(), "usdc", USDC);
    std::fs::create_dir_all(dir.path().join("folds")).unwrap();
    std::fs::write(
        dir.path().join("folds/count.sql"),
        "SELECT count(*) AS n FROM usdc__transfer",
    )
    .unwrap();
    dir
}

#[test]
fn a_nest_shipping_folds_is_refused_not_served_without_them() {
    let dir = nest_with_folds();
    let err = Config::load(dir.path()).expect_err("folds/ must not load silently");
    let msg = format!("{err:#}");
    assert!(msg.contains("folds/"), "{msg}");
    #[cfg(not(feature = "folds"))]
    assert!(
        msg.contains("--features folds"),
        "must name the feature: {msg}"
    );

    std::fs::remove_dir_all(dir.path().join("folds")).unwrap();
    Config::load(dir.path()).expect("the same nest without folds/ loads");
}

#[cfg(not(feature = "folds"))]
#[test]
fn a_default_build_has_no_fold_command() {
    use clap::CommandFactory;
    use nuthatch::cli::Cli;

    fn walk(cmd: &clap::Command, path: &str) {
        for sub in cmd.get_subcommands() {
            let p = format!("{path} {}", sub.get_name());
            assert!(!sub.get_name().contains("fold"), "`{p}` is folds surface");
            walk(sub, &p);
        }
        for arg in cmd.get_arguments() {
            assert!(
                !arg.get_id().as_str().contains("fold"),
                "`{path} --{}` is folds surface",
                arg.get_id()
            );
        }
    }
    walk(&Cli::command(), "nuthatch");
}

#[test]
fn folds_is_off_by_default_and_graph_enables_it() {
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(root().join("Cargo.toml")).unwrap()).unwrap();
    let features = &manifest["features"];
    let names = |f: &str| -> Vec<String> {
        features[f]
            .as_array()
            .unwrap_or_else(|| panic!("feature `{f}` is declared"))
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    assert!(!names("default").contains(&"folds".to_string()));
    assert!(names("graph").contains(&"folds".to_string()));
    names("folds");

    let lib = std::fs::read_to_string(root().join("src/lib.rs")).unwrap();
    assert!(
        lib.contains("#[cfg(feature = \"folds\")]\npub mod folds;"),
        "src/folds.rs must be declared only behind the feature"
    );
}
