//! `nuthatch transform`'s non-creating open, driven through the real binary (issue #474).
//!
//! #448 gave `run_transform` the same fix `nuthatch sql` got in #413 - `Store::open_existing`
//! instead of `Store::open` - one line, in the same PR, with no test of its own. `cli-reference.md`
//! already promised "must contain a nuthatch.redb with indexed transfers"; a creating open made that
//! false and self-satisfying: the command reported `0 transfers` / `✓ 0 facts out` for "there is no
//! nest here" and left an empty `nuthatch.redb` behind for a later `holds_data` check to misread as
//! a real store (the `(mtime, len)` failure mode from #471).
//!
//! The exit status alone can't see that - a probe that makes its own answer true still exits 0. The
//! only thing that catches it is what is left on disk afterwards.

use std::path::Path;

/// Run `nuthatch transform <component> --dir <dir>` and return its exit status with its
/// stdout+stderr. The component path need not exist: a non-creating open must fail before the
/// component is ever loaded.
fn run_transform(dir: &Path) -> (std::process::ExitStatus, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args(["transform", "does-not-exist.wasm", "--dir"])
        .arg(dir)
        .output()
        .expect("running the nuthatch binary");
    (
        out.status,
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

/// **No nest here → refused, and nothing created.** The reported defect, both halves: a wrong exit
/// code alone would pass even a creating open that then failed elsewhere (e.g. loading the wasm
/// component), so the directory is checked too.
#[test]
fn transform_against_an_empty_directory_fails_and_creates_nothing() {
    let empty = tempfile::tempdir().unwrap();

    let (status, output) = run_transform(empty.path());

    assert!(
        !status.success(),
        "a directory with no store has no transfers to transform, got:\n{output}"
    );
    assert!(
        output.contains("no nest to transform at"),
        "the failure should name the reason, got:\n{output}"
    );
    assert!(
        std::fs::read_dir(empty.path()).unwrap().next().is_none(),
        "a non-creating open must leave the directory exactly as empty as it found it, got: {:?}",
        std::fs::read_dir(empty.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>()
    );
}
