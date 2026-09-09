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
    // **A case-insensitive collision is refused, not merged.**
    //
    // The manifest is a `BTreeMap` and would happily hold both `Prices` and `prices` as separate
    // tables. `define_offchain_views` then creates `offchain__Prices` and `offchain__prices`, and
    // DuckDB resolves identifiers case-insensitively - so the second `CREATE OR REPLACE VIEW`
    // replaces the first and one table silently answers with the other's rows.
    //
    // Refusing is the honest half of the fix. Lower-casing the name instead would make the two the
    // same table, which is a guess about intent: an operator who dropped `Prices` and `prices` may
    // well have meant two datasets, and quietly concatenating them is the same silent wrongness in
    // a different place. The error names the existing table so the choice is theirs.
    if let Some(existing) = catalogue
        .tables
        .keys()
        .find(|k| k.as_str() != table && k.eq_ignore_ascii_case(table))
    {
        bail!(
            "offchain table `{table}` collides with `{existing}`, which is already in the \
             manifest: SQL view names are case-insensitive, so both would resolve to the same \
             view and one would silently answer with the other's rows. Rename one of them."
        );
    }
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

/// **One file description, read twice, rather than one path opened twice.**
///
/// This used to build the metadata from `File::open(path)` and then take the bytes from a separate
/// `std::fs::read(path)`. A source replaced between those two calls - a feed rewriting its export,
/// an operator re-running a job - would be described by the first file's row count and columns while
/// the stored snapshot held the second file's bytes, and the manifest would state something the
/// segment does not contain. Nothing about that is detectable afterwards, because the hash is taken
/// over the bytes that won.
///
/// The path is resolved once, here, and everything else happens on [`read_parquet_handle`], which
/// has no path to re-open. That is deliberate: a test can assert the property, but a signature that
/// cannot express the defect is worth more than a test that watches for it.
fn read_parquet(path: &Path) -> Result<(Vec<u8>, usize, Vec<String>)> {
    let file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    read_parquet_handle(file).with_context(|| format!("reading {}", path.display()))
}

/// Metadata and bytes from one open file description.
///
/// `try_clone` shares the description, so both reads see the same inode whatever happens to the name
/// it was opened under. The offset is shared with it, hence the explicit rewind before taking the
/// bytes.
fn read_parquet_handle(mut file: std::fs::File) -> Result<(Vec<u8>, usize, Vec<String>)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use std::io::{Read, Seek, SeekFrom};
    let builder = ParquetRecordBatchReaderBuilder::try_new(file.try_clone()?)
        .context("invalid Parquet source")?;
    let columns = builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect::<Vec<_>>();
    validate_columns(&columns)?;
    let rows = builder.metadata().file_metadata().num_rows() as usize;
    drop(builder);
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok((bytes, rows, columns))
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

    /// Two table names differing only in case would become one DuckDB view, and the later
    /// `CREATE OR REPLACE VIEW` would make one table answer with the other's rows. Refused rather
    /// than merged, because merging is a guess about what the operator meant.
    #[test]
    fn a_case_only_table_name_collision_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.csv");
        std::fs::write(&src, "token\nWETH\n").unwrap();

        drop_file(dir.path(), &src, "prices").expect("the first drop defines the table");

        let err = drop_file(dir.path(), &src, "Prices")
            .expect_err("a case-only variant must be refused, not silently shadow the first")
            .to_string();
        assert!(err.contains("collides with"), "{err}");
        assert!(
            err.contains("prices"),
            "the error must name the existing table: {err}"
        );

        // And the first table is untouched by the refusal.
        let cat = load(dir.path()).unwrap();
        assert!(cat.tables.contains_key("prices"));
        assert!(!cat.tables.contains_key("Prices"));
    }

    /// The same name is not a collision with itself - re-dropping a table must keep working.
    #[test]
    fn re_dropping_the_same_table_is_not_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        std::fs::write(&a, "token\nWETH\n").unwrap();
        let b = dir.path().join("b.csv");
        std::fs::write(&b, "token\nUSDC\n").unwrap();
        drop_file(dir.path(), &a, "prices").unwrap();
        drop_file(dir.path(), &b, "prices").expect("a second snapshot of the same table is normal");
        assert_eq!(load(dir.path()).unwrap().tables["prices"].len(), 2);
    }

    /// The manifest must describe the bytes that were stored. Reading metadata from one open of the
    /// path and the bytes from another lets the source change in between, so the row count and
    /// columns end up describing a file the snapshot does not contain.
    #[test]
    fn parquet_metadata_and_bytes_come_from_one_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.parquet");
        write_parquet_fixture(&src, &["a", "b"], 3);

        let (bytes, rows, columns) = read_parquet(&src).unwrap();
        assert_eq!(rows, 3);
        assert_eq!(columns, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            bytes,
            std::fs::read(&src).unwrap(),
            "the bytes returned must be the file that was described"
        );
    }

    /// The property the fix rests on, asserted against `read_parquet_handle` itself rather than
    /// against the file system in isolation: the handle is opened, the **path** is then replaced by
    /// a genuinely different file, and the metadata and bytes that come back must both describe the
    /// version the handle was opened on.
    ///
    /// This is the test the first attempt got wrong twice. `File::create` truncates the same inode
    /// in place, so a "replacement" written that way is visible through an open handle and proves
    /// the opposite; the replacement has to be a `rename`, which is what an atomic publish does. And
    /// asserting only that `read_parquet`'s bytes match the file on disk passes whether or not the
    /// bytes were re-read by path, because in that test nothing changes in between - the mutation
    /// survived, which is how the gap was found.
    #[test]
    fn metadata_and_bytes_describe_the_version_the_handle_was_opened_on() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("p.parquet");
        write_parquet_fixture(&src, &["a", "b"], 3);
        let three = std::fs::read(&src).unwrap();

        let handle = std::fs::File::open(&src).unwrap();

        // An atomic publish of a different file over the same name, after the open.
        let replacement = dir.path().join("p.parquet.new");
        write_parquet_fixture(&replacement, &["a", "b"], 9);
        std::fs::rename(&replacement, &src).unwrap();
        assert_ne!(
            three,
            std::fs::read(&src).unwrap(),
            "the fixture must actually differ, or this proves nothing"
        );

        let (bytes, rows, columns) = read_parquet_handle(handle).unwrap();
        assert_eq!(
            rows, 3,
            "the row count must be the opened version's, not the path's"
        );
        assert_eq!(columns, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            bytes, three,
            "the bytes stored must be the same version the row count describes"
        );
    }

    fn write_parquet_fixture(path: &std::path::Path, cols: &[&str], rows: usize) {
        use arrow::array::StringArray;
        use arrow::datatypes::{DataType, Field, Schema};
        use arrow::record_batch::RecordBatch;
        use std::sync::Arc;
        let schema = Arc::new(Schema::new(
            cols.iter()
                .map(|c| Field::new(*c, DataType::Utf8, false))
                .collect::<Vec<_>>(),
        ));
        let arrays: Vec<arrow::array::ArrayRef> = cols
            .iter()
            .map(|c| {
                Arc::new(StringArray::from(
                    (0..rows).map(|i| format!("{c}{i}")).collect::<Vec<_>>(),
                )) as arrow::array::ArrayRef
            })
            .collect();
        let batch = RecordBatch::try_new(schema.clone(), arrays).unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut w = parquet::arrow::ArrowWriter::try_new(file, schema, None).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }
}
