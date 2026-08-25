//! #661: launch copy that still describes 2.5.0. The RFC-index half of that issue is done; the
//! launch pages were the rest. A phrase returning here is a published claim that is false.
//!
//! #843: it named three files, and `docs/launch/` holds five. The exact banned phrase appended to
//! `home-turf.md` - a real, tracked launch doc - left this suite green. It walks the tree now, so a
//! page added tomorrow is covered the day it is added rather than the day somebody remembers to list
//! it. `ivm_claims` already made this exact fix one file away, and says why in its own comment:
//! "so a new page cannot reintroduce the claim without being added to SURFACES first".
//!
//! What this still does **not** do, deliberately: `STALE` is a list of sentences that were actually
//! published, so the same false claim in new words passes. Closing that means guessing at English,
//! and a regression guard against known-bad text is worth having on its own terms. The limitation is
//! recorded rather than papered over - see #843.

use std::path::{Path, PathBuf};

fn launch_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/launch")
}

const STALE: &[&str] = &[
    "has no executor yet",
    "cannot resolve yet",
    "aren't indexed at all yet",
    "It's v2.5.0",
    "currently reasoned from first principles",
];

/// Every `.md` under `dir`, recursively.
fn markdown(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

#[test]
fn launch_copy_does_not_describe_2_5_0() {
    let root = launch_dir();
    let mut files = Vec::new();
    markdown(&root, &mut files);
    files.sort();

    // An empty walk would pass silently and prove nothing - the same absent-means-healthy shape the
    // rest of this sprint is about. The directory is committed, so zero files means the walk broke.
    assert!(
        files.len() >= 5,
        "expected the committed launch pages under {}, walked {} file(s) - the walk is broken, \
         not the copy",
        root.display(),
        files.len()
    );

    let mut offenders = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let name = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string();
        for phrase in STALE {
            if text.contains(phrase) {
                offenders.push(format!("{name}: still contains `{phrase}`"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "launch copy still describes a binary we no longer ship (walked {} page(s)):\n{}",
        files.len(),
        offenders.join("\n")
    );
}
