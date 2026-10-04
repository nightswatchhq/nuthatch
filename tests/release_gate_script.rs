//! #1749 - `scripts/release-gate.sh` must fail a binary that refuses one of the recorded queries,
//! pass one that answers them all, and fail a time regression past its stated bound or an answer
//! that differs from its baseline's (#1772).
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
            body.push_str(&format!(
                "{id}\ttest\ttests/release_gate_script.rs\t{sql}\n"
            ));
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
        &[
            "--passes",
            "2",
            "--write-baseline",
            baseline.to_str().unwrap(),
        ],
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
    assert_eq!(
        out.status.code(),
        Some(0),
        "inside the default bound:\n{text}"
    );

    let (out, text) = c.gate(
        &set,
        &["--baseline", base],
        &[("GATE_QUERY_SLACK_MS", "0"), ("GATE_P99_SLACK_MS", "0")],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "past a zero-slack bound:\n{text}"
    );
    let answers = line_for(&text, "answers");
    assert!(
        answers.starts_with("SLOW ") && answers.contains("regressed"),
        "{answers}"
    );
    assert!(text.contains("p99") && text.contains("REGRESSED"), "{text}");
}

impl Case {
    /// Defines `gate_probe` over the fixture table and `gate_rows` over literals. `wrong` stands
    /// in for a candidate that answers block 5 as 50, sorts on `s` the other way round, is 1e-13
    /// off on a float, and returns `gate_rows` in the reverse order: a view's own ORDER BY is not
    /// kept, but a VALUES list's order is.
    fn probe(&self, wrong: bool) {
        let views = self.nest.join("views");
        std::fs::create_dir_all(&views).unwrap();
        let (block, s, third, rows) = if wrong {
            (
                "CASE WHEN block_number = 5 THEN 50 ELSE block_number END",
                "100 - CAST(block_number AS BIGINT)",
                "CAST(block_number AS DOUBLE) / 3 + CAST(0.0000000000001 AS DOUBLE)",
                "(3, 'c'), (2, 'b'), (1, 'a')",
            )
        } else {
            (
                "block_number",
                "CAST(block_number AS BIGINT)",
                "CAST(block_number AS DOUBLE) / 3",
                "(1, 'a'), (2, 'b'), (3, 'c')",
            )
        };
        std::fs::write(
            views.join("90-gate-probe.sql"),
            format!(
                "CREATE VIEW gate_probe AS SELECT {block} AS block, {s} AS s, {third} AS third \
                 FROM \"{}\";\n",
                self.table
            ),
        )
        .unwrap();
        std::fs::write(
            views.join("91-gate-rows.sql"),
            format!("CREATE VIEW gate_rows AS SELECT * FROM (VALUES {rows}) AS v(k, label);\n"),
        )
        .unwrap();
    }

    /// Writes a baseline from the right answers, then gates the wrong ones against it, both runs
    /// with `extra`.
    fn against_a_wrong_candidate(&self, set: &Path, extra: &[&str]) -> (Output, String) {
        let baseline = self.dir.path().join("baseline.tsv");
        let base_out = self.dir.path().join("baseline-out");
        self.probe(false);
        let mut args = vec![
            "--out",
            base_out.to_str().unwrap(),
            "--write-baseline",
            baseline.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        let (out, text) = self.gate(set, &args, &[]);
        assert_eq!(out.status.code(), Some(0), "the baseline run:\n{text}");
        self.probe(true);
        let mut args = vec!["--baseline", baseline.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.gate(set, &args, &[])
    }
}

/// #1772: a statement that answers, but not what production answered, fails and is named, with
/// the first row at which the two differ. Row order is part of the answer only under a top-level
/// ORDER BY: a statement without one passes with its rows reversed (the quoted ORDER BY is a
/// literal, not a clause), and a float 1e-13 off is equal to 12 significant digits, as a number or
/// cast to text.
#[test]
fn an_answer_that_differs_from_the_baseline_fails_with_its_first_differing_row() {
    let c = case();
    let set = c.set(&[
        ("value", "SELECT block FROM gate_probe".to_string()),
        (
            "ordered",
            "SELECT third FROM gate_probe ORDER BY s".to_string(),
        ),
        (
            "unordered",
            "SELECT k, label FROM gate_rows WHERE 'ORDER BY' <> ''".to_string(),
        ),
        (
            "float",
            "SELECT third, CAST(third * CAST('1e22' AS DOUBLE) AS VARCHAR) AS text FROM gate_probe"
                .to_string(),
        ),
        (
            "volatile",
            "SELECT block FROM gate_probe WHERE block < 10".to_string(),
        ),
    ]);
    // Volatile is held to its row count, and block 5 answered as 50 is a row fewer.
    let body = std::fs::read_to_string(&set).unwrap();
    std::fs::write(&set, format!("# volatile: volatile a row short\n{body}")).unwrap();
    let (out, text) = c.against_a_wrong_candidate(&set, &[]);
    assert_eq!(out.status.code(), Some(1), "{text}");
    let value = line_for(&text, "value");
    assert!(
        value.starts_with("FAIL ") && value.contains("answer differs"),
        "{value}"
    );
    assert!(
        text.contains("first differing row, row 5:")
            && text.contains("candidate: {\"block\":50}")
            && text.contains("baseline:  {\"block\":5}"),
        "the first differing row of each side:\n{text}"
    );
    let ordered = line_for(&text, "ordered");
    assert!(
        ordered.starts_with("FAIL ") && ordered.contains("answer differs (rows compared in order)"),
        "{ordered}"
    );
    assert!(
        line_for(&text, "unordered").starts_with("ok "),
        "only the order differs, and it has no top-level ORDER BY:\n{text}"
    );
    assert!(
        line_for(&text, "float").starts_with("ok "),
        "equal to 12 significant digits:\n{text}"
    );
    let volatile = line_for(&text, "volatile");
    assert!(
        volatile.starts_with("FAIL ")
            && volatile.contains("answer differs: 7 rows against the baseline's 8"),
        "{volatile}"
    );
    assert!(
        text.contains("RESULT: FAIL - answer differs: value, ordered, volatile"),
        "{text}"
    );
}

/// A statement tagged volatile in the set is held to its row count, so a different answer with
/// the same number of rows passes; untagged, it fails (the test above).
#[test]
fn a_volatile_statement_that_answers_differently_passes_on_its_row_count() {
    let c = case();
    let set = c.set(&[
        ("changing", "SELECT block FROM gate_probe".to_string()),
        (
            "unordered",
            "SELECT k, label FROM gate_rows WHERE 'ORDER BY' <> ''".to_string(),
        ),
    ]);
    let body = std::fs::read_to_string(&set).unwrap();
    std::fs::write(
        &set,
        format!("# volatile: changing its answer moves with the clock\n{body}"),
    )
    .unwrap();
    let (out, text) = c.against_a_wrong_candidate(&set, &[]);
    assert_eq!(out.status.code(), Some(0), "{text}");
    let changing = line_for(&text, "changing");
    assert!(
        changing.starts_with("ok ") && changing.contains("volatile, so compared on its row count"),
        "{changing}"
    );
    assert!(line_for(&text, "unordered").starts_with("ok "), "{text}");
    assert!(
        text.contains("1 match, 0 differ, 1 compared on row count only"),
        "{text}"
    );
}

/// #1773: production serves two statements at once. At `--concurrency 2` the set goes out a pair at
/// a time, both statements of a pair in flight together, and each keeps its own row and time.
#[test]
fn at_concurrency_two_a_pair_is_in_flight_together_and_both_are_recorded() {
    let c = case();
    // Slow enough on eight rows that two run one after the other cannot overlap by accident.
    let slow = |salt: u32| {
        let from: Vec<String> = (0..6).map(|i| format!("\"{}\" t{i}", c.table)).collect();
        format!(
            "SELECT count(DISTINCT {}) AS n FROM {}",
            (0..6)
                .map(|i| format!("t{i}.block_number * {}", 10u64.pow(i) + u64::from(salt)))
                .collect::<Vec<_>>()
                .join(" + "),
            from.join(", ")
        )
    };
    let set = c.set(&[("first", slow(0)), ("second", slow(1))]);
    let (out, text) = c.gate(&set, &["--concurrency", "2"], &[]);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("concurrency 2"), "{text}");
    for id in ["first", "second"] {
        let line = line_for(&text, id);
        assert!(line.starts_with("ok "), "{line}");
        assert_eq!(line.split_whitespace().nth(2), Some("1"), "{line}");
    }

    // group<TAB>id<TAB>start ms<TAB>end ms, one line per statement sent.
    let schedule = std::fs::read_to_string(c.dir.path().join("out/schedule-pass-1.tsv")).unwrap();
    let rows: Vec<Vec<&str>> = schedule.lines().map(|l| l.split('\t').collect()).collect();
    assert_eq!(rows.len(), 2, "{schedule}");
    assert_eq!(rows[0][0], rows[1][0], "one pair, one group: {schedule}");
    let span = |r: &Vec<&str>| (r[2].parse::<u64>().unwrap(), r[3].parse::<u64>().unwrap());
    let ((s0, e0), (s1, e1)) = (span(&rows[0]), span(&rows[1]));
    assert!(
        s0 < e1 && s1 < e0,
        "the pair ran one after the other, not together: {schedule}"
    );
}

/// At `--concurrency 2` each statement's answer is still captured and compared as it is one at a
/// time. Each pair holds one statement that differs and one that matches, so an answer recorded
/// against its partner's id fails on both.
#[test]
fn at_concurrency_two_an_answer_that_differs_still_fails_and_is_named() {
    let c = case();
    let set = c.set(&[
        (
            "unordered",
            "SELECT k, label FROM gate_rows WHERE 'ORDER BY' <> ''".to_string(),
        ),
        ("value", "SELECT block FROM gate_probe".to_string()),
        (
            "ordered",
            "SELECT third FROM gate_probe ORDER BY s".to_string(),
        ),
        (
            "float",
            "SELECT third, CAST(third * CAST('1e22' AS DOUBLE) AS VARCHAR) AS text FROM gate_probe"
                .to_string(),
        ),
    ]);
    let (out, text) = c.against_a_wrong_candidate(&set, &["--concurrency", "2"]);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("concurrency 2"), "{text}");
    let value = line_for(&text, "value");
    assert!(
        value.starts_with("FAIL ") && value.contains("answer differs"),
        "{value}"
    );
    assert!(
        text.contains("first differing row, row 5:")
            && text.contains("candidate: {\"block\":50}")
            && text.contains("baseline:  {\"block\":5}"),
        "the first differing row of each side:\n{text}"
    );
    let ordered = line_for(&text, "ordered");
    assert!(
        ordered.starts_with("FAIL ") && ordered.contains("answer differs (rows compared in order)"),
        "{ordered}"
    );
    assert!(line_for(&text, "unordered").starts_with("ok "), "{text}");
    assert!(line_for(&text, "float").starts_with("ok "), "{text}");
    assert!(
        text.contains("2 match, 2 differ")
            && text.contains("RESULT: FAIL - answer differs: value, ordered"),
        "{text}"
    );

