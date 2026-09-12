//! RFC-0052 S1: mirror sealed, non-provisional segments to a prefix.
//!
//! The publisher is a level-triggered reconciler: local catalogue versus remote catalogue.
//! Parquet goes first, then schema, then the catalogue (conditional), then `publish.json`.
//! `BundleStore` is not reused; this talks filesystem paths or `object_store` directly.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::seal::{self, Manifest, Segment, MANIFEST_FILE};

/// What one `sync` actually wrote, so a second run can assert it only touched `publish.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub dataset: String,
    pub sealed_through: Option<u64>,
    pub uploaded: Vec<String>,
    pub skipped: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct PublishEnvelope {
    layout_version: u32,
    nuthatch_version: String,
    chain_id: u64,
    data_identity: String,
    nid: String,
    bundle_hash: String,
    sealed_through: Option<u64>,
    published_at: String,
    catalogue_sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema_sha256: Option<String>,
    tables: Vec<String>,
}

#[async_trait]
trait Mirror: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    async fn put_file(&self, key: &str, src: &Path) -> Result<()>;
    /// Create if `expected` is `None`; otherwise replace only when the current bytes match.
    async fn put_if(&self, key: &str, bytes: &[u8], expected: Option<&[u8]>) -> Result<()>;
    async fn head_size(&self, key: &str) -> Result<Option<u64>>;
}

struct FsMirror {
    root: PathBuf,
}

impl FsMirror {
    fn path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }
}

#[async_trait]
impl Mirror for FsMirror {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(key)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", key)),
        }
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", key))
    }

    async fn put_file(&self, key: &str, src: &Path) -> Result<()> {
        let path = self.path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::copy(src, &path)
            .with_context(|| format!("copying {} to {}", src.display(), key))?;
        Ok(())
    }

    async fn put_if(&self, key: &str, bytes: &[u8], expected: Option<&[u8]>) -> Result<()> {
        let current = self.get(key).await?;
        match (expected, current.as_deref()) {
            (None, Some(_)) => bail!(
                "refusing to publish {key}: it already exists (another publisher holds this dataset)"
            ),
            (Some(exp), Some(got)) if exp != got => bail!(
                "refusing to publish {key}: the remote catalogue changed since this run started"
            ),
            (Some(_), None) => bail!(
                "refusing to publish {key}: expected an existing catalogue and found none"
            ),
            _ => self.put(key, bytes).await,
        }
    }

    async fn head_size(&self, key: &str) -> Result<Option<u64>> {
        match std::fs::metadata(self.path(key)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("stat {}", key)),
        }
    }
}

#[cfg(feature = "object-store")]
struct ObjMirror {
    inner: std::sync::Arc<dyn object_store::ObjectStore>,
    prefix: object_store::path::Path,
}

#[cfg(feature = "object-store")]
impl ObjMirror {
    fn from_locator(locator: &str) -> Result<Self> {
        use object_store::path::Path as ObjPath;
        if let Some(rest) = locator.strip_prefix("memory://") {
            return Ok(Self {
                inner: memory_store(locator),
                prefix: ObjPath::from(rest.trim_start_matches('/')),
            });
        }
        let url = url::Url::parse(locator)
            .with_context(|| format!("parsing publish target {locator:?}"))?;
        let opts = std::env::vars().map(|(k, v)| (k.to_ascii_lowercase(), v));
        let (store, path) = object_store::parse_url_opts(&url, opts)
            .with_context(|| format!("opening publish target {locator:?}"))?;
        Ok(Self {
            inner: std::sync::Arc::from(store),
            prefix: path,
        })
    }

    fn key(&self, key: &str) -> object_store::path::Path {
        let mut p = self.prefix.clone();
        for part in key.split('/') {
            if !part.is_empty() {
                p = p.child(part);
            }
        }
        p
    }
}

#[cfg(feature = "object-store")]
#[async_trait]
impl Mirror for ObjMirror {
    async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        use object_store::ObjectStore as _;
        match self.inner.get(&self.key(key)).await {
            Ok(g) => Ok(Some(g.bytes().await.context("reading object")?.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(key.to_string())),
        }
    }

