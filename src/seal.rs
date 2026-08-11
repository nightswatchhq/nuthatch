//! Per-table sealing (RFC-0001 step 4): once a block range is final, each table's rows in that
//! range are written to their own content-addressed Parquet segment, catalogued per table in the
//! manifest. The columnar cold layer is append-only - it never sees a reorg, because reorgs only
//! ever touch the mutable hot store (see store::rollback_to).
//!
//! All tables in a nest ingest from the same block stream and seal together per finalized range, so
//! `sealed_through` stays a single global watermark and the whole range is pruned from hot once every
//! table's segment is durable (the indexer does the prune).
//!
//! ## Scope of the content hash (audit F-D3)
//!
//! A segment's hash is taken over the **Parquet file bytes**, and those bytes include the `created_by`
//! metadata string that `arrow-rs`/`parquet` stamps with its own version. So the guarantee is:
//!
//! - **Same binary → identical bytes → identical hash.** This is the property everything relies on:
//!   re-running a backfill, or two operators running the same release, produce byte-identical segments
//!   that dedupe against each other. Determinism holds.
//! - **Across nuthatch versions built on different arrow-rs releases, segment identity may differ**
//!   even when every decoded row is identical, because `created_by` changed underneath us.
//!
//! That is a limit on *segment identity*, not on correctness: the rows are the same, the queries return
//! the same answers, and re-execution still verifies. What it means practically is that a segment hash
//! is a strong identity within a release and a weak one across releases - so do not use one as a
//! cross-version equality proof (compare decoded rows for that), and expect a version bump that moves
//! arrow-rs to re-seal rather than dedupe. Pinning `created_by` would buy cross-version identity if that
//! ever becomes worth the coupling to a parquet-rs internal; it is deliberately not pinned today.

use anyhow::{Context, Result};
use arrow::array::{ArrayRef, StringArray, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SEGMENTS_DIR: &str = "segments";

/// The **shared segment store** for the runtime `dir` belongs to, if it has one (RFC-0033 §11a).
///
/// Derived by convention rather than plumbed: a mounted dataset lives at `<root>/data/<nid>`, so its
/// runtime root is two levels up and the store is `<root>/segments`. A solo `nuthatch dev` nest is
/// not under a `data/` parent, gets `None`, and keeps its segments where they have always been -
/// sharing is a property of a runtime hosting several nests, not of a nest.
///
/// The alternative was threading a root through every seal and query call site, which would have made
/// the layout a parameter of functions that have no business knowing about it.
pub fn shared_store(dir: &Path) -> Option<PathBuf> {
    let parent = dir.parent()?;
    if parent.file_name()? != crate::runtime::DATA_DIR {
        return None;
    }
    Some(parent.parent()?.join(SEGMENTS_DIR))
}

/// Where a segment's bytes actually live: the shared store when there is one, else beside the nest.
///
/// **Falls back to the per-dataset path when the shared copy is absent**, so a dataset migrated
/// before slice C still reads. A missing shared segment is not evidence of corruption; it is evidence
/// of a layout that has not been relocated yet.
pub fn segment_path(dir: &Path, file: &str, hash: &str) -> PathBuf {
    if let Some(store) = shared_store(dir) {
        let shared = store.join(format!("{hash}.parquet"));
        if shared.exists() {
            return shared;
        }
    }
    dir.join(SEGMENTS_DIR).join(file)
}
pub const MANIFEST_FILE: &str = "manifest.json";

/// One sealed Parquet file. `hash` is the content address (sha256 of the file bytes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub hash: String,
    pub from_block: u64,
    pub to_block: u64,
    pub rows: usize,
    pub file: String,
    /// The discovered-child registry's content hash at seal time (RFC-0009): records exactly which
    /// factory-discovered set produced this segment, so its child rows are reproducible. `None` for a
    /// static (non-factory) nest. Absent in pre-RFC-0009 manifests (serde default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry_snapshot: Option<String>,
}

/// The segment catalogue: per-table lists of sealed segments.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub tables: BTreeMap<String, Vec<Segment>>,
}