    let schedule = std::fs::read_to_string(c.dir.path().join("out/schedule-pass-1.tsv")).unwrap();
    let groups: Vec<(&str, &str)> = schedule
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f[0], f[1])
        })
        .collect();
    assert_eq!(
        groups,
        [
            ("1", "unordered"),
            ("1", "value"),
            ("2", "ordered"),
            ("2", "float")
        ],
        "{schedule}"
    );
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

/// The serving process's peak RSS is held to the per-cursor budget. A one-megabyte ceiling no real
/// server fits under must fail a set that otherwise answers, and name the peak.
#[test]
fn a_peak_rss_over_the_budget_fails_a_set_that_answers() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let (out, text) = c.gate(&set, &[], &[("GATE_MAX_RSS_MB", "1")]);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(line_for(&text, "answers").starts_with("ok "), "{text}");
    assert!(text.contains("peak RSS") && text.contains("OVER"), "{text}");

    let (out, text) = c.gate(&set, &[], &[]);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains("peak RSS"),
        "the peak is reported on a pass too:\n{text}"
    );
}

/// Starts a gate script with its output in a file; `finish` kills it if it overruns, so a lock that
/// deadlocks fails the test rather than hanging it.
fn run_bounded(mut cmd: Command, dir: &Path, name: &str) -> Child {
    let out = std::fs::File::create(dir.join(format!("{name}.out"))).unwrap();
    let err = out.try_clone().unwrap();
    cmd.stdout(out).stderr(err);
    cmd.spawn().expect("spawn a gate script")
}

fn finish(mut child: Child, dir: &Path, name: &str, secs: u64) -> (Option<i32>, String) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{name} still running after {secs}s:\n{}",
                std::fs::read_to_string(dir.join(format!("{name}.out"))).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let text = std::fs::read_to_string(dir.join(format!("{name}.out"))).unwrap();
    (status.code(), text)
}

/// The calls `release-gate-run.sh` makes, answered from `$FAKE_GH_DIR`: `releases` (one
/// `<tag> full|pre|draft` per line, newest first), `status-<sha>` (the state of the newest gate
/// status on a commit; absent is none) and `bin-<tag>/nuthatch` (the binary a release ships). A
/// tag's commit is `sha-<tag>`. Posted statuses are appended to `posted`, and with their context
/// first to `posted-ctx`; downloads to `downloaded`. `status-<sha>-<context, / as _>` answers for
/// one context before `status-<sha>` answers for all.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
set -euo pipefail
d=$FAKE_GH_DIR
case "$1 $2" in
  "api -X")
    sha=${4##*/} state="" desc="" ctx=""
    shift 4
    while [ $# -ge 2 ]; do
      case "$2" in
        state=*) state=${2#state=} ;;
        description=*) desc=${2#description=} ;;
        context=*) ctx=${2#context=} ;;
      esac
      shift 2
    done
    echo "$sha $state $desc" >>"$d/posted"
    echo "$ctx $sha $state $desc" >>"$d/posted-ctx" ;;
  "api "*)
    case "$2" in
      */statuses)
        sha=${2%/statuses}; sha=${sha##*/}
        ctx=$(printf '%s' "$*" | sed -n 's/.*\.context == "\([^"]*\)".*/\1/p' | head -n 1)
        cat "$d/status-$sha-${ctx//\//_}" 2>/dev/null || cat "$d/status-$sha" 2>/dev/null || echo none ;;
      */commits/*) echo "sha-${2##*/}" ;;
      *) echo "fake gh: unexpected api call: $*" >&2; exit 3 ;;
    esac ;;
  "release list")
    pre=1
    for a in "$@"; do [ "$a" != --exclude-pre-releases ] || pre=0; done
    while read -r t kind; do
      [ "$kind" != draft ] || continue
      [ "$kind" != pre ] || [ "$pre" -eq 1 ] || continue
      echo "$t"
    done <"$d/releases" ;;
  "release download")
    t=$3 out=""
    while [ $# -gt 0 ]; do
      if [ "$1" = -D ]; then out=$2; fi
      shift
    done
    asset=nuthatch-x86_64-unknown-linux-gnu.tar.gz
    echo "$t" >>"$d/downloaded"
    tar -czf "$out/$asset" -C "$d/bin-$t" nuthatch
    if command -v sha256sum >/dev/null; then
      (cd "$out" && sha256sum "$asset" >"$asset.sha256")
    else
      (cd "$out" && shasum -a 256 "$asset" >"$asset.sha256")
    fi ;;
  *) echo "fake gh: unexpected call: $*" >&2; exit 3 ;;
