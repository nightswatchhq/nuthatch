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

use proptest::prelude::*;

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

/// Abort a nest **and wait for it to have stopped**, so its redb lock is released.
async fn shutdown_and_settle(rt: indexer::NestRuntime) {
    rt.ingest.abort();
    let _ = (&mut { rt.ingest }).await;
    if let Some(w) = rt.alert_worker {
        w.abort();
        let _ = w.await;
    }
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

async fn entity_converges_after_reorg(fork: u64) {
    assert!((1..CHAIN_LEN).contains(&fork));
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

/// #866 criterion 13: `--seal-direct` either rebuilds entities from the sealed corpus before serving,
/// or refuses the combination clearly. It refuses - and the refusal is the point.
///
/// Seal-direct writes finalized history straight to Parquet without passing it through the ingest
/// path, so an entity fed from decoded windows would see none of it. Left to run, the nest would
/// complete, serve, and answer with an empty relation: *"a completed run with an empty entity is the
/// failure this criterion exists for."*
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seal_direct_refuses_a_nest_that_declares_an_entity() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);

    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    declare_entity(dir.path());

    let err = indexer::spawn_nest(
        tape,
        dir.path().to_path_buf(),
        cfg,
        None,
        true, // --seal-direct
        1,
        Some(2),
        false,
        None,
    )
    .await
    .err()
    .expect("seal-direct plus a declared entity must not start");

    let err = format!("{err:#}");
    assert!(err.contains("--seal-direct cannot be combined"), "{err}");
    assert!(
        err.contains("`received`"),
        "the refusal must name the entity: {err}"
    );
    assert!(
        err.contains("served empty"),
        "and say what would otherwise happen: {err}"
    );
}

/// The same nest without `--seal-direct` starts and folds - so the refusal above is about the
/// combination and not about entities being unable to start at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_nest_without_seal_direct_starts_and_folds() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);

    let rt = spawn_with_entity(dir.path(), tape, CHAIN_LEN).await;
    let rows = relation(&rt);
    shutdown(rt);
    assert!(!rows.is_empty(), "the entity folded the chain");
}

/// #866 criterion 8: a dead entity circuit is a **terminal fault for the nest**, not a quiet freeze.
///
/// §5.2: *"A circuit thread dying is a terminal fault for that nest under RFC-0026. Serving frozen
/// derived state as healthy is not graceful degradation; it is a lie with a pleasant HTTP status."*
///
/// The fault is induced through the declared bound rather than by reaching into the view, so the
/// path under test is the real one: the bound bites inside the circuit thread, the thread stops, and
/// the ingest loop's health check has to notice and escalate. A test that killed the thread directly
/// would prove the check and not the escalation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_entity_circuit_ends_the_nest_rather_than_freezing_it() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);

    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    // One row admitted, and the first window carries two.
    std::fs::write(
        dir.path().join("entities.toml"),
        r#"[[entities]]
name = "received"
sql = "SELECT t.to, SUM(t.value) FROM usdc__transfer t GROUP BY t.to"
key = ["to"]
max_rows = 1
"#,
    )
    .unwrap();

    let rt = indexer::spawn_nest(
        tape,
        dir.path().to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("the nest starts - the bound is a runtime fault, not a load-time refusal");

    // The loop must *end*. Freezing here - the entity dead, the cursor still polling, `/ready` still
    // 200 - is the failure this criterion is named for, and it would show up as a timeout.
    let outcome = tokio::time::timeout(POLL_TIMEOUT, rt.ingest)
        .await
        .expect("the ingest loop must terminate, not carry on with a dead entity")
        .expect("the task itself should not panic");

    let err = format!(
        "{:#}",
        outcome.expect_err("a dead entity circuit is a terminal fault")
    );
    assert!(
        err.contains("entity `received`"),
        "the fault must name the entity: {err}"
    );
    assert!(
        err.contains("max_rows"),
        "and the cause, not merely that something stopped: {err}"
    );
}

