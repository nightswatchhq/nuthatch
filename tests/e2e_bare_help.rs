//! The first thing a stranger types (#693).
//!
//! `--help` got the grouped `Commands:` listing in 2.6.2; a bare `nuthatch` did not, because the
//! interception tested `argv.len() == 2` and a bare invocation is length one. So the grouping shipped
//! everywhere except the shortest way to ask for it.
//!
//! These run the real binary rather than calling the renderer, because the fault was in the argv
//! guard in `main`, not in the renderer - a unit test on `render_top_level_help` passed throughout.

use std::process::Command;

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args(args)
        .output()
        .expect("run nuthatch");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The listing is the same whichever way it is asked for. Asserted as equality rather than by
/// checking for headings in both, so the two cannot drift into near-identical variants - which is
/// exactly how `--help` came to be grouped while the bare form stayed flat.
#[test]
fn a_bare_invocation_lists_the_same_grouped_commands_as_help() {
    let (_, help_out, _) = run(&["--help"]);
    let (_, _, bare_err) = run(&[]);

    for group in ["CORE:", "OPERATING:", "SCALED:"] {
        assert!(
            bare_err.contains(group),
            "a bare `nuthatch` must show the `{group}` group, got:\n{bare_err}"
        );
    }
    assert_eq!(
        bare_err, help_out,
        "the bare listing and the `--help` listing must be the same text"
    );
}

/// A bare invocation is a **usage error**, not a help request, and that distinction survives the
/// prettier listing. Clap wrote to stderr and exited 2; so does this. A shell script that runs
/// `nuthatch` by mistake must not see success, and must not have its stdout polluted.
#[test]
fn a_bare_invocation_is_still_an_error_on_stderr() {
    let (code, out, err) = run(&[]);
    assert_eq!(code, 2, "a missing subcommand is a usage error");
    assert!(out.is_empty(), "nothing on stdout, got {out:?}");
    assert!(!err.is_empty(), "the listing belongs on stderr");

    let (hcode, hout, herr) = run(&["--help"]);
    assert_eq!(hcode, 0, "asking for help succeeds");
    assert!(!hout.is_empty(), "help goes to stdout");
    assert!(herr.is_empty(), "help writes nothing to stderr, got {herr:?}");
}
