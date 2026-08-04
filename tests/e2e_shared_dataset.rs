//! **Two mounts, one dataset, one backfill** (RFC-0032 slice 2).
//!
//! The payoff of keying data by nest identity is that mounting the same nest twice costs nothing:
//! one store, one place in the cursor, one indexing pass, two routes. This test is the proof, and it
//! is deliberately built around what would go *wrong* rather than what should go right.
//!
//! A note on the acceptance criterion RFC-0032 §9 proposed - "assert the RPC request count is that of
//! one backfill". It is weaker than it sounds, because RFC-0012's shared cursor already fetches the
//! **union** of its nests' logs once per window and demuxes: two *distinct* nests on one cursor
//! already cost one nest's worth of RPC chatter. So the request count is asserted here as a
//! regression guard against a second fetch appearing, not as the discriminator. The discriminators
//! are the ones below it: one dataset directory, one store, and identical bytes through both doors.
//!
//! What *would* have happened without this slice is worth stating: iterating aliases instead of
//! datasets calls `Store::open` on the same redb file twice, and redb refuses. The old shape did not
//! silently double-index - it could not come up at all.

mod common;

use std::path::Path;
use std::sync::Arc;

use nuthatch::roost::{Dataset, Roost, DATA_DIR, ROOST_FILE};
use nuthatch::store::HotStore;
use nuthatch::{analytics, health::RoostHealth, indexer, migrate, roost};

use common::tape::*;

