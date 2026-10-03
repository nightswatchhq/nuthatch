//! #1729 - `deploy-nest.sh roll` must roll the binary systemd runs, and say so only when it has.
//!
//! On 2026-10-02 it printed `ok nuthatch-dips -> 4.1.1` and the unit came back on 4.1.0: three units
//! on the box take ExecStart from a drop-in, the script edited the unit file, and it confirmed by
//! grepping the file it had just written. These tests run the real script against a fake
//! `systemctl` that resolves ExecStart the way systemd does (last file that sets it wins) and a fake
//! `curl` whose `/ready` reports the version of whatever binary was running at the last restart.
//!
//! #1750 - with `--smoke <file>` the roll then runs each statement against `/sql`, and on any refusal
//! puts the unit back on its previous binary. The fake answers `/sql` from `sql-<version>` in the
//! state directory: `<substring> <mode>` fails any statement containing the substring.

use std::path::{Path, PathBuf};
use std::process::Command;

fn script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts/deploy-nest.sh")
}

const FAKE_SYSTEMCTL: &str = r#"#!/usr/bin/env bash
# show -p ExecStart --value U | daemon-reload | restart U | is-active U
effective() {
  local u=$1 line="" f
  for f in "$NUTHATCH_UNIT_DIR/$u.service" "$NUTHATCH_UNIT_DIR/$u.service.d"/*.conf; do
    [ -f "$f" ] || continue
    while IFS= read -r l; do
      case "$l" in ExecStart=*) line=${l#ExecStart=} ;; esac
    done < "$f"
  done
  echo "$line"
}
case "$1" in
  show) u=$5; cmd=$(effective "$u"); bin=${cmd%% *}; echo "{ path=$bin ; argv[]=$cmd ; }" ;;
  daemon-reload) ;;
  restart) cmd=$(effective "$2"); echo "${cmd%% *}" > "$FAKE_STATE/running" ;;
  is-active) echo active ;;
esac
"#;

const FAKE_CURL: &str = r#"#!/usr/bin/env bash
bin=$(cat "$FAKE_STATE/running")
url="" q=""
for a in "$@"; do
  case "$a" in http://*) url=$a ;; q=*) q=${a#q=} ;; esac
done
case "$url" in */sql*)
  v=$("$bin" --version | awk '{print $2}')
  printf '%s\n' "$q" >> "$FAKE_STATE/sql-log"
  mode=ok
  if [ -f "$FAKE_STATE/sql-$v" ]; then
    read -r pat m < "$FAKE_STATE/sql-$v"
    case "$q" in *"$pat"*) mode=$m ;; esac
  fi
  case "$mode" in
    ok) printf '{"columns":["count"],"rows":[[7]]}\n200' ;;
    error) printf '{"error":"Catalog Error: Table lodestar_epochs does not exist"}\n200' ;;
    oom) printf '{"columns":["msg"],"rows":[["Out of Memory Error: failed to allocate 2.0 GiB"]]}\n200' ;;
    http500) printf 'internal failure\n500' ;;
  esac
  exit 0 ;;
esac
# A restart that came back on the old process, as a no-op roll does.
[ -f "$FAKE_STATE/stale" ] && bin=$(dirname "$bin")/nuthatch-4.1.0
v=$("$bin" --version | awk '{print $2}')
if [ -f "$FAKE_STATE/spaced" ]; then printf '{ "version" : "%s", "ready" : true, "last_block" : 42 }' "$v"; elif [ -f "$FAKE_STATE/serve-only" ]; then printf '{"version":"%s","ready":true}' "$v"; else printf '{"version":"%s","ready":true,"last_block":42}' "$v"; fi
"#;

fn fake_binary(dir: &Path, version: &str) -> PathBuf {
    let p = dir.join(format!("nuthatch-{version}"));
    std::fs::write(&p, format!("#!/bin/sh\necho nuthatch {version}\n")).unwrap();
    make_executable(&p);
    p
}

fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Rig {
    _root: tempfile::TempDir,
    bin: PathBuf,
    units: PathBuf,
    path: String,
    state: PathBuf,
}