esac
"#;

struct Releases {
    dir: PathBuf,
}

/// Fake releases of the real binary. Each ships a wrapper that serves the real binary as its child
/// after holding `pad` bytes itself, so the RSS the gate samples (the wrapper's) differs by release.
fn releases(c: &Case, list: &[(&str, &str, usize)], gated: &[(&str, &str)]) -> Releases {
    use std::os::unix::fs::PermissionsExt;
    let dir = c.dir.path().join("gh");
    let fakes = dir.join("fakes");
    std::fs::create_dir_all(&fakes).unwrap();
    let gh = fakes.join("gh");
    std::fs::write(&gh, FAKE_GH).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut listing = String::new();
    for (tag, kind, pad) in list {
        listing.push_str(&format!("{tag} {kind}\n"));
        let bin_dir = dir.join(format!("bin-{tag}"));
        std::fs::create_dir_all(&bin_dir).unwrap();
        let real = env!("CARGO_BIN_EXE_nuthatch");
        let wrapper = bin_dir.join("nuthatch");
        std::fs::write(
            &wrapper,
            format!(
                "#!/usr/bin/env bash\n\
                 [ \"${{1:-}}\" = serve ] || exec '{real}' \"$@\"\n\
                 pad=$(head -c {pad} /dev/zero | tr '\\0' x)\n\
                 '{real}' \"$@\" &\n\
                 child=$!\n\
                 trap 'kill $child 2>/dev/null; wait $child; exit 0' TERM INT\n\
                 wait $child\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(dir.join("releases"), listing).unwrap();
    for (tag, state) in gated {
        std::fs::write(dir.join(format!("status-sha-{tag}")), format!("{state}\n")).unwrap();
    }
    Releases { dir }
}

impl Releases {
    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn runner(
        &self,
        c: &Case,
        set: &Path,
        args: &[&str],
        env: &[(&str, &str)],
    ) -> (Option<i32>, String) {
        let mut cmd = Command::new(root().join("scripts/release-gate-run.sh"));
        cmd.args(args)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.join("fakes").display(),
                    std::env::var("PATH").unwrap()
                ),
            )
            .env("FAKE_GH_DIR", &self.dir)
            .env("GATE_STATE", c.dir.path().join("state"))
            .env("GATE_NEST", &c.nest)
            .env("GATE_SET", set)
            .env("GATE_PASSES", "1")
            .env("GATE_REPO", "test/nuthatch")
            .env_remove("GATE_LOCK_HELD");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = run_bounded(cmd, c.dir.path(), "runner");
        finish(child, c.dir.path(), "runner", 120)
    }
}

/// On 2026-10-03 production peaked at 3324 MiB over the 2048 budget with every statement answered,
/// and the runner posted error ("no baseline") without measuring the candidate, so nothing could be
/// gated while production was over. Production's peak is reported; the verdict is the candidate's.
#[test]
fn production_over_its_rss_budget_is_still_a_baseline_for_the_candidate() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(
        &c,
        &[("v4.3.0-rc1", "pre", 0), ("v4.2.0", "full", 160_000_000)],
        &[],
    );
    let (code, text) = r.runner(&c, &set, &["v4.3.0-rc1"], &[("GATE_MAX_RSS_MB", "100")]);
    assert_eq!(code, Some(0), "{text}");
    let posted = r.read("posted");
    let verdict = posted.lines().last().unwrap_or_default();
    assert!(
        verdict.starts_with("sha-v4.3.0-rc1 success ") && verdict.contains("v4.2.0"),
        "{posted}\n{text}"
    );
    assert!(
        verdict.contains("MiB"),
        "production's own peak belongs in the candidate's status: {verdict}"
    );
    assert!(text.contains("over the 100 MiB budget"), "{text}");
    // The copy has no PROVENANCE, so production is a guess, and the status says so.
    assert!(verdict.contains("no PROVENANCE"), "{verdict}");
    assert!(text.contains("has no PROVENANCE"), "{text}");
}

/// A statement production fails as well cannot be a regression, and the output says so, but the
/// candidate is still failed for refusing it: its verdict is its own.
#[test]
fn a_statement_production_also_fails_still_fails_the_candidate_and_is_not_a_regression() {
    let c = case();
    let set = c.set(&[
        ("answers", c.counts()),
        ("refused", "SELECT a FROM no_such_table".to_string()),
    ]);
    let r = releases(&c, &[("v4.3.0-rc1", "pre", 0), ("v4.2.0", "full", 0)], &[]);
    let (code, text) = r.runner(&c, &set, &["v4.3.0-rc1"], &[]);
    assert_eq!(code, Some(1), "{text}");
    let posted = r.read("posted");
    let verdict = posted.lines().last().unwrap_or_default();
    assert!(
        verdict.starts_with("sha-v4.3.0-rc1 failure ") && verdict.contains("refused"),
        "{posted}\n{text}"
    );
    assert!(
        text.contains("production v4.2.0 fails refused itself, so it cannot regress"),
        "{text}"
    );
}

/// On 2026-10-03 `--poll` walked back through every past release without a status, posting error
/// on v4.1.0 and then v4.0.2. Only a release newer than production is gated; a prerelease of
/// production's own version is older than it.
#[test]
fn poll_never_gates_a_release_at_or_below_production() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(
        &c,
        &[
            ("v4.2.0", "full", 0),
            ("v4.2.0-rc1", "pre", 0),
            ("v4.1.0", "full", 0),
            ("v4.0.2", "full", 0),
        ],
        &[("v4.2.0", "success")],
    );
    let (code, text) = r.runner(&c, &set, &["--poll"], &[]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(r.read("posted"), "", "nothing is gated:\n{text}");
    assert_eq!(r.read("downloaded"), "", "nothing is fetched:\n{text}");
    assert_eq!(text, "", "nothing to gate is quiet");
}

/// A prerelease of a newer version is gated, compared by number: v4.10 is newer than v4.9.
#[test]
fn poll_gates_a_prerelease_of_a_newer_version() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(
        &c,
        &[
            ("v4.10.0-rc.1", "pre", 0),
            ("v4.9.0", "full", 0),
            ("v4.8.0", "full", 0),
        ],
        &[("v4.9.0", "success")],
    );
    // The runner holds the copy's lock itself from before the refresh, so a gate run by hand cannot
    // slip in between the refresh and the two measurements.
    let lock = std::fs::canonicalize(c.dir.path())
        .unwrap()
        .join("nest.gate-lock.d/pid");
    let refresh = format!("[ \"$(cat '{}')\" = \"$PPID\" ]", lock.display());
    let (code, text) = r.runner(
        &c,
        &set,
        &["--poll"],
        &[("GATE_LOCK_PORTABLE", "1"), ("GATE_REFRESH", &refresh)],
    );
    assert_eq!(code, Some(0), "{text}");
    let posted = r.read("posted");
    assert!(
        posted
            .lines()
            .last()
            .unwrap_or_default()
            .starts_with("sha-v4.10.0-rc.1 success "),
        "{posted}\n{text}"
    );
    assert!(!posted.contains("sha-v4.8.0"), "{posted}");
}

