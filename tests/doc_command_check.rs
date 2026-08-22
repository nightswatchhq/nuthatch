//! #769 - a documented command is a claim, and claims want a checker rather than a good memory
//! (`scripts/version-check.sh`'s line, generalised from version strings to commands and flags).
//!
//! `public/llms.txt` told coding agents to run `nuthatch roost` for five releases after the
//! subcommand was removed in 2.0. This walks every `nuthatch ...` invocation written in backticks
//! across the doc set and resolves it against clap's own command tree (`Cli::command()`, the same
//! ground truth `src/skill.rs::generate_cli_reference` renders from) - not a hand-kept list, which
//! would just be `CONFIG_SOURCES` again with the same failure mode.
//!
//! Scoped to commands and flags only, per the issue - not prose, not numbers (#767's problem).
//!
//! Deliberately anchored on backtick-formatted invocations, not bare mentions of the word
//! "nuthatch": these are what a reader copies and runs, the same reasoning `version-check.sh` gives
//! for anchoring on the container-tag lines rather than every version-shaped number in a file. A
//! flag is only checked when it appears inside such an invocation - the wider doc set (RFCs,
//! benchmarks, operator runbooks) mentions plenty of non-nuthatch `--flags` (docker, curl, grep,
//! cargo) that a blind whole-text scan would flag as fiction. `tests/skill_refs.rs` already runs
//! that blind scan, safely, because its scope is `skills/nuthatch-builder/*.md` alone - a skill
//! that is entirely about nuthatch usage. This file does not widen that one.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::CommandFactory;
use nuthatch::cli::Cli;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Deliberate historical or explanatory mentions of a command/flag the binary no longer accepts.
/// Every entry needs the file it appears in, the exact unresolved word, and why it stays. This is
/// the useful artifact acceptance #4 asks for, not a way to make the check quieter - an entry that
/// stops matching anything is dead weight and fails the build too (see `allowlist_has_no_stale_entries`).
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "docs/operators.md",
        "roost",
        "the pre-2.0 -> 2.0 migration table maps the retired `nuthatch roost dev` to its \
         replacement; the mapping needs both names",
    ),
    (
        "docs/progress-log.md",
        "roost",
        "dated progress-log entries describe RFC-0012 as it was built, before RFC-0032 retired the \
         roost - it's a record of the past, not current usage",
    ),
    (
        "docs/rfcs/0012-multi-nest-runtime-and-nest-packaging.md",
        "roost",
        "RFC-0012 is the design doc that introduced `nuthatch roost dev`; RFC-0032 retired it and \
         RFCs are dated writing, not current docs (same exemption `version-check.sh` gives them)",
    ),
    (
        "docs/rfcs/0027-the-live-roost.md",
        "roost",
        "RFC-0027 is titled around the roost and designs its CLI; dated writing, same as 0012",
    ),
    (
        "docs/sprint-languid-lapwing.md",
        "roost",
        "the sprint doc that fixed the incident names the removed command as the regression it \
         closes - it would be a strange irony for this check to make that sentence unwritable",
    ),
    (
        "docs/sprint-nocturnal-nightjar.md",
        "roost",
        "this sprint doc's own #769 entry states the regression test this file is - \"reintroducing \
         `nuthatch roost` into any documented file must fail the build\" - same reasoning as the \
         sprint-languid-lapwing.md entry above",
    ),
    (
        "docs/progress-log.md",
        "upgrade",
        "the 2026-07-21 entry documents `nuthatch nest upgrade` as it was that day (RFC-0020); the \
         file's own header warns no entry is kept in step with the binary - it does not exist in \
         2.2.0+",
    ),
    (
        "docs/progress-log.md",
        "diff",
        "same RFC-0020 entry, `nuthatch nest diff` - dated progress-log writing, not current usage",
    ),
    (
        "docs/progress-log.md",
        "mount",
        "the 2026-07-18 RFC-0012 slice 6 entry documents `nuthatch nest mount` as shipped that day; \
         dated progress-log writing describing history, same as the roost entries above",
    ),
    (
        "docs/progress-log.md",
        "pack",
        "the 2026-07-18 RFC-0012 slice 5 entry documents `nuthatch nest pack` as shipped that day - \
         the verb was later folded into `nuthatch nest bundle`",
    ),
    (
        "docs/rfcs/0012-multi-nest-runtime-and-nest-packaging.md",
        "pack",
        "RFC-0012 §5 designs `nuthatch nest pack`, the verb `nest bundle` later replaced; dated \
         design writing, same exemption `version-check.sh` gives docs/rfcs/",
    ),
    (
        "docs/rfcs/0012-multi-nest-runtime-and-nest-packaging.md",
        "mount",
        "RFC-0012 §6 designs `nuthatch nest mount`, since removed; dated design writing",
    ),
    (
        "docs/rfcs/0001-generalized-decode-and-nests.md",
        "build",
        "\"a future `nuthatch build --aot` could revisit\" - explicitly a proposal for later, never \
         built; the RFC says so itself",
    ),
    (
        "docs/rfcs/0001-generalized-decode-and-nests.md",
        "--aot",
        "the same never-built proposal - see the `build` entry above",
    ),
    (
        "docs/rfcs/0016-governed-semantic-layer-and-agent-grade-mcp.md",
        "eval",
        "RFC-0016 §1 proposes a `nuthatch eval` harness; not a shipped top-level subcommand",
    ),
    (
        "docs/rfcs/0024-eth-call-execution-engine.md",
        "state-cache",
        "RFC-0024 proposes `nuthatch state-cache clear` for the L3 call-result cache; not a shipped \
         subcommand",
    ),
];

