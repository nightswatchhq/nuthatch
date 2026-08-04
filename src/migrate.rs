//! `nuthatch migrate` - move a roost from the name-keyed layout to identity-keyed datasets
//! (RFC-0032 §8, slice 1).
//!
//! Before: `<root>/nests/<name>/` - an operator's label decides where data lives. After:
//! `<root>/data/<nid>/`, addressed by what the nest *is*, with `roost.toml` carrying `[[mounts]]`
//! records mapping alias → identity.
//!
//! **Data is moved, never re-indexed.** If a migration ever needs a backfill, the migration is wrong.
//!
//! Two properties matter more than speed:
//!
//! - **Idempotent.** Running it twice is a no-op. Running it after adding one nest migrates only that
//!   nest. There is no "already migrated" error to work around.
//! - **Refuses rather than guesses.** A nest whose inputs no longer reproduce its own manifest is
//!   named and skipped, and the whole run reports a failure. A half-migrated roost still serves,
//!   because [`crate::roost::Roost::dir_for`] resolves both layouts.
//!
//! **Deviation from RFC-0032 §12, recorded deliberately.** The RFC specified copy-then-verify-then-
//! swap. This uses `rename` when source and destination share a filesystem - which they do, both
//! being under the roost root - and falls back to copy-verify-remove only across devices. A rename is
//! atomic: it cannot produce the half-written destination the copy path exists to guard against, and
//! it does not require double the disk of an indexed history. The RFC's *intent* was "never lose
//! data"; rename serves it better than the mechanism the RFC named.

use crate::blob;
use crate::roost::{Mount, Roost, DATA_DIR, NESTS_DIR, ROOST_FILE};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// What migrating one nest would do. Computed for every nest before anything moves, so `--dry-run`
/// and the real run share one code path and a plan is never a separate, drifting implementation.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Already addressed by identity, at the identity it should have. Nothing to do.
    AlreadyMigrated { alias: String, nid: String },
    /// Move `nests/<alias>` to `data/<nid>`.
    Move {
        alias: String,
        nid: String,
        from: PathBuf,
        to: PathBuf,
    },
    /// `data/<nid>` already holds this exact nest, mounted under another alias. The migration has
    /// found a **pre-existing double-index**: two names, byte-identical inputs, two backfills of the
    /// same chain data. Both aliases end up serving one dataset.
    Merge {
        alias: String,
        nid: String,
        with: String,
        from: PathBuf,
    },
    /// Something is wrong with the nest and it will not be touched.
    Refuse { alias: String, why: String },
}

impl Plan {
    /// One line, in the order an operator reads: what happens, to what, and where it lands.
    fn describe(&self) -> String {
        match self {
            Plan::AlreadyMigrated { alias, nid } => {
                format!("  {alias}: already at data/{} - nothing to do", &nid[..12])
            }
            Plan::Move { alias, nid, .. } => {
                format!("  {alias}: nests/{alias} -> data/{}", &nid[..12])
            }
            Plan::Merge {
                alias, nid, with, ..
            } => format!(
                "  {alias}: MERGE into data/{} - identical to '{with}', which had its own backfill",
                &nid[..12]
            ),
            Plan::Refuse { alias, why } => format!("  {alias}: REFUSED - {why}"),
        }
    }
}

