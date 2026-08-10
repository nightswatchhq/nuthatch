//! **Early cutoff on the serving path** (RFC-0033 §5, issue #364).
//!
//! A cosmetic edit - a comment in a view, a line of README - moves a nest's NID and leaves its *data*
//! identity alone. That is what `Manifest::data_identity` is for, and `migrate` has honoured it since
//! slice 5. The runtime never did: `MountTable::datasets` resolved `data/<nid>` straight out of the
//! mount record, and a freshly-installed edited nest resolved onto a directory holding its inputs and
//! no data at all. The nest then re-fetched the entire chain to rebuild bytes that were sitting one
//! directory away.
//!
//! **The discriminator is a provider that can no longer serve the history.** Asserting "it did not
//! re-index" against a tape that still holds every block is an absence assertion - the rows come out
//! identical either way, because a re-index reproduces them. So the second run gets a tape whose
//! blocks `1..=6` carry the same hashes and timestamps and **no logs**: the chain is still walkable,
//! the history is gone. Adoption is then the only route by which the edited nest can hold those rows,
//! and the assertion is a positive one.

mod common;

use std::path::Path;
use std::sync::Arc;

use nuthatch::runtime::{MountTable, DATA_DIR, MOUNTS_FILE};
use nuthatch::{analytics, health::RuntimeHealth, indexer, migrate, runtime};

use common::tape::*;

