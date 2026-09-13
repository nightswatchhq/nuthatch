//! #1343: RFC-0052 S1's acceptance criterion was "reconciler over FsStore **and S3 (MinIO in
//! CI)**", but #1335 shipped the S3 mirror (`ObjMirror::from_locator` for `s3://`) with every sync
//! test in `src/publish.rs` run against `memory://` and no CI job starting a real object store. S3
//! publishing had never touched a real S3-compatible endpoint. This is that run, gated so it needs
//! a live MinIO and stays offline (and green) everywhere else.
//!
//! Checks the three things the closed S1 acceptance actually asked for:
//!
//!   1. the remote `manifest.json` is byte-identical to the local sealed catalogue;
//!   2. for every table, `count(*)` and `sum(value)` over the object fetched from the bucket match
//!      the same query over the local sealed segment;
//!   3. a second `sync` puts nothing but `publish.json`.
//!
//! ## Why not DuckDB httpfs
//!
//! `httpfs` is not statically linked into this binary's bundled DuckDB, and RFC-0045 holds that it
//! must not be: loading it would mean fetching an extension at runtime (the exact phone-home
//! `tests/duckdb_extensions_are_static.rs` guards against) and would put an SSRF surface behind
//! every `/sql` query. So table 2's check does not point DuckDB at `s3://` directly - it fetches
//! each table's object from the bucket with the same `object_store` S3 client `ObjMirror` uses
//! (independent of `publish::verify`'s own re-download below), writes it to a local temp file, and
//! hands *that* to nuthatch's ordinary bundled DuckDB via `read_parquet`. That still proves the
//! object landed in the bucket intact and that DuckDB parses it identically to the local copy,
//! without asking the shipped binary for a capability the RFC forbids it.
//!
//! ## Running it
//!
//! Needs a MinIO (or other S3-compatible store) with an existing bucket:
//!
//! ```sh
//! docker run -d -p 9000:9000 -e MINIO_ROOT_USER=nuthatch -e MINIO_ROOT_PASSWORD=nuthatch-minio \
//!   minio/minio:RELEASE.2025-10-15T17-29-55Z server /data
//! aws --endpoint-url http://127.0.0.1:9000 s3 mb s3://nuthatch-publish-test
//! NUTHATCH_MINIO_ENDPOINT=http://127.0.0.1:9000 AWS_ENDPOINT=http://127.0.0.1:9000 \
//!   AWS_ACCESS_KEY_ID=nuthatch AWS_SECRET_ACCESS_KEY=nuthatch-minio AWS_REGION=us-east-1 \
//!   AWS_ALLOW_HTTP=true cargo test --test e2e_minio_publish
//! ```
//!
//! Without `NUTHATCH_MINIO_ENDPOINT` the suite skips - a laptop convenience and a CI lie, so CI
//! sets `NUTHATCH_REQUIRE_MINIO=1`, turning a missing endpoint into a hard failure there.

#![cfg(feature = "object-store")]

use std::path::Path;

use nuthatch::publish::{sync, verify};
use nuthatch::registry::{DecodedRow, Value as DecodedValue};
use nuthatch::seal::{load_manifest, seal_range, segment_path, MANIFEST_FILE, SEGMENTS_DIR};

/// Above `SEAL_TABLE_FLOOR` (1,000), so both tables' segments are final rather than provisional
/// and both are published - RFC-0052 S1 only mirrors sealed, non-provisional segments.
const ROWS_PER_TABLE: u64 = 1_000;

