//! #1749 - `scripts/release-gate.sh` must fail a binary that refuses one of the recorded queries,
//! pass one that answers them all, and fail a time regression past its stated bound.
//!
//! A tiny nest is sealed once from the fixture chain by the real binary, then each case runs the
//! real script against a copy of it with a query set of its own: the gate serves the copy with
//! `serve`, which takes the redb's exclusive lock, so two cases cannot share one directory.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const ERC20_TRANSFER_ABI: &str = r#"[{"anonymous":false,"inputs":[
  {"indexed":true,"name":"from","type":"address"},
  {"indexed":true,"name":"to","type":"address"},
  {"indexed":false,"name":"value","type":"uint256"}],"name":"Transfer","type":"event"}]"#;

/// The fixture chain's defaults: blocks 1..=8 carry one Transfer each.
const CONTRACT: &str = "0x000000000000000000000000000000000000c0de";
const TIP: u64 = 9;
const FINALIZED: u64 = 8;
const TRANSFERS: u64 = 8;

struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn get(url: &str) -> Option<String> {
    let out = Command::new("curl")
        .args(["-fsS", "-m", "8", url])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn poll<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {secs}s waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn pin(rpc_port: u16, path: &str, n: u64) {
    let ok = Command::new("curl")
        .args(["-fsS", "-m", "5", "-XPOST"])
        .arg(format!("http://127.0.0.1:{rpc_port}/control/{path}"))
        .args(["-d", &format!("{{\"number\": {n}}}")])
        .status()
        .expect("curl")
        .success();
    assert!(ok, "could not pin {path} to {n}");
}

/// A sealed fixture nest, built once per test binary, and the name of its one table.
fn template() -> &'static (tempfile::TempDir, String) {
    static T: OnceLock<(tempfile::TempDir, String)> = OnceLock::new();
    T.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let rpc_port = free_port();
        let _rpc = Reaped(
            Command::new("python3")
                .arg(root().join("scripts/fixture_rpc.py"))
                .args(["--port", &rpc_port.to_string(), "--contract", CONTRACT])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn fixture_rpc.py"),
        );
        poll("the fixture RPC", 30, || {
            get(&format!("http://127.0.0.1:{rpc_port}/control/state"))
        });
        pin(rpc_port, "tip", TIP);
        pin(rpc_port, "finalized", FINALIZED);

        let nest = dir.path().join("nest");
        let abi = dir.path().join("erc20.abi.json");
        std::fs::write(&abi, ERC20_TRANSFER_ABI).unwrap();
        let init = Command::new(env!("CARGO_BIN_EXE_nuthatch"))
            .args(["init", CONTRACT, "--chain", "arbitrum-one"])
            .args(["--rpc", &format!("http://127.0.0.1:{rpc_port}/")])
            .arg("--abi")
            .arg(&abi)
            .arg("--dir")
            .arg(&nest)
            .output()
            .expect("run init");
        assert!(
            init.status.success(),
            "init failed:\n{}",
            String::from_utf8_lossy(&init.stderr)
        );

        let port = free_port();
        let mut dev = Reaped(
            Command::new(env!("CARGO_BIN_EXE_nuthatch"))
                .args(["dev", "--dir"])
                .arg(&nest)
                .args(["--listen", &format!("127.0.0.1:{port}"), "--seal-direct"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn dev"),
        );
        let api = format!("http://127.0.0.1:{port}");
        poll("the nest to seal the fixture history", 120, || {
            let body = get(&format!("{api}/sql?q=SELECT%201"))?;
            let v: serde_json::Value = serde_json::from_str(&body).ok()?;
            v["provenance"]["sealed_through"]
                .as_u64()
                .filter(|&n| n >= FINALIZED)
        });
        let tables: serde_json::Value =
            serde_json::from_str(&poll("the tables listing", 30, || {
                get(&format!("{api}/tables"))
            }))
            .unwrap();
        let table = tables["tables"][0]["name"]
            .as_str()
            .or_else(|| tables["tables"][0]["table"].as_str())
            .expect("a table")
            .to_string();
        let _ = Command::new("kill")
            .args(["-TERM", &dev.0.id().to_string()])
            .status();
        let _ = dev.0.wait();
        assert!(
            nest.join("nuthatch.redb").is_file(),
            "dev left no redb to serve"
        );
        (dir, table)
    })
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let dest = to.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &dest);
        } else {
            std::fs::copy(&path, &dest).unwrap();
        }
    }
}

struct Case {
    dir: tempfile::TempDir,
    nest: PathBuf,
    table: String,
}

fn case() -> Case {
    let (template, table) = template();
    let dir = tempfile::tempdir().unwrap();
    let nest = dir.path().join("nest");
    copy_dir(&template.path().join("nest"), &nest);
    Case {
        dir,
        nest,
        table: table.clone(),
    }
}

