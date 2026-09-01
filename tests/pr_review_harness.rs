//! #1056 - the review harness's own guarantees, gated in CI.
//!
//! Jules found 26 of 28 real defects last sprint. Both misses trace to what it was *given*, not to
//! the model: it saw a diff and never a commit range, so on the 3.1.0 release PR it reported at high
//! severity that the wasmtime security fix was absent - from a release that contains it, bumped
//! eleven commits earlier. A release is a range.
//!
//! These are claims about a harness, so they get a test rather than a comment. `--dry-run` prints the
//! prompt without a key or a model, which is what makes them checkable at all.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn dry_run(commits: Option<&Path>) -> String {
    let diff = root().join("target/pr-review-test.diff");
    std::fs::write(&diff, "diff --git a/x b/x\n+one line\n").expect("write diff");
    let mut c = Command::new("python3");
    c.arg(root().join("scripts/pr-review.py"))
        .arg("--diff")
        .arg(&diff)
        .args(["--title", "release: 3.1.0", "--dry-run"]);
    if let Some(p) = commits {
        c.arg("--commits-file").arg(p);
    }
    let out = c.output().expect("run pr-review.py");
    assert!(
        out.status.success(),
        "pr-review.py --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_reviewer_is_given_the_commit_range_not_only_the_diff() {
    let commits = root().join("target/pr-review-test.commits");
    std::fs::write(
        &commits,
        "4bc5402e9 security: wasmtime 46.0.3 - RUSTSEC-2026-0268 and RUSTSEC-2026-0269\n\
         726fd5134 fix(#1042): a cold start has not looked yet\n",
    )
    .expect("write commits");

    let prompt = dry_run(Some(&commits));
    assert!(
        prompt.contains("wasmtime 46.0.3"),
        "the commit subjects are not in the prompt, so the reviewer would again report a security \
         fix missing from a release that contains it:\n{prompt}"
    );
    assert!(
        prompt.contains("Commits on this branch (2)"),
        "the count is not stated, and a reviewer cannot tell 'no commits supplied' from 'a branch \
         with no commits':\n{prompt}"
    );
    // The diff must still be there - a prompt that swapped one context for the other would satisfy
    // the assertions above while reviewing nothing.
    assert!(
        prompt.contains("```diff"),
        "the diff is gone from the prompt:\n{prompt}"
    );
}

#[test]
fn a_missing_commit_range_says_so_rather_than_looking_like_an_empty_branch() {
    let prompt = dry_run(None);
    assert!(
        prompt.contains("(not supplied)"),
        "with no commits file the prompt must say so explicitly. Rendering nothing would read as a \
         branch with no commits, which is a different and false claim:\n{prompt}"
    );
}

#[test]
fn every_finding_carries_its_own_certainty_distinct_from_merge_safety() {
    let script = std::fs::read_to_string(root().join("scripts/pr-review.py")).expect("read");
    assert!(
        script.contains("\"certainty\""),
        "findings have no per-finding certainty. `confidence` measures whether the PR is safe to \
         merge, so a correct high-severity finding drives it *down* - the two move together and a \
         reader can use neither to triage. Today's two wrong findings scored 18 and 34, \
         indistinguishable from the correct high-severity ones beside them"
    );
    let idx = script
        .find("\"required\": [\"severity\"")
        .expect("findings required list");
    let required = &script[idx..idx + 200];
    assert!(
        required.contains("certainty"),
        "`certainty` is described but not required, so the model may omit it and the render falls \
         back to `?`:\n{required}"
    );

    let wf = std::fs::read_to_string(root().join(".github/workflows/pr-review.yml")).expect("read");
    assert!(
        wf.contains("certainty \\(.certainty"),
        "certainty is collected but never rendered beside the finding, which is the only place a \
         reader triages"
    );
    assert!(
        wf.contains("merge-safety"),
        "the header still calls the merge-safety score `confidence`, which is the ambiguity this \
         change exists to remove"
    );
}

/// The commit list must not silently stop at a page boundary. `gh pr view --json commits` caps at
/// 100, and a release PR can exceed that - producing a *truncated* history that looks complete,
/// which is the same class of fault as the missing history it replaced.
#[test]
fn the_commit_list_is_paginated_rather_than_capped_at_one_page() {
    let wf = std::fs::read_to_string(root().join(".github/workflows/pr-review.yml")).expect("read");
    assert!(
        wf.contains("--paginate") && wf.contains("/commits"),
        "the commit list is fetched without pagination, so a PR over the page limit is silently          truncated and the reviewer again reasons from an incomplete history"
    );
    // Comments stripped first. The assertion below failed on this file's own explanatory comment,
    // which *mentions* `gh pr view --json commits` while explaining why it is not used - the mirror
    // of the "gate matches its own comment" fault this repo keeps finding. A check must read the
    // code, not the prose about it.
    let code: String = wf
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("--json commits"),
        "still using `gh pr view --json commits`, which caps at 100 with no indication it did"
    );
}

#[test]
fn a_superseded_review_is_neutral_rather_than_a_red_verdict() {
    let wf = std::fs::read_to_string(root().join(".github/workflows/pr-review.yml")).expect("read");
    assert!(
        wf.contains("if: cancelled()"),
        "nothing runs when the review is cancelled, so `Jules approval` keeps the failure the killed \
         run left behind. A red check must mean a finding - during #1054 that state was reported as \
         'still red' repeatedly when a review had simply been killed by the next push"
    );
    assert!(
        wf.contains("conclusion:\"neutral\""),
        "the cancellation handler does not set the check to neutral"
    );
}
