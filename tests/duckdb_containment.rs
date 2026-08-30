//! RFC-0042 slice 1 (#936): the engine must not spread while we are deciding whether to remove it.
//!
//! §6 asks for an analytical boundary across which "DuckDB-specific connection, value or AST types do
//! not escape". Measuring first, as slice 0 did, found that boundary mostly already exists:
//! `analytics.rs` holds 53 connection operations and its public functions take `&Path` and `&str` and
//! return `serde_json::Value`. Nothing of DuckDB's crosses it.
//!
//! It leaks in exactly two modules, and this file freezes that.
//!
//! **A shrink-only list, deliberately.** A hand-kept allowlist is the `CONFIG_SOURCES` failure mode -
//! it needs editing on every legitimate change until someone relaxes it into meaninglessness. This one
//! is different in the direction that matters: **removing a site is the goal**, so the list may only
//! get shorter. Adding to it requires a deliberate edit and a reviewer asking why the engine is
//! spreading during the sprint that exists to decide whether to remove it.
//!
//! # #976: what this file used to be, and why it was not a gate
//!
//! Until 2026-08-30 the signature scan was `line.starts_with("pub fn") || starts_with("pub struct")`
//! against three hardcoded type names. That is a prefix match on one line, and it could not see:
//!
//!   * `pub(crate) fn` - which is what **all five** of `graft.rs`'s connection-taking functions are,
//!     so the assertion below passed green while the doc comment above it said `graft.rs` "does not"
//!     satisfy §6 and that this file "records the gap with a number". There was no number. The prose
//!     and the assertion said opposite things, and the prose was the true one;
//!   * `pub async fn`, `pub unsafe fn`, `pub extern fn` - none begins with `pub fn`;
//!   * `pub type Alias = duckdb::Connection;` - type aliases were not scanned at all;
//!   * any **multiline** signature, where the type lands on a different line from the `pub`;
//!   * every DuckDB type except `Connection`, `DuckValue` and `ValueRef`.
//!
//! That is the same failure this project keeps finding: a property asserted in prose that the code
//! did not deliver. RFC-0042 §14 keeps DuckDB, which turns this boundary from a transitional
//! measurement into a standing commitment, so the gate now has to be one.
//!
//! # The two rules, and why `pub(crate)` is not simply banned
//!
//! §6 says the engine must not escape *the analytical boundary*. A `pub(crate)` item does not escape
//! the crate, so banning it outright would be inventing a stricter rule than the RFC asks for and
//! would fail on day one against `graft.rs`. Banning nothing is what we had. So:
//!
//!   1. **Crate-external `pub`**: zero tolerance, every module, no allowlist. A DuckDB type in a
//!      genuinely public signature is the thing §6 names.
//!   2. **`pub(crate)` and friends**: allowed, but **pinned site by site** with a count - which is
//!      what the old comment promised and never did. Growth needs a deliberate edit here.
//!
//! # Deriving the prohibited names instead of listing them
//!
//! The old list named three types and missed the rest. Enumerating a forbidden vocabulary over a
//! growing library is the denylist failure `analytics.rs` already wrote down. So each file's
//! prohibited set is **read out of its own `use duckdb::...` imports**, including `as` renames, plus
//! any fully-qualified `duckdb::` path. Add `use duckdb::Appender` and the scan covers `Appender`
//! with no edit here.
//!
//! Note it must *not* be a bare list of plausible type names: `analytics.rs` returns
//! `serde_json::Value`, and a scan for a bare `Value` would call that a leak.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The six sites slice 0 inventoried, with their roles. See `docs/rfcs/0042-slice0-bom.md`.
const KNOWN: &[&str] = &[
    "analytics.rs",             // general SQL, views, hot+cold federation
    "entities.rs",              // the admissible function vocabulary, from duckdb_functions()
    "entity_lower.rs",          // AST for lowering authored SQL to a circuit
    "graft.rs",                 // canonical plan, engine version, determinism gate
    "seal.rs",                  // segment-binding oracle (test-only)
    "authored_entity_spike.rs", // RFC-0041 spike, reachable via `nuthatch bench`
];