/// #866 criterion 8, the other half: *"It cannot freeze quietly while sibling nests continue to
/// report it healthy."*
///
/// Two nests on one cursor. One declares an entity with a bound its first window breaks; the other
/// declares none. The faulted nest must be quarantined **by name**, and the healthy one must keep
/// indexing - the blast radius is the nest, not the cursor.
///
/// Both nests sit on the same chain deliberately: that is what puts them on one cursor, and a blast
/// radius that widened to the cursor is precisely what this is watching for. `fail_fast` is `false`,
/// production's default - it is the one setting under which a quarantined nest is *expected* to take
/// its cursor with it, so passing this test under it would mean nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_faulted_entity_quarantines_its_own_nest_and_leaves_its_neighbour_indexing() {
    use nuthatch::health::RuntimeHealth;

    let doomed_dir = tempfile::tempdir().unwrap();
    let healthy_dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);

    // Distinct nest names, and not merely for readability: `NestIngest` takes its name from
    // `config.nest.name`, which is what `RuntimeHealth` keys a quarantine by. Scaffolding both as
    // `usdc` registers both under one identity, and the isolation this test is about becomes
    // unobservable - the first version did exactly that and reported no quarantine at all.
    //
    // The contract alias moves with the name, so the doomed nest's table is `doomed__transfer`.
    let doomed_cfg = scaffold_nest(doomed_dir.path(), "doomed", USDC);
    std::fs::write(
        doomed_dir.path().join("entities.toml"),
        r#"[[entities]]
name = "received"
sql = "SELECT t.to, SUM(t.value) FROM doomed__transfer t GROUP BY t.to"
key = ["to"]
max_rows = 1
"#,
    )
    .unwrap();
    // The neighbour declares no entity at all, so nothing about it can fault for this reason.
    let healthy_cfg = scaffold_nest(healthy_dir.path(), "healthy", USDC);

    let health = Arc::new(RuntimeHealth::new());
    health.register("doomed", &doomed_cfg.nest.chain);
    health.register("healthy", &healthy_cfg.nest.chain);

    let cursor = indexer::spawn_runtime(
        tape,
        vec![
            (
                "doomed".to_string(),
                doomed_dir.path().to_path_buf(),
                doomed_cfg,
            ),
            (
                "healthy".to_string(),
                healthy_dir.path().to_path_buf(),
                healthy_cfg,
            ),
        ],
        None,
        false,
        1,
        Some(2),
        false,
        None,
        health.clone(),
        false,
    )
    .await
    .expect("spawn_runtime");

    // The faulted nest is quarantined, and the reason names the entity rather than saying only that
    // something stopped.
    let quarantined = wait_until(POLL_TIMEOUT, || health.status("doomed").is_some()).await;
    assert!(
        quarantined,
        "the nest whose entity died must be quarantined, not left quietly frozen"
    );
    let reason = health.status("doomed").unwrap().reason;
    assert!(
        reason.contains("entity `received`"),
        "the quarantine must say which entity: {reason}"
    );

    // The neighbour reaches the tip - after its sibling died, not before it started.
    let want = CHAIN_LEN.to_string();
    let neighbour = cursor
        .states
        .iter()
        .find(|(n, _)| n == "healthy")
        .expect("the healthy nest is on this cursor");
    let landed = wait_until(POLL_TIMEOUT, || {
        neighbour
            .1
            .store
            .get_meta("last_block")
            .ok()
            .flatten()
            .as_deref()
            == Some(want.as_str())
    })
    .await;
    assert!(
        landed,
        "the healthy nest must keep indexing - the blast radius is the nest, not the cursor"
    );
    assert!(
        health.status("healthy").is_none(),
        "and it must not be quarantined by its neighbour's fault"
    );

    cursor.ingest.abort();
}