fn endpoint() -> Option<String> {
    match std::env::var("NUTHATCH_MINIO_ENDPOINT") {
        Ok(u) => Some(u),
        Err(_) if std::env::var("NUTHATCH_REQUIRE_MINIO").is_ok() => panic!(
            "NUTHATCH_REQUIRE_MINIO is set but NUTHATCH_MINIO_ENDPOINT is not - this suite would \
             have silently skipped, which is the exact gap #1343 exists to close"
        ),
        Err(_) => {
            eprintln!(
                "SKIPPED: set NUTHATCH_MINIO_ENDPOINT (plus AWS_* env, incl. AWS_ALLOW_HTTP=true) \
                 to run the MinIO publish suite"
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

fn row(table: &str, block: u64, log_index: u64, value: u64) -> String {
    DecodedRow {
        table: table.into(),
        params: vec![("value".into(), DecodedValue::U64(value))],
        block_number: block,
        block_hash: format!("0x{block:064x}"),
        block_timestamp: 1_700_000_000 + block,
        timestamps: true,
        log_index,
        tx_hash: format!("0x{block:064x}"),
        address: "0xaa".into(),
    }
    .to_json()
    .to_string()
}

/// Two tables, each `ROWS_PER_TABLE` rows sealed in one call, so both land as final segments in one
/// manifest write.
fn two_table_nest() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_nest(dir.path());
    let mut rows = Vec::with_capacity(2 * ROWS_PER_TABLE as usize);
    for i in 0..ROWS_PER_TABLE {
        rows.push(row("usdc__transfer", 10 + i, 0, i + 1));
        rows.push(row("usdc__approval", 10 + i, 1, (i + 1) * 3));
    }
    seal_range(dir.path(), &rows, 10, 9 + ROWS_PER_TABLE)
        .unwrap()
        .expect("sealed");
    dir
}

/// `count(*)` and `sum(value)` for a local Parquet file via nuthatch's own bundled DuckDB - no
/// extensions, no network. `value` is stored as text (every non-block/log/seq column is, per
/// `seal::rows_to_batch`), hence the cast.
fn count_and_sum(path: &Path) -> (i64, i64) {
    let conn = duckdb::Connection::open_in_memory().expect("open duckdb");
    conn.prepare(&format!(
        "SELECT count(*), sum(TRY_CAST(value AS BIGINT)) FROM read_parquet('{}')",
        path.display()
    ))
    .and_then(|mut s| s.query_row([], |r| Ok((r.get(0)?, r.get(1)?))))
    .expect("count/sum over a sealed segment")
}

/// Fetch one object's raw bytes from the bucket with a fresh `object_store` S3 client, built the
/// same way `ObjMirror::from_locator` builds its own from the `AWS_*` env - a second, independent
/// path to the same object, not a rerun of `publish::verify`'s.
async fn fetch(bucket: &str, key: &str) -> Vec<u8> {
    use object_store::{path::Path as ObjPath, ObjectStore};
    let url = url::Url::parse(&format!("s3://{bucket}")).expect("parse bucket url");
    let opts = std::env::vars().map(|(k, v)| (k.to_ascii_lowercase(), v));
    let (store, _) = object_store::parse_url_opts(&url, opts).expect("open S3 client");
    let got = store
        .get(&ObjPath::from(key))
        .await
        .unwrap_or_else(|e| panic!("GET {key}: {e}"));
    got.bytes().await.expect("read object bytes").to_vec()
}

#[tokio::test]
async fn sync_and_verify_against_a_real_s3_compatible_store() {
    let Some(_) = endpoint() else { return };
    let bucket = bucket();
    // Unique per run: a rerun against a bucket that already holds a prior run's prefix must not
    // observe that prior run's objects as "already there".
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prefix = format!("e2e-minio-publish/{}-{nanos}", std::process::id());
    let target = format!("s3://{bucket}/{prefix}");

    let nest = two_table_nest();
    let local_manifest = load_manifest(nest.path()).unwrap();
    assert_eq!(
        local_manifest.tables.len(),
        2,
        "premise: two tables must have sealed, got {:?}",
        local_manifest.tables.keys().collect::<Vec<_>>()
    );
    for (table, segs) in &local_manifest.tables {
        assert!(
            segs.iter().all(|s| !s.provisional),
            "premise: {table}'s segment must be final, not provisional"
        );
    }

    let first = sync(nest.path(), &target, false).await.unwrap();
    assert_eq!(
        first
            .uploaded
            .iter()
            .filter(|k| k.ends_with(".parquet"))
            .count(),
        2,
        "expected one segment per table to upload, got {:?}",
        first.uploaded
    );
    assert!(first.uploaded.iter().any(|k| k == MANIFEST_FILE));
    assert!(first.uploaded.iter().any(|k| k == "publish.json"));
    let data_identity = first.dataset.clone();

    // `nuthatch publish verify --deep`: re-downloads and re-hashes every segment, and checks the
    // remote manifest.json is byte-identical to the local one along with everything else the
    // envelope claims (nid, chain_id, tables, schema). This is criterion 1 in full, run through the
    // actual CLI-backing function rather than reimplemented here.
    verify(nest.path(), &target, true).await.unwrap();

    // Criterion 1, asserted directly and independently of `verify`'s own comparison: the exact
    // bytes `sync` wrote for the catalogue, fetched back over the wire.
    let local_manifest_bytes =
        std::fs::read(nest.path().join(SEGMENTS_DIR).join(MANIFEST_FILE)).unwrap();
    let remote_manifest_bytes = fetch(
        &bucket,
        &format!("{prefix}/{data_identity}/{MANIFEST_FILE}"),
    )
    .await;
    assert_eq!(
        remote_manifest_bytes, local_manifest_bytes,
        "remote manifest.json must be byte-identical to the local sealed catalogue"
    );

    // Criterion 2: per table, DuckDB's count(*)/sum(value) over the bucket's copy must match the
    // same query over the local sealed segment.
    for (table, segs) in &local_manifest.tables {
        let seg = &segs[0];
        let local_path = segment_path(nest.path(), &seg.file, &seg.hash);
        let local = count_and_sum(&local_path);
        assert_eq!(
            local.0, ROWS_PER_TABLE as i64,
            "{table}: local row count drifted from the fixture"
        );

        let remote_key = format!("{prefix}/{data_identity}/{table}/{}.parquet", seg.hash);
        let remote_bytes = fetch(&bucket, &remote_key).await;
        let tmp = tempfile::tempdir().unwrap();
        let remote_path = tmp.path().join("remote.parquet");
        std::fs::write(&remote_path, &remote_bytes).unwrap();
        let remote = count_and_sum(&remote_path);

        assert_eq!(
            remote, local,
            "{table}: (count, sum) over the bucket does not match the local sealed rows - \
             remote {remote:?}, local {local:?}"
        );
    }

    // Criterion 3: nothing changed locally or remotely, so a second sync must put only the
    // envelope (its `published_at` always moves). Same observation `a_second_sync_only_puts_publish_json`
    // and `a_second_memory_sync_only_puts_publish_json` make in `src/publish.rs`, now against a live
    // S3-compatible store rather than a tempdir or an in-process memory store.
    let second = sync(nest.path(), &target, false).await.unwrap();
    assert_eq!(
        second.uploaded,
        vec!["publish.json".to_string()],
        "a second sync against MinIO must upload nothing but the envelope"
    );
}
