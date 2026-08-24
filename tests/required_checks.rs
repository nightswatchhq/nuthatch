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
fn required_checks_file_names_the_ten_including_the_signature() {
    let n = names();
    assert!(
        n.iter().any(|s| s == "reviewed-by signature"),
        "the file that forgot this is how the job ran red onto scenery for a week: {n:?}"
    );
    assert!(n.iter().any(|s| s == "fmt · clippy · test"), "{n:?}");
    assert_eq!(
        n.len(),
        10,
        "ten contexts on main when this was written: {n:?}"
    );
}

#[test]
fn live_endpoints_keys_on_json_fields_not_prose() {
    let yml = include_str!("../.github/workflows/live-endpoints.yml");
    assert!(
        !yml.contains("retry \"getLogs window"),
        "the live-endpoints gate must not grep doctor's prose (#716)"
    );
    assert!(
        !yml.contains("retry \"archive depth"),
        "archive check must not grep doctor's prose (#716)"
    );
    assert!(yml.contains("--json"), "doctor --json is the contract");
    assert!(yml.contains("max_window"), "retry keys on max_window");
}
