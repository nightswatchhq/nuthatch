//! #1261, RFC-0052 S3: the fixture half of the Trino contract test. It seals a nest with one plain
//! table and one **drifted** table (its second seal adds a column, so the two segments' Parquet
//! schemas differ), publishes it to MinIO, and writes the numbers a Trino external table over the
//! prefix must reproduce. `scripts/trino-contract.sh` is the other half: the `trino-contract` CI
//! job runs this, then asks Trino.
//!
//! The drift is placed so it is silent: `sender` sorts between `log_index` and `table`
//! (`reading-segments.md` §Ordering), so only text columns shift. Mapped by index, the first
//! segment's `value` falls off the end of its file and reads as NULL rather than failing a type
//! check, which is what `hive.parquet.use-column-names=true` exists to prevent.
//!
//! `value` holds numbers past `i64::MAX`, so the sum is `TRY_CAST(value AS DECIMAL(38,0))` on both
//! engines; a `BIGINT` cast would read every row as NULL.
//!
//! Gated like `e2e_minio_publish.rs`: skips without `NUTHATCH_MINIO_ENDPOINT`, fails without it
//! under `NUTHATCH_REQUIRE_MINIO`. `NUTHATCH_TRINO_FIXTURE_OUT` names the file the expected numbers
//! go to.

#![cfg(feature = "object-store")]

use std::path::{Path, PathBuf};

use nuthatch::publish::{sync, verify};
use nuthatch::registry::{DecodedRow, Value as DecodedValue};
use nuthatch::seal::{load_manifest, seal_range, segment_path, Segment};

/// At `SEAL_TABLE_FLOOR`, so every segment is final: a provisional one would be folded into the
/// next seal, erasing the drift, and would not be published at all.
const ROWS_PER_SEAL: u64 = 1_000;

const PLAIN: &str = "usdc__transfer";
const DRIFTED: &str = "usdc__drift";

fn endpoint() -> Option<String> {
    match std::env::var("NUTHATCH_MINIO_ENDPOINT") {
        Ok(u) => Some(u),
        Err(_) if std::env::var("NUTHATCH_REQUIRE_MINIO").is_ok() => panic!(
            "NUTHATCH_REQUIRE_MINIO is set but NUTHATCH_MINIO_ENDPOINT is not - the Trino contract \
             fixture would have silently skipped"
        ),
        Err(_) => {
            eprintln!(
                "SKIPPED: set NUTHATCH_MINIO_ENDPOINT (plus AWS_* env, incl. AWS_ALLOW_HTTP=true) \
                 to run the Trino contract fixture"
            );
            None
        }
    }
}

fn bucket() -> String {
    std::env::var("NUTHATCH_MINIO_BUCKET").unwrap_or_else(|_| "nuthatch-publish-test".into())
}

fn write_nest(dir: &Path) {
    std::fs::write(
        dir.join("nuthatch.toml"),
        r#"[nest]
name = "t"
chain = "ethereum"
chain_id = 1
rpc_urls = ["http://127.0.0.1:1"]
schema_version = 1

[[contracts]]
alias = "usdc"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/usdc.json"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("abis")).unwrap();
    std::fs::write(
        dir.join("abis/usdc.json"),
        r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("schema.json"),
        serde_json::to_string(&serde_json::json!({"tables": []})).unwrap(),
    )
    .unwrap();
}

fn row(table: &str, block: u64, value: u128, sender: Option<[u8; 20]>) -> String {
    let mut params = Vec::new();
    if let Some(s) = sender {
        params.push(("sender".to_string(), DecodedValue::Address(s)));
    }
    params.push((
        "value".to_string(),
        DecodedValue::Word16(value.to_be_bytes()),
    ));
    DecodedRow {
        table: table.into(),
        params,
        block_number: block,
        block_hash: format!("0x{block:064x}"),
        block_timestamp: 1_700_000_000 + block,
        timestamps: true,
        log_index: 0,
        tx_hash: format!("0x{block:064x}"),
        address: "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".into(),
    }
    .to_json()
    .to_string()
}

fn plain_value(i: u64) -> u128 {
    (i as u128 + 1) * 100_000_000_000_000_000_000
}

fn drifted_value(i: u64) -> u128 {
    (i as u128 + 1) * 7_000_000_000_000_000_003
}

/// Two seals of `ROWS_PER_SEAL` rows per table. The drifted table's first seal has no `sender`; its
/// second has one on every row.
fn drifted_nest() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_nest(dir.path());
    for seal in 0..2u64 {
        let from = 10 + seal * ROWS_PER_SEAL;
        let to = from + ROWS_PER_SEAL - 1;
        let mut rows = Vec::new();
        for i in 0..ROWS_PER_SEAL {
            let n = seal * ROWS_PER_SEAL + i;
            rows.push(row(PLAIN, from + i, plain_value(n), None));
            let sender = (seal == 1).then(|| [(i % 251) as u8 + 1; 20]);
            rows.push(row(DRIFTED, from + i, drifted_value(n), sender));
        }
        seal_range(dir.path(), &rows, from, to)
            .unwrap()
            .expect("sealed");
    }
    dir
}

fn footer_columns(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).unwrap();
    let builder =
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

