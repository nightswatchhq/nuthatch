//! #1717 - kill the real binary at a named instant in the seal path, restart it, and count.
//!
//! `e2e_crash_safety.rs` calls `seal_range` twice and compares. That is a function, not a death:
//! #1631 and #1632 were both orderings between a segment, its manifest entry and the watermark that
//! only a process dying between two of them can show. Here the binary is built with
//! `--features crash-points`, run against the fixture chain over HTTP, aborted at a point the seal
//! path names, and started again on the same directory. Whatever it sealed before dying and whatever
//! it seals after, every transfer must be readable exactly once.
//!
//! Each case also proves its point fired. A point the run never reaches would leave a restart that
//! passes trivially.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
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

/// A fixture chain and an `init`ed nest pointed at it.
/// The fixture chain over HTTP, pinned to the tip and finality the cases expect.
fn fixture_chain() -> (Reaped, u16) {
    let rpc_port = free_port();
    let rpc = Reaped(
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
    (rpc, rpc_port)
}

/// `init` a nest at `nest`, pointed at the fixture chain.
fn init_nest(nest: &Path, rpc_port: u16) {
    std::fs::create_dir_all(nest.parent().unwrap()).unwrap();
    let abi = nest.with_extension("abi.json");
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
}

/// A fixture chain and an `init`ed nest pointed at it.
fn chain_and_nest(dir: &Path) -> (Reaped, PathBuf) {
    let (rpc, rpc_port) = fixture_chain();
    let nest = dir.join("nest");
    init_nest(&nest, rpc_port);
    (rpc, nest)
}

fn dev(nest: &Path, port: u16, crash_at: Option<&str>, log: &Path) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nuthatch"));
    cmd.args(["dev", "--dir"])
        .arg(nest)
        .args(["--listen", &format!("127.0.0.1:{port}"), "--seal-direct"])
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(log).unwrap());
    match crash_at {
        Some(point) => cmd.env("NUTHATCH_CRASH_AT", point),
        None => cmd.env_remove("NUTHATCH_CRASH_AT"),
    };
    cmd.spawn().expect("spawn dev")
}

fn read(path: &Path) -> String {
    let mut s = String::new();
    let _ = std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut s));
    s
}

fn sql(api: &str, q: &str) -> Option<serde_json::Value> {
    let body = get(&format!("{api}/sql?q={}", url_encode(q)))?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    v["error"].is_null().then_some(v)
}

/// Die at `point`, restart, and require every transfer exactly once.
fn survives(point: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (_rpc, nest) = chain_and_nest(dir.path());

    let port = free_port();
    let first_log = dir.path().join("first.log");
    let mut first = Reaped(dev(&nest, port, Some(point), &first_log));
    let status = poll(&format!("the nest to die at {point}"), 120, || {
        first.0.try_wait().ok().flatten()
    });
    assert!(
        !status.success() && read(&first_log).contains(&format!("crash point {point}: aborting")),
        "the run was meant to abort at `{point}` and did not reach it (exit {status}), so a restart \
         would prove nothing. Its log:\n{}",
        read(&first_log)
    );
    drop(first);

    let port = free_port();
    let second_log = dir.path().join("second.log");
    let _second = Reaped(dev(&nest, port, None, &second_log));
    let api = format!("http://127.0.0.1:{port}");
    poll("the restarted nest to seal the history", 120, || {
        sql(&api, "SELECT 1")?["provenance"]["sealed_through"]
            .as_u64()
            .filter(|&n| n >= FINALIZED)
    });
    let tables: serde_json::Value = serde_json::from_str(&poll("the tables listing", 30, || {
        get(&format!("{api}/tables"))
    }))
    .unwrap();
    let table = tables["tables"][0]["name"]
        .as_str()
        .or_else(|| tables["tables"][0]["table"].as_str())
        .expect("a table")
        .to_string();
    let answer = poll("the count", 30, || {
        sql(
            &api,
            &format!(
                "SELECT count(*) AS n, count(DISTINCT block_number || ':' || log_index) AS d \
                 FROM \"{table}\""
            ),
        )
    });
    let row = &answer["rows"][0];
    let (n, d) = (row["n"].as_u64(), row["d"].as_u64());
    assert_eq!(
        (n, d),
        (Some(TRANSFERS), Some(TRANSFERS)),
        "after dying at `{point}` and restarting, `{table}` holds {n:?} rows over {d:?} distinct \
         logs, against {TRANSFERS} transfers. More rows than logs is a double count; fewer distinct \
         logs is a loss. Restart log:\n{}",
        read(&second_log)
    );
}

#[test]
fn a_death_before_the_manifest_entry_loses_and_doubles_nothing() {
    survives("seal:before-manifest");
}

#[test]
fn a_death_between_the_manifest_and_the_watermark_loses_and_doubles_nothing() {
    survives("seal:after-manifest");
}

#[test]
fn a_death_just_after_the_seal_direct_watermark_loses_and_doubles_nothing() {
    survives("seal-direct:after-watermark");
}