    async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        use object_store::ObjectStore as _;
        self.inner
            .put(
                &self.key(key),
                object_store::PutPayload::from(bytes.to_vec()),
            )
            .await
            .with_context(|| format!("putting {key}"))?;
        Ok(())
    }

    async fn put_file(&self, key: &str, src: &Path) -> Result<()> {
        let bytes = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
        self.put(key, &bytes).await
    }

    async fn put_if(&self, key: &str, bytes: &[u8], expected: Option<&[u8]>) -> Result<()> {
        let current = self.get(key).await?;
        match (expected, current.as_deref()) {
            (None, Some(_)) => bail!(
                "refusing to publish {key}: it already exists (another publisher holds this dataset)"
            ),
            (Some(exp), Some(got)) if exp != got => bail!(
                "refusing to publish {key}: the remote catalogue changed since this run started"
            ),
            (Some(_), None) => bail!(
                "refusing to publish {key}: expected an existing catalogue and found none"
            ),
            _ => self.put(key, bytes).await,
        }
    }

    async fn head_size(&self, key: &str) -> Result<Option<u64>> {
        use object_store::ObjectStore as _;
        match self.inner.head(&self.key(key)).await {
            Ok(m) => Ok(Some(m.size as u64)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(key.to_string())),
        }
    }
}

#[cfg(feature = "object-store")]
fn memory_store(locator: &str) -> std::sync::Arc<dyn object_store::ObjectStore> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static STORES: OnceLock<Mutex<HashMap<String, Arc<object_store::memory::InMemory>>>> =
        OnceLock::new();
    let mut map = STORES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("memory publish store");
    map.entry(locator.to_string())
        .or_insert_with(|| Arc::new(object_store::memory::InMemory::new()))
        .clone()
}

