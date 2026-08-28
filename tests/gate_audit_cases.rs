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

    // Guard against a vacuous pass: an empty case list would exit 0 and prove nothing.
    let targets = text.matches("target present in").count();
    assert!(
        targets >= 6,
        "the audit only has {targets} live case(s), which is too few to be watching the gate set. \
         Either cases were removed or the script stopped parsing them:\n{text}"
    );

    assert!(
        out.status.success(),
        "gate-audit.sh --check failed. A SKIP means the artefact a case mutates has changed shape, \
         so that gate is no longer audited at all - fix the case, do not delete it:\n{text}"
    );
}
