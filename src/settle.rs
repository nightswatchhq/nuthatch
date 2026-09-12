//! RFC-0046 S3: the back office.
//!
//! The nest already served. This process drains `authorisations.jsonl`, hands each row to an
//! operator `--exec` (or a test double), and records what happened. It holds no query path and
//! does not run inside `dev`/`serve`.

use anyhow::{bail, Context, Result};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::counter::{LOG, PAYERS, SPENT};

/// What happened when the back office tried to move money.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Settled,
    /// The token (or the operator script) refused. Do not retry; the payer's promise did not clear.
    Failed(String),
    /// Transport, a missing binary, a crash. Leave the row pending.
    Deferred(String),
}

pub trait Submit {
    fn submit(&self, row: &serde_json::Value) -> Outcome;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub settled: u64,
    pub failed: u64,
    pub deferred: u64,
    pub remaining: u64,
}

/// Drain pending authorisations. Settled and failed rows leave the pending log and land in
/// `spent.jsonl` so the nest still refuses the nonce. Deferred rows stay pending.
pub fn drain(dir: &Path, submit: &dyn Submit) -> Result<DrainReport> {
    let settler_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("settler.lock"))?;
    settler_lock
        .try_lock()
        .context("another settler is already running")?;
    let pending_path = dir.join(LOG);
    let pending = {
        let _guard = crate::counter::lock_queue(dir)?;
        read_jsonl(&pending_path)?
    };
    let mut completed = std::collections::BTreeSet::new();
    let mut report = DrainReport::default();
    for row in pending {
        // `spent.jsonl` is an outcome journal as well as the counter's compact replay set. If a
        // prior run reached the durable journal but crashed before rewriting the pending queue,
        // finalise that result. Calling the operator command a second time would be a second
        // attempt to move the same money.
        let recorded = recorded_outcome(dir, &row)?;
        let was_recorded = recorded.is_some();
        let outcome = recorded.unwrap_or_else(|| submit.submit(&row));
        match outcome {
            Outcome::Deferred(_) => {
                report.deferred += 1;
            }
            Outcome::Settled => {
                let _guard = crate::counter::lock_queue(dir)?;
                if !was_recorded {
                    append_jsonl(&dir.join(SPENT), &spent_row(&row, "settled", ""))?;
                }
                append_jsonl(&dir.join(PAYERS), &payer_row(&row, "settled", ""))?;
                report.settled += 1;
                completed.insert(authorisation_key(&row));
            }
            Outcome::Failed(detail) => {
                let _guard = crate::counter::lock_queue(dir)?;
                if !was_recorded {
                    append_jsonl(&dir.join(SPENT), &spent_row(&row, "failed", &detail))?;
                }
                append_jsonl(&dir.join(PAYERS), &payer_row(&row, "failed", &detail))?;
                report.failed += 1;
                completed.insert(authorisation_key(&row));
            }
        }
    }
    // New payments may have arrived while the external command ran. Re-read under the same
    // cross-process lock as the server's append and remove only rows completed by this run.
    let _guard = crate::counter::lock_queue(dir)?;
    let mut kept = read_jsonl(&pending_path)?;
    kept.retain(|row| !completed.contains(&authorisation_key(row)));
    write_jsonl(&pending_path, &kept)?;
    report.remaining = kept.len() as u64;
    Ok(report)
}

fn authorisation_key(row: &serde_json::Value) -> [String; 3] {
    ["network", "payer", "nonce"]
        .map(|key| row[key].as_str().expect("validated pending row").to_owned())
}

/// `--exec`: stdin is one pending row. Exit 0 settled, 2 failed, anything else deferred.
/// The command must be idempotent by `(network, payer, nonce)`: a process can die after external
/// settlement succeeds but before its local journal write. On retry the command must reconcile
/// that authorisation's prior transaction and return its outcome, without submitting a new debit.
pub struct ExecSubmit {
    pub command: String,
}

