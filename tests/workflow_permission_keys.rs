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

/// What a `permissions:` line turned out to be.
#[derive(Debug, PartialEq)]
enum Perms {
    /// Keys the scanner read, with their line numbers.
    Keys(Vec<(usize, String)>),
    /// A form this scanner does not understand. **Not the same as "no keys"**, and the difference is
    /// the whole design: a scanner that silently reports nothing for a form it cannot read passes
    /// the workflow GitHub is about to reject, which is the failure this file exists to prevent.
    Unparseable(usize, String),
}

/// Scan every `permissions:` block in a workflow.
///
/// Deliberately line-based, matching `pr_review_harness.rs` and `release_provenance.rs`, which
/// already read workflow files this way. Hand-parsing YAML is an arms race - block form, inline flow
/// mapping, multi-line flow mapping, trailing comments - and this does not try to win it. It handles
/// the forms it can prove it handles and **refuses the rest**, so the guarantee is bounded rather
/// than optimistic.
fn permission_lines(path: &Path) -> Perms {
    let text = std::fs::read_to_string(path).expect("read workflow");
    let mut out = Vec::new();
    let mut block_indent: Option<usize> = None;

    for (i, raw) in text.lines().enumerate() {
        let line = i + 1;
        let indent = raw.len() - raw.trim_start().len();
        let trimmed = raw.trim();

        if let Some(open) = block_indent {
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if indent <= open {
                block_indent = None; // block ended; re-test this line below
            } else if let Some((key, _)) = trimmed.split_once(':') {
                out.push((line, key.trim().to_string()));
                continue;
            } else {
                return Perms::Unparseable(line, trimmed.to_string());
            }
        }

        let Some(rest) = trimmed.strip_prefix("permissions:") else {
            continue;
        };
        // A trailing `# comment` is valid and must not turn a mapping into an unknown form.
        let rest = rest.split('#').next().unwrap_or("").trim();

        if rest.is_empty() {
            block_indent = Some(indent);
        } else if rest == "{}" || rest == "read-all" || rest == "write-all" {
            // Scalar and empty forms carry no keys to check.
        } else if let Some(inner) = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            for pair in inner.split(',') {
                let pair = pair.trim();
                if pair.is_empty() {
                    continue;
                }
                match pair.split_once(':') {
                    Some((key, _)) => out.push((line, key.trim().to_string())),
                    None => return Perms::Unparseable(line, trimmed.to_string()),
                }
            }
        } else {
            // A flow mapping split across lines lands here, and so does anything else new.
            return Perms::Unparseable(line, trimmed.to_string());
        }
    }
    Perms::Keys(out)
}

#[test]
fn every_workflow_permission_key_is_one_github_accepts() {
    let mut bad = Vec::new();
    for wf in workflows() {
        let name = wf.file_name().unwrap().to_string_lossy().into_owned();
        match permission_lines(&wf) {
            Perms::Keys(keys) => {
                for (line, key) in keys {
                    if !VALID.contains(&key.as_str()) {
                        bad.push(format!(
                            "{name}:{line}: `{key}` is not a workflow permission"
                        ));
                    }
                }
            }
            Perms::Unparseable(line, text) => bad.push(format!(
                "{name}:{line}: this scanner cannot read `{text}`, so it cannot vouch for the keys \
                 in it. Rewrite it as a block mapping, or teach the scanner the form - reporting \
                 nothing here would be the silent pass this test exists to prevent"
            )),
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

/// The scanner must be able to see the fault it was written for, and must **refuse** what it cannot
/// read. A test that would pass against the broken file is worth nothing, and this suite's own
/// history is the argument: #1095's predecessor checked a string the scanner never matched.
#[test]
fn the_scanner_finds_the_key_that_caused_1095() {
    let dir = tempfile::tempdir().unwrap();

    let block = dir.path().join("block.yml");
    std::fs::write(
        &block,
        "name: x\npermissions:\n  contents: read\n  administration: read\n\njobs:\n  a:\n    permissions:\n      issues: write\n",
    )
    .unwrap();
    assert_eq!(
        permission_lines(&block),
        Perms::Keys(vec![
            (3, "contents".into()),
            (4, "administration".into()),
            (9, "issues".into()),
        ]),
        "the scanner must read both the top-level and the job-level block"
    );

    // Inline flow mapping, with a trailing comment, which GitHub rejects just as hard.
    let inline = dir.path().join("inline.yml");
    std::fs::write(
        &inline,
        "name: y\npermissions: { contents: read, administration: read } # why\njobs:\n  a:\n    permissions: read-all\n",
    )
    .unwrap();
    assert_eq!(
        permission_lines(&inline),
        Perms::Keys(vec![(2, "contents".into()), (2, "administration".into())]),
        "an inline mapping must be scanned, a trailing comment must not hide it, and \
         `permissions: read-all` has no keys to check"
    );

    // A flow mapping split across lines is valid YAML this scanner does not read. It must say so
    // rather than report no keys, which would be a silent pass on a file GitHub will reject.
    let multiline = dir.path().join("multiline.yml");
    std::fs::write(
        &multiline,
        "name: z\npermissions: {\n  administration: read\n}\n",
    )
    .unwrap();
    assert!(
        matches!(permission_lines(&multiline), Perms::Unparseable(2, _)),
        "an unreadable form must be refused, not silently treated as empty"
    );

    let empty = dir.path().join("empty.yml");
    std::fs::write(&empty, "name: w\npermissions: {}\n").unwrap();
    assert_eq!(permission_lines(&empty), Perms::Keys(vec![]));
}
