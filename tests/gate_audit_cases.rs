//! #913 - the audit's own cases must not rot.
//!
//! `scripts/gate-audit.sh` mutates the artefact each gate guards and asserts the gate goes red. Its
//! weakness is the one it was built to find: when a doc is rewritten and a case's needle stops
//! matching, that case **silently stops covering its gate** and the audit still reports success for
//! everything else. That is #913 shape 1, reappearing inside the tool built to detect shape 1.
//!
//! So `--check` verifies every case still has a target, cheaply - no `cargo test` runs - and this
//! wires it into CI. The full mutating run is slow (one `cargo test` per case) and belongs in the
//! nightly sweep; keeping the *drift* check on every push is what stops the audit decaying between
//! sweeps.

use std::process::Command;

/// The complete case list in `scripts/gate-audit.sh`, pinned by name.
///
/// Pinned rather than counted (#974). Both directions are asserted: a missing case means coverage
/// was deleted, an extra case means this list went stale. Either way it is one edit here in the same
/// commit, which is the point - the audit's own coverage should not be able to change quietly.
const EXPECTED_CASES: &[&str] = &[
    "ivm_claims",
    "ivm_claims_views",
    "rfc_index_status",
    "doc_command_check",
    "required_checks",
    "skill_refs_authored",
    "skill_refs_stale",
    "tape_clean",
    "actions_pinned_tag",
    "actions_pinned_toolchain",
];

/// `--check` prints `  <name>   target present in <file>` for each live case.
trait LiveCaseLine {
    fn strip_suffix_once_target(&self) -> Option<String>;
}
impl LiveCaseLine for str {
    fn strip_suffix_once_target(&self) -> Option<String> {
        let at = self.find("target present in")?;
        let name = self[..at].trim();
        (!name.is_empty()).then(|| name.to_string())
    }
}

#[test]
fn every_gate_audit_case_still_has_a_target() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("scripts/gate-audit.sh");
    assert!(
        script.exists(),
        "{} is missing - the #913 audit is the only thing that checks the gates can fail",
        script.display()
    );

    let out = Command::new("bash")
        .arg(&script)
        .arg("--check")
        .current_dir(root)
        .output()
        .expect("run gate-audit.sh --check");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // #974: this used to be `targets >= 6` against ten declared cases, which meant four could be
    // deleted from `CASES` outright with this still green. A floor cannot see deletion; only the
    // complete set can. Drift (a needle that stops matching) was already caught - it produces a SKIP
    // and the script exits nonzero - so the floor was a weak backstop behind a check that worked,
    // guarding the one direction that was actually open.
    let expected: Vec<&str> = EXPECTED_CASES.to_vec();
    let live: Vec<String> = text
        .lines()
        .filter_map(|l| l.trim().strip_suffix_once_target())
        .collect();
    let missing: Vec<&&str> = expected
        .iter()
        .filter(|e| !live.iter().any(|l| l == *e))
        .collect();
    assert!(
        missing.is_empty(),
        "these gate-audit cases are declared in the test but no longer live in the script: \
         {missing:?}. A case that vanishes takes its gate's coverage with it, silently - which is \
         #913 shape 1 inside the tool built to detect it. Do not delete a case; fix it.\n{text}"
    );
    let extra: Vec<&String> = live
        .iter()
        .filter(|l| !expected.iter().any(|e| *e == l.as_str()))
        .collect();
    assert!(
        extra.is_empty(),
        "the script has gained cases this test does not know about: {extra:?}. Good - add them to \
         `EXPECTED_CASES` in the same commit, so the set stays pinned rather than becoming a floor \
         again.\n{text}"
    );

    assert!(
        out.status.success(),
        "gate-audit.sh --check failed. A SKIP means the artefact a case mutates has changed shape, \
         so that gate is no longer audited at all - fix the case, do not delete it:\n{text}"
    );
}
