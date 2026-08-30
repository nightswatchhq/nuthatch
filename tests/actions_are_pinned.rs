//! #829 - every third-party Action runs with this repository's credentials, so it must be pinned to
//! an immutable commit SHA rather than a tag anyone can move.
//!
//! The pinning itself was a morning's work. **This file is the part that matters**, because #913's
//! finding is that a gate written once and never exercised stops watching: a single unpinned `uses:`
//! added in six months' time restores the whole hazard, silently, and nothing else in the repo would
//! notice. A tag is not a version - it is a pointer, and the person who can move it is not
//! necessarily the person who published it.
//!
//! Deliberately a **shape** check rather than an allowlist of known-good SHAs. An allowlist is the
//! `CONFIG_SOURCES` failure mode that `doc_command_check.rs` and `analytics.rs` both call out: it
//! needs hand-editing on every legitimate bump, so it either blocks Dependabot or gets relaxed until
//! it means nothing. "Forty hex characters" needs no maintenance and cannot drift.

use std::path::{Path, PathBuf};

fn workflows() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows")
}

/// Every `uses:` in every workflow, as `(file, line number, reference)`.
fn every_uses() -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let dir = workflows();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .collect();
    files.sort();
    assert!(
        !files.is_empty(),
        "no workflow files found under {} - this gate would pass vacuously, which is exactly the \
         failure #913 is about",
        dir.display()
    );
    for f in files {
        let name = f.file_name().unwrap().to_string_lossy().to_string();
        for (i, line) in std::fs::read_to_string(&f).unwrap().lines().enumerate() {
            // Both spellings. A step in a list is `- uses: …`; a bare `uses:` appears under a
            // job key. Matching only the second found 15 of 53 on the day this was written, and the
            // floor assertion below is the only reason that undercount was ever noticed.
            let t = line.trim().trim_start_matches("- ").trim_start();
            if let Some(rest) = t.strip_prefix("uses:") {
                out.push((name.clone(), i + 1, rest.trim().to_string()));
            }
        }
    }
    out
}

/// A local composite action (`./.github/actions/foo`) is this repository's own code and carries no
/// third-party trust; a `docker://` reference is pinned by digest elsewhere. Everything else is
/// somebody else's code running with our credentials.
fn is_third_party(reference: &str) -> bool {
    !reference.starts_with('.') && !reference.starts_with("docker://")
}

fn pinned_sha(reference: &str) -> Option<&str> {
    let after_at = reference.split('#').next()?.trim().rsplit_once('@')?.1;
    (after_at.len() == 40 && after_at.chars().all(|c| c.is_ascii_hexdigit())).then_some(after_at)
}

#[test]
fn every_third_party_action_is_pinned_to_a_commit_sha() {
    let all = every_uses();
    let third_party: Vec<_> = all
        .iter()
        .filter(|(_, _, r)| is_third_party(r))
        .cloned()
        .collect();

    // Without this the test passes on an empty set - the "mechanism cannot fail" shape from #913.
    assert!(
        third_party.len() >= 20,
        "expected the workflows to use a substantial number of third-party actions, found {}. \
         Either the scan stopped matching reality or the workflows changed shape; check the parser \
         before relaxing this number.",
        third_party.len()
    );

    let unpinned: Vec<String> = third_party
        .iter()
        .filter(|(_, _, r)| pinned_sha(r).is_none())
        .map(|(f, n, r)| format!("  {f}:{n}  {r}"))
        .collect();

    assert!(
        unpinned.is_empty(),
        "these actions are pinned to a mutable ref, so whoever can move that tag can run code with \
         this repository's release credentials (#829). Pin each to a full 40-character commit SHA \
         and keep the version as a trailing comment:\n\n\
         \x20   uses: <owner>/<action>@<40-char-commit-sha> # <version>\n\n\
         Resolve the SHA with:  gh api repos/<owner>/<repo>/commits/<tag> --jq .sha\n\n{}",
        unpinned.join("\n")
    );
}

/// A pin with no version comment is unreadable: nobody can tell `a5f673d0` from `7c8d7d13` at a
/// glance, so a reviewer cannot see that a bump went from v4 to v5. The comment is what makes the
/// pin auditable, and #829 asks for it explicitly.
#[test]
fn every_pin_records_the_version_it_pins() {
    let missing: Vec<String> = every_uses()
        .into_iter()
        .filter(|(_, _, r)| is_third_party(r) && pinned_sha(r).is_some())
        .filter(|(_, _, r)| !r.contains('#'))
        .map(|(f, n, r)| format!("  {f}:{n}  {r}"))
        .collect();
    assert!(
        missing.is_empty(),
        "a bare SHA is not reviewable - add the version it pins as a trailing comment so a bump is \
         legible in a diff:\n{}",
        missing.join("\n")
    );
}

/// Dependabot is the other half of #829. Immutable pins never advance on their own, so without a
/// bot raising them an action sits at a vulnerable commit forever and the hardening has simply
/// traded one risk for another.
#[test]
fn dependabot_can_advance_the_pins() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/dependabot.yml");
    let s = std::fs::read_to_string(&p).unwrap_or_else(|e| {
        panic!(
            "{} is missing ({e}). Pinning without it leaves every action frozen at whatever commit \
             was current the day it was pinned.",
            p.display()
        )
    });
    assert!(
        s.contains("github-actions"),
        "dependabot.yml exists but does not watch the `github-actions` ecosystem, so the pins this \
         repo just took on will never be raised:\n{s}"
    );
}

/// **#928.** `dtolnay/rust-toolchain` is the one action here whose *ref* is the configuration: the
/// `1.95.0` branch bakes the version into its own script and declares no `toolchain` input, while the
/// `nightly` branch defaults to `nightly`. Two branches of one repository, two different compilers.
///
/// Pinning both to SHAs (#829) removed the only signal that says which is which, and Dependabot walked
/// straight into it: its first run proposed rewriting the **nightly** pin to the **1.95.0** SHA with
/// the comment still reading `# nightly`. The fuzz job would have installed stable, `cargo-fuzz`
/// needs nightly, and nothing in the diff looked wrong.
///
/// `dependabot.yml` now ignores that dependency, but an ignore rule is a request rather than a
/// mechanism - somebody can still make this edit by hand. This is the mechanism: two distinct
/// toolchains must keep two distinct SHAs, checked offline, no network.
#[test]
fn the_two_rust_toolchain_pins_stay_distinct() {
    let mut by_comment: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for (file, line, r) in every_uses() {
        let Some(rest) = r.strip_prefix("dtolnay/rust-toolchain@") else {
            continue;
        };
        let (sha, comment) = rest.split_once('#').unwrap_or_else(|| {
            panic!("{file}:{line}: a rust-toolchain pin with no version comment is unreadable: {r}")
        });
        by_comment
            .entry(comment.trim().to_string())
            .or_default()
            .insert(sha.trim().to_string());
    }
    assert!(
        by_comment.len() >= 2,
        "expected at least two distinct rust-toolchain lines (a pinned release and nightly); found \
         {by_comment:?}. If the fuzz job stopped using nightly, that is the bug this guards."
    );
    for (comment, shas) in &by_comment {
        assert_eq!(
            shas.len(),
            1,
            "`# {comment}` is pinned to more than one SHA: {shas:?}"
        );
    }
    let all: std::collections::BTreeSet<&String> = by_comment.values().flatten().collect();
    assert_eq!(
        all.len(),
        by_comment.len(),
        "two rust-toolchain comments share a SHA, so two different toolchains resolve to the same \
         action branch - one of them is not the compiler its comment claims (#928):\n{by_comment:?}"
    );
}
