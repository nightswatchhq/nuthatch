//! #819: README (and the other operator-facing surfaces) claimed general incremental views.
//! Only three built-in DBSP relations ship. A phrase returning here is a published claim that is false.

use std::fs;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Surfaces a stranger actually reads. RFCs, the progress log, and grant copy are exempt:
/// they describe a destination or a date, not the binary on the door.
const SURFACES: &[&str] = &[
    "README.md",
    "CLAUDE.md",
    "docs/operators.md",
    "docs/launch/show-hn.md",
    "docs/launch/home-turf.md",
    "docs/launch/community.md",
    "docs/launch/port-queue-nest.md",
    "skills/nuthatch-builder/SKILL.md",
    "skills/nuthatch-builder/views.md",
];

/// Broad wording that implies arbitrary authored views are already IVM.
const BROAD: &[&str] = &[
    "Incremental views maintained by DBSP",
    "entities as incremental views over decoded events",
    "entity views are **incremental**",
    "Incremental views (DBSP) mean",
    "have no incremental-view engine. Nuthatch does",
    "incremental-view blockchain indexing",
];

fn walk_md(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_md(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
}

#[test]
fn operator_facing_copy_does_not_claim_general_ivm() {
    let root = root();
    let mut offenders = Vec::new();
    for rel in SURFACES {
        let text = fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"));
        for phrase in BROAD {
            if text.contains(phrase) {
                offenders.push(format!("{rel}: still contains `{phrase}`"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "operator-facing copy still claims general IVM (#819):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn readme_names_the_three_circuits_and_query_time_views() {
    let text = fs::read_to_string(root().join("README.md")).expect("README.md");
    for needle in [
        "balances",
        "exposure",
        "velocity",
        "views/*.sql",
        "RFC-0041",
    ] {
        assert!(
            text.contains(needle),
            "README.md must name `{needle}` so the two speeds stay distinct"
        );
    }
    assert!(
        text.contains("query time") || text.contains("at query time"),
        "README.md must say authored views run at query time"
    );
}

#[test]
fn claude_md_parks_authored_incremental_entities_on_rfc_0041() {
    let text = fs::read_to_string(root().join("CLAUDE.md")).expect("CLAUDE.md");
    // Asserts the property, not the vocabulary. The 2026-08-24 carve-out replaced "frozen" with an
    // approved-but-unshipped status, which is a different sentence and the same guarantee: the
    // standing brief must never read as though authored incremental entities already work.
    assert!(
        text.contains("RFC-0041") && text.contains("do not describe them as shipped"),
        "CLAUDE.md must name RFC-0041 and say it is not shipped, whatever its approval status"
    );
    assert!(
        text.contains("views/*.sql") && text.contains("Not incremental"),
        "CLAUDE.md must say authored views are not incremental"
    );
}

/// A second pass over every markdown file under skills/ and docs/launch/, so a new page
/// cannot reintroduce the claim without being added to SURFACES first.
#[test]
fn launch_and_skill_trees_have_no_unlisted_broad_claim() {
    let root = root();
    let mut files = Vec::new();
    walk_md(&root.join("docs/launch"), &mut files);
    walk_md(&root.join("skills"), &mut files);
    let listed: Vec<PathBuf> = SURFACES.iter().map(|s| root.join(s)).collect();
    let mut offenders = Vec::new();
    for path in files {
        if listed.iter().any(|p| p == &path) {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        for phrase in BROAD {
            if text.contains(phrase) {
                offenders.push(format!(
                    "{}: contains `{phrase}` but is not in SURFACES",
                    path.strip_prefix(&root).unwrap().display()
                ));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a launch/skill page claims general IVM and is not gated:\n{}",
        offenders.join("\n")
    );
}