fn open_mirror(target: &str) -> Result<Box<dyn Mirror>> {
    let t = target.trim();
    if t.starts_with("s3://") || t.starts_with("memory://") {
        #[cfg(feature = "object-store")]
        {
            return Ok(Box::new(ObjMirror::from_locator(t)?));
        }
        #[cfg(not(feature = "object-store"))]
        {
            bail!(
                "object-store publish targets need a build with `--features object-store` \
                 (the default binary has this)"
            );
        }
    }
    Ok(Box::new(FsMirror {
        root: PathBuf::from(t),
    }))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn parquet_key(table: &str, hash: &str) -> String {
    format!("{table}/{hash}.parquet")
}

fn want_entries(manifest: &Manifest) -> Vec<(String, &Segment)> {
    let mut out = Vec::new();
    for (table, segs) in &manifest.tables {
        for s in segs {
            if !s.provisional {
                out.push((table.clone(), s));
            }
        }
    }
    out.sort_by(|a, b| {
        (a.1.from_block, a.1.to_block, &a.1.hash).cmp(&(b.1.from_block, b.1.to_block, &b.1.hash))
    });
    out
}

fn remote_have(remote: &Manifest) -> BTreeSet<(String, String)> {
    remote
        .tables
        .iter()
        .flat_map(|(t, segs)| {
            segs.iter()
                .filter(|s| !s.provisional)
                .map(move |s| (t.clone(), s.hash.clone()))
        })
        .collect()
}

fn identity_of(dir: &Path) -> Result<(String, String, String, u64)> {
    let m = crate::blob::build_manifest(dir, None)?;
    let cfg = crate::config::Config::load(dir)?;
    Ok((m.data_identity(), m.nid(), m.blob_hash(), cfg.nest.chain_id))
}

fn rfc3339_utc(secs: u64) -> String {
    let z = secs as i64;
    let days = z.div_euclid(86400);
    let rem = z.rem_euclid(86400) as u64;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{hour:02}:{min:02}:{sec:02}Z")
}

fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

/// Reconcile `dir`'s sealed catalogue onto `target/<data_identity>/`.
pub async fn sync(dir: &Path, target: &str, dry_run: bool) -> Result<SyncReport> {
    let local_path = dir.join(seal::SEGMENTS_DIR).join(MANIFEST_FILE);
    let local_bytes = match std::fs::read(&local_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::to_vec(&Manifest::default())?
        }
        Err(e) => return Err(e).context("reading local catalogue"),
    };
    let local: Manifest =
        serde_json::from_slice(&local_bytes).context("corrupt local catalogue")?;
    let (data_identity, nid, bundle_hash, chain_id) = identity_of(dir)?;
    let mirror = open_mirror(target)?;
    let prefix = |k: &str| format!("{data_identity}/{k}");

    let remote_bytes = mirror.get(&prefix(MANIFEST_FILE)).await?;
    let remote: Manifest = match &remote_bytes {
        Some(b) => serde_json::from_slice(b).context("corrupt remote catalogue")?,
        None => Manifest::default(),
    };
    let have = remote_have(&remote);
    let want = want_entries(&local);
    let skipped = want
        .iter()
        .filter(|(t, s)| have.contains(&(t.clone(), s.hash.clone())))
        .count();
    let missing: Vec<(String, &Segment)> = want
        .iter()
        .filter(|(t, s)| !have.contains(&(t.clone(), s.hash.clone())))
        .cloned()
        .collect();

    if dry_run {
        return Ok(SyncReport {
            dataset: data_identity,
            sealed_through: want.iter().map(|(_, s)| s.to_block).max(),
            uploaded: missing
                .iter()
                .map(|(t, s)| parquet_key(t, &s.hash))
                .collect(),
            skipped,
        });
    }

    let mut uploaded = Vec::new();
    for (table, seg) in &missing {
        let src = seal::segment_path(dir, &seg.file, &seg.hash);
        if !src.exists() {
            bail!(
                "catalogue names {} but the file is missing at {}",
                seg.file,
                src.display()
            );
        }
        let key = prefix(&parquet_key(table, &seg.hash));
        mirror.put_file(&key, &src).await?;
        uploaded.push(parquet_key(table, &seg.hash));
    }

    let schema_path = dir.join("schema.json");
    let schema_bytes = std::fs::read(&schema_path).ok();
    let schema_sha = schema_bytes.as_ref().map(|b| sha256_hex(b));
    if let Some(bytes) = &schema_bytes {
        let remote_schema = mirror.get(&prefix("schema.json")).await?;
        if remote_schema.as_deref() != Some(bytes.as_slice()) {
            mirror.put(&prefix("schema.json"), bytes).await?;
            uploaded.push("schema.json".into());
        }
    }

    let cat_key = prefix(MANIFEST_FILE);
    if remote_bytes.as_deref() != Some(local_bytes.as_slice()) {
        mirror
            .put_if(&cat_key, &local_bytes, remote_bytes.as_deref())
            .await?;
        uploaded.push(MANIFEST_FILE.into());
    }

    let sealed_through = want.iter().map(|(_, s)| s.to_block).max();
    let tables: Vec<String> = local.tables.keys().cloned().collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let envelope = PublishEnvelope {
        layout_version: 1,
        nuthatch_version: env!("CARGO_PKG_VERSION").to_string(),
        chain_id,
        data_identity: data_identity.clone(),
        nid,
        bundle_hash,
        sealed_through,
        published_at: rfc3339_utc(now),
        catalogue_sha256: sha256_hex(&local_bytes),
        schema_sha256: schema_sha,
        tables,
    };
    let env_bytes = serde_json::to_vec_pretty(&envelope)?;
    mirror.put(&prefix("publish.json"), &env_bytes).await?;
    uploaded.push("publish.json".into());

    Ok(SyncReport {
        dataset: data_identity,
        sealed_through,
        uploaded,
        skipped,
    })
}