/// The control: a point name nothing carries must not abort. Without it a harness that killed the
/// process on any `NUTHATCH_CRASH_AT` would pass every case above for the wrong reason.
#[test]
fn an_unnamed_point_does_not_abort() {
    let dir = tempfile::tempdir().unwrap();
    let (_rpc, nest) = chain_and_nest(dir.path());
    let port = free_port();
    let log = dir.path().join("dev.log");
    let mut run = Reaped(dev(&nest, port, Some("no-such-point"), &log));
    let api = format!("http://127.0.0.1:{port}");
    poll("the nest to seal the history", 120, || {
        sql(&api, "SELECT 1")?["provenance"]["sealed_through"]
            .as_u64()
            .filter(|&n| n >= FINALIZED)
    });
    assert!(
        run.0.try_wait().unwrap().is_none(),
        "the nest exited under a point name nothing carries:\n{}",
        read(&log)
    );
}

/// Run `dev` on `nest` until it has sealed the fixture history, then stop it.
fn seal_once(nest: &Path, log: &Path) {
    let port = free_port();
    let mut run = Reaped(dev(nest, port, None, log));
    let api = format!("http://127.0.0.1:{port}");
    poll("the nest to seal the history", 120, || {
        sql(&api, "SELECT 1")?["provenance"]["sealed_through"]
            .as_u64()
            .filter(|&n| n >= FINALIZED)
    });
    let _ = Command::new("kill")
        .args(["-TERM", &run.0.id().to_string()])
        .status();
    let _ = run.0.wait();
}

fn prune(runtime: &Path, crash_at: Option<&str>) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_nuthatch"));
    cmd.args(["prune", "--yes", "--dir"]).arg(runtime);
    match crash_at {
        Some(point) => cmd.env("NUTHATCH_CRASH_AT", point),
        None => cmd.env_remove("NUTHATCH_CRASH_AT"),
    };
    cmd.output().expect("run prune")
}

fn segment_files(runtime: &Path) -> usize {
    std::fs::read_dir(runtime.join("segments"))
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
        .count()
}

/// A prune dies after removing an unmounted dataset and before its orphan pass. The surviving mount
/// must read every row, and the next prune must reclaim the segment only the removed dataset named.
/// Before #1717 it answered "Nothing to prune" and kept that segment for good.
#[test]
fn a_prune_killed_before_its_orphan_pass_loses_nothing_and_is_finished_by_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = dir.path().join("runtime");
    let (_rpc, rpc_port) = fixture_chain();
    // Two nests over one chain: `usdc` from genesis, `late` from block 5, so they share no segment
    // and each owns one. `late` is the one unmounted and pruned.
    for name in ["usdc", "late"] {
        let nest = runtime.join("nests").join(name);
        init_nest(&nest, rpc_port);
        if name == "late" {
            let toml = nest.join("nuthatch.toml");
            let raw = std::fs::read_to_string(&toml).unwrap();
            std::fs::write(&toml, format!("{raw}start_block = 5\n")).unwrap();
        }
        seal_once(&nest, &dir.path().join(format!("{name}.log")));
    }
    std::fs::write(
        runtime.join("mounts.toml"),
        format!(
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
             rpc_urls = [\"http://127.0.0.1:{rpc_port}/\"]\nnests = [\"usdc\", \"late\"]\n"
        ),
    )
    .unwrap();
    let migrate = Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args(["migrate", "--dir"])
        .arg(&runtime)
        .output()
        .unwrap();
    assert!(
        migrate.status.success(),
        "{}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    assert_eq!(
        segment_files(&runtime),
        2,
        "premise: each dataset owns a segment"
    );

    let table = std::fs::read_to_string(runtime.join("mounts.toml")).unwrap();
    let kept = table
        .split("[[mounts]]")
        .filter(|block| !block.contains("alias = \"late\""))
        .collect::<Vec<_>>()
        .join("[[mounts]]");
    assert_ne!(kept, table, "premise: `late` had a mount record to remove");
    std::fs::write(runtime.join("mounts.toml"), kept).unwrap();

    let killed = prune(&runtime, Some("prune:after-datasets"));
    let said = String::from_utf8_lossy(&killed.stderr);
    assert!(
        !killed.status.success() && said.contains("crash point prune:after-datasets: aborting"),
        "the prune was meant to die between its two passes:\n{said}"
    );
    assert_eq!(
        segment_files(&runtime),
        2,
        "premise: the dead prune left the removed dataset's segment behind"
    );

    let finished = prune(&runtime, None);
    assert!(finished.status.success());
    assert_eq!(
        segment_files(&runtime),
        1,
        "the next prune left the segment no dataset references:\n{}",
        String::from_utf8_lossy(&finished.stdout)
    );

    let port = free_port();
    let log = dir.path().join("runtime.log");
    let _rt = Reaped(dev(&runtime, port, None, &log));
    let api = format!("http://127.0.0.1:{port}/usdc");
    let answer = poll("the surviving mount to answer", 120, || {
        sql(&api, "SELECT count(*) AS n FROM c0__transfer")
    });
    assert_eq!(
        answer["rows"][0]["n"].as_u64(),
        Some(TRANSFERS),
        "the surviving mount lost rows to the prune:\n{}",
        read(&log)
    );
}
