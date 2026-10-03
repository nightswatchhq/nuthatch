//! #1774 - the release gate's copy is refreshed from Helsinki.
//!
//! `deploy/release-gate/helsinki/gate-export.sh` is the forced command on Helsinki: `snapshot`
//! stages a copy of the allocations nest whose redb did not move while it was copied, and a
//! read-only rsync pulls it. `deploy/release-gate/refresh-from-helsinki.sh` is the ThinkPad's
//! `GATE_REFRESH`. Both run here against fakes on PATH: `curl` for the nest's `/ready`, a `cp` that
//! can write the live store mid-copy, and `ssh` and `rsync` that serve a staged tree from disk.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_exe(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    make_executable(p);
}

fn sha256(p: &Path) -> String {
    let o = Command::new("bash")
        .arg("-c")
        .arg(r#"if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1"#)
        .arg("sha")
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8(o.stdout).unwrap().trim().to_string()
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

fn output(o: std::process::Output) -> (i32, String) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    (o.status.code().unwrap_or(-1), text)
}

// --- Helsinki: gate-export.sh ---

/// `/ready` with last_block 1000, or a new one on every call when `moving` exists.
const EXPORT_CURL: &str = r#"#!/usr/bin/env bash
n=$(( $(cat "$FAKE_STATE/curl-calls" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$FAKE_STATE/curl-calls"
lb=1000
[ -f "$FAKE_STATE/moving" ] && lb=$((1000 + n))
printf '{"ready":true,"version":"4.2.0","last_block":%s,"sealed_through":900}' "$lb"
"#;

/// The real cp, after which a copy of the live redb may find the store written under it: `redb-writes`
/// holds how many copies see a commit land, `manifest-writes` how many see a seal replace the manifest.
const EXPORT_CP: &str = r#"#!/usr/bin/env bash
/bin/cp "$@"
src=""
for a in "$@"; do case "$a" in -*) ;; *) [ -n "$src" ] || src=$a ;; esac; done
case "$src" in
  */nuthatch.redb)
    n=$(cat "$FAKE_STATE/redb-writes" 2>/dev/null || echo 0)
    if [ "$n" -gt 0 ]; then printf 'commit' >> "$src"; echo $((n - 1)) > "$FAKE_STATE/redb-writes"; fi
    n=$(cat "$FAKE_STATE/manifest-writes" 2>/dev/null || echo 0)
    if [ "$n" -gt 0 ]; then
      m=$(dirname "$src")/segments/manifest.json
      printf '{"tables":{"t":["b.parquet"]}}' > "$m.tmp" && mv "$m.tmp" "$m"
      echo $((n - 1)) > "$FAKE_STATE/manifest-writes"
    fi ;;
esac
"#;

const EXPORT_RSYNC: &str = r#"#!/usr/bin/env bash
echo "$*" > "$FAKE_STATE/rsync-args"
"#;

struct Helsinki {
    _root: tempfile::TempDir,
    path: String,
    state: PathBuf,
    nest: PathBuf,
    stage: PathBuf,
    conf: PathBuf,
}

fn helsinki() -> Helsinki {
    let root = tempfile::tempdir().unwrap();
    let fakes = root.path().join("fakes");
    let state = root.path().join("state");
    let nest = root.path().join("alloc-nest");
    for d in [
        &fakes,
        &state,
        &nest.join("segments"),
        &nest.join("views"),
        &nest.join(".git"),
    ] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(nest.join("nuthatch.redb"), "redb-bytes").unwrap();
    std::fs::write(nest.join("nuthatch.toml"), "[nest]\n").unwrap();
    std::fs::write(nest.join("semantic.toml"), "").unwrap();
    std::fs::write(nest.join("schema.json"), "{}").unwrap();
    std::fs::write(nest.join("views/a.sql"), "SELECT 1").unwrap();
    std::fs::write(nest.join(".git/HEAD"), "ref").unwrap();
    std::fs::write(
        nest.join("segments/manifest.json"),
        r#"{"tables":{"t":["a.parquet"]}}"#,
    )
    .unwrap();
    std::fs::write(nest.join("segments/a.parquet"), "PAR1").unwrap();
    write_exe(&fakes.join("curl"), EXPORT_CURL);
    write_exe(&fakes.join("cp"), EXPORT_CP);
    write_exe(&fakes.join("rsync"), EXPORT_RSYNC);
    let stage = root.path().join("gate/stage");
    let conf = root.path().join("gate-export.env");
    std::fs::write(
        &conf,
        format!(
            "NEST_DIR={}\nNEST_URL=http://127.0.0.1:8107\nSTAGE={}\n",
            nest.display(),
            stage.display()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap());
    Helsinki {
        _root: root,
        path,
        state,
        nest,
        stage,
        conf,
    }
}

fn export(h: &Helsinki, command: &str) -> (i32, String) {
    output(
        Command::new("bash")
            .arg(root().join("deploy/release-gate/helsinki/gate-export.sh"))
            .env("PATH", &h.path)
            .env("FAKE_STATE", &h.state)
            .env("GATE_EXPORT_CONF", &h.conf)
            .env("GATE_EXPORT_ATTEMPTS", "3")
            .env("GATE_EXPORT_RETRY_SECS", "0")
            .env("SSH_ORIGINAL_COMMAND", command)
            .output()
            .expect("run gate-export.sh"),
    )
}

#[test]
fn a_snapshot_stages_the_nest_with_its_redb_and_says_what_it_took() {
    let h = helsinki();
    let (code, text) = export(&h, "snapshot");
    assert_eq!(code, 0, "{text}");
    assert_eq!(read(&h.stage.join("nuthatch.redb")), "redb-bytes");
    assert_eq!(read(&h.stage.join("views/a.sql")), "SELECT 1");
    assert!(
        h.stage.join("schema.json").exists(),
        "schema.json was left behind"
    );
    assert!(!h.stage.join(".git").exists(), ".git was staged");
    let seg = |p: &Path| {
        std::fs::metadata(p.join("segments/a.parquet"))
            .unwrap()
            .ino()
    };
    assert_eq!(
        seg(&h.stage),
        seg(&h.nest),
        "a sealed segment was copied, not hardlinked"
    );
    assert!(text.contains("sealed_through=900"), "{text}");
    assert!(text.contains("last_block=1000"), "{text}");
    assert!(text.contains("version=4.2.0"), "{text}");
    assert!(
        text.contains(&format!(
            "redb_sha256={}",
            sha256(&h.nest.join("nuthatch.redb"))
        )),
        "{text}"
    );
    assert_eq!(read(&h.stage.join("PROVENANCE")).trim(), text.trim());
}

/// A commit landing while the redb is copied leaves a copy of neither state: it is taken again.
#[test]
fn a_commit_during_the_copy_is_taken_again() {
    let h = helsinki();
    std::fs::write(h.state.join("redb-writes"), "1").unwrap();
    let (code, text) = export(&h, "snapshot");
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("attempt 1: the store moved"), "{text}");
    assert!(text.contains("attempt=2"), "{text}");
    assert_eq!(
        read(&h.stage.join("nuthatch.redb")),
        read(&h.nest.join("nuthatch.redb")),
        "the staged redb is not the store as it now stands"
    );
}

/// A seal replaces the manifest and then commits; a manifest that moved is a different segment set.
#[test]
fn a_seal_during_the_copy_is_taken_again() {
    let h = helsinki();
    std::fs::write(h.state.join("manifest-writes"), "1").unwrap();
    let (code, text) = export(&h, "snapshot");
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("attempt=2"), "{text}");
    assert_eq!(
        read(&h.stage.join("segments/manifest.json")),
        read(&h.nest.join("segments/manifest.json"))
    );
}

