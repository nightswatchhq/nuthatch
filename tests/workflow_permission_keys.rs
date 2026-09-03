//! #1095 - a `permissions:` key GitHub does not accept kills a workflow **before its first job**.
//!
//! `required-contexts.yml` carried `administration: read`, which is an App/PAT scope and not a
//! workflow permission. GitHub rejected the whole file at validation, so every run died with zero
//! jobs, no logs and a 404 from the jobs API - for about a hundred runs. The job whose entire purpose
//! is noticing branch protection drift had never once made the comparison, and nothing said so,
//! because a workflow that fails before it starts looks exactly like a workflow nobody watches.
//!
//! That is a whole-file, silent, indefinite failure caused by one word, and it is checkable offline
//! from the tree. This is that check.

use std::path::{Path, PathBuf};

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

/// Collect `(file, line number, key)` for every mapping key under a `permissions:` block.
///
/// Deliberately line-based, matching the other workflow tests in this suite: adding a YAML parser to
/// the dev-dependencies to read four short blocks would be a heavier answer than the question.
/// A block ends at the first line indented no deeper than the `permissions:` key itself.
fn permission_keys(path: &Path) -> Vec<(usize, String)> {
    let text = std::fs::read_to_string(path).expect("read workflow");
    let mut out = Vec::new();
    let mut block_indent: Option<usize> = None;
    for (i, line) in text.lines().enumerate() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();

        if let Some(open) = block_indent {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if indent <= open {
                block_indent = None; // the block ended; fall through and re-test this line
            } else if let Some((key, _)) = trimmed.split_once(':') {
                out.push((i + 1, key.trim().to_string()));
                continue;
            } else {
                continue;
            }
        }

        if trimmed == "permissions:" {
            block_indent = Some(indent);
        }
        // A scalar form - `permissions: read-all` or `permissions: {}` - has no keys to check.
    }
    out
}

#[test]
fn every_workflow_permission_key_is_one_github_accepts() {
    let mut bad = Vec::new();
    for wf in workflows() {
        for (line, key) in permission_keys(&wf) {
            if !VALID.contains(&key.as_str()) {
                bad.push(format!(
                    "{}:{line}: `{key}` is not a workflow permission",
                    wf.file_name().unwrap().to_string_lossy()
                ));
            }
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

/// The scanner must be able to see the fault it was written for. A test that would pass against the
/// broken file is worth nothing, and this suite's own history is the argument: #1095's predecessor
/// checked a string the scanner never matched.
#[test]
fn the_scanner_finds_the_key_that_caused_1095() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("broken.yml");
    std::fs::write(
        &f,
        "name: x\npermissions:\n  contents: read\n  administration: read\n\njobs:\n  a:\n    permissions:\n      issues: write\n",
    )
    .unwrap();
    let keys = permission_keys(&f);
    assert_eq!(
        keys.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>(),
        vec!["contents", "administration", "issues"],
        "the scanner must read both the top-level and the job-level block"
    );
    let invalid: Vec<&str> = keys
        .iter()
        .map(|(_, k)| k.as_str())
        .filter(|k| !VALID.contains(k))
        .collect();
    assert_eq!(invalid, vec!["administration"]);
}
