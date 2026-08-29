//! #932 - an answer must not be labelled more current than the rows it is made of.
//!
//! `/sql` executed its query against a maintained relation and *then* built the response, whose
//! `provenance.entities[].applied_through` called `applied_through()` afresh. Two read-lock
//! acquisitions, rows first, watermark second. A batch landing between them produced an answer whose
//! rows were from block N carrying the label N+8.
//!
//! Measured on the alpha soak: **1 in 12** reads on a 0.25s-block chain, 0 in 28 on a 12s one. The
//! relation was never wrong and every mismatch self-corrected on the next read - the *label* was
//! wrong, and the label is the claim an agent cites.
//!
//! The fix is a discipline (take both off one guard) and a discipline can be undone by one careless
//! line, so this is the mechanism. It is a **source** gate rather than a behavioural one because the
//! race needs a batch to land inside a window of microseconds; asserting it never happens would be a
//! test that passes for the wrong reason on a quiet machine, which is the shape #913 is about.

use std::path::PathBuf;

fn serve_rs() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/serve.rs");
    let raw = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    // Scan the code, not the commentary. A gate that greps its own explanatory prose passes with the
    // guarded thing deleted - that happened twice in this sprint already.
    let stripped: String = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        stripped.len() < raw.len(),
        "no comments stripped from serve.rs - the filter has stopped working and this gate would be \
         matching prose"
    );
    stripped
}

/// Every remaining bare `applied_through()` in the serving path must be one of the two cases where
/// there are no rows for it to disagree with.
#[test]
fn the_serving_path_does_not_reread_the_watermark() {
    let s = serve_rs();
    let calls: Vec<(usize, String)> = s
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("applied_through()"))
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect();

    // Guard against a vacuous pass: if the scan finds nothing at all, either the file changed shape
    // or the pattern stopped matching, and "no violations" would mean nothing.
    assert!(
        !calls.is_empty(),
        "no `applied_through()` calls found in serve.rs at all - the scan has stopped matching \
         reality (#913 shape 1), fix it rather than trusting the pass"
    );

    // The two legitimate ones: the standalone /metrics gauge, which reports no rows beside it, and
    // the `unwrap_or_else` fallback for an entity that contributed no rows to the query.
    let violations: Vec<&(usize, String)> = calls
        .iter()
        .filter(|(_, l)| !l.contains("unwrap_or_else") && !l.starts_with("e.applied_through()"))
        .collect();

    assert!(
        violations.is_empty(),
        "these read the watermark separately from the rows it describes (#932). Take both off one \
         guard - `rows_as_json_with_watermark`, `len_and_watermark`, `get_with_watermark`, \
         `head_rows_with_watermark` - and pass the captured value down:\n{violations:#?}"
    );
}

/// `derived_provenance` takes the watermark as an argument. If it ever reads one itself again, every
/// caller silently reverts to two acquisitions, and the compiler will not complain.
#[test]
fn derived_provenance_is_handed_its_watermark() {
    let s = serve_rs();
    let sig = s
        .split_once("fn derived_provenance(")
        .expect("derived_provenance has been renamed or removed")
        .1;
    let head = &sig[..sig.find(") -> Value").unwrap_or(sig.len().min(400))];
    assert!(
        head.contains("applied_through: u64"),
        "derived_provenance must be *given* the watermark captured with the rows, not read one \
         (#932). Signature was:\nfn derived_provenance({head})"
    );
}
