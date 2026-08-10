//! `nuthatch prune` - reclaim the disk of datasets nothing mounts any more (RFC-0032 §5, slice 4).
//!
//! Unmounting a nest deletes its **mount record**, never its data. That is deliberate and it is the
//! whole point of the design: re-backfilling is precisely the cost identity-keyed storage exists to
//! avoid, so an accidental unmount must not trigger one. Unmount/remount is free.
//!
//! The price is disk held by nothing. This is the command that reclaims it - explicitly, on an
//! operator's schedule, never automatically. A background collector would eventually delete an
//! operator's history on a timer they had forgotten about.
//!
//! **Collectability is derived, not stored.** A dataset directory with no mount record naming its
//! identity is collectable; that is the whole rule. There is no marker file and no "collectable
//! since" timestamp, for the same reason the refcount is a count over the table rather than a
//! number kept beside it: anything stored separately eventually disagrees with the thing it
//! describes, and here the disagreement deletes data.
//!
//! **Listing is the default and deleting needs `--yes`**, which is the opposite of `migrate
//! --dry-run`. The asymmetry is intentional: migrating moves data and is recoverable, pruning
//! removes indexed history and is not. The dangerous direction should be the one you have to ask
//! for.

use crate::runtime::{MountTable, DATA_DIR};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// A dataset on disk that no mount refers to.
#[derive(Debug, PartialEq, Eq)]
pub struct Collectable {
    /// The nest identity the directory is named for.
    pub nid: String,
    pub dir: PathBuf,
    /// Bytes it is holding, so the report says what pruning would actually buy.
    pub bytes: u64,
}

/// Total bytes under `dir`, following no symlinks.
fn size_of(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map(|m| m.len()).unwrap_or(0),
                _ => {} // a symlink is not ours to size, and never ours to follow
            }
        }
    }
    total
}

/// Every dataset under `data/` that no mount record refers to (RFC-0032 §5).
///
/// Reads the mount table first and fails loudly if it cannot: a runtime whose `mounts.toml` is
/// unreadable has *no* known mounts, and treating that as "nothing is mounted" would make every
/// dataset collectable at once. The one failure this command must never have.
pub fn collectable(dir: &Path) -> Result<Vec<Collectable>> {
    let mounts = MountTable::load(dir)
        .with_context(|| format!("reading the mount table of {}", dir.display()))?;
    let mounted: std::collections::HashSet<&str> =
        mounts.mounts.iter().map(|m| m.nid.as_str()).collect();

    let data = dir.join(DATA_DIR);
    let Ok(entries) = std::fs::read_dir(&data) else {
        return Ok(Vec::new()); // nothing migrated yet - nothing to collect
    };

    let mut out = Vec::new();
    for e in entries.flatten() {
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let nid = e.file_name().to_string_lossy().to_string();
        // Only ever consider directories that *look* like datasets. A stray file or a directory an
        // operator dropped in here by hand is not ours to delete.
        if nid.len() != 64 || !nid.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if mounted.contains(nid.as_str()) {
            continue;
        }
        let path = e.path();
        let bytes = size_of(&path);
        out.push(Collectable {
            nid,
            dir: path,
            bytes,
        });
    }
    out.sort_by(|a, b| a.nid.cmp(&b.nid));
    Ok(out)
}

/// Every segment hash any surviving dataset still references (RFC-0033 §11a).
///
/// **Derived from the manifests, never stored.** A stored count drifts, and here drift deletes bytes
/// another dataset is reading - the one data-loss failure mode in the shared store, and the same
/// reasoning as RFC-0032 §5's dataset refcount.
fn referenced_segments(dir: &Path, surviving: &[String]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for nid in surviving {
        let Ok(manifest) = crate::seal::load_manifest(&MountTable::data_dir(dir, nid)) else {
            continue;
        };
        for segs in manifest.tables.values() {
            for s in segs {
                out.insert(s.hash.clone());
            }
        }
    }
    out
}

