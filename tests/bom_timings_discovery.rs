//! #1027 - the BOM timing helper must find the report Cargo actually wrote.
//!
//! `cargo build --timings` writes **two** files: a timestamped `cargo-timing-<ts>.html` and an
//! unsuffixed `cargo-timing.html`. The helper globbed only the first, so a fresh checkout's first
//! `--timings` run - which can leave only the unsuffixed copy - was rejected with "no report found",
//! on a machine that had just produced one.
//!
//! The sharp part, and the reason this has a test now: the comment immediately above that glob
//! **named `cargo-timing.html`** as the reason not to sort by filename, and then the next line
//! excluded it. Prose describing a hazard the code did not handle, in a helper whose whole job is
//! producing numbers RFC-0042 §14 cites.
//!
//! These drive the real script, because the defect lived in argument handling and file discovery -
//! neither of which is reachable by testing the parsing in isolation.

use std::fs;
use std::path::Path;
use std::process::Command;

/// The smallest input the helper accepts: one `UNIT_DATA` array.
fn report(units: &[(&str, f64)]) -> String {
    let items: Vec<String> = units
        .iter()
        .map(|(n, d)| format!(r#"{{"name":"{n}","duration":{d}}}"#))
        .collect();
    format!(
        "<html><script>const UNIT_DATA = [{}];</script></html>",
        items.join(",")
    )
}

fn run(args: &[&str]) -> (bool, String) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("python3")
        .arg(root.join("scripts/bom-timings.py"))
        .args(args)
        .current_dir(root)
        .output()
        .expect("run bom-timings.py");
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
fn a_directory_holding_only_the_unsuffixed_report_is_accepted() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("cargo-timing.html"),
        report(&[("duckdb v1.0.0", 100.0), ("wasmtime v46.0.3", 50.0)]),
    )
    .unwrap();

    let (ok, text) = run(&["--dir", d.path().to_str().unwrap()]);
    assert!(
        ok,
        "a directory containing only `cargo-timing.html` was rejected. That is what a first \
         `cargo build --timings` run can leave, and the helper's own comment names the file \
         (#1027):\n{text}"
    );
    assert!(
        text.contains("duckdb"),
        "the report was found but not attributed:\n{text}"
    );
}

#[test]
fn the_timestamped_form_still_works() {
    let d = tempfile::tempdir().unwrap();
    fs::write(
        d.path().join("cargo-timing-20260831T090000Z.html"),
        report(&[("duckdb v1.0.0", 10.0)]),
    )
    .unwrap();
    let (ok, text) = run(&["--dir", d.path().to_str().unwrap()]);
    assert!(ok, "the timestamped form regressed:\n{text}");
}

/// The reason discovery is by mtime rather than by name.
///
/// `cargo-timing.html` sorts *before* every `cargo-timing-<ts>.html`, so a name-ordered "last"
/// would silently prefer whichever the alphabet favours over whichever Cargo wrote most recently.
#[test]
fn the_newest_report_wins_even_when_the_name_order_disagrees() {
    let d = tempfile::tempdir().unwrap();
    let old = d.path().join("cargo-timing-20260101T000000Z.html");
    let new = d.path().join("cargo-timing.html");
    // Distinct attributions, so the output says which file was read.
    fs::write(&old, report(&[("duckdb v1.0.0", 999.0)])).unwrap();
    fs::write(&new, report(&[("wasmtime v46.0.3", 42.0)])).unwrap();

    // Stamp explicit mtimes rather than sleeping. Two writes in the same tick can land with an
    // identical `st_mtime_ns` - this project has been bitten by exactly that - so "write, then
    // write" is not a reliable ordering. `touch -t CCYYMMDDhhmm` is accepted by both GNU and BSD.
    for (f, stamp) in [(&old, "202601010000"), (&new, "202608310900")] {
        let st = Command::new("touch")
            .arg("-t")
            .arg(stamp)
            .arg(f)
            .status()
            .expect("touch");
        assert!(st.success(), "could not stamp {}", f.display());
    }

    let (ok, text) = run(&["--dir", d.path().to_str().unwrap()]);
    assert!(ok, "discovery failed with both forms present:\n{text}");
    assert!(
        text.contains("cargo-timing.html") && !text.contains("20260101"),
        "the older, alphabetically-later report was chosen. Name order puts the unsuffixed file \
         first, which is exactly why this picks by mtime (#1027):\n{text}"
    );
}

#[test]
fn an_empty_directory_fails_and_names_both_forms() {
    let d = tempfile::tempdir().unwrap();
    let (ok, text) = run(&["--dir", d.path().to_str().unwrap()]);
    assert!(!ok, "an empty directory must not produce a BOM:\n{text}");
    assert!(
        text.contains("cargo-timing.html") && text.contains("cargo-timing-*.html"),
        "the failure must name both forms it looked for, or the next person adds the glob back \
         one file at a time:\n{text}"
    );
}
