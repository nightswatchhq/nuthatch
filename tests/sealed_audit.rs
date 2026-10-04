//! The sampled audit of sealed ranges against a second endpoint (#1786, RFC-0049 §10 item 5).
//!
//! #1670: an `eth_getLogs` answer missing a log from the middle of a range seals as if whole, and
//! nothing on the ingest path notices. These seal a tape through `backfill_direct`, then audit it
//! against a second tape that differs by one log in block 30.

mod common;

use std::path::Path;

use nuthatch::sealed_audit::{audit_range, sample_ranges, Auditor, RangeReport};
use nuthatch::{config::Config, indexer};

use common::tape::*;

const OMITTED_BLOCK: u64 = 30;

/// Blocks 1..=60, two transfers each. `omit` drops block 30's second log, as a provider with a hole
/// in its answer would; `ts_shift` moves every header timestamp, which the audit must not see.
fn tape(omit: bool, ts_shift: u64) -> TapeSource {
    let tape = TapeSource::new();
    let (a1, a2) = (account(1), account(2));
    for b in 1..=60u64 {
        let mut fx = transfers_block(
            b,
            0,
            1_700_000_000 + b + ts_shift,
            USDC,
            &[(&a1, &a2, 100 * b as u128), (&a2, &a1, 100 * b as u128 + 1)],
        );
        if omit && b == OMITTED_BLOCK {
            fx.logs.pop();
        }
        tape.insert_block(b, fx);
    }
    tape.advance_tip_to(60);
    tape.advance_finalized_to(60);
    tape
}

async fn seal_from(dir: &Path, first: &TapeSource) -> Config {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    let registry = nuthatch::registry::from_nest(dir, &cfg).expect("registry");
    let addresses: Vec<String> = cfg.contracts.iter().map(|c| c.address.clone()).collect();
    let topic0s: Vec<String> = registry
        .topic0s()
        .iter()
        .map(|t| format!("0x{}", hex::encode(t)))
        .collect();
    indexer::backfill_direct(
        first,
        &registry,
        dir,
        &addresses,
        &topic0s,
        &[],
        None,
        0,
        1,
        60,
        20,
        nuthatch::chains::DEFAULT_SEAL_SPAN,
        true,
    )
    .await
    .expect("backfill_direct");
    cfg
}

/// Seal from `first`, then audit the sampled ranges against `second`.
async fn audit(first: TapeSource, second: TapeSource) -> Vec<RangeReport> {
    let dir = tempfile::tempdir().unwrap();
    let cfg = seal_from(dir.path(), &first).await;
    let auditor = Auditor::new(dir.path(), &cfg).expect("auditor");
    // A span wider than the sealed range clamps to all of it, so every sample covers block 30.
    let ranges = sample_ranges(dir.path(), &auditor, None, 7, 0..2, 1_000)
        .unwrap()
        .expect("something sealed");
    let mut reports = Vec::new();
    for (from, to) in ranges {
        reports.push(
            audit_range(dir.path(), &auditor, &second, from, to)
                .await
                .expect("audit_range"),
        );
    }
    reports
}

#[tokio::test]
async fn a_log_the_indexing_endpoint_omitted_is_reported() {
    for r in audit(tape(true, 0), tape(false, 0)).await {
        assert_eq!((r.from, r.to), (1, 60));
        assert_eq!((r.sealed_rows, r.endpoint_rows), (119, 120));
        assert_eq!(r.endpoint_only.len(), 1, "{r:?}");
        assert!(r.sealed_only.is_empty() && r.differing.is_empty(), "{r:?}");
        assert!(
            r.endpoint_only[0].contains(&format!("\"block_number\":{OMITTED_BLOCK}")),
            "{}",
            r.endpoint_only[0]
        );
        let lines = r.describe("https://second.example");
        assert!(lines[0].contains("MISMATCH: blocks 1..=60 against https://second.example: 1 row(s) the endpoint served"), "{}", lines[0]);
        assert!(lines[1].contains("endpoint only:"), "{lines:?}");
    }
}

#[tokio::test]
async fn a_log_the_audit_endpoint_omits_is_reported() {
    for r in audit(tape(false, 0), tape(true, 0)).await {
        assert_eq!(r.sealed_only.len(), 1, "{r:?}");
        assert!(
            r.endpoint_only.is_empty() && r.differing.is_empty(),
            "{r:?}"
        );
        assert!(
            r.sealed_only[0].contains(&format!("\"block_number\":{OMITTED_BLOCK}")),
            "{}",
            r.sealed_only[0]
        );
        assert_eq!(r.mismatches(), 1);
    }
}

#[tokio::test]
async fn identical_endpoints_report_clean_whatever_their_timestamps() {
    for r in audit(tape(false, 0), tape(false, 3_600)).await {
        assert_eq!(r.mismatches(), 0, "{r:?}");
        assert_eq!((r.sealed_rows, r.endpoint_rows), (120, 120));
        assert!(r.describe("x")[0].starts_with("sealed audit clean"));
    }
}

#[tokio::test]
async fn a_log_that_decodes_differently_is_reported_as_differing() {
    let second = tape(false, 0);
    let (a1, a2) = (account(1), account(2));
    second.insert_block(
        OMITTED_BLOCK,
        transfers_block(
            OMITTED_BLOCK,
            0,
            1_700_000_000 + OMITTED_BLOCK,
            USDC,
            &[(&a1, &a2, 1), (&a2, &a1, 2)],
        ),
    );
    for r in audit(tape(false, 0), second).await {
        assert_eq!(r.differing.len(), 2, "{r:?}");
        assert!(
            r.endpoint_only.is_empty() && r.sealed_only.is_empty(),
            "{r:?}"
        );
    }
}

#[tokio::test]
async fn a_range_over_the_log_budget_is_compared_only_as_far_as_it_was_fetched() {
    let dense = || {
        let tape = TapeSource::new();
        let (a1, a2) = (account(1), account(2));
        for b in 1..=60u64 {
            let transfers: Vec<(&str, &str, u128)> = (0..400)
                .map(|i| (a1.as_str(), a2.as_str(), (1_000 * b + i) as u128))
                .collect();
            tape.insert_block(
                b,
                transfers_block(b, 0, 1_700_000_000 + b, USDC, &transfers),
            );
        }
        tape.advance_tip_to(60);
        tape.advance_finalized_to(60);
        tape
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = seal_from(dir.path(), &dense()).await;
    let auditor = Auditor::new(dir.path(), &cfg).unwrap();
    let r = audit_range(dir.path(), &auditor, &dense(), 1, 60)
        .await
        .unwrap();
    let held = nuthatch::sealed_audit::SAMPLE_LOG_BUDGET as u64 / 400;
    assert_eq!((r.from, r.to, r.requested_to), (1, held, 60), "{r:?}");
    assert_eq!(r.mismatches(), 0, "{r:?}");
    assert_eq!(r.sealed_rows as u64, held * 400);
}

#[tokio::test]
async fn a_range_inside_one_segment_compares_only_its_own_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = seal_from(dir.path(), &tape(false, 0)).await;
    let auditor = Auditor::new(dir.path(), &cfg).unwrap();
    let r = audit_range(dir.path(), &auditor, &tape(false, 0), 10, 19)
        .await
        .unwrap();
    assert_eq!((r.sealed_rows, r.endpoint_rows), (20, 20), "{r:?}");
    assert_eq!(r.mismatches(), 0, "{r:?}");
}