/// HEAD every published file. `--deep` re-hashes parquet against the local bytes.
pub async fn verify(dir: &Path, target: &str, deep: bool) -> Result<()> {
    let local = seal::load_manifest(dir)?;
    let (data_identity, _, _, _) = identity_of(dir)?;
    let mirror = open_mirror(target)?;
    let prefix = |k: &str| format!("{data_identity}/{k}");
    let remote_bytes = mirror
        .get(&prefix(MANIFEST_FILE))
        .await?
        .context("remote catalogue missing")?;
    let local_bytes = std::fs::read(dir.join(seal::SEGMENTS_DIR).join(MANIFEST_FILE))
        .context("reading local catalogue")?;
    if remote_bytes != local_bytes {
        bail!("remote catalogue is not byte-identical to local");
    }
    let env_bytes = mirror
        .get(&prefix("publish.json"))
        .await?
        .context("remote publish.json missing")?;
    let env: PublishEnvelope =
        serde_json::from_slice(&env_bytes).context("corrupt remote publish.json")?;
    let cat_sha = sha256_hex(&local_bytes);
    if env.catalogue_sha256 != cat_sha {
        bail!("publish.json catalogue_sha256 does not match the catalogue");
    }
    if let Ok(local_schema) = std::fs::read(dir.join("schema.json")) {
        let remote_schema = mirror
            .get(&prefix("schema.json"))
            .await?
            .context("remote schema.json missing")?;
        if remote_schema != local_schema {
            bail!("schema.json does not match local");
        }
        let schema_sha = sha256_hex(&local_schema);
        if env.schema_sha256.as_deref() != Some(schema_sha.as_str()) {
            bail!("publish.json schema_sha256 does not match schema.json");
        }
    }
    for (table, seg) in want_entries(&local) {
        let key = prefix(&parquet_key(&table, &seg.hash));
        let size = mirror
            .head_size(&key)
            .await?
            .with_context(|| format!("missing {key}"))?;
        let src = seal::segment_path(dir, &seg.file, &seg.hash);
        let local_size = std::fs::metadata(&src)
            .with_context(|| format!("stat {}", src.display()))?
            .len();
        if size != local_size {
            bail!("{key} is {size} bytes remotely, {local_size} locally");
        }
        if deep {
            let remote = mirror
                .get(&key)
                .await?
                .with_context(|| format!("downloading {key}"))?;
            let local_file = std::fs::read(&src)?;
            if sha256_hex(&remote) != sha256_hex(&local_file) {
                bail!("{key} hash does not match the local segment");
            }
        }
    }
    Ok(())
}

/// `nuthatch publish sync`.
pub async fn run_sync(dir: &Path, target: &str, dry_run: bool) -> Result<()> {
    let report = sync(dir, target, dry_run).await?;
    let verb = if dry_run { "would upload" } else { "uploaded" };
    println!(
        "dataset {} sealed_through {:?} {verb} {} skipped {}",
        report.dataset,
        report.sealed_through,
        report.uploaded.len(),
        report.skipped
    );
    Ok(())
}

