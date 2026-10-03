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

    /// Writes a baseline from the right answers, then gates the wrong ones against it.
    fn against_a_wrong_candidate(&self, set: &Path) -> (Output, String) {
        let baseline = self.dir.path().join("baseline.tsv");
        let base_out = self.dir.path().join("baseline-out");
        self.probe(false);
        let (out, text) = self.gate(
            set,
            &[
                "--out",
                base_out.to_str().unwrap(),
                "--write-baseline",
                baseline.to_str().unwrap(),
            ],
            &[],
        );
        assert_eq!(out.status.code(), Some(0), "the baseline run:\n{text}");
        self.probe(true);
        self.gate(set, &["--baseline", baseline.to_str().unwrap()], &[])
    }
}

/// #1772: a statement that answers, but not what production answered, fails and is named, with
/// the first row at which the two differ. Row order is part of the answer only under a top-level
/// ORDER BY: a statement without one passes with its rows reversed (the quoted ORDER BY is a
/// literal, not a clause), and a float 1e-13 off is equal to 12 significant digits.
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
        ("float", "SELECT third FROM gate_probe".to_string()),
        (
            "volatile",
            "SELECT block FROM gate_probe WHERE block < 10".to_string(),
        ),
    ]);
    // Volatile is held to its row count, and block 5 answered as 50 is a row fewer.
    let body = std::fs::read_to_string(&set).unwrap();
    std::fs::write(&set, format!("# volatile: volatile a row short\n{body}")).unwrap();
    let (out, text) = c.against_a_wrong_candidate(&set);
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
        volatile.starts_with("FAIL ") && volatile.contains("answer differs: 7 rows against the baseline's 8"),
        "{volatile}"
    );
    assert!(
        text.contains("RESULT: FAIL - answer differs: value, ordered, volatile"),
        "{text}"
    );
}

/// A statement tagged volatile in the set is held to its row count, so a different answer with
/// the same number of rows passes; an untagged one in the same run would not.
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
    let (out, text) = c.against_a_wrong_candidate(&set);
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
/// tag's commit is `sha-<tag>`. Posted statuses are appended to `posted`, downloads to `downloaded`.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
set -euo pipefail
d=$FAKE_GH_DIR
case "$1 $2" in
  "api -X")
    sha=${4##*/} state="" desc=""
    shift 4
    while [ $# -ge 2 ]; do
      case "$2" in state=*) state=${2#state=} ;; description=*) desc=${2#description=} ;; esac
      shift 2
    done
    echo "$sha $state $desc" >>"$d/posted" ;;
  "api "*)
    case "$2" in
      */statuses) sha=${2%/statuses}; sha=${sha##*/}; cat "$d/status-$sha" 2>/dev/null || echo none ;;
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