/// A one-nest roost in the pre-2.0 layout, migrated, then given a second alias onto the same
/// identity - which is exactly what a second tenant mounting the same nest produces.
fn two_mounts_one_nest(root: &Path) -> String {
    std::fs::write(
        root.join(ROOST_FILE),
        "[roost]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
         rpc_urls = []\nnests = [\"primary\"]\n",
    )
    .unwrap();
    let nest = root.join("nests").join("primary");
    std::fs::create_dir_all(&nest).unwrap();
    scaffold_nest(&nest, "primary", USDC);

    migrate::run(root, false).expect("migrate");
    let nid = Roost::load(root).unwrap().mounts[0].nid.clone();

    // The second mount: a different alias, the same identity. No second directory, no second copy.
    let mut roost = Roost::load(root).unwrap();
    roost.roost.nests.push("mirror".to_string());
    roost.mounts.push(nuthatch::roost::Mount {
        alias: "mirror".to_string(),
        nid: nid.clone(),
    });
    std::fs::write(
        root.join(ROOST_FILE),
        toml::to_string_pretty(&roost).unwrap(),
    )
    .unwrap();
    nid
}

fn rows(dir: &Path) -> Vec<serde_json::Value> {
    analytics::query(
        dir,
        &format!(
            "SELECT * FROM \"{}\" ORDER BY block_number, log_index",
            transfer_table("primary")
        ),
    )
    .unwrap()
}

/// Drive a roost dir the way `roost::dev` does - datasets, not aliases - and hand back the served
/// states plus the tape, so the test asserts against the same wiring production uses.
async fn bring_up(
    root: &Path,
) -> (
    Vec<(String, nuthatch::serve::AppState)>,
    Arc<TapeSource>,
    std::collections::HashMap<String, u64>,
) {
    let roost = Roost::load(root).unwrap();
    let datasets = roost.datasets(root);

    let tape = Arc::new(TapeSource::new());
    let a1 = account(1);
    let a2 = account(2);
    for b in 1..=6u64 {
        tape.insert_block(
            b,
            transfers_block(
                b,
                0,
                1_700_000_000 + b,
                USDC,
                &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
            ),
        );
    }
    tape.advance_tip_to(6);

    let mounted: Vec<_> = datasets
        .iter()
        .map(|ds| {
            let cfg = nuthatch::config::Config::load(&ds.dir).unwrap();
            (ds.canonical().to_string(), ds.dir.clone(), cfg)
        })
        .collect();

    let health = Arc::new(RoostHealth::new());
    for ds in &datasets {
        health.register(ds.canonical(), "arbitrum-one");
    }
    let cursor = indexer::spawn_roost(
        tape.clone(),
        mounted,
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
    .expect("spawn_roost");

    let landed = wait_until(POLL_TIMEOUT, || {
        cursor
            .states
            .iter()
            .all(|(_, s)| s.store.get_meta("last_block").ok().flatten().as_deref() == Some("6"))
    })
    .await;
    assert!(landed, "the dataset did not index to the tip in time");

    // Seal past finality, so the rows compared later are real Parquet segments rather than an empty
    // cold side that would make the comparison vacuous.
    tape.advance_finalized_to(4);
    tape.insert_block(7, empty_block(7, 0, 1_700_000_007));
    tape.advance_tip_to(7);
    let sealed = wait_until(POLL_TIMEOUT, || {
        cursor
            .states
            .iter()
            .all(|(_, s)| s.store.sealed_through() >= 4)
    })
    .await;
    assert!(sealed, "the dataset did not seal in time");

    let mut estimates = std::collections::HashMap::new();
    let states = roost::fan_out_aliases(&datasets, cursor.states, &health, &mut estimates);

    cursor.ingest.abort();
    for (_, w) in cursor.alert_workers {
        w.abort();
    }
    (states, tape, estimates)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_mounts_of_one_nest_share_a_single_dataset() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let nid = two_mounts_one_nest(root);

    // --- The grouping: two aliases collapse to one dataset before anything is opened. ---
    let roost = Roost::load(root).unwrap();
    let datasets = roost.datasets(root);
    assert_eq!(
        datasets.len(),
        1,
        "two aliases over one identity must be ONE dataset, got {datasets:?}"
    );
    let ds: &Dataset = &datasets[0];
    assert_eq!(
        ds.refcount(),
        2,
        "the derived refcount must see both mounts"
    );
    assert_eq!(ds.aliases, vec!["primary", "mirror"]);
    assert_eq!(ds.canonical(), "primary", "the first alias indexes");
    assert_eq!(ds.nid.as_deref(), Some(nid.as_str()));

    // Exactly one directory on disk, whatever the aliases say.
    let dirs: Vec<_> = std::fs::read_dir(root.join(DATA_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(
        dirs.len(),
        1,
        "a second dataset directory was created: {dirs:?}"
    );

    // --- The runtime: index once, serve twice. ---
    let (states, tape, estimates) = bring_up(root).await;
    let calls_shared = tape.logs_call_count();

    // The alias must not be charged a footprint the dataset already paid, or the per-cursor budget
    // would refuse a mount that costs nothing.
    assert_eq!(
        estimates.get("mirror").copied(),
        Some(0),
        "a shared mount was charged RSS a second time - sharing must not be taxed"
    );

    let served: std::collections::HashMap<_, _> = states.into_iter().collect();
    assert_eq!(served.len(), 2, "both aliases must be served");
    let primary = served.get("primary").expect("primary served");
    let mirror = served.get("mirror").expect("mirror served");

    // Two doors, one room. Same directory, and the same store object behind both.
    assert_eq!(primary.dir, mirror.dir, "the aliases resolved to two rooms");
    assert!(
        Arc::ptr_eq(&primary.store, &mirror.store),
        "the aliases hold two different stores - the dataset was opened twice"
    );

    // And the data really is identical through either door, not merely pointing at the same path.
    let via_primary = rows(&primary.dir);
    let via_mirror = rows(&mirror.dir);
    assert!(!via_primary.is_empty(), "the fixture indexed nothing");
    assert_eq!(via_primary, via_mirror);

    // --- The regression guard: sharing must not add fetching. ---
    // Control: the identical fixture with a single mount. If the shared run cost more `logs` calls
    // than this, a second backfill appeared.
    let solo_root = tempfile::tempdir().unwrap();
    let solo_root = solo_root.path();
    std::fs::write(
        solo_root.join(ROOST_FILE),
        "[roost]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
         rpc_urls = []\nnests = [\"primary\"]\n",
    )
    .unwrap();
    let solo_nest = solo_root.join("nests").join("primary");
    std::fs::create_dir_all(&solo_nest).unwrap();
    scaffold_nest(&solo_nest, "primary", USDC);
    migrate::run(solo_root, false).unwrap();
    let (_, solo_tape, _) = bring_up(solo_root).await;

    assert!(
        calls_shared <= solo_tape.logs_call_count(),
        "mounting the same nest twice cost more log fetches than mounting it once \
         ({calls_shared} vs {}) - a second backfill is running",
        solo_tape.logs_call_count()
    );
}

/// A shared dataset's health must be shared too. Only the canonical mount is ever quarantined, so an
/// alias that reported its own state would say "indexing" while nothing was - the false-healthy
/// answer `/ready` exists to prevent.
#[test]
fn an_alias_reports_the_health_of_the_dataset_it_shares() {
    let health = RoostHealth::new();
    health.register("primary", "arbitrum-one");
    health.register_alias("mirror", "primary", "arbitrum-one");

    assert!(
        health.status("mirror").is_none(),
        "both should start healthy"
    );

    health.quarantine_nest("primary", "decode blew up".into(), 1, Some(30));

    let via_alias = health
        .status("mirror")
        .expect("the alias must inherit the quarantine of the dataset it shares");
    assert_eq!(via_alias.reason, "decode blew up");
    assert!(
        health.unhealthy().iter().any(|(n, _)| n == "mirror"),
        "a shared dataset's fault must hold readiness for every alias serving it"
    );
}