/// A finding: an unresolved subcommand word or flag, where it was found, and what was wrong.
#[derive(Debug, Clone)]
struct Finding {
    file: String,
    line: usize,
    bad_token: String,
    detail: String,
}

impl Finding {
    fn is_allowed(&self) -> bool {
        ALLOWED
            .iter()
            .any(|(f, tok, _)| *f == self.file && *tok == self.bad_token)
    }
}

/// Doc files in scope: `README.md`, every `.md` under `docs/`, and the authored files of
/// `skills/nuthatch-builder/` (RFC-0017's builder skill - the thing `init` scaffolds into a new
/// project's `.claude/skills/`). `cli-reference.md` itself is excluded: it's rendered from
/// `Cli::command()` by `src/skill.rs`, the same ground truth this file checks against, so it can't
/// drift by construction - `tests/skill_refs.rs::committed_cli_reference_is_not_stale` is the gate
/// for that file specifically. `tests/skill_refs.rs` already drift-checks the *other* skill files'
/// flags against the reference, both directions - walking them here too is a second, weaker copy of
/// half of that check, but the only one that also validates subcommand *names*, which the
/// flags-only check can't.
fn doc_files() -> Vec<PathBuf> {
    let root = repo_root();
    let mut out = vec![root.join("README.md")];
    collect_md(&root.join("docs"), &mut out);
    for entry in std::fs::read_dir(root.join(nuthatch::skill::SKILL_DIR))
        .expect("skills/nuthatch-builder must exist")
        .flatten()
    {
        let path = entry.path();
        let is_generated_reference =
            path.file_name().and_then(|n| n.to_str()) == Some("cli-reference.md");
        if path.extension().and_then(|e| e.to_str()) == Some("md") && !is_generated_reference {
            out.push(path);
        }
    }
    out
}

fn collect_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_md(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

/// Every backtick-delimited span in `text`, paired with the 1-indexed line it starts on: fenced
/// ``` blocks (content between the fences, language tag stripped) and single-backtick inline spans.
/// A naive `` ` `` split would treat the closing fence of a code block as an inline-span opener;
/// fences are found and removed first so the remaining single backticks are all genuine inline code.
fn backtick_spans(text: &str) -> Vec<(usize, String)> {
    let mut spans = Vec::new();
    let mut in_fence = false;
    let mut fence_start_line = 0;
    let mut fence_buf = String::new();
    let mut line_no = 0;

    for line in text.lines() {
        line_no += 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_fence {
                spans.push((fence_start_line, std::mem::take(&mut fence_buf)));
                in_fence = false;
            } else {
                in_fence = true;
                fence_start_line = line_no + 1;
                fence_buf.clear();
            }
            continue;
        }
        if in_fence {
            fence_buf.push_str(line);
            fence_buf.push('\n');
            continue;
        }
        // Inline spans: split on single backticks, odd-indexed segments are span content.
        for (i, seg) in line.split('`').enumerate() {
            if i % 2 == 1 && !seg.is_empty() {
                spans.push((line_no, seg.to_string()));
            }
        }
    }
    spans
}

