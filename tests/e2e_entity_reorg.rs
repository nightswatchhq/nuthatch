//! RFC-0041 §5.2, end to end: an authored incremental entity survives a reorg.
//!
//! **Why this is not a unit test.** `entity_view`'s own property test feeds both the `+1` and the
//! `-1` side from the same source, and passes. The real paths do not agree by construction: an
//! insertion is a `DecodedRow` straight from the registry, while a retraction is reconstructed from
//! the hot store's JSON. DBSP cancels by key, so if those two produce different rows nothing
//! cancels - the entity keeps the fact that was rolled back *and* gains a row at weight `-1` that
//! nothing will ever cancel again.
//!
//! That divergence is invisible to any test that builds both sides itself. It is only visible here,
//! through `spawn_nest`, `entities_in_range` and a real fork.

mod common;

use std::sync::Arc;

use nuthatch::indexer;

use common::tape::*;

/// A canonical block: one transfer of `100 * b` from account 1 to account 2.
fn canonical_block(b: u64) -> BlockFixture {
    transfers_block(
        b,
        0,
        1_700_000_000 + b,
        USDC,
        &[(account(1).as_str(), account(2).as_str(), (100 * b) as u128)],
    )
}

/// A replacement block: a *different* recipient and amount, so an entity that failed to retract the
/// orphaned row would hold a group the clean run does not have, rather than merely a wrong total.
fn replacement_block(b: u64) -> BlockFixture {
    transfers_block(
        b,
        1,
        1_700_000_500 + b,
        USDC,
        &[(
            account(3).as_str(),
            account(4).as_str(),
            (7_000 + b) as u128,
        )],
    )
}

/// `SELECT to, SUM(value) FROM usdc__transfer GROUP BY to`, declared the way an author would.
///
/// No filter and no join on purpose: this test is about the retraction path, and every operator it
/// does not use is one that cannot mask a divergence by discarding the row that carries it.
fn declare_entity(dir: &std::path::Path) {
    std::fs::write(
        dir.join("entities.toml"),
        r#"[[entities]]
name = "received"
sql = "SELECT t.to, SUM(t.value) FROM usdc__transfer t GROUP BY t.to"
key = ["to"]
max_rows = 10000
"#,
    )
    .expect("write entities.toml");
}

const CHAIN_LEN: u64 = 8;

async fn spawn_with_entity(
    dir: &std::path::Path,
    tape: Arc<TapeSource>,
    tip: u64,
) -> indexer::NestRuntime {
    let cfg = scaffold_nest(dir, "usdc", USDC);
    declare_entity(dir);
    let rt = indexer::spawn_nest(
        tape,
        dir.to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest with a declared entity");
    let store = rt.state.store.clone();
    let tip_str = tip.to_string();
    let landed = wait_until(POLL_TIMEOUT, || {
        store.get_meta("last_block").ok().flatten().as_deref() == Some(tip_str.as_str())
    })
    .await;
    assert!(landed, "nest did not index to block {tip} in time");
    rt
}

fn shutdown(rt: indexer::NestRuntime) {
    rt.ingest.abort();
    if let Some(w) = rt.alert_worker {
        w.abort();
    }
}

/// The entity's relation, rendered so a mismatch reads as data rather than as `Row(vec![...])`.
fn relation(rt: &indexer::NestRuntime) -> Vec<(String, String)> {
    let entity = rt
        .state
        .entities
        .first()
        .expect("the nest declared one entity");
    entity.flush();
    let mut out: Vec<(String, String)> = entity
        .relation()
        .iter()
        .map(|(k, v)| (format!("{k:?}"), format!("{v:?}")))
        .collect();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entity_converges_on_a_clean_replay_after_a_reorg() {
    let fork = 4;

    // Reorged nest: index the canonical chain, then rewrite everything above the fork.
    let reorged_dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);
    let rt = spawn_with_entity(reorged_dir.path(), tape.clone(), CHAIN_LEN).await;

    let before = relation(&rt);
    assert!(
        !before.is_empty(),
        "the entity must have folded the canonical chain before the reorg is worth anything"
    );

    tape.reorg(
        fork,
        ((fork + 1)..=CHAIN_LEN).map(replacement_block).collect(),
    );

    let store = rt.state.store.clone();
    let want_hash = block_hash(CHAIN_LEN, 1);
    let converged = wait_until(POLL_TIMEOUT, || {
        match store.get_entity(&nuthatch::store::Store::entity_key(CHAIN_LEN, 0)) {
            Ok(Some(raw)) => serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| v["block_hash"].as_str().map(|h| h == want_hash))
                .unwrap_or(false),
            _ => false,
        }
    })
    .await;
    assert!(converged, "the reorg did not reconverge in time");
    let after = relation(&rt);
    let health = rt.state.entities[0].fault();
    shutdown(rt);
    assert_eq!(health, None, "the entity must still be running");

    // Clean nest: index the post-reorg chain directly, never having seen the orphaned blocks.
    let clean_dir = tempfile::tempdir().unwrap();
    let clean_tape = Arc::new(TapeSource::new());
    for b in 1..=fork {
        clean_tape.insert_block(b, canonical_block(b));
    }
    for b in (fork + 1)..=CHAIN_LEN {
        clean_tape.insert_block(b, replacement_block(b));
    }
    clean_tape.advance_tip_to(CHAIN_LEN);
    let clean_rt = spawn_with_entity(clean_dir.path(), clean_tape, CHAIN_LEN).await;
    let clean = relation(&clean_rt);
    shutdown(clean_rt);

    assert_ne!(
        before, clean,
        "the reorg must actually change the entity, or this test proves nothing"
    );
    assert_eq!(
        after, clean,
        "a reorged entity must equal a clean replay over the post-reorg chain"
    );
}
