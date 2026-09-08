//! RFC-0046 S0 (#1217). Payment is absent, and that absence is a gate, not a paragraph.
//!
//! §1: delete every payment feature from the tree and a self-hoster loses nothing; enable one and
//! the binary still runs, unpriced, for anyone who did not. This file is the compile-time half
//! (no payment crate, no payment source, no payment flag on `dev`/`serve`). The HTTP half lives
//! next to the router it has to bind: `serve::tests::an_unpriced_nest_does_not_charge_on_the_default_surface`.
//!
//! `PAYMENT_SURFACE` is empty today because there is no payment code. S1 (#1218) adds files here.
//! Deleting every listed file must leave the default binary serving - which is why a listed file
//! may not be an unconditional `mod` of `src/lib.rs`.

use std::path::{Path, PathBuf};

use clap::CommandFactory;
use nuthatch::cli::Cli;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Source files that implement payment. Empty until S1. Every path is repo-relative.
const PAYMENT_SURFACE: &[&str] = &[];

fn is_x402_crate(name: &str) -> bool {
    name.to_ascii_lowercase()
        .split(['-', '_', '.'])
        .any(|p| p == "x402")
}

fn payment_token(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    s.split(['-', '_', '.', '/', ' '])
        .any(|p| matches!(p, "x402" | "payment" | "facilitator"))
}

fn strip_line_comments_rs(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_line_comments_toml(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Production text: full-line comments gone, and `#[cfg(test)]` items removed by brace matching
/// rather than truncating the file. Rust allows production items after a test module.
fn production_src(src: &str) -> String {
    strip_cfg_test_items(&strip_line_comments_rs(src))
}

fn strip_cfg_test_items(src: &str) -> String {
    let bytes = src.as_bytes();
    let needle = b"#[cfg(test)]";
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(needle) {
            i += needle.len();
            while i < bytes.len() && bytes[i] != b'{' && bytes[i] != b';' {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b';' {
                i += 1;
                continue;
            }
            if i < bytes.len() && bytes[i] == b'{' {
                let mut depth = 1;
                i += 1;
                while i < bytes.len() && depth > 0 {
                    match bytes[i] {
                        b'{' => depth += 1,
                        b'}' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "target" | ".git") {
                continue;
            }
            rust_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn cargo_tomls(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if matches!(name, "target" | ".git") {
                continue;
            }
            cargo_tomls(&p, out);
        } else if p.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
            out.push(p);
        }
    }
}

fn filename_is_payment(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.contains("x402") || name == "payment.rs" || name.ends_with("_payment.rs")
}

fn production_carries_payment(src: &str) -> bool {
    let text = production_src(src);
    for needle in [
        "PAYMENT_REQUIRED",
        "payment-required",
        "Payment-Signature",
        "PAYMENT-SIGNATURE",
        "payment-signature",
    ] {
        if text.contains(needle) {
            return true;
        }
    }
    for token in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        if token.eq_ignore_ascii_case("x402") {
            return true;
        }
    }
    false
}

fn mod_stem(line: &str) -> Option<&str> {
    let t = line.trim().trim_end_matches(';').trim();
    if t.contains('{') {
        return None;
    }
    let t = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub "))
        .unwrap_or(t);
    t.strip_prefix("mod ")
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains(' ') && !s.contains('['))
}

fn unconditional_mod_stems(src: &str) -> Vec<String> {
    let text = production_src(src);
    let mut prev = String::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let cfg_on_line = t.contains("#[cfg");
        if let Some(stem) = mod_stem(t) {
            if !cfg_on_line && !prev.starts_with("#[cfg") {
                out.push(stem.to_string());
            }
        }
        prev = t.to_string();
    }
    out
}

fn lockfile_packages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut expect_name = false;
    for line in text.lines() {
        if line.trim() == "[[package]]" {
            expect_name = true;
            continue;
        }
        if expect_name {
            if let Some(rest) = line.trim().strip_prefix("name = \"") {
                if let Some(name) = rest.strip_suffix('"') {
                    out.push(name.to_string());
                }
                expect_name = false;
            }
        }
    }
    out
}

#[test]
fn payment_surface_files_exist_and_are_the_complete_set() {
    let root = root();
    let listed: Vec<PathBuf> = PAYMENT_SURFACE.iter().map(|r| root.join(r)).collect();
    for (rel, path) in PAYMENT_SURFACE.iter().zip(&listed) {
        assert!(
            path.is_file(),
            "{rel} is on PAYMENT_SURFACE but is not a file - the list drifted"
        );
    }

    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("decode/src"), &mut files);
    assert!(
        files.len() > 40,
        "src walk found {} rust files; the scanner is not looking at the tree",
        files.len()
    );

    let listed_rel: Vec<String> = PAYMENT_SURFACE.iter().map(|s| (*s).to_string()).collect();
    let mut unlisted = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_str()
            .unwrap()
            .replace('\\', "/");
        if listed_rel.iter().any(|l| l == &rel) {
            continue;
        }
        let src = std::fs::read_to_string(f).unwrap();
        if filename_is_payment(f) || production_carries_payment(&src) {
            unlisted.push(rel);
        }
    }
    assert!(
        unlisted.is_empty(),
        "payment source is not on PAYMENT_SURFACE, so deleting the list would not delete it (#1217):\n{}",
        unlisted.join("\n")
    );
}