/// Walk one `nuthatch ...` invocation's tokens against the clap tree starting at `root`, recording
/// any subcommand word that doesn't resolve and any `--flag` not present anywhere in the tree.
/// Stops requiring subcommand resolution the moment a leaf (a command with no further subcommands)
/// is reached - the rest of the tokens are that leaf's own args/positionals, out of scope here.
fn walk_invocation(
    tokens: &[&str],
    root: &clap::Command,
    real_flags: &BTreeSet<String>,
    file: &str,
    line: usize,
    findings: &mut Vec<Finding>,
) {
    let mut cmd = root;
    let mut resolving = true; // still expecting subcommand words
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if tok == "#" {
            break; // shell comment: nothing after this is a real token
        }
        if let Some(stripped) = tok.strip_prefix("--") {
            let name = stripped.split('=').next().unwrap_or(stripped);
            let flag = format!("--{name}");
            if !real_flags.contains(&flag) {
                findings.push(Finding {
                    file: file.to_string(),
                    line,
                    bad_token: flag.clone(),
                    detail: format!("`{flag}` is not a flag `nuthatch` accepts anywhere"),
                });
            }
            i += 1;
            continue;
        }
        if resolving && tok.starts_with('<') {
            // A doc placeholder (`<old>`, `<blob>`, ...), not a further subcommand word - the
            // author is showing shape, not a literal invocation past this point.
            resolving = false;
            i += 1;
            continue;
        }
        if resolving && cmd.has_subcommands() {
            // Hidden is a `--help` rendering choice (`skill-refs`), not a parser refusal - a
            // hidden subcommand is still real and must resolve here.
            let single = cmd.get_subcommands().find(|s| s.get_name() == tok);
            // `list|add` shorthand: valid only if every alternative names a real subcommand at
            // this position. Which branch was meant is ambiguous, so - like a placeholder - this
            // is where subcommand resolution stops, not a further chain to descend.
            let alternation_ok = tok.contains('|')
                && tok
                    .split('|')
                    .all(|part| cmd.get_subcommands().any(|s| s.get_name() == part));
            match single {
                Some(next) => {
                    cmd = next;
                }
                None if alternation_ok => {
                    resolving = false;
                }
                None => {
                    findings.push(Finding {
                        file: file.to_string(),
                        line,
                        bad_token: tok.to_string(),
                        detail: format!(
                            "`{tok}` is not a subcommand of `nuthatch{}`",
                            invocation_path(root, cmd)
                        ),
                    });
                    resolving = false;
                }
            }
        } else {
            // Either already at a leaf, or we already gave up after an unknown word above -
            // remaining bare words are positional values, not further subcommands to validate.
            resolving = false;
        }
        i += 1;
    }
}

