//! `docs/reading-published-nest.md` is the contract for reading a mirror (RFC-0052 S3). This
//! publishes a nest and holds the page to what actually landed: the layout, resolution rule 0, the
//! `publish.json` field table and its `catalogue_sha256` commit check.

use std::collections::BTreeSet;
use std::path::Path;

use sha2::{Digest, Sha256};

const PAGE: &str = include_str!("../docs/reading-published-nest.md");

fn write_nest(dir: &Path) {
    std::fs::create_dir_all(dir.join("abis")).unwrap();
    std::fs::write(
        dir.join("nuthatch.toml"),
        "[nest]\nname = \"published\"\nchain = \"ethereum\"\nchain_id = 1\n\
         rpc_urls = [\"http://127.0.0.1:1\"]\nschema_version = 1\n\n\
         [[contracts]]\nalias = \"usdc\"\n\
         address = \"0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48\"\nabi = \"abis/usdc.json\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("abis/usdc.json"),
        r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]},{"type":"event","name":"Approval","anonymous":false,"inputs":[{"name":"owner","type":"address","indexed":true},{"name":"spender","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
    )
    .unwrap();
    std::fs::write(dir.join("schema.json"), r#"{"tables":[]}"#).unwrap();
}

/// Enough rows per table to clear `SEAL_TABLE_FLOOR`, so neither segment is provisional.
fn rows(first_block: u64) -> Vec<String> {
    let n = nuthatch::seal::SEAL_TABLE_FLOOR as u64;
    (0..n)
        .flat_map(|i| {
            [
                format!(
                    r#"{{"table":"usdc__transfer","from":"0xaaaa","to":"0xbbbb","value":"{}","block_number":{},"tx_hash":"0xcc","log_index":{i}}}"#,
                    i + 1,
                    first_block + i % 100
                ),
                format!(
                    r#"{{"table":"usdc__approval","owner":"0xaaaa","spender":"0xdddd","value":"{}","block_number":{},"tx_hash":"0xdd","log_index":{i}}}"#,
                    i + 2,
                    first_block + i % 100
                ),
            ]
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The field names in the page's `publish.json` table, between its two markers.
fn documented_fields() -> BTreeSet<String> {
    let start = PAGE
        .find("<!-- publish.json fields")
        .expect("the page lost its publish.json field table");
    let end = PAGE
        .find("<!-- /publish.json fields -->")
        .expect("the page lost the end of its publish.json field table");
    PAGE[start..end]
        .lines()
        .filter_map(|line| line.strip_prefix("| `"))
        .filter_map(|rest| rest.split('`').next())
        .map(str::to_string)
        .collect()
}

#[tokio::test]
async fn the_published_layout_and_envelope_match_the_page() {
    let nest = tempfile::tempdir().unwrap();
    write_nest(nest.path());
    nuthatch::seal::seal_range(nest.path(), &rows(1), 1, 100)
        .unwrap()
        .expect("sealed");
    let target = tempfile::tempdir().unwrap();

    let report = nuthatch::publish::sync(nest.path(), target.path().to_str().unwrap(), false)
        .await
        .unwrap();

    let dataset = target.path().join(&report.dataset);
    assert!(
        report.dataset.len() == 64
            && report
                .dataset
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "the page promises 64 lowercase hex characters, got {}",
        report.dataset
    );

    // Resolution rule 0: every catalogued segment is `<dataset>/<table>/<hash>.parquet`.
    assert!(PAGE.contains("`<dataset>/<t>/<h>.parquet`"));
    let manifest_bytes = std::fs::read(dataset.join("manifest.json")).unwrap();
    let local = std::fs::read(nest.path().join("segments/manifest.json")).unwrap();
    assert_eq!(
        manifest_bytes, local,
        "the page promises a byte-identical catalogue"
    );
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    let mut objects = 0;
    for (table, segments) in manifest["tables"].as_object().unwrap() {
        for seg in segments.as_array().unwrap() {
            assert_ne!(
                seg["provisional"], true,
                "a provisional entry was published"
            );
            let hash = seg["hash"].as_str().unwrap();
            let object = dataset.join(table).join(format!("{hash}.parquet"));
            assert!(
                object.is_file(),
                "rule 0 names {} but it is absent",
                object.display()
            );
            objects += 1;
        }
    }
    assert_eq!(objects, 2, "both tables should have published one segment");

    // The field table lists exactly what `publish.json` carries.
    let envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dataset.join("publish.json")).unwrap()).unwrap();
    let written: BTreeSet<String> = envelope.as_object().unwrap().keys().cloned().collect();
    assert_eq!(written, documented_fields());

    // The commit check a reader is told to make holds on a finished publish.
    assert_eq!(
        envelope["catalogue_sha256"].as_str().unwrap(),
        sha256_hex(&manifest_bytes)
    );
    assert_eq!(
        envelope["schema_sha256"].as_str().unwrap(),
        sha256_hex(&std::fs::read(dataset.join("schema.json")).unwrap())
    );
    assert_eq!(envelope["data_identity"].as_str().unwrap(), report.dataset);
    assert_eq!(envelope["layout_version"], 1);
}

/// The page's resolver, run as written against a published directory.
fn resolver_snippet() -> &'static str {
    let section = &PAGE[PAGE
        .find("## A resolver")
        .expect("the page lost its resolver")..];
    let body = &section[section
        .find("```sh\n")
        .expect("the resolver lost its sh block")
        + 6..];
    &body[..body
        .find("```")
        .expect("the resolver block is unterminated")]
}

#[tokio::test]
async fn the_pages_resolver_lists_every_segment_in_block_order() {
    let nest = tempfile::tempdir().unwrap();
    write_nest(nest.path());
    nuthatch::seal::seal_range(nest.path(), &rows(1), 1, 100)
        .unwrap()
        .expect("sealed");
    nuthatch::seal::seal_range(nest.path(), &rows(101), 101, 200)
        .unwrap()
        .expect("sealed");
    let target = tempfile::tempdir().unwrap();
    let report = nuthatch::publish::sync(nest.path(), target.path().to_str().unwrap(), false)
        .await
        .unwrap();
    let dataset = target.path().join(&report.dataset);

    let manifest = nuthatch::seal::load_manifest(nest.path()).unwrap();
    let mut segs = manifest.tables["usdc__transfer"].clone();
    segs.sort_by(|a, b| {
        (a.from_block, a.to_block, &a.hash).cmp(&(b.from_block, b.to_block, &b.hash))
    });
    assert_eq!(
        segs.len(),
        2,
        "the fixture should seal two transfer segments"
    );
    let expected: Vec<String> = segs
        .iter()
        .map(|s| format!("{}/usdc__transfer/{}.parquet", dataset.display(), s.hash))
        .collect();

    let out = std::process::Command::new("bash")
        .arg("-euo")
        .arg("pipefail")
        .arg("-c")
        .arg(resolver_snippet())
        .env("DATASET", &dataset)
        .env("TABLE", "usdc__transfer")
        .output()
        .expect("bash, jq and shasum are needed to run the page's resolver");
    assert!(
        out.status.success(),
        "the resolver failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let listed: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(listed, expected);
}