/// A hand-run gate against the copy the runner was using made the runner's `serve` fail to start
/// on the redb lock. Two gate runs against one copy wait for each other; both answer.
#[test]
fn two_gate_runs_against_one_copy_serialise() {
    for portable in [false, true] {
        let c = case();
        let set = c.set(&[("answers", c.counts())]);
        let start = |name: &str| {
            let mut cmd = Command::new(root().join("scripts/release-gate.sh"));
            cmd.args(["--passes", "2", "--out"])
                .arg(c.dir.path().join(name))
                .arg(env!("CARGO_BIN_EXE_nuthatch"))
                .arg(&c.nest)
                .arg(&set)
                .env_remove("GATE_LOCK_HELD");
            if portable {
                cmd.env("GATE_LOCK_PORTABLE", "1");
            }
            run_bounded(cmd, c.dir.path(), name)
        };
        let a = start("first");
        let b = start("second");
        let (code_a, text_a) = finish(a, c.dir.path(), "first", 300);
        let (code_b, text_b) = finish(b, c.dir.path(), "second", 300);
        assert_eq!(code_a, Some(0), "portable={portable}:\n{text_a}");
        assert_eq!(code_b, Some(0), "portable={portable}:\n{text_b}");
        assert!(
            format!("{text_a}{text_b}").contains("waiting"),
            "one run waited for the other (portable={portable}):\n{text_a}\n{text_b}"
        );
        assert!(
            !c.dir.path().join("nest.gate-lock.d").exists(),
            "the lock is released on exit"
        );
    }
}

/// Nothing releases a mkdir lock for a holder that was killed, so a lock whose pid has gone is
/// broken rather than waited on for ever.
#[test]
fn a_lock_left_by_a_killed_run_is_broken() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    let held = c.dir.path().join("nest.gate-lock.d");
    std::fs::create_dir(&held).unwrap();
    std::fs::write(held.join("pid"), format!("{pid}\n")).unwrap();
    let mut cmd = Command::new(root().join("scripts/release-gate.sh"));
    cmd.args(["--passes", "1", "--out"])
        .arg(c.dir.path().join("out"))
        .arg(env!("CARGO_BIN_EXE_nuthatch"))
        .arg(&c.nest)
        .arg(&set)
        .env("GATE_LOCK_PORTABLE", "1")
        .env_remove("GATE_LOCK_HELD");
    let child = run_bounded(cmd, c.dir.path(), "gate");
    let (code, text) = finish(child, c.dir.path(), "gate", 120);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.contains("breaking"), "{text}");
}

/// On 2026-10-04 v4.3.1 was gated against v4.3.0, the latest release, while the allocations nest
/// ran 4.2.1, so the statements 4.3.1 fixed read as answers that differ. Production is the version
/// the refreshed copy's PROVENANCE records.
#[test]
fn production_is_the_version_the_refreshed_copy_records_not_the_latest_release() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(
        &c,
        &[
            ("v4.3.1", "full", 0),
            ("v4.3.0", "full", 0),
            ("v4.2.1", "full", 0),
        ],
        &[],
    );
    let provenance = c.nest.join("PROVENANCE");
    assert!(
        !provenance.exists(),
        "the refresh writes it, not the fixture"
    );
    let refresh = format!(
        "printf 'taken_at=2026-10-04T00:00:00Z\\nversion=4.2.1\\nsealed_through=8\\n' >'{}'",
        provenance.display()
    );
    let (code, text) = r.runner(&c, &set, &["v4.3.1"], &[("GATE_REFRESH", &refresh)]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(r.read("downloaded"), "v4.2.1\nv4.3.1\n", "{text}");
    let posted = r.read("posted");
    assert!(
        posted.lines().all(|l| !l.contains("v4.3.0")),
        "{posted}\n{text}"
    );
    let verdict = posted.lines().last().unwrap_or_default();
    assert!(
        verdict.starts_with("sha-v4.3.1 success ") && verdict.contains("(against v4.2.1)"),
        "{posted}\n{text}"
    );
    assert!(text.contains("PROVENANCE records"), "{text}");
}

/// `--poll`'s "newer than production" uses the same production: a candidate at or below what the
/// copy records production running is not gated, though it is newer than the latest full release.
#[test]
fn poll_measures_newer_against_the_production_the_copy_records() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(
        &c,
        &[
            ("v4.3.0-rc.2", "pre", 0),
            ("v4.3.0-rc.1", "pre", 0),
            ("v4.2.1", "full", 0),
        ],
        &[],
    );
    std::fs::write(c.nest.join("PROVENANCE"), "version=4.3.0-rc.2\n").unwrap();
    let (code, text) = r.runner(&c, &set, &["--poll"], &[]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(r.read("posted"), "", "nothing is gated:\n{text}");
    assert_eq!(r.read("downloaded"), "", "nothing is fetched:\n{text}");
    assert_eq!(text, "", "nothing to gate is quiet");
}

/// Production rolled to the candidate between the poll and the refresh: the refreshed copy names
/// the candidate itself, so there is nothing to gate and nothing is posted.
#[test]
fn poll_leaves_a_candidate_production_was_rolled_to_during_the_refresh() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(&c, &[("v4.3.0-rc.2", "pre", 0), ("v4.2.1", "full", 0)], &[]);
    let provenance = c.nest.join("PROVENANCE");
    std::fs::write(&provenance, "version=4.2.1\n").unwrap();
    let refresh = format!("printf 'version=4.3.0-rc.2\\n' >'{}'", provenance.display());
    let (code, text) = r.runner(&c, &set, &["--poll"], &[("GATE_REFRESH", &refresh)]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(r.read("posted"), "", "nothing is gated:\n{text}");
    assert_eq!(r.read("downloaded"), "", "nothing is fetched:\n{text}");
    assert!(text.contains("no longer newer"), "{text}");
}

/// `--production` still overrides the copy's PROVENANCE.
#[test]
fn production_flag_overrides_the_copys_provenance() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(
        &c,
        &[
            ("v4.3.1", "full", 0),
            ("v4.3.0", "full", 0),
            ("v4.2.1", "full", 0),
        ],
        &[],
    );
    std::fs::write(c.nest.join("PROVENANCE"), "version=4.2.1\n").unwrap();
    let (code, text) = r.runner(&c, &set, &["v4.3.1", "--production", "v4.3.0"], &[]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(r.read("downloaded"), "v4.3.0\nv4.3.1\n", "{text}");
}

/// A PROVENANCE whose version is not a release version is not guessed around: the run exits 2 and
/// posts one error status on the candidate naming the version, by tag or after a poll's refresh.
#[test]
fn a_provenance_version_that_is_not_a_release_posts_one_error_naming_it() {
    for poll in [false, true] {
        let c = case();
        let set = c.set(&[("answers", c.counts())]);
        let r = releases(&c, &[("v4.3.1", "full", 0), ("v4.2.1", "full", 0)], &[]);
        let provenance = c.nest.join("PROVENANCE");
        let (args, refresh): (&[&str], String) = if poll {
            std::fs::write(&provenance, "version=4.2.1\n").unwrap();
            let refresh = format!(
                "printf 'version=dirty-build\\n' >'{}'",
                provenance.display()
            );
            (&["--poll"], refresh)
        } else {
            std::fs::write(&provenance, "version=dirty-build\n").unwrap();
            (&["v4.3.1"], String::new())
        };
        let env: &[(&str, &str)] = if poll {
            &[("GATE_REFRESH", &refresh)]
        } else {
            &[]
        };
        let (code, text) = r.runner(&c, &set, args, env);
        assert_eq!(code, Some(2), "poll={poll}: {text}");
        let posted = r.read("posted");
        assert_eq!(posted.lines().count(), 1, "poll={poll}: {posted}\n{text}");
        assert!(
            posted.starts_with("sha-v4.3.1 error ") && posted.contains("dirty-build"),
            "poll={poll}: {posted}\n{text}"
        );
        assert_eq!(r.read("downloaded"), "", "poll={poll}: {text}");
    }
}

