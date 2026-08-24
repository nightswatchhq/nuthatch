//! #681 - the drift gate for the RFC index.
//!
//! `docs/rfcs/README.md`'s status column is a hand-written summary of each `docs/rfcs/NNNN-*.md`'s
//! own hand-written `Status:` line. Two *authored* surfaces that are supposed to agree, with nothing
//! watching them - which is the shape that has now bitten five times (#417, #658, #661, #679, #687).
//! `skill_refs.rs` solved the same shape for the builder skill, and the reason it worked is that one
//! side of its comparison is generated. Here neither side is, so this check is deliberately narrower:
//! it compares one coarse lifecycle keyword per side and understands none of the surrounding prose.
//!
//! What it therefore cannot tell you is that a doc has stopped describing reality - only that the
//! index and the doc have stopped describing *each other*. That is the smaller half of the problem
//! and the only half a test can hold.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The status lifecycle from the index's own preamble: Draft → Accepted → Implemented →
/// (Superseded / Parked).
const LIFECYCLE: [&str; 5] = ["Draft", "Accepted", "Implemented", "Superseded", "Parked"];

fn rfc_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/rfcs")
}

#[test]
fn rfc_index_agrees_with_each_rfc_doc_header() {
    let index = index_rows();
    assert!(
        index.len() >= 38,
        "only {} rows parsed out of the RFC index - the table shape changed",
        index.len()
    );

    let mut offenders = Vec::new();
    for (num, row) in &index {
        let path = rfc_dir().join(&row.file);
        let doc = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("RFC-{num} index row links to {}: {e}", row.file));

        let Some(doc_status) = doc_status_line(&doc) else {
            offenders.push(format!(
                "RFC-{num}: {} has no `Status:` line for the index row to agree with",
                row.file
            ));
            continue;
        };

        let index_kw = lifecycle_keyword(&row.status);
        let doc_kw = lifecycle_keyword(&doc_status);
        if index_kw != doc_kw {
            offenders.push(format!(
                "RFC-{num}: index says {}, doc says {}\n    index: {}\n    doc:   {}",
                describe(index_kw),
                describe(doc_kw),
                truncate(&row.status),
                truncate(&doc_status),
            ));
        }
    }

    assert!(
        offenders.is_empty(),
        "the RFC index and these RFCs' own headers disagree - fix whichever is stale \
         (the index is reconciled against docs/progress-log.md; a doc header is not):\n{}",
        offenders.join("\n")
    );
}

/// Every RFC on disk has a row, so a new one cannot ship unindexed - the other direction of #658.
#[test]
fn every_rfc_doc_has_an_index_row() {
    let index = index_rows();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(rfc_dir()).unwrap() {
        let path = entry.unwrap().path();
        let Some(num) = rfc_number(&path) else {
            continue;
        };
        if !index.contains_key(&num) {
            missing.push(format!(
                "RFC-{num} ({}) is not in the index",
                display(&path)
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "docs/rfcs/README.md is missing rows:\n{}",
        missing.join("\n")
    );
}

struct Row {
    file: String,
    status: String,
}

/// The index table's rows, keyed by RFC number. The table is `| RFC | Title | Depends on | Status |`,
/// and the status prose may itself contain a `|` (RFC-0034 quotes `sql = "open" | "deny"`), so only
/// the first three columns are split off and the remainder is the status.
fn index_rows() -> BTreeMap<String, Row> {
    let text = std::fs::read_to_string(rfc_dir().join("README.md")).expect("the RFC index");
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("| [") else {
            continue;
        };
        let Some((num, rest)) = rest.split_once(']') else {
            continue;
        };
        if num.len() != 4 || !num.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Some(file) = rest
            .strip_prefix('(')
            .and_then(|r| r.split_once(')'))
            .map(|(f, _)| f)
        else {
            continue;
        };
        // Past the leading `|`, the columns are title, depends-on, then everything else.
        let cells: Vec<&str> = line.trim_matches('|').splitn(4, '|').collect();
        if cells.len() < 4 {
            continue;
        }
        rows.insert(
            num.to_string(),
            Row {
                file: file.to_string(),
                status: cells[3].trim().to_string(),
            },
        );
    }
    rows
}

/// The text after `Status:` on an RFC's own header line, in either form the corpus uses:
/// `- Status: ...` (0001-0035) or `**Status:** ...` (0036 onward).
fn doc_status_line(doc: &str) -> Option<String> {
    doc.lines().find_map(|line| {
        let s = line.trim().trim_start_matches("- ").trim_start();
        let s = s.strip_prefix("**Status:**").or_else(|| {
            s.strip_prefix("Status:")
                .or_else(|| s.strip_prefix("**Status**:"))
        })?;
        Some(s.trim().to_string())
    })
}

/// The first lifecycle keyword to appear in a status text, case-insensitively. Deliberately
/// position-based rather than prefix-based: both surfaces lead with the state and then qualify it
/// in prose ("**Parked after pilot**", "**§1 Implemented · §2 retired**", "**Optimism implemented,
/// Polygon shipped but not yet trustworthy**"), and the leading word is the one that has drifted
/// every time. `None` means the text names no state at all, which is itself a finding.
fn lifecycle_keyword(status: &str) -> Option<&'static str> {
    let haystack = status.to_ascii_lowercase();
    LIFECYCLE
        .iter()
        .filter_map(|kw| word_position(&haystack, &kw.to_ascii_lowercase()).map(|at| (at, *kw)))
        .min()
        .map(|(_, kw)| kw)
}

/// The byte offset of `needle` in `haystack` as a whole word, so "drafted" is not "Draft" - RFC-0030
/// and 0031 both say "drafted 2026-08-03" one clause after their real status.
fn word_position(haystack: &str, needle: &str) -> Option<usize> {
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let at = from + rel;
        let end = at + needle.len();
        let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
        let after_ok = end == bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn describe(kw: Option<&'static str>) -> String {
    kw.map_or_else(|| "no lifecycle status".to_string(), |k| format!("`{k}`"))
}

fn truncate(s: &str) -> String {
    let clean = s.replace('\n', " ");
    match clean.char_indices().nth(100) {
        Some((at, _)) => format!("{}...", &clean[..at]),
        None => clean,
    }
}

/// #676: RFC-0015's status used to say slices 2-6 were in progress six releases after they
/// shipped. The acceptance bar is the two-minute first query, not the slice list; the status
/// line tracking slices hid that the bar was unmet (#672). This fails if the status line goes
/// back to talking about work in progress.
#[test]
fn rfc_0015_status_line_does_not_say_in_progress() {
    let doc =
        std::fs::read_to_string(rfc_dir().join("0015-the-delightful-core.md")).expect("RFC-0015");
    let status = doc_status_line(&doc).expect("RFC-0015 has a Status line");
    assert!(
        !status.to_ascii_lowercase().contains("in progress"),
        "RFC-0015 status still talks about slices in progress:\n{status}"
    );
}

fn rfc_number(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if path.extension()?.to_str()? != "md" || name == "README.md" {
        return None;
    }
    let num = name.split('-').next()?;
    (num.len() == 4 && num.chars().all(|c| c.is_ascii_digit())).then(|| num.to_string())
}

fn display(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().to_string()
}
