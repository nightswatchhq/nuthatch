//! RFC-0052 S1: mirror sealed, non-provisional segments to a prefix.
//!
//! The publisher is a level-triggered reconciler: local catalogue versus remote catalogue.
//! Parquet goes first, then schema, then the catalogue (conditional), then `publish.json`.
//! `BundleStore` is not reused; this talks filesystem paths or `object_store` directly.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::seal::{self, Manifest, Segment, MANIFEST_FILE};

/// What one `sync` actually wrote, so a second run can assert it only touched `publish.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncReport {
    pub dataset: String,
    pub sealed_through: Option<u64>,
    pub uploaded: Vec<String>,
    pub uploaded_bytes: u64,
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
        let path = self.path(key);
        let lock = publication_lock(&format!("fs:{}", path.display()));
        let _g = lock.lock().await;
        let _cross = fs_exclusive_lock(&path)?;
        cas_bytes(self.get(key).await?, expected, key)?;
        self.put(key, bytes).await
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
        use object_store::{ObjectStore as _, PutMode, PutOptions, UpdateVersion};
        let lock = publication_lock(&format!("obj:{}:{key}", self.prefix));
        let _g = lock.lock().await;
        let loc = self.key(key);
        match expected {
            None => self
                .inner
                .put_opts(
                    &loc,
                    object_store::PutPayload::from(bytes.to_vec()),
                    PutOptions {
                        mode: PutMode::Create,
                        ..PutOptions::default()
                    },
                )
                .await
                .map_err(|e| match e {
                    object_store::Error::AlreadyExists { .. } => anyhow::anyhow!(
                        "refusing to publish {key}: it already exists (another publisher holds this dataset)"
                    ),
                    other => anyhow::Error::new(other).context(key.to_string()),
                })
                .map(|_| ()),
            Some(exp) => {
                let got = self.inner.get(&loc).await.map_err(|e| match e {
                    object_store::Error::NotFound { .. } => anyhow::anyhow!(
                        "refusing to publish {key}: expected an existing catalogue and found none"
                    ),
                    other => anyhow::Error::new(other).context(key.to_string()),
                })?;
                let etag = got.meta.e_tag.clone();
                let version = got.meta.version.clone();
                let body = got.bytes().await.context("reading catalogue for CAS")?;
                if body.as_ref() != exp {
                    bail!(
                        "refusing to publish {key}: the remote catalogue changed since this run started"
                    );
                }
                self.inner
                    .put_opts(
                        &loc,
                        object_store::PutPayload::from(bytes.to_vec()),
                        PutOptions {
                            mode: PutMode::Update(UpdateVersion {
                                e_tag: etag,
                                version,
                            }),
                            ..PutOptions::default()
                        },
                    )
                    .await
                    .map_err(|e| match e {
                        object_store::Error::Precondition { .. } => anyhow::anyhow!(
                            "refusing to publish {key}: the remote catalogue changed since this run started"
                        ),
                        other => anyhow::Error::new(other).context(key.to_string()),
                    })
                    .map(|_| ())
            }
        }
    }

    async fn head_size(&self, key: &str) -> Result<Option<u64>> {
        use object_store::ObjectStore as _;
        match self.inner.head(&self.key(key)).await {
            Ok(m) => Ok(Some(m.size)),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(key.to_string())),
        }
    }
}

fn publication_lock(id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("publish locks");
    map.entry(id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// Exclusive `flock` on `path.lock`, so two `nuthatch publish sync` processes cannot
/// both pass the catalogue CAS. Dropping the file releases the lock.
struct FsExclusiveLock {
    _file: std::fs::File,
}

fn fs_exclusive_lock(path: &Path) -> Result<FsExclusiveLock> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc != 0 {
            bail!("flock {} failed", lock_path.display());
        }
    }
    Ok(FsExclusiveLock { _file: file })
}

