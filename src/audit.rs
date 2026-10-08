//! The audit surface (RFC-0008 C6). `audit report` summarises the threshold flags in a range with
//! their block bounds, for a human record; `audit sealed` lives in `sealed_audit`.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

/// `nuthatch audit report --from --to` - a summary of the flags in a range and their block bounds.
/// Returned as JSON; the CLI can render markdown from it.
pub fn report(dir: &Path, from: u64, to: u64) -> Result<Value> {
    let flags = crate::analytics::query(
        dir,
        &format!(
            "SELECT count(*) AS n, min(block_number) AS lo, max(block_number) AS hi \
             FROM threshold_flag WHERE block_number BETWEEN {from} AND {to}"
        ),
    )
    .unwrap_or_default();

    let flag_row = flags.first().cloned().unwrap_or(json!({}));
    Ok(json!({
        "range": { "from": from, "to": to },
        "threshold_flags": {
            "count": flag_row.get("n").cloned().unwrap_or(json!(0)),
            "block_bounds": [flag_row.get("lo").cloned().unwrap_or(json!(null)), flag_row.get("hi").cloned().unwrap_or(json!(null))],
        },
    }))
}

/// Render a report JSON as a compact markdown record for a human audit file.
pub fn report_markdown(r: &Value) -> String {
    let tf = &r["threshold_flags"];
    format!(
        "# Compliance audit report\n\n\
         - Range: blocks {}-{}\n\
         - Threshold flags: {}\n",
        r["range"]["from"], r["range"]["to"], tf["count"],
    )
}

/// CLI entry: report/sealed dispatch.
pub async fn run(args: crate::cli::AuditArgs) -> Result<()> {
    use std::path::PathBuf;
    match args.what {
        crate::cli::AuditWhat::Sealed(a) => crate::sealed_audit::run_cli(a).await,
        crate::cli::AuditWhat::Report(a) => {
            let dir = PathBuf::from(&a.dir);
            let r = report(&dir, a.from, a.to)?;
            if a.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", report_markdown(&r));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_counts_sealed_flags_in_range_only() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let flag = |block: u64| {
            format!(
                r#"{{"table":"threshold_flag","kind":"threshold_flag","address":"0xaa","value":"500","block_number":{block},"log_index":0}}"#
            )
        };
        crate::seal::seal_range(d, &[flag(10), flag(12), flag(30)], 10, 30).unwrap();

        let rep = report(d, 10, 20).unwrap();
        assert_eq!(rep["threshold_flags"]["count"], Value::from(2u64));
        assert_eq!(
            rep["threshold_flags"]["block_bounds"][0],
            Value::from(10u64)
        );
        assert_eq!(
            rep["threshold_flags"]["block_bounds"][1],
            Value::from(12u64)
        );
        assert!(report_markdown(&rep).contains("Threshold flags: 2"));
    }
}
