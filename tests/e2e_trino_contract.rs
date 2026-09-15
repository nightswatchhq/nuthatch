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
//! #1359, RFC-0055 S3 uses the same fixture. The nest carries authored views, `emit dune` translates
//! them, and each translation must return the rows the nest's own view returns: in DuckDB over
//! tables shaped like the Dune upload here, and on Trino over the prefix in the CI job. `usdc__wide`
//! holds values past `DECIMAL(38,0)`, so `_dec` and `_overflow` are compared where they do something.
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
const WIDE: &str = "usdc__wide";

/// The Dune namespace the translated views read, swapped for the Trino catalogue in the CI job.
const SOURCE: &str = "trino_fixture";

const BASE_VIEWS: &str = r#"
CREATE VIEW transfer_totals AS
SELECT count(*) AS transfers, CAST(sum(value_dec) AS VARCHAR) AS total,
       min(block_number) AS first_block, max(block_number) AS last_block
FROM usdc__transfer;

CREATE VIEW wide_values AS
SELECT value_overflow AS overflowed, count(*) AS n, sum(value_dec) AS total_dec,
       count(value_dec) AS with_dec
FROM usdc__wide
GROUP BY value_overflow;

CREATE VIEW top_blocks AS
SELECT lower(address) AS contract, block_number AS block, value_dec AS amount
FROM usdc__transfer
WHERE NOT value_overflow AND value_dec > 1.5
ORDER BY value_dec DESC, block_number ASC
LIMIT 3;

-- Valid DuckDB, and refused: avg is a double in DuckDB and a decimal in DuneSQL.
CREATE VIEW mean_value AS SELECT avg(value_dec) AS mean FROM usdc__transfer;
"#;

const DERIVED_VIEWS: &str = r#"
CREATE VIEW sender_kinds AS
WITH marked AS (
    SELECT CASE WHEN sender IS NULL THEN 'none' ELSE 'some' END AS kind, block_number, value_dec
    FROM usdc__drift
    WHERE block_number BETWEEN 10 AND 2009 AND tx_hash LIKE '0x%' AND log_index IN (0, 1)
)
SELECT m.kind AS kind, count(*) AS n, sum(m.value_dec) AS total, max(t.last_block) AS last_block
FROM marked m CROSS JOIN transfer_totals t
GROUP BY m.kind
UNION ALL
SELECT 'all' AS kind, count(*) AS n, sum(value_dec) AS total, NULL AS last_block
FROM usdc__drift;

CREATE VIEW drift_summary AS
SELECT coalesce(max(sender), 'no sender') AS any_sender,
       count(*) FILTER (WHERE sender IS NOT NULL) AS with_sender,
       CAST(sum(- value_dec) AS VARCHAR) AS negated,
       'rows:' || CAST(count(*) AS VARCHAR) AS label,
       (SELECT count(*) FROM usdc__transfer WHERE log_index NOT IN (5, 6)) AS transfers,
       EXISTS (SELECT 1 FROM transfer_totals WHERE transfers > 0) AS has_totals
FROM usdc__drift;
"#;

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