impl Submit for ExecSubmit {
    fn submit(&self, row: &serde_json::Value) -> Outcome {
        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&self.command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Outcome::Deferred(format!("spawn: {e}")),
        };
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(error) = writeln!(stdin, "{row}") {
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::Deferred(format!("write authorisation to exec: {error}"));
            }
        }
        match child.wait_with_output() {
            Ok(out) if out.status.success() => Outcome::Settled,
            Ok(out) if out.status.code() == Some(2) => {
                let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Outcome::Failed(if detail.is_empty() {
                    "exec exited 2".into()
                } else {
                    detail
                })
            }
            Ok(out) => Outcome::Deferred(format!(
                "exec exited {}",
                out.status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into())
            )),
            Err(e) => Outcome::Deferred(format!("wait: {e}")),
        }
    }
}

pub fn run(args: crate::cli::SettleArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    if args.dry_run && args.exec.is_some() {
        bail!("pass --dry-run or --exec, not both");
    }
    if args.dry_run {
        let pending = read_jsonl(&dir.join(LOG))?;
        println!("{} pending authorisation(s)", pending.len());
        for row in &pending {
            println!("{row}");
        }
        return Ok(());
    }
    let Some(command) = args.exec else {
        bail!("nuthatch settle needs --exec (or --dry-run to list the queue)");
    };
    let report = drain(&dir, &ExecSubmit { command })?;
    println!(
        "settled {}  failed {}  deferred {}  remaining {}",
        report.settled, report.failed, report.deferred, report.remaining
    );
    Ok(())
}

fn spent_row(row: &serde_json::Value, outcome: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "network": row.get("network").cloned().unwrap_or(serde_json::Value::Null),
        "payer": row.get("payer").cloned().unwrap_or(serde_json::Value::Null),
        "nonce": row.get("nonce").cloned().unwrap_or(serde_json::Value::Null),
        "outcome": outcome,
        "detail": detail,
    })
}

fn recorded_outcome(dir: &Path, row: &serde_json::Value) -> Result<Option<Outcome>> {
    let path = dir.join(SPENT);
    let file = match std::fs::File::open(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        Ok(file) => file,
    };
    for line in std::io::BufReader::new(file).lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        let recorded: serde_json::Value =
            serde_json::from_str(&line).with_context(|| format!("parse {}", path.display()))?;
        if same_authorisation(&recorded, row) {
            return Ok(
                match recorded.get("outcome").and_then(serde_json::Value::as_str) {
                    // Old spent rows were created before S3, so carry no external-result claim.
                    None => None,
                    Some("settled") => Some(Outcome::Settled),
                    Some("failed") => Some(Outcome::Failed(
                        recorded
                            .get("detail")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("recorded failure")
                            .to_owned(),
                    )),
                    Some(other) => bail!("unknown recorded settlement outcome {other:?}"),
                },
            );
        }
    }
    Ok(None)
}

fn same_authorisation(left: &serde_json::Value, right: &serde_json::Value) -> bool {
    ["network", "payer", "nonce"]
        .into_iter()
        .all(|key| left.get(key) == right.get(key))
}

fn payer_row(row: &serde_json::Value, outcome: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({
        "network": row.get("network").cloned().unwrap_or(serde_json::Value::Null),
        "payer": row.get("payer").cloned().unwrap_or(serde_json::Value::Null),
        "nonce": row.get("nonce").cloned().unwrap_or(serde_json::Value::Null),
        "outcome": outcome,
        "detail": detail,
    })
}

fn read_jsonl(path: &Path) -> Result<Vec<serde_json::Value>> {
    let file = match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        Ok(f) => f,
    };
    let mut out = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }
        // This is a money queue, not an opportunistic event feed. Skipping a corrupt line would
        // quietly forget an authorisation the nest has already honoured. Stop and let the
        // operator repair the file instead.
        let row =
            serde_json::from_str(&line).with_context(|| format!("parse {}", path.display()))?;
        validate_row(&row).with_context(|| format!("validate {}", path.display()))?;
        out.push(row);
    }
    Ok(out)
}