/// What a `seal_range` call sealed.
#[derive(Debug, Default)]
pub struct SealSummary {
    pub tables: usize,
    pub rows: usize,
}

/// Seal every table's rows in a finalized `[from, to]` range. Rows are grouped by their `table`
/// field; each group becomes one content-addressed Parquet segment catalogued under its table.
/// Returns None if the range held no rows.
pub fn seal_range(
    dir: &Path,
    entity_json: &[String],
    from: u64,
    to: u64,
) -> Result<Option<SealSummary>> {
    seal_range_with_snapshot(dir, entity_json, from, to, None)
}

/// Like [`seal_range`], but records `registry_snapshot` (the discovered-child registry's content hash)
/// on each segment's manifest entry - the factory paths (RFC-0009) pass it so a segment records which
/// discovered set produced it. A static nest passes `None` (via `seal_range`).
pub fn seal_range_with_snapshot(
    dir: &Path,
    entity_json: &[String],
    from: u64,
    to: u64,
    registry_snapshot: Option<&str>,
) -> Result<Option<SealSummary>> {
    let mut by_table: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for j in entity_json {
        let Ok(v) = serde_json::from_str::<Value>(j) else {
            continue;
        };
        let table = v
            .get("table")
            .and_then(Value::as_str)
            .unwrap_or("rows")
            .to_string();
        by_table.entry(table).or_default().push(v);
    }
    if by_table.is_empty() {
        return Ok(None);
    }

    let seg_dir = dir.join(SEGMENTS_DIR);
    std::fs::create_dir_all(&seg_dir)
        .with_context(|| format!("cannot create {}", seg_dir.display()))?;
    let mut manifest = load_manifest(dir)?;
    let mut summary = SealSummary::default();

    for (table, rows) in by_table {
        let batch = rows_to_batch(&rows)?;
        let bytes = write_parquet(&batch)?;
        let hash = hex::encode(Sha256::digest(&bytes));
        let file = format!("{table}-{hash}.parquet");
        let segments = manifest.tables.entry(table).or_default();
        // Content-addressed idempotency: an identical segment (same table + hash) is already
        // catalogued, so re-sealing the same rows - e.g. re-running `nuthatch screen` over a range to
        // re-audit - is a no-op rather than a double-listed (double-counted) segment.
        if segments.iter().any(|s| s.hash == hash) {
            continue;
        }
        // Write once, into the shared store when this dataset belongs to a runtime (RFC-0033 §11a).
        // Content-addressed, so a second nest sealing byte-identical rows finds it already there and
        // the write is a no-op rather than a duplicate.
        match shared_store(dir) {
            Some(store) => {
                std::fs::create_dir_all(&store).context("creating the shared segment store")?;
                let shared = store.join(format!("{hash}.parquet"));
                if !shared.exists() {
                    std::fs::write(&shared, &bytes).context("failed to write shared segment")?;
                }
            }
            None => {
                std::fs::write(seg_dir.join(&file), &bytes).context("failed to write segment")?;
            }
        }
        summary.tables += 1;
        summary.rows += rows.len();
        segments.push(Segment {
            hash,
            from_block: from,
            to_block: to,
            rows: rows.len(),
            file,
            registry_snapshot: registry_snapshot.map(str::to_string),
        });
    }

    save_manifest(dir, &manifest)?;
    Ok(Some(summary))
}