fn cas_bytes(current: Option<Vec<u8>>, expected: Option<&[u8]>, key: &str) -> Result<()> {
    match (expected, current.as_deref()) {
        (None, Some(_)) => bail!(
            "refusing to publish {key}: it already exists (another publisher holds this dataset)"
        ),
        (Some(exp), Some(got)) if exp != got => {
            bail!("refusing to publish {key}: the remote catalogue changed since this run started")
        }
        (Some(_), None) => {
            bail!("refusing to publish {key}: expected an existing catalogue and found none")
        }
        _ => Ok(()),
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
    #[cfg(test)]
    if let Some(name) = t.strip_prefix("test://") {
        return tests::test_mirror(name);
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

async fn remote_present(
    mirror: &dyn Mirror,
    prefix: &str,
    remote: &Manifest,
) -> Result<BTreeSet<(String, String)>> {
    let mut have = BTreeSet::new();
    for (table, segs) in &remote.tables {
        for s in segs.iter().filter(|s| !s.provisional) {
            let key = format!("{prefix}/{}", parquet_key(table, &s.hash));
            if mirror.head_size(&key).await?.is_some() {
                have.insert((table.clone(), s.hash.clone()));
            }
        }
    }
    Ok(have)
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
    sync_with(dir, target, dry_run, DEFAULT_PARALLELISM, None).await
}

/// [`sync`] with `parallelism` uploads in flight, reporting progress to `metrics` when given.
pub async fn sync_with(
    dir: &Path,
    target: &str,
    dry_run: bool,
    parallelism: usize,
    metrics: Option<&crate::metrics::NestMetrics>,
) -> Result<SyncReport> {
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
    let have = remote_present(mirror.as_ref(), &data_identity, &remote).await?;
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
            uploaded_bytes: 0,
            skipped,
        });
    }

    if let Some(m) = metrics {
        m.set_publish_pending(missing.len() as u64);
    }
    let mirror_ref = mirror.as_ref();
    let attempts: Vec<(String, Result<u64>)> =
        futures::stream::iter(missing.iter().map(|(table, seg)| {
            let src = seal::segment_path(dir, &seg.file, &seg.hash);
            let name = parquet_key(table, &seg.hash);
            let key = prefix(&name);
            async move {
                let upload = async {
                    let len = match std::fs::metadata(&src) {
                        Ok(meta) => meta.len(),
                        Err(_) => bail!(
                            "catalogue names {} but the file is missing at {}",
                            seg.file,
                            src.display()
                        ),
                    };
                    mirror_ref.put_file(&key, &src).await?;
                    if let Some(m) = metrics {
                        m.publish_uploaded(len);
                    }
                    Ok(len)
                };
                (name, upload.await)
            }
        }))
        .buffer_unordered(parallelism.max(1))
        .collect()
        .await;
    let mut parquet = Vec::with_capacity(attempts.len());
    let mut failed = Vec::new();
    for (name, attempt) in attempts {
        match attempt {
            Ok(len) => parquet.push((name, len)),
            Err(e) => failed.push((name, e)),
        }
    }
    if !failed.is_empty() {
        failed.sort_by(|a, b| a.0.cmp(&b.0));
        return Err(UploadFailures(failed).into());
    }
    parquet.sort();
    let uploaded_bytes = parquet.iter().map(|(_, len)| len).sum();
    let mut uploaded: Vec<String> = parquet.into_iter().map(|(name, _)| name).collect();

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
        uploaded_bytes,
        skipped,
    })
}

/// HEAD every published file. `--deep` re-hashes parquet against the local bytes.
pub async fn verify(dir: &Path, target: &str, deep: bool) -> Result<()> {
    let local = seal::load_manifest(dir)?;
    let (data_identity, nid, bundle_hash, chain_id) = identity_of(dir)?;
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
    let want = want_entries(&local);
    let sealed_through = want.iter().map(|(_, s)| s.to_block).max();
    let tables: Vec<String> = local.tables.keys().cloned().collect();
    if env.layout_version != 1 {
        bail!(
            "publish.json layout_version is {}, expected 1",
            env.layout_version
        );
    }
    if env.data_identity != data_identity {
        bail!("publish.json data_identity does not match this nest");
    }
    if env.nid != nid {
        bail!("publish.json nid does not match this nest");
    }
    if env.bundle_hash != bundle_hash {
        bail!("publish.json bundle_hash does not match this nest");
    }
    if env.chain_id != chain_id {
        bail!("publish.json chain_id does not match this nest");
    }
    if env.sealed_through != sealed_through {
        bail!("publish.json sealed_through does not match the local catalogue");
    }
    if env.tables != tables {
        bail!("publish.json tables do not match the local catalogue");
    }
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

/// Objects uploaded at once when nobody said otherwise (RFC-0052 §3.4).
pub const DEFAULT_PARALLELISM: usize = 2;
/// Consecutive failures on one object before the mirror reports itself dead-lettered.
const DEAD_LETTER_AFTER: u32 = 5;
/// Where a failure outside any one object's upload is counted.
const CATALOGUE: &str = "catalogue";

/// The objects a pass could not upload, by key. The pass wrote no catalogue.
#[derive(Debug)]
pub struct UploadFailures(pub Vec<(String, anyhow::Error)>);

impl std::fmt::Display for UploadFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.0.iter().map(|(k, _)| k.as_str()).collect();
        write!(
            f,
            "{} object(s) failed to upload: {}",
            keys.len(),
            keys.join(", ")
        )?;
        if let Some((key, e)) = self.0.first() {
            write!(f, " ({key}: {e:#})")?;
        }
        Ok(())
    }
}

