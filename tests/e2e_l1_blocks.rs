//! `[extract] l1_blocks` (#1839): `l1_block_number` for the blocks that carry a nest's logs, from the
//! headers of those blocks only, the same on the seal-direct and hot paths, and rolled back by a reorg.

mod common;

use std::sync::Arc;
use std::time::Duration;

use nuthatch::{indexer, seal, serve};

use common::tape::*;

const L1_BASE: u64 = 20_000_000;

/// Blocks 1..=8 with a transfer at 2, 4 and 7, each header reporting `L1_BASE + 3 * b`.
fn tape() -> Arc<TapeSource> {
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=8u64 {
        let fx = if [2, 4, 7].contains(&b) {
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(a1.as_str(), a2.as_str(), 100 * b as u128)],
            )
        } else {
            empty_block(b, 0, 1_700_000_000 + b)
        };
        tape.set_l1_block_number(&fx.hash, L1_BASE + 3 * b);
        tape.insert_block(b, fx);
    }
    tape.advance_tip_to(8);
    tape.advance_finalized_to(5);
    tape
}

async fn serve_nest(
    tape: Arc<TapeSource>,
    dir: &std::path::Path,
    cfg: nuthatch::config::Config,
) -> (String, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let rt = indexer::spawn_nest(
        tape,
        dir.to_path_buf(),
        cfg,
        None,
        true,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = serve::router(serve::SharedNest::new(rt.state));
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), rt.ingest)
}

async fn sql(base: &str, q: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{base}/sql"))
        .query(&[("q", q)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn l1_rows(base: &str) -> Vec<(u64, u64)> {
    let v = sql(
        base,
        "SELECT block_number, l1_block_number FROM l1_blocks ORDER BY block_number",
    )
    .await;
    v["rows"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    (
                        r["block_number"].as_u64().unwrap(),
                        // `/sql` serves a u64 column as a decimal string.
                        r["l1_block_number"]
                            .as_str()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or_else(|| panic!("{v}")),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `l1_blocks` as soon as `done` holds of it, or as last seen when the wait runs out.
async fn poll_l1_rows(base: &str, done: impl Fn(&[(u64, u64)]) -> bool) -> Vec<(u64, u64)> {
    let start = std::time::Instant::now();
    loop {
        let rows = l1_rows(base).await;
        if done(&rows) || start.elapsed() >= POLL_TIMEOUT {
            return rows;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l1_block_numbers_land_for_log_bearing_blocks_on_both_paths_and_follow_a_reorg() {
    let dir = tempfile::tempdir().unwrap();
    scaffold_nest(dir.path(), "usdc", USDC);
    let toml = dir.path().join("nuthatch.toml");
    let mut raw = std::fs::read_to_string(&toml).unwrap();
    raw.push_str("\n[extract]\nl1_blocks = true\n");
    std::fs::write(&toml, raw).unwrap();
    let cfg = nuthatch::config::Config::load(dir.path()).expect("load");
    let tape = tape();
    let (base, ingest) = serve_nest(tape.clone(), dir.path(), cfg).await;

    let want = vec![(2, L1_BASE + 6), (4, L1_BASE + 12), (7, L1_BASE + 21)];
    let got = poll_l1_rows(&base, |r| r.len() >= 3).await;
    assert_eq!(
        got, want,
        "one row per log-bearing block, with its header's value"
    );

    // Blocks 2 and 4 are past finality and were sealed by seal-direct; 7 is in the hot store.
    let manifest = seal::load_manifest(dir.path()).expect("manifest");
    assert!(
        manifest
            .tables
            .get(nuthatch::registry::L1_BLOCKS_TABLE)
            .is_some_and(|s| !s.is_empty()),
        "seal-direct must seal l1_blocks: {:?}",
        manifest.tables.keys().collect::<Vec<_>>()
    );

    let joined = sql(
        &base,
        "SELECT t.block_number, l.l1_block_number FROM usdc__transfer t \
         JOIN l1_blocks l USING (block_number, block_hash) ORDER BY t.block_number",
    )
    .await;
    assert_eq!(
        joined["count"], 3,
        "every transfer joins its l1 row: {joined}"
    );

    let mut asked = tape.headers_asked();
    asked.sort_unstable();
    asked.dedup();
    assert_eq!(asked, vec![2, 4, 7], "headers only for log-bearing blocks");

    // Block 9 lands on the hot path above the checkpoint at 8, then a fork empties it and moves the
    // transfer to block 10: the rollback has to take 9's l1 row with it.
    let (a1, a2) = (account(1), account(2));
    let nine = transfers_block(
        9,
        0,
        1_700_000_009,
        USDC,
        &[(a1.as_str(), a2.as_str(), 900)],
    );
    tape.set_l1_block_number(&nine.hash, L1_BASE + 27);
    tape.insert_block(9, nine);
    tape.insert_block(10, empty_block(10, 0, 1_700_000_010));
    let got = poll_l1_rows(&base, |r| r.len() >= 4).await;
    assert_eq!(got.last(), Some(&(9, L1_BASE + 27)), "{got:?}");

    let ten = transfers_block(
        10,
        1,
        1_700_000_110,
        USDC,
        &[(a1.as_str(), a2.as_str(), 1001)],
    );
    tape.set_l1_block_number(&ten.hash, 99);
    tape.reorg(8, vec![empty_block(9, 1, 1_700_000_109), ten]);
    let after = poll_l1_rows(&base, |r| r.last() == Some(&(10, 99))).await;
    assert_eq!(
        after,
        vec![
            (2, L1_BASE + 6),
            (4, L1_BASE + 12),
            (7, L1_BASE + 21),
            (10, 99)
        ],
        "the reorg must roll back block 9's l1 row and add block 10's"
    );
    assert!(!ingest.is_finished(), "ingest stopped: {:?}", ingest.await);

    ingest.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_nest_without_l1_blocks_has_no_table_and_fetches_no_headers() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    let tape = tape();
    let (base, ingest) = serve_nest(tape.clone(), dir.path(), cfg).await;

    let start = std::time::Instant::now();
    let mut landed = false;
    while !landed && start.elapsed() < POLL_TIMEOUT {
        let n = sql(&base, "SELECT count(*) AS n FROM usdc__transfer").await;
        landed = n["rows"][0]["n"].as_u64() == Some(3);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(landed, "transfers never landed");

    let tables: serde_json::Value = reqwest::get(format!("{base}/tables"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["table"].as_str())
        .collect();
    assert!(!names.contains(&"l1_blocks"), "{names:?}");
    let manifest = seal::load_manifest(dir.path()).expect("manifest");
    assert!(
        !manifest
            .tables
            .contains_key(nuthatch::registry::L1_BLOCKS_TABLE),
        "{:?}",
        manifest.tables.keys().collect::<Vec<_>>()
    );
    assert!(
        tape.headers_asked().is_empty(),
        "{:?}",
        tape.headers_asked()
    );

    ingest.abort();
}