/// A table's segments in block order, resolved to local files.
fn files(dir: &Path, segs: &[Segment]) -> Vec<PathBuf> {
    let mut segs: Vec<&Segment> = segs.iter().collect();
    segs.sort_by(|a, b| {
        (a.from_block, a.to_block, &a.hash).cmp(&(b.from_block, b.to_block, &b.hash))
    });
    segs.iter()
        .map(|s| segment_path(dir, &s.file, &s.hash))
        .collect()
}

/// `(count(*), sum(value) as decimal text, count(sender))` via nuthatch's bundled DuckDB, by name.
fn duckdb_numbers(files: &[PathBuf], has_sender: bool) -> (i64, String, i64) {
    let list = files
        .iter()
        .map(|p| format!("'{}'", p.display()))
        .collect::<Vec<_>>()
        .join(", ");
    let sender = if has_sender { "count(sender)" } else { "0" };
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
    conn.prepare(&format!(
        "SELECT count(*), CAST(sum(TRY_CAST(value AS DECIMAL(38,0))) AS VARCHAR), {sender} \
         FROM read_parquet([{list}], union_by_name=true)"
    ))
    .and_then(|mut s| s.query_row([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))))
    .expect("count/sum over the sealed segments")
}

/// Seals the fixture, proves the drift from the footers, and returns DuckDB's numbers per table.
fn sealed_fixture() -> (
    tempfile::TempDir,
    serde_json::Map<String, serde_json::Value>,
) {
    let nest = drifted_nest();
    let manifest = load_manifest(nest.path()).unwrap();
    for table in [PLAIN, DRIFTED] {
        let segs = &manifest.tables[table];
        assert_eq!(segs.len(), 2, "premise: {table} must have two segments");
        assert!(
            segs.iter().all(|s| !s.provisional),
            "premise: {table}'s segments must be final"
        );
    }

    let plain = files(nest.path(), &manifest.tables[PLAIN]);
    let drifted = files(nest.path(), &manifest.tables[DRIFTED]);

    let (p0, p1) = (footer_columns(&plain[0]), footer_columns(&plain[1]));
    assert_eq!(p0, p1, "premise: the plain table must not drift");
    let (d0, d1) = (footer_columns(&drifted[0]), footer_columns(&drifted[1]));
    assert!(
        !d0.contains(&"sender".to_string()) && d1.contains(&"sender".to_string()),
        "the drifted table's second segment must add `sender`: {d0:?} then {d1:?}"
    );
    let at = |cols: &[String], c: &str| cols.iter().position(|x| x == c).unwrap();
    assert_ne!(
        at(&d0, "value"),
        at(&d1, "value"),
        "`value` must move between the drifted table's segments, or an index-mapped read of it \
         would still be right: {d0:?} then {d1:?}"
    );
    assert_eq!(
        at(&d0, "value"),
        d0.len() - 1,
        "`value` must be the first segment's last column, so index mapping reads it as NULL \
         rather than failing on a type: {d0:?}"
    );
    assert!(
        at(&d1, "sender") > at(&d1, "log_index"),
        "only Utf8 columns may shift, or the index-mapped read errors instead of reading wrong: \
         {d1:?}"
    );

    let rows = 2 * ROWS_PER_SEAL;
    let mut expected = serde_json::Map::new();
    for (table, files, value, has_sender) in [
        (PLAIN, &plain, plain_value as fn(u64) -> u128, false),
        (DRIFTED, &drifted, drifted_value as fn(u64) -> u128, true),
    ] {
        let (count, sum, senders) = duckdb_numbers(files, has_sender);
        let want: u128 = (0..rows).map(value).sum();
        assert!(
            want > i64::MAX as u128,
            "the sum must need the decimal cast"
        );
        assert_eq!(count, rows as i64, "{table}: DuckDB count");
        assert_eq!(
            sum,
            want.to_string(),
            "{table}: DuckDB sum against the fixture"
        );
        if has_sender {
            assert_eq!(
                senders, ROWS_PER_SEAL as i64,
                "{table}: DuckDB count(sender)"
            );
        }
        eprintln!("duckdb {table}: count={count} sum={sum} count_sender={senders}");
        expected.insert(
            table.to_string(),
            serde_json::json!({ "count": count, "sum": sum, "count_sender": senders }),
        );
    }
    (nest, expected)
}

/// Offline, so the drift is proven wherever the suite runs, not only where MinIO does.
#[test]
fn the_fixture_drifts_and_duckdb_reads_it_by_name() {
    sealed_fixture();
}

#[tokio::test]
async fn publish_a_drifted_nest_and_record_what_trino_must_return() {
    let Some(_) = endpoint() else { return };
    let bucket = bucket();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("e2e-trino-contract/{}-{nanos}", std::process::id());
    let target = format!("s3://{bucket}/{prefix}");

    let (nest, expected) = sealed_fixture();
    let report = sync(nest.path(), &target, false).await.unwrap();
    verify(nest.path(), &target, true).await.unwrap();

    let fixture = serde_json::json!({
        "bucket": bucket,
        "prefix": prefix,
        "dataset": report.dataset,
        "tables": expected,
    });
    eprintln!("fixture: {fixture}");
    if let Ok(out) = std::env::var("NUTHATCH_TRINO_FIXTURE_OUT") {
        std::fs::write(&out, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();
    }
}