/// Build an Arrow batch from a table's JSON rows. `block_number`/`log_index` are UInt64; every other
/// column is Utf8 (values already carry their canonical text form - hex, decimal, or string).
fn rows_to_batch(rows: &[Value]) -> Result<RecordBatch> {
    let mut columns: BTreeSet<String> = BTreeSet::new();
    for r in rows {
        if let Some(obj) = r.as_object() {
            columns.extend(obj.keys().cloned());
        }
    }
    let columns: Vec<String> = columns.into_iter().collect();

    let mut fields = Vec::with_capacity(columns.len());
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(columns.len());
    for col in &columns {
        if col == "block_number" || col == "log_index" || col == "_seq" || col == "block_timestamp"
        {
            let vals: Vec<u64> = rows
                .iter()
                .map(|r| r.get(col).and_then(Value::as_u64).unwrap_or(0))
                .collect();
            fields.push(Field::new(col, DataType::UInt64, false));
            arrays.push(Arc::new(UInt64Array::from(vals)));
        } else {
            let vals: Vec<Option<String>> = rows
                .iter()
                .map(|r| match r.get(col) {
                    Some(Value::String(s)) => Some(s.clone()),
                    None | Some(Value::Null) => None,
                    Some(other) => Some(other.to_string()),
                })
                .collect();
            fields.push(Field::new(col, DataType::Utf8, true));
            arrays.push(Arc::new(StringArray::from(vals)));
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .context("failed to build record batch")
}

fn write_parquet(batch: &RecordBatch) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let props = WriterProperties::builder()
        .set_compression(Compression::SNAPPY)
        .build();
    let mut writer = ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))
        .context("failed to create parquet writer")?;
    writer.write(batch).context("failed to write batch")?;
    writer.close().context("failed to finalise parquet")?;
    Ok(buf)
}

/// Load the segment catalogue (empty if none yet).
pub fn load_manifest(dir: &Path) -> Result<Manifest> {
    let path = manifest_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw).context("corrupt segments manifest"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Manifest::default()),
        Err(e) => Err(e).context("failed to read manifest"),
    }
}

/// The **serving-path** counterpart to [`verify_and_quarantine`]: which of `dir`'s manifest segments
/// no longer hash to their content address. Returns their hashes, for `analytics::define_views` to
/// drop from a rebuilt view (issue #433).
///
/// ## Why the hash and not a DuckDB probe
///
/// #430 drops a segment that will not **bind**, probing with `conn.prepare` over `read_parquet`. That
/// catches footer corruption and nothing else. Measured in the DuckDB CLI (1.5.3) against a segment
/// whose data region is overwritten but whose footer is intact:
///
/// - `SELECT 1 FROM read_parquet([f]) LIMIT 0` - the #430 probe - **succeeds**;
/// - `count(*)` and `max(col)` **succeed**, answered from Parquet metadata without reading a page;
/// - `SELECT * … LIMIT 1` fails on that file, and **succeeds** on one where only the late row groups
///   are corrupt, because it never reads them.
///
/// So the only sound DuckDB-side discriminator is a full scan of every column of every segment, and
/// it would be pinned to whatever the query planner prunes this release. The content address is not:
/// sealed segments are immutable, so any changed byte is unambiguous corruption, and this is already
/// the check `verify_and_quarantine` makes at startup. Using the same one here is what stops the
/// serving path and the startup path disagreeing about the same file, which was half of #419.
///
/// **Reduce here, never quarantine.** Quarantine moves bytes, and the shared store's bytes belong to
/// every dataset referencing them (RFC-0033 §11a). Dropping a segment from one query's view changes
/// nothing on disk and so is safe for a shared segment, which is why this returns hashes rather than
/// calling the startup pass.
///
/// ## Cost, and the cache that is deliberately not here
///
/// This reads and hashes every catalogued segment, every time it is called. That is not free, and the
/// thing keeping it affordable is the *caller*: `run` only asks after a query has **bound and then
/// died reading rows**. A bind failure - a typo, a missing column, the commonest error on this
/// surface - never reaches here, so the cheap way to provoke a sweep does not exist.
///
/// **There was a memo here keyed on the file's `(mtime, len)`, and it was wrong.** The idea was that a
/// segment already verified in this process is a `stat` rather than a read. Measured on this box
/// (btrfs, Linux 6.12): rewriting a file in place with the same length inside one timer tick leaves
/// `st_mtime_ns` **byte-identical**, because the kernel stamps writes from a per-tick cached clock. So
/// the key cannot see an in-place same-length overwrite - which is precisely the corruption this
/// function exists to catch. A cache that reports `intact` about a corrupt segment, inside the fix for
/// reporting healthy about corrupt data, is the bug wearing the fix's clothes. It was removed rather
/// than tuned, and `seal::tests::a_corrupt_segment_is_caught_even_when_its_mtime_is_unchanged` is
/// what stops it coming back. That test exists because my first claim here - that the page-corruption
/// test already covered it - was **false**: mutating the memo back in left the whole suite green.
///
/// If sweep cost ever does need bounding, bound it on something that cannot be stale - a coalescing
/// flag so concurrent queries share one sweep, or the gateway, which is already where this project
/// puts per-caller rate limiting (#365). Not on a filesystem timestamp.
pub fn segments_failing_verification(dir: &Path) -> BTreeSet<String> {
    let Ok(manifest) = load_manifest(dir) else {
        return BTreeSet::new();
    };
    let mut bad = BTreeSet::new();
    for (table, segs) in &manifest.tables {
        for s in segs {
            let path = segment_path(dir, &s.file, &s.hash);
            // An absent file is not corruption, and `define_views` already skips it by existence.
            if !path.exists() {
                continue;
            }
            // An unreadable segment is not intact: it cannot serve rows either way, and saying
            // "fine" about a file we could not read is the failure this whole issue is about.
            let intact = std::fs::read(&path)
                .is_ok_and(|bytes| hex::encode(Sha256::digest(&bytes)) == s.hash);
            if !intact {
                tracing::error!(
                    "segment {} for table {table} does not match its content address - dropping it \
                     from this query (cold data reduced). Restart to quarantine it, or re-seal the \
                     range to restore it.",
                    s.file
                );
                bad.insert(s.hash.clone());
            }
        }
    }
    bad
}