/// Before a poll has chosen a candidate there is no commit to post on: the run exits 2 and says so.
#[test]
fn a_bad_provenance_before_a_poll_chooses_exits_loud_with_no_status() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let r = releases(&c, &[("v4.3.1", "full", 0), ("v4.2.1", "full", 0)], &[]);
    std::fs::write(c.nest.join("PROVENANCE"), "version=dirty-build\n").unwrap();
    let (code, text) = r.runner(&c, &set, &["--poll"], &[]);
    assert_eq!(code, Some(2), "{text}");
    assert_eq!(r.read("posted"), "", "{text}");
    assert!(
        text.contains("dirty-build") && text.contains("no candidate is chosen yet"),
        "{text}"
    );
}

// --- every production nest (#1794) ---

/// The ThinkPad's gate config for these runs: per nest, its own copy of the fixture nest, its own
/// set and its own production environment, and a refresh kind. Returns the config's path.
/// A nest's name, its set's `(id, sql)` lines, its env file's body and its refresh kind.
type NestRow<'a> = (&'a str, &'a [(&'a str, String)], &'a str, &'a str);

fn nests_conf(c: &Case, nests: &[NestRow]) -> PathBuf {
    let mut conf = String::from("# name copy set env refresh\n");
    for (name, lines, env, refresh) in nests {
        let copy = c.dir.path().join(name);
        copy_dir(&c.nest, &copy);
        let set = c.dir.path().join(format!("{name}-queries.tsv"));
        let mut body = String::from("# id\tconsumer\tsite\tsql\n");
        for (id, sql) in *lines {
            body.push_str(&format!(
                "{id}\ttest\ttests/release_gate_script.rs\t{sql}\n"
            ));
        }
        std::fs::write(&set, body).unwrap();
        let env_file = c.dir.path().join(format!("{name}.env"));
        std::fs::write(&env_file, format!("# read from {name}'s unit\n{env}")).unwrap();
        conf.push_str(&format!(
            "{name}\t{}\t{}\t{}\t{refresh}\n",
            copy.display(),
            set.display(),
            env_file.display()
        ));
    }
    let path = c.dir.path().join("nests.conf");
    std::fs::write(&path, conf).unwrap();
    path
}

const ENV_TWO: &str = "NUTHATCH_SQL_MAX_CONCURRENCY=2\nNUTHATCH_ENGINE=burrmill\n";
const ENV_ONE: &str = "NUTHATCH_SQL_MAX_CONCURRENCY=1\nNUTHATCH_ENGINE=burrmill\n";

fn posted_for<'a>(posted: &'a str, context: &str) -> Vec<&'a str> {
    posted
        .lines()
        .filter(|l| l.split(' ').next() == Some(context))
        .collect()
}

/// On 2026-10-03 the QoS nest refused its daily views all day and the gate, which ran the
/// allocations nest alone, never saw it. Every nest in the config is gated, each under its own
/// environment and refresh, and each posts its own status; the run's exit is the worst of theirs.
#[test]
fn every_nest_in_the_config_is_gated_and_posts_its_own_status() {
    use std::os::unix::fs::PermissionsExt;
    let c = case();
    let answers = [("answers", c.counts())];
    let refused = [
        ("answers", c.counts()),
        ("daily", "SELECT a FROM no_such_view".to_string()),
    ];
    let conf = nests_conf(
        &c,
        &[
            ("alloc-nest", &answers, ENV_TWO, "helsinki"),
            ("qos-nest", &refused, ENV_ONE, "local"),
        ],
    );
    let r = releases(&c, &[("v4.3.0-rc1", "pre", 0), ("v4.2.0", "full", 0)], &[]);
    let refresh_log = c.dir.path().join("refreshed");
    let refresh = c.dir.path().join("fake-refresh");
    std::fs::write(
        &refresh,
        format!(
            "#!/usr/bin/env bash\necho \"$* $GATE_NEST\" >>'{}'\n",
            refresh_log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&refresh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let set = c.set(&[("unused", c.counts())]);
    let (code, text) = r.runner(
        &c,
        &set,
        &["v4.3.0-rc1"],
        &[
            ("GATE_NESTS", conf.to_str().unwrap()),
            ("GATE_REFRESH_SCRIPT", refresh.to_str().unwrap()),
        ],
    );
    assert_eq!(code, Some(1), "the worst nest decides:\n{text}");
    let posted = r.read("posted-ctx");
    let alloc = posted_for(&posted, "release-gate/alloc-nest");
    let qos = posted_for(&posted, "release-gate/qos-nest");
    assert_eq!(alloc.len(), 2, "pending then a verdict:\n{posted}");
    assert!(
        alloc[1].starts_with("release-gate/alloc-nest sha-v4.3.0-rc1 success "),
        "{posted}\n{text}"
    );
    assert_eq!(qos.len(), 2, "pending then a verdict:\n{posted}");
    assert!(
        qos[1].starts_with("release-gate/qos-nest sha-v4.3.0-rc1 failure ")
            && qos[1].contains("daily"),
        "{posted}\n{text}"
    );
    assert!(qos[0].contains("against the qos-nest copy"), "{posted}");
    // Each nest ran under its own environment file, at its own concurrency.
    let qos_env = c.dir.path().join("qos-nest.env");
    let from = format!("(from {})", qos_env.display());
    assert!(
        text.lines()
            .any(|l| l.starts_with("[qos-nest] ") && l.contains(&from)),
        "the candidate's run:\n{text}"
    );
    let runs = c.dir.path().join("state/runs");
    let production = std::fs::read_dir(&runs)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().contains("-qos-nest-"))
        .map(|e| std::fs::read_to_string(e.path().join("production.txt")).unwrap())
        .unwrap_or_default();
    assert!(
        production.contains(&from),
        "production's run:\n{production}"
    );
    assert!(
        text.contains("[qos-nest] release-gate: concurrency 1"),
        "{text}"
    );
    assert!(
        text.contains("[alloc-nest] release-gate: concurrency 2"),
        "{text}"
    );
    let refreshed = std::fs::read_to_string(&refresh_log).unwrap_or_default();
    assert_eq!(
        refreshed,
        format!(
            "alloc-nest {}\n--local qos-nest {}\n",
            c.dir.path().join("alloc-nest").display(),
            c.dir.path().join("qos-nest").display()
        ),
        "{text}"
    );
}

/// A poll gates, nest by nest, a release that nest has no status for: the allocations nest gated
/// the candidate already, so only the QoS nest gates it now.
#[test]
fn poll_gates_each_nest_that_lacks_its_own_status() {
    let c = case();
    let answers = [("answers", c.counts())];
    let conf = nests_conf(
        &c,
        &[
            ("alloc-nest", &answers, ENV_TWO, "none"),
            ("qos-nest", &answers, ENV_TWO, "none"),
        ],
    );
    let r = releases(&c, &[("v4.3.0-rc1", "pre", 0), ("v4.2.0", "full", 0)], &[]);
    std::fs::write(
        r.dir.join("status-sha-v4.3.0-rc1-release-gate_alloc-nest"),
        "success\n",
    )
    .unwrap();
    for nest in ["alloc-nest", "qos-nest"] {
        std::fs::write(
            c.dir.path().join(nest).join("PROVENANCE"),
            "version=4.2.0\n",
        )
        .unwrap();
    }
    let set = c.set(&[("unused", c.counts())]);
    let (code, text) = r.runner(
        &c,
        &set,
        &["--poll"],
        &[("GATE_NESTS", conf.to_str().unwrap())],
    );
    assert_eq!(code, Some(0), "{text}");
    let posted = r.read("posted-ctx");
    assert!(
        posted_for(&posted, "release-gate/alloc-nest").is_empty(),
        "{posted}\n{text}"
    );
    let qos = posted_for(&posted, "release-gate/qos-nest");
    assert!(
        qos.last()
            .is_some_and(|l| l.starts_with("release-gate/qos-nest sha-v4.3.0-rc1 success ")),
        "{posted}\n{text}"
    );
    assert!(
        !text.contains("[alloc-nest]"),
        "a nest with nothing to gate is quiet:\n{text}"
    );
}

/// A config line that does not parse gates nothing, rather than half the nests.
#[test]
fn a_malformed_nests_config_gates_nothing() {
    let c = case();
    let answers = [("answers", c.counts())];
    let conf = nests_conf(&c, &[("alloc-nest", &answers, ENV_TWO, "none")]);
    let mut body = std::fs::read_to_string(&conf).unwrap();
    body.push_str("qos-nest /nowhere /nowhere.tsv /nowhere.env none stray\n");
    std::fs::write(&conf, body).unwrap();
    let r = releases(&c, &[("v4.3.0-rc1", "pre", 0), ("v4.2.0", "full", 0)], &[]);
    let set = c.set(&[("unused", c.counts())]);
    let (code, text) = r.runner(
        &c,
        &set,
        &["v4.3.0-rc1"],
        &[("GATE_NESTS", conf.to_str().unwrap())],
    );
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("line 3"), "{text}");
    assert_eq!(r.read("posted"), "", "{text}");
    assert_eq!(r.read("downloaded"), "", "{text}");
}

