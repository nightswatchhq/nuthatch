//! #715 - the required-check set must exist in the repo, or it drifts in GitHub settings alone.

fn names() -> Vec<String> {
    let raw = include_str!("../.github/required-checks.txt");
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

#[test]
fn required_checks_file_names_the_eleven_including_signature_and_jules() {
    let n = names();
    assert!(
        n.iter().any(|s| s == "reviewed-by signature"),
        "the file that forgot this is how the job ran red onto scenery for a week: {n:?}"
    );
    assert!(n.iter().any(|s| s == "fmt · clippy · test"), "{n:?}");
    assert!(
        n.iter().any(|s| s == "Jules approval"),
        "the external review gate is not optional: {n:?}"
    );
    assert_eq!(
        n.len(),
        11,
        "eleven contexts on main after Jules became a required App check: {n:?}"
    );
}

#[test]
fn live_endpoints_keys_on_json_fields_not_prose() {
    let script = include_str!("../scripts/probe-shipped-defaults.sh");
    assert!(
        !script.contains("retry \"getLogs window"),
        "the probe script must not grep doctor's prose (#716)"
    );
    assert!(
        script.contains("doctor --json"),
        "doctor --json is the contract"
    );
    assert!(script.contains("max_window"), "retry keys on max_window");
}