#[test]
fn listed_payment_files_are_not_unconditional_modules() {
    let root = root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("decode/src"), &mut files);
    for rel in PAYMENT_SURFACE {
        let stem = Path::new(rel)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(rel);
        for f in &files {
            let src = std::fs::read_to_string(f).unwrap();
            let stems = unconditional_mod_stems(&src);
            let where_ = f
                .strip_prefix(&root)
                .unwrap()
                .to_str()
                .unwrap()
                .replace('\\', "/");
            assert!(
                !stems.iter().any(|s| s == stem),
                "{rel} is `mod {stem}` in {where_} without a cfg - deleting it would stop the default \
                 binary compiling, which is the property S1 has to keep (#1217)"
            );
        }
    }
}

#[test]
fn default_lockfile_has_no_x402_crate() {
    let root = root();
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    let packages = lockfile_packages(&lock);
    assert!(
        packages.len() > 100,
        "Cargo.lock parsed {} packages; the parser is not seeing the lockfile",
        packages.len()
    );
    let hits: Vec<&String> = packages.iter().filter(|n| is_x402_crate(n)).collect();
    assert!(
        hits.is_empty(),
        "x402 crate on the default lockfile, so the binary cannot be built without a payment \
         dependency (#1217): {hits:?}"
    );

    let toml = strip_line_comments_toml(&std::fs::read_to_string(root.join("Cargo.toml")).unwrap());
    assert!(
        !toml
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            .any(|t| t == "x402"),
        "Cargo.toml names an x402 crate on the nuthatch package"
    );
    let features = toml
        .split("[features]")
        .nth(1)
        .unwrap_or("")
        .split('[')
        .next()
        .unwrap_or("");
    assert!(
        !payment_token(features),
        "a default-on feature names payment: {features}"
    );

    let mut tomls = Vec::new();
    cargo_tomls(&root, &mut tomls);
    assert!(
        !tomls.is_empty(),
        "no Cargo.toml files found; the walk is not looking at the tree"
    );
    let mut toml_hits = Vec::new();
    for p in &tomls {
        let text = strip_line_comments_toml(&std::fs::read_to_string(p).unwrap());
        if text
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            .any(|t| t == "x402")
        {
            toml_hits.push(p.strip_prefix(&root).unwrap_or(p).display().to_string());
        }
    }
    assert!(
        toml_hits.is_empty(),
        "x402 crate named in Cargo.toml (#1217): {}",
        toml_hits.join(", ")
    );
}

fn assert_arg_is_not_payment(where_: &str, arg: &clap::Arg) {
    let mut bits = vec![arg.get_id().as_str().to_string()];
    if let Some(long) = arg.get_long() {
        bits.push(long.to_string());
    }
    if let Some(env) = arg.get_env() {
        bits.push(env.to_string_lossy().into_owned());
    }
    let blob = bits.join(" ");
    assert!(
        !payment_token(&blob),
        "{where_} grew a payment flag, so a key could become required to start \
         (#1217): {bits:?}"
    );
}

#[test]
fn nuthatch_dev_and_serve_take_no_payment_flag() {
    let cmd = Cli::command();
    // Root `global = true` args are accepted on every subcommand but are not in
    // `sub.get_arguments()`; `nuthatch --x402 dev` would otherwise pass this test.
    for arg in cmd.get_arguments() {
        assert_arg_is_not_payment("nuthatch (global)", arg);
    }
    for name in ["dev", "serve"] {
        let sub = cmd
            .find_subcommand(name)
            .unwrap_or_else(|| panic!("{name} subcommand missing"));
        for arg in sub.get_arguments() {
            assert_arg_is_not_payment(&format!("nuthatch {name}"), arg);
        }
    }
}

#[test]
fn the_http_boundary_is_wired_through_the_real_router() {
    let src = std::fs::read_to_string(root().join("src/serve.rs")).unwrap();
    assert!(
        src.contains("fn an_unpriced_nest_does_not_charge_on_the_default_surface"),
        "the HTTP half of #1217 is missing from serve.rs - a tree gate with no router probe \
         cannot see a default-on 402"
    );
    let tests = src.split("#[cfg(test)]").nth(1).unwrap_or("");
    assert!(
        tests.contains("fn an_unpriced_nest_does_not_charge_on_the_default_surface"),
        "the HTTP half must sit in the test module so production_src does not match it"
    );
}