/// Startup integrity pass: verify every manifest segment's file exists and its bytes hash to the
/// recorded content address. A file that's missing, unreadable, or hash-mismatched is corrupt or
/// tampered with - quarantine it (move to a sibling `quarantine/` dir so `define_views` skips it) and
/// log loudly, then continue. A corrupt segment must *reduce* a table's cold data, never crash-loop the
/// node. Sealed data is immutable and content-addressed, so a hash mismatch is unambiguous corruption.
/// Returns the number of segments quarantined. Best-effort - never fatal (an IO error just logs).
pub fn verify_and_quarantine(dir: &Path) -> Result<usize> {
    let manifest = load_manifest(dir)?;
    // Sibling of `segments/`, deliberately *outside* it - nothing globs or SQL-reads the quarantine.
    let quarantine = dir.join("quarantine");
    let mut quarantined = 0usize;

    for (table, segs) in &manifest.tables {
        for s in segs {
            let path = segment_path(dir, &s.file, &s.hash);
            // **Never quarantine a shared segment** (RFC-0033 §11a). Quarantine *moves* the file, and
            // in the shared store those bytes belong to every dataset referencing them - one nest's
            // integrity pass must not yank data out from under its neighbours. A corrupt shared
            // segment is reported loudly and left in place; the operator decides, with the knowledge
            // that more than one dataset is affected.
            let is_shared = shared_store(dir).is_some_and(|st| path.starts_with(&st));
            let reason = match std::fs::read(&path) {
                Ok(bytes) if hex::encode(Sha256::digest(&bytes)) == s.hash => continue, // intact
                Ok(_) => "hash mismatch (corrupt or tampered)",
                // Already gone from disk - nothing to move; `define_views` skips it. Not counted.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => "unreadable",
            };
            if is_shared {
                tracing::error!(
                    "shared segment {} for table {table} is {reason} - left in place because other \
                     datasets reference it. Remove it deliberately once you know what else breaks.",
                    s.hash
                );
                continue;
            }
            std::fs::create_dir_all(&quarantine).ok();
            let dest = quarantine.join(&s.file);
            match std::fs::rename(&path, &dest) {
                Ok(()) => {
                    tracing::error!(
                        "quarantined segment {} for table {table} ({reason}) → {} - cold data for this \
                         table is reduced; re-seal the range to restore it",
                        s.file,
                        dest.display()
                    );
                    quarantined += 1;
                }
                Err(e) => tracing::error!(
                    "segment {} for {table} is {reason} but could not be quarantined: {e}",
                    s.file
                ),
            }
        }
    }
    if quarantined > 0 {
        tracing::error!(
            "startup integrity: quarantined {quarantined} corrupt segment(s) - data is reduced, node \
             continues; investigate disk health and re-seal the affected ranges"
        );
    }
    Ok(quarantined)
}

