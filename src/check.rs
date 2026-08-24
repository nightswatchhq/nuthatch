//! `nuthatch check` - run a nest's invariant/parity checks (RFC-0002 §5).
//!
//! Each `checks/<name>.sql` is a read-only query run over the nest's sealed data (the same DuckDB
//! surface as `/sql`, so it sees the per-event tables *and* the nest's derived views). Its result is
//! compared to a recorded expected fixture `checks/expected/<name>.json`. For the Horizon nest those
//! fixtures are the deployed subgraph's answers at a pinned block, so this is a parity check; the
//! framework itself is generic (any nest can ship invariant checks).
//!
//! Hermetic by design - it compares against committed fixtures, not a live endpoint, so it runs in
//! CI with no network. `--update` re-records the fixtures from current results (authoring, run once
//! against known-good sealed data). Refreshing fixtures from a live subgraph is a nest-side chore,
//! not this command's job.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use crate::analytics;
use crate::cli::CheckArgs;

pub fn check(args: CheckArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);

    // Grafting (RFC-0033) is reported **before** the parity checks, and before the no-checks bail: a
    // nest with no `checks/*.sql` is the common case, and its author still deserves to know which of
    // their views can never be reused. Reporting it after the bail made this dead code for most
    // nests - found by running the command rather than by the tests, which supplied a checks dir.
    let graft = crate::graft::report(&dir);
    if let Some(cycle) = &graft.cycle {
        bail!(
            "this nest's derivations form a cycle: {cycle}. A derivation may read decoded events and \
             other derivations, never itself."
        );
    }
    for (view, why) in &graft.never_graftable {
        println!("! view {view} can never be reused across an edit: it {why}");
    }
    for view in &graft.uncanonical {
        println!(
            "! view {view}: plan could not be canonicalised, so only a byte-identical edit matches"
        );
    }

    let checks = collect_checks(&dir, args.name.as_deref())?;

    let mut failures = 0usize;

    // RFC-0018 §1: authored views are validated as part of `check` - a broken view, or one that
    // references a table/column the registry no longer has (**drift**), fails loudly with a
    // fuzzy-matched fix hint instead of vanishing silently. This runs before the parity checks so a
    // drifted view is caught even if it's the reason a parity check would fail.
    //
    // Deliberately **before** the no-checks bail below, and evaluated regardless of whether it finds
    // anything (#539): a nest with views and no `checks/*.sql` yet is a normal intermediate state,
    // and it's exactly when an author most wants this validator. The old order bailed on the empty
    // `checks/` dir first, which made view drift invisible until a check happened to exist.
    let views_validated = nest_schema(&dir).map(|schema| {
        let issues = analytics::validate_nest_views(&dir, &schema);
        let n = issues.len();
        for issue in issues {
            let hint = issue
                .hint
                .map(|h| format!("\n    hint: {h}"))
                .unwrap_or_default();
            let first = issue.error.lines().next().unwrap_or(&issue.error);
            println!("✗ view {}: {first}{hint}", issue.file);
            failures += 1;
        }
        n
    });
    let entity_issues = crate::entities::validate(&dir);
    for issue in &entity_issues {
        println!("✗ incremental entity {}: {}", issue.name, issue.error);
        failures += 1;
    }

    if checks.is_empty() {
        let has_views = !analytics::nest_view_files(&dir).is_empty();
        let has_entities = crate::entities::has_declarations(&dir);
        // Only truly nothing to check - no checks, and either no views or no schema to validate them
        // against - keeps the original bail. A nest with views that were actually validated above
        // got a real answer instead, even with `checks/` empty.
        if (!has_views || views_validated.is_none()) && !has_entities {
            bail!(
                "no checks found in {} (expected checks/*.sql)",
                dir.join("checks").display()
            );
        }
        if failures > 0 {
            bail!("{failures} authored-definition issue(s) found");
        }
        println!("✓ no checks/*.sql, but all authored view(s) and entity declaration(s) validate cleanly");
        return Ok(());
    }

    let expected_dir = dir.join("checks").join("expected");
    if args.update {
        std::fs::create_dir_all(&expected_dir)
            .with_context(|| format!("cannot create {}", expected_dir.display()))?;
    }

    for (name, sql_path) in &checks {
        let sql = std::fs::read_to_string(sql_path)
            .with_context(|| format!("cannot read {}", sql_path.display()))?;
        let got = match analytics::query(&dir, &sql) {
            Ok(rows) => rows,
            Err(e) => {
                println!("✗ {name}: query failed - {e:#}");
                failures += 1;
                continue;
            }
        };
        let exp_path = expected_dir.join(format!("{name}.json"));

        if args.update {
            std::fs::write(&exp_path, serde_json::to_string_pretty(&got)?)
                .with_context(|| format!("cannot write {}", exp_path.display()))?;
            println!("● {name}: recorded {} row(s)", got.len());
            continue;
        }

        let expected: Vec<Value> = match std::fs::read_to_string(&exp_path) {
            Ok(raw) => serde_json::from_str(&raw)
                .with_context(|| format!("corrupt fixture {}", exp_path.display()))?,
            Err(_) => {
                println!(
                    "✗ {name}: no expected fixture - run `nuthatch check --update` to record it"
                );
                failures += 1;
                continue;
            }
        };

        match diff(&expected, &got) {
            None => println!("✓ {name}: {} row(s) match", got.len()),
            Some(msg) => {
                println!("✗ {name}: {msg}");
                failures += 1;
            }
        }
    }

    if failures > 0 {
        bail!("{failures}/{} check(s) failed", checks.len());
    }
    println!("✓ all {} check(s) passed", checks.len());
    Ok(())
}