/// A unit whose ExecStart names 4.1.0, optionally overridden by a drop-in, with 4.1.1 installed.
fn a_box(with_drop_in: bool) -> Rig {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    let units = root.path().join("units");
    let fakes = root.path().join("fakes");
    let state = root.path().join("state");
    for d in [&bin, &units, &fakes, &state] {
        std::fs::create_dir_all(d).unwrap();
    }
    let old = fake_binary(&bin, "4.1.0");
    fake_binary(&bin, "4.1.1");
    let exec = format!(
        "{} dev --dir /opt/nest --listen 127.0.0.1:8104",
        old.display()
    );
    std::fs::write(
        units.join("dips.service"),
        format!("[Service]\nExecStart={exec}\n"),
    )
    .unwrap();
    if with_drop_in {
        std::fs::create_dir_all(units.join("dips.service.d")).unwrap();
        std::fs::write(
            units.join("dips.service.d/rpc-graphops.conf"),
            format!("[Service]\nExecStart=\nExecStart={exec} --window 25000\n"),
        )
        .unwrap();
    }
    std::fs::write(state.join("running"), old.display().to_string()).unwrap();
    for (name, body) in [("systemctl", FAKE_SYSTEMCTL), ("curl", FAKE_CURL)] {
        let p = fakes.join(name);
        std::fs::write(&p, body).unwrap();
        make_executable(&p);
    }
    let path = format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap());
    Rig {
        _root: root,
        bin,
        units,
        path,
        state,
    }
}

fn roll(b: &Rig) -> (bool, String) {
    roll_with(b, &[])
}

