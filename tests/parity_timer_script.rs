//! #1713 - the daily parity timer on Helsinki, and #1718 - the head mode it runs beside the sealed one.
//!
//! The wrapper (`deploy/parity/nuthatch-parity.sh`) runs against a fake `lodestar-parity.sh` whose
//! exit codes and output come from the state directory, and a fake `curl` that records each Discord
//! post. The head-mode tests run the real `scripts/lodestar-parity.sh` against a fake `curl` for the
//! nest and a `file://` gateway for the subgraph, so only the pin rule is exercised.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn set_mode(p: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// `<mode>-exits` holds the exit codes of successive runs, consumed one per call; `<mode>-out` is
/// printed after the PIN line.
const FAKE_PARITY: &str = r#"#!/usr/bin/env bash
mode=${PARITY_MODE:-sealed}
echo "$mode" >> "$FAKE_STATE/calls"
[ -n "${GRAPH_API_KEY:-}" ] || { echo "FAIL GRAPH_API_KEY is unset" >&2; exit 1; }
codes=$(cat "$FAKE_STATE/$mode-exits" 2>/dev/null || echo 0)
set -- $codes
code=${1:-0}
shift || true
echo "$*" > "$FAKE_STATE/$mode-exits"
if [ "$mode" = head ]; then pin=1000; else pin=900; fi
echo "PIN mode=$mode block=$pin version=4.2.0"
[ -f "$FAKE_STATE/$mode-out" ] && cat "$FAKE_STATE/$mode-out"
exit "$code"
"#;

const FAKE_CURL: &str = r#"#!/usr/bin/env bash
data=""
while [ $# -gt 0 ]; do
  case "$1" in -d|--data|--data-binary) data=$2; shift ;; esac
  shift
done
[ -f "$FAKE_STATE/curl-fail" ] && exit 22
printf '%s\n' "$data" >> "$FAKE_STATE/posts"
"#;

struct Rig {
    _root: tempfile::TempDir,
    path: String,
    state: PathBuf,
    logs: PathBuf,
    env_file: PathBuf,
    hook: PathBuf,
    parity: PathBuf,
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let fakes = root.path().join("fakes");
    let state = root.path().join("state");
    let logs = root.path().join("logs");
    for d in [&fakes, &state] {
        std::fs::create_dir_all(d).unwrap();
    }
    let curl = fakes.join("curl");
    std::fs::write(&curl, FAKE_CURL).unwrap();
    make_executable(&curl);
    let parity = root.path().join("lodestar-parity.sh");
    std::fs::write(&parity, FAKE_PARITY).unwrap();
    make_executable(&parity);
    let env_file = root.path().join("parity.env");
    std::fs::write(&env_file, "GRAPH_API_KEY=k3y\n").unwrap();
    set_mode(&env_file, 0o600);
    let hook = root.path().join("hook");
    std::fs::write(&hook, "https://discord.example/api/webhooks/1/x\n").unwrap();
    set_mode(&hook, 0o600);
    let path = format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap());
    Rig {
        _root: root,
        path,
        state,
        logs,
        env_file,
        hook,
        parity,
    }
}

fn exits(r: &Rig, mode: &str, codes: &str) {
    std::fs::write(r.state.join(format!("{mode}-exits")), codes).unwrap();
}

fn out(r: &Rig, mode: &str, body: &str) {
    std::fs::write(r.state.join(format!("{mode}-out")), body).unwrap();
}

