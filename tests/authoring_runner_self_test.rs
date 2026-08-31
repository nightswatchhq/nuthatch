//! #1050 - the authoring runner's own honesty, gated in CI.
//!
//! `eval/run-authoring.py` is Python and CI's required check is `cargo test`, so this is the bridge.
//! It carries #1051's lesson from its first commit rather than after a published zero: that issue is
//! what the alternative looks like - two defects in the RFC-0016 runner that were invisible
//! precisely because nothing exercised them, one of which would have let a broken scorer publish a
//! schema-valid 0/15.
//!
//! A **normal test**: the self-test needs no key, no nest, no model and no network, so there is no
//! reason for it to sit behind `--ignored` where nothing would run it.

#[test]
fn the_authoring_runner_passes_its_own_self_test() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("eval/run-authoring.py");
    assert!(script.exists(), "{} is missing", script.display());

    let out = std::process::Command::new("python3")
        .arg(&script)
        .arg("--self-test")
        .output()
        .expect("run python3; it is present on every supported CI image and dev box");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // The reported result as well as the status: a self-test that stopped running its checks would
    // exit 0 having proved nothing, which is the same shape as the defects it guards.
    assert!(
        stdout.contains("self-test: PASS"),
        "the authoring runner's self-test did not pass:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        out.status.success(),
        "the authoring self-test printed PASS but exited {:?}:\n{stdout}",
        out.status.code()
    );

    // Each property must actually have been exercised. Without this the assertion above is
    // satisfied by a self-test that quietly stopped checking something.
    for probe in [
        "the scenario carries RFC-0017's three criteria",
        "the sealed-through criterion agrees with the chain's finality pin",
        "an unreachable scorer raises",
        "a DECIMAL string equals its number",
        "a wrong total does not pass",
        "a missing column does not pass",
        "the builder skill the subject is given exists",
        "an unindexed nest scores rather than aborting",
        "a malformed /sql shape is fatal (rows is null)",
        "the reap check has a live child to kill (normal exit)",
        "a backgrounded child is reaped (normal exit)",
        "a backgrounded child is reaped (timeout)",
    ] {
        assert!(
            stdout.contains(probe),
            "the authoring self-test no longer exercises {probe:?}; it cannot pass for the right \
             reason:\n{stdout}"
        );
    }
}