/// Internal (`pub(crate)`) signatures that currently carry a DuckDB type, pinned with their count.
///
/// **This is the number the old doc comment claimed to record and did not.** Five of them, all in
/// `graft.rs`, all taking `&Connection`: the parser/canonicalisation role slice 0 inventoried and
/// RFC-0042 §14 keeps. They are crate-internal, so §6's boundary is not crossed - but they are the
/// role a future removal has to replace, so they are counted rather than tolerated silently.
///
/// The list may **shrink**. Growth is a deliberate edit and a question in review.
const INTERNAL_EXPOSURE: &[(&str, &str)] = &[
    ("graft.rs", "canonical_plan"),
    ("graft.rs", "engine_version"),
    ("graft.rs", "parser_connection"),
    ("graft.rs", "build"),
    ("graft.rs", "determinism_gate"),
];

// ---------------------------------------------------------------------------------------------
// Scanner. Pure functions over source text so the regression controls at the bottom can drive them
// with synthetic input - a gate whose own parser is untested is the thing this file is about.
// ---------------------------------------------------------------------------------------------

/// Remove line comments, block comments and string/char literal bodies.
///
/// Comments matter because this file's own prose names every symbol it guards, and a scan that reads
/// documentation passes with the guarded thing deleted. String literals matter because an error
/// message containing `pub fn foo(conn: &Connection)` would otherwise register as a declaration.
fn strip_comments_and_strings(src: &str) -> String {
    let b = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < b.len() {
        // raw string: r"..." / r#"..."#
        if b[i] == b'r' && i + 1 < b.len() && (b[i + 1] == b'"' || b[i + 1] == b'#') {
            let mut j = i + 1;
            let mut hashes = 0;
            while j < b.len() && b[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                j += 1;
                let close: String = format!("\"{}", "#".repeat(hashes));
                if let Some(end) = src[j..].find(&close) {
                    out.push(' ');
                    i = j + end + close.len();
                    continue;
                }
                out.push(' ');
                break;
            }
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let mut depth = 1;
            i += 2;
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                } else {
                    if b[i] == b'\n' {
                        out.push('\n');
                    }
                    i += 1;
                }
            }
            continue;
        }
        if b[i] == b'"' {
            i += 1;
            out.push('"');
            while i < b.len() {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == b'"' {
                    i += 1;
                    break;
                }
                if b[i] == b'\n' {
                    out.push('\n');
                }
                i += 1;
            }
            out.push('"');
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// The DuckDB-derived names this file makes visible, read from its own `use duckdb::...` items.
///
/// Handles `use duckdb::Connection;`, `use duckdb::{Config, Connection};`,
/// `use duckdb::types::{Value as DuckValue, ValueRef};`. The bound name is what a signature would
/// mention, so an `as` rename contributes the rename.
fn duckdb_names(clean: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let flat: String = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut rest = flat.as_str();
    while let Some(p) = rest.find("use duckdb::") {
        let after = &rest[p + "use duckdb::".len()..];
        let end = after.find(';').unwrap_or(after.len());
        let item = &after[..end];
        // Take the braced group if there is one, else the final path segment.
        let group = match (item.find('{'), item.rfind('}')) {
            (Some(a), Some(b)) if b > a => item[a + 1..b].to_string(),
            _ => item.rsplit("::").next().unwrap_or("").to_string(),
        };
        for part in group.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            // `Value as DuckValue` binds `DuckValue`; `types::Foo` binds `Foo`.
            let bound = part
                .rsplit(" as ")
                .next()
                .unwrap_or(part)
                .rsplit("::")
                .next()
                .unwrap_or(part)
                .trim()
                .trim_end_matches('}');
            if !bound.is_empty() && bound.chars().next().is_some_and(|c| c.is_alphabetic()) {
                names.insert(bound.to_string());
            }
        }
        rest = &after[end.min(after.len())..];
    }
    names
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Item {
    name: String,
    kind: String,
    /// `true` for a bare `pub`, `false` for `pub(crate)` / `pub(super)` / `pub(in path)`.
    external: bool,
    signature: String,
}

/// Every `pub`-ish item declaration with its **complete** signature, however many lines it spans.
///
/// The signature runs from the `pub` to the first `{`, `;` or `=` that is not inside `(...)`, `[...]`
/// or `<...>` - so a multiline argument list, a `where` clause and a generic bound are all included,
/// which is precisely what the old one-line prefix match could not do.
fn public_items(clean: &str) -> Vec<Item> {
    const KINDS: &[&str] = &[
        "fn", "struct", "enum", "trait", "type", "const", "static", "union", "mod",
    ];
    let b = clean.as_bytes();
    let mut items = Vec::new();
    let mut i = 0;
    while let Some(rel) = clean[i..].find("pub") {
        let start = i + rel;
        i = start + 3;
        // `pub` must be a whole token at a declaration position.
        let before_ok = start == 0
            || !(b[start - 1] as char).is_alphanumeric()
                && b[start - 1] != b'_'
                && b[start - 1] != b':';
        if !before_ok {
            continue;
        }
        let mut j = start + 3;
        let mut external = true;
        // optional `(crate)` / `(super)` / `(in ::path)`
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == b'(' {
            external = false;
            let mut depth = 0;
            while j < b.len() {
                if b[j] == b'(' {
                    depth += 1;
                } else if b[j] == b')' {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                j += 1;
            }
        }
        // modifiers, then the kind keyword
        let mut kind = String::new();
        loop {
            while j < b.len() && (b[j] as char).is_whitespace() {
                j += 1;
            }
            let ws = j;
            while j < b.len() && ((b[j] as char).is_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let word = &clean[ws..j];
            if word.is_empty() {
                break;
            }
            if KINDS.contains(&word) {
                kind = word.to_string();
                break;
            }
            if !matches!(word, "async" | "unsafe" | "extern" | "default") {
                break;
            }
            // `extern "C"` - skip the ABI string
            while j < b.len() && (b[j] as char).is_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == b'"' {
                j += 1;
                while j < b.len() && b[j] != b'"' {
                    j += 1;
                }
                j += 1;
            }
        }
        if kind.is_empty() {
            continue;
        }
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        let ns = j;
        while j < b.len() && ((b[j] as char).is_alphanumeric() || b[j] == b'_') {
            j += 1;
        }
        let name = clean[ns..j].to_string();
        // signature body to the first terminator at nesting depth zero
        let mut k = j;
        let (mut paren, mut angle, mut brack) = (0i32, 0i32, 0i32);
        while k < b.len() {
            match b[k] {
                b'(' => paren += 1,
                b')' => paren -= 1,
                b'[' => brack += 1,
                b']' => brack -= 1,
                b'<' => angle += 1,
                // `->` must not decrement the generic depth
                b'>' if k > 0 && b[k - 1] != b'-' => angle -= 1,
                b'{' | b';' | b'=' if paren <= 0 && brack <= 0 && angle <= 0 => break,
                _ => {}
            }
            k += 1;
        }
        // For `type`/`const`/`static` the interesting half is after the `=`.
        let end = if matches!(kind.as_str(), "type" | "const" | "static") {
            let mut m = k;
            while m < b.len() && b[m] != b';' {
                m += 1;
            }
            m.min(b.len())
        } else {
            k.min(b.len())
        };
        items.push(Item {
            name,
            kind,
            external,
            signature: clean[start..end]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        });
        i = end.max(i);
    }
    items
}

/// Does this signature mention a DuckDB type - by a name the file imported, or by a `duckdb::` path?
fn mentions_duckdb(signature: &str, names: &BTreeSet<String>) -> bool {
    if signature.contains("duckdb::") {
        return true;
    }
    names.iter().any(|n| {
        signature.match_indices(n.as_str()).any(|(at, _)| {
            let before = signature[..at].chars().next_back();
            let after = signature[at + n.len()..].chars().next();
            let boundary = |c: Option<char>| !c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            boundary(before) && boundary(after)
        })
    })
}

fn src_files() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().to_string(),
                std::fs::read_to_string(&p).unwrap_or_default(),
            )
        })
        .collect();
    out.sort();
    assert!(
        out.len() > 20,
        "found only {} source files - the scan has stopped matching reality (#913 shape 1)",
        out.len()
    );
    out
}

