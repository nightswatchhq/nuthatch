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

/// A scratch directory for the fixtures these tests hand the script.
///
/// **Not `<manifest>/target`**, which is where they used to go (#1128). `CARGO_MANIFEST_DIR` is the
/// *worktree*, and `<worktree>/target` does not exist whenever `CARGO_TARGET_DIR` points elsewhere -
/// which is exactly what building from a git worktree against a shared target directory does. The
/// write then failed with `NotFound` and these two tests were **red locally and green in CI**, for a
/// reason with nothing to do with the code under test. Two stray files were left in the shared target
/// as evidence.
///
/// Nothing about these fixtures wants to outlive the test, so a tempdir is the right home for them
/// and the path question stops existing.
fn fixtures() -> tempfile::TempDir {
    tempfile::tempdir().expect("a scratch directory for the fixtures")
}

fn dry_run_in(dir: &Path, commits: Option<&Path>) -> String {
    dry_run_with_base(dir, commits, None)
}

/// As above, plus what the base already carries since the previous release (#1201).
fn dry_run_with_base(
    dir: &Path,
    commits: Option<&Path>,
    base_commits: Option<(&Path, &str)>,
) -> String {
    let base = dir.join("base");
    std::fs::write(&base, "main").expect("write base");
    let diff = dir.join("diff");
    std::fs::write(&diff, "diff --git a/x b/x\n+one line\n").expect("write diff");
    let mut c = Command::new("python3");
    c.arg(root().join("scripts/pr-review.py"))
        .arg("--diff")
        .arg(&diff)
        .args(["--title", "release: 3.1.0", "--dry-run"]);
    c.arg("--base-file").arg(&base);
    if let Some(p) = commits {
        c.arg("--commits-file").arg(p);
    }
    if let Some((p, range)) = base_commits {
        c.arg("--base-commits-file")
            .arg(p)
            .args(["--base-range", range]);
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
    let dir = fixtures();
    let commits = dir.path().join("commits");
    std::fs::write(
        &commits,
        "4bc5402e9 security: wasmtime 46.0.3 - RUSTSEC-2026-0268 and RUSTSEC-2026-0269\n\
         726fd5134 fix(#1042): a cold start has not looked yet\n",
    )
    .expect("write commits");

    let prompt = dry_run_in(dir.path(), Some(&commits));
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
    // The count must be the number of COMMITS, not of lines in the file. The base branch was once
    // appended to that same list, so a two-commit PR announced three - a number stated to be
    // trustworthy and wrong by one. It travels in its own file now.
    assert!(
        prompt.contains("Base branch:"),
        "the base branch is not stated, so the reviewer cannot tell what the range is against:\n{prompt}"
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
    let dir = fixtures();
    let prompt = dry_run_in(dir.path(), None);
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

/// #1201: **a release branch cut after its fix merged carries the version bump and nothing else.**
///
/// #1056 taught the reviewer this branch's commits, which fixed the case where the fix was an
/// earlier commit *on the branch*. It cannot help when the fix landed on `main` first: the 3.6.1
/// release PR was rejected three times running, at certainty 100, for "the claimed sealing fix is
/// absent from the diff" - while that fix sat one commit below it on the base it was cut from.
///
/// So the reviewer is told what the base already carries since the previous release, and that list
/// is what makes "absent from this diff" distinguishable from "absent from this release".
#[test]
fn the_reviewer_is_told_what_the_base_already_carries() {
    let dir = fixtures();
    let commits = dir.path().join("commits");
    std::fs::write(
        &commits,
        "cad64d1b release: 3.6.1 - a sparse nest stopped sealing\n",
    )
    .expect("write commits");
    let base_commits = dir.path().join("base_commits");
    std::fs::write(
        &base_commits,
        "df792e80 fix(#1199): bound a held finalized range by a per-chain seal span\n         947a8f41 fix(#1196): probe the endpoints we ship\n",
    )
    .expect("write base commits");

    let prompt = dry_run_with_base(
        dir.path(),
        Some(&commits),
        Some((&base_commits, "v3.6.0...main (2 of 2 commits listed)")),
    );
    assert!(
        prompt.contains("per-chain seal span"),
        "the base's commits are not in the prompt, so the reviewer would again report a fix missing \
         from a release that contains it:\n{prompt}"
    );
    assert!(
        prompt.contains("v3.6.0...main (2 of 2 commits listed)"),
        "the range is not stated, so a truncated compare cannot be told from a whole one - the \
         compare endpoint caps its commit array at 250 and this is the only place that shows \
         it:\n{prompt}"
    );
    // The two lists answer different questions and must not be run together: one is what this pull
    // request wrote, the other is what merging it ships.
    assert!(
        prompt.contains("Commits on this branch (1)") && prompt.contains("release: 3.6.1"),
        "the branch's own commit list was lost or merged into the base list:\n{prompt}"
    );
    assert!(
        prompt.contains("```diff"),
        "the diff is gone from the prompt:\n{prompt}"
    );
}

/// The instruction has to travel with the data. A reviewer handed a second commit list and no rule
/// about it can still write "the implementation is not in this diff" and be red about it.
#[test]
fn the_reviewer_is_told_that_a_release_may_legitimately_hold_no_implementation() {
    let script = std::fs::read_to_string(root().join("scripts/pr-review.py")).expect("read");
    let idx = script.find("SYSTEM = ").expect("the system prompt");
    let system = &script[idx..script[idx..].find("SCHEMA = ").expect("end of system") + idx];
    assert!(
        system.contains("A release is a range"),
        "the system prompt never tells the reviewer that a release contains what is on its base, so \
         the second commit list is data it has no rule for (#1201)"
    );
    assert!(
        system.contains("not in this diff"),
        "the system prompt does not name the exact sentence this exists to stop the reviewer \
         writing about a release"
    );
}

/// The workflow has to actually fetch and pass it. The script growing an argument nothing supplies
/// would satisfy every assertion above while the reviewer sees no more than it did before.
#[test]
fn the_workflow_fetches_and_passes_the_base_range() {
    let wf = std::fs::read_to_string(root().join(".github/workflows/pr-review.yml")).expect("read");
    // Comments stripped: this file explains the fault at length beside the fix, and an assertion
    // that matches the prose passes with the code deleted - a fault this repo has found before.
    let code: String = wf
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("--base-commits-file pr.base_commits"),
        "the workflow does not pass the base's commit list, so the script's new argument is never \
         supplied and nothing changes for the reviewer"
    );
    assert!(
        code.contains("--base-range"),
        "the range is not passed, so the prompt cannot say what the base list covers"
    );
    assert!(
        code.contains("/compare/") && code.contains("total_commits"),
        "the base range is not fetched from the compare endpoint with its true total, so a range \
         capped at 250 commits would read as a complete one"
    );
    assert!(
        code.contains("releases/latest"),
        "nothing determines the previous release, so there is no range to compare against"
    );
}

/// Both scores are 0-100 by contract, so the schema must say so. `"type": "integer"` alone accepts
/// `150` and renders it as a certainty.
#[test]
fn both_scores_are_bounded_not_merely_typed() {
    let script = std::fs::read_to_string(root().join("scripts/pr-review.py")).expect("read");
    let count = script.matches("\"maximum\": 100").count();
    assert!(
        count >= 2,
        "expected both `confidence` and `certainty` to be bounded 0-100; found {count} bound(s)"
    );
}

/// A commit list that could not be fetched must **stop** the review, not render as "(not supplied)"
/// and let a verdict be produced from the diff alone - which is the failure this whole PR removes.
#[test]
fn a_failed_commit_fetch_stops_the_review() {
    let wf = std::fs::read_to_string(root().join(".github/workflows/pr-review.yml")).expect("read");
    let code: String = wf
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !code.contains("|| : > pr.commits"),
        "the commit fetch still fails soft into an empty file, which reads as `(not supplied)`"
    );
    assert!(
        code.contains("[ -s pr.commits ]"),
        "nothing checks the commit list is non-empty, so an empty result still reaches the model"
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
    // ...and it must not swallow its own failure. If the PATCH fails silently the check stays red
    // from the run that was cancelled - precisely the state this handler exists to prevent - and
    // nobody ever learns why. A guard that hides its own failure is not a guard.
    let handler = wf
        .split("a superseded review is neutral")
        .nth(1)
        .expect("the cancellation step")
        .split("- name:")
        .next()
        .expect("step body");
    assert!(
        !handler.contains("--input - <<<\"$payload\" >/dev/null || true"),
        "the neutralising PATCH swallows its failure with `|| true`:\n{handler}"
    );
    assert!(
        handler.contains("::error::"),
        "the cancellation handler does not report a failure to neutralise:\n{handler}"
    );
}
