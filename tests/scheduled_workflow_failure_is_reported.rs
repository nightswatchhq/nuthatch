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
//! **And the `needs:` edge is asserted, because `failure()` observes ancestors and nothing else.**
//! A reporter job with the right `uses:` and the right `if:` but no `needs:` has no ancestor whose
//! failure `failure()` could report, so it is simply skipped on the exact scheduled failure it exists
//! for. The gate therefore requires every other job in a scheduled workflow to be a transitive
//! ancestor of the reporter: a job added later that can fail without the reporter observing it is
//! the same silent red as the one this file is about. Found in review, after the paragraph above
//! had already been written - the parts were present, the guard was armed, and the wire between the
//! failing job and the reporter was still unchecked.
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

/// Every job in the workflow's `jobs:` mapping, as `(key, lines of its block)`.
///
/// A job's block runs from its key up to the next key at the same indentation. Everything below is
/// asserted against *these* lines rather than the whole file, so a `uses:`, an `if:` or a `needs:`
/// sitting in a different job cannot vouch for this one. Walking the `jobs:` mapping rather than
/// searching for a key anywhere in the file also means the reporter has to *be* a job, not merely a
/// key somebody spelled the same way further down.
fn jobs(stripped: &str) -> Vec<(String, Vec<&str>)> {
    let mut lines = stripped.lines().peekable();
    if !lines.by_ref().any(|l| l == "jobs:") {
        return Vec::new();
    }
    // The indentation of the first job key is the indentation of every job key.
    let Some(indent) = lines
        .peek()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
    else {
        return Vec::new();
    };
    let mut out: Vec<(String, Vec<&str>)> = Vec::new();
    for line in lines {
        let this = line.len() - line.trim_start().len();
        if line.trim().is_empty() || this > indent {
            if let Some((_, block)) = out.last_mut() {
                block.push(line);
            }
            continue;
        }
        if this < indent {
            // A top-level key after `jobs:` ends the mapping.
            break;
        }
        match line.trim().strip_suffix(':') {
            Some(key) => out.push((key.to_owned(), Vec::new())),
            None => break,
        }
    }
    out
}

/// The jobs a block `needs:`, in all three YAML spellings: `needs: [a, b]`, `needs: a`, and a
/// block sequence of `- a` lines under a bare `needs:`.
fn needs_of(block: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_seq = false;
    for line in block {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("needs:") {
            let rest = rest.trim();
            in_seq = rest.is_empty();
            if let Some(list) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                out.extend(
                    list.split(',')
                        .map(|j| j.trim().to_owned())
                        .filter(|j| !j.is_empty()),
                );
            } else if !rest.is_empty() {
                out.push(rest.to_owned());
            }
            continue;
        }
        if in_seq {
            match t.strip_prefix("- ") {
                Some(job) => out.push(job.trim().to_owned()),
                None => in_seq = false,
            }
        }
    }
    out
}

/// Every job whose failure `failure()` in `job` can observe: its `needs:`, transitively.
fn ancestors(all: &[(String, Vec<&str>)], job: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut todo = vec![job.to_owned()];
    while let Some(j) = todo.pop() {
        let Some((_, block)) = all.iter().find(|(k, _)| *k == j) else {
            continue;
        };
        for n in needs_of(block) {
            if !seen.contains(&n) {
                seen.push(n.clone());
                todo.push(n);
            }
        }
    }
    seen.sort();
    seen
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
        let all = jobs(stripped);
        let reporter = REPORTER_JOB.trim_end_matches(':');
        let Some((_, block)) = all.iter().find(|(k, _)| k == reporter) else {
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
        // `failure()` is true only when an *ancestor* job failed, so the reporter observes exactly
        // its transitive `needs:` and nothing else. No `needs:` means no ancestor and a reporter that
        // is skipped on the one event it exists for; a job outside the `needs:` closure can go red
        // on a scheduled run with the reporter none the wiser, which is #1130 again with one more
        // job in the file.
        let observed = ancestors(&all, reporter);
        for n in &observed {
            if !all.iter().any(|(k, _)| k == n) {
                failures.push(format!(
                    "{name}: the `{REPORTER_JOB}` job needs `{n}`, which is not a job in this \
                     workflow - GitHub would reject the file, and the reporter observes nothing"
                ));
            }
        }
        let unobserved: Vec<&String> = all
            .iter()
            .map(|(k, _)| k)
            .filter(|k| *k != reporter && !observed.contains(k))
            .collect();
        if !unobserved.is_empty() {
            failures.push(format!(
                "{name}: the `{REPORTER_JOB}` job does not `needs:` {} (directly or through \
                 another job), so a scheduled failure there is invisible to it - `failure()` only \
                 sees ancestors",
                unobserved
                    .iter()
                    .map(|k| format!("`{k}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "a scheduled workflow whose failure nobody is told about (#1130):\n  {}",
        failures.join("\n  ")
    );
}