impl std::error::Error for UploadFailures {}

/// Consecutive failures per object. Every missing object is attempted on every pass that reaches
/// the uploads, so an object absent from a pass's failures has been published.
#[derive(Debug, Default)]
struct Failures(HashMap<String, u32>);

impl Failures {
    fn succeeded(&mut self) {
        self.0.clear();
    }

    /// Counts a failed pass and returns the objects it has just dead-lettered.
    fn failed(&mut self, e: &anyhow::Error) -> Vec<String> {
        let failed: Vec<String> = match e.downcast_ref::<UploadFailures>() {
            Some(UploadFailures(objects)) => objects.iter().map(|(k, _)| k.clone()).collect(),
            None => vec![CATALOGUE.to_string()],
        };
        // A pass that failed before uploading anything says nothing about the objects.
        if failed != [CATALOGUE] {
            self.0.retain(|key, _| failed.contains(key));
        }
        let mut dead = Vec::new();
        for key in failed {
            let count = self.0.entry(key.clone()).or_default();
            *count += 1;
            if *count == DEAD_LETTER_AFTER {
                dead.push(key);
            }
        }
        dead
    }

    fn dead(&self) -> bool {
        self.0.values().any(|n| *n >= DEAD_LETTER_AFTER)
    }
}

/// How a running nest mirrors itself (RFC-0052 S2). Operator config, never the nest's identity.
#[derive(Debug, Clone)]
pub struct Settings {
    pub target: String,
    pub interval: std::time::Duration,
    pub parallelism: usize,
}

/// A running mirror. Dropping it stops the reconciler and cancels any upload in flight.
pub struct Publisher {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Publisher {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

/// Start mirroring `dir`, reporting under `nest`. The reconciler owns a thread and a runtime, so a
/// slow or blocking store never holds a worker the ingestion loop needs; a seal only signals it.
pub fn spawn(dir: PathBuf, nest: String, settings: Settings) -> Result<Publisher> {
    open_mirror(&settings.target)?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("publish-{nest}"))
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(reconcile(dir, nest, settings, stopped)),
                Err(e) => tracing::error!("publishing {nest} did not start: {e}"),
            }
        })
        .context("starting the publisher thread")?;
    Ok(Publisher { stop: Some(stop) })
}

