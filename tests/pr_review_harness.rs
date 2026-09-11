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

// ── What is this branch's own work, and what did this reviewer already say ────────────────────

fn dry_run_with(dir: &Path, extra: &[(&str, &str)]) -> String {
    let base = dir.join("base");
    std::fs::write(&base, "main").expect("write base");
    let diff = dir.join("diff2");
    std::fs::write(&diff, "diff --git a/x b/x\n+one line\n").expect("write diff");
    let mut c = Command::new("python3");
    c.arg(root().join("scripts/pr-review.py"))
        .arg("--diff")
        .arg(&diff)
        .args(["--title", "t", "--dry-run"])
        .arg("--base-file")
        .arg(&base);
    for (flag, path) in extra {
        c.arg(flag).arg(path);
    }
    let out = c.output().expect("run pr-review.py");
    assert!(
        out.status.success(),
        "pr-review.py --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A stacked branch that merged `main` in has a diff against its own base carrying all of main, and
/// the reviewer raised a `high` against a file it had shipped at 88/100 hours earlier on the pull
/// request that wrote it. The comparison against the *default* branch is what separates the two.
#[test]
fn the_reviewer_is_told_which_files_are_this_branchs_own() {
    let dir = fixtures();
    let own = dir.path().join("own");
    std::fs::write(&own, "src/port_emit.rs\nsrc/port_report.rs\n").expect("write");
    let prompt = dry_run_with(dir.path(), &[("--own-files-file", own.to_str().unwrap())]);
    assert!(prompt.contains("src/port_emit.rs"), "{prompt}");
    assert!(
        prompt.contains("(2)"),
        "the count must be stated so a truncated list cannot pass for a whole one: {prompt}"
    );
    assert!(
        prompt.contains("came from a merge and is already on the default branch"),
        "the list is useless without the rule that reads it: {prompt}"
    );
}

/// Absent is a fact, not an empty list. An unsupplied file must not read as "this branch changes
/// nothing", which would make every file in the diff look like somebody else's merge.
#[test]
fn an_unsupplied_own_file_list_says_so_rather_than_reading_as_empty() {
    let dir = fixtures();
    let prompt = dry_run_with(dir.path(), &[]);
    assert!(prompt.contains("(not supplied)"), "{prompt}");
}

/// Thirteen passes on one pull request, `ship` at 91/100 on the seventh, then four reversals while
/// its findings were being fixed. Every pass started from nothing.
#[test]
fn the_reviewer_is_given_its_own_previous_verdicts() {
    let dir = fixtures();
    let prior = dir.path().join("prior");
    std::fs::write(
        &prior,
        "<!-- pr-review:luna -->\n### Jules 91/100\nverdict: **ship**\nNo findings.\n",
    )
    .expect("write");
    let prompt = dry_run_with(
        dir.path(),
        &[("--prior-reviews-file", prior.to_str().unwrap())],
    );
    assert!(prompt.contains("verdict: **ship**"), "{prompt}");
    assert!(
        prompt.contains("Your previous reviews of this pull request"),
        "{prompt}"
    );
}

#[test]
fn a_first_pass_is_told_it_is_a_first_pass() {
    let dir = fixtures();
    let prompt = dry_run_with(dir.path(), &[]);
    assert!(
        prompt.contains("this is your first pass"),
        "an empty history must not read as a reviewer who said nothing: {prompt}"
    );
}

/// The rules that read the two new inputs. Without them the inputs are decoration.
#[test]
fn the_prompt_carries_the_rules_that_read_the_new_inputs() {
    let dir = fixtures();
    let prompt = dry_run_with(dir.path(), &[]);
    let _ = prompt;
    let src = std::fs::read_to_string(root().join("scripts/pr-review.py")).expect("read");
    for needle in [
        "already on the default branch is not this pull request's work",
        "may not move from `ship` to `changes-requested`",
        "You cannot run anything",
        "Never state that a named test fails",
        "at 80 however sure it feels",
    ] {
        assert!(
            src.contains(needle),
            "the reviewer is not told `{needle}`, so the input it reads is decoration"
        );
    }
}

/// The workflow has to fetch and pass both, or the script's new arguments are never supplied and
/// nothing changes for the reviewer. Comments stripped: this file explains its faults at length
/// beside the fix, and an assertion matching the prose passes with the code deleted.
#[test]
fn the_workflow_fetches_and_passes_the_new_inputs() {
    let wf = std::fs::read_to_string(root().join(".github/workflows/pr-review.yml")).expect("read");
    let code: String = wf
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("--own-files-file pr.own_files"),
        "the workflow does not pass the branch's own file list"
    );
    assert!(
        code.contains("--prior-reviews-file pr.prior_reviews"),
        "the workflow does not pass the previous reviews"
    );
    assert!(
        code.contains("default_branch") && code.contains("/compare/$default_branch..."),
        "the own-file list must come from a comparison against the DEFAULT branch; comparing \
         against the PR's own base is the thing that goes wrong for a stacked branch"
    );
    assert!(
        code.contains("pr-review:luna") && code.contains("/issues/$PR/comments"),
        "nothing fetches this reviewer's previous comments, so pr.prior_reviews is always empty"
    );
}

/// A large file must not evict the files after it (#1282).
///
/// The budget used to be spent as `diff[:MAX_DIFF_CHARS]`, which is path order, so one oversized file
/// took the whole allowance and every file sorted after it was simply absent from the review. Measured
/// on #1282: an 847,499-character recorded introspection fixture was 73% of a 1,161,244-character diff,
/// the cut landed inside it, and the entire test file that proved the change correct was invisible. The
/// reviewer then raised the same finding four passes running while every reply cited tests it had never
/// been shown - reasoning correctly from a mutilated input.
///
/// Now the largest files are shortened instead, so every smaller file arrives whole.
#[test]
fn one_huge_file_does_not_evict_the_files_after_it() {
    let dir = fixtures();
    let base = dir.path().join("base");
    std::fs::write(&base, "main").expect("write base");

    // Named so git's path order puts the big one first, which is the case that used to lose the rest.
    let big = "x".repeat(900_000);
    let diff = format!(
        "diff --git a/a_huge.json b/a_huge.json\n+{big}\n\
         diff --git a/b_small.rs b/b_small.rs\n+fn the_assertion_that_proves_it() {{}}\n\
         diff --git a/c_small.rs b/c_small.rs\n+fn another_one() {{}}\n"
    );
    let path = dir.path().join("diff-big");
    std::fs::write(&path, &diff).expect("write diff");

    let out = Command::new("python3")
        .arg(root().join("scripts/pr-review.py"))
        .arg("--diff")
        .arg(&path)
        .args(["--title", "a pull request with a recorded fixture in it", "--dry-run"])
        .arg("--base-file")
        .arg(&base)
        .output()
        .expect("run pr-review.py");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let prompt = String::from_utf8_lossy(&out.stdout);

    for want in [
        "b_small.rs",
        "the_assertion_that_proves_it",
        "c_small.rs",
        "another_one",
    ] {
        assert!(
            prompt.contains(want),
            "{want} was evicted by the large file; the reviewer would not see it"
        );
    }
    // The big file is present but shortened, and says so where it was cut, so the reviewer does not
    // read a part for the whole.
    assert!(prompt.contains("a_huge.json"), "the large file is still named");
    assert!(
        prompt.contains("this file was shortened to fit the review budget"),
        "a shortened file must say so inline"
    );
    assert!(
        prompt.len() < 420_000,
        "the budget is still enforced: {} chars",
        prompt.len()
    );
}