fn run(r: &Rig) -> (i32, String) {
    let o = Command::new("bash")
        .arg(root().join("deploy/parity/nuthatch-parity.sh"))
        .env("PATH", &r.path)
        .env("FAKE_STATE", &r.state)
        .env("PARITY_SCRIPT", &r.parity)
        .env("PARITY_ENV_FILE", &r.env_file)
        .env("PARITY_WEBHOOK_FILE", &r.hook)
        .env("PARITY_LOG_DIR", &r.logs)
        .env("PARITY_HEAD_RETRY_SECS", "0")
        .env_remove("GRAPH_API_KEY")
        .output()
        .expect("run nuthatch-parity.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.code().unwrap_or(-1), text)
}

fn posts(r: &Rig) -> Vec<String> {
    std::fs::read_to_string(r.state.join("posts"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn runs_tsv(r: &Rig) -> String {
    std::fs::read_to_string(r.logs.join("runs.tsv")).unwrap_or_default()
}

fn logs_matching(r: &Rig, suffix: &str) -> Vec<PathBuf> {
    std::fs::read_dir(&r.logs)
        .map(|d| {
            d.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.to_string_lossy().ends_with(suffix))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_disagreement_posts_one_line_naming_the_pin_version_and_comparison() {
    let r = rig();
    exits(&r, "sealed", "1");
    out(
        &r,
        "sealed",
        "subgraph _meta.block.number=950 pin=900\nlodestar_allocations nest=10 subgraph_isLegacy_false=11 DIFF\nFAIL subgraph comparison failed\n",
    );
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    let p = posts(&r);
    assert_eq!(p.len(), 1, "one post, got {p:?}\n{text}");
    let line = &p[0];
    assert!(line.starts_with("{\"content\":\""), "{line}");
    assert!(line.contains("sealed"), "{line}");
    assert!(line.contains("900"), "the pin is missing: {line}");
    assert!(line.contains("4.2.0"), "the version is missing: {line}");
    assert!(
        line.contains("lodestar_allocations nest=10 subgraph_isLegacy_false=11 DIFF"),
        "the failing comparison is missing: {line}"
    );
    assert_eq!(logs_matching(&r, "-sealed.log").len(), 1, "{text}");
    assert!(
        runs_tsv(&r).contains("\tsealed\t1\t900\t4.2.0"),
        "{}",
        runs_tsv(&r)
    );
}

#[test]
fn known_differences_are_logged_and_not_posted() {
    let r = rig();
    exits(&r, "sealed", "2");
    exits(&r, "head", "2");
    let (code, text) = run(&r);
    assert_eq!(code, 2, "{text}");
    assert!(posts(&r).is_empty(), "exit 2 posted: {:?}", posts(&r));
    let tsv = runs_tsv(&r);
    assert!(tsv.contains("\tsealed\t2\t900"), "{tsv}");
    assert!(tsv.contains("\thead\t2\t1000"), "{tsv}");
}

#[test]
fn both_modes_run_and_are_recorded() {
    let r = rig();
    let (code, text) = run(&r);
    assert_eq!(code, 0, "{text}");
    let calls = std::fs::read_to_string(r.state.join("calls")).unwrap();
    assert_eq!(calls, "sealed\nhead\n");
    assert_eq!(logs_matching(&r, "-sealed.log").len(), 1);
    assert_eq!(logs_matching(&r, "-head.log").len(), 1);
    let log = std::fs::read_to_string(&logs_matching(&r, "-head.log")[0]).unwrap();
    assert!(log.contains("PIN mode=head block=1000"), "{log}");
    assert!(log.contains("exit 0"), "the exit code is not kept: {log}");
    assert!(posts(&r).is_empty());
}

#[test]
fn a_head_that_cannot_be_reached_is_retried_then_recorded_unposted() {
    let r = rig();
    exits(&r, "head", "3 3 3");
    let (code, text) = run(&r);
    assert_eq!(code, 3, "{text}");
    assert!(posts(&r).is_empty(), "{:?}", posts(&r));
    let tsv = runs_tsv(&r);
    assert_eq!(tsv.matches("\thead\t3\t").count(), 3, "{tsv}");
    assert!(
        text.contains("could not be brought to the same head"),
        "{text}"
    );
}

#[test]
fn a_head_reached_on_retry_counts() {
    let r = rig();
    exits(&r, "head", "3 0");
    let (code, text) = run(&r);
    assert_eq!(code, 0, "{text}");
    let tsv = runs_tsv(&r);
    assert!(tsv.contains("\thead\t3\t"), "{tsv}");
    assert!(tsv.contains("\thead\t0\t"), "{tsv}");
}

#[test]
fn a_head_disagreement_pages_as_well() {
    let r = rig();
    exits(&r, "head", "1");
    out(
        &r,
        "head",
        "  total_rewards 10/11 epochs agree from 1195 DIFF\n",
    );
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    let p = posts(&r);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains("head"), "{}", p[0]);
    assert!(p[0].contains("1000"), "{}", p[0]);
    assert!(
        p[0].contains("total_rewards 10/11 epochs agree from 1195 DIFF"),
        "{}",
        p[0]
    );
}

/// An exit the script does not define is not a pass and not a known difference.
#[test]
fn an_undefined_exit_status_pages() {
    let r = rig();
    exits(&r, "sealed", "137");
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    assert_eq!(posts(&r).len(), 1, "{text}");
    assert!(posts(&r)[0].contains("137"), "{:?}", posts(&r));
}

#[test]
fn a_missing_key_pages_rather_than_passing() {
    let r = rig();
    std::fs::write(&r.env_file, "NEST_URL=http://127.0.0.1:8107\n").unwrap();
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("GRAPH_API_KEY"), "{text}");
    let p = posts(&r);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains("GRAPH_API_KEY"), "{}", p[0]);
    assert!(
        !r.state.join("calls").exists(),
        "the comparison ran without a key"
    );
}

#[test]
fn a_readable_env_file_is_refused() {
    let r = rig();
    set_mode(&r.env_file, 0o644);
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("0600"), "{text}");
    assert!(!r.state.join("calls").exists());
}

#[test]
fn the_key_never_reaches_the_log_or_the_post() {
    let r = rig();
    exits(&r, "sealed", "1");
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    assert_eq!(posts(&r).len(), 1, "{text}");
    assert!(!text.contains("k3y"), "{text}");
    for p in posts(&r) {
        assert!(!p.contains("k3y"), "{p}");
    }
    for l in logs_matching(&r, ".log") {
        let body = std::fs::read_to_string(&l).unwrap();
        assert!(!body.contains("k3y"), "{body}");
    }
}

#[test]
fn an_unreadable_webhook_is_loud() {
    let r = rig();
    exits(&r, "sealed", "1");
    std::fs::remove_file(&r.hook).unwrap();
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("cannot read"), "{text}");
}

#[test]
fn a_failed_post_is_loud() {
    let r = rig();
    exits(&r, "sealed", "1");
    std::fs::write(r.state.join("curl-fail"), "").unwrap();
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("could not post"), "{text}");
}