/// The invocation path from `root` down to `cmd`, e.g. " audit replay", for error messages.
fn invocation_path(root: &clap::Command, cmd: &clap::Command) -> String {
    if std::ptr::eq(root, cmd) {
        return String::new();
    }
    // Small tree, small doc corpus - a BFS per finding is cheap and avoids threading path state
    // through the recursive walk above.
    fn find_path<'a>(node: &'a clap::Command, target: &str, path: &mut Vec<&'a str>) -> bool {
        for sub in node.get_subcommands() {
            path.push(sub.get_name());
            if sub.get_name() == target || find_path(sub, target, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    find_path(root, cmd.get_name(), &mut path);
    if path.is_empty() {
        format!(" {}", cmd.get_name())
    } else {
        format!(" {}", path.join(" "))
    }
}

/// Every long flag anywhere in the tree the parser accepts, independent of `src/skill.rs`'s own
/// renderer - same reasoning as `tests/skill_refs.rs::collect_real_flags`: a bug shared between the
/// renderer and this check must not be able to make both agree on a wrong answer. Hidden is not
/// filtered here either, for the same reason as subcommands: it hides an arg from `--help`, it does
/// not stop the parser accepting it.
///
/// `--help`/`--version` are seeded explicitly: they're clap-synthesised rather than declared on
/// `Cli`, and `Command::get_arguments()` only reports them once the command has been `build()`-ed -
/// which `CommandFactory::command()` does not guarantee. Both are real on every subcommand.
fn real_flags() -> BTreeSet<String> {
    fn walk(cmd: &clap::Command, out: &mut BTreeSet<String>) {
        for arg in cmd.get_arguments() {
            if let Some(long) = arg.get_long() {
                out.insert(format!("--{long}"));
            }
        }
        for sub in cmd.get_subcommands() {
            walk(sub, out);
        }
    }
    let mut out = BTreeSet::new();
    out.insert("--help".to_string());
    out.insert("--version".to_string());
    walk(&Cli::command(), &mut out);
    out
}

/// Find every `nuthatch ...` invocation in `text` and check its subcommand chain and flags. An
/// invocation starts at a bare `nuthatch` token (exact match - `nuthatch.toml`, `nuthatch-builder`,
/// `nuthatch:2.5.0`, `nuthatch_tip_height` are all distinct whitespace-delimited tokens and never
/// match) that begins a shell segment - the first token on a line, or straight after `&&`/`||`/`;`/
/// `|`/`--`/`sudo` - and runs until the next such separator or end of line. The segment-start
/// requirement is what tells `journalctl -u nuthatch -f` and `docker run --name nuthatch ...` (a
/// service/container *name* argument) apart from an actual invocation, and lets `claude mcp add
/// nuthatch -- nuthatch mcp --url ...` resolve only the second `nuthatch` - the one after `--` that
/// is actually being run.
fn check_text(text: &str, file: &str, real_flags: &BTreeSet<String>, findings: &mut Vec<Finding>) {
    let root = Cli::command();
    for (start_line, span) in backtick_spans(text) {
        for (offset, line) in span.lines().enumerate() {
            let line_no = start_line + offset;
            let tokens: Vec<&str> = line.split_whitespace().collect();
            let mut i = 0;
            let mut at_segment_start = true;
            while i < tokens.len() {
                let is_separator = matches!(tokens[i], "&&" | "||" | ";" | "|" | "--");
                if tokens[i] == "nuthatch" && at_segment_start {
                    let mut end = i + 1;
                    while end < tokens.len()
                        && !matches!(tokens[end], "&&" | "||" | ";" | "|" | "--")
                        && tokens[end] != "nuthatch"
                    {
                        end += 1;
                    }
                    walk_invocation(
                        &tokens[i + 1..end],
                        &root,
                        real_flags,
                        file,
                        line_no,
                        findings,
                    );
                    i = end;
                    // `end` may itself be a separator/`nuthatch` - re-examine it as the start of
                    // the next segment rather than skipping past it.
                    at_segment_start = true;
                    continue;
                } else if is_separator || tokens[i] == "sudo" {
                    at_segment_start = true;
                    i += 1;
                } else {
                    at_segment_start = false;
                    i += 1;
                }
            }
        }
    }
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[test]
fn every_documented_command_and_flag_is_real() {
    let root = repo_root();
    let real = real_flags();
    let mut findings = Vec::new();

    for path in doc_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file = rel(&root, &path);
        check_text(&text, &file, &real, &mut findings);
    }

    let offenders: Vec<String> = findings
        .iter()
        .filter(|f| !f.is_allowed())
        .map(|f| format!("{}:{}: {}", f.file, f.line, f.detail))
        .collect();

    assert!(
        offenders.is_empty(),
        "documented commands/flags the binary does not accept - fix the docs, fix the binary, or \
         add a reasoned entry to ALLOWED in tests/doc_command_check.rs:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn allowlist_has_no_stale_entries() {
    let root = repo_root();
    let real = real_flags();
    let mut findings = Vec::new();
    for path in doc_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file = rel(&root, &path);
        check_text(&text, &file, &real, &mut findings);
    }

    let seen: BTreeSet<(&str, &str)> = findings
        .iter()
        .map(|f| (f.file.as_str(), f.bad_token.as_str()))
        .collect();

    let stale: Vec<String> = ALLOWED
        .iter()
        .filter(|(file, tok, _)| !seen.contains(&(*file, *tok)))
        .map(|(file, tok, _)| format!("{file}: `{tok}` (no longer an unresolved mention here)"))
        .collect();

    assert!(
        stale.is_empty(),
        "ALLOWED entries in tests/doc_command_check.rs no longer match anything, which hides the \
         next real gap - remove them:\n{}",
        stale.join("\n")
    );
}

// ── Proving the mechanism can fail before it is trusted (acceptance #3) ──────────────────────
//
// These run against synthetic fixtures, not the real doc tree, so they stay meaningful even after
// every current ALLOWED entry above is one day deleted. Each is the literal regression test named
// in the issue: reintroducing a removed subcommand into a documented file must fail the build.

#[test]
fn detects_a_reintroduced_removed_subcommand() {
    let real = real_flags();
    let mut findings = Vec::new();
    check_text(
        "Run `nuthatch roost dev` to serve every mounted nest.",
        "docs/example.md",
        &real,
        &mut findings,
    );
    assert!(
        findings.iter().any(|f| f.bad_token == "roost"),
        "reintroducing `nuthatch roost` into a documented file must be detected"
    );
}

#[test]
fn detects_an_unreal_flag() {
    let real = real_flags();
    let mut findings = Vec::new();
    check_text(
        "```sh\nnuthatch dev --this-flag-does-not-exist\n```",
        "docs/example.md",
        &real,
        &mut findings,
    );
    assert!(
        findings
            .iter()
            .any(|f| f.bad_token == "--this-flag-does-not-exist"),
        "a nonexistent flag inside a real invocation must be detected"
    );
}

#[test]
fn a_real_multi_level_invocation_is_clean() {
    let real = real_flags();
    let mut findings = Vec::new();
    check_text(
        "```sh\nnuthatch audit replay --dir . --from 100 --to 200\nnuthatch labels import addrs.csv --alias sanctioned\n```",
        "docs/example.md",
        &real,
        &mut findings,
    );
    assert!(
        findings.is_empty(),
        "real subcommand chains and flags must not be flagged: {findings:?}"
    );
}

#[test]
fn positional_values_after_a_leaf_are_not_mistaken_for_subcommands() {
    let real = real_flags();
    let mut findings = Vec::new();
    check_text(
        "`nuthatch init 0xA0b86991c6218b36c1D19D4a2e9Eb0cE3606eB48 --alias usdc`",
        "docs/example.md",
        &real,
        &mut findings,
    );
    assert!(
        findings.is_empty(),
        "an address positional after `init` must not be checked as a subcommand: {findings:?}"
    );
}

#[test]
fn a_bare_mention_of_the_product_name_is_not_an_invocation() {
    let real = real_flags();
    let mut findings = Vec::new();
    check_text(
        "nuthatch.toml, nuthatch-builder, and ghcr.io/nightswatchhq/nuthatch:2.5.0 are not commands.",
        "docs/example.md",
        &real,
        &mut findings,
    );
    assert!(
        findings.is_empty(),
        "prose mentions of the product name must not be parsed as invocations: {findings:?}"
    );
}

#[test]
fn an_allowlisted_mention_is_suppressed_only_in_its_own_file() {
    assert!(
        ALLOWED
            .iter()
            .any(|(f, t, _)| *f == "docs/operators.md" && *t == "roost"),
        "sanity: the allow-list this test exercises must still name the operators.md roost entry"
    );
    let finding = Finding {
        file: "docs/operators.md".to_string(),
        line: 1,
        bad_token: "roost".to_string(),
        detail: String::new(),
    };
    assert!(finding.is_allowed());

    let same_word_elsewhere = Finding {
        file: "docs/some-other-file.md".to_string(),
        line: 1,
        bad_token: "roost".to_string(),
        detail: String::new(),
    };
    assert!(
        !same_word_elsewhere.is_allowed(),
        "an allow-list entry is pinned to its file - the same word elsewhere must still fail"
    );
}