#[test]
fn a_store_that_never_holds_still_fails_and_keeps_the_last_stage() {
    let h = helsinki();
    std::fs::create_dir_all(&h.stage).unwrap();
    std::fs::write(h.stage.join("PROVENANCE"), "old\n").unwrap();
    std::fs::write(h.state.join("moving"), "").unwrap();
    let (code, text) = export(&h, "snapshot");
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("did not hold still"), "{text}");
    assert_eq!(read(&h.stage.join("PROVENANCE")), "old\n");
    assert!(!h.stage.with_extension("new").exists());
    assert!(
        !h.stage.with_extension("lock").exists(),
        "the lock was left behind"
    );
}

#[test]
fn the_pull_is_a_read_only_rsync_of_the_stage() {
    let h = helsinki();
    assert_eq!(export(&h, "snapshot").0, 0);
    let cmd = format!(
        "rsync --server --sender -logDtpre.iLsfxCIvu . {}/",
        h.stage.display()
    );
    let (code, text) = export(&h, &cmd);
    assert_eq!(code, 0, "{text}");
    assert_eq!(
        read(&h.state.join("rsync-args")).trim(),
        format!(
            "--server --sender -logDtpre.iLsfxCIvu . {}/",
            h.stage.display()
        )
    );
}

#[test]
fn anything_else_is_refused() {
    let h = helsinki();
    assert_eq!(export(&h, "snapshot").0, 0);
    let s = h.stage.display().to_string();
    for cmd in [
        format!("rsync --server -logDtpre.iLsfxCIvu . {s}/"),
        format!("rsync --server --sender -logDtpre.iLsfxCIvu --remove-source-files . {s}/"),
        format!("rsync --server --sender -logDtpre.iLsfxCIvu . {s}/../"),
        "rsync --server --sender -logDtpre.iLsfxCIvu . /etc/".to_string(),
        format!("rsync --server --sender -logDtpre.iLsfxCIvu;id . {s}/"),
        "cat /etc/passwd".to_string(),
        "snapshot; rm -rf /".to_string(),
        String::new(),
    ] {
        let (code, text) = export(&h, &cmd);
        assert_eq!(code, 1, "{cmd:?} was not refused:\n{text}");
        assert!(text.contains("refused"), "{cmd:?}: {text}");
        assert!(
            !h.state.join("rsync-args").exists(),
            "{cmd:?} reached rsync"
        );
    }
}

