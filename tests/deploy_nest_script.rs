//! #1729 - `deploy-nest.sh roll` must roll the binary systemd runs, and say so only when it has.
//!
//! On 2026-10-02 it printed `ok nuthatch-dips -> 4.1.1` and the unit came back on 4.1.0: three units
//! on the box take ExecStart from a drop-in, the script edited the unit file, and it confirmed by
//! grepping the file it had just written. These tests run the real script against a fake
//! `systemctl` that resolves ExecStart the way systemd does (last file that sets it wins) and a fake
//! `curl` whose `/ready` reports the version of whatever binary was running at the last restart.

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
# A restart that came back on the old process, as a no-op roll does.
[ -f "$FAKE_STATE/stale" ] && bin=$(dirname "$bin")/nuthatch-4.1.0
v=$("$bin" --version | awk '{print $2}')
printf '{"version":"%s","ready":true,"last_block":42}' "$v"
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
    let out = Command::new("bash")
        .arg(script())
        .args(["roll", "dips", "4.1.1"])
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
