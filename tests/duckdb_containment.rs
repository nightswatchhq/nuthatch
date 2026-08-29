//! RFC-0042 slice 1 (#936): the engine must not spread while we are deciding whether to remove it.
//!
//! §6 asks for an analytical boundary across which "DuckDB-specific connection, value or AST types do
//! not escape". Measuring first, as slice 0 did, found that boundary mostly already exists:
//! `analytics.rs` holds 53 connection operations and its public functions take `&Path` and `&str` and
//! return `serde_json::Value`. Nothing of DuckDB's crosses it.
//!
//! It leaks in exactly two modules, and this file freezes that.
//!
//! **A shrink-only list, deliberately.** A hand-kept allowlist is the `CONFIG_SOURCES` failure mode -
//! it needs editing on every legitimate change until someone relaxes it into meaninglessness. This one
//! is different in the direction that matters: **removing a site is the goal**, so the list may only
//! get shorter. Adding to it requires a deliberate edit and a reviewer asking why the engine is
//! spreading during the sprint that exists to decide whether to remove it.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The six sites slice 0 inventoried, with their roles. See `docs/rfcs/0042-slice0-bom.md`.
const KNOWN: &[&str] = &[
    "analytics.rs",             // general SQL, views, hot+cold federation
    "entities.rs",              // the admissible function vocabulary, from duckdb_functions()
    "entity_lower.rs",          // AST for lowering authored SQL to a circuit
    "graft.rs",                 // canonical plan, engine version, determinism gate
    "seal.rs",                  // segment-binding oracle (test-only)
    "authored_entity_spike.rs", // RFC-0041 spike, reachable via `nuthatch bench`
];

fn src_files() -> Vec<(String, String)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().to_string(),
                std::fs::read_to_string(&p).unwrap_or_default(),
            )
        })
        .collect();
    out.sort();
    assert!(
        out.len() > 20,
        "found only {} source files - the scan has stopped matching reality (#913 shape 1)",
        out.len()
    );
    out
}

/// Which files reach DuckDB at all. Comments stripped first: this file's own prose names every site,
/// and a gate that matches its own documentation passes with the guarded thing gone.
fn duckdb_sites() -> BTreeSet<String> {
    src_files()
        .into_iter()
        .filter(|(_, body)| {
            body.lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .any(|l| l.contains("duckdb::") || l.contains("use duckdb"))
        })
        .map(|(name, _)| name)
        .collect()
}

#[test]
fn the_engine_does_not_spread_beyond_the_known_sites() {
    let found = duckdb_sites();
    assert!(
        !found.is_empty(),
        "no DuckDB sites found at all - either the scan broke or the engine is gone, and only one of \
         those is plausible today"
    );
    let known: BTreeSet<String> = KNOWN.iter().map(|s| s.to_string()).collect();
    let new: Vec<&String> = found.difference(&known).collect();
    assert!(
        new.is_empty(),
        "these modules reach DuckDB and are not in the slice-0 inventory:\n  {new:?}\n\n\
         RFC-0042 is deciding whether the engine can be removed; every new site is one more entry on \
         the deletion checklist, and one nobody costed. If this is deliberate, add it to `KNOWN` with \
         its role and say why in the PR (#936)."
    );
    // The list may shrink - that is the point of the RFC - but a vanished site should be noticed
    // rather than silently tolerated, because it means the inventory in slice 0 is now wrong.
    let gone: Vec<&String> = known.difference(&found).collect();
    assert!(
        gone.is_empty(),
        "these are in the inventory but no longer reach DuckDB: {gone:?}. Good news, and \
         `docs/rfcs/0042-slice0-bom.md` now overstates the role count - update both together."
    );
}

/// §6's actual requirement. `analytics.rs` already satisfies it; `graft.rs` does not, and this records
/// the gap with a number so slice 2 cannot quietly inherit it as normal.
#[test]
fn the_analytical_surface_keeps_duckdb_types_internal() {
    let files = src_files();
    for name in ["analytics.rs", "graft.rs"] {
        let body = &files
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("{name} not found"))
            .1;
        let leaks: Vec<&str> = body
            .lines()
            .filter(|l| {
                l.trim_start().starts_with("pub fn") || l.trim_start().starts_with("pub struct")
            })
            .filter(|l| {
                l.contains("Connection") || l.contains("DuckValue") || l.contains("ValueRef")
            })
            .collect();
        assert!(
            leaks.is_empty(),
            "{name} exposes a DuckDB type in a public signature. Both modules must keep the engine \
             internal - `analytics.rs` because it is the analytical surface RFC-0042 §6 wants a \
             boundary at, `graft.rs` because it writes the engine string into grafting identity and \
             a caller holding a `Connection` makes the engine part of that contract (#944):\n{leaks:#?}"
        );
    }
}