/// A two-nest runtime in the pre-2.0 layout, migrated into `data/<nid>` with a mount record each.
/// Two nests rather than one, per #364's acceptance: the co-tenant is what proves adoption is scoped
/// to the dataset that moved rather than applied to everything on the cursor.
fn two_nest_runtime(root: &Path) -> (String, String) {
    std::fs::write(
        root.join(MOUNTS_FILE),
        "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
         rpc_urls = []\nnests = [\"alpha\", \"beta\"]\n",
    )
    .unwrap();
    for (alias, address) in [("alpha", USDC), ("beta", ARB)] {
        let nest = root.join("nests").join(alias);
        std::fs::create_dir_all(&nest).unwrap();
        scaffold_nest(&nest, alias, address);
    }
    migrate::run(root, false, false).expect("migrate");

    let mounts = MountTable::load(root).unwrap();
    let nid = |alias: &str| {
        mounts
            .mounts
            .iter()
            .find(|m| m.alias == alias)
            .unwrap()
            .nid
            .clone()
    };
    (nid("alpha"), nid("beta"))
}

/// Blocks `1..=6`, one transfer each on both contracts, tip 6.
fn full_history() -> Arc<TapeSource> {
    let tape = Arc::new(TapeSource::new());
    let (a1, a2) = (account(1), account(2));
    for b in 1..=6u64 {
        let mut fixture = transfers_block(
            b,
            0,
            1_700_000_000 + b,
            USDC,
            &[(a1.as_str(), a2.as_str(), (100 * b) as u128)],
        );
        let arb = transfers_block(
            b,
            0,
            1_700_000_000 + b,
            ARB,
            &[(a1.as_str(), a2.as_str(), (7 * b) as u128)],
        );
        fixture.logs.extend(arb.logs);
        tape.insert_block(b, fixture);
    }
    tape.advance_tip_to(6);
    tape
}

/// The same chain as [`full_history`] - same hashes, same timestamps, so ancestry still walks - with
/// **no logs in `1..=6`**, plus two fresh empty blocks on top. A provider that pruned its history.
fn pruned_history() -> Arc<TapeSource> {
    let tape = Arc::new(TapeSource::new());
    for b in 1..=8u64 {
        tape.insert_block(b, empty_block(b, 0, 1_700_000_000 + b));
    }
    tape.advance_tip_to(8);
    tape
}

/// Drive the runtime dir exactly as `runtime::dev` does: resolve datasets, adopt, load, spawn one
/// cursor. Going through [`runtime::load_mounted`] rather than a hand-rolled loop is the point - a
/// fixture that loads configs itself proves the loader and never the mount path.
async fn bring_up(
    root: &Path,
    tape: Arc<TapeSource>,
    tip: u64,
    finalized: u64,
) -> Vec<(String, Option<String>)> {
    let mounts = MountTable::load(root).unwrap();
    let datasets = mounts.datasets(root);
    let multi_tenant = mounts.is_multi_tenant();
    let mounted = runtime::load_mounted(root, &datasets, multi_tenant).expect("load");

    let health = Arc::new(RuntimeHealth::new());
    for (name, _, _) in &mounted {
        health.register(name, "arbitrum-one");
    }
    let names: Vec<String> = mounted.iter().map(|(n, _, _)| n.clone()).collect();
    let cursor = indexer::spawn_runtime(
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
    .expect("spawn_runtime");

    let want = tip.to_string();
    let landed = wait_until(POLL_TIMEOUT, || {
        cursor.states.iter().all(|(_, s)| {
            s.store.get_meta("last_block").ok().flatten().as_deref() == Some(want.as_str())
        })
    })
    .await;
    assert!(landed, "the runtime did not reach block {tip} in time");

    // Seal past finality, so the rows compared later are real Parquet segments and the adoption is
    // not being judged on the hot store alone. Sealing runs at a window boundary, so finality moving
    // is not enough on its own - a fresh block has to arrive after it.
    tape.advance_finalized_to(finalized);
    tape.insert_block(tip + 1, empty_block(tip + 1, 0, 1_700_000_001 + tip));
    tape.advance_tip_to(tip + 1);
    let sealed = wait_until(POLL_TIMEOUT, || {
        cursor
            .states
            .iter()
            .all(|(_, s)| s.store.sealed_through() >= finalized)
    })
    .await;
    assert!(
        sealed,
        "the runtime did not seal through {finalized} in time"
    );

    // Read the heights off the live states: reopening a store the aborted cursor still holds would
    // answer `None` and say nothing about the run.
    let heights = names
        .iter()
        .map(|n| {
            let h = cursor
                .states
                .iter()
                .find(|(name, _)| name == n)
                .and_then(|(_, s)| s.store.get_meta("last_block").ok().flatten());
            (n.clone(), h)
        })
        .collect();

    // Run 2 reopens these very stores in this same process, so aborting is not enough: `abort` only
    // *requests* cancellation, and until the task is actually polled again it still holds its `Store`
    // clone - and with it redb's file lock. Await each handle so the tasks are gone, then drop our own
    // clones, before any of this returns to a caller that is about to open the same databases.
    cursor.ingest.abort();
    let _ = cursor.ingest.await;
    for (_, w) in cursor.alert_workers {
        w.abort();
        let _ = w.await;
    }
    drop(cursor.states);
    heights
}

/// Every transfer a nest has sealed, in chain order. A nest that indexed nothing has no table at
/// all, and that is an empty history rather than a broken fixture - reported as `[]` so the
/// assertion that cares can say what it wanted instead of unwrapping a catalog error.
fn transfers(dir: &Path, nest: &str) -> Vec<serde_json::Value> {
    analytics::query(
        dir,
        &format!(
            "SELECT block_number, value FROM \"{}\" ORDER BY block_number, log_index",
            transfer_table(nest)
        ),
    )
    .unwrap_or_default()
}

/// Install an edited copy of a dataset's inputs under the identity they now hash to - what any
/// install of an edited nest package leaves behind - and point the mount record at it.
fn install_edited(root: &Path, alias: &str, old_nid: &str, edit: impl FnOnce(&Path)) -> String {
    let staging = root.join("staging");
    copy_inputs(&MountTable::data_dir(root, old_nid), &staging);
    edit(&staging);

    let nid = nuthatch::blob::nest_nid(&staging).expect("the edited nest must hash");
    assert_ne!(
        nid, old_nid,
        "the edit must move the NID, or nothing is being tested"
    );
    let dest = MountTable::data_dir(root, &nid);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::rename(&staging, &dest).unwrap();

    let mut mounts = MountTable::load(root).unwrap();
    mounts
        .mounts
        .iter_mut()
        .find(|m| m.alias == alias)
        .unwrap()
        .nid = nid.clone();
    std::fs::write(
        root.join(MOUNTS_FILE),
        toml::to_string_pretty(&mounts).unwrap(),
    )
    .unwrap();
    nid
}

/// Copy a dataset's **authored inputs** only - no store, no segments. A newly installed nest package
/// carries exactly this much.
fn copy_inputs(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "nuthatch.redb" || name == "segments" {
            continue;
        }
        let to = dst.join(&name);
        if entry.file_type().unwrap().is_dir() {
            copy_inputs(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), &to).unwrap();
        }
    }
}