impl Case {
    fn set(&self, lines: &[(&str, String)]) -> PathBuf {
        let path = self.dir.path().join("set.tsv");
        let mut body = String::from("# id\tconsumer\tsite\tsql\n");
        for (id, sql) in lines {
            body.push_str(&format!("{id}\ttest\ttests/release_gate_script.rs\t{sql}\n"));
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn counts(&self) -> String {
        format!("SELECT count(*) AS n FROM \"{}\"", self.table)
    }

    fn gate(&self, set: &Path, extra: &[&str], env: &[(&str, &str)]) -> (Output, String) {
        let mut cmd = Command::new(root().join("scripts/release-gate.sh"));
        cmd.args(["--passes", "1", "--out"])
            .arg(self.dir.path().join("out"))
            .args(extra)
            .arg(env!("CARGO_BIN_EXE_nuthatch"))
            .arg(&self.nest)
            .arg(set);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().expect("run release-gate.sh");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out, text)
    }
}

fn line_for<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines()
        .find(|l| l.split_whitespace().nth(1) == Some(id))
        .unwrap_or_else(|| panic!("no result line for `{id}` in:\n{text}"))
}

#[test]
fn a_query_the_binary_refuses_fails_the_gate_and_is_named() {
    let c = case();
    let set = c.set(&[
        ("answers", c.counts()),
        ("refused", "SELECT a FROM no_such_table".to_string()),
    ]);
    let (out, text) = c.gate(&set, &[], &[]);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        line_for(&text, "answers").starts_with("ok "),
        "the answering query must still be reported as answering:\n{text}"
    );
    let refused = line_for(&text, "refused");
    assert!(
        refused.starts_with("FAIL ") && refused.contains("no_such_table"),
        "{refused}"
    );
    assert!(text.contains("RESULT: FAIL - failed: refused"), "{text}");
}

#[test]
fn a_set_the_binary_answers_passes_and_records_a_baseline() {
    let c = case();
    let set = c.set(&[
        ("answers", c.counts()),
        ("history", format!("SELECT * FROM \"{}\"", c.table)),
    ]);
    let baseline = c.dir.path().join("baseline.tsv");
    let (out, text) = c.gate(
        &set,
        &["--passes", "2", "--write-baseline", baseline.to_str().unwrap()],
        &[],
    );
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("RESULT: PASS"), "{text}");
    let answers = line_for(&text, "answers");
    assert_eq!(
        answers.split_whitespace().nth(2),
        Some("1"),
        "ROWS is the rows returned, one for a count: {answers}"
    );
    // The copy is served with its sealed history, not empty: every fixture transfer comes back.
    let history = line_for(&text, "history");
    assert_eq!(
        history.split_whitespace().nth(2),
        Some(TRANSFERS.to_string().as_str()),
        "{history}"
    );
    let recorded = std::fs::read_to_string(&baseline).unwrap();
    let row = recorded
        .lines()
        .find(|l| l.starts_with("answers\t"))
        .unwrap_or_else(|| panic!("no baseline row:\n{recorded}"));
    let cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(&cols[1..4], ["test", "ok", "1"], "{row}");
    assert!(cols[4].parse::<i64>().is_ok(), "a median time: {row}");
}

/// The bound decides, not the comparison: the same baseline passes under the default slack and
/// fails with the slack taken away. The baseline time is -1 so any real time is slower than it.
#[test]
fn a_regression_past_the_bound_fails_and_one_inside_it_does_not() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let baseline = c.dir.path().join("baseline.tsv");
    std::fs::write(
        &baseline,
        "# a hand-made baseline\nanswers\ttest\tok\t1\t-1\t-\t\n",
    )
    .unwrap();
    let base = baseline.to_str().unwrap();

    let (out, text) = c.gate(&set, &["--baseline", base], &[]);
    assert_eq!(out.status.code(), Some(0), "inside the default bound:\n{text}");

    let (out, text) = c.gate(
        &set,
        &["--baseline", base],
        &[("GATE_QUERY_SLACK_MS", "0"), ("GATE_P99_SLACK_MS", "0")],
    );
    assert_eq!(out.status.code(), Some(1), "past a zero-slack bound:\n{text}");
    let answers = line_for(&text, "answers");
    assert!(
        answers.starts_with("SLOW ") && answers.contains("regressed"),
        "{answers}"
    );
    assert!(text.contains("p99") && text.contains("REGRESSED"), "{text}");
}

/// A copy without its redb serves no sealed history; that is a broken rig, not a verdict on the
/// binary, so it exits 2 rather than 1 and never starts a server.
#[test]
fn a_copy_without_its_redb_is_a_setup_fault_not_a_failure() {
    let c = case();
    std::fs::remove_file(c.nest.join("nuthatch.redb")).unwrap();
    let set = c.set(&[("answers", c.counts())]);
    let (out, text) = c.gate(&set, &[], &[]);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("no nuthatch.redb"), "{text}");
}