/// The environment `serve` runs under is the nest's file alone: what it sets is applied, and a
/// NUTHATCH_* setting the caller happens to carry is not.
#[test]
fn a_nests_environment_file_replaces_the_callers() {
    use std::os::unix::fs::PermissionsExt;
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    let seen = c.dir.path().join("serve-env");
    let bin = c.dir.path().join("nuthatch");
    std::fs::write(
        &bin,
        format!(
            "#!/usr/bin/env bash\n\
             [ \"${{1:-}}\" != serve ] || env | grep '^NUTHATCH_' | sort >'{}'\n\
             exec '{}' \"$@\"\n",
            seen.display(),
            env!("CARGO_BIN_EXE_nuthatch")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = c.dir.path().join("nest.env");
    std::fs::write(
        &env,
        "# read from the unit\nNUTHATCH_SQL_MAX_CONCURRENCY=1\nNUTHATCH_SQL_MEMO_BYTES=1048576\n",
    )
    .unwrap();
    let gate = |extra_env: &[(&str, &str)]| {
        let mut cmd = Command::new(root().join("scripts/release-gate.sh"));
        cmd.args(["--passes", "1", "--env"])
            .arg(&env)
            .arg("--out")
            .arg(c.dir.path().join("out"))
            .arg(&bin)
            .arg(&c.nest)
            .arg(&set);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code(), text)
    };
    let (code, text) = gate(&[("NUTHATCH_MAX_RSS", "1GB")]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap(),
        "NUTHATCH_SQL_MAX_CONCURRENCY=1\nNUTHATCH_SQL_MEMO_BYTES=1048576\n",
        "{text}"
    );

    std::fs::write(&env, "RUST_LOG=debug\n").unwrap();
    let (code, text) = gate(&[]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("not a NUTHATCH_*=VALUE line"), "{text}");

    // An empty read is a fault; a unit that sets nothing says so, and serves on the defaults.
    std::fs::write(&env, "# read from the unit\n").unwrap();
    let (code, text) = gate(&[]);
    assert_eq!(code, Some(2), "{text}");
    assert!(text.contains("no NUTHATCH_* settings"), "{text}");

    std::fs::write(&env, "# read from the unit\n# none: the unit sets no NUTHATCH_* settings\n").unwrap();
    let (code, text) = gate(&[("NUTHATCH_MAX_RSS", "1GB")]);
    assert_eq!(code, Some(0), "{text}");
    assert_eq!(std::fs::read_to_string(&seen).unwrap(), "", "{text}");
}

/// The set lives in the private kittiwake repo, so the runner has no default to fall back on: an
/// unset `GATE_SET` is a setup fault that names the variable, before anything is fetched or posted.
#[test]
fn an_unset_gate_set_is_a_setup_fault_that_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let fakes = dir.path().join("fakes");
    std::fs::create_dir_all(&fakes).unwrap();
    let gh = fakes.join("gh");
    std::fs::write(&gh, FAKE_GH).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(dir.path().join("releases"), "v4.3.0-rc1 pre\n").unwrap();
    let mut cmd = Command::new(root().join("scripts/release-gate-run.sh"));
    cmd.arg("v4.3.0-rc1")
        .env(
            "PATH",
            format!("{}:{}", fakes.display(), std::env::var("PATH").unwrap()),
        )
        .env("FAKE_GH_DIR", dir.path())
        .env("GATE_STATE", dir.path().join("state"))
        .env_remove("GATE_SET")
        .env_remove("GATE_LOCK_HELD");
    let child = run_bounded(cmd, dir.path(), "runner");
    let (code, text) = finish(child, dir.path(), "runner", 30);
    assert_eq!(code, Some(2), "{text}");
    assert!(
        text.contains("GATE_SET is not set") && text.contains("nuthatch-gate/alloc-queries.tsv"),
        "{text}"
    );
    assert!(!dir.path().join("posted").exists(), "{text}");
    assert!(!dir.path().join("downloaded").exists(), "{text}");
}