/// `nuthatch publish verify`.
pub async fn run_verify(dir: &Path, target: &str, deep: bool) -> Result<()> {
    verify(dir, target, deep).await?;
    println!("ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{DecodedRow, Value as DecodedValue};
    use crate::seal::{seal_range, test_set_table_floor};
    use serde_json::json;

    fn row(block: u64, value: u64) -> String {
        DecodedRow {
            table: "usdc__transfer".into(),
            params: vec![(
                "value".into(),
                DecodedValue::Word32(alloy_primitives::U256::from(value).to_be_bytes::<32>()),
            )],
            block_number: block,
            block_hash: "0xbh".into(),
            block_timestamp: 1_700_000_000 + block,
            timestamps: true,
            log_index: 0,
            tx_hash: "0xtx".into(),
            address: "0xaa".into(),
        }
        .to_json()
        .to_string()
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
            serde_json::to_string(&json!({"tables":[]})).unwrap(),
        )
        .unwrap();
    }

    fn sealed_nest() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_nest(dir.path());
        test_set_table_floor(dir.path(), 0);
        seal_range(dir.path(), &[row(10, 5), row(11, 7)], 10, 11)
            .unwrap()
            .expect("sealed");
        dir
    }

    fn duck_count(glob: &str) -> i64 {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let sql = format!("SELECT count(*) FROM read_parquet('{glob}', union_by_name=true)");
        conn.query_row(&sql, [], |r| r.get(0)).unwrap()
    }

    fn duck_sum_dec(glob: &str) -> String {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let sql = format!(
            "SELECT CAST(sum(TRY_CAST(value AS DECIMAL(38,0))) AS VARCHAR) \
             FROM read_parquet('{glob}', union_by_name=true)"
        );
        conn.query_row(&sql, [], |r| r.get::<_, String>(0)).unwrap()
    }

    #[tokio::test]
    async fn sync_makes_the_remote_catalogue_byte_identical_and_the_rows_match() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let report = sync(nest.path(), mirror.path().to_str().unwrap(), false)
            .await
            .unwrap();
        assert!(report.uploaded.iter().any(|k| k.ends_with(".parquet")));
        assert!(report.uploaded.iter().any(|k| k == MANIFEST_FILE));
        assert!(report.uploaded.iter().any(|k| k == "publish.json"));

        let local =
            std::fs::read(nest.path().join(seal::SEGMENTS_DIR).join(MANIFEST_FILE)).unwrap();
        let remote =
            std::fs::read(mirror.path().join(&report.dataset).join(MANIFEST_FILE)).unwrap();
        assert_eq!(remote, local, "remote catalogue must be the local bytes");

        let local_glob = nest
            .path()
            .join(seal::SEGMENTS_DIR)
            .join("*.parquet")
            .display()
            .to_string();
        let remote_glob = mirror
            .path()
            .join(&report.dataset)
            .join("usdc__transfer")
            .join("*.parquet")
            .display()
            .to_string();
        assert_eq!(duck_count(&local_glob), duck_count(&remote_glob));
        assert_eq!(duck_sum_dec(&local_glob), duck_sum_dec(&remote_glob));
    }

    #[tokio::test]
    async fn a_second_sync_only_puts_publish_json() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap();
        sync(nest.path(), target, false).await.unwrap();
        let second = sync(nest.path(), target, false).await.unwrap();
        assert_eq!(
            second.uploaded,
            vec!["publish.json".to_string()],
            "idempotent: only the envelope, whose published_at moves, is rewritten"
        );
    }

    #[tokio::test]
    async fn provisional_segments_are_not_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        write_nest(dir.path());
        // Default floor is 1000; one row is provisional.
        seal_range(dir.path(), &[row(1, 1)], 1, 1)
            .unwrap()
            .expect("sealed");
        let manifest = seal::load_manifest(dir.path()).unwrap();
        assert!(
            manifest.tables["usdc__transfer"][0].provisional,
            "premise: the sealed row is under the floor"
        );
        let mirror = tempfile::tempdir().unwrap();
        let report = sync(dir.path(), mirror.path().to_str().unwrap(), false)
            .await
            .unwrap();
        assert!(
            !report.uploaded.iter().any(|k| k.ends_with(".parquet")),
            "a provisional segment must not be published"
        );
        verify(dir.path(), mirror.path().to_str().unwrap(), false)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn verify_accepts_a_faithful_mirror() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap();
        sync(nest.path(), target, false).await.unwrap();
        verify(nest.path(), target, true).await.unwrap();
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let report = sync(nest.path(), mirror.path().to_str().unwrap(), true)
            .await
            .unwrap();
        assert!(!report.uploaded.is_empty());
        assert!(
            std::fs::read_dir(mirror.path()).unwrap().next().is_none(),
            "dry-run must not create the prefix"
        );
    }

    #[tokio::test]
    async fn verify_fails_if_publish_json_is_gone() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap();
        let report = sync(nest.path(), target, false).await.unwrap();
        std::fs::remove_file(mirror.path().join(&report.dataset).join("publish.json")).unwrap();
        let err = verify(nest.path(), target, false).await.unwrap_err();
        assert!(
            err.to_string().contains("publish.json"),
            "wanted a publish.json failure, got {err}"
        );
    }

    #[tokio::test]
    async fn verify_fails_if_schema_json_is_gone() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap();
        let report = sync(nest.path(), target, false).await.unwrap();
        std::fs::remove_file(mirror.path().join(&report.dataset).join("schema.json")).unwrap();
        let err = verify(nest.path(), target, false).await.unwrap_err();
        assert!(
            err.to_string().contains("schema.json"),
            "wanted a schema.json failure, got {err}"
        );
    }

    #[cfg(feature = "object-store")]
    #[tokio::test]
    async fn a_second_memory_sync_only_puts_publish_json() {
        let nest = sealed_nest();
        let target = "memory://s1-idempotent";
        sync(nest.path(), target, false).await.unwrap();
        let second = sync(nest.path(), target, false).await.unwrap();
        assert_eq!(second.uploaded, vec!["publish.json".to_string()]);
        verify(nest.path(), target, true).await.unwrap();
    }

    #[cfg(feature = "object-store")]
    #[tokio::test]
    async fn object_store_put_if_refuses_a_replaced_catalogue() {
        let m = ObjMirror::from_locator("memory://s1-put-if").unwrap();
        m.put("manifest.json", b"old").await.unwrap();
        let err = m
            .put_if("manifest.json", b"new", Some(b"stale"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("changed"),
            "wanted a changed-catalogue refusal, got {err}"
        );
    }
}