#[test]
fn a_pull_before_any_snapshot_is_refused() {
    let h = helsinki();
    let cmd = format!(
        "rsync --server --sender -logDtpre.iLsfxCIvu . {}/",
        h.stage.display()
    );
    let (code, text) = export(&h, &cmd);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("nothing is staged"), "{text}");
}

// --- the ThinkPad: refresh-from-helsinki.sh ---

/// `snapshot` prints `snapshot-out` if present, else the stage's PROVENANCE.
const REFRESH_SSH: &str = r#"#!/usr/bin/env bash
echo "$*" >> "$FAKE_STATE/ssh-args"
for last; do :; done
[ "$last" = snapshot ] || exit 2
[ -f "$FAKE_STATE/ssh-fail" ] && { echo "ssh: connect to host 100.82.188.91 port 22: timed out" >&2; exit 255; }
if [ -f "$FAKE_STATE/snapshot-out" ]; then cat "$FAKE_STATE/snapshot-out"; else cat "$FAKE_STATE/stage/PROVENANCE"; fi
"#;

/// Copies the stage into the last argument. `rsync-fail` fails after a partial copy; `corrupt`
/// damages the redb in transit, `corrupt-manifest` the segment manifest.
const REFRESH_RSYNC: &str = r#"#!/usr/bin/env bash
echo "$*" > "$FAKE_STATE/rsync-args"
for dst; do :; done
mkdir -p "$dst"
if [ -f "$FAKE_STATE/rsync-fail" ]; then
  /bin/cp "$FAKE_STATE/stage/nuthatch.toml" "$dst/"
  echo "rsync error: some files/attrs were not transferred (code 23)" >&2
  exit 23
fi
/bin/cp -R "$FAKE_STATE/stage/." "$dst/"
[ -f "$FAKE_STATE/corrupt" ] && printf 'torn' >> "$dst/nuthatch.redb"
[ -f "$FAKE_STATE/corrupt-manifest" ] && printf 'torn' >> "$dst/segments/manifest.json"
exit 0
"#;

struct ThinkPad {
    _root: tempfile::TempDir,
    path: String,
    state: PathBuf,
    nest: PathBuf,
    key: PathBuf,
}