/// Which files reach DuckDB at all. Comments stripped first: this file's own prose names every site,
/// and a gate that matches its own documentation passes with the guarded thing gone.
fn duckdb_sites() -> BTreeSet<String> {
    src_files()
        .into_iter()
        .filter(|(_, body)| {
            let clean = strip_comments_and_strings(body);
            clean.contains("duckdb::") || clean.contains("use duckdb")
        })
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn the_engine_does_not_spread_beyond_the_known_sites() {
    let found = duckdb_sites();
    assert!(
        !found.is_empty(),
        "no DuckDB sites found at all - either the scan broke or the engine is gone, and only one of \
         those is plausible today"
    );
    let known: BTreeSet<String> = KNOWN.iter().map(|s| s.to_string()).collect();
    let new: Vec<&String> = found.difference(&known).collect();
    assert!(
        new.is_empty(),
        "these modules reach DuckDB and are not in the slice-0 inventory:\n  {new:?}\n\n\
         RFC-0042 §14 keeps the engine, which makes this boundary a standing commitment rather than a \
         transitional measurement; every new site is one more entry on any future deletion checklist. \
         If this is deliberate, add it to `KNOWN` with its role and say why in the PR (#936)."
    );
    let gone: Vec<&String> = known.difference(&found).collect();
    assert!(
        gone.is_empty(),
        "these are in the inventory but no longer reach DuckDB: {gone:?}. Good news, and \
         `docs/rfcs/0042-slice0-bom.md` now overstates the role count - update both together."
    );
}

/// Rule 1: nothing crate-external, anywhere in `src/`, may mention a DuckDB type in its signature.
///
/// Every module, not just the analytical two - a leak from a module nobody inventoried is worse, not
/// better. There is no allowlist here on purpose: §6's boundary is exactly this.
#[test]
fn no_public_signature_anywhere_exposes_a_duckdb_type() {
    let mut leaks: Vec<String> = Vec::new();
    for (name, body) in src_files() {
        let clean = strip_comments_and_strings(&body);
        let names = duckdb_names(&clean);
        if names.is_empty() && !clean.contains("duckdb::") {
            continue;
        }
        for it in public_items(&clean) {
            if it.external && mentions_duckdb(&it.signature, &names) {
                leaks.push(format!(
                    "{name}: {} {} :: {}",
                    it.kind, it.name, it.signature
                ));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "a DuckDB type escapes into a crate-external signature. RFC-0042 §6 asks for a boundary \
         across which \"DuckDB-specific connection, value or AST types do not escape\", and §14 keeps \
         the engine, so this boundary is now permanent rather than transitional:\n{leaks:#?}"
    );
}

/// Rule 2: `pub(crate)` exposure is allowed but pinned, with the number the old comment promised.
#[test]
fn internal_duckdb_exposure_is_pinned_and_may_only_shrink() {
    let mut found: BTreeSet<(String, String)> = BTreeSet::new();
    for (name, body) in src_files() {
        let clean = strip_comments_and_strings(&body);
        let names = duckdb_names(&clean);
        if names.is_empty() && !clean.contains("duckdb::") {
            continue;
        }
        for it in public_items(&clean) {
            if !it.external && mentions_duckdb(&it.signature, &names) {
                found.insert((name.clone(), it.name));
            }
        }
    }
    let pinned: BTreeSet<(String, String)> = INTERNAL_EXPOSURE
        .iter()
        .map(|(f, n)| (f.to_string(), n.to_string()))
        .collect();

    let added: Vec<_> = found.difference(&pinned).collect();
    assert!(
        added.is_empty(),
        "new crate-internal signatures carry a DuckDB type, and this list may only shrink:\n\
         {added:#?}\n\n\
         These do not cross §6's boundary - they are internal - but they are the roles any future \
         removal has to replace, and slice 0 costed a fixed set of them. If this is deliberate, add \
         it to `INTERNAL_EXPOSURE` and say in the PR which role it serves (#976)."
    );
    let removed: Vec<_> = pinned.difference(&found).collect();
    assert!(
        removed.is_empty(),
        "these pinned internal sites no longer carry a DuckDB type: {removed:#?}. That is the \
         direction of travel - delete them from `INTERNAL_EXPOSURE` in the same commit."
    );
    assert_eq!(
        found.len(),
        5,
        "the internal-exposure count changed. It was five, all in `graft.rs`, all `&Connection` - \
         the parser/canonicalisation role RFC-0042 §14 keeps. Found: {found:#?}"
    );
}

// ---------------------------------------------------------------------------------------------
// Regression controls (#976). A gate is not proven by passing; it is proven by failing on demand.
// Each case is a form the previous one-line prefix scan could not see.
// ---------------------------------------------------------------------------------------------

fn scan(src: &str) -> Vec<Item> {
    let clean = strip_comments_and_strings(src);
    let names = duckdb_names(&clean);
    public_items(&clean)
        .into_iter()
        .filter(|it| mentions_duckdb(&it.signature, &names))
        .collect()
}

#[test]
fn control_a_multiline_signature_is_caught() {
    let found = scan(
        "use duckdb::Connection;\n\
         pub fn wide(\n    label: &str,\n    conn: &Connection,\n) -> u32 { 0 }\n",
    );
    assert_eq!(found.len(), 1, "multiline signature missed: {found:#?}");
    assert!(found[0].external, "should be classed crate-external");
}

#[test]
fn control_a_type_alias_is_caught() {
    let found = scan("pub type Handle = duckdb::Connection;\n");
    assert_eq!(found.len(), 1, "type alias missed: {found:#?}");
    assert_eq!(found[0].kind, "type");
}

#[test]
fn control_pub_async_and_unsafe_fns_are_caught() {
    let found = scan(
        "use duckdb::Connection;\n\
         pub async fn a(c: &Connection) {}\n\
         pub unsafe fn b(c: &Connection) {}\n",
    );
    assert_eq!(found.len(), 2, "async/unsafe fns missed: {found:#?}");
}

#[test]
fn control_pub_crate_is_seen_but_classed_internal() {
    let found = scan("use duckdb::Connection;\npub(crate) fn c(c: &Connection) {}\n");
    assert_eq!(found.len(), 1, "pub(crate) missed entirely: {found:#?}");
    assert!(
        !found[0].external,
        "pub(crate) must not count as crate-external - that is the distinction rule 1 and rule 2 rest on"
    );
}

#[test]
fn control_a_renamed_import_is_caught() {
    let found = scan(
        "use duckdb::types::{Value as DuckValue, ValueRef};\n\
         pub fn v(x: DuckValue) {}\npub fn r(y: ValueRef) {}\n",
    );
    assert_eq!(found.len(), 2, "`as` rename or ValueRef missed: {found:#?}");
}

#[test]
fn control_a_type_not_imported_from_duckdb_is_not_a_leak() {
    // The false-positive direction. `serde_json::Value` is `analytics.rs`'s actual return type, and a
    // scan for a bare list of plausible names would call it a leak and be relaxed into uselessness.
    let found = scan("use duckdb::Connection;\npub fn j() -> serde_json::Value { todo!() }\n");
    assert!(
        found.is_empty(),
        "false positive on serde_json::Value: {found:#?}"
    );
}

#[test]
fn control_prose_and_string_literals_do_not_register() {
    // The gate must not match its own documentation, nor an error message quoting a signature.
    let found = scan(
        "use duckdb::Connection;\n\
         /// pub fn documented(c: &Connection) - described, not declared.\n\
         // pub fn commented(c: &Connection)\n\
         pub fn real() -> &'static str { \"pub fn quoted(c: &Connection)\" }\n",
    );
    assert!(
        found.is_empty(),
        "comment or string literal registered as a declaration: {found:#?}"
    );
}