/// The nest's decode-registry table schemas, for view drift-validation. `None` if the dir isn't a
/// nest (no config) - view validation is then skipped, not fatal.
fn nest_schema(dir: &Path) -> Option<Vec<crate::registry::TableSchema>> {
    let cfg = crate::config::Config::load(dir).ok()?;
    let reg = crate::registry::from_nest(dir, &cfg).ok()?;
    Some(reg.schema())
}

/// Every `checks/<name>.sql` (sorted), optionally filtered to names containing `filter`.
fn collect_checks(dir: &Path, filter: Option<&str>) -> Result<Vec<(String, PathBuf)>> {
    let checks_dir = dir.join("checks");
    let entries = match std::fs::read_dir(&checks_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .filter_map(|p| {
            let name = p.file_stem()?.to_string_lossy().into_owned();
            match filter {
                Some(f) if !name.contains(f) => None,
                _ => Some((name, p)),
            }
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Compare expected vs actual result sets. Returns None if identical, else a human diff of the first
/// discrepancy. Row order is significant - checks should `ORDER BY` for a deterministic comparison.
fn diff(expected: &[Value], got: &[Value]) -> Option<String> {
    if expected.len() != got.len() {
        return Some(format!(
            "row count differs: expected {}, got {}",
            expected.len(),
            got.len()
        ));
    }
    for (i, (e, g)) in expected.iter().zip(got).enumerate() {
        if e != g {
            return Some(format!(
                "row {i} differs:\n    expected: {e}\n    got:      {g}"
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diff_detects_count_and_value_mismatches() {
        let a = vec![json!({"x": 1}), json!({"x": 2})];
        assert!(diff(&a, &a).is_none());
        assert!(diff(&a, &a[..1]).unwrap().contains("row count"));
        let b = vec![json!({"x": 1}), json!({"x": 9})];
        assert!(diff(&a, &b).unwrap().contains("row 1 differs"));
    }

    /// A minimal nest with no `[[contracts]]` - `Config::load` and `DecodeRegistry::from_nest` both
    /// succeed with an empty schema, so `nest_schema` returns `Some(vec![])` without needing any ABI
    /// fixture. Enough to drive view validation on self-contained views that reference no real table.
    fn write_minimal_nest(dir: &Path) {
        std::fs::write(
            dir.join("nuthatch.toml"),
            "[nest]\nname = \"t\"\nchain = \"mainnet\"\nchain_id = 1\nrpc_urls = []\n",
        )
        .unwrap();
    }

    /// #539 fix 3: a nest with views and no `checks/*.sql` yet is a normal intermediate state, not an
    /// error - `check` must still validate the views instead of bailing on the missing directory.
    #[test]
    fn no_checks_directory_still_validates_a_clean_view() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_nest(dir.path());
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-ok.sql"),
            "CREATE VIEW ok AS SELECT 1 AS x;",
        )
        .unwrap();

        let result = check(CheckArgs {
            name: None,
            dir: dir.path().display().to_string(),
            update: false,
        });
        assert!(
            result.is_ok(),
            "a clean view with no checks/ must not bail on the missing directory: {result:?}"
        );
    }

    /// The other half: a broken view with no `checks/*.sql` must fail *because of the view*, not
    /// disappear behind (or be misreported as) the old "no checks found" bail.
    #[test]
    fn no_checks_directory_still_reports_a_broken_view() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_nest(dir.path());
        std::fs::create_dir_all(dir.path().join("views")).unwrap();
        std::fs::write(
            dir.path().join("views/10-broken.sql"),
            "CREATE VIEW broken AS SELECT * FROM nonexistent_table;",
        )
        .unwrap();

        let err = check(CheckArgs {
            name: None,
            dir: dir.path().display().to_string(),
            update: false,
        })
        .unwrap_err()
        .to_string();
        assert!(
            !err.contains("no checks found"),
            "must not be the generic empty-checks bail: {err}"
        );
        assert!(err.contains("authored-definition issue"), "{err}");
    }

    /// Unchanged: a nest with neither `checks/*.sql` nor any views has truly nothing to check.
    #[test]
    fn no_checks_and_no_views_still_bails() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_nest(dir.path());

        let err = check(CheckArgs {
            name: None,
            dir: dir.path().display().to_string(),
            update: false,
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("no checks found"), "{err}");
    }

    #[test]
    fn no_checks_directory_still_validates_a_clean_entity() {
        let dir = tempfile::tempdir().unwrap();
        write_minimal_nest(dir.path());
        std::fs::create_dir_all(dir.path().join("entities")).unwrap();
        std::fs::write(
            dir.path().join("entities.toml"),
            "[[entities]]\nname = \"constant\"\nsql = \"entities/constant.sql\"\nkey = [\"id\"]\nmax_rows = 1\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("entities/constant.sql"), "SELECT 1 AS id").unwrap();

        let result = check(CheckArgs {
            name: None,
            dir: dir.path().display().to_string(),
            update: false,
        });
        assert!(
            result.is_ok(),
            "a clean entity with no checks/ must not bail on the missing directory: {result:?}"
        );
    }

    #[test]
    fn collect_filters_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let checks = dir.path().join("checks");
        std::fs::create_dir_all(&checks).unwrap();
        std::fs::write(checks.join("parity_rewards.sql"), "SELECT 1").unwrap();
        std::fs::write(checks.join("parity_allocs.sql"), "SELECT 1").unwrap();
        std::fs::write(checks.join("other.sql"), "SELECT 1").unwrap();

        let all = collect_checks(dir.path(), None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].0, "other"); // sorted
        let parity = collect_checks(dir.path(), Some("parity")).unwrap();
        assert_eq!(parity.len(), 2);
        assert!(parity.iter().all(|(n, _)| n.contains("parity")));
    }
}