fn stage_provenance(stage: &Path, sealed: u64) {
    let p = format!(
        "taken_at=2026-10-04T05:00:00Z\nnest_dir=/opt/nuthatch/graph-allocations-nest-next\nversion=4.2.0\nlast_block={}\nsealed_through={sealed}\nredb_sha256={}\nmanifest_sha256={}\nattempt=1\n",
        sealed + 100,
        sha256(&stage.join("nuthatch.redb")),
        sha256(&stage.join("segments/manifest.json"))
    );
    std::fs::write(stage.join("PROVENANCE"), p).unwrap();
}

fn thinkpad() -> ThinkPad {
    let root = tempfile::tempdir().unwrap();
    let fakes = root.path().join("fakes");
    let state = root.path().join("state");
    let stage = state.join("stage");
    let nest = root.path().join("release-gate/alloc-nest");
    for d in [&fakes, &stage.join("segments"), &nest.join("segments")] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(stage.join("nuthatch.redb"), "new-redb").unwrap();
    std::fs::write(stage.join("nuthatch.toml"), "[nest]\n").unwrap();
    std::fs::write(stage.join("segments/manifest.json"), "{}").unwrap();
    stage_provenance(&stage, 200);
    std::fs::write(nest.join("nuthatch.redb"), "old-redb").unwrap();
    std::fs::write(nest.join("nuthatch.toml"), "[nest]\n").unwrap();
    std::fs::write(nest.join("segments/manifest.json"), "{}").unwrap();
    std::fs::write(nest.join("PROVENANCE"), "sealed_through=100\n").unwrap();
    write_exe(&fakes.join("ssh"), REFRESH_SSH);
    write_exe(&fakes.join("rsync"), REFRESH_RSYNC);
    let key = root.path().join("nuthatch-gate");
    std::fs::write(&key, "key").unwrap();
    let path = format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap());
    ThinkPad {
        _root: root,
        path,
        state,
        nest,
        key,
    }
}

fn refresh(t: &ThinkPad) -> (i32, String) {
    refresh_with(t, &[])
}

fn refresh_with(t: &ThinkPad, extra: &[(&str, &str)]) -> (i32, String) {
    let mut cmd = Command::new("bash");
    cmd.arg(root().join("deploy/release-gate/refresh-from-helsinki.sh"))
        .env("PATH", &t.path)
        .env("FAKE_STATE", &t.state)
        .env("GATE_NEST", &t.nest)
        .env("GATE_SSH_KEY", &t.key)
        .env("TMPDIR", t.state.parent().unwrap())
        .env_remove("GATE_LOCK_HELD");
    for (k, v) in extra {
        cmd.env(k, v);
    }
    output(cmd.output().expect("run refresh-from-helsinki.sh"))
}

/// A refresh by hand must not swap the copy out from under a gate that is serving it.
#[test]
fn a_refresh_waits_for_a_gate_holding_the_copy() {
    let t = thinkpad();
    // Detached, so init reaps it when it ends: a zombie child of this test would pass `kill -0`.
    let o = Command::new("sh")
        .arg("-c")
        .arg("sleep 3 >/dev/null 2>&1 & echo $!")
        .output()
        .unwrap();
    let pid = String::from_utf8(o.stdout).unwrap().trim().to_string();
    let lock = t.nest.with_extension("gate-lock.d");
    std::fs::create_dir_all(&lock).unwrap();
    std::fs::write(lock.join("pid"), &pid).unwrap();
    let (code, text) = refresh_with(&t, &[("GATE_LOCK_PORTABLE", "1")]);
    let held_to_the_end = !Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success();
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("waiting"), "{text}");
    assert!(
        held_to_the_end,
        "the copy was swapped while the gate held it"
    );
}

fn unchanged(t: &ThinkPad) {
    assert_eq!(
        read(&t.nest.join("nuthatch.redb")),
        "old-redb",
        "the copy was replaced"
    );
    assert!(
        !t.nest.with_extension("incoming").exists(),
        "a half-pulled copy was left beside it"
    );
}