fn append_jsonl(path: &Path, row: &serde_json::Value) -> Result<()> {
    let existed = path
        .try_exists()
        .with_context(|| format!("stat {}", path.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("append {}", path.display()))?;
    writeln!(file, "{row}").with_context(|| format!("write {}", path.display()))?;
    file.sync_data()
        .with_context(|| format!("sync {}", path.display()))?;
    // A durable file body is not enough for a newly-created outcome file. Its directory entry
    // must survive the same crash boundary as the queue rewrite below.
    if !existed {
        sync_parent(path)?;
    }
    Ok(())
}

fn write_jsonl(path: &Path, rows: &[serde_json::Value]) -> Result<()> {
    if rows.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => {
                sync_parent(path)?;
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
    }
    let tmp = path.with_extension("jsonl.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        for row in rows {
            writeln!(file, "{row}").with_context(|| format!("write {}", tmp.display()))?;
        }
        file.sync_data()
            .with_context(|| format!("sync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", tmp.display()))?;
    sync_parent(path)?;
    Ok(())
}

fn validate_row(row: &serde_json::Value) -> Result<()> {
    for key in ["network", "payer", "nonce", "authorisation"] {
        if row.get(key).and_then(serde_json::Value::as_str).is_none() {
            bail!("pending row has no string {key}");
        }
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .context("queue path has no parent directory")?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("sync directory {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        answers: std::sync::Mutex<std::collections::VecDeque<Outcome>>,
    }

    impl Submit for Scripted {
        fn submit(&self, _row: &serde_json::Value) -> Outcome {
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Outcome::Deferred("empty script".into()))
        }
    }

    fn row(payer: &str, nonce: &str) -> serde_json::Value {
        serde_json::json!({
            "network": "testnet",
            "payer": payer,
            "nonce": nonce,
            "authorisation": "hdr",
        })
    }

    #[test]
    fn a_settled_row_leaves_the_queue_and_stays_spent() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(&dir.path().join(LOG), &[row("0xaa", "0x01")]).unwrap();
        let report = drain(
            dir.path(),
            &Scripted {
                answers: std::sync::Mutex::new(vec![Outcome::Settled].into()),
            },
        )
        .unwrap();
        assert_eq!(
            report,
            DrainReport {
                settled: 1,
                failed: 0,
                deferred: 0,
                remaining: 0
            }
        );
        assert!(
            !dir.path().join(LOG).exists(),
            "an empty queue is no file, not an empty one"
        );
        assert!(crate::counter::nonce_is_spent(dir.path(), "testnet", "0xaa", "0x01").unwrap());
        let payers = std::fs::read_to_string(dir.path().join(PAYERS)).unwrap();
        assert!(payers.contains("\"outcome\":\"settled\""), "{payers}");
    }

    #[test]
    fn a_failed_row_is_spent_and_named_on_the_payer() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(&dir.path().join(LOG), &[row("0xaa", "0x01")]).unwrap();
        drain(
            dir.path(),
            &Scripted {
                answers: std::sync::Mutex::new(
                    vec![Outcome::Failed("insufficient funds".into())].into(),
                ),
            },
        )
        .unwrap();
        assert!(crate::counter::nonce_is_spent(dir.path(), "testnet", "0xaa", "0x01").unwrap());
        assert!(crate::counter::payer_has_failed(dir.path(), "testnet", "0xaa").unwrap());
        assert!(!dir.path().join(LOG).exists());
    }

    #[test]
    fn a_deferred_row_stays_pending_and_is_not_a_credit_hit() {
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(&dir.path().join(LOG), &[row("0xaa", "0x01")]).unwrap();
        let report = drain(
            dir.path(),
            &Scripted {
                answers: std::sync::Mutex::new(vec![Outcome::Deferred("rpc down".into())].into()),
            },
        )
        .unwrap();
        assert_eq!(report.deferred, 1);
        assert_eq!(report.remaining, 1);
        assert!(crate::counter::nonce_is_spent(dir.path(), "testnet", "0xaa", "0x01").unwrap());
        assert!(!crate::counter::payer_has_failed(dir.path(), "testnet", "0xaa").unwrap());
        let pending = std::fs::read_to_string(dir.path().join(LOG)).unwrap();
        assert!(pending.contains("0x01"), "{pending}");
    }

    #[test]
    fn exec_exit_codes_are_the_three_outcomes() {
        let ok = ExecSubmit {
            command: "cat >/dev/null; exit 0".into(),
        };
        assert_eq!(ok.submit(&row("0xaa", "0x01")), Outcome::Settled);
        let no = ExecSubmit {
            command: "cat >/dev/null; echo insufficient >&2; exit 2".into(),
        };
        match no.submit(&row("0xaa", "0x01")) {
            Outcome::Failed(d) => assert!(d.contains("insufficient"), "{d}"),
            other => panic!("{other:?}"),
        }
        let miss = ExecSubmit {
            command: "cat >/dev/null; exit 7".into(),
        };
        match miss.submit(&row("0xaa", "0x01")) {
            Outcome::Deferred(d) => assert!(d.contains("7"), "{d}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_corrupt_pending_row_stops_the_drain_without_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LOG), b"not json\n").unwrap();
        let err = drain(
            dir.path(),
            &Scripted {
                answers: std::sync::Mutex::new(Vec::new().into()),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("parse"), "{err:#}");
        assert_eq!(std::fs::read(dir.path().join(LOG)).unwrap(), b"not json\n");
    }

    #[test]
    fn an_incomplete_pending_row_stops_the_drain_without_submitting_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(LOG), b"{\"network\":\"testnet\"}\n").unwrap();
        let err = drain(
            dir.path(),
            &Scripted {
                answers: std::sync::Mutex::new(Vec::new().into()),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("validate"), "{err:#}");
    }

    #[test]
    fn an_outcome_journalled_before_a_crash_is_not_submitted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let pending = row("0xaa", "0x01");
        write_jsonl(&dir.path().join(LOG), std::slice::from_ref(&pending)).unwrap();
        append_jsonl(&dir.path().join(SPENT), &spent_row(&pending, "settled", "")).unwrap();
        let report = drain(
            dir.path(),
            &Scripted {
                answers: std::sync::Mutex::new(Vec::new().into()),
            },
        )
        .unwrap();
        assert_eq!(report.settled, 1);
        assert!(!dir.path().join(LOG).exists());
    }

    #[test]
    fn payment_recorded_while_settling_survives_queue_replacement() {
        struct ConcurrentSale<'a>(&'a Path);
        impl Submit for ConcurrentSale<'_> {
            fn submit(&self, _: &serde_json::Value) -> Outcome {
                let (config, header) = crate::counter::x402::tests::payment_for_http_test();
                crate::counter::verify_and_record(self.0, &config, &header, crate::counter::now())
                    .unwrap();
                Outcome::Settled
            }
        }
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(&dir.path().join(LOG), &[row("old-payer", "old-nonce")]).unwrap();
        let report = drain(dir.path(), &ConcurrentSale(dir.path())).unwrap();
        assert_eq!(report.settled, 1);
        assert_eq!(report.remaining, 1);
        let pending = read_jsonl(&dir.path().join(LOG)).unwrap();
        assert_ne!(pending[0]["payer"], "old-payer");
        let (config, header) = crate::counter::x402::tests::payment_for_http_test();
        assert_eq!(
            crate::counter::verify_and_record(dir.path(), &config, &header, crate::counter::now()),
            Err(crate::counter::x402::Refusal::AlreadyUsed)
        );
    }

    #[test]
    fn a_second_settler_cannot_submit_the_same_queue() {
        struct SecondSettler<'a>(&'a Path);
        impl Submit for SecondSettler<'_> {
            fn submit(&self, _: &serde_json::Value) -> Outcome {
                let err = drain(self.0, self).unwrap_err();
                assert!(err.to_string().contains("another settler"), "{err:#}");
                Outcome::Settled
            }
        }
        let dir = tempfile::tempdir().unwrap();
        write_jsonl(&dir.path().join(LOG), &[row("payer", "nonce")]).unwrap();
        assert_eq!(
            drain(dir.path(), &SecondSettler(dir.path()))
                .unwrap()
                .settled,
            1
        );
    }
}
