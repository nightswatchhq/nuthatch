//! #1095 - a `permissions:` key GitHub does not accept kills a workflow **before its first job**.
//!
//! `required-contexts.yml` carried `administration: read`, which is an App/PAT scope and not a
//! workflow permission. GitHub rejected the whole file at validation, so every run died with zero
//! jobs, no logs and a 404 from the jobs API - for about a hundred runs. The job whose entire purpose
//! is noticing branch protection drift had never once made the comparison, and nothing said so,
//! because a workflow that fails before it starts looks exactly like a workflow nobody watches.
//!
//! That is a whole-file, silent, indefinite failure caused by one word, and it is checkable offline
//! from the tree.
//!
//! **This parses the YAML rather than scanning lines**, and the history is the argument for it.
//! Three line-based revisions each missed a different valid spelling - the inline flow mapping
//! `permissions: { … }`, the same with a trailing comment, the multi-line flow mapping, and a quoted
//! `"permissions":` key. Every miss looked identical to success: the scanner reported no keys, so the
//! test passed on a file GitHub would reject. `yaml-rust2` is already a dependency of this crate for
//! subgraph manifests, so the real parser costs nothing and ends the guessing.

use std::path::{Path, PathBuf};
use yaml_rust2::{Yaml, YamlLoader};

/// Every key GitHub accepts inside a workflow `permissions:` block.
/// <https://docs.github.com/actions/using-jobs/assigning-permissions-to-jobs>
const VALID: &[&str] = &[
    "actions",
    "attestations",
    "checks",
    "contents",
    "deployments",
    "discussions",
    "id-token",
    "issues",
    "models",
    "packages",
    "pages",
    "pull-requests",
    "repository-projects",
    "security-events",
    "statuses",
];

fn workflows() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("the workflows directory must exist")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .collect();
    v.sort();
    v
}

/// Every key of every `permissions` mapping anywhere in the document.
///
/// Walks the whole tree rather than looking in the two places permissions are usually written, so a
/// job-level block, a reusable-workflow call and anywhere GitHub adds them next are all covered
/// without this test needing to know where to look.
fn permission_keys(doc: &Yaml, out: &mut Vec<String>) {
    match doc {
        Yaml::Hash(h) => {
            for (k, v) in h {
                if k.as_str() == Some("permissions") {
                    // A scalar - `read-all`, `write-all` - grants no individual keys.
                    if let Yaml::Hash(perms) = v {
                        for (pk, _) in perms {
                            out.push(
                                pk.as_str()
                                    .map(str::to_string)
                                    .unwrap_or_else(|| format!("{pk:?}")),
                            );
                        }
                    }
                }
                permission_keys(v, out);
            }
        }
        Yaml::Array(a) => {
            for v in a {
                permission_keys(v, out);
            }
        }
        _ => {}
    }
}

fn keys_in(path: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read_to_string(path).expect("read workflow");
    let docs = YamlLoader::load_from_str(&text).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for d in &docs {
        permission_keys(d, &mut out);
    }
    Ok(out)
}

#[test]
fn every_workflow_permission_key_is_one_github_accepts() {
    let mut bad = Vec::new();
    for wf in workflows() {
        let name = wf.file_name().unwrap().to_string_lossy().into_owned();
        match keys_in(&wf) {
            Ok(keys) => {
                for key in keys {
                    if !VALID.contains(&key.as_str()) {
                        bad.push(format!("{name}: `{key}` is not a workflow permission"));
                    }
                }
            }
            // Unparseable YAML is its own failure: GitHub would reject it too, and a test that
            // shrugged at it would be the silent pass this file exists to prevent.
            Err(e) => bad.push(format!("{name}: is not valid YAML: {e}")),
        }
    }
    assert!(
        bad.is_empty(),
        "GitHub rejects the whole file for one of these, and every run then dies before its first \
         job with no logs at all - the #1095 failure. `administration` is the specific word that did \
         it: it is an App/PAT scope, not a workflow one, and the token that needs it is supplied as a \
         secret instead.\n{}",
        bad.join("\n")
    );
}

/// The parser must see the fault in every spelling a workflow may legally use. A test that would pass
/// against the broken file is worth nothing, and this suite's own history is the argument: #1095's
/// predecessor checked a string the scanner never matched.
#[test]
fn the_bad_key_is_found_in_every_yaml_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let cases: &[(&str, &str)] = &[
        (
            "block",
            "name: x\npermissions:\n  contents: read\n  administration: read\n",
        ),
        (
            "inline",
            "name: x\npermissions: { contents: read, administration: read }\n",
        ),
        (
            "inline with a trailing comment",
            "name: x\npermissions: { administration: read } # why\n",
        ),
        (
            "flow mapping across lines",
            "name: x\npermissions: {\n  administration: read\n}\n",
        ),
        (
            "quoted key",
            "name: x\n\"permissions\": { administration: read }\n",
        ),
        (
            "single-quoted key",
            "name: x\n'permissions': { administration: read }\n",
        ),
        (
            "nested under a job",
            "name: x\njobs:\n  a:\n    permissions:\n      administration: read\n",
        ),
    ];
    for (label, yaml) in cases {
        let f = dir.path().join("w.yml");
        std::fs::write(&f, yaml).unwrap();
        let keys = keys_in(&f).unwrap_or_else(|e| panic!("{label}: {e}"));
        assert!(
            keys.iter().any(|k| k == "administration"),
            "the {label} form hid the bad key: got {keys:?}"
        );
    }

    // Forms that legitimately carry no individual keys.
    for (label, yaml) in [
        ("scalar", "name: x\npermissions: read-all\n"),
        ("empty mapping", "name: x\npermissions: {}\n"),
    ] {
        let f = dir.path().join("w.yml");
        std::fs::write(&f, yaml).unwrap();
        assert!(
            keys_in(&f).unwrap().is_empty(),
            "the {label} form should yield no keys"
        );
    }
}