/// **RFC-0041 §5.3 and #865.** A restarted entity comes back complete, from this nest's own stored
/// history, **without a single historical RPC call**.
///
/// The seed is sealed segments plus the unsealed hot tail, fed through the same circuit as any
/// window - §5.1's "backfill uses larger batches, but not different semantics" taken literally. It
/// is not a separate seed relation combined with a delta, which is what §5.3's wording describes and
/// what comes apart for any entity with a join: a finalized row joining a hot one is in neither half.
///
/// Two things make this more than "it is not empty": the restarted nest is compared against a **cold
/// nest indexed over the whole chain**, which is criterion 5's "matches uninterrupted execution"; and
/// the tape's `logs` call count is read across the restart, which is #865's claim stated as a number.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_entity_is_rebuilt_from_stored_history_with_no_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);

    let first = spawn_with_entity(dir.path(), tape.clone(), CHAIN_LEN).await;
    let before = relation(&first);
    assert!(!before.is_empty(), "the first run must actually fill it");
    shutdown_and_settle(first).await;

    // Restart over the same directory. Nothing new to index, so any `logs` call the restart makes is
    // a historical one - which is exactly what #865 forbids.
    let calls_before = tape.logs_call_count();
    let second = spawn_with_entity(dir.path(), tape.clone(), CHAIN_LEN).await;
    let after = relation(&second);
    let unavailable = second.state.entities[0].unavailable().map(str::to_string);
    let applied = second.state.entities[0].applied_through();
    shutdown_and_settle(second).await;

    assert_eq!(
        unavailable, None,
        "a seeded entity is available, not waiting for a rebuild it already had"
    );
    assert_eq!(
        after, before,
        "the rebuilt relation must be the one the entity had before the restart"
    );
    assert_eq!(
        applied, CHAIN_LEN,
        "and it must answer for the head it was seeded through, not block 0"
    );

    // The tip-follower polls, so this is not zero calls - it is zero calls *per historical block*.
    // Re-indexing eight blocks through a two-block window would take four or more `logs` calls on its
    // own, before the poll loop adds any.
    let historical = tape.logs_call_count() - calls_before;
    assert!(
        historical < (CHAIN_LEN / 2) as usize,
        "the seed must read stored history, not re-fetch it: {historical} logs calls across a \
         restart of a {CHAIN_LEN}-block chain"
    );

    // And the whole thing must match a nest that never restarted.
    let cold_dir = tempfile::tempdir().unwrap();
    let cold = spawn_with_entity(cold_dir.path(), tape, CHAIN_LEN).await;
    let clean = relation(&cold);
    shutdown_and_settle(cold).await;
    assert_eq!(
        after, clean,
        "criterion 5: a seeded entity matches uninterrupted execution"
    );
}

/// **The half of the seed that the hot store cannot answer.**
///
/// Sealing *prunes* the sealed rows out of hot (`prune_and_set_meta`), so once a range is finalized
/// the only copy of it is Parquet. A seed that reads the hot store alone therefore rebuilds a
/// relation covering the unsealed tail and nothing else - populated, plausible, and missing all of
/// history.
///
/// The previous restart test could not see that: an eight-block chain seals nothing, so skipping
/// sealed segments entirely left it green. This one finalizes through block 8 first, and the
/// assertion below can only hold if the sealed rows were read back from Parquet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seeded_entity_includes_the_sealed_range_the_hot_store_no_longer_holds() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=10u64 {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(10);
    let first = spawn_with_entity(dir.path(), tape.clone(), 10).await;

    // Finalize through 8 so [1,8] seals and leaves the hot store, then index past it.
    tape.advance_finalized_to(8);
    for b in 11..=14u64 {
        tape.insert_block(b, empty_block(b, 0, 1_700_000_100 + b));
    }
    tape.advance_tip_to(14);
    let store = first.state.store.clone();
    assert!(
        wait_until(POLL_TIMEOUT, || store.sealed_through() >= 8).await,
        "range [1,8] did not seal in time - without it this test is about nothing"
    );
    assert!(
        wait_until(POLL_TIMEOUT, || {
            store.get_meta("last_block").ok().flatten().as_deref() == Some("14")
        })
        .await,
        "nest did not index to block 14"
    );

    // The premise, asserted rather than assumed: the sealed rows are gone from hot.
    let hot_rows = store.entities_in_range(1, 8).unwrap();
    assert!(
        hot_rows.is_empty(),
        "the sealed range is still in the hot store, so reading hot alone would be enough and this \
         test would prove nothing: {} row(s)",
        hot_rows.len()
    );

    let before = relation(&first);
    assert!(!before.is_empty());
    drop(store);
    shutdown_and_settle(first).await;

    let second = spawn_with_entity(dir.path(), tape.clone(), 14).await;
    let after = relation(&second);
    let unavailable = second.state.entities[0].unavailable().map(str::to_string);
    shutdown_and_settle(second).await;

    assert_eq!(unavailable, None, "the seed must have succeeded");
    assert_eq!(
        after, before,
        "a seeded entity must carry the sealed range, which only Parquet still holds"
    );
}