#[test]
fn a_quote_in_the_failing_line_still_makes_valid_json() {
    let r = rig();
    exits(&r, "sealed", "1");
    out(
        &r,
        "sealed",
        "FAIL nest at \"http://127.0.0.1:8107\" did not answer /ready\\n\n",
    );
    let (code, text) = run(&r);
    assert_eq!(code, 1, "{text}");
    let p = posts(&r);
    assert_eq!(p.len(), 1, "{p:?}");
    let v: serde_json::Value = serde_json::from_str(&p[0]).expect("the post is not JSON");
    assert!(
        v["content"]
            .as_str()
            .unwrap()
            .contains("\"http://127.0.0.1:8107\""),
        "{v}"
    );
}

#[test]
fn old_logs_are_pruned_and_new_ones_kept() {
    let r = rig();
    std::fs::create_dir_all(&r.logs).unwrap();
    let old = r.logs.join("20200101T000000Z-sealed.log");
    std::fs::write(&old, "exit 0\n").unwrap();
    let t = Command::new("touch")
        .args(["-t", "202001010000"])
        .arg(&old)
        .status()
        .unwrap();
    assert!(t.success());
    let (code, text) = run(&r);
    assert_eq!(code, 0, "{text}");
    assert!(!old.exists(), "a log past the retention survived");
    assert_eq!(logs_matching(&r, "-sealed.log").len(), 1);
}

// --- the head mode in scripts/lodestar-parity.sh (#1718) ---

