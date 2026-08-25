//! #845 - the drift check must refuse to report success when it has compared nothing.
//!
//! `scripts/check-required-contexts.sh` is the only thing that compares `.github/required-checks.txt`
//! against the protection GitHub actually enforces. Before this it was invoked by no workflow, no
//! test and no documented command - and would have exited 0 without a token anyway, so wiring it up
//! carelessly would have produced a permanently green step that read nothing.
//!
//! The script derives its repo root from its own location, so each test builds a throwaway root with
//! its own `scripts/` and `.github/` and runs the real script inside it. No network: every case here
//! is one the script must decide before it would reach the API.

use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// A throwaway repo root holding the real script and the given required-checks file.
fn root_with(contexts: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
    std::fs::create_dir_all(dir.path().join(".github")).unwrap();
    std::fs::copy(
        manifest().join("scripts/check-required-contexts.sh"),
        dir.path().join("scripts/check-required-contexts.sh"),
    )
    .unwrap();
    std::fs::write(dir.path().join(".github/required-checks.txt"), contexts).unwrap();
    dir
}

fn run(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new("bash")
        .arg(root.join("scripts/check-required-contexts.sh"))
        .args(args)
        // Strip both, so a developer's ambient token cannot make these tests reach the network.
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .output()
        .expect("bash must be on PATH");
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.code().unwrap_or(-1), s)
}

const TEN: &str = "# comment\nfmt · clippy · test\nbuild release\nreviewed-by signature\n";

#[test]
fn no_token_is_a_failure_not_a_pass() {
    let root = root_with(TEN);
    let (code, out) = run(root.path(), &[]);
    assert_eq!(code, 1, "a tokenless run must fail:\n{out}");
    assert!(out.contains("FAIL"), "{out}");
    assert!(
        !out.contains("match live protection"),
        "must not claim a comparison it did not make:\n{out}"
    );
}

#[test]
fn the_failure_names_a_recovery_path_that_exists_in_the_failing_state() {
    // A required-check failure message that says "see the docs" is unreachable advice at the moment
    // it is needed. This one has to name the Actions permission, because the default GITHUB_TOKEN
    // cannot read branch protection and that is the non-obvious half.
    let root = root_with(TEN);
    let (_, out) = run(root.path(), &[]);
    assert!(out.contains("administration: read"), "{out}");
    assert!(out.contains("--offline"), "{out}");
    assert!(out.contains("gh auth token"), "{out}");
}

#[test]
fn offline_passes_but_says_it_compared_nothing() {
    let root = root_with(TEN);
    let (code, out) = run(root.path(), &["--offline"]);
    assert_eq!(code, 0, "--offline is a deliberate, allowed mode:\n{out}");
    assert!(
        out.contains("NOT compared") && out.contains("not a drift check"),
        "an offline pass must not read like a drift check that passed:\n{out}"
    );
}

#[test]
fn a_file_missing_the_signature_context_fails_before_any_network() {
    let root = root_with("fmt · clippy · test\nbuild release\n");
    let (code, out) = run(root.path(), &["--offline"]);
    assert_eq!(code, 1, "the signature context is not optional:\n{out}");
    assert!(out.contains("reviewed-by signature"), "{out}");
}

#[test]
fn an_unknown_flag_is_refused_rather_than_ignored() {
    // A typo'd flag silently ignored would run the network path when the caller asked for offline,
    // or vice versa. Both are worse than a usage error.
    let root = root_with(TEN);
    let (code, out) = run(root.path(), &["--ofline"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("usage"), "{out}");
}

/// The committed script and the committed list are the ones CI runs, so assert against them rather
/// than only against fixtures - a fixture-only suite cannot see the real file drifting.
#[test]
fn the_committed_list_still_names_the_signature_context() {
    let listed: Vec<String> =
        std::fs::read_to_string(manifest().join(".github/required-checks.txt"))
            .expect("required-checks.txt")
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
    assert!(
        listed.iter().any(|l| l == "reviewed-by signature"),
        "{listed:?}"
    );
    let _: PathBuf = manifest().to_path_buf();
}
