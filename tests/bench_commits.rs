//! #756 - a `commit` field you cannot `git cat-file` is the 289 ev/s failure mode.
//!
//! We used to record squash-merged PR heads. They exist on GitHub's API and nowhere a clone of
//! `main` can see. README's "How fast is it" traced to three of them. This walks every
//! `docs/bench/*.json` and requires the commit to resolve, and refuses the three documented ghosts
//! even on a clone that has no history.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The three README-facing ghosts named in #756. A string match so the check does not depend on
/// git having the objects.
const GHOSTS: &[&str] = &["707e1af", "12ba1ad", "ffb49a8"];

fn json_string_field<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let i = raw.find(&needle)?;
    let after = raw[i + needle.len()..].trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    Some(&after[..end])
}

fn git_cat_file(sha: &str) -> bool {
    Command::new("git")
        .args(["cat-file", "-t", sha])
        .current_dir(repo_root())
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "commit")
        .unwrap_or(false)
}

#[test]
fn bench_artifact_commits_are_not_the_known_ghosts() {
    let mut hits = Vec::new();
    for ent in std::fs::read_dir(repo_root().join("docs/bench")).unwrap() {
        let path = ent.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let Some(commit) = json_string_field(&raw, "commit") else {
            continue;
        };
        if GHOSTS.iter().any(|g| commit.starts_with(g)) {
            hits.push(format!(
                "{}: {commit}",
                path.file_name().unwrap().to_string_lossy()
            ));
        }
    }
    assert!(
        hits.is_empty(),
        "docs/bench artefacts still cite squash heads that are not on main (#756): {hits:?}"
    );
}

#[test]
fn bench_artifact_commits_resolve_in_git() {
    if !git_cat_file("HEAD") {
        return;
    }
    // A merge commit from July 2026 that is on main. If this clone is too shallow to see it,
    // skip the reachability half rather than fail every PR; CI unshallows for this job.
    if !git_cat_file("8e94f6c") {
        eprintln!("skipping reachability: clone does not contain origin/main history (#756)");
        return;
    }

    let mut missing = Vec::new();
    let mut unidentified = Vec::new();
    for ent in std::fs::read_dir(repo_root().join("docs/bench")).unwrap() {
        let path = ent.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        let Some(commit) = json_string_field(&raw, "commit") else {
            continue;
        };
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !git_cat_file(commit) {
            missing.push(format!("{name}: {commit}"));
        }
        if commit.is_empty() {
            unidentified.push(name);
        }
    }
    assert!(
        unidentified.is_empty(),
        "empty commit fields: {unidentified:?}"
    );
    assert!(
        missing.is_empty(),
        "docs/bench artefacts cite commits git cannot see (#756): {missing:?}"
    );
}