const NEST_CURL: &str = r#"#!/usr/bin/env bash
for a in "$@"; do
  case "$a" in
    */ready) printf '{"ready":true,"version":"4.2.0","last_block":1000,"sealed_through":900}'; exit 0 ;;
    */sql) printf '{"columns":["n"],"rows":[{"n":5}]}'; exit 0 ;;
  esac
done
exit 7
"#;

const NETWORK_SG: &str = "DZz4kDTdmzWLWsV373w2bSmoar3umKKH9y82SUKr5qmp";

fn parity(mode: Option<&str>, gateway_body: &str, extra: &[(&str, &str)]) -> (i32, String) {
    let root_dir = tempfile::tempdir().unwrap();
    let fakes = root_dir.path().join("fakes");
    std::fs::create_dir_all(&fakes).unwrap();
    let curl = fakes.join("curl");
    std::fs::write(&curl, NEST_CURL).unwrap();
    make_executable(&curl);
    let gw = root_dir.path().join("gw");
    let sg = gw.join(format!("api/k/subgraphs/id/{NETWORK_SG}"));
    std::fs::create_dir_all(sg.parent().unwrap()).unwrap();
    std::fs::write(&sg, gateway_body).unwrap();
    let mut cmd = Command::new("bash");
    cmd.arg(root().join("scripts/lodestar-parity.sh"))
        .env(
            "PATH",
            format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap()),
        )
        .env("NEST_URL", "http://127.0.0.1:9")
        .env("GRAPH_API_KEY", "k")
        .env("GRAPH_GATEWAY", format!("file://{}", gw.display()))
        .env_remove("PINNED_BLOCK")
        .env_remove("PARITY_MODE");
    if let Some(m) = mode {
        cmd.env("PARITY_MODE", m);
    }
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let o = cmd.output().expect("run lodestar-parity.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.code().unwrap_or(-1), text)
}

fn meta(block: u64) -> String {
    format!("{{\"data\":{{\"_meta\":{{\"block\":{{\"number\":{block}}}}}}}}}")
}

#[test]
fn head_mode_pins_at_last_block() {
    let (_, text) = parity(Some("head"), &meta(950), &[]);
    assert!(
        text.contains("PIN mode=head block=1000 version=4.2.0"),
        "{text}"
    );
}

#[test]
fn sealed_mode_still_pins_at_sealed_through() {
    let (_, text) = parity(None, &meta(850), &[]);
    assert!(
        text.contains("PIN mode=sealed block=900 version=4.2.0"),
        "{text}"
    );
}

/// The subgraph behind the nest's head is a comparison that did not happen: not a pass, and not a
/// disagreement worth a page.
#[test]
fn head_mode_with_the_subgraph_behind_is_not_measured() {
    let (code, text) = parity(Some("head"), &meta(950), &[]);
    assert_eq!(code, 3, "{text}");
    assert!(text.contains("NOT MEASURED"), "{text}");
}

/// The gateway can answer `_meta` from one indexer and the pinned query from another that is behind.
#[test]
fn head_mode_with_an_indexer_short_of_the_pin_is_not_measured() {
    let body = r#"{"errors":[{"message":"Failed to decode `block.number` value: `subgraph QmX has only indexed up to block number 990 and data for block number 1000 is therefore not yet available`"}]}"#;
    let (code, text) = parity(Some("head"), body, &[]);
    assert_eq!(code, 3, "{text}");
}

/// The sealed pin is hours old, so a subgraph short of it is a fault, as it always was.
#[test]
fn sealed_mode_with_the_subgraph_behind_still_fails() {
    let (code, text) = parity(None, &meta(850), &[]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("below pin 900"), "{text}");
}

#[test]
fn head_mode_refuses_an_explicit_pin() {
    let (code, text) = parity(Some("head"), &meta(2000), &[("PINNED_BLOCK", "850")]);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("PINNED_BLOCK cannot be combined with PARITY_MODE=head"),
        "{text}"
    );
}

#[test]
fn an_unknown_mode_is_refused() {
    let (code, text) = parity(Some("tip"), &meta(2000), &[]);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("PARITY_MODE"), "{text}");
}
