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

/// The seal-boundary rule is now asserted **behaviourally**, and not here.
///
/// # What this test used to be (#980)
///
/// It read `src/indexer.rs` as a string, took the 1400 characters before `fn take_sealable`, and
/// asserted that window contained the literals `"from the **data**"` and `"identical"`. It sealed
/// nothing. The window it searched *was* the doc comment, so it was a gate matching its own
/// documentation - an implementation could make segment cuts depend on fetch batching, keep the
/// prose intact, and stay green.
///
/// **It was green, and the property was false.** Writing the real test found it: `take_sealable`
/// was called once per fetched chunk (`if let`, not `while let`), so a chunk carrying several
/// multiples of `SEAL_DIRECT_BATCH` sealed only one segment and left the rest for the final flush.
/// On a 30,000-block corpus, `--window 320` produced **6 segments, largest 20,003 rows**, and
/// `--window 163840` produced **2, largest 99,993** - the same chain, different content addresses,
/// and no dedup between the two operators. Fixed in the same change.
///
/// The real assertions live in `src/indexer.rs`'s `mod tests`, because `take_sealable` is private -
/// which is the constraint that pushed the original at the source text in the first place:
///
///   * `seal_boundaries_are_identical_across_fetch_windows` - five `--window` values from 1 to
///     163,840 over one corpus, asserting identical segment counts, cut blocks, rows and remainder;
///   * `a_block_is_never_split_across_two_segments` - the other half of the documented rule;
///   * `the_observable_can_actually_see_a_boundary_move` - the control, so the two above cannot
///     pass vacuously.
///
/// What remains here is the thing an integration test *can* check and the unit tests cannot: that
/// the measurement motivating #947 keeps its numbers.

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