/// Shared segments no surviving dataset references, with their sizes.
fn orphan_segments(dir: &Path, surviving: &[String]) -> Vec<(PathBuf, u64)> {
    let referenced = referenced_segments(dir, surviving);
    let Ok(entries) = std::fs::read_dir(dir.join(crate::seal::SEGMENTS_DIR)) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
        .filter_map(|e| {
            let hash = e.path().file_stem()?.to_string_lossy().to_string();
            if referenced.contains(&hash) {
                return None;
            }
            Some((e.path(), e.metadata().ok()?.len()))
        })
        .collect()
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// `nuthatch prune <dir> [--yes]`: report collectable datasets, and with `--yes` remove them.
pub fn run(dir: &Path, yes: bool) -> Result<()> {
    let found = collectable(dir)?;
    if found.is_empty() {
        println!(
            "Nothing to prune in {} - every dataset is mounted.",
            dir.display()
        );
        return Ok(());
    }

    let total: u64 = found.iter().map(|c| c.bytes).sum();
    println!(
        "{} dataset(s) in {} are mounted by nothing, holding {}:\n",
        found.len(),
        dir.display(),
        human(total)
    );
    for c in &found {
        println!("  data/{}  {}", &c.nid[..12], human(c.bytes));
    }
    println!();

    if !yes {
        println!(
            "Nothing was deleted. This is indexed history: re-creating it means a full backfill, \
             which is the cost identity-keyed storage exists to avoid.\n\
             Re-mounting any of these is free while the data is still here.\n\n\
             Run `nuthatch prune --dir {} --yes` to remove them.",
            dir.display()
        );
        return Ok(());
    }

    // **Manifest first, bytes second** (RFC-0033 §11a). Removing the dataset - and with it the
    // manifest referencing its segments - before collecting orphans means an interrupted prune leaves
    // unreferenced bytes (recoverable, just disk) rather than dangling references (not).
    for c in &found {
        std::fs::remove_dir_all(&c.dir).with_context(|| format!("removing {}", c.dir.display()))?;
        // An adoption killed part-way leaves a staging sibling. It is not a dataset, so `collectable`
        // never lists it, and it would otherwise outlive the only thing that clears it.
        let _ = std::fs::remove_dir_all(crate::runtime::adopt_staging(&c.dir));
        println!("Removed data/{}", &c.nid[..12]);
    }

    let surviving: Vec<String> = MountTable::load(dir)
        .map(|r| r.mounts.iter().map(|m| m.nid.clone()).collect())
        .unwrap_or_default();
    let orphans = orphan_segments(dir, &surviving);
    let mut freed_segments = 0u64;
    for (path, size) in &orphans {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        freed_segments += size;
    }
    if !orphans.is_empty() {
        println!(
            "Removed {} shared segment(s) no dataset referenced ({}).",
            orphans.len(),
            human(freed_segments)
        );
    }

    println!("\nFreed {}.", human(total + freed_segments));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Mount, MOUNTS_FILE};

    /// A mounts with `mounted` recorded and `orphans` sitting in `data/` with no record.
    fn fixture(dir: &Path, mounted: &[&str], orphans: &[&str]) {
        let mut mounts =
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n"
                .to_string();
        for (i, nid) in mounted.iter().enumerate() {
            mounts.push_str(&format!(
                "\n[[mounts]]\nalias = \"n{i}\"\nnid = \"{nid}\"\n"
            ));
        }
        std::fs::write(dir.join(MOUNTS_FILE), mounts).unwrap();
        for nid in mounted.iter().chain(orphans) {
            let d = dir.join(DATA_DIR).join(nid);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("nuthatch.redb"), vec![0u8; 1024]).unwrap();
        }
    }

    fn nid(seed: &str) -> String {
        seed.repeat(64 / seed.len())
    }

    #[test]
    fn only_unmounted_datasets_are_collectable() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (nid("aa"), nid("bb"));
        fixture(d.path(), &[&a], &[&b]);

        let found = collectable(d.path()).unwrap();
        assert_eq!(found.len(), 1, "expected only the orphan, got {found:?}");
        assert_eq!(found[0].nid, b);
        assert_eq!(found[0].bytes, 1024);
    }

    /// Two mounts sharing one dataset, one of them unmounted: the refcount is still 1, so the data
    /// must NOT be collectable. Getting this wrong deletes a tenant's data because a *different*
    /// tenant unmounted.
    #[test]
    fn a_dataset_another_mount_still_wants_is_not_collectable() {
        let d = tempfile::tempdir().unwrap();
        let shared = nid("cc");
        std::fs::write(
            d.path().join(MOUNTS_FILE),
            format!(
                "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\nrpc_urls = []\n\n\
                 [[mounts]]\ntenant = \"globex\"\nalias = \"usdc\"\nnid = \"{shared}\"\n"
            ),
        )
        .unwrap();
        let dir = d.path().join(DATA_DIR).join(&shared);
        std::fs::create_dir_all(&dir).unwrap();

        assert!(
            collectable(d.path()).unwrap().is_empty(),
            "a dataset one tenant still mounts was offered up for deletion"
        );
    }

    /// Listing must never delete. This is the guard between an operator typing `prune` to see what
    /// is there and losing indexed history.
    #[test]
    fn listing_deletes_nothing_and_yes_deletes_everything_collectable() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (nid("aa"), nid("bb"));
        fixture(d.path(), &[&a], &[&b]);
        let orphan = d.path().join(DATA_DIR).join(&b);

        run(d.path(), false).unwrap();
        assert!(orphan.is_dir(), "listing deleted a dataset");

        run(d.path(), true).unwrap();
        assert!(
            !orphan.exists(),
            "--yes did not remove the collectable dataset"
        );
        assert!(
            d.path().join(DATA_DIR).join(&a).is_dir(),
            "prune removed a MOUNTED dataset"
        );
    }

    /// An adoption killed part-way leaves `data/<nid>.adopting/` - a full copy of a dataset's derived
    /// state. `adopt_dataset` clears it on the dataset's next start, but a dataset that is unmounted
    /// and pruned never has one, so the scratch would sit there at dataset size forever.
    #[test]
    fn a_pruned_dataset_takes_its_adoption_staging_with_it() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (nid("aa"), nid("bb"));
        fixture(d.path(), &[&a], &[&b]);
        let staging = crate::runtime::adopt_staging(&d.path().join(DATA_DIR).join(&b));
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("nuthatch.redb"), vec![0u8; 1024]).unwrap();
        let kept = crate::runtime::adopt_staging(&d.path().join(DATA_DIR).join(&a));
        std::fs::create_dir_all(&kept).unwrap();

        assert!(
            collectable(d.path())
                .unwrap()
                .iter()
                .all(|c| c.nid.len() == 64 && c.nid != format!("{b}.adopting")),
            "staging must never be listed as a dataset"
        );
        run(d.path(), true).unwrap();
        assert!(
            !staging.exists(),
            "the pruned dataset's staging outlived it, and nothing else ever clears it"
        );
        assert!(
            kept.is_dir(),
            "prune removed the staging of a dataset it did not collect"
        );
    }

    /// A directory that is not a dataset is not ours to delete, however tempting the tidy-up.
    #[test]
    fn foreign_directories_are_left_alone() {
        let d = tempfile::tempdir().unwrap();
        let a = nid("aa");
        fixture(d.path(), &[&a], &[]);
        for stray in ["notes", "aa", &"ff".repeat(20)] {
            std::fs::create_dir_all(d.path().join(DATA_DIR).join(stray)).unwrap();
        }

        assert!(collectable(d.path()).unwrap().is_empty());
        run(d.path(), true).unwrap();
        for stray in ["notes", "aa", &"ff".repeat(20)] {
            assert!(
                d.path().join(DATA_DIR).join(stray).is_dir(),
                "prune deleted {stray}, which is not a dataset"
            );
        }
    }

    /// An unreadable mount table means "we do not know what is mounted", never "nothing is". The
    /// difference is every dataset in the runtime.
    #[test]
    fn an_unreadable_mount_table_refuses_rather_than_collecting_everything() {
        let d = tempfile::tempdir().unwrap();
        let a = nid("aa");
        fixture(d.path(), &[&a], &[]);
        std::fs::write(d.path().join(MOUNTS_FILE), "this is not toml {{{").unwrap();

        assert!(
            collectable(d.path()).is_err(),
            "an unparseable mounts.toml must refuse, not offer every dataset for deletion"
        );
        assert!(run(d.path(), true).is_err());
        assert!(
            d.path().join(DATA_DIR).join(&a).is_dir(),
            "data was deleted"
        );
    }

    /// RFC-0033 §11a slice B, **the test that matters**: pruning one of two datasets that share
    /// segments must leave the other's data readable.
    ///
    /// This is the one data-loss failure mode in the shared store. A `prune` that deleted a dataset
    /// directory wholesale - which is what it did before slice A - would take bytes another dataset
    /// is still reading.
    #[test]
    fn pruning_one_dataset_does_not_delete_a_segment_another_still_references() {
        use crate::seal::{Segment, SEGMENTS_DIR};
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let (keep, drop_) = (nid("aa"), nid("bb"));

        // Two datasets. Both reference `shared`; only the pruned one references `lonely`.
        let shared_hash = "11".repeat(32);
        let lonely_hash = "22".repeat(32);
        std::fs::create_dir_all(root.join(SEGMENTS_DIR)).unwrap();
        for h in [&shared_hash, &lonely_hash] {
            std::fs::write(
                root.join(SEGMENTS_DIR).join(format!("{h}.parquet")),
                b"bytes",
            )
            .unwrap();
        }
        let seg = |h: &str| Segment {
            hash: h.to_string(),
            from_block: 1,
            to_block: 2,
            rows: 1,
            file: format!("t-{h}.parquet"),
            registry_snapshot: None,
        };
        for (nid_, hashes) in [
            (&keep, vec![shared_hash.clone()]),
            (&drop_, vec![shared_hash.clone(), lonely_hash.clone()]),
        ] {
            let dir = MountTable::data_dir(root, nid_);
            std::fs::create_dir_all(dir.join(SEGMENTS_DIR)).unwrap();
            let mut m = crate::seal::Manifest::default();
            m.tables
                .insert("t".to_string(), hashes.iter().map(|h| seg(h)).collect());
            std::fs::write(
                dir.join(SEGMENTS_DIR).join("manifest.json"),
                serde_json::to_string(&m).unwrap(),
            )
            .unwrap();
        }

        // Only `keep` is mounted, so `drop_` is collectable.
        std::fs::write(
            root.join(MOUNTS_FILE),
            format!(
                "[runtime]\nname = \"r\"\n\n[[chains]]\nchain = \"arbitrum-one\"\n\
                 chain_id = 42161\nrpc_urls = []\n\n[[mounts]]\nalias = \"a\"\nnid = \"{keep}\"\n"
            ),
        )
        .unwrap();

        run(root, true).unwrap();

        let shared = root
            .join(SEGMENTS_DIR)
            .join(format!("{shared_hash}.parquet"));
        let lonely = root
            .join(SEGMENTS_DIR)
            .join(format!("{lonely_hash}.parquet"));
        assert!(
            shared.exists(),
            "prune deleted a segment the surviving dataset still references - this is the data-loss \
             failure mode the shared store was designed around"
        );
        assert!(
            !lonely.exists(),
            "a segment nothing references should have been collected"
        );
        assert!(
            MountTable::data_dir(root, &keep).is_dir(),
            "the mounted dataset survives"
        );
        assert!(!MountTable::data_dir(root, &drop_).exists());
    }

    /// Mount, unmount, prune, and the mount record's identity is all it ever took to find the data.
    #[test]
    fn unmounting_makes_a_dataset_collectable_and_remounting_makes_it_safe_again() {
        let d = tempfile::tempdir().unwrap();
        let a = nid("aa");
        fixture(d.path(), &[&a], &[]);
        assert!(collectable(d.path()).unwrap().is_empty());

        // Unmount: the record goes, the data stays.
        std::fs::write(
            d.path().join(MOUNTS_FILE),
            "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
             rpc_urls = []\nnests = [\"placeholder\"]\n",
        )
        .unwrap();
        assert_eq!(collectable(d.path()).unwrap().len(), 1);
        assert!(
            d.path().join(DATA_DIR).join(&a).is_dir(),
            "unmount deleted data - collection must be deferred"
        );

        // Remount the same identity: collectable again becomes empty, with no backfill involved.
        let mut mounts = MountTable::load(d.path()).unwrap();
        mounts.runtime.nests.clear();
        mounts.mounts.push(Mount {
            tenant: "default".into(),
            alias: "back".into(),
            nid: a.clone(),
            sql: Default::default(),
            queries: Vec::new(),
        });
        std::fs::write(
            d.path().join(MOUNTS_FILE),
            toml::to_string_pretty(&mounts).unwrap(),
        )
        .unwrap();
        assert!(collectable(d.path()).unwrap().is_empty());
    }
}