/// A `schema.json` table entry, in the shape `nuthatch schema` writes.
fn table_schema(table: &str, params: &[(&str, &str)]) -> serde_json::Value {
    let mut columns: Vec<serde_json::Value> = [
        ("block_number", "u64"),
        ("block_hash", "bytes32"),
        ("block_timestamp", "u64"),
        ("tx_hash", "bytes32"),
        ("log_index", "u64"),
        ("address", "address"),
        ("_seq", "u64"),
    ]
    .iter()
    .map(|(name, storage)| {
        serde_json::json!({"indexed": false, "name": name, "sol_type": "implicit", "storage": storage})
    })
    .collect();
    for (name, sol_type) in params {
        let storage = if *sol_type == "address" {
            "address"
        } else {
            "word32"
        };
        columns.push(serde_json::json!({
            "indexed": false, "name": name, "sol_type": sol_type, "storage": storage
        }));
    }
    serde_json::json!({
        "table": table, "alias": "usdc", "event": "", "topic0": "", "columns": columns
    })
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
    let tables = [
        table_schema(PLAIN, &[("value", "uint256")]),
        table_schema(DRIFTED, &[("sender", "address"), ("value", "uint256")]),
        table_schema(WIDE, &[("value", "uint256")]),
    ];
    std::fs::write(
        dir.join("schema.json"),
        serde_json::to_string(&serde_json::json!({ "tables": tables })).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("views")).unwrap();
    std::fs::write(dir.join("views/10-base.sql"), BASE_VIEWS).unwrap();
    std::fs::write(dir.join("views/20-derived.sql"), DERIVED_VIEWS).unwrap();
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

/// One row in ten is 39 digits, past `DECIMAL(38,0)`, so its `_dec` is NULL and its `_overflow` true.
fn wide_value(i: u64) -> u128 {
    if i % 10 == 0 {
        u128::MAX - i as u128
    } else {
        (i as u128 + 1) * 3
    }
}

const DECIMAL_38: u128 = 100_000_000_000_000_000_000_000_000_000_000_000_000;

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
            rows.push(row(WIDE, from + i, wide_value(n), None));
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

fn file_list(files: &[PathBuf]) -> String {
    files
        .iter()
        .map(|p| format!("'{}'", p.display()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `(count(*), sum(value) as decimal text, count(sender))` via nuthatch's bundled DuckDB, by name.
fn duckdb_numbers(files: &[PathBuf], has_sender: bool) -> (i64, String, i64) {
    let list = file_list(files);
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
    for table in [PLAIN, DRIFTED, WIDE] {
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

    let (count, sum, _) = duckdb_numbers(&files(nest.path(), &manifest.tables[WIDE]), false);
    assert!(
        (0..rows).map(wide_value).any(|v| v >= DECIMAL_38),
        "premise: {WIDE} must hold values past DECIMAL(38,0)"
    );
    let fits: u128 = (0..rows).map(wide_value).filter(|v| *v < DECIMAL_38).sum();
    assert_eq!(count, rows as i64, "{WIDE}: DuckDB count");
    assert_eq!(
        sum,
        fits.to_string(),
        "{WIDE}: DuckDB sums only the values that fit"
    );
    eprintln!("duckdb {WIDE}: count={count} sum={sum}");
    expected.insert(
        WIDE.to_string(),
        serde_json::json!({ "count": count, "sum": sum, "count_sender": 0 }),
    );
    (nest, expected)
}

/// One `|`-joined text line per row. The same projection wraps the nest's view and its translation,
/// so a difference between them belongs to the translation.
fn lines_query(inner: &str, columns: &[String]) -> String {
    let expr = columns
        .iter()
        .map(|c| {
            format!(
                "coalesce(CAST(\"{}\" AS VARCHAR), '<null>')",
                c.replace('"', "\"\"")
            )
        })
        .collect::<Vec<_>>()
        .join(" || '|' || ");
    format!("SELECT {expr} AS line FROM ({inner}) q")
}

/// Every view translated and run both ways in DuckDB: the nest's own view through `analytics`, and
/// the translation over tables in the upload's shape. Returns what the Trino half runs and expects.
fn translated_views(nest: &Path) -> serde_json::Map<String, serde_json::Value> {
    let schema: serde_json::Value =
        serde_json::from_slice(&std::fs::read(nest.join("schema.json")).unwrap()).unwrap();
    let tables: Vec<nuthatch::registry::TableSchema> =
        serde_json::from_value(schema["tables"].clone()).unwrap();
    let outcomes = nuthatch::dune_views::translate(
        &nuthatch::analytics::nest_view_files(nest),
        &tables,
        SOURCE,
    )
    .unwrap();

    let manifest = load_manifest(nest).unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    conn.execute_batch(&format!(
        "ATTACH ':memory:' AS dune; CREATE SCHEMA dune.{SOURCE};"
    ))
    .unwrap();
    for table in [PLAIN, DRIFTED, WIDE] {
        let list = file_list(&files(nest, &manifest.tables[table]));
        // RFC-0055 §4's upload: the four counters as integers, every other column as its text.
        conn.execute_batch(&format!(
            "CREATE TABLE dune.{SOURCE}.{table} AS SELECT * REPLACE (\
             CAST(block_number AS BIGINT) AS block_number, CAST(log_index AS BIGINT) AS log_index, \
             CAST(_seq AS BIGINT) AS _seq, CAST(block_timestamp AS BIGINT) AS block_timestamp) \
             FROM read_parquet([{list}], union_by_name=true)"
        ))
        .unwrap();
    }

    let mut out = serde_json::Map::new();
    let mut refused = Vec::new();
    for o in outcomes {
        let name = o.name.clone().expect("every fixture statement is a view");
        let view = match o.result {
            Ok(v) => v,
            Err(why) => {
                refused.push((name, why));
                continue;
            }
        };
        let own = format!("SELECT * FROM \"{name}\"");
        let mut want: Vec<String> =
            nuthatch::analytics::query(nest, &lines_query(&own, &view.columns))
                .unwrap_or_else(|e| panic!("the nest's own view `{name}` did not answer: {e:#}"))
                .iter()
                .map(|r| r["line"].as_str().expect("a text line").to_string())
                .collect();
        want.sort();
        let mut got: Vec<String> = conn
            .prepare(&lines_query(&view.sql, &view.columns))
            .unwrap_or_else(|e| {
                panic!(
                    "`{name}` translated to SQL DuckDB rejects: {e}\n{}",
                    view.sql
                )
            })
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        got.sort();
        assert!(
            !want.is_empty(),
            "`{name}` answered no rows, so agreement would prove nothing"
        );
        assert_eq!(
            got, want,
            "`{name}`: the translation and the nest's view disagree\n{}",
            view.sql
        );
        let trino = view
            .sql
            .replace(&format!("dune.{SOURCE}."), "hive.nuthatch.");
        assert!(
            !trino.contains("dune."),
            "`{name}` still names Dune after the catalogue swap:\n{trino}"
        );
        eprintln!("view {name}: {} rows agree", want.len());
        out.insert(
            name,
            serde_json::json!({
                "columns": view.columns,
                "lines": want,
                "trino_sql": lines_query(&trino, &view.columns),
            }),
        );
    }

    assert_eq!(
        refused.len(),
        1,
        "only `mean_value` is refused: {refused:?}"
    );
    assert_eq!(refused[0].0, "mean_value");
    assert!(refused[0].1.contains("`avg`"), "{}", refused[0].1);
    assert_eq!(
        out.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "drift_summary",
            "sender_kinds",
            "top_blocks",
            "transfer_totals",
            "wide_values"
        ]
    );

    // Anchored to the fixture's own numbers, so two engines agreeing on a wrong answer still fails.
    let rows = 2 * ROWS_PER_SEAL;
    let plain: u128 = (0..rows).map(plain_value).sum();
    assert_eq!(
        out["transfer_totals"]["lines"],
        serde_json::json!([format!("{rows}|{plain}|10|2009")])
    );
    let fits: Vec<u128> = (0..rows)
        .map(wide_value)
        .filter(|v| *v < DECIMAL_38)
        .collect();
    let overflowed = rows as usize - fits.len();
    assert_eq!(
        out["wide_values"]["lines"],
        serde_json::json!([
            format!(
                "false|{}|{}|{}",
                fits.len(),
                fits.iter().sum::<u128>(),
                fits.len()
            ),
            format!("true|{overflowed}|<null>|0"),
        ])
    );
    out
}

/// Offline, so the drift is proven wherever the suite runs, not only where MinIO does.
#[test]
fn the_fixture_drifts_and_duckdb_reads_it_by_name() {
    sealed_fixture();
}

/// RFC-0055 S3 (#1359): every translated view returns what the nest's own view returns.
#[test]
fn every_translated_view_returns_the_rows_the_nest_view_returns() {
    let (nest, _) = sealed_fixture();
    translated_views(nest.path());
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
    let views = translated_views(nest.path());
    let report = sync(nest.path(), &target, false).await.unwrap();
    verify(nest.path(), &target, true, false).await.unwrap();

    let fixture = serde_json::json!({
        "bucket": bucket,
        "prefix": prefix,
        "dataset": report.dataset,
        "tables": expected,
        "views": views,
    });
    eprintln!("fixture: {fixture}");
    if let Ok(out) = std::env::var("NUTHATCH_TRINO_FIXTURE_OUT") {
        std::fs::write(&out, serde_json::to_vec_pretty(&fixture).unwrap()).unwrap();
    }
}
