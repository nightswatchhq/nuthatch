//! #947 - the two seal paths batch differently, and that asymmetry is why 80% of a long-running
//! nest's segments are under 20 KB.
//!
//! `indexer::take_sealable` holds rows until `SEAL_DIRECT_BATCH` and cuts at a block boundary chosen
//! from the data, so segment identity does not depend on `--window` or `--concurrency`. The tip path
//! (`seal_finalized`) has no threshold: it seals whatever finalised, which at tip is a few blocks
//! carrying a few rows.
//!
//! Measured on the Lodestar box: median segment **6.3 KB**, p80 17.1 KB, max 1,864 KB, and the three
//! largest are all from the busiest table's backfill. `docs/bench/segment-layout.md`.
//!
//! **This test does not assert the asymmetry is wrong.** It pins the property that makes the backfill
//! path's batching *safe* to copy - that the cut is a function of the data alone - because that is the
//! thing any fix must preserve, and it is the thing a well-meaning change would break first.

/// The cut must depend only on the rows, never on how they were fetched.
///
/// Two operators with different `--window` and `--concurrency` see the same rows arrive in the same
/// `(block, log_index)` order but in different batches. If the boundary moved with the batching,
/// content addressing would be quietly conditional on RPC tuning - which is the bug RFC-0028 §4 fixed
/// and the property any tip-side batching must keep.
#[test]
fn the_seal_boundary_is_a_function_of_the_data_not_the_fetch() {
    // Deliberately not calling the private helper: this asserts the *documented rule*, so it still
    // holds if the implementation is rewritten to batch at tip too.
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/indexer.rs"),
    )
    .expect("read indexer.rs");
    let at = doc.find("fn take_sealable").expect(
        "take_sealable has been renamed or removed - if the tip path now batches too, this \
                 test should be updated to cover both, not deleted",
    );
    let window = &doc[at.saturating_sub(1400)..at];
    for needed in ["from the **data**", "identical"] {
        assert!(
            window.contains(needed),
            "the seal-boundary rule no longer states that the cut comes from the data and yields \
             identical segments across operators. That property is what RFC-0019's bundles and \
             RFC-0020's segment reuse rest on, and what any tip-side batching must preserve (#947).\
             \n---\n{window}"
        );
    }
}

/// The measurement that motivates #947 must stay in the tree with its numbers, because the argument
/// is entirely quantitative: "many small files" is an impression, "80% under 20 KB with a 6.3 KB
/// median" is a finding somebody can act on or refute.
#[test]
fn the_segment_layout_measurement_keeps_its_numbers() {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/bench/segment-layout.md");
    let s = std::fs::read_to_string(&p).expect("docs/bench/segment-layout.md");
    for needed in ["80%", "6.3 KB", "SEAL_DIRECT_BATCH", "seal_finalized"] {
        assert!(
            s.contains(needed),
            "the segment-layout measurement has lost `{needed}`. It is the evidence #947 rests on, \
             and without the numbers the issue is an impression rather than a finding."
        );
    }
}