fn data_dirs(root: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(root.join(DATA_DIR))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    v.sort();
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cosmetic_edit_adopts_the_dataset_it_came_from_instead_of_re_indexing() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (alpha_nid, beta_nid) = two_nest_runtime(root);

    // --- Run 1: index both nests over a chain the provider can still serve. ---
    bring_up(root, full_history(), 6, 4).await;
    let before = transfers(&MountTable::data_dir(root, &alpha_nid), "alpha");
    // `analytics::query` reads the **sealed** side, so this is blocks 1..=4 - the segments, which are
    // the expensive half to rebuild and the half a re-index would have to re-fetch.
    assert_eq!(
        before.len(),
        4,
        "the fixture must seal four transfers, or the comparison below is vacuous: {before:?}"
    );

    // --- The edit: a comment in an authored view. Cosmetic by construction - `views/**` is excluded
    // from the data identity because nothing materialises them. ---
    let new_nid = install_edited(root, "alpha", &alpha_nid, |dir| {
        let views = dir.join("views");
        std::fs::create_dir_all(&views).unwrap();
        std::fs::write(
            views.join("10-note.sql"),
            "-- a comment, and nothing else\n",
        )
        .unwrap();
    });

    // The premise, asserted rather than assumed: the new identity resolves onto a directory with the
    // nest's inputs and **no data**. This is the state that costs a full backfill.
    let dest = MountTable::data_dir(root, &new_nid);
    assert!(
        dest.join("nuthatch.toml").is_file(),
        "the edited inputs must be installed"
    );
    assert!(
        !dest.join("nuthatch.redb").exists(),
        "the fixture must start with no store, or it is not reproducing a re-index"
    );

    // --- Run 2: the same chain, with the history no longer available from the provider. ---
    let heights = bring_up(root, pruned_history(), 8, 6).await;
    assert_eq!(
        heights
            .iter()
            .find(|(n, _)| n == "alpha")
            .and_then(|(_, h)| h.clone())
            .as_deref(),
        Some("9"),
        "the adopted store must carry on to the new tip, not restart: {heights:?}"
    );

    // The load-bearing assertion. Every transfer the original nest ever saw is here, under the new
    // identity, on a runtime whose provider served none of them - so adoption is the only route by
    // which they can be present. The four sealed ones came across as segments and the two that were
    // still hot came across in the store, which is why the count is six rather than the four `before`
    // could see: run 2 sealed through 6 and the rest of the adopted history became visible.
    let after = transfers(&dest, "alpha");
    let expected: Vec<u64> = (1..=6).collect();
    assert_eq!(
        after
            .iter()
            .map(|r| r["block_number"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        expected,
        "the edited nest must carry the history it adopted - the provider cannot serve it any more, \
         so a missing row means it re-indexed and lost the chain"
    );
    assert_eq!(
        after[..before.len()],
        before[..],
        "the sealed rows must come across byte for byte, not be re-derived"
    );
    // Adoption **copies**: the source dataset may still be mounted by another tenant.
    assert!(
        MountTable::data_dir(root, &alpha_nid)
            .join("nuthatch.redb")
            .is_file(),
        "adoption must not take data away from the dataset it came from"
    );
    // And it is scoped to the dataset that moved: beta was never touched.
    assert_eq!(
        data_dirs(root).len(),
        3,
        "expected alpha's old and new datasets plus beta: {:?}",
        data_dirs(root)
    );
    assert!(
        transfers(&MountTable::data_dir(root, &beta_nid), "beta").len() >= 6,
        "the co-tenant must be unaffected"
    );
}

/// The dangerous direction. A nest whose inputs genuinely change the data it will store must **not**
/// adopt, however similar the rest of it looks - `migrate::a_substantive_edit_does_not_adopt` holds
/// this line for the migration path, and the mount path needs its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_substantive_edit_does_not_adopt_on_the_mount_path() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (alpha_nid, _) = two_nest_runtime(root);

    bring_up(root, full_history(), 6, 4).await;

    // A different contract address: same shape, different data, whatever else matches.
    let new_nid = install_edited(root, "alpha", &alpha_nid, |dir| {
        let cfg = std::fs::read_to_string(dir.join("nuthatch.toml")).unwrap();
        std::fs::write(dir.join("nuthatch.toml"), cfg.replace(USDC, ARB)).unwrap();
    });

    let dest = MountTable::data_dir(root, &new_nid);
    let mounts = MountTable::load(root).unwrap();
    let datasets = mounts.datasets(root);
    runtime::load_mounted(root, &datasets, mounts.is_multi_tenant()).expect("load");

    assert!(
        !dest.join("nuthatch.redb").exists(),
        "a nest indexing a different contract must not inherit another contract's data"
    );
}