#[test]
fn a_refresh_swaps_in_the_staged_copy_and_keeps_the_last() {
    let t = thinkpad();
    let (code, text) = refresh(&t);
    assert_eq!(code, 0, "{text}");
    assert_eq!(read(&t.nest.join("nuthatch.redb")), "new-redb");
    assert_eq!(
        read(&t.nest.with_extension("prev").join("nuthatch.redb")),
        "old-redb"
    );
    assert!(text.contains("sealed_through 100 -> 200"), "{text}");
    let ssh = read(&t.state.join("ssh-args"));
    assert!(ssh.contains(&format!("-i {}", t.key.display())), "{ssh}");
    assert!(ssh.contains("BatchMode=yes"), "{ssh}");
    assert!(ssh.contains("root@100.82.188.91 snapshot"), "{ssh}");
    let rsync = read(&t.state.join("rsync-args"));
    assert!(
        rsync.contains(&format!("--link-dest={}", t.nest.display())),
        "{rsync}"
    );
    assert!(
        rsync.contains("root@100.82.188.91:/var/lib/nuthatch-gate/stage/"),
        "{rsync}"
    );
}

#[test]
fn a_failed_snapshot_changes_nothing() {
    let t = thinkpad();
    std::fs::write(t.state.join("ssh-fail"), "").unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("could not take a snapshot"), "{text}");
    assert!(
        !t.state.join("rsync-args").exists(),
        "it pulled without a snapshot"
    );
    unchanged(&t);
}

#[test]
fn a_failed_pull_changes_nothing() {
    let t = thinkpad();
    std::fs::write(t.state.join("rsync-fail"), "").unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("rsync from"), "{text}");
    unchanged(&t);
}

/// A redb damaged between Helsinki's stage and the ThinkPad is refused, not handed to `serve`.
#[test]
fn a_torn_redb_is_refused() {
    let t = thinkpad();
    std::fs::write(t.state.join("corrupt"), "").unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("does not match the one staged"), "{text}");
    unchanged(&t);
}

/// The manifest names the segment set `serve` reads; a damaged one is refused like a damaged redb.
#[test]
fn a_torn_manifest_is_refused() {
    let t = thinkpad();
    std::fs::write(t.state.join("corrupt-manifest"), "").unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(
        text.contains("segments/manifest.json does not match the one staged"),
        "{text}"
    );
    unchanged(&t);
}

#[test]
fn a_stage_that_is_not_the_snapshot_is_refused() {
    let t = thinkpad();
    let p = read(&t.state.join("stage/PROVENANCE")).replace("attempt=1", "attempt=2");
    std::fs::write(t.state.join("snapshot-out"), p).unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("not the snapshot just taken"), "{text}");
    unchanged(&t);
}

#[test]
fn an_older_snapshot_is_refused() {
    let t = thinkpad();
    stage_provenance(&t.state.join("stage"), 50);
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("older than the copy's 100"), "{text}");
    unchanged(&t);
}

#[test]
fn a_snapshot_without_a_redb_hash_is_refused() {
    let t = thinkpad();
    std::fs::write(t.state.join("snapshot-out"), "sealed_through=200\n").unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("no redb_sha256"), "{text}");
    unchanged(&t);
}

#[test]
fn a_missing_key_is_loud() {
    let t = thinkpad();
    std::fs::remove_file(&t.key).unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 1, "{text}");
    assert!(text.contains("no ssh key"), "{text}");
    unchanged(&t);
}

/// The first refresh, onto a copy taken by hand with no PROVENANCE, has nothing to compare against.
#[test]
fn a_copy_without_provenance_is_refreshed() {
    let t = thinkpad();
    std::fs::remove_file(t.nest.join("PROVENANCE")).unwrap();
    let (code, text) = refresh(&t);
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("sealed_through unknown -> 200"), "{text}");
}

/// End to end: what gate-export stages is what the refresh accepts.
#[test]
fn a_real_snapshot_passes_the_refresh_checks() {
    let h = helsinki();
    assert_eq!(export(&h, "snapshot").0, 0);
    let t = thinkpad();
    std::fs::remove_dir_all(t.state.join("stage")).unwrap();
    let cp = Command::new("/bin/cp")
        .arg("-R")
        .arg(&h.stage)
        .arg(t.state.join("stage"))
        .status()
        .unwrap();
    assert!(cp.success());
    let (code, text) = refresh(&t);
    assert_eq!(code, 0, "{text}");
    assert_eq!(read(&t.nest.join("nuthatch.redb")), "redb-bytes");
    assert!(text.contains("sealed_through 100 -> 900"), "{text}");
}