/// **RFC-0041 §8 / #864 criterion 4, as a property.** *"Randomized apply/retract/replacement
/// sequences converge byte-for-byte to a clean replay."*
///
/// The fixed-fork case above is the readable one; this is the one that covers the depth. Both run
/// through `spawn_nest`, so the `+1` side comes from the decode registry and the `-1` side is
/// reconstructed from the hot store's JSON - two representations that must produce the same `Row` or
/// nothing cancels. `entity_view`'s own property test builds both sides itself and cannot see that.
///
/// Depth rather than fork position, and stratified, for the reason #291 records: a uniform fork over
/// a fixed chain is not a uniform depth, and a generator that never produces the deep case passes
/// forever while proving nothing. Case count is low because every case drives two full nests.
fn entity_reorg_depth() -> impl Strategy<Value = u64> {
    prop_oneof![
        1u64..=2,                       // the everyday tip flutter
        3u64..=(CHAIN_LEN / 2),         // mid
        (CHAIN_LEN / 2 + 1)..CHAIN_LEN, // deep: unwinds most of the chain
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(6))]
    #[test]
    fn an_entity_converges_at_any_reorg_depth(depth in entity_reorg_depth()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(entity_converges_after_reorg(CHAIN_LEN - depth));
    }
}

/// The fixed case, kept as the readable one and as a guard on the generator's own reachability: a
/// deep fork here would be a different test from a shallow one, and this pins the mid case.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_entity_converges_on_a_clean_replay_after_a_reorg() {
    entity_converges_after_reorg(4).await;
}

/// **#822 criterion 2: a direct keyed read does not invoke DuckDB or scan canonical fact history.**
///
/// Asserted by construction rather than by inspecting a plan: `derived_key` reads the circuit's own
/// output map. What this test can check from outside is that the answer is *right*, that it carries
/// the provenance criterion 9 asks for, and that its applied-through is the **entity's** watermark
/// rather than the nest's head - which is the difference between reporting and pretending.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_derived_keyed_read_answers_from_maintained_state_with_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let tape = Arc::new(TapeSource::new());
    for b in 1..=CHAIN_LEN {
        tape.insert_block(b, canonical_block(b));
    }
    tape.advance_tip_to(CHAIN_LEN);
    let rt = spawn_with_entity(dir.path(), tape, CHAIN_LEN).await;
    rt.state.entities[0].flush();

    let entity = &rt.state.entities[0];
    let want: i128 = (1..=CHAIN_LEN).map(|b| (100 * b) as i128).sum();
    let got = entity
        .relation()
        .get(&nuthatch::entity_row::Row(vec![
            nuthatch::entity_row::Scalar::Str(account(2)),
        ]))
        .cloned();
    assert_eq!(
        got,
        Some(nuthatch::entity_row::Row(vec![
            nuthatch::entity_row::Scalar::Int(want)
        ])),
        "the maintained relation must hold the answer a keyed read would return"
    );

    // Criterion 9's provenance, and the part that matters: the entity answers for the head it has
    // folded. Serving the nest's head here is exactly how a partial relation gets stamped current.
    assert_eq!(entity.applied_through(), CHAIN_LEN);
    assert!(entity.is_current(CHAIN_LEN));
    assert!(!entity.is_current(CHAIN_LEN + 1));
    assert_eq!(entity.unavailable(), None);
    assert_eq!(entity.fault(), None);

    shutdown_and_settle(rt).await;
}
