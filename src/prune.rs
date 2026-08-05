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

    for c in &found {
        std::fs::remove_dir_all(&c.dir).with_context(|| format!("removing {}", c.dir.display()))?;
        println!("Removed data/{}", &c.nid[..12]);
    }
    println!("\nFreed {}.", human(total));
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
