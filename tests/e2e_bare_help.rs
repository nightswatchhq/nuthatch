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
    assert!(
        herr.is_empty(),
        "help writes nothing to stderr, got {herr:?}"
    );
}

/// #695. A warning during `init` must arrive in `init`'s idiom, not in log format.
///
/// The default filter is `nuthatch=info`, so every `tracing::warn!` in the crate prints during
/// `init` with a timestamp, a level and ANSI colouring, straight through the `→`/`✓` block. A clean
/// run never shows one - which is why this went unnoticed - and a stranger with a slow public
/// endpoint gets several.
///
/// Driven with `RUST_LOG` rather than by arranging a slow endpoint: the fault is the *formatter*
/// chosen for the pretty commands, and any event through that formatter proves it. Arranging a real
/// timeout would test the network instead.
#[test]
fn a_log_line_during_init_arrives_in_inits_own_idiom() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args([
            "init",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "--chain",
            "mainnet",
        ])
        .current_dir(dir.path())
        .env("RUST_LOG", "nuthatch=debug")
        .output()
        .expect("run init");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Network-dependent: if nothing was logged at all there is nothing to judge, and a test that
    // silently passes on an empty run is the flattering failure this repository keeps rediscovering.
    let logged: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("rpc ") || l.contains("failed for"))
        .collect();
    if logged.is_empty() {
        eprintln!("no log lines emitted (offline?) - nothing to judge, not asserting");
        return;
    }
    for line in logged {
        assert!(
            line.trim_start().starts_with('·'),
            "a log line during init must use the `·` idiom, got: {line:?}"
        );
        assert!(
            !line.contains("WARN") && !line.contains("DEBUG") && !line.contains("\x1b["),
            "no level or ANSI in init's output, got: {line:?}"
        );
        assert!(
            !line.contains("2026-") && !line.contains("Z  "),
            "no ISO timestamp in init's output, got: {line:?}"
        );
    }
}

/// #694. The ABI-resolved tick is *printed*, not merely formattable.
///
/// `project.rs` has `abi_resolved_lines()` (a pure formatter, two unit tests) and
/// `print_abi_resolved()` (calls it, prints). Nothing covered the wiring: deleting both call sites
/// left the whole suite green, so the line a stranger relies on to know the ABI was found could have
/// vanished silently.
///
/// The same class as #693 and #672 in this repository - a unit test on the renderer says nothing
/// about whether anyone calls it.
#[test]
fn init_prints_the_abi_resolved_tick_and_not_only_formats_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args([
            "init",
            "0x1f9840a85d5aF5bf1D1762F925BDADdC4201F984",
            "--chain",
            "mainnet",
        ])
        .current_dir(dir.path())
        .output()
        .expect("run init");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // Offline runs cannot resolve an ABI at all, and a test that passes on a run that never got
    // there would be worse than none. Judge only when the scaffold actually succeeded.
    if !text.contains("scaffolded nest") {
        eprintln!("init did not complete (offline?) - nothing to judge, not asserting");
        return;
    }
    assert!(
        text.contains("ABI resolved via"),
        "init completed without printing the ABI-resolved tick:\n{text}"
    );
}
