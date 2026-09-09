//! Immutable local file snapshots (RFC-0045 stage 1).
//!
//! This is intentionally outside the chain catalogue: importing a file must make it queryable, not
//! make it an input to replay. The catalogue is content-addressed and records enough provenance to
//! reproduce a result from the retained snapshot.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DIR: &str = "offchain";
const SEGMENTS: &str = "segments";
const MANIFEST: &str = "manifest.json";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalogue {
    #[serde(default)]
    pub tables: BTreeMap<String, Vec<Snapshot>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub hash: String,
    pub file: String,
    pub rows: usize,
    pub columns: Vec<String>,
    pub source: String,
    pub ingested_at: String,
    pub tool_version: String,
}

pub fn catalogue_path(dir: &Path) -> PathBuf {
    dir.join(DIR).join(MANIFEST)
}

pub fn load(dir: &Path) -> Result<Catalogue> {
    let path = catalogue_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).context("corrupt offchain provenance manifest"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Catalogue::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// Ingest one local snapshot. The source is read now and never during indexing or query execution.
pub fn drop_file(dir: &Path, source: &Path, table: &str) -> Result<()> {
    validate_table(table)?;
    let (bytes, rows, columns) = read_source(source)?;
    if columns.is_empty() {
        bail!("offchain source {} has no columns", source.display());
    }
    let hash = hex::encode(Sha256::digest(&bytes));
    let out_dir = dir.join(DIR).join(SEGMENTS);
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("cannot create {}", out_dir.display()))?;
    let file = format!("{table}-{hash}.parquet");
    let out = out_dir.join(&file);
    if !out.exists() {
        std::fs::write(&out, &bytes).with_context(|| format!("writing {}", out.display()))?;
    }
    let mut catalogue = load(dir)?;
    let snapshots = catalogue.tables.entry(table.to_string()).or_default();
    if !snapshots.iter().any(|s| s.hash == hash) {
        snapshots.push(Snapshot {
            hash,
            file,
            rows,
            columns,
            source: source.display().to_string(),
            ingested_at: now_stamp(),
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
        });
        save(dir, &catalogue)?;
    }
    println!("sealed offchain snapshot for offchain__{table}");
    Ok(())
}

fn read_source(path: &Path) -> Result<(Vec<u8>, usize, Vec<String>)> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "csv" => read_csv(path),
        "json" => read_json(path),
        "parquet" => read_parquet(path),
        _ => bail!(
            "offchain source {} must be CSV, JSON, or Parquet",
            path.display()
        ),
    }
}

fn read_csv(path: &Path) -> Result<(Vec<u8>, usize, Vec<String>)> {
    let mut reader =
        csv::Reader::from_path(path).with_context(|| format!("reading {}", path.display()))?;
    let headers = reader
        .headers()?
        .iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    validate_columns(&headers)?;
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record?;
        if record.len() != headers.len() {
            bail!(
                "CSV row has {} fields; header has {}",
                record.len(),
                headers.len()
            );
        }
        let mut obj = Map::new();
        for (name, value) in headers.iter().zip(record.iter()) {
            obj.insert(name.clone(), Value::String(value.to_string()));
        }
        rows.push(Value::Object(obj));
    }
    Ok((
        crate::seal::write_snapshot_parquet(&rows)?,
        rows.len(),
        headers,
    ))
}

fn read_json(path: &Path) -> Result<(Vec<u8>, usize, Vec<String>)> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let rows: Vec<Value> =
        serde_json::from_slice(&raw).context("JSON source must be an array of objects")?;
    let mut columns = std::collections::BTreeSet::new();
    for row in &rows {
        let Some(obj) = row.as_object() else {
            bail!("JSON source must contain objects, not scalar values");
        };
        columns.extend(obj.keys().cloned());
    }
    let columns = columns.into_iter().collect::<Vec<_>>();
    validate_columns(&columns)?;
    Ok((
        crate::seal::write_snapshot_parquet(&rows)?,
        rows.len(),
        columns,
    ))
}

fn read_parquet(path: &Path) -> Result<(Vec<u8>, usize, Vec<String>)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(file).context("invalid Parquet source")?;
    let columns = builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect::<Vec<_>>();
    validate_columns(&columns)?;
    let rows = builder.metadata().file_metadata().num_rows() as usize;
    Ok((std::fs::read(path)?, rows, columns))
}

fn validate_table(table: &str) -> Result<()> {
    if table.is_empty()
        || !table
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("offchain table name must contain only letters, digits, and '_'");
    }
    Ok(())
}
fn validate_columns(columns: &[String]) -> Result<()> {
    if columns.iter().any(|c| c.is_empty()) {
        bail!("offchain source has an empty column name");
    }
    Ok(())
}
fn save(dir: &Path, catalogue: &Catalogue) -> Result<()> {
    let path = catalogue_path(dir);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(catalogue)?)
        .context("writing offchain provenance manifest")?;
    std::fs::rename(tmp, path).context("installing offchain provenance manifest")
}
fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    format!(
        "unix:{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_snapshot_is_content_addressed_and_records_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("prices.csv");
        std::fs::write(&input, "token,price\nWETH,3210\n").unwrap();
        drop_file(dir.path(), &input, "prices").unwrap();

        let catalogue = load(dir.path()).unwrap();
        let snapshot = &catalogue.tables["prices"][0];
        assert_eq!(snapshot.rows, 1);
        assert_eq!(snapshot.columns, ["token", "price"]);
        assert_eq!(snapshot.source, input.display().to_string());
        assert!(dir
            .path()
            .join(DIR)
            .join(SEGMENTS)
            .join(&snapshot.file)
            .exists());

        drop_file(dir.path(), &input, "prices").unwrap();
        assert_eq!(load(dir.path()).unwrap().tables["prices"].len(), 1);
    }

    #[test]
    fn imported_snapshot_joins_sealed_chain_data_on_the_normal_sql_surface() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("prices.csv");
        std::fs::write(&input, "token,price\nWETH,3210\n").unwrap();
        drop_file(dir.path(), &input, "prices").unwrap();

        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"dex__swap","token":"WETH","amount":"2","block_number":10,"log_index":0,"tx_hash":"0xabc"}"#.to_string()],
            10,
            10,
        )
        .unwrap();

        let rows = crate::analytics::query(
            dir.path(),
            r#"SELECT s.token, s.amount, p.price
               FROM "dex__swap" s JOIN offchain__prices p USING (token)"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["token"], "WETH");
        assert_eq!(rows[0]["amount"], "2");
        assert_eq!(rows[0]["price"], "3210");
    }

    #[test]
    fn removing_offchain_snapshots_does_not_prevent_chain_data_from_answering() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("prices.json");
        std::fs::write(&input, r#"[{"token":"WETH","price":3210}]"#).unwrap();
        drop_file(dir.path(), &input, "prices").unwrap();
        crate::seal::seal_range(
            dir.path(),
            &[r#"{"table":"dex__swap","token":"WETH","block_number":10,"log_index":0,"tx_hash":"0xabc"}"#.to_string()],
            10,
            10,
        )
        .unwrap();
        std::fs::remove_dir_all(dir.path().join(DIR)).unwrap();

        let rows = crate::analytics::query(dir.path(), r#"SELECT token FROM "dex__swap""#).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["token"], "WETH");
    }
}