fn save_manifest(dir: &Path, manifest: &Manifest) -> Result<()> {
    let raw = serde_json::to_string_pretty(manifest)?;
    // The manifest is the segment catalogue - the crown jewels of a `kill -9`-survivable single binary
    // (a half-written `manifest.json` orphans every otherwise-fine `.parquet` and fails all cold reads,
    // deadlock-review finding M8). Write a sibling temp file then rename it over the target: `rename`
    // is atomic on the same filesystem, so a reader/crash sees either the old manifest or the new one,
    // never a torn one.
    let path = manifest_path(dir);
    let tmp = path.with_extension("json.tmp");
    // COR-9: fsync the temp file's *bytes* before the rename, and the directory *entry* after - so the
    // atomic rename survives power loss, not just process death. Without the fsyncs a `rename` can be
    // reordered before the data hits disk, exposing a torn/empty manifest that orphans the segments.
    {
        let f = std::fs::File::create(&tmp).context("failed to create manifest temp")?;
        use std::io::Write;
        (&f).write_all(raw.as_bytes())
            .context("failed to write manifest temp")?;
        f.sync_all().context("failed to fsync manifest temp")?;
    }
    std::fs::rename(&tmp, &path).context("failed to install manifest")?;
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all(); // best-effort dir fsync (unsupported on some platforms)
    }
    Ok(())
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join(SEGMENTS_DIR).join(MANIFEST_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use std::fs::File;

    fn transfer(block: u64, li: u64, value: &str) -> String {
        format!(
            r#"{{"table":"usdc__transfer","from":"0xaaaa","to":"0xbbbb","value":"{value}","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }
    fn approval(block: u64, li: u64) -> String {
        format!(
            r#"{{"table":"usdc__approval","owner":"0xaaaa","spender":"0xdddd","value":"1","block_number":{block},"tx_hash":"0xcc","log_index":{li}}}"#
        )
    }

    /// **Issue #433.** The serving-path discriminator must catch page corruption that every cheap
    /// DuckDB probe waves through, and must not accuse a healthy segment.
    ///
    /// The fixture asserts its own premise. Corrupting the data region while leaving the footer and
    /// magic bytes intact is the whole condition - if the file stopped binding it would be #430's
    /// case, already covered, and this test would be proving something else under this name. So it
    /// checks `read_parquet` still binds the corrupt file before asserting the hash catches it. That
    /// gap between "binds" and "hashes wrong" is exactly the ground #433 sits on.
    #[test]
    fn segments_failing_verification_catches_page_corruption_that_still_binds() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();
        seal_range(dir.path(), &[transfer(101, 0, "7")], 101, 101).unwrap();
        let segs = load_manifest(dir.path()).unwrap().tables["usdc__transfer"].clone();
        assert_eq!(segs.len(), 2);

        // Nothing is wrong yet, and saying so is half the test: a discriminator that always answered
        // "corrupt" would reduce every table on the first execution error in a healthy nest.
        assert!(
            segments_failing_verification(dir.path()).is_empty(),
            "a healthy tree must accuse nothing"
        );

        // Destroy the pages of the block-101 segment, keeping footer and magic bytes.
        let victim = segs.iter().find(|s| s.from_block == 101).unwrap();
        let path = segment_path(dir.path(), &victim.file, &victim.hash);
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        let footer_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
        let end = len - 8 - footer_len;
        assert!(end > 4, "the fixture needs a data region to corrupt");
        bytes[4..end].fill(0xFF);
        std::fs::write(&path, &bytes).unwrap();

        // **The fixture is the condition it names.** Still a Parquet file as far as binding goes.
        let conn = duckdb::Connection::open_in_memory().unwrap();
        assert!(
            conn.prepare(&format!(
                "SELECT 1 FROM read_parquet(['{}'], union_by_name=true) LIMIT 0",
                path.display()
            ))
            .is_ok(),
            "if this no longer binds, the fixture has become #430's footer-corrupt case and this \
             test has stopped testing #433"
        );

        let bad = segments_failing_verification(dir.path());
        assert_eq!(
            bad.len(),
            1,
            "exactly the corrupt segment, and not its healthy sibling"
        );
        assert!(bad.contains(&victim.hash), "and it is the one we corrupted");
    }

    /// **Issue #433, and the reason there is no cache in `segments_failing_verification`.**
    ///
    /// A verdict about a segment's bytes must not be keyed on the segment's timestamp. Measured on
    /// this box (btrfs, Linux 6.12): rewriting a file in place with the same length inside one kernel
    /// timer tick leaves `st_mtime_ns` **byte-identical**, because writes are stamped from a per-tick
    /// cached clock. So a `(mtime, len)` memo cannot see an in-place same-length overwrite - exactly
    /// the corruption this is for.
    ///
    /// I know this because I wrote that memo, and then wrote in its doc comment that the test above
    /// would catch it coming back. **It would not**: mutating the memo back in left the whole suite
    /// green, because the two sweeps in that test happen to straddle a tick. A claim about a
    /// mechanism, with nothing behind it, in the fix for claims with nothing behind them.
    ///
    /// So this constructs the condition deterministically instead of hoping for it: corrupt the bytes
    /// and put the original timestamp back, which is what a same-tick overwrite does anyway. Linux
    /// only - it needs GNU `touch -d @<epoch.nanos>` - and CI is Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_corrupt_segment_is_caught_even_when_its_mtime_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(200, 0, "9")], 200, 200).unwrap();
        let seg = load_manifest(dir.path()).unwrap().tables["usdc__transfer"][0].clone();
        let path = segment_path(dir.path(), &seg.file, &seg.hash);

        // Ask once while it is healthy: this is what would populate any cache.
        assert!(segments_failing_verification(dir.path()).is_empty());

        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let len = bytes.len();
        let footer_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
        bytes[4..len - 8 - footer_len].fill(0xFF);
        std::fs::write(&path, &bytes).unwrap();

        // Put the clock back, so the file is byte-different and timestamp-identical.
        let d = before.duration_since(std::time::UNIX_EPOCH).unwrap();
        let ok = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("@{}.{:09}", d.as_secs(), d.subsec_nanos()))
            .arg(&path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "the fixture needs GNU touch to restore the timestamp");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "the whole point of this test is that the timestamp did not move"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            len as u64,
            "and neither did the length"
        );

        assert_eq!(
            segments_failing_verification(dir.path()).len(),
            1,
            "corruption must be caught from the bytes - a verdict cached on (mtime, len) would call \
             this segment intact, which is the failure this whole issue is about"
        );
    }

    #[test]
    fn verify_quarantines_a_corrupt_segment_and_leaves_intact_ones() {
        let dir = tempfile::tempdir().unwrap();
        seal_range(dir.path(), &[transfer(100, 0, "5")], 100, 100).unwrap();
        seal_range(dir.path(), &[transfer(101, 0, "7")], 101, 101).unwrap();
        let manifest = load_manifest(dir.path()).unwrap();
        let segs = &manifest.tables["usdc__transfer"];
        assert_eq!(segs.len(), 2);

        // A clean tree quarantines nothing.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 0);

        // Corrupt the first segment's bytes (simulate disk rot / tampering).
        let bad = dir.path().join(SEGMENTS_DIR).join(&segs[0].file);
        std::fs::write(&bad, b"not a parquet file anymore").unwrap();

        // Verify quarantines exactly the corrupt one; the intact one stays put.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 1);
        assert!(!bad.exists(), "corrupt file moved out of segments/");
        assert!(
            dir.path().join("quarantine").join(&segs[0].file).exists(),
            "corrupt file is in quarantine/"
        );
        assert!(
            dir.path().join(SEGMENTS_DIR).join(&segs[1].file).exists(),
            "intact segment untouched"
        );
        // Idempotent: the already-quarantined (now-missing) segment isn't re-counted.
        assert_eq!(verify_and_quarantine(dir.path()).unwrap(), 0);
    }

    #[test]
    fn seals_each_table_to_its_own_segment() {
        let dir = tempfile::tempdir().unwrap();
        let rows = vec![
            transfer(100, 0, "5"),
            transfer(100, 1, "7"),
            approval(101, 0),
        ];
        let summary = seal_range(dir.path(), &rows, 100, 101).unwrap().unwrap();
        assert_eq!(summary.tables, 2); // transfer + approval
        assert_eq!(summary.rows, 3);

        let manifest = load_manifest(dir.path()).unwrap();
        assert_eq!(manifest.tables["usdc__transfer"].len(), 1);
        assert_eq!(manifest.tables["usdc__transfer"][0].rows, 2);
        assert_eq!(manifest.tables["usdc__approval"][0].rows, 1);

        // The transfer segment reads back with the right rows.
        let seg = &manifest.tables["usdc__transfer"][0];
        let file = File::open(dir.path().join(SEGMENTS_DIR).join(&seg.file)).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .build()
            .unwrap();
        let total: usize = reader.map(|b| b.unwrap().num_rows()).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn empty_range_seals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seal_range(dir.path(), &[], 1, 2).unwrap().is_none());
    }

    #[test]
    fn content_address_is_deterministic() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let rows = vec![transfer(1, 0, "1")];
        seal_range(dir1.path(), &rows, 1, 1).unwrap();
        seal_range(dir2.path(), &rows, 1, 1).unwrap();
        let a = &load_manifest(dir1.path()).unwrap().tables["usdc__transfer"][0].hash;
        let b = &load_manifest(dir2.path()).unwrap().tables["usdc__transfer"][0].hash;
        assert_eq!(a, b); // same rows in → same content address
    }

    /// RFC-0004 §1 path-equivalence: rows sealed *directly* (the seal-direct backfill path) and the
    /// same rows sealed *after a redb round-trip* (the hot-then-seal path) yield byte-identical
    /// segments. `seal_range` is the one shared writer, so the two backfill paths are provably the
    /// same bytes - the determinism claim the optimisation rests on.
    #[test]
    fn seal_direct_matches_seal_via_hot_store() {
        use crate::store::Store;
        let rows = vec![
            transfer(100, 0, "5"),
            transfer(100, 1, "7"),
            approval(101, 0),
            transfer(102, 0, "9"),
        ];

        // Path A - direct: seal the decoded rows as-is.
        let da = tempfile::tempdir().unwrap();
        seal_range(da.path(), &rows, 100, 102).unwrap();

        // Path B - via hot store: write to redb, read the range back, then seal.
        let db = tempfile::tempdir().unwrap();
        let store = Store::open(&db.path().join("hot.redb")).unwrap();
        for r in &rows {
            let v: Value = serde_json::from_str(r).unwrap();
            let key = Store::entity_key(
                v["block_number"].as_u64().unwrap(),
                v["log_index"].as_u64().unwrap(),
            );
            store.put_entity(&key, r).unwrap();
        }
        let readback = store.entities_in_range(100, 102).unwrap();
        seal_range(db.path(), &readback, 100, 102).unwrap();

        // Same tables, same per-table content hashes.
        let ma = load_manifest(da.path()).unwrap();
        let mb = load_manifest(db.path()).unwrap();
        assert_eq!(
            ma.tables.keys().collect::<Vec<_>>(),
            mb.tables.keys().collect::<Vec<_>>()
        );
        for (table, segs) in &ma.tables {
            assert_eq!(
                segs[0].hash, mb.tables[table][0].hash,
                "segment hash differs for {table} between direct and via-hot-store paths"
            );
        }
    }
}