async fn reconcile(
    dir: PathBuf,
    nest: String,
    settings: Settings,
    mut stopped: tokio::sync::oneshot::Receiver<()>,
) {
    let metrics = crate::metrics::METRICS.nest(&nest);
    metrics.set_publish_enabled();
    let mut changes = seal::manifest_changes(&dir);
    let mut failures = Failures::default();
    loop {
        let pass = sync_with(
            &dir,
            &settings.target,
            false,
            settings.parallelism,
            Some(metrics.as_ref()),
        );
        tokio::select! {
            _ = &mut stopped => return,
            result = pass => match result {
                Ok(report) => {
                    metrics.publish_succeeded(report.sealed_through);
                    failures.succeeded();
                }
                Err(e) => {
                    let dead = failures.failed(&e);
                    metrics.publish_failed(failures.dead());
                    for object in &dead {
                        tracing::error!(
                            "publishing {nest}: {object} has failed {DEAD_LETTER_AFTER} times in a \
                             row; dead-lettered, retrying at a tenth of the rate until it succeeds: {e:#}"
                        );
                    }
                    if dead.is_empty() && !failures.dead() {
                        tracing::warn!("publishing {nest} failed, will retry: {e:#}");
                    }
                }
            },
        }
        let wait = if failures.dead() {
            settings.interval.saturating_mul(10)
        } else {
            settings.interval
        };
        // Level-triggered: a missed wake-up costs one interval, never an object.
        tokio::select! {
            _ = &mut stopped => return,
            _ = tokio::time::sleep(wait) => {}
            changed = changes.changed() => {
                if changed.is_err() {
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{DecodedRow, Value as DecodedValue};
    use crate::seal::{seal_range, test_set_table_floor};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    type Shared = std::sync::Arc<dyn Mirror>;

    fn test_mirrors() -> &'static std::sync::Mutex<std::collections::HashMap<String, Shared>> {
        static MIRRORS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, Shared>>,
        > = std::sync::OnceLock::new();
        MIRRORS.get_or_init(Default::default)
    }

    pub(super) fn test_mirror(name: &str) -> Result<Box<dyn Mirror>> {
        let mirror = test_mirrors()
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .with_context(|| format!("no test mirror {name}"))?;
        Ok(Box::new(Delegate(mirror)))
    }

    fn install(name: &str, mirror: Shared) -> String {
        test_mirrors()
            .lock()
            .unwrap()
            .insert(name.to_string(), mirror);
        format!("test://{name}")
    }

    struct Delegate(Shared);

    #[async_trait]
    impl Mirror for Delegate {
        async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            self.0.get(key).await
        }
        async fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
            self.0.put(key, bytes).await
        }
        async fn put_file(&self, key: &str, src: &Path) -> Result<()> {
            self.0.put_file(key, src).await
        }
        async fn put_if(&self, key: &str, bytes: &[u8], expected: Option<&[u8]>) -> Result<()> {
            self.0.put_if(key, bytes, expected).await
        }
        async fn head_size(&self, key: &str) -> Result<Option<u64>> {
            self.0.head_size(key).await
        }
    }

    /// Blocks its thread outright in `put_file`, as a synchronous call on a slow disk does.
    struct Blocking;

    #[async_trait]
    impl Mirror for Blocking {
        async fn get(&self, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn put(&self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn put_file(&self, _: &str, _: &Path) -> Result<()> {
            std::thread::sleep(Duration::from_secs(5));
            Ok(())
        }
        async fn put_if(&self, _: &str, _: &[u8], _: Option<&[u8]>) -> Result<()> {
            Ok(())
        }
        async fn head_size(&self, _: &str) -> Result<Option<u64>> {
            Ok(None)
        }
    }

    struct Failing;

    #[async_trait]
    impl Mirror for Failing {
        async fn get(&self, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn put(&self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn put_file(&self, key: &str, _: &Path) -> Result<()> {
            bail!("refusing {key}")
        }
        async fn put_if(&self, _: &str, _: &[u8], _: Option<&[u8]>) -> Result<()> {
            Ok(())
        }
        async fn head_size(&self, _: &str) -> Result<Option<u64>> {
            Ok(None)
        }
    }

    #[derive(Default)]
    struct Counting {
        in_flight: AtomicUsize,
        most: AtomicUsize,
    }

    #[async_trait]
    impl Mirror for Counting {
        async fn get(&self, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn put(&self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn put_file(&self, _: &str, _: &Path) -> Result<()> {
            let now = self.in_flight.fetch_add(1, SeqCst) + 1;
            self.most.fetch_max(now, SeqCst);
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.in_flight.fetch_sub(1, SeqCst);
            Ok(())
        }
        async fn put_if(&self, _: &str, _: &[u8], _: Option<&[u8]>) -> Result<()> {
            Ok(())
        }
        async fn head_size(&self, _: &str) -> Result<Option<u64>> {
            Ok(None)
        }
    }

    async fn eventually(what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn settings(target: String, interval: Duration, parallelism: usize) -> Settings {
        Settings {
            target,
            interval,
            parallelism,
        }
    }

    #[tokio::test]
    async fn a_seal_wakes_the_publisher_long_before_its_interval() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap().to_string();
        let _publisher = spawn(
            nest.path().to_path_buf(),
            "publish-wake".into(),
            settings(target, Duration::from_secs(3600), 2),
        )
        .unwrap();
        let (identity, _, _, _) = identity_of(nest.path()).unwrap();
        let remote = mirror.path().join(&identity).join(MANIFEST_FILE);
        let local = nest.path().join(seal::SEGMENTS_DIR).join(MANIFEST_FILE);
        let matches = || std::fs::read(&remote).ok() == std::fs::read(&local).ok();
        eventually("the first pass", matches).await;
        seal_range(nest.path(), &[row(12, 9)], 12, 12)
            .unwrap()
            .expect("sealed");
        eventually("a seal to wake the mirror inside its hour", matches).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_blocking_store_never_holds_the_callers_runtime() {
        let nest = sealed_nest();
        let target = install("blocking", std::sync::Arc::new(Blocking));
        let _publisher = spawn(
            nest.path().to_path_buf(),
            "publish-blocking".into(),
            settings(target, Duration::from_secs(3600), 1),
        )
        .unwrap();
        // Long enough for the publisher to be inside its five-second put.
        std::thread::sleep(Duration::from_millis(500));
        let started = std::time::Instant::now();
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the caller's runtime waited {:?} on the store",
            started.elapsed()
        );
        let started = std::time::Instant::now();
        seal_range(nest.path(), &[row(12, 9)], 12, 12)
            .unwrap()
            .expect("sealed");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a seal waited {:?} on the store",
            started.elapsed()
        );
    }

    fn refused(keys: &[&str]) -> anyhow::Error {
        UploadFailures(
            keys.iter()
                .map(|k| (k.to_string(), anyhow::anyhow!("refused")))
                .collect(),
        )
        .into()
    }

    #[test]
    fn an_object_failing_beside_others_keeps_its_own_count() {
        let mut failures = Failures::default();
        for pass in 1..DEAD_LETTER_AFTER {
            let keys: &[&str] = if pass % 2 == 0 { &["a", "b"] } else { &["a"] };
            assert!(failures.failed(&refused(keys)).is_empty());
        }
        assert_eq!(
            failures.failed(&refused(&["a", "b"])),
            vec!["a".to_string()]
        );
        assert!(failures.dead());
    }

    #[test]
    fn a_catalogue_failure_neither_resets_nor_counts_an_object() {
        let mut failures = Failures::default();
        for _ in 1..DEAD_LETTER_AFTER {
            failures.failed(&refused(&["a"]));
        }
        failures.failed(&anyhow::anyhow!("bucket unreachable"));
        assert!(!failures.dead());
        assert_eq!(failures.failed(&refused(&["a"])), vec!["a".to_string()]);
    }

    #[test]
    fn an_object_that_uploads_starts_counting_again() {
        let mut failures = Failures::default();
        for _ in 1..DEAD_LETTER_AFTER {
            failures.failed(&refused(&["a"]));
        }
        failures.failed(&refused(&["b"]));
        failures.failed(&refused(&["a"]));
        assert!(!failures.dead());
    }

    #[derive(Default)]
    struct RefusingFirst {
        refused: std::sync::Mutex<Option<String>>,
        stored: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait]
    impl Mirror for RefusingFirst {
        async fn get(&self, _: &str) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
        async fn put(&self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn put_file(&self, key: &str, _: &Path) -> Result<()> {
            let mut refused = self.refused.lock().unwrap();
            if refused.is_none() {
                *refused = Some(key.to_string());
                bail!("refusing {key}");
            }
            self.stored.lock().unwrap().push(key.to_string());
            Ok(())
        }
        async fn put_if(&self, _: &str, _: &[u8], _: Option<&[u8]>) -> Result<()> {
            Ok(())
        }
        async fn head_size(&self, _: &str) -> Result<Option<u64>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn one_refused_object_does_not_stop_the_others_uploading() {
        let nest = sealed_nest();
        seal_range(nest.path(), &[row(12, 9)], 12, 12)
            .unwrap()
            .expect("sealed");
        let mirror = std::sync::Arc::new(RefusingFirst::default());
        let target = install("refusing-first", mirror.clone());
        let e = sync_with(nest.path(), &target, false, 1, None)
            .await
            .unwrap_err();
        let UploadFailures(failed) = e.downcast_ref::<UploadFailures>().expect("per object");
        let refused = mirror.refused.lock().unwrap().clone().unwrap();
        assert_eq!(failed.len(), 1);
        assert!(
            refused.ends_with(&failed[0].0),
            "{refused} vs {}",
            failed[0].0
        );
        assert!(
            !mirror.stored.lock().unwrap().is_empty(),
            "nothing uploaded after the refusal"
        );
    }

    #[tokio::test]
    async fn repeated_failures_on_one_object_dead_letter_the_mirror() {
        let nest = sealed_nest();
        let target = install("failing", std::sync::Arc::new(Failing));
        let _publisher = spawn(
            nest.path().to_path_buf(),
            "publish-failing".into(),
            settings(target, Duration::from_millis(5), 2),
        )
        .unwrap();
        let metrics = crate::metrics::METRICS.nest("publish-failing");
        eventually("the mirror to dead-letter", || {
            metrics.publish_dead_letter()
        })
        .await;
        assert!(
            metrics.publish_errors() >= u64::from(DEAD_LETTER_AFTER),
            "{} errors",
            metrics.publish_errors()
        );
        assert!(crate::metrics::METRICS
            .render()
            .contains("nuthatch_publish_dead_letter{nest=\"publish-failing\"} 1"));
    }

    #[tokio::test]
    async fn uploads_run_in_parallel_up_to_the_bound_and_no_further() {
        let nest = tempfile::tempdir().unwrap();
        write_nest(nest.path());
        test_set_table_floor(nest.path(), 0);
        for block in 10..14 {
            seal_range(nest.path(), &[row(block, block)], block, block)
                .unwrap()
                .expect("sealed");
        }
        let counting = std::sync::Arc::new(Counting::default());
        let target = install("counting", counting.clone());
        let report = sync_with(nest.path(), &target, false, 2, None)
            .await
            .unwrap();
        let parquet = report
            .uploaded
            .iter()
            .filter(|k| k.ends_with(".parquet"))
            .count();
        assert_eq!(parquet, 4);
        assert_eq!(counting.most.load(SeqCst), 2);
    }

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

    fn parquet_rows(dir: &Path) -> i64 {
        use parquet::file::reader::{FileReader, SerializedFileReader};
        fn walk(p: &Path, acc: &mut i64) {
            if p.is_dir() {
                for e in std::fs::read_dir(p).unwrap() {
                    walk(&e.unwrap().path(), acc);
                }
            } else if p.extension().and_then(|s| s.to_str()) == Some("parquet") {
                let r = SerializedFileReader::new(std::fs::File::open(p).unwrap()).unwrap();
                *acc += r.metadata().file_metadata().num_rows();
            }
        }
        let mut n = 0;
        walk(dir, &mut n);
        n
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

        assert_eq!(
            parquet_rows(&nest.path().join(seal::SEGMENTS_DIR)),
            parquet_rows(&mirror.path().join(&report.dataset)),
            "mirrored parquet must carry the same rows as the sealed directory"
        );
    }

    #[tokio::test]
    async fn a_second_sync_reuploads_a_deleted_segment() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap();
        let first = sync(nest.path(), target, false).await.unwrap();
        let parquet = first
            .uploaded
            .iter()
            .find(|k| k.ends_with(".parquet"))
            .expect("first sync uploaded parquet")
            .clone();
        std::fs::remove_file(mirror.path().join(&first.dataset).join(&parquet)).unwrap();
        let second = sync(nest.path(), target, false).await.unwrap();
        assert!(
            second.uploaded.iter().any(|k| k == &parquet),
            "missing object must be put again, got {:?}",
            second.uploaded
        );
        verify(nest.path(), target, false).await.unwrap();
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
    async fn verify_fails_if_the_envelope_nid_is_wrong() {
        let nest = sealed_nest();
        let mirror = tempfile::tempdir().unwrap();
        let target = mirror.path().to_str().unwrap();
        let report = sync(nest.path(), target, false).await.unwrap();
        let path = mirror.path().join(&report.dataset).join("publish.json");
        let mut env: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        env["nid"] = serde_json::json!("0".repeat(64));
        std::fs::write(&path, serde_json::to_vec_pretty(&env).unwrap()).unwrap();
        let err = verify(nest.path(), target, false).await.unwrap_err();
        assert!(
            err.to_string().contains("nid"),
            "wanted an nid failure, got {err}"
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

    #[tokio::test]
    async fn concurrent_put_if_does_not_regress_the_catalogue() {
        let dir = tempfile::tempdir().unwrap();
        let m = FsMirror {
            root: dir.path().to_path_buf(),
        };
        m.put("manifest.json", b"old").await.unwrap();
        let a = m.put_if("manifest.json", b"aaa", Some(b"old"));
        let b = m.put_if("manifest.json", b"bbb", Some(b"old"));
        let (ra, rb) = tokio::join!(a, b);
        assert!(
            ra.is_ok() ^ rb.is_ok(),
            "exactly one CAS should win, got {ra:?} {rb:?}"
        );
        let got = m.get("manifest.json").await.unwrap().unwrap();
        assert!(
            got == b"aaa" || got == b"bbb",
            "winner must be one of the two new catalogues, got {got:?}"
        );
    }
}
