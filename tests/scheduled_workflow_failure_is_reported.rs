//! #1130 - **a scheduled workflow can be red indefinitely with nothing to surface it.**
//!
//! `required contexts match protection` failed on `main` every day from 2026-08-28 to 2026-09-02.
//! Six consecutive red runs, and the streak was found only while retiring an unrelated gate
//! (#1094). Nobody was ignoring it. There was no surface on which it appeared:
//!
//! - Required contexts gate **pull requests**. A `schedule:` run attaches its check to no PR, so
//!   marking the context required would not have surfaced it. Worth stating plainly, because
//!   "make it required" is the intuitive fix and it demonstrably does not work here.
//! - A red `main` shows on the commit only for the workflow the push triggered. A nightly run
//!   lands against the same SHA later and is not what the branch badge reflects.
//!
//! So the remedy is that each scheduled workflow files an issue when it fails, and this file is the
//! part that keeps working. The same reasoning as `actions_are_pinned.rs`: the wiring was an hour,
//! but a scheduled workflow added in six months' time with no reporter restores the whole hazard
//! silently, and nothing else in the repo would notice.
//!
//! **It checks the guard, not just the presence of the job.** A reporter wired with an `if:` that
//! can never be true would satisfy a presence check while surfacing nothing - the failure mode
//! recorded in this repo's own notes about tests that pass with the mechanism removed. So both the
//! `failure()` and the `github.event_name == 'schedule'` halves are asserted.
//!
//! **And all three are asserted *inside the reporter job's own block*.** The first version of this
//! file looked for the job name, the `uses:` and the guard independently anywhere in the workflow,
//! which proves none of them belong together: a correctly-guarded reporter job whose action step had
//! been moved into some other job would have passed. Worth recording rather than quietly fixing,
//! because it is the structural form of the very mistake the paragraph above is about - the parts
//! were all present and the association between them was unchecked.
//!
//! **Comments are stripped before anything is matched.** Two gates in this repo have passed while
//! the guarded thing was deleted, because they matched the explanatory prose in a comment rather
//! than the code. The prose above says `schedule:` and `report-scheduled-failure` several times;
//! without the strip, this file could vouch for itself.

use std::path::PathBuf;

fn workflows_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows")
}

fn reporter_action() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/actions/report-scheduled-failure/action.yml")
}

/// The path a workflow references the local reporter by. A composite action in this repository, so
/// it carries no third-party trust and needs no SHA pin (see `actions_are_pinned.rs`).
const REPORTER_USES: &str = "uses: ./.github/actions/report-scheduled-failure";
const REPORTER_JOB: &str = "report-scheduled-failure:";

/// YAML with `#` comments removed.
///
/// Only a `#` at the start of a line or following whitespace opens a comment, which leaves a `#`
/// inside a quoted value alone - a URL fragment or a `jq` filter would otherwise be truncated and
/// the truncation would be invisible.
fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|line| {
            let bytes = line.as_bytes();
            let mut cut = line.len();
            for (i, c) in line.char_indices() {
                if c == '#' && (i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
                    cut = i;
                    break;
                }
            }
            line[..cut].trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every workflow file, as `(file name, comment-stripped contents)`.
fn workflows() -> Vec<(String, String)> {
    let dir = workflows_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no workflow files under {} - this gate would pass vacuously, which is the failure it \
         exists to prevent",
        dir.display()
    );
    files
        .into_iter()
        .map(|f| {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let body = std::fs::read_to_string(&f)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", f.display()));
            (name, strip_comments(&body))
        })
        .collect()
}

/// A workflow runs on a schedule if a bare `schedule:` key appears. Deliberately a shape check and
/// not a list of known scheduled workflows: an allowlist needs hand-editing every time one is added
/// and so drifts into meaninglessness, the `CONFIG_SOURCES` failure mode this repo has hit before.
fn is_scheduled(stripped: &str) -> bool {
    stripped.lines().any(|l| l.trim() == "schedule:")
}

/// The lines of one named job, from its key up to the next key at the same indentation.
///
/// Everything below is asserted against *these* lines rather than the whole file, so a `uses:` or an
/// `if:` sitting in a different job cannot vouch for this one.
fn job_block<'a>(stripped: &'a str, job_key: &str) -> Option<Vec<&'a str>> {
    let mut lines = stripped.lines();
    let indent = lines
        .by_ref()
        .find(|l| l.trim() == job_key)
        .map(|l| l.len() - l.trim_start().len())?;
    let mut out = Vec::new();
    for line in lines {
        if line.trim().is_empty() {
            out.push(line);
            continue;
        }
        let this = line.len() - line.trim_start().len();
        // A sibling key ends the block. A deeper line, or a list item under it, is still ours.
        if this <= indent {
            break;
        }
        out.push(line);
    }
    Some(out)
}

#[test]
fn the_reporter_action_exists() {
    let p = reporter_action();
    assert!(
        p.is_file(),
        "{} is missing, so every reporter job below references an action that cannot load",
        p.display()
    );
}

#[test]
fn every_scheduled_workflow_reports_its_failures() {
    let all = workflows();
    let scheduled: Vec<&(String, String)> = all.iter().filter(|(_, s)| is_scheduled(s)).collect();

    // The floor. Zero means the detector stopped recognising `schedule:`, not that the repo stopped
    // scheduling anything, and a silent zero here is the same class of bug as the one being gated.
    assert!(
        !scheduled.is_empty(),
        "found no scheduled workflows among {} files - the `schedule:` detector has broken, \
         because this repo does schedule work",
        all.len()
    );

    let mut failures = Vec::new();
    for (name, stripped) in &scheduled {
        let Some(block) = job_block(stripped, REPORTER_JOB) else {
            failures.push(format!(
                "{name}: runs on a schedule but has no `{REPORTER_JOB}` job"
            ));
            continue;
        };
        // A step in a list is `- uses: …`; a bare `uses:` appears under a job key. Both spellings,
        // for the reason `actions_are_pinned.rs` gives: matching only the second undercounted 15 of
        // 53 there, and it was a floor assertion that noticed.
        if !block
            .iter()
            .any(|l| l.trim().trim_start_matches("- ").trim_start() == REPORTER_USES)
        {
            failures.push(format!(
                "{name}: the `{REPORTER_JOB}` job does not itself `{REPORTER_USES}`, so it \
                 surfaces nothing - a `uses:` elsewhere in the file does not count"
            ));
        }
        // Both halves, because either one missing makes the job useless in a different way: without
        // `failure()` it files an issue on every green run, and without the event check it files
        // one for a failing pull request that is already visible on the pull request.
        let armed = block
            .iter()
            .filter_map(|l| l.trim().strip_prefix("if:"))
            .any(|c| c.contains("failure()") && c.contains("github.event_name == 'schedule'"));
        if !armed {
            failures.push(format!(
                "{name}: the `{REPORTER_JOB}` job has no `if:` combining `failure()` with \
                 `github.event_name == 'schedule'`, so the reporter cannot fire on the case it \
                 exists for"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "a scheduled workflow whose failure nobody is told about (#1130):\n  {}",
        failures.join("\n  ")
    );
}
