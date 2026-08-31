//! #1051 - the Tier-B runner's own honesty, gated in CI.
//!
//! `eval/run-tier-b.py` had two defects that were invisible because nothing exercised them:
//!
//!  * a scoring failure - unreachable URL, timeout, HTTP error, a server that never came up -
//!    left the row set empty and scored the question **failed**, so a broken scorer published a
//!    schema-valid **0/15** with nothing to say why. That is the one way a fabricated-looking
//!    number could arrive entirely by accident, in a file whose premise is that published numbers
//!    are real;
//!  * no failing query was recorded, so the published zero could not say *whether* the agent
//!    invented a table name, tripped the `value`/`value_dec` big-int footgun the fixture exists to
//!    probe, or fell over the `"from"`/`"to"` reserved words.
//!
//! The runner is Python and CI's required check is `cargo test`, so this is the bridge. It is a
//! **normal test**: the self-test needs no key, no nest, no model and no network, so there is no
//! reason for it to sit behind `--ignored` where nothing would run it.

#[test]
fn the_tier_b_runner_passes_its_own_self_test() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = root.join("eval/run-tier-b.py");
    assert!(script.exists(), "{} is missing", script.display());

    let out = std::process::Command::new("python3")
        .arg(&script)
        .arg("--self-test")
        .output()
        .expect("run python3; it is present on every supported CI image and dev box");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    // Assert on the *reported* result as well as the status. A self-test that stopped running its
    // checks would exit 0 having proved nothing, which is the same shape as the defects above.
    assert!(
        stdout.contains("self-test: PASS"),
        "the runner's self-test did not pass:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
    assert!(
        out.status.success(),
        "the runner's self-test printed PASS but exited {:?}:\n{stdout}",
        out.status.code()
    );

    // Each of the four checks must actually have run. Without this the assertion above is satisfied
    // by a self-test that silently stopped exercising a property - and both defects it guards were
    // themselves things nothing exercised.
    for probe in [
        "unreachable scorer raises",
        "shape mismatch raises",
        "report without final_query is refused",
        "report with final_query is accepted",
    ] {
        assert!(
            stdout.contains(probe),
            "the self-test no longer exercises {probe:?}; it cannot pass for the right reason:\n{stdout}"
        );
    }
}
