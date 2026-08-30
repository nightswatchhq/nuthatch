//! #977 - a benchmark helper that records a timing when the request failed.
//!
//! `scripts/noise-floor.sh` and `scripts/concurrent-floor.sh` both ran
//! `curl -s --max-time N ... > /dev/null` with no `--fail` and no status inspection, then recorded
//! `t1 - t0` unconditionally.
//!
//! **The failure direction is the dangerous one.** A dead or refusing server answers in
//! microseconds, so the worse the server, the tighter the reported floor - and `noise-floor.sh`
//! produces `docs/bench/noise-floor.md`, the threshold every RFC-0042 measurement was judged
//! against. `concurrent-floor.sh` was worse still: `serve.rs` refuses past `SQL_MAX_CONCURRENCY`
//! with a **503** via `try_acquire_owned`, so at concurrency 4/8/16 most requests were refused
//! instantly and counted as fast successes, making the most saturated levels look the fastest.
//!
//! These tests drive the scripts against a socket that behaves badly on purpose. No nuthatch
//! instance is involved: the point is what the harness does with a bad answer, and a real server
//! cannot be made to refuse on demand.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A one-line HTTP server that answers every request with the same status, immediately.
///
/// Returns the bound port and a flag that stops it. Answering *fast* is the whole point: it
/// reproduces the condition under which the old harness reported its most attractive numbers.
fn serve_status(status: &'static str) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let handle = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut s, _)) => {
                    let _ = drain_request(&s);
                    let body = "{}";
                    let _ = s.write_all(
                        format!(
                            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    );
                    let _ = s.flush();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (port, stop, handle)
}

fn drain_request(s: &TcpStream) -> std::io::Result<()> {
    let mut r = BufReader::new(s);
    let mut line = String::new();
    while r.read_line(&mut line)? > 0 {
        if line == "\r\n" || line == "\n" {
            break;
        }
        line.clear();
    }
    Ok(())
}

fn run_script(script: &str, port: u16, extra: &[(&str, &str)]) -> (bool, String) {
    let path: &Path = &root().join("scripts").join(script);
    assert!(path.exists(), "{} is missing", path.display());
    let mut cmd = Command::new("bash");
    cmd.arg(path)
        .current_dir(root())
        .env("PORT", port.to_string());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run script");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn noise_floor_refuses_a_sample_count_below_the_documented_minimum() {
    // `docs/bench/noise-floor.md` asks for >= 15 because the distribution is bimodal. The script
    // documented that and accepted anything.
    let (port, stop, h) = serve_status("200 OK");
    let (ok, text) = run_script("noise-floor.sh", port, &[("N", "3")]);
    stop.store(true, Ordering::SeqCst);
    let _ = h.join();
    assert!(
        !ok,
        "N=3 was accepted against a documented minimum of 15. A smaller sample cannot see the second \
         mode and reports a tighter floor than the system has:\n{text}"
    );
    assert!(
        text.contains("below the documented minimum"),
        "the refusal must say why, so the next person does not simply raise the number:\n{text}"
    );
}

#[test]
fn noise_floor_fails_rather_than_timing_a_server_that_refuses() {
    // 503 is what `serve.rs` actually returns past `SQL_MAX_CONCURRENCY`, so this is the live shape
    // rather than an invented one. Before #977 this produced a full table of very fast timings.
    let (port, stop, h) = serve_status("503 Service Unavailable");
    let (ok, text) = run_script("noise-floor.sh", port, &[("N", "15")]);
    stop.store(true, Ordering::SeqCst);
    let _ = h.join();
    assert!(
        !ok,
        "a server refusing every request produced a successful benchmark run. That is the failure \
         direction that matters: refusals are fast, so the floor reads tighter the more broken the \
         server is:\n{text}"
    );
    assert!(
        text.contains("successful samples"),
        "the failure must name the missing successes rather than dying obscurely:\n{text}"
    );
}

#[test]
fn noise_floor_fails_when_nothing_is_listening() {
    // A port with no server at all: connection refused returns even faster than a 503.
    let free = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let (ok, text) = run_script("noise-floor.sh", port, &[("N", "15")]);
    assert!(
        !ok,
        "the harness produced a benchmark against a port with nothing listening:\n{text}"
    );
}

#[test]
fn concurrent_floor_reports_refusals_instead_of_timing_them() {
    let (port, stop, h) = serve_status("503 Service Unavailable");
    let (_ok, text) = run_script("concurrent-floor.sh", port, &[("DUR", "1")]);
    stop.store(true, Ordering::SeqCst);
    let _ = h.join();

    assert!(
        text.contains("failed"),
        "the table must carry a refusal column - at these concurrencies refusals are the finding, \
         not noise:\n{text}"
    );
    // Every request was refused, so throughput must read zero. Before #977 the refusals were
    // counted as completed requests and this printed its highest req/s of the run.
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .next()
                .is_some_and(|c| c.parse::<u32>().is_ok())
        })
        .collect();
    assert!(
        !rows.is_empty(),
        "no measurement rows printed at all:\n{text}"
    );
    for row in &rows {
        let f: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            f[1], "0.0",
            "throughput must be zero when every request was refused, got {} in row `{row}`:\n{text}",
            f[1]
        );
    }
}

#[test]
fn concurrent_floor_still_measures_a_healthy_server() {
    // The other direction. A gate that fails on everything is not a gate, and a harness that
    // refuses a working server would simply be switched off.
    let (port, stop, h) = serve_status("200 OK");
    let (_ok, text) = run_script("concurrent-floor.sh", port, &[("DUR", "1")]);
    stop.store(true, Ordering::SeqCst);
    let _ = h.join();
    let rows: Vec<&str> = text
        .lines()
        .filter(|l| {
            l.split_whitespace()
                .next()
                .is_some_and(|c| c.parse::<u32>().is_ok())
        })
        .collect();
    assert!(
        !rows.is_empty(),
        "no measurement rows against a healthy server:\n{text}"
    );
    let any_throughput = rows.iter().any(|r| {
        r.split_whitespace()
            .nth(1)
            .and_then(|v| v.parse::<f64>().ok())
            .is_some_and(|v| v > 0.0)
    });
    assert!(
        any_throughput,
        "a server answering 200 produced no successful throughput - the harness now rejects \
         everything, which is the opposite failure:\n{text}"
    );
}
