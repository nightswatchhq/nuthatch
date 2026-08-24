//! #741 - a published performance number is a claim, and claims want a file rather than a memory.
//!
//! `docs/benchmarks.md` says every figure traces to a committed `docs/bench/*.json` with commit,
//! provider and hardware. Magpie existed because 8.7x / 20x were typed into that page and outlived
//! the harness. This walks every `bench/*.json` citation on that page and checks the file exists
//! and is identified. It does not run a backfill (#285/#298).

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// A report filename: `722-hot.json`, not a glob, not a path.
fn is_report_name(name: &str) -> bool {
    name.ends_with(".json")
        && !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Collect `docs/bench/<name>.json` citations. Two forms appear on the page: a markdown link
/// `](bench/722-hot.json)` and a path written in prose / backticks as `docs/bench/722-hot.json`.
/// The house-rule sentence itself says `docs/bench/*.json`; that is a glob, not a citation.
fn citations(md: &str) -> Vec<String> {
    let mut out = Vec::new();
    for pat in ["](bench/", "docs/bench/"] {
        let mut search_from = 0;
        while let Some(i) = md[search_from..].find(pat) {
            let after_start = search_from + i + pat.len();
            let after = &md[after_start..];
            let end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_'))
                .unwrap_or(after.len());
            let name = &after[..end];
            if is_report_name(name) {
                out.push(name.to_string());
            }
            search_from = after_start + end.max(1);
            if search_from >= md.len() {
                break;
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn json_string_field<'a>(raw: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let i = raw.find(&needle)?;
    let after = raw[i + needle.len()..].trim_start();
    let after = after.strip_prefix('"')?;
    let end = after.find('"')?;
    Some(&after[..end])
}

#[test]
fn cited_bench_reports_exist_and_are_identified() {
    let md = std::fs::read_to_string(repo_root().join("docs/benchmarks.md"))
        .expect("docs/benchmarks.md");
    let cited = citations(&md);
    assert!(
        !cited.is_empty(),
        "benchmarks.md cites no docs/bench/*.json - the house rule has nothing to check"
    );

    let mut missing = Vec::new();
    let mut unidentified = Vec::new();
    for name in &cited {
        let path = repo_root().join("docs/bench").join(name);
        let Ok(raw) = std::fs::read_to_string(&path) else {
            missing.push(name.clone());
            continue;
        };
        for key in ["commit", "provider", "hardware"] {
            match json_string_field(&raw, key) {
                Some(s) if !s.is_empty() => {}
                _ => unidentified.push(format!("{name} missing {key}")),
            }
        }
    }
    assert!(
        missing.is_empty(),
        "benchmarks.md cites reports that are not in the tree: {missing:?}"
    );
    assert!(
        unidentified.is_empty(),
        "cited reports must carry commit, provider, hardware (#741): {unidentified:?}"
    );
}

#[test]
fn citations_parser_ignores_globs_and_keeps_the_filename() {
    let md = "House rule: `docs/bench/*.json`.\n\
              Artifacts: [`docs/bench/722-hot.json`](bench/722-hot.json), \
              [`docs/bench/722-seal-direct.json`](bench/722-seal-direct.json).\n\
              Kept as `docs/bench/point-read-devbox.json`.\n";
    assert_eq!(
        citations(md),
        [
            "722-hot.json",
            "722-seal-direct.json",
            "point-read-devbox.json"
        ]
    );
}

/// A cited backfill report's `events_per_sec` (truncated to the integer the tables print) must
/// appear on the page. Typing 8.7x over a real artifact, Magpie's shape, fails this.
#[test]
fn cited_backfill_events_per_sec_appear_on_the_page() {
    let md = std::fs::read_to_string(repo_root().join("docs/benchmarks.md")).unwrap();
    let hay = md.replace(',', "");
    let mut missing = Vec::new();
    for name in citations(&md) {
        let raw = std::fs::read_to_string(repo_root().join("docs/bench").join(&name)).unwrap();
        let Some(eps) = json_number_field(&raw, "events_per_sec") else {
            continue;
        };
        let n = eps.trunc() as i64;
        if !hay.contains(&n.to_string()) {
            missing.push(format!("{name} events_per_sec={eps} ({n})"));
        }
    }
    assert!(
        missing.is_empty(),
        "cited backfill reports whose events/sec is not on the page: {missing:?}"
    );
}

fn json_number_field(raw: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{key}\":");
    let i = raw.find(&needle)?;
    let after = raw[i + needle.len()..].trim_start();
    let end = after.find([',', '}', '\n', ' ']).unwrap_or(after.len());
    after[..end].trim().parse().ok()
}

/// #783: the $1,192 extrapolation had no method breakdown at 20 CU/header. It must not return as
/// a current claim. The withdrawal sentence is allowed to name the figure.
#[test]
fn withdrawn_full_history_cost_is_not_a_current_claim() {
    let md = std::fs::read_to_string(repo_root().join("docs/benchmarks.md")).unwrap();
    for needle in ["~$1,192", "$1,192", "$1192"] {
        if let Some(i) = md.find(needle) {
            let lo = i.saturating_sub(80);
            let hi = (i + needle.len() + 80).min(md.len());
            let window = &md[lo..hi];
            assert!(
                window.to_ascii_lowercase().contains("withdrawn"),
                "{needle} appears without being withdrawn: …{window}…"
            );
        }
    }
}
