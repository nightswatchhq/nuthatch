//! #744 - the clean USDC tape must not carry a recorded error.
//!
//! `usdc-120-fixed` preserves 429s on purpose: that is how #784 was found, and it is why the
//! seal-direct arm aborted instead of answering the storage-path question. The clean tape is the
//! one both arms actually replay. A 429 in *that* file would make the comparison abort again,
//! and the published ratio would be the 0.92x failure mode with a tape around it.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn the_clean_usdc_tape_contains_no_recorded_error() {
    let path = repo_root().join("docs/bench/tapes/usdc-120-fixed-clean/entries.jsonl");
    let body = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e} - the clean tape is how #744 gets a number",
            path.display()
        )
    });
    assert!(
        !body.is_empty(),
        "{} is empty - a tape with no keys cannot answer a storage-path question",
        path.display()
    );
    let mut keys = 0usize;
    for (i, line) in body.lines().enumerate() {
        keys += 1;
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), i + 1));
        let outcomes = v
            .get("outcomes")
            .and_then(|o| o.as_array())
            .unwrap_or_else(|| panic!("{}:{}: missing outcomes", path.display(), i + 1));
        for o in outcomes {
            let outcome = o.get("outcome").and_then(|s| s.as_str()).unwrap_or("");
            assert_ne!(
                outcome,
                "err",
                "{}:{}: recorded error on key {} - this tape is the one both arms replay; \
                 a 429 here is the #784 tape wearing the clean name. Put errors in \
                 docs/bench/tapes/usdc-120-fixed/, not here.",
                path.display(),
                i + 1,
                v.get("key").and_then(|k| k.as_str()).unwrap_or("?")
            );
        }
    }
    assert!(keys > 0, "{} has no entries", path.display());
}

#[test]
fn the_429_tape_is_untouched_by_the_clean_gate() {
    // Control: the test above would fail this file. If it starts passing, the 429 reproduction
    // has been silently "cleaned" and #784 is no longer replayable.
    let path = repo_root().join("docs/bench/tapes/usdc-120-fixed/entries.jsonl");
    let body = std::fs::read_to_string(&path).expect("the 429 tape stays");
    assert!(
        body.contains("\"outcome\":\"err\""),
        "usdc-120-fixed must keep at least one recorded error; it is the #784 reproduction"
    );
}