/// Stands in for burrmill-bench's `gate-duck` (#1796): copies the answers a case wrote into
/// `$STUB_ANSWERS` to the output directory, and lists the pin's segments beside them, so the
/// script's comparison is tested without building DuckDB.
const STUB_DUCK: &str = r#"#!/usr/bin/env bash
set -euo pipefail
[ "$1" = gate-duck ] || { echo "stub: not gate-duck: $*" >&2; exit 3; }
mkdir -p "$4"
cp "$STUB_ANSWERS"/* "$4"/ 2>/dev/null || true
ls "$2/segments" >"$STUB_ANSWERS/../pin-segments"
"#;

impl Case {
    /// The answers the stub gives as DuckDB's: `<id>.json` a body, `<id>.err` a refusal,
    /// `views.err` a view DuckDB would not define.
    fn duck(&self, answers: &[(&str, &str)]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.dir.path().join("duck-answers");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (file, body) in answers {
            std::fs::write(dir.join(file), body).unwrap();
        }
        let stub = self.dir.path().join("gate-duck");
        std::fs::write(&stub, STUB_DUCK).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        stub
    }

    fn reference(&self, set: &Path, extra: &[&str], duck: Option<&Path>) -> (Output, String) {
        let mut cmd = Command::new(root().join("scripts/gate/reference.sh"));
        cmd.arg("--out")
            .arg(self.dir.path().join("ref-out"))
            .args(extra)
            .arg(env!("CARGO_BIN_EXE_nuthatch"))
            .arg(&self.nest)
            .arg(set)
            .env("STUB_ANSWERS", self.dir.path().join("duck-answers"))
            .env_remove("GATE_LOCK_HELD");
        match duck {
            Some(d) => cmd.env("GATE_DUCK", d),
            None => cmd.env_remove("GATE_DUCK"),
        };
        let out = cmd.output().expect("run reference.sh");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out, text)
    }

    fn pin_at_finalized(&self) {
        std::fs::write(
            self.nest.join("PROVENANCE"),
            format!("sealed_through={FINALIZED}\n"),
        )
        .unwrap();
    }
}

fn count_body(n: u64) -> String {
    format!(r#"{{"count":1,"rows":[{{"n":{n}}}]}}"#)
}

/// #1796: the binary's answer is compared with DuckDB's, not with a previous release's, in the
/// gate's canonical form. A different value fails with the first differing row. Under a top-level
/// ORDER BY the same rows in another order fail as an order that differs, naming the first row out
/// of place; a float 1e-13 off is equal; a volatile statement is held to its row count, reordered
/// or not.
#[test]
fn an_answer_that_differs_from_duckdbs_fails_and_is_named() {
    let c = case();
    let set = c.set(&[
        ("count", c.counts()),
        ("wrong", c.counts()),
        (
            "ordered",
            "SELECT k FROM (VALUES (1), (2), (3)) AS v(k) ORDER BY k".to_string(),
        ),
        (
            "ordered_wrong",
            "SELECT k FROM (VALUES (1), (2), (3)) AS v(k) ORDER BY k DESC".to_string(),
        ),
        (
            "unordered",
            "SELECT k FROM (VALUES (2), (3), (1)) AS v(k)".to_string(),
        ),
        ("float", "SELECT CAST(1 AS DOUBLE) / 3 AS x".to_string()),
        (
            "volatile",
            "SELECT k FROM (VALUES (1), (2)) AS v(k)".to_string(),
        ),
        (
            "volatile_ordered",
            "SELECT k FROM (VALUES (1), (2)) AS v(k) ORDER BY k".to_string(),
        ),
    ]);
    let body = std::fs::read_to_string(&set).unwrap();
    std::fs::write(&set, format!(
            "# volatile: volatile ties\n# volatile: volatile_ordered ties under its ORDER BY\n{body}"
        )).unwrap();
    c.pin_at_finalized();
    let duck = c.duck(&[
        ("count.json", &count_body(TRANSFERS)),
        ("wrong.json", &count_body(TRANSFERS - 1)),
        (
            "ordered.json",
            r#"{"count":3,"rows":[{"k":3},{"k":2},{"k":1}]}"#,
        ),
        (
            "ordered_wrong.json",
            r#"{"count":3,"rows":[{"k":3},{"k":2},{"k":4}]}"#,
        ),
        (
            "unordered.json",
            r#"{"count":3,"rows":[{"k":3},{"k":2},{"k":1}]}"#,
        ),
        (
            "float.json",
            r#"{"count":1,"rows":[{"x":0.3333333333334333}]}"#,
        ),
        ("volatile.json", r#"{"count":2,"rows":[{"k":7},{"k":8}]}"#),
        (
            "volatile_ordered.json",
            r#"{"count":2,"rows":[{"k":2},{"k":1}]}"#,
        ),
    ]);
    let (out, text) = c.reference(&set, &[], Some(&duck));
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(line_for(&text, "count").starts_with("ok "), "{text}");
    let wrong = line_for(&text, "wrong");
    assert!(
        wrong.starts_with("FAIL ") && wrong.contains("differs from DuckDB"),
        "{wrong}"
    );
    assert!(
        text.contains(&format!("burrmill: {{\"n\":{TRANSFERS}}}"))
            && text.contains(&format!("duckdb:   {{\"n\":{}}}", TRANSFERS - 1)),
        "the first differing row of each side:\n{text}"
    );
    assert!(
        line_for(&text, "ordered_wrong").contains("differs from DuckDB (rows compared in order)"),
        "{text}"
    );
    assert!(
        line_for(&text, "ordered").starts_with("FAIL ")
            && line_for(&text, "ordered").contains("order differs from DuckDB"),
        "{text}"
    );
    assert!(
        text.contains("first row out of place, row 1:")
            && text.contains("burrmill: {\"k\":1}")
            && text.contains("duckdb:   {\"k\":3}"),
        "the first row out of place:\n{text}"
    );
    assert!(
        line_for(&text, "volatile_ordered").starts_with("ok "),
        "a volatile statement reordered under its ORDER BY passes on its count:\n{text}"
    );
    assert!(line_for(&text, "unordered").starts_with("ok "), "{text}");
    assert!(line_for(&text, "float").starts_with("ok "), "{text}");
    assert!(
        line_for(&text, "volatile").contains("volatile, so compared on its row count"),
        "{text}"
    );
    assert!(
        text.contains("3 match DuckDB, 2 compared on row count only, 3 differ"),
        "{text}"
    );
    assert!(
        text.contains("RESULT: FAIL - differs from DuckDB: wrong, ordered, ordered_wrong"),
        "{text}"
    );
}

/// A statement DuckDB will not run is listed with its reason and not compared; one the binary does
/// not answer fails. A reason that is a view DuckDB would not define names the view.
#[test]
fn a_statement_duckdb_will_not_run_is_listed_and_one_the_binary_refuses_fails() {
    let c = case();
    let set = c.set(&[
        ("count", c.counts()),
        ("dialect", "SELECT 1 AS one".to_string()),
        (
            "viewed",
            format!(
                "SELECT count(*) AS n FROM \"{}\" AS gate_only_here",
                c.table
            ),
        ),
    ]);
    c.pin_at_finalized();
    let duck = c.duck(&[
        ("count.json", &count_body(TRANSFERS)),
        (
            "dialect.err",
            "Parser Error: syntax error at or near \"one\"\n",
        ),
        (
            "viewed.err",
            "Catalog Error: Table with name gate_only_here does not exist!\n",
        ),
        (
            "views.err",
            "gate_only_here\t90-x.sql\tBinder Error: no such function\n",
        ),
    ]);
    let (out, text) = c.reference(&set, &[], Some(&duck));
    assert_eq!(out.status.code(), Some(0), "{text}");
    let dialect = line_for(&text, "dialect");
    assert!(
        dialect.starts_with("skip ") && dialect.contains("Parser Error"),
        "{dialect}"
    );
    assert!(
        line_for(&text, "viewed").contains(
            "it reads gate_only_here (90-x.sql), which DuckDB will not define: Binder Error"
        ),
        "{text}"
    );
    assert!(text.contains("2 not compared"), "{text}");
    assert!(text.contains("RESULT: PASS"), "{text}");

    let set = c.set(&[
        ("count", c.counts()),
        ("refused", "SELECT a FROM no_such_table".to_string()),
    ]);
    let duck = c.duck(&[
        ("count.json", &count_body(TRANSFERS)),
        ("refused.json", r#"{"count":1,"rows":[{"a":1}]}"#),
    ]);
    let (out, text) = c.reference(&set, &[], Some(&duck));
    assert_eq!(out.status.code(), Some(1), "{text}");
    let refused = line_for(&text, "refused");
    assert!(
        refused.starts_with("FAIL ")
            && refused.contains("the binary did not answer")
            && refused.contains("no_such_table"),
        "{refused}"
    );
    assert!(
        text.contains("RESULT: FAIL - not answered: refused"),
        "{text}"
    );
}

/// The pin holds only segments sealed at or below it, so a segment past it reaches neither engine:
/// the one sealed segment is listed a second time as a block past the pin, and the binary still
/// counts every transfer once. A pin below every segment is a setup fault.
#[test]
fn only_segments_at_or_below_the_sealed_pin_are_read() {
    let c = case();
    let manifest_path = c.nest.join("segments/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let segs = manifest["tables"][&c.table].as_array_mut().unwrap();
    let mut past = segs[0].clone();
    let file = past["file"].as_str().unwrap().to_string();
    let copy = file.replace(".parquet", "-past.parquet");
    std::fs::copy(
        c.nest.join("segments").join(&file),
        c.nest.join("segments").join(&copy),
    )
    .unwrap();
    past["file"] = copy.clone().into();
    past["from_block"] = (FINALIZED + 1).into();
    past["to_block"] = (FINALIZED + 1).into();
    segs.push(past);
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();

    let set = c.set(&[("count", c.counts())]);
    let duck = c.duck(&[("count.json", &count_body(TRANSFERS))]);
    let pin = FINALIZED.to_string();
    let (out, text) = c.reference(&set, &["--sealed-through", &pin], Some(&duck));
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(line_for(&text, "count").starts_with("ok "), "{text}");
    let pinned = std::fs::read_to_string(c.dir.path().join("pin-segments")).unwrap();
    assert!(
        pinned.contains(&file) && !pinned.contains(&copy),
        "the pin's segments: {pinned}"
    );

    // Pinned past it, it is read and the count doubles: the case above does not pass by accident.
    let past = (FINALIZED + 1).to_string();
    let (out, text) = c.reference(&set, &["--sealed-through", &past], Some(&duck));
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains(&format!("burrmill: {{\"n\":{}}}", 2 * TRANSFERS)),
        "{text}"
    );

    let (out, text) = c.reference(&set, &["--sealed-through", "0"], Some(&duck));
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("no sealed segment at or below block 0"),
        "{text}"
    );
}

/// Without a pin or a DuckDB to compare with there is no verdict to give: exit 2, never a pass.
#[test]
fn no_pin_or_no_duckdb_is_a_setup_fault() {
    let c = case();
    let set = c.set(&[("count", c.counts())]);
    let duck = c.duck(&[("count.json", &count_body(TRANSFERS))]);
    let (out, text) = c.reference(&set, &[], Some(&duck));
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(
        text.contains("no PROVENANCE with sealed_through="),
        "{text}"
    );
    c.pin_at_finalized();
    let (out, text) = c.reference(&set, &[], None);
    assert_eq!(out.status.code(), Some(2), "{text}");
    assert!(text.contains("GATE_DUCK is not set"), "{text}");
}

/// With GATE_DUCK set the runner checks the candidate against DuckDB after the gate and posts that
/// verdict as its own status, so a wrong answer production shares still turns something red.
#[test]
fn the_runner_posts_the_duckdb_reference_as_its_own_status() {
    let c = case();
    let set = c.set(&[("answers", c.counts())]);
    std::fs::write(
        c.nest.join("PROVENANCE"),
        format!("version=4.2.0\nsealed_through={FINALIZED}\n"),
    )
    .unwrap();
    let r = releases(&c, &[("v4.3.0", "full", 0), ("v4.2.0", "full", 0)], &[]);
    let duck = c.duck(&[("answers.json", &count_body(TRANSFERS + 1))]);
    let stub_answers = c.dir.path().join("duck-answers");
    let (code, text) = r.runner(
        &c,
        &set,
        &["v4.3.0"],
        &[
            ("GATE_DUCK", duck.to_str().unwrap()),
            ("STUB_ANSWERS", stub_answers.to_str().unwrap()),
        ],
    );
    assert_eq!(code, Some(1), "{text}");
    let posted = r.read("posted-ctx");
    assert!(
        posted_for(&posted, "release-gate/alloc-nest")
            .iter()
            .any(|l| l.contains("sha-v4.3.0 success 1 of 1 answered")),
        "the relative gate still passes:\n{posted}"
    );
    let duck = posted_for(&posted, "release-gate/duckdb");
    assert!(
        duck.last()
            .is_some_and(|l| l.contains("sha-v4.3.0 failure differs from DuckDB: answers")),
        "{posted}\n{text}"
    );
}

/// With GATE_NESTS each nest is checked against DuckDB on its own copy and set, under its own
/// environment, and posts release-gate/duckdb-<name>, so a red one names its nest. A copy with no
/// sealed pin in its PROVENANCE is said to have none and posts nothing.
#[test]
fn each_nest_posts_its_own_duckdb_reference() {
    let c = case();
    let alloc = [("answers", c.counts())];
    let qos = [("qos_count", c.counts())];
    let dips = [("dips_count", c.counts())];
    let conf = nests_conf(
        &c,
        &[
            ("alloc-nest", &alloc, ENV_TWO, "none"),
            ("qos-nest", &qos, ENV_ONE, "none"),
            ("dips-nest", &dips, ENV_ONE, "none"),
        ],
    );
    for n in ["alloc-nest", "qos-nest"] {
        std::fs::write(
            c.dir.path().join(n).join("PROVENANCE"),
            format!("version=4.2.0\nsealed_through={FINALIZED}\n"),
        )
        .unwrap();
    }
    let r = releases(&c, &[("v4.3.0", "full", 0), ("v4.2.0", "full", 0)], &[]);
    let duck = c.duck(&[
        ("answers.json", &count_body(TRANSFERS)),
        ("qos_count.json", &count_body(TRANSFERS + 1)),
        ("dips_count.json", &count_body(TRANSFERS)),
    ]);
    let stub_answers = c.dir.path().join("duck-answers");
    let set = c.set(&[("unused", c.counts())]);
    let (code, text) = r.runner(
        &c,
        &set,
        &["v4.3.0"],
        &[
            ("GATE_NESTS", conf.to_str().unwrap()),
            ("GATE_DUCK", duck.to_str().unwrap()),
            ("STUB_ANSWERS", stub_answers.to_str().unwrap()),
        ],
    );
    assert_eq!(code, Some(1), "{text}");
    let posted = r.read("posted-ctx");
    let alloc = posted_for(&posted, "release-gate/duckdb-alloc-nest");
    assert!(
        alloc.last().is_some_and(|l| l.contains(" success ")),
        "{posted}\n{text}"
    );
    let qos = posted_for(&posted, "release-gate/duckdb-qos-nest");
    assert!(
        qos.last()
            .is_some_and(|l| l.contains(" failure differs from DuckDB: qos_count")),
        "{posted}\n{text}"
    );
    assert!(
        posted_for(&posted, "release-gate/duckdb-dips-nest").is_empty()
            && posted_for(&posted, "release-gate/duckdb").is_empty(),
        "{posted}"
    );
    assert!(
        text.contains("[dips-nest] release-gate-run: dips-nest: no sealed_through in"),
        "{text}"
    );
    let qos_env = c.dir.path().join("qos-nest.env");
    assert!(
        text.lines()
            .any(|l| l.starts_with("[qos-nest] reference: budget")
                && l.contains("NUTHATCH_SQL_MAX_CONCURRENCY=1")),
        "the reference runs under the nest's environment, {}:\n{text}",
        qos_env.display()
    );
}