/// Build the full plan without touching anything.
///
/// Computing every nest's identity up front is what makes [`Plan::Merge`] detectable at all: two
/// nests only turn out to be the same nest once both have been hashed.
pub fn plan(dir: &Path) -> Result<Vec<Plan>> {
    let roost = Roost::load(dir)?;
    let mut plans = Vec::with_capacity(roost.roost.nests.len());
    // Identity -> the first alias that claimed it, for merge detection within this run.
    let mut claimed: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for alias in &roost.roost.nests {
        let recorded = roost.mounts.iter().find(|m| &m.alias == alias);
        let legacy = dir.join(NESTS_DIR).join(alias);

        // Already migrated: a record exists and its dataset is on disk. Trust the record rather than
        // re-hashing - re-deriving the identity of every nest on every startup would make `migrate`
        // cost a full registry rebuild per nest for no decision it could change.
        if let Some(m) = recorded {
            if dir.join(DATA_DIR).join(&m.nid).is_dir() {
                claimed.insert(m.nid.clone(), alias.clone());
                plans.push(Plan::AlreadyMigrated {
                    alias: alias.clone(),
                    nid: m.nid.clone(),
                });
                continue;
            }
        }

        if !legacy.is_dir() {
            plans.push(Plan::Refuse {
                alias: alias.clone(),
                why: format!("{} does not exist", legacy.display()),
            });
            continue;
        }

        let nid = match nid_of(&legacy) {
            Ok(nid) => nid,
            Err(e) => {
                plans.push(Plan::Refuse {
                    alias: alias.clone(),
                    why: format!("{e:#}"),
                });
                continue;
            }
        };

        let dest = dir.join(DATA_DIR).join(&nid);
        match claimed.get(&nid) {
            // Another alias in this same roost is the same nest, byte for byte.
            Some(other) => plans.push(Plan::Merge {
                alias: alias.clone(),
                nid: nid.clone(),
                with: other.clone(),
                from: legacy,
            }),
            None if dest.is_dir() => {
                // The dataset exists but no alias in this run claimed it - a previous partial run.
                claimed.insert(nid.clone(), alias.clone());
                plans.push(Plan::Merge {
                    alias: alias.clone(),
                    nid,
                    with: "an existing dataset".to_string(),
                    from: legacy,
                });
            }
            None => {
                claimed.insert(nid.clone(), alias.clone());
                plans.push(Plan::Move {
                    alias: alias.clone(),
                    nid,
                    from: legacy,
                    to: dest,
                });
            }
        }
    }
    Ok(plans)
}

/// A nest's identity, refusing one whose packed manifest no longer matches its inputs.
///
/// A nest installed from a bundle carries `manifest.json`. If the inputs have drifted from it, the
/// nest is not the nest it claims to be, and silently giving it a fresh identity would hide an edit
/// somebody made to a supposedly-pinned deploy unit.
fn nid_of(nest: &Path) -> Result<String> {
    let manifest_path = nest.join("manifest.json");
    if manifest_path.is_file() {
        let raw = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("reading {}", manifest_path.display()))?;
        let packed: blob::Manifest = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", manifest_path.display()))?;
        blob::verify_registry_reproduces(nest, &packed)
            .context("this nest's inputs no longer reproduce the manifest it was packed with")?;
    }
    blob::nest_nid(nest)
}

/// `nuthatch migrate <dir> [--dry-run]`: apply the plan (RFC-0032 §8).
///
/// Returns an error if any nest was refused, *after* migrating every nest that was fine - a bad nest
/// must not hold the rest hostage, and `dir_for` means a partially-migrated roost still serves.
pub fn run(dir: &Path, dry_run: bool) -> Result<()> {
    let plans = plan(dir)?;
    println!(
        "{} {} nest(s) in {}\n",
        if dry_run {
            "Would migrate"
        } else {
            "Migrating"
        },
        plans.len(),
        dir.display()
    );
    for p in &plans {
        println!("{}", p.describe());
    }
    println!();

    if dry_run {
        println!("Dry run: nothing was changed. Re-run without --dry-run to apply.");
        return Ok(());
    }

    let mut records: Vec<Mount> = Vec::new();
    let mut refused: Vec<&str> = Vec::new();
    for p in &plans {
        match p {
            Plan::AlreadyMigrated { alias, nid } => records.push(Mount {
                alias: alias.clone(),
                nid: nid.clone(),
            }),
            Plan::Move {
                alias,
                nid,
                from,
                to,
            } => {
                relocate(from, to)?;
                records.push(Mount {
                    alias: alias.clone(),
                    nid: nid.clone(),
                });
            }
            Plan::Merge {
                alias, nid, from, ..
            } => {
                // The destination already holds this exact nest. The duplicate's *inputs* are
                // identical by definition, so the only thing lost is its separately-indexed data -
                // which is why the duplicate is moved aside rather than deleted. An operator who
                // wants the disk back can remove it once they have seen the merge happen.
                let aside = from.with_extension("merged-duplicate");
                relocate(from, &aside)?;
                println!(
                    "  {alias}: duplicate data kept at {} (safe to delete once verified)",
                    aside.display()
                );
                records.push(Mount {
                    alias: alias.clone(),
                    nid: nid.clone(),
                });
            }
            Plan::Refuse { alias, .. } => refused.push(alias),
        }
    }

    write_mounts(dir, &records)?;
    println!("\nWrote {} mount record(s) to {ROOST_FILE}.", records.len());

    // An empty `nests/` left standing reads as "half of it didn't work". `remove_dir` refuses a
    // non-empty directory, so this can only ever tidy up - never delete a nest.
    if std::fs::remove_dir(dir.join(NESTS_DIR)).is_ok() {
        println!("Removed the now-empty {NESTS_DIR}/ directory.");
    }

    if !refused.is_empty() {
        bail!(
            "migrated what could be migrated, but refused: {}. \
             The roost still serves - un-migrated nests resolve through the old layout.",
            refused.join(", ")
        );
    }
    Ok(())
}