fn roll_with(b: &Rig, extra: &[&str]) -> (bool, String) {
    let out = Command::new("bash")
        .arg(script())
        .args(["roll", "dips", "4.1.1"])
        .args(extra)
        .env("PATH", &b.path)
        .env("NUTHATCH_BIN_DIR", &b.bin)
        .env("NUTHATCH_UNIT_DIR", &b.units)
        .env("FAKE_STATE", &b.state)
        .env("ROLL_POLL_SECS", "0")
        .output()
        .expect("run deploy-nest.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn running(b: &Rig) -> String {
    std::fs::read_to_string(b.state.join("running"))
        .unwrap()
        .trim()
        .to_string()
}

#[test]
fn a_roll_edits_the_drop_in_that_overrides_the_unit_file() {
    let b = a_box(true);
    let (ok, out) = roll(&b);
    assert!(ok, "the roll failed:\n{out}");
    assert!(
        running(&b).ends_with("nuthatch-4.1.1"),
        "the script reported success and the unit restarted on {}:\n{out}",
        running(&b)
    );
    let drop_in =
        std::fs::read_to_string(b.units.join("dips.service.d/rpc-graphops.conf")).unwrap();
    assert!(drop_in.contains("nuthatch-4.1.1 dev"), "{drop_in}");
    assert!(out.contains("via rpc-graphops.conf"), "{out}");
}

/// The read-only legacy unit runs `serve`, whose /ready has no last_block. Reading a field that is
/// absent must not end the script under `set -e` and `pipefail`.
#[test]
fn a_serve_only_unit_with_no_last_block_still_rolls() {
    let b = a_box(false);
    std::fs::write(b.state.join("serve-only"), "").unwrap();
    let (ok, out) = roll(&b);
    assert!(ok, "the roll died on a /ready with no last_block:\n{out}");
    assert!(running(&b).ends_with("nuthatch-4.1.1"), "{out}");
}

/// JSON may put whitespace around the colon; a field reader that cannot see past it would reject a
/// healthy restart.
#[test]
fn a_ready_answer_with_spaced_json_is_read() {
    let b = a_box(true);
    std::fs::write(b.state.join("spaced"), "").unwrap();
    let (ok, out) = roll(&b);
    assert!(ok, "a spaced /ready answer was not read:\n{out}");
    assert!(out.contains("last_block 42 -> 42"), "{out}");
}

/// The QoS nest listens on a tailnet address, not loopback. The port lookup matched only
/// 127.0.0.1 and the empty match ended the script silently under `pipefail`.
#[test]
fn a_unit_listening_on_a_non_loopback_address_rolls() {
    let b = a_box(false);
    let unit = b.units.join("dips.service");
    let raw = std::fs::read_to_string(&unit).unwrap();
    std::fs::write(&unit, raw.replace("127.0.0.1:8104", "100.83.44.63:8124")).unwrap();
    let (ok, out) = roll(&b);
    assert!(ok, "the roll died on a non-loopback listen address:\n{out}");
    assert!(running(&b).ends_with("nuthatch-4.1.1"), "{out}");
}

#[test]
fn a_roll_without_a_drop_in_edits_the_unit_file() {
    let b = a_box(false);
    let (ok, out) = roll(&b);
    assert!(ok, "the roll failed:\n{out}");
    assert!(running(&b).ends_with("nuthatch-4.1.1"), "{out}");
    assert!(out.contains("via dips.service"), "{out}");
}

/// A restart that comes back still serving the old version must fail the roll, whatever the files
/// say: `/ready` is the proof, and readiness alone flips true across a no-op restart too.
#[test]
fn a_roll_whose_restart_still_serves_the_old_version_fails() {
    let b = a_box(true);
    std::fs::write(b.state.join("stale"), "").unwrap();
    let (ok, out) = roll(&b);
    assert!(
        !ok && out.contains("reports version 4.1.0 on /ready, not 4.1.1"),
        "the script must refuse a roll /ready does not confirm, got ok={ok}:\n{out}"
    );
}

const SMOKE: &str = "-- the panels Lodestar sends\n\nSELECT count(*) FROM lodestar_indexer_daily\n  -- indented comment\nSELECT count(*) FROM lodestar_epochs\n";

fn smoke_file(b: &Rig) -> String {
    let p = b.state.join("smoke.sql");
    std::fs::write(&p, SMOKE).unwrap();
    p.display().to_string()
}

fn sql_log(b: &Rig) -> Vec<String> {
    std::fs::read_to_string(b.state.join("sql-log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// 4.1.1 refuses any statement containing `statement` in the given way; 4.1.0 answers everything.
fn new_version_refuses(b: &Rig, statement: &str, mode: &str) {
    std::fs::write(b.state.join("sql-4.1.1"), format!("{statement} {mode}\n")).unwrap();
}

#[test]
fn a_smoke_that_passes_runs_every_statement_and_keeps_the_roll() {
    let b = a_box(true);
    let smoke = smoke_file(&b);
    let (ok, out) = roll_with(&b, &["--smoke", &smoke]);
    assert!(ok, "a passing smoke failed the roll:\n{out}");
    assert!(running(&b).ends_with("nuthatch-4.1.1"), "{out}");
    assert_eq!(
        sql_log(&b),
        [
            "SELECT count(*) FROM lodestar_indexer_daily",
            "SELECT count(*) FROM lodestar_epochs"
        ],
        "comments and blank lines are not statements, and every statement runs:\n{out}"
    );
}

fn assert_rolled_back(b: &Rig, ok: bool, out: &str, why: &str) {
    assert!(!ok, "a refused smoke must fail the roll:\n{out}");
    assert!(
        out.contains("SELECT count(*) FROM lodestar_epochs") && out.contains(why),
        "the failure must name the statement and the error ({why}):\n{out}"
    );
    assert!(
        running(b).ends_with("nuthatch-4.1.0"),
        "the unit was left on {} after its smoke failed:\n{out}",
        running(b)
    );
    let drop_in =
        std::fs::read_to_string(b.units.join("dips.service.d/rpc-graphops.conf")).unwrap();
    assert!(
        drop_in.contains("nuthatch-4.1.0 dev") && !drop_in.contains("4.1.1"),
        "the file systemd runs must name the previous binary again:\n{drop_in}"
    );
    assert!(out.contains("rolled back to 4.1.0"), "{out}");
}

#[test]
fn a_smoke_answered_with_an_error_rolls_the_unit_back() {
    let b = a_box(true);
    let smoke = smoke_file(&b);
    new_version_refuses(&b, "lodestar_epochs", "error");
    let (ok, out) = roll_with(&b, &["--smoke", &smoke]);
    assert_rolled_back(&b, ok, &out, "Catalog Error");
}

#[test]
fn a_smoke_answered_out_of_memory_rolls_the_unit_back() {
    let b = a_box(true);
    let smoke = smoke_file(&b);
    new_version_refuses(&b, "lodestar_epochs", "oom");
    let (ok, out) = roll_with(&b, &["--smoke", &smoke]);
    assert_rolled_back(&b, ok, &out, "Out of Memory");
}

#[test]
fn a_smoke_answered_with_an_http_error_rolls_the_unit_back() {
    let b = a_box(true);
    let smoke = smoke_file(&b);
    new_version_refuses(&b, "lodestar_epochs", "http500");
    let (ok, out) = roll_with(&b, &["--smoke", &smoke]);
    assert_rolled_back(&b, ok, &out, "HTTP 500");
}

/// A smoke file with no statements would examine nothing and report all clear.
#[test]
fn a_smoke_file_with_no_statements_is_refused_before_the_unit_is_touched() {
    let b = a_box(true);
    let p = b.state.join("empty.sql");
    std::fs::write(&p, "-- nothing here\n\n").unwrap();
    let (ok, out) = roll_with(&b, &["--smoke", &p.display().to_string()]);
    assert!(!ok && out.contains("no statements"), "{out}");
    assert!(running(&b).ends_with("nuthatch-4.1.0"), "{out}");
}