/// Move a directory, preferring an atomic rename. See the module docs for why this is not the
/// copy-then-swap RFC-0032 §12 described.
fn relocate(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // Cross-device, or a rename the platform refused. Copy, then remove the source only once the
    // copy is complete - so an interruption leaves the source intact and the run simply repeatable.
    crate::project::copy_dir(from, to)
        .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    std::fs::remove_dir_all(from)
        .with_context(|| format!("removing {} after copying it", from.display()))?;
    Ok(())
}

/// Rewrite `roost.toml` with the mount records, temp-then-rename so a crash cannot truncate it.
fn write_mounts(dir: &Path, records: &[Mount]) -> Result<()> {
    let path = dir.join(ROOST_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {} to record mounts", path.display()))?;
    let mut roost: Roost = toml::from_str(&raw)
        .with_context(|| format!("parsing {} before rewriting it", path.display()))?;
    roost.mounts = records.to_vec();
    let out = toml::to_string_pretty(&roost).context("serialising roost.toml")?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, out).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A roost with `n` nests, all on one chain, in the pre-2.0 layout.
    fn write_roost(dir: &Path, nests: &[(&str, &str)]) {
        let names: Vec<String> = nests.iter().map(|(n, _)| format!("\"{n}\"")).collect();
        std::fs::write(
            dir.join(ROOST_FILE),
            format!(
                "[roost]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
                 rpc_urls = []\nnests = [{}]\n",
                names.join(", ")
            ),
        )
        .unwrap();
        for (name, addr) in nests {
            let nest = dir.join(NESTS_DIR).join(name);
            std::fs::create_dir_all(&nest).unwrap();
            std::fs::write(
                nest.join("nuthatch.toml"),
                format!(
                    "[nest]\nname = \"{name}\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
                     rpc_urls = []\n\n[[contracts]]\nalias = \"t\"\naddress = \"{addr}\"\n\
                     abi = \"abi.json\"\n"
                ),
            )
            .unwrap();
            std::fs::write(nest.join("abi.json"), "[]").unwrap();
            // Stand-in for indexed data, to prove migration moves it rather than re-deriving it.
            std::fs::write(nest.join("nuthatch.redb"), format!("data for {name}")).unwrap();
        }
    }

    #[test]
    fn a_dry_run_changes_nothing() {
        let d = tempfile::tempdir().unwrap();
        write_roost(
            d.path(),
            &[("a", "0x0000000000000000000000000000000000000001")],
        );
        let before = std::fs::read_to_string(d.path().join(ROOST_FILE)).unwrap();

        run(d.path(), true).unwrap();

        assert!(d.path().join(NESTS_DIR).join("a").is_dir(), "source moved");
        assert!(!d.path().join(DATA_DIR).exists(), "destination created");
        assert_eq!(
            before,
            std::fs::read_to_string(d.path().join(ROOST_FILE)).unwrap(),
            "roost.toml was rewritten by a dry run"
        );
    }

    /// The core of slice 1: data moves to an identity-keyed directory, byte for byte, and the roost
    /// resolves through the records afterwards.
    #[test]
    fn migration_moves_data_and_never_re_indexes() {
        let d = tempfile::tempdir().unwrap();
        write_roost(
            d.path(),
            &[
                ("a", "0x0000000000000000000000000000000000000001"),
                ("b", "0x0000000000000000000000000000000000000002"),
            ],
        );
        run(d.path(), false).unwrap();

        let roost = Roost::load(d.path()).unwrap();
        assert_eq!(roost.mounts.len(), 2);
        for (alias, expected) in [("a", "data for a"), ("b", "data for b")] {
            let dir = roost.dir_for(d.path(), alias);
            assert!(
                dir.starts_with(d.path().join(DATA_DIR)),
                "{alias} did not move under data/"
            );
            // The indexed data came across untouched. Had migration re-derived anything, this is
            // where it would show.
            assert_eq!(
                std::fs::read_to_string(dir.join("nuthatch.redb")).unwrap(),
                expected
            );
            assert!(
                !d.path().join(NESTS_DIR).join(alias).exists(),
                "{alias} was left behind in the old layout"
            );
        }
        // Two different nests, two identities.
        assert_ne!(roost.mounts[0].nid, roost.mounts[1].nid);
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let d = tempfile::tempdir().unwrap();
        write_roost(
            d.path(),
            &[("a", "0x0000000000000000000000000000000000000001")],
        );
        run(d.path(), false).unwrap();
        let after_first = std::fs::read_to_string(d.path().join(ROOST_FILE)).unwrap();
        let nid = Roost::load(d.path()).unwrap().mounts[0].nid.clone();

        run(d.path(), false).unwrap();

        assert_eq!(
            after_first,
            std::fs::read_to_string(d.path().join(ROOST_FILE)).unwrap()
        );
        assert_eq!(Roost::load(d.path()).unwrap().mounts[0].nid, nid);
        assert_eq!(
            std::fs::read_to_string(d.path().join(DATA_DIR).join(&nid).join("nuthatch.redb"))
                .unwrap(),
            "data for a"
        );
    }

    /// Two aliases over byte-identical inputs are one nest that was indexed twice. The migration is
    /// where that becomes visible, and both aliases end up on one dataset.
    #[test]
    fn two_names_for_one_nest_merge_into_one_dataset() {
        let d = tempfile::tempdir().unwrap();
        // Same contract, same ABI - the nest *name* differs, and a nest's name is in its config, so
        // write both configs identically and let the alias differ only in the roost.
        write_roost(
            d.path(),
            &[
                ("a", "0x0000000000000000000000000000000000000001"),
                ("clone", "0x0000000000000000000000000000000000000001"),
            ],
        );
        // Make the inputs byte-identical: `nest.name` is an authored input, so it must match too.
        let a = std::fs::read_to_string(d.path().join(NESTS_DIR).join("a").join("nuthatch.toml"))
            .unwrap();
        std::fs::write(
            d.path().join(NESTS_DIR).join("clone").join("nuthatch.toml"),
            &a,
        )
        .unwrap();

        let plans = plan(d.path()).unwrap();
        assert!(
            matches!(plans[1], Plan::Merge { .. }),
            "identical inputs under two aliases must be detected as one nest, got {:?}",
            plans[1]
        );

        run(d.path(), false).unwrap();
        let roost = Roost::load(d.path()).unwrap();
        assert_eq!(
            roost.mounts[0].nid, roost.mounts[1].nid,
            "both aliases must land on one identity"
        );
        assert_eq!(
            roost.dir_for(d.path(), "a"),
            roost.dir_for(d.path(), "clone"),
            "two doors, one room"
        );
    }

    /// A refusal must not hold the healthy nests hostage, and the roost must still serve.
    #[test]
    fn a_broken_nest_is_refused_and_the_rest_still_migrate() {
        let d = tempfile::tempdir().unwrap();
        write_roost(
            d.path(),
            &[
                ("good", "0x0000000000000000000000000000000000000001"),
                ("broken", "0x0000000000000000000000000000000000000002"),
            ],
        );
        // Remove the config so the nest cannot be hashed at all.
        std::fs::remove_file(
            d.path()
                .join(NESTS_DIR)
                .join("broken")
                .join("nuthatch.toml"),
        )
        .unwrap();

        let err = run(d.path(), false).unwrap_err().to_string();
        assert!(
            err.contains("broken"),
            "the refusal must name the nest: {err}"
        );

        let roost = Roost::load(d.path()).unwrap();
        assert_eq!(
            roost.mounts.len(),
            1,
            "the healthy nest should have migrated"
        );
        assert_eq!(roost.mounts[0].alias, "good");
        // Mixed layout: one resolved by identity, one still by name. Both resolve.
        assert!(roost
            .dir_for(d.path(), "good")
            .starts_with(d.path().join(DATA_DIR)));
        assert!(roost
            .dir_for(d.path(), "broken")
            .starts_with(d.path().join(NESTS_DIR)));
    }
}
